# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Renamed the project from Mousehop to HopDeck（桌联）. Crate names, the
  binary, app id (`com.hopdeck.HopDeck`), desktop/icon/service names,
  config and state paths, IPC/video socket names, environment variables
  (`HOPDECK_*`), and the mDNS service type (`_hopdeck._udp.local.`) all
  moved. Ports and event serialization are unchanged, but the `Hello`
  protocol magic is now `HOPDECK\0`, so Mousehop peers are rejected
  (deliberate hard break).
- Restructured the workspace: libraries live under `crates/`, shipped
  executables under `apps/`. Application identity types moved to
  `hopdeck-input-event`, removing the capture layer's dependency on the
  frontend IPC crate.

## [0.11.3](https://github.com/jondkinney/mousehop/compare/v0.11.2...v0.11.3) - 2026-05-20

### Added

- *(desktop)* install desktop entry on first launch
- *(tray)* recolor tray glyph at runtime via currentColor

## [0.11.2] - 2026-05-19

### Fixed

- Flatpak icon validation: GdkPixbuf's SVG sniffer reads only the first
  ~256 bytes looking for the `<svg` tag, and the multi-line XML
  docstring above the root element pushed it past that window. Moved
  the docstring inside `<svg>` so Flatpak's icon validator accepts the
  app icon and the Flatpak bundle exports cleanly.

## [0.11.1] - 2026-05-19

### Fixed

- Typo "occured" → "occurred" in the `IpcError::Io` thiserror message
  (user-visible in logs and CLI output), a doc comment on
  `FrontendEvent::Error`, and the `capture_event_occured` local
  variable in `input-capture`'s libei capture loop.
