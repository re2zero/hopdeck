//! Reverse tunnel forwarding (M2c): carry local TCP connections from the
//! restricted side through the session peer and out to destinations only
//! the peer can reach.
//!
//! Design: `.relay/DESIGN-m2c-tunnel-forward.md`. Transport is a dedicated
//! TCP channel (default port 4253) with mutual TLS reusing the session
//! certificate; frames are multiplexed per stream with a credit-window
//! flow control. The exit side enforces an allow-list of targets (IP /
//! CIDR / domain patterns — domains resolve at the exit) and only admits
//! channels whose certificate matches a live input session.

mod channel;
mod exit;
mod forward;
mod frames;
mod rules;
mod tls;

pub use exit::{ExitConfig, spawn as spawn_exit};
pub use forward::{ForwardSpec, TargetAddr, spawn as spawn_forward};
pub use rules::AllowList;

/// Default TCP port of the tunnel channel (see `firewall/hopdeck.xml`).
pub const DEFAULT_TUNNEL_PORT: u16 = 4253;
