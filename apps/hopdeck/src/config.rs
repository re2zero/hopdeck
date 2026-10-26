use crate::capture_test::TestCaptureArgs;
use crate::emulation_test::TestEmulationArgs;
use clap::{Parser, Subcommand, ValueEnum};
use notify::{EventKind, RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env::{self, VarError};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io};
use thiserror::Error;
use toml;
use toml_edit::{self, DocumentMut};

use hopdeck_cli::CliArgs;
use hopdeck_ipc::{
    ClipboardSuppression, ConnectMode, ConnectionMode, CrossingModifier, DEFAULT_PORT,
    IncomingPeerConfig, Position,
};

use crate::tunnel::{DEFAULT_TUNNEL_PORT, ForwardSpec, TargetAddr};

use input_event::scancode::{
    self,
    Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift},
};

use shadow_rs::shadow;

shadow!(build);

/// Local build's 8-byte ASCII short commit hash, suitable for use
/// in [`hopdeck_proto::ProtoEvent::Hello`]. Pads with `'?'` if
/// shadow_rs returns an unexpected length so the field is always
/// well-formed on the wire.
pub fn local_commit() -> [u8; 8] {
    let bytes = build::SHORT_COMMIT.as_bytes();
    let mut out = [b'?'; 8];
    let n = bytes.len().min(8);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

const CONFIG_FILE_NAME: &str = "config.toml";
const CERT_FILE_NAME: &str = "hopdeck.pem";

fn default_path() -> Result<PathBuf, VarError> {
    #[cfg(unix)]
    let default_path = {
        let xdg_config_home =
            env::var("XDG_CONFIG_HOME").unwrap_or(format!("{}/.config", env::var("HOME")?));
        format!("{xdg_config_home}/hopdeck/")
    };

    #[cfg(not(unix))]
    let default_path = {
        let app_data =
            env::var("LOCALAPPDATA").unwrap_or(format!("{}/.config", env::var("USERPROFILE")?));
        format!("{app_data}\\hopdeck\\")
    };
    Ok(PathBuf::from(default_path))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct ConfigToml {
    capture_backend: Option<CaptureBackend>,
    emulation_backend: Option<EmulationBackend>,
    port: Option<u16>,
    release_bind: Option<Vec<scancode::Linux>>,
    /// pixel threshold for the wall-press auto-release fallback.
    /// 0 (or absent) disables it; the cursor only releases on the
    /// release-bind chord or a peer-side `Leave`.
    release_threshold_px: Option<u32>,
    /// Advertise (and consume) `_hopdeck._udp.local.` Bonjour
    /// service records. The TXT record's `primary=` field tells the
    /// dialer which interface IP the OS prefers (macOS service order
    /// / Linux default route), so when a peer is multi-homed the
    /// dialer biases toward the right interface instead of racing
    /// blindly. Default true; turn off on networks where mDNS
    /// multicast (224.0.0.251) is firewalled.
    mdns_discovery: Option<bool>,
    cert_path: Option<PathBuf>,
    clients: Option<Vec<TomlClient>>,
    authorized_fingerprints: Option<HashMap<String, IncomingPeerConfig>>,
    /// Access password for inbound sessions. When set, peers whose
    /// fingerprint is not in `authorized_fingerprints` may complete the DTLS
    /// handshake but must answer a protocol-level challenge with
    /// `SHA-256(salt || nonce || password)` before the session is admitted;
    /// authorized fingerprints never see the challenge. The password is
    /// held in plain text (v1 simplification — it never crosses the wire,
    /// but protect the config file).
    access_password: Option<String>,
    /// Per-OS clipboard suppression lists. Each host machine reads
    /// and writes only its own OS's slot; the rest round-trip
    /// untouched so a single config can be shared across machines.
    /// `None` / empty means no suppression — the user list is
    /// purely additive on top of platform-specific automatic
    /// detection (e.g. macOS `org.nspasteboard.ConcealedType`).
    clipboard_suppress_apps: Option<ClipboardSuppression>,
    /// Reverse tunnel exit (M2c). Presence enables the exit listener;
    /// absent means this machine neither accepts tunnel channels nor
    /// forwards anything. See `.relay/DESIGN-m2c-tunnel-forward.md`.
    tunnel: Option<TomlTunnel>,
    /// Screen-sharing channel (M3a). Only honored by binaries built with
    /// the `video` feature; elsewhere `enabled = true` logs a warning.
    video: Option<TomlVideo>,
}

/// `[video]` — the screen-sharing channel this machine *sends* on
/// (the controlled side captures and encodes; the controlling peer
/// dials in and renders). See `.relay/DESIGN-m3a-video.md`.
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Default)]
pub struct TomlVideo {
    /// Master switch. Default `false` — no video listener is bound and
    /// no `VideoOffer` is announced.
    enabled: Option<bool>,
    /// UDP port of the video listener. `0` (default) binds an ephemeral
    /// port announced dynamically via `VideoOffer`, so no fixed
    /// firewall entry is needed.
    port: Option<u16>,
    /// Capture backend: `"dummy"` (test pattern, default) or `"portal"`
    /// (xdg-desktop-portal ScreenCast, Linux only — a daemon built without
    /// portal support logs a warning and stays off). Unknown values fall
    /// back to `dummy` with a warning; a broken `[video]` section must not
    /// take input sharing down.
    source: Option<String>,
    /// Pattern geometry announced in `VideoOffer` when the source cannot
    /// report a size (`dummy`), and the fallback announcement while a
    /// `portal` session is still negotiating.
    width: Option<u16>,
    height: Option<u16>,
    max_fps: Option<u8>,
    /// XOR parity on the video channel: one recovery packet per group of
    /// 8 data packets, making any single lost packet reconstructable.
    /// Default `false` (LAN loss is rare; PLI keyframe re-request covers
    /// the rest). Both peers need this release — older receivers ignore
    /// the parity flag and misread the stream.
    parity: Option<bool>,
}

/// Resolved `[video]` configuration (defaults filled in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    pub enabled: bool,
    pub port: u16,
    /// Capture backend selected by `[video] source`.
    pub source: VideoSourceKind,
    /// Announced geometry when the source cannot report a size (`dummy`),
    /// and the fallback while a `portal` session is still negotiating.
    pub width: u16,
    pub height: u16,
    pub max_fps: u8,
    /// XOR parity packets on the video channel (design §6, increment ⑤).
    pub parity: bool,
}

/// Capture backend for the video channel (`[video] source`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoSourceKind {
    /// Deterministic color-bar pattern — drives every automated test
    /// (headless environments have no portal).
    #[default]
    Dummy,
    /// xdg-desktop-portal ScreenCast + PipeWire (Linux, needs the
    /// `video_portal` build feature; unavailable builds log and stay off).
    Portal,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 0,
            source: VideoSourceKind::default(),
            width: 1280,
            height: 720,
            max_fps: 30,
            parity: false,
        }
    }
}

/// `[tunnel]` — the exit side of the reverse tunnel.
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Default)]
pub struct TomlTunnel {
    /// Listen address of the tunnel channel; default `127.0.0.1:4253`.
    listen: Option<String>,
    /// Allow rules for forwarded targets (IP / CIDR / `*.`-domain,
    /// optional `:port`). Default deny when absent or empty.
    allow: Option<Vec<String>>,
}

/// One `[[clients.tunnel_forwards]]` entry: a local listener whose
/// connections ride the peer's tunnel exit to `target` (host:port; the
/// exit resolves domains).
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct TomlTunnelForward {
    listen: String,
    target: String,
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
struct TomlClient {
    hostname: Option<String>,
    host_name: Option<String>,
    ips: Option<Vec<IpAddr>>,
    port: Option<u16>,
    position: Option<Position>,
    activate_on_startup: Option<bool>,
    enter_hook: Option<String>,
    /// Base connection policy (`auto` | `fastest`). Absent in legacy
    /// configs → `Auto`. See [`hopdeck_ipc::ConnectionMode`].
    #[serde(default)]
    mode: Option<ConnectionMode>,
    /// Session establishment direction (`direct` | `reverse`). Absent
    /// in legacy configs → `Direct`. See [`hopdeck_ipc::ConnectMode`].
    #[serde(default)]
    connect_mode: Option<ConnectMode>,
    /// Per-network address locks, keyed by network fingerprint. Absent
    /// in legacy configs. See [`hopdeck_ipc::ClientConfig::network_locks`].
    #[serde(default)]
    network_locks: Option<HashMap<String, IpAddr>>,
    /// Per-pair clipboard send gate. Absent in legacy configs;
    /// defaults to `false` so existing pairs don't start
    /// transmitting clipboard text on upgrade.
    clipboard_send: Option<bool>,
    /// macOS-only, per-outgoing-peer Command-to-Control alias.
    /// Absent in legacy configs and default-off.
    #[serde(default)]
    command_as_ctrl: Option<bool>,
    /// Per-outgoing-client edge-crossing gate. Absent in legacy configs and
    /// default-off, preserving automatic crossing.
    #[serde(default)]
    require_crossing_modifier: Option<bool>,
    /// Modifier family retained independently of the toggle so disabling and
    /// re-enabling the gate does not lose the user's selection.
    #[serde(default)]
    crossing_modifier: Option<CrossingModifier>,
    /// Local listeners forwarded through this peer's tunnel exit.
    /// Absent in legacy configs; default empty.
    #[serde(default)]
    tunnel_forwards: Option<Vec<TomlTunnelForward>>,
    /// Port of this peer's tunnel exit. Absent → `tunnel::DEFAULT_TUNNEL_PORT`.
    #[serde(default)]
    tunnel_port: Option<u16>,
}

impl ConfigToml {
    fn new(path: &Path) -> Result<ConfigToml, ConfigError> {
        let config = fs::read_to_string(path)?;
        Ok(toml::from_str::<_>(&config)?)
    }

    /// The resolved `[video]` configuration (defaults filled in).
    fn video(&self) -> VideoConfig {
        let Some(video) = self.video.as_ref() else {
            return VideoConfig::default();
        };
        let defaults = VideoConfig::default();
        // Lenient by design: a mistyped `source` downgrades to the dummy
        // pattern with a warning instead of failing the whole config load.
        let source = match video.source.as_deref() {
            None | Some("dummy") => VideoSourceKind::Dummy,
            Some("portal") => VideoSourceKind::Portal,
            Some(other) => {
                log::warn!("config [video] source = {other:?} is not dummy|portal — using dummy");
                VideoSourceKind::Dummy
            }
        };
        VideoConfig {
            enabled: video.enabled.unwrap_or(false),
            port: video.port.unwrap_or(0),
            source,
            width: video.width.unwrap_or(defaults.width),
            height: video.height.unwrap_or(defaults.height),
            max_fps: video.max_fps.unwrap_or(defaults.max_fps),
            parity: video.parity.unwrap_or(defaults.parity),
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version=build::CLAP_LONG_VERSION, about, long_about = None)]
struct Args {
    /// the listen port for hopdeck
    #[arg(short, long)]
    port: Option<u16>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// capture backend override
    #[arg(long)]
    capture_backend: Option<CaptureBackend>,

    /// emulation backend override
    #[arg(long)]
    emulation_backend: Option<EmulationBackend>,

    /// path to non-default certificate location
    #[arg(long)]
    cert_path: Option<PathBuf>,

    /// subcommands
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// test input emulation
    TestEmulation(TestEmulationArgs),
    /// test input capture
    TestCapture(TestCaptureArgs),
    /// HopDeck commandline interface
    Cli(CliArgs),
    /// run in daemon mode
    Daemon,
    /// Add (or, with --remove, delete) a host-firewall rule allowing
    /// hopdeck's UDP port inbound — for the detected firewall
    /// (ufw/firewalld/nftables/iptables on Linux, Windows Firewall).
    /// Run with sudo / as administrator.
    Firewall(FirewallArgs),
    /// macOS-only: probe Accessibility permission in a fresh process
    /// (which bypasses the cached TCC trust state in already-running
    /// processes). Exits with status 0 if granted, 1 if not. Used by
    /// the daemon's TCC.db watcher to detect "remove from list" cases
    /// where AXIsProcessTrusted in the parent process keeps reporting
    /// cached-true even after the entry is removed.
    #[cfg(target_os = "macos")]
    AxProbe,
}

#[derive(clap::Args, Clone, Debug, Eq, PartialEq)]
pub struct FirewallArgs {
    /// Remove the rule instead of adding it.
    #[arg(long)]
    pub remove: bool,
    /// Print the commands that would run, without executing them.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum CaptureBackend {
    #[cfg(all(unix, feature = "libei_capture", not(target_os = "macos")))]
    #[serde(rename = "input-capture-portal")]
    InputCapturePortal,
    #[cfg(all(unix, feature = "layer_shell_capture", not(target_os = "macos")))]
    #[serde(rename = "layer-shell")]
    LayerShell,
    #[cfg(all(unix, feature = "x11_capture", not(target_os = "macos")))]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl Display for CaptureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(all(unix, feature = "libei_capture", not(target_os = "macos")))]
            CaptureBackend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(all(unix, feature = "layer_shell_capture", not(target_os = "macos")))]
            CaptureBackend::LayerShell => write!(f, "layer-shell"),
            #[cfg(all(unix, feature = "x11_capture", not(target_os = "macos")))]
            CaptureBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            CaptureBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => write!(f, "MacOS"),
            CaptureBackend::Dummy => write!(f, "dummy"),
        }
    }
}

impl From<CaptureBackend> for input_capture::Backend {
    fn from(backend: CaptureBackend) -> Self {
        match backend {
            #[cfg(all(unix, feature = "libei_capture", not(target_os = "macos")))]
            CaptureBackend::InputCapturePortal => Self::InputCapturePortal,
            #[cfg(all(unix, feature = "layer_shell_capture", not(target_os = "macos")))]
            CaptureBackend::LayerShell => Self::LayerShell,
            #[cfg(all(unix, feature = "x11_capture", not(target_os = "macos")))]
            CaptureBackend::X11 => Self::X11,
            #[cfg(windows)]
            CaptureBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => Self::MacOs,
            CaptureBackend::Dummy => Self::Dummy,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum EmulationBackend {
    #[cfg(all(unix, feature = "wlroots_emulation", not(target_os = "macos")))]
    #[serde(rename = "wlroots")]
    Wlroots,
    #[cfg(all(unix, feature = "libei_emulation", not(target_os = "macos")))]
    #[serde(rename = "libei")]
    Libei,
    #[cfg(all(unix, feature = "rdp_emulation", not(target_os = "macos")))]
    #[serde(rename = "xdp")]
    Xdp,
    #[cfg(all(unix, feature = "x11_emulation", not(target_os = "macos")))]
    #[serde(rename = "x11")]
    X11,
    #[cfg(all(unix, feature = "usbip_emulation", not(target_os = "macos")))]
    #[serde(rename = "usbip")]
    Usbip,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl From<EmulationBackend> for input_emulation::Backend {
    fn from(backend: EmulationBackend) -> Self {
        match backend {
            #[cfg(all(unix, feature = "wlroots_emulation", not(target_os = "macos")))]
            EmulationBackend::Wlroots => Self::Wlroots,
            #[cfg(all(unix, feature = "libei_emulation", not(target_os = "macos")))]
            EmulationBackend::Libei => Self::Libei,
            #[cfg(all(unix, feature = "rdp_emulation", not(target_os = "macos")))]
            EmulationBackend::Xdp => Self::Xdp,
            #[cfg(all(unix, feature = "x11_emulation", not(target_os = "macos")))]
            EmulationBackend::X11 => Self::X11,
            #[cfg(all(unix, feature = "usbip_emulation", not(target_os = "macos")))]
            EmulationBackend::Usbip => Self::Usbip,
            #[cfg(windows)]
            EmulationBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => Self::MacOs,
            EmulationBackend::Dummy => Self::Dummy,
        }
    }
}

impl Display for EmulationBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(all(unix, feature = "wlroots_emulation", not(target_os = "macos")))]
            EmulationBackend::Wlroots => write!(f, "wlroots"),
            #[cfg(all(unix, feature = "libei_emulation", not(target_os = "macos")))]
            EmulationBackend::Libei => write!(f, "libei"),
            #[cfg(all(unix, feature = "rdp_emulation", not(target_os = "macos")))]
            EmulationBackend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(all(unix, feature = "x11_emulation", not(target_os = "macos")))]
            EmulationBackend::X11 => write!(f, "X11"),
            #[cfg(all(unix, feature = "usbip_emulation", not(target_os = "macos")))]
            EmulationBackend::Usbip => write!(f, "usbip"),
            #[cfg(windows)]
            EmulationBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => write!(f, "macos"),
            EmulationBackend::Dummy => write!(f, "dummy"),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    /// command line arguments
    args: Args,
    /// path to the certificate file used
    cert_path: PathBuf,
    /// path to the config file used
    config_path: PathBuf,
    /// path to config directory (parent of above)
    config_dir: PathBuf,
    /// the (optional) toml config and it's path
    config_toml: Option<ConfigToml>,
    // filesystem watcher
    watcher: notify::RecommendedWatcher,
    // channel for filesystem events
    watch_rx: tokio::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigClient {
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub active: bool,
    pub enter_hook: Option<String>,
    pub mode: ConnectionMode,
    pub connect_mode: ConnectMode,
    pub network_locks: HashMap<String, IpAddr>,
    pub clipboard_send: bool,
    pub command_as_ctrl: bool,
    pub require_crossing_modifier: bool,
    pub crossing_modifier: CrossingModifier,
    /// Local listeners forwarded through this peer's tunnel exit.
    /// Invalid entries are dropped with a warning, not a load error.
    pub tunnel_forwards: Vec<ForwardSpec>,
    pub tunnel_port: u16,
}

impl From<TomlClient> for ConfigClient {
    fn from(toml: TomlClient) -> Self {
        let active = toml.activate_on_startup.unwrap_or(false);
        let enter_hook = toml.enter_hook;
        let hostname = toml.hostname;
        let ips = HashSet::from_iter(toml.ips.into_iter().flatten());
        let port = toml.port.unwrap_or(DEFAULT_PORT);
        let pos = toml.position.unwrap_or_default();
        let mode = toml.mode.unwrap_or_default();
        let connect_mode = toml.connect_mode.unwrap_or_default();
        let network_locks = toml.network_locks.unwrap_or_default();
        let clipboard_send = toml.clipboard_send.unwrap_or(false);
        let command_as_ctrl = toml.command_as_ctrl.unwrap_or(false);
        let require_crossing_modifier = toml.require_crossing_modifier.unwrap_or(false);
        let crossing_modifier = toml.crossing_modifier.unwrap_or_default();
        let mut tunnel_forwards = Vec::new();
        for entry in toml.tunnel_forwards.into_iter().flatten() {
            match parse_tunnel_forward(&entry) {
                Ok(spec) => tunnel_forwards.push(spec),
                Err(e) => log::warn!(
                    "config: dropping tunnel forward `{}` -> `{}`: {e}",
                    entry.listen,
                    entry.target
                ),
            }
        }
        Self {
            ips,
            hostname,
            port,
            pos,
            active,
            enter_hook,
            mode,
            connect_mode,
            network_locks,
            clipboard_send,
            command_as_ctrl,
            require_crossing_modifier,
            crossing_modifier,
            tunnel_forwards,
            tunnel_port: toml.tunnel_port.unwrap_or(DEFAULT_TUNNEL_PORT),
        }
    }
}

fn parse_tunnel_forward(entry: &TomlTunnelForward) -> Result<ForwardSpec, String> {
    let listen: SocketAddr = entry
        .listen
        .parse()
        .map_err(|_| format!("invalid listen address `{}`", entry.listen))?;
    let target: TargetAddr = entry
        .target
        .parse()
        .map_err(|e| format!("invalid target `{}`: {e}", entry.target))?;
    Ok(ForwardSpec { listen, target })
}

impl From<ConfigClient> for TomlClient {
    fn from(client: ConfigClient) -> Self {
        let hostname = client.hostname;
        let host_name = None;
        let mut ips = client.ips.into_iter().collect::<Vec<_>>();
        ips.sort();
        let ips = Some(ips);
        let port = if client.port == DEFAULT_PORT {
            None
        } else {
            Some(client.port)
        };
        let position = Some(client.pos);
        let activate_on_startup = if client.active { Some(true) } else { None };
        let enter_hook = client.enter_hook;
        // Persist `clipboard_send = true` only when the user has
        // opted in. Default-false stays implicit so configs upgraded
        // from older builds don't gain a new field on every save.
        let clipboard_send = if client.clipboard_send {
            Some(true)
        } else {
            None
        };
        // Like clipboard sharing, the modifier alias is an explicit
        // opt-in. Keep default-false implicit in the persisted file.
        let command_as_ctrl = if client.command_as_ctrl {
            Some(true)
        } else {
            None
        };
        let require_crossing_modifier = if client.require_crossing_modifier {
            Some(true)
        } else {
            None
        };
        // Remember a non-default selection even while the requirement is off.
        let crossing_modifier = (client.crossing_modifier != CrossingModifier::default())
            .then_some(client.crossing_modifier);
        // Persist the mode only when it diverges from the default, and
        // the lock map only when non-empty, so untouched configs don't
        // sprout new keys on every save.
        let mode = (client.mode != ConnectionMode::default()).then_some(client.mode);
        let connect_mode =
            (client.connect_mode != ConnectMode::default()).then_some(client.connect_mode);
        let network_locks = (!client.network_locks.is_empty()).then_some(client.network_locks);
        // Tunnel forwards are opt-in like clipboard sharing: stay implicit
        // when empty so untouched configs don't grow new sections.
        let tunnel_forwards = (!client.tunnel_forwards.is_empty()).then(|| {
            client
                .tunnel_forwards
                .iter()
                .map(|spec| TomlTunnelForward {
                    listen: spec.listen.to_string(),
                    target: format!("{}:{}", spec.target.host, spec.target.port),
                })
                .collect()
        });
        let tunnel_port = (client.tunnel_port != DEFAULT_TUNNEL_PORT).then_some(client.tunnel_port);
        Self {
            hostname,
            host_name,
            ips,
            port,
            position,
            activate_on_startup,
            enter_hook,
            mode,
            connect_mode,
            network_locks,
            clipboard_send,
            command_as_ctrl,
            require_crossing_modifier,
            crossing_modifier,
            tunnel_forwards,
            tunnel_port,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Var(#[from] VarError),
    #[error(transparent)]
    Watcher(#[from] notify::Error),
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        let args = Args::parse();

        // --config <file> overrules default location
        let config_path = args
            .config
            .clone()
            .unwrap_or(default_path()?.join(CONFIG_FILE_NAME));
        let config_dir = config_path
            .parent()
            .expect("config directory")
            .to_path_buf();

        // Ensure the config directory exists and write a default config file
        // if none is present. Runs on every Config::new(), regardless of which
        // entry path (GUI main, spawned daemon, CLI, test commands) we're on,
        // so a fresh Mac never hits "No such file or directory" on config.toml
        // and notify::Watcher (which requires the dir to exist on macOS
        // FSEvents and some Linux backends) has a concrete path to watch.
        fs::create_dir_all(&config_dir)?;
        if !config_path.exists() {
            let default_toml = toml::to_string_pretty(&ConfigToml::default())
                .expect("default ConfigToml serialization cannot fail");
            fs::write(&config_path, default_toml)?;
        }

        let config_toml = match ConfigToml::new(&config_path) {
            Err(e) => {
                log::warn!("{config_path:?}: {e}");
                log::warn!("Continuing without config file ...");
                None
            }
            Ok(c) => Some(c),
        };

        // --cert-path <file> overrules default location
        let cert_path = args
            .cert_path
            .clone()
            .or(config_toml.as_ref().and_then(|c| c.cert_path.clone()))
            .unwrap_or(default_path()?.join(CERT_FILE_NAME));

        let (tx, watch_rx) = tokio::sync::mpsc::channel(16);
        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        )?;
        let mut config = Config {
            args,
            cert_path,
            config_path,
            config_dir,
            config_toml,
            watcher,
            watch_rx,
        };
        config.watch()?;
        Ok(config)
    }

    fn watch(&mut self) -> Result<(), notify::Error> {
        self.watcher
            .watch(&self.config_dir, notify::RecursiveMode::NonRecursive)?;
        Ok(())
    }

    fn unwatch(&mut self) -> Result<(), notify::Error> {
        self.watcher.unwatch(&self.config_dir)?;
        Ok(())
    }

    pub async fn changed(&mut self) -> Result<(), notify::Error> {
        loop {
            let event = self.watch_rx.recv().await.expect("channel closed");
            let event = event.expect("filesystem event");
            if event.paths.contains(&self.config_path)
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
                && self.read_from_disk()?
            {
                return Ok(());
            }
        }
    }

    /// the command to run
    pub fn command(&self) -> Option<Command> {
        self.args.command.clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// public key fingerprints authorized for connection, with each
    /// peer's per-pair receive-side post-processing preferences.
    pub fn authorized_fingerprints(&self) -> HashMap<String, IncomingPeerConfig> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.authorized_fingerprints.clone())
            .unwrap_or_default()
    }

    /// access password for inbound sessions (`None` = password auth off).
    /// Stored in plain text by design (v1); it never crosses the wire.
    pub fn access_password(&self) -> Option<String> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.access_password.clone())
    }

    /// The `[tunnel]` exit configuration, when the section is present.
    /// `None` = exit disabled. A bad listen address or allow rule disables
    /// the exit with a warning instead of failing the whole config load —
    /// a broken tunnel must never take input sharing down with it.
    pub fn tunnel(&self) -> Option<crate::tunnel::ExitConfig> {
        let tunnel = self.config_toml.as_ref()?.tunnel.as_ref()?;
        let default_listen = format!("127.0.0.1:{DEFAULT_TUNNEL_PORT}");
        let listen: SocketAddr = tunnel
            .listen
            .as_deref()
            .unwrap_or(&default_listen)
            .parse()
            .map_err(|e| {
                log::warn!("config: invalid [tunnel].listen: {e} — tunnel exit disabled");
                e
            })
            .ok()?;
        let allow = match &tunnel.allow {
            Some(rules) => match crate::tunnel::AllowList::from_strings(rules) {
                Ok(list) => list,
                Err(e) => {
                    log::warn!("config: invalid [tunnel].allow: {e} — tunnel exit disabled");
                    return None;
                }
            },
            // Absent allow list = default deny: the exit runs but rejects
            // every OPEN. Explicitly configured, still safe by default.
            None => crate::tunnel::AllowList::default(),
        };
        Some(crate::tunnel::ExitConfig { listen, allow })
    }

    /// The `[video]` screen-sharing configuration with defaults filled in.
    /// Absent section = disabled with default geometry.
    pub fn video(&self) -> VideoConfig {
        self.config_toml
            .as_ref()
            .map(|c| c.video())
            .unwrap_or_default()
    }

    /// Toggle the sharing side's `[video] enabled` master switch
    /// (frontend-initiated `SetVideoEnabled`). Only the flag moves:
    /// the running listener is rebuilt by the service; geometry,
    /// source and port stay whatever the section (or defaults) say.
    pub fn set_video_enabled(&mut self, enabled: bool) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .video
            .get_or_insert_with(Default::default)
            .enabled = Some(enabled);
    }

    /// path to certificate
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// optional input-capture backend override
    pub fn capture_backend(&self) -> Option<CaptureBackend> {
        self.args
            .capture_backend
            .or(self.config_toml.as_ref().and_then(|c| c.capture_backend))
    }

    /// optional input-emulation backend override
    pub fn emulation_backend(&self) -> Option<EmulationBackend> {
        self.args
            .emulation_backend
            .or(self.config_toml.as_ref().and_then(|c| c.emulation_backend))
    }

    /// the CLI `--emulation-backend` override, if any. When set it wins
    /// over config.toml, so a frontend backend switch is refused.
    pub fn emulation_backend_override(&self) -> Option<EmulationBackend> {
        self.args.emulation_backend
    }

    /// Persist the input-emulation backend selection (`None` = automatic
    /// probing). The CLI override, when present, keeps precedence.
    pub fn set_emulation_backend(&mut self, backend: Option<EmulationBackend>) {
        let toml = self.config_toml.get_or_insert_with(Default::default);
        toml.emulation_backend = backend;
    }

    /// the port to use (initially)
    pub fn port(&self) -> u16 {
        self.args
            .port
            .or(self.config_toml.as_ref().and_then(|c| c.port))
            .unwrap_or(DEFAULT_PORT)
    }

    /// list of configured clients
    pub fn clients(&self) -> Vec<ConfigClient> {
        self.config_toml
            .as_ref()
            .map(|c| c.clients.clone())
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(From::<TomlClient>::from)
            .collect()
    }

    /// release bind for returning control to the host
    pub fn release_bind(&self) -> Vec<scancode::Linux> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.release_bind.clone())
            .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned()))
    }

    /// Persist a new release-bind chord. An empty `bind` clears the
    /// override so the built-in default applies again.
    pub fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        let slot = &mut self.config_toml.as_mut().expect("config").release_bind;
        if bind.is_empty() {
            *slot = None;
        } else {
            *slot = Some(bind);
        }
    }

    /// Pixel threshold for the wall-press auto-release fallback.
    /// 0 disables it.
    pub fn release_threshold_px(&self) -> u32 {
        self.config_toml
            .as_ref()
            .and_then(|c| c.release_threshold_px)
            .unwrap_or(0)
    }

    pub fn set_release_threshold_px(&mut self, threshold: u32) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .release_threshold_px = Some(threshold);
    }

    /// Whether mDNS-SD discovery is enabled. Defaults to true when
    /// the key is absent.
    pub fn mdns_discovery(&self) -> bool {
        self.config_toml
            .as_ref()
            .and_then(|c| c.mdns_discovery)
            .unwrap_or(true)
    }

    pub fn set_mdns_discovery(&mut self, enabled: bool) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").mdns_discovery = Some(enabled);
    }

    /// set configured clients
    pub fn set_clients(&mut self, clients: Vec<ConfigClient>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").clients = if clients.is_empty() {
            None
        } else {
            Some(clients.into_iter().map(Into::into).collect())
        };
    }

    /// set authorized keys
    pub fn set_authorized_keys(&mut self, fingerprints: HashMap<String, IncomingPeerConfig>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .authorized_fingerprints = Some(fingerprints);
    }

    /// set the daemon listen port (frontend-initiated `ChangePort`). The
    /// running listener is rebuilt by the emulation task; this only makes
    /// the choice survive the next start.
    pub fn set_port(&mut self, port: u16) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").port = Some(port);
    }

    /// Snapshot of the persisted per-OS suppression lists.
    pub fn clipboard_suppression(&self) -> ClipboardSuppression {
        self.config_toml
            .as_ref()
            .and_then(|c| c.clipboard_suppress_apps.clone())
            .unwrap_or_default()
    }

    /// Replace the persisted per-OS suppression lists. `None` is
    /// written when every slot is empty so a fresh config doesn't
    /// gain a new top-level key on every save.
    pub fn set_clipboard_suppression(&mut self, suppression: ClipboardSuppression) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .clipboard_suppress_apps = if suppression.is_empty() {
            None
        } else {
            Some(suppression)
        };
    }

    pub fn read_from_disk(&mut self) -> Result<bool, io::Error> {
        log::info!("reading config from {:?}", self.config_path);

        let current_config = fs::read_to_string(&self.config_path)?;
        let current_config = match current_config.parse::<DocumentMut>() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("{:?} {e}", self.config_path());
                return Ok(false);
            }
        };
        let mut changed = false;
        match toml_edit::de::from_document::<ConfigToml>(current_config) {
            Ok(current_config) => {
                changed = self
                    .config_toml
                    .as_ref()
                    .is_none_or(|c| c != &current_config);
                self.config_toml.replace(current_config);
            }
            Err(e) => log::warn!("{:?} {e}", self.config_path()),
        };
        Ok(changed)
    }

    pub async fn write_back(&mut self) -> Result<(), io::Error> {
        log::info!("writing config to {:?}", self.config_path);
        /* the new config */
        let new_config = self.config_toml.clone().unwrap_or_default();
        let new_config = toml_edit::ser::to_string_pretty(&new_config).expect("config");

        /*
         * TODO merge with current config file to preserve comments
         * => eventually we might want to split this up into clients configured
         * via the config file and clients managed through the GUI / frontend.
         * The latter should be saved to $XDG_DATA_HOME instead of $XDG_CONFIG_HOME,
         * and clients configured through .config could be made permanent.
         * For now we just override the config file.
         */

        let _ = self.unwatch();
        /* Offload the write — and especially the fsync — to the
         * blocking pool. `sync_all()` can stall for many ms on a
         * busy disk, and `write_back` runs on the single-threaded
         * runtime that also drains captured input events; a
         * synchronous fsync here would briefly stall forwarding.
         * Serializing the TOML above is cheap and stays inline;
         * only the I/O is offloaded. The watcher is re-armed after
         * the write completes (await), preserving the unwatch/watch
         * bracket that suppresses a self-write reload. */
        let path = self.config_path().to_path_buf();
        let res = tokio::task::spawn_blocking(move || -> Result<(), io::Error> {
            if let Some(p) = path.parent() {
                fs::create_dir_all(p)?;
            }
            let mut f = File::create(&path)?;
            f.write_all(new_config.as_bytes())?;
            f.sync_all()?;
            Ok(())
        })
        .await
        .expect("config write task panicked");

        let _ = self.watch();

        res
    }
}

#[cfg(test)]
mod connection_mode_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn client_with(mode: ConnectionMode, locks: HashMap<String, IpAddr>) -> ConfigClient {
        ConfigClient {
            ips: Default::default(),
            hostname: Some("host.local".into()),
            port: DEFAULT_PORT,
            pos: Position::Left,
            active: true,
            enter_hook: None,
            mode,
            connect_mode: ConnectMode::default(),
            network_locks: locks,
            clipboard_send: false,
            command_as_ctrl: false,
            require_crossing_modifier: false,
            crossing_modifier: CrossingModifier::default(),
            tunnel_forwards: Vec::new(),
            tunnel_port: DEFAULT_TUNNEL_PORT,
        }
    }

    fn config_with_no_clients() -> Config {
        let (_watch_tx, watch_rx) = tokio::sync::mpsc::channel(1);
        let watcher = RecommendedWatcher::new(
            |_result: Result<notify::Event, notify::Error>| {},
            notify::Config::default(),
        )
        .expect("create config watcher");

        Config {
            args: Args {
                port: None,
                config: None,
                capture_backend: None,
                emulation_backend: None,
                cert_path: None,
                command: None,
            },
            cert_path: PathBuf::new(),
            config_path: PathBuf::new(),
            config_dir: PathBuf::new(),
            config_toml: Some(ConfigToml::default()),
            watcher,
            watch_rx,
        }
    }

    #[test]
    fn removing_last_client_roundtrips_as_no_clients() {
        let mut config = config_with_no_clients();
        config.set_clients(vec![client_with(ConnectionMode::default(), HashMap::new())]);
        assert_eq!(config.clients().len(), 1);

        config.set_clients(Vec::new());
        assert!(config.clients().is_empty());
        assert_eq!(config.config_toml.as_ref().expect("config").clients, None);

        let serialized =
            toml_edit::ser::to_string_pretty(config.config_toml.as_ref().expect("config"))
                .expect("serialize config");
        let reloaded: ConfigToml = toml::from_str(&serialized).expect("reload config");
        assert_eq!(reloaded.clients, None);
    }

    #[test]
    fn mode_and_network_locks_roundtrip_through_toml() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 153));
        let mut locks = HashMap::new();
        locks.insert("gw:aa:bb:cc:dd:ee:ff".to_string(), ip);
        // Use a non-default mode so it's actually persisted.
        let toml: TomlClient = client_with(ConnectionMode::Auto, locks.clone()).into();
        assert_eq!(toml.mode, Some(ConnectionMode::Auto));
        assert_eq!(toml.network_locks.as_ref(), Some(&locks));
        let back: ConfigClient = toml.into();
        assert_eq!(back.mode, ConnectionMode::Auto);
        assert_eq!(back.network_locks.get("gw:aa:bb:cc:dd:ee:ff"), Some(&ip));
    }

    #[test]
    fn defaults_stay_implicit_in_toml() {
        // The default mode + empty locks shouldn't write new keys on save.
        let toml: TomlClient = client_with(ConnectionMode::default(), HashMap::new()).into();
        assert_eq!(toml.mode, None);
        assert_eq!(toml.network_locks, None);
        assert_eq!(toml.command_as_ctrl, None);
        assert_eq!(toml.require_crossing_modifier, None);
        assert_eq!(toml.crossing_modifier, None);
        // A non-default mode IS persisted.
        let toml: TomlClient = client_with(ConnectionMode::Auto, HashMap::new()).into();
        assert_eq!(toml.mode, Some(ConnectionMode::Auto));
    }

    #[test]
    fn connect_mode_roundtrips_through_toml() {
        // Direct stays implicit in the persisted file.
        let toml: TomlClient = client_with(ConnectionMode::default(), HashMap::new()).into();
        assert_eq!(toml.connect_mode, None);
        // A non-default connect mode IS persisted and survives a reload.
        let mut client = client_with(ConnectionMode::default(), HashMap::new());
        client.connect_mode = ConnectMode::Reverse;
        let toml: TomlClient = client.into();
        assert_eq!(toml.connect_mode, Some(ConnectMode::Reverse));
        let back: ConfigClient = toml.into();
        assert_eq!(back.connect_mode, ConnectMode::Reverse);
    }

    #[test]
    fn legacy_toml_without_connect_mode_uses_direct() {
        let parsed: TomlClient = toml::from_str(
            r#"
            hostname = "host.local"
            port = 4252
            "#,
        )
        .expect("parse legacy client");
        assert_eq!(parsed.connect_mode, None);
        let cc: ConfigClient = parsed.into();
        assert_eq!(cc.connect_mode, ConnectMode::Direct);
    }

    #[test]
    fn legacy_toml_without_mode_uses_default() {
        let parsed: TomlClient = toml::from_str(
            r#"
            hostname = "host.local"
            port = 4252
            "#,
        )
        .expect("parse legacy client");
        assert_eq!(parsed.mode, None);
        let cc: ConfigClient = parsed.into();
        assert_eq!(cc.mode, ConnectionMode::default());
        assert!(cc.network_locks.is_empty());
        assert!(!cc.command_as_ctrl);
        assert!(!cc.require_crossing_modifier);
        assert_eq!(cc.crossing_modifier, CrossingModifier::Control);
    }

    #[test]
    fn command_as_ctrl_true_roundtrips_through_toml() {
        let mut client = client_with(ConnectionMode::default(), HashMap::new());
        client.command_as_ctrl = true;
        let toml: TomlClient = client.into();
        assert_eq!(toml.command_as_ctrl, Some(true));

        let back: ConfigClient = toml.into();
        assert!(back.command_as_ctrl);
    }

    #[test]
    fn crossing_modifier_roundtrips_and_remembers_selection_while_disabled() {
        let mut client = client_with(ConnectionMode::default(), HashMap::new());
        client.require_crossing_modifier = true;
        client.crossing_modifier = CrossingModifier::Alt;
        let toml: TomlClient = client.into();
        assert_eq!(toml.require_crossing_modifier, Some(true));
        assert_eq!(toml.crossing_modifier, Some(CrossingModifier::Alt));

        let mut back: ConfigClient = toml.into();
        assert!(back.require_crossing_modifier);
        assert_eq!(back.crossing_modifier, CrossingModifier::Alt);

        back.require_crossing_modifier = false;
        let disabled: TomlClient = back.into();
        assert_eq!(disabled.require_crossing_modifier, None);
        assert_eq!(disabled.crossing_modifier, Some(CrossingModifier::Alt));
    }
}

#[cfg(test)]
mod tunnel_roundtrip_tests {
    use super::*;

    #[test]
    fn tunnel_forwards_round_trip_through_toml() {
        let toml_source = r#"
[[clients]]
hostname = "a"
ips = ["192.168.101.16"]
port = 4307
connect_mode = "reverse"
tunnel_port = 14253

[[clients.tunnel_forwards]]
listen = "127.0.0.1:2222"
target = "127.0.0.1:22"

[[clients.tunnel_forwards]]
listen = "127.0.0.1:18080"
target = "nas.local:5000"

[[clients]]
hostname = "plain"
ips = ["10.0.0.1"]
"#;
        let config: ConfigToml = toml::from_str(toml_source).expect("parse");
        let clients: Vec<ConfigClient> = config
            .clients
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(ConfigClient::from)
            .collect();

        let forwarded = &clients[0];
        assert_eq!(forwarded.tunnel_port, 14253);
        assert_eq!(forwarded.tunnel_forwards.len(), 2);
        assert_eq!(
            forwarded.tunnel_forwards[0].listen.to_string(),
            "127.0.0.1:2222"
        );
        assert_eq!(forwarded.tunnel_forwards[0].target.host, "127.0.0.1");
        assert_eq!(forwarded.tunnel_forwards[0].target.port, 22);
        assert_eq!(forwarded.tunnel_forwards[1].target.host, "nas.local");
        assert_eq!(forwarded.tunnel_forwards[1].target.port, 5000);
        // A client without forwards stays default: empty vec + default port.
        assert!(clients[1].tunnel_forwards.is_empty());
        assert_eq!(clients[1].tunnel_port, DEFAULT_TUNNEL_PORT);

        // Round trip: persisted TOML keeps explicit forwards, stays
        // implicit (no new fields) for the untouched client.
        let persisted: Vec<TomlClient> =
            clients.clone().into_iter().map(TomlClient::from).collect();
        assert_eq!(
            persisted[0].tunnel_forwards.as_ref().expect("kept").len(),
            2
        );
        assert_eq!(persisted[0].tunnel_port, Some(14253));
        assert!(persisted[1].tunnel_forwards.is_none());
        assert!(persisted[1].tunnel_port.is_none());

        // And re-parsing the persisted form yields the same clients.
        let serialized = toml::to_string(&ConfigToml {
            clients: Some(persisted),
            ..Default::default()
        })
        .expect("serialize");
        let reparsed: ConfigToml = toml::from_str(&serialized).expect("reparse");
        let reparsed_clients: Vec<ConfigClient> = reparsed
            .clients
            .unwrap_or_default()
            .into_iter()
            .map(ConfigClient::from)
            .collect();
        assert_eq!(reparsed_clients[0], clients[0]);
    }

    #[test]
    fn invalid_forward_entries_are_dropped_with_defaults() {
        let toml_source = r#"
[[clients]]
hostname = "a"
ips = ["10.0.0.1"]

[[clients.tunnel_forwards]]
listen = "not-an-addr"
target = "127.0.0.1:22"

[[clients.tunnel_forwards]]
listen = "127.0.0.1:2223"
target = "no-port-here"
"#;
        let config: ConfigToml = toml::from_str(toml_source).expect("parse");
        let client =
            ConfigClient::from(config.clients.into_iter().flatten().next().expect("client"));
        assert!(
            client.tunnel_forwards.is_empty(),
            "invalid entries are dropped, not fatal"
        );
    }
}

#[cfg(test)]
mod video_config_tests {
    use super::*;

    #[test]
    fn video_section_resolves_with_defaults() {
        let toml_source = r#"
[video]
enabled = true
port = 4262
"#;
        let config: ConfigToml = toml::from_str(toml_source).expect("parse");
        let resolved = config.video();
        assert!(resolved.enabled);
        assert_eq!(resolved.port, 4262);
        // Geometry not given → defaults.
        assert_eq!(resolved.width, 1280);
        assert_eq!(resolved.height, 720);
        assert_eq!(resolved.max_fps, 30);
        assert_eq!(resolved.source, VideoSourceKind::Dummy);
        assert!(!resolved.parity, "parity is off by default");
    }

    #[test]
    fn video_absent_section_defaults_to_disabled() {
        let config: ConfigToml = toml::from_str("port = 4242").expect("parse");
        let resolved = config.video();
        assert!(!resolved.enabled);
        assert_eq!(resolved.port, 0);
    }

    #[test]
    fn video_round_trips_through_toml() {
        let toml_source = r#"
[video]
enabled = true
port = 0
source = "dummy"
width = 320
height = 180
max_fps = 60
parity = true
"#;
        let config: ConfigToml = toml::from_str(toml_source).expect("parse");
        assert_eq!(
            config.video(),
            VideoConfig {
                enabled: true,
                port: 0,
                source: VideoSourceKind::Dummy,
                width: 320,
                height: 180,
                max_fps: 60,
                parity: true,
            }
        );

        // Round trip: re-serializing keeps the section; untouched configs
        // gain no [video] section at all.
        let serialized = toml::to_string_pretty(&config).expect("serialize");
        let reparsed: ConfigToml = toml::from_str(&serialized).expect("reparse");
        assert_eq!(reparsed.video(), config.video());

        let plain: ConfigToml = toml::from_str("port = 4242").expect("parse");
        let plain_serialized = toml::to_string_pretty(&plain).expect("serialize");
        assert!(!plain_serialized.contains("[video]"));
    }

    #[test]
    fn video_source_parses_and_falls_back_leniently() {
        let portal: ConfigToml = toml::from_str("[video]\nsource = \"portal\"").expect("parse");
        assert_eq!(portal.video().source, VideoSourceKind::Portal);

        let mistyped: ConfigToml = toml::from_str("[video]\nsource = \"pipewire\"").expect("parse");
        assert_eq!(mistyped.video().source, VideoSourceKind::Dummy);
    }
}
