//! Desktop integration for a `cargo install`ed HopDeck.
//!
//! `cargo install` places only the executable — no icon, no `.desktop`
//! entry — so a crates.io install wouldn't appear in launchers or file
//! managers. This module writes those files into the user's XDG data
//! dir (the same files an AUR / Flatpak install drops system-wide,
//! just user-local).
//!
//! [`ensure_first_launch`] runs silently and one-shot on the first
//! normal launch, so the integration appears with no command for the
//! user to remember. Best-effort throughout — desktop-file housekeeping
//! must never break app startup.
//!
//! Linux-only; macOS uses `.app` bundles (`cargo bundle --release`)
//! and Windows uses the Start Menu (out of scope).

use std::io;
use std::path::PathBuf;

/// App icon and desktop entry, embedded into the binary so the install
/// works from a `cargo install`ed binary with no repo checkout present.
/// `assets/hopdeck.svg` is the single icon source; packaging (Flatpak,
/// Nix, release tarballs) installs the same file.
const ICON_SVG: &[u8] = include_bytes!("../assets/hopdeck.svg");
const DESKTOP_ENTRY: &str = include_str!("../hopdeck.desktop");

/// Basename of the installed desktop entry and icon. Deliberately simple
/// so launchers resolve `Icon=hopdeck` without reverse-DNS parsing; the
/// macOS bundle/libei app id stays `com.hopdeck.HopDeck`.
const DESKTOP_ID: &str = "hopdeck";
const ICON_NAME: &str = "hopdeck";

/// Pre-0.18 reverse-DNS names. Older builds wrote a user-level copy with
/// these names, which would shadow the packaged `hopdeck.desktop`.
const LEGACY_DESKTOP_ID: &str = "com.hopdeck.HopDeck";

/// Binary name written into the desktop entry's `Exec=` line. Rewritten
/// to the running executable's absolute path so a launcher's PATH-less
/// environment can still find it.
const BINARY_NAME: &str = "hopdeck";

/// `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`.
fn xdg_data_home() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither XDG_DATA_HOME nor HOME is set",
        )
    })?;
    Ok(PathBuf::from(home).join(".local/share"))
}

/// `$XDG_STATE_HOME`, falling back to `$HOME/.local/state`.
fn xdg_state_home() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither XDG_STATE_HOME nor HOME is set",
        )
    })?;
    Ok(PathBuf::from(home).join(".local/state"))
}

/// Write the icon + desktop entry into the user's XDG data dir.
fn install() -> io::Result<()> {
    let data = xdg_data_home()?;

    // Icon → icons/hicolor/scalable/apps/hopdeck.svg, so the desktop
    // entry's `Icon=hopdeck` resolves by name.
    let icon_dir = data.join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icon_dir)?;
    let icon_path = icon_dir.join(format!("{ICON_NAME}.svg"));
    std::fs::write(&icon_path, ICON_SVG)?;

    // Desktop entry → applications/hopdeck.desktop, with `Exec=` rewritten
    // to this binary's absolute path: `cargo install` drops it in
    // ~/.cargo/bin, which a launcher's environment may not have on PATH.
    let app_dir = data.join("applications");
    std::fs::create_dir_all(&app_dir)?;
    let exe = std::env::current_exe()?.display().to_string();
    let entry = DESKTOP_ENTRY.replace(&format!("Exec={BINARY_NAME}\n"), &format!("Exec={exe}\n"));
    let entry_path = app_dir.join(format!("{DESKTOP_ID}.desktop"));
    std::fs::write(&entry_path, entry)?;

    Ok(())
}

/// Does a system XDG data dir already provide our desktop entry? An
/// AUR / Flatpak / distro package drops it under, typically,
/// `/usr/share/applications` — in which case a user-local copy would
/// only shadow it.
fn packaged_entry_exists() -> bool {
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    std::env::split_paths(&dirs).any(|d| {
        d.join(format!("applications/{DESKTOP_ID}.desktop"))
            .is_file()
    })
}

/// Remove the old reverse-DNS user-level entry/icon written by earlier
/// builds. Runs on every launch (not just the first) so an upgrade from
/// an older binary clears the shadowing copy even though its install
/// marker already exists. Best-effort: a packaged install may also
/// clean these via its postinst.
fn cleanup_legacy() {
    let Ok(data) = xdg_data_home() else {
        return;
    };
    let _ = std::fs::remove_file(data.join(format!("applications/{LEGACY_DESKTOP_ID}.desktop")));
    let _ = std::fs::remove_file(data.join(format!(
        "icons/hicolor/scalable/apps/{LEGACY_DESKTOP_ID}.svg"
    )));
}

/// Install the desktop integration silently on the first normal launch,
/// so a `cargo install`ed HopDeck shows up in launchers without the
/// user knowing to run an extra command.
///
/// One-shot: a marker in the XDG state dir records that the
/// first-launch step has run, so it never repeats — not even if the
/// user later removes the entry on purpose. Flatpak / AUR / distro
/// installs already ship an entry and are skipped. Best-effort: any
/// failure is swallowed and left to retry on the next launch.
pub fn ensure_first_launch() {
    let _ = try_ensure_first_launch();
}

fn try_ensure_first_launch() -> io::Result<()> {
    // Inside a Flatpak the runtime ships the entry, and the sandboxed
    // XDG dirs make a user-local copy pointless either way.
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Ok(());
    }

    // Remove a pre-rename user copy even when the install marker below
    // says the one-time step already ran.
    cleanup_legacy();

    // The marker means the one-time first-launch step is already done.
    let state = xdg_state_home()?;
    let marker_dir = state.join(env!("CARGO_PKG_NAME"));
    let marker = marker_dir.join("desktop-install-done");
    if marker.exists() {
        return Ok(());
    }

    // Install only if nothing already provides the entry — neither an
    // earlier run of this nor a system package.
    let user_entry = xdg_data_home()?.join(format!("applications/{DESKTOP_ID}.desktop"));
    if !user_entry.exists() && !packaged_entry_exists() {
        install()?;
    }

    // Record completion last, so a failed install above is retried on
    // the next launch rather than marked done.
    std::fs::create_dir_all(&marker_dir)?;
    std::fs::write(
        &marker,
        "HopDeck ran its one-time first-launch desktop integration.\n",
    )?;
    Ok(())
}
