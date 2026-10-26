use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{Encoder, EncoderConfig, FrameType, IntraFramePeriod, Profile, UsageType};
use openh264::formats::YUVSource;

use crate::codec::{EncodedFrame, VideoDecoder, VideoEncoder};
use crate::frame::VideoFrame;
use crate::yuv::{bgra_to_i420, i420_to_bgra};

/// OpenH264 software encoder (H.264 baseline).
///
/// Input is BGRA; the BGRA→I420 conversion happens here so the CPU cost of
/// the conversion segment stays attributable to the codec stage.
pub struct OpenH264Encoder {
    encoder: Encoder,
}

impl OpenH264Encoder {
    /// `keyframe_interval` = IDR every N frames (0 disables periodic IDRs;
    /// keyframes then only on `force_keyframe`).
    pub fn new(keyframe_interval: u32) -> Result<Self, crate::VideoError> {
        let mut config = EncoderConfig::new()
            .profile(Profile::Baseline)
            .usage_type(UsageType::ScreenContentRealTime)
            .num_threads(1)
            .skip_frames(false);
        if keyframe_interval > 0 {
            config =
                config.intra_frame_period(IntraFramePeriod::from_num_frames(keyframe_interval));
        }
        Ok(Self {
            encoder: Encoder::with_api_config(OpenH264API::from_source(), config)?,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn encode(
        &mut self,
        frame: &VideoFrame,
        force_keyframe: bool,
    ) -> Result<EncodedFrame, crate::VideoError> {
        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let planes = bgra_to_i420(&frame.bgra, frame.width, frame.height);
        let bitstream = self.encoder.encode(&planes)?;
        Ok(EncodedFrame {
            index: frame.index,
            keyframe: matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I),
            data: bitstream.to_vec(),
        })
    }
}

/// OpenH264 software decoder producing BGRA frames (I420→BGRA conversion is
/// part of the decode segment per the M3a design).
///
/// After any decode failure the decoder "poisons" itself: non-keyframes are
/// skipped until the next IDR restores a reference point (log-recorded).
pub struct OpenH264Decoder {
    decoder: Decoder,
    poisoned: bool,
}

impl Default for OpenH264Decoder {
    fn default() -> Self {
        Self::new().expect("openh264 decoder construction cannot fail in practice")
    }
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self, crate::VideoError> {
        Ok(Self {
            decoder: Decoder::new()?,
            poisoned: false,
        })
    }
}

/// Decoding-state bits (see `DECODING_STATE` in the OpenH264 SDK) that mean
/// "this frame can't be decoded from the current stream state" rather than
/// "the decoder is broken": frame pending, reference lost, bitstream error,
/// dependency layer lost, parameter sets missing, data error concealed.
const RECOVERABLE_STATES: i64 = 0b0011_1111;

impl VideoDecoder for OpenH264Decoder {
    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<VideoFrame>, crate::VideoError> {
        if self.poisoned && !frame.keyframe {
            return Ok(None);
        }
        match self.decoder.decode(&frame.data) {
            Err(e) => {
                self.poisoned = true;
                let native = e.native_code();
                if native > 0 && native <= RECOVERABLE_STATES {
                    log::debug!("video: skipping undecodable frame (state {native:#x})");
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
            Ok(None) => Ok(None),
            Ok(Some(decoded)) => {
                self.poisoned = false;
                let (width, height) = decoded.dimensions();
                let (y_stride, uv_stride, _) = decoded.strides();
                let bgra = i420_to_bgra(
                    decoded.y(),
                    decoded.u(),
                    decoded.v(),
                    y_stride,
                    uv_stride,
                    width as u32,
                    height as u32,
                );
                Ok(Some(
                    VideoFrame::new(width as u32, height as u32, frame.index, bgra)
                        .map_err(|e| crate::VideoError::Codec(e.to_string()))?,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::DummyVideoSource;
    use crate::capture::dummy::read_pattern_index;
    use futures::StreamExt;

    async fn pattern_frames(n: u64) -> Vec<VideoFrame> {
        DummyVideoSource::new(320, 180, 480, Some(n))
            .collect()
            .await
    }

    #[tokio::test]
    async fn encode_decode_roundtrip() {
        let frames = pattern_frames(3).await;
        let mut enc = OpenH264Encoder::new(0).unwrap();
        let mut dec = OpenH264Decoder::new().unwrap();

        let mut decoded_any = false;
        for frame in &frames {
            let encoded = enc.encode(frame, false).unwrap();
            if let Some(out) = dec.decode(&encoded).unwrap() {
                decoded_any = true;
                assert_eq!(out.width, 320);
                assert_eq!(out.height, 180);
                assert_eq!(
                    read_pattern_index(&out),
                    frame.index,
                    "counter pixels must survive"
                );
            }
        }
        assert!(decoded_any);
    }

    #[tokio::test]
    async fn decode_before_keyframe_is_none() {
        let frames = pattern_frames(2).await;
        let mut enc = OpenH264Encoder::new(0).unwrap();
        let mut dec = OpenH264Decoder::new().unwrap();

        // Skip the first (IDR) frame entirely; the P frame alone cannot
        // produce output yet.
        let _idr = enc.encode(&frames[0], false).unwrap();
        let p = enc.encode(&frames[1], false).unwrap();
        assert!(!p.keyframe);
        assert!(dec.decode(&p).unwrap().is_none());
    }

    #[tokio::test]
    async fn force_keyframe_sets_flag() {
        let mut src = DummyVideoSource::new(320, 180, 480, Some(2));
        let first = src.next().await.unwrap();
        let second = src.next().await.unwrap();
        let mut enc = OpenH264Encoder::new(0).unwrap();

        assert!(enc.encode(&first, false).unwrap().keyframe);
        assert!(!enc.encode(&second, false).unwrap().keyframe);
        assert!(enc.encode(&second, true).unwrap().keyframe);
    }
}
