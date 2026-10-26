//! Console window: device list, device detail and the status bar.
//!
//! The frame is the fixed-height shell (design D16) fed by live daemon
//! state. Increment 2 added search direct-connect, the editable device
//! detail and live list badges; increment 3 adds the authorization
//! flow (connection-attempt dialog, inline trust state); increment 5
//! adds the settings overlay (self info + per-trusted-peer sharing
//! options); increment 6 adds the interactive layout canvas: device
//! rows drag onto the six layout slots (`layout.rs`); increment 7 adds
//! the debug log bar: a ring-buffer log sink fed by a console-side
//! journal plus the `log` facade, rendered above the status bar
//! (`logbar.rs`).

mod demo;
mod detail;
mod layout;
/// `pub(crate)`: `main.rs` creates the shared sink and installs the
/// facade logger before the window exists.
pub(crate) mod logbar;
mod settings;
#[cfg(all(test, feature = "ui-test-support"))]
mod ui_tests;
mod video;
pub(crate) mod window_deco;

use crate::ipc::{FeedEvent, RequestWriter};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyWindowHandle, AppContext, ClickEvent, Context, Div, Entity, Hsla, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, TestSupportExt, Window,
    assets::IconName,
    component::{
        ActiveTheme, Icon, Selectable, Sizable, TitleBar, WindowExt,
        button::{Button, ButtonVariants},
        h_flex,
        input::{Input, InputEvent, InputState},
        notification::Notification,
        status_bar::StatusBar,
        v_flex,
    },
    div,
};

use self::detail::latency_label;
use self::layout::DeviceDrag;
use self::logbar::{LogLevel, LogSink};
use mousehop_ipc::{
    ClientConfig, ClientHandle, ClientState, ConnectMode, CrossingModifier, DEFAULT_PORT,
    FrontendEvent, FrontendRequest, IncomingPeerConfig, Position, Status,
};
use std::collections::HashMap;
use std::net::IpAddr;

/// The whole console window state. Device data is the daemon's mirror:
/// every field here comes from a `FrontendEvent`.
pub struct Console {
    feed: FeedState,
    writer: Option<RequestWriter>,
    clients: Vec<DeviceEntry>,
    selected: Option<ClientHandle>,
    /// Direct-connect in flight: the IP whose `Created`/`State` event
    /// should be auto-selected (and stop filtering the list).
    pending_connect: Option<IpAddr>,
    /// Direct-connect retry in flight. `Created` for a new client
    /// arrives before the dial succeeds, so auto-select (see
    /// `pending_connect`) cannot drive this: a denied or unreachable
    /// peer never emits an event at all, and after the daemon's dial
    /// backoff goes quiet nothing would re-kick the lazy dial — e.g.
    /// the peer authorizing us a few seconds later.
    retrying: Option<(IpAddr, u16)>,
    retry_attempts: u32,
    /// A background re-kick task is running for the retry target.
    retry_loop_running: bool,
    /// Last connection-attempt dialog per fingerprint, so handshake
    /// retries while the user is deciding (or just declined) don't
    /// re-open the dialog every couple of seconds.
    last_auth_prompt: Option<(String, std::time::Instant)>,
    /// Fingerprints parked at the daemon's authorization gate, with
    /// the most recent knock time (design D15's 待认证 state). The
    /// daemon re-fires `ConnectionAttempt` while the peer retries, so
    /// stale entries simply age out instead of needing a dedicated
    /// "knocker gave up" signal.
    awaiting_auth: HashMap<String, std::time::Instant>,
    /// Edit form for the selected device; `None` outside edit mode.
    edit: Option<EditDraft>,
    /// Mirror of the daemon's `authorized_fingerprints`.
    authorized: HashMap<String, IncomingPeerConfig>,
    /// Whether the settings overlay (design D15) is open.
    settings_open: bool,
    /// The open settings page (design v2.2's five-entry nav).
    settings_tab: self::settings::SettingsTab,
    /// This device's public-key fingerprint (`PublicKeyFingerprint`,
    /// pushed by the daemon's `Sync`).
    self_fingerprint: Option<String>,
    /// Reusable description input for the authorization dialog (the
    /// dialog is opened from window-less contexts, so the input lives
    /// on the console).
    auth_desc: Entity<InputState>,
    /// Reusable password input for the access-password dialog (same
    /// window-less-context rationale as `auth_desc`).
    auth_password: Entity<InputState>,
    /// The settings overlay's listen-port input (常规 pane); read on
    /// apply, left untouched elsewhere.
    port_input: Entity<InputState>,
    port: u16,
    port_error: Option<SharedString>,
    capture: Status,
    emulation: Status,
    /// Mirror of the daemon's input-emulation backend selection in its
    /// config spelling (`Some("x11")`, …); `None` = automatic probing.
    /// Pushed on sync and after every accepted backend switch.
    emulation_backend: Option<String>,
    self_hostname: SharedString,
    search: Entity<InputState>,
    /// Debug log bar (design D14⑦): open/closed, the facade capture
    /// level, the shared ring buffer and its poll bookkeeping.
    debug_open: bool,
    log_level: LogLevel,
    log_sink: LogSink,
    log_gen_seen: std::cell::Cell<u64>,
    log_scroll: ScrollHandle,
    /// Screen view (M3a increment ④): the daemon's video-channel mirror
    /// plus the local frame buffer fed by the video feed socket.
    video: video::VideoView,
    /// Window access for notifications and input edits outside event
    /// handlers (subscribe callbacks have no `&mut Window`).
    window: AnyWindowHandle,
}

enum FeedState {
    Connecting,
    Connected,
    Disconnected(Option<SharedString>),
}

/// Knocks older than this are stale: the peer stopped retrying (gave
/// up, or the dialog was declined), so the badge falls back to
/// offline. The daemon re-fires every ~2s while a peer is retrying,
/// giving live knocks ample headroom.
const AUTH_KNOCK_TTL: std::time::Duration = std::time::Duration::from_secs(12);

#[derive(Clone)]
struct DeviceEntry {
    handle: ClientHandle,
    config: ClientConfig,
    state: ClientState,
}

impl DeviceEntry {
    fn new(handle: ClientHandle, config: ClientConfig, state: ClientState) -> Self {
        Self {
            handle,
            config,
            state,
        }
    }

    fn name(&self) -> SharedString {
        self.config
            .hostname
            .clone()
            .unwrap_or_else(|| format!("设备 {}", self.handle))
            .into()
    }

    /// Best-known reach address for the row subtitle.
    fn addr_label(&self) -> SharedString {
        if let Some(active) = self.state.active_addr {
            return active.to_string().into();
        }
        if let Some(ip) = self.state.ips.iter().next().or(self.config.fix_ips.first()) {
            return format!("{ip}:{}", self.config.port).into();
        }
        "地址未知".into()
    }

    fn position_label(&self) -> &'static str {
        position_label(self.config.pos)
    }

    fn connect_mode_label(&self) -> &'static str {
        connect_mode_label(self.config.connect_mode)
    }

    fn online(&self) -> bool {
        self.state.alive
    }

    /// Whether the direct-connect query matches this device (hostname
    /// or address substring), shared by list filtering and the
    /// select-on-enter behavior.
    fn matches(&self, filter: &str) -> bool {
        self.config
            .hostname
            .as_deref()
            .is_some_and(|h| h.to_lowercase().contains(filter))
            || self.addr_label().to_lowercase().contains(filter)
    }

    /// The IP a "connect" action should dial: the first pinned address,
    /// else any known candidate.
    fn dial_ip(&self) -> Option<IpAddr> {
        self.config
            .fix_ips
            .first()
            .copied()
            .or_else(|| self.state.ips.iter().next().copied())
    }
}

fn position_label(pos: Position) -> &'static str {
    match pos {
        Position::Left => "左侧",
        Position::Right => "右侧",
        Position::Top => "上方",
        Position::Bottom => "下方",
        Position::TopLeft => "左上角",
        Position::TopRight => "右上角",
        Position::BottomLeft => "左下角",
        Position::BottomRight => "右下角",
        Position::Unplaced => "未布置",
    }
}

fn connect_mode_label(mode: ConnectMode) -> &'static str {
    match mode {
        ConnectMode::Direct => "直连",
        ConnectMode::Reverse => "反向外呼",
    }
}

/// The edit-form draft: input entities plus the two enum choices that
/// render as chip buttons instead of text fields.
struct EditDraft {
    handle: ClientHandle,
    device_name: SharedString,
    hostname: Entity<InputState>,
    ips: Entity<InputState>,
    port: Entity<InputState>,
    pos: Position,
    connect_mode: ConnectMode,
    command_as_ctrl: bool,
    require_crossing_modifier: bool,
    crossing_modifier: CrossingModifier,
}

impl Console {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, log_sink: LogSink) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索设备，或输入 IP 回车直连"));
        cx.subscribe(&search, |this, search, event, cx| match event {
            InputEvent::Change => cx.notify(),
            InputEvent::PressEnter {
                secondary: false,
                shift: false,
            } => this.connect_from_search(&search, cx),
            _ => {}
        })
        .detach();

        let auth_desc =
            cx.new(|cx| InputState::new(window, cx).placeholder("备注名，如 书房笔记本"));
        let auth_password = cx.new(|cx| InputState::new(window, cx).placeholder("访问密码"));
        let port_input = cx.new(|cx| InputState::new(window, cx).placeholder("如 4242，留空不动"));

        let self_hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "本机".to_string());

        let log_level = logbar::initial_level();
        let video_tx = video::spawn_tick_pump(cx);
        let mut console = Self {
            feed: FeedState::Connecting,
            writer: None,
            clients: Vec::new(),
            selected: None,
            pending_connect: None,
            retrying: None,
            retry_attempts: 0,
            retry_loop_running: false,
            last_auth_prompt: None,
            awaiting_auth: HashMap::new(),
            edit: None,
            authorized: HashMap::new(),
            settings_open: false,
            settings_tab: self::settings::SettingsTab::General,
            self_fingerprint: None,
            auth_desc,
            auth_password,
            port_input,
            port: 0,
            port_error: None,
            capture: Status::default(),
            emulation: Status::default(),
            emulation_backend: None,
            self_hostname: self_hostname.into(),
            search,
            debug_open: false,
            log_level,
            log_sink,
            log_gen_seen: std::cell::Cell::new(0),
            log_scroll: ScrollHandle::new(),
            video: video::VideoView::new(video_tx),
            window: window.window_handle(),
        };
        console.spawn_log_poll(cx);
        console.journal(LogLevel::Info, "控制台启动");
        if demo::enabled() {
            console.apply_demo_data(cx);
        }
        console
    }

    /// Entry point for everything the IPC layer observes.
    pub fn on_feed(&mut self, event: FeedEvent, cx: &mut Context<Self>) {
        // Demo mode is a closed dataset injected in `new`: live events
        // (a leftover daemon's `Sync` overwrite, or a disconnect wipe)
        // would clobber it, so the feed is ignored entirely.
        if demo::enabled() {
            return;
        }
        match event {
            FeedEvent::Connected(writer) => {
                self.feed = FeedState::Connected;
                self.writer = Some(writer);
                // `Sync` is a superset of `Enumerate`: it also pushes
                // the statuses, the port and this device's key
                // fingerprint the settings overlay shows.
                self.request(FrontendRequest::Sync);
                self.journal(LogLevel::Ok, "服务通道已建立");
            }
            FeedEvent::Disconnected(reason) => {
                self.journal(
                    LogLevel::Error,
                    match &reason {
                        Some(reason) => format!("服务通道断开：{reason}"),
                        None => "服务通道断开".to_string(),
                    },
                );
                self.feed = FeedState::Disconnected(reason.map(SharedString::from));
                self.writer = None;
                self.clients.clear();
                self.selected = None;
                self.edit = None;
                self.pending_connect = None;
                self.retrying = None;
                self.authorized.clear();
                self.awaiting_auth.clear();
                self.settings_open = false;
                self.self_fingerprint = None;
                self.video.reset();
            }
            FeedEvent::Daemon(event) => self.apply_daemon_event(*event, cx),
        }
        cx.notify();
    }

    fn apply_daemon_event(&mut self, event: FrontendEvent, cx: &mut Context<Self>) {
        match event {
            FrontendEvent::Enumerate(list) => {
                self.journal(LogLevel::Debug, format!("同步设备列表：{} 台", list.len()));
                self.clients = list
                    .into_iter()
                    .map(|(handle, config, state)| DeviceEntry::new(handle, config, state))
                    .collect();
                self.sort_clients();
            }
            FrontendEvent::Created(handle, config, state) => {
                let entry = DeviceEntry::new(handle, config, state);
                self.journal(
                    LogLevel::Info,
                    format!("设备新增：{}（{}）", entry.name(), entry.addr_label()),
                );
                self.clients.push(entry);
                self.sort_clients();
                self.settle_pending_connect(handle, cx);
            }
            FrontendEvent::State(handle, config, state) => {
                // Alive transitions are journaled from the pre-update
                // snapshot; the borrow of `entry_mut` can't span it.
                let transition = self.entry(handle).and_then(|entry| {
                    (entry.state.alive != state.alive).then(|| (entry.name(), state.alive))
                });
                if let Some(entry) = self.entry_mut(handle) {
                    entry.config = config;
                    entry.state = state;
                } else {
                    self.clients
                        .push(DeviceEntry::new(handle, config, state.clone()));
                    self.sort_clients();
                }
                if let Some((name, alive)) = transition {
                    self.journal(
                        if alive { LogLevel::Ok } else { LogLevel::Warn },
                        format!("{}{}", name, if alive { " 上线" } else { " 离线" }),
                    );
                }
                self.settle_pending_connect(handle, cx);
            }
            FrontendEvent::Deleted(handle) => {
                if let Some(entry) = self.entry(handle) {
                    self.journal(LogLevel::Info, format!("设备移除：{}", entry.name()));
                }
                self.clients.retain(|entry| entry.handle != handle);
                if self.selected == Some(handle) {
                    self.selected = None;
                }
                if self.edit.as_ref().is_some_and(|edit| edit.handle == handle) {
                    self.edit = None;
                }
            }
            FrontendEvent::PortChanged(port, error) => {
                match &error {
                    Some(error) => {
                        self.journal(LogLevel::Error, format!("UDP 端口 {port} 异常：{error}"))
                    }
                    None if port != 0 => {
                        self.journal(LogLevel::Info, format!("UDP 端口监听 {port}"))
                    }
                    None => {}
                }
                self.port = port;
                self.port_error = error.map(SharedString::from);
            }
            FrontendEvent::CaptureStatus(status) => self.capture = status,
            FrontendEvent::EmulationStatus(status) => self.emulation = status,
            FrontendEvent::EmulationBackend(name) => {
                self.journal(
                    LogLevel::Info,
                    format!("仿真后端 → {}", name.as_deref().unwrap_or("自动")),
                );
                self.emulation_backend = name;
            }
            FrontendEvent::PublicKeyFingerprint(fp) => self.self_fingerprint = Some(fp),
            FrontendEvent::AuthorizedUpdated(keys) => {
                // Once authorized the peer is past the gate; the badge
                // reverts to connecting/offline until it dials through.
                for (fp, peer) in &keys {
                    if !self.authorized.contains_key(fp) {
                        let name = if peer.description.is_empty() {
                            short_fingerprint(fp).to_string()
                        } else {
                            peer.description.clone()
                        };
                        self.journal(LogLevel::Ok, format!("已授权：{name}"));
                    }
                }
                self.awaiting_auth.retain(|fp, _| !keys.contains_key(fp));
                self.authorized = keys;
            }
            FrontendEvent::ConnectionAttempt { fingerprint } => {
                self.awaiting_auth
                    .insert(fingerprint.clone(), std::time::Instant::now());
                self.prompt_authorization(fingerprint, cx)
            }
            FrontendEvent::PasswordRequired { addr } => self.prompt_access_password(addr, cx),
            FrontendEvent::DeviceConnected { fingerprint, .. } => {
                self.awaiting_auth.remove(&fingerprint);
            }
            FrontendEvent::VideoState {
                handle,
                viewing,
                available,
                width,
                height,
                max_fps,
            } => {
                let was_viewing = self.video.state.viewing;
                self.video.daemon_state(video::VideoStateMirror {
                    handle,
                    viewing,
                    available,
                    width,
                    height,
                    max_fps,
                });
                if !was_viewing && viewing {
                    self.journal(LogLevel::Ok, "画面通道已连接");
                } else if was_viewing && !viewing {
                    self.journal(LogLevel::Info, "画面通道已断开");
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Wake-ups from the local video feed reader thread (`video.rs`).
    fn on_video_tick(&mut self, tick: video::VideoTick, cx: &mut Context<Self>) {
        match tick {
            video::VideoTick::Frame => self.video.frames += 1,
            video::VideoTick::Ended => {
                if self.video.state.viewing {
                    self.journal(LogLevel::Warn, "画面通道异常断开");
                }
                self.video.on_feed_ended();
            }
        }
        cx.notify();
    }

    /// An unauthorized device knocked (DTLS handshake reached the
    /// daemon): ask the user once per dialog lifetime. Retried
    /// handshake attempts while the dialog is up are dropped.
    fn prompt_authorization(&mut self, fingerprint: String, cx: &mut Context<Self>) {
        const AUTH_PROMPT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);
        if self.authorized.contains_key(&fingerprint) || self.writer.is_none() {
            return;
        }
        if matches!(&self.last_auth_prompt, Some((fp, at)) if *fp == fingerprint && at.elapsed() < AUTH_PROMPT_COOLDOWN)
        {
            return;
        }
        self.last_auth_prompt = Some((fingerprint.clone(), std::time::Instant::now()));
        self.journal(
            LogLevel::Info,
            format!(
                "认证请求：{}（等待本机确认）",
                short_fingerprint(&fingerprint)
            ),
        );
        let writer = self.writer.clone();
        let input = self.auth_desc.clone();
        let window = self.window;
        let _ = window.update(cx, move |_, window, cx| {
            if window.has_active_dialog(cx) {
                return;
            }
            input.update(cx, |state, cx| state.set_value("", window, cx));
            window.open_alert_dialog(cx, move |alert, _, _| {
                let fingerprint = fingerprint.clone();
                let writer = writer.clone();
                let input = input.clone();
                alert
                    .title("远程设备请求连接")
                    .description(format!(
                        "指纹 {}。确认后该设备可向本机发送键鼠事件；指纹仅在此次核对。",
                        short_fingerprint(&fingerprint)
                    ))
                    .child(Input::new(&input).small())
                    .confirm()
                    .ok_text("允许连接")
                    .cancel_text("忽略")
                    .on_ok(move |_, _, cx| {
                        let desc = input.read(cx).value().trim().to_string();
                        let desc = if desc.is_empty() {
                            "已授权设备".to_string()
                        } else {
                            desc
                        };
                        if let Some(writer) = &writer {
                            writer.send(FrontendRequest::AuthorizeKey(desc, fingerprint.clone()));
                        }
                        true
                    })
                    .on_cancel(|_, _, _| true)
            });
        });
    }

    /// The peer we dialed wants the access password (session admission
    /// for unauthorized devices on the receiving side). One dialog per
    /// challenge — the daemon re-fires `PasswordRequired` only when a
    /// fresh challenge is actually open, so no cooldown is needed here.
    fn prompt_access_password(&mut self, addr: IpAddr, cx: &mut Context<Self>) {
        if self.writer.is_none() {
            return;
        }
        self.journal(
            LogLevel::Info,
            format!("访问密码：{addr} 需要密码（等待输入）"),
        );
        let writer = self.writer.clone();
        let input = self.auth_password.clone();
        let window = self.window;
        let _ = window.update(cx, move |_, window, cx| {
            if window.has_active_dialog(cx) {
                return;
            }
            input.update(cx, |state, cx| state.set_value("", window, cx));
            window.open_alert_dialog(cx, move |alert, _, _| {
                let addr = addr;
                let writer = writer.clone();
                let input = input.clone();
                alert
                    .title("对方需要访问密码")
                    .description(format!(
                        "设备 {addr} 开启了访问密码。输入后本次会话放行；不落入对方的持久授权。"
                    ))
                    .child(Input::new(&input).small())
                    .confirm()
                    .ok_text("发送密码")
                    .cancel_text("取消")
                    .on_ok(move |_, _, cx| {
                        let password = input.read(cx).value().to_string();
                        if !password.is_empty() {
                            if let Some(writer) = &writer {
                                writer
                                    .send(FrontendRequest::SubmitAccessPassword { addr, password });
                            }
                        }
                        true
                    })
                    .on_cancel(|_, _, _| true)
            });
        });
    }

    /// Auto-select the device a direct-connect was sent to, so the new
    /// entry is visible without hunting for it, and release the filter.
    fn settle_pending_connect(&mut self, handle: ClientHandle, cx: &mut Context<Self>) {
        let Some(ip) = self.pending_connect else {
            return;
        };
        let hit = self.entry(handle).is_some_and(|entry| {
            entry.config.fix_ips.contains(&ip) || entry.state.ips.contains(&ip)
        });
        if hit {
            // Selection settles immediately; dial retries keep going
            // (`retrying`) until the device reports alive.
            self.pending_connect = None;
            self.selected = Some(handle);
            self.clear_search(cx);
        }
    }

    fn entry(&self, handle: ClientHandle) -> Option<&DeviceEntry> {
        self.clients.iter().find(|entry| entry.handle == handle)
    }

    fn entry_mut(&mut self, handle: ClientHandle) -> Option<&mut DeviceEntry> {
        self.clients.iter_mut().find(|entry| entry.handle == handle)
    }

    fn selected_entry(&self) -> Option<&DeviceEntry> {
        self.selected.and_then(|handle| self.entry(handle))
    }

    fn sort_clients(&mut self) {
        self.clients
            .sort_by_key(|entry| entry.config.hostname.clone().unwrap_or_default());
    }

    fn request(&self, request: FrontendRequest) {
        if let Some(writer) = &self.writer {
            writer.send(request);
        }
    }

    /// Surface a toast even though subscribe/feed callbacks carry no
    /// `&mut Window`.
    fn notify_now(&self, note: Notification, cx: &mut Context<Self>) {
        let _ = self.window.update(cx, |_, window, cx| {
            window.push_notification(note, cx);
        });
    }

    fn clear_search(&self, cx: &mut Context<Self>) {
        let search = self.search.clone();
        let _ = self.window.update(cx, |_, window, cx| {
            search.update(cx, |state, cx| state.set_value("", window, cx));
        });
    }

    /// Enter in the search box: a query that matches an existing device
    /// just selects it; anything else must parse as `ip` or `ip:port`
    /// and becomes a direct-connect request to the daemon.
    fn connect_from_search(&mut self, search: &Entity<InputState>, cx: &mut Context<Self>) {
        let query = search.read(cx).value().trim().to_string();
        if query.is_empty() {
            return;
        }
        let filter = query.to_lowercase();
        if let Some(entry) = self.clients.iter().find(|entry| entry.matches(&filter)) {
            self.selected = Some(entry.handle);
            self.clear_search(cx);
            cx.notify();
            return;
        }

        let Some((ip, port)) = parse_direct_address(&query) else {
            self.notify_now(
                Notification::warning(format!(
                    "未找到匹配「{query}」的设备，它也不是有效地址（如 192.168.1.10 或 192.168.1.10:{DEFAULT_PORT}）"
                )),
                cx,
            );
            return;
        };
        if self.writer.is_none() {
            self.notify_now(Notification::error("服务未连接，无法发起连接请求"), cx);
            return;
        }
        self.request(FrontendRequest::ConnectTo(ip, port));
        self.journal(LogLevel::Info, format!("直连请求：{ip}:{port}"));
        self.pending_connect = Some(ip);
        self.retrying = Some((ip, port));
        self.retry_attempts = 1;
        self.spawn_connect_retries(cx);
        self.notify_now(
            Notification::info(format!("正在向 {ip}:{port} 发起连接请求，等待对方确认…")),
            cx,
        );
        self.clear_search(cx);
        cx.notify();
    }

    /// Re-send `ConnectTo` every few seconds until the target device
    /// reports alive (or the attempt budget runs out). A denied or
    /// unreachable peer never emits an event, so after the daemon's
    /// dial backoff goes quiet nothing would otherwise retry —
    /// including the case where the peer authorizes us seconds later.
    fn spawn_connect_retries(&mut self, cx: &mut Context<Self>) {
        if self.retry_loop_running {
            return;
        }
        self.retry_loop_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                let Ok(retry) = this.update(cx, |this, _| {
                    let Some((ip, port)) = this.retrying else {
                        this.retry_loop_running = false;
                        return false;
                    };
                    let alive = ip_alive(this.clients.iter(), ip);
                    if alive || this.retry_attempts >= 10 {
                        this.retrying = None;
                        this.retry_attempts = 0;
                        this.retry_loop_running = false;
                        return false;
                    }
                    this.retry_attempts += 1;
                    this.journal(
                        LogLevel::Debug,
                        format!("直连重试 {}/10 → {ip}:{port}", this.retry_attempts),
                    );
                    this.request(FrontendRequest::ConnectTo(ip, port));
                    true
                }) else {
                    return;
                };
                if !retry {
                    return;
                }
            }
        })
        .detach();
    }

    fn is_connected(&self) -> bool {
        matches!(self.feed, FeedState::Connected)
    }

    /// Feed state wording for the status bar's disconnected segment.
    fn feed_label(&self) -> SharedString {
        match &self.feed {
            FeedState::Connecting => "正在连接服务…".into(),
            FeedState::Connected => "服务已连接".into(),
            FeedState::Disconnected(Some(reason)) => format!("服务连接失败：{reason}").into(),
            FeedState::Disconnected(None) => "服务已断开".into(),
        }
    }

    fn render_shell(
        &self,
        visible: &[DeviceEntry],
        resize_zones: Option<gpui_kit::AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Bare drag strip: the window manager already identifies the
            // app (公子: 标题没必要). The minimize/maximize/close buttons
            // are drawn here rather than relying on the platform's
            // window_controls(), which not every backend reports.
            .child(
                TitleBar::new().child(
                    h_flex()
                        .ml_auto()
                        .items_center()
                        .gap_1()
                        .pr_2()
                        .child(
                            Button::new("win-minimize")
                                .icon(Icon::new(IconName::WindowMinimize))
                                .ghost()
                                .on_click(cx.listener(|_, _: &ClickEvent, window, _| {
                                    window.minimize_window();
                                })),
                        )
                        .child(
                            Button::new("win-maximize")
                                .icon(Icon::new(IconName::WindowMaximize))
                                .ghost()
                                .on_click(cx.listener(|_, _: &ClickEvent, window, _| {
                                    window.zoom_window();
                                })),
                        )
                        .child(
                            Button::new("win-close")
                                .icon(Icon::new(IconName::X))
                                .ghost()
                                .on_click(cx.listener(|_, _: &ClickEvent, window, _| {
                                    window.remove_window();
                                })),
                        ),
                ),
            )
            .child(self.render_main(visible, cx))
            .when(self.debug_open, |shell| {
                shell.child(self.render_log_bar(cx))
            })
            .child(self.render_status_bar(cx))
            // Painted last so the edge bands sit above all content.
            .when_some(resize_zones, |shell, zones| shell.child(zones))
    }

    fn render_main(&self, visible: &[DeviceEntry], cx: &mut Context<Self>) -> impl IntoElement {
        // Settings is a page, not an overlay (公子: 避免浮层 hover 泄漏
        // 到背后的设备行): the two-pane area is swapped out entirely.
        if self.settings_open {
            return self.render_settings_page(cx).into_any_element();
        }
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(self.render_device_panel(visible, cx))
            .child(self.render_detail_panel(cx))
            .into_any_element()
    }

    fn render_device_panel(
        &self,
        visible: &[DeviceEntry],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .w_72()
            .flex_shrink_0()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div().p_2().child(
                    Input::new(&self.search)
                        .small()
                        .prefix(Icon::new(IconName::Search).small().text_color(muted)),
                ),
            )
            .child(self.render_device_list(visible, cx))
            .child(self.render_self_bar(cx))
    }

    fn render_device_list(
        &self,
        visible: &[DeviceEntry],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mut body = v_flex().p_2().gap_1();
        if self.clients.is_empty() {
            body = body.child(self.render_empty_list(cx).into_any_element());
        } else if visible.is_empty() {
            // design v2.2's filtered empty state: a label plus the
            // direct-connect hint.
            body = body
                .child(
                    div()
                        .px_1()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .child("无匹配设备"),
                )
                .child(
                    div()
                        .px_1()
                        .text_xs()
                        .text_color(muted)
                        .child("输入对方 IP 回车，可直接发起连接。"),
                );
        } else {
            body = body
                .child(
                    div()
                        .px_1()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("局域网 · {}", visible.len())),
                )
                .children(
                    visible
                        .iter()
                        .map(|entry| self.render_device_row(entry, cx).into_any_element()),
                );
        }

        div()
            .id("device-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(body)
    }

    fn render_empty_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .px_3()
            .py_4()
            .text_sm()
            .text_color(muted)
            .child("未发现设备。可在上方输入 IP 回车发起直连。")
    }

    fn render_device_row(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = entry.handle;
        let selected = self.selected == Some(handle);
        let online = entry.online();
        let name = entry.name();
        let addr = entry.addr_label();

        h_flex()
            .id(("device-row", handle))
            .test_support()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .text_sm()
            .when(selected, |row| row.bg(cx.theme().secondary))
            .hover(|row| row.bg(cx.theme().secondary_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = Some(handle);
                cx.notify();
            }))
            // Drag source for the layout canvas (`layout.rs`); the ghost
            // floats under the cursor until the drop resolves.
            .on_drag(
                DeviceDrag {
                    handle,
                    name: name.clone(),
                    online,
                },
                |drag, _, _, cx| cx.new(|_| drag.clone()),
            )
            .child(
                h_flex()
                    .size_7()
                    .flex_shrink_0()
                    .rounded_md()
                    .bg(cx.theme().secondary)
                    .when(selected, |box_| box_.bg(cx.theme().background))
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::Monitor)
                            .small()
                            .text_color(cx.theme().secondary_foreground),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .child(self.render_presence_dot(online, entry, cx))
                            .child(
                                div()
                                    .text_color(if online {
                                        cx.theme().foreground
                                    } else {
                                        cx.theme().muted_foreground
                                    })
                                    .truncate()
                                    .child(name),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(addr),
                    ),
            )
            .child(self.render_row_tail(entry, cx))
    }

    /// Row status dot: green online, amber parked at the authorization
    /// gate, accent while dialing, muted offline — design v2.2's
    /// `.mh-dot` color code (the row carries no status pill).
    fn render_presence_dot(
        &self,
        online: bool,
        entry: &DeviceEntry,
        cx: &mut Context<Self>,
    ) -> Div {
        let color = if online {
            cx.theme().success
        } else if self.awaiting_authorization(entry) {
            cx.theme().warning
        } else if entry.state.resolving || self.dial_in_flight(entry) {
            cx.theme().accent
        } else {
            cx.theme().muted_foreground
        };
        presence_dot(color)
    }

    /// Right-aligned row tail: the live latency for an online device,
    /// the interim state word otherwise (design keeps the pill out of
    /// the list; the dot carries the state).
    fn render_row_tail(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let mut tail = div().flex_shrink_0().text_xs();
        if entry.online() {
            tail = tail
                .text_color(muted)
                .font_family(mono)
                .child(latency_label(&entry.state));
        } else if self.awaiting_authorization(entry) {
            tail = tail.text_color(cx.theme().warning).child("待认证");
        } else if entry.state.resolving || self.dial_in_flight(entry) {
            tail = tail.text_color(muted).child("连接中");
        }
        tail
    }

    /// Row/header status chip: online, then parked at the local
    /// authorization gate, then dialing (DNS or the direct-connect
    /// retry loop), then offline.
    fn status_chip(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> (&'static str, Hsla) {
        if entry.online() {
            ("在线", cx.theme().success)
        } else if self.awaiting_authorization(entry) {
            ("待认证", cx.theme().warning)
        } else if entry.state.resolving || self.dial_in_flight(entry) {
            ("连接中", cx.theme().muted_foreground)
        } else {
            ("离线", cx.theme().muted_foreground)
        }
    }

    /// The device's fingerprint is waiting at the daemon's
    /// authorization gate: it knocked (`ConnectionAttempt`) but the
    /// user has not allowed it yet. The initiator side has no protocol
    /// signal for this — its own row only ever shows 连接中.
    fn awaiting_authorization(&self, entry: &DeviceEntry) -> bool {
        entry
            .state
            .peer_fingerprint
            .as_deref()
            .is_some_and(|fp| self.awaiting_auth.contains_key(fp))
    }

    /// The device is the target of the in-flight direct-connect retry
    /// loop — the initiator's only "connecting" knowledge is its own
    /// dials (a pending peer never emits an event).
    fn dial_in_flight(&self, entry: &DeviceEntry) -> bool {
        self.retrying.is_some_and(|(ip, _)| {
            entry.config.fix_ips.contains(&ip) || entry.state.ips.contains(&ip)
        })
    }

    /// Left-bottom self bar (design v2.2's `.mh-self`): the local
    /// device's name plus a `本机 · fp …` subtitle and the settings
    /// gear.
    fn render_self_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        let accent = cx.theme().accent;
        let subtitle = match &self.self_fingerprint {
            Some(fp) => format!("本机 · fp {}", short_self_fingerprint(fp)),
            None => "本机".to_string(),
        };
        h_flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .flex_shrink_0()
            .child(
                h_flex()
                    .size_8()
                    .flex_shrink_0()
                    .rounded_md()
                    .bg(accent.opacity(0.12))
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Monitor).small().text_color(accent)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(self.self_hostname.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .font_family(mono)
                            .truncate()
                            .child(subtitle),
                    ),
            )
            .child(
                Button::new("open-settings")
                    .icon(Icon::new(IconName::Settings))
                    .ghost()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.settings_open = true;
                        cx.notify();
                    })),
            )
    }

    fn render_detail_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(self.render_detail(cx))
            .child(self.render_layout_canvas(cx))
    }

    /// Status bar (design v2.2's `.mh-statusbar`): identity + backends
    /// on the left, device count, the cloud placeholder and the log /
    /// settings buttons on the right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let status_text = move |text: SharedString| div().text_xs().text_color(muted).child(text);
        let capture_on = bool::from(self.capture);
        let emulation_on = bool::from(self.emulation);
        let backend = self
            .emulation_backend
            .as_deref()
            .unwrap_or("自动")
            .to_string();

        StatusBar::new()
            .flex_shrink_0()
            .left(presence_dot(if self.is_connected() {
                cx.theme().success
            } else {
                cx.theme().muted_foreground
            }))
            .left(status_text(self.self_hostname.clone()))
            // The design's status bar is a connected-state mock; the
            // disconnected reason still needs a home (D27③), so it
            // replaces the port segment while the feed is down.
            .left(if self.is_connected() {
                status_text(match (self.port, &self.port_error) {
                    (0, None) => "端口未就绪".into(),
                    (port, None) => format!(":{port}").into(),
                    (port, Some(error)) => format!(":{port} {error}").into(),
                })
                .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(self.feed_label())
                    .into_any_element()
            })
            .left(status_text(
                format!(
                    "仿真 {}{} · 捕获 {}",
                    backend,
                    if emulation_on { "" } else { "（关）" },
                    if capture_on { "开" } else { "关" }
                )
                .into(),
            ))
            .right(status_text(
                format!("设备 {} 台", self.clients.len()).into(),
            ))
            .right(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(presence_dot(muted))
                    .child(status_text("云服务未配置".into())),
            )
            .right(
                Button::new("toggle-debug-log")
                    .icon(Icon::new(IconName::Bug))
                    .ghost()
                    .small()
                    .selected(self.debug_open)
                    .toggled(self.debug_open)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.set_debug_open(!this.debug_open, cx);
                    })),
            )
    }
}

/// Small rounded label, design v2.2's `.mh-badge`: soft tinted
/// background with a matching foreground.
fn chip(label: &'static str, bg: Hsla, fg: Hsla) -> Div {
    h_flex()
        .h_5()
        .px_2()
        .items_center()
        .rounded_full()
        .text_xs()
        .bg(bg)
        .text_color(fg)
        .flex_shrink_0()
        .child(label.to_string())
}

/// Whether any configured client listing `ip` is currently alive.
fn ip_alive<'a>(clients: impl Iterator<Item = &'a DeviceEntry>, ip: IpAddr) -> bool {
    clients
        .filter(|entry| entry.config.fix_ips.contains(&ip) || entry.state.ips.contains(&ip))
        .any(|entry| entry.online())
}

fn presence_dot(color: Hsla) -> Div {
    div().size_2().rounded_full().bg(color).flex_shrink_0()
}

/// First byte groups of a colon-hex fingerprint, for dialog copy where
/// the full string would overwhelm the layout.
fn short_fingerprint(fp: &str) -> &str {
    fp.get(..20).unwrap_or(fp)
}

/// The self-bar fingerprint hint, design v2.2's `fp 77:b1:77…9a:2e`:
/// first three hex groups, ellipsis, last group.
fn short_self_fingerprint(fp: &str) -> String {
    let groups: Vec<&str> = fp.split(':').collect();
    match groups.as_slice() {
        [a, b, c, .., last] if groups.len() > 4 => format!("{a}:{b}:{c}…{last}"),
        _ => fp.to_string(),
    }
}

/// Parse a direct-connect query: a bare IP, or an `ip:port` pair
/// (brackets required around IPv6 with a port). Hostnames are not
/// dialed — the daemon's candidate list is IP-based.
fn parse_direct_address(query: &str) -> Option<(IpAddr, u16)> {
    let query = query.trim();
    if let Ok(ip) = query.parse::<IpAddr>() {
        return Some((ip, DEFAULT_PORT));
    }
    let (host, port) = query.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = port.parse::<u16>().ok()?;
    if port == 0 {
        return None;
    }
    host.parse::<IpAddr>().ok().map(|ip| (ip, port))
}

impl Render for Console {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Expire authorization knocks the daemon stopped refreshing:
        // the peer gave up or the dialog was declined. Demo mode has no
        // daemon to re-fire its scripted knock, so it stays fresh.
        if demo::enabled() {
            if let Some(at) = self.awaiting_auth.get_mut(demo::FP_OPS) {
                *at = std::time::Instant::now();
            }
        } else {
            self.awaiting_auth
                .retain(|_, at| at.elapsed() < AUTH_KNOCK_TTL);
        }
        let filter = self.search.read(cx).value().to_lowercase();
        let visible: Vec<DeviceEntry> = if filter.is_empty() {
            self.clients.clone()
        } else {
            self.clients
                .iter()
                .filter(|entry| entry.matches(&filter))
                .cloned()
                .collect()
        };
        let resize_zones = window_deco::resize_zone_overlay(window).map(|e| e.into_any_element());
        self.render_shell(&visible, resize_zones, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn parse_direct_address_accepts_bare_ip() {
        assert_eq!(
            parse_direct_address("192.168.1.10"),
            Some((IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), DEFAULT_PORT))
        );
        assert_eq!(
            parse_direct_address(" ::1 "),
            Some((IpAddr::V6("::1".parse().unwrap()), DEFAULT_PORT))
        );
    }

    #[test]
    fn parse_direct_address_accepts_ip_port() {
        assert_eq!(
            parse_direct_address("10.8.15.60:4356"),
            Some((IpAddr::V4(Ipv4Addr::new(10, 8, 15, 60)), 4356))
        );
        assert_eq!(
            parse_direct_address("[::1]:5000"),
            Some((IpAddr::V6("::1".parse().unwrap()), 5000))
        );
    }

    #[test]
    fn parse_direct_address_rejects_hostnames_and_garbage() {
        assert_eq!(parse_direct_address("my-laptop"), None);
        assert_eq!(parse_direct_address("300.1.2.3"), None);
        assert_eq!(parse_direct_address("10.0.0.1:"), None);
        assert_eq!(parse_direct_address("10.0.0.1:0"), None);
        assert_eq!(parse_direct_address("10.0.0.1:99999"), None);
        assert_eq!(parse_direct_address(""), None);
    }
}
