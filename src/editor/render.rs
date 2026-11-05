//! Flattens a base image plus annotations into final pixels (for save/copy).

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, ScaleFont};
use image::{Rgba as Px, RgbaImage};
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

use super::model::{Annotation, BlurStyle, Effects, Kind, P, Rgba, StampSymbol, arrow_geometry, contrast_on, ellipse_points, norm, stamp_symbol};

/// Ubuntu Bold from Google Fonts (Ubuntu Font Licence, see assets/fonts/UFL.txt),
/// bundled so annotations render identically on every platform.
const ANNOTATION_FONT: &[u8] = include_bytes!("../../assets/fonts/Ubuntu-Bold.ttf");

/// Bytes of the font used for text annotations, both on screen and on export.
pub fn annotation_font_bytes() -> &'static [u8] {
    ANNOTATION_FONT
}

fn font() -> FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(ANNOTATION_FONT).expect("bundled annotation font"))
        .clone()
}

/// Ubuntu has no Hebrew, so those glyphs come from Noto Sans Hebrew Bold (SIL OFL).
const HEBREW_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSansHebrew-Bold.ttf");

pub fn hebrew_font_bytes() -> &'static [u8] {
    HEBREW_FONT
}

fn hebrew_font() -> FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(HEBREW_FONT).expect("bundled Hebrew font"))
        .clone()
}

/// Font size as egui uses it (em size), converted to ab_glyph's glyph-height
/// scale, so exported text matches the on-screen editor exactly. (Not
/// `pt_to_px_scale`: that assumes 96 dpi and comes out 4/3 too large.)
fn px_scale(f: &FontRef<'_>, size: f32) -> ab_glyph::PxScale {
    let units_per_em = f.units_per_em().unwrap_or(1000.0);
    ab_glyph::PxScale::from(size * f.height_unscaled() / units_per_em)
}

pub fn line_height(size: f32) -> f32 {
    let f = font();
    let s = f.as_scaled(px_scale(&f, size));
    s.height() + s.line_gap()
}

fn mirror(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        _ => c,
    }
}

/// One character of a line, in left-to-right display order.
pub struct VisualChar {
    /// Index of this character in the logical (typing-order) line.
    pub logical: usize,
    /// Already mirrored for brackets when it sits in a right-to-left run.
    pub ch: char,
    pub rtl: bool,
}

fn has_rtl(line: &str) -> bool {
    line.chars().any(|c| ('\u{0590}'..='\u{08FF}').contains(&c) || ('\u{FB1D}'..='\u{FEFF}').contains(&c))
}

/// One line of text in left-to-right display order. Fonts and egui both lay
/// glyphs out in the order given, so right-to-left runs (Hebrew) have to be
/// reordered up front or they come out reversed.
pub fn visual_chars(line: &str) -> Vec<VisualChar> {
    if !has_rtl(line) {
        return line.chars().enumerate().map(|(logical, ch)| VisualChar { logical, ch, rtl: false }).collect();
    }
    // Byte offset -> char index.
    let mut char_at = vec![0; line.len() + 1];
    for (ci, (bi, c)) in line.char_indices().enumerate() {
        char_at[bi..bi + c.len_utf8()].fill(ci);
    }
    let info = unicode_bidi::BidiInfo::new(line, None);
    let mut out = Vec::new();
    for para in &info.paragraphs {
        let (levels, runs) = info.visual_runs(para, para.range.clone());
        for run in runs {
            let rtl = levels[run.start].is_rtl();
            let chars: Vec<(usize, char)> = line[run.clone()].char_indices().map(|(bi, c)| (char_at[run.start + bi], c)).collect();
            let push = |(logical, ch): (usize, char)| VisualChar { logical, ch: if rtl { mirror(ch) } else { ch }, rtl };
            if rtl {
                out.extend(chars.into_iter().rev().map(push));
            } else {
                out.extend(chars.into_iter().map(push));
            }
        }
    }
    out
}

#[cfg(test)]
pub fn visual_order(line: &str) -> String {
    visual_chars(line).into_iter().map(|v| v.ch).collect()
}

pub struct PlacedGlyph {
    font: usize,
    glyph: ab_glyph::Glyph,
    pub ch: char,
    /// Left edge, in px from the start of the line.
    pub x: f32,
    pub advance: f32,
    /// Index of this character in the logical (typing-order) line.
    pub logical: usize,
    pub rtl: bool,
}

/// Lay out one line (already split on '\n') at `size`, starting at x = 0:
/// glyphs in visual order with per-character font fallback, plus the width.
/// Export and the on-screen editor both draw from this, so they always match.
pub fn layout_line(line: &str, size: f32) -> (Vec<PlacedGlyph>, f32) {
    let fonts = [font(), hebrew_font()];
    let mut placed = Vec::new();
    let mut caret = 0.0;
    let mut prev: Option<(usize, ab_glyph::GlyphId)> = None;
    for vc in visual_chars(line) {
        let c = vc.ch;
        let fi = if fonts[0].glyph_id(c).0 == 0 && fonts[1].glyph_id(c).0 != 0 { 1 } else { 0 };
        let scale = px_scale(&fonts[fi], size);
        let s = fonts[fi].as_scaled(scale);
        let id = s.glyph_id(c);
        if let Some((pf, p)) = prev
            && pf == fi
        {
            caret += s.kern(p, id);
        }
        let advance = s.h_advance(id);
        placed.push(PlacedGlyph {
            font: fi,
            glyph: id.with_scale_and_position(scale, ab_glyph::point(caret, 0.0)),
            ch: c,
            x: caret,
            advance,
            logical: vc.logical,
            rtl: vc.rtl,
        });
        caret += advance;
        prev = Some((fi, id));
    }
    (placed, caret)
}

/// Width and height of (possibly multi-line) text at `size` px.
pub fn measure_text(text: &str, size: f32) -> (f32, f32) {
    let lines: Vec<&str> = text.split('\n').collect();
    let w = lines.iter().map(|l| layout_line(l, size).1).fold(0.0, f32::max);
    (w, lines.len() as f32 * line_height(size))
}

fn to_pixmap(img: &RgbaImage) -> Pixmap {
    let mut pm = Pixmap::new(img.width(), img.height()).expect("pixmap size");
    for (dst, src) in pm.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(img.as_raw().as_chunks::<4>().0) {
        let a = src[3] as u32;
        dst[0] = (src[0] as u32 * a / 255) as u8;
        dst[1] = (src[1] as u32 * a / 255) as u8;
        dst[2] = (src[2] as u32 * a / 255) as u8;
        dst[3] = src[3];
    }
    pm
}

fn from_pixmap(pm: &Pixmap) -> RgbaImage {
    let mut data = Vec::with_capacity(pm.pixels().len() * 4);
    for p in pm.pixels() {
        let c = p.demultiply();
        data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    RgbaImage::from_raw(pm.width(), pm.height(), data).expect("pixmap dims")
}

/// Apply pixel-changing annotations (blur, spotlight, magnifier, fill) in order.
pub fn apply_raster(base: &RgbaImage, anns: &[Annotation]) -> RgbaImage {
    let mut img = base.clone();
    for ann in anns.iter().filter(|a| a.is_raster()) {
        match &ann.kind {
            Kind::Blur { a, b, style, strength } => match style {
                BlurStyle::Pixelate => pixelate(&mut img, *a, *b, *strength),
                BlurStyle::Gaussian => gaussian(&mut img, *a, *b, *strength),
            },
            Kind::Spotlight { a, b } => spotlight(&mut img, *a, *b),
            Kind::Magnify { a, b, zoom } => magnify(&mut img, *a, *b, *zoom),
            Kind::Fill { seed, tolerance, .. } => {
                if let Some((mask, _)) = flood_region(&img, *seed, *tolerance) {
                    for (i, px) in img.pixels_mut().enumerate() {
                        if mask[i / 64] >> (i % 64) & 1 == 1 {
                            *px = Px(ann.color);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    img
}

/// `(x0, y0, x1, y1)` in pixels, `x1`/`y1` exclusive.
pub type PixelBounds = (u32, u32, u32, u32);

/// The pixels a paint-bucket click at `seed` covers: those connected to it
/// whose every channel is within `tolerance` of the seed pixel (compared with
/// the seed, not neighbours, so gradients don't leak). Returns a bit mask
/// (row-major) and the bounds `(x0, y0, x1, y1)`, exclusive.
pub fn flood_region(img: &RgbaImage, seed: P, tolerance: f32) -> Option<(Vec<u64>, PixelBounds)> {
    let (w, h) = img.dimensions();
    let (sx, sy) = (seed.x.floor(), seed.y.floor());
    if sx < 0.0 || sy < 0.0 || sx >= w as f32 || sy >= h as f32 {
        return None;
    }
    let (sx, sy) = (sx as u32, sy as u32);
    let target = img.get_pixel(sx, sy).0;
    let tol = tolerance.clamp(0.0, 255.0) as i16;
    let raw = img.as_raw();
    let at = |x: u32, y: u32| y as usize * w as usize + x as usize;
    let matches = |i: usize| (0..4).all(|c| (raw[i * 4 + c] as i16 - target[c] as i16).abs() <= tol);
    let mut mask = vec![0u64; (w as usize * h as usize).div_ceil(64)];
    let is_set = |m: &[u64], i: usize| m[i / 64] >> (i % 64) & 1 == 1;
    let (mut x0, mut y0, mut x1, mut y1) = (sx, sy, sx + 1, sy + 1);
    let mut stack = vec![(sx, sy)];
    while let Some((x, y)) = stack.pop() {
        let i = at(x, y);
        if is_set(&mask, i) || !matches(i) {
            continue;
        }
        // Grow the run left and right, mark it, then queue the rows above and below.
        let (mut l, mut r) = (x, x);
        while l > 0 && !is_set(&mask, at(l - 1, y)) && matches(at(l - 1, y)) {
            l -= 1;
        }
        while r + 1 < w && !is_set(&mask, at(r + 1, y)) && matches(at(r + 1, y)) {
            r += 1;
        }
        for xx in l..=r {
            let j = at(xx, y);
            mask[j / 64] |= 1 << (j % 64);
        }
        (x0, x1, y0, y1) = (x0.min(l), x1.max(r + 1), y0.min(y), y1.max(y + 1));
        for ny in [y.checked_sub(1), (y + 1 < h).then_some(y + 1)].into_iter().flatten() {
            let mut in_run = false;
            for xx in l..=r {
                let j = at(xx, ny);
                let open = !is_set(&mask, j) && matches(j);
                if open && !in_run {
                    stack.push((xx, ny));
                }
                in_run = open;
            }
        }
    }
    Some((mask, (x0, y0, x1, y1)))
}

/// A loupe: inside the ellipse `a`–`b`, show the pixels around its centre `zoom`× larger.
fn magnify(img: &mut RgbaImage, a: P, b: P, zoom: f32) {
    let (min, max) = norm(a, b);
    let (cx, cy) = ((min.x + max.x) / 2.0, (min.y + max.y) / 2.0);
    let (rx, ry) = ((max.x - min.x) / 2.0, (max.y - min.y) / 2.0);
    let zoom = zoom.max(1.0);
    let (x0, y0, x1, y1) = clamp_rect(img, a, b);
    if rx < 1.0 || ry < 1.0 || x1 <= x0 || y1 <= y0 {
        return;
    }
    // Sample from a copy: the loupe reads pixels it is also overwriting.
    let src = image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
    for y in y0..y1 {
        for x in x0..x1 {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            if (dx / rx).powi(2) + (dy / ry).powi(2) > 1.0 {
                continue;
            }
            // Nearest pixel keeps screenshots crisp when enlarged.
            let sx = ((cx + dx / zoom).floor() as i64 - x0 as i64).clamp(0, src.width() as i64 - 1) as u32;
            let sy = ((cy + dy / zoom).floor() as i64 - y0 as i64).clamp(0, src.height() as i64 - 1) as u32;
            img.put_pixel(x, y, *src.get_pixel(sx, sy));
        }
    }
}

fn clamp_rect(img: &RgbaImage, a: P, b: P) -> (u32, u32, u32, u32) {
    let (min, max) = norm(a, b);
    let x0 = (min.x.max(0.0) as u32).min(img.width());
    let y0 = (min.y.max(0.0) as u32).min(img.height());
    let x1 = (max.x.max(0.0) as u32).min(img.width());
    let y1 = (max.y.max(0.0) as u32).min(img.height());
    (x0, y0, x1, y1)
}

/// Effect size in pixels at `strength` (1–100): at 50 it's the original
/// pixelate block, scaled to the image so it reads the same at any resolution.
fn blur_radius(img: &RgbaImage, strength: f32) -> f32 {
    let base = (img.width().max(img.height()) / 120).max(8) as f32;
    base * strength.clamp(1.0, 100.0) / crate::editor::model::DEFAULT_BLUR_STRENGTH
}

fn pixelate(img: &mut RgbaImage, a: P, b: P, strength: f32) {
    let (x0, y0, x1, y1) = clamp_rect(img, a, b);
    let block = (blur_radius(img, strength).round() as u32).max(2);
    let mut by = y0;
    while by < y1 {
        let mut bx = x0;
        let bh = block.min(y1 - by);
        while bx < x1 {
            let bw = block.min(x1 - bx);
            let mut sum = [0u64; 4];
            for y in by..by + bh {
                for x in bx..bx + bw {
                    let p = img.get_pixel(x, y);
                    for c in 0..4 {
                        sum[c] += p[c] as u64;
                    }
                }
            }
            let n = (bw * bh) as u64;
            let avg = Px([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, (sum[3] / n) as u8]);
            for y in by..by + bh {
                for x in bx..bx + bw {
                    img.put_pixel(x, y, avg);
                }
            }
            bx += block;
        }
        by += block;
    }
}

/// Smooth blur inside `a`–`b`. Strong blurs run on a downscaled copy (a large
/// Gaussian kernel at full size is slow) and are scaled back up, which looks
/// the same once blurred.
fn gaussian(img: &mut RgbaImage, a: P, b: P, strength: f32) {
    let (x0, y0, x1, y1) = clamp_rect(img, a, b);
    let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if w < 2 || h < 2 {
        return;
    }
    let sigma = (blur_radius(img, strength) * 0.6).max(1.0);
    let region = image::imageops::crop_imm(img, x0, y0, w, h).to_image();
    let factor = (sigma / 3.0).floor().max(1.0);
    let (sw, sh) = (((w as f32 / factor) as u32).max(2), ((h as f32 / factor) as u32).max(2));
    let small = if factor > 1.0 { image::imageops::resize(&region, sw, sh, image::imageops::FilterType::Triangle) } else { region };
    let blurred = imageproc::filter::gaussian_blur_f32(&small, sigma / factor);
    let out = if factor > 1.0 { image::imageops::resize(&blurred, w, h, image::imageops::FilterType::Triangle) } else { blurred };
    image::imageops::replace(img, &out, x0 as i64, y0 as i64);
}

fn spotlight(img: &mut RgbaImage, a: P, b: P) {
    let (x0, y0, x1, y1) = clamp_rect(img, a, b);
    for (x, y, p) in img.enumerate_pixels_mut() {
        if x < x0 || x >= x1 || y < y0 || y >= y1 {
            for c in 0..3 {
                p[c] = (p[c] as f32 * 0.4) as u8;
            }
        }
    }
}

fn paint(color: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(color[0], color[1], color[2], color[3]);
    p.anti_alias = true;
    p
}

fn stroke(width: f32) -> Stroke {
    Stroke {
        width: width.max(0.5),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

fn polyline(pts: &[P], close: bool) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let first = pts.first()?;
    pb.move_to(first.x, first.y);
    if pts.len() == 1 {
        // A single dab: a zero-length segment renders as a round dot.
        pb.line_to(first.x + 0.01, first.y);
    }
    for p in &pts[1..] {
        pb.line_to(p.x, p.y);
    }
    if close {
        pb.close();
    }
    pb.finish()
}

pub const HIGHLIGHT_ALPHA: u8 = 110;
pub const HIGHLIGHT_WIDTH_FACTOR: f32 = 4.0;

fn draw_vector(pm: &mut Pixmap, ann: &Annotation) {
    let t = Transform::identity();
    let color = ann.color;
    match &ann.kind {
        Kind::Arrow { a, b } => {
            let (end, head) = arrow_geometry(*a, *b, ann.width);
            if let Some(path) = polyline(&[*a, end], false) {
                pm.stroke_path(&path, &paint(color), &stroke(ann.width), t, None);
            }
            if let Some(path) = polyline(&head, true) {
                pm.fill_path(&path, &paint(color), FillRule::Winding, t, None);
            }
        }
        Kind::Line { a, b } => {
            if let Some(path) = polyline(&[*a, *b], false) {
                pm.stroke_path(&path, &paint(color), &stroke(ann.width), t, None);
            }
        }
        Kind::Rect { a, b } => {
            let (min, max) = norm(*a, *b);
            let pts = [min, P::new(max.x, min.y), max, P::new(min.x, max.y)];
            if let Some(path) = polyline(&pts, true) {
                if ann.filled {
                    pm.fill_path(&path, &paint(color), FillRule::Winding, t, None);
                } else {
                    pm.stroke_path(&path, &paint(color), &stroke(ann.width), t, None);
                }
            }
        }
        Kind::Ellipse { a, b } => {
            if let Some(path) = polyline(&ellipse_points(*a, *b, 96), true) {
                if ann.filled {
                    pm.fill_path(&path, &paint(color), FillRule::Winding, t, None);
                } else {
                    pm.stroke_path(&path, &paint(color), &stroke(ann.width), t, None);
                }
            }
        }
        Kind::Pen { pts } => {
            if let Some(path) = polyline(pts, false) {
                pm.stroke_path(&path, &paint(color), &stroke(ann.width), t, None);
            }
        }
        Kind::Highlight { pts } => {
            let c = [color[0], color[1], color[2], HIGHLIGHT_ALPHA];
            if let Some(path) = polyline(pts, false) {
                let mut s = stroke(ann.width * HIGHLIGHT_WIDTH_FACTOR);
                s.line_cap = LineCap::Square;
                pm.stroke_path(&path, &paint(c), &s, t, None);
            }
        }
        Kind::Text { pos, text, size, background } => {
            let text_color = if *background {
                let pad = size * 0.35;
                let (w, h) = measure_text(text, *size);
                if let Some(r) = tiny_skia::Rect::from_xywh(pos.x - pad, pos.y - pad, w + pad * 2.0, h + pad * 2.0) {
                    let path = crate::icon::rounded_rect(r, pad);
                    pm.fill_path(&path, &paint(color), FillRule::Winding, t, None);
                }
                contrast_on(color)
            } else {
                color
            };
            draw_text(pm, *pos, text, *size, text_color);
        }
        Kind::Step { pos, n, radius } => {
            if let Some(path) = PathBuilder::from_circle(pos.x, pos.y, *radius) {
                pm.fill_path(&path, &paint(color), FillRule::Winding, t, None);
            }
            let label = n.to_string();
            let size = radius * 1.15;
            let (w, h) = measure_text(&label, size);
            draw_text(pm, P::new(pos.x - w / 2.0, pos.y - h / 2.0), &label, size, contrast_on(color));
        }
        Kind::Image { pos, w, h, img } => {
            if img.width() == 0 || img.height() == 0 || *w < 1.0 || *h < 1.0 {
                return;
            }
            let src = to_pixmap(img);
            let sx = w / img.width() as f32;
            let sy = h / img.height() as f32;
            let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
            pm.draw_pixmap(0, 0, src.as_ref(), &paint, Transform::from_row(sx, 0.0, 0.0, sy, pos.x, pos.y), None);
        }
        Kind::Magnify { a, b, .. } => {
            // The ring; the enlarged pixels come from `apply_raster`.
            if let Some(path) = polyline(&ellipse_points(*a, *b, 96), true) {
                pm.stroke_path(&path, &paint(color), &stroke(ann.width), t, None);
            }
        }
        Kind::Stamp { pos, size, stamp } => {
            if let Some(path) = PathBuilder::from_circle(pos.x, pos.y, size / 2.0) {
                pm.fill_path(&path, &paint(color), FillRule::Winding, t, None);
            }
            let ink = contrast_on(color);
            for part in stamp_symbol(*stamp, *pos, *size) {
                match part {
                    StampSymbol::Strokes(lines, width) => {
                        for line in lines {
                            if let Some(path) = polyline(&line, false) {
                                pm.stroke_path(&path, &paint(ink), &stroke(width), t, None);
                            }
                        }
                    }
                    StampSymbol::Fill(ref pts) | StampSymbol::Cut(ref pts) => {
                        let c = if matches!(part, StampSymbol::Cut(_)) { color } else { ink };
                        if let Some(path) = polyline(&pts[1..], true) {
                            pm.fill_path(&path, &paint(c), FillRule::Winding, t, None);
                        }
                    }
                    StampSymbol::Glyph(g, size) => {
                        let (w, h) = measure_text(g, size);
                        draw_text(pm, P::new(pos.x - w / 2.0, pos.y - h / 2.0), g, size, ink);
                    }
                }
            }
        }
        Kind::Blur { .. } | Kind::Spotlight { .. } | Kind::Fill { .. } => {}
    }
}

pub(crate) fn draw_text(pm: &mut Pixmap, pos: P, text: &str, size: f32, color: Rgba) {
    let fonts = [font(), hebrew_font()];
    let f = &fonts[0];
    let s = f.as_scaled(px_scale(f, size));
    let lh = line_height(size);
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for (li, line) in text.split('\n').enumerate() {
        let baseline = pos.y + li as f32 * lh + s.ascent();
        for PlacedGlyph { font, mut glyph, .. } in layout_line(line, size).0 {
            glyph.position = ab_glyph::point(pos.x + glyph.position.x, baseline);
            let Some(outline) = fonts[font].outline_glyph(glyph) else { continue };
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let x = bounds.min.x as i32 + gx as i32;
                let y = bounds.min.y as i32 + gy as i32;
                if x < 0 || y < 0 || x >= pw || y >= ph {
                    return;
                }
                // usize: y * pw overflows i32 on tall canvases (up to 16000x40000).
                let i = (y as usize * pw as usize + x as usize) * 4;
                let a = cov.clamp(0.0, 1.0) * color[3] as f32 / 255.0;
                for c in 0..3 {
                    let src = color[c] as f32 * a;
                    data[i + c] = (src + data[i + c] as f32 * (1.0 - a)).round() as u8;
                }
                data[i + 3] = (255.0 * a + data[i + 3] as f32 * (1.0 - a)).round() as u8;
            });
        }
    }
}

/// Full export: raster effects, vector annotations, then border/shadow.
pub fn render(base: &RgbaImage, anns: &[Annotation], effects: &Effects) -> RgbaImage {
    let rastered = apply_raster(base, anns);
    let mut pm = to_pixmap(&rastered);
    // Every mark in order: raster ones only add their outline (the magnifier ring).
    for ann in anns {
        draw_vector(&mut pm, ann);
    }
    apply_effects(from_pixmap(&pm), effects)
}

pub fn apply_effects(mut img: RgbaImage, fx: &Effects) -> RgbaImage {
    let bw = fx.border_width.round() as u32;
    if bw > 0 {
        let mut framed = RgbaImage::from_pixel(img.width() + bw * 2, img.height() + bw * 2, Px(fx.border_color));
        image::imageops::overlay(&mut framed, &img, bw as i64, bw as i64);
        img = framed;
    }
    if fx.shadow {
        let m = (img.width().max(img.height()) / 40).clamp(12, 48);
        let (off, sigma) = (m / 3, m as f32 / 3.0);
        let (w, h) = (img.width() + m * 2, img.height() + m * 2);
        let mut mask = image::GrayImage::new(w, h);
        for y in 0..img.height() {
            for x in 0..img.width() {
                mask.put_pixel(x + m, y + m + off, image::Luma([img.get_pixel(x, y)[3]]));
            }
        }
        let mask = imageproc::filter::gaussian_blur_f32(&mask, sigma);
        let mut out = RgbaImage::from_fn(w, h, |x, y| Px([0, 0, 0, (mask.get_pixel(x, y)[0] as f32 * 0.45) as u8]));
        image::imageops::overlay(&mut out, &img, m as i64, m as i64);
        img = out;
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(kind: Kind) -> Annotation {
        Annotation { kind, color: [235, 59, 52, 255], width: 6.0, filled: false }
    }

    #[test]
    fn blur_styles_and_strength() {
        // Vertical stripes, so any blur or pixelation visibly changes pixels.
        let img = RgbaImage::from_fn(240, 240, |x, _| if x % 4 < 2 { Px([0, 0, 0, 255]) } else { Px([255, 255, 255, 255]) });
        let blur = |style, strength| Annotation {
            kind: Kind::Blur { a: P::new(40.0, 40.0), b: P::new(200.0, 200.0), style, strength },
            color: [30, 136, 229, 255],
            width: 1.0,
            filled: false,
        };
        for style in BlurStyle::ALL {
            let out = apply_raster(&img, &[blur(style, 50.0)]);
            assert_ne!(out.get_pixel(120, 120), img.get_pixel(120, 120), "{style:?} changes the inside");
            assert_eq!(out.get_pixel(10, 10), img.get_pixel(10, 10), "{style:?} leaves the outside");
        }
        // Pixel blocks grow with strength: count color runs along one row.
        let runs = |im: &RgbaImage| (41..200).filter(|&x| im.get_pixel(x, 100) != im.get_pixel(x - 1, 100)).count();
        let (weak, strong) = (apply_raster(&img, &[blur(BlurStyle::Pixelate, 20.0)]), apply_raster(&img, &[blur(BlurStyle::Pixelate, 100.0)]));
        assert!(runs(&strong) < runs(&weak), "stronger pixelate means bigger blocks");
    }

    #[test]
    fn old_projects_load_blur_as_pixelate() {
        let json = r#"{"kind":{"Blur":{"a":{"x":0.0,"y":0.0},"b":{"x":10.0,"y":10.0}}},"color":[0,0,0,255],"width":1.0,"filled":false}"#;
        let ann: Annotation = serde_json::from_str(json).unwrap();
        assert!(matches!(ann.kind, Kind::Blur { style: BlurStyle::Pixelate, strength, .. } if strength == 50.0));
    }

    #[test]
    fn paint_bucket_fills_only_the_connected_area() {
        // White canvas with a closed black ring; fill inside the ring.
        let mut img = RgbaImage::from_pixel(60, 60, Px([255, 255, 255, 255]));
        for i in 10..50 {
            for (x, y) in [(i, 10), (i, 49), (10, i), (49, i)] {
                img.put_pixel(x, y, Px([0, 0, 0, 255]));
            }
        }
        let fill = Annotation {
            kind: Kind::Fill { seed: P::new(30.0, 30.0), tolerance: 32.0, area: (P::new(0.0, 0.0), P::new(1.0, 1.0)) },
            color: [30, 136, 229, 255],
            width: 1.0,
            filled: true,
        };
        let out = render(&img, &[fill], &Effects::default());
        assert_eq!(out.get_pixel(30, 30).0, [30, 136, 229, 255], "inside filled");
        assert_eq!(out.get_pixel(10, 30).0, [0, 0, 0, 255], "ring kept");
        assert_eq!(out.get_pixel(5, 5).0, [255, 255, 255, 255], "outside untouched");
        let (_, bounds) = flood_region(&img, P::new(30.0, 30.0), 32.0).unwrap();
        assert_eq!(bounds, (11, 11, 49, 49));
        assert!(flood_region(&img, P::new(-1.0, 5.0), 32.0).is_none());
    }

    #[test]
    fn magnifier_enlarges_inside_the_loupe_only() {
        // A 2x2 dark dot at the centre of a white image, magnified 3x.
        let mut img = RgbaImage::from_pixel(80, 80, Px([255, 255, 255, 255]));
        for (x, y) in [(39, 39), (40, 39), (39, 40), (40, 40)] {
            img.put_pixel(x, y, Px([0, 0, 0, 255]));
        }
        let corner = Px([200, 0, 0, 255]);
        img.put_pixel(0, 0, corner);
        let loupe = Annotation { kind: Kind::Magnify { a: P::new(20.0, 20.0), b: P::new(60.0, 60.0), zoom: 3.0 }, color: [0, 0, 0, 255], width: 0.5, filled: false };
        let out = apply_raster(&img, &[loupe]);
        let dark = |im: &RgbaImage| im.pixels().filter(|p| p.0 == [0, 0, 0, 255]).count();
        assert_eq!(dark(&img), 4);
        assert_eq!(dark(&out), 36, "2x2 dot becomes 6x6");
        assert_eq!(*out.get_pixel(0, 0), corner, "outside the loupe untouched");
    }

    #[test]
    fn stamps_draw_inside_their_badge() {
        let base = RgbaImage::from_pixel(120, 120, Px([255, 255, 255, 255]));
        for stamp in super::super::model::Stamp::ALL {
            let mut a = ann(Kind::Stamp { pos: P::new(60.0, 60.0), size: 80.0, stamp });
            a.color = [46, 184, 92, 255];
            let out = render(&base, &[a], &Effects::default());
            // Badge colour at its edge, the white symbol somewhere inside, nothing outside.
            assert_eq!(out.get_pixel(60, 24).0, [46, 184, 92, 255], "{stamp:?} badge");
            let symbol = (30..90).flat_map(|y| (30..90).map(move |x| (x, y))).any(|(x, y)| out.get_pixel(x, y).0[1] == 255 && out.get_pixel(x, y).0[0] == 255);
            assert!(symbol, "{stamp:?} symbol");
            assert_eq!(out.get_pixel(5, 5).0, [255, 255, 255, 255], "{stamp:?} outside");
        }
    }

    #[test]
    fn renders_pasted_image_layers() {
        let base = RgbaImage::from_pixel(200, 100, Px([255, 255, 255, 255]));
        let pasted = std::sync::Arc::new(RgbaImage::from_pixel(10, 10, Px([0, 0, 255, 255])));
        let layer = ann(Kind::Image { pos: P::new(50.0, 20.0), w: 40.0, h: 40.0, img: pasted });
        let out = render(&base, std::slice::from_ref(&layer), &Effects::default());
        assert_eq!(out.get_pixel(70, 40).0, [0, 0, 255, 255]); // inside the scaled layer
        assert_eq!(out.get_pixel(10, 10).0, [255, 255, 255, 255]); // untouched canvas
        // Layers survive a save/load round trip.
        let json = serde_json::to_string(&layer).unwrap();
        let back: Annotation = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.kind, Kind::Image { w, .. } if w == 40.0));
    }

    #[test]
    fn renders_shapes_text_and_effects() {
        let base = RgbaImage::from_pixel(400, 200, Px([255, 255, 255, 255]));
        let anns = vec![
            ann(Kind::Arrow { a: P::new(20.0, 20.0), b: P::new(180.0, 120.0) }),
            ann(Kind::Text { pos: P::new(200.0, 40.0), text: "Hi".into(), size: 40.0, background: true }),
            ann(Kind::pixelate(P::new(0.0, 150.0), P::new(100.0, 200.0))),
        ];
        let out = render(&base, &anns, &Effects::default());
        assert_eq!(out.dimensions(), (400, 200));
        // Arrow shaft passes through the midpoint; callout box is filled red.
        assert_eq!(out.get_pixel(100, 70)[0], 235);
        assert!(out.get_pixel(100, 70)[1] < 100);

        let fx = Effects { border_width: 10.0, shadow: true, ..Default::default() };
        let framed = render(&base, &[], &fx);
        assert!(framed.width() > 420 && framed.height() > 220);
    }

    #[test]
    fn hebrew_is_reordered_and_has_glyphs() {
        // Logical order (typing order) becomes visual left-to-right order.
        assert_eq!(visual_order("שלום"), "םולש");
        assert_eq!(visual_order("abc 123"), "abc 123");
        // Hebrew sentence with an embedded number keeps the number readable.
        assert_eq!(visual_order("שלום 123"), "123 םולש");
        assert_eq!(visual_order("a (שלום)"), "a (םולש)");

        let base = RgbaImage::from_pixel(300, 80, Px([255, 255, 255, 255]));
        let a = ann(Kind::Text { pos: P::new(10.0, 10.0), text: "שלום עולם".into(), size: 40.0, background: false });
        let out = render(&base, &[a], &Effects::default());
        assert!(out.pixels().any(|p| p[1] < 100), "Hebrew text drew no ink");
        let (w, _) = measure_text("שלום עולם", 40.0);
        assert!(w > 100.0, "Hebrew width {w} suggests missing glyphs");
        if let Ok(dir) = std::env::var("HEBREW_PREVIEW_DIR") {
            out.save(std::path::Path::new(&dir).join("hebrew.png")).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ocr_reads_rendered_text() {
        let base = RgbaImage::from_pixel(900, 160, Px([255, 255, 255, 255]));
        let mut a = ann(Kind::Text { pos: P::new(20.0, 40.0), text: "Hello Rega 2026".into(), size: 64.0, background: false });
        a.color = [0, 0, 0, 255];
        let img = render(&base, &[a], &Effects::default());
        let text = crate::ocr::recognize(&img).expect("ocr");
        assert!(text.to_lowercase().contains("hello rega 2026"), "got {text:?}");
    }
}

