//! H.264 codec segment: encoder/decoder traits and backends.
//!
//! v1 ships the OpenH264 software backend only (H.264 baseline, no B frames,
//! lowest latency — the approved M3a baseline; hardware encoders attach as
//! additional backend impls later and must fall back to this one).

pub mod openh264;
pub use openh264::{OpenH264Decoder, OpenH264Encoder};

use crate::frame::VideoFrame;

/// One encoded access unit (a single H.264 frame).
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    /// Sender-side frame counter; zero for frames rebuilt across the wire.
    pub index: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub trait VideoEncoder: Send {
    /// Encodes one frame. `force_keyframe` requests an IDR (used by the PLI
    /// path); periodic IDRs are configured on the backend itself.
    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedFrame, crate::VideoError>;
}

pub trait VideoDecoder: Send {
    /// Decodes one access unit; `None` until the stream has enough state —
    /// before the first keyframe, or after any decode failure until the next
    /// keyframe arrives (self-resync; callers treat this as frame skip, not
    /// an error). This is also the hook where a PLI keyframe request fires in
    /// increment ⑤.
    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<VideoFrame>, crate::VideoError>;
}
