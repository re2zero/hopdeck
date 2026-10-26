//! Settings overlay (design v2.2's `.mh-settings`): a centered sheet
//! over the main area with a five-entry nav — 常规 / 连接 / 安全 / 云端
//! / 高级. The daemon-backed controls live where the design puts them:
//! identity, the listen port and the screen-sharing master switch
//! (P1.2) under 常规, the per-trusted-peer receive-side options (D22
//! semantics) plus trust management under 安全, logging and the
//! emulation backend under 高级. 连接 and 云端 are guidance/placeholder
//! pages: their mock controls have no IPC behind them yet, and
//! rendering a dead control would lie.

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, ClickEvent, Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window,
    assets::IconName,
    component::{
        ActiveTheme, Disableable, Icon, Selectable, Sizable, WindowExt,
        button::{Button, ButtonVariants},
        h_flex,
        input::Input,
        notification::Notification,
        switch::Switch,
        v_flex,
    },
    div, px,
};
use mousehop_ipc::{FrontendRequest, IncomingPeerConfig};

use super::Console;
use super::logbar::LogLevel;

/// The settings nav selection (design v2.2's five tabs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsTab {
    General,
    Connect,
    Security,
    Cloud,
    Advanced,
}

impl SettingsTab {
    fn entry(self) -> (&'static str, IconName, &'static str) {
        match self {
            SettingsTab::General => ("general", IconName::Settings, "常规"),
            SettingsTab::Connect => ("connect", IconName::Globe, "连接"),
            SettingsTab::Security => ("security", IconName::Shield, "安全"),
            SettingsTab::Cloud => ("cloud", IconName::Cloud, "云端"),
            SettingsTab::Advanced => ("advanced", IconName::Bug, "高级"),
        }
    }

    const ALL: [SettingsTab; 5] = [
        SettingsTab::General,
        SettingsTab::Connect,
        SettingsTab::Security,
        SettingsTab::Cloud,
        SettingsTab::Advanced,
    ];
}

impl Console {
    /// Settings as a main-window page (公子: 不用浮层——hover 穿透背后
    /// 的设备行). The split mirrors the main view: a left column as wide
    /// as the device list holds the nav, the content area starts at the
    /// left edge just like the device detail (公子: 设置项靠左，划分
    /// 布局与设备列表/设备信息一致). Header keeps a back arrow on the
    /// left and a close icon on the right.
    pub(super) fn render_settings_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(px(44.))
                    .flex_shrink_0()
                    .items_center()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("settings-back")
                            .icon(Icon::new(IconName::ArrowLeft))
                            .ghost()
                            .tooltip("返回")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.settings_open = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .justify_center()
                            .child(div().text_sm().child("设置")),
                    )
                    .child(
                        Button::new("settings-close")
                            .icon(Icon::new(IconName::X))
                            .ghost()
                            .tooltip("关闭")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.settings_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_settings_nav(cx))
                    .child(self.render_settings_pane(cx)),
            )
    }

    /// The left nav column, as wide as the device list (design's
    /// `.mh-nav` on the main view's grid).
    fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_72()
            .flex_shrink_0()
            .min_h_0()
            .p_2()
            .gap_0p5()
            .border_r_1()
            .border_color(cx.theme().border)
            .children(SettingsTab::ALL.map(|tab| {
                let (id, icon, label) = tab.entry();
                let selected = self.settings_tab == tab;
                Button::new(SharedString::from(format!("settings-tab-{id}")))
                    .label(label)
                    .icon(Icon::new(icon).small())
                    .small()
                    .when(selected, |b| b.primary())
                    .when(!selected, |b| b.ghost())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.settings_tab = tab;
                        cx.notify();
                    }))
            }))
    }

    fn render_settings_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The settings column keeps a fixed width centered in the pane
        // (公子: 内容固定宽度居中，全屏不过宽). Two flex_1 spacers
        // flank the column. NOTE: the container must be an explicit
        // flex (h_flex) — a bare div() lays out as block and ignores
        // every flex property, which is why centering silently broke.
        h_flex()
            .id("settings-body")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .items_stretch()
            .overflow_hidden()
            .child(div().flex_1().min_w_0())
            .child(
                v_flex()
                    .id("settings-scroll")
                    .h_full()
                    .w(px(640.))
                    .flex_shrink_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(match self.settings_tab {
                        SettingsTab::General => self.render_general_pane(cx).into_any_element(),
                        SettingsTab::Connect => self.render_connect_pane(cx).into_any_element(),
                        SettingsTab::Security => self.render_security_pane(cx).into_any_element(),
                        SettingsTab::Cloud => self.render_cloud_pane(cx).into_any_element(),
                        SettingsTab::Advanced => self.render_advanced_pane(cx).into_any_element(),
                    }),
            )
            .child(div().flex_1().min_w_0())
    }

    /// 常规: this device's identity plus the daemon's listen port,
    /// both as cards matching the device-detail rows (公子: 设置项与
    /// 设备项相同布局，依然是卡片式).
    fn render_general_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fingerprint = self
            .self_fingerprint
            .as_deref()
            .map(super::short_self_fingerprint)
            .unwrap_or_else(|| "…".to_string());

        v_flex()
            .p_4()
            .gap_2()
            .child(self.render_kv_section("本机", cx))
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(self.render_kv("设备名", self.self_hostname.clone(), muted))
                    .child(self.render_kv("本机指纹", fingerprint.into(), muted))
                    .child(self.render_kv(
                        "UDP 端口",
                        SharedString::from(self.port.to_string()),
                        muted,
                    )),
            )
            .child(self.render_kv_section("服务", cx))
            .child(self.render_port_row(cx))
            .child(
                div().text_xs().text_color(muted).child(
                    "局域网发现（mDNS）由 config.toml 的 mdns_discovery 控制，改动重启生效。",
                ),
            )
            .child(self.render_kv_section("画面共享", cx))
            .child(self.render_video_share_card(cx))
    }

    /// 画面共享 master switch (P1.2): flips the daemon's `[video]`
    /// enabled live via `SetVideoEnabled`. The switch renders from the
    /// `VideoShare` echo rather than a local optimistic flip, so a
    /// dropped request simply leaves it unmoved — same contract as the
    /// per-peer switches on the 安全 tab.
    fn render_video_share_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let writer = self.writer.clone();
        v_flex()
            .id("settings-video-share-card")
            .test_support()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(option_row(
                "允许其他设备查看本机屏幕",
                "开启后，已信任的设备即可连接本机的画面通道；首次被查看时系统会弹出屏幕选择框（选择结果当前不跨重启保留）。",
                Switch::new("settings-video-share")
                    .checked(self.video_share_enabled)
                    .on_click(move |checked, _, _| {
                        if let Some(writer) = &writer {
                            writer.send(FrontendRequest::SetVideoEnabled(*checked));
                        }
                    })
                    .small(),
                muted,
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("画面经端到端 DTLS 加密传输；未在本机授权（信任）的设备无法连接画面通道。"),
            )
    }

    /// 监听端口 card: an input plus an apply button; empty submits are
    /// a no-op so the row doubles as a display of the current port.
    fn render_port_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let writer = self.writer.clone();
        let port = self.port;
        let controls = h_flex()
            .items_center()
            .gap_2()
            .child(Input::new(&self.port_input).small().w(px(140.)))
            .child(
                Button::new("apply-port")
                    .label("应用")
                    .small()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let text = this.port_input.read(cx).value().trim().to_string();
                        let Ok(new_port) = text.parse::<u16>() else {
                            if text.is_empty() {
                                return;
                            }
                            this.notify_now(Notification::error("端口无效：请输入 1–65535"), cx);
                            return;
                        };
                        if new_port == 0 || new_port == port {
                            return;
                        }
                        if let Some(writer) = &writer {
                            writer.send(FrontendRequest::ChangePort(new_port));
                        }
                        this.port_input
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        this.journal(LogLevel::Info, format!("监听端口申请 → {new_port}"));
                    })),
            );
        self.render_edit_card("监听端口", "改动后 UDP 通道在端口上重建", controls, cx)
    }

    /// 连接: guidance only — the three D8 connection modes and where
    /// to configure them. The mock's controls here have no daemon side.
    fn render_connect_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .p_4()
            .gap_1()
            .child(self.render_kv_section("连接方式", cx))
            .child(
                div()
                    .mt_1()
                    .text_sm()
                    .text_color(muted)
                    .child("连接方式按设备配置：在设备详情的「修改配置」中选择，反向外呼的拨号方向与控制方向解耦。"),
            )
            .child(self.render_mode_note("直连", "双向可拨；同一局域网或路由可达即用。", cx))
            .child(self.render_mode_note(
                "反向外呼",
                "网络受限侧主动外呼建立会话，覆盖单向网络（被控端在 NAT 内）。",
                cx,
            ))
            .child(self.render_mode_note(
                "经转发端口",
                "目标填 DNAT / ssh -L 后的地址即可；UDP 需 UDP 隧道转发。",
                cx,
            ))
            .child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(muted)
                    .child("搜索框输入对方 IP 回车，可对不在列表中的设备直接发起连接（覆盖跨网段不可发现场景）。"),
            )
    }

    fn render_mode_note(
        &self,
        title: &str,
        hint: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .mt_1()
            .gap_0p5()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(div().text_sm().child(title.to_string()))
            .child(div().text_xs().text_color(muted).child(hint.to_string()))
    }

    /// 安全: the connection-auth explainer, then the trusted-device
    /// cards (D22's per-fingerprint receive-side options moved into the
    /// design's 安全 tab) with per-card and bulk revoke.
    fn render_security_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex()
            .p_4()
            .gap_1()
            .child(self.render_kv_section("连接认证", cx))
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("指纹仅在首次确认时核对一次，之后自动信任；随时可在设备详情里撤销信任。访问密码在 config.toml 的 access_password 配置，凭密码连接不落入持久授权。"),
            );

        if self.authorized.is_empty() {
            return section.child(
                div()
                    .mt_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("暂无已信任设备。设备首次连接并授权后，可在此调整其共享选项。"),
            );
        }

        let mut peers: Vec<_> = self.authorized.iter().collect();
        peers.sort_by_key(|(_, peer)| peer.description.clone());
        section = section
            .child(
                h_flex()
                    .mt_3()
                    .items_center()
                    .child(self.render_kv_section("已信任设备", cx))
                    .child(
                        h_flex().ml_auto().child(
                            Button::new("revoke-all")
                                .label("全部撤销")
                                .ghost()
                                .danger()
                                .small()
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    let fingerprints: Vec<String> =
                                        this.authorized.keys().cloned().collect();
                                    let count = fingerprints.len();
                                    let writer = this.writer.clone();
                                    window.open_alert_dialog(cx, move |alert, _, _| {
                                        let fingerprints = fingerprints.clone();
                                        let writer = writer.clone();
                                        alert
                                            .title("撤销全部信任？")
                                            .description(format!(
                                                "所有 {count} 台已信任设备将被移出信任列表，下次连接都需要重新确认。"
                                            ))
                                            .confirm()
                                            .ok_text("全部撤销")
                                            .cancel_text("取消")
                                            .on_ok(move |_, _, _| {
                                                for fp in &fingerprints {
                                                    if let Some(writer) = &writer {
                                                        writer.send(
                                                            FrontendRequest::RemoveAuthorizedKey(
                                                                fp.clone(),
                                                            ),
                                                        );
                                                    }
                                                }
                                                true
                                            })
                                    });
                                })),
                        ),
                    ),
            )
            .children(
                peers
                    .into_iter()
                    .map(|(fp, peer)| self.render_peer_card(fp, peer, cx)),
            );
        section
    }

    /// 云端: the reserved cloud-remote-control page — every control is
    /// a disabled placeholder until that milestone lands.
    fn render_cloud_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        v_flex()
            .p_4()
            .gap_1()
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .p_3()
                    .rounded_lg()
                    .bg(cx.theme().accent.opacity(0.12))
                    .child(
                        Icon::new(IconName::Cloud)
                            .small()
                            .text_color(cx.theme().accent),
                    )
                    .child(
                        v_flex()
                            .child(div().text_sm().child("云端远程控制 · 即将推出"))
                            .child(
                                div().text_xs().text_color(muted).child(
                                    "跨网段中发现与中继转发，出门也能连回家里与公司的机器。",
                                ),
                            ),
                    ),
            )
            .child(self.render_kv_section("账户", cx))
            .child(
                self.render_edit_card(
                    "登录后可管理云端设备",
                    "使用 Mousehop 云账户登录",
                    Button::new("cloud-login")
                        .label("登录")
                        .small()
                        .disabled(true),
                    cx,
                ),
            )
            .child(self.render_kv_section("我的设备码", cx))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("他人可凭设备码向你发起远程协助。"),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .font_family(mono)
                    .text_color(muted)
                    .child("MH-XXXX-XXXX"),
            )
            .child(
                div().mt_2().text_xs().text_color(muted).child(
                    "端到端加密沿用现有 DTLS 指纹互信：云中继只转发密文，无法解密输入与画面。",
                ),
            )
    }

    /// 高级: logging (design's 日志 section) and the backends section
    /// that used to own this overlay.
    fn render_advanced_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .p_4()
            .gap_2()
            .child(self.render_kv_section("日志", cx))
            .child(self.render_log_settings(cx))
            .child(self.render_kv_section("后端", cx))
            .child(self.render_backend_card(cx))
            .child(self.render_usbip_card(cx))
            .child(
                div().text_xs().text_color(muted).child(
                    "输入捕获后端（portal / layer-shell / X11）由 config.toml 与运行环境探测决定，当前会话启动时生效。",
                ),
            )
    }

    /// Input emulation backend (design 常规页「后端」区): the live
    /// selector as a card matching the device-detail rows. Clicks send
    /// `SetEmulationBackend`; the daemon echoes the effective value
    /// through `EmulationBackend`, which updates the chips and journals
    /// the switch. USB/IP keeps its guidance card below.
    fn render_backend_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.emulation_backend.as_deref();
        let options = [
            ("backend-auto", None, "自动"),
            ("backend-x11", Some("x11"), "X11"),
            ("backend-usbip", Some("usbip"), "USB/IP"),
        ];

        let mut chips = h_flex().items_center().gap_2();
        for (id, value, label) in options {
            let selected = current == value;
            let request = value.map(str::to_string);
            chips = chips.child(
                Button::new(id)
                    .label(label)
                    .small()
                    .when(selected, |b| b.primary())
                    .when(!selected, |b| b.ghost())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                        this.request(FrontendRequest::SetEmulationBackend(request.clone()));
                    })),
            );
        }
        self.render_edit_card(
            "输入仿真",
            "注入本机键鼠用的后端；本机全局，改动即时生效并写盘（config.toml 的 emulation_backend；自动 = 依次探测 libei → layer-shell → X11）",
            chips,
            cx,
        )
        .id("settings-backend")
        .test_support()
    }

    /// USB/IP is the compositor-free fallback (M1): injection at the
    /// kernel's USB stack via a loopback USB/IP virtual HID device. It
    /// is deliberately never auto-selected, so the card carries the
    /// receiver-side preparation an operator must do first.
    fn render_usbip_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        let foreground = cx.theme().foreground;
        let command = move |text: &'static str| {
            div()
                .text_xs()
                .text_color(foreground)
                .font_family(mono.clone())
                .child(text.to_string())
        };

        v_flex()
            .id("backend-usbip-card")
            .test_support()
            .mt_1()
            .gap_1()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_sm().child("USB/IP 注入（Linux）"))
                    .child(super::chip(
                        "需要接收端配置",
                        cx.theme().warning.opacity(0.15),
                        cx.theme().warning,
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("内核级注入，不依赖合成器支持（libei portal 不可用时的兜底）。启用步骤："),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("① 在被控的 Linux 机器上准备一次接收端（root，加载 vhci_hcd 并挂载虚拟 HID 设备）："),
            )
            .child(command("sudo scripts/usbip-receiver-setup.sh attach"))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("② 在上方「输入仿真」选 USB/IP（或写 config.toml 的 emulation_backend；该后端从不自动选中）："),
            )
            .child(command("emulation_backend = \"usbip\""))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("③ Windows 接收端需 usbip-win 系第三方驱动，属实验性。"),
            )
    }

    fn render_peer_card(
        &self,
        fingerprint: &str,
        peer: &IncomingPeerConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let name = if peer.description.is_empty() {
            format!("设备 {}", super::short_fingerprint(fingerprint))
        } else {
            peer.description.clone()
        };
        let fp = fingerprint.to_string();
        let writer = self.writer.clone();

        v_flex()
            .mt_1()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .child(
                h_flex()
                    .items_center()
                    .child(div().text_sm().child(name))
                    .child(
                        Button::new(SharedString::from(format!("settings-revoke-{fingerprint}")))
                            .label("撤销信任")
                            .ghost()
                            .danger()
                            .small()
                            .ml_auto()
                            .on_click(move |_, window, cx| {
                                let fp = fp.clone();
                                let writer = writer.clone();
                                window.open_alert_dialog(cx, move |alert, _, _| {
                                    let fp = fp.clone();
                                    let writer = writer.clone();
                                    alert
                                        .title("撤销信任？")
                                        .description(
                                            "该设备将无法再向本机发送键鼠事件，直至重新授权。",
                                        )
                                        .confirm()
                                        .ok_text("撤销信任")
                                        .cancel_text("取消")
                                        .on_ok(move |_, _, _| {
                                            if let Some(writer) = &writer {
                                                writer.send(FrontendRequest::RemoveAuthorizedKey(
                                                    fp.clone(),
                                                ));
                                            }
                                            true
                                        })
                                });
                            }),
                    ),
            )
            .child(option_row(
                "剪贴板接收",
                "应用该设备发来的剪贴板文本",
                self.render_clipboard_switch(fingerprint, peer),
                muted,
            ))
            .child(option_row(
                "自然滚动",
                "反转该设备发来的滚轮方向",
                self.render_natural_scroll_switch(fingerprint, peer),
                muted,
            ))
            .child(option_row(
                "指针灵敏度",
                "该设备指针移动速度的倍率",
                self.render_sensitivity_stepper(fingerprint, peer, cx),
                muted,
            ))
    }
}

/// Label + hint on the left, the control on the right. Free-standing so
/// the log bar's settings section (`logbar.rs`) shares the row style.
pub(super) fn option_row(
    label: &str,
    hint: &str,
    control: impl IntoElement,
    muted: Hsla,
) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().child(label.to_string()))
                .child(div().text_xs().text_color(muted).child(hint.to_string())),
        )
        .child(control)
}

/// The debug-bar toggle. `on_click` carries no console access, so the
/// owning entity is captured and the flip goes through
/// `Console::set_debug_open`.
pub(super) fn log_switch(entity: Entity<Console>, checked: bool) -> Switch {
    Switch::new("settings-debug-log")
        .checked(checked)
        .on_click(move |checked, _, cx| {
            entity.update(cx, |this, cx| this.set_debug_open(*checked, cx));
        })
        .small()
}

/// One 日志级别 chip (info / debug / trace). `selected` drives the
/// active styling; the caller's listener flips the capture level.
pub(super) fn level_button(
    level: LogLevel,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(SharedString::from(format!("log-level-{}", level.name())))
        .label(level.name())
        .ghost()
        .small()
        .selected(selected)
        .toggled(selected)
        .on_click(on_click)
}

impl Console {
    fn render_clipboard_switch(
        &self,
        fingerprint: &str,
        peer: &IncomingPeerConfig,
    ) -> impl IntoElement {
        let writer = self.writer.clone();
        let fp = fingerprint.to_string();
        Switch::new(SharedString::from(format!("clip-{fingerprint}")))
            .checked(peer.clipboard_receive)
            .on_click(move |checked, _, _| {
                if let Some(writer) = &writer {
                    writer.send(FrontendRequest::SetIncomingPeerClipboardReceive(
                        fp.clone(),
                        *checked,
                    ));
                }
            })
            .small()
            .into_any_element()
    }

    fn render_natural_scroll_switch(
        &self,
        fingerprint: &str,
        peer: &IncomingPeerConfig,
    ) -> impl IntoElement {
        let writer = self.writer.clone();
        let fp = fingerprint.to_string();
        Switch::new(SharedString::from(format!("natural-{fingerprint}")))
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
    }

    /// − value + stepper, 0.1 per click, clamped to 0.1–3.0×. The next
    /// value is computed at render time so rapid clicks step past
    /// not-yet-arrived echoes instead of re-sending the same value.
    pub(super) fn render_sensitivity_stepper(
        &self,
        fingerprint: &str,
        peer: &IncomingPeerConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = peer.mouse_sensitivity;
        let minus_next = step_sensitivity(current, -1);
        let plus_next = step_sensitivity(current, 1);
        let minus_fp = fingerprint.to_string();
        let plus_fp = fingerprint.to_string();

        h_flex()
            .items_center()
            .gap_1()
            .child(
                Button::new(SharedString::from(format!("sens-minus-{fingerprint}")))
                    .icon(Icon::new(IconName::Minus))
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                        this.request(FrontendRequest::SetIncomingPeerSensitivity(
                            minus_fp.clone(),
                            minus_next,
                        ));
                    })),
            )
            .child(
                div()
                    .w(px(44.))
                    .text_center()
                    .text_sm()
                    .child(format!("{current:.1}×")),
            )
            .child(
                Button::new(SharedString::from(format!("sens-plus-{fingerprint}")))
                    .icon(Icon::new(IconName::Plus))
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                        this.request(FrontendRequest::SetIncomingPeerSensitivity(
                            plus_fp.clone(),
                            plus_next,
                        ));
                    })),
            )
    }
}

/// Step a sensitivity by `tenths` tenths, clamped to the daemon-meaning
/// range 0.1–3.0×. Computed in the integer tenths domain so repeated
/// ±0.1 steps don't accumulate binary-fraction drift.
fn step_sensitivity(value: f64, tenths: i32) -> f64 {
    let current_tenths = (value * 10.0).round() as i32;
    (current_tenths + tenths).clamp(1, 30) as f64 / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitivity_steps_in_tenths() {
        assert_eq!(step_sensitivity(1.0, 1), 1.1);
        assert_eq!(step_sensitivity(1.0, -1), 0.9);
        assert_eq!(step_sensitivity(1.0, 12), 2.2);
        assert_eq!(step_sensitivity(1.0, -5), 0.5);
    }

    #[test]
    fn sensitivity_clamps_to_daemon_range() {
        assert_eq!(step_sensitivity(3.0, 1), 3.0);
        assert_eq!(step_sensitivity(0.1, -1), 0.1);
        assert_eq!(step_sensitivity(0.0, 1), 0.1);
        assert_eq!(step_sensitivity(9.9, -1), 3.0);
    }

    #[test]
    fn sensitivity_rounds_odd_input_before_stepping() {
        // A value typed into config.toml as 1.15 snaps to the tenths
        // grid first: `round` is half-away-from-zero, so it rounds up
        // to 1.2's neighbor 1.3 after the step, not to 1.25 + 0.1.
        assert_eq!(step_sensitivity(1.15, 1), 1.3);
    }
}
