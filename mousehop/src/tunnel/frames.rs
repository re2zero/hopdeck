//! Tunnel frame codec: length-prefixed frames multiplexing TCP streams over
//! the tunnel channel.
//!
//! Wire layout (big-endian, matching `mousehop-proto` conventions):
//! `[tag: u8][stream_id: u32][payload_len: u32][payload]`.
//!
//! Frame semantics follow `.relay/DESIGN-m2c-tunnel-forward.md`: OPEN /
//! OPEN_ACK are JSON (small, human-debuggable), DATA / ACK / CLOSE are raw
//! binary. Half-close: CLOSE means "no more data from me" (TCP FIN); a
//! stream is destroyed when both sides closed or either side errors.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Maximum accepted frame payload. Anything larger is a protocol violation
/// and kills the channel rather than risking unbounded allocation.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

/// Largest DATA payload the sender produces. Well under the frame cap so a
/// full chunk always fits a single frame.
pub const MAX_DATA_CHUNK: usize = 16 * 1024;

/// Per-stream credit window: bytes in flight (sent minus acknowledged)
/// before the sender stops reading its local socket.
pub const CREDIT_WINDOW: u64 = 256 * 1024;

/// Per-stream dial timeout for the exit side reaching the real target.
pub const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub const TAG_OPEN: u8 = 1;
pub const TAG_OPEN_ACK: u8 = 2;
pub const TAG_DATA: u8 = 3;
pub const TAG_ACK: u8 = 4;
pub const TAG_CLOSE: u8 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    pub target: String,
    pub port: u16,
    /// Echoed verbatim by OPEN_ACK so the initiator can match the reply to
    /// its pending dial even if the peer reorders work.
    pub nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAck {
    pub ok: bool,
    pub nonce: u64,
    /// Human-readable rejection reason; empty on success. Deliberately
    /// coarse — it must not leak internal details (rule tables, resolver
    /// errors) to the peer.
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Open { stream: u32, open: Open },
    OpenAck { stream: u32, ack: OpenAck },
    Data { stream: u32, data: Vec<u8> },
    Ack { stream: u32, consumed: u32 },
    Close { stream: u32 },
}

impl Frame {
    pub fn stream(&self) -> u32 {
        match self {
            Frame::Open { stream, .. }
            | Frame::OpenAck { stream, .. }
            | Frame::Data { stream, .. }
            | Frame::Ack { stream, .. }
            | Frame::Close { stream } => *stream,
        }
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Frame::Open { stream, open } => {
                write!(f, "OPEN {stream} -> {}:{}", open.target, open.port)
            }
            Frame::OpenAck { stream, ack } => {
                if ack.ok {
                    write!(f, "OPEN_ACK {stream} ok")
                } else {
                    write!(f, "OPEN_ACK {stream} rejected: {}", ack.reason)
                }
            }
            Frame::Data { stream, data } => write!(f, "DATA {stream} {}B", data.len()),
            Frame::Ack { stream, consumed } => write!(f, "ACK {stream} {consumed}"),
            Frame::Close { stream } => write!(f, "CLOSE {stream}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame too large: {0} bytes (cap {MAX_FRAME_PAYLOAD})")]
    TooLarge(usize),
    #[error("unknown frame tag {0}")]
    UnknownTag(u8),
    #[error("malformed frame payload: {0}")]
    Malformed(&'static str),
}

/// Encode a frame into `out` (append).
pub fn encode(frame: &Frame, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let (tag, stream, payload): (u8, u32, Vec<u8>) = match frame {
        Frame::Open { stream, open } => (
            TAG_OPEN,
            *stream,
            serde_json::to_vec(open).map_err(|_| FrameError::Malformed("open"))?,
        ),
        Frame::OpenAck { stream, ack } => (
            TAG_OPEN_ACK,
            *stream,
            serde_json::to_vec(ack).map_err(|_| FrameError::Malformed("open_ack"))?,
        ),
        Frame::Data { stream, data } => (TAG_DATA, *stream, data.clone()),
        Frame::Ack { stream, consumed } => (TAG_ACK, *stream, consumed.to_be_bytes().to_vec()),
        Frame::Close { stream } => (TAG_CLOSE, *stream, Vec::new()),
    };
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(FrameError::TooLarge(payload.len()));
    }
    out.reserve(9 + payload.len());
    out.push(tag);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(())
}

/// Try to decode one frame from `buf`. Returns `Ok(None)` when `buf` holds
/// less than one complete frame; on success the consumed bytes are drained
/// from `buf`.
pub fn decode(buf: &mut Vec<u8>) -> Result<Option<Frame>, FrameError> {
    if buf.len() < 9 {
        return Ok(None);
    }
    let tag = buf[0];
    let stream = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    let len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(FrameError::TooLarge(len));
    }
    if buf.len() < 9 + len {
        return Ok(None);
    }
    let payload = buf[9..9 + len].to_vec();
    buf.drain(..9 + len);
    let frame = match tag {
        TAG_OPEN => Frame::Open {
            stream,
            open: serde_json::from_slice(&payload).map_err(|_| FrameError::Malformed("open"))?,
        },
        TAG_OPEN_ACK => Frame::OpenAck {
            stream,
            ack: serde_json::from_slice(&payload).map_err(|_| FrameError::Malformed("open_ack"))?,
        },
        TAG_DATA => Frame::Data {
            stream,
            data: payload,
        },
        TAG_ACK => {
            if payload.len() != 4 {
                return Err(FrameError::Malformed("ack payload must be 4 bytes"));
            }
            Frame::Ack {
                stream,
                consumed: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
            }
        }
        TAG_CLOSE => Frame::Close { stream },
        other => return Err(FrameError::UnknownTag(other)),
    };
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) {
        let mut buf = Vec::new();
        encode(&frame, &mut buf).expect("encode");
        // Feeding byte-by-byte must yield exactly one frame at the end and
        // never a premature decode.
        let mut acc = Vec::new();
        for byte in buf.iter() {
            acc.push(*byte);
            if acc.len() < 9 {
                assert_eq!(decode(&mut acc).expect("no error"), None);
            }
        }
        let decoded = decode(&mut acc).expect("decode").expect("one full frame");
        assert_eq!(decoded, frame);
        assert!(acc.is_empty(), "no residue after one frame");
    }

    #[test]
    fn open_round_trips() {
        roundtrip(Frame::Open {
            stream: 1,
            open: Open {
                target: "nas.local".into(),
                port: 5000,
                nonce: 0xdead_beef,
            },
        });
    }

    #[test]
    fn open_ack_round_trips() {
        roundtrip(Frame::OpenAck {
            stream: 3,
            ack: OpenAck {
                ok: false,
                nonce: 7,
                reason: "denied by allow rules".into(),
            },
        });
        roundtrip(Frame::OpenAck {
            stream: 4,
            ack: OpenAck {
                ok: true,
                nonce: 9,
                reason: String::new(),
            },
        });
    }

    #[test]
    fn data_ack_close_round_trip() {
        roundtrip(Frame::Data {
            stream: 5,
            data: vec![0u8; MAX_DATA_CHUNK],
        });
        roundtrip(Frame::Ack {
            stream: 5,
            consumed: 16384,
        });
        roundtrip(Frame::Close { stream: 5 });
    }

    #[test]
    fn two_frames_in_one_buffer_decode_in_order() {
        let mut buf = Vec::new();
        encode(
            &Frame::Open {
                stream: 1,
                open: Open {
                    target: "a".into(),
                    port: 1,
                    nonce: 1,
                },
            },
            &mut buf,
        )
        .unwrap();
        encode(&Frame::Close { stream: 2 }, &mut buf).unwrap();
        let first = decode(&mut buf).unwrap().unwrap();
        let second = decode(&mut buf).unwrap().unwrap();
        assert!(matches!(first, Frame::Open { stream: 1, .. }));
        assert!(matches!(second, Frame::Close { stream: 2 }));
        assert!(decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn oversized_len_is_rejected() {
        let mut buf = Vec::new();
        buf.push(TAG_DATA);
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&((MAX_FRAME_PAYLOAD + 1) as u32).to_be_bytes());
        let err = decode(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }

    #[test]
    fn unknown_tag_is_rejected() {
        let mut buf = Vec::new();
        buf.push(99);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        assert!(matches!(decode(&mut buf), Err(FrameError::UnknownTag(99))));
    }
}
