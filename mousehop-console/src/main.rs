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

    gpui_kit::application().run(move |cx: &mut App| {
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
            app_id: Some("mousehop-console".into()),
            ..TitleBar::window_options()
        };

        let (_, console) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Console::new(window, cx, log_sink.clone()))
        })
        .expect("failed to open console window");

        ipc::spawn(cx, console.downgrade());
    });
}
