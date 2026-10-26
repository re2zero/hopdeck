//! Video pipeline for HopDeck (M3a): capture → encode → packetize →
//! reassemble → decode.
//!
//! Segments mirror the M3a design (`.relay/DESIGN-m3a-video.md`):
//! - [`capture`]: BGRA frame sources (PipeWire portal / DXGI later, dummy now).
//! - [`yuv`]: CPU BGRA↔I420 conversion between capture and codec.
//! - [`codec`]: [`codec::VideoEncoder`]/[`codec::VideoDecoder`] traits with an
//!   OpenH264 software backend (H.264 baseline, no B frames).
//! - [`transport`]: datagram framing `[seq u16][flags u8][ts u32][len u16]`
//!   with payload capped below typical MTU, reassembly with gap detection,
//!   optional XOR parity recovery, and one-way keyframe requests (PLI).
//!
//! The wire format and trait surface are stable targets for the daemon video
//! channel (increment ②); loss recovery and keyframe re-request landed in
//! increment ⑤.

pub mod capture;
pub mod codec;
pub mod error;
pub mod frame;
pub mod transport;
pub mod yuv;

pub use error::VideoError;
pub use frame::VideoFrame;
