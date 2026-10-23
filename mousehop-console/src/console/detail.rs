//! Device detail panel: summary view, edit form and the actions that
//! talk to the daemon (connect / remove / config updates).

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext, ClickEvent, Context, Div, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window,
    assets::IconName,
    component::{
        ActiveTheme, Disableable, Icon, Sizable, WindowExt,
        button::{Button, ButtonVariant, ButtonVariants},
        h_flex,
        input::{Input, InputState},
        notification::Notification,
        switch::Switch,
        v_flex,
    },
    div,
};
use mousehop_ipc::{ClientHandle, ConnectMode, FrontendRequest, Position};
use std::net::IpAddr;

use super::{Console, DeviceEntry, EditDraft, position_label};

impl Console {
    pub(super) fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("device-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(match self.selected_entry() {
                Some(entry) if self.editing(entry.handle) => {
                    self.render_edit_form(entry, cx).into_any_element()
                }
                Some(entry) => self.render_device_detail(entry, cx).into_any_element(),
                None => self.render_detail_placeholder(cx).into_any_element(),
            })
    }

    fn editing(&self, handle: ClientHandle) -> bool {
        self.edit.as_ref().is_some_and(|edit| edit.handle == handle)
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

        v_flex()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(self.render_device_icon(cx))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                div().text_lg().child(format!(
                                    "{name}{}",
                                    if online { "" } else { " · 离线" }
                                )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(format!("{addr} · {}", entry.position_label())),
                            ),
                    )
                    .child(
                        h_flex()
                            .ml_auto()
                            .gap_1()
                            .child({
                                let (label, tone) = self.status_chip(entry, cx);
                                super::chip(label, tone.opacity(0.15), tone)
                            })
                            .when(entry.config.connect_mode == ConnectMode::Reverse, |row| {
                                row.child(super::chip(
                                    entry.connect_mode_label(),
                                    cx.theme().secondary,
                                    cx.theme().secondary_foreground,
                                ))
                            }),
                    ),
            )
            .child(self.render_kv_section("概览", cx))
            .child(self.render_kv("屏幕位置", entry.position_label().into(), muted))
            .child(self.render_kv("连接方式", entry.connect_mode_label().into(), muted))
            .child(self.render_kv("端口", entry.config.port.to_string().into(), muted))
            .child(self.render_kv("对端版本", peer_version(&entry.state), muted))
            .child(self.render_kv("往返延迟", latency_label(&entry.state), muted))
            .child(self.render_share_section(entry, cx))
            .child(self.render_trust_section(entry, cx))
            .child(self.render_device_actions(entry, cx))
    }

    /// Send-side sharing for this device (design v2.2's detail 共享选项
    /// row): the per-client `clipboard_send` gate. Clipboard text only
    /// flows when this AND the peer's receive gate (settings overlay)
    /// are on. The daemon echoes the change through the usual `State`
    /// event, so the switch state is the config mirror.
    fn render_share_section(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let handle = entry.handle;
        let name = entry.name();
        let writer = self.writer.clone();
        let entity = cx.entity();

        div().child(
            v_flex()
                .mt_1()
                .child(self.render_kv_section("共享选项", cx))
                .child(super::settings::option_row(
                    "剪贴板发送",
                    "本机剪贴板变更同步给该设备（对方也需开启接收）",
                    Switch::new(SharedString::from(format!("clip-send-{handle}")))
                        .checked(entry.config.clipboard_send)
                        .on_click(move |checked, _, cx| {
                            if let Some(writer) = &writer {
                                writer.send(FrontendRequest::SetClientClipboardSend(
                                    handle, *checked,
                                ));
                            }
                            entity.update(cx, |this, cx| {
                                this.journal(
                                    super::logbar::LogLevel::Info,
                                    format!(
                                        "剪贴板发送「{name}」→ {}",
                                        if *checked { "开" } else { "关" }
                                    ),
                                );
                            });
                        })
                        .small(),
                    muted,
                )),
        )
    }

    /// Inline trust state (design D15): once the peer's fingerprint is
    /// known (first inbound session) and authorized, show the record
    /// with a guarded revoke. Devices without a fingerprint yet simply
    /// render nothing here.
    fn render_trust_section(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        div().when_some(
            entry
                .state
                .peer_fingerprint
                .as_deref()
                .and_then(|fp| self.authorized.get(fp).map(|peer| (fp, peer))),
            |this, (fp, peer)| {
                let fp = fp.to_string();
                this.child(
                    v_flex()
                        .mt_1()
                        .gap_1()
                        .child(self.render_kv_section("信任", cx))
                        .child(self.render_kv("状态", "已信任".into(), cx.theme().muted_foreground))
                        .child(self.render_kv(
                            "备注",
                            peer.description.clone().into(),
                            cx.theme().muted_foreground,
                        ))
                        .child(self.render_kv(
                            "最近地址",
                            peer.last_addr.clone().unwrap_or_else(|| "—".into()).into(),
                            cx.theme().muted_foreground,
                        ))
                        .child(self.render_kv(
                            "指纹",
                            super::short_fingerprint(&fp).to_string().into(),
                            cx.theme().muted_foreground,
                        ))
                        .child(
                            Button::new("revoke-trust")
                                .label("撤销信任")
                                .ghost()
                                .danger()
                                .small()
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.confirm_revoke(fp.clone(), window, cx);
                                })),
                        ),
                )
            },
        )
    }

    fn render_device_icon(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_10()
            .flex_shrink_0()
            .rounded_lg()
            .bg(cx.theme().secondary)
            .items_center()
            .justify_center()
            .child(
                Icon::new(IconName::Monitor)
                    .small()
                    .text_color(cx.theme().secondary_foreground),
            )
    }

    pub(super) fn render_kv_section(
        &self,
        title: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .mt_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(title.to_string())
    }

    pub(super) fn render_kv(
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

    fn render_device_actions(
        &self,
        entry: &DeviceEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let online = entry.online();

        let mut row = h_flex().flex_wrap().gap_2().mt_2();
        if online {
            row = row.child(self.render_edit_button(cx, true));
        } else {
            row = row.child(
                Button::new("connect-device")
                    .icon(Icon::new(IconName::RefreshCw))
                    .label("连接")
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.connect_selected(cx);
                    })),
            );
            row = row.child(self.render_edit_button(cx, false));
        }
        row = row.child(
            Button::new("remove-device")
                .icon(Icon::new(IconName::Trash))
                .label("移除设备")
                .ghost()
                .danger()
                .small()
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    let Some(entry) = this.selected_entry() else {
                        return;
                    };
                    this.confirm_remove(entry.handle, window, cx);
                })),
        );
        row.child(
            Button::new("remote-view")
                .icon(Icon::new(IconName::Eye))
                .label("远程查看 · 即将支持")
                .outline()
                .small()
                .disabled(true),
        )
    }

    /// Online the edit action is the primary step (design v2.2); while
    /// a device is offline, connect takes the spotlight instead.
    fn render_edit_button(&self, cx: &mut Context<Self>, primary: bool) -> impl IntoElement {
        Button::new("edit-device")
            .icon(Icon::new(IconName::SlidersHorizontal))
            .label("修改配置")
            .small()
            .when(primary, |b| b.primary())
            .when(!primary, |b| b.outline())
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.start_edit(window, cx);
            }))
    }

    // ── edit form ────────────────────────────────────────────────────

    fn render_edit_form(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let edit = self
            .edit
            .as_ref()
            .expect("render_edit_form without an edit draft");

        v_flex()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(self.render_device_icon(cx))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                div()
                                    .text_lg()
                                    .child(format!("修改配置 · {}", edit.device_name)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("更改会立即重连该设备"),
                            ),
                    ),
            )
            .child(self.render_kv_section("基本", cx))
            .child(self.render_edit_field("设备名称", Input::new(&edit.hostname).small(), cx))
            .child(self.render_edit_field("地址", Input::new(&edit.ips).small(), cx))
            .child(self.render_edit_field("端口", Input::new(&edit.port).small(), cx))
            .child(self.render_position_chips(edit, cx))
            .child(self.render_connect_mode_chips(edit, cx))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .mt_2()
                    .child(
                        Button::new("save-edit")
                            .icon(Icon::new(IconName::Check))
                            .label("保存")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save_edit(window, cx);
                            })),
                    )
                    .child(
                        Button::new("cancel-edit")
                            .label("取消")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.edit = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("当前地址 {}", entry.addr_label())),
                    ),
            )
    }

    fn render_edit_field(
        &self,
        label: &str,
        input: impl IntoElement,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w_16()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(label.to_string()),
            )
            .child(input)
    }

    fn render_position_chips(&self, edit: &EditDraft, cx: &mut Context<Self>) -> impl IntoElement {
        let positions = [
            ("edit-pos-left", Position::Left),
            ("edit-pos-right", Position::Right),
            ("edit-pos-top", Position::Top),
            ("edit-pos-bottom", Position::Bottom),
        ];
        let mut row = h_flex().items_center().gap_2().child(
            div()
                .w_16()
                .flex_shrink_0()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("屏幕位置"),
        );
        for (id, pos) in positions {
            let selected = edit.pos == pos;
            row = row.child(
                Button::new(id)
                    .label(position_label(pos))
                    .small()
                    .when(selected, |b| b.primary())
                    .when(!selected, |b| b.ghost())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(edit) = this.edit.as_mut() {
                            edit.pos = pos;
                        }
                        cx.notify();
                    })),
            );
        }
        row
    }

    fn render_connect_mode_chips(
        &self,
        edit: &EditDraft,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let modes = [
            ("edit-mode-direct", ConnectMode::Direct, "直连"),
            ("edit-mode-reverse", ConnectMode::Reverse, "反向外呼"),
        ];
        let mut row = h_flex().items_center().gap_2().child(
            div()
                .w_16()
                .flex_shrink_0()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("连接方式"),
        );
        for (id, mode, label) in modes {
            let selected = edit.connect_mode == mode;
            row = row.child(
                Button::new(id)
                    .label(label)
                    .small()
                    .when(selected, |b| b.primary())
                    .when(!selected, |b| b.ghost())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(edit) = this.edit.as_mut() {
                            edit.connect_mode = mode;
                        }
                        cx.notify();
                    })),
            );
        }
        row
    }

    // ── actions ──────────────────────────────────────────────────────

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let config = entry.config.clone();
        let device_name = entry.name();

        let hostname = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("留空则自动命名");
            state.set_value(config.hostname.clone().unwrap_or_default(), window, cx);
            state
        });
        let ips = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("IP 地址，多个用逗号分隔");
            state.set_value(
                config
                    .fix_ips
                    .iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                window,
                cx,
            );
            state
        });
        let port = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("端口");
            state.set_value(config.port.to_string(), window, cx);
            state
        });

        self.edit = Some(EditDraft {
            handle: entry.handle,
            device_name,
            hostname,
            ips,
            port,
            pos: config.pos,
            connect_mode: config.connect_mode,
        });
        cx.notify();
    }

    /// Validate the draft and push the changed fields to the daemon as
    /// individual update requests (the daemon resets the DTLS session
    /// for the fields that need it).
    fn save_edit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.as_ref() else {
            return;
        };
        let handle = edit.handle;
        let hostname_text = edit.hostname.read(cx).value().trim().to_string();
        let ips_text = edit.ips.read(cx).value().trim().to_string();
        let port_text = edit.port.read(cx).value().trim().to_string();

        let port = match port_text.parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => {
                self.notify_now(Notification::error("端口无效：请输入 1–65535"), cx);
                return;
            }
        };
        let parsed: Result<Vec<IpAddr>, _> = if ips_text.is_empty() {
            Ok(Vec::new())
        } else {
            ips_text
                .split([',', ';', ' '])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| part.parse::<IpAddr>())
                .collect()
        };
        let Ok(ips) = parsed else {
            self.notify_now(
                Notification::error("地址无效：请输入合法 IP，多个用逗号分隔"),
                cx,
            );
            return;
        };

        let Some(entry) = self.entry(handle) else {
            self.edit = None;
            return;
        };
        let old = &entry.config;
        let hostname = (!hostname_text.is_empty()).then_some(hostname_text);
        let ips_changed = {
            let mut current = old.fix_ips.clone();
            current.sort();
            let mut next = ips.clone();
            next.sort();
            current != next
        };

        let mut changed = false;
        if old.hostname != hostname {
            self.request(FrontendRequest::UpdateHostname(handle, hostname));
            changed = true;
        }
        if old.port != port {
            self.request(FrontendRequest::UpdatePort(handle, port));
            changed = true;
        }
        if ips_changed {
            self.request(FrontendRequest::UpdateFixIps(handle, ips));
            changed = true;
        }
        if old.pos != edit.pos {
            self.request(FrontendRequest::UpdatePosition(handle, edit.pos));
            changed = true;
        }
        if old.connect_mode != edit.connect_mode {
            self.request(FrontendRequest::UpdateConnectMode(
                handle,
                edit.connect_mode,
            ));
            changed = true;
        }
        self.edit = None;
        if changed {
            self.notify_now(Notification::success("配置已更新"), cx);
        }
        cx.notify();
    }

    /// Remove uses a confirm dialog (design D15: destructive actions
    /// get a second step).
    fn confirm_remove(&self, handle: ClientHandle, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .entry(handle)
            .map(|entry| entry.name())
            .unwrap_or_default();
        let writer = self.writer.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let name = name.clone();
            let writer = writer.clone();
            alert
                .title(format!("移除设备「{name}」？"))
                .description("将删除该设备的全部配置，且无法撤销。")
                .confirm()
                .ok_text("移除")
                .cancel_text("取消")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, _| {
                    if let Some(writer) = &writer {
                        writer.send(FrontendRequest::Delete(handle));
                    }
                    true
                })
        });
    }

    /// Revoking trust removes the daemon's authorized-fingerprint
    /// entry (design D15: guarded by a second confirmation).
    fn confirm_revoke(&self, fingerprint: String, window: &mut Window, cx: &mut Context<Self>) {
        let writer = self.writer.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let fingerprint = fingerprint.clone();
            let writer = writer.clone();
            alert
                .title("撤销信任？")
                .description("该设备将无法再向本机发送键鼠事件，直至重新授权。")
                .confirm()
                .ok_text("撤销信任")
                .cancel_text("取消")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, _| {
                    if let Some(writer) = &writer {
                        writer.send(FrontendRequest::RemoveAuthorizedKey(fingerprint.clone()));
                    }
                    true
                })
        });
    }

    /// Re-dial an offline device from the detail panel — the same
    /// direct-connect the search box issues.
    fn connect_selected(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let Some(ip) = entry.dial_ip() else {
            self.notify_now(
                Notification::warning("该设备没有可拨号的地址，请先在配置中填写 IP"),
                cx,
            );
            return;
        };
        let port = entry.config.port;
        let handle = entry.handle;
        self.request(FrontendRequest::ConnectTo(ip, port));
        self.pending_connect = Some(ip);
        self.retrying = Some((ip, port));
        self.retry_attempts = 1;
        self.spawn_connect_retries(cx);
        self.selected = Some(handle);
        self.notify_now(Notification::info(format!("正在连接 {ip}:{port}…")), cx);
        cx.notify();
    }
}

fn peer_version(state: &mousehop_ipc::ClientState) -> SharedString {
    state
        .peer_commit
        .map(|commit| {
            commit
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
                .into()
        })
        .unwrap_or_else(|| "未知".into())
}

fn latency_label(state: &mousehop_ipc::ClientState) -> SharedString {
    state
        .latencies
        .values()
        .flatten()
        .min()
        .map(|us| format!("{:.1} ms", f64::from(*us) / 1000.0).into())
        .unwrap_or_else(|| "—".into())
}
