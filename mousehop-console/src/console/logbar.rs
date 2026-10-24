//! Debug log bar (design D14⑦, v2.2's `.mh-logbar`): a fixed-height
//! bar above the status bar mirroring two sources into one ring
//! buffer — the console's own event journal (`Console::journal` calls
//! from the feed/device/placement paths) and the `log` facade output
//! of the dependency crates, captured by a logger installed at
//! startup. Toggled from the status-bar bug button and the settings
//! overlay's 日志 section; the level selector gates only the facade
//! capture, journal lines always record what happened.

use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt,
    assets::IconName,
    component::{ActiveTheme, Icon, Sizable, h_flex, v_flex},
    div, px,
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use time::macros::format_description;

use super::Console;

/// How many lines the sink keeps before dropping the oldest.
const LOG_CAP: usize = 400;

/// How often the console looks for new sink lines while the bar is
/// open (journal paths notify eagerly; only facade lines need this).
const LOG_POLL: std::time::Duration = std::time::Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
    Ok,
    Debug,
    Trace,
}

impl LogLevel {
    /// Fixed-width level tag, the design mock's `padEnd(5)` column.
    fn label(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN ",
            LogLevel::Info => "INFO ",
            LogLevel::Ok => "OK   ",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }

    /// Lowercase name without padding, for the env var / settings UI.
    pub(super) fn name(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Ok => "ok",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }

    fn parse(raw: &str) -> Option<LogLevel> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "ok" | "success" => Some(LogLevel::Ok),
            "debug" => Some(LogLevel::Debug),
            "trace" => Some(LogLevel::Trace),
            _ => None,
        }
    }

    fn from_record(level: log::Level) -> LogLevel {
        match level {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Info => LogLevel::Info,
            log::Level::Debug => LogLevel::Debug,
            log::Level::Trace => LogLevel::Trace,
        }
    }

    /// The `log::LevelFilter` this selector value gates the facade
    /// capture with (`Ok` only ever comes from the journal itself).
    fn facade_filter(self) -> log::LevelFilter {
        match self {
            LogLevel::Trace => log::LevelFilter::Trace,
            LogLevel::Debug => log::LevelFilter::Debug,
            _ => log::LevelFilter::Info,
        }
    }
}

#[derive(Clone)]
pub(crate) struct LogEntry {
    stamp: SharedString,
    level: LogLevel,
    message: SharedString,
}

struct LogShared {
    lines: Mutex<VecDeque<LogEntry>>,
    /// Bumped on every push so the poll task can detect new facade
    /// lines without locking the buffer.
    gen: AtomicU64,
}

/// Cloneable handle to the console's log ring buffer. One instance is
/// shared between the facade logger, the journal call sites and the
/// log bar render.
#[derive(Clone)]
pub(crate) struct LogSink(Arc<LogShared>);

impl LogSink {
    pub(crate) fn new() -> Self {
        Self(Arc::new(LogShared {
            lines: Mutex::new(VecDeque::with_capacity(LOG_CAP)),
            gen: AtomicU64::new(0),
        }))
    }

    fn push(&self, level: LogLevel, message: SharedString) {
        let mut lines = self
            .0
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lines.len() >= LOG_CAP {
            lines.pop_front();
        }
        lines.push_back(LogEntry {
            stamp: now_stamp(),
            level,
            message,
        });
        drop(lines);
        self.0.gen.fetch_add(1, Ordering::Release);
    }

    fn gen(&self) -> u64 {
        self.0.gen.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> Vec<LogEntry> {
        self.0
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// Local wall clock `HH:MM:SS` (the design mock's `toTimeString`
/// slice). Falls back to UTC when the local offset is undeterminable.
fn now_stamp() -> SharedString {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    now.format(&format_description!("[hour]:[minute]:[second]"))
        .unwrap_or_else(|_| "--:--:--".to_string())
        .into()
}

/// Last path segment of a log target — enough to tell which component
/// spoke without filling the bar with module paths.
fn short_target(target: &str) -> &str {
    target.rsplit("::").next().unwrap_or(target)
}

struct FacadeLogger {
    sink: LogSink,
}

impl log::Log for FacadeLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        log::max_level() >= metadata.level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.sink.push(
            LogLevel::from_record(record.level()),
            format!("{}: {}", short_target(record.target()), record.args()).into(),
        );
    }

    fn flush(&self) {}
}

/// Install the facade capture. Called once from `main` before any
/// window exists; a second call (tests, embeddings) is a no-op.
pub(crate) fn install_facade_logger(sink: LogSink, level: LogLevel) {
    if log::set_boxed_logger(Box::new(FacadeLogger { sink })).is_ok() {
        log::set_max_level(level.facade_filter());
    }
}

/// Initial facade level: `MOUSEHOP_CONSOLE_LOG` if it names a level,
/// else `info`. Debug floods the ring within seconds (gpui logs a
/// line per text layout), evicting the journal lines the bar exists
/// to show; info keeps dependency noise to real events.
pub(crate) fn initial_level() -> LogLevel {
    std::env::var("MOUSEHOP_CONSOLE_LOG")
        .ok()
        .and_then(|raw| LogLevel::parse(&raw))
        .unwrap_or(LogLevel::Info)
}

impl Console {
    /// Record a console-side event. Journal lines always land in the
    /// buffer (cheap, capped); the poll task handles repaints when the
    /// bar is closed or the caller has no notify of its own.
    pub(super) fn journal(&self, level: LogLevel, message: impl Into<SharedString>) {
        self.log_sink.push(level, message.into());
    }

    /// Drain new facade lines into the UI while the bar is open. The
    /// entity owns the sink but pushes come from arbitrary threads, so
    /// this poll — instead of a notify per push — batches them.
    pub(super) fn spawn_log_poll(&self, cx: &mut Context<Self>) {
        let sink = self.log_sink.clone();
        cx.spawn(async move |this, cx| {
            let mut seen = sink.gen();
            loop {
                cx.background_executor().timer(LOG_POLL).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    let gen = this.log_sink.gen();
                    if gen != seen {
                        seen = gen;
                        if this.debug_open {
                            cx.notify();
                        }
                    }
                }) else {
                    return;
                };
            }
        })
        .detach();
    }

    /// The fixed-height log bar: head row (title, current level hint,
    /// line count) above the scrollable mono body. New lines pin the
    /// view to the bottom; scrolling up while idle stays put.
    pub(super) fn render_log_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let lines = self.log_sink.snapshot();

        let body = div()
            .id("log-body")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.log_scroll)
            .px_3()
            .py_1()
            .font_family(cx.theme().mono_font_family.clone())
            .text_xs()
            .children(lines.iter().map(|entry| {
                let color = self.log_level_color(entry.level, cx);
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(muted)
                            .child(entry.stamp.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(color)
                            .child(entry.level.label()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_color(cx.theme().foreground)
                            .child(entry.message.clone()),
                    )
            }));

        // Pin to the bottom when lines arrived since the last paint;
        // `Cell` because render only gets `&self`.
        if self.log_sink.gen() != self.log_gen_seen.get() {
            self.log_gen_seen.set(self.log_sink.gen());
            self.log_scroll.scroll_to_bottom();
        }

        v_flex()
            .h(px(128.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(muted)
                    .child(Icon::new(IconName::Bug).small())
                    .child(div().text_color(cx.theme().foreground).child("调试日志"))
                    .child(format!("MOUSEHOP_CONSOLE_LOG={}", self.log_level.name()))
                    .child(
                        h_flex()
                            .ml_auto()
                            .gap_2()
                            .child(format!("{} 行", lines.len())),
                    ),
            )
            .child(body)
    }

    fn log_level_color(&self, level: LogLevel, cx: &mut Context<Self>) -> gpui_kit::Hsla {
        let theme = cx.theme();
        match level {
            LogLevel::Error => theme.danger,
            LogLevel::Warn => theme.warning,
            LogLevel::Info => theme.accent,
            LogLevel::Ok => theme.success,
            LogLevel::Debug | LogLevel::Trace => theme.muted_foreground,
        }
    }

    /// Settings overlay 日志 section: the bar toggle plus the facade
    /// capture level (info / debug / trace, design v2.2's selector).
    pub(super) fn render_log_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let entity = cx.entity();

        v_flex()
            .child(self.render_kv_section("日志", cx))
            .child(super::settings::option_row(
                "调试日志",
                "在窗口底部显示事件日志",
                super::settings::log_switch(entity, self.debug_open),
                muted,
            ))
            .child(super::settings::option_row(
                "日志级别",
                "依赖库日志的捕获下限（本机事件始终记录）",
                self.render_level_buttons(cx),
                muted,
            ))
    }

    fn render_level_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = h_flex().gap_1();
        for level in [LogLevel::Info, LogLevel::Debug, LogLevel::Trace] {
            row = row.child(super::settings::level_button(
                level,
                level == self.log_level,
                cx.listener(move |this, _: &gpui_kit::ClickEvent, _, cx| {
                    this.set_log_level(level, cx);
                }),
            ));
        }
        row
    }

    fn set_log_level(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        self.log_level = level;
        log::set_max_level(level.facade_filter());
        self.journal(LogLevel::Info, format!("依赖日志级别 → {}", level.name()));
        cx.notify();
    }

    /// Shared toggle path for the status-bar bug button and the
    /// settings switch.
    pub(super) fn set_debug_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.debug_open = open;
        if open {
            // Opening after idle time: one fresh pin instead of
            // repainting for everything accumulated meanwhile.
            self.log_gen_seen.set(self.log_sink.gen());
            self.log_scroll.scroll_to_bottom();
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_caps_at_capacity() {
        let sink = LogSink::new();
        for i in 0..(LOG_CAP + 50) {
            sink.push(LogLevel::Info, format!("line {i}").into());
        }
        let snap = sink.snapshot();
        assert_eq!(snap.len(), LOG_CAP);
        assert_eq!(snap.last().unwrap().message, "line 449");
        assert_eq!(snap.first().unwrap().message, "line 50");
    }

    #[test]
    fn gen_tracks_pushes() {
        let sink = LogSink::new();
        assert_eq!(sink.gen(), 0);
        sink.push(LogLevel::Warn, "x".into());
        sink.push(LogLevel::Warn, "y".into());
        assert_eq!(sink.gen(), 2);
    }

    #[test]
    fn level_parse_matches_env_spellings() {
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse(" TRACE "), Some(LogLevel::Trace));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("verbose"), None);
    }

    #[test]
    fn short_target_keeps_last_segment() {
        assert_eq!(short_target("gpui_kit::component::button"), "button");
        assert_eq!(short_target("mousehop_console"), "mousehop_console");
        assert_eq!(short_target(""), "");
    }

    #[test]
    fn facade_filter_maps_selector_values() {
        assert_eq!(LogLevel::Trace.facade_filter(), log::LevelFilter::Trace);
        assert_eq!(LogLevel::Debug.facade_filter(), log::LevelFilter::Debug);
        assert_eq!(LogLevel::Info.facade_filter(), log::LevelFilter::Info);
        assert_eq!(LogLevel::Warn.facade_filter(), log::LevelFilter::Info);
    }
}
