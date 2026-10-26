//! Device detail panel: summary view, edit form and the actions that
//! talk to the daemon (connect / remove / config updates).

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AppContext, ClickEvent, Context, Div, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window,
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
    div, img, px,
};
use mousehop_ipc::{
    ClientHandle, ConnectMode, CrossingModifier, FrontendRequest, IncomingPeerConfig, Position,
};
use std::net::IpAddr;

use super::{Console, DeviceEntry, EditDraft, position_label};

impl Console {
    pub(super) fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("device-detail")
            .test_support()
            .flex_1()
            .min_h_0()
            .flex_col()
            .items_stretch()
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
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(muted)
            .child(Icon::new(IconName::MousePointer2).large().text_color(muted))
            .child(div().text_sm().child("选择左侧设备查看详情"))
            .child(
                div()
                    .max_w(px(300.))
                    .text_center()
                    .text_xs()
                    .child("对方不在这个网络？在搜索框输入对方 IP 回车即可直接发起连接。"),
            )
    }

    fn render_device_detail(
        &self,
        entry: &DeviceEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                        v_flex().min_w_0().child(div().text_lg().child(name)).child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(format!("{addr} · {}", entry.connect_mode_label())),
                        ),
                    )
                    .child(
                        h_flex()
                            .ml_auto()
                            .gap_1p5()
                            .child({
                                let (label, tone) = self.status_chip(entry, cx);
                                super::chip(label, tone.opacity(0.15), tone)
                            })
                            .child(super::chip(
                                entry.connect_mode_label(),
                                cx.theme().secondary,
                                cx.theme().secondary_foreground,
                            ))
                            .child(self.render_cloud_badge(cx)),
                    ),
            )
            .child(self.render_kv_section("概览", cx))
            .child(self.render_overview_grid(entry, cx))
            .child(self.render_kv_section("安全", cx))
            .child(self.render_trust_section(entry, cx))
            .child(self.render_share_section(entry, cx))
            .child(self.render_video_section(entry, cx))
            .child(self.render_device_actions(entry, cx))
    }

    /// The cloud remote-view reservation (design v2.2's dashed 云
    /// badge): purely a placeholder until the cloud milestone lands.
    fn render_cloud_badge(&self, cx: &mut Context<Self>) -> Div {
        div()
            .h_5()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .rounded_full()
            .border_1()
            .border_dashed()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Cloud).xsmall())
            .child("云")
    }

    /// Overview strip (design v2.2's `.mh-kv`): a bordered card cut
    /// into four labeled cells by 1px dividers.
    fn render_overview_grid(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let cells = [
            ("共享位置", entry.position_label().into()),
            ("往返延迟", latency_label(&entry.state)),
            ("对端版本", peer_version(&entry.state)),
            ("来源", SharedString::from("局域网")),
        ];
        div()
            .mt_1()
            .flex()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .children(
                cells
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, value))| {
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .px_3()
                            .py_2()
                            .when(index > 0, |cell| {
                                cell.border_l_1().border_color(cx.theme().border)
                            })
                            .child(div().text_xs().text_color(muted).child(label))
                            .child(div().text_sm().truncate().child(value))
                    }),
            )
    }

    /// Design v2.2's 共享选项 card trio. The daemon's semantics (D22①)
    /// split the gates by direction: the send-side clipboard gate is
    /// per-client, while natural scroll and sensitivity apply to what
    /// this machine *receives* from the peer (keyed on its certificate
    /// fingerprint). Shown for a trusted online device, as in the mock.
    fn render_share_section(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        let trusted = self
            .entry_trust(entry)
            .map(|(fp, peer)| (fp.to_string(), peer.clone()));
        let shareable = entry.online() && trusted.is_some();
        let handle = entry.handle;
        let name = entry.name();

        div().when(shareable, |this| {
            let Some((fingerprint, peer)) = trusted else {
                return this;
            };
            let writer = self.writer.clone();
            let entity = cx.entity();

            let clip_switch = Switch::new(SharedString::from(format!("clip-send-{handle}")))
                .checked(entry.config.clipboard_send)
                .on_click(move |checked, _, cx| {
                    if let Some(writer) = &writer {
                        writer.send(FrontendRequest::SetClientClipboardSend(handle, *checked));
                    }
                    entity.update(cx, |this, _| {
                        this.journal(
                            super::logbar::LogLevel::Info,
                            format!(
                                "剪贴板发送「{name}」→ {}",
                                if *checked { "开" } else { "关" }
                            ),
                        );
                    });
                })
                .small();

            let natural_switch = {
                let writer = self.writer.clone();
                let fp = fingerprint.clone();
                Switch::new(SharedString::from(format!("detail-natural-{handle}")))
                    .checked(peer.natural_scroll)
                    .on_click(move |checked, _, _| {
                        if let Some(writer) = &writer {
                            writer.send(FrontendRequest::SetIncomingPeerNaturalScroll(
                                fp.clone(),
                                *checked,
                            ));
                        }
                    })
                    .small()
            };

            this.child(
                v_flex()
                    .mt_1()
                    .child(self.render_kv_section("共享选项", cx))
                    .child(
                        h_flex()
                            .mt_1()
                            .gap_2()
                            .child(self.render_option_card(
                                IconName::Clipboard,
                                "剪贴板共享",
                                "本机剪贴板同步给该设备（对方也需开启接收）",
                                clip_switch.into_any_element(),
                                cx,
                            ))
                            .child(self.render_option_card(
                                IconName::Mouse,
                                "自然滚动",
                                "该设备发来的滚轮方向反转",
                                natural_switch.into_any_element(),
                                cx,
                            ))
                            .child(
                                self.render_option_card(
                                    IconName::Gauge,
                                    "指针灵敏度",
                                    "该设备指针移动速度倍率",
                                    self.render_sensitivity_stepper(&fingerprint, &peer, cx)
                                        .into_any_element(),
                                    cx,
                                ),
                            ),
                    ),
            )
        })
    }

    /// One 共享选项 card (design's `.mh-optcard`): icon + title, a
    /// hint line, then the control row pinned to the bottom.
    fn render_option_card(
        &self,
        icon: IconName,
        title: &str,
        hint: &str,
        control: AnyElement,
        cx: &mut Context<Self>,
    ) -> Div {
        let muted = cx.theme().muted_foreground;
        let title = title.to_string();
        let hint = hint.to_string();
        v_flex()
            .flex_1()
            .min_w(px(148.))
            .gap_1()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .text_sm()
                    .child(Icon::new(icon).small().text_color(muted))
                    .child(title),
            )
            .child(div().text_xs().text_color(muted).child(hint))
            .child(h_flex().mt_1().items_center().justify_end().child(control))
    }

    /// Screen-view section (M3a increment ④): the picture toggle for the
    /// device whose video offer is on file plus, while viewing, the live
    /// frame surface fed by the daemon's local video feed socket. The
    /// daemon owns the state — this switch mirrors `VideoState.viewing`
    /// exactly like the clipboard row mirrors its config.
    fn render_video_section(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let handle = entry.handle;
        let name = entry.name();
        let is_offer = self.video.state.handle == Some(handle);
        let viewing = self.video.state.viewing && is_offer;

        let (hint, control) = if is_offer {
            let writer = self.writer.clone();
            let entity = cx.entity();
            (
                if viewing {
                    "正在查看该设备的共享画面"
                } else {
                    "拉起该设备共享的画面（对方停止共享或离线即断开）"
                },
                Switch::new(SharedString::from(format!("video-view-{handle}")))
                    .checked(viewing)
                    .on_click(move |checked, _, cx| {
                        if let Some(writer) = &writer {
                            writer.send(FrontendRequest::SetVideoView(handle, *checked));
                        }
                        entity.update(cx, |this, _| {
                            this.journal(
                                super::logbar::LogLevel::Info,
                                format!(
                                    "远程查看「{name}」→ {}",
                                    if *checked { "开" } else { "关" }
                                ),
                            );
                        });
                    })
                    .small(),
            )
        } else {
            (
                "该设备未提供画面共享（需对方开启 [video]）",
                Switch::new(SharedString::from(format!("video-view-{handle}")))
                    .checked(false)
                    .disabled(true)
                    .small(),
            )
        };

        let mut section = v_flex()
            .mt_1()
            .child(self.render_kv_section("画面", cx))
            .child(super::settings::option_row(
                "远程查看",
                hint,
                control,
                muted,
            ));
        if viewing {
            section = section.child(self.render_video_surface(cx));
        }
        div().child(section)
    }

    /// The live picture surface: newest decoded frame from the local
    /// feed, letterboxed into a fixed-height panel (gpui's `img` with
    /// Contain fit scales it; the render-period naturally drops frames
    /// when the UI is slower than the stream).
    fn render_video_surface(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let state = &self.video.state;
        let stats = format!(
            "{}×{} · {} 帧",
            state.width, state.height, self.video.frames
        );
        let surface = h_flex()
            .h_64()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .justify_center()
            .bg(gpui_kit::black())
            .child(match self.video.latest_image() {
                // `Img` injects the frame's aspect ratio into its layout
                // style, and that ratio overrides a width+height pair
                // (observed in the increment ④ smoke: a `w_full` + `h_64`
                // image rendered at natural height and got clipped). Fix
                // the height alone — the width derives from the ratio —
                // and let this row center it over the black backdrop.
                Some(image) => img(image).h_64().flex_shrink_0().into_any_element(),
                None => h_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(muted)
                    .child("等待画面…")
                    .into_any_element(),
            });
        v_flex()
            .gap_1()
            .child(surface)
            .child(div().text_xs().text_color(muted).child(stats))
    }

    /// The peer's authorized record, when its fingerprint is known and
    /// trusted. Backs the share cards and the trust row.
    fn entry_trust<'a>(
        &'a self,
        entry: &'a DeviceEntry,
    ) -> Option<(&'a str, &'a IncomingPeerConfig)> {
        entry
            .state
            .peer_fingerprint
            .as_deref()
            .and_then(|fp| self.authorized.get(fp).map(|peer| (fp, peer)))
    }

    /// Design v2.2's 安全 section: while the device waits at the local
    /// authorization gate it gets the inline auth card (accept/decline
    /// with the fingerprint to compare); otherwise the trust row card
    /// (trusted + guarded revoke, or the not-yet-trusted hint).
    fn render_trust_section(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> Div {
        div().child(match self.pending_entry_fingerprint(entry) {
            Some(fingerprint) => self
                .render_pending_auth_card(entry, fingerprint, cx)
                .into_any_element(),
            None => self.render_trust_row(entry, cx).into_any_element(),
        })
    }

    /// The fingerprint waiting at the gate for THIS device, if any.
    fn pending_entry_fingerprint(&self, entry: &DeviceEntry) -> Option<String> {
        entry
            .state
            .peer_fingerprint
            .as_deref()
            .filter(|fp| self.awaiting_auth.contains_key(*fp))
            .map(str::to_string)
    }

    fn render_pending_auth_card(
        &self,
        entry: &DeviceEntry,
        fingerprint: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        let name = entry.name();
        let handle = entry.handle;
        let writer = self.writer.clone();
        let entity = cx.entity();
        let decline_fp = fingerprint.clone();
        let decline_name = name.clone();
        let accept_name = name.clone();

        v_flex()
            .mt_1()
            .gap_0()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().warning.opacity(0.5))
            .overflow_hidden()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(cx.theme().warning.opacity(0.12))
                    .text_color(cx.theme().warning)
                    .child(Icon::new(IconName::ShieldAlert).small())
                    .child(div().text_sm().child(format!("{name} 请求连接到本机"))),
            )
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("首次连接需要确认一次，确认后自动信任，不再重复询问。请核对对方屏幕上显示的指纹与本机一致："),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(cx.theme().secondary)
                            .text_xs()
                            .font_family(mono)
                            .child(fingerprint.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(SharedString::from(format!("auth-accept-{handle}")))
                                    .icon(Icon::new(IconName::ShieldCheck))
                                    .label("接受并信任")
                                    .primary()
                                    .small()
                                    .on_click(move |_, _, _| {
                                        if let Some(writer) = &writer {
                                            writer.send(FrontendRequest::AuthorizeKey(
                                                accept_name.to_string(),
                                                fingerprint.clone(),
                                            ));
                                        }
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!("auth-decline-{handle}")))
                                    .label("忽略")
                                    .ghost()
                                    .danger()
                                    .small()
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, _, cx| {
                                            this.awaiting_auth.remove(&decline_fp);
                                            entity.update(cx, |this, _| {
                                                this.journal(
                                                    super::logbar::LogLevel::Info,
                                                    format!("已忽略「{decline_name}」的连接请求（未通知服务，对方重试超时后自动停止）"),
                                                );
                                            });
                                        },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("若你启用了访问密码（config.toml 的 access_password），对方在自己那端输入密码即可直接通过，无需逐台确认。"),
                    ),
            )
    }

    fn render_trust_row(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let trusted = self.entry_trust(entry);
        let handle = entry.handle;

        h_flex()
            .mt_1()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                div().child(
                    Icon::new(if trusted.is_some() {
                        IconName::ShieldCheck
                    } else {
                        IconName::ShieldAlert
                    })
                    .small()
                    .text_color(if trusted.is_some() {
                        cx.theme().success
                    } else {
                        cx.theme().warning
                    }),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().child(if trusted.is_some() {
                        "已信任"
                    } else {
                        "未信任"
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(match trusted {
                                Some((fp, peer)) => match &peer.description {
                                    desc if desc.is_empty() => SharedString::from(format!(
                                        "首次连接时已核对指纹 · {}",
                                        super::short_fingerprint(fp)
                                    )),
                                    desc => SharedString::from(format!(
                                        "首次连接时已核对指纹 · {} · 备注 {desc}",
                                        super::short_fingerprint(fp)
                                    )),
                                },
                                None => "下次连接时需要确认或输入访问密码".into(),
                            }),
                    ),
            )
            .when(trusted.is_some(), |row| {
                row.child(
                    Button::new(SharedString::from(format!("revoke-trust-{handle}")))
                        .label("撤销信任")
                        .ghost()
                        .danger()
                        .small()
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            let Some(fp) = this.entry_trust_by_handle(handle) else {
                                return;
                            };
                            this.confirm_revoke(fp, window, cx);
                        })),
                )
            })
    }

    fn entry_trust_by_handle(&self, handle: ClientHandle) -> Option<String> {
        self.entry(handle)
            .and_then(|entry| self.entry_trust(entry).map(|(fp, _)| fp.to_string()))
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
                .icon(Icon::new(IconName::Cloud))
                .label("远程查看")
                .outline()
                .small()
                .disabled(true)
                .tooltip("云端远程控制即将推出：经云中继查看对端画面。局域网画面查看见上方「画面」开关。"),
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
            .gap_2()
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
            .child(self.render_edit_card(
                "设备名称",
                "留空则自动命名",
                Input::new(&edit.hostname).small().w(px(320.)),
                cx,
            ))
            .child(self.render_edit_card(
                "地址",
                "IP 地址，多个用逗号分隔",
                Input::new(&edit.ips).small().w(px(320.)),
                cx,
            ))
            .child(self.render_edit_card(
                "端口",
                "UDP 端口",
                Input::new(&edit.port).small().w(px(120.)),
                cx,
            ))
            .child(self.render_edit_card(
                "屏幕位置",
                "共享屏在本机的方位",
                self.render_position_chips(edit, cx),
                cx,
            ))
            .child(self.render_edit_card(
                "连接方式",
                "对端在 NAT 内不可直连时选反向外呼",
                self.render_connect_mode_chips(edit, cx),
                cx,
            ))
            .child(self.render_kv_section("输入映射", cx))
            .child(self.render_send_option_rows(edit, cx))
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

    /// 编辑表单的行卡（与共享选项三卡同语言）：左侧标签+说明、右侧
    /// 控件，让每行右缘对齐同一张边框，避免裸 label 行的参差感。
    /// The settings panes reuse the same card for their own rows.
    pub(super) fn render_edit_card(
        &self,
        label: &str,
        hint: &str,
        control: impl IntoElement,
        cx: &Context<Self>,
    ) -> Div {
        let muted = cx.theme().muted_foreground;
        // Vertical padding matches the device-detail cards (公子: item
        // 高上下与设备项一致 — measured 57px there vs 67px here at p_3).
        h_flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().child(label.to_string()))
                    .child(div().text_xs().text_color(muted).child(hint.to_string())),
            )
            .child(control)
    }

    fn render_position_chips(&self, edit: &EditDraft, cx: &mut Context<Self>) -> impl IntoElement {
        let positions = [
            ("edit-pos-left", Position::Left),
            ("edit-pos-right", Position::Right),
            ("edit-pos-top", Position::Top),
            ("edit-pos-bottom", Position::Bottom),
            ("edit-pos-topleft", Position::TopLeft),
            ("edit-pos-topright", Position::TopRight),
            ("edit-pos-bottomleft", Position::BottomLeft),
            ("edit-pos-bottomright", Position::BottomRight),
        ];
        let mut row = h_flex().flex_wrap().items_center().gap_2();
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
        let mut row = h_flex().items_center().gap_2();
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

    /// 输入映射 rows: send-side key-mapping options (the remaining D8-era
    /// fields). Draft state like the chips above — applying on 保存 goes
    /// through the daemon's live `SetClient*` handlers, so no reconnect
    /// is needed for these.
    fn render_send_option_rows(
        &self,
        edit: &EditDraft,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();

        let cmd_switch = {
            let entity = entity.clone();
            Switch::new("edit-command-as-ctrl")
                .checked(edit.command_as_ctrl)
                .on_click(move |checked, _, cx| {
                    entity.update(cx, |this, _| {
                        if let Some(edit) = this.edit.as_mut() {
                            edit.command_as_ctrl = *checked;
                        }
                    });
                })
                .small()
        };
        let require_switch = {
            let entity = entity.clone();
            Switch::new("edit-require-crossing")
                .checked(edit.require_crossing_modifier)
                .on_click(move |checked, _, cx| {
                    entity.update(cx, |this, _| {
                        if let Some(edit) = this.edit.as_mut() {
                            edit.require_crossing_modifier = *checked;
                        }
                    });
                })
                .small()
        };

        // The crossing-modifier chips belong to the same option, so they
        // live inside that card instead of a separate bare row.
        let mut crossing_card = self.render_edit_card(
            "跨越需按修饰键",
            "开启后指针须按住所选修饰键才能进入该设备（默认靠边即切）",
            require_switch,
            cx,
        );
        if edit.require_crossing_modifier {
            let modifiers = [
                ("edit-crossing-ctrl", CrossingModifier::Control, "Ctrl"),
                ("edit-crossing-alt", CrossingModifier::Alt, "Alt"),
                ("edit-crossing-shift", CrossingModifier::Shift, "Shift"),
                ("edit-crossing-super", CrossingModifier::Super, "Super"),
            ];
            let mut row = h_flex().items_center().gap_2().mt_2();
            for (id, modifier, label) in modifiers {
                let selected = edit.crossing_modifier == modifier;
                row = row.child(
                    Button::new(id)
                        .label(label)
                        .small()
                        .when(selected, |b| b.primary())
                        .when(!selected, |b| b.ghost())
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some(edit) = this.edit.as_mut() {
                                edit.crossing_modifier = modifier;
                            }
                            cx.notify();
                        })),
                );
            }
            crossing_card = crossing_card.child(row);
        }

        v_flex()
            .gap_2()
            .child(self.render_edit_card(
                "Command→Ctrl",
                "仅 macOS 发送端：对端收到的 Command 键改按 Control 注入",
                cmd_switch,
                cx,
            ))
            .child(crossing_card)
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
            command_as_ctrl: config.command_as_ctrl,
            require_crossing_modifier: config.require_crossing_modifier,
            crossing_modifier: config.crossing_modifier,
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
        if old.command_as_ctrl != edit.command_as_ctrl {
            self.request(FrontendRequest::SetClientCommandAsCtrl(
                handle,
                edit.command_as_ctrl,
            ));
            changed = true;
        }
        if old.require_crossing_modifier != edit.require_crossing_modifier {
            self.request(FrontendRequest::SetClientRequireCrossingModifier(
                handle,
                edit.require_crossing_modifier,
            ));
            changed = true;
        }
        if old.crossing_modifier != edit.crossing_modifier {
            self.request(FrontendRequest::SetClientCrossingModifier(
                handle,
                edit.crossing_modifier,
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

pub(super) fn latency_label(state: &mousehop_ipc::ClientState) -> SharedString {
    state
        .latencies
        .values()
        .flatten()
        .min()
        .map(|us| format!("{:.1} ms", f64::from(*us) / 1000.0).into())
        .unwrap_or_else(|| "—".into())
}
