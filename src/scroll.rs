//! Scrolling capture: repeatedly scroll the content under a region and stitch
//! the frames together by finding how far the content moved between them.

use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use image::RgbaImage;

pub enum ScrollMsg {
    Progress(String),
    Done(Result<RgbaImage>),
}

const MAX_STEPS: usize = 120;
const MAX_HEIGHT: u32 = 40_000;
const SAMPLES: usize = 64;

/// Region is in monitor-local logical points.
pub fn run(monitor_origin: (i32, i32), region: (u32, u32, u32, u32), progress: &Sender<ScrollMsg>) -> Result<RgbaImage> {
    use enigo::{Axis, Coordinate, Enigo, Mouse, Settings};

    let monitor = crate::capture::monitor_with_origin(monitor_origin).context("monitor not found")?;
    let d = crate::capture::dpi(&monitor);
    let phys = |v: u32| (v as f32 * d).round() as u32;
    let (x, y, w, h) = region;
    let (px, py, pw, ph_) = (phys(x), phys(y), phys(w).max(2), phys(h).max(2));
    let mut enigo = Enigo::new(&Settings::default())
        .context("input control unavailable — grant Accessibility permission")?;
    enigo.move_mouse(
        ((monitor_origin.0 as f32 + (x + w / 2) as f32) * d).round() as i32,
        ((monitor_origin.1 as f32 + (y + h / 2) as f32) * d).round() as i32,
        Coordinate::Abs,
    )?;
    thread::sleep(Duration::from_millis(150));

    let first = monitor.capture_region(px, py, pw, ph_)?;
    let ph = first.height();
    let mut prev = first.clone();
    // Strips to stitch, joined once at the end (re-copying the growing image every
    // step is quadratic), and how many bottom rows of `prev` are a fixed footer.
    let mut strips = vec![first];
    let mut height = ph;
    let mut footer = 0u32;
    let mut clicks = 3i32;

    for step in 0..MAX_STEPS {
        let _ = progress.send(ScrollMsg::Progress(format!("Scrolling… step {}, {} px", step + 1, height)));
        enigo.scroll(clicks, Axis::Vertical)?;
        thread::sleep(Duration::from_millis(450));
        let cur = monitor.capture_region(px, py, pw, ph_)?;

        let Some(m) = find_shift(&prev, &cur) else {
            log::info!("scroll capture: could not match frames, stopping");
            break;
        };
        if m.shift == 0 {
            break; // reached the end
        }
        if step == 0 {
            // Drop the fixed footer from the first frame; it is re-added at the end.
            footer = m.bottom;
            let body = image::imageops::crop_imm(&strips[0], 0, 0, strips[0].width(), ph - footer).to_image();
            strips[0] = body;
            height -= footer;
        }
        // New content is the `shift` rows just above the footer.
        let new_top = ph - m.bottom - m.shift;
        strips.push(image::imageops::crop_imm(&cur, 0, new_top, cur.width(), m.shift).to_image());
        height += m.shift;

        // Aim for roughly half a region of movement per step.
        let per_click = m.shift as f32 / clicks as f32;
        if per_click > 0.0 {
            clicks = ((ph as f32 * 0.5 / per_click) as i32).clamp(1, 30);
        }
        prev = cur;
        if height > MAX_HEIGHT {
            break;
        }
    }

    if footer > 0 {
        strips.push(image::imageops::crop_imm(&prev, 0, ph - footer, prev.width(), footer).to_image());
    }
    Ok(stack(&strips))
}

/// Join same-width strips top to bottom.
fn stack(strips: &[RgbaImage]) -> RgbaImage {
    let width = strips.first().map_or(0, |s| s.width());
    let mut raw = Vec::with_capacity(strips.iter().map(|s| s.as_raw().len()).sum());
    for s in strips {
        raw.extend_from_slice(s.as_raw());
    }
    let height = strips.iter().map(|s| s.height()).sum();
    RgbaImage::from_raw(width, height, raw).expect("strips share a width")
}

struct Shift {
    shift: u32,
    bottom: u32,
}

/// Per-row luminance signature, sampled across the width (skipping the
/// right edge where scrollbars live).
fn signatures(img: &RgbaImage) -> Vec<[u8; SAMPLES]> {
    let usable = (img.width() as f32 * 0.94) as u32;
    (0..img.height())
        .map(|y| {
            let mut row = [0u8; SAMPLES];
            for (i, v) in row.iter_mut().enumerate() {
                let x = (i as u32 * usable) / SAMPLES as u32;
                let p = img.get_pixel(x, y);
                *v = ((p[0] as u32 * 3 + p[1] as u32 * 6 + p[2] as u32) / 10) as u8;
            }
            row
        })
        .collect()
}

fn row_diff(a: &[u8; SAMPLES], b: &[u8; SAMPLES]) -> u32 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u32).sum()
}

/// How many rows the content moved between `prev` and `cur`, ignoring
/// fixed headers/footers. `shift == 0` means nothing moved.
fn find_shift(prev: &RgbaImage, cur: &RgbaImage) -> Option<Shift> {
    let (a, b) = (signatures(prev), signatures(cur));
    let h = a.len().min(b.len());
    let same = |i: usize| row_diff(&a[i], &b[i]) < SAMPLES as u32;

    let top = (0..h).take_while(|&i| same(i)).count();
    if top == h {
        return Some(Shift { shift: 0, bottom: 0 });
    }
    let limit = h * 2 / 5;
    let top = top.min(limit);
    let bottom = (0..h).rev().take_while(|&i| same(i)).count().min(limit);

    let band = &(top..h - bottom);
    let len = band.len();
    if len < 16 {
        return None;
    }
    let min_overlap = (len / 8).max(8);
    let mut best = (u64::MAX, 0usize);
    for d in 1..=(len - min_overlap) {
        let rows = len - d;
        let step = (rows / 200).max(1);
        let mut cost = 0u64;
        let mut n = 0u64;
        for i in (0..rows).step_by(step) {
            cost += row_diff(&a[band.start + i + d], &b[band.start + i]) as u64;
            n += 1;
        }
        let avg = cost * 1000 / (n * SAMPLES as u64);
        if avg < best.0 {
            best = (avg, d);
        }
    }
    // Average per-sample difference below ~3 levels counts as a match.
    (best.0 < 3000).then_some(Shift {
        shift: best.1 as u32,
        bottom: bottom as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tall "page" with a fixed header and footer, viewed through a window.
    fn page() -> RgbaImage {
        RgbaImage::from_fn(300, 3000, |x, y| {
            let v = ((x * 7 + y * 13) ^ (y * 31 / 7)) % 251;
            image::Rgba([v as u8, (v * 3 % 256) as u8, (y % 256) as u8, 255])
        })
    }

    fn view(page: &RgbaImage, scroll: u32) -> RgbaImage {
        let mut v = image::imageops::crop_imm(page, 0, scroll, 300, 400).to_image();
        for y in (0..40).chain(370..400) {
            for x in 0..300 {
                v.put_pixel(x, y, image::Rgba([20, 20, 90, 255]));
            }
        }
        v
    }

    #[test]
    fn finds_scroll_shift_with_sticky_header_and_footer() {
        let p = page();
        let m = find_shift(&view(&p, 0), &view(&p, 137)).expect("match");
        assert_eq!(m.shift, 137);
        assert_eq!(m.bottom, 30);
        assert_eq!(find_shift(&view(&p, 500), &view(&p, 500)).unwrap().shift, 0);
    }
}
