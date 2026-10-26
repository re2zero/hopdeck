//! Demo data (`MOUSEHOP_CONSOLE_DEMO=1`): fills the console with a
//! fixed set of devices covering every renderable state — online
//! trusted, online reverse, pending authorization, offline — without
//! a daemon. Testing-phase visual aid, not a product path: the writer
//! stays `None`, so actions render but send nothing.

use mousehop_ipc::{
    ClientConfig, ClientHandle, ClientState, ConnectMode, FrontendEvent, IncomingPeerConfig,
    Position, Status,
};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::{Console, FeedState, logbar::LogLevel};

const FP_SELF: &str = "77:b1:77:9a:2e:c4:01:33:a7:5f:d0:8e:6b:12:9c:f4:48:aa:e2:07:63:bd:91:0d:55:fa:2b:c8:74:e6:19:3d";
const FP_MI: &str = "a3:19:0c:5e:88:d2:41:7b:90:cc:16:aa:6f:3d:52:e8:b4:11:9d:07:2a:c9:f3:64:58:e0:1b:77:dc:34:9a:2e";
const FP_STUDIO: &str = "f0:6a:33:91:2c:b8:e5:14:7d:aa:08:c3:62:9f:d1:47:3b:e6:50:18:8c:24:fb:6d:95:0a:71:ce:39:83:16:4b";
/// The scripted pending-authorization knock (`ops-mini`); mod.rs keeps
/// it fresh across renders because no daemon re-fires it in demo mode.
pub(super) const FP_OPS: &str = "51:ed:8c:72:b1:77:1d:ab:b3:10:fa:61:b2:67:00:0f:2e:e0:d7:b6:bf:1c:19:e1:32:55:26:b4:f9:82:a9:eb";

pub(super) fn enabled() -> bool {
    std::env::var("MOUSEHOP_CONSOLE_DEMO").is_ok_and(|v| v == "1")
}

fn ip(octets: [u8; 4]) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(octets))
}

/// One demo device spec; `device` turns it into the daemon's triple.
struct DemoDevice {
    handle: ClientHandle,
    hostname: &'static str,
    addr: [u8; 4],
    port: u16,
    pos: Position,
    connect_mode: ConnectMode,
    clipboard_send: bool,
    online: bool,
    active: bool,
    latency_us: Option<u32>,
    fingerprint: Option<&'static str>,
}

fn device(
    DemoDevice {
        handle,
        hostname,
        addr,
        port,
        pos,
        connect_mode,
        clipboard_send,
        online,
        active,
        latency_us,
        fingerprint,
    }: DemoDevice,
) -> (ClientHandle, ClientConfig, ClientState) {
    let ip = ip(addr);
    let config = ClientConfig {
        hostname: Some(hostname.to_string()),
        fix_ips: vec![ip],
        port,
        pos,
        connect_mode,
        clipboard_send,
        ..Default::default()
    };
    let state = ClientState {
        active,
        active_addr: online.then_some(SocketAddr::new(ip, port)),
        alive: online,
        ips: HashSet::from([ip]),
        latencies: latency_us.map(|us| (ip, Some(us))).into_iter().collect(),
        peer_commit: online.then_some([0xef, 0xc2, 0x8c, 0xd0, 0x9a, 0x14, 0x33, 0x77]),
        peer_fingerprint: fingerprint.map(str::to_string),
        ..Default::default()
    };
    (handle, config, state)
}

impl Console {
    /// Inject the demo dataset through the regular daemon-event path so
    /// every rendering rule (badges, trust cards, share cards, canvas)
    /// runs exactly as it would against a real daemon.
    pub(super) fn apply_demo_data(&mut self, cx: &mut gpui_kit::Context<Self>) {
        self.feed = FeedState::Connected;

        let devices = vec![
            device(DemoDevice {
                handle: 0,
                hostname: "mi-book-pro",
                addr: [10, 8, 15, 23],
                port: 4242,
                pos: Position::Right,
                connect_mode: ConnectMode::Direct,
                clipboard_send: true,
                online: true,
                active: true,
                latency_us: Some(1200),
                fingerprint: Some(FP_MI),
            }),
            device(DemoDevice {
                handle: 1,
                hostname: "studio-ryzen",
                addr: [172, 16, 4, 11],
                port: 53112,
                pos: Position::Left,
                connect_mode: ConnectMode::Reverse,
                clipboard_send: true,
                online: true,
                active: true,
                latency_us: Some(800),
                fingerprint: Some(FP_STUDIO),
            }),
            device(DemoDevice {
                handle: 2,
                hostname: "ops-mini",
                addr: [10, 8, 15, 77],
                port: 4242,
                pos: Position::TopLeft,
                connect_mode: ConnectMode::Direct,
                clipboard_send: false,
                online: false,
                active: false,
                latency_us: None,
                fingerprint: Some(FP_OPS),
            }),
            device(DemoDevice {
                handle: 3,
                hostname: "old-imac",
                addr: [192, 168, 1, 40],
                port: 4242,
                pos: Position::BottomRight,
                connect_mode: ConnectMode::Direct,
                clipboard_send: false,
                online: false,
                active: false,
                latency_us: None,
                fingerprint: None,
            }),
        ];

        for event in [
            FrontendEvent::PortChanged(4242, None),
            FrontendEvent::CaptureStatus(Status::Enabled),
            FrontendEvent::EmulationStatus(Status::Enabled),
            FrontendEvent::PublicKeyFingerprint(FP_SELF.to_string()),
            FrontendEvent::VideoShare { enabled: true },
            FrontendEvent::Enumerate(devices),
            FrontendEvent::AuthorizedUpdated(HashMap::from([
                (
                    FP_MI.to_string(),
                    IncomingPeerConfig {
                        description: "书房笔记本".to_string(),
                        clipboard_receive: true,
                        natural_scroll: false,
                        mouse_sensitivity: 1.0,
                        last_addr: Some("10.8.15.23".to_string()),
                        last_hostname: Some("mi-book-pro".to_string()),
                    },
                ),
                (
                    FP_STUDIO.to_string(),
                    IncomingPeerConfig {
                        description: "studio-ryzen".to_string(),
                        clipboard_receive: true,
                        natural_scroll: true,
                        mouse_sensitivity: 1.2,
                        last_addr: Some("172.16.4.11".to_string()),
                        last_hostname: Some("studio-ryzen".to_string()),
                    },
                ),
            ])),
        ] {
            self.apply_daemon_event(event, cx);
        }

        // Pending authorization without the knock dialog: the demo
        // should open on the design's main view, not a modal. The
        // inline card in the detail pane renders from the same state.
        self.awaiting_auth
            .insert(FP_OPS.to_string(), std::time::Instant::now());

        // The design mock pre-selects the first device.
        self.selected = Some(0);
        self.journal(LogLevel::Info, "演示数据已加载（MOUSEHOP_CONSOLE_DEMO=1）");
        cx.notify();
    }
}
