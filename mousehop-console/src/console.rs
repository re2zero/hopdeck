//! Console window: device list, device detail and the status bar.
//!
//! Increment 1 renders the fixed-height shell (design D16) fed by live
//! daemon state. Search direct-connect, the auth flow, the settings
//! overlay and the interactive layout canvas land in later increments;
//! their surfaces are present as placeholders so the frame stays stable.

use crate::ipc::{FeedEvent, RequestWriter};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AppContext, ClickEvent, Context, Div, Entity, Hsla, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    assets::IconName,
    component::{
        ActiveTheme, Disableable, Icon, Sizable, TitleBar,
        button::{Button, ButtonVariants},
        h_flex,
        input::{Input, InputEvent, InputState},
        status_bar::StatusBar,
        v_flex,
    },
    div,
};
use mousehop_ipc::{
    ClientConfig, ClientHandle, ClientState, FrontendEvent, FrontendRequest, Position, Status,
};

/// The whole console window state. Device data is the daemon's mirror:
/// every field here comes from a `FrontendEvent`.
pub struct Console {
    feed: FeedState,
    writer: Option<RequestWriter>,
    clients: Vec<DeviceEntry>,
    selected: Option<ClientHandle>,
    port: u16,
    port_error: Option<SharedString>,
    capture: Status,
    emulation: Status,
    self_hostname: SharedString,
    search: Entity<InputState>,
}

enum FeedState {
    Connecting,
    Connected,
    Disconnected(Option<SharedString>),
}

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
        match self.config.pos {
            Position::Left => "左侧",
            Position::Right => "右侧",
            Position::Top => "上方",
            Position::Bottom => "下方",
        }
    }

    fn online(&self) -> bool {
        self.state.alive
    }
}

impl Console {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索设备，或输入 IP 回车直连"));
        cx.subscribe(&search, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
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
            port: 0,
            port_error: None,
            capture: Status::default(),
            emulation: Status::default(),
            self_hostname: self_hostname.into(),
            search,
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
            }
            FrontendEvent::State(handle, config, state) => {
                if let Some(entry) = self.entry_mut(handle) {
                    entry.config = config;
                    entry.state = state;
                }
            }
            FrontendEvent::Deleted(handle) => {
                self.clients.retain(|entry| entry.handle != handle);
                if self.selected == Some(handle) {
                    self.selected = None;
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

    fn entry_mut(&mut self, handle: ClientHandle) -> Option<&mut DeviceEntry> {
        self.clients.iter_mut().find(|entry| entry.handle == handle)
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

    fn render_shell(&self, visible: &[&DeviceEntry], cx: &mut Context<Self>) -> impl IntoElement {
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

    fn render_main(&self, visible: &[&DeviceEntry], cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(self.render_device_panel(visible, cx))
            .child(self.render_detail_panel(cx))
    }

    fn render_device_panel(
        &self,
        visible: &[&DeviceEntry],
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
        visible: &[&DeviceEntry],
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
        let position = entry.position_label();

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
                            .text_color(cx.theme().foreground)
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
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(position),
            )
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

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("device-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(match self.selected_entry() {
                Some(entry) => self.render_device_detail(entry, cx).into_any_element(),
                None => self.render_detail_placeholder(cx).into_any_element(),
            })
    }

    fn selected_entry(&self) -> Option<&DeviceEntry> {
        self.selected
            .and_then(|handle| self.clients.iter().find(|entry| entry.handle == handle))
    }

    fn render_detail_placeholder(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(muted)
            .child("选择左侧设备查看详情")
    }

    fn render_device_detail(
        &self,
        entry: &DeviceEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let online = entry.online();
        let muted = cx.theme().muted_foreground;
        let name = entry.name();
        let addr = entry.addr_label();
        let position = entry.position_label();
        let port = entry.config.port.to_string();

        v_flex()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(self.render_presence_dot(online, cx))
                    .child(div().text_lg().child(format!(
                        "{}{}",
                        name,
                        if online { "" } else { " · 离线" }
                    ))),
            )
            .child(self.render_detail_line("地址", addr, muted))
            .child(self.render_detail_line("屏幕位置", position.into(), muted))
            .child(self.render_detail_line("端口", port.into(), muted))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("edit-device")
                            .outline()
                            .small()
                            .label("修改配置")
                            .disabled(true),
                    )
                    .child(
                        Button::new("remove-device")
                            .outline()
                            .small()
                            .label("移除设备")
                            .disabled(true),
                    ),
            )
    }

    fn render_detail_line(
        &self,
        label: &str,
        value: SharedString,
        label_color: gpui_kit::Hsla,
    ) -> impl IntoElement {
        h_flex()
            .gap_3()
            .text_sm()
            .child(
                div()
                    .w_16()
                    .flex_shrink_0()
                    .text_color(label_color)
                    .child(label.to_string()),
            )
            .child(value)
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

fn presence_dot(color: Hsla) -> Div {
    div().size_2().rounded_full().bg(color).flex_shrink_0()
}

impl Render for Console {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.search.read(cx).value().to_lowercase();
        let visible: Vec<&DeviceEntry> = self
            .clients
            .iter()
            .filter(|entry| {
                filter.is_empty()
                    || entry
                        .config
                        .hostname
                        .as_deref()
                        .is_some_and(|h| h.to_lowercase().contains(&filter))
                    || entry.addr_label().to_lowercase().contains(&filter)
            })
            .collect();
        self.render_shell(&visible, cx)
    }
}
