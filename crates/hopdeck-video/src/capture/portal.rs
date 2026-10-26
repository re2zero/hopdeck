//! xdg-desktop-portal ScreenCast capture backend (Linux, `portal` feature).
//!
//! `PortalCapture::start` runs the ashpd ScreenCast flow on the daemon's
//! async runtime: first run pops the compositor's picker dialog, later runs
//! are silent once a restore token is passed back (v1 only logs the token —
//! persisting it into `[video]` lands with a later increment, so expect one
//! dialog per daemon run until then). Frames come from a dedicated PipeWire
//! thread: `open_pipe_wire_remote` hands us an fd, `Context::connect_fd`
//! attaches to the portal's PipeWire instance, and a capture stream fixated
//! on BGRx pumps shm buffers that are converted to tightly-packed BGRA and
//! pushed into a tokio watch — latest frame wins.
//!
//! Backpressure is deliberate frame dropping, matching the design's ≤1-frame
//! latency budget: consumers read the newest frame whenever their pacing
//! allows. The stream only pumps while a consumer exists ([`PortalCapture::subscribe`]
//! activates it, dropping the last [`PortalFrames`] pauses it), so an idle
//! video channel costs no buffer copies.
//!
//! Nothing here can be exercised by CI (Xvfb has no portal); the unit tests
//! cover the pure pieces — BGRx→BGRA conversion and the pacing stream.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, Session};
use ashpd::enumflags2::BitFlags;
use futures_core::Stream;
use pipewire as pw;
use pipewire::spa::param::format::MediaSubtype;
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::pod::Pod;
use tokio::sync::watch;
use tokio::time::Sleep;

use crate::frame::VideoFrame;

/// Negotiated frame size, shared between the PipeWire thread and the owner.
type GeometrySlot = Arc<Mutex<Option<(u32, u32)>>>;

/// Commands sent into the PipeWire thread while its loop is running.
enum Cmd {
    Quit,
    Pump(bool),
}

/// An eager portal capture session: the ashpd session plus a PipeWire thread
/// pumping converted frames. Owned by the daemon's [`crate::video`] host —
/// `stop` (also `Drop`) quits the pump thread and joins it.
pub struct PortalCapture {
    geometry: GeometrySlot,
    frames: watch::Sender<Option<VideoFrame>>,
    cmd: pw::channel::Sender<Cmd>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Dropping the session tears down the portal side; keep it until then.
    _session: Session<Screencast>,
    restore_token: Option<String>,
    max_fps: u32,
}

impl PortalCapture {
    /// Starts a ScreenCast session for the primary monitor and spins up the
    /// PipeWire pump. `restore_token` (from a previous run's
    /// [`Self::restore_token`]) suppresses the picker dialog when still valid.
    ///
    /// Returns once the session is authorized; the first frames may still be
    /// a few ticks away (format negotiation happens on the pump thread and
    /// [`Self::geometry`] reports it).
    pub async fn start(
        max_fps: u32,
        restore_token: Option<String>,
    ) -> Result<Self, crate::VideoError> {
        let proxy = Screencast::new()
            .await
            .map_err(|e| crate::VideoError::Portal(e.to_string()))?;
        let session = proxy
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|e| crate::VideoError::Portal(e.to_string()))?;

        // Embedded bakes the pointer into the pixels: visible cursor with no
        // rendering support on the receiver. Monitor-only, single source.
        // ExplicitlyRevoked keeps the grant until the user revokes it, so a
        // stored restore token skips future dialogs.
        let mut select = SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::Embedded)
            .set_sources(BitFlags::from(SourceType::Monitor))
            .set_multiple(false)
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        if let Some(token) = restore_token.as_deref() {
            select = select.set_restore_token(token);
        }
        proxy
            .select_sources(&session, select)
            .await
            .map_err(|e| crate::VideoError::Portal(e.to_string()))?;

        let started = proxy
            .start(&session, None, StartCastOptions::default())
            .await
            .map_err(|e| crate::VideoError::Portal(e.to_string()))?
            .response()
            .map_err(|e| crate::VideoError::Portal(e.to_string()))?;
        let streams = started.streams();
        let Some(stream) = streams.first() else {
            return Err(crate::VideoError::Portal(
                "portal returned no streams".into(),
            ));
        };
        let node_id = stream.pipe_wire_node_id();
        // The portal knows the size up front; the pump thread overwrites it
        // with the authoritative negotiated format once param_changed fires.
        let geometry: GeometrySlot = Arc::new(Mutex::new(stream.size().and_then(|(w, h)| {
            if w > 0 && h > 0 {
                Some((w as u32, h as u32))
            } else {
                None
            }
        })));
        let restore_token = started.restore_token().map(str::to_owned);

        let fd = proxy
            .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
            .await
            .map_err(|e| crate::VideoError::Portal(e.to_string()))?;

        let (frames_tx, _frames_rx) = watch::channel(None);
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let host_frames = frames_tx.clone();
        let pump_geometry = Arc::clone(&geometry);
        let worker = std::thread::Builder::new()
            .name("hopdeck-video-portal".into())
            .spawn(move || run_pump(fd, node_id, pump_geometry, frames_tx, cmd_rx))
            .map_err(|e| crate::VideoError::PipeWire(e.to_string()))?;

        Ok(Self {
            geometry,
            frames: host_frames,
            cmd: cmd_tx,
            worker: Some(worker),
            _session: session,
            restore_token,
            max_fps,
        })
    }

    /// Negotiated frame size, `None` until the pump thread settled on a
    /// format (or the session died before negotiating).
    pub fn geometry(&self) -> Option<(u32, u32)> {
        *self.geometry.lock().expect("portal geometry lock")
    }

    /// Restore token issued for this session, if the portal granted
    /// persistence. Feed it back into [`Self::start`] to skip the dialog.
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }

    /// A fps-capped, latest-wins view of the capture. Activates the PipeWire
    /// stream; dropping the returned value pauses pumping again.
    pub fn subscribe(&self) -> PortalFrames {
        let _ = self.cmd.send(Cmd::Pump(true));
        PortalFrames::new(self.frames.subscribe(), self.cmd.clone(), self.max_fps)
    }

    /// Quits the pump thread and joins it. Idempotent; also runs on `Drop`.
    pub fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.cmd.send(Cmd::Quit);
            let _ = worker.join();
        }
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

// ------------------------------------------------------------------ //
// PipeWire pump thread                                                //
// ------------------------------------------------------------------ //

fn run_pump(
    fd: OwnedFd,
    node_id: u32,
    geometry: GeometrySlot,
    frames: watch::Sender<Option<VideoFrame>>,
    cmd_rx: pw::channel::Receiver<Cmd>,
) {
    pw::init();
    if let Err(e) = run_pump_inner(fd, node_id, geometry, frames, cmd_rx) {
        log::error!("video: portal pump thread failed: {e}");
    }
}

fn run_pump_inner(
    fd: OwnedFd,
    node_id: u32,
    geometry: GeometrySlot,
    frames: watch::Sender<Option<VideoFrame>>,
    cmd_rx: pw::channel::Receiver<Cmd>,
) -> Result<(), crate::VideoError> {
    let map_err = |e: pw::Error| crate::VideoError::PipeWire(e.to_string());

    // The refcounted variants all the way down: the pump on/off callback
    // attached to the loop must be 'static, so the stream it reaches cannot
    // borrow the core (`StreamBox<'c>`) — it has to own a refcount (`StreamRc`).
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(map_err)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(map_err)?;
    let core = context.connect_fd_rc(fd, None).map_err(map_err)?;

    // The stream object must stay reachable from loop-thread callbacks
    // (pump on/off): an Rc slot filled once the stream exists, before the
    // loop starts running.
    let stream_slot: Rc<RefCell<Option<pw::stream::StreamRc>>> = Rc::new(RefCell::new(None));
    let stop = {
        let mainloop_for_cb = mainloop.clone();
        let loop_ref = mainloop.loop_();
        let slot = Rc::clone(&stream_slot);
        cmd_rx.attach(loop_ref, move |cmd| match cmd {
            Cmd::Quit => mainloop_for_cb.quit(),
            Cmd::Pump(active) => {
                if let Some(stream) = slot.borrow().as_ref() {
                    if let Err(e) = stream.set_active(active) {
                        log::warn!("video: portal set_active({active}) failed: {e}");
                    }
                }
            }
        })
    };

    let stream = pw::stream::StreamRc::new(
        core,
        "hopdeck-video-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(map_err)?;

    struct PumpState {
        geometry: GeometrySlot,
        frames: watch::Sender<Option<VideoFrame>>,
        negotiated: RefCell<Option<(u32, u32)>>,
        counter: Cell<u64>,
    }

    let _listener = stream
        .add_local_listener_with_user_data(PumpState {
            geometry,
            frames,
            negotiated: RefCell::new(None),
            counter: Cell::new(0),
        })
        .param_changed(|_stream, state, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) = match parse_format(param) {
                Ok(parsed) => parsed,
                Err(e) => {
                    log::debug!("video: portal param is not a format: {e}");
                    return;
                }
            };
            if media_type != pipewire::spa::param::format::MediaType::Video
                || media_subtype != MediaSubtype::Raw
            {
                return;
            }
            let mut info = VideoInfoRaw::default();
            if let Err(e) = info.parse(param) {
                log::warn!("video: portal format parse failed: {e}");
                return;
            }
            if info.format() != VideoFormat::BGRx {
                log::error!(
                    "video: portal negotiated {:?} — only BGRx is supported, frames suppressed",
                    info.format()
                );
                return;
            }
            let (width, height) = (info.size().width, info.size().height);
            log::info!("video: portal capture negotiated {width}x{height} BGRx");
            *state.geometry.lock().expect("portal geometry lock") = Some((width, height));
            *state.negotiated.borrow_mut() = Some((width, height));
        })
        .process(|stream, state| {
            let (width, height) = {
                let negotiated = state.negotiated.borrow();
                let Some((width, height)) = *negotiated else {
                    return;
                };
                (width, height)
            };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let (offset, size, stride) = {
                let chunk = data.chunk();
                (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride(),
                )
            };
            let Some(raw) = data.data() else {
                return;
            };
            let start = offset.min(raw.len());
            let end = offset.saturating_add(size).min(raw.len());
            let Some(bgra) = bgra_from_bgrx(&raw[start..end], stride as usize, width, height)
            else {
                log::warn!(
                    "video: portal frame too small: {size}B @stride {stride} for {width}x{height}"
                );
                return;
            };
            let index = state.counter.get();
            state.counter.set(index + 1);
            match VideoFrame::new(width, height, index, bgra) {
                Ok(frame) => {
                    state.frames.send_replace(Some(frame));
                }
                Err(e) => log::warn!("video: portal frame rejected: {e}"),
            }
        })
        .register()
        .map_err(map_err)?;

    // Fixate the wire format: portal sources always offer BGRx; the size is
    // whatever the portal stream negotiates (no constraint here).
    let format_obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pipewire::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Id,
            VideoFormat::BGRx
        ),
    );
    let format_bytes = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format_obj),
    )
    .map_err(|e| crate::VideoError::PipeWire(e.to_string()))?
    .0
    .into_inner();
    let format_pod = Pod::from_bytes(&format_bytes).expect("serialized format pod is well-formed");
    let mut params = [format_pod];
    stream
        .connect(
            pipewire::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(map_err)?;

    // Reachable from the Cmd::Pump callback once the loop runs.
    *stream_slot.borrow_mut() = Some(stream);
    mainloop.run();
    drop(stop);
    Ok(())
}

/// Converts one BGRx shm frame (row pitch `stride`, pad byte ignored) into
/// the pipeline's tightly-packed BGRA with alpha forced opaque.
/// `raw` spans the chunk payload, not the whole mapped buffer.
fn bgra_from_bgrx(raw: &[u8], stride: usize, width: u32, height: u32) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || stride < w * 4 {
        return None;
    }
    let needed = stride * (h - 1) + w * 4;
    if raw.len() < needed {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let src = &raw[row * stride..row * stride + w * 4];
        let dst = &mut out[row * w * 4..(row + 1) * w * 4];
        for px in 0..w {
            dst[px * 4] = src[px * 4];
            dst[px * 4 + 1] = src[px * 4 + 1];
            dst[px * 4 + 2] = src[px * 4 + 2];
            dst[px * 4 + 3] = 255;
        }
    }
    Some(out)
}

// ------------------------------------------------------------------ //
// Consumer side: fps-capped latest-wins stream                        //
// ------------------------------------------------------------------ //

/// The armed `changed()` future of [`PortalFrames`], polled across calls.
type ChangedFuture = Pin<Box<dyn Future<Output = Result<(), watch::error::RecvError>> + Send>>;

/// A paced view of one portal capture. Dropping the last instance pauses
/// the underlying PipeWire stream (pump resumes on the next
/// [`PortalCapture::subscribe`]).
pub struct PortalFrames {
    rx: watch::Receiver<Option<VideoFrame>>,
    cmd: pw::channel::Sender<Cmd>,
    /// Drives `watch::Receiver::changed` across polls; owns its own receiver
    /// clone so it can be a stored, self-referential-free future.
    changed: Option<ChangedFuture>,
    min_interval: Duration,
    last: Option<Instant>,
    pending: bool,
    timer: Pin<Box<Sleep>>,
}

impl PortalFrames {
    fn new(
        rx: watch::Receiver<Option<VideoFrame>>,
        cmd: pw::channel::Sender<Cmd>,
        max_fps: u32,
    ) -> Self {
        Self {
            rx,
            cmd,
            changed: None,
            min_interval: Duration::from_nanos(1_000_000_000 / u64::from(max_fps.max(1))),
            last: None,
            pending: false,
            timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
        }
    }
}

impl Drop for PortalFrames {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Pump(false));
    }
}

impl Stream for PortalFrames {
    type Item = VideoFrame;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<VideoFrame>> {
        let this = &mut *self;
        loop {
            if this.changed.is_none() {
                let mut rx = this.rx.clone();
                this.changed = Some(Box::pin(async move { rx.changed().await }));
            }
            match ready!(
                this.changed
                    .as_mut()
                    .expect("armed above")
                    .as_mut()
                    .poll(cx)
            ) {
                Ok(()) => {
                    if this.rx.borrow().is_some() {
                        this.pending = true;
                    }
                    this.changed = None;
                }
                // Capture stopped: end the stream so the send loop winds down.
                Err(_) => return Poll::Ready(None),
            }
            if !this.pending {
                return Poll::Pending;
            }
            let due = this.last.map_or(Instant::now(), |t| t + this.min_interval);
            if Instant::now() < due {
                this.timer.as_mut().reset(due.into());
                ready!(this.timer.as_mut().poll(cx));
                continue;
            }
            let frame = this.rx.borrow_and_update().clone();
            this.pending = false;
            this.last = Some(Instant::now());
            if let Some(frame) = frame {
                return Poll::Ready(Some(frame));
            }
            // Reset to no-frame (capture restart); keep waiting for pixels.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgrx_conversion_handles_stride_and_alpha() {
        // 2x2 frames at stride 12: each row carries one padding byte column.
        let mut raw = vec![0u8; 12 * 2];
        for row in 0..2 {
            for col in 0..2 {
                let px = &mut raw[row * 12 + col * 4..][..4];
                px[0] = (row * 2 + col) as u8; // B
                px[1] = 10 + (row * 2 + col) as u8; // G
                px[2] = 20 + (row * 2 + col) as u8; // R
                px[3] = 0xEE; // pad — must not leak into the output
            }
        }
        let out = bgra_from_bgrx(&raw, 12, 2, 2).expect("sizes fit");
        assert_eq!(out.len(), 2 * 2 * 4);
        for px in 0..4 {
            assert_eq!(
                &out[px * 4..px * 4 + 3],
                &[px as u8, 10 + px as u8, 20 + px as u8]
            );
            assert_eq!(out[px * 4 + 3], 255, "alpha forced opaque");
        }
    }

    #[test]
    fn bgrx_conversion_rejects_short_frames() {
        assert!(bgra_from_bgrx(&[0u8; 7], 8, 2, 2).is_none());
        assert!(bgra_from_bgrx(&[0u8; 64], 8, 4, 2).is_none());
        assert!(bgra_from_bgrx(&[0u8; 64], 4, 0, 2).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn portal_frames_paces_to_max_fps_and_latest_wins() {
        let (tx, rx) = watch::channel(None);
        // Bypass the real pump thread: the pacing stream only needs the watch
        // and a command sender to talk to.
        let (cmd_tx, _cmd_rx) = pw::channel::channel::<Cmd>();
        let mut frames = PortalFrames::new(rx, cmd_tx, 30);

        use futures::StreamExt;

        // Three frames sent back-to-back inside one pacing window collapse
        // into the newest one.
        for index in 0..3u64 {
            tx.send_replace(Some(
                VideoFrame::new(4, 4, index, vec![0u8; 4 * 4 * 4]).unwrap(),
            ));
        }
        let first = frames.next().await.expect("first frame is immediate");
        assert_eq!(first.index, 2, "latest frame wins");

        // Within the 33ms window nothing else is due even if frames arrive.
        tx.send_replace(Some(
            VideoFrame::new(4, 4, 3, vec![0u8; 4 * 4 * 4]).unwrap(),
        ));
        tokio::time::advance(Duration::from_millis(10)).await;
        assert!(
            futures::poll!(frames.next()).is_pending(),
            "pacing window not elapsed yet"
        );

        tokio::time::advance(Duration::from_millis(25)).await;
        let second = frames.next().await.expect("due frame delivered");
        assert_eq!(second.index, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn portal_frames_ends_when_capture_stops() {
        let (tx, rx) = watch::channel(None);
        let (cmd_tx, _cmd_rx) = pw::channel::channel::<Cmd>();
        let mut frames = PortalFrames::new(rx, cmd_tx, 60);
        drop(tx);

        use futures::StreamExt;
        assert!(frames.next().await.is_none(), "dropped sender ends stream");
    }
}
