//! Settings overlay (design D15/D16): a centered sheet over the main
//! area with this device's info and the per-trusted-peer sharing
//! options — clipboard receive, natural scroll and pointer
//! sensitivity, all backed by the daemon's `SetIncomingPeer*` requests.
//! The daemon confirms every change via `AuthorizedUpdated`, which the
//! console already mirrors, so the controls are plain
//! request-send-without-local-state.

use gpui_kit::{
    App, ClickEvent, Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window,
    assets::IconName,
    component::{
        ActiveTheme, Icon, Selectable, Sizable,
        button::{Button, ButtonVariants},
        h_flex,
        switch::Switch,
        v_flex,
    },
    div, px,
};
use mousehop_ipc::{FrontendRequest, IncomingPeerConfig};

use super::Console;
use super::logbar::LogLevel;

impl Console {
    pub(super) fn render_settings_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().foreground.opacity(0.3))
            .child(self.render_settings_sheet(cx))
    }

    fn render_settings_sheet(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(560.))
            .max_h_full()
            .overflow_hidden()
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .shadow_lg()
            .child(
                h_flex()
                    .items_center()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().child("设置"))
                    .child(
                        Button::new("close-settings")
                            .icon(Icon::new(IconName::X))
                            .ghost()
                            .small()
                            .ml_auto()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.settings_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("settings-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .p_4()
                            .gap_1()
                            .child(self.render_self_section(cx))
                            .child(self.render_sharing_section(cx))
                            .child(self.render_log_settings(cx)),
                    ),
            )
    }

    /// This device's identity: hostname, key fingerprint and the UDP
    /// port the daemon listens on.
    fn render_self_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fingerprint = self
            .self_fingerprint
            .as_deref()
            .map(|fp| super::short_fingerprint(fp).to_string())
            .unwrap_or_else(|| "…".into());

        v_flex()
            .child(self.render_kv_section("本机", cx))
            .child(self.render_kv("设备名", self.self_hostname.clone(), muted))
            .child(self.render_kv("本机指纹", fingerprint.into(), muted))
            .child(self.render_kv("UDP 端口", self.port.to_string().into(), muted))
    }

    /// Sharing options per trusted peer. The daemon keys these on the
    /// peer's certificate fingerprint, so each authorized entry gets
    /// its own card; with none authorized the section explains how to
    /// get there.
    fn render_sharing_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex().child(self.render_kv_section("共享选项", cx));
        if self.authorized.is_empty() {
            return section.child(
                div()
                    .mt_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("暂无已信任设备。设备首次连接并授权后，可在此调整其共享选项。"),
            );
        }

        let mut peers: Vec<_> = self.authorized.iter().collect();
        peers.sort_by_key(|(_, peer)| peer.description.clone());
        for (fp, peer) in peers {
            section = section.child(self.render_peer_card(fp, peer, cx));
        }
        section
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

        v_flex()
            .mt_1()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .child(div().text_sm().child(name))
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
    fn render_sensitivity_stepper(
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
