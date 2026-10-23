//! Console window: device list, device detail and the status bar.
//!
//! The frame is the fixed-height shell (design D16) fed by live daemon
//! state. Increment 2 adds search direct-connect (`FrontendRequest::
//! ConnectTo`), the editable device detail and live list badges. The
//! auth flow, the settings overlay and the interactive layout canvas
//! land in later increments; their surfaces are placeholders so the
//! frame stays stable.

mod detail;

use crate::ipc::{FeedEvent, RequestWriter};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AnyWindowHandle, AppContext, ClickEvent, Context, Div, Entity, Hsla,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window,
    assets::IconName,
    component::{
        ActiveTheme, Disableable, Icon, Sizable, TitleBar, WindowExt,
        button::{Button, ButtonVariants},
        h_flex,
        input::{Input, InputEvent, InputState},
        notification::Notification,
        status_bar::StatusBar,
        v_flex,
    },
    div,
};
use mousehop_ipc::{
    ClientConfig, ClientHandle, ClientState, ConnectMode, DEFAULT_PORT, FrontendEvent,
    FrontendRequest, Position, Status,
};
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
    /// Edit form for the selected device; `None` outside edit mode.
    edit: Option<EditDraft>,
    port: u16,
    port_error: Option<SharedString>,
    capture: Status,
    emulation: Status,
    self_hostname: SharedString,
    search: Entity<InputState>,
    /// Window access for notifications and input edits outside event
    /// handlers (subscribe callbacks have no `&mut Window`).
    window: AnyWindowHandle,
}

enum FeedState {
    Connecting,
    Connected,
    Disconnected(Option<SharedString>),
}

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
}

impl Console {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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

        let self_hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "本机".to_string());

        Self {
            feed: FeedState::Connecting,
            writer: None,
            clients: Vec::new(),
            selected: None,
            pending_connect: None,
            edit: None,
            port: 0,
            port_error: None,
            capture: Status::default(),
            emulation: Status::default(),
            self_hostname: self_hostname.into(),
            search,
            window: window.window_handle(),
        }
    }

    /// Entry point for everything the IPC layer observes.
    pub fn on_feed(&mut self, event: FeedEvent, cx: &mut Context<Self>) {
        match event {
            FeedEvent::Connected(writer) => {
                self.feed = FeedState::Connected;
                self.writer = Some(writer);
                self.request(FrontendRequest::Enumerate());
            }
            FeedEvent::Disconnected(reason) => {
                self.feed = FeedState::Disconnected(reason.map(SharedString::from));
                self.writer = None;
                self.clients.clear();
                self.selected = None;
                self.edit = None;
                self.pending_connect = None;
            }
            FeedEvent::Daemon(event) => self.apply_daemon_event(*event, cx),
        }
        cx.notify();
    }

    fn apply_daemon_event(&mut self, event: FrontendEvent, cx: &mut Context<Self>) {
        match event {
            FrontendEvent::Enumerate(list) => {
                self.clients = list
                    .into_iter()
                    .map(|(handle, config, state)| DeviceEntry::new(handle, config, state))
                    .collect();
                self.sort_clients();
            }
            FrontendEvent::Created(handle, config, state) => {
                self.clients.push(DeviceEntry::new(handle, config, state));
                self.sort_clients();
                self.settle_pending_connect(handle, cx);
            }
            FrontendEvent::State(handle, config, state) => {
                if let Some(entry) = self.entry_mut(handle) {
                    entry.config = config;
                    entry.state = state;
                } else {
                    self.clients
                        .push(DeviceEntry::new(handle, config, state.clone()));
                    self.sort_clients();
                }
                self.settle_pending_connect(handle, cx);
            }
            FrontendEvent::Deleted(handle) => {
                self.clients.retain(|entry| entry.handle != handle);
                if self.selected == Some(handle) {
                    self.selected = None;
                }
                if self.edit.as_ref().is_some_and(|edit| edit.handle == handle) {
                    self.edit = None;
                }
            }
            FrontendEvent::PortChanged(port, error) => {
                self.port = port;
                self.port_error = error.map(SharedString::from);
            }
            FrontendEvent::CaptureStatus(status) => self.capture = status,
            FrontendEvent::EmulationStatus(status) => self.emulation = status,
            _ => {}
        }
        cx.notify();
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
        self.pending_connect = Some(ip);
        self.notify_now(
            Notification::info(format!("正在向 {ip}:{port} 发起连接请求，等待对方确认…")),
            cx,
        );
        self.clear_search(cx);
        cx.notify();
    }

    fn feed_label(&self) -> SharedString {
        match &self.feed {
            FeedState::Connecting => "正在连接服务…".into(),
            FeedState::Connected => "服务已连接".into(),
            FeedState::Disconnected(Some(reason)) => format!("服务连接失败：{reason}").into(),
            FeedState::Disconnected(None) => "服务已断开".into(),
        }
    }

    fn is_connected(&self) -> bool {
        matches!(self.feed, FeedState::Connected)
    }

    fn render_presence_dot(&self, online: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let color = if online {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        };
        presence_dot(color)
    }

    fn render_shell(&self, visible: &[DeviceEntry], cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Mousehop 控制台"),
                ),
            )
            .child(self.render_main(visible, cx))
            .child(self.render_status_bar(cx))
    }

    fn render_main(&self, visible: &[DeviceEntry], cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(self.render_device_panel(visible, cx))
            .child(self.render_detail_panel(cx))
    }

    fn render_device_panel(
        &self,
        visible: &[DeviceEntry],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .w_72()
            .flex_shrink_0()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(div().p_2().child(Input::new(&self.search).small()))
            .child(self.render_device_list(visible, cx))
            .child(self.render_self_bar(cx))
    }

    fn render_device_list(
        &self,
        visible: &[DeviceEntry],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows: Vec<AnyElement> = if visible.is_empty() {
            vec![self.render_empty_list(cx).into_any_element()]
        } else {
            visible
                .iter()
                .map(|entry| self.render_device_row(entry, cx).into_any_element())
                .collect()
        };

        div()
            .id("device-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(v_flex().p_2().gap_1().children(rows))
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
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_sm()
            .text_sm()
            .when(selected, |row| row.bg(cx.theme().secondary))
            .hover(|row| row.bg(cx.theme().secondary_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = Some(handle);
                cx.notify();
            }))
            .child(self.render_presence_dot(online, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_color(if online {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .truncate()
                            .child(name),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(addr),
                    ),
            )
            .child(self.render_row_chips(entry, cx))
    }

    /// Right-aligned status chips: online/offline plus the reverse
    /// dial marker (direct stays unlabeled — it is the quiet default).
    fn render_row_chips(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        let mut chips = h_flex().gap_1().flex_shrink_0();
        let (label, tone) = if entry.state.resolving {
            ("连接中", cx.theme().muted_foreground)
        } else if entry.online() {
            ("在线", cx.theme().success)
        } else {
            ("离线", cx.theme().muted_foreground)
        };
        chips = chips.child(chip(label, tone.opacity(0.15), tone));
        if entry.config.connect_mode == ConnectMode::Reverse {
            chips = chips.child(chip(
                entry.connect_mode_label(),
                cx.theme().secondary,
                cx.theme().secondary_foreground,
            ));
        }
        chips
    }

    fn render_self_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .flex_shrink_0()
            .child(Icon::new(IconName::Monitor).small().text_color(muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .child(self.self_hostname.clone()),
            )
            .child(
                Button::new("open-settings")
                    .icon(Icon::new(IconName::Settings))
                    .ghost()
                    .small()
                    .disabled(true),
            )
    }

    fn render_detail_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(self.render_detail(cx))
            .child(self.render_layout_strip(cx))
    }

    /// Static preview of the layout canvas; drag-and-drop lands in the
    /// next increment. Host box in the middle, one slot per side.
    fn render_layout_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .gap_3()
            .py_4()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(self.render_layout_slot("左侧", cx))
            .child(self.render_layout_host(cx))
            .child(self.render_layout_slot("右侧", cx))
    }

    fn render_layout_slot(&self, label: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .w_20()
            .h_12()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_dashed()
            .border_color(muted)
            .text_xs()
            .text_color(muted)
            .child(label.to_string())
    }

    fn render_layout_host(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fg = cx.theme().secondary_foreground;
        v_flex()
            .w_24()
            .h_16()
            .items_center()
            .justify_center()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(Icon::new(IconName::Monitor).small().text_color(fg))
            .child(div().text_xs().text_color(fg).child("本机"))
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let status_text = move |text: SharedString| div().text_xs().text_color(muted).child(text);
        let capture_on = bool::from(self.capture);
        let emulation_on = bool::from(self.emulation);

        StatusBar::new()
            .flex_shrink_0()
            .left(self.render_presence_dot(self.is_connected(), cx))
            .left(status_text(self.feed_label()))
            .left(status_text(
                format!("采集 {}", if capture_on { "开" } else { "关" }).into(),
            ))
            .left(status_text(
                format!("注入 {}", if emulation_on { "开" } else { "关" }).into(),
            ))
            .right(status_text(match (self.port, &self.port_error) {
                (0, None) => "端口未知".into(),
                (port, None) => format!("UDP 端口 {port}").into(),
                (port, Some(error)) => format!("端口 {port}：{error}").into(),
            }))
            .right(status_text(
                format!("设备 {} 台", self.clients.len()).into(),
            ))
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

fn presence_dot(color: Hsla) -> Div {
    div().size_2().rounded_full().bg(color).flex_shrink_0()
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        self.render_shell(&visible, cx)
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
