//! Interactive layout canvas (design D16): the host screen surrounded by
//! device slots — one each on the left, right and bottom edge, up to
//! three on the top row. Device rows drag onto a slot to (re)place the
//! device; placement is the daemon's [`FrontendRequest::UpdatePosition`],
//! so the canvas is a live view of `config.pos` with capacity guards,
//! not a separate layout store. There is deliberately no "unplaced"
//! state: the daemon's `Position` has no such variant (adding one spans
//! capture/emulation/frontends and is parked as a product decision), so every
//! device sits in exactly one slot and dropping onto an occupied one is
//! rejected with a toast.

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    ClickEvent, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window,
    assets::IconName,
    component::{
        ActiveTheme, Icon, WindowExt,
        button::{Button, ButtonVariants},
        h_flex,
        notification::Notification,
        v_flex,
    },
    div, px,
};
use hopdeck_ipc::{ClientHandle, FrontendRequest, Position};

use super::{Console, DeviceEntry, position_label};

/// The value carried from a list row to a canvas slot during a drag:
/// identity for the drop handler, plus what the floating ghost renders.
#[derive(Clone)]
pub(super) struct DeviceDrag {
    pub(super) handle: ClientHandle,
    pub(super) name: SharedString,
    pub(super) online: bool,
}

impl Render for DeviceDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fg = if self.online {
            cx.theme().foreground
        } else {
            cx.theme().muted_foreground
        };
        h_flex()
            .px_2()
            .py_1()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_sm()
            .text_color(fg)
            .shadow_md()
            .child(self.name.clone())
    }
}

/// Every placeable canvas position, row-major (公子: 下方 3 格全开放，
/// 控制方向含四角).
#[cfg(test)]
pub(super) const CANVAS_SLOTS: [Position; 8] = [
    Position::TopLeft,
    Position::Top,
    Position::TopRight,
    Position::Left,
    Position::Right,
    Position::BottomLeft,
    Position::Bottom,
    Position::BottomRight,
];

/// Why a placement is rejected, surfaced as a toast.
#[derive(Debug, PartialEq, Eq)]
enum PlacementRejection {
    /// Each canvas slot holds exactly one device.
    SlotOccupied,
}

/// The guard behind drop handling: `None` places the device, `Some` is
/// the rejection reason. Re-dropping onto its own slot is a silent
/// no-op before this.
fn placement_rejection(occupant_count: usize) -> Option<PlacementRejection> {
    (occupant_count > 0).then_some(PlacementRejection::SlotOccupied)
}

/// Colored bar on the canvas edge where the device takes over the cursor
/// (design's `.mh-edge`): a `Left` device sits left of the host, so its
/// box lights the edge facing the host. The design centers the bar at
/// 52% of the slot edge — approximated with a fixed inset.
fn edge_bar(pos: Position, color: Hsla) -> Div {
    let vertical = |side_right: bool| {
        let bar = div().absolute().bg(color).top_3().bottom_3().w_1();
        if side_right {
            bar.right_0()
        } else {
            bar.left_0()
        }
    };
    let horizontal = |side_bottom: bool| {
        let bar = div().absolute().bg(color).left_3().right_3().h_1();
        if side_bottom {
            bar.bottom_0()
        } else {
            bar.top_0()
        }
    };
    match pos {
        // Corners light both adjacent edges.
        Position::TopLeft => div()
            .absolute()
            .inset_0()
            .child(vertical(false))
            .child(horizontal(false)),
        Position::TopRight => div()
            .absolute()
            .inset_0()
            .child(vertical(true))
            .child(horizontal(false)),
        Position::BottomLeft => div()
            .absolute()
            .inset_0()
            .child(vertical(false))
            .child(horizontal(true)),
        Position::BottomRight => div()
            .absolute()
            .inset_0()
            .child(vertical(true))
            .child(horizontal(true)),
        Position::Left => vertical(false),
        Position::Right => vertical(true),
        Position::Top => horizontal(false),
        Position::Bottom => horizontal(true),
        Position::Unplaced => div().absolute(),
    }
}

impl Console {
    /// The layout strip under the device detail: header, then the three
    /// canvas rows (top / mid / bottom).
    pub(super) fn render_layout_canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .flex_shrink_0()
            .gap_2()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(muted).child("屏幕布局"))
                    .child(
                        h_flex()
                            .flex_1()
                            .justify_end()
                            .text_xs()
                            .text_color(muted)
                            .child("拖入空格设为共享屏 · 四角与四边共 8 个方位"),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary.opacity(0.35))
                    // Fixed row heights, not flex equalization: flex_1
                    // rows let content pull rows apart (公子: 有设备时
                    // 高度没有保持). 76px × 3 + gaps + padding = 260.
                    .child(self.render_top_row(cx).h(px(76.)).flex_shrink_0())
                    .child(self.render_mid_row(cx).h(px(76.)).flex_shrink_0())
                    .child(self.render_bottom_row(cx).h(px(76.)).flex_shrink_0()),
            )
    }

    /// The canvas is a 3×3 grid: the host screen in the center, three
    /// slots on the top edge, one each left/right, and the bottom
    /// middle — the two lower corners stay disabled (公子: 九宫格，
    /// 左下右下禁掉即可得 1-6). Each cell holds the devices assigned
    /// to its position; empty cells are drop targets.
    fn render_top_row(&self, cx: &mut Context<Self>) -> Div {
        self.render_slot_row([Position::TopLeft, Position::Top, Position::TopRight], cx)
    }

    fn render_mid_row(&self, cx: &mut Context<Self>) -> Div {
        h_flex()
            .gap_2()
            .items_stretch()
            .child(self.render_slot_cell(Position::Left, cx))
            .child(self.render_host_box(cx))
            .child(self.render_slot_cell(Position::Right, cx))
    }

    fn render_bottom_row(&self, cx: &mut Context<Self>) -> Div {
        // 公子: 下方 3 格全开放 — all eight surrounding slots take a
        // device, corners included.
        self.render_slot_row(
            [
                Position::BottomLeft,
                Position::Bottom,
                Position::BottomRight,
            ],
            cx,
        )
    }

    fn render_slot_row<const N: usize>(&self, slots: [Position; N], cx: &mut Context<Self>) -> Div {
        let mut row = h_flex().gap_2().items_stretch();
        for pos in slots {
            row = row.child(self.render_slot_cell(pos, cx));
        }
        row
    }

    /// One grid cell: the device assigned to `pos`, or a dropzone.
    fn render_slot_cell(&self, pos: Position, cx: &mut Context<Self>) -> Div {
        match self.clients.iter().find(|client| client.config.pos == pos) {
            Some(entry) => div()
                .flex_1()
                .min_w_0()
                .child(self.render_placed_box(entry, cx)),
            None => div()
                .flex_1()
                .min_w_0()
                .child(self.render_dropzone(pos, cx)),
        }
    }

    fn render_dropzone(&self, pos: Position, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = cx.theme().accent;
        // Same skeleton — and the same cell size — as the host box
        // (公子: 九宫格框始终保持与本机同宽高): dashed accent marks the
        // cell as free, solid accent on hover.
        h_flex()
            .h_full()
            .w_full()
            .min_w_0()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_dashed()
            .border_color(accent.opacity(0.45))
            .bg(cx.theme().secondary)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .drag_over::<DeviceDrag>(move |style, _, _, _| {
                style
                    .border_color(accent)
                    .text_color(accent)
                    .bg(accent.opacity(0.1))
            })
            .on_drop(cx.listener(move |this, drag: &DeviceDrag, window, cx| {
                this.place_device(drag.handle, pos, window, cx);
            }))
            .child(format!(
                "+ 拖入设为{}",
                position_label(pos)
                    .trim_end_matches('侧')
                    .trim_end_matches('方')
            ))
    }

    fn render_placed_box(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = entry.handle;
        let pos = entry.config.pos;
        let online = entry.online();
        let active = entry.state.active;
        let name = entry.name();
        let fg = if online {
            cx.theme().foreground
        } else {
            cx.theme().muted_foreground
        };
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().accent;
        let mut sub = entry.addr_label().to_string();
        if !online {
            sub.push_str(" · 离线");
        }
        // 移出布局 = 停用（D26①）：边缘不再触发，但槽位仍被该设备占用。
        if !active {
            sub.push_str(" · 已停用");
        }

        v_flex()
            .id(("layout-slot", handle))
            .h_full()
            .flex_1()
            .min_w_0()
            .items_center()
            .justify_center()
            .gap_0p5()
            .px_3()
            .relative()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(accent.opacity(0.5))
            .bg(cx.theme().secondary)
            .when(!online || !active, |box_| box_.opacity(0.6))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = Some(handle);
                cx.notify();
            }))
            .drag_over::<DeviceDrag>(move |style, _, _, _| style.border_color(accent))
            .on_drop(cx.listener(move |this, drag: &DeviceDrag, window, cx| {
                this.place_device(drag.handle, pos, window, cx);
            }))
            .child(
                div()
                    .max_w_full()
                    .text_center()
                    .text_sm()
                    .text_color(fg)
                    .truncate()
                    .child(name),
            )
            .child(
                div()
                    .max_w_full()
                    .text_center()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(sub),
            )
            .child(edge_bar(
                pos,
                if active {
                    cx.theme().success
                } else {
                    cx.theme().muted_foreground
                },
            ))
            .child(self.render_slot_action(entry, cx))
    }

    /// The slot's remove action, icon-only so it reads the same in any
    /// locale (公子): moves the device off the canvas entirely — the
    /// daemon records `Position::Unplaced`, the cell becomes a drop
    /// target again, and the device waits in the list to be dragged
    /// back onto any free slot.
    fn render_slot_action(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = entry.handle;

        div().absolute().top_1().right_1().child(
            Button::new(("layout-remove", handle))
                .icon(Icon::new(IconName::X))
                .ghost()
                .tooltip("移出布局")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.remove_from_layout(handle, window, cx);
                })),
        )
    }

    /// Move a device off the canvas: `UpdatePosition(Unplaced)`. No
    /// capture barrier is armed for an unplaced client, so this
    /// supersedes the old deactivate-then-restore dance (D26① was a
    /// workaround for the Position enum having no off-canvas value).
    fn remove_from_layout(
        &mut self,
        handle: ClientHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entry(handle) else {
            return;
        };
        if entry.config.pos == Position::Unplaced {
            return;
        }
        let name = entry.name();
        // Demo echo, same rationale as `place_device`.
        if super::demo::enabled() {
            if let Some(entry) = self.clients.iter_mut().find(|e| e.handle == handle) {
                entry.config.pos = Position::Unplaced;
            }
        } else {
            self.request(FrontendRequest::UpdatePosition(handle, Position::Unplaced));
        }
        cx.notify();
        self.journal(
            super::logbar::LogLevel::Info,
            format!("布局移出：「{name}」（未布置，可再拖入任意空格）"),
        );
        window.push_notification(Notification::info(format!("「{name}」已移出布局")), cx);
    }

    fn render_host_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fg = cx.theme().secondary_foreground;
        v_flex()
            .h_full()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().accent)
            .bg(cx.theme().secondary)
            .child(div().text_xs().text_color(fg).truncate().child("本机屏幕"))
            .child(
                div()
                    .text_sm()
                    .text_color(fg)
                    .truncate()
                    .child(self.self_hostname.clone()),
            )
    }

    /// Place (or move) a device onto a slot: guarded by capacity, sent
    /// to the daemon as `UpdatePosition`, echoed back through the usual
    /// `State` event. Toasts go through `window.push_notification`
    /// directly: drop listeners already run inside a window update, and
    /// a re-entrant `window.update` (what `notify_now` does for the
    /// window-less feed callbacks) gets swallowed there.
    fn place_device(
        &mut self,
        handle: ClientHandle,
        pos: Position,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entry(handle) else {
            return;
        };
        if entry.config.pos == pos {
            return;
        }
        let name = entry.name();
        let occupant = self
            .clients
            .iter()
            .find(|other| other.handle != handle && other.config.pos == pos)
            .map(|other| other.name());
        match placement_rejection(occupant.is_some() as usize) {
            Some(PlacementRejection::SlotOccupied) => {
                let occupant = occupant.expect("guarded by is_some");
                self.journal(
                    super::logbar::LogLevel::Warn,
                    format!(
                        "布局拒绝：{}已有「{}」（{name}）",
                        position_label(pos),
                        occupant
                    ),
                );
                window.push_notification(
                    Notification::warning(format!(
                        "{}已有「{}」，先移出再放置",
                        position_label(pos),
                        occupant
                    )),
                    cx,
                );
            }
            None => {
                // Demo mode has no daemon to echo `UpdatePosition` back
                // as `State` events (the writer is `None`), so mutate
                // the mirrored entry in place — otherwise the drop
                // would never repaint (公子: 拖入没有更新效果，修复).
                if super::demo::enabled() {
                    if let Some(entry) = self.clients.iter_mut().find(|e| e.handle == handle) {
                        entry.config.pos = pos;
                    }
                } else {
                    self.request(FrontendRequest::UpdatePosition(handle, pos));
                }
                cx.notify();
                self.journal(
                    super::logbar::LogLevel::Info,
                    format!("布局：{name} → {}", position_label(pos)),
                );
                window.push_notification(
                    Notification::success(format!("「{name}」已放到{}", position_label(pos))),
                    cx,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slot_holds_one_device() {
        for _pos in CANVAS_SLOTS {
            assert_eq!(placement_rejection(0), None);
            assert_eq!(
                placement_rejection(1),
                Some(PlacementRejection::SlotOccupied)
            );
        }
    }

    #[test]
    fn canvas_has_three_rows_around_the_host() {
        assert_eq!(CANVAS_SLOTS.len(), 8);
        assert!(CANVAS_SLOTS.contains(&Position::BottomLeft));
        assert!(CANVAS_SLOTS.contains(&Position::BottomRight));
    }
}
