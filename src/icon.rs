//! App and tray icons, drawn at runtime so there are no binary assets to ship.

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

/// Four viewfinder corners and a dot — the Rega mark.
pub(crate) fn draw_mark(pixmap: &mut Pixmap, size: f32, inset: f32, color: (u8, u8, u8, u8)) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, color.3);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: size * 0.09,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        ..Default::default()
    };
    let (a, b) = (inset, size - inset);
    let arm = size * 0.22;
    let mut pb = PathBuilder::new();
    for (cx, cy, dx, dy) in [(a, a, 1.0, 1.0), (b, a, -1.0, 1.0), (a, b, 1.0, -1.0), (b, b, -1.0, -1.0)] {
        pb.move_to(cx + dx * arm, cy);
        pb.line_to(cx, cy);
        pb.line_to(cx, cy + dy * arm);
    }
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    if let Some(dot) = PathBuilder::from_circle(size / 2.0, size / 2.0, size * 0.11) {
        pixmap.fill_path(&dot, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn to_rgba(pixmap: Pixmap) -> Vec<u8> {
    pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

/// The logo's gradient, top-left to bottom-right.
pub const LOGO_FROM: [u8; 3] = [255, 94, 98];
pub const LOGO_TO: [u8; 3] = [124, 77, 255];

/// Colorful app icon (window / dock).
pub fn app_icon(size: u32) -> (Vec<u8>, u32, u32) {
    let mut pixmap = Pixmap::new(size, size).expect("icon size");
    let s = size as f32;
    let mut bg = Paint::default();
    bg.anti_alias = true;
    bg.shader = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(s, s),
        vec![
            tiny_skia::GradientStop::new(0.0, tiny_skia::Color::from_rgba8(LOGO_FROM[0], LOGO_FROM[1], LOGO_FROM[2], 255)),
            tiny_skia::GradientStop::new(1.0, tiny_skia::Color::from_rgba8(LOGO_TO[0], LOGO_TO[1], LOGO_TO[2], 255)),
        ],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    )
    .expect("gradient");
    if let Some(rect) = Rect::from_xywh(s * 0.04, s * 0.04, s * 0.92, s * 0.92) {
        let r = s * 0.22;
        let path = rounded_rect(rect, r);
        pixmap.fill_path(&path, &bg, FillRule::Winding, Transform::identity(), None);
    }
    draw_mark(&mut pixmap, s, s * 0.24, (255, 255, 255, 255));
    (to_rgba(pixmap), size, size)
}

pub fn rounded_rect(rect: Rect, r: f32) -> tiny_skia::Path {
    let (l, t, rr, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0);
    // Cubic approximation of a quarter circle.
    let k = r * 0.552_284_8;
    let mut pb = PathBuilder::new();
    pb.move_to(l + r, t);
    pb.line_to(rr - r, t);
    pb.cubic_to(rr - r + k, t, rr, t + r - k, rr, t + r);
    pb.line_to(rr, b - r);
    pb.cubic_to(rr, b - r + k, rr - r + k, b, rr - r, b);
    pb.line_to(l + r, b);
    pb.cubic_to(l + r - k, b, l, b - r + k, l, b - r);
    pb.line_to(l, t + r);
    pb.cubic_to(l, t + r - k, l + r - k, t, l + r, t);
    pb.close();
    pb.finish().expect("rounded rect path")
}
