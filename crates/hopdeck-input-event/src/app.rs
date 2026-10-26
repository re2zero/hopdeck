//! Application identity shared by the capture layer and the IPC surface.
//!
//! Clipboard suppression compares the frontmost app against a user list
//! of per-OS identifiers. The capture backends produce these values, the
//! daemon stores them in config, and the frontends render them — so the
//! types live here in the input domain rather than in `hopdeck-ipc`,
//! which would make the capture layer depend on the frontend protocol.

use serde::{Deserialize, Serialize};

/// Cross-platform identifier for an application whose clipboard
/// should not be shared with peers. The variant captures the
/// platform-specific string the OS surfaces for "this is app X":
/// macOS bundle ID, Windows executable basename, X11 `WM_CLASS`,
/// Wayland `xdg-toplevel.app_id`. Comparison is case-insensitive
/// within the same variant; cross-variant comparisons are always
/// `false` so a `LinuxX11("Chromium")` entry does not unexpectedly
/// suppress a Mac peer's `MacBundle("org.chromium.Chromium")`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum AppIdent {
    /// macOS bundle identifier, e.g. `com.1password.1password7`.
    MacBundle(String),
    /// Windows executable basename (lowercased), e.g.
    /// `1password.exe`.
    WindowsExe(String),
    /// X11 `WM_CLASS` instance/name (lowercased), e.g. `firefox`.
    LinuxX11(String),
    /// Wayland `xdg-toplevel.app_id` (lowercased), e.g.
    /// `org.mozilla.firefox`.
    LinuxWayland(String),
}

impl AppIdent {
    /// Case-insensitive equality within the same variant.
    pub fn matches(&self, other: &AppIdent) -> bool {
        match (self, other) {
            (AppIdent::MacBundle(a), AppIdent::MacBundle(b))
            | (AppIdent::WindowsExe(a), AppIdent::WindowsExe(b))
            | (AppIdent::LinuxX11(a), AppIdent::LinuxX11(b))
            | (AppIdent::LinuxWayland(a), AppIdent::LinuxWayland(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }

    /// Human-readable rendering: `value (kind)` so the GUI can show
    /// `1password.exe (Windows)` rather than the raw enum.
    pub fn label(&self) -> String {
        match self {
            AppIdent::MacBundle(v) => format!("{v} (macOS bundle)"),
            AppIdent::WindowsExe(v) => format!("{v} (Windows)"),
            AppIdent::LinuxX11(v) => format!("{v} (X11)"),
            AppIdent::LinuxWayland(v) => format!("{v} (Wayland)"),
        }
    }
}

/// The [`AppIdent`] variant that this binary's host OS produces.
/// Computed once per call (cheap) so a Wayland session that
/// starts/stops mid-process picks up the change at the next
/// suppression check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    MacBundle,
    WindowsExe,
    LinuxWayland,
    LinuxX11,
}

impl HostKind {
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            HostKind::MacBundle
        }
        #[cfg(windows)]
        {
            HostKind::WindowsExe
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if std::env::var_os("WAYLAND_DISPLAY")
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            {
                HostKind::LinuxWayland
            } else {
                HostKind::LinuxX11
            }
        }
    }

    /// Wrap a host-OS identifier string in the matching [`AppIdent`]
    /// variant for the runtime suppression check.
    pub fn make_ident(self, value: String) -> AppIdent {
        match self {
            HostKind::MacBundle => AppIdent::MacBundle(value),
            HostKind::WindowsExe => AppIdent::WindowsExe(value),
            HostKind::LinuxWayland => AppIdent::LinuxWayland(value),
            HostKind::LinuxX11 => AppIdent::LinuxX11(value),
        }
    }

    /// Short noun used in GUI prompts ("Bundle ID", "Executable
    /// name", "WM_CLASS", …).
    pub fn entry_noun(self) -> &'static str {
        match self {
            HostKind::MacBundle => "Bundle ID",
            HostKind::WindowsExe => "Executable name",
            HostKind::LinuxWayland => "App ID",
            HostKind::LinuxX11 => "WM_CLASS",
        }
    }

    /// Placeholder shown inside the GUI's add-an-app text field.
    pub fn placeholder(self) -> &'static str {
        match self {
            HostKind::MacBundle => "e.g. com.1password.1password7",
            HostKind::WindowsExe => "e.g. 1Password.exe",
            HostKind::LinuxWayland => "e.g. org.keepassxc.KeePassXC",
            HostKind::LinuxX11 => "e.g. KeePassXC",
        }
    }
}

/// Convenience: wrap `value` in the host-OS [`AppIdent`] variant.
/// Equivalent to `HostKind::current().make_ident(value)`.
pub fn host_app_ident(value: String) -> AppIdent {
    HostKind::current().make_ident(value)
}

/// One running-app candidate for the suppression-list picker. The
/// `identifier` is the host-OS string the runtime suppression check
/// compares against (bundle ID on macOS, exe basename on Windows,
/// `xdg-toplevel.app_id` on Wayland, `WM_CLASS` on X11). The
/// `display_name` is what we show in the dropdown — usually the
/// app's localized name on macOS, or just the identifier on
/// platforms where there's no separate user-facing label.
/// `icon_png` is an optional PNG-encoded icon (~32×32) the GUI can
/// render alongside the name; `None` on platforms where icon
/// extraction isn't implemented or when the OS reports no icon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningApp {
    pub display_name: String,
    pub identifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_png: Option<Vec<u8>>,
}
