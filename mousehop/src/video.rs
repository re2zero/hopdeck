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

    // ---------------------------------------------------------------- //
    // Controlled side: listener + sender                                //
    // ---------------------------------------------------------------- //

    pub(super) struct Host {
        port: u16,
        config: VideoConfig,
        /// Kills the accept loop and every send session.
        halt: CancellationToken,
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
                halt: CancellationToken::new(),
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
            spawn_local(accept_loop(listener, config, halt.clone()));
            Self { port, config, halt }
        }

        pub(super) fn offer(&self) -> Option<ProtoEvent> {
            if self.port == 0 {
                return None;
            }
            Some(ProtoEvent::VideoOffer {
                port: self.port,
                codec: mousehop_proto::VIDEO_CODEC_OPENH264,
                max_fps: self.config.max_fps,
                width: self.config.width,
                height: self.config.height,
            })
        }

        pub(super) fn halt(&mut self) {
            self.halt.cancel();
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

    async fn accept_loop(listener: DynListener, config: VideoConfig, halt: CancellationToken) {
        loop {
            tokio::select! {
                _ = halt.cancelled() => {
                    let _ = listener.close().await;
                    break;
                }
                // workaround for https://github.com/webrtc-rs/webrtc/issues/614
                _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                accepted = listener.accept() => match accepted {
                    Ok((conn, addr)) => start_send_session(conn, addr, &config, &halt),
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
        halt: &CancellationToken,
    ) {
        log::info!("video: authorized peer {addr} dialed the video channel — sending");
        let token = halt.child_token();
        spawn_local(send_loop(conn.clone(), addr, *config, token.clone()));
        spawn_local(read_loop(conn, addr, token));
    }

    async fn send_loop(
        conn: ArcConn,
        peer: SocketAddr,
        config: VideoConfig,
        token: CancellationToken,
    ) {
        let mut source = mv::capture::DummyVideoSource::new(
            u32::from(config.width),
            u32::from(config.height),
            u32::from(config.max_fps),
            None,
        );
        // IDR roughly every 2s so a mid-stream join recovers quickly.
        let keyframe_interval = u32::from(config.max_fps).saturating_mul(2).max(1);
        let mut encoder = match mv::codec::OpenH264Encoder::new(keyframe_interval) {
            Ok(encoder) => encoder,
            Err(e) => {
                log::warn!("video: encoder init failed: {e}");
                return;
            }
        };
        let mut packetizer = mv::transport::Packetizer::new();
        let start = Instant::now();
        let mut sent_frames = 0u64;
        log::info!(
            "video: streaming {}x{}@{}fps to {peer} (dummy source, increment ③ replaces it)",
            config.width,
            config.height,
            config.max_fps
        );
        loop {
            let frame = tokio::select! {
                _ = token.cancelled() => break,
                frame = source.next() => match frame {
                    Some(frame) => frame,
                    None => break,
                },
            };
            let encoded = match encoder.encode(&frame, false) {
                Ok(encoded) => encoded,
                Err(e) => {
                    log::warn!("video: encode failed: {e}");
                    break;
                }
            };
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

    /// Reads the peer→sender direction. v1 carries no reverse traffic (PLI
    /// lands here in increment ⑤); its real job today is observing the peer
    /// closing so the sender stops encoding into a dead channel.
    async fn read_loop(conn: ArcConn, peer: SocketAddr, token: CancellationToken) {
        let mut buf = vec![0u8; RECV_BUF_LEN];
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                received = conn.recv(&mut buf) => match received {
                    Ok(0) | Err(_) => {
                        log::debug!("video: {peer} closed the video channel");
                        break;
                    }
                    Ok(_) => {}
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

    pub(super) struct Peer {
        cert: Certificate,
        active: Option<ActiveReceive>,
    }

    impl Peer {
        pub(super) fn new(cert: Certificate) -> Self {
            Self { cert, active: None }
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
                    self.teardown("superseded by a newer offer");
                    let target = SocketAddr::new(addr.ip(), *port);
                    log::info!(
                        "video: peer offered {width}x{height}@{max_fps} on {target} — dialing"
                    );
                    let task = spawn_local(receive_loop(self.cert.clone(), target));
                    self.active = Some(ActiveReceive {
                        handle,
                        session,
                        task,
                    });
                }
                ProtoEvent::VideoStop if self.offers_from(handle, session) => {
                    self.teardown("peer withdrew the offer");
                }
                _ => {}
            }
        }

        pub(super) fn peer_gone(&mut self, handle: ClientHandle, session: ConnectionSession) {
            if self.offers_from(handle, session) {
                self.teardown("input session ended");
            }
        }

        fn offers_from(&self, handle: ClientHandle, session: ConnectionSession) -> bool {
            self.active
                .as_ref()
                .is_some_and(|active| active.handle == handle && active.session == session)
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
    /// task is aborted. v1 consumption is logging only — the console hookup
    /// is increment ④.
    async fn receive_loop(cert: Certificate, target: SocketAddr) {
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
            let access_unit = match reassembler.push(packet) {
                Ok(Some(access_unit)) => access_unit,
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("video: reassembly failed: {e}");
                    continue;
                }
            };
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
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("video: decode failed: {e}");
                    break;
                }
            }
        }
        let _ = conn.close().await;
        log::info!("video: receive channel from {target} closed after {decoded_frames} frames");
    }
}
