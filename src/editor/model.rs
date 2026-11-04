//! Annotation data model. All coordinates are in image pixels.

use std::sync::Arc;

use image::RgbaImage;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct P {
    pub x: f32,
    pub y: f32,
}

impl P {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn dist(self, o: P) -> f32 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2)).sqrt()
    }
}

pub type Rgba = [u8; 4];

/// How the blur tool hides things.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlurStyle {
    /// Mosaic of flat blocks. ("Redact" briefly existed as a style; a filled
    /// rectangle does the same, so such marks load as pixelate.)
    #[default]
    #[serde(alias = "Redact")]
    Pixelate,
    /// Smooth Gaussian blur.
    Gaussian,
}

impl BlurStyle {
    pub const ALL: [BlurStyle; 2] = [BlurStyle::Pixelate, BlurStyle::Gaussian];

    pub fn label(self) -> &'static str {
        match self {
            BlurStyle::Pixelate => "Pixelate",
            BlurStyle::Gaussian => "Blur",
        }
    }
}

pub const DEFAULT_BLUR_STRENGTH: f32 = 50.0;

fn default_blur_strength() -> f32 {
    DEFAULT_BLUR_STRENGTH
}

impl Kind {
    /// A pixelate blur at the default strength.
    #[cfg(test)]
    pub fn pixelate(a: P, b: P) -> Kind {
        Kind::Blur { a, b, style: BlurStyle::Pixelate, strength: DEFAULT_BLUR_STRENGTH }
    }
}

/// The symbols the stamp tool can place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Stamp {
    Check,
    Cross,
    Warning,
    Question,
    Info,
    Plus,
    ThumbsUp,
    ThumbsDown,
    Smile,
    Sad,
    Heart,
    Star,
    Sparkle,
    Lightning,
    Idea,
    Flag,
    Pin,
    Bookmark,
    Eye,
    Lock,
    Clock,
    Target,
    Arrow,
    Play,
}

impl Stamp {
    /// Picker order: six per row (status, reactions, highlights, objects).
    pub const ALL: [Stamp; 24] = [
        Stamp::Check,
        Stamp::Cross,
        Stamp::Warning,
        Stamp::Question,
        Stamp::Info,
        Stamp::Plus,
        Stamp::ThumbsUp,
        Stamp::ThumbsDown,
        Stamp::Smile,
        Stamp::Sad,
        Stamp::Heart,
        Stamp::Star,
        Stamp::Sparkle,
        Stamp::Lightning,
        Stamp::Idea,
        Stamp::Flag,
        Stamp::Pin,
        Stamp::Bookmark,
        Stamp::Eye,
        Stamp::Lock,
        Stamp::Clock,
        Stamp::Target,
        Stamp::Arrow,
        Stamp::Play,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stamp::Check => "Check",
            Stamp::Cross => "Cross",
            Stamp::Warning => "Warning",
            Stamp::Question => "Question",
            Stamp::Info => "Info",
            Stamp::Plus => "Plus",
            Stamp::ThumbsUp => "Thumbs up",
            Stamp::ThumbsDown => "Thumbs down",
            Stamp::Smile => "Smile",
            Stamp::Sad => "Sad",
            Stamp::Heart => "Heart",
            Stamp::Star => "Star",
            Stamp::Sparkle => "Sparkle",
            Stamp::Lightning => "Lightning",
            Stamp::Idea => "Idea",
            Stamp::Flag => "Flag",
            Stamp::Pin => "Pin",
            Stamp::Bookmark => "Bookmark",
            Stamp::Eye => "Eye",
            Stamp::Lock => "Lock",
            Stamp::Clock => "Clock",
            Stamp::Target => "Target",
            Stamp::Arrow => "Arrow",
            Stamp::Play => "Play",
        }
    }
}

/// One part of a stamp's symbol (in image coordinates), shared by the canvas
/// and the exporter so both look the same. Parts are drawn in order.
pub enum StampSymbol {
    /// Round-capped strokes of the given width.
    Strokes(Vec<Vec<P>>, f32),
    /// A filled outline, closed (last point = second point) and star-shaped
    /// around its first point, so it can be fan-filled.
    Fill(Vec<P>),
    /// Like `Fill`, but in the badge's color: cuts a hole into earlier parts.
    Cut(Vec<P>),
    /// A single bold character, centered.
    Glyph(&'static str, f32),
}

/// The parts of `stamp` on a badge of `size` diameter centered at `c`.
pub fn stamp_symbol(stamp: Stamp, c: P, size: f32) -> Vec<StampSymbol> {
    let r = size / 2.0;
    // Shapes are designed on a unit badge (radius 1, y down); ThumbsDown is ThumbsUp flipped.
    let (unit, flip) = match stamp {
        Stamp::ThumbsDown => (unit_symbol(Stamp::ThumbsUp), -1.0),
        s => (unit_symbol(s), 1.0),
    };
    let at = |q: P| P::new(c.x + q.x * r, c.y + q.y * r * flip);
    let map = |pts: Vec<P>| pts.into_iter().map(at).collect::<Vec<_>>();
    unit.into_iter()
        .map(|part| match part {
            StampSymbol::Strokes(lines, w) => StampSymbol::Strokes(lines.into_iter().map(map).collect(), w * r),
            StampSymbol::Fill(pts) => StampSymbol::Fill(map(pts)),
            StampSymbol::Cut(pts) => StampSymbol::Cut(map(pts)),
            StampSymbol::Glyph(g, s) => StampSymbol::Glyph(g, s * r),
        })
        .collect()
}

fn unit_symbol(stamp: Stamp) -> Vec<StampSymbol> {
    use StampSymbol::{Cut, Fill, Glyph, Strokes};
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    let p = P::new;
    // A polygon, fanned from its vertex average (all shapes here are star-shaped around it).
    let poly = |v: &[(f32, f32)]| {
        let n = v.len() as f32;
        let mut pts = vec![p(v.iter().map(|q| q.0).sum::<f32>() / n, v.iter().map(|q| q.1).sum::<f32>() / n)];
        pts.extend(v.iter().chain(&v[..1]).map(|&(x, y)| p(x, y)));
        pts
    };
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| poly(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)]);
    let arc = |cx: f32, cy: f32, rad: f32, a0: f32, a1: f32| -> Vec<P> {
        let n = ((a1 - a0).abs() / TAU * 48.0).ceil().max(4.0) as usize;
        (0..=n).map(|i| {
            let t = a0 + (a1 - a0) * i as f32 / n as f32;
            p(cx + t.cos() * rad, cy + t.sin() * rad)
        }).collect()
    };
    let disc = |cx: f32, cy: f32, rad: f32| {
        let mut pts = vec![p(cx, cy)];
        pts.extend(arc(cx, cy, rad, 0.0, TAU));
        pts
    };
    // A star with `n` points alternating between radii `outer` and `inner`.
    let star = |cx: f32, cy: f32, n: usize, outer: f32, inner: f32| {
        let mut pts = vec![p(cx, cy)];
        for i in 0..=2 * n {
            let t = i as f32 / (2 * n) as f32 * TAU - FRAC_PI_2;
            let k = if i % 2 == 0 { outer } else { inner };
            pts.push(p(cx + t.cos() * k, cy + t.sin() * k));
        }
        pts
    };
    match stamp {
        Stamp::Check => vec![Strokes(vec![vec![p(-0.45, 0.02), p(-0.12, 0.35), p(0.48, -0.3)]], 0.2)],
        Stamp::Cross => vec![Strokes(vec![vec![p(-0.35, -0.35), p(0.35, 0.35)], vec![p(0.35, -0.35), p(-0.35, 0.35)]], 0.2)],
        Stamp::Warning => vec![Glyph("!", 1.24)],
        Stamp::Question => vec![Glyph("?", 1.24)],
        Stamp::Info => vec![Glyph("i", 1.24)],
        Stamp::Plus => vec![Strokes(vec![vec![p(-0.42, 0.0), p(0.42, 0.0)], vec![p(0.0, -0.42), p(0.0, 0.42)]], 0.2)],
        Stamp::ThumbsUp | Stamp::ThumbsDown => vec![
            Fill(rect(-0.52, -0.04, -0.3, 0.48)),
            Fill(rect(-0.24, -0.08, 0.4, 0.48)),
            Fill(disc(0.34, -0.02, 0.08)),
            Fill(disc(0.34, 0.42, 0.08)),
            Fill(poly(&[(-0.24, -0.08), (-0.02, -0.52), (0.16, -0.5), (0.12, -0.08)])),
            Fill(disc(0.07, -0.5, 0.09)),
            // Finger gaps.
            Cut(rect(0.12, 0.08, 0.48, 0.12)),
            Cut(rect(0.12, 0.22, 0.48, 0.26)),
            Cut(rect(0.12, 0.36, 0.48, 0.4)),
        ],
        Stamp::Smile => vec![
            Fill(disc(-0.22, -0.16, 0.1)),
            Fill(disc(0.22, -0.16, 0.1)),
            Strokes(vec![arc(0.0, -0.02, 0.36, 0.5, PI - 0.5)], 0.12),
        ],
        Stamp::Sad => vec![
            Fill(disc(-0.22, -0.16, 0.1)),
            Fill(disc(0.22, -0.16, 0.1)),
            Strokes(vec![arc(0.0, 0.52, 0.32, PI + 0.6, TAU - 0.6)], 0.12),
        ],
        Stamp::Heart => {
            // Parametric heart, centered a little low so the fan centre sees the whole outline.
            let mut pts = vec![p(0.0, 0.08)];
            for i in 0..=48 {
                let t = i as f32 / 48.0 * TAU;
                let x = 16.0 * t.sin().powi(3);
                let y = 13.0 * t.cos() - 5.0 * (2.0 * t).cos() - 2.0 * (3.0 * t).cos() - (4.0 * t).cos();
                pts.push(p(x / 16.0 * 0.55, -y / 16.0 * 0.55 + 0.05));
            }
            vec![Fill(pts)]
        }
        Stamp::Star => vec![Fill(star(0.0, 0.04, 5, 0.62, 0.25))],
        Stamp::Sparkle => vec![Fill(star(-0.08, 0.08, 4, 0.52, 0.13)), Fill(star(0.36, -0.36, 4, 0.22, 0.06))],
        Stamp::Lightning => vec![
            Fill(poly(&[(0.16, -0.62), (-0.34, 0.1), (-0.02, 0.1), (0.04, -0.1)])),
            Fill(poly(&[(0.02, -0.1), (0.34, -0.1), (-0.16, 0.62), (-0.04, 0.1)])),
        ],
        Stamp::Idea => vec![
            Fill(disc(0.0, -0.16, 0.36)),
            Fill(poly(&[(-0.22, 0.06), (0.22, 0.06), (0.16, 0.3), (-0.16, 0.3)])),
            Fill(rect(-0.15, 0.36, 0.15, 0.44)),
            Fill(rect(-0.1, 0.48, 0.1, 0.55)),
        ],
        Stamp::Flag => vec![
            Strokes(vec![vec![p(-0.36, -0.5), p(-0.36, 0.56)]], 0.12),
            Fill(poly(&[(-0.36, -0.52), (0.46, -0.52), (0.24, -0.27), (0.46, -0.02), (-0.36, -0.02)])),
        ],
        Stamp::Pin => vec![
            Fill(disc(0.0, -0.14, 0.38)),
            Fill(poly(&[(-0.3, 0.06), (0.3, 0.06), (0.0, 0.62)])),
            Cut(disc(0.0, -0.14, 0.15)),
        ],
        Stamp::Bookmark => vec![
            Fill(poly(&[(-0.32, -0.52), (0.0, -0.52), (0.0, 0.26), (-0.32, 0.56)])),
            Fill(poly(&[(0.0, -0.52), (0.32, -0.52), (0.32, 0.56), (0.0, 0.26)])),
        ],
        Stamp::Eye => {
            // Almond: two arcs meeting in points at the corners.
            let mut almond = vec![p(0.0, 0.0)];
            almond.extend(arc(0.0, 0.5, 0.86, PI + 0.62, TAU - 0.62));
            almond.extend(arc(0.0, -0.5, 0.86, 0.62, PI - 0.62));
            almond.push(almond[1]);
            vec![Fill(almond), Cut(disc(0.0, 0.0, 0.25)), Fill(disc(0.0, 0.0, 0.14))]
        }
        Stamp::Lock => {
            let mut shackle = vec![p(-0.24, 0.0)];
            shackle.extend(arc(0.0, -0.18, 0.24, PI, TAU));
            shackle.push(p(0.24, 0.0));
            vec![
                Strokes(vec![shackle], 0.11),
                Fill(rect(-0.4, -0.04, 0.4, 0.54)),
                Cut(disc(0.0, 0.18, 0.08)),
                Cut(rect(-0.035, 0.2, 0.035, 0.38)),
            ]
        }
        Stamp::Clock => vec![
            Strokes(vec![arc(0.0, 0.0, 0.5, 0.0, TAU)], 0.1),
            Strokes(vec![vec![p(0.0, -0.32), p(0.0, 0.0), p(0.24, 0.12)]], 0.1),
        ],
        Stamp::Target => vec![Strokes(vec![arc(0.0, 0.0, 0.48, 0.0, TAU)], 0.11), Fill(disc(0.0, 0.0, 0.2))],
        Stamp::Arrow => vec![
            Strokes(vec![vec![p(-0.46, 0.0), p(0.12, 0.0)]], 0.18),
            Fill(poly(&[(0.04, -0.34), (0.5, 0.0), (0.04, 0.34)])),
        ],
        Stamp::Play => vec![Fill(poly(&[(-0.22, -0.38), (0.44, 0.0), (-0.22, 0.38)]))],
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Kind {
    Arrow { a: P, b: P },
    Line { a: P, b: P },
    Rect { a: P, b: P },
    Ellipse { a: P, b: P },
    Pen { pts: Vec<P> },
    Highlight { pts: Vec<P> },
    Text { pos: P, text: String, size: f32, background: bool },
    Step { pos: P, n: u32, radius: f32 },
    /// Hides what's under `a`–`b`. Older projects lack `style`/`strength`
    /// and load as the pixelate effect they had.
    Blur {
        a: P,
        b: P,
        #[serde(default)]
        style: BlurStyle,
        /// 1–100; 50 is the original pixel size.
        #[serde(default = "default_blur_strength")]
        strength: f32,
    },
    Spotlight { a: P, b: P },
    /// A loupe: the ellipse inside `a`–`b` shows the image under it `zoom`× larger.
    Magnify { a: P, b: P, zoom: f32 },
    /// A round badge with a symbol; `size` is its diameter.
    Stamp { pos: P, size: f32, stamp: Stamp },
    /// Paint bucket: floods the region around `seed` whose colors are within
    /// `tolerance` (0–255) of the seed's. `area` is the filled region's bounds,
    /// measured when placed, for selecting it.
    Fill { seed: P, tolerance: f32, area: (P, P) },
    /// A pasted picture placed on the canvas; `w`/`h` is its on-canvas size.
    Image {
        pos: P,
        w: f32,
        h: f32,
        #[serde(with = "png_base64")]
        img: Arc<RgbaImage>,
    },
}

/// Pasted images are stored inside the project JSON as base64 PNG.
mod png_base64 {
    use std::io::Cursor;
    use std::sync::Arc;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use image::RgbaImage;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error as _};

    pub fn serialize<S: Serializer>(img: &Arc<RgbaImage>, s: S) -> Result<S::Ok, S::Error> {
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).map_err(S::Error::custom)?;
        s.serialize_str(&STANDARD.encode(png.into_inner()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Arc<RgbaImage>, D::Error> {
        let text = String::deserialize(d)?;
        let bytes = STANDARD.decode(text).map_err(D::Error::custom)?;
        let img = image::load_from_memory(&bytes).map_err(D::Error::custom)?;
        Ok(Arc::new(img.to_rgba8()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub kind: Kind,
    pub color: Rgba,
    pub width: f32,
    pub filled: bool,
}

/// Normalized rect corners (min, max).
pub fn norm(a: P, b: P) -> (P, P) {
    (P::new(a.x.min(b.x), a.y.min(b.y)), P::new(a.x.max(b.x), a.y.max(b.y)))
}

impl Annotation {
    /// Blur, spotlight, magnifier and fill change pixels; everything else is drawn on top.
    pub fn is_raster(&self) -> bool {
        matches!(self.kind, Kind::Blur { .. } | Kind::Spotlight { .. } | Kind::Magnify { .. } | Kind::Fill { .. })
    }

    pub fn bounds(&self) -> (P, P) {
        match &self.kind {
            Kind::Arrow { a, b }
            | Kind::Line { a, b }
            | Kind::Rect { a, b }
            | Kind::Ellipse { a, b }
            | Kind::Blur { a, b, .. }
            | Kind::Spotlight { a, b }
            | Kind::Magnify { a, b, .. } => norm(*a, *b),
            Kind::Stamp { pos, size, .. } => (P::new(pos.x - size / 2.0, pos.y - size / 2.0), P::new(pos.x + size / 2.0, pos.y + size / 2.0)),
            Kind::Fill { area, .. } => norm(area.0, area.1),
            Kind::Pen { pts } | Kind::Highlight { pts } => {
                let mut min = P::new(f32::MAX, f32::MAX);
                let mut max = P::new(f32::MIN, f32::MIN);
                for p in pts {
                    min = P::new(min.x.min(p.x), min.y.min(p.y));
                    max = P::new(max.x.max(p.x), max.y.max(p.y));
                }
                (min, max)
            }
            Kind::Text { pos, text, size, background } => {
                let (w, h) = crate::editor::render::measure_text(text, *size);
                let pad = if *background { size * 0.35 } else { 0.0 };
                (P::new(pos.x - pad, pos.y - pad), P::new(pos.x + w.max(size * 0.5) + pad, pos.y + h + pad))
            }
            Kind::Step { pos, radius, .. } => (
                P::new(pos.x - radius, pos.y - radius),
                P::new(pos.x + radius, pos.y + radius),
            ),
            Kind::Image { pos, w, h, .. } => (*pos, P::new(pos.x + w, pos.y + h)),
        }
    }

    pub fn hit(&self, p: P, tol: f32) -> bool {
        let tol = tol + self.width / 2.0;
        match &self.kind {
            Kind::Arrow { a, b } | Kind::Line { a, b } => seg_dist(p, *a, *b) <= tol,
            Kind::Pen { pts } | Kind::Highlight { pts } => {
                let tol = if matches!(self.kind, Kind::Highlight { .. }) { tol * 3.0 } else { tol };
                pts.windows(2).any(|w| seg_dist(p, w[0], w[1]) <= tol)
                    || pts.first().is_some_and(|q| q.dist(p) <= tol)
            }
            Kind::Rect { a, b } if !self.filled => {
                let (min, max) = norm(*a, *b);
                let inside = |m: f32| p.x >= min.x - m && p.x <= max.x + m && p.y >= min.y - m && p.y <= max.y + m;
                inside(tol) && !inside(-tol)
            }
            Kind::Ellipse { a, b } if !self.filled => {
                let (min, max) = norm(*a, *b);
                let (cx, cy) = ((min.x + max.x) / 2.0, (min.y + max.y) / 2.0);
                let (rx, ry) = (((max.x - min.x) / 2.0).max(1.0), ((max.y - min.y) / 2.0).max(1.0));
                let d = (((p.x - cx) / rx).powi(2) + ((p.y - cy) / ry).powi(2)).sqrt();
                (d - 1.0).abs() * rx.min(ry) <= tol
            }
            Kind::Step { pos, radius, .. } => pos.dist(p) <= *radius + tol,
            Kind::Stamp { pos, size, .. } => pos.dist(p) <= size / 2.0 + tol,
            Kind::Magnify { a, b, .. } => {
                let (min, max) = norm(*a, *b);
                let (cx, cy) = ((min.x + max.x) / 2.0, (min.y + max.y) / 2.0);
                let (rx, ry) = (((max.x - min.x) / 2.0).max(1.0), ((max.y - min.y) / 2.0).max(1.0));
                ((p.x - cx) / (rx + tol)).powi(2) + ((p.y - cy) / (ry + tol)).powi(2) <= 1.0
            }
            _ => {
                let (min, max) = self.bounds();
                p.x >= min.x - tol && p.x <= max.x + tol && p.y >= min.y - tol && p.y <= max.y + tol
            }
        }
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        let m = |p: &mut P| {
            p.x += dx;
            p.y += dy;
        };
        match &mut self.kind {
            Kind::Arrow { a, b }
            | Kind::Line { a, b }
            | Kind::Rect { a, b }
            | Kind::Ellipse { a, b }
            | Kind::Blur { a, b, .. }
            | Kind::Spotlight { a, b }
            | Kind::Magnify { a, b, .. } => {
                m(a);
                m(b);
            }
            Kind::Fill { seed, area, .. } => {
                m(seed);
                m(&mut area.0);
                m(&mut area.1);
            }
            Kind::Pen { pts } | Kind::Highlight { pts } => pts.iter_mut().for_each(m),
            Kind::Text { pos, .. } | Kind::Step { pos, .. } | Kind::Stamp { pos, .. } | Kind::Image { pos, .. } => m(pos),
        }
    }

    /// Uniform scale about the origin (used by image resize).
    pub fn scale(&mut self, f: f32) {
        let s = |p: &mut P| {
            p.x *= f;
            p.y *= f;
        };
        self.width *= f;
        match &mut self.kind {
            Kind::Arrow { a, b }
            | Kind::Line { a, b }
            | Kind::Rect { a, b }
            | Kind::Ellipse { a, b }
            | Kind::Blur { a, b, .. }
            | Kind::Spotlight { a, b }
            | Kind::Magnify { a, b, .. } => {
                s(a);
                s(b);
            }
            Kind::Fill { seed, area, .. } => {
                s(seed);
                s(&mut area.0);
                s(&mut area.1);
            }
            Kind::Stamp { pos, size, .. } => {
                s(pos);
                *size *= f;
            }
            Kind::Pen { pts } | Kind::Highlight { pts } => pts.iter_mut().for_each(s),
            Kind::Text { pos, size, .. } => {
                s(pos);
                *size *= f;
            }
            Kind::Step { pos, radius, .. } => {
                s(pos);
                *radius *= f;
            }
            Kind::Image { pos, w, h, .. } => {
                s(pos);
                *w *= f;
                *h *= f;
            }
        }
    }

    /// Too small to keep (an accidental click with a drag tool).
    pub fn is_degenerate(&self) -> bool {
        match &self.kind {
            Kind::Pen { pts } | Kind::Highlight { pts } => pts.is_empty(),
            Kind::Text { text, .. } => text.trim().is_empty(),
            Kind::Step { .. } | Kind::Stamp { .. } | Kind::Fill { .. } => false,
            _ => {
                let (min, max) = self.bounds();
                (max.x - min.x).max(max.y - min.y) < 3.0
            }
        }
    }
}

fn seg_dist(p: P, a: P, b: P) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return p.dist(a);
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0);
    p.dist(P::new(a.x + t * dx, a.y + t * dy))
}

/// Arrow geometry shared by the on-screen view and the exporter:
/// (end of the shaft, head triangle).
pub fn arrow_geometry(a: P, b: P, width: f32) -> (P, [P; 3]) {
    let len = a.dist(b).max(0.001);
    let (ux, uy) = ((b.x - a.x) / len, (b.y - a.y) / len);
    let head = (width * 4.0 + 8.0).min(len * 0.6);
    let half = head * 0.55;
    let base = P::new(b.x - ux * head, b.y - uy * head);
    let (px, py) = (-uy, ux);
    let shaft_end = P::new(base.x + ux * head * 0.2, base.y + uy * head * 0.2);
    (
        shaft_end,
        [b, P::new(base.x + px * half, base.y + py * half), P::new(base.x - px * half, base.y - py * half)],
    )
}

pub fn ellipse_points(a: P, b: P, segments: usize) -> Vec<P> {
    let (min, max) = norm(a, b);
    let (cx, cy) = ((min.x + max.x) / 2.0, (min.y + max.y) / 2.0);
    let (rx, ry) = ((max.x - min.x) / 2.0, (max.y - min.y) / 2.0);
    (0..segments)
        .map(|i| {
            let t = i as f32 / segments as f32 * std::f32::consts::TAU;
            P::new(cx + rx * t.cos(), cy + ry * t.sin())
        })
        .collect()
}

/// Text color that stays readable on top of `bg`.
pub fn contrast_on(bg: Rgba) -> Rgba {
    let lum = 0.299 * bg[0] as f32 + 0.587 * bg[1] as f32 + 0.114 * bg[2] as f32;
    if lum > 150.0 { [20, 20, 20, 255] } else { [255, 255, 255, 255] }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effects {
    pub border_width: f32,
    pub border_color: Rgba,
    pub shadow: bool,
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            border_width: 0.0,
            border_color: [40, 40, 40, 255],
            shadow: false,
        }
    }
}

/// Everything needed to reopen a capture for editing.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Project {
    pub annotations: Vec<Annotation>,
    pub effects: Effects,
    pub next_step: u32,
}
