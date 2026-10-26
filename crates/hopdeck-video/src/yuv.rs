use openh264::formats::YUVSource;

/// Planar I420 (YUV 4:2:0) image in limited range (BT.601), the format
/// OpenH264 consumes and produces.
///
/// Chroma dimensions are `(width + 1) / 2 x (height + 1) / 2` so odd capture
/// sizes survive; chroma of the rightmost column / bottom row covers a
/// truncated 2x2 block.
#[derive(Clone, Debug)]
pub struct YuvPlanes {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl YuvPlanes {
    pub fn y_stride(&self) -> usize {
        self.width as usize
    }

    pub fn uv_stride(&self) -> usize {
        (self.width as usize).div_ceil(2)
    }
}

impl YUVSource for YuvPlanes {
    fn dimensions(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        let uv = self.uv_stride();
        (self.y_stride(), uv, uv)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Converts packed BGRA8 (byte order `[B, G, R, A]`) to planar I420.
///
/// Chroma is the BT.601 conversion of the average RGB of each 2x2 block.
pub fn bgra_to_i420(bgra: &[u8], width: u32, height: u32) -> YuvPlanes {
    let w = width as usize;
    let h = height as usize;
    let uv_stride = w.div_ceil(2);
    let mut planes = YuvPlanes {
        width,
        height,
        y: vec![0; w * h],
        u: vec![128; uv_stride * (h.div_ceil(2))],
        v: vec![128; uv_stride * (h.div_ceil(2))],
    };

    for row in 0..h {
        let src = &bgra[row * w * 4..][..w * 4];
        let dst = &mut planes.y[row * w..][..w];
        for (col, d) in dst.iter_mut().enumerate() {
            let b = src[col * 4] as i32;
            let g = src[col * 4 + 1] as i32;
            let r = src[col * 4 + 2] as i32;
            *d = clip((66 * r + 129 * g + 25 * b + 128) >> 8, 0, 255).wrapping_add(16) as u8;
        }
    }

    for by in 0..h.div_ceil(2) {
        for bx in 0..uv_stride {
            let (r, g, b) = average_block(bgra, w, h, bx, by);
            let u = clip((-38 * r - 74 * g + 112 * b + 128) >> 8, -128, 127).wrapping_add(128);
            let v = clip((112 * r - 94 * g - 18 * b + 128) >> 8, -128, 127).wrapping_add(128);
            planes.u[by * uv_stride + bx] = u as u8;
            planes.v[by * uv_stride + bx] = v as u8;
        }
    }

    planes
}

/// Converts planar I420 (arbitrary strides, e.g. decoder output) back to
/// packed BGRA8 with alpha 255.
pub fn i420_to_bgra(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    y_stride: usize,
    uv_stride: usize,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut bgra = vec![0u8; w * h * 4];

    for row in 0..h {
        let uv_row = (row / 2) * uv_stride;
        for col in 0..w {
            let yp = y[row * y_stride + col] as i32;
            let up = u[uv_row + col / 2] as i32;
            let vp = v[uv_row + col / 2] as i32;

            let yv = 298 * (yp - 16);
            let cb = up - 128;
            let cr = vp - 128;
            let r = (yv + 409 * cr + 128) >> 8;
            let g = (yv - 100 * cb - 208 * cr + 128) >> 8;
            let b = (yv + 516 * cb + 128) >> 8;

            let dst = &mut bgra[(row * w + col) * 4..][..4];
            dst[0] = b.clamp(0, 255) as u8;
            dst[1] = g.clamp(0, 255) as u8;
            dst[2] = r.clamp(0, 255) as u8;
            dst[3] = 255;
        }
    }

    bgra
}

/// Clamps an intermediate conversion value; for legal RGB inputs the BT.601
/// results already land inside the limited ranges, so this only guards
/// arithmetic edges.
fn clip(v: i32, lo: i32, hi: i32) -> i32 {
    v.clamp(lo, hi)
}

/// Averages the RGB of the 2x2 block whose top-left pixel is `(bx*2, by*2)`,
/// clamped at the right/bottom image edges.
fn average_block(bgra: &[u8], w: usize, h: usize, bx: usize, by: usize) -> (i32, i32, i32) {
    let mut sum = (0i64, 0i64, 0i64);
    let mut n = 0;
    for dy in 0..2 {
        let row = by * 2 + dy;
        if row >= h {
            break;
        }
        for dx in 0..2 {
            let col = bx * 2 + dx;
            if col >= w {
                break;
            }
            let px = &bgra[(row * w + col) * 4..];
            sum.0 += px[2] as i64;
            sum.1 += px[1] as i64;
            sum.2 += px[0] as i64;
            n += 1;
        }
    }
    (sum.0 as i32 / n, sum.1 as i32 / n, sum.2 as i32 / n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid BGRA image of a single color.
    fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Vec<u8> {
        (0..width * height).flat_map(|_| bgra).collect()
    }

    #[test]
    fn roundtrip_primary_colors() {
        // [B, G, R, A]
        for color in [
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            [0, 0, 255, 255],
            [255, 0, 0, 255],
            [0, 255, 0, 255],
        ] {
            let bgra = solid(16, 16, color);
            let planes = bgra_to_i420(&bgra, 16, 16);
            let back = i420_to_bgra(
                &planes.y,
                &planes.u,
                &planes.v,
                planes.y_stride(),
                planes.uv_stride(),
                16,
                16,
            );
            // BT.601 limited range clips saturated colors; only assert the
            // luminance axis and that the other channels stay near black.
            let lum = color[0].max(color[1]).max(color[2]);
            for px in back.chunks_exact(4) {
                for c in 0..3 {
                    if color[c] == lum {
                        assert!(
                            (px[c] as i32 - lum as i32).abs() <= 4,
                            "{color:?} channel {c} -> {px:?}"
                        );
                    } else {
                        assert!(px[c] <= 4, "{color:?} channel {c} -> {px:?}");
                    }
                }
                assert_eq!(px[3], 255);
            }
        }
    }

    #[test]
    fn odd_dimensions_roundtrip() {
        let bgra = solid(5, 3, [0, 0, 255, 255]);
        let planes = bgra_to_i420(&bgra, 5, 3);
        assert_eq!(planes.u.len(), 3 * 2);
        assert_eq!(planes.v.len(), 3 * 2);
        let back = i420_to_bgra(
            &planes.y,
            &planes.u,
            &planes.v,
            planes.y_stride(),
            planes.uv_stride(),
            5,
            3,
        );
        assert_eq!(back.len(), 5 * 3 * 4);
        for px in back.chunks_exact(4) {
            assert!(px[2] >= 250, "red channel {px:?}");
            assert!(px[0] <= 4 && px[1] <= 4, "{px:?}");
        }
    }

    #[test]
    fn yuv_source_view_matches_planes() {
        let bgra = solid(8, 8, [0, 128, 0, 255]);
        let planes = bgra_to_i420(&bgra, 8, 8);
        let (w, h) = planes.dimensions();
        assert_eq!((w, h), (8, 8));
        assert_eq!(planes.y().len(), 64);
        assert_eq!(planes.u().len(), 16);
    }
}
