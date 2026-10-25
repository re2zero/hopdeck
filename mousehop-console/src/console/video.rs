//! Screen-view state (M3a increment ④). The daemon mirrors the video
//! channel through `FrontendEvent::VideoState`; the frame pixels ride
//! the local binary feed socket (`mousehop_ipc::video_feed`) — the JSON
//! channel never carries pixels — and the detail page paints the newest
//! frame through gpui's `img` element.
//!
//! Handoff between the blocking feed reader thread and the UI follows
//! the design's double-buffer shape (D33 §8): the reader overwrites one
//! shared buffer and bumps a revision, the UI copies at render pace and
//! skips rebuilds when the revision hasn't moved — the reader never
//! waits on the painter.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::RenderImage;
use mousehop_ipc::{ClientHandle, connect_video_feed, read_video_frame};

use super::Console;

/// Mirror of the daemon's `FrontendEvent::VideoState` payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct VideoStateMirror {
    pub handle: Option<ClientHandle>,
    pub viewing: bool,
    pub available: bool,
    pub width: u16,
    pub height: u16,
    pub max_fps: u16,
}

/// Wake-ups from the feed reader thread, pumped onto the UI executor.
pub(super) enum VideoTick {
    /// A fresh frame landed in the buffer — repaint.
    Frame,
    /// The reader thread exited (socket closed, daemon without video
    /// support, or the view was turned off).
    Ended,
}

#[derive(Default)]
struct FrameBuf {
    width: u32,
    height: u32,
    revision: u64,
    bgra: Option<Vec<u8>>,
}

pub(super) struct VideoView {
    pub state: VideoStateMirror,
    buffer: Arc<Mutex<FrameBuf>>,
    /// Stop flag of the live feed reader thread, if any.
    stop: Option<Arc<AtomicBool>>,
    /// Frames observed since the feed connected (display only — the
    /// daemon's counters stay on its side of the socket).
    pub frames: u64,
    /// The image painted by the last render plus the buffer revision it
    /// was built from; rebuilt only when the reader bumped the revision.
    /// Interior-mutable because the render helpers only get `&self`
    /// (same shape as `Console::log_gen_seen`).
    painted: RefCell<Option<(u64, Arc<RenderImage>)>>,
    tick_tx: Option<UnboundedSender<VideoTick>>,
}

impl VideoView {
    pub(super) fn new(tick_tx: UnboundedSender<VideoTick>) -> Self {
        Self {
            state: VideoStateMirror::default(),
            buffer: Arc::new(Mutex::new(FrameBuf::default())),
            stop: None,
            frames: 0,
            painted: RefCell::new(None),
            tick_tx: Some(tick_tx),
        }
    }

    /// Apply the daemon's mirror and reconcile the feed connection with
    /// the `viewing` flag: connect the reader when viewing starts, stop
    /// it (and drop the stale picture) when it ends.
    pub(super) fn daemon_state(&mut self, mirror: VideoStateMirror) {
        self.state = mirror;
        if self.state.viewing {
            if self.stop.is_none() {
                self.spawn_reader();
            }
        } else {
            self.stop_reader();
        }
    }

    /// The daemon connection died — nothing is on file anymore.
    pub(super) fn reset(&mut self) {
        self.stop_reader();
        self.state = VideoStateMirror::default();
    }

    fn stop_reader(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.frames = 0;
        self.buffer.lock().expect("video frame buffer lock").bgra = None;
        self.painted.borrow_mut().take();
    }

    fn spawn_reader(&mut self) {
        let Some(tx) = self.tick_tx.clone() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let buffer = Arc::clone(&self.buffer);
        let stop_for_thread = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("video-feed".into())
            .spawn(move || read_feed_loop(buffer, stop_for_thread, tx));
        match spawned {
            Ok(_handle) => self.stop = Some(stop),
            Err(e) => log::warn!("video feed reader spawn failed: {e}"),
        }
    }

    /// Feed reader reported its exit (not user-initiated): drop the
    /// stale picture so the panel never shows a frozen screen.
    pub(super) fn on_feed_ended(&mut self) {
        self.stop = None;
        self.buffer.lock().expect("video frame buffer lock").bgra = None;
        self.painted.borrow_mut().take();
    }

    /// The newest frame as a paintable image, or `None` while nothing
    /// has arrived or nothing changed since the last paint.
    pub(super) fn latest_image(&self) -> Option<Arc<RenderImage>> {
        let buf = self.buffer.lock().expect("video frame buffer lock");
        let bgra = buf.bgra.as_ref()?;
        if buf.revision == 0 {
            return None;
        }
        if let Some((revision, image)) = self.painted.borrow().as_ref() {
            if *revision == buf.revision {
                return Some(Arc::clone(image));
            }
        }
        // BGRA bytes ride as-is: gpui uploads its textures as BGRA8
        // (`RenderImage` documents BGRA), so the buffer must NOT be
        // channel-swapped into an "RGBA" image.
        let image = image::RgbaImage::from_raw(buf.width, buf.height, bgra.clone())?;
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(image)]));
        *self.painted.borrow_mut() = Some((buf.revision, Arc::clone(&image)));
        Some(image)
    }
}

/// Blocking reader for the daemon's video feed socket. Runs on its own
/// thread; the read timeout makes the stop flag observable within a
/// quarter-second, everything else exits via socket errors.
fn read_feed_loop(
    buffer: Arc<Mutex<FrameBuf>>,
    stop: Arc<AtomicBool>,
    tx: UnboundedSender<VideoTick>,
) {
    use std::io::ErrorKind;
    // The daemon binds the feed socket lazily on its first dial, so the
    // `viewing` mirror can beat the socket into existence — retry for a
    // grace window instead of giving up on the first ENOENT.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let stream = loop {
        match connect_video_feed() {
            Ok(stream) => break stream,
            Err(e) => {
                if stop.load(Ordering::Relaxed) || std::time::Instant::now() >= deadline {
                    log::warn!("video feed connect failed: {e}");
                    let _ = tx.unbounded_send(VideoTick::Ended);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };
    let mut stream = stream;
    if let Err(e) = stream.set_read_timeout(Some(std::time::Duration::from_millis(250))) {
        log::warn!("video feed read timeout setup failed: {e}");
    }
    while !stop.load(Ordering::Relaxed) {
        match read_video_frame(&mut stream) {
            Ok(frame) => {
                {
                    let mut buf = buffer.lock().expect("video frame buffer lock");
                    buf.width = u32::from(frame.width);
                    buf.height = u32::from(frame.height);
                    buf.revision += 1;
                    buf.bgra = Some(frame.bgra);
                }
                if tx.unbounded_send(VideoTick::Frame).is_err() {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(e) => {
                log::debug!("video feed closed: {e}");
                break;
            }
        }
    }
    let _ = tx.unbounded_send(VideoTick::Ended);
}

/// Create the tick channel and its UI pump task; called once from
/// `Console::new` so `VideoView` only ever holds the sender.
pub(super) fn spawn_tick_pump(cx: &mut gpui_kit::Context<Console>) -> UnboundedSender<VideoTick> {
    let (tx, mut rx) = unbounded::<VideoTick>();
    cx.spawn(async move |this, cx| {
        while let Some(tick) = rx.next().await {
            if this
                .update(cx, |this, cx| this.on_video_tick(tick, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
    tx
}
