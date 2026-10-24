/// Uncompressed BGRA8 frame, tightly packed (`width * height * 4` bytes,
/// byte order `[B, G, R, A]`).
///
/// This is the canonical interchange format of the pipeline: capture backends
/// produce it and [`crate::codec::VideoDecoder`] hands it to the renderer in
/// the same layout ely's `RenderSurface` draw closure expects.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// Producer-side frame counter. Sources stamp their sequence number;
    /// decoded frames carry the counter of the `EncodedFrame` they came from
    /// (zero for frames rebuilt across the wire, which carries no index).
    pub index: u64,
    pub bgra: Vec<u8>,
}

impl VideoFrame {
    pub fn new(
        width: u32,
        height: u32,
        index: u64,
        bgra: Vec<u8>,
    ) -> Result<Self, crate::VideoError> {
        let expected = width as usize * height as usize * 4;
        if bgra.len() != expected {
            return Err(crate::VideoError::FrameSizeMismatch {
                width,
                height,
                expected,
                actual: bgra.len(),
            });
        }
        Ok(Self {
            width,
            height,
            index,
            bgra,
        })
    }
}
