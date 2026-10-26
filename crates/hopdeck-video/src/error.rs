use thiserror::Error;

#[derive(Debug, Error)]
pub enum VideoError {
    #[error("frame buffer is {actual} bytes, expected {expected} for {width}x{height} BGRA")]
    FrameSizeMismatch {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },

    #[error("packet payload is {actual} bytes, header declares {declared}")]
    PacketLengthMismatch { declared: usize, actual: usize },

    #[error("packet payload of {0} bytes exceeds the {1} byte limit")]
    PacketTooLarge(usize, usize),

    #[error("malformed parity packet: {0}")]
    Parity(String),

    #[error("codec failure: {0}")]
    Codec(String),

    #[error("portal screencast failure: {0}")]
    Portal(String),

    #[error("pipewire capture failure: {0}")]
    PipeWire(String),

    #[error(transparent)]
    Openh264(#[from] openh264::Error),
}
