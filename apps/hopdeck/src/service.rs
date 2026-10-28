use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient, EmulationBackend},
    connect::HopDeckConnection,
    crypto,
    discovery::{Discovery, DiscoveryEvent, PrimaryCache},
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    latency::{LatencyProber, ProbeResult},
    listen::{HopDeckListener, ListenerCreationError, ListenerSession},
    network::current_network_id,
};
use futures::StreamExt;
use hopdeck_ipc::{
    AppIdent, AsyncFrontendListener, ClientConfig, ClientHandle, ConnectMode, ConnectionMode,
    DEFAULT_PORT, FrontendEvent, FrontendRequest, HostKind, IncomingPeerConfig, IpcError,
    IpcListenerCreationError, Position, RemoteHostState, Status, TransportKind,
};
use hopdeck_proto::ProtoEvent;
use input_capture::clipboard::{ClipboardMonitor, SuppressionList};
use input_capture::frontmost_app;
use input_event::{ClipboardEvent, Event as InputEvent};
use local_channel::mpsc::{Receiver, channel};
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};
use webrtc_dtls::crypto::Certificate;

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c().await
    }
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// background per-address latency prober. Feeds the GUI's
    /// per-candidate RTT display so the user can compare interfaces on
    /// a multi-homed peer before locking one.
    latency: LatencyProber,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
    /// Access password shared with the listener's auth gate; refreshed
    /// from config on every config reload.
    access_password: Arc<RwLock<Option<String>>>,
    /// Shared mDNS browse cache. Used at DTLS-accept time to
    /// reverse-lookup the connecting peer's IP back to the system
    /// hostname they advertise via Bonjour, so the GUI can show a
    /// human-readable identity in the Incoming Connections list.
    primary_cache: PrimaryCache,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    /// Listener sessions accepted (`Connected`) and still up, keyed by
    /// source addr — the announce targets when the video master switch
    /// flips mid-run. `incoming_conn_info` only fills at `Entered`
    /// (the cursor actually crossing), which is too late for sharing:
    /// a connected-but-never-entered peer must still learn the offer.
    live_incoming: HashMap<SocketAddr, ListenerSession>,
    /// Video channel sender (M3a). Inert unless the `video` feature is
    /// compiled and `[video] enabled = true` — `offer()` returns `None`.
    video: crate::video::VideoHost,
    /// Kept so `SetVideoEnabled` can respawn the host mid-run with the
    /// identity the input listener already advertises.
    cert: Certificate,
    /// Mirror of the sharing-side master switch, echoed as
    /// `FrontendEvent::VideoShare` on change and on `Sync`. `true`
    /// whenever the switch says on — even when a broken `[video]`
    /// section or a missing `video` feature left the host itself inert.
    video_share_enabled: bool,
    /// Last mirror the capture side reported for the controlling-side
    /// video channel (offer on file / viewing / geometry). Broadcast as
    /// `FrontendEvent::VideoState` on change and on `Sync`.
    video_state: crate::video::VideoStateInfo,
    /// Startup snapshot of per-client tunnel forwards. The ipc
    /// `ClientConfig` doesn't carry them (no protocol change), so
    /// `save_config` re-attaches them from here; entries are dropped when
    /// the client is deleted. Editing forwards requires a daemon restart.
    tunnel_forwards: HashMap<ClientHandle, (u16, Vec<crate::tunnel::ForwardSpec>)>,
    /// Retained peer lock-recovery states, keyed by authenticated certificate
    /// fingerprint so a reconnect on a new source port keeps the same identity.
    remote_host_states: HashMap<String, RemoteHostRecovery>,
    next_trigger_handle: u64,
    /// mDNS-SD service registration + browse. Advertises our primary
    /// interface IP for peer dialers to bias toward; populates
    /// shared `PrimaryCache` (read by `HopDeckConnection`) from
    /// peer announcements.
    discovery: Discovery,
    /// Stream of resolved peer advertisements from [`Discovery`]'s
    /// browse task — folded into client candidate sets + interface
    /// labels in the main loop.
    discovery_event_rx: Receiver<DiscoveryEvent>,
    /// Opaque fingerprint of the network the machine is currently on
    /// (default-gateway MAC, or subnet fallback). Drives per-network
    /// lock resolution; recomputed periodically and on interface
    /// changes. `None` when undeterminable (locks then don't apply).
    current_network: Option<String>,
    /// Outgoing connection handle to fan clipboard frames out from
    /// the capture / forwarding paths. Same handle Capture owns;
    /// cloned in `Service::new` so Service can call `send` directly
    /// without routing through the capture session loop.
    conn: HopDeckConnection,
    /// Cross-platform clipboard poller. `None` when the platform
    /// clipboard couldn't be opened (headless CI, Wayland session
    /// without compositor support). Service drains it in the main
    /// loop and fans the resulting events out to peers whose
    /// `clipboard_send` is true.
    clipboard_monitor: Option<ClipboardMonitor>,
    /// Recent forwards keyed on `(originator_fingerprint, hash)`.
    /// Used to break N-peer rebroadcast cycles: when this device
    /// receives a forwarded clipboard frame and would re-fan to
    /// other peers, the entry under (origin, content_hash) blocks
    /// the duplicate. Pruned lazily — entries older than
    /// `RECENT_FORWARD_TTL` are dropped on each clipboard event.
    recent_forwarded: HashMap<(String, u64), Instant>,
    /// Shared with [`ClipboardMonitor`]; mutations to the inner
    /// `HashSet` take effect on the next clipboard poll without
    /// rebuilding the monitor.
    clipboard_suppression: SuppressionList,
    /// Per-client count of consecutive probe observations where a
    /// substantially-faster candidate beat the active path, gating the
    /// `Fastest`-mode auto-upgrade so noise can't trigger a switch.
    fastest_streak: HashMap<ClientHandle, u8>,
    /// Reverse-mode clients kicked by activation but not yet served by
    /// the periodic dial tick; drained by [`Self::on_reverse_dial_tick`].
    pending_reverse_kicks: Vec<ClientHandle>,
}

/// Consecutive probe observations a faster candidate must win before
/// `Fastest` mode actually switches to it. With the ~5 s probe cadence
/// this is ~15 s of sustained advantage — long enough that a transient
/// latency dip never causes a flap.
const FASTEST_UPGRADE_STREAK: u8 = 3;

const RECENT_FORWARD_TTL: Duration = Duration::from_secs(1);

fn clipboard_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    session: ListenerSession,
    pos: Position,
}

#[derive(Clone, Debug)]
struct RemoteHostRecovery {
    addr: SocketAddr,
    state: RemoteHostState,
}

fn mark_locked_remote_host_unavailable(
    states: &mut HashMap<String, RemoteHostRecovery>,
    addr: SocketAddr,
) -> Option<(String, SocketAddr)> {
    states
        .iter_mut()
        .find(|(_, recovery)| recovery.addr == addr && recovery.state == RemoteHostState::Locked)
        .map(|(fingerprint, recovery)| {
            recovery.state = RemoteHostState::Unavailable;
            (fingerprint.clone(), recovery.addr)
        })
}

fn record_remote_host_state(
    states: &mut HashMap<String, RemoteHostRecovery>,
    fingerprint: &str,
    addr: SocketAddr,
    state: RemoteHostState,
) -> bool {
    if state == RemoteHostState::Unlocked {
        return states.remove(fingerprint).is_some();
    }
    let changed = states
        .get(fingerprint)
        .is_none_or(|recovery| recovery.addr != addr || recovery.state != state);
    states.insert(fingerprint.to_owned(), RemoteHostRecovery { addr, state });
    changed
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        let mut forward_specs: HashMap<ClientHandle, (u16, Vec<crate::tunnel::ForwardSpec>)> =
            HashMap::new();
        for client in config.clients() {
            let handle = client_manager.add_with_config(client.clone());
            if !client.tunnel_forwards.is_empty() {
                forward_specs.insert(handle, (client.tunnel_port, client.tunnel_forwards));
            }
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        let access_password = Arc::new(RwLock::new(config.access_password()));
        // listener + connection. The primary-IP cache is owned by
        // the dialer side so its references survive Discovery
        // toggles; Discovery writes peer hints into it as browse
        // events arrive.
        let listener = HopDeckListener::new(
            config.port(),
            cert.clone(),
            authorized_keys.clone(),
            access_password.clone(),
        )
        .await?;
        let primary_cache: PrimaryCache = Default::default();

        // input capture + emulation. Emulation is wired before the
        // connection layer: reverse-dialed transports travel from the
        // dialer to the listener through the emulation request channel.
        let capture_backend = config.capture_backend().map(|b| b.into());
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);
        let conn = HopDeckConnection::new(
            cert.clone(),
            client_manager.clone(),
            primary_cache.clone(),
            Some(emulation.adopt_channel()),
        );

        // Push the persisted authorized-peers table into the receive
        // pipeline so per-peer post-processing is applied from the
        // first incoming packet.
        emulation.set_incoming_peers(authorized_keys.read().expect("lock").clone());

        // Reverse tunnel (M2c). The exit reads session liveness from the
        // client manager (the reverse session lives in the connect side);
        // forwarders dial independently and are gated by the exit's check.
        if let Some(exit_config) = config.tunnel() {
            crate::tunnel::spawn_exit(
                exit_config,
                cert.clone(),
                authorized_keys.clone(),
                client_manager.clone(),
            );
        }
        for (handle, (tunnel_port, specs)) in &forward_specs {
            crate::tunnel::spawn_forward(
                client_manager.clone(),
                *handle,
                cert.clone(),
                authorized_keys.clone(),
                specs.clone(),
                *tunnel_port,
            );
        }

        // Video channel (M3a). Bound before the first input session can be
        // announced; a bind failure logs and disables video, never fatal.
        let video_config = config.video();
        let video_share_enabled = video_config.enabled;
        if video_config.enabled && !cfg!(feature = "video") {
            log::warn!(
                "config [video] enabled = true, but this binary was built without the \
                 `video` feature — screen sharing disabled"
            );
        }
        let video =
            crate::video::VideoHost::spawn(cert.clone(), authorized_keys.clone(), video_config)
                .await;
        let video_peer = crate::video::VideoPeer::new(cert.clone());

        let conn_for_service = conn.sender_clone();
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            config.release_threshold_px(),
            video_peer,
        );

        // create dns resolver
        let resolver = DnsResolver::new()?;

        // background latency prober — pulls candidate addresses from
        // the (shared) client manager itself, so no targets need to be
        // pushed in as clients/DNS change.
        let latency = LatencyProber::new(client_manager.clone());

        let port = config.port();
        let (discovery_event_tx, discovery_event_rx) = channel();
        let discovery = Discovery::new(
            port,
            config.mdns_discovery(),
            primary_cache.clone(),
            discovery_event_tx,
        );
        // Resolve the lock (if any) for the network we boot on, before
        // the first dial, so a locked client connects on the right
        // interface immediately.
        let current_network = current_network_id();
        for handle in client_manager.registered_clients() {
            client_manager.recompute_active_lock(handle, current_network.as_deref());
        }
        // ClipboardMonitor is best-effort: a headless CI environment
        // or a Wayland session without compositor support yields a
        // permanent error here. We log and proceed without clipboard
        // sync rather than tying daemon startup to clipboard
        // availability.
        let clipboard_suppression: SuppressionList = {
            let host = HostKind::current();
            let initial: HashSet<AppIdent> = config
                .clipboard_suppression()
                .host()
                .iter()
                .cloned()
                .map(|s| host.make_ident(s))
                .collect();
            Arc::new(std::sync::Mutex::new(initial))
        };
        let clipboard_monitor =
            match ClipboardMonitor::with_suppression(clipboard_suppression.clone()) {
                Ok(m) => Some(m),
                Err(e) => {
                    log::warn!("clipboard monitor unavailable: {e}; clipboard sync disabled");
                    None
                }
            };
        let service = Self {
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            latency,
            authorized_keys,
            access_password,
            primary_cache,
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            live_incoming: Default::default(),
            video,
            cert,
            video_share_enabled,
            video_state: Default::default(),
            tunnel_forwards: forward_specs,
            remote_host_states: Default::default(),
            next_trigger_handle: 0,
            discovery,
            discovery_event_rx,
            current_network,
            conn: conn_for_service,
            clipboard_monitor,
            recent_forwarded: HashMap::new(),
            clipboard_suppression,
            fastest_streak: HashMap::new(),
            pending_reverse_kicks: Vec::new(),
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        // Periodic refresh of the Discovery service registration so
        // its TXT record stays accurate when the OS-preferred
        // interface (default route) changes — e.g. user switches
        // off Wi-Fi and Mac falls back to Ethernet. Cheap: at most
        // one re-publish every 30s, and a no-op when the primary
        // hasn't moved. `Skip` so a long suspend doesn't backlog-
        // burst on resume.
        let mut discovery_refresh_tick = tokio::time::interval(Duration::from_secs(30));
        discovery_refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // skip the immediate-fire of the first tick — Discovery
        // already published once at startup
        discovery_refresh_tick.tick().await;

        // Reverse-mode tunnel keepalive: pings establish / maintain the
        // session this (controlled) machine dials out to its controller.
        let mut reverse_dial_tick = tokio::time::interval(Duration::from_secs(5));
        reverse_dial_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Frontend state sync: the connect-side mutators (`set_alive`,
        // `set_peer_commit`, latency samples) change client state with
        // no event reaching the service — their relay lives in the
        // capture loop, which is dead whenever no capture backend is
        // available. Poll the dirty flag and re-broadcast changed
        // clients so frontends never freeze on a stale snapshot.
        let mut frontend_sync_tick = tokio::time::interval(Duration::from_secs(2));
        frontend_sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request).await,
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event).await,
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                result = self.latency.event() => self.handle_probe_result(result).await,
                event = recv_discovery(&mut self.discovery_event_rx) => {
                    self.handle_discovery_event(event)
                }
                _ = self.config.changed() => self.handle_config_change().await,
                _ = discovery_refresh_tick.tick() => self.on_discovery_refresh().await,
                _ = reverse_dial_tick.tick() => self.on_reverse_dial_tick().await,
                _ = frontend_sync_tick.tick() => self.on_frontend_sync_tick(),
                event = recv_clipboard(&mut self.clipboard_monitor) => {
                    self.handle_local_clipboard_event(event).await;
                }
                result = shutdown_signal() => break result?,
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;
        log::debug!("terminating latency prober ...");
        self.latency.terminate().await;

        Ok(())
    }

    async fn handle_frontend_request(
        &mut self,
        request: Option<Result<FrontendRequest, IpcError>>,
    ) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config().await;
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config().await;
            }
            FrontendRequest::SubmitAccessPassword { addr, password } => {
                self.conn.submit_access_password(addr, password);
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config().await;
            }
            FrontendRequest::ConnectTo(ip, port) => {
                self.connect_to(ip, port).await;
                self.save_config().await;
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle).await;
                self.save_config().await;
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.conn.reset_handle(handle).await;
                self.update_fix_ips(handle, fix_ips);
                self.save_config().await;
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.conn.reset_handle(handle).await;
                self.update_hostname(handle, host);
                self.save_config().await;
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.conn.reset_handle(handle).await;
                self.update_port(handle, port);
                self.save_config().await;
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config().await;
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config().await;
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::SaveConfiguration => self.save_config().await,
            FrontendRequest::SetReleaseThreshold(threshold) => {
                self.config.set_release_threshold_px(threshold);
                self.capture.set_release_threshold(threshold);
                self.notify_frontend(FrontendEvent::ReleaseThreshold(threshold));
                self.save_config().await;
            }
            FrontendRequest::SetReleaseBind(bind) => {
                self.config.set_release_bind(bind);
                let canonical = self.config.release_bind();
                self.capture.set_release_bind(canonical.clone());
                self.notify_frontend(FrontendEvent::ReleaseBind(canonical));
                self.save_config().await;
            }
            FrontendRequest::SetIncomingPeerNaturalScroll(fp, natural_scroll) => {
                self.set_incoming_peer_natural_scroll(fp, natural_scroll);
                self.save_config().await;
            }
            FrontendRequest::SetIncomingPeerSensitivity(fp, sensitivity) => {
                self.set_incoming_peer_sensitivity(fp, sensitivity);
                self.save_config().await;
            }
            FrontendRequest::SetMdnsDiscovery(enabled) => {
                self.config.set_mdns_discovery(enabled);
                self.discovery.set_enabled(enabled);
                self.notify_frontend(FrontendEvent::MdnsDiscovery(enabled));
                self.save_config().await;
            }
            FrontendRequest::SetClientClipboardSend(handle, enabled) => {
                if self.client_manager.set_clipboard_send(handle, enabled) {
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetClientCommandAsCtrl(handle, enabled) => {
                if self.client_manager.set_command_as_ctrl(handle, enabled) {
                    self.capture.set_command_as_ctrl(handle, enabled);
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetClientRequireCrossingModifier(handle, required) => {
                if self
                    .client_manager
                    .set_require_crossing_modifier(handle, required)
                {
                    let modifier = self.client_manager.required_crossing_modifier(handle);
                    self.capture.set_crossing_modifier(handle, modifier);
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetClientCrossingModifier(handle, modifier) => {
                if self.client_manager.set_crossing_modifier(handle, modifier) {
                    let modifier = self.client_manager.required_crossing_modifier(handle);
                    self.capture.set_crossing_modifier(handle, modifier);
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetEmulationBackend(name) => {
                self.apply_emulation_backend(name).await;
            }
            FrontendRequest::SetConnectionMode(handle, mode) => {
                // Resolve the live network first: the user is acting on a
                // connected machine, so a stale cached `None` (from a
                // transient blip) must not leave this network's lock in
                // place to silently re-activate later.
                self.refresh_network().await;
                let net = self.current_network.clone();
                if self.client_manager.set_mode(handle, mode, net.as_deref()) {
                    // The mode change may have cleared this network's
                    // lock; re-resolve and re-dial so the new policy
                    // takes effect immediately.
                    self.client_manager
                        .recompute_active_lock(handle, net.as_deref());
                    self.conn.reset_handle(handle).await;
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::UpdateConnectMode(handle, connect_mode) => {
                if self.client_manager.set_connect_mode(handle, connect_mode) {
                    self.broadcast_client(handle);
                    self.save_config().await;
                    if connect_mode == ConnectMode::Reverse {
                        // Establish the tunnel on the next loop pass rather
                        // than waiting for the dial tick.
                        self.pending_reverse_kicks.push(handle);
                    }
                }
            }
            FrontendRequest::SetNetworkLock(handle, ip) => {
                // Resolve the live network first (see SetConnectionMode).
                self.refresh_network().await;
                let Some(net) = self.current_network.clone() else {
                    log::warn!("cannot lock client {handle}: current network unknown");
                    return;
                };
                if self.client_manager.set_network_lock(handle, &net, ip) {
                    self.client_manager
                        .recompute_active_lock(handle, Some(&net));
                    // Drop any live connection so the lock takes effect
                    // on the next event instead of waiting for the
                    // current (possibly wrong-interface) path to drop.
                    self.conn.reset_handle(handle).await;
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetIncomingPeerClipboardReceive(fp, enabled) => {
                self.set_incoming_peer_clipboard_receive(fp, enabled);
                self.save_config().await;
            }
            FrontendRequest::AddSuppressedApp(value) => {
                self.add_suppressed_app(value);
                self.save_config().await;
            }
            FrontendRequest::RemoveSuppressedApp(value) => {
                self.remove_suppressed_app(value);
                self.save_config().await;
            }
            FrontendRequest::ListRunningApps => {
                let apps = frontmost_app::list_running_apps();
                self.notify_frontend(FrontendEvent::RunningApps(apps));
            }
            FrontendRequest::SetVideoView(handle, view) => {
                // The capture state machine owns the video peer; the
                // state change echoes back as `VideoState` and is
                // broadcast from there. When no capture backend is
                // running the request loop still processes it, so the
                // toggle works without a capture backend too.
                self.capture.set_video_view(handle, view);
            }
            FrontendRequest::SetVideoEnabled(enabled) => {
                self.set_video_share(enabled).await;
            }
        }
    }

    fn add_suppressed_app(&mut self, value: String) {
        let value = value.trim().to_owned();
        if value.is_empty() {
            return;
        }
        let mut suppression = self.config.clipboard_suppression();
        let host = suppression.host_mut();
        if !host.iter().any(|v| v.eq_ignore_ascii_case(&value)) {
            host.push(value);
        }
        self.commit_suppression(suppression);
    }

    fn remove_suppressed_app(&mut self, value: String) {
        let mut suppression = self.config.clipboard_suppression();
        suppression
            .host_mut()
            .retain(|v| !v.eq_ignore_ascii_case(&value));
        self.commit_suppression(suppression);
    }

    /// Persist the per-OS struct, refresh the runtime `HashSet`
    /// shared with [`ClipboardMonitor`], and push the host slot to
    /// the GUI. Centralized so add/remove can't drift apart.
    fn commit_suppression(&mut self, suppression: hopdeck_ipc::ClipboardSuppression) {
        let host = HostKind::current();
        let host_list = suppression.host().clone();
        {
            let mut guard = self.clipboard_suppression.lock().expect("lock");
            guard.clear();
            for s in &host_list {
                guard.insert(host.make_ident(s.clone()));
            }
        }
        self.config.set_clipboard_suppression(suppression);
        self.notify_frontend(FrontendEvent::SuppressedAppsUpdated(host_list));
    }

    /// Refresh `last_addr` / `last_hostname` for the authorized-peer
    /// entry matching `fingerprint` whenever a DTLS connect lands.
    /// Hostname comes from a reverse-lookup against the mDNS
    /// `hostname → primary_ip` cache; falls through to addr-only
    /// if discovery isn't running on either end or the peer's
    /// announced primary differs from the IP it actually connected
    /// from. Persists the update so the GUI keeps a useful
    /// identification across restarts.
    async fn update_incoming_peer_address(&mut self, addr: SocketAddr, fingerprint: &str) {
        let ip = addr.ip().to_string();
        let hostname = self.lookup_hostname_for_ip(addr.ip());
        // Scope the write guard to this block: `save_config().await`
        // below must not run while the non-async RwLock is held, or
        // the lock would span a suspension point. The block yields
        // the cloned snapshot and drops the guard before the await.
        let snapshot = {
            let mut keys = self.authorized_keys.write().expect("lock");
            let Some(peer) = keys.get_mut(fingerprint) else {
                return; // unauthorized peer; nothing to update
            };
            let mut changed = peer.last_addr.as_deref() != Some(&ip);
            if changed {
                peer.last_addr = Some(ip);
            }
            if let Some(h) = hostname {
                if peer.last_hostname.as_deref() != Some(h.as_str()) {
                    peer.last_hostname = Some(h);
                    changed = true;
                }
            }
            if !changed {
                return;
            }
            keys.clone()
        };
        // No need to push to InputEmulation — last_addr/last_hostname
        // are display-only; per-pair scroll/sensitivity is unaffected.
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(snapshot));
        self.backfill_peer_fingerprints();
        self.save_config().await;
    }

    /// Link authorized-peer entries to configured clients whose
    /// `peer_fingerprint` is still unset. The live association is
    /// written by `Connected`, but that event can land before the
    /// matching client exists (the peer dials in first, the user adds
    /// the device afterwards) or while the client is inactive —
    /// `ClientManager::get_client` only matches active clients. The
    /// authorized entry's persisted `last_addr`, and its mDNS
    /// hostname mapped through the discovery cache, survive both
    /// orderings. Idempotent: unchanged clients are not re-broadcast.
    fn backfill_peer_fingerprints(&mut self) {
        let keys = self.authorized_keys.read().expect("lock").clone();
        if keys.is_empty() {
            return;
        }
        for (handle, config, state) in self.client_manager.get_client_states() {
            if state.peer_fingerprint.is_some() {
                continue;
            }
            let known_peer_ip = |peer: &IncomingPeerConfig| {
                let by_addr = peer
                    .last_addr
                    .as_deref()
                    .and_then(|addr| addr.parse::<IpAddr>().ok());
                let by_hostname = peer
                    .last_hostname
                    .as_deref()
                    .and_then(|host| self.primary_cache.borrow().get(host).copied());
                [by_addr, by_hostname]
                    .into_iter()
                    .flatten()
                    .any(|ip| config.fix_ips.contains(&ip) || state.ips.contains(&ip))
            };
            let fingerprint = keys
                .iter()
                .find(|(_, peer)| known_peer_ip(peer))
                .map(|(fp, _)| fp.clone());
            let Some(fingerprint) = fingerprint else {
                continue;
            };
            if self
                .client_manager
                .set_peer_fingerprint(handle, Some(fingerprint))
            {
                self.broadcast_client(handle);
            }
        }
    }

    fn lookup_hostname_for_ip(&self, target: std::net::IpAddr) -> Option<String> {
        self.primary_cache
            .borrow()
            .iter()
            .find_map(|(host, ip)| (*ip == target).then(|| host.clone()))
    }

    fn set_incoming_peer_natural_scroll(&mut self, fingerprint: String, natural_scroll: bool) {
        if let Some(peer) = self
            .authorized_keys
            .write()
            .expect("lock")
            .get_mut(&fingerprint)
        {
            peer.natural_scroll = natural_scroll;
        }
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn set_incoming_peer_sensitivity(&mut self, fingerprint: String, sensitivity: f64) {
        if let Some(peer) = self
            .authorized_keys
            .write()
            .expect("lock")
            .get_mut(&fingerprint)
        {
            peer.mouse_sensitivity = sensitivity;
        }
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn set_incoming_peer_clipboard_receive(
        &mut self,
        fingerprint: String,
        clipboard_receive: bool,
    ) {
        if let Some(peer) = self
            .authorized_keys
            .write()
            .expect("lock")
            .get_mut(&fingerprint)
        {
            peer.clipboard_receive = clipboard_receive;
        }
        let keys = self.authorized_keys.read().expect("lock").clone();
        // Emulation needs to know so the receive-side gate matches
        // the new value immediately, not after a config-change cycle.
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    async fn save_config(&mut self) {
        let clients = self.client_manager.get_client_states();
        let clients = clients
            .into_iter()
            .map(|(handle, c, s)| {
                let (tunnel_port, tunnel_forwards) = self
                    .tunnel_forwards
                    .get(&handle)
                    .map(|(port, specs)| (*port, specs.clone()))
                    .unwrap_or((crate::tunnel::DEFAULT_TUNNEL_PORT, Vec::new()));
                ConfigClient {
                    ips: HashSet::from_iter(c.fix_ips),
                    hostname: c.hostname,
                    port: c.port,
                    pos: c.pos,
                    active: s.active,
                    enter_hook: c.cmd,
                    mode: c.mode,
                    connect_mode: c.connect_mode,
                    network_locks: c.network_locks,
                    clipboard_send: c.clipboard_send,
                    command_as_ctrl: c.command_as_ctrl,
                    require_crossing_modifier: c.require_crossing_modifier,
                    crossing_modifier: c.crossing_modifier,
                    transport: c.transport,
                    tunnel_forwards,
                    tunnel_port,
                }
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back().await {
            log::warn!("failed to write config: {e}");
        }
    }

    async fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h).await;
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let release_threshold = self.config.release_threshold_px();
        self.capture.set_release_threshold(release_threshold);
        self.notify_frontend(FrontendEvent::ReleaseThreshold(release_threshold));
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        *self.access_password.write().unwrap() = self.config.access_password();
        self.emulation.set_incoming_peers(authorized_keys);
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    async fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                session,
                pos,
                fingerprint,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, session, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, session, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr, session } => {
                if self
                    .incoming_session(addr)
                    .is_some_and(|current| current != session)
                {
                    log::debug!(
                        "ignoring disconnect for stale inbound session {session} from {addr}"
                    );
                    return;
                }
                self.mark_remote_host_unavailable(addr);
                if let Some(addr) = self.remove_incoming(addr, session) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
                // Only drop the announce target for the session that
                // actually died: a late disconnect for a replaced
                // generation must not unregister its successor.
                if self.live_incoming.get(&addr) == Some(&session) {
                    self.live_incoming.remove(&addr);
                }
                // Clear the display transport unless an outbound session
                // to the same peer is still alive (it owns the label then).
                if let Some(handle) = self.client_manager.get_client(addr) {
                    if !self.client_manager.alive(handle)
                        && self.client_manager.set_transport(handle, None)
                    {
                        self.broadcast_client(handle);
                    }
                }
                self.refresh_incoming_flags();
                // An input session died: stop streaming to whoever held the
                // video channel, but keep the listener. Coarse by design
                // (any disconnect ends all sends) — the next `Connected`
                // re-announces the same port and the reconnected peer
                // re-dials; halting the host here would kill screen sharing
                // until the user toggled it off and on again.
                self.video.stop_sending();
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    let changed = self.port != port;
                    self.port = port;
                    self.discovery.set_port(port);
                    if changed {
                        // The listener is rebuilt live by the emulation
                        // task; the config is what the next start reads,
                        // so an explicit port change must land there.
                        self.config.set_port(port);
                        self.save_config().await;
                    }
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify(completion) => {
                self.capture.release_for_handover(completion)
            }
            EmulationEvent::Connected {
                addr,
                session,
                fingerprint,
                transport,
            } => {
                // Accept itself replaces an inbound transport generation. If
                // the previous generation had already created a return
                // barrier, retire it now rather than waiting for the new peer
                // to cross. The replacement may disconnect before Entered,
                // in which case retaining the old session would make its
                // terminal event look stale forever.
                if let Some(previous_session) = self
                    .incoming_session(addr)
                    .filter(|previous| *previous != session)
                {
                    if self.remove_incoming(addr, previous_session).is_some() {
                        self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                    }
                }
                self.update_incoming_peer_address(addr, &fingerprint).await;
                if let Some(recovery) = self.remote_host_states.get_mut(&fingerprint) {
                    recovery.addr = addr;
                }
                // Link the inbound session back to the configured
                // client so frontends can show its trust state inline,
                // and record which transport carried it.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    let fingerprint_changed = self
                        .client_manager
                        .set_peer_fingerprint(handle, Some(fingerprint.clone()));
                    let transport_changed =
                        self.client_manager.set_transport(handle, Some(transport));
                    if fingerprint_changed || transport_changed {
                        self.broadcast_client(handle);
                    }
                }
                self.notify_frontend(FrontendEvent::DeviceConnected {
                    addr,
                    fingerprint: fingerprint.clone(),
                });
                self.maybe_adopt_reverse_session(addr, session, fingerprint, transport)
                    .await;
                // Track the live session for mid-run video announces;
                // a replacement session overwrites the stale entry.
                self.live_incoming.insert(addr, session);
                self.refresh_incoming_flags();
                // Video announce (M3a). Reverse-mode sessions offer no
                // inbound path for the peer to dial our video port, so they
                // stay silent — D34: v1 ships video for direct topology only.
                if let Some(offer) = self.video.offer() {
                    let reverse = self.client_manager.get_client(addr).is_some_and(|handle| {
                        self.client_manager.connect_mode(handle) == ConnectMode::Reverse
                    });
                    if reverse {
                        log::debug!(
                            "video: skipping announce on reverse session {session} from {addr}"
                        );
                    } else {
                        self.emulation.announce_video(addr, session, offer);
                    }
                }
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
            EmulationEvent::RemoteHostState {
                addr,
                fingerprint,
                state,
            } => {
                let state = match state {
                    hopdeck_proto::HostInputState::Locked => RemoteHostState::Locked,
                    hopdeck_proto::HostInputState::Unlocked => RemoteHostState::Unlocked,
                };
                let changed = record_remote_host_state(
                    &mut self.remote_host_states,
                    &fingerprint,
                    addr,
                    state,
                );
                if changed {
                    self.notify_frontend(FrontendEvent::RemoteHostState {
                        fingerprint,
                        addr,
                        state,
                    });
                }
            }
            EmulationEvent::ClipboardReceived {
                addr,
                from_fingerprint,
                content,
            } => {
                self.handle_clipboard_received(addr, from_fingerprint, content)
                    .await;
            }
        }
    }

    /// Local clipboard change picked up by the polling
    /// [`ClipboardMonitor`]. Stamp the originator fingerprint on the
    /// wire frame and fan out to every active outgoing client whose
    /// `clipboard_send` is true. Records `(self_fp, hash)` in
    /// `recent_forwarded` so a later forwarded copy of the same
    /// content (re-arriving via another peer in an N-peer ring)
    /// won't be redundantly re-broadcast.
    async fn handle_local_clipboard_event(&mut self, event: Option<input_capture::CaptureEvent>) {
        let Some(event) = event else {
            return;
        };
        let input_capture::CaptureEvent::Input(InputEvent::Clipboard(ClipboardEvent::Text(
            content,
        ))) = event
        else {
            return;
        };
        let targets = self.client_manager.clipboard_send_targets();
        if targets.is_empty() {
            log::trace!(
                "clipboard captured locally ({} bytes) but no peer has clipboard_send=true; skipping fan-out",
                content.len()
            );
            return;
        }
        let from_fingerprint = self.public_key_fingerprint.clone();
        let hash = clipboard_hash(&content);
        self.prune_recent_forwarded();
        self.recent_forwarded
            .insert((from_fingerprint.clone(), hash), Instant::now());
        log::info!(
            "broadcasting local clipboard ({} bytes) to {} peer(s)",
            content.len(),
            targets.len()
        );
        for handle in targets {
            let event = ProtoEvent::Clipboard {
                from_fingerprint: from_fingerprint.clone(),
                content: content.clone(),
            };
            if let Err(e) = self.conn.send(event, handle).await {
                log::debug!("clipboard send to client {handle} failed: {e}");
            }
        }
    }

    /// Forwarded clipboard frame just landed via the listen side
    /// (the local clipboard has already been updated by
    /// `emulation::ListenTask`). Refresh the
    /// [`ClipboardMonitor`]'s last-known content so the next 500ms
    /// poll doesn't see this as a fresh local change and bounce it
    /// back, then forward to other peers honoring the recent-
    /// forwarded gate.
    async fn handle_clipboard_received(
        &mut self,
        from_addr: SocketAddr,
        from_fingerprint: String,
        content: String,
    ) {
        if let Some(monitor) = self.clipboard_monitor.as_ref() {
            monitor.update_last_content(content.clone());
        }
        let hash = clipboard_hash(&content);
        self.prune_recent_forwarded();
        let key = (from_fingerprint.clone(), hash);
        if self.recent_forwarded.contains_key(&key) {
            log::debug!(
                "skipping clipboard re-fan-out: already forwarded ({}, {} bytes) within {}ms",
                &from_fingerprint[..from_fingerprint.len().min(8)],
                content.len(),
                RECENT_FORWARD_TTL.as_millis()
            );
            return;
        }
        let targets = self.client_manager.clipboard_send_targets();
        let forward_targets: Vec<ClientHandle> = targets
            .into_iter()
            .filter(|h| {
                // Skip the client we just received from. Identified
                // by IP rather than full SocketAddr so a peer's
                // ephemeral source port (which differs between its
                // outgoing and our cached active_addr) doesn't
                // accidentally include them.
                self.client_manager
                    .active_addr(*h)
                    .map(|a| a.ip() != from_addr.ip())
                    .unwrap_or(true)
            })
            .collect();
        if forward_targets.is_empty() {
            return;
        }
        self.recent_forwarded.insert(key, Instant::now());
        log::info!(
            "forwarding clipboard ({} bytes, originator {}) to {} peer(s)",
            content.len(),
            &from_fingerprint[..from_fingerprint.len().min(8)],
            forward_targets.len()
        );
        for handle in forward_targets {
            let event = ProtoEvent::Clipboard {
                from_fingerprint: from_fingerprint.clone(),
                content: content.clone(),
            };
            if let Err(e) = self.conn.send(event, handle).await {
                log::debug!("clipboard forward to client {handle} failed: {e}");
            }
        }
    }

    fn prune_recent_forwarded(&mut self) {
        self.recent_forwarded
            .retain(|_, ts| ts.elapsed() < RECENT_FORWARD_TTL);
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin { handle, handover } => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation
                        .send_leave_event(incoming.addr, incoming.session, handover);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.spawn_hook_command(handle);
            }
            ICaptureEvent::PeerCommitUpdated(handle) => {
                self.broadcast_client(handle);
            }
            ICaptureEvent::PasswordRequired(addr) => {
                log::info!("peer {addr} requires the access password; prompting frontend");
                self.notify_frontend(FrontendEvent::PasswordRequired { addr });
            }
            ICaptureEvent::VideoState(info) => {
                if info != self.video_state {
                    log::debug!(
                        "video state: offer for {:?}, viewing={} ({}x{}@{}fps)",
                        info.handle,
                        info.viewing,
                        info.width,
                        info.height,
                        info.max_fps
                    );
                    self.video_state = info.clone();
                    self.notify_frontend(self.video_state_event());
                }
            }
        }
    }

    /// Build the frontend event from the current mirror. Kept beside
    /// `handle_capture_event` so the two stay field-for-field in sync.
    fn video_state_event(&self) -> FrontendEvent {
        FrontendEvent::VideoState {
            handle: self.video_state.handle,
            viewing: self.video_state.viewing,
            available: self.video_state.available,
            width: self.video_state.width,
            height: self.video_state.height,
            max_fps: self.video_state.max_fps,
        }
    }

    /// The `SetVideoEnabled` handler: persist the sharing-side master
    /// switch, then rebuild the video host so the choice takes effect
    /// without a daemon restart (P1.2). Enabling announces the fresh
    /// offer on every established direct session; disabling withdraws
    /// it before the host's own channels die. Always echoes
    /// `VideoShare`, so the frontend switch settles even on no-ops.
    async fn set_video_share(&mut self, enabled: bool) {
        if self.video_share_enabled == enabled {
            self.notify_frontend(self.video_share_event());
            return;
        }
        self.video_share_enabled = enabled;
        self.config.set_video_enabled(enabled);
        self.save_config().await;
        if !enabled {
            // Withdraw while the announcing input sessions are still
            // alive: viewers clear the offer and hang up their receive
            // channel before `halt` kills the host side.
            self.announce_video_to_sessions(ProtoEvent::VideoStop);
            self.video.halt();
        } else {
            if !cfg!(feature = "video") {
                log::warn!(
                    "config [video] enabled = true, but this binary was built without the \
                     `video` feature — screen sharing disabled"
                );
            }
            let mut video_config = self.config.video();
            video_config.enabled = true;
            self.video = crate::video::VideoHost::spawn(
                self.cert.clone(),
                self.authorized_keys.clone(),
                video_config,
            )
            .await;
            // A portal source has no geometry until its capture
            // negotiates; `offer()` then returns `None` and existing
            // sessions stay silent until they re-establish (or until a
            // later toggle round announces them).
            if let Some(offer) = self.video.offer() {
                self.announce_video_to_sessions(offer);
            }
        }
        self.notify_frontend(self.video_share_event());
    }

    /// Announce a video-channel event (the offer on enable,
    /// `VideoStop` on disable) on every established direct-mode
    /// incoming session. The `Connected` arm announces per new session;
    /// this catches sessions that predate the switch flip. Delivery is
    /// best-effort: the listen task drops announces for replaced
    /// sessions, and reverse sessions have no inbound path for a peer
    /// to dial (D34) — the same rule the Connected arm applies.
    fn announce_video_to_sessions(&self, event: ProtoEvent) {
        for (addr, session) in &self.live_incoming {
            let reverse = self.client_manager.get_client(*addr).is_some_and(|handle| {
                self.client_manager.connect_mode(handle) == ConnectMode::Reverse
            });
            if reverse {
                log::debug!("video: skipping announce on reverse session {session} from {addr}");
                continue;
            }
            self.emulation
                .announce_video(*addr, *session, event.clone());
        }
    }

    fn video_share_event(&self) -> FrontendEvent {
        FrontendEvent::VideoShare {
            enabled: self.video_share_enabled,
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    /// Apply a background latency sample and re-broadcast the client
    /// only when the stored value actually moved. The reading is
    /// quantized to 100 µs buckets so ordinary sub-microsecond jitter
    /// on a stable link doesn't churn the frontend every probe cycle.
    /// Also feeds the `Fastest`-mode convergence check.
    async fn handle_probe_result(&mut self, result: ProbeResult) {
        let ProbeResult {
            handle,
            ip,
            rtt_micros,
        } = result;
        let bucketed = rtt_micros.map(|us| us - (us % 100));
        if self.client_manager.set_latency(handle, ip, bucketed) {
            self.broadcast_client(handle);
        }
        self.maybe_upgrade_fastest(handle).await;
    }

    /// In `Fastest` mode (and only when not locked), nudge the live
    /// connection toward the lowest-latency candidate — but only once a
    /// candidate has been *sustainedly and substantially* faster than
    /// the active path (see [`ClientManager::fastest_upgrade_candidate`]
    /// and [`FASTEST_UPGRADE_STREAK`]), so probe noise can't cause
    /// flapping. Tearing down the connection makes the next send
    /// re-dial, which picks the new fastest.
    async fn maybe_upgrade_fastest(&mut self, handle: ClientHandle) {
        let eligible = self.client_manager.get_active_lock(handle).is_none()
            && self.client_manager.get_mode(handle) == ConnectionMode::Fastest;
        if !eligible {
            self.fastest_streak.remove(&handle);
            return;
        }
        if self
            .client_manager
            .fastest_upgrade_candidate(handle)
            .is_some()
        {
            let streak = self.fastest_streak.entry(handle).or_insert(0);
            *streak += 1;
            if *streak >= FASTEST_UPGRADE_STREAK {
                self.fastest_streak.remove(&handle);
                log::info!("client {handle}: switching to a sustainedly-faster interface");
                self.conn.reset_handle(handle).await;
            }
        } else {
            self.fastest_streak.remove(&handle);
        }
    }

    /// Fold a resolved peer advertisement into every client whose
    /// configured hostname matches the mDNS instance: add the
    /// discovered addresses to the candidate set and record per-address
    /// interface kinds. Re-broadcasts only the clients that changed.
    fn handle_discovery_event(&mut self, event: DiscoveryEvent) {
        let DiscoveryEvent::Resolved {
            instance,
            addresses,
            interfaces,
        } = event;
        for handle in self.client_manager.registered_clients() {
            let matches = self
                .client_manager
                .get_hostname(handle)
                .is_some_and(|h| crate::discovery::normalize_mdns_name(&h) == instance);
            if !matches {
                continue;
            }
            // Discovered ips must land before interface labels so the
            // label filter (which keeps only current candidates) sees
            // the freshly-added addresses.
            let discovered_changed = self
                .client_manager
                .set_discovered_ips(handle, addresses.clone());
            let interfaces_changed = self
                .client_manager
                .set_interfaces(handle, interfaces.clone());
            if discovered_changed {
                // The candidate set moved, so any in-progress
                // Fastest-upgrade streak no longer describes the
                // current candidates — start it fresh.
                self.fastest_streak.remove(&handle);
            }
            if discovered_changed || interfaces_changed {
                self.broadcast_client(handle);
            }
        }
    }

    /// Periodic discovery housekeeping: re-publish our advertisement
    /// (primary interface may have changed) and re-evaluate which
    /// network we're on for per-network lock resolution.
    async fn on_discovery_refresh(&mut self) {
        self.discovery.refresh();
        self.refresh_network().await;
    }

    /// Keep the tunnel alive for reverse-mode clients: while such a
    /// client is active, send a ping every tick. On an established
    /// session it is the ordinary heartbeat; with no session it is what
    /// triggers the lazy dial inside `HopDeckConnection::send` — the
    /// one-way-network path where this (controlled) machine must dial
    /// out because the controller cannot reach it inbound. Activation
    /// kicks land in `pending_reverse_kicks` and are served here.
    async fn on_reverse_dial_tick(&mut self) {
        let mut kicks = std::mem::take(&mut self.pending_reverse_kicks);
        kicks.extend(self.client_manager.active_clients());
        for handle in kicks {
            if self.client_manager.connect_mode(handle) != ConnectMode::Reverse {
                continue;
            }
            // D31④: a reverse transport that died before/without a live
            // adopted session looks alive forever — the ping below is a
            // plain UDP write on the dead DTLS conn and succeeds silently,
            // so the send path never errors and nothing else re-dials.
            // Detect the zombie (active address installed, no live session
            // for it) and reset the handle so the ping re-dials instead of
            // feeding a corpse. Liveness comes from `live_incoming`
            // (`Connected`), not `incoming_conn_info` (`Entered`): an
            // adopted reverse session is alive long before the pointer
            // crosses, and the Entered-gated map would reset it every
            // tick — observed on a real device as a 5s session flap.
            if let Some(addr) = self.client_manager.active_addr(handle) {
                if !self.live_incoming.contains_key(&addr) {
                    log::warn!(
                        "reverse client {handle} keeps a transport at {addr} \
                         with no live session — resetting so it re-dials"
                    );
                    self.conn.reset_handle(handle).await;
                }
            }
            if let Err(e) = self.conn.send(ProtoEvent::Ping, handle).await {
                log::debug!("reverse dial to client {handle} still pending: {e}");
            }
        }
    }

    /// Re-broadcast every registered client when the connect side
    /// mutated state silently (Pong liveness, Hello commit, latency
    /// samples — see `ClientManager::take_client_states_dirty`).
    /// Idempotent snapshots: frontends dedupe by content.
    fn on_frontend_sync_tick(&mut self) {
        if !self.client_manager.take_client_states_dirty() {
            return;
        }
        let handles: Vec<ClientHandle> = self
            .client_manager
            .get_client_states()
            .into_iter()
            .map(|(handle, _, _)| handle)
            .collect();
        for handle in handles {
            self.broadcast_client(handle);
        }
    }

    /// Controller half of reverse-connect: an inbound DTLS session
    /// whose source matches a Reverse-mode client is that client's
    /// transport *to us* — take it away from the emulation side and
    /// install it as the capture send path, so input flows out over
    /// the tunnel the client dialed. Skipped when
    ///  - the session's source address is one of the client's
    ///    configured dial targets (the echo of our own reverse dial,
    ///    which that client already adopted listener-side), or
    ///  - the client already has a live transport (adoption fills a
    ///    void; it never competes with an established session).
    async fn maybe_adopt_reverse_session(
        &mut self,
        addr: SocketAddr,
        session: ListenerSession,
        fingerprint: String,
        transport: TransportKind,
    ) {
        let Some(handle) = self.client_manager.get_client(addr) else {
            return;
        };
        if self.client_manager.connect_mode(handle) != ConnectMode::Reverse {
            return;
        }
        let port = self.client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let is_dial_echo = self
            .client_manager
            .get_ips(handle)
            .unwrap_or_default()
            .iter()
            .any(|&ip| SocketAddr::new(ip, port) == addr);
        if is_dial_echo {
            log::debug!(
                "inbound session {session} from {addr} echoes client {handle}'s own reverse dial; \
                 leaving it emulation-side"
            );
            return;
        }
        if self.client_manager.active_addr(handle).is_some() {
            log::debug!(
                "skipping reverse adoption from {addr}: client {handle} already has a transport"
            );
            return;
        }
        let Some(conn) = self.emulation.take_conn(addr, session).await else {
            log::debug!("reverse adoption of session {session} from {addr}: already gone");
            return;
        };
        self.conn
            .adopt_inbound(handle, conn, addr, Some(fingerprint), transport)
            .await;
    }

    /// Recompute the current-network fingerprint. On change, re-resolve
    /// every client's active lock and re-dial those whose lock moved so
    /// the new network's pin (or its absence) takes effect at once.
    async fn refresh_network(&mut self) {
        let net = current_network_id();
        if net == self.current_network {
            return;
        }
        log::info!("network changed: {:?} -> {:?}", self.current_network, net);
        self.current_network = net;
        for handle in self.client_manager.registered_clients() {
            if self
                .client_manager
                .recompute_active_lock(handle, self.current_network.as_deref())
            {
                self.conn.reset_handle(handle).await;
                self.broadcast_client(handle);
            }
        }
    }

    /// Hot-swap the input-emulation backend from a frontend request.
    /// `name` is the config spelling (`"x11"`, `"usbip"`, …) or `None`
    /// for automatic probing. Refused while a CLI `--emulation-backend`
    /// override is in effect — it would silently win over the persisted
    /// value. The frontend echo always carries the effective value; a
    /// backend that fails to come up surfaces through the usual
    /// `EmulationDisabled` status event.
    async fn apply_emulation_backend(&mut self, name: Option<String>) {
        let echo = |this: &mut Self| {
            this.notify_frontend(FrontendEvent::EmulationBackend(
                this.config.emulation_backend().map(|b| b.to_string()),
            ));
        };
        let parse = |name: &str| -> Result<EmulationBackend, String> {
            serde_json::from_value(serde_json::Value::String(name.to_owned()))
                .map_err(|error| format!("unknown emulation backend {name:?}: {error}"))
        };
        let parsed = match name.as_deref().map(parse).transpose() {
            Ok(parsed) => parsed,
            Err(error) => {
                log::warn!("rejected emulation backend switch: {error}");
                echo(self);
                return;
            }
        };
        if self.config.emulation_backend_override().is_some() {
            log::warn!(
                "emulation backend switch ignored: --emulation-backend override is in effect"
            );
            echo(self);
            return;
        }
        log::info!(
            "switching emulation backend → {}",
            parsed.as_ref().map_or("auto".to_owned(), |b| b.to_string())
        );
        self.emulation.switch_backend(parsed.map(Into::into));
        self.config.set_emulation_backend(parsed);
        self.save_config().await;
        echo(self);
    }

    fn sync_frontend(&mut self) {
        // Frontends connect after startup: reconcile any authorized
        // peer recorded by a previous session against the clients
        // loaded from config before the first enumeration.
        self.backfill_peer_fingerprints();
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::EmulationBackend(
            self.config.emulation_backend().map(|b| b.to_string()),
        ));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        self.notify_frontend(FrontendEvent::ReleaseThreshold(
            self.config.release_threshold_px(),
        ));
        self.notify_frontend(FrontendEvent::ReleaseBind(self.config.release_bind()));
        self.notify_frontend(FrontendEvent::MdnsDiscovery(self.config.mdns_discovery()));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        self.notify_frontend(self.video_state_event());
        self.notify_frontend(self.video_share_event());
        let host_list = self.config.clipboard_suppression().host().clone();
        self.notify_frontend(FrontendEvent::SuppressedAppsUpdated(host_list));
        let recovery_events: Vec<_> = self
            .remote_host_states
            .iter()
            .map(|(fingerprint, recovery)| FrontendEvent::RemoteHostState {
                fingerprint: fingerprint.clone(),
                addr: recovery.addr,
                state: recovery.state,
            })
            .collect();
        for event in recovery_events {
            self.notify_frontend(event);
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(
        &mut self,
        addr: SocketAddr,
        session: ListenerSession,
        pos: Position,
        fingerprint: String,
    ) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture
            .create(handle, pos, CaptureType::EnterOnly, false, None);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                session,
                pos,
            },
        );
    }

    fn update_incoming(
        &mut self,
        addr: SocketAddr,
        session: ListenerSession,
        pos: Position,
        fingerprint: String,
    ) {
        let incoming = self
            .incoming_conn_info
            .values()
            .find(|incoming| incoming.addr == addr)
            .expect("no such client");
        let previous_session = incoming.session;
        let changed = incoming.session != session
            || incoming.fingerprint != fingerprint
            || incoming.pos != pos;
        if changed {
            self.remove_incoming(addr, previous_session);
            self.add_incoming(addr, session, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn incoming_session(&self, addr: SocketAddr) -> Option<ListenerSession> {
        self.incoming_conn_info
            .values()
            .find(|incoming| incoming.addr == addr)
            .map(|incoming| incoming.session)
    }

    fn remove_incoming(
        &mut self,
        addr: SocketAddr,
        session: ListenerSession,
    ) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr && incoming.session == session)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn mark_remote_host_unavailable(&mut self, addr: SocketAddr) {
        let update = mark_locked_remote_host_unavailable(&mut self.remote_host_states, addr);
        if let Some((fingerprint, addr)) = update {
            self.notify_frontend(FrontendEvent::RemoteHostState {
                fingerprint,
                addr,
                state: RemoteHostState::Unavailable,
            });
        }
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        // New authorizations land with default post-processing; the
        // user can tune natural-scroll / sensitivity from the
        // expanded row in the Incoming Connections list.
        let entry = IncomingPeerConfig {
            description: desc,
            ..IncomingPeerConfig::default()
        };
        self.authorized_keys
            .write()
            .expect("lock")
            .insert(fp, entry);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        self.backfill_peer_fingerprints();
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
        self.backfill_peer_fingerprints();
    }

    /// Console direct-connect (search box): reuse an existing client
    /// that already lists `ip` among its candidates, otherwise create
    /// one, then activate it and kick a dial right away — Direct mode
    /// otherwise waits for the first lazy send, which would leave the
    /// console staring at an offline row after an explicit connect.
    /// Only one client may hold a screen position, so activating can
    /// deactivate the previous occupant; frontends mirror that via the
    /// resulting State events.
    async fn connect_to(&mut self, ip: IpAddr, port: u16) {
        let port = if port == 0 { DEFAULT_PORT } else { port };
        let existing = self
            .client_manager
            .get_client_states()
            .into_iter()
            .find(|(_, c, s)| c.fix_ips.contains(&ip) || s.ips.contains(&ip));
        let handle = match existing {
            Some((handle, _, _)) => {
                log::info!("connect-to {ip}:{port}: reusing existing client {handle}");
                handle
            }
            None => {
                let handle = self.client_manager.add_client();
                self.client_manager.set_config(
                    handle,
                    ClientConfig {
                        fix_ips: vec![ip],
                        port,
                        ..Default::default()
                    },
                );
                self.client_manager.set_fix_ips(handle, vec![ip]);
                let (c, s) = self.client_manager.get_state(handle).unwrap();
                self.notify_frontend(FrontendEvent::Created(handle, c, s));
                log::info!("connect-to {ip}:{port}: created client {handle}");
                handle
            }
        };
        self.backfill_peer_fingerprints();
        self.activate_client(handle);
        if let Err(e) = self.conn.send(ProtoEvent::Ping, handle).await {
            // A failed dial leaves the client offline in the list; the
            // retry backoff gates re-attempts, same as the lazy path.
            log::debug!("connect-to {ip}:{port}: dial pending: {e}");
        }
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            let command_as_ctrl = self.client_manager.command_as_ctrl(handle);
            let crossing_modifier = self.client_manager.required_crossing_modifier(handle);
            self.capture.create(
                handle,
                pos,
                CaptureType::Default,
                command_as_ctrl,
                crossing_modifier,
            );
            self.broadcast_client(handle);
            if self.client_manager.connect_mode(handle) == ConnectMode::Reverse {
                // Dial out immediately instead of waiting up to a tick.
                self.pending_reverse_kicks.push(handle);
            }
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    async fn remove_client(&mut self, handle: ClientHandle) {
        // Retire the exact outbound DTLS generation before Slab can reuse this
        // numeric handle for a different configured peer. The remote side
        // releases its emulation handle on connection close, and Capture gets
        // the matching terminal event before any replacement can dial.
        self.conn.reset_handle(handle).await;
        // Same handle-reuse concern for the tunnel snapshot: drop it before
        // the slot can be handed to another peer.
        self.tunnel_forwards.remove(&handle);
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
        // The edited-in address may be an authorized peer's last known
        // address — link the trust record now rather than waiting for
        // the next inbound session.
        self.backfill_peer_fingerprints();
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    /// Recompute [`hopdeck_ipc::ClientState::incoming`] from the live
    /// inbound session table. `alive` only covers the outbound
    /// transport, so a peer that dialed us must still render as
    /// connected while our own dial is down or absent.
    fn refresh_incoming_flags(&mut self) {
        let live_ips: HashSet<IpAddr> = self.live_incoming.keys().map(|addr| addr.ip()).collect();
        for (handle, config, state) in self.client_manager.get_client_states() {
            let incoming = config
                .fix_ips
                .iter()
                .chain(state.ips.iter())
                .any(|ip| live_ips.contains(ip));
            if self.client_manager.set_incoming(handle, incoming) {
                self.broadcast_client(handle);
            }
        }
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn spawn_hook_command(&self, handle: ClientHandle) {
        let Some(cmd) = self.client_manager.get_enter_cmd(handle) else {
            return;
        };
        tokio::task::spawn_local(async move {
            log::info!("spawning command!");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute cmd: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{cmd} exited successfully");
                    } else {
                        log::warn!("{cmd} exited with {s}");
                    }
                }
                Err(e) => log::warn!("{cmd}: {e}"),
            }
        });
    }
}

/// `tokio::select!` arm helper for the optional [`ClipboardMonitor`].
/// Resolves to `Some(event)` when the monitor surfaces a change and
/// to a never-completing future when no monitor is alive — keeping
/// the surrounding `select!` from busy-spinning when clipboard sync
/// is unavailable on the host.
async fn recv_clipboard(
    monitor: &mut Option<ClipboardMonitor>,
) -> Option<input_capture::CaptureEvent> {
    match monitor.as_mut() {
        Some(m) => m.recv().await,
        None => std::future::pending().await,
    }
}

/// `tokio::select!` arm helper for the discovery event stream. Pends
/// forever once the channel is closed so the `select!` doesn't
/// busy-spin during shutdown.
async fn recv_discovery(rx: &mut Receiver<DiscoveryEvent>) -> DiscoveryEvent {
    match rx.recv().await {
        Some(event) => event,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_hash_is_deterministic_within_run() {
        let h1 = clipboard_hash("hello, world");
        let h2 = clipboard_hash("hello, world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn clipboard_hash_distinguishes_different_inputs() {
        assert_ne!(clipboard_hash("foo"), clipboard_hash("bar"));
        assert_ne!(clipboard_hash(""), clipboard_hash("\0"));
    }

    #[test]
    fn recent_forwarded_prune_evicts_expired_entries() {
        // Mirrors `Service::prune_recent_forwarded` so the eviction
        // contract is documented as code rather than implicit in
        // `HashMap::retain`.
        let mut map: HashMap<(String, u64), Instant> = HashMap::new();
        let now = Instant::now();
        let stale = now
            .checked_sub(Duration::from_secs(2))
            .expect("clock far enough from epoch for the test to subtract 2s");
        map.insert(("fp_a".into(), 1), stale);
        map.insert(("fp_b".into(), 2), now);
        map.retain(|_, ts| ts.elapsed() < RECENT_FORWARD_TTL);
        assert!(!map.contains_key(&("fp_a".to_string(), 1)));
        assert!(map.contains_key(&("fp_b".to_string(), 2)));
    }

    #[test]
    fn disconnect_downgrades_confirmed_lock_to_unavailable() {
        let addr: SocketAddr = "192.0.2.4:4252".parse().expect("address");
        let mut states = HashMap::from([(
            "peer-fingerprint".to_owned(),
            RemoteHostRecovery {
                addr,
                state: RemoteHostState::Locked,
            },
        )]);

        assert_eq!(
            mark_locked_remote_host_unavailable(&mut states, addr),
            Some(("peer-fingerprint".to_owned(), addr))
        );
        assert_eq!(
            states["peer-fingerprint"].state,
            RemoteHostState::Unavailable
        );
        assert_eq!(
            mark_locked_remote_host_unavailable(&mut states, addr),
            None,
            "an unavailable peer must not be reported as a fresh lock"
        );
    }

    #[test]
    fn repeated_wire_state_does_not_reopen_recovery_ui() {
        let addr: SocketAddr = "192.0.2.4:4252".parse().expect("address");
        let mut states = HashMap::new();
        assert!(record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Locked,
        ));
        assert!(!record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Locked,
        ));
        assert!(record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Unlocked,
        ));
        assert!(!record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Unlocked,
        ));
    }
}
