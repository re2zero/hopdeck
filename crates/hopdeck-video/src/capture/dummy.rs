use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use futures_core::Stream;
use tokio::time::{self, Interval};

use crate::frame::VideoFrame;

/// Bottom strip of the pattern: this many binary cells encode the frame
/// index (MSB first, white = 1, black = 0), so a decoded frame's index can
/// be read back from pixels to assert zero loss / zero mis-order end to end.
pub const COUNTER_CELLS: usize = 16;

const STRIP_HEIGHT: u32 = 16;
/// SMPTE-style bars, left to right, as `[B, G, R]`.
const BARS: [[u8; 3]; 8] = [
    [255, 255, 255], // white
    [255, 255, 0],   // yellow
    [255, 0, 255],   // cyan
    [0, 255, 0],     // green
    [255, 0, 0],     // magenta
    [0, 0, 255],     // red
    [0, 128, 0],     // dark green
    [0, 0, 0],       // black
];

/// Renders the deterministic test pattern for `index`: color bars with a
/// bottom strip of [`COUNTER_CELLS`] binary cells carrying the frame index.
pub fn render_pattern(width: u32, height: u32, index: u64) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let strip_top = h.saturating_sub(STRIP_HEIGHT as usize);
    let mut bgra = vec![0u8; w * h * 4];

    for row in 0..h {
        for col in 0..w {
            let px = &mut bgra[(row * w + col) * 4..][..4];
            if row >= strip_top {
                let cell = COUNTER_CELLS - 1 - col * COUNTER_CELLS / w;
                let bit = (index >> cell) & 1 == 1;
                let v = if bit { 255 } else { 0 };
                px[0] = v;
                px[1] = v;
                px[2] = v;
            } else {
                let bar = col * BARS.len() / w;
                px[0] = BARS[bar][0];
                px[1] = BARS[bar][1];
                px[2] = BARS[bar][2];
            }
            px[3] = 255;
        }
    }

    bgra
}

/// Reads the frame index back out of a decoded pattern frame.
///
/// The strip is far from color-bar edges and uses only black/white, so the
/// BT.601 roundtrip cannot flip a bit; the threshold only guards against
/// codec ringing at cell borders.
pub fn read_pattern_index(frame: &VideoFrame) -> u64 {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let strip_top = h.saturating_sub(STRIP_HEIGHT as usize);
    let sample_row = strip_top + (STRIP_HEIGHT as usize) / 2;

    let mut index = 0u64;
    for cell in (0..COUNTER_CELLS).rev() {
        let col =
            ((COUNTER_CELLS - 1 - cell) * w / COUNTER_CELLS + (w / COUNTER_CELLS) / 2).min(w - 1);
        let px = &frame.bgra[(sample_row * w + col) * 4..];
        index = (index << 1) | u64::from(px[0] > 128);
    }
    index
}

/// Emits `frames` pattern frames at `fps` (or endlessly when `frames` is
/// `None`), reading like a capture backend without platform dependencies.
pub struct DummyVideoSource {
    interval: Interval,
    width: u32,
    height: u32,
    next: u64,
    remaining: Option<u64>,
}

impl DummyVideoSource {
    pub fn new(width: u32, height: u32, fps: u32, frames: Option<u64>) -> Self {
        Self {
            interval: time::interval(Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))),
            width,
            height,
            next: 0,
            remaining: frames,
        }
    }
}

impl Stream for DummyVideoSource {
    type Item = VideoFrame;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        ready!(self.interval.poll_tick(cx));
        if Some(0) == self.remaining {
            return Poll::Ready(None);
        }
        self.remaining = self.remaining.map(|n| n - 1);
        let index = self.next;
        self.next += 1;
        let bgra = render_pattern(self.width, self.height, index);
        Poll::Ready(Some(
            VideoFrame::new(self.width, self.height, index, bgra).expect("pattern size matches"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_roundtrips() {
        for index in [0u64, 1, 2, 0x1234, 0xffff] {
            let frame = VideoFrame::new(320, 180, index, render_pattern(320, 180, index)).unwrap();
            assert_eq!(read_pattern_index(&frame), index);
        }
    }

    #[test]
    fn bars_at_interior_centers() {
        let bgra = render_pattern(320, 180, 7);
        for (i, bar) in BARS.iter().enumerate() {
            let col = i * 320 / 8 + 320 / 16;
            let px = &bgra[(40 * 320 + col) * 4..];
            assert_eq!(&px[..3], bar, "bar {i}");
        }
    }

    #[tokio::test]
    async fn stream_counts_and_ends() {
        use futures::StreamExt;
        let frames: Vec<VideoFrame> = DummyVideoSource::new(320, 180, 240, Some(5))
            .collect()
            .await;
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[4].index, 4);
    }
}
