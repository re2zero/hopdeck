//! Datagram framing for the video channel (M3a design §6).
//!
//! Wire format, big endian: `[seq u16][flags u8][ts u32][len u16][payload]`.
//! One encoded frame is split into fragments of at most [`DEFAULT_MTU`]
//! payload bytes (below typical MTU to avoid black-hole fragmentation); the
//! first and last fragment carry [`FLAG_FIRST`]/[`FLAG_LAST`], the frame's
//! keyframe-ness rides on [`FLAG_KEY`]. Sequence numbers wrap at 65536 and
//! count data packets only — parity packets are sideband (they consume no
//! seq, so a lost parity packet is a silently lost recovery chance rather
//! than a phantom data gap).
//!
//! Loss handling (design §6, increment ⑤):
//!
//! - **Keyframe request (PLI)**: the receiver counts unrecoverable loss
//!   ([`Reassembler::keyframe_requests`]) and answers with a one-way
//!   [`FLAG_PLI`] packet on the same channel; the sender forces an IDR on
//!   its next frame. Fire-and-forget — no NACK retransmission station.
//! - **Optional XOR parity** ([`Packetizer::with_parity`], off by
//!   default): every *N* data packets are followed by one [`FLAG_PARITY`]
//!   packet carrying the XOR of the group plus each member's length and
//!   flags, so a single lost packet is reconstructed exactly instead of
//!   dropping the stream to the next keyframe. Recovery engages once the
//!   first parity packet has been seen; a group's parity follows its
//!   members, so the very first group rides unprotected.
//!
//! Reassembly is order-strict with no jitter buffer: in-order packets are
//! assembled at once, packets ahead of the delivery point are held until
//! the gap closes — by the missing packet arriving, parity rebuilding it,
//! or a resync. Resync jumps to the next held `FLAG_FIRST` (dropping what
//! the gap destroyed) and raises [`Reassembler::keyframe_requests`] so the
//! caller asks the sender for a fresh IDR.

use std::collections::{BTreeMap, VecDeque};

pub const DEFAULT_MTU: usize = 1200;
pub const PACKET_HEADER_LEN: usize = 2 + 1 + 4 + 2;

pub const FLAG_FIRST: u8 = 1 << 0;
pub const FLAG_LAST: u8 = 1 << 1;
pub const FLAG_KEY: u8 = 1 << 2;
/// XOR parity packet (see [`Packetizer::with_parity`]).
pub const FLAG_PARITY: u8 = 1 << 3;
/// Reverse-direction keyframe request (PLI semantics, design §6): a
/// zero-payload packet the *receiver* sends when it cannot go on without
/// an IDR. The data path must ignore it.
pub const FLAG_PLI: u8 = 1 << 4;

/// Flags that can appear on a data packet.
const FLAG_DATA_MASK: u8 = FLAG_FIRST | FLAG_LAST | FLAG_KEY;

/// Default XOR parity group size (`[video] parity`, design §6).
pub const PARITY_GROUP_SIZE: usize = 8;
/// Parity groups above this are clamped, keeping the parity header and the
/// receiver-side recovery window bounded.
pub const MAX_PARITY_GROUP: usize = 64;

/// Parity payload: `[group_first u16][count u8][xor_len u16]`, then
/// `count` × `[member_len u16][member_flags u8]`, then the XOR blob of
/// `xor_len` bytes (members zero-padded to the longest).
const PARITY_HEADER_LEN: usize = 5;

/// Distances below half the sequence space mean "ahead of the delivery
/// point"; anything else is a late duplicate or pre-resync leftover.
const SEQ_HALF: u16 = 32768;
/// Out-of-order packets held while a gap waits for parity. Bounds memory
/// when a gap can never close; past this the reassembler force-resyncs.
const PENDING_CAP: usize = 128;
/// Delivered packets kept around so parity can XOR a group straddling the
/// delivery point back together (needs up to `group - 1` of them).
const RECENT_CAP: usize = MAX_PARITY_GROUP;

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
    /// The one-way keyframe request the receive side sends on
    /// unrecoverable loss (design §6: PLI without a NACK station).
    pub fn keyframe_request() -> Self {
        Self {
            seq: 0,
            flags: FLAG_PLI,
            ts_ms: 0,
            payload: Vec::new(),
        }
    }

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

/// Sender-side sequence number allocator; one per video stream. With
/// parity enabled, every `n` data packets are followed by an XOR parity
/// packet covering them (sideband — see the module docs).
#[derive(Debug, Default)]
pub struct Packetizer {
    next_seq: u16,
    parity_group: Option<usize>,
    /// Seq of the open group's first member.
    group_start: u16,
    /// (flags, payload) of the open group's members so far.
    group: Vec<(u8, Vec<u8>)>,
}

impl Packetizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit an XOR parity packet after every `group` data packets, making
    /// any single lost packet recoverable (design §6). `0` disables;
    /// values above [`MAX_PARITY_GROUP`] are clamped. Receivers without
    /// this increment ignore the parity packets' flag and misread the
    /// stream — enable only against a same-release peer.
    pub fn with_parity(group: usize) -> Self {
        let parity_group = match group {
            0 => None,
            n => Some(n.min(MAX_PARITY_GROUP)),
        };
        Self {
            parity_group,
            ..Self::default()
        }
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
            let seq = self.next_seq;
            let payload = chunk.to_vec();
            packets.push(Packet {
                seq,
                flags: f,
                ts_ms,
                payload: payload.clone(),
            });
            self.next_seq = self.next_seq.wrapping_add(1);
            if let Some(size) = self.parity_group {
                if self.group.is_empty() {
                    self.group_start = seq;
                }
                self.group.push((f & FLAG_DATA_MASK, payload));
                if self.group.len() == size {
                    packets.push(self.parity_packet(ts_ms));
                }
            }
        }
        Ok(packets)
    }

    /// Seals the open group: XOR the members (zero-padded to the longest)
    /// with their lengths and flags in the payload so the receiver can
    /// rebuild any single missing member exactly.
    fn parity_packet(&mut self, ts_ms: u32) -> Packet {
        let count = self.group.len();
        let xor_len = self.group.iter().map(|(_, p)| p.len()).max().unwrap_or(0);
        let mut blob = vec![0u8; xor_len];
        for (_, payload) in &self.group {
            for (byte, xor) in payload.iter().zip(blob.iter_mut()) {
                *xor ^= byte;
            }
        }
        let mut payload = Vec::with_capacity(PARITY_HEADER_LEN + count * 3 + xor_len);
        payload.extend_from_slice(&self.group_start.to_be_bytes());
        payload.push(count as u8);
        payload.extend_from_slice(&(xor_len as u16).to_be_bytes());
        for (flags, member) in &self.group {
            payload.extend_from_slice(&(member.len() as u16).to_be_bytes());
            payload.push(*flags);
        }
        payload.extend_from_slice(&blob);
        self.group.clear();
        Packet {
            seq: 0,
            flags: FLAG_PARITY,
            ts_ms,
            payload,
        }
    }
}

/// A parsed parity payload: the group's first seq, each member's length
/// and flags, and the XOR blob.
struct ParityGroup<'a> {
    first: u16,
    lens: Vec<u16>,
    flags: Vec<u8>,
    blob: &'a [u8],
}

/// Parses a parity payload (see [`Packetizer::parity_packet`]).
fn parse_parity(payload: &[u8]) -> Result<ParityGroup<'_>, VideoError> {
    if payload.len() < PARITY_HEADER_LEN {
        return Err(VideoError::Parity(format!(
            "payload of {} bytes is shorter than the {PARITY_HEADER_LEN} byte header",
            payload.len()
        )));
    }
    let first = u16::from_be_bytes([payload[0], payload[1]]);
    let count = payload[2] as usize;
    let xor_len = u16::from_be_bytes([payload[3], payload[4]]) as usize;
    if count == 0 || payload.len() != PARITY_HEADER_LEN + count * 3 + xor_len {
        return Err(VideoError::Parity(format!(
            "declares {count} members and a {xor_len} byte blob but holds {} bytes",
            payload.len()
        )));
    }
    let mut lens = Vec::with_capacity(count);
    let mut flags = Vec::with_capacity(count);
    for i in 0..count {
        let base = PARITY_HEADER_LEN + i * 3;
        lens.push(u16::from_be_bytes([payload[base], payload[base + 1]]));
        flags.push(payload[base + 2]);
    }
    if lens.iter().any(|&len| len as usize > xor_len) {
        return Err(VideoError::Parity(
            "a member is longer than the XOR blob".into(),
        ));
    }
    let blob = &payload[PARITY_HEADER_LEN + count * 3..];
    Ok(ParityGroup {
        first,
        lens,
        flags,
        blob,
    })
}

/// Receiver-side reassembly with parity recovery and keyframe requests
/// (increment ⑤).
///
/// Delivery model: packets are consumed strictly in seq order. The
/// in-order packet is assembled immediately; anything ahead of the
/// delivery point waits in `pending` until the gap closes — by the missing
/// packet arriving, parity rebuilding it, or a resync to the next held
/// `FLAG_FIRST`. `recent` keeps the last delivered packets so parity can
/// reconstruct a group straddling the delivery point.
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
    /// Out-of-order data packets awaiting the gap to close, keyed by seq.
    pending: BTreeMap<u16, Packet>,
    /// Last delivered data packets, kept for parity recovery of groups
    /// that start before the delivery point.
    recent: VecDeque<Packet>,
    /// A FLAG_PARITY packet has been seen — the stream carries parity.
    parity_seen: bool,
    /// The gap at `next_seq` is confirmed unrecoverable: resync at the
    /// next held frame start.
    gap_armed: bool,
    keyframe_requests: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lost_frames(&self) -> u64 {
        self.lost_frames
    }

    /// Monotonic count of unrecoverable-loss events. Each new value is the
    /// cue to send a [`Packet::keyframe_request`] to the sender.
    pub fn keyframe_requests(&self) -> u64 {
        self.keyframe_requests
    }

    /// Feeds one packet; returns every frame this completed (a parity
    /// recovery can finish several at once).
    pub fn push(&mut self, pkt: Packet) -> Result<Vec<EncodedFrame>, VideoError> {
        if pkt.flags & FLAG_PLI != 0 {
            // Reverse-direction signal; the data path never acts on it.
            return Ok(Vec::new());
        }
        if pkt.flags & FLAG_PARITY != 0 {
            self.parity_seen = true;
            if self.armed {
                self.on_parity(&pkt.payload)?;
                self.maybe_resync();
            }
            return Ok(self.drain());
        }
        if !self.armed {
            if pkt.flags & FLAG_FIRST == 0 {
                return Ok(Vec::new());
            }
            self.armed = true;
            self.next_seq = pkt.seq;
        } else if pkt.seq != self.next_seq {
            if pkt.seq.wrapping_sub(self.next_seq) >= SEQ_HALF {
                // Behind the delivery point: a late duplicate or a
                // leftover from before a resync.
                return Ok(Vec::new());
            }
            if !self.parity_seen {
                // A parity-less stream never closes this gap on its own.
                self.gap_armed = true;
            }
            self.pending.insert(pkt.seq, pkt);
            self.maybe_resync();
            return Ok(self.drain());
        }

        let mut out = Vec::new();
        if let Some(frame) = self.deliver(pkt) {
            out.push(frame);
        }
        out.extend(self.drain());
        Ok(out)
    }

    /// Assembles one in-order packet into the open frame.
    fn deliver(&mut self, pkt: Packet) -> Option<EncodedFrame> {
        self.gap_armed = false;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.parity_seen {
            self.recent.push_back(pkt.clone());
            while self.recent.len() > RECENT_CAP {
                self.recent.pop_front();
            }
        }
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
            return None;
        }
        self.fragments.push(pkt.payload);

        if pkt.flags & FLAG_LAST != 0 {
            self.in_frame = false;
            let data = std::mem::take(&mut self.fragments)
                .into_iter()
                .flatten()
                .collect();
            return Some(EncodedFrame {
                index: 0,
                keyframe: self.keyframe,
                data,
            });
        }
        None
    }

    /// Delivers every pending packet that is now exactly in order.
    fn drain(&mut self) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        while let Some(pkt) = self.pending.remove(&self.next_seq) {
            if let Some(frame) = self.deliver(pkt) {
                out.push(frame);
            }
        }
        out
    }

    /// Reconstructs a single missing member of a parity group, or records
    /// that the covered gap is unrecoverable.
    fn on_parity(&mut self, payload: &[u8]) -> Result<(), VideoError> {
        let group = parse_parity(payload)?;
        let (first, lens, flags, blob) = (group.first, &group.lens, &group.flags, group.blob);
        let offset = self.next_seq.wrapping_sub(first);
        let covers = offset < lens.len() as u16;
        let ahead = first.wrapping_sub(self.next_seq) < SEQ_HALF;
        if !covers {
            if ahead && !self.pending.is_empty() {
                // A later group's parity: the group covering the open gap
                // has already sent (and evidently lost) its own.
                self.gap_armed = true;
                self.keyframe_requests += 1;
            }
            return Ok(());
        }
        // The group spans the delivery point; every not-yet-delivered
        // member is a hole, and the delivery point itself is always one
        // of them (it is never in `pending`).
        let missing: Vec<usize> = (0..lens.len())
            .filter(|&i| {
                let seq = first.wrapping_add(i as u16);
                seq.wrapping_sub(self.next_seq) < SEQ_HALF && !self.pending.contains_key(&seq)
            })
            .collect();
        match missing.len() {
            0 => {}
            1 => {
                let idx = missing[0];
                let target = first.wrapping_add(idx as u16);
                // XOR the blob with every surviving member (zero-padded to
                // the blob length); length and flags come from the header.
                let mut recovered = blob.to_vec();
                for i in 0..lens.len() {
                    if i == idx {
                        continue;
                    }
                    let seq = first.wrapping_add(i as u16);
                    let member = if seq.wrapping_sub(self.next_seq) >= SEQ_HALF {
                        // Delivered before the gap — `recent` keeps it.
                        self.recent.iter().rev().find(|p| p.seq == seq)
                    } else {
                        self.pending.get(&seq)
                    };
                    let Some(member) = member else {
                        // An input is gone (pruned or never kept): the
                        // group is unrecoverable.
                        self.gap_armed = true;
                        self.keyframe_requests += 1;
                        return Ok(());
                    };
                    for (byte, xor) in member.payload.iter().zip(recovered.iter_mut()) {
                        *xor ^= byte;
                    }
                }
                recovered.truncate(lens[idx] as usize);
                self.pending.insert(
                    target,
                    Packet {
                        seq: target,
                        flags: flags[idx] & FLAG_DATA_MASK,
                        ts_ms: 0,
                        payload: recovered,
                    },
                );
            }
            _ => {
                self.gap_armed = true;
                self.keyframe_requests += 1;
            }
        }
        Ok(())
    }

    /// Resyncs to the next held frame start once the gap is confirmed
    /// unrecoverable; everything between the gap and that frame is lost.
    fn maybe_resync(&mut self) {
        if !self.gap_armed && self.pending.len() <= PENDING_CAP {
            return;
        }
        let Some(target) = self.smallest_frame_start() else {
            // No frame start in sight: stay bounded by dropping the
            // oldest held packet (smallest distance ahead of the gap =
            // earliest sent; nothing can be assembled from it anyway).
            while self.pending.len() > PENDING_CAP {
                let oldest = self
                    .pending
                    .keys()
                    .copied()
                    .min_by_key(|seq| seq.wrapping_sub(self.next_seq));
                match oldest {
                    Some(seq) => {
                        self.pending.remove(&seq);
                    }
                    None => break,
                }
            }
            return;
        };
        if self.in_frame {
            self.lost_frames += 1;
        }
        self.keyframe_requests += 1;
        let stale: Vec<u16> = self.pending.range(..target).map(|(seq, _)| *seq).collect();
        for seq in stale {
            if let Some(pkt) = self.pending.remove(&seq) {
                if pkt.flags & FLAG_FIRST != 0 {
                    self.lost_frames += 1;
                }
            }
        }
        self.fragments.clear();
        self.in_frame = false;
        self.next_seq = target;
        self.gap_armed = false;
    }

    /// The earliest held packet that starts a frame, scanning forward from
    /// the delivery point (wrapping through the lower range second).
    fn smallest_frame_start(&self) -> Option<u16> {
        self.pending
            .range(self.next_seq..)
            .chain(self.pending.range(..self.next_seq))
            .find(|(_, pkt)| pkt.flags & FLAG_FIRST != 0)
            .map(|(seq, _)| *seq)
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
            .flat_map(|p| re.push(p.clone()).unwrap())
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
    fn keyframe_request_roundtrip() {
        let pkt = Packet::keyframe_request();
        let parsed = Packet::from_bytes(&pkt.to_vec().unwrap()).unwrap();
        assert_eq!(parsed.flags, FLAG_PLI);
        assert!(parsed.payload.is_empty());
        // The data path ignores it entirely.
        let mut re = Reassembler::new();
        assert!(re.push(pkt).unwrap().is_empty());
        assert_eq!(re.keyframe_requests(), 0);
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
        assert_eq!(re.keyframe_requests(), 0);
    }

    #[test]
    fn seq_wraps_at_65536() {
        let mut pz = Packetizer {
            next_seq: 65534,
            ..Packetizer::default()
        };
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
            assert!(re.push(p.clone()).unwrap().is_empty());
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
        // The abandoned frame asked for a keyframe.
        assert_eq!(re.keyframe_requests(), 1);
    }

    #[test]
    fn mid_stream_join_ignores_stray_packets() {
        let mut pz = Packetizer::new();
        let packets = pz
            .packetize(&frame(0, false, 3000), 0, DEFAULT_MTU)
            .unwrap();
        let mut re = Reassembler::new();
        for p in &packets[1..] {
            assert!(re.push(p.clone()).unwrap().is_empty());
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
        assert_eq!(re.keyframe_requests(), 1);
    }

    #[test]
    fn late_duplicates_are_dropped() {
        let mut pz = Packetizer::new();
        let mut re = Reassembler::new();
        let f1 = pz.packetize(&frame(0, true, 3000), 0, DEFAULT_MTU).unwrap();
        assert_eq!(push_all(&mut re, &f1).len(), 1);
        // A retransmission of seq 0 lands after the delivery point moved on.
        assert!(re.push(f1[0].clone()).unwrap().is_empty());
        let f2 = pz.packetize(&frame(1, false, 100), 1, DEFAULT_MTU).unwrap();
        assert_eq!(push_all(&mut re, &f2).len(), 1);
    }

    // --------------------- parity (increment ⑤) --------------------- //

    /// Feeds `packets` except the seqs in `drop`, returning the frames.
    fn feed_except(re: &mut Reassembler, packets: &[Packet], drop: &[u16]) -> Vec<EncodedFrame> {
        packets
            .iter()
            .filter(|p| !drop.contains(&p.seq) || p.flags & (FLAG_PARITY | FLAG_PLI) != 0)
            .flat_map(|p| re.push(p.clone()).unwrap())
            .collect()
    }

    #[test]
    fn parity_off_emits_no_parity_packets() {
        let mut pz = Packetizer::new();
        let packets = pz.packetize(&frame(0, true, 500), 0, 100).unwrap();
        assert!(packets.iter().all(|p| p.flags & FLAG_PARITY == 0));
        assert!(Packetizer::with_parity(0).parity_group.is_none());
    }

    #[test]
    fn parity_packet_layout_and_sideband_seqs() {
        let mut pz = Packetizer::with_parity(2);
        let packets = pz.packetize(&frame(0, true, 250), 7, 100).unwrap();
        // 3 data packets; parity seals right after the 2nd member.
        assert_eq!(packets.len(), 4);
        assert_eq!(packets[0].seq, 0);
        assert_eq!(packets[1].seq, 1);
        assert_eq!(packets[3].seq, 2);
        let parity = &packets[2];
        assert_eq!(parity.flags, FLAG_PARITY);
        assert_eq!(parity.seq, 0, "parity is sideband, no seq of its own");

        let group = parse_parity(&parity.payload).unwrap();
        assert_eq!(group.first, 0);
        assert_eq!(group.lens, vec![100, 100]);
        // Every fragment of a keyframe carries FLAG_KEY.
        assert_eq!(group.flags, vec![FLAG_FIRST | FLAG_KEY, FLAG_KEY]);
        assert_eq!(group.blob.len(), 100);
        for (i, byte) in group.blob.iter().enumerate() {
            assert_eq!(*byte, packets[0].payload[i] ^ packets[1].payload[i]);
        }
        assert_eq!(parity.ts_ms, 7);
    }

    #[test]
    fn parity_group_spans_packetize_calls() {
        let mut pz = Packetizer::with_parity(3);
        let first = pz.packetize(&frame(0, true, 100), 0, 100).unwrap();
        assert_eq!(first.len(), 1, "group still open, no parity yet");
        let second = pz.packetize(&frame(1, false, 200), 1, 100).unwrap();
        // Members: seq 1, 2 seal the group started at seq 0.
        assert_eq!(second.len(), 3);
        let parity = second.iter().find(|p| p.flags & FLAG_PARITY != 0).unwrap();
        let group = parse_parity(&parity.payload).unwrap();
        assert_eq!(group.first, 0);
        assert_eq!(group.lens, vec![100, 100, 100]);
    }

    #[test]
    fn parity_group_clamped_to_max() {
        let mut pz = Packetizer::with_parity(1000);
        let packets = pz
            .packetize(&frame(0, false, 100 * (MAX_PARITY_GROUP + 1)), 0, 100)
            .unwrap();
        let parities = packets
            .iter()
            .filter(|p| p.flags & FLAG_PARITY != 0)
            .count();
        assert_eq!(parities, 1, "one parity after the clamped 64 members");
    }

    #[test]
    fn parity_recovers_single_mid_frame_loss() {
        let mut pz = Packetizer::with_parity(2);
        let mut re = Reassembler::new();
        // Warm-up frame so parity is in play (recovery engages once the
        // first parity packet has been seen — module docs).
        let f0 = pz.packetize(&frame(0, true, 200), 0, 100).unwrap();
        let f1 = pz.packetize(&frame(1, false, 250), 1, 100).unwrap();
        let mut packets = f0;
        packets.extend(f1);
        // Group [0,1] seals with the warm-up; group [2,3] loses seq 3 and
        // its parity rebuilds it.
        let out = feed_except(&mut re, &packets, &[3]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].data, frame(1, false, 250).data);
        assert_eq!(re.lost_frames(), 0);
        assert_eq!(re.keyframe_requests(), 0);
    }

    #[test]
    fn parity_recovers_keyframe_first_packet() {
        let mut pz = Packetizer::with_parity(2);
        let mut re = Reassembler::new();
        let f0 = pz.packetize(&frame(0, true, 200), 0, 100).unwrap();
        let f1 = pz.packetize(&frame(1, true, 200), 1, 100).unwrap();
        let mut packets = f0;
        packets.extend(f1);
        // Dropping the keyframe's FIRST leaves its tail held until parity
        // rebuilds it with FIRST|KEY flags intact.
        let out = feed_except(&mut re, &packets, &[2]);
        assert_eq!(out.len(), 2);
        assert!(out[1].keyframe);
        assert_eq!(out[1].data, frame(1, true, 200).data);
        assert_eq!(re.lost_frames(), 0);
        assert_eq!(re.keyframe_requests(), 0);
    }

    #[test]
    fn parity_recovers_across_frame_boundary() {
        let mut pz = Packetizer::with_parity(3);
        let mut re = Reassembler::new();
        let f0 = pz.packetize(&frame(0, true, 200), 0, 100).unwrap();
        let f1 = pz.packetize(&frame(1, false, 100), 1, 100).unwrap();
        let f2 = pz.packetize(&frame(2, false, 200), 2, 100).unwrap();
        let f3 = pz.packetize(&frame(3, false, 100), 3, 100).unwrap();
        let mut packets = f0;
        packets.extend(f1);
        packets.extend(f2);
        packets.extend(f3);
        // Packet order: 0, 1, 2, P[0-2], 3, 4, 5, P[3-5]. Group [3-5]
        // spans frames 2 and 3; dropping seq 4 rebuilds it from the
        // already-delivered seq 3 (`recent`) and the held seq 5.
        let out = feed_except(&mut re, &packets, &[4]);
        assert_eq!(out.len(), 4);
        assert_eq!(out[2].data, frame(2, false, 200).data);
        assert_eq!(out[3].data, frame(3, false, 100).data);
        assert_eq!(re.lost_frames(), 0);
        assert_eq!(re.keyframe_requests(), 0);
    }

    #[test]
    fn parity_recovery_uses_recent_for_straddling_group() {
        let mut pz = Packetizer::with_parity(2);
        let mut re = Reassembler::new();
        let f1 = pz.packetize(&frame(0, true, 300), 0, 100).unwrap();
        let f2 = pz.packetize(&frame(1, false, 200), 1, 100).unwrap();
        let mut packets = f1;
        packets.extend(f2);
        // Packet order: seq 0, 1, P[0,1], 2, 3, P[2,3]. Drop seq 3 — the
        // group starts before the delivery point, so the rebuild must XOR
        // the already-delivered seq 2 out of `recent`.
        let out = feed_except(&mut re, &packets, &[3]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].data, frame(1, false, 200).data);
        assert_eq!(re.keyframe_requests(), 0);
    }

    #[test]
    fn parity_double_loss_gives_up_and_requests_keyframe() {
        let mut pz = Packetizer::with_parity(4);
        let mut re = Reassembler::new();
        let f1 = pz.packetize(&frame(0, true, 350), 0, 100).unwrap();
        let f2 = pz.packetize(&frame(1, false, 200), 1, 100).unwrap();
        let mut packets = f1;
        packets.extend(f2);
        // Group [0, 3] loses seqs 1 and 2 — beyond XOR's single-loss reach.
        // Frame 2's FIRST (seq 4) then triggers the resync.
        let out = feed_except(&mut re, &packets, &[1, 2]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, frame(1, false, 200).data);
        assert_eq!(re.lost_frames(), 1);
        assert!(re.keyframe_requests() >= 1);
    }

    #[test]
    fn parity_lost_parity_resyncs_on_later_parity() {
        let mut pz = Packetizer::with_parity(2);
        let mut re = Reassembler::new();
        let f1 = pz.packetize(&frame(0, true, 200), 0, 100).unwrap();
        let f2 = pz.packetize(&frame(1, false, 200), 1, 100).unwrap();
        let f3 = pz.packetize(&frame(2, false, 200), 2, 100).unwrap();
        let mut packets = f1;
        packets.extend(f2);
        packets.extend(f3);
        // Drop seq 2 (frame 2's FIRST) AND its group's parity (the parity
        // packets are sideband at seq 0; frame 2's carries ts 1): the gap
        // only resolves when the *next* group's parity proves the chance
        // is gone, and the resync salvages frame 3.
        let out = packets
            .iter()
            .filter(|p| p.seq != 2 && !(p.flags & FLAG_PARITY != 0 && p.ts_ms == 1))
            .flat_map(|p| re.push(p.clone()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(out.len(), 2, "frames 1 and 3 survive");
        assert_eq!(out[1].data, frame(2, false, 200).data);
        assert!(re.keyframe_requests() >= 1);
    }

    #[test]
    fn parity_malformed_payload_rejected() {
        let mut re = Reassembler::new();
        re.push(Packet {
            seq: 0,
            flags: FLAG_FIRST,
            ts_ms: 0,
            payload: vec![0],
        })
        .unwrap();
        let short = Packet {
            seq: 0,
            flags: FLAG_PARITY,
            ts_ms: 0,
            payload: vec![0, 0, 2, 0],
        };
        assert!(re.push(short).is_err());
        let zero_members = Packet {
            seq: 0,
            flags: FLAG_PARITY,
            ts_ms: 0,
            payload: vec![0, 0, 0, 0, 0],
        };
        assert!(re.push(zero_members).is_err());
        // A member claiming more bytes than the blob is inconsistent.
        let inconsistent = Packet {
            seq: 0,
            flags: FLAG_PARITY,
            ts_ms: 0,
            payload: vec![0, 0, 1, 0, 8, 0, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8],
        };
        assert!(re.push(inconsistent).is_err());
    }

    #[test]
    fn parity_recovers_across_seq_wrap() {
        let mut pz = Packetizer {
            next_seq: 65534,
            parity_group: Some(2),
            ..Packetizer::default()
        };
        let mut re = Reassembler::new();
        let f1 = pz.packetize(&frame(0, true, 200), 0, 100).unwrap();
        let f2 = pz.packetize(&frame(1, true, 200), 1, 100).unwrap();
        let mut packets = f1;
        packets.extend(f2);
        // Groups: [65534, 65535] then [0, 1] — the wrapped group loses its
        // first member and parity rebuilds it.
        let out = feed_except(&mut re, &packets, &[0]);
        assert_eq!(out.len(), 2);
        assert!(out[1].keyframe);
        assert_eq!(out[1].data, frame(1, true, 200).data);
        assert_eq!(re.keyframe_requests(), 0);
    }

    #[test]
    fn pending_flood_stays_bounded_and_resyncs() {
        let mut pz = Packetizer::new();
        let mut re = Reassembler::new();
        let f1 = pz.packetize(&frame(0, true, 3000), 0, DEFAULT_MTU).unwrap();
        assert_eq!(push_all(&mut re, &f1).len(), 1);
        // A lost whole frame opens the gap; a flood of frameless tails
        // behind it must not grow without bound, and the next real frame
        // still assembles once it starts.
        let _lost = pz.packetize(&frame(1, false, 100), 1, DEFAULT_MTU).unwrap();
        for i in 0..400 {
            let tail = pz
                .packetize(&frame(i as u64 + 2, false, 100), i, DEFAULT_MTU)
                .unwrap();
            let mut stray = tail[0].clone();
            stray.flags &= !FLAG_FIRST;
            re.push(stray).unwrap();
        }
        let f3 = pz
            .packetize(&frame(999, false, DEFAULT_MTU + 5), 999, DEFAULT_MTU)
            .unwrap();
        let out = push_all(&mut re, &f3);
        assert_eq!(out.len(), 1);
        assert!(re.keyframe_requests() >= 1);
    }
}
