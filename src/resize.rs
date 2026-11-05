//! Resizing RGBA video frames: captures to the recording size, decoded video
//! to a GIF's size.
//!
//! pic-scale's SIMD resampling (NEON, or SSE4.1 / AVX2 when the CPU has them:
//! it checks at run time) takes a sixth to a tenth of the time of `image`'s
//! Triangle filter, which used to cost a full-screen Retina recording a third
//! of its frame rate.

use std::sync::Arc;

use image::RgbaImage;
use pic_scale::{ImageSize, ImageStore, ImageStoreMut, Resampling, ResamplingFunction, Scaler, WorkloadStrategy};

/// Resizes frames of one size to another, set up once for the pair so each
/// frame only pays for the resampling.
pub struct Resizer {
    from: (u32, u32),
    to: (u32, u32),
    /// The filter weights for `from` → `to`; `None` if pic-scale can't
    /// resample between them (a zero side), and `image` does instead.
    plan: Option<Arc<Resampling<u8, 4>>>,
    /// The plan's working memory, kept between frames.
    scratch: Vec<u8>,
}

impl Resizer {
    pub fn new(from: (u32, u32), to: (u32, u32)) -> Self {
        // Both filters widen with the shrink like `image`'s Triangle, which
        // recordings used before, so every source pixel counts. pic-scale fits
        // Bilinear's taps into round(2 × shrink) pixels, which leaves out a
        // neighbour that matters below a 1.25× shrink and when enlarging;
        // Mitchell–Netravalli has room for it there, at 1.5× the cost.
        let shrinks = |a: u32, b: u32| a as f64 >= b as f64 * 1.25;
        let filter = if shrinks(from.0, to.0) && shrinks(from.1, to.1) {
            ResamplingFunction::Bilinear
        } else {
            ResamplingFunction::MitchellNetravalli
        };
        // Frames are opaque: no alpha to premultiply. PreferSpeed's 16-bit
        // NEON arithmetic would brighten flat colours by up to 2 levels, for
        // 7% less time.
        let plan = Scaler::new(filter)
            .set_workload_strategy(WorkloadStrategy::PreferQuality)
            .plan_rgba_resampling(size(from), size(to), false)
            .inspect_err(|e| log::debug!("no resampling plan for {from:?} → {to:?}: {e}"))
            .ok();
        let scratch = plan.as_ref().map(|plan| plan.alloc_scratch()).unwrap_or_default();
        Self { from, to, plan, scratch }
    }

    /// The size of frames this was set up for.
    pub fn source(&self) -> (u32, u32) {
        self.from
    }

    /// The size frames are resized to.
    pub fn target(&self) -> (u32, u32) {
        self.to
    }

    /// `src` resized to the target size. A frame of another size than the
    /// one this was set up for is still resized, just without the setup.
    pub fn resize(&mut self, src: &RgbaImage) -> RgbaImage {
        let (w, h) = self.to;
        if src.dimensions() != self.from {
            return resize(src, w, h);
        }
        if self.from == self.to {
            return src.clone();
        }
        if let Some(plan) = &self.plan {
            let mut out = vec![0; w as usize * h as usize * 4];
            let resampled = (|| {
                let src = ImageStore::<u8, 4>::from_slice(src.as_raw(), self.from.0 as usize, self.from.1 as usize)?;
                let mut dst = ImageStoreMut::<u8, 4>::from_slice(&mut out, w as usize, h as usize)?;
                plan.resample_with_scratch(&src, &mut dst, &mut self.scratch)
            })();
            match resampled {
                Ok(()) => {
                    if let Some(image) = RgbaImage::from_raw(w, h, out) {
                        return image;
                    }
                }
                // Not expected: the sizes were checked when planning.
                Err(e) => log::debug!("resampling {:?} → {:?} failed: {e}", self.from, self.to),
            }
        }
        image::imageops::resize(src, w, h, image::imageops::FilterType::Triangle)
    }
}

/// `src` resized to `width × height`.
pub fn resize(src: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    Resizer::new(src.dimensions(), (width, height)).resize(src)
}

fn size((w, h): (u32, u32)) -> ImageSize {
    ImageSize::new(w as usize, h as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn psnr(a: &RgbaImage, b: &RgbaImage) -> f64 {
        assert_eq!(a.dimensions(), b.dimensions());
        let sum: f64 = a.as_raw().iter().zip(b.as_raw()).map(|(&p, &q)| (p as f64 - q as f64).powi(2)).sum();
        let mse = sum / a.as_raw().len() as f64;
        10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
    }

    /// Screen-like content of any size: the README's editor screenshot, tiled.
    fn screen(w: u32, h: u32) -> RgbaImage {
        let ui = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/images/editor.png")).unwrap().to_rgba8();
        RgbaImage::from_fn(w, h, |x, y| *ui.get_pixel(x % ui.width(), y % ui.height()))
    }

    fn triangle(src: &RgbaImage, w: u32, h: u32) -> RgbaImage {
        image::imageops::resize(src, w, h, image::imageops::FilterType::Triangle)
    }

    /// As close to `image`'s Triangle filter, which recordings used before, as
    /// its own rounding: at recording, GIF and thumbnail shrinks, around the
    /// 1.25× switch between filters, and enlarging.
    #[test]
    fn matches_image_triangle_quality() {
        let src = screen(1512, 982);
        for (w, h) in [(1280, 832), (961, 625), (640, 416), (378, 245), (1450, 942), (1210, 786), (1209, 785), (1512, 600), (1001, 982), (2001, 1301), (3024, 1964)] {
            let ours = Resizer::new(src.dimensions(), (w, h)).resize(&src);
            assert_eq!(ours.dimensions(), (w, h));
            let db = psnr(&ours, &triangle(&src, w, h));
            assert!(db >= 38.0, "1512×982 → {w}×{h}: {db:.1} dB");
        }
    }

    #[test]
    fn scales_tiny_and_odd_regions_to_exact_sizes() {
        // A tiny region scaled up to the encoders' 64 px minimum, the other way, and odd sides.
        for ((sw, sh), (w, h)) in [((41, 31), (84, 64)), ((16, 16), (64, 64)), ((84, 64), (41, 31)), ((333, 77), (65, 15)), ((7, 5), (7, 4))] {
            let src = screen(sw + 600, sh + 400);
            let src = image::imageops::crop_imm(&src, 600, 400, sw, sh).to_image();
            let ours = resize(&src, w, h);
            assert_eq!(ours.dimensions(), (w, h), "{sw}×{sh} → {w}×{h}");
            let db = psnr(&ours, &triangle(&src, w, h));
            assert!(db >= 30.0, "{sw}×{sh} → {w}×{h}: {db:.1} dB");
            assert!(ours.pixels().all(|p| p[3] == 255), "{sw}×{sh} → {w}×{h}: stays opaque");
        }
        // Flat colours stay exactly what they were, shrunk or enlarged.
        for c in [[10, 200, 30, 255], [128, 128, 128, 255], [1, 254, 77, 255]] {
            let flat = RgbaImage::from_pixel(97, 61, image::Rgba(c));
            for (w, h) in [(64, 40), (32, 20), (90, 60), (300, 200)] {
                assert!(resize(&flat, w, h).pixels().all(|p| p.0 == c), "{c:?} → {w}×{h}");
            }
        }
        let flat = RgbaImage::from_pixel(97, 61, image::Rgba([10, 200, 30, 255]));
        // Nothing to do, and nothing to resize to.
        assert_eq!(resize(&flat, 97, 61), flat);
        assert_eq!(resize(&flat, 0, 0).dimensions(), (0, 0));
    }

    /// One `Resizer` for a whole recording: each frame comes out as if
    /// resized on its own, also after a frame of another size.
    #[test]
    fn a_resizer_serves_many_frames() {
        let base = screen(800, 500);
        for to in [(512, 320), (700, 438)] {
            let mut resizer = Resizer::new((800, 500), to);
            assert_eq!((resizer.source(), resizer.target()), ((800, 500), to));
            for i in 0..20 {
                let frame = RgbaImage::from_fn(800, 500, |x, y| {
                    let p = *base.get_pixel((x + i * 13) % 800, y);
                    if (i * 30..i * 30 + 40).contains(&x) { image::Rgba([255, 214, 0, 255]) } else { p }
                });
                assert_eq!(resizer.resize(&frame), resize(&frame, to.0, to.1), "frame {i} → {to:?}");
                if i == 10 {
                    let other = screen(640, 480);
                    assert_eq!(resizer.resize(&other), resize(&other, to.0, to.1));
                }
            }
        }
    }

    /// Per-frame resize times against `image`'s Triangle, best and median.
    /// `cargo test --release resize_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn resize_speed() {
        let time = |n: usize, f: &mut dyn FnMut() -> RgbaImage| {
            let mut times: Vec<Duration> = (0..n)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(f());
                    t.elapsed()
                })
                .collect();
            times.sort();
            (times[0].as_secs_f64() * 1e3, times[n / 2].as_secs_f64() * 1e3)
        };
        let pairs = [((3024, 1964), (1920, 1246)), ((2560, 1600), (1920, 1200)), ((5120, 2880), (1920, 1080)), ((2000, 1300), (1920, 1248)), ((1920, 1200), (1280, 800)), ((1920, 1080), (960, 540))];
        for ((sw, sh), (w, h)) in pairs {
            let src = screen(sw, sh);
            let (old_min, old_med) = time(9, &mut || triangle(&src, w, h));
            let mut resizer = Resizer::new((sw, sh), (w, h));
            let (new_min, new_med) = time(41, &mut || resizer.resize(&src));
            println!(
                "{sw}×{sh} → {w}×{h}: Triangle {old_min:.1} ms (median {old_med:.1}), Resizer {new_min:.2} ms (median {new_med:.2}): {:.1}× faster",
                old_med / new_med
            );
        }
    }
}
