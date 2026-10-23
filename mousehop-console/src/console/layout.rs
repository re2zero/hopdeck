//! Interactive layout canvas (design D16): the host screen surrounded by
//! device slots — one each on the left, right and bottom edge, up to
//! three on the top row. Device rows drag onto a slot to (re)place the
//! device; placement is the daemon's [`FrontendRequest::UpdatePosition`],
//! so the canvas is a live view of `config.pos` with capacity guards,
//! not a separate layout store. There is deliberately no "unplaced"
//! state: the daemon's `Position` has no such variant (adding one spans
//! capture/emulation/GTK and is parked as a product decision), so every
//! device sits in exactly one slot and dropping onto an occupied one is
//! rejected with a toast.

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    ClickEvent, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window,
    component::{ActiveTheme, WindowExt, h_flex, notification::Notification, v_flex},
    div,
};
use mousehop_ipc::{ClientHandle, FrontendRequest, Position};

use super::{Console, DeviceEntry, position_label};

/// How many devices fit on the canvas's top row.
const TOP_SLOTS: usize = 3;

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

/// Why a placement is rejected, surfaced as a toast.
#[derive(Debug, PartialEq, Eq)]
enum PlacementRejection {
    /// Left/right/bottom hold a single device.
    SlotOccupied,
    /// The top row's three slots are taken.
    TopFull,
}

/// The guard behind drop handling: `None` places the device, `Some` is
/// the rejection reason. `occupant_count` excludes the dragged device
/// itself (re-dropping onto its own slot is a silent no-op before this).
fn placement_rejection(pos: Position, occupant_count: usize) -> Option<PlacementRejection> {
    match pos {
        Position::Top if occupant_count >= TOP_SLOTS => Some(PlacementRejection::TopFull),
        Position::Top => None,
        _ if occupant_count > 0 => Some(PlacementRejection::SlotOccupied),
        _ => None,
    }
}

/// Colored bar on the canvas edge where the device takes over the cursor
/// (design's `.mh-edge`): a `Left` device sits left of the host, so its
/// box lights the edge facing the host.
fn edge_bar(pos: Position, color: Hsla) -> Div {
    let base = div().absolute().bg(color);
    match pos {
        Position::Left => base.right_0().top_0().bottom_0().w_1(),
        Position::Right => base.left_0().top_0().bottom_0().w_1(),
        Position::Top => base.bottom_0().left_0().right_0().h_1(),
        Position::Bottom => base.top_0().left_0().right_0().h_1(),
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
                            .child("把左侧设备拖到本机屏幕四周 · 上排最多 3 块"),
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
                    .child(self.render_top_row(cx))
                    .child(self.render_mid_row(cx))
                    .child(self.render_bottom_row(cx)),
            )
    }

    fn render_top_row(&self, cx: &mut Context<Self>) -> Div {
        let mut row = h_flex().gap_2().min_h_12().items_stretch();
        let placed = self.clients_at(Position::Top);
        for entry in &placed {
            row = row.child(self.render_placed_box(entry, cx));
        }
        if placed.len() < TOP_SLOTS {
            row = row.child(self.render_dropzone(Position::Top, cx));
        }
        row
    }

    fn render_mid_row(&self, cx: &mut Context<Self>) -> Div {
        h_flex()
            .gap_2()
            .min_h_24()
            .items_stretch()
            .child(self.render_side_slot(Position::Left, cx))
            .child(self.render_host_box(cx))
            .child(self.render_side_slot(Position::Right, cx))
    }

    fn render_bottom_row(&self, cx: &mut Context<Self>) -> Div {
        h_flex()
            .min_h_12()
            .items_stretch()
            .child(self.render_side_slot(Position::Bottom, cx))
    }

    /// Every device currently sitting on one side. Legacy configs may
    /// list more than a slot holds — they all render (packed) rather
    /// than hiding state; new placements are guarded instead.
    fn clients_at(&self, pos: Position) -> Vec<&DeviceEntry> {
        self.clients
            .iter()
            .filter(|entry| entry.config.pos == pos)
            .collect()
    }

    /// One side of the mid/bottom rows: the placed device boxes, or an
    /// empty dropzone. The slot itself is a passive wrapper — drop and
    /// highlight live on the boxes inside so occupied slots stay
    /// drop targets (they answer with the rejection toast).
    fn render_side_slot(&self, pos: Position, cx: &mut Context<Self>) -> Div {
        let occupants = self.clients_at(pos);
        let mut slot = h_flex().flex_1().min_w_0().gap_2().items_stretch();
        if occupants.is_empty() {
            slot = slot.child(self.render_dropzone(pos, cx));
        } else {
            for entry in occupants {
                slot = slot.child(self.render_placed_box(entry, cx));
            }
        }
        slot
    }

    fn render_dropzone(&self, pos: Position, cx: &mut Context<Self>) -> impl IntoElement {
        let label = position_label(pos);
        let accent = cx.theme().accent;
        h_flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_dashed()
            .border_color(cx.theme().border)
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
            .child(format!("{label} · 拖设备到此"))
    }

    fn render_placed_box(&self, entry: &DeviceEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = entry.handle;
        let pos = entry.config.pos;
        let online = entry.online();
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

        v_flex()
            .id(("layout-slot", handle))
            .flex_1()
            .min_w_0()
            .justify_center()
            .gap_0p5()
            .px_3()
            .relative()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .when(!online, |box_| box_.opacity(0.6))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = Some(handle);
                cx.notify();
            }))
            .drag_over::<DeviceDrag>(move |style, _, _, _| style.border_color(accent))
            .on_drop(cx.listener(move |this, drag: &DeviceDrag, window, cx| {
                this.place_device(drag.handle, pos, window, cx);
            }))
            .child(div().text_sm().text_color(fg).truncate().child(name))
            .child(div().text_xs().text_color(muted).truncate().child(sub))
            .child(edge_bar(pos, cx.theme().success))
    }

    fn render_host_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fg = cx.theme().secondary_foreground;
        v_flex()
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
        let occupants: Vec<SharedString> = self
            .clients
            .iter()
            .filter(|other| other.handle != handle && other.config.pos == pos)
            .map(|other| other.name())
            .collect();
        match placement_rejection(pos, occupants.len()) {
            Some(PlacementRejection::TopFull) => {
                window.push_notification(
                    Notification::warning(format!("上排最多放置 {TOP_SLOTS} 块屏")),
                    cx,
                );
            }
            Some(PlacementRejection::SlotOccupied) => {
                window.push_notification(
                    Notification::warning(format!(
                        "{}屏已有「{}」，先移出再放置",
                        position_label(pos),
                        occupants[0]
                    )),
                    cx,
                );
            }
            None => {
                self.request(FrontendRequest::UpdatePosition(handle, pos));
                window.push_notification(
                    Notification::success(format!("「{name}」已放到{}屏", position_label(pos))),
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
    fn top_row_holds_three_devices() {
        assert_eq!(placement_rejection(Position::Top, 0), None);
        assert_eq!(placement_rejection(Position::Top, 2), None);
        assert_eq!(
            placement_rejection(Position::Top, 3),
            Some(PlacementRejection::TopFull)
        );
    }

    #[test]
    fn single_sides_hold_one_device() {
        for pos in [Position::Left, Position::Right, Position::Bottom] {
            assert_eq!(placement_rejection(pos, 0), None);
            assert_eq!(
                placement_rejection(pos, 1),
                Some(PlacementRejection::SlotOccupied)
            );
        }
    }
}
