use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub use input_event::{AppIdent, CrossingModifier, HostKind, RunningApp, host_app_ident, scancode};

mod connect;
mod connect_async;
mod listen;
mod video_feed;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect, connect_once};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;
pub use video_feed::{
    FRAME_HEADER_LEN, VideoFeedFrame, VideoFeedHandle, connect_video_feed, decode_frame,
    encode_frame, read_video_frame, video_socket_path,
};

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind hopdeck socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occurred: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4252;

/// Per-OS clipboard suppression lists. Each host machine reads and
/// writes only the field that matches its own OS (`host()` /
/// `host_mut()`); the remaining fields round-trip through
/// `config.toml` untouched, so a single config file shipped across
/// machines (dotfiles, Syncthing, …) keeps each machine's
/// suppressed-app list intact. Strings are opaque platform
/// identifiers in the section's natural shape — bundle ID for
/// macOS, exe basename for Windows, etc.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardSuppression {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macos: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linux_wayland: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linux_x11: Vec<String>,
}

impl ClipboardSuppression {
    /// Borrow the host machine's slot — the only one the GUI/CLI on
    /// this device ever interacts with. On Linux the choice between
    /// `linux_wayland` and `linux_x11` is decided at runtime via
    /// `WAYLAND_DISPLAY` so a single binary serves both session
    /// types correctly.
    pub fn host(&self) -> &Vec<String> {
        match HostKind::current() {
            HostKind::MacBundle => &self.macos,
            HostKind::WindowsExe => &self.windows,
            HostKind::LinuxWayland => &self.linux_wayland,
            HostKind::LinuxX11 => &self.linux_x11,
        }
    }

    /// Mutable borrow of [`Self::host`] for add/remove operations.
    pub fn host_mut(&mut self) -> &mut Vec<String> {
        match HostKind::current() {
            HostKind::MacBundle => &mut self.macos,
            HostKind::WindowsExe => &mut self.windows,
            HostKind::LinuxWayland => &mut self.linux_wayland,
            HostKind::LinuxX11 => &mut self.linux_x11,
        }
    }

    /// True when every per-OS slot is empty — used by the config
    /// writer to drop the key entirely instead of writing an empty
    /// table on save.
    pub fn is_empty(&self) -> bool {
        self.macos.is_empty()
            && self.windows.is_empty()
            && self.linux_wayland.is_empty()
            && self.linux_x11.is_empty()
    }
}

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
    /// The four canvas corners (console layout v3): a corner client is
    /// triggered from the two adjacent screen edges near that corner.
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// Not on the layout canvas: no capture barrier is armed and the
    /// emulation side never sends. Configs from older releases never
    /// carry this value; the console's remove action writes it.
    Unplaced,
}

impl Position {
    /// The position across the host screen along the same axis. A
    /// corner's opposite is the mirrored corner; `Unplaced` is its own
    /// opposite (nothing to be across from).
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
            Position::TopLeft => Position::TopRight,
            Position::TopRight => Position::TopLeft,
            Position::BottomLeft => Position::BottomRight,
            Position::BottomRight => Position::BottomLeft,
            Position::Unplaced => Position::Unplaced,
        }
    }

    /// Whether this position arms a capture barrier / emulation route.
    /// `Unplaced` devices stay in the device list but sit out of the
    /// layout canvas entirely.
    pub fn is_placed(self) -> bool {
        self != Position::Unplaced
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            "topleft" | "top-left" => Ok(Self::TopLeft),
            "topright" | "top-right" => Ok(Self::TopRight),
            "bottomleft" | "bottom-left" => Ok(Self::BottomLeft),
            "bottomright" | "bottom-right" => Ok(Self::BottomRight),
            "unplaced" => Ok(Self::Unplaced),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
                Position::TopLeft => "top-left",
                Position::TopRight => "top-right",
                Position::BottomLeft => "bottom-left",
                Position::BottomRight => "bottom-right",
                Position::Unplaced => "unplaced",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            "top-left" => Ok(Position::TopLeft),
            "top-right" => Ok(Position::TopRight),
            "bottom-left" => Ok(Position::BottomLeft),
            "bottom-right" => Ok(Position::BottomRight),
            "unplaced" => Ok(Position::Unplaced),
            _ => Err(()),
        }
    }
}

/// Base outbound-connection policy for a peer. Portable — it applies
/// on every network, and is the fallback whenever the current network
/// has no explicit lock in [`ClientConfig::network_locks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    /// Race every candidate address; first DTLS handshake wins
    /// (biased toward the mDNS-advertised primary interface). The
    /// historical behavior — predictable and never auto-switches,
    /// but doesn't account for measured latency.
    Auto,
    /// Prefer the lowest-latency reachable candidate and stay on it
    /// (sticky). Only re-evaluates when the active path dies or
    /// another candidate is *sustainedly and substantially* faster.
    /// The default: for latency-sensitive input it's a strict
    /// improvement on Auto, and portable (no hand-pinned IPs).
    #[default]
    Fastest,
}

/// How the DTLS session toward this peer is established.
///
/// `Direct` (default) keeps the historical lazy behavior: the machine
/// that wants to *send* dials on first use, so the session exists only
/// while this side is controlling.
///
/// `Reverse` is the one-way-network lever: this machine proactively
/// dials the peer while the client is active and keeps the session
/// alive with pings — for a peer that cannot be reached inbound
/// (firewalled laptop, NAT without port mapping) but can dial out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectMode {
    /// Dial lazily on first send (historical behavior).
    #[default]
    Direct,
    /// Dial proactively while the client is active and keep the
    /// session alive; for peers that cannot be reached inbound.
    Reverse,
}

#[derive(Debug, Error)]
#[error("not a valid connect mode: {mode} — expected `direct` or `reverse`")]
pub struct ConnectModeParseError {
    mode: String,
}

impl FromStr for ConnectMode {
    type Err = ConnectModeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "direct" => Ok(Self::Direct),
            "reverse" => Ok(Self::Reverse),
            _ => Err(ConnectModeParseError { mode: s.into() }),
        }
    }
}

/// Physical kind of the network interface an address lives on, as
/// classified by the *advertising* peer (interface type is invisible
/// from L3, so the peer publishes it via mDNS TXT). Used purely for a
/// human-readable label in the address picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IfaceKind {
    /// Ethernet / wired.
    Wired,
    /// Wi-Fi (802.11).
    WiFi,
    /// Anything else — VPN/tunnel, cellular, bridge, unknown.
    Other,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
    /// Base connection policy (see [`ConnectionMode`]). Used on any
    /// network without an explicit per-network lock. Defaults to
    /// `Auto`.
    #[serde(default)]
    pub mode: ConnectionMode,
    /// Per-network address locks for a multi-homed peer, keyed by an
    /// opaque network fingerprint (default-gateway MAC, or a subnet
    /// fallback — see the daemon's `network` module). When the
    /// machine is on a network present in this map, the dialer
    /// connects *only* to the mapped IP — the lever to stop a
    /// dual-homed peer from flapping, scoped so it doesn't break
    /// connectivity on other networks (a different LAN simply isn't
    /// in the map → falls back to `mode`). Sticky: a locked address
    /// that goes unreachable does *not* silently fail over. Empty in
    /// legacy configs and by default.
    #[serde(default)]
    pub network_locks: HashMap<String, IpAddr>,
    /// Whether changes to this device's clipboard should be
    /// propagated to this peer. Per-pair gate on the *send* side;
    /// the receiving peer's `IncomingPeerConfig::clipboard_receive`
    /// is the matching gate on the receive side. Both must be true
    /// for clipboard text to flow. Defaults to `false` — clipboard
    /// is a meaningfully different trust scope than mouse/keyboard.
    #[serde(default)]
    pub clipboard_send: bool,
    /// On a macOS sender, translate this outgoing peer's Command keys
    /// into the corresponding Control keys. This is a one-way alias,
    /// not a swap: physical Control remains Control. Defaults to
    /// `false` so existing peers keep their current modifier mapping.
    #[serde(default)]
    pub command_as_ctrl: bool,
    /// When true, the pointer may enter this outgoing client only while
    /// [`Self::crossing_modifier`] is physically held. Default-off preserves
    /// the historical automatic edge crossing for existing configurations.
    #[serde(default)]
    pub require_crossing_modifier: bool,
    /// Modifier family used by [`Self::require_crossing_modifier`]. Stored
    /// separately so turning the requirement off and back on remembers the
    /// user's selection.
    #[serde(default)]
    pub crossing_modifier: CrossingModifier,
    /// How the session toward this peer is established. Defaults to
    /// [`ConnectMode::Direct`]; see the type docs for the one-way
    /// network motivation behind [`ConnectMode::Reverse`].
    #[serde(default)]
    pub connect_mode: ConnectMode,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            mode: ConnectionMode::default(),
            network_locks: HashMap::new(),
            clipboard_send: false,
            command_as_ctrl: false,
            require_crossing_modifier: false,
            crossing_modifier: CrossingModifier::default(),
            connect_mode: ConnectMode::default(),
        }
    }
}

pub type ClientHandle = u64;

/// Per-incoming-peer settings, keyed on the peer's TLS certificate
/// fingerprint in `Config::authorized_fingerprints`. Holds the
/// human-readable description plus receive-side post-processing
/// preferences applied to events forwarded from this peer.
///
/// Wire format is forward-compatible: legacy configs that store a
/// bare string per fingerprint deserialize as
/// `IncomingPeerConfig { description: <string>, .. defaults }`. The
/// custom `Deserialize` impl handles both shapes so users upgrading
/// from older versions don't lose their authorizations.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IncomingPeerConfig {
    pub description: String,
    /// Sign-flip scroll deltas before injection. Mirrors the
    /// libinput natural-scroll concept, but applies only to
    /// virtual-pointer events forwarded from this specific peer
    /// (which bypass libinput entirely on Wayland).
    pub natural_scroll: bool,
    /// Linear multiplier applied to motion deltas before injection.
    /// 1.0 = passthrough. Useful when senders capture motion at
    /// different scales (e.g. a Mac trackpad's small floating-point
    /// deltas vs a Windows high-DPI mouse's count deltas).
    pub mouse_sensitivity: f64,
    /// Most recent IP this peer connected from. Stored as a plain
    /// string (no port) so reconnects from the same machine on a
    /// new ephemeral port don't churn the value. Persists across
    /// daemon restarts so the GUI has something to show even when
    /// the peer is currently offline.
    pub last_addr: Option<String>,
    /// mDNS-resolved hostname for `last_addr` at the time of last
    /// connect, recovered from the discovery layer's
    /// `hostname → ip` cache. `None` if discovery wasn't running
    /// on either side or the peer's mDNS record didn't match the
    /// IP it connected from.
    pub last_hostname: Option<String>,
    /// Whether clipboard text propagated by this peer should be
    /// applied to the local clipboard. Per-pair gate on the
    /// *receive* side; the sending peer's
    /// `ClientConfig::clipboard_send` is the matching gate on the
    /// send side. Both must be true for clipboard text to flow.
    /// Defaults to `false`.
    pub clipboard_receive: bool,
}

impl Default for IncomingPeerConfig {
    fn default() -> Self {
        Self {
            description: String::new(),
            natural_scroll: false,
            mouse_sensitivity: 1.0,
            last_addr: None,
            last_hostname: None,
            clipboard_receive: false,
        }
    }
}

impl<'de> Deserialize<'de> for IncomingPeerConfig {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // Legacy: a bare string is just the description.
        // Current: a struct with description + post-processing fields.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Legacy(String),
            Full {
                description: String,
                #[serde(default)]
                natural_scroll: bool,
                #[serde(default = "default_sensitivity")]
                mouse_sensitivity: f64,
                #[serde(default)]
                last_addr: Option<String>,
                #[serde(default)]
                last_hostname: Option<String>,
                #[serde(default)]
                clipboard_receive: bool,
            },
        }
        fn default_sensitivity() -> f64 {
            1.0
        }
        Ok(match Repr::deserialize(de)? {
            Repr::Legacy(description) => Self {
                description,
                ..Self::default()
            },
            Repr::Full {
                description,
                natural_scroll,
                mouse_sensitivity,
                last_addr,
                last_hostname,
                clipboard_receive,
            } => Self {
                description,
                natural_scroll,
                mouse_sensitivity,
                last_addr,
                last_hostname,
                clipboard_receive,
            },
        })
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// Addresses learned from the peer's mDNS service advertisement
    /// (it publishes every interface's address, so a dual-homed peer
    /// surfaces all of them without the user pinning anything). Folded
    /// into [`Self::ips`] alongside `fix_ips`/`dns_ips`.
    #[serde(default)]
    pub discovered_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// Per-address interface kind (Wired/Wi-Fi/Other) as advertised by
    /// the peer over mDNS TXT. Display-only: drives the label next to
    /// each address in the picker. Absent for peers that don't
    /// advertise it (older builds, mDNS off).
    #[serde(default)]
    pub interfaces: HashMap<IpAddr, IfaceKind>,
    /// The lock that applies on the machine's *current* network,
    /// resolved by the daemon from [`ClientConfig::network_locks`].
    /// `Some` means the dialer is pinned to this IP here; `None` means
    /// the base [`ClientConfig::mode`] is in effect. Exposed so the
    /// GUI can show which address (if any) is pinned on this network.
    #[serde(default)]
    pub active_lock: Option<IpAddr>,
    /// Most-recent round-trip latency probe per candidate address, in
    /// microseconds. Surfaced in the GUI next to each address so the
    /// user can pick which interface to lock on a multi-homed peer.
    /// A value of `Some(us)` is a reachable address measured at `us`
    /// microseconds; `None` is an address whose last probe timed out
    /// or was unreachable; an *absent* key means "not probed yet".
    /// Populated by the daemon's background latency prober; defaults
    /// to empty and is never persisted (live runtime state only).
    #[serde(default)]
    pub latencies: HashMap<IpAddr, Option<u32>>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
    /// TLS certificate fingerprint from the peer's most recent
    /// inbound session, matched back to this configured client by
    /// source address. Links an outgoing client entry to its record
    /// in `authorized_fingerprints` so a frontend can show the trust
    /// state inline. `None` until the first inbound connection.
    #[serde(default)]
    pub peer_fingerprint: Option<String>,
}

/// Receiver-side view of a remote host's input availability. `Locked` and
/// `Unlocked` are authenticated, peer-confirmed states. `Unavailable` is a
/// local connection fact and must never be presented as confirmation that it
/// is safe to type a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteHostState {
    Locked,
    Unlocked,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occurred
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// the active input-emulation backend in its config spelling
    /// (`"x11"`, `"usbip"`, …), or `None` for automatic probing.
    /// Pushed on sync and after every accepted backend switch.
    EmulationBackend(Option<String>),
    /// authorized public key fingerprints have been updated.
    /// The map's value carries each peer's per-pair receive-side
    /// post-processing preferences alongside its description.
    AuthorizedUpdated(HashMap<String, IncomingPeerConfig>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// Authenticated lock-recovery status for a remote host. The frontend
    /// presents guidance only; it never accepts or transports a password.
    RemoteHostState {
        fingerprint: String,
        addr: SocketAddr,
        state: RemoteHostState,
    },
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt { fingerprint: String },
    /// The daemon dialed a peer that requires the access password and is
    /// waiting for the answer. The frontend collects the password and
    /// replies via [`FrontendRequest::SubmitAccessPassword`]; the daemon
    /// holds it in memory only (never persisted). `addr` is the peer's IP.
    PasswordRequired { addr: IpAddr },
    /// pixel threshold for the wall-press auto-release fallback.
    /// 0 means disabled.
    ReleaseThreshold(u32),
    /// Modifier+key chord that, when held, releases the captured
    /// cursor back to this host. Linux scancodes so the daemon can
    /// match against its in-flight key state without a key-table
    /// round-trip.
    ReleaseBind(Vec<scancode::Linux>),
    /// whether mDNS-SD discovery is on. When true, hopdeck
    /// advertises a `_hopdeck._udp.local.` Bonjour service whose
    /// TXT record's `primary=` field hints at the OS-preferred
    /// interface (Mac service order / Linux default route), so
    /// peers can bias their connection attempts toward the right
    /// interface on multi-homed hosts.
    MdnsDiscovery(bool),
    /// Snapshot of the host-OS clipboard-suppression list. Pushed
    /// on Sync and after every Add/Remove so the GUI never has to
    /// query. The strings are opaque platform identifiers in the
    /// host's natural shape — bundle ID on macOS, exe basename on
    /// Windows, `xdg-toplevel.app_id` on Wayland, `WM_CLASS` on
    /// X11. Other-OS sections in `config.toml` are intentionally
    /// not surfaced — you edit those by sitting in front of that
    /// machine.
    SuppressedAppsUpdated(Vec<String>),
    /// Reply to [`FrontendRequest::ListRunningApps`]: best-effort
    /// list of apps currently running on this device. Empty when
    /// enumeration isn't reachable (no compositor support, missing
    /// permissions, transient race).
    RunningApps(Vec<RunningApp>),
    /// Screen-sharing state on the controlling side (M3a): the device
    /// whose video offer is on file (`None` = none), whether this
    /// machine is currently viewing it, and the announced geometry.
    /// Pushed whenever any of that changes and on `Sync`. The frame
    /// pixels themselves ride the separate binary video feed socket
    /// (`video_feed`), never this JSON channel.
    VideoState {
        #[serde(default)]
        handle: Option<ClientHandle>,
        #[serde(default)]
        viewing: bool,
        #[serde(default)]
        available: bool,
        #[serde(default)]
        width: u16,
        #[serde(default)]
        height: u16,
        #[serde(default)]
        max_fps: u16,
    },
    /// The sharing side's screen-sharing master switch (`[video]`
    /// `enabled` in config.toml), pushed when it flips via
    /// [`FrontendRequest::SetVideoEnabled`] and on `Sync`. This is the
    /// state of *this* machine being viewable — per-client viewing
    /// state rides [`FrontendEvent::VideoState`].
    VideoShare {
        #[serde(default)]
        enabled: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// Direct-connect from the console search box: ensure a client
    /// exists for `ip:port` (reusing one that already lists the
    /// address among its candidates), activate it, and start dialing
    /// immediately. The outcome reaches the frontend through the
    /// usual `Created`/`State` events — an unreachable peer simply
    /// stays offline in the list.
    ConnectTo(IpAddr, u16),
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// Answer a pending [`FrontendEvent::PasswordRequired`] challenge on
    /// the outbound connection toward `addr`. Held in memory only; a
    /// rejected password is dropped so the next attempt prompts again.
    SubmitAccessPassword { addr: IpAddr, password: String },
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// save config file
    SaveConfiguration,
    /// set the wall-press auto-release pixel threshold (0 = disabled)
    SetReleaseThreshold(u32),
    /// set the modifier+key chord that releases captured input. Empty
    /// vec restores the built-in default (Ctrl+Shift+Alt+Meta).
    SetReleaseBind(Vec<scancode::Linux>),
    /// set whether forwarded scroll events from a specific
    /// authorized peer should be sign-inverted on injection.
    /// Keyed on the peer's TLS certificate fingerprint.
    SetIncomingPeerNaturalScroll(String, bool),
    /// set the linear motion-sensitivity multiplier for events
    /// forwarded from a specific authorized peer. 1.0 = passthrough.
    /// Keyed on the peer's TLS certificate fingerprint.
    SetIncomingPeerSensitivity(String, f64),
    /// turn mDNS-SD discovery on or off
    SetMdnsDiscovery(bool),
    /// Toggle whether clipboard changes on this device propagate to
    /// the given outgoing client. Per-pair send-side gate.
    SetClientClipboardSend(ClientHandle, bool),
    /// On a macOS sender, toggle the one-way Command-to-Control alias
    /// for the given outgoing client.
    SetClientCommandAsCtrl(ClientHandle, bool),
    /// Toggle whether crossing into an outgoing client requires its selected
    /// modifier to be held.
    SetClientRequireCrossingModifier(ClientHandle, bool),
    /// Select the modifier family that gates crossing into an outgoing
    /// client.
    SetClientCrossingModifier(ClientHandle, CrossingModifier),
    /// Switch this machine's input-emulation backend live: `Some`
    /// names the backend in its config spelling (`"x11"`, `"usbip"`,
    /// …), `None` restores automatic probing. The daemon tears down
    /// the old backend (releasing held keys), creates the new one and
    /// echoes [`FrontendEvent::EmulationBackend`]; the value persists
    /// to config.toml unless a CLI `--emulation-backend` override is
    /// in effect.
    SetEmulationBackend(Option<String>),
    /// Set the base connection policy ([`ConnectionMode`]) for a
    /// client, and clear any explicit lock on the *current* network
    /// (so picking "Auto"/"Fastest" in the GUI releases a pin set
    /// here). Other networks' locks are untouched.
    SetConnectionMode(ClientHandle, ConnectionMode),
    /// Set how the session toward this client is established
    /// ([`ConnectMode`]). `Reverse` also makes the daemon dial the
    /// peer proactively while the client is active.
    UpdateConnectMode(ClientHandle, ConnectMode),
    /// Pin the outbound address to `ip` on the machine's *current*
    /// network. The daemon computes the network fingerprint, so the
    /// GUI only sends the chosen IP. Numeric only (never a hostname)
    /// so re-resolution can't repoint the lock. Persisted; sticky.
    SetNetworkLock(ClientHandle, IpAddr),
    /// Toggle whether clipboard text from the given authorized peer
    /// is applied to this device's clipboard. Per-pair receive-side
    /// gate, keyed on the peer's TLS certificate fingerprint.
    SetIncomingPeerClipboardReceive(String, bool),
    /// Add a host-OS app identifier to the clipboard suppression
    /// list — that app's clipboard contents will never be
    /// broadcast to peers. The kind is implicit from the OS the
    /// daemon is running on (see [`host_app_ident`]). Idempotent.
    AddSuppressedApp(String),
    /// Remove a host-OS app identifier from the clipboard
    /// suppression list. Idempotent.
    RemoveSuppressedApp(String),
    /// Ask the daemon to enumerate currently-running apps so the
    /// "From running apps" tab in the suppression-list modal can be
    /// populated. The daemon replies with a
    /// [`FrontendEvent::RunningApps`].
    ListRunningApps,
    /// Start or stop viewing a device's shared screen (M3a). The
    /// daemon dials that peer's video channel when it holds a live
    /// `VideoOffer` from it (and tears the channel down on `false`).
    /// State changes echo back via [`FrontendEvent::VideoState`]; the
    /// daemon's `[video] enabled` config on the *sharing* side remains
    /// the master switch ([`FrontendRequest::SetVideoEnabled`]).
    SetVideoView(ClientHandle, bool),
    /// Toggle whether this machine shares its screen at all (the
    /// sharing side's `[video] enabled` master switch, previously
    /// config-only). The daemon binds/tears down the video listener
    /// live, announces the offer to (or withdraws it from) every
    /// established direct session, persists the choice to
    /// config.toml, and echoes [`FrontendEvent::VideoShare`].
    SetVideoEnabled(bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const HOPDECK_SOCKET_NAME: &str = "hopdeck-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(HOPDECK_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(HOPDECK_SOCKET_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ident_matches_within_variant_case_insensitive() {
        let entry = AppIdent::WindowsExe("1Password.exe".into());
        let detected = AppIdent::WindowsExe("1password.exe".into());
        assert!(entry.matches(&detected));
        assert!(detected.matches(&entry));
    }

    #[test]
    fn app_ident_matches_rejects_value_mismatch() {
        let a = AppIdent::LinuxX11("firefox".into());
        let b = AppIdent::LinuxX11("chromium".into());
        assert!(!a.matches(&b));
    }

    #[test]
    fn app_ident_matches_rejects_cross_variant() {
        // A Mac entry for `org.mozilla.firefox` must NOT suppress a
        // Linux peer that reports `firefox` as its WM_CLASS — same
        // semantic app, different platform identifiers, and the
        // suppression list lives on the device that captures the
        // clipboard, not the device that displays the window.
        let mac = AppIdent::MacBundle("org.mozilla.firefox".into());
        let linux = AppIdent::LinuxX11("firefox".into());
        assert!(!mac.matches(&linux));
    }

    #[test]
    fn app_ident_serde_round_trip_json() {
        let cases = [
            AppIdent::MacBundle("com.1password.1password7".into()),
            AppIdent::WindowsExe("1password.exe".into()),
            AppIdent::LinuxX11("firefox".into()),
            AppIdent::LinuxWayland("org.mozilla.firefox".into()),
        ];
        for original in cases {
            let s = serde_json::to_string(&original).expect("encode");
            let decoded: AppIdent = serde_json::from_str(&s).expect("decode");
            assert_eq!(original, decoded, "round-trip mismatch: {s}");
        }
    }

    #[test]
    fn video_state_event_round_trip_and_defaults() {
        let event = FrontendEvent::VideoState {
            handle: Some(7),
            viewing: true,
            available: true,
            width: 320,
            height: 180,
            max_fps: 30,
        };
        let s = serde_json::to_string(&event).expect("encode");
        match serde_json::from_str::<FrontendEvent>(&s).expect("decode") {
            FrontendEvent::VideoState {
                handle,
                viewing,
                available,
                width,
                height,
                max_fps,
            } => {
                assert_eq!(handle, Some(7));
                assert!(viewing);
                assert!(available);
                assert_eq!((width, height, max_fps), (320, 180, 30));
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        // A newer daemon omitting newly-added fields must still decode
        // (the variant's fields are all `#[serde(default)]`).
        let decoded: FrontendEvent =
            serde_json::from_str(r#"{"VideoState":{"handle":7,"viewing":true}}"#)
                .expect("lenient decode");
        match decoded {
            FrontendEvent::VideoState {
                handle,
                viewing,
                available,
                width,
                ..
            } => {
                assert_eq!(handle, Some(7));
                assert!(viewing);
                assert!(!available);
                assert_eq!(width, 0);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn video_share_event_round_trip_and_defaults() {
        let event = FrontendEvent::VideoShare { enabled: true };
        let s = serde_json::to_string(&event).expect("encode");
        assert_eq!(s, r#"{"VideoShare":{"enabled":true}}"#);
        match serde_json::from_str::<FrontendEvent>(&s).expect("decode") {
            FrontendEvent::VideoShare { enabled } => assert!(enabled),
            other => panic!("unexpected variant: {other:?}"),
        }

        // Older console JSON without the field must decode (the field
        // is `#[serde(default)]`), mirroring `VideoState`'s leniency.
        let decoded: FrontendEvent = serde_json::from_str(r#"{"VideoShare":{}}"#).expect("lenient");
        match decoded {
            FrontendEvent::VideoShare { enabled } => assert!(!enabled),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn frontend_request_set_video_enabled_wire_shape_is_pinned() {
        let json = serde_json::to_string(&FrontendRequest::SetVideoEnabled(true)).unwrap();
        assert_eq!(json, r#"{"SetVideoEnabled":true}"#);
        match serde_json::from_str(&json).unwrap() {
            FrontendRequest::SetVideoEnabled(enabled) => assert!(enabled),
            other => panic!("wrong variant decoded: {other:?}"),
        }
    }

    #[test]
    fn app_ident_serde_kind_tag_is_snake_case() {
        // Pin the tag rendering — config.toml + IPC consumers depend
        // on these strings, so an accidental rename in a serde
        // attribute would silently break legacy configs.
        let mac = AppIdent::MacBundle("x".into());
        assert_eq!(
            serde_json::to_string(&mac).unwrap(),
            r#"{"kind":"mac_bundle","value":"x"}"#
        );
        let exe = AppIdent::WindowsExe("y".into());
        assert_eq!(
            serde_json::to_string(&exe).unwrap(),
            r#"{"kind":"windows_exe","value":"y"}"#
        );
        let x11 = AppIdent::LinuxX11("z".into());
        assert_eq!(
            serde_json::to_string(&x11).unwrap(),
            r#"{"kind":"linux_x11","value":"z"}"#
        );
        let wl = AppIdent::LinuxWayland("w".into());
        assert_eq!(
            serde_json::to_string(&wl).unwrap(),
            r#"{"kind":"linux_wayland","value":"w"}"#
        );
    }

    #[test]
    fn app_ident_label_includes_platform() {
        assert!(
            AppIdent::MacBundle("com.x.y".into())
                .label()
                .contains("macOS bundle")
        );
        assert!(
            AppIdent::WindowsExe("z.exe".into())
                .label()
                .contains("Windows")
        );
        assert!(AppIdent::LinuxX11("z".into()).label().contains("X11"));
        assert!(
            AppIdent::LinuxWayland("z".into())
                .label()
                .contains("Wayland")
        );
    }

    #[test]
    fn incoming_peer_legacy_string_deserializes() {
        // Configs from before the per-pair scroll/sensitivity work
        // store each authorized peer as a bare description string.
        // Custom Deserialize must accept that shape so upgrading
        // doesn't silently drop authorizations.
        let json = r#""my laptop""#;
        let peer: IncomingPeerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(peer.description, "my laptop");
        assert!(!peer.natural_scroll);
        assert!(!peer.clipboard_receive);
        assert_eq!(peer.mouse_sensitivity, 1.0);
    }

    #[test]
    fn incoming_peer_full_with_clipboard_receive_round_trips() {
        let original = IncomingPeerConfig {
            description: "laptop".into(),
            natural_scroll: true,
            mouse_sensitivity: 1.5,
            last_addr: Some("10.0.0.1".into()),
            last_hostname: Some("foo.local".into()),
            clipboard_receive: true,
        };
        let s = serde_json::to_string(&original).unwrap();
        let decoded: IncomingPeerConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn incoming_peer_legacy_full_without_clipboard_receive_defaults_false() {
        // A v0 "Full" entry (struct shape but no clipboard_receive
        // field) must default the new field to false rather than
        // refusing to decode.
        let json = r#"{
            "description": "laptop",
            "natural_scroll": true,
            "mouse_sensitivity": 1.5,
            "last_addr": "10.0.0.1",
            "last_hostname": "foo.local"
        }"#;
        let peer: IncomingPeerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(peer.description, "laptop");
        assert!(peer.natural_scroll);
        assert_eq!(peer.mouse_sensitivity, 1.5);
        assert!(!peer.clipboard_receive);
    }

    #[test]
    fn frontend_request_connect_to_wire_shape_is_pinned() {
        // The request stream is line-delimited JSON with no handshake;
        // the exact rendering matters because a daemon and frontends
        // built from different revisions can meet on one socket.
        use std::net::Ipv4Addr;
        let request = FrontendRequest::ConnectTo(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 4356);
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"ConnectTo":["192.168.1.10",4356]}"#);
        match serde_json::from_str(&json).unwrap() {
            FrontendRequest::ConnectTo(ip, port) => {
                assert_eq!(ip.to_string(), "192.168.1.10");
                assert_eq!(port, 4356);
            }
            other => panic!("wrong variant decoded: {other:?}"),
        }
    }

    #[test]
    fn client_config_opt_in_fields_default_false() {
        // ClientConfig uses derive(Deserialize) with #[serde(default)]
        // on opt-in fields — a legacy JSON object that omits them must
        // still decode without silently enabling either feature.
        let json = r#"{
            "hostname": null,
            "fix_ips": [],
            "port": 4252,
            "pos": "left",
            "cmd": null
        }"#;
        let cfg: ClientConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.clipboard_send);
        assert!(!cfg.command_as_ctrl);
        assert!(!cfg.require_crossing_modifier);
        assert_eq!(cfg.crossing_modifier, CrossingModifier::Control);
    }

    #[test]
    fn client_config_command_as_ctrl_round_trips() {
        let cfg = ClientConfig {
            command_as_ctrl: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: ClientConfig = serde_json::from_str(&json).unwrap();
        assert!(decoded.command_as_ctrl);
    }

    #[test]
    fn client_config_crossing_modifier_round_trips() {
        let cfg = ClientConfig {
            require_crossing_modifier: true,
            crossing_modifier: CrossingModifier::Super,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: ClientConfig = serde_json::from_str(&json).unwrap();
        assert!(decoded.require_crossing_modifier);
        assert_eq!(decoded.crossing_modifier, CrossingModifier::Super);
    }
}
