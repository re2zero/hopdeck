// Windows: GUI subsystem so launching hopdeck-console.exe does not
// open an extra console window alongside the console UI.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod console;
mod ipc;

use console::Console;
use gpui_kit::{
    App, AppContext, Bounds, WindowBounds, WindowOptions, component::TitleBar, point, px, size,
};

fn main() {
    // The debug log bar's facade capture must exist before any
    // dependency logs: install it ahead of the window.
    let log_sink = console::logbar::LogSink::new();
    let log_level = console::logbar::initial_level();
    console::logbar::install_facade_logger(log_sink.clone(), log_level);

    gpui_kit::application()
        // Icon SVGs resolve through the asset source; without this every
        // Icon paints blank (element tree still registers, so tests pass).
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);

            // The console keeps a fixed product height (design D16): content
            // switches must not resize the window. The user may still resize it.
            let bounds = Bounds {
                origin: point(px(120.), px(80.)),
                size: size(px(1180.), px(780.)),
            };
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.), px(680.))),
                app_id: Some("hopdeck-console".into()),
                // Taskbar / alt-tab identity now that the in-window
                // title text is gone.
                titlebar: Some(gpui_kit::TitlebarOptions {
                    title: Some("HopDeck".into()),
                    ..TitleBar::title_bar_options()
                }),
                // Client-side decorations: the self-drawn TitleBar is the
                // only one (公子: 系统标题栏下多了一行空的标题栏，去掉).
                // Its window buttons (minimize/maximize/close) only
                // render in this mode.
                window_decorations: Some(gpui_kit::WindowDecorations::Client),
                ..TitleBar::window_options()
            };

            let (_, console) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Console::new(window, cx, log_sink.clone()))
            })
            .expect("failed to open console window");

            // The self-drawn TitleBar is meant to be the only one. On X11
            // WMs without _GTK_FRAME_EXTENTS support (dde-kwin) gpui falls
            // back to server decorations anyway; strip the WM frame
            // ourselves (公子: 隐藏系统标题栏，避免跨系统不一致).
            console::window_deco::strip_server_frame();

            ipc::spawn(cx, console.downgrade());
        });
}
