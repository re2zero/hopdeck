//! Full-pipeline loopback (M3a design §10): dummy color-bar/counter frames →
//! soft encode → packetize → reassemble → soft decode → pixel assertions.
//!
//! The frame counter embedded in the pattern is read back from decoded
//! pixels, so passing asserts end-to-end zero mis-order and zero silent
//! corruption without any protocol-level trust.

use mousehop_video::capture::DummyVideoSource;
use mousehop_video::capture::dummy::read_pattern_index;
use mousehop_video::codec::{OpenH264Decoder, OpenH264Encoder, VideoDecoder, VideoEncoder};
use mousehop_video::transport::{DEFAULT_MTU, Packetizer, Reassembler};

const W: u32 = 320;
const H: u32 = 180;

#[tokio::test]
async fn loopback_without_loss_is_pixel_exact() {
    const FRAMES: u64 = 12;

    let mut src = DummyVideoSource::new(W, H, 480, Some(FRAMES));
    let mut enc = OpenH264Encoder::new(0).unwrap();
    let mut pz = Packetizer::new();
    let mut re = Reassembler::new();
    let mut dec = OpenH264Decoder::new().unwrap();

    let mut decoded_indices = Vec::new();
    while let Some(frame) = futures::StreamExt::next(&mut src).await {
        let encoded = enc.encode(&frame, false).unwrap();
        assert_eq!(encoded.index, frame.index);
        for pkt in pz
            .packetize(&encoded, frame.index as u32 * 33, DEFAULT_MTU)
            .unwrap()
        {
            for access_unit in re.push(pkt).unwrap() {
                let img = dec
                    .decode(&access_unit)
                    .unwrap()
                    .expect("every frame decodes on a lossless link");
                let counter = read_pattern_index(&img);
                assert_eq!(
                    counter, frame.index,
                    "decoded counter must match sender index"
                );
                decoded_indices.push(counter);
            }
        }
    }

    assert_eq!(decoded_indices, (0..FRAMES).collect::<Vec<_>>());
    assert_eq!(re.lost_frames(), 0);
}

#[tokio::test]
async fn colorbars_survive_the_roundtrip() {
    let mut src = DummyVideoSource::new(W, H, 480, Some(1));
    let frame = futures::StreamExt::next(&mut src).await.unwrap();
    let mut enc = OpenH264Encoder::new(0).unwrap();
    let mut dec = OpenH264Decoder::new().unwrap();

    let encoded = enc.encode(&frame, false).unwrap();
    let img = dec.decode(&encoded).unwrap().unwrap();

    // Bar interiors: white (bar 0) and black (bar 7) sit at row 40, far from
    // strip and edges. BT.601 limited range preserves both; allow ±2 for
    // codec rounding.
    let row = 40usize;
    for (bar, expected) in [(0usize, 255u8), (7, 0)] {
        let col = bar * W as usize / 8 + W as usize / 16;
        let px = &img.bgra[(row * W as usize + col) * 4..][..4];
        assert!(
            (px[0] as i32 - expected as i32).abs() <= 2,
            "B of bar {bar}: {px:?}"
        );
        assert!(
            (px[1] as i32 - expected as i32).abs() <= 2,
            "G of bar {bar}: {px:?}"
        );
        assert!(
            (px[2] as i32 - expected as i32).abs() <= 2,
            "R of bar {bar}: {px:?}"
        );
        assert_eq!(px[3], 255, "alpha of bar {bar}");
    }
}

#[tokio::test]
async fn loopback_recovers_after_packet_loss() {
    // 720p makes the keyframes large enough to span multiple packets, so the
    // injected drop is guaranteed to tear a frame regardless of encoder
    // byte-size choices.
    const W7: u32 = 1280;
    const H7: u32 = 720;
    const FRAMES: u64 = 30;
    // IDR every 10 frames (0, 10, 20). Killing one packet of frame 10's
    // keyframe makes 10..20 undecodable; the IDR at 20 resumes the stream.
    let mut src = DummyVideoSource::new(W7, H7, 480, Some(FRAMES));
    let mut enc = OpenH264Encoder::new(10).unwrap();
    let mut pz = Packetizer::new();
    let mut re = Reassembler::new();
    let mut dec = OpenH264Decoder::new().unwrap();

    let mut decoded_indices = Vec::new();
    while let Some(frame) = futures::StreamExt::next(&mut src).await {
        let encoded = enc.encode(&frame, false).unwrap();
        for (i, pkt) in pz
            .packetize(&encoded, frame.index as u32 * 33, DEFAULT_MTU)
            .unwrap()
            .into_iter()
            .enumerate()
        {
            // Drop the second packet of the frame-10 keyframe.
            if frame.index == 10 && i == 1 {
                continue;
            }
            if let Some(access_unit) = re.push(pkt).unwrap().pop() {
                // Frames that cannot decode report None and are skipped.
                if let Some(img) = dec.decode(&access_unit).unwrap() {
                    assert_eq!(img.width, W7);
                    decoded_indices.push(read_pattern_index(&img));
                }
            }
        }
    }

    assert_eq!(
        re.lost_frames(),
        1,
        "frame 10's fragments must be counted lost"
    );
    assert!(
        re.keyframe_requests() >= 1,
        "the unrecoverable gap must raise a keyframe request"
    );
    assert_eq!(
        decoded_indices,
        (0..10).chain(20..FRAMES).collect::<Vec<_>>(),
        "stream must resume pixel-exact at the next keyframe"
    );
}

#[tokio::test]
async fn parity_recovers_single_packet_loss_pixel_exact() {
    // Same loss as above, but with XOR parity on: the dropped packet is
    // rebuilt and the stream never skips a beat (design §10 loss matrix,
    // "lose 1 → recover" branch).
    const W7: u32 = 1280;
    const H7: u32 = 720;
    const FRAMES: u64 = 30;
    let mut src = DummyVideoSource::new(W7, H7, 480, Some(FRAMES));
    let mut enc = OpenH264Encoder::new(10).unwrap();
    let mut pz = Packetizer::with_parity(4);
    let mut re = Reassembler::new();
    let mut dec = OpenH264Decoder::new().unwrap();

    let mut decoded_indices = Vec::new();
    while let Some(frame) = futures::StreamExt::next(&mut src).await {
        let encoded = enc.encode(&frame, false).unwrap();
        for (i, pkt) in pz
            .packetize(&encoded, frame.index as u32 * 33, DEFAULT_MTU)
            .unwrap()
            .into_iter()
            .enumerate()
        {
            // Drop one packet well after the first parity group has been
            // seen (recovery engages from there — module docs).
            if frame.index == 15 && i == 1 {
                continue;
            }
            for access_unit in re.push(pkt).unwrap() {
                if let Some(img) = dec.decode(&access_unit).unwrap() {
                    assert_eq!(img.width, W7);
                    decoded_indices.push(read_pattern_index(&img));
                }
            }
        }
    }

    assert_eq!(decoded_indices, (0..FRAMES).collect::<Vec<_>>());
    assert_eq!(re.lost_frames(), 0, "parity erased the loss");
    assert_eq!(re.keyframe_requests(), 0, "no keyframe needed");
}
