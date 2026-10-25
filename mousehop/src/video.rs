//! Daemon video channel (M3a increment ②b, design `.relay/DESIGN-m3a-video.md`).
//!
//! The controlled side ([`VideoHost`]) binds an independent UDP+DTLS
//! listener, announces it via `ProtoEvent::VideoOffer` on the input session,
//! and streams encoded frames to whichever authorized peer dials the video
//! port. The controlling side ([`VideoPeer`]) lives inside the capture state
//! machine, dials on `VideoOffer` and tears down on `VideoStop`/disconnect.
//!
//! The whole channel rides a dedicated socket (D5: video must never compete
//! with input on the DTLS event loop) and reuses the same certificate and
//! `authorized_fingerprints` table as the input listener. One deliberate
//! deviation from the input listener: **no access-password bypass**. The
//! video channel has no protocol-level challenge that could gate an admitted
//! peer, so letting password-mode peers through DTLS would hand the screen
//! stream to anyone holding a throwaway certificate — admission stays
//! fingerprint-strict.
//!
//! The module is always compiled; without the `video` feature both façades
//! swap to inert stubs with identical signatures so the integration sites in
//! `service.rs` / `capture.rs` stay cfg-free.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use mousehop_ipc::{ClientHandle, IncomingPeerConfig};
use mousehop_proto::ProtoEvent;
use webrtc_dtls::crypto::Certificate;

use crate::config::VideoConfig;
use crate::connect::ConnectionSession;

#[cfg(not(feature = "video"))]
mod imp {
    use super::{Certificate, ClientHandle, ConnectionSession, ProtoEvent, VideoConfig};
    use mousehop_ipc::IncomingPeerConfig;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, RwLock};

    /// No-op stand-in: video is compiled out, nothing binds or dials.
    pub(super) struct Host;

    impl Host {
        pub(super) async fn spawn(
            _cert: Certificate,
            _authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
            _config: VideoConfig,
        ) -> Self {
            Self
        }

        pub(super) fn offer(&self) -> Option<ProtoEvent> {
            None
        }

        pub(super) fn halt(&mut self) {}
    }

    /// No-op stand-in: every event is ignored.
    pub(super) struct Peer;

    impl Peer {
        pub(super) fn new(_cert: Certificate) -> Self {
            Self
        }

        pub(super) fn handle_proto(
            &mut self,
            _handle: ClientHandle,
            _session: ConnectionSession,
            _addr: SocketAddr,
            _event: &ProtoEvent,
        ) {
        }

        pub(super) fn peer_gone(&mut self, _handle: ClientHandle, _session: ConnectionSession) {}

        pub(super) fn set_view(&mut self, _handle: ClientHandle, _view: bool) {}

        pub(super) fn state(&self) -> super::VideoStateInfo {
            super::VideoStateInfo::default()
        }
    }
}

/// Controlled-side video sender. Construction never fails: a broken bind
/// logs and leaves video off, so a bad `[video]` section cannot take input
/// sharing down with it.
pub(crate) struct VideoHost(imp::Host);

impl VideoHost {
    pub(crate) async fn spawn(
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
        config: VideoConfig,
    ) -> Self {
        Self(imp::Host::spawn(cert, authorized_keys, config).await)
    }

    /// The `VideoOffer` to announce on a freshly established input session,
    /// or `None` while video is disabled.
    pub(crate) fn offer(&self) -> Option<ProtoEvent> {
        self.0.offer()
    }

    /// Stop every send session (input session died, service shutting down).
    pub(crate) fn halt(&mut self) {
        self.0.halt()
    }
}

/// Controlling-side video receiver, owned by the capture state machine.
pub(crate) struct VideoPeer(imp::Peer);

/// Mirror of the controlling-side video channel, as the frontend should
/// see it: which client's offer is on file, whether it is currently
/// being viewed, and the announced geometry. `Default` = nothing on
/// file, not viewing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct VideoStateInfo {
    pub handle: Option<ClientHandle>,
    pub viewing: bool,
    pub available: bool,
    pub width: u16,
    pub height: u16,
    pub max_fps: u16,
}

impl VideoPeer {
    pub(crate) fn new(cert: Certificate) -> Self {
        Self(imp::Peer::new(cert))
    }

    /// Feed a connection event; only `VideoOffer`/`VideoStop` are acted on.
    pub(crate) fn handle_proto(
        &mut self,
        handle: ClientHandle,
        session: ConnectionSession,
        addr: std::net::SocketAddr,
        event: &ProtoEvent,
    ) {
        self.0.handle_proto(handle, session, addr, event)
    }

    /// The input session for this peer ended — drop a matching channel.
    pub(crate) fn peer_gone(&mut self, handle: ClientHandle, session: ConnectionSession) {
        self.0.peer_gone(handle, session)
    }

    /// Start or stop viewing a client's shared screen (the console's
    /// picture toggle). Dialing waits for an offer from that client; a
    /// stored offer is dialed immediately. The preference persists
    /// across sessions so a re-offer re-dials automatically.
    pub(crate) fn set_view(&mut self, handle: ClientHandle, view: bool) {
        self.0.set_view(handle, view)
    }

    /// Current state for the frontend mirror.
    pub(crate) fn state(&self) -> VideoStateInfo {
        self.0.state()
    }
}

#[cfg(feature = "video")]
mod imp {
    use super::{Certificate, ClientHandle, IncomingPeerConfig, ProtoEvent, VideoConfig};
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    use futures::StreamExt;
    use mousehop_video as mv;
    use mousehop_video::codec::{VideoDecoder, VideoEncoder};
    use rustls::pki_types::CertificateDer;
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc;
    use tokio::task::{JoinHandle, spawn_local};
    use tokio_util::sync::CancellationToken;
    use webrtc_dtls::conn::DTLSConn;
    use webrtc_dtls::listener::listen;
    use webrtc_dtls::{
        Error as DtlsError,
        config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    };
    use webrtc_util::Conn;
    use webrtc_util::conn::Listener;

    use crate::config::VideoSourceKind;
    use crate::connect::ConnectionSession;
    use crate::crypto;

    type ArcConn = Arc<dyn Conn + Send + Sync>;
    type DynListener = Box<dyn Listener + Send + Sync>;
    type VerifyPeerCertificateFn = Arc<
        dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), DtlsError>) + Send + Sync,
    >;

    /// DTLS handshake budget for the receive-side dial (mirrors the input
    /// channel's connect timeout).
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
    /// Backoff after a failed accept so a dying listener cannot busy-loop.
    const ACCEPT_RETRY_BACKOFF: Duration = Duration::from_secs(1);
    /// Recv buffer for video datagrams (an MTU-framed packet is ≤1200B of
    /// payload; the slack covers DTLS record overhead with room to spare).
    const RECV_BUF_LEN: usize = 65536;
    /// Info-level progress interval on the receive side (frame counters).
    const RX_LOG_INTERVAL: u64 = 60;
    /// Poll cadence while a send session waits for the portal capture to
    /// come up (first run needs the user to pick a screen).
    const PORTAL_WAIT_RETRY: Duration = Duration::from_millis(300);
    /// Total budget for that wait before giving the dialing peer nothing.
    const PORTAL_WAIT_BUDGET: Duration = Duration::from_secs(8);
    /// Minimum spacing between keyframe requests to the sender (design §6:
    /// PLIs are fire-and-forget; the periodic IDR is the backstop anyway).
    const PLI_MIN_INTERVAL: Duration = Duration::from_millis(300);

    // ---------------------------------------------------------------- //
    // Portal capture glue                                               //
    // ---------------------------------------------------------------- //
    // Two same-signature variants keep Host/send_loop cfg-free: the real
    // one (Linux + `video_portal`) runs an eager mousehop-video
    // PortalCapture; everywhere else it degrades to "no capture" with a
    // clear log.

    #[cfg(all(target_os = "linux", feature = "video_portal"))]
    mod portal_glue {
        use std::sync::{Arc, Mutex};

        use mousehop_video::capture::VideoSource;
        use mousehop_video::capture::portal::PortalCapture;

        use super::{CancellationToken, VideoConfig, spawn_local};

        /// The eagerly-started capture; `None` until the portal session is
        /// authorized (first run waits for the user to pick a screen).
        pub type Slot = Arc<Mutex<Option<PortalCapture>>>;

        pub fn new_slot() -> Slot {
            Arc::new(Mutex::new(None))
        }

        pub fn spawn_setup(config: VideoConfig, slot: Slot, halt: CancellationToken) {
            spawn_local(async move {
                match PortalCapture::start(u32::from(config.max_fps), None).await {
                    Ok(capture) => {
                        // Restore-token persistence into [video] lands with a
                        // later increment; until then the picker shows every
                        // run and the token is only logged.
                        match capture.restore_token() {
                            Some(token) => log::info!(
                                "video: portal granted restore token {token} — \
                                 persistence comes with a later increment, so \
                                 expect one picker dialog per daemon run"
                            ),
                            None => log::info!(
                                "video: portal granted no restore token — the \
                                 picker will show again next run"
                            ),
                        }
                        if let Some((width, height)) = capture.geometry() {
                            log::info!("video: portal capture ready {width}x{height}");
                        }
                        *slot.lock().expect("portal slot lock") = Some(capture);
                    }
                    Err(e) => log::warn!(
                        "video: portal capture unavailable — screen sharing stays off: {e}"
                    ),
                }
                let _ = halt; // setup is best-effort; send sessions die via their own tokens
            });
        }

        /// A paced frame stream for a send session, activating the pump;
        /// `None` while the capture is not up yet.
        pub fn subscribe(slot: &Slot) -> Option<Box<dyn VideoSource>> {
            slot.lock()
                .expect("portal slot lock")
                .as_ref()
                .map(|capture| Box::new(capture.subscribe()) as Box<dyn VideoSource>)
        }

        /// Negotiated geometry for the `VideoOffer`; `None` while unknown.
        pub fn geometry(slot: &Slot) -> Option<(u16, u16)> {
            let (width, height) = slot
                .lock()
                .expect("portal slot lock")
                .as_ref()
                .and_then(|capture| capture.geometry())?;
            Some((u16::try_from(width).ok()?, u16::try_from(height).ok()?))
        }

        pub fn stop(slot: &Slot) {
            if let Some(capture) = slot.lock().expect("portal slot lock").as_mut() {
                capture.stop();
            }
        }
    }

    #[cfg(not(all(target_os = "linux", feature = "video_portal")))]
    mod portal_glue {
        use std::sync::{Arc, Mutex};

        use mousehop_video::capture::VideoSource;

        use super::{CancellationToken, VideoConfig};

        pub type Slot = Arc<Mutex<Option<()>>>;

        pub fn new_slot() -> Slot {
            Arc::new(Mutex::new(None))
        }

        pub fn spawn_setup(_config: VideoConfig, _slot: Slot, _halt: CancellationToken) {
            log::warn!(
                "video: [video] source = portal needs a Linux build with the \
                 `video_portal` feature — screen sharing stays off"
            );
        }

        pub fn subscribe(_slot: &Slot) -> Option<Box<dyn VideoSource>> {
            None
        }

        pub fn geometry(_slot: &Slot) -> Option<(u16, u16)> {
            None
        }

        pub fn stop(_slot: &Slot) {}
    }

    // ---------------------------------------------------------------- //
    // Controlled side: listener + sender                                //
    // ---------------------------------------------------------------- //

    pub(super) struct Host {
        port: u16,
        config: VideoConfig,
        /// Eager capture slot; only filled when `[video] source = portal`.
        portal: portal_glue::Slot,
        /// Kills the accept loop, every send session and the capture.
        halt: CancellationToken,
        /// Set by `halt`: the accept loop is gone for the process lifetime,
        /// so the announced port is dead — `offer` stops advertising it
        /// instead of promising pixels nothing will ever deliver.
        halted: bool,
    }

    impl Host {
        pub(super) async fn spawn(
            cert: Certificate,
            authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
            config: VideoConfig,
        ) -> Self {
            let disabled = Self {
                port: 0,
                config,
                portal: portal_glue::new_slot(),
                halt: CancellationToken::new(),
                halted: false,
            };
            if !config.enabled {
                return disabled;
            }
            let dtls_config = Config {
                certificates: vec![cert],
                extended_master_secret: ExtendedMasterSecretType::Require,
                client_auth: RequireAnyClientCert,
                verify_peer_certificate: Some(video_verify_peer(authorized_keys)),
                ..Default::default()
            };
            // A single wildcard bind keeps one port to announce. Dual-stack
            // first (a Linux v6 socket accepts v4-mapped flows by default),
            // falling back to v4-only where IPv6 is unavailable. Unlike the
            // per-IP input listener this can reply from a different source IP
            // on exotic multi-homed hosts — accepted v1 limitation, the input
            // channel keeps its per-IP binds.
            let listener: DynListener = match listen(
                SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), config.port),
                dtls_config.clone(),
            )
            .await
            {
                Ok(listener) => Box::new(listener),
                Err(v6_error) => {
                    log::debug!(
                        "video: [::]:{} bind failed ({v6_error}); trying 0.0.0.0",
                        config.port
                    );
                    match listen(
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), config.port),
                        dtls_config,
                    )
                    .await
                    {
                        Ok(listener) => Box::new(listener),
                        Err(v4_error) => {
                            log::warn!(
                                "video: no listener could bind (v6: {v6_error}, v4: {v4_error}) \
                                 — screen sharing disabled"
                            );
                            return disabled;
                        }
                    }
                }
            };
            let bound = match listener.addr().await {
                Ok(addr) => addr,
                Err(e) => {
                    log::warn!("video: bound listener reports no address: {e} — video disabled");
                    return disabled;
                }
            };
            let port = bound.port();
            if port == 0 {
                log::warn!("video: listener bound without a port — video disabled");
                return disabled;
            }
            log::info!("video: listening for DTLS on {bound}");
            let halt = CancellationToken::new();
            let portal = portal_glue::new_slot();
            if matches!(config.source, VideoSourceKind::Portal) {
                log::info!(
                    "video: capture source = portal (xdg-desktop-portal ScreenCast + PipeWire)"
                );
                portal_glue::spawn_setup(config, Arc::clone(&portal), halt.clone());
            }
            spawn_local(accept_loop(
                listener,
                config,
                Arc::clone(&portal),
                halt.clone(),
            ));
            Self {
                port,
                config,
                portal,
                halt,
                halted: false,
            }
        }

        pub(super) fn offer(&self) -> Option<ProtoEvent> {
            if self.port == 0 || self.halted {
                return None;
            }
            // Portal geometry is announced only once the pump negotiated a
            // format — sessions before that get no offer rather than a
            // promise of pixels we could not deliver (config width/height
            // stay dummy-source-only).
            let (width, height) = match self.config.source {
                VideoSourceKind::Portal => portal_glue::geometry(&self.portal)?,
                VideoSourceKind::Dummy => (self.config.width, self.config.height),
            };
            Some(ProtoEvent::VideoOffer {
                port: self.port,
                codec: mousehop_proto::VIDEO_CODEC_OPENH264,
                max_fps: self.config.max_fps,
                width,
                height,
            })
        }

        pub(super) fn halt(&mut self) {
            self.halted = true;
            self.halt.cancel();
            portal_glue::stop(&self.portal);
        }
    }

    /// Strict authorized-fingerprints check. Every accepted video connection
    /// is therefore authorized; there is no password bypass (module docs).
    fn video_verify_peer(
        authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
    ) -> VerifyPeerCertificateFn {
        Arc::new(
            move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                let fingerprint = certs
                    .iter()
                    .map(|c| crypto::generate_fingerprint(c))
                    .next()
                    .ok_or(DtlsError::ErrVerifyDataMismatch)?;
                if authorized_keys
                    .read()
                    .expect("lock")
                    .contains_key(&fingerprint)
                {
                    return Ok(());
                }
                log::info!("video: rejecting unauthorized peer {fingerprint} at the DTLS layer");
                Err(DtlsError::ErrVerifyDataMismatch)
            },
        )
    }

    async fn accept_loop(
        listener: DynListener,
        config: VideoConfig,
        portal: portal_glue::Slot,
        halt: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = halt.cancelled() => {
                    let _ = listener.close().await;
                    break;
                }
                // workaround for https://github.com/webrtc-rs/webrtc/issues/614
                _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                accepted = listener.accept() => match accepted {
                    Ok((conn, addr)) => start_send_session(conn, addr, &config, &portal, &halt),
                    Err(e) => {
                        if halt.is_cancelled() {
                            break;
                        }
                        log::warn!("video: accept: {e:?}");
                        tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                    }
                },
            }
        }
        log::debug!("video: accept loop stopped");
    }

    /// An authorized peer dialed the video port: it replaces any previous
    /// sender (single slot — the newest picture wins). The session's tasks
    /// clean up via their token (peer close observed by the read loop) or
    /// the global halt; no join bookkeeping is kept.
    fn start_send_session(
        conn: ArcConn,
        addr: SocketAddr,
        config: &VideoConfig,
        portal: &portal_glue::Slot,
        halt: &CancellationToken,
    ) {
        log::info!("video: authorized peer {addr} dialed the video channel — sending");
        let token = halt.child_token();
        // Reverse-direction traffic: the receiver's keyframe requests
        // arrive here and turn into a forced IDR in `send_loop`.
        let (pli_tx, pli_rx) = mpsc::unbounded_channel();
        spawn_local(send_loop(
            conn.clone(),
            addr,
            *config,
            Arc::clone(portal),
            token.clone(),
            pli_rx,
        ));
        spawn_local(read_loop(conn, addr, token, pli_tx));
    }

    async fn send_loop(
        conn: ArcConn,
        peer: SocketAddr,
        config: VideoConfig,
        portal: portal_glue::Slot,
        token: CancellationToken,
        mut pli_rx: mpsc::UnboundedReceiver<()>,
    ) {
        let (mut source, source_name): (Box<dyn mv::capture::VideoSource>, &'static str) =
            match config.source {
                VideoSourceKind::Dummy => (
                    Box::new(mv::capture::DummyVideoSource::new(
                        u32::from(config.width),
                        u32::from(config.height),
                        u32::from(config.max_fps),
                        None,
                    )),
                    "dummy",
                ),
                VideoSourceKind::Portal => {
                    // The capture may still be authorizing (first-run picker)
                    // — hold the session open briefly before giving up.
                    let mut waited = Duration::ZERO;
                    loop {
                        match portal_glue::subscribe(&portal) {
                            Some(source) => break (source, "portal"),
                            None if token.is_cancelled() || waited >= PORTAL_WAIT_BUDGET => {
                                log::warn!(
                                    "video: portal capture never came up for {peer} — \
                                     closing the video session"
                                );
                                return;
                            }
                            None => {
                                tokio::time::sleep(PORTAL_WAIT_RETRY).await;
                                waited += PORTAL_WAIT_RETRY;
                            }
                        }
                    }
                }
            };
        // IDR roughly every 2s so a mid-stream join recovers quickly.
        let keyframe_interval = u32::from(config.max_fps).saturating_mul(2).max(1);
        let mut encoder = match mv::codec::OpenH264Encoder::new(keyframe_interval) {
            Ok(encoder) => encoder,
            Err(e) => {
                log::warn!("video: encoder init failed: {e}");
                return;
            }
        };
        let mut packetizer = if config.parity {
            mv::transport::Packetizer::with_parity(mv::transport::PARITY_GROUP_SIZE)
        } else {
            mv::transport::Packetizer::new()
        };
        let start = Instant::now();
        let mut sent_frames = 0u64;
        log::info!(
            "video: streaming to {peer} (source: {source_name}, announced {}x{}@{}fps{})",
            config.width,
            config.height,
            config.max_fps,
            if config.parity {
                format!(
                    ", XOR parity every {} packets",
                    mv::transport::PARITY_GROUP_SIZE
                )
            } else {
                String::new()
            }
        );
        let mut force_key = false;
        loop {
            let frame = tokio::select! {
                _ = token.cancelled() => break,
                _ = pli_rx.recv() => {
                    if !force_key {
                        log::info!("video: keyframe requested by {peer} — next frame is an IDR");
                    }
                    force_key = true;
                    continue;
                }
                frame = source.next() => match frame {
                    Some(frame) => frame,
                    None => break,
                },
            };
            let encoded = match encoder.encode(&frame, force_key) {
                Ok(encoded) => encoded,
                Err(e) => {
                    log::warn!("video: encode failed: {e}");
                    break;
                }
            };
            force_key = false;
            let ts_ms = start.elapsed().as_millis() as u32;
            let packets = match packetizer.packetize(&encoded, ts_ms, mv::transport::DEFAULT_MTU) {
                Ok(packets) => packets,
                Err(e) => {
                    log::warn!("video: packetize failed: {e}");
                    break;
                }
            };
            let mut send_failed = false;
            for packet in packets {
                match packet.to_vec() {
                    Ok(bytes) => {
                        if let Err(e) = conn.send(&bytes).await {
                            log::warn!("video: send to {peer} failed: {e}");
                            send_failed = true;
                            break;
                        }
                    }
                    Err(e) => {
                        log::warn!("video: serialize failed: {e}");
                        send_failed = true;
                        break;
                    }
                }
            }
            if send_failed {
                break;
            }
            sent_frames += 1;
            if sent_frames == 1 {
                log::info!("video: first frame sent to {peer}");
            }
        }
        let _ = conn.close().await;
        log::info!("video: sender to {peer} stopped after {sent_frames} frames");
    }

    /// Reads the peer→sender direction: the receiver's keyframe requests
    /// (design §6 PLI) are forwarded to `send_loop`, everything else is
    /// ignored. Also observes the peer closing so the sender stops
    /// encoding into a dead channel.
    async fn read_loop(
        conn: ArcConn,
        peer: SocketAddr,
        token: CancellationToken,
        pli_tx: mpsc::UnboundedSender<()>,
    ) {
        let mut buf = vec![0u8; RECV_BUF_LEN];
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                received = conn.recv(&mut buf) => match received {
                    Ok(0) | Err(_) => {
                        log::debug!("video: {peer} closed the video channel");
                        break;
                    }
                    Ok(n) => {
                        if let Ok(packet) = mv::transport::Packet::from_bytes(&buf[..n]) {
                            if packet.flags & mv::transport::FLAG_PLI != 0
                                && pli_tx.send(()).is_err()
                            {
                                // Sender is gone; stop reading.
                                break;
                            }
                        }
                    }
                },
            }
        }
    }

    // ---------------------------------------------------------------- //
    // Controlling side: dialer + receiver                               //
    // ---------------------------------------------------------------- //

    struct ActiveReceive {
        handle: ClientHandle,
        session: ConnectionSession,
        task: JoinHandle<()>,
    }

    /// The newest `VideoOffer` on file. Offers arrive per session but
    /// v1 views a single screen: a fresher offer replaces whatever was
    /// stored, and nothing dials until the frontend's view toggle says
    /// so (the ②-era auto-dial is gone — see `set_view`).
    struct StoredOffer {
        handle: ClientHandle,
        session: ConnectionSession,
        target: SocketAddr,
        max_fps: u16,
        width: u16,
        height: u16,
    }

    pub(super) struct Peer {
        cert: Certificate,
        offer: Option<StoredOffer>,
        active: Option<ActiveReceive>,
        /// Per-client view preference from the frontend. Outlives
        /// sessions: with it on, every fresh offer dials immediately.
        views: HashMap<ClientHandle, bool>,
        /// Local feed the decoder pushes frames into for the frontend
        /// process. Serving starts lazily on the first dial (the
        /// capture-task context is required for `spawn_local`).
        feed: mousehop_ipc::VideoFeedHandle,
        feed_serving: bool,
    }

    impl Peer {
        pub(super) fn new(cert: Certificate) -> Self {
            Self {
                cert,
                offer: None,
                active: None,
                views: HashMap::new(),
                feed: mousehop_ipc::VideoFeedHandle::new(),
                feed_serving: false,
            }
        }

        pub(super) fn handle_proto(
            &mut self,
            handle: ClientHandle,
            session: ConnectionSession,
            addr: SocketAddr,
            event: &ProtoEvent,
        ) {
            match event {
                ProtoEvent::VideoOffer {
                    port,
                    codec,
                    max_fps,
                    width,
                    height,
                } => {
                    if *codec != mousehop_proto::VIDEO_CODEC_OPENH264 {
                        log::warn!("video: ignoring offer with unsupported codec {codec}");
                        return;
                    }
                    let target = SocketAddr::new(addr.ip(), *port);
                    let superseded = self.offer.as_ref().is_some_and(|old| old.target != target);
                    log::info!(
                        "video: offer on file from client {handle}: {width}x{height}@{max_fps} \
                         on {target}{}",
                        if superseded {
                            " (supersedes the previous)"
                        } else {
                            ""
                        }
                    );
                    self.offer = Some(StoredOffer {
                        handle,
                        session,
                        target,
                        max_fps: u16::from(*max_fps),
                        width: *width,
                        height: *height,
                    });
                    if self.views.get(&handle).copied().unwrap_or(false) {
                        self.dial_stored_offer();
                    } else {
                        log::debug!("video: view off — waiting for the frontend to ask");
                    }
                }
                ProtoEvent::VideoStop => {
                    let from_offer = self
                        .offer
                        .as_ref()
                        .is_some_and(|offer| offer.handle == handle && offer.session == session);
                    if from_offer {
                        self.offer = None;
                    }
                    if self.active_is_from(handle, session) || from_offer {
                        self.teardown("peer withdrew the offer");
                    }
                }
                _ => {}
            }
        }

        pub(super) fn peer_gone(&mut self, handle: ClientHandle, session: ConnectionSession) {
            let from_offer = self
                .offer
                .as_ref()
                .is_some_and(|offer| offer.handle == handle && offer.session == session);
            if from_offer {
                self.offer = None;
            }
            if self.active_is_from(handle, session) || from_offer {
                self.teardown("input session ended");
            }
        }

        pub(super) fn set_view(&mut self, handle: ClientHandle, view: bool) {
            self.views.insert(handle, view);
            if view {
                if self
                    .offer
                    .as_ref()
                    .is_some_and(|offer| offer.handle == handle)
                {
                    self.dial_stored_offer();
                } else {
                    log::debug!(
                        "video: view on for client {handle} — no offer yet, will dial on arrival"
                    );
                }
            } else if self
                .active
                .as_ref()
                .is_some_and(|active| active.handle == handle)
            {
                self.teardown("view toggled off");
            }
        }

        pub(super) fn state(&self) -> super::VideoStateInfo {
            match self.offer.as_ref() {
                Some(offer) => super::VideoStateInfo {
                    handle: Some(offer.handle),
                    viewing: self.active.is_some(),
                    available: true,
                    width: offer.width,
                    height: offer.height,
                    max_fps: offer.max_fps,
                },
                None => super::VideoStateInfo::default(),
            }
        }

        fn active_is_from(&self, handle: ClientHandle, session: ConnectionSession) -> bool {
            self.active
                .as_ref()
                .is_some_and(|active| active.handle == handle && active.session == session)
        }

        /// Dial the offer on file. No-op when it belongs to another
        /// client, none is stored, or a channel is already live (single
        /// slot — the newest picture wins).
        fn dial_stored_offer(&mut self) {
            let Some(offer) = self.offer.as_ref() else {
                return;
            };
            if self.active.is_some() {
                return;
            }
            if !self.views.get(&offer.handle).copied().unwrap_or(false) {
                return;
            }
            log::info!(
                "video: dialing {} for client {} ({}x{}@{}fps)",
                offer.target,
                offer.handle,
                offer.width,
                offer.height,
                offer.max_fps
            );
            if !self.feed_serving {
                self.feed_serving = true;
                let feed = self.feed.clone();
                spawn_local(feed.serve());
            }
            let feed = self.feed.clone();
            let task = spawn_local(receive_loop(self.cert.clone(), offer.target, feed));
            self.active = Some(ActiveReceive {
                handle: offer.handle,
                session: offer.session,
                task,
            });
        }

        fn teardown(&mut self, reason: &str) {
            if let Some(active) = self.active.take() {
                log::info!("video: receive channel torn down ({reason})");
                active.task.abort();
            }
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            if let Some(active) = self.active.take() {
                active.task.abort();
            }
        }
    }

    /// Dials the peer's video port and decodes until the channel dies or the
    /// task is aborted. Decoded frames go to the frontend's local video
    /// feed (increment ④); the log lines remain the E2E smoke's black-box
    /// signal.
    async fn receive_loop(
        cert: Certificate,
        target: SocketAddr,
        feed: mousehop_ipc::VideoFeedHandle,
    ) {
        // Bind family must match the target (see connect.rs `connect`).
        let bind_addr = match target {
            SocketAddr::V4(_) => "0.0.0.0:0",
            SocketAddr::V6(_) => "[::]:0",
        };
        let udp = match UdpSocket::bind(bind_addr).await {
            Ok(udp) => Arc::new(udp),
            Err(e) => {
                log::warn!("video: dial {target}: bind failed: {e}");
                return;
            }
        };
        if let Err(e) = udp.connect(target).await {
            log::warn!("video: dial {target}: connect failed: {e}");
            return;
        }
        let config = Config {
            certificates: vec![cert],
            server_name: "ignored".to_owned(),
            insecure_skip_verify: true,
            extended_master_secret: ExtendedMasterSecretType::Require,
            ..Default::default()
        };
        let conn: ArcConn =
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, DTLSConn::new(udp, config, true, None))
                .await
            {
                Ok(Ok(conn)) => Arc::new(conn),
                Ok(Err(e)) => {
                    log::warn!("video: DTLS handshake with {target} failed: {e}");
                    return;
                }
                Err(_) => {
                    log::warn!("video: DTLS handshake with {target} timed out");
                    return;
                }
            };
        log::info!("video: channel established with {target}");
        let mut reassembler = mv::transport::Reassembler::new();
        let mut decoder = match mv::codec::OpenH264Decoder::new() {
            Ok(decoder) => decoder,
            Err(e) => {
                log::warn!("video: decoder init failed: {e}");
                return;
            }
        };
        let mut buf = vec![0u8; RECV_BUF_LEN];
        let mut decoded_frames = 0u64;
        // Keyframe-request state (design §6): the reassembler counts
        // unrecoverable loss, the decoder's "no reference state" skips add
        // their own; requests coalesce into one PLI per rate-limit window.
        let mut pli_pending = false;
        let mut pli_seen_requests = reassembler.keyframe_requests();
        let mut last_pli: Option<Instant> = None;
        loop {
            let n = match conn.recv(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    log::debug!("video: recv from {target} ended: {e}");
                    break;
                }
            };
            if n == 0 {
                break;
            }
            let packet = match mv::transport::Packet::from_bytes(&buf[..n]) {
                Ok(packet) => packet,
                Err(e) => {
                    log::warn!("video: dropping malformed packet from {target}: {e}");
                    continue;
                }
            };
            let frames = match reassembler.push(packet) {
                Ok(frames) => frames,
                Err(e) => {
                    log::warn!("video: reassembly failed: {e}");
                    continue;
                }
            };
            let requests = reassembler.keyframe_requests();
            if requests != pli_seen_requests {
                pli_seen_requests = requests;
                pli_pending = true;
            }
            let mut decode_failed = false;
            for access_unit in frames {
                match decoder.decode(&access_unit) {
                    Ok(Some(frame)) => {
                        decoded_frames += 1;
                        // The dummy source embeds its frame counter in the pixels,
                        // so this reads back the *true* end-to-end sequence (D36:
                        // never trust protocol metadata for that). Portal sources
                        // in increment ③ carry no counter; this log line then just
                        // degrades to per-frame tracing.
                        let counter = mv::capture::dummy::read_pattern_index(&frame);
                        if decoded_frames == 1 {
                            log::info!(
                                "video: first frame decoded {}x{} counter={counter} (from {target})",
                                frame.width,
                                frame.height
                            );
                        } else if decoded_frames.is_multiple_of(RX_LOG_INTERVAL) {
                            log::info!(
                                "video: decoded {decoded_frames} frames, latest counter={counter}, \
                                 lost={} (from {target})",
                                reassembler.lost_frames()
                            );
                        } else {
                            log::debug!(
                                "video: frame counter={counter} {}x{}",
                                frame.width,
                                frame.height
                            );
                        }
                        // Forward to the console's picture panel; a geometry
                        // that doesn't fit the feed's u16 header can't render
                        // anyway, so it is dropped from the preview only.
                        match (u16::try_from(frame.width), u16::try_from(frame.height)) {
                            (Ok(width), Ok(height)) => {
                                feed.push(width, height, frame.index, &frame.bgra)
                            }
                            _ => log::debug!(
                                "video: frame {}x{} exceeds the feed's geometry range — \
                                 not sent to the frontend",
                                frame.width,
                                frame.height
                            ),
                        }
                    }
                    Ok(None) => {
                        // Pre-IDR join or post-loss poison: a non-keyframe
                        // is undecodable until an IDR arrives — ask for one
                        // instead of waiting out the periodic interval.
                        if !access_unit.keyframe {
                            pli_pending = true;
                        }
                    }
                    Err(e) => {
                        log::warn!("video: decode failed: {e}");
                        decode_failed = true;
                        break;
                    }
                }
            }
            if decode_failed {
                break;
            }
            if pli_pending && last_pli.is_none_or(|sent| sent.elapsed() >= PLI_MIN_INTERVAL) {
                match mv::transport::Packet::keyframe_request().to_vec() {
                    Ok(bytes) => match conn.send(&bytes).await {
                        Ok(_) => {
                            log::debug!("video: keyframe request (PLI) sent to {target}");
                            pli_pending = false;
                            last_pli = Some(Instant::now());
                        }
                        Err(e) => log::debug!("video: keyframe request to {target} failed: {e}"),
                    },
                    Err(e) => log::debug!("video: keyframe request serialize failed: {e}"),
                }
            }
        }
        let _ = conn.close().await;
        log::info!("video: receive channel from {target} closed after {decoded_frames} frames");
    }

    #[cfg(test)]
    mod host_halt {
        use super::*;

        /// A halted host must stop advertising its port: the accept loop is
        /// gone for the process lifetime, so a fresh input session would
        /// otherwise dial into a dead end (增量⑥, D38⑦).
        #[test]
        fn offer_stops_after_halt() {
            let mut host = Host {
                port: 4262,
                config: VideoConfig::default(),
                portal: portal_glue::new_slot(),
                halt: CancellationToken::new(),
                halted: false,
            };
            assert!(host.offer().is_some());
            host.halt();
            assert!(host.offer().is_none());
        }
    }
}
