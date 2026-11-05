//! RGBA ↔ NV12 in BT.709 video range (Y 16–235, Cb/Cr 16–240), the form the
//! Media Foundation H.264 encoder takes and its decoder returns. VideoToolbox
//! converts BGRA itself, so on macOS this is only exercised by tests.
//!
//! On x86-64, which every Windows build is, SSE2 converts 16 pixels at a time
//! with the scalar code's own integer arithmetic, so the bytes are the same;
//! the scalar code does what's left of each row, and all of it elsewhere.
#![cfg_attr(not(windows), allow(dead_code))]

use image::RgbaImage;

// BT.709 coefficients scaled for video range, in 16.16 fixed point.
const YR: i32 = 11966; // 0.182586
const YG: i32 = 40254; // 0.614231
const YB: i32 = 4064; // 0.062007
const UR: i32 = -6596; // -0.100644
const UG: i32 = -22188; // -0.338572
const UB: i32 = 28784; // 0.439216
const VR: i32 = 28784; // 0.439216
const VG: i32 = -26145; // -0.398942
const VB: i32 = -2639; // -0.040274

const KY: i32 = 76309; // 1.164384
const KRV: i32 = 117489; // 1.792741
const KGU: i32 = 13975; // 0.213249
const KGV: i32 = 34925; // 0.532909
const KBU: i32 = 138439; // 2.112402

const HALF: i32 = 1 << 15;

/// Write `rgba` (`w × h`, tightly packed, `w` and `h` even) as NV12: a full-size
/// Y plane with rows `y_stride` bytes apart, then interleaved Cb/Cr at half size
/// with rows `uv_stride` bytes apart. Chroma is the average of each 2×2 block.
pub(crate) fn rgba_to_nv12(rgba: &[u8], w: usize, h: usize, y_plane: &mut [u8], y_stride: usize, uv_plane: &mut [u8], uv_stride: usize) {
    debug_assert!(w.is_multiple_of(2) && h.is_multiple_of(2) && rgba.len() >= w * h * 4);
    for row in 0..h / 2 {
        let (top, bottom) = rgba[2 * row * w * 4..(2 * row + 2) * w * 4].split_at(w * 4);
        let (y0, y1) = y_plane[2 * row * y_stride..].split_at_mut(y_stride);
        rows_to_nv12(top, bottom, &mut y0[..w], &mut y1[..w], &mut uv_plane[row * uv_stride..][..w]);
    }
}

/// Two RGBA rows as their two rows of Y and their row of Cb/Cr.
fn rows_to_nv12(top: &[u8], bottom: &[u8], y0: &mut [u8], y1: &mut [u8], uv: &mut [u8]) {
    // SAFETY: SSE2 is part of x86-64, so every CPU this runs on has it.
    #[cfg(target_arch = "x86_64")]
    let done = unsafe { sse2::rows_to_nv12(top, bottom, y0, y1, uv) };
    #[cfg(not(target_arch = "x86_64"))]
    let done = 0;
    scalar_rows_to_nv12(&top[done * 4..], &bottom[done * 4..], &mut y0[done..], &mut y1[done..], &mut uv[done..]);
}

/// [`rows_to_nv12`], a pixel at a time.
fn scalar_rows_to_nv12(top: &[u8], bottom: &[u8], y0: &mut [u8], y1: &mut [u8], uv: &mut [u8]) {
    for (src, dst) in [(top, y0), (bottom, y1)] {
        for (px, y) in src.as_chunks::<4>().0.iter().zip(dst) {
            let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
            *y = (((YR * r + YG * g + YB * b + HALF) >> 16) + 16) as u8;
        }
    }
    let blocks = top.as_chunks::<8>().0.iter().zip(bottom.as_chunks::<8>().0);
    for ((top, bottom), uv) in blocks.zip(uv.as_chunks_mut::<2>().0) {
        let sum = |c: usize| top[c] as i32 + top[4 + c] as i32 + bottom[c] as i32 + bottom[4 + c] as i32;
        // Sums of four pixels: shift two more bits to average.
        let (r, g, b) = (sum(0), sum(1), sum(2));
        uv[0] = (((UR * r + UG * g + UB * b + (HALF << 2)) >> 18) + 128).clamp(0, 255) as u8;
        uv[1] = (((VR * r + VG * g + VB * b + (HALF << 2)) >> 18) + 128).clamp(0, 255) as u8;
    }
}

/// The `w × h` picture at the top-left of an NV12 frame (planes as in
/// [`rgba_to_nv12`]), as opaque RGBA.
pub(crate) fn nv12_to_rgba(y_plane: &[u8], y_stride: usize, uv_plane: &[u8], uv_stride: usize, w: usize, h: usize) -> RgbaImage {
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let ys = &y_plane[row * y_stride..][..w];
        // A Cb/Cr pair for every two pixels, and the last one if `w` is odd.
        let uvs = &uv_plane[row / 2 * uv_stride..][..w.div_ceil(2) * 2];
        row_to_rgba(ys, uvs, &mut out[row * w * 4..(row + 1) * w * 4]);
    }
    RgbaImage::from_raw(w as u32, h as u32, out).expect("sized buffer")
}

/// A row of Y and the row of Cb/Cr pairs it shares as RGBA.
fn row_to_rgba(ys: &[u8], uvs: &[u8], dst: &mut [u8]) {
    // SAFETY: as in `rows_to_nv12`.
    #[cfg(target_arch = "x86_64")]
    let done = unsafe { sse2::row_to_rgba(ys, uvs, dst) };
    #[cfg(not(target_arch = "x86_64"))]
    let done = 0;
    scalar_row_to_rgba(&ys[done..], &uvs[done..], &mut dst[done * 4..]);
}

/// [`row_to_rgba`], a pixel at a time.
fn scalar_row_to_rgba(ys: &[u8], uvs: &[u8], dst: &mut [u8]) {
    for (col, px) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let y = KY * (ys[col] as i32 - 16);
        let (u, v) = (uvs[col / 2 * 2] as i32 - 128, uvs[col / 2 * 2 + 1] as i32 - 128);
        px[0] = ((y + KRV * v + HALF) >> 16).clamp(0, 255) as u8;
        px[1] = ((y - KGU * u - KGV * v + HALF) >> 16).clamp(0, 255) as u8;
        px[2] = ((y + KBU * u + HALF) >> 16).clamp(0, 255) as u8;
        px[3] = 255;
    }
}

/// The row conversions in SSE2, which every x86-64 CPU has: the scalar code's
/// 32-bit sums, 16 pixels at a time. A pixel's channels go in 16-bit pairs,
/// (R, B) and (G, A), or (Cb, Cr), so that one `_mm_madd_epi16` multiplies
/// both by their coefficients and adds the products. A coefficient too big for
/// 16 bits is split into one that fits and a power of two, a shift.
///
/// The intrinsics are safe to call only where `#[target_feature]` lists SSE2,
/// even though x86-64 builds always have it on, so every function here does.
#[cfg(target_arch = "x86_64")]
mod sse2 {
    use std::arch::x86_64::*;

    use super::{HALF, KBU, KGU, KGV, KRV, KY, UB, UG, UR, VB, VG, VR, YB, YG, YR};

    /// Two 16-bit coefficients in one 32-bit lane, `lo` in its low half, for
    /// `_mm_madd_epi16`.
    const fn pair(lo: i32, hi: i32) -> i32 {
        assert!(lo as i16 as i32 == lo && hi as i16 as i32 == hi, "a coefficient doesn't fit 16 bits");
        (((hi as u32) << 16) | (lo as u32 & 0xffff)) as i32
    }

    // For (R, B) and (G, A). G·YG = G·(YG − 2¹⁶) + (G << 16).
    const Y_RB: i32 = pair(YR, YB);
    const Y_G: i32 = pair(YG - (1 << 16), 0);
    const U_RB: i32 = pair(UR, UB);
    const U_G: i32 = pair(UG, 0);
    const V_RB: i32 = pair(VR, VB);
    const V_G: i32 = pair(VG, 0);

    // For (Y − 16, 0) and (Cb − 128, Cr − 128). KY·Y = (KY − 2¹⁶)·Y + (Y << 16),
    // KRV·Cr = (KRV − 2¹⁷)·Cr + (Cr << 17), −KGV·Cr = (2¹⁶ − KGV)·Cr − (Cr << 16),
    // KBU·Cb = (KBU − 2¹⁷)·Cb + (Cb << 17).
    const Y_Y: i32 = pair(KY - (1 << 16), 0);
    const R_UV: i32 = pair(0, KRV - (1 << 17));
    const G_UV: i32 = pair(-KGU, (1 << 16) - KGV);
    const B_UV: i32 = pair(KBU - (1 << 17), 0);

    #[target_feature(enable = "sse2")]
    fn load(bytes: &[u8; 16]) -> __m128i {
        // SAFETY: reads exactly `bytes`, which needn't be aligned.
        unsafe { _mm_loadu_si128(bytes.as_ptr().cast()) }
    }

    #[target_feature(enable = "sse2")]
    fn store(bytes: &mut [u8; 16], v: __m128i) {
        // SAFETY: writes exactly `bytes`, which needn't be aligned.
        unsafe { _mm_storeu_si128(bytes.as_mut_ptr().cast(), v) }
    }

    /// Convert the leading multiple of 16 pixels of two rows as
    /// `super::rows_to_nv12` does; returns how many pixels that was.
    #[target_feature(enable = "sse2")]
    pub(super) fn rows_to_nv12(top: &[u8], bottom: &[u8], y0: &mut [u8], y1: &mut [u8], uv: &mut [u8]) -> usize {
        let blocks = top.as_chunks::<64>().0.iter().zip(bottom.as_chunks::<64>().0);
        let rows = y0.as_chunks_mut::<16>().0.iter_mut().zip(y1.as_chunks_mut::<16>().0).zip(uv.as_chunks_mut::<16>().0);
        let mut done = 0;
        for ((top, bottom), ((y0, y1), uv)) in blocks.zip(rows) {
            let (top, bottom) = (Pixels::load(top), Pixels::load(bottom));
            store(y0, top.luma());
            store(y1, bottom.luma());
            store(uv, Pixels::chroma(&top, &bottom));
            done += 16;
        }
        done
    }

    /// Sixteen RGBA pixels, four to a vector, in 16-bit pairs.
    struct Pixels {
        /// R in the low half of each pixel's lane, B in the high.
        rb: [__m128i; 4],
        /// G in the low half, A in the high.
        ga: [__m128i; 4],
    }

    impl Pixels {
        #[target_feature(enable = "sse2")]
        fn load(px: &[u8; 64]) -> Self {
            let low_bytes = _mm_set1_epi32(0x00ff_00ff);
            let quarters = px.as_chunks::<16>().0;
            let v: [__m128i; 4] = std::array::from_fn(|i| load(&quarters[i]));
            Self { rb: v.map(|v| _mm_and_si128(v, low_bytes)), ga: v.map(|v| _mm_and_si128(_mm_srli_epi32(v, 8), low_bytes)) }
        }

        /// Their Y.
        #[target_feature(enable = "sse2")]
        fn luma(&self) -> __m128i {
            let y = |i: usize| {
                let products = _mm_add_epi32(_mm_madd_epi16(self.rb[i], _mm_set1_epi32(Y_RB)), _mm_madd_epi16(self.ga[i], _mm_set1_epi32(Y_G)));
                let sum = _mm_add_epi32(products, _mm_add_epi32(_mm_slli_epi32(self.ga[i], 16), _mm_set1_epi32(HALF)));
                _mm_srli_epi32(sum, 16)
            };
            let k16 = _mm_set1_epi16(16);
            _mm_packus_epi16(_mm_add_epi16(_mm_packs_epi32(y(0), y(1)), k16), _mm_add_epi16(_mm_packs_epi32(y(2), y(3)), k16))
        }

        /// The Cb/Cr pairs of the eight 2×2 blocks `top` and `bottom` make.
        #[target_feature(enable = "sse2")]
        fn chroma(top: &Self, bottom: &Self) -> __m128i {
            // The channel sums of the four blocks in vectors `i` and `i + 1`, still
            // in pairs: the rows added, then each even pixel to its odd neighbour.
            let sums = |t: &[__m128i; 4], b: &[__m128i; 4], i: usize| {
                let (left, right) = (_mm_castsi128_ps(_mm_add_epi16(t[i], b[i])), _mm_castsi128_ps(_mm_add_epi16(t[i + 1], b[i + 1])));
                let even = _mm_castps_si128(_mm_shuffle_ps(left, right, 0b10_00_10_00));
                let odd = _mm_castps_si128(_mm_shuffle_ps(left, right, 0b11_01_11_01));
                _mm_add_epi16(even, odd)
            };
            let four_blocks = |i: usize| {
                let (rb, ga) = (sums(&top.rb, &bottom.rb, i), sums(&top.ga, &bottom.ga, i));
                let channel = |k_rb: i32, k_g: i32| {
                    let products = _mm_add_epi32(_mm_madd_epi16(rb, _mm_set1_epi32(k_rb)), _mm_madd_epi16(ga, _mm_set1_epi32(k_g)));
                    _mm_srai_epi32(_mm_add_epi32(products, _mm_set1_epi32(HALF << 2)), 18)
                };
                let (u, v) = (channel(U_RB, U_G), channel(V_RB, V_G));
                // Cb Cr Cb Cr … in 16 bits.
                _mm_packs_epi32(_mm_unpacklo_epi32(u, v), _mm_unpackhi_epi32(u, v))
            };
            let k128 = _mm_set1_epi16(128);
            _mm_packus_epi16(_mm_add_epi16(four_blocks(0), k128), _mm_add_epi16(four_blocks(2), k128))
        }
    }

    /// Convert the leading multiple of 16 pixels of a row as
    /// `super::row_to_rgba` does; returns how many pixels that was.
    #[target_feature(enable = "sse2")]
    pub(super) fn row_to_rgba(ys: &[u8], uvs: &[u8], dst: &mut [u8]) -> usize {
        let blocks = ys.as_chunks::<16>().0.iter().zip(uvs.as_chunks::<16>().0);
        let mut done = 0;
        for ((ys, uvs), dst) in blocks.zip(dst.as_chunks_mut::<64>().0) {
            let (ys, uvs, zero) = (load(ys), load(uvs), _mm_setzero_si128());
            // Y − 16 of pixels 0–7 and 8–15, and Cb − 128, Cr − 128 of their pairs.
            let y = [_mm_unpacklo_epi8(ys, zero), _mm_unpackhi_epi8(ys, zero)].map(|y| _mm_sub_epi16(y, _mm_set1_epi16(16)));
            let uv = [_mm_unpacklo_epi8(uvs, zero), _mm_unpackhi_epi8(uvs, zero)].map(|uv| _mm_sub_epi16(uv, _mm_set1_epi16(128)));
            let ([r0, g0, b0], [r1, g1, b1]) = (rgb8(y[0], uv[0]), rgb8(y[1], uv[1]));
            let (r, g, b, a) = (_mm_packus_epi16(r0, r1), _mm_packus_epi16(g0, g1), _mm_packus_epi16(b0, b1), _mm_set1_epi8(-1));
            let (rg, ba) = ([_mm_unpacklo_epi8(r, g), _mm_unpackhi_epi8(r, g)], [_mm_unpacklo_epi8(b, a), _mm_unpackhi_epi8(b, a)]);
            let pixels = [
                _mm_unpacklo_epi16(rg[0], ba[0]),
                _mm_unpackhi_epi16(rg[0], ba[0]),
                _mm_unpacklo_epi16(rg[1], ba[1]),
                _mm_unpackhi_epi16(rg[1], ba[1]),
            ];
            for (dst, pixels) in dst.as_chunks_mut::<16>().0.iter_mut().zip(pixels) {
                store(dst, pixels);
            }
            done += 16;
        }
        done
    }

    /// R, G and B of eight pixels in 16 bits, not yet clamped: `y` holds their
    /// Y − 16, `uv` the Cb − 128, Cr − 128 of the four pairs they make.
    #[target_feature(enable = "sse2")]
    fn rgb8(y: __m128i, uv: __m128i) -> [__m128i; 3] {
        let half = _mm_set1_epi32(HALF);
        // Each pair's share, in 32-bit lanes.
        let cr = _mm_and_si128(uv, _mm_set1_epi32(!0xffff)); // Cr << 16
        let r = _mm_add_epi32(_mm_add_epi32(_mm_madd_epi16(uv, _mm_set1_epi32(R_UV)), _mm_slli_epi32(cr, 1)), half);
        let g = _mm_add_epi32(_mm_sub_epi32(_mm_madd_epi16(uv, _mm_set1_epi32(G_UV)), cr), half);
        let b = _mm_add_epi32(_mm_add_epi32(_mm_madd_epi16(uv, _mm_set1_epi32(B_UV)), _mm_slli_epi32(uv, 17)), half);
        // Each pixel's: KY·(Y − 16), for pixels 0–3 and 4–7.
        let zero = _mm_setzero_si128();
        let ky = |y: __m128i| _mm_add_epi32(_mm_madd_epi16(y, _mm_set1_epi32(Y_Y)), _mm_slli_epi32(y, 16));
        let y = [ky(_mm_unpacklo_epi16(y, zero)), ky(_mm_unpackhi_epi16(y, zero))];
        [r, g, b].map(|pairs| {
            // A pair's share goes to both its pixels.
            let low = _mm_srai_epi32(_mm_add_epi32(y[0], _mm_unpacklo_epi32(pairs, pairs)), 16);
            let high = _mm_srai_epi32(_mm_add_epi32(y[1], _mm_unpackhi_epi32(pairs, pairs)), 16);
            _mm_packs_epi32(low, high)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_nv12(img: &RgbaImage, pad: usize) -> (Vec<u8>, Vec<u8>, usize) {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let stride = w + pad;
        let (mut y, mut uv) = (vec![0; stride * h], vec![0; stride * h / 2]);
        rgba_to_nv12(img.as_raw(), w, h, &mut y, stride, &mut uv, stride);
        (y, uv, stride)
    }

    #[test]
    fn known_colors() {
        for (rgb, yuv) in [
            ([0, 0, 0], [16, 128, 128]),
            ([255, 255, 255], [235, 128, 128]),
            ([255, 0, 0], [63, 102, 240]),
            ([0, 0, 255], [32, 240, 118]),
        ] {
            let img = RgbaImage::from_pixel(2, 2, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
            let (y, uv, _) = to_nv12(&img, 0);
            assert_eq!([y[0], uv[0], uv[1]], yuv, "{rgb:?}");
            let back = nv12_to_rgba(&y, 2, &uv, 2, 2, 2);
            for c in 0..3 {
                assert!((back.get_pixel(1, 1)[c] as i32 - rgb[c] as i32).abs() <= 2, "{rgb:?} → {:?}", back.get_pixel(1, 1));
            }
        }
    }

    #[test]
    fn round_trips_a_smooth_picture_with_padded_rows() {
        let img = RgbaImage::from_fn(64, 32, |x, y| image::Rgba([(x * 4) as u8, (y * 8) as u8, 200 - x as u8, 255]));
        let (y, uv, stride) = to_nv12(&img, 16);
        let back = nv12_to_rgba(&y, stride, &uv, stride, 64, 32);
        let worst = img.pixels().zip(back.pixels()).flat_map(|(a, b)| (0..3).map(move |c| (a[c] as i32 - b[c] as i32).abs())).max();
        assert!(worst.unwrap() <= 6, "worst channel error {worst:?}");
    }

    /// `len` pseudo-random bytes (xorshift), the same for the same seed; with
    /// `extremes`, only 0 and 255, where sums and clamping are at their limits.
    fn noise(len: usize, seed: u64, extremes: bool) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(len);
        if extremes {
            bytes.iter_mut().for_each(|b| *b = if *b & 1 == 0 { 0 } else { 255 });
        }
        bytes
    }

    /// Where two rows first differ, if they do.
    fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
        a.iter().zip(b).position(|(x, y)| x != y).or((a.len() != b.len()).then(|| a.len().min(b.len())))
    }

    /// 1080p and 4K, a width that isn't a multiple of 16, and narrow pictures
    /// that leave every remainder of a 16-pixel block to the scalar code.
    fn sizes(narrow: impl Iterator<Item = usize>, h: usize) -> Vec<(usize, usize)> {
        let mut sizes = vec![(1920, 1080), (3840, 2160), (1366, 768)];
        sizes.extend(narrow.map(|w| (w, h)));
        sizes
    }

    /// Random frames give exactly the scalar code's bytes (on x86-64, SSE2 does
    /// most of the work), rows padded or not, and the padding is left alone.
    #[test]
    fn rgba_to_nv12_matches_the_scalar_code() {
        const PAD: u8 = 0xa5;
        for (w, h) in sizes((2..=50).step_by(2), 4) {
            for (pad, extremes) in [(0, false), (w % 7 + 2, false), (3, true)] {
                let (y_stride, uv_stride) = (w + pad, w + 2 * pad);
                let rgba = noise(w * h * 4, (w * h + pad) as u64, extremes);
                let (mut y, mut uv) = (vec![PAD; y_stride * h], vec![PAD; uv_stride * h / 2]);
                rgba_to_nv12(&rgba, w, h, &mut y, y_stride, &mut uv, uv_stride);
                for row in 0..h / 2 {
                    let (top, bottom) = rgba[2 * row * w * 4..(2 * row + 2) * w * 4].split_at(w * 4);
                    let (mut y0, mut y1, mut c) = (vec![0; w], vec![0; w], vec![0; w]);
                    scalar_rows_to_nv12(top, bottom, &mut y0, &mut y1, &mut c);
                    for (k, want) in [(2 * row, &y0), (2 * row + 1, &y1)] {
                        let at = first_difference(&y[k * y_stride..][..w], want);
                        assert_eq!(at, None, "{w}×{h}, padded {pad}: Y differs in row {k}");
                    }
                    let at = first_difference(&uv[row * uv_stride..][..w], &c);
                    assert_eq!(at, None, "{w}×{h}, padded {pad}: Cb/Cr differs in row {row}");
                }
                let padding = |plane: &[u8], stride: usize| plane.chunks(stride).all(|row| row[w..].iter().all(|&b| b == PAD));
                assert!(padding(&y, y_stride) && padding(&uv, uv_stride), "{w}×{h}, padded {pad}: padding written");
            }
        }
    }

    /// As above, back: random Y and Cb/Cr (far outside video range too, so that
    /// clamping is tested both ways), odd sizes and padded rows.
    #[test]
    fn nv12_to_rgba_matches_the_scalar_code() {
        let mut sizes = sizes(1..=40, 3);
        sizes.push((1919, 1079));
        for (w, h) in sizes {
            for (pad, extremes) in [(0, false), (w % 5 + 1, false), (2, true)] {
                let (y_stride, uv_stride) = (w + pad, w.div_ceil(2) * 2 + 2 * pad);
                let seed = (w * h + pad) as u64;
                let (y_plane, uv_plane) = (noise(y_stride * h, seed, extremes), noise(uv_stride * h.div_ceil(2), !seed, extremes));
                let rgba = nv12_to_rgba(&y_plane, y_stride, &uv_plane, uv_stride, w, h);
                assert_eq!(rgba.dimensions(), (w as u32, h as u32));
                for row in 0..h {
                    let mut want = vec![0; w * 4];
                    scalar_row_to_rgba(&y_plane[row * y_stride..][..w], &uv_plane[row / 2 * uv_stride..][..w.div_ceil(2) * 2], &mut want);
                    let at = first_difference(&rgba.as_raw()[row * w * 4..][..w * 4], &want);
                    assert_eq!(at, None, "{w}×{h}, padded {pad}: RGBA differs in row {row}");
                }
            }
        }
    }

    /// SSE2 converts every whole 16-pixel block, so the two tests above test
    /// it, and the scalar code only gets the rest of a row.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sse2_converts_whole_blocks() {
        for w in [2, 14, 16, 18, 1366, 1920] {
            let rgba = noise(w * 8, w as u64, false);
            let (top, bottom) = rgba.split_at(w * 4);
            let (mut y0, mut y1, mut uv, mut back) = (vec![0; w], vec![0; w], vec![0; w], vec![0; w * 4]);
            // SAFETY: SSE2 is part of x86-64.
            let done = unsafe { (sse2::rows_to_nv12(top, bottom, &mut y0, &mut y1, &mut uv), sse2::row_to_rgba(&y0, &uv, &mut back)) };
            assert_eq!(done, (w / 16 * 16, w / 16 * 16), "{w} pixels");
        }
    }
}
