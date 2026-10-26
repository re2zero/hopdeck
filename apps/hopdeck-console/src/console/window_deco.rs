//! System-decoration strip for X11 sessions whose WM lacks
//! `_GTK_FRAME_EXTENTS` support (dde-kwin among them). gpui then refuses
//! client-side decorations and falls back to the WM title bar, stacking it
//! above our self-drawn TitleBar (公子: 跨系统统一，隐藏系统标题栏).
//!
//! The fallback is decided from `_NET_SUPPORTED` and never re-checked, so
//! the only way through is to write `_MOTIF_WM_HINTS` (decorations = none)
//! on our own window from a second X connection. KWin honors Motif hints
//! whether the window is mapped or not. Stripping the WM frame also
//! removes its resize borders, so [`resize_zone_overlay`] hands edge
//! resizing back via `start_window_resize` (a `_NET_WM_MOVERESIZE`
//! request every WM honors).

#[cfg(target_os = "linux")]
mod imp {
    use std::process;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use gpui_kit::InteractiveElement as _;
    use gpui_kit::{
        AnyElement, CursorStyle, IntoElement, MouseButton, ParentElement as _, Pixels, ResizeEdge,
        Size, Styled as _, Window, div, px,
    };
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::AtomEnum;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::wrapper::ConnectionExt as _;

    static FRAME_STRIPPED: AtomicBool = AtomicBool::new(false);

    /// Whether [`strip_server_frame`] hid the WM frame on this machine.
    fn frame_stripped() -> bool {
        FRAME_STRIPPED.load(Ordering::Relaxed)
    }

    /// Read-only view for the render path: the console paints its own
    /// window controls only when the WM title bar was actually removed.
    /// With the frame still in place the WM supplies them.
    pub fn server_frame_stripped() -> bool {
        frame_stripped()
    }

    /// Hide the system title bar on X11. Spawns a polling thread because
    /// the window is created asynchronously relative to callers.
    pub fn strip_server_frame() {
        const MWM_HINTS_DECORATIONS: u32 = 1 << 1;
        const RETRY: Duration = Duration::from_millis(100);
        const GIVE_UP: Duration = Duration::from_secs(5);

        thread::spawn(move || {
            let deadline = Instant::now() + GIVE_UP;
            while Instant::now() < deadline {
                if strip_once(MWM_HINTS_DECORATIONS) {
                    return;
                }
                thread::sleep(RETRY);
            }
            log::warn!("x11: console window never appeared; keeping system title bar");
        });
    }

    /// One discovery attempt over a fresh X connection. `true` when done
    /// (stripped, or X is unavailable and retrying is pointless).
    fn strip_once(mwm_hints_decorations: u32) -> bool {
        use x11rb::protocol::xproto::PropMode;

        let Ok((conn, screen)) = x11rb::connect(None) else {
            return true; // no X to talk to (Wayland/headless): WM keeps its frame
        };
        let root = conn.setup().roots[screen].root;
        let atom = |name: &[u8]| match conn.intern_atom(false, name) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => Some(reply.atom),
                Err(_) => None,
            },
            Err(_) => None,
        };
        let Some(motif) = atom(b"_MOTIF_WM_HINTS") else {
            return true;
        };
        let Some(net_wm_pid) = atom(b"_NET_WM_PID") else {
            return true;
        };
        let Some(net_client_list) = atom(b"_NET_CLIENT_LIST") else {
            return true; // WM without EWMH client list: nothing to match against
        };

        // Root's direct children are WM frame windows (KWin reparents);
        // _NET_CLIENT_LIST holds the actual client windows.
        let client_windows: Vec<x11rb::protocol::xproto::Window> =
            match conn.get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, 1024) {
                Ok(cookie) => match cookie.reply() {
                    Ok(prop) => prop
                        .value
                        .chunks_exact(4)
                        .filter_map(|c| <[u8; 4]>::try_from(c).ok())
                        .map(u32::from_ne_bytes)
                        .collect(),
                    Err(_) => return false,
                },
                Err(_) => return false,
            };

        let mut found = false;
        for window in client_windows {
            let pid = match conn.get_property(false, window, net_wm_pid, AtomEnum::CARDINAL, 0, 1) {
                Ok(cookie) => match cookie.reply() {
                    Ok(prop) => prop
                        .value
                        .get(..4)
                        .and_then(|b| <[u8; 4]>::try_from(b).ok())
                        .map(u32::from_ne_bytes),
                    Err(_) => return false,
                },
                Err(_) => return false,
            };
            if pid != Some(process::id()) {
                continue;
            }
            // flags = MWM_HINTS_DECORATIONS, decorations = none.
            let hints = [mwm_hints_decorations, 0, 0, 0, 0];
            if conn
                .change_property32(PropMode::REPLACE, window, motif, motif, &hints)
                .is_err()
            {
                return false;
            }
            found = true;
        }
        let _ = conn.flush();
        if found {
            FRAME_STRIPPED.store(true, Ordering::Relaxed);
        }
        found
    }

    /// Transparent hit bands along the window edges that keep edge-drag
    /// resizing working once the WM frame is gone. `None` when the WM
    /// still provides the frame itself.
    pub fn resize_zone_overlay(window: &Window) -> Option<impl IntoElement> {
        if !frame_stripped()
            || !window.is_resizable()
            || !matches!(window.window_decorations(), gpui_kit::Decorations::Server)
        {
            return None;
        }
        let size = window.window_bounds().get_bounds().size;
        Some(resize_zones(size))
    }

    fn resize_zones(size: Size<Pixels>) -> impl IntoElement {
        const HIT: Pixels = px(4.);
        const CORNER: Pixels = px(8.);

        let (w, h) = (size.width, size.height);
        let mut zones: Vec<AnyElement> = Vec::new();

        let mut push =
            |edge: ResizeEdge, left: Pixels, top: Pixels, width: Pixels, height: Pixels| {
                zones.push(
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(width)
                        .h(height)
                        .cursor(cursor_for(edge))
                        .on_mouse_down(MouseButton::Left, move |_, window, _| {
                            window.start_window_resize(edge);
                        })
                        .into_any_element(),
                );
            };

        push(ResizeEdge::Top, px(0.), px(0.), w, HIT);
        push(ResizeEdge::Bottom, px(0.), h - HIT, w, HIT);
        push(ResizeEdge::Left, px(0.), px(0.), HIT, h);
        push(ResizeEdge::Right, w - HIT, px(0.), HIT, h);
        // Corners after edges so they win the hit test.
        push(ResizeEdge::TopLeft, px(0.), px(0.), CORNER, CORNER);
        push(ResizeEdge::TopRight, w - CORNER, px(0.), CORNER, CORNER);
        push(ResizeEdge::BottomLeft, px(0.), h - CORNER, CORNER, CORNER);
        push(
            ResizeEdge::BottomRight,
            w - CORNER,
            h - CORNER,
            CORNER,
            CORNER,
        );

        div().absolute().size_full().children(zones)
    }

    fn cursor_for(edge: ResizeEdge) -> CursorStyle {
        match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
            ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use imp::{resize_zone_overlay, server_frame_stripped, strip_server_frame};

#[cfg(not(target_os = "linux"))]
mod imp {
    use gpui_kit::{IntoElement, Window};

    pub fn strip_server_frame() {}

    pub fn server_frame_stripped() -> bool {
        false
    }

    pub fn resize_zone_overlay(_window: &Window) -> Option<impl IntoElement> {
        None::<gpui_kit::AnyElement>
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) use imp::{resize_zone_overlay, server_frame_stripped, strip_server_frame};
