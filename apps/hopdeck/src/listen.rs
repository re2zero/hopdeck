use futures::{Stream, StreamExt};
use hopdeck_ipc::{IncomingPeerConfig, TransportKind};
use hopdeck_proto::{
    CAP_ACCESS_PASSWORD, MAX_CLIPBOARD_SIZE, MAX_EVENT_SIZE, PROTOCOL_MAGIC, ProtoEvent,
    decode_clipboard_event, decode_display_layout_event, decode_fixed_event,
    encode_clipboard_event, encode_display_layout_event,
};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::MissedTickBehavior;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, spawn_local},
};
use tokio_rustls::TlsAcceptor;
use webrtc_dtls::{
    config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
    listener::listen,
};
use webrtc_util::{Conn, Error, conn::Listener};

use crate::{connect::handshake_allows_event, crypto, tcp_transport, tunnel};

/// Failed access-password attempts allowed per peer IP inside
/// [`AUTH_FAILURE_WINDOW`] before challenges are refused outright.
const MAX_AUTH_FAILURES: usize = 3;
/// Sliding window for the access-password failure counter.
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(30);
/// How long a peer may take to answer an open password challenge — manual
/// password entry on the dialing side — before the connection is closed.
const AUTH_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Idle-recv grace while a challenge is open (the peer stays silent until
/// the user submits the password; a normal `RECV_IDLE_TIMEOUT` would kill
/// the connection mid-entry).
const AUTH_CHALLENGE_IDLE: Duration = Duration::from_secs(150);

/// Session access-password gate for peers whose fingerprint is not in the
/// authorized table. When an access password is configured, such peers pass
/// the DTLS layer (their fingerprint is logged) but their `Accept` event is
/// withheld and the protocol-level challenge in `read_loop` must pass
/// before the session is surfaced above the listener. Authorized
/// fingerprints bypass the gate entirely; the authorized table itself is
/// never written by a password success (session-level admission only).
#[derive(Clone)]
struct AuthGate {
    /// Configured access password; `None` disables password auth.
    password: Arc<RwLock<Option<String>>>,
    authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
    /// Recent failed proof attempts per peer IP for rate limiting.
    failures: Rc<Mutex<HashMap<IpAddr, VecDeque<Instant>>>>,
}

impl AuthGate {
    fn enabled(&self) -> bool {
        self.password.read().expect("lock").is_some()
    }

    /// True when this fingerprint is in the persistent authorization
    /// table (regardless of whether password auth is on).
    fn is_authorized(&self, fingerprint: &str) -> bool {
        self.authorized_keys
            .read()
            .expect("lock")
            .contains_key(fingerprint)
    }

    /// True when this fingerprint must answer the challenge: password auth
    /// on, fingerprint not persistently authorized.
    fn needs_challenge(&self, fingerprint: &str) -> bool {
        self.enabled() && !self.is_authorized(fingerprint)
    }

    fn password(&self) -> Option<String> {
        self.password.read().expect("lock").clone()
    }

    fn is_rate_limited(&self, ip: IpAddr) -> bool {
        let mut failures = self.failures.lock().expect("lock");
        let Some(recent) = failures.get_mut(&ip) else {
            return false;
        };
        let cutoff = Instant::now() - AUTH_FAILURE_WINDOW;
        recent.retain(|at| *at > cutoff);
        recent.len() >= MAX_AUTH_FAILURES
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut failures = self.failures.lock().expect("lock");
        let recent = failures.entry(ip).or_default();
        let cutoff = Instant::now() - AUTH_FAILURE_WINDOW;
        recent.retain(|at| *at > cutoff);
        recent.push_back(Instant::now());
    }

    fn clear_failures(&self, ip: IpAddr) {
        self.failures.lock().expect("lock").remove(&ip);
    }
}

/// Random bytes for challenge material. Failure to gather entropy is a
/// process-fatal condition; panicking inside the read task takes the
/// session down with a clear message instead of admitting the peer with a
/// predictable challenge.
fn random_bytes(buf: &mut [u8]) {
    getrandom::fill(buf).expect("OS entropy source");
}

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
    #[error(transparent)]
    WebrtcDtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    #[error("no listener could be bound on any local address")]
    NoBoundListener,
}

type ArcConn = Arc<dyn Conn + Send + Sync>;
type DynListener = Box<dyn Listener + Send + Sync>;
pub(crate) type ListenerSession = u64;

#[derive(Clone)]
struct AcceptedConnection {
    session: ListenerSession,
    conn: ArcConn,
    /// Leaf certificate fingerprint captured at handshake time. Stored
    /// with the connection so the transport-agnostic session code never
    /// has to downcast into a DTLS handle again.
    fingerprint: String,
    /// Which transport carried this session (`Udp` DTLS or `Tcp` TLS).
    transport: TransportKind,
}

#[derive(Clone, Default)]
struct ListenerSessions {
    next: Rc<Cell<ListenerSession>>,
    current: Rc<RefCell<HashMap<SocketAddr, ListenerSession>>>,
}

impl ListenerSessions {
    fn allocate(&self, addr: SocketAddr) -> ListenerSession {
        let mut session = self.next.get().wrapping_add(1);
        if session == 0 {
            session = 1;
        }
        self.next.set(session);
        self.current.borrow_mut().insert(addr, session);
        session
    }

    fn is_current(&self, addr: SocketAddr, session: ListenerSession) -> bool {
        self.current.borrow().get(&addr).copied() == Some(session)
    }

    fn remove_if_current(&self, addr: SocketAddr, session: ListenerSession) -> bool {
        if !self.is_current(addr, session) {
            return false;
        }
        self.current.borrow_mut().remove(&addr);
        true
    }
}

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
        session: ListenerSession,
    },
    Accept {
        addr: SocketAddr,
        session: ListenerSession,
        fingerprint: String,
        /// Transport that carried the session, surfaced so the service
        /// can label the live session (UDP DTLS vs TCP TLS).
        transport: TransportKind,
    },
    Disconnected {
        addr: SocketAddr,
        session: ListenerSession,
    },
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct HopDeckListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: ListenerSessions,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
    /// Session access-password gate shared with every accept/read task.
    auth_gate: AuthGate,
    /// macOS-only: held for its `Drop` side effect (stops the
    /// CFRunLoop in the power-observer thread). The observer sends
    /// `()` into the wake channel on system-wake; the supervisor
    /// task drains that channel and force-closes peer conns so
    /// reconnect happens immediately after a screensaver/sleep
    /// dismissal instead of waiting out `RECV_IDLE_TIMEOUT`.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    power_observer: crate::macos_power::PowerObserver,
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

/// One local IP's listeners and the tasks that accept on them. Both
/// protocols share the same port (UDP DTLS and TCP TLS can coexist), so
/// a slot carries one accept task per transport; either may be absent
/// when its bind failed (TCP is best-effort and must never take UDP
/// down with it).
struct ListenerSlot {
    /// Background task that calls the DTLS listener's `accept()` in a
    /// loop and forwards events into the shared `listen_tx` / `conns`.
    udp_accept_task: Option<JoinHandle<()>>,
    /// Background task that accepts TCP connections and spawns a TLS
    /// handshake per connection. `None` when the TCP bind failed.
    tcp_accept_task: Option<JoinHandle<()>>,
}

impl ListenerSlot {
    fn udp_bound(&self) -> bool {
        self.udp_accept_task.is_some()
    }
}

impl Drop for ListenerSlot {
    fn drop(&mut self) {
        // Abort on `Drop` so dropping the supervisor (or removing an IP)
        // never leaves an accept loop running against a dead slot.
        if let Some(task) = self.udp_accept_task.take() {
            task.abort();
        }
        if let Some(task) = self.tcp_accept_task.take() {
            task.abort();
        }
    }
}

/// Everything the accept paths share: the event channel, the accepted-
/// connection table, the session counter, the rejected-fingerprint
/// queue and the TLS acceptor for the TCP side.
#[derive(Clone)]
struct AcceptContext {
    listen_tx: Sender<ListenEvent>,
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: ListenerSessions,
    connection_attempts: Arc<Mutex<VecDeque<String>>>,
    auth_gate: AuthGate,
    tls: TlsAcceptor,
}

impl AcceptContext {
    /// Register a freshly handshaked connection under its peer address.
    async fn register(
        &self,
        addr: SocketAddr,
        conn: &ArcConn,
        fingerprint: String,
        transport: TransportKind,
    ) {
        register_accepted_conn(
            &self.listen_tx,
            &self.conns,
            &self.sessions,
            addr,
            conn,
            &self.auth_gate,
            fingerprint,
            transport,
        )
        .await;
    }
}

impl HopDeckListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
        access_password: Arc<RwLock<Option<String>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (request_port_change, request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();
        let connection_attempts: Arc<Mutex<VecDeque<String>>> = Default::default();

        let auth_gate = AuthGate {
            password: access_password.clone(),
            authorized_keys: authorized_keys.clone(),
            failures: Default::default(),
        };

        let authorized = authorized_keys.clone();
        let verify_password = access_password.clone();
        let verify_peer_certificate: Option<VerifyPeerCertificateFn> = {
            let connection_attempts = connection_attempts.clone();
            Some(Arc::new(
                move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                    assert!(certs.len() == 1);
                    let fingerprints = certs
                        .iter()
                        .map(|c| crypto::generate_fingerprint(c))
                        .collect::<Vec<_>>();
                    if authorized
                        .read()
                        .expect("lock")
                        .contains_key(&fingerprints[0])
                    {
                        return Ok(());
                    }
                    let fingerprint = fingerprints.into_iter().next().expect("fingerprint");
                    // With an access password configured, unauthorized peers
                    // are let through the DTLS layer — the protocol-level
                    // challenge in `read_loop` gates the session instead. The
                    // fingerprint is logged so the operator can identify the
                    // caller; it never enters the authorized table here.
                    if verify_password.read().expect("lock").is_some() {
                        log::info!(
                            "access-password mode: admitted unauthorized peer {fingerprint} \
                             at the DTLS layer; protocol challenge follows"
                        );
                        return Ok(());
                    }
                    connection_attempts
                        .lock()
                        .expect("lock")
                        .push_back(fingerprint);
                    Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
                },
            ))
        };
        let cfg = Config {
            certificates: vec![cert.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            verify_peer_certificate,
            ..Default::default()
        };

        let conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>> =
            Rc::new(AsyncMutex::new(HashMap::new()));
        let sessions = ListenerSessions::default();
        // TLS 1.3 over TCP, same certificate and same post-handshake
        // fingerprint authorization as the DTLS side (chain validation is
        // impossible for a self-signed cert; identity is the pinned leaf).
        let tls = TlsAcceptor::from(Arc::new(tunnel::tls::server_config(&cert)?));
        let accept_ctx = AcceptContext {
            listen_tx: listen_tx.clone(),
            conns: conns.clone(),
            sessions: sessions.clone(),
            connection_attempts: connection_attempts.clone(),
            auth_gate: auth_gate.clone(),
            tls,
        };

        // Bind one listener per local address (v4 + v6, skipping
        // loopback / link-local / multicast) instead of a single
        // wildcard listener. With a single 0.0.0.0 / [::] bind on a
        // multi-homed host, replies use the kernel's preferred
        // outbound interface as source IP — which may not match the
        // IP the peer dialed, breaking DTLS 4-tuple matching.
        // Per-IP binds make replies symmetric automatically: each
        // listener's reply socket is bound to a specific IP, so the
        // kernel uses *that* IP as source.
        let initial_addrs = enumerate_listenable_addrs();
        if initial_addrs.is_empty() {
            // Fall back to 0.0.0.0 so we at least listen somewhere if
            // interface enumeration fails (very unusual).
            log::warn!("no listenable IP addresses found; falling back to 0.0.0.0");
        }
        let mut listeners: HashMap<IpAddr, ListenerSlot> = HashMap::new();
        let mut bound_count = 0usize;
        for ip in &initial_addrs {
            let slot = bind_ip(*ip, port, &cfg, &accept_ctx).await;
            if slot.udp_bound() {
                bound_count += 1;
            }
            listeners.insert(*ip, slot);
        }
        if bound_count == 0 {
            // Either enumeration returned no addrs, or every bind
            // failed. Try `0.0.0.0:port` as a last resort.
            let fallback = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
            let slot = bind_ip(fallback, port, &cfg, &accept_ctx).await;
            if !slot.udp_bound() {
                // Re-run just the DTLS bind so the caller gets the actual
                // cause (port in use, missing permission, …) instead of a
                // generic "no listener" message.
                return match try_bind_listener(fallback, port, &cfg).await {
                    Ok(_) => Err(ListenerCreationError::NoBoundListener),
                    Err(e) => Err(e),
                };
            }
            listeners.insert(fallback, slot);
            log::warn!(
                "listening on {fallback}:{port} (fallback — symmetric replies not guaranteed)"
            );
        }

        // macOS wake → force-close-all-conns plumbing. On non-macOS
        // the receiver is `None` and the supervisor's wake arm stays
        // permanently pending — no behavior change there.
        #[cfg(target_os = "macos")]
        let (power_observer, wake_rx) = {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let observer = crate::macos_power::PowerObserver::spawn(tx).await;
            (observer, Some(rx))
        };
        #[cfg(not(target_os = "macos"))]
        let wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;

        let listen_task = spawn_supervisor_task(
            port,
            cfg,
            listeners,
            accept_ctx,
            request_port_change_rx,
            port_changed_tx,
            wake_rx,
        );

        Ok(Self {
            conns,
            sessions,
            listen_rx,
            listen_tx,
            listen_task,
            port_changed,
            request_port_change,
            auth_gate,
            #[cfg(target_os = "macos")]
            power_observer,
        })
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        self.request_port_change.send(port).expect("channel closed");
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed.recv().await.expect("channel closed")
    }

    pub(crate) fn current_session(&self, addr: SocketAddr) -> Option<ListenerSession> {
        self.sessions.current.borrow().get(&addr).copied()
    }

    /// Register an outbound-dialed transport (reverse-connect) as
    /// if it had been accepted inbound: from here the listener's read
    /// loop, heartbeat and reply path own it exactly like an accept.
    /// Used by the controlled side of a reverse-mode client, whose
    /// dialed connection must serve the *injection* role even though
    /// it was established by dialing. The peer still has to complete
    /// the protocol `Hello` handshake like any inbound session.
    pub(crate) async fn adopt(
        &self,
        peer_addr: SocketAddr,
        conn: ArcConn,
        fingerprint: String,
        transport: TransportKind,
    ) {
        log::info!(
            "adopting reverse-dialed {transport} connection to {peer_addr} as inbound session"
        );
        register_accepted_conn(
            &self.listen_tx,
            &self.conns,
            &self.sessions,
            peer_addr,
            &conn,
            &self.auth_gate,
            fingerprint,
            transport,
        )
        .await;
    }

    /// Take a still-current accepted connection out of the listener's
    /// table and hand it to the caller — the controller half of
    /// reverse-connect, where an inbound session from a reverse-mode
    /// client becomes the capture send path. The session's read loop
    /// ends on its own (its table entry is gone) without publishing a
    /// `Disconnected`, so callers that retire emulation-side
    /// bookkeeping must do so themselves.
    pub(crate) async fn take_conn(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
    ) -> Option<ArcConn> {
        let conn = {
            let mut conns_guard = self.conns.lock().await;
            match conns_guard.get(&addr) {
                Some(slot) if slot.session == session => {
                    conns_guard.remove(&addr).map(|slot| slot.conn)
                }
                _ => None,
            }
        };
        if conn.is_some() {
            self.sessions.remove_if_current(addr, session);
        }
        conn
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let conns = self.conns.lock().await;
        for slot in conns.values() {
            let _ = slot.conn.close().await;
        }
        self.listen_tx.close();
    }

    pub(crate) async fn reply(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
        event: ProtoEvent,
    ) {
        log::trace!("reply {event} >=>=>=>=>=> {addr} session {session}");
        let bytes_owned = match &event {
            ProtoEvent::Clipboard { .. } => encode_clipboard_event(&event),
            ProtoEvent::DisplayLayout { .. } => encode_display_layout_event(&event),
            _ => Ok(Vec::new()),
        };
        let bytes_owned = match bytes_owned {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            Ok(_) => None,
            Err(e) => {
                log::warn!("dropping invalid reply to {addr}: {e}");
                return;
            }
        };
        let bytes_fixed: ([u8; MAX_EVENT_SIZE], usize) = if bytes_owned.is_some() {
            ([0u8; MAX_EVENT_SIZE], 0)
        } else {
            event.into()
        };
        let buf: &[u8] = if let Some(bytes) = bytes_owned.as_deref() {
            bytes
        } else {
            &bytes_fixed.0[..bytes_fixed.1]
        };
        let conn = self
            .conns
            .lock()
            .await
            .get(&addr)
            .filter(|slot| slot.session == session)
            .map(|slot| slot.conn.clone());
        if let Some(conn) = conn {
            let _ = conn.send(buf).await;
        }
    }

    pub(crate) async fn get_certificate_fingerprint(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
    ) -> Option<String> {
        // Recorded once at registration, on both transports, so this is a
        // plain table lookup — no transport downcast.
        self.conns
            .lock()
            .await
            .get(&addr)
            .filter(|slot| slot.session == session)
            .map(|slot| slot.fingerprint.clone())
    }
}

impl Stream for HopDeckListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            match self.listen_rx.poll_next_unpin(cx) {
                std::task::Poll::Ready(Some(event)) => {
                    let current = listen_event_is_current(&self.sessions, &event);
                    if current {
                        return std::task::Poll::Ready(Some(event));
                    }
                    log::debug!("dropping queued event from a replaced inbound session");
                }
                other => return other,
            }
        }
    }
}

fn listen_event_is_current(sessions: &ListenerSessions, event: &ListenEvent) -> bool {
    match event {
        ListenEvent::Msg { addr, session, .. } | ListenEvent::Accept { addr, session, .. } => {
            sessions.is_current(*addr, *session)
        }
        // The read loop removes the current token before it publishes this
        // terminal event. Accept it only while no replacement generation has
        // appeared.
        ListenEvent::Disconnected { addr, .. } => sessions.current.borrow().get(addr).is_none(),
        ListenEvent::Rejected { .. } => true,
    }
}

/// Whether an address is worth binding a DTLS listener to: a remote
/// peer must be able to reach it. Excludes loopback, link-local
/// (needs a scope id we don't carry), unspecified, and multicast.
/// Used by both the initial enumeration and the live `IfEvent::Up`
/// filter so the two stay in lockstep on what counts as listenable.
fn is_listenable_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local(),
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                // fe80::/10 — link-local; reachable only with a
                // scope id which neither our mDNS records nor the
                // peer config carry, so a bind that succeeds still
                // can't accept connections from a remote peer.
                && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// Enumerate local addresses suitable for binding a DTLS listener.
/// Returns both IPv4 and IPv6 addresses; `is_listenable_addr` drops
/// the ones a remote peer cannot reach (loopback, link-local,
/// unspecified, multicast).
fn enumerate_listenable_addrs() -> Vec<IpAddr> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("get_if_addrs failed: {e}");
            return Vec::new();
        }
    };
    ifaces
        .into_iter()
        .map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) => IpAddr::V4(v4.ip),
            if_addrs::IfAddr::V6(v6) => IpAddr::V6(v6.ip),
        })
        .filter(|ip| is_listenable_addr(*ip))
        .collect()
}

async fn try_bind_listener(
    ip: IpAddr,
    port: u16,
    cfg: &Config,
) -> Result<DynListener, ListenerCreationError> {
    let addr = SocketAddr::new(ip, port);
    let listener = listen(addr, cfg.clone()).await?;
    Ok(Box::new(listener))
}

/// Bind both transports on one local address. UDP+DTLS is the primary
/// path and is required (the caller counts successful UDP binds); TCP is
/// best-effort — a port collision or missing permission must never take
/// the UDP listener down with it, because `transport = udp` peers depend
/// on UDP alone.
async fn bind_ip(ip: IpAddr, port: u16, cfg: &Config, ctx: &AcceptContext) -> ListenerSlot {
    let addr = SocketAddr::new(ip, port);
    let udp_accept_task = match try_bind_listener(ip, port, cfg).await {
        Ok(listener) => {
            log::info!("listening for DTLS on {ip}:{port}");
            Some(spawn_accept_task(listener, ctx.clone()))
        }
        Err(e) => {
            log::warn!("failed to bind DTLS listener on {ip}:{port}: {e}");
            None
        }
    };
    let tcp_accept_task = match TcpListener::bind(addr).await {
        Ok(listener) => {
            log::info!("listening for TLS over TCP on {ip}:{port}");
            Some(spawn_tcp_accept_task(listener, ctx.clone()))
        }
        Err(e) => {
            log::warn!(
                "failed to bind TCP listener on {ip}:{port}: {e} — TCP fallback unavailable \
                 on this address"
            );
            None
        }
    };
    ListenerSlot {
        udp_accept_task,
        tcp_accept_task,
    }
}

/// Spawn an accept loop for one bound DTLS listener. Each accepted
/// `(conn, addr)` is registered in the shared `conns` map and an
/// `Accept` event is published (withheld while the access-password
/// challenge is pending). Verify-peer-certificate failures are
/// re-published as `Rejected` so the UI can surface unauthorized
/// fingerprints. The accept loop exits when the listener errors
/// out or its task is aborted (interface went down, port changed).
fn spawn_accept_task(listener: DynListener, ctx: AcceptContext) -> JoinHandle<()> {
    spawn_local(async move {
        loop {
            // Do not wrap this in `select!`/`timeout`: webrtc-dtls runs the
            // entire DTLS handshake inside `DTLSListener::accept()`. Cancelling
            // the future mid-handshake drops its state (the peer's ClientHello
            // is consumed, but no reply is ever sent), so every inbound
            // connection from a peer that doesn't complete within the deadline
            // dies silently.
            match listener.accept().await {
                Ok((conn, addr)) => {
                    // The DTLS handle is concrete here, so this is the one
                    // place the peer certificate is read on the UDP path; it
                    // then travels with the connection as plain data.
                    let Some(fingerprint) = dtls_peer_fingerprint(&conn).await else {
                        log::warn!("DTLS peer {addr} presented no certificate — closing");
                        let _ = conn.close().await;
                        continue;
                    };
                    ctx.register(addr, &conn, fingerprint, TransportKind::Udp)
                        .await;
                }
                Err(e) => {
                    if let Error::Std(ref se) = e {
                        if let Some(de) = se.0.downcast_ref::<webrtc_dtls::Error>() {
                            match de {
                                webrtc_dtls::Error::ErrVerifyDataMismatch => {
                                    if let Some(fingerprint) =
                                        ctx.connection_attempts.lock().expect("lock").pop_front()
                                    {
                                        ctx.listen_tx
                                            .send(ListenEvent::Rejected { fingerprint })
                                            .expect("channel closed");
                                    }
                                }
                                _ => log::warn!("accept: {de}"),
                            }
                        } else {
                            log::warn!("accept: {se:?}");
                        }
                    } else {
                        log::warn!("accept: {e:?}");
                    }
                }
            }
        }
    })
}

/// Leaf certificate fingerprint of an accepted DTLS connection.
async fn dtls_peer_fingerprint(conn: &ArcConn) -> Option<String> {
    let dtls: &DTLSConn = conn.as_any().downcast_ref()?;
    let state = dtls.connection_state().await;
    state
        .peer_certificates
        .first()
        .map(|cert| crypto::generate_fingerprint(cert))
}

/// TLS handshake ceiling for one accepted TCP connection. Generous — the
/// handshake happens on a dedicated task, so a stalled client costs
/// nothing but its own resource.
const TCP_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the accept loop for one bound TCP listener. Each connection gets
/// its own task for the TLS handshake, so one slow or malicious client
/// cannot stall the other accepts (the UDP accept loop is serialized by
/// webrtc-dtls and deliberately *not* cancelled — see `spawn_accept_task`).
fn spawn_tcp_accept_task(listener: TcpListener, ctx: AcceptContext) -> JoinHandle<()> {
    spawn_local(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    spawn_local(accept_tcp_connection(stream, addr, ctx.clone()));
                }
                Err(e) => {
                    // A transient accept error (EMFILE, …) must not spin the
                    // loop; back off briefly. The task itself is aborted on
                    // interface/port changes.
                    log::warn!("TCP accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    })
}

/// Complete one inbound TCP session: TLS handshake, fingerprint check,
/// then hand the transport to the shared registration path.
///
/// Authorization mirrors the DTLS semantics exactly:
///   * authorized fingerprint → register (Accept published);
///   * unauthorized with an access password configured → register with
///     the `Accept` withheld so `read_loop` can run the challenge;
///   * unauthorized without one → publish `Rejected` (the
///     `ConnectionAttempt` the UI shows) and close without registering.
async fn accept_tcp_connection(stream: TcpStream, addr: SocketAddr, ctx: AcceptContext) {
    if let Err(e) = stream.set_nodelay(true) {
        log::debug!("{addr}: TCP_NODELAY failed: {e}");
    }
    let stream = match tokio::time::timeout(TCP_TLS_HANDSHAKE_TIMEOUT, ctx.tls.accept(stream)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            log::debug!("TCP TLS handshake from {addr} failed: {e}");
            return;
        }
        Err(_) => {
            log::warn!("TCP TLS handshake from {addr} timed out — closing");
            return;
        }
    };
    let Some(fingerprint) = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(tunnel::tls::peer_fingerprint)
    else {
        log::warn!("TCP peer {addr} presented no certificate — closing");
        return;
    };
    if !ctx.auth_gate.is_authorized(&fingerprint) && !ctx.auth_gate.enabled() {
        log::info!("refusing unauthorized TCP peer {addr} ({fingerprint})");
        ctx.listen_tx
            .send(ListenEvent::Rejected { fingerprint })
            .expect("channel closed");
        // Dropping the stream closes the connection without ever
        // entering the protocol layer.
        return;
    }
    let conn: ArcConn = match tcp_transport::server_conn(stream) {
        Ok(conn) => tcp_transport::into_conn(conn),
        Err(e) => {
            log::warn!("TCP peer {addr}: could not read socket addresses: {e}");
            return;
        }
    };
    ctx.register(addr, &conn, fingerprint, TransportKind::Tcp)
        .await;
}

/// Shared registration path for a freshly handshaked inbound
/// connection, on either transport. Used by the DTLS accept loop, the
/// TCP accept task and [`HopDeckListener::adopt`] (reverse-dialed
/// transports). Allocates the session generation, replaces any prior
/// connection from the same peer address, publishes `Accept` — unless
/// the peer owes the access-password challenge, in which case the accept
/// is withheld until `read_loop` verifies it — and starts the read loop.
///
/// `fingerprint` and `transport` are passed in rather than recovered from
/// the connection: the DTLS handle is not the only transport anymore, and
/// downcasting would panic on a TCP session.
#[allow(clippy::too_many_arguments)]
async fn register_accepted_conn(
    listen_tx: &Sender<ListenEvent>,
    conns: &Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: &ListenerSessions,
    addr: SocketAddr,
    conn: &ArcConn,
    auth_gate: &AuthGate,
    fingerprint: String,
    transport: TransportKind,
) {
    log::info!("{transport} client connected, ip: {addr}");
    let session = sessions.allocate(addr);
    let replaced = {
        let mut conns_guard = conns.lock().await;
        conns_guard.insert(
            addr,
            AcceptedConnection {
                session,
                conn: conn.clone(),
                fingerprint: fingerprint.clone(),
                transport,
            },
        )
    };
    if let Some(replaced) = replaced {
        log::info!(
            "replacing prior {} session {} for reused peer address {addr}",
            replaced.transport,
            replaced.session
        );
        let _ = replaced.conn.close().await;
    }
    // A peer that reconnects gets a new source port (TCP always, UDP
    // usually), so the address-keyed table alone would keep the previous
    // session alive until its read loop notices the close or hits the
    // idle timeout — and both sessions could inject input in the
    // meantime. One certificate fingerprint is one peer: the newest
    // authenticated session supersedes every older one.
    evict_peer_sessions(conns, sessions, addr, session, &fingerprint, listen_tx).await;
    if auth_gate.needs_challenge(&fingerprint) {
        log::info!(
            "withholding session for {addr} (unauthorized peer {fingerprint}) \
             pending the access-password challenge"
        );
    } else {
        listen_tx
            .send(ListenEvent::Accept {
                addr,
                session,
                fingerprint: fingerprint.clone(),
                transport,
            })
            .expect("channel closed");
    }
    spawn_local(read_loop(
        conns.clone(),
        sessions.clone(),
        addr,
        session,
        conn.clone(),
        listen_tx.clone(),
        fingerprint,
        transport,
        auth_gate.clone(),
    ));
}

/// Close and forget every *other* live session belonging to the same
/// peer fingerprint. Called right after a new session is registered, so
/// at most one authenticated session per peer exists in the listener
/// table.
///
/// Eviction removes the entry under the connection table's lock (a stale
/// read loop must not be able to evict the survivor later), then closes
/// the connection and publishes `Disconnected` for the evicted address
/// so emulation-side bookkeeping is cleaned up immediately rather than
/// when the old read loop eventually wakes.
async fn evict_peer_sessions(
    conns: &Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: &ListenerSessions,
    keep_addr: SocketAddr,
    keep_session: ListenerSession,
    fingerprint: &str,
    listen_tx: &Sender<ListenEvent>,
) {
    let evicted = {
        let mut guard = conns.lock().await;
        let stale: Vec<(SocketAddr, ListenerSession)> = guard
            .iter()
            .filter(|(addr, slot)| {
                **addr != keep_addr
                    && slot.session != keep_session
                    && slot.fingerprint == fingerprint
            })
            .map(|(addr, slot)| (*addr, slot.session))
            .collect();
        let mut removed = Vec::with_capacity(stale.len());
        for (old_addr, old_session) in stale {
            if let Some(slot) = remove_connection_if_current(&mut guard, old_addr, old_session) {
                sessions.remove_if_current(old_addr, old_session);
                removed.push((old_addr, slot));
            }
        }
        removed
    };
    for (old_addr, slot) in evicted {
        log::info!(
            "closing superseded {} session {} from the same peer {fingerprint} at {old_addr}",
            slot.transport,
            slot.session
        );
        let _ = slot.conn.close().await;
        let _ = listen_tx.send(ListenEvent::Disconnected {
            addr: old_addr,
            session: slot.session,
        });
    }
}

/// Supervisor task: owns the set of active listeners, watches for
/// interface up/down events via `if_watch`, and rebuilds listeners
/// on port change. Each listener slot is keyed by its local IPv4
/// address and holds both transports; on `IfEvent::Up` we add a slot,
/// on `IfEvent::Down` we drop one (which aborts its accept tasks).
fn spawn_supervisor_task(
    initial_port: u16,
    cfg: Config,
    initial_listeners: HashMap<IpAddr, ListenerSlot>,
    ctx: AcceptContext,
    mut request_port_change_rx: Receiver<u16>,
    port_changed_tx: Sender<Result<u16, ListenerCreationError>>,
    mut wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
) -> JoinHandle<()> {
    spawn_local(async move {
        let mut port = initial_port;
        let mut listeners = initial_listeners;
        // The accept paths need the shared connection table for the
        // post-wake force-close; everything else lives in `ctx`.
        let conns = ctx.conns.clone();
        let mut watcher = match if_watch::tokio::IfWatcher::new() {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!(
                    "if_watch::IfWatcher::new failed: {e}; interface plug/unplug \
                     will not be detected (restart hopdeck to pick up new addrs)"
                );
                None
            }
        };
        // Periodic reconciliation: enumerate live IPs and diff against
        // `listeners`. Network.framework on macOS doesn't reliably fire
        // `IfEvent::Down` when an interface is administratively
        // disabled (e.g. user toggles Wi-Fi off in System Settings),
        // leaving stale slots bound to vanished IPs that no traffic
        // can reach. Polling every 30s catches whatever if-watch
        // misses — both adds (covers missed Up events too) and drops.
        // `Skip` so a long suspend (laptop closed for hours) doesn't
        // burst-fire backlog ticks at resume.
        let mut reconcile_tick = tokio::time::interval(Duration::from_secs(30));
        reconcile_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the immediate-first tick — we just enumerated at startup
        // and don't want to thrash listeners on the first iteration.
        reconcile_tick.tick().await;
        loop {
            tokio::select! {
                _ = reconcile_tick.tick() => {
                    let current_ips: HashSet<IpAddr> =
                        enumerate_listenable_addrs().into_iter().collect();
                    let to_drop: Vec<IpAddr> = listeners
                        .keys()
                        .filter(|ip| !current_ips.contains(*ip))
                        .copied()
                        .collect();
                    for ip in to_drop {
                        // `to_drop` was just collected from
                        // `listeners.keys()` and we run single-
                        // threaded, so the remove always returns Some.
                        listeners.remove(&ip);
                        log::info!(
                            "reconcile: dropping stale listener on {ip}:{port} \
                             (IP no longer present on any interface)"
                        );
                    }
                    // `bind_ip` is async, so `entry().or_insert_with(...)`
                    // doesn't fit — we only want to insert on UDP bind
                    // success. Match the `Entry::Vacant` slot up front so
                    // the same hash lookup covers both the existence check
                    // and the later insert, satisfying clippy::map_entry.
                    for ip in current_ips {
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            listeners.entry(ip)
                        {
                            let bound = bind_ip(ip, port, &cfg, &ctx).await;
                            if bound.udp_bound() {
                                slot.insert(bound);
                                log::info!(
                                    "reconcile: now listening on {ip}:{port} \
                                     (IP appeared without an Up event)"
                                );
                            }
                        }
                    }
                }
                ev = async {
                    match watcher.as_mut() {
                        Some(w) => w.select_next_some().await,
                        None => std::future::pending().await,
                    }
                } => match ev {
                    Ok(if_watch::IfEvent::Up(net)) => {
                        let ip = net.addr();
                        if is_listenable_addr(ip) && !listeners.contains_key(&ip) {
                            let bound = bind_ip(ip, port, &cfg, &ctx).await;
                            if bound.udp_bound() {
                                listeners.insert(ip, bound);
                                log::info!("interface up: now listening on {ip}:{port}");
                            }
                        }
                    }
                    Ok(if_watch::IfEvent::Down(net)) => {
                        let ip = net.addr();
                        if listeners.remove(&ip).is_some() {
                            log::info!("interface down: stopped listening on {ip}:{port}");
                        }
                    }
                    Err(e) => log::debug!("if_watch error: {e}"),
                },
                wake = async {
                    match wake_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // `None` would mean the wake-sender was dropped;
                    // treat that as observer-gone, ignore further
                    // wakes by stripping the receiver. `Some(())` is
                    // the post-wake signal — force-close every
                    // accepted session. Each `read_loop`'s `recv`
                    // errors out and the existing exit path removes
                    // the slot, so a peer reconnect lands on a clean
                    // accept instead of stacking on top of a stale
                    // session for up to `RECV_IDLE_TIMEOUT`.
                    match wake {
                        Some(()) => {
                            let g = conns.lock().await;
                            log::info!(
                                "supervisor: post-wake — closing {} peer conn(s) \
                                 to force fresh reconnect",
                                g.len()
                            );
                            for (a, c) in g.iter() {
                                log::debug!("post-wake close: {a}");
                                let _ = c.conn.close().await;
                            }
                        }
                        None => {
                            log::debug!(
                                "supervisor: wake channel closed; \
                                 power observer no longer signaling"
                            );
                            wake_rx = None;
                        }
                    }
                }
                p = request_port_change_rx.recv() => {
                    let new_port = p.expect("channel closed");
                    listeners.clear(); // Drop aborts each accept task
                    let mut bound = 0usize;
                    let addrs = enumerate_listenable_addrs();
                    for ip in &addrs {
                        let slot = bind_ip(*ip, new_port, &cfg, &ctx).await;
                        if slot.udp_bound() {
                            bound += 1;
                        }
                        listeners.insert(*ip, slot);
                    }
                    if bound == 0 {
                        port_changed_tx
                            .send(Err(ListenerCreationError::NoBoundListener))
                            .expect("channel closed");
                    } else {
                        port = new_port;
                        port_changed_tx
                            .send(Ok(port))
                            .expect("channel closed");
                    }
                }
            }
        }
    })
}

/// Max silence on an accepted session before it's torn down.
/// UDP/DTLS carries no FIN — a peer that goes silent
/// (asleep, network-partitioned, daemon killed) leaves an otherwise-
/// blocked `recv` waiting forever, and the slot in `conns` becomes
/// a zombie that nothing else evicts. The connector side sends a
/// `Ping` cycle roughly every 2s, so under healthy operation we see
/// traffic well within this window; any longer gap means the path
/// is broken in a way our reply attempts won't recover from. Set
/// generously (3–5× the connector's natural cadence) so a paused
/// peer process during e.g. a stop-the-world GC isn't mistaken for
/// a dead connection.
const RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

#[allow(clippy::too_many_arguments)]
async fn read_loop(
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: ListenerSessions,
    addr: SocketAddr,
    session: ListenerSession,
    conn: ArcConn,
    listen_tx: Sender<ListenEvent>,
    fingerprint: String,
    transport: TransportKind,
    auth_gate: AuthGate,
) -> Result<(), Error> {
    // Buffer sized for the largest legal variable-length frame; fixed
    // mouse/keyboard datagrams use only the first MAX_EVENT_SIZE bytes and
    // topology remains below the clipboard cap.
    let mut b = [0u8; MAX_CLIPBOARD_SIZE];

    // Per-connection watchdog deadline. An open access-password challenge
    // extends it (see below); the cell lives here, not on the shared gate,
    // so one peer's challenge can't stretch another peer's handshake
    // window.
    let deadline = Rc::new(Cell::new(Instant::now() + HELLO_TIMEOUT));

    // Handshake gate: the peer must present a `Hello` carrying
    // `PROTOCOL_MAGIC` within `HELLO_TIMEOUT`. `hello_watchdog`
    // closes the connection if it doesn't; a `Hello` with the wrong
    // magic is refused immediately below. This is the listen-side
    // half of the deliberate hard cut-over from lan-mouse — a
    // foreign peer is rejected with a clear log instead of silently
    // half-working.
    let hello_ok = Rc::new(Cell::new(false));
    spawn_local(hello_watchdog(
        conns.clone(),
        addr,
        session,
        conn.clone(),
        hello_ok.clone(),
        deadline.clone(),
    ));

    // Access-password challenge state: while `Some`, the expected proof is
    // pending and only `AuthPassword` is meaningful from the peer.
    let mut expected_proof: Option<[u8; 32]> = None;
    // Session-level admission: either the fingerprint is persistently
    // authorized (no challenge) or the challenge passed on this transport.
    let mut session_authorized = !auth_gate.needs_challenge(&fingerprint);

    loop {
        // While a challenge is open the peer stays silent until the user
        // submits the password — don't mistake that for a dead connection.
        let idle = if expected_proof.is_some() {
            AUTH_CHALLENGE_IDLE
        } else {
            RECV_IDLE_TIMEOUT
        };
        let n = match tokio::time::timeout(idle, conn.recv(&mut b)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => break,
            Err(_) => {
                log::warn!("{addr}: no datagram in {idle:?} — closing stale connection");
                let _ = conn.close().await;
                break;
            }
        };
        if n == 0 {
            continue;
        }
        if !accepted_connection_is_current(&conns, addr, session).await {
            log::debug!("ending replaced inbound {transport} read session {session} for {addr}");
            break;
        }
        let datagram = &b[..n];
        match decode_listen_datagram(datagram) {
            Some(event) if !handshake_allows_event(hello_ok.get(), &event) => {
                log::debug!("ignoring pre-Hello event from {addr}: {event}");
            }
            Some(ProtoEvent::Hello {
                magic,
                commit,
                capabilities,
            }) => {
                if magic != PROTOCOL_MAGIC {
                    log::warn!(
                        "refusing {addr}: peer presented a foreign protocol \
                         handshake (not hopdeck) — closing connection"
                    );
                    break;
                }
                if !session_authorized {
                    // Access-password challenge management. A duplicate
                    // Hello while the challenge is open is a retransmission
                    // racing the AuthRequired reply — ignore it.
                    if expected_proof.is_some() {
                        log::debug!(
                            "ignoring duplicate Hello from {addr} while the \
                             access-password challenge is open"
                        );
                        continue;
                    }
                    if capabilities & CAP_ACCESS_PASSWORD == 0 {
                        log::warn!(
                            "refusing {addr}: unauthorized peer does not support \
                             access-password auth — closing connection"
                        );
                        let _ = conn.close().await;
                        break;
                    }
                    if auth_gate.is_rate_limited(addr.ip()) {
                        log::warn!(
                            "refusing {addr}: too many failed access-password \
                             attempts — cooling down"
                        );
                        let _ = conn.close().await;
                        break;
                    }
                    let Some(password) = auth_gate.password() else {
                        // Race: the password was removed between the DTLS
                        // admission and the challenge; treat as unauthorized.
                        log::warn!(
                            "refusing {addr}: unauthorized peer without an \
                             access password configured — closing connection"
                        );
                        let _ = conn.close().await;
                        break;
                    };
                    let mut salt = [0u8; 16];
                    let mut nonce = [0u8; 16];
                    random_bytes(&mut salt);
                    random_bytes(&mut nonce);
                    expected_proof = Some(crypto::access_password_proof(&salt, &nonce, &password));
                    deadline.set(Instant::now() + AUTH_CHALLENGE_TIMEOUT);
                    log::info!("challenging {addr} for the access password");
                    self_reply(&conn, ProtoEvent::AuthRequired { salt, nonce }).await;
                    continue;
                }
                hello_ok.set(true);
                listen_tx
                    .send(ListenEvent::Msg {
                        event: ProtoEvent::Hello {
                            magic,
                            commit,
                            capabilities,
                        },
                        addr,
                        session,
                    })
                    .expect("channel closed");
            }
            Some(ProtoEvent::AuthPassword { proof }) if expected_proof.is_some() => {
                let expected = expected_proof.take().expect("checked above");
                if auth_gate.is_rate_limited(addr.ip()) {
                    log::warn!(
                        "refusing {addr}: too many failed access-password \
                         attempts — cooling down"
                    );
                    let _ = conn.close().await;
                    break;
                }
                if proof == expected {
                    log::info!("access password accepted for {addr}; session admitted");
                    session_authorized = true;
                    auth_gate.clear_failures(addr.ip());
                    deadline.set(Instant::now() + HELLO_TIMEOUT);
                    self_reply(&conn, ProtoEvent::AuthResult { ok: true }).await;
                    // Publish the withheld Accept now — ahead of the peer's
                    // next Hello retransmission, which flows through the
                    // normal handshake path and completes the session.
                    listen_tx
                        .send(ListenEvent::Accept {
                            addr,
                            session,
                            fingerprint: fingerprint.clone(),
                            transport,
                        })
                        .expect("channel closed");
                } else {
                    auth_gate.record_failure(addr.ip());
                    log::warn!("{addr} failed the access-password challenge — rejecting");
                    self_reply(&conn, ProtoEvent::AuthResult { ok: false }).await;
                    let _ = conn.close().await;
                    break;
                }
            }
            Some(ProtoEvent::AuthPassword { .. }) => {
                log::debug!("ignoring unrequested AuthPassword from {addr}");
            }
            Some(ProtoEvent::AuthRequired { .. } | ProtoEvent::AuthResult { .. }) => {
                // The listen side never accepts these from a peer; a
                // pre-handshake sender would otherwise ride the extended
                // handshake gate into the forwarding path.
                log::debug!("ignoring out-of-place auth event from {addr}");
            }
            Some(event) => listen_tx
                .send(ListenEvent::Msg {
                    event,
                    addr,
                    session,
                })
                .expect("channel closed"),
            None => {
                // Skip the malformed/unknown datagram and keep
                // listening. Each recv returns one full
                // datagram, so a parse error here can't desync a
                // stream; the next call gets a fresh, framed
                // message. This makes the protocol forward-
                // compatible: a peer running a newer HopDeck
                // version can introduce additional event types
                // and old peers will simply ignore them rather
                // than dropping the connection.
                log::debug!("ignoring undecodable {n}-byte event from {addr}");
            }
        }
    }
    log::info!("{transport} client disconnected {addr:?}");
    let mut conns = conns.lock().await;
    // A peer can reconnect with the same UDP 4-tuple before the old
    // read loop observes its close. Remove this slot only if it still
    // belongs to our exact Arc; a stale loop must never evict the
    // replacement connection.
    if remove_connection_if_current(&mut conns, addr, session).is_none() {
        log::debug!("ignoring stale disconnect for replaced {transport} client {addr:?}");
    } else {
        sessions.remove_if_current(addr, session);
        drop(conns);
        let _ = listen_tx.send(ListenEvent::Disconnected { addr, session });
    }
    Ok(())
}

/// Send one fixed-size protocol frame directly over an accepted
/// transport. The normal reply path routes through the listener (which
/// re-checks the session generation); the access-password exchange runs
/// inside the connection's own read task, which already owns this exact
/// session, so it replies on the held `ArcConn` instead.
async fn self_reply(conn: &ArcConn, event: ProtoEvent) {
    let display = format!("{event}");
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
    log::trace!("auth reply {display} >=>=>=>=>=> {len} bytes");
    if let Err(e) = conn.send(&buf[..len]).await {
        log::debug!("auth reply send failed: {e}");
    }
}

async fn accepted_connection_is_current(
    conns: &AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>,
    addr: SocketAddr,
    session: ListenerSession,
) -> bool {
    conns
        .lock()
        .await
        .get(&addr)
        .is_some_and(|slot| slot.session == session)
}

fn remove_connection_if_current(
    conns: &mut HashMap<SocketAddr, AcceptedConnection>,
    addr: SocketAddr,
    session: ListenerSession,
) -> Option<AcceptedConnection> {
    conns
        .get(&addr)
        .is_some_and(|current| current.session == session)
        .then(|| conns.remove(&addr))
        .flatten()
}

/// Connection-establishment deadline. A peer that has not presented
/// a `PROTOCOL_MAGIC`-stamped `Hello` within this window is not a
/// hopdeck instance and its connection is closed. The connect side
/// retransmits its `Hello` across a shorter span, so a genuine
/// hopdeck peer always flips `hello_ok` well before this fires.
const HELLO_TIMEOUT: Duration = Duration::from_secs(12);

/// Close `conn` if `hello_ok` is still unset once the shared deadline
/// expires. Reaching the close path means the peer never spoke a valid
/// hopdeck handshake — the listen-side enforcement of the hard
/// cut-over from lan-mouse. The timeout only fires while our
/// generation still owns the address: a session handed over to the
/// capture side (reverse adoption) has left the connection table, and
/// its orphaned watchdog — which can never observe the handshake that
/// the new owner processes — must stand down instead of killing the
/// adopted transport. The deadline lives in a per-connection cell so an
/// open access-password challenge (manual password entry on the peer)
/// can extend it past the plain handshake window.
async fn hello_watchdog(
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    addr: SocketAddr,
    session: ListenerSession,
    conn: ArcConn,
    hello_ok: Rc<Cell<bool>>,
    deadline: Rc<Cell<Instant>>,
) {
    loop {
        let now = Instant::now();
        let deadline_at = deadline.get();
        if now < deadline_at {
            tokio::time::sleep(deadline_at - now).await;
            continue;
        }
        break;
    }
    if !hello_ok.get() && accepted_connection_is_current(&conns, addr, session).await {
        log::warn!(
            "refusing {addr}: peer did not complete the hopdeck handshake \
             within the window — closing connection"
        );
        let _ = conn.close().await;
    }
}

/// Classify an accepted-transport frame by its first-byte event-type tag and
/// route through a variable-length codec or the fixed-buffer `try_into` path.
/// Mirrors `decode_proto_datagram` on the connect side so both directions
/// accept the same wire formats.
fn decode_listen_datagram(bytes: &[u8]) -> Option<ProtoEvent> {
    use hopdeck_proto::EventType;
    let tag = *bytes.first()?;
    if tag == EventType::Clipboard as u8 {
        return decode_clipboard_event(bytes).ok();
    }
    if tag == EventType::DisplayLayout as u8 {
        return decode_display_layout_event(bytes).ok();
    }
    decode_fixed_event(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use tokio::net::UdpSocket;
    use tokio_rustls::TlsConnector;

    async fn test_conn() -> ArcConn {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("test socket"))
    }

    fn test_fingerprint() -> String {
        "aa:bb:cc".to_owned()
    }

    fn accepted(session: ListenerSession, conn: ArcConn) -> AcceptedConnection {
        AcceptedConnection {
            session,
            conn,
            fingerprint: test_fingerprint(),
            transport: TransportKind::Udp,
        }
    }

    fn self_signed() -> Certificate {
        Certificate::generate_self_signed(["hopdeck-listen-test".to_owned()]).expect("certificate")
    }

    /// An `AcceptContext` backed by a loopback listener's worth of state,
    /// plus the event receiver to inspect what the accept path published.
    fn tcp_context(
        server_cert: &Certificate,
        authorized: &[String],
        password: Option<&str>,
    ) -> (AcceptContext, Receiver<ListenEvent>) {
        let (listen_tx, listen_rx) = channel();
        let authorized_keys = authorized
            .iter()
            .map(|fp| (fp.clone(), IncomingPeerConfig::default()))
            .collect();
        let ctx = AcceptContext {
            listen_tx,
            conns: Rc::new(AsyncMutex::new(HashMap::new())),
            sessions: ListenerSessions::default(),
            connection_attempts: Default::default(),
            auth_gate: AuthGate {
                password: Arc::new(RwLock::new(password.map(str::to_owned))),
                authorized_keys: Arc::new(RwLock::new(authorized_keys)),
                failures: Default::default(),
            },
            tls: TlsAcceptor::from(Arc::new(
                tunnel::tls::server_config(server_cert).expect("server TLS config"),
            )),
        };
        (ctx, listen_rx)
    }

    /// Run one TLS client/server handshake against
    /// `accept_tcp_connection`. Returns the peer address the server saw
    /// and the client stream (kept alive by the caller so the session
    /// does not close mid-assertion).
    async fn run_tls_accept(
        ctx: &AcceptContext,
        client_cert: &Certificate,
    ) -> (SocketAddr, tokio_rustls::client::TlsStream<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tcp listener");
        let addr = listener.local_addr().expect("listener address");
        let peer = Rc::new(Cell::new(None::<SocketAddr>));

        let server = {
            let ctx = ctx.clone();
            let peer = peer.clone();
            async move {
                let (stream, peer_addr) = listener.accept().await.expect("accept tcp");
                peer.set(Some(peer_addr));
                accept_tcp_connection(stream, peer_addr, ctx).await;
            }
        };
        let client = async move {
            let config = tunnel::tls::client_config(client_cert).expect("client TLS config");
            let connector = TlsConnector::from(Arc::new(config));
            let sock = TcpStream::connect(addr).await.expect("tcp connect");
            let name = ServerName::try_from("ignored").expect("server name");
            connector.connect(name, sock).await.expect("tls connect")
        };
        let (_, stream) = tokio::join!(server, client);
        (peer.get().expect("server saw the peer address"), stream)
    }

    #[tokio::test]
    async fn authorized_tcp_peer_registers_with_fingerprint_and_transport() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server_cert = self_signed();
                let client_cert = self_signed();
                let fingerprint = crypto::certificate_fingerprint(&client_cert);
                let (ctx, mut events) =
                    tcp_context(&server_cert, std::slice::from_ref(&fingerprint), None);

                let (peer_addr, _client) = run_tls_accept(&ctx, &client_cert).await;

                match events.recv().await.expect("accept event") {
                    ListenEvent::Accept {
                        addr,
                        session,
                        fingerprint: got,
                        transport,
                    } => {
                        assert_eq!(addr, peer_addr);
                        assert_eq!(got, fingerprint);
                        assert_eq!(transport, TransportKind::Tcp);
                        // The registered slot carries the same identity, so
                        // `get_certificate_fingerprint` needs no downcast.
                        let conns = ctx.conns.lock().await;
                        let slot = conns.get(&addr).expect("registered");
                        assert_eq!(slot.session, session);
                        assert_eq!(slot.fingerprint, fingerprint);
                        assert_eq!(slot.transport, TransportKind::Tcp);
                    }
                    _ => panic!("expected an Accept event"),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn unauthorized_tcp_peer_is_registered_but_accept_withheld_in_password_mode() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server_cert = self_signed();
                let client_cert = self_signed();
                let (ctx, mut events) = tcp_context(&server_cert, &[], Some("secret"));

                let (peer_addr, _client) = run_tls_accept(&ctx, &client_cert).await;

                // Registered so `read_loop` can run the access-password
                // challenge, but no `Accept` is published yet.
                let conns = ctx.conns.lock().await;
                assert_eq!(conns.len(), 1);
                assert!(conns.contains_key(&peer_addr));
                drop(conns);
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), events.recv())
                        .await
                        .is_err(),
                    "accept must be withheld until the challenge passes"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn unauthorized_tcp_peer_without_password_is_rejected() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server_cert = self_signed();
                let client_cert = self_signed();
                let fingerprint = crypto::certificate_fingerprint(&client_cert);
                let (ctx, mut events) = tcp_context(&server_cert, &[], None);

                let (_peer_addr, _client) = run_tls_accept(&ctx, &client_cert).await;

                match events.recv().await.expect("rejection event") {
                    ListenEvent::Rejected { fingerprint: got } => {
                        assert_eq!(got, fingerprint);
                    }
                    _ => panic!("expected a Rejected event"),
                }
                assert!(
                    ctx.conns.lock().await.is_empty(),
                    "an unauthorized peer must never enter the session table"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn registering_a_peer_evicts_its_old_session_at_another_address() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let old_addr: SocketAddr = "127.0.0.1:4001".parse().expect("old address");
                let keep_addr: SocketAddr = "127.0.0.1:4002".parse().expect("keep address");
                let other_addr: SocketAddr = "127.0.0.1:4003".parse().expect("other address");
                let (listen_tx, mut listen_rx) = channel();
                let sessions = ListenerSessions::default();
                let old_session = sessions.allocate(old_addr);
                let keep_session = sessions.allocate(keep_addr);
                let other_session = sessions.allocate(other_addr);
                let conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>> =
                    Rc::new(AsyncMutex::new(HashMap::from([
                        (
                            old_addr,
                            AcceptedConnection {
                                session: old_session,
                                conn: test_conn().await,
                                fingerprint: "aa".into(),
                                transport: TransportKind::Udp,
                            },
                        ),
                        (
                            keep_addr,
                            AcceptedConnection {
                                session: keep_session,
                                conn: test_conn().await,
                                fingerprint: "aa".into(),
                                transport: TransportKind::Tcp,
                            },
                        ),
                        (
                            other_addr,
                            AcceptedConnection {
                                session: other_session,
                                conn: test_conn().await,
                                fingerprint: "bb".into(),
                                transport: TransportKind::Udp,
                            },
                        ),
                    ])));

                evict_peer_sessions(&conns, &sessions, keep_addr, keep_session, "aa", &listen_tx)
                    .await;

                {
                    let guard = conns.lock().await;
                    assert!(!guard.contains_key(&old_addr), "superseded peer removed");
                    assert!(guard.contains_key(&keep_addr), "newest session kept");
                    assert!(
                        guard.contains_key(&other_addr),
                        "a different peer is untouched"
                    );
                }
                assert_eq!(sessions.current.borrow().get(&old_addr), None);
                match listen_rx.recv().await.expect("disconnect event") {
                    ListenEvent::Disconnected { addr, session } => {
                        assert_eq!(addr, old_addr);
                        assert_eq!(session, old_session);
                    }
                    _ => panic!("expected a Disconnected event for the superseded session"),
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listen_rx.recv())
                        .await
                        .is_err(),
                    "only the superseded session is reported"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn reconnect_from_a_new_ephemeral_port_supersedes_the_old_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server_cert = self_signed();
                let client_cert = self_signed();
                let fingerprint = crypto::certificate_fingerprint(&client_cert);
                let (ctx, mut events) =
                    tcp_context(&server_cert, std::slice::from_ref(&fingerprint), None);

                let (first_addr, _first_client) = run_tls_accept(&ctx, &client_cert).await;
                let first_session = match events.recv().await.expect("first accept") {
                    ListenEvent::Accept { addr, session, .. } => {
                        assert_eq!(addr, first_addr);
                        session
                    }
                    _ => panic!("expected an Accept for the first session"),
                };

                let (second_addr, _second_client) = run_tls_accept(&ctx, &client_cert).await;
                assert_ne!(
                    first_addr, second_addr,
                    "a TCP reconnect uses a fresh ephemeral port"
                );
                // Emulation hears about the replacement in order: tear down
                // the old handle first, then accept the new one.
                match events.recv().await.expect("superseded disconnect") {
                    ListenEvent::Disconnected { addr, session } => {
                        assert_eq!(addr, first_addr);
                        assert_eq!(session, first_session);
                    }
                    _ => panic!("expected the old session to be disconnected"),
                }
                match events.recv().await.expect("second accept") {
                    ListenEvent::Accept { addr, session, .. } => {
                        assert_eq!(addr, second_addr);
                        assert_ne!(session, first_session);
                    }
                    _ => panic!("expected an Accept for the second session"),
                }

                let conns = ctx.conns.lock().await;
                assert_eq!(conns.len(), 1, "only the newest session stays live");
                assert!(conns.contains_key(&second_addr));
            })
            .await;
    }

    #[tokio::test]
    async fn stale_same_address_read_loop_cannot_remove_replacement() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let replacement = test_conn().await;
        let old_session = 1;
        let replacement_session = 2;
        let mut conns = HashMap::from([(addr, accepted(replacement_session, replacement.clone()))]);

        assert!(remove_connection_if_current(&mut conns, addr, old_session).is_none());
        assert!(Arc::ptr_eq(
            &conns.get(&addr).expect("replacement remains").conn,
            &replacement,
        ));
    }

    #[tokio::test]
    async fn current_read_loop_removes_only_its_connection() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let current = test_conn().await;
        let session = 7;
        let mut conns = HashMap::from([(addr, accepted(session, current.clone()))]);

        let removed = remove_connection_if_current(&mut conns, addr, session)
            .expect("current connection removed");
        assert_eq!(removed.session, session);
        assert!(Arc::ptr_eq(&removed.conn, &current));
        assert!(!conns.contains_key(&addr));
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn take_conn_yields_only_the_current_generation() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let current = test_conn().await;
        let sessions = ListenerSessions::default();
        let stale = sessions.allocate(addr);
        let active = sessions.allocate(addr);
        let conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>> = Rc::new(
            AsyncMutex::new(HashMap::from([(addr, accepted(active, current.clone()))])),
        );
        let (listen_tx, listen_rx) = local_channel::mpsc::channel();
        let (request_port_change, _request_port_change_rx) = local_channel::mpsc::channel();
        let (_port_changed_tx, port_changed) = local_channel::mpsc::channel();
        let listener = HopDeckListener {
            listen_rx,
            listen_tx,
            listen_task: tokio::spawn(async {}),
            conns,
            sessions,
            request_port_change,
            port_changed,
            auth_gate: AuthGate {
                password: Arc::new(RwLock::new(None)),
                authorized_keys: Arc::new(RwLock::new(HashMap::new())),
                failures: Default::default(),
            },
        };

        assert!(listener.take_conn(addr, stale).await.is_none());
        assert!(!listener.conns.lock().await.is_empty());

        let taken = listener
            .take_conn(addr, active)
            .await
            .expect("current session yielded");
        assert!(Arc::ptr_eq(&taken, &current));
        assert!(listener.conns.lock().await.is_empty());
        assert_eq!(listener.current_session(addr), None);
    }

    #[test]
    fn queued_old_session_is_filtered_after_same_address_accept() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let sessions = ListenerSessions::default();
        let old = sessions.allocate(addr);
        let replacement = sessions.allocate(addr);

        assert!(!sessions.is_current(addr, old));
        assert!(sessions.is_current(addr, replacement));
    }

    #[test]
    fn terminal_event_is_dropped_if_same_address_was_replaced() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let sessions = ListenerSessions::default();
        let ended = sessions.allocate(addr);
        assert!(sessions.remove_if_current(addr, ended));
        let disconnected = ListenEvent::Disconnected {
            addr,
            session: ended,
        };
        assert!(listen_event_is_current(&sessions, &disconnected));

        let _replacement = sessions.allocate(addr);
        assert!(!listen_event_is_current(&sessions, &disconnected));
    }
}
