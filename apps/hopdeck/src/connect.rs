use crate::client::ClientManager;
use crate::config::local_commit;
use crate::crypto;
use crate::discovery::{PrimaryCache, normalize_mdns_name};
use crate::emulation::EmulationRequest;
use crate::tcp_transport;
use crate::tunnel;
use hopdeck_ipc::{
    ClientHandle, ConnectMode, ConnectionMode, DEFAULT_PORT, TransportKind, TransportMode,
};
use hopdeck_proto::{
    MAX_CLIPBOARD_SIZE, MAX_EVENT_SIZE, PROTOCOL_MAGIC, ProtoEvent, decode_clipboard_event,
    decode_display_layout_event, decode_fixed_event, encode_clipboard_event,
    encode_display_layout_event,
};
use input_event::{Event as InputEvent, KeyboardEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::ServerName;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    io,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use tokio_rustls::TlsConnector;
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

type ArcConn = Arc<dyn Conn + Send + Sync>;
pub(crate) type ConnectionSession = u64;

#[derive(Clone)]
struct ConnectionSlot {
    handle: ClientHandle,
    session: ConnectionSession,
    conn: ArcConn,
}

#[derive(Clone, Default)]
struct SessionTracker {
    next: Rc<Cell<ConnectionSession>>,
    current: Rc<RefCell<HashMap<ClientHandle, ConnectionSession>>>,
}

impl SessionTracker {
    fn allocate(&self, handle: ClientHandle) -> ConnectionSession {
        let mut session = self.next.get().wrapping_add(1);
        if session == 0 {
            session = 1;
        }
        self.next.set(session);
        self.current.borrow_mut().insert(handle, session);
        session
    }

    fn is_current(&self, handle: ClientHandle, session: ConnectionSession) -> bool {
        self.current.borrow().get(&handle).copied() == Some(session)
    }

    fn current(&self, handle: ClientHandle) -> Option<ConnectionSession> {
        self.current.borrow().get(&handle).copied()
    }

    fn remove_if_current(&self, handle: ClientHandle, session: ConnectionSession) -> bool {
        if !self.is_current(handle, session) {
            return false;
        }
        self.current.borrow_mut().remove(&handle);
        true
    }
}

#[derive(Debug, Error)]
pub(crate) enum HopDeckConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    #[error("tcp: {0}")]
    Tcp(io::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

impl HopDeckConnectionError {
    /// Whether this failure tells us *nothing* about the peer: no answer
    /// arrived on this transport. Only such a failure justifies trying the
    /// other transport — a peer that reached its security layer and refused
    /// us (bad fingerprint, failed access password, foreign service on the
    /// port) is reachable and would refuse the fallback in exactly the same
    /// way, so falling back would only duplicate the authorization prompt.
    fn is_transport_failure(&self) -> bool {
        match self {
            HopDeckConnectionError::Timeout | HopDeckConnectionError::NotConnected => true,
            HopDeckConnectionError::Bind(e) | HopDeckConnectionError::Tcp(e) => {
                is_unreachable_io(e)
            }
            HopDeckConnectionError::Webrtc(webrtc_util::Error::Io(e)) => is_unreachable_io(&e.0),
            HopDeckConnectionError::Dtls(e) => is_unreachable_dtls(e),
            HopDeckConnectionError::Tls(_) | HopDeckConnectionError::TargetEmulationDisabled => {
                false
            }
            // Any other webrtc-util error means the local stack gave up
            // mid-handshake with the peer involved; not a pure transport
            // miss.
            HopDeckConnectionError::Webrtc(_) => false,
        }
    }
}

fn is_unreachable_io(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::AddrNotAvailable
    )
}

/// DTLS failures that mean "nothing answered on this socket" rather than
/// "the peer answered and refused". `ErrAlertFatalOrClose` and the
/// certificate/verify errors are deliberately *not* listed: those are the
/// authentication refusals that must not trigger a transport fallback.
fn is_unreachable_dtls(e: &webrtc_dtls::Error) -> bool {
    match e {
        webrtc_dtls::Error::ErrDeadlineExceeded
        | webrtc_dtls::Error::ErrConnClosed
        | webrtc_dtls::Error::ErrContextCanceled
        | webrtc_dtls::Error::ErrHandshakeInProgress => true,
        webrtc_dtls::Error::Io(e) => is_unreachable_io(&e.0),
        webrtc_dtls::Error::Util(webrtc_util::Error::Io(e)) => is_unreachable_io(&e.0),
        _ => false,
    }
}

/// Events delivered from the outbound connection tasks to the capture loop.
/// A disconnect is deliberately distinct from a wire `Leave`: the peer is no
/// longer reachable, so capture must be released locally without trying to
/// send cleanup events over (and potentially reconnect) the dead session.
#[derive(Debug)]
pub(crate) enum HopDeckConnectionEvent {
    Received {
        handle: ClientHandle,
        addr: SocketAddr,
        session: ConnectionSession,
        event: ProtoEvent,
    },
    Disconnected {
        handle: ClientHandle,
        session: ConnectionSession,
    },
}

/// Before a DTLS peer proves it speaks HopDeck, only its protocol `Hello`
/// and the access-password exchange may affect connection or input state.
/// The caller still validates the hello's magic and rejects a foreign
/// value. The auth events must pass the gate precisely so the challenge
/// can complete before the handshake does; every other event stays
/// dropped until `hello_ok` flips.
///
/// `VideoOffer`/`VideoStop` are the one deliberate addition (D39): the
/// sharing side announces its video port the moment the input session
/// comes up, which races the controlling side's `Hello` echo and used
/// to lose every offer. They only ever *store* an offer inside the
/// capture state machine — the controlling side dials nothing until the
/// frontend asks (increment ④'s view toggle) — so an early arrival
/// cannot affect input state, and a fresher offer supersedes the
/// stored one.
pub(crate) fn handshake_allows_event(hello_ok: bool, event: &ProtoEvent) -> bool {
    hello_ok
        || matches!(
            event,
            ProtoEvent::Hello { .. }
                | ProtoEvent::AuthRequired { .. }
                | ProtoEvent::AuthPassword { .. }
                | ProtoEvent::AuthResult { .. }
                | ProtoEvent::VideoOffer { .. }
                | ProtoEvent::VideoStop
        )
}

/// Cleanup already committed to an exact transport generation must remain
/// deliverable after Service removes or reconfigures the corresponding
/// `ClientManager` entry. Ordinary input is deliberately excluded: a stale
/// capture event must not gain permission merely because it still knows an old
/// session number.
fn is_session_cleanup_event(event: &ProtoEvent) -> bool {
    matches!(
        event,
        ProtoEvent::Leave(_)
            | ProtoEvent::HandoverLeave { .. }
            | ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key { state: 0, .. }))
            | ProtoEvent::HandoverInput {
                event: InputEvent::Keyboard(KeyboardEvent::Key { state: 0, .. }),
                ..
            }
            | ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }))
            | ProtoEvent::HandoverInput {
                event: InputEvent::Keyboard(KeyboardEvent::Modifiers {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0,
                }),
                ..
            }
    )
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial backoff between connect attempts that find no usable address
/// (no static IPs, no DNS-resolved IPs, no mDNS primary hint). Doubles
/// on each subsequent failure up to [`MAX_RETRY_BACKOFF`]. The backoff
/// is bypassed entirely when the input set changes (e.g. mDNS browse
/// resolves a primary, DNS lookup returns IPs) so a peer that comes
/// back online reconnects on the next mouse event without waiting.
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// Per-handle gate that throttles repeat connect attempts when nothing
/// new is available to dial. `signature` hashes the candidate set we
/// last attempted; if the current set differs we skip the gate and
/// retry immediately. Otherwise `next_attempt_at` enforces exponential
/// backoff capped at [`MAX_RETRY_BACKOFF`].
struct RetryState {
    next_attempt_at: Instant,
    backoff: Duration,
    signature: u64,
}

fn signature_of(ips: &HashSet<IpAddr>, primary: Option<IpAddr>) -> u64 {
    let mut sorted: Vec<IpAddr> = ips.iter().copied().collect();
    sorted.sort();
    let mut hasher = DefaultHasher::new();
    sorted.hash(&mut hasher);
    primary.hash(&mut hasher);
    hasher.finish()
}

/// Update `retry_state[handle]` after a failed connect attempt: doubles
/// the backoff (capped at [`MAX_RETRY_BACKOFF`]) and stamps the
/// candidate-set signature so a later signature change can short-
/// circuit the gate.
fn record_retry_failure(
    retry_state: &Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    handle: ClientHandle,
    ips: &HashSet<IpAddr>,
    primary: Option<IpAddr>,
) {
    let sig = signature_of(ips, primary);
    let mut map = retry_state.borrow_mut();
    let entry = map.entry(handle).or_insert(RetryState {
        next_attempt_at: Instant::now(),
        backoff: INITIAL_RETRY_BACKOFF,
        signature: sig,
    });
    entry.signature = sig;
    let next = entry.backoff;
    entry.next_attempt_at = Instant::now() + next;
    entry.backoff = (next * 2).min(MAX_RETRY_BACKOFF);
}

/// A successful dial: the transport the session runs on plus the identity
/// facts that used to be recovered by downcasting the DTLS connection.
struct Dialed {
    conn: ArcConn,
    addr: SocketAddr,
    transport: TransportKind,
    /// Leaf certificate fingerprint of the peer, taken right after the
    /// handshake on either transport.
    peer_fingerprint: Option<String>,
}

/// How long a TCP connect plus the TLS handshake on top of it may take.
/// Same budget as the DTLS handshake; a LAN round-trip is orders of
/// magnitude shorter.
const TCP_CONNECT_TIMEOUT: Duration = DEFAULT_CONNECTION_TIMEOUT;

/// How long `TransportMode::Auto` waits for *any* UDP candidate to
/// handshake before it also tries TCP. UDP stays the preferred path: when
/// it works it wins well within this budget, and when the network drops
/// UDP one-way this bounds the extra latency before the fallback dial
/// starts.
const AUTO_UDP_TIMEOUT: Duration = Duration::from_secs(2);

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<Dialed, (SocketAddr, HopDeckConnectionError)> {
    log::info!("connecting to {addr} over udp ...");
    // Bind family must match the target's: a 0.0.0.0 socket fails
    // `connect()` to a v6 peer with EAFNOSUPPORT, and vice versa.
    // On a v4-only kernel the `[::]:0` bind itself errors out and
    // the caller treats it as a normal per-address connect failure.
    let bind_addr: &str = match addr {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let conn = Arc::new(
        UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, HopDeckConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => {
                let peer_fingerprint = dtls_peer_fingerprint(&dtls_conn).await;
                Ok(Dialed {
                    conn: Arc::new(dtls_conn),
                    addr,
                    transport: TransportKind::Udp,
                    peer_fingerprint,
                })
            }
            Err(e) => Err((addr, e.into())),
        }
    }
}

/// Leaf certificate fingerprint presented by a DTLS peer. Read while the
/// connection is still concrete — the session layer no longer downcasts.
async fn dtls_peer_fingerprint(conn: &DTLSConn) -> Option<String> {
    let state = conn.connection_state().await;
    state
        .peer_certificates
        .first()
        .map(|cert| crypto::generate_fingerprint(cert))
}

/// Dial one address with TLS 1.3 over TCP, reusing the session
/// certificate and the tunnel's rustls configuration.
async fn connect_tcp(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<Dialed, (SocketAddr, HopDeckConnectionError)> {
    log::info!("connecting to {addr} over tcp ...");
    let config =
        tunnel::tls::client_config(&cert).map_err(|e| (addr, HopDeckConnectionError::Tls(e)))?;
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = match tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Err(_) => return Err((addr, HopDeckConnectionError::Timeout)),
        Ok(Err(e)) => return Err((addr, HopDeckConnectionError::Tcp(e))),
        Ok(Ok(tcp)) => tcp,
    };
    if let Err(e) = tcp.set_nodelay(true) {
        log::debug!("{addr}: TCP_NODELAY failed: {e}");
    }
    let server_name = ServerName::try_from("ignored").expect("static server name");
    let stream = match tokio::time::timeout(
        TCP_CONNECT_TIMEOUT,
        connector.connect(server_name, tcp),
    )
    .await
    {
        Err(_) => return Err((addr, HopDeckConnectionError::Timeout)),
        Ok(Err(e)) => return Err((addr, HopDeckConnectionError::Tcp(e))),
        Ok(Ok(stream)) => stream,
    };
    let peer_fingerprint = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(tunnel::tls::peer_fingerprint);
    let conn =
        tcp_transport::client_conn(stream).map_err(|e| (addr, HopDeckConnectionError::Tcp(e)))?;
    Ok(Dialed {
        conn: tcp_transport::into_conn(conn),
        addr,
        transport: TransportKind::Tcp,
        peer_fingerprint,
    })
}

/// Time the preferred address gets to handshake alone before the
/// rest of the candidate list joins the race. Modeled on RFC 8305
/// "happy eyeballs" v6→v4 fallback delay; long enough that a healthy
/// preferred address virtually always wins, short enough that a
/// broken preferred path only slightly delays connect.
const PREFERRED_ADDR_HEAD_START: Duration = Duration::from_millis(200);

/// What a transport race learned, so the caller can decide whether
/// switching transport makes sense at all.
#[derive(Default)]
struct DialFailures {
    /// At least one address answered and then refused us: a bad
    /// fingerprint, a failed access password, a foreign service on the
    /// port. The peer is reachable.
    rejected: bool,
    /// At least one address failed purely in transit (timeout, refused,
    /// unreachable) without any authentication-level answer.
    unreachable: bool,
    last: Option<HopDeckConnectionError>,
}

impl DialFailures {
    fn record(&mut self, error: HopDeckConnectionError) {
        if error.is_transport_failure() {
            self.unreachable = true;
        } else {
            self.rejected = true;
        }
        self.last = Some(error);
    }

    /// Trying another transport is only justified when nothing answered on
    /// this one. Any authentication-level refusal means another transport
    /// would hit the same wall on the same peer.
    fn transport_unreachable(&self) -> bool {
        self.unreachable && !self.rejected
    }

    fn into_error(self) -> HopDeckConnectionError {
        self.last.unwrap_or(HopDeckConnectionError::NotConnected)
    }
}

async fn race_addrs<F, Fut>(
    addrs: &[SocketAddr],
    preferred: Option<SocketAddr>,
    cert: Certificate,
    dial: F,
) -> Result<Dialed, DialFailures>
where
    F: Fn(SocketAddr, Certificate) -> Fut + 'static,
    Fut: std::future::Future<Output = Result<Dialed, (SocketAddr, HopDeckConnectionError)>>
        + 'static,
{
    let mut failures = DialFailures::default();
    let mut joinset = JoinSet::new();
    if let Some(p) = preferred {
        // Dial the peer's mDNS-advertised primary first. If it
        // handshakes within `PREFERRED_ADDR_HEAD_START` we're done
        // before the others even start — the dialer biases toward
        // the OS-preferred interface (Mac service order, Linux
        // default route) without relying on RTT racing alone.
        joinset.spawn_local(dial(p, cert.clone()));
        let head_start = tokio::time::sleep(PREFERRED_ADDR_HEAD_START);
        tokio::pin!(head_start);
        loop {
            tokio::select! {
                _ = &mut head_start => break,
                Some(r) = joinset.join_next() => match r.expect("join error") {
                    Ok(dialed) => return Ok(dialed),
                    Err((a, e)) => {
                        log::warn!("failed to connect to {a}: `{e}`");
                        failures.record(e);
                    }
                },
            }
        }
    }
    for &addr in addrs {
        if Some(addr) == preferred {
            // already racing; don't dial the same socket twice
            continue;
        }
        joinset.spawn_local(dial(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(failures),
            Some(r) => match r.expect("join error") {
                Ok(dialed) => return Ok(dialed),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`");
                    failures.record(e);
                }
            },
        };
    }
}

/// Race every candidate address on one transport. `budget` bounds the
/// whole race (used by `Auto`'s short UDP window); `None` keeps the
/// historical per-address timeouts only.
async fn connect_any(
    addrs: &[SocketAddr],
    preferred: Option<SocketAddr>,
    cert: Certificate,
    transport: TransportKind,
    budget: Option<Duration>,
) -> Result<Dialed, DialFailures> {
    // The two branches are different `impl Future` types, so box the
    // race to share the timeout wrapper below.
    let race: std::pin::Pin<Box<dyn std::future::Future<Output = Result<Dialed, DialFailures>>>> =
        match transport {
            TransportKind::Udp => Box::pin(race_addrs(addrs, preferred, cert, connect)),
            TransportKind::Tcp => Box::pin(race_addrs(addrs, preferred, cert, connect_tcp)),
        };
    match budget {
        None => race.await,
        Some(budget) => match tokio::time::timeout(budget, race).await {
            Ok(result) => result,
            Err(_) => {
                log::info!(
                    "no {transport} handshake within {budget:?} — treating the path as \
                     unreachable"
                );
                Err(DialFailures {
                    unreachable: true,
                    ..Default::default()
                })
            }
        },
    }
}

/// Ordered transports to attempt for one dial.
///
/// `Auto` prefers whichever transport last succeeded for this peer
/// (remembered in memory for the daemon's lifetime) so a reconnect on a
/// one-way-UDP network does not pay the UDP timeout again. With no cached
/// answer UDP goes first; even a cached TCP answer keeps a single UDP
/// retry so the session returns to the lower-latency path once the
/// network heals.
fn transport_plan(mode: TransportMode, cached: Option<TransportKind>) -> Vec<TransportKind> {
    match mode {
        TransportMode::Udp => vec![TransportKind::Udp],
        TransportMode::Tcp => vec![TransportKind::Tcp],
        TransportMode::Auto => match cached {
            Some(TransportKind::Tcp) => vec![TransportKind::Tcp, TransportKind::Udp],
            _ => vec![TransportKind::Udp, TransportKind::Tcp],
        },
    }
}

/// Walk the transport plan for one dial. Returns the dial result plus the
/// transports actually attempted, in order (observability and tests).
///
/// A transport is only followed by the next one when its failure was a
/// pure transport failure — see [`DialFailures::transport_unreachable`].
async fn dial_transports(
    addrs: &[SocketAddr],
    preferred: Option<SocketAddr>,
    cert: Certificate,
    mode: TransportMode,
    cached: Option<TransportKind>,
) -> (Result<Dialed, HopDeckConnectionError>, Vec<TransportKind>) {
    let plan = transport_plan(mode, cached);
    let mut attempted = Vec::with_capacity(plan.len());
    let mut last_error = None;
    for (index, transport) in plan.iter().copied().enumerate() {
        attempted.push(transport);
        // `Auto` gives UDP a short window: it is the lower-latency path,
        // so a working UDP link must win, but a network that drops UDP
        // one-way must not cost the full handshake timeout on every
        // reconnect.
        let budget = match (mode, transport) {
            (TransportMode::Auto, TransportKind::Udp) => Some(AUTO_UDP_TIMEOUT),
            _ => None,
        };
        match connect_any(addrs, preferred, cert.clone(), transport, budget).await {
            Ok(dialed) => return (Ok(dialed), attempted),
            Err(failures) => {
                let next = plan.get(index + 1).copied();
                let retry = next.is_some() && failures.transport_unreachable();
                last_error = Some(failures.into_error());
                if !retry {
                    break;
                }
                log::info!(
                    "peer unreachable over {transport} — falling back to {}",
                    next.expect("checked above")
                );
            }
        }
    }
    (
        Err(last_error.unwrap_or(HopDeckConnectionError::NotConnected)),
        attempted,
    )
}

pub(crate) struct HopDeckConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    /// In-flight dial token per handle. The token is also the prospective DTLS
    /// session generation, so reset/delete can invalidate a dial before Slab
    /// reuses the numeric handle for another peer.
    connecting: Rc<Mutex<HashMap<ClientHandle, ConnectionSession>>>,
    recv_rx: Receiver<HopDeckConnectionEvent>,
    recv_tx: Sender<HopDeckConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    /// Send timestamp of the most-recent keepalive ping per active
    /// address. `receive_loop` subtracts it on `Pong` to get the live
    /// round-trip latency of the *active* connection — measured over
    /// the real DTLS/UDP path, so it's accurate and works even where a
    /// host firewall drops the TCP probe (the active address is then
    /// excluded from TCP probing; see [`ClientManager::probe_targets`]).
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
    /// Map of `peer_hostname -> primary_ipv4` populated by the
    /// `Discovery` mDNS browse task. Read on every `connect_to_handle`
    /// to bias which address gets the handshake head-start. Empty
    /// when discovery is disabled or no peer hint has arrived yet.
    primary_hints: PrimaryCache,
    /// Reverse-connect handoff: on a successful dial to a Reverse-mode
    /// client, the finished transport is sent here so the listener
    /// side can adopt it (the controlled side of a reverse tunnel
    /// injects, so its dialed connection must serve the inbound role).
    /// `None` in tests / send-only clones that never dial.
    reverse_adopt: Option<Sender<EmulationRequest>>,
    /// Per-handle retry gate. Suppresses connect attempts when the previous
    /// attempt failed and nothing new is available to dial,
    /// so an offline peer doesn't trigger a fresh `connect_to_handle`
    /// (and the associated DNS / mDNS lookup churn) on every mouse
    /// event. Cleared on successful connect; bypassed automatically
    /// when the candidate-set signature changes.
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    /// Access-password answers submitted by the frontend, keyed by peer
    /// IP. The receive loop consults it when the peer issues an
    /// `AuthRequired` challenge; entries persist in memory only (never
    /// persisted to disk) so a reconnect re-answers automatically, and a
    /// rejected password is dropped so the next attempt prompts again.
    /// A plain std mutex: only short, await-free lookups.
    pending_passwords: Rc<std::sync::Mutex<HashMap<IpAddr, String>>>,
    /// Last transport that successfully carried a session to each peer.
    /// `Auto` dials that transport first so a reconnect on a
    /// one-way-UDP network skips the UDP timeout. In memory only; a
    /// restart re-probes from UDP, which is the safe default (a stale
    /// TCP preference would survive a network move that restored UDP).
    transport_cache: Rc<RefCell<HashMap<ClientHandle, TransportKind>>>,
}

impl HopDeckConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        primary_hints: PrimaryCache,
        reverse_adopt: Option<Sender<EmulationRequest>>,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            sessions: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            ping_sent_at: Default::default(),
            primary_hints,
            reverse_adopt,
            retry_state: Default::default(),
            pending_passwords: Default::default(),
            transport_cache: Default::default(),
        }
    }

    /// Store the access password the frontend collected for `ip`. Held in
    /// memory only; the receive loop answers the next `AuthRequired` with
    /// it. A later `AuthResult { ok: false }` drops the entry.
    pub(crate) fn submit_access_password(&self, ip: IpAddr, password: String) {
        log::debug!("access password submitted for {ip}");
        self.pending_passwords
            .lock()
            .expect("lock")
            .insert(ip, password);
    }

    pub(crate) async fn recv(&mut self) -> HopDeckConnectionEvent {
        loop {
            let event = self.recv_rx.recv().await.expect("channel closed");
            let is_current =
                match &event {
                    HopDeckConnectionEvent::Received {
                        handle,
                        addr,
                        session,
                        ..
                    } => {
                        self.sessions.is_current(*handle, *session)
                            && self.conns.lock().await.get(addr).is_some_and(|slot| {
                                slot.handle == *handle && slot.session == *session
                            })
                    }
                    HopDeckConnectionEvent::Disconnected {
                        handle, session, ..
                    } => self
                        .sessions
                        .current(*handle)
                        .is_none_or(|current| current == *session),
                };
            if is_current {
                return event;
            }
            log::debug!("dropping queued event from a replaced outbound DTLS session");
        }
    }

    /// Return the authenticated transport generation currently selected for
    /// `handle`. Capture pins a handover and every Ack-gated input event to
    /// this value so a reconnect cannot inherit an in-flight crossing.
    pub(crate) fn current_session(&self, handle: ClientHandle) -> Option<ConnectionSession> {
        self.client_manager.active_addr(handle)?;
        self.sessions.current(handle)
    }

    /// Cheap send-only handle that shares all the dialer state with
    /// `self`. The clone's `recv_rx` is a dead stub — only the
    /// original [`HopDeckConnection`] (held by Capture) drains the
    /// live receiver. Used by Service to fan clipboard frames out
    /// without routing through the capture session loop.
    pub(crate) fn sender_clone(&self) -> Self {
        let (_, dead_rx) = channel();
        Self {
            cert: self.cert.clone(),
            client_manager: self.client_manager.clone(),
            conns: self.conns.clone(),
            sessions: self.sessions.clone(),
            connecting: self.connecting.clone(),
            recv_rx: dead_rx,
            recv_tx: self.recv_tx.clone(),
            ping_response: self.ping_response.clone(),
            ping_sent_at: self.ping_sent_at.clone(),
            primary_hints: self.primary_hints.clone(),
            reverse_adopt: self.reverse_adopt.clone(),
            retry_state: self.retry_state.clone(),
            pending_passwords: self.pending_passwords.clone(),
            transport_cache: self.transport_cache.clone(),
        }
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), HopDeckConnectionError> {
        let event_display = format!("{event}");
        // Lock-recovery status is control-plane information, not input
        // emulation. It must still reach a peer whose latest Pong reported
        // emulation unavailable (including the reconnect window before the
        // first Pong), otherwise the recovery dialog can never explain why
        // forwarding stopped.
        let send_when_emulation_inactive = matches!(&event, ProtoEvent::HostInputState { .. });
        // Clipboard and display-topology frames are variable-length and can't
        // ride the fixed-size codec; route them through dedicated helpers. For
        // all other events the existing 21-byte path is faster.
        let bytes_owned: Option<Vec<u8>> = match &event {
            ProtoEvent::Clipboard { .. } => match encode_clipboard_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping oversize clipboard event for client {handle}: {e}");
                    return Ok(());
                }
            },
            ProtoEvent::DisplayLayout { .. } => match encode_display_layout_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping invalid display layout for client {handle}: {e}");
                    return Ok(());
                }
            },
            _ => None,
        };
        let bytes_fixed: ([u8; MAX_EVENT_SIZE], usize) = if bytes_owned.is_some() {
            ([0u8; MAX_EVENT_SIZE], 0)
        } else {
            event.into()
        };
        let buf: &[u8] = if let Some(v) = bytes_owned.as_deref() {
            v
        } else {
            &bytes_fixed.0[..bytes_fixed.1]
        };
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let slot = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(slot) = slot {
                if slot.handle != handle || !self.sessions.is_current(handle, slot.session) {
                    return Err(HopDeckConnectionError::NotConnected);
                }
                if !self.client_manager.alive(handle) && !send_when_emulation_inactive {
                    return Err(HopDeckConnectionError::TargetEmulationDisabled);
                }
                match slot.conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        let removed = disconnect(
                            &self.client_manager,
                            handle,
                            addr,
                            slot.session,
                            &self.conns,
                            &self.sessions,
                            &self.ping_response,
                            &self.ping_sent_at,
                            &self.recv_tx,
                        )
                        .await;
                        // Wake the receive/keepalive tasks even if this slot
                        // was replaced between lookup and the failed send.
                        let _ = slot.conn.close().await;
                        if let Some(removed) = removed {
                            if !Arc::ptr_eq(&removed, &slot.conn) {
                                let _ = removed.close().await;
                            }
                        }
                        return Err(e.into());
                    }
                }
                log::trace!("{event_display} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains_key(&handle) && self.should_attempt(handle) {
            // A prospective session is not established yet. Clear any stale
            // address first so `current_session()` cannot expose the dial token
            // to Capture as a usable DTLS generation.
            self.client_manager.set_active_addr(handle, None);
            self.client_manager.set_alive(handle, false);
            let session = self.sessions.allocate(handle);
            connecting.insert(handle, session);
            // connect in the background
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                session,
                self.conns.clone(),
                self.sessions.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
                self.ping_sent_at.clone(),
                self.primary_hints.clone(),
                self.reverse_adopt.clone(),
                self.retry_state.clone(),
                self.pending_passwords.clone(),
                self.transport_cache.clone(),
            ));
        }
        Err(HopDeckConnectionError::NotConnected)
    }

    /// Send only on the exact already-established DTLS session. Unlike
    /// [`Self::send`], this never starts a connection and never falls through
    /// to a replacement session. It is used for atomic handover retries,
    /// Ack-gated input, and release cleanup whose meaning belongs to one
    /// particular transport generation.
    pub(crate) async fn send_on_session(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
        session: ConnectionSession,
    ) -> Result<(), HopDeckConnectionError> {
        let event_display = format!("{event}");
        let send_when_emulation_inactive = matches!(&event, ProtoEvent::HostInputState { .. });
        let session_cleanup = is_session_cleanup_event(&event);
        let bytes_owned: Option<Vec<u8>> = match &event {
            ProtoEvent::Clipboard { .. } => match encode_clipboard_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping oversize clipboard event for client {handle}: {e}");
                    return Ok(());
                }
            },
            ProtoEvent::DisplayLayout { .. } => match encode_display_layout_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping invalid display layout for client {handle}: {e}");
                    return Ok(());
                }
            },
            _ => None,
        };
        let bytes_fixed: ([u8; MAX_EVENT_SIZE], usize) = if bytes_owned.is_some() {
            ([0u8; MAX_EVENT_SIZE], 0)
        } else {
            event.into()
        };
        let buf: &[u8] = if let Some(v) = bytes_owned.as_deref() {
            v
        } else {
            &bytes_fixed.0[..bytes_fixed.1]
        };

        // Resolve the already-pinned transport directly. Client deletion and
        // hostname/port reconfiguration can clear or mutate active_addr before
        // Capture processes its asynchronous Destroy and sends key-up/Leave.
        // Looking up the exact (handle, session) slot lets that cleanup finish
        // without ever falling through to a replacement generation.
        let (addr, slot) = {
            let conns = self.conns.lock().await;
            conns.iter().find_map(|(&addr, slot)| {
                (slot.handle == handle && slot.session == session).then(|| (addr, slot.clone()))
            })
        }
        .ok_or(HopDeckConnectionError::NotConnected)?;
        if !self.sessions.is_current(handle, session) {
            return Err(HopDeckConnectionError::NotConnected);
        }
        if self.client_manager.active_addr(handle) != Some(addr) && !session_cleanup {
            return Err(HopDeckConnectionError::NotConnected);
        }
        if !self.client_manager.alive(handle) && !send_when_emulation_inactive && !session_cleanup {
            return Err(HopDeckConnectionError::TargetEmulationDisabled);
        }
        if let Err(e) = slot.conn.send(buf).await {
            log::warn!("client {handle} session {session} failed to send: {e}");
            let removed = disconnect(
                &self.client_manager,
                handle,
                addr,
                session,
                &self.conns,
                &self.sessions,
                &self.ping_response,
                &self.ping_sent_at,
                &self.recv_tx,
            )
            .await;
            let _ = slot.conn.close().await;
            if let Some(removed) = removed {
                if !Arc::ptr_eq(&removed, &slot.conn) {
                    let _ = removed.close().await;
                }
            }
            return Err(e.into());
        }
        log::trace!("{event_display} >->->->->- {addr} session {session}");
        Ok(())
    }

    /// Tear down any live connection for `handle` and clear its retry
    /// gate so the next send re-dials from scratch. Called when the
    /// user changes the locked address: the path we're on may be the
    /// wrong interface now, so we drop it and let `connect_to_handle`
    /// re-evaluate (honoring the new lock) on the next event. Closing
    /// the connection after the shared disconnect teardown has notified
    /// capture, so the local pointer is released immediately rather than
    /// waiting for `receive_loop` to observe the close.
    pub(crate) async fn reset_handle(&self, handle: ClientHandle) {
        let session = self.sessions.current(handle);
        if let Some(session) = session {
            if let Some(addr) = self.client_manager.active_addr(handle) {
                let conn = disconnect(
                    &self.client_manager,
                    handle,
                    addr,
                    session,
                    &self.conns,
                    &self.sessions,
                    &self.ping_response,
                    &self.ping_sent_at,
                    &self.recv_tx,
                )
                .await;
                if let Some(conn) = conn {
                    let _ = conn.close().await;
                }
            } else {
                // No established address means this is an in-flight dial.
                // Invalidating its prospective session prevents it from
                // installing after config deletion/handle reuse.
                self.sessions.remove_if_current(handle, session);
            }
            remove_connecting_if_current(&self.connecting, handle, session).await;
        }
        self.retry_state.borrow_mut().remove(&handle);
    }

    /// Install a listener-accepted DTLS transport as `handle`'s
    /// capture send path — the controller half of reverse-connect.
    /// The inbound session was accepted by *our* listener because a
    /// reverse-mode client dialed out to us; from here the capture
    /// pipeline treats it like any dialed connection: hello handshake,
    /// ping keepalive, and a receive loop for the control plane. The
    /// listener-side read loop for this session ends on its own once
    /// its entry is removed from the listener's table.
    ///
    /// Any dial already in flight for `handle` loses by generation:
    /// the session allocated here supersedes it, so its completion is
    /// discarded by the existing current-session check.
    pub(crate) async fn adopt_inbound(
        &self,
        handle: ClientHandle,
        conn: ArcConn,
        addr: SocketAddr,
        fingerprint: Option<String>,
        transport: TransportKind,
    ) {
        log::info!("client ({handle}) adopting inbound {transport} transport from {addr}");
        link_peer_fingerprint(
            &self.client_manager,
            handle,
            fingerprint.as_deref(),
            transport,
        );
        self.transport_cache.borrow_mut().insert(handle, transport);
        self.client_manager.set_transport(handle, Some(transport));
        let session = self.sessions.allocate(handle);
        // Input stays gated until the adopted peer completes the
        // protocol handshake and answers a ping, same as after a dial.
        self.client_manager.set_alive(handle, false);
        self.client_manager.set_active_addr(handle, Some(addr));
        let replaced = self.conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn: conn.clone(),
            },
        );
        if let Some(replaced) = replaced {
            self.ping_response
                .borrow_mut()
                .remove(&(addr, replaced.session));
            self.ping_sent_at
                .borrow_mut()
                .remove(&(addr, replaced.session));
            let _ = replaced.conn.close().await;
            if replaced.handle != handle
                && self.sessions.is_current(replaced.handle, replaced.session)
                && self.client_manager.active_addr(replaced.handle) == Some(addr)
            {
                self.client_manager.set_alive(replaced.handle, false);
                self.client_manager.set_active_addr(replaced.handle, None);
                self.client_manager.set_peer_commit(replaced.handle, None);
                self.sessions
                    .remove_if_current(replaced.handle, replaced.session);
                let _ = self.recv_tx.send(HopDeckConnectionEvent::Disconnected {
                    handle: replaced.handle,
                    session: replaced.session,
                });
            }
        }

        let hello_ok = Rc::new(Cell::new(false));
        let auth_pending = Rc::new(Cell::new(false));
        spawn_local(hello_handshake(
            addr,
            conn.clone(),
            hello_ok.clone(),
            auth_pending.clone(),
        ));

        spawn_local(ping_pong(
            self.client_manager.clone(),
            handle,
            addr,
            session,
            conn.clone(),
            self.conns.clone(),
            self.sessions.clone(),
            self.recv_tx.clone(),
            self.ping_response.clone(),
            self.ping_sent_at.clone(),
            auth_pending.clone(),
        ));

        spawn_local(receive_loop(
            self.client_manager.clone(),
            handle,
            addr,
            session,
            conn,
            self.conns.clone(),
            self.sessions.clone(),
            self.recv_tx.clone(),
            self.ping_response.clone(),
            self.ping_sent_at.clone(),
            hello_ok,
            self.pending_passwords.clone(),
            auth_pending,
        ));
    }

    /// Decide whether to spawn another `connect_to_handle` for `handle`.
    /// Returns true (and refreshes the recorded signature) when:
    ///   - we have no prior attempt for this handle, or
    ///   - the candidate-set signature has changed since the last
    ///     attempt (new IP from DNS, or new mDNS primary), or
    ///   - the recorded backoff has elapsed.
    ///
    /// Otherwise returns false; the caller treats this as "still in
    /// cooldown, keep returning NotConnected silently."
    fn should_attempt(&self, handle: ClientHandle) -> bool {
        let ips = self.client_manager.get_ips(handle).unwrap_or_default();
        let primary = self.client_manager.get_hostname(handle).and_then(|h| {
            let key = normalize_mdns_name(&h);
            self.primary_hints.borrow().get(&key).copied()
        });
        let sig = signature_of(&ips, primary);
        let mut state = self.retry_state.borrow_mut();
        match state.get_mut(&handle) {
            None => true,
            Some(s) if s.signature != sig => {
                s.signature = sig;
                s.next_attempt_at = Instant::now();
                s.backoff = INITIAL_RETRY_BACKOFF;
                true
            }
            Some(s) => Instant::now() >= s.next_attempt_at,
        }
    }
}

async fn remove_connecting_if_current(
    connecting: &Mutex<HashMap<ClientHandle, ConnectionSession>>,
    handle: ClientHandle,
    session: ConnectionSession,
) {
    let mut connecting = connecting.lock().await;
    if connecting.get(&handle) == Some(&session) {
        connecting.remove(&handle);
    }
}

async fn finish_failed_dial(
    connecting: &Mutex<HashMap<ClientHandle, ConnectionSession>>,
    sessions: &SessionTracker,
    handle: ClientHandle,
    session: ConnectionSession,
) {
    sessions.remove_if_current(handle, session);
    remove_connecting_if_current(connecting, handle, session).await;
}

/// Link the authenticated peer certificate to the client entry so the
/// dialing side's trust view works like the listener's (which records
/// the same fingerprint at `Accept`). Without this, a dialer whose
/// authorized entry went stale shows 「未信任」 even mid-session.
/// `set_peer_fingerprint` marks the client dirty; the service's
/// periodic frontend sync re-broadcasts it — no dedicated event.
fn link_peer_fingerprint(
    client_manager: &ClientManager,
    handle: ClientHandle,
    fingerprint: Option<&str>,
    transport: TransportKind,
) {
    let Some(fingerprint) = fingerprint else {
        return;
    };
    if client_manager.set_peer_fingerprint(handle, Some(fingerprint.to_owned())) {
        log::info!(
            "client {handle}: peer fingerprint {fingerprint} linked from the {transport} session"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    session: ConnectionSession,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    connecting: Rc<Mutex<HashMap<ClientHandle, ConnectionSession>>>,
    tx: Sender<HopDeckConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
    primary_hints: PrimaryCache,
    reverse_adopt: Option<Sender<EmulationRequest>>,
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    pending_passwords: Rc<std::sync::Mutex<HashMap<IpAddr, String>>>,
    transport_cache: Rc<RefCell<HashMap<ClientHandle, TransportKind>>>,
) -> Result<(), HopDeckConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(ips_set) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = ips_set
            .iter()
            .copied()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        // mDNS-advertised primary IP for this peer, if known. Used
        // by `connect_any` as a head-start address: the dialer races
        // it alone for ~200ms before joining the rest of the list,
        // so a healthy primary almost always wins regardless of
        // raw RTT ordering.
        let primary_ip = client_manager.get_hostname(handle).and_then(|h| {
            let key = normalize_mdns_name(&h);
            primary_hints.borrow().get(&key).copied()
        });
        let primary_preferred = primary_ip.map(|ip| SocketAddr::new(ip, port));
        // Resolve the connection policy for this peer:
        //  * a per-network lock (already resolved against the current
        //    LAN in `active_lock`) pins the dial set to one address, so
        //    a dual-homed peer stops flapping between interfaces. Sticky
        //    — an unreachable locked address fails rather than silently
        //    falling back to another interface.
        //  * otherwise the base mode decides: `Auto` races every
        //    candidate biased to the mDNS primary; `Fastest` biases the
        //    head-start toward the lowest-latency reachable candidate
        //    (falling back to the mDNS primary before any probe lands).
        let (addrs, preferred) = match client_manager.get_active_lock(handle) {
            Some(ip) => {
                let sa = SocketAddr::new(ip, port);
                (vec![sa], Some(sa))
            }
            None => match client_manager.get_mode(handle) {
                ConnectionMode::Auto => (addrs, primary_preferred),
                ConnectionMode::Fastest => {
                    let fastest = client_manager
                        .lowest_latency_addr(handle)
                        .map(|ip| SocketAddr::new(ip, port));
                    (addrs, fastest.or(primary_preferred))
                }
            },
        };
        let mode = client_manager.transport_mode(handle);
        let cached = transport_cache.borrow().get(&handle).copied();
        let plan = transport_plan(mode, cached);
        log::info!(
            "client ({handle}) connecting ... (ips: {addrs:?}, preferred: {preferred:?}, \
             transport: {mode}, plan: {plan:?})"
        );
        if addrs.is_empty() && preferred.is_none() {
            // Nothing to dial. Bump backoff and bail without spawning
            // DTLS/TLS work or spamming logs on every subsequent mouse
            // event — `should_attempt` will keep gating until either
            // the backoff elapses or new info arrives.
            record_retry_failure(&retry_state, handle, &ips_set, primary_ip);
            finish_failed_dial(&connecting, &sessions, handle, session).await;
            return Err(HopDeckConnectionError::NotConnected);
        }
        let (result, attempted) =
            dial_transports(&addrs, preferred, cert.clone(), mode, cached).await;
        log::debug!("client ({handle}) attempted transports: {attempted:?}");
        let Dialed {
            conn,
            addr,
            transport,
            peer_fingerprint,
        } = match result {
            Ok(dialed) => dialed,
            Err(e) => {
                record_retry_failure(&retry_state, handle, &ips_set, primary_ip);
                finish_failed_dial(&connecting, &sessions, handle, session).await;
                return Err(e);
            }
        };
        let mut conns_guard = conns.lock().await;
        if !sessions.is_current(handle, session) || client_manager.get_state(handle).is_none() {
            drop(conns_guard);
            finish_failed_dial(&connecting, &sessions, handle, session).await;
            let _ = conn.close().await;
            log::debug!(
                "discarding completed dial for reset/replaced client {handle} session {session}"
            );
            return Err(HopDeckConnectionError::NotConnected);
        }
        log::info!("client ({handle}) connected over {transport} @ {addr}");
        link_peer_fingerprint(
            &client_manager,
            handle,
            peer_fingerprint.as_deref(),
            transport,
        );
        // Remember the transport that worked so a reconnect on this
        // network skips the failed path (Auto dials the cached one first).
        transport_cache.borrow_mut().insert(handle, transport);
        client_manager.set_transport(handle, Some(transport));

        // Reverse-connect: this host dials *out* to its controller
        // because the controller cannot reach it inbound, and the
        // controlled side is the one that injects — so the finished
        // transport is handed to the listener, whose read loop becomes
        // the connection's only reader. Capture-side bookkeeping is
        // still installed so the reverse keepalive tick sees a live
        // session (and re-dials after a drop) instead of finding
        // nothing. `alive` is set without a Pong round-trip here: this
        // side runs no receive loop to process one, and liveness is
        // enforced by the listener heartbeat plus the controller's
        // ping cycle.
        if client_manager.connect_mode(handle) == ConnectMode::Reverse {
            client_manager.set_alive(handle, true);
            client_manager.set_active_addr(handle, Some(addr));
            let replaced = conns_guard.insert(
                addr,
                ConnectionSlot {
                    handle,
                    session,
                    conn: conn.clone(),
                },
            );
            drop(conns_guard);
            if let Some(replaced) = replaced {
                ping_response.borrow_mut().remove(&(addr, replaced.session));
                ping_sent_at.borrow_mut().remove(&(addr, replaced.session));
                log::info!(
                    "replacing prior outbound session {} for {addr}",
                    replaced.session
                );
                let _ = replaced.conn.close().await;
            }
            remove_connecting_if_current(&connecting, handle, session).await;
            retry_state.borrow_mut().remove(&handle);
            // A reverse session is adopted by our own listener, which
            // surfaces it like any inbound accept — including the peer
            // fingerprint for the trust view. Without one we cannot
            // register it; fail cleanly instead of leaving a slot the
            // listener never reads.
            let Some(adopt_fingerprint) = peer_fingerprint.clone() else {
                log::warn!(
                    "client {handle} reverse {transport} session presented no peer \
                     certificate; closing dialed connection"
                );
                let _ = conn.close().await;
                sessions.remove_if_current(handle, session);
                client_manager.set_alive(handle, false);
                client_manager.set_active_addr(handle, None);
                client_manager.set_transport(handle, None);
                return Ok(());
            };
            match &reverse_adopt {
                Some(adopt) => adopt
                    .send(EmulationRequest::AdoptReverse {
                        addr,
                        conn: conn.clone(),
                        fingerprint: adopt_fingerprint,
                        transport,
                    })
                    .expect("channel closed"),
                None => {
                    // No adopt channel wired (should not happen in a
                    // wired service): fail the session cleanly instead
                    // of leaving a slot nobody reads.
                    log::warn!(
                        "client {handle} is reverse-mode but no adopt channel is wired; \
                         closing dialed connection"
                    );
                    let _ = conn.close().await;
                    sessions.remove_if_current(handle, session);
                    client_manager.set_alive(handle, false);
                    client_manager.set_active_addr(handle, None);
                    client_manager.set_transport(handle, None);
                }
            }
            return Ok(());
        }

        // `alive` belongs to the authenticated protocol session, not merely
        // this client handle. A reconnect can otherwise inherit `true` from
        // the previous connection and send input before the new peer has
        // completed Hello/Pong validation.
        client_manager.set_alive(handle, false);
        client_manager.set_active_addr(handle, Some(addr));
        let replaced = conns_guard.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn: conn.clone(),
            },
        );
        drop(conns_guard);
        if let Some(replaced) = replaced {
            ping_response.borrow_mut().remove(&(addr, replaced.session));
            ping_sent_at.borrow_mut().remove(&(addr, replaced.session));
            log::info!(
                "replacing prior outbound session {} for {addr}",
                replaced.session
            );
            let _ = replaced.conn.close().await;
            if replaced.handle != handle
                && sessions.is_current(replaced.handle, replaced.session)
                && client_manager.active_addr(replaced.handle) == Some(addr)
            {
                client_manager.set_alive(replaced.handle, false);
                client_manager.set_active_addr(replaced.handle, None);
                client_manager.set_peer_commit(replaced.handle, None);
                sessions.remove_if_current(replaced.handle, replaced.session);
                let _ = tx.send(HopDeckConnectionEvent::Disconnected {
                    handle: replaced.handle,
                    session: replaced.session,
                });
            }
        }
        remove_connecting_if_current(&connecting, handle, session).await;
        retry_state.borrow_mut().remove(&handle);

        // Protocol handshake. hopdeck refuses any peer that does not
        // present a valid `Hello` (carrying `PROTOCOL_MAGIC`) shortly
        // after the DTLS connection authenticates — a deliberate hard
        // cut-over so hopdeck never silently half-interoperates with
        // lan-mouse. `receive_loop` flips `hello_ok` once the peer's
        // echoed Hello validates; `hello_handshake` retransmits until
        // then and tears the connection down if the window elapses.
        // An access-password challenge (peer's `AuthRequired`) suspends
        // the window while the user answers it.
        let hello_ok = Rc::new(Cell::new(false));
        let auth_pending = Rc::new(Cell::new(false));
        spawn_local(hello_handshake(
            addr,
            conn.clone(),
            hello_ok.clone(),
            auth_pending.clone(),
        ));

        // poll connection for active
        spawn_local(ping_pong(
            client_manager.clone(),
            handle,
            addr,
            session,
            conn.clone(),
            conns.clone(),
            sessions.clone(),
            tx.clone(),
            ping_response.clone(),
            ping_sent_at.clone(),
            auth_pending.clone(),
        ));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            session,
            conn,
            conns,
            sessions,
            tx,
            ping_response.clone(),
            ping_sent_at,
            hello_ok,
            pending_passwords,
            auth_pending,
        ));
        return Ok(());
    }
    finish_failed_dial(&connecting, &sessions, handle, session).await;
    Err(HopDeckConnectionError::NotConnected)
}

/// Number of times the connect side retransmits its `Hello` while
/// waiting for the peer to echo a valid one back, and the gap
/// between attempts. Their product is the effective handshake
/// deadline: if `hello_ok` is still unset after the final attempt
/// the peer never spoke a valid hopdeck handshake and the
/// connection is closed.
const HELLO_MAX_ATTEMPTS: u32 = 8;
const HELLO_RETRY_INTERVAL: Duration = Duration::from_millis(750);

/// Hard ceiling on the whole handshake while an access-password challenge
/// is open (manual password entry) — past it the connection is closed even
/// if the challenge is technically still pending. Covers the dialing
/// side's password wait ([`AUTH_PASSWORD_WAIT`]) plus slack.
const HELLO_TOTAL_DEADLINE: Duration = Duration::from_secs(150);

/// How long the frontend may take to submit the access password after the
/// peer issued its challenge. Mirrors the listen-side
/// `AUTH_CHALLENGE_TIMEOUT`.
const AUTH_PASSWORD_WAIT: Duration = Duration::from_secs(120);

/// Drive the protocol handshake on a freshly-connected outbound DTLS
/// link. Retransmits our [`ProtoEvent::hello`] until `receive_loop`
/// flips `hello_ok` (the peer echoed a `PROTOCOL_MAGIC`-stamped
/// Hello) or the attempt budget runs out. A peer that never returns
/// a valid Hello — a stock lan-mouse, or anything that is not
/// hopdeck — has its connection refused here. This is the
/// connect-side half of the deliberate hard cut-over from lan-mouse.
/// While an access-password challenge is in flight (`auth_pending`),
/// the attempt budget is suspended and the total-wait ceiling applies
/// instead.
async fn hello_handshake(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    hello_ok: Rc<Cell<bool>>,
    auth_pending: Rc<Cell<bool>>,
) {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::hello(local_commit()).into();
    let started = Instant::now();
    let mut attempts = 0;
    while !hello_ok.get() {
        if auth_pending.get() {
            if started.elapsed() >= HELLO_TOTAL_DEADLINE {
                break;
            }
            tokio::time::sleep(HELLO_RETRY_INTERVAL).await;
            continue;
        }
        if attempts >= HELLO_MAX_ATTEMPTS {
            break;
        }
        attempts += 1;
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }
        tokio::time::sleep(HELLO_RETRY_INTERVAL).await;
    }
    if !hello_ok.get() {
        log::warn!(
            "refusing {addr}: peer did not complete the hopdeck handshake \
             (no valid Hello) — closing connection"
        );
        let _ = conn.close().await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn ping_pong(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
    conn: ArcConn,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    tx: Sender<HopDeckConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
    auth_pending: Rc<Cell<bool>>,
) {
    loop {
        if client_manager.get_state(handle).is_none()
            || !connection_is_current(&conns, &sessions, handle, addr, session).await
        {
            disconnect(
                &client_manager,
                handle,
                addr,
                session,
                &conns,
                &sessions,
                &ping_response,
                &ping_sent_at,
                &tx,
            )
            .await;
            let _ = conn.close().await;
            return;
        }
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if client_manager.get_state(handle).is_none()
                || !connection_is_current(&conns, &sessions, handle, addr, session).await
            {
                disconnect(
                    &client_manager,
                    handle,
                    addr,
                    session,
                    &conns,
                    &sessions,
                    &ping_response,
                    &ping_sent_at,
                    &tx,
                )
                .await;
                let _ = conn.close().await;
                return;
            }
            // Stamp the send time so `receive_loop` can derive the live
            // RTT from the matching Pong. On a LAN the Pong returns well
            // within the 500 ms inter-ping gap, so the most-recent stamp
            // is the one being answered.
            ping_sent_at
                .borrow_mut()
                .insert((addr, session), Instant::now());
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                disconnect(
                    &client_manager,
                    handle,
                    addr,
                    session,
                    &conns,
                    &sessions,
                    &ping_response,
                    &ping_sent_at,
                    &tx,
                )
                .await;
                let _ = conn.close().await;
                return;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&(addr, session)) {
            if auth_pending.get() {
                // An access-password challenge is in flight: the peer drops
                // everything but the auth exchange pre-handshake, so the
                // unanswered ping round must not tear the connection down.
                log::debug!(
                    "{addr} ping round unanswered while access-password auth \
                     is in flight; retrying"
                );
                continue;
            }
            log::warn!("{addr} did not respond, closing connection");
            disconnect(
                &client_manager,
                handle,
                addr,
                session,
                &conns,
                &sessions,
                &ping_response,
                &ping_sent_at,
                &tx,
            )
            .await;
            let _ = conn.close().await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
    conn: ArcConn,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    tx: Sender<HopDeckConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
    hello_ok: Rc<Cell<bool>>,
    pending_passwords: Rc<std::sync::Mutex<HashMap<IpAddr, String>>>,
    auth_pending: Rc<Cell<bool>>,
) {
    // Buffer sized for the largest legal variable-length frame so a single
    // DTLS recv never gets truncated. Fixed events use only the first
    // MAX_EVENT_SIZE bytes; topology remains below the clipboard cap.
    let mut buf = [0u8; MAX_CLIPBOARD_SIZE];
    while let Ok(n) = conn.recv(&mut buf).await {
        if n == 0 {
            continue;
        }
        let datagram = &buf[..n];
        let event = match decode_proto_datagram(datagram) {
            Some(event) => event,
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            None => {
                log::debug!("ignoring undecodable {n}-byte event from {addr}");
                continue;
            }
        };
        log::trace!("{addr} <==<==<== {event}");
        if client_manager.get_state(handle).is_none()
            || !connection_is_current(&conns, &sessions, handle, addr, session).await
        {
            log::debug!("ending replaced outbound DTLS receive session {session} for {addr}");
            break;
        }
        if !handshake_allows_event(hello_ok.get(), &event) {
            log::debug!("ignoring pre-Hello event from {addr}: {event}");
            continue;
        }
        match event {
            ProtoEvent::AuthRequired { salt, nonce } => {
                // The peer requires the access password before completing
                // the handshake. Surface the requirement to the frontend
                // (password dialog), then answer as soon as a password is
                // submitted — the hello/keepalive windows are suspended
                // for the duration.
                auth_pending.set(true);
                let asked_at = Instant::now();
                tx.send(HopDeckConnectionEvent::Received {
                    handle,
                    addr,
                    session,
                    event: ProtoEvent::AuthRequired { salt, nonce },
                })
                .expect("channel closed");
                let password = loop {
                    let submitted = pending_passwords
                        .lock()
                        .expect("lock")
                        .get(&addr.ip())
                        .cloned();
                    if let Some(password) = submitted {
                        break Some(password);
                    }
                    if asked_at.elapsed() >= AUTH_PASSWORD_WAIT
                        || client_manager.get_state(handle).is_none()
                        || !connection_is_current(&conns, &sessions, handle, addr, session).await
                    {
                        break None;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                };
                match password {
                    Some(password) => {
                        log::info!("{addr}: answering the access-password challenge");
                        let proof = crypto::access_password_proof(&salt, &nonce, &password);
                        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) =
                            ProtoEvent::AuthPassword { proof }.into();
                        if let Err(e) = conn.send(&buf[..len]).await {
                            log::warn!("{addr}: failed to send the password answer: {e}");
                            break;
                        }
                        // Stay in `auth_pending`: the peer either echoes a
                        // Hello (accepted) or replies AuthResult { ok: false }.
                    }
                    None => {
                        log::warn!(
                            "{addr}: no access password submitted within \
                             {AUTH_PASSWORD_WAIT:?} — closing connection"
                        );
                        let _ = conn.close().await;
                        break;
                    }
                }
            }
            ProtoEvent::AuthResult { ok } => {
                if ok {
                    log::info!("{addr} accepted the access password; completing handshake");
                    // hello_handshake resumes and retransmits the Hello;
                    // the peer echoes it through the normal path.
                    auth_pending.set(false);
                } else {
                    log::warn!("{addr} rejected the access password — closing connection");
                    // Drop the wrong answer so the next attempt prompts again.
                    pending_passwords.lock().expect("lock").remove(&addr.ip());
                    auth_pending.set(false);
                    let _ = conn.close().await;
                    break;
                }
            }
            ProtoEvent::Pong(b) => {
                client_manager.set_active_addr(handle, Some(addr));
                client_manager.set_alive(handle, b);
                ping_response.borrow_mut().insert((addr, session));
                // Live RTT of the active connection over the real DTLS
                // path — accurate and firewall-proof (unlike the TCP
                // probe). Quantize to 100 µs to match the prober.
                if let Some(sent) = ping_sent_at.borrow_mut().remove(&(addr, session)) {
                    let us = sent.elapsed().as_micros().min(u32::MAX as u128) as u32;
                    client_manager.set_latency(handle, addr.ip(), Some(us - (us % 100)));
                }
            }
            ProtoEvent::Hello {
                magic,
                commit,
                capabilities,
            } => {
                if magic != PROTOCOL_MAGIC {
                    log::warn!(
                        "refusing {addr}: peer presented a foreign protocol \
                         handshake (not hopdeck) — closing connection"
                    );
                    let _ = conn.close().await;
                    break;
                }
                hello_ok.set(true);
                client_manager.set_peer_commit(handle, Some(commit));
                // Forward to capture.rs so Service can
                // broadcast — without this the GUI's
                // version-status indicator only updates when
                // the listen-side `PeerHello` happens to
                // match `get_client(addr)`, which fails when
                // Mac dials in before Linux's outbound dial
                // has populated `active_addr`.
                tx.send(HopDeckConnectionEvent::Received {
                    handle,
                    addr,
                    session,
                    event: ProtoEvent::Hello {
                        magic,
                        commit,
                        capabilities,
                    },
                })
                .expect("channel closed");
            }
            event => tx
                .send(HopDeckConnectionEvent::Received {
                    handle,
                    addr,
                    session,
                    event,
                })
                .expect("channel closed"),
        }
    }
    log::debug!("{addr}: receive loop ended");
    disconnect(
        &client_manager,
        handle,
        addr,
        session,
        &conns,
        &sessions,
        &ping_response,
        &ping_sent_at,
        &tx,
    )
    .await;
}

/// Classify the first byte of a DTLS datagram and dispatch through
/// a variable-length codec or the fixed-buffer `try_into` path. Returns
/// `None` on any decode failure (bad tag, truncated payload, oversize frame).
fn decode_proto_datagram(bytes: &[u8]) -> Option<ProtoEvent> {
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

async fn connection_is_current(
    conns: &Mutex<HashMap<SocketAddr, ConnectionSlot>>,
    sessions: &SessionTracker,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
) -> bool {
    sessions.is_current(handle, session)
        && conns
            .lock()
            .await
            .get(&addr)
            .is_some_and(|slot| slot.handle == handle && slot.session == session)
}

#[allow(clippy::too_many_arguments)]
async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
    conns: &Mutex<HashMap<SocketAddr, ConnectionSlot>>,
    sessions: &SessionTracker,
    ping_response: &RefCell<HashSet<(SocketAddr, ConnectionSession)>>,
    ping_sent_at: &RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>,
    tx: &Sender<HopDeckConnectionEvent>,
) -> Option<ArcConn> {
    log::warn!("client ({handle}) @ {addr} session {session} connection closed");
    clear_session_ping_state(ping_response, ping_sent_at, addr, session);
    let mut conns = conns.lock().await;
    let removed = match conns.get(&addr) {
        Some(slot) if slot.handle == handle && slot.session == session => {
            conns.remove(&addr).map(|slot| slot.conn)
        }
        Some(_) => {
            log::debug!(
                "ignoring stale disconnect for client ({handle}) @ {addr}: session {session} was replaced"
            );
            return None;
        }
        None => None,
    };
    let active: Vec<SocketAddr> = conns.keys().copied().collect();
    drop(conns);

    // A receive task from an older address can finish after a replacement
    // connection has already become active. Only the task that owned the
    // current address may clear client state or release capture.
    let manager_removed = client_manager.get_state(handle).is_none();
    if sessions.is_current(handle, session)
        && (manager_removed || client_manager.active_addr(handle) == Some(addr))
    {
        if !manager_removed {
            client_manager.set_alive(handle, false);
            client_manager.set_active_addr(handle, None);
            client_manager.set_peer_commit(handle, None);
            client_manager.set_transport(handle, None);
        }
        sessions.remove_if_current(handle, session);
        let _ = tx.send(HopDeckConnectionEvent::Disconnected { handle, session });
    }
    log::info!("active connections: {active:?}");
    removed
}

fn clear_session_ping_state(
    ping_response: &RefCell<HashSet<(SocketAddr, ConnectionSession)>>,
    ping_sent_at: &RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>,
    addr: SocketAddr,
    session: ConnectionSession,
) {
    ping_response.borrow_mut().remove(&(addr, session));
    ping_sent_at.borrow_mut().remove(&(addr, session));
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::{Event, KeyboardEvent, PointerEvent, display::DisplayLayout};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use webrtc_dtls::config::ClientAuthType::RequireAnyClientCert;
    use webrtc_util::conn::Listener;

    fn connection_map() -> Mutex<HashMap<SocketAddr, ConnectionSlot>> {
        Mutex::new(HashMap::new())
    }

    #[test]
    fn transport_plan_matches_the_configured_mode() {
        // Auto: UDP first with no history; a cached TCP answer goes first
        // but keeps one UDP retry.
        assert_eq!(
            transport_plan(TransportMode::Auto, None),
            vec![TransportKind::Udp, TransportKind::Tcp]
        );
        assert_eq!(
            transport_plan(TransportMode::Auto, Some(TransportKind::Udp)),
            vec![TransportKind::Udp, TransportKind::Tcp]
        );
        assert_eq!(
            transport_plan(TransportMode::Auto, Some(TransportKind::Tcp)),
            vec![TransportKind::Tcp, TransportKind::Udp]
        );
        // Forced modes never touch the other transport.
        assert_eq!(
            transport_plan(TransportMode::Udp, Some(TransportKind::Tcp)),
            vec![TransportKind::Udp]
        );
        assert_eq!(
            transport_plan(TransportMode::Tcp, Some(TransportKind::Udp)),
            vec![TransportKind::Tcp]
        );
    }

    #[test]
    fn only_pure_transport_failures_allow_a_fallback() {
        let mut timeout = DialFailures::default();
        timeout.record(HopDeckConnectionError::Timeout);
        assert!(timeout.transport_unreachable(), "timeout → fallback");

        let mut refused = DialFailures::default();
        refused.record(HopDeckConnectionError::Bind(io::Error::from(
            io::ErrorKind::ConnectionRefused,
        )));
        assert!(refused.transport_unreachable(), "refused → fallback");

        // A peer that answered with a fatal DTLS alert is reachable; a TCP
        // retry would only repeat the refusal (and the authorization
        // prompt).
        let mut rejected = DialFailures::default();
        rejected.record(HopDeckConnectionError::Dtls(
            webrtc_dtls::Error::ErrAlertFatalOrClose,
        ));
        assert!(!rejected.transport_unreachable(), "alert → no fallback");

        let mut cert_rejected = DialFailures::default();
        cert_rejected.record(HopDeckConnectionError::Dtls(
            webrtc_dtls::Error::ErrVerifyDataMismatch,
        ));
        assert!(
            !cert_rejected.transport_unreachable(),
            "verify mismatch → no fallback"
        );

        // One address timing out and another refusing the certificate
        // means the peer is reachable: the rejection wins.
        let mut mixed = DialFailures::default();
        mixed.record(HopDeckConnectionError::Timeout);
        mixed.record(HopDeckConnectionError::Dtls(
            webrtc_dtls::Error::ErrVerifyDataMismatch,
        ));
        assert!(!mixed.transport_unreachable());
    }

    #[test]
    fn tls_and_emulation_refusals_are_not_transport_failures() {
        assert!(!HopDeckConnectionError::TargetEmulationDisabled.is_transport_failure());
        assert!(
            !HopDeckConnectionError::Webrtc(webrtc_util::Error::Other("local".into()))
                .is_transport_failure()
        );
        assert!(
            HopDeckConnectionError::Tcp(io::Error::from(io::ErrorKind::HostUnreachable))
                .is_transport_failure()
        );
    }

    /// The core fallback acceptance case end to end over loopback: nothing
    /// answers on UDP, TLS/TCP does, `Auto` selects TCP, and the wire
    /// framing carries real protocol events in both directions.
    #[tokio::test]
    async fn auto_falls_back_to_tcp_and_frames_round_trip() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server_cert =
                    Certificate::generate_self_signed(["hopdeck-tcp-server".to_owned()])
                        .expect("server certificate");
                let client_cert =
                    Certificate::generate_self_signed(["hopdeck-tcp-client".to_owned()])
                        .expect("client certificate");
                let server_fingerprint = crypto::certificate_fingerprint(&server_cert);

                let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind tcp");
                let addr = tcp.local_addr().expect("tcp address");
                let acceptor = TlsAcceptor::from(Arc::new(
                    tunnel::tls::server_config(&server_cert).expect("server TLS config"),
                ));
                let server = tokio::spawn(async move {
                    let (stream, _) = tcp.accept().await.expect("accept tcp");
                    let stream = acceptor.accept(stream).await.expect("tls accept");
                    let conn = tcp_transport::into_conn(
                        tcp_transport::server_conn(stream).expect("wrap tls stream"),
                    );
                    let mut buf = [0u8; MAX_EVENT_SIZE];
                    let n = conn.recv(&mut buf).await.expect("recv hello");
                    let hello = decode_fixed_event(&buf[..n]).expect("decode hello");
                    let (pong, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Pong(true).into();
                    conn.send(&pong[..len]).await.expect("send pong");
                    hello
                });

                // `addr` has no UDP listener: the UDP dial is refused
                // immediately on loopback, so the test is not waiting out
                // the 2s auto budget.
                let (result, attempted) =
                    dial_transports(&[addr], None, client_cert, TransportMode::Auto, None).await;
                assert_eq!(
                    attempted,
                    vec![TransportKind::Udp, TransportKind::Tcp],
                    "UDP must be tried first, TCP as the fallback"
                );
                let dialed = result.expect("auto dial over TCP");
                assert_eq!(dialed.transport, TransportKind::Tcp);
                assert_eq!(dialed.addr, addr);
                assert_eq!(
                    dialed.peer_fingerprint.as_deref(),
                    Some(server_fingerprint.as_str()),
                    "the dialed side records the server's leaf fingerprint"
                );

                let (hello, len): ([u8; MAX_EVENT_SIZE], usize) =
                    ProtoEvent::hello(*b"deadbeef").into();
                dialed.conn.send(&hello[..len]).await.expect("send hello");
                let mut buf = [0u8; MAX_EVENT_SIZE];
                let n = dialed.conn.recv(&mut buf).await.expect("recv pong");
                assert!(matches!(
                    decode_fixed_event(&buf[..n]),
                    Ok(ProtoEvent::Pong(true))
                ));

                let server_saw = server.await.expect("server task");
                assert!(matches!(
                    server_saw,
                    ProtoEvent::Hello { magic, .. } if magic == PROTOCOL_MAGIC
                ));
            })
            .await;
    }

    /// UDP stays the preferred path when it answers: the dial completes on
    /// UDP and never touches the TCP fallback.
    #[tokio::test]
    async fn auto_stays_on_udp_when_the_peer_answers() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server_cert =
                    Certificate::generate_self_signed(["hopdeck-udp-server".to_owned()])
                        .expect("server certificate");
                let client_cert =
                    Certificate::generate_self_signed(["hopdeck-udp-client".to_owned()])
                        .expect("client certificate");
                let server_fingerprint = crypto::certificate_fingerprint(&server_cert);

                let config = Config {
                    certificates: vec![server_cert],
                    extended_master_secret: ExtendedMasterSecretType::Require,
                    client_auth: RequireAnyClientCert,
                    verify_peer_certificate: Some(Arc::new(|_, _| Ok(()))),
                    ..Default::default()
                };
                let listen_addr: SocketAddr = "127.0.0.1:0".parse().expect("listen address");
                let listener = webrtc_dtls::listener::listen(listen_addr, config)
                    .await
                    .expect("listen udp");
                let addr = listener.addr().await.expect("dtls address");
                let server = tokio::spawn(async move {
                    let (conn, _peer) = listener.accept().await.expect("dtls accept");
                    let mut buf = [0u8; MAX_EVENT_SIZE];
                    conn.recv(&mut buf).await.expect("recv over dtls");
                    conn
                });

                let (result, attempted) =
                    dial_transports(&[addr], None, client_cert, TransportMode::Auto, None).await;
                assert_eq!(
                    attempted,
                    vec![TransportKind::Udp],
                    "a healthy UDP peer must not trigger the TCP dial"
                );
                let dialed = result.expect("auto dial over UDP");
                assert_eq!(dialed.transport, TransportKind::Udp);
                assert_eq!(
                    dialed.peer_fingerprint.as_deref(),
                    Some(server_fingerprint.as_str())
                );

                let (ping, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ping.into();
                dialed.conn.send(&ping[..len]).await.expect("send ping");
                let _server_conn = server.await.expect("server task");
            })
            .await;
    }

    async fn test_conn() -> ArcConn {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("test socket"))
    }

    async fn connected_test_conn() -> (ArcConn, UdpSocket, SocketAddr) {
        let receiver = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("receiver socket");
        let addr = receiver.local_addr().expect("receiver address");
        let sender = UdpSocket::bind("127.0.0.1:0").await.expect("sender socket");
        sender.connect(addr).await.expect("connect sender");
        (Arc::new(sender), receiver, addr)
    }

    #[test]
    fn variable_topology_datagram_routes_through_display_codec() {
        let layout = DisplayLayout::new([(-1024, 0, 1024, 600), (0, 0, 3072, 1728)]);
        let bytes = encode_display_layout_event(&ProtoEvent::display_layout(layout.clone()))
            .expect("encode topology");
        assert!(matches!(
            decode_proto_datagram(&bytes),
            Some(ProtoEvent::DisplayLayout { layout: decoded, .. }) if decoded == layout
        ));

        let mut malformed = bytes;
        malformed[1] = hopdeck_proto::DISPLAY_LAYOUT_VERSION.wrapping_add(1);
        assert!(decode_proto_datagram(&malformed).is_none());
    }

    #[test]
    fn handshake_rejects_all_non_hello_events_until_validated() {
        let input = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 1,
            dx: 2.0,
            dy: 3.0,
        }));
        for event in [ProtoEvent::Ack(0), ProtoEvent::Pong(true), input] {
            assert!(!handshake_allows_event(false, &event));
            assert!(handshake_allows_event(true, &event));
        }
        assert!(handshake_allows_event(
            false,
            &ProtoEvent::hello(*b"deadbeef")
        ));
        // The access-password exchange must pass the gate pre-handshake:
        // the challenge arrives before any valid Hello, and the answer
        // flows back the same way.
        assert!(handshake_allows_event(
            false,
            &ProtoEvent::AuthRequired {
                salt: [0; 16],
                nonce: [0; 16],
            }
        ));
        assert!(handshake_allows_event(
            false,
            &ProtoEvent::AuthPassword { proof: [0; 32] }
        ));
        assert!(handshake_allows_event(
            false,
            &ProtoEvent::AuthResult { ok: true }
        ));
    }

    #[tokio::test]
    async fn active_disconnect_notifies_capture_even_if_map_was_already_drained() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        client_manager.set_peer_commit(handle, Some(*b"deadbeef"));
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([(addr, session)]));
        let ping_sent_at = RefCell::new(HashMap::from([((addr, session), Instant::now())]));
        let (tx, mut rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            addr,
            session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_none());
        assert!(matches!(
            rx.recv().await,
            Some(HopDeckConnectionEvent::Disconnected { handle: disconnected, session: disconnected_session })
                if disconnected == handle && disconnected_session == session
        ));
        assert_eq!(client_manager.active_addr(handle), None);
        assert!(!client_manager.alive(handle));
        assert_eq!(sessions.current(handle), None);
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert_eq!(
            client_manager
                .get_state(handle)
                .expect("client state")
                .1
                .peer_commit,
            None
        );
    }

    #[tokio::test]
    async fn matching_connection_identity_removes_and_notifies() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let current = test_conn().await;
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([(addr, session)]));
        let ping_sent_at = RefCell::new(HashMap::from([((addr, session), Instant::now())]));
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn: current.clone(),
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        let (tx, mut rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            addr,
            session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_some_and(|conn| Arc::ptr_eq(&conn, &current)));
        assert!(matches!(
            rx.recv().await,
            Some(HopDeckConnectionEvent::Disconnected { handle: disconnected, session: disconnected_session })
                if disconnected == handle && disconnected_session == session
        ));
        assert_eq!(client_manager.active_addr(handle), None);
        assert!(!client_manager.alive(handle));
        assert_eq!(sessions.current(handle), None);
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert!(conns.lock().await.get(&addr).is_none());
    }

    #[tokio::test]
    async fn stale_disconnect_does_not_clear_replacement_connection() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let old_addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let new_addr: SocketAddr = "127.0.0.2:4242".parse().expect("test address");
        let commit = *b"cafebabe";
        client_manager.set_active_addr(handle, Some(new_addr));
        client_manager.set_alive(handle, true);
        client_manager.set_peer_commit(handle, Some(commit));
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([(old_addr, session)]));
        let ping_sent_at = RefCell::new(HashMap::from([((old_addr, session), Instant::now())]));
        let (tx, _rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            old_addr,
            session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_none());
        assert_eq!(client_manager.active_addr(handle), Some(new_addr));
        assert!(client_manager.alive(handle));
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert_eq!(
            client_manager
                .get_state(handle)
                .expect("client state")
                .1
                .peer_commit,
            Some(commit)
        );
    }

    #[tokio::test]
    async fn stale_same_address_disconnect_preserves_replacement_connection() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let replacement = test_conn().await;
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let old_session = sessions.allocate(handle);
        let replacement_session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([
            (addr, old_session),
            (addr, replacement_session),
        ]));
        let ping_sent_at = RefCell::new(HashMap::from([
            ((addr, old_session), Instant::now()),
            ((addr, replacement_session), Instant::now()),
        ]));
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session: replacement_session,
                conn: replacement.clone(),
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        let commit = *b"cafebabe";
        client_manager.set_peer_commit(handle, Some(commit));
        let (tx, _rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            addr,
            old_session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_none());
        assert_eq!(client_manager.active_addr(handle), Some(addr));
        assert!(client_manager.alive(handle));
        assert_eq!(
            ping_response.borrow().iter().copied().collect::<Vec<_>>(),
            vec![(addr, replacement_session)]
        );
        assert_eq!(ping_sent_at.borrow().len(), 1);
        assert!(
            ping_sent_at
                .borrow()
                .contains_key(&(addr, replacement_session))
        );
        assert_eq!(
            client_manager
                .get_state(handle)
                .expect("client state")
                .1
                .peer_commit,
            Some(commit)
        );
        let current = conns
            .lock()
            .await
            .get(&addr)
            .cloned()
            .expect("replacement remains");
        assert_eq!(current.session, replacement_session);
        assert!(Arc::ptr_eq(&current.conn, &replacement));
    }

    #[tokio::test]
    async fn queued_old_session_is_rejected_after_replacement() {
        let handle = 7;
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let sessions = SessionTracker::default();
        let old_session = sessions.allocate(handle);
        let current_session = sessions.allocate(handle);
        let conns = connection_map();
        let current_conn = test_conn().await;
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session: current_session,
                conn: current_conn,
            },
        );

        assert!(!sessions.is_current(handle, old_session));
        assert!(!connection_is_current(&conns, &sessions, handle, addr, old_session).await);
        assert!(connection_is_current(&conns, &sessions, handle, addr, current_session).await);
    }

    #[tokio::test]
    async fn deleted_client_terminal_disconnect_removes_slot_and_tracker() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let current = test_conn().await;
        let conns = Rc::new(connection_map());
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn: current.clone(),
            },
        );
        let ping_response = Rc::new(RefCell::new(HashSet::from([(addr, session)])));
        let ping_sent_at = Rc::new(RefCell::new(HashMap::from([(
            (addr, session),
            Instant::now(),
        )])));
        let (tx, mut rx) = channel();
        client_manager
            .remove_client(handle)
            .expect("remove configured client");

        ping_pong(
            client_manager,
            handle,
            addr,
            session,
            current,
            conns.clone(),
            sessions.clone(),
            tx,
            ping_response.clone(),
            ping_sent_at.clone(),
            Rc::new(Cell::new(false)),
        )
        .await;

        assert!(conns.lock().await.is_empty());
        assert_eq!(sessions.current(handle), None);
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert!(matches!(
            rx.recv().await,
            Some(HopDeckConnectionEvent::Disconnected {
                handle: disconnected,
                session: disconnected_session,
            }) if disconnected == handle && disconnected_session == session
        ));
    }

    #[tokio::test]
    async fn reset_handle_invalidates_inflight_dial_before_handle_reuse() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let cert = Certificate::generate_self_signed(["hopdeck-dial-reset-test".to_owned()])
            .expect("test certificate");
        let connection =
            HopDeckConnection::new(cert, client_manager.clone(), PrimaryCache::default(), None);
        let session = connection.sessions.allocate(handle);
        connection.connecting.lock().await.insert(handle, session);

        connection.reset_handle(handle).await;

        assert_eq!(connection.sessions.current(handle), None);
        assert!(!connection.connecting.lock().await.contains_key(&handle));
    }

    #[tokio::test]
    async fn old_dial_completion_cannot_clear_replacement_attempt_token() {
        let handle = 7;
        let connecting = Mutex::new(HashMap::new());
        let sessions = SessionTracker::default();
        let old_session = sessions.allocate(handle);
        let replacement_session = sessions.allocate(handle);
        connecting.lock().await.insert(handle, replacement_session);

        finish_failed_dial(&connecting, &sessions, handle, old_session).await;

        assert_eq!(sessions.current(handle), Some(replacement_session));
        assert_eq!(
            connecting.lock().await.get(&handle).copied(),
            Some(replacement_session)
        );
    }

    #[tokio::test]
    async fn session_pinned_send_never_falls_through_to_replacement() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let cert = Certificate::generate_self_signed(["hopdeck-session-send-test".to_owned()])
            .expect("test certificate");
        let connection =
            HopDeckConnection::new(cert, client_manager.clone(), PrimaryCache::default(), None);
        let old_session = connection.sessions.allocate(handle);
        let replacement_session = connection.sessions.allocate(handle);
        connection.conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session: replacement_session,
                conn: test_conn().await,
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);

        assert!(matches!(
            connection
                .send_on_session(ProtoEvent::Ack(17), handle, old_session)
                .await,
            Err(HopDeckConnectionError::NotConnected)
        ));
        assert_eq!(
            connection.current_session(handle),
            Some(replacement_session)
        );
    }

    #[tokio::test]
    async fn pinned_release_cleanup_survives_client_manager_removal() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let cert = Certificate::generate_self_signed(["hopdeck-release-cleanup-test".to_owned()])
            .expect("test certificate");
        let connection =
            HopDeckConnection::new(cert, client_manager.clone(), PrimaryCache::default(), None);
        let session = connection.sessions.allocate(handle);
        let (conn, receiver, addr) = connected_test_conn().await;
        connection.conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn,
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        client_manager
            .remove_client(handle)
            .expect("remove configured client before asynchronous capture cleanup");

        let cleanup = [
            ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 0,
            })),
            ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            })),
            ProtoEvent::Leave(0),
            ProtoEvent::HandoverInput {
                serial: 47,
                event: Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 30,
                    state: 0,
                }),
            },
            ProtoEvent::HandoverInput {
                serial: 47,
                event: Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0,
                }),
            },
            ProtoEvent::HandoverLeave {
                serial: 47,
                mode: hopdeck_proto::LEAVE_HANDOVER,
            },
        ];
        for expected in cleanup {
            connection
                .send_on_session(expected.clone(), handle, session)
                .await
                .expect("exact-session cleanup remains deliverable");
            let mut bytes = [0u8; MAX_EVENT_SIZE];
            let n = tokio::time::timeout(Duration::from_secs(1), receiver.recv(&mut bytes))
                .await
                .expect("cleanup datagram timeout")
                .expect("receive cleanup datagram");
            let decoded = decode_fixed_event(&bytes[..n]).expect("decode cleanup datagram");
            assert_eq!(format!("{decoded}"), format!("{expected}"));
        }

        let stale_key_down = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: 1,
            key: 30,
            state: 1,
        }));
        assert!(matches!(
            connection
                .send_on_session(stale_key_down, handle, session)
                .await,
            Err(HopDeckConnectionError::NotConnected)
        ));

        let stale_transactional_key_down = ProtoEvent::HandoverInput {
            serial: 47,
            event: Event::Keyboard(KeyboardEvent::Key {
                time: 1,
                key: 30,
                state: 1,
            }),
        };
        assert!(matches!(
            connection
                .send_on_session(stale_transactional_key_down, handle, session)
                .await,
            Err(HopDeckConnectionError::NotConnected)
        ));
    }
}
