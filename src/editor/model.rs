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
    Blur { a: P, b: P },
    Spotlight { a: P, b: P },
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
    /// Blur and spotlight change pixels; everything else is drawn on top.
    pub fn is_raster(&self) -> bool {
        matches!(self.kind, Kind::Blur { .. } | Kind::Spotlight { .. })
    }

    pub fn bounds(&self) -> (P, P) {
        match &self.kind {
            Kind::Arrow { a, b }
            | Kind::Line { a, b }
            | Kind::Rect { a, b }
            | Kind::Ellipse { a, b }
            | Kind::Blur { a, b }
            | Kind::Spotlight { a, b } => norm(*a, *b),
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
            | Kind::Blur { a, b }
            | Kind::Spotlight { a, b } => {
                m(a);
                m(b);
            }
            Kind::Pen { pts } | Kind::Highlight { pts } => pts.iter_mut().for_each(m),
            Kind::Text { pos, .. } | Kind::Step { pos, .. } | Kind::Image { pos, .. } => m(pos),
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
            | Kind::Blur { a, b }
            | Kind::Spotlight { a, b } => {
                s(a);
                s(b);
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
            Kind::Step { .. } => false,
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
