//! Datagram framing for the video channel (M3a design §6).
//!
//! Wire format, big endian: `[seq u16][flags u8][ts u32][len u16][payload]`.
//! One encoded frame is split into fragments of at most [`DEFAULT_MTU`]
//! payload bytes (below typical MTU to avoid black-hole fragmentation); the
//! first and last fragment carry [`FLAG_FIRST`]/[`FLAG_LAST`], the frame's
//! keyframe-ness rides on [`FLAG_KEY`]. Sequence numbers wrap at 65536.
//!
//! Reassembly is deliberately order-strict with no jitter buffer: on any
//! sequence gap the pending frame is dropped and the stream resyncs at the
//! next `FLAG_FIRST` packet (later: a keyframe request). This is the LAN
//! trade-off recorded in the design — no NACK retransmission station.

pub const DEFAULT_MTU: usize = 1200;
pub const PACKET_HEADER_LEN: usize = 2 + 1 + 4 + 2;

pub const FLAG_FIRST: u8 = 1 << 0;
pub const FLAG_LAST: u8 = 1 << 1;
pub const FLAG_KEY: u8 = 1 << 2;
/// Reserved for the optional XOR parity scheme (design §6, increment ⑤).
pub const FLAG_PARITY: u8 = 1 << 3;

use crate::VideoError;
use crate::codec::EncodedFrame;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub seq: u16,
    pub flags: u8,
    /// Sender media timestamp in milliseconds (wraps at u32::MAX).
    pub ts_ms: u32,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn to_bytes(&self, dst: &mut Vec<u8>) -> Result<(), VideoError> {
        if self.payload.len() > u16::MAX as usize {
            return Err(VideoError::PacketTooLarge(
                self.payload.len(),
                u16::MAX as usize,
            ));
        }
        dst.extend_from_slice(&self.seq.to_be_bytes());
        dst.push(self.flags);
        dst.extend_from_slice(&self.ts_ms.to_be_bytes());
        dst.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        dst.extend_from_slice(&self.payload);
        Ok(())
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, VideoError> {
        let mut dst = Vec::with_capacity(PACKET_HEADER_LEN + self.payload.len());
        self.to_bytes(&mut dst)?;
        Ok(dst)
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, VideoError> {
        if buf.len() < PACKET_HEADER_LEN {
            return Err(VideoError::PacketLengthMismatch {
                declared: PACKET_HEADER_LEN,
                actual: buf.len(),
            });
        }
        let declared = u16::from_be_bytes([buf[7], buf[8]]) as usize;
        let payload = &buf[PACKET_HEADER_LEN..];
        if payload.len() != declared {
            return Err(VideoError::PacketLengthMismatch {
                declared,
                actual: payload.len(),
            });
        }
        Ok(Self {
            seq: u16::from_be_bytes([buf[0], buf[1]]),
            flags: buf[2],
            ts_ms: u32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]),
            payload: payload.to_vec(),
        })
    }
}

/// Sender-side sequence number allocator; one per video stream.
#[derive(Debug, Default)]
pub struct Packetizer {
    next_seq: u16,
}

impl Packetizer {
    pub fn new() -> Self {
        Self { next_seq: 0 }
    }

    /// Splits one access unit into packets. A zero-length frame still emits
    /// a single FIRST|LAST packet so sequence accounting stays exact.
    pub fn packetize(
        &mut self,
        frame: &EncodedFrame,
        ts_ms: u32,
        mtu: usize,
    ) -> Result<Vec<Packet>, VideoError> {
        if mtu < 1 {
            return Err(VideoError::PacketTooLarge(usize::MAX, 0));
        }
        let mut flags = 0u8;
        if frame.keyframe {
            flags |= FLAG_KEY;
        }
        let chunks: Vec<&[u8]> = if frame.data.is_empty() {
            vec![&[]]
        } else {
            frame.data.chunks(mtu).collect()
        };
        let mut packets = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            let mut f = flags;
            if i == 0 {
                f |= FLAG_FIRST;
            }
            if i == chunks.len() - 1 {
                f |= FLAG_LAST;
            }
            packets.push(Packet {
                seq: self.next_seq,
                flags: f,
                ts_ms,
                payload: chunk.to_vec(),
            });
            self.next_seq = self.next_seq.wrapping_add(1);
        }
        Ok(packets)
    }
}

/// Receiver-side reassembly. Feeds every arriving packet; complete frames
/// come out in sender order.
#[derive(Debug, Default)]
pub struct Reassembler {
    next_seq: u16,
    fragments: Vec<Vec<u8>>,
    ts_ms: u32,
    keyframe: bool,
    /// Armed on the first `FLAG_FIRST` seen; before that, arbitrary packets
    /// (mid-frame on join) are ignored.
    armed: bool,
    /// True between a seen FIRST and its LAST; stray mid-frame packets
    /// outside a frame are ignored rather than reassembled.
    in_frame: bool,
    lost_frames: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lost_frames(&self) -> u64 {
        self.lost_frames
    }

    /// Returns `Some` when this packet completed a frame.
    pub fn push(&mut self, pkt: Packet) -> Result<Option<EncodedFrame>, VideoError> {
        if !self.armed {
            if pkt.flags & FLAG_FIRST == 0 {
                return Ok(None);
            }
            self.armed = true;
            self.next_seq = pkt.seq;
        } else if pkt.seq != self.next_seq {
            // Gap: whatever frame was in flight is unrecoverable.
            if self.in_frame {
                self.lost_frames += 1;
                self.fragments.clear();
                self.in_frame = false;
            }
            self.next_seq = pkt.seq;
            if pkt.flags & FLAG_FIRST == 0 {
                return Ok(None);
            }
        }

        self.next_seq = self.next_seq.wrapping_add(1);
        if pkt.flags & FLAG_FIRST != 0 {
            // Defensive: a FIRST while a frame is open means the previous
            // frame ended without the LAST we were holding.
            if self.in_frame {
                self.lost_frames += 1;
                self.fragments.clear();
            }
            self.in_frame = true;
            self.ts_ms = pkt.ts_ms;
            self.keyframe = pkt.flags & FLAG_KEY != 0;
        } else if !self.in_frame {
            // Post-gap resync packet or stray tail: not reassemblable.
            return Ok(None);
        }
        self.fragments.push(pkt.payload);

        if pkt.flags & FLAG_LAST != 0 {
            self.in_frame = false;
            let data = std::mem::take(&mut self.fragments)
                .into_iter()
                .flatten()
                .collect();
            return Ok(Some(EncodedFrame {
                index: 0,
                keyframe: self.keyframe,
                data,
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(index: u64, keyframe: bool, size: usize) -> EncodedFrame {
        EncodedFrame {
            index,
            keyframe,
            data: (0..size).map(|i| (i % 251) as u8).collect(),
        }
    }

    fn push_all(re: &mut Reassembler, packets: &[Packet]) -> Vec<EncodedFrame> {
        packets
            .iter()
            .filter_map(|p| re.push(p.clone()).unwrap())
            .collect()
    }

    #[test]
    fn packet_roundtrip() {
        let pkt = Packet {
            seq: 65535,
            flags: FLAG_FIRST | FLAG_KEY,
            ts_ms: 0xdeadbeef,
            payload: vec![1, 2, 3],
        };
        let parsed = Packet::from_bytes(&pkt.to_vec().unwrap()).unwrap();
        assert_eq!(pkt, parsed);
    }

    #[test]
    fn truncated_and_overlong_packets_rejected() {
        let pkt = Packet {
            seq: 0,
            flags: 0,
            ts_ms: 0,
            payload: vec![7; 10],
        };
        let mut bytes = pkt.to_vec().unwrap();
        assert!(Packet::from_bytes(&bytes[..5]).is_err());
        bytes.push(0);
        assert!(Packet::from_bytes(&bytes).is_err());
    }

    #[test]
    fn single_and_multi_fragment_frames() {
        let mut pz = Packetizer::new();
        let packets = pz.packetize(&frame(0, true, 100), 5, DEFAULT_MTU).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].flags, FLAG_FIRST | FLAG_LAST | FLAG_KEY);

        let packets = pz
            .packetize(&frame(1, false, DEFAULT_MTU * 2 + 7), 6, DEFAULT_MTU)
            .unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].flags, FLAG_FIRST);
        assert_eq!(packets[1].flags, 0);
        assert_eq!(packets[2].flags, FLAG_LAST);
        assert_eq!(packets[0].seq, 1);
        assert_eq!(packets[2].seq, 3);
    }

    #[test]
    fn reassembly_in_order() {
        let mut pz = Packetizer::new();
        let mut re = Reassembler::new();
        let packets = pz
            .packetize(&frame(0, true, DEFAULT_MTU + 100), 9, DEFAULT_MTU)
            .unwrap();
        let out = push_all(&mut re, &packets);
        assert_eq!(out.len(), 1);
        assert!(out[0].keyframe);
        assert_eq!(out[0].data.len(), DEFAULT_MTU + 100);
        assert_eq!(re.lost_frames(), 0);
    }

    #[test]
    fn seq_wraps_at_65536() {
        let mut pz = Packetizer { next_seq: 65534 };
        let packets = pz
            .packetize(&frame(0, false, 2500), 0, DEFAULT_MTU)
            .unwrap();
        assert_eq!(packets[0].seq, 65534);
        assert_eq!(packets[1].seq, 65535);
        assert_eq!(packets[2].seq, 0);

        let mut re = Reassembler::new();
        let out = push_all(&mut re, &packets);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn gap_drops_pending_and_resyncs() {
        let mut pz = Packetizer::new();
        let mut re = Reassembler::new();

        let f1 = pz.packetize(&frame(0, true, 3000), 0, DEFAULT_MTU).unwrap();
        let f2 = pz
            .packetize(&frame(1, false, 3000), 1, DEFAULT_MTU)
            .unwrap();

        // Frame 1 arrives complete.
        assert_eq!(push_all(&mut re, &f1).len(), 1);
        // Frame 2 loses its first packet; the rest cannot start a frame.
        for p in &f2[1..] {
            assert!(re.push(p.clone()).unwrap().is_none());
        }
        assert_eq!(re.lost_frames(), 0, "no pending frame yet, nothing counted");

        let f3 = pz
            .packetize(&frame(2, false, 3000), 2, DEFAULT_MTU)
            .unwrap();
        // Frame 3's packets arrive after the gap; until its LAST they hold no
        // complete frame — and seq jumps are tolerated as resync.
        let out = push_all(&mut re, &f3);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data.len(), 3000);
        assert_eq!(re.lost_frames(), 0);
    }

    #[test]
    fn mid_stream_join_ignores_stray_packets() {
        let mut pz = Packetizer::new();
        let packets = pz
            .packetize(&frame(0, false, 3000), 0, DEFAULT_MTU)
            .unwrap();
        let mut re = Reassembler::new();
        for p in &packets[1..] {
            assert!(re.push(p.clone()).unwrap().is_none());
        }
        assert!(re.fragments.is_empty());
        // Next FIRST arms the receiver.
        let next = pz.packetize(&frame(1, true, 1), 1, DEFAULT_MTU).unwrap();
        assert_eq!(push_all(&mut re, &next).len(), 1);
    }

    #[test]
    fn first_without_last_counts_as_loss() {
        let mut pz = Packetizer::new();
        let mut re = Reassembler::new();
        let f1 = pz
            .packetize(&frame(0, false, 3000), 0, DEFAULT_MTU)
            .unwrap();
        // First fragment of f1 lands, then a NEW frame starts (sender gave up
        // on f1 without telling us — defensive path).
        re.push(f1[0].clone()).unwrap();
        let f2 = pz.packetize(&frame(1, false, 100), 1, DEFAULT_MTU).unwrap();
        let out = push_all(&mut re, &f2);
        assert_eq!(out.len(), 1);
        assert_eq!(re.lost_frames(), 1);
    }
}
