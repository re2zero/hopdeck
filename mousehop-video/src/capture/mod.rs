//! BGRA frame sources.
//!
//! Backends live behind tight cfgs per the workspace convention; the dummy
//! backend is cross-platform and drives every automated test (Xvfb has no
//! portal, mirroring the dummy capture precedent).

use futures_core::Stream;

use crate::frame::VideoFrame;

pub mod dummy;
pub use dummy::{COUNTER_CELLS, DummyVideoSource};

/// A stream of captured BGRA frames.
///
/// Backends: PipeWire portal (Linux), DXGI desktop duplication (Windows),
/// dummy (tests / headless).
pub trait VideoSource: Stream<Item = VideoFrame> + Unpin + Send {}

impl<T: Stream<Item = VideoFrame> + Unpin + Send> VideoSource for T {}
