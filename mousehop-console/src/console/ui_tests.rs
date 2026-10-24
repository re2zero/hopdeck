//! Headless UI integration tests, run with:
//!
//! ```text
//! cargo test -p mousehop-console --features ui-test-support
//! ```
//!
//! They drive the real `Console` view in a test window and assert on
//! observed elements (the `.test_support()` markers on the exercised
//! elements are inert without the feature). gpui-kit's own
//! `test-support` is deliberately avoided — see `[features]` in
//! Cargo.toml for the resolution conflict — so the small harness below
//! re-creates the needed pieces of gpui-kit's `test` module from
//! public `gpui` window APIs and `gpui-base` observation.

use gpui::TestAppContext;
use gpui_kit::base::test_support::{self, ElementSnapshot};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, InputEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, Size, Window, WindowBounds, WindowOptions,
    px,
};

use super::Console;
use super::logbar::LogSink;
use crate::ipc::FeedEvent;
use mousehop_ipc::{ClientConfig, ClientState, FrontendEvent};

/// Invalidate and complete a frame so fresh state and observations
/// land in the last-frame registry the queries read.
fn frame(window: &mut Window, cx: &mut App) {
    window.refresh();
    window.draw(cx).clear(cx);
}

fn snapshot(window: &Window, id: ElementId) -> Option<ElementSnapshot> {
    test_support::find(window, &[], &id)
}

fn require(window: &Window, id: ElementId) -> ElementSnapshot {
    let moved = id.clone();
    snapshot(window, id).unwrap_or_else(|| {
        panic!(
            "missing ElementId {moved:?}; registered paths: {}",
            test_support::registered_paths(window)
        )
    })
}

/// Move, press and release at the element's center, one frame per
/// step — the same event sequence gpui-kit's `click` dispatches.
fn click(window: &mut Window, id: ElementId, cx: &mut App) {
    let shown = id.clone();
    let target = require(window, id);
    assert!(target.visible(), "ElementId {shown:?} is not visible");
    let position = target.bounds().center();
    let modifiers = Default::default();
    window.dispatch_event(
        MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        }
        .to_platform_input(),
        cx,
    );
    frame(window, cx);
    window.dispatch_event(
        MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
            first_mouse: true,
        }
        .to_platform_input(),
        cx,
    );
    frame(window, cx);
    window.dispatch_event(
        MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
        }
        .to_platform_input(),
        cx,
    );
    frame(window, cx);
}

/// Open the console at the production window size with no daemon
/// attached (`writer: None`, feed still connecting) — everything the
/// tests exercise renders from local state alone.
fn open_console(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Console>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: Size {
                width: px(1180.),
                height: px(780.),
            },
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Console::new(window, cx, LogSink::new())),
        )
        .expect("open console test window")
    })
}

#[gpui::test]
fn settings_show_backend_section_with_usbip_guidance(cx: &mut TestAppContext) {
    let (window, content) = open_console(cx);
    cx.update_window(window, |_, window, cx| {
        content.update(cx, |console, _| console.settings_open = true);
        frame(window, cx);
        require(window, "settings-backend".into());
        let card = require(window, "backend-usbip-card".into());
        assert!(card.visible());
    })
    .unwrap();
}

#[gpui::test]
fn debug_toggle_reveals_log_bar(cx: &mut TestAppContext) {
    let (window, content) = open_console(cx);
    cx.update_window(window, |_, window, cx| {
        frame(window, cx);
        assert!(
            snapshot(window, "log-body".into()).is_none(),
            "log bar must start closed"
        );
        content.update(cx, |console, cx| console.set_debug_open(true, cx));
        frame(window, cx);
        let body = require(window, "log-body".into());
        assert!(body.visible());
    })
    .unwrap();
}

#[gpui::test]
fn clicking_device_row_selects_it(cx: &mut TestAppContext) {
    let (window, content) = open_console(cx);
    cx.update_window(window, |_, window, cx| {
        content.update(cx, |console, cx| {
            console.on_feed(
                FeedEvent::Daemon(Box::new(FrontendEvent::Created(
                    7,
                    ClientConfig {
                        hostname: Some("alpha-book".to_string()),
                        ..Default::default()
                    },
                    ClientState::default(),
                ))),
                cx,
            );
        });
        frame(window, cx);
        click(window, ("device-row", 7_u64).into(), cx);
        let selected = content.read_with(cx, |console, _| console.selected);
        assert_eq!(selected, Some(7), "row click must select the device");
        frame(window, cx);
        require(window, "device-detail".into());
    })
    .unwrap();
}
