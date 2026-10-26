# HopDeck Agent Instructions

## Overview

HopDeck（桌联）is an open-source Software KVM sharing mouse/keyboard input across local networks. The Rust workspace combines one GPUI frontend (the control console), CLI/daemon mode, and multi-OS capture/emulation backends for Linux, Windows, and macOS.

## Core principles

- **Scope discipline.** Only implement what was requested; describe follow-up work instead of absorbing it.
- **Clarify OS behavior.** Ask when requirements touch OS-specific capture/emulation (they differ significantly).
- **Docs stay current.** Update [README.md](README.md) or [DOC.md](DOC.md) when touching public APIs or platform support.
- **Rust idioms.** Use `Result`/`Option`, `thiserror` for errors, descriptive logs, and concise comments for non-obvious invariants.

## Terminology

- **Client:** A remote machine that can receive or send input events. Each client is either _active_ (receiving events) or _inactive_ (can send events back). This mutual exclusion prevents feedback loops.
- **Backend:** OS-specific implementation for capture or emulation (e.g., libei, layer-shell, wlroots, X11, Windows, macOS).
- **Handle:** A per-client identifier used to route events and track state (pressed keys, position).

## Workspace layout

Libraries live in `crates/`, shipped executables in `apps/`:

- `apps/hopdeck` — `hopdeck` binary: daemon + CLI + launcher for the console (package `hopdeck`, lib + bin).
- `apps/hopdeck-console` — `hopdeck-console` binary: GPUI control console.
- `crates/hopdeck-input-event` — shared input types, scancodes, and app-identity types (`AppIdent`, `HostKind`, `RunningApp`).
- `crates/hopdeck-input-capture` — OS capture backends.
- `crates/hopdeck-input-emulation` — OS emulation backends.
- `crates/hopdeck-proto` — UDP wire protocol.
- `crates/hopdeck-ipc` — daemon ↔ frontend IPC (JSON over a Unix socket).
- `crates/hopdeck-video` — screen capture, H.264 codec, transport.
- `crates/hopdeck-cli` — CLI frontend library.

No-argument `hopdeck` starts the daemon as a child and runs the sibling
`hopdeck-console` binary; when the console exits, the daemon is stopped.

## Architecture

**Data path:** `input-capture` → `hopdeck` daemon → `hopdeck-proto` (UDP + DTLS) → `hopdeck` daemon → `input-emulation`

**Control path:** frontends (`hopdeck-console`, `hopdeck-cli`) ↔ `hopdeck-ipc` ↔ `hopdeck` daemon

- **input-capture:** Reads OS events into a `Stream<CaptureEvent>`. Backends tried in priority order (libei → layer-shell → X11 → fallback). Tracks `pressed_keys` to avoid stuck modifiers. `position_map` queues events when multiple clients share a screen edge.
- **input-emulation:** Replays events via the `Emulation` trait (`consume`, `create`, `destroy`, `terminate`). Maintains `pressed_keys` and releases them on disconnect.
- **hopdeck-proto:** Wire events; UDP payloads. Connection requests are TCP on the same port. Version bumps required when serialization changes.
- **hopdeck-ipc:** Frontend protocol and socket plumbing. This is not the network wire format.
- **input-event:** Shared scancode enums, abstract event types, and app-identity values—extend here, don't duplicate translations. `input-capture` must never depend on `hopdeck-ipc`; shared domain values belong in `input-event`.

## Feature & cfg discipline

- Feature flags live in root `Cargo.toml`. Gate OS-specific modules with tight cfgs (e.g., `cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))`).
- Prefer module-level gating over per-function cfgs to avoid empty stubs.
- New backends: add feature in `Cargo.toml`, create gated module, log backend selection.

## Async patterns

- Tokio runtime with `futures` streams and `async_trait`. Model new flows as streams or async methods.
- Avoid blocking; use `spawn_blocking` if needed. Prefer existing single-threaded stream handling.
- `InputCapture` implements `Stream` and manually pumps backends—don't short-circuit this logic.

## Commands

```sh
cargo build                                                # daemon + CLI (workspace default-members)
cargo build -p hopdeck-console                             # GPUI console (separate invocation: ashpd feature conflict)
cargo build -p hopdeck                                     # one crate
cargo test --workspace --exclude hopdeck-console           # daemon-side tests
cargo test -p hopdeck-console                              # console tests
cargo fmt && cargo clippy --workspace --exclude hopdeck-console --all-targets --all-features
cargo clippy -p hopdeck-console --all-targets --all-features
RUST_LOG=hopdeck=debug cargo run -p hopdeck                # debug logging
cargo test -p hopdeck-console --features ui-test-support   # console UI tests
```

The console and daemon cannot share one cargo invocation: gpui enables
ashpd's `async-io` feature, while the daemon's libei/portal backends enable
ashpd's `tokio` feature and ashpd rejects both at once. This is why the
console is not a default workspace member.

Run from repo root—no `cd` in scripts.

Headless environments without X11/Wayland development libraries cannot build
the full workspace. For daemon-only verification use:

```sh
cargo test -p hopdeck --no-default-features --features video
cargo clippy -p hopdeck --no-default-features --features video --all-targets
```

## Testing

- Unit tests for utilities; integration tests for protocol behavior.
- OS-specific backends: test via the console/CLI on target OS or document manual verification.
- Dummy backend exercises pipeline without real dependencies.
- Verify `terminate()` releases keys on unexpected disconnect.
- GPUI UI tests need `--features ui-test-support` and an `.id(...)` element
  must also call `.test_support()` to enter the headless registry.

## Workflow

1. Clarify ambiguous requirements, especially OS-specific behavior.
2. Implement minimal change; flag follow-up work.
3. Add proportional tests; run `cargo test` on affected crates.
4. Run `cargo fmt`, then clippy and tests as two invocations (daemon workspace
   with `--exclude hopdeck-console`, then the console).
