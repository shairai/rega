//! The moving gradient around a capture region. The selection overlay's
//! outline and the recording border (Core Animation on macOS, egui windows on
//! Windows) all draw it, so its palette, motion and width live here.
//!
//! It is a conic gradient in the logo colours, coral → violet → coral,
//! centred on the region and turning clockwise once every [`TURN_SECS`].
//! Positions are in top-left points (y down); angles and phases are in turns,
//! clockwise from straight up.

use std::f32::consts::TAU;
use std::sync::Arc;

use eframe::egui::epaint::tessellator::path;
use eframe::egui::epaint::{ColorMode, PathShape, PathStroke, RoundedRect};
use eframe::egui::{Color32, Mesh, Pos2, Rect, Shape, StrokeKind};

/// Seconds per turn: a calm drift rather than a spin.
pub const TURN_SECS: f64 = 3.0;

/// Thickness in points, drawn just outside the region.
pub const WIDTH: f32 = 3.0;

/// Corner radius of the outer edge, in points. The inner edge's is `WIDTH`
/// less, so the band keeps its width round the corners (and covers the
/// region's very corners a little; it never shows in a capture).
pub const RADIUS: f32 = 10.0;

/// The corner radii (inner, outer) of the band around `r`: [`RADIUS`] on the
/// outside, less for a region too small to take it.
pub fn radii(r: Rect) -> (f32, f32) {
    let inner = (RADIUS - WIDTH).min(r.width() / 2.0).min(r.height() / 2.0).max(0.0);
    (inner, inner + WIDTH)
}

/// Longest gap between mesh vertices, in points: the colour changes too
/// little over it for the straight blend in between to show.
const STEP: f32 = 10.0;

/// Most quads along one side, so a huge region costs no more than this.
const MAX_STEPS: usize = 128;

/// The palette once around: coral at 0 and 1, violet at ½. Eased so it rests
/// on the two logo colours instead of creasing at them.
pub fn palette(turns: f32) -> [u8; 3] {
    let s = (1.0 - (turns * TAU).cos()) / 2.0;
    let (a, b) = (crate::icon::LOGO_FROM, crate::icon::LOGO_TO);
    std::array::from_fn(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * s).round() as u8)
}

/// How far the gradient has turned after `secs` seconds (0 ≤ phase < 1).
pub fn phase(secs: f64) -> f32 {
    (secs / TURN_SECS).rem_euclid(1.0) as f32
}

/// The colour at `p` once the gradient around `center` has turned `phase`.
pub fn color_at(p: Pos2, center: Pos2, phase: f32) -> Color32 {
    let d = p - center;
    let [r, g, b] = palette(d.x.atan2(-d.y) / TAU - phase);
    Color32::from_rgb(r, g, b)
}

/// The outline of `r`: a band [`WIDTH`] thick just outside it, rounded at
/// the corners (see [`radii`]) and coloured for `phase`. egui smooths its
/// edges. Its shape depends only on `r`, so only the colours move.
pub fn ring(r: Rect, phase: f32) -> Shape {
    let (inner, _) = radii(r);
    // Stroked along the band's middle, so it spans exactly r..r + WIDTH.
    let mut corners = Vec::new();
    path::rounded_rectangle(&mut corners, RoundedRect::new(r.expand(WIDTH / 2.0), inner + WIDTH / 2.0));
    // egui colours only the path's points and blends straight between them:
    // points along the sides too keep the blend on the gradient.
    let mut points = Vec::with_capacity(corners.len() + 4 * MAX_STEPS);
    for (i, &a) in corners.iter().enumerate() {
        let b = corners[(i + 1) % corners.len()];
        let steps = (((b - a).length() / STEP).ceil() as usize).clamp(1, MAX_STEPS);
        points.extend((0..steps).map(|k| a.lerp(b, k as f32 / steps as f32)));
    }
    let center = r.center();
    let color = ColorMode::UV(Arc::new(move |_, p| color_at(p, center, phase)));
    Shape::Path(PathShape { points, closed: true, fill: Color32::TRANSPARENT, stroke: PathStroke { width: WIDTH, color, kind: StrokeKind::Middle } })
}

/// One strip of the recording border, filling `local` (the strip window's
/// own points) and coloured by where each point is on screen: `origin` is
/// where `local`'s top-left sits there, `center` the region's centre. Strips
/// that meet show the same colours where they meet.
pub fn strip(local: Rect, origin: Pos2, center: Pos2, phase: f32) -> Mesh {
    let to_screen = origin - local.min;
    let (a, b) = if local.width() >= local.height() {
        ([local.left_top(), local.right_top()], [local.left_bottom(), local.right_bottom()])
    } else {
        ([local.left_top(), local.left_bottom()], [local.right_top(), local.right_bottom()])
    };
    let mut mesh = Mesh::default();
    band(&mut mesh, a, b, |p| color_at(p + to_screen, center, phase));
    mesh
}

/// Quads between the edges `a` and `b` (running the same way), with a pair
/// of vertices every `STEP` points or so, each coloured by `color`.
fn band(mesh: &mut Mesh, a: [Pos2; 2], b: [Pos2; 2], color: impl Fn(Pos2) -> Color32) {
    let len = (a[1] - a[0]).length().max((b[1] - b[0]).length());
    let steps = ((len / STEP).ceil() as usize).clamp(1, MAX_STEPS);
    let first = mesh.vertices.len() as u32;
    for k in 0..=steps {
        let t = k as f32 / steps as f32;
        for p in [a[0].lerp(a[1], t), b[0].lerp(b[1], t)] {
            mesh.colored_vertex(p, color(p));
        }
    }
    for k in 0..steps as u32 {
        let v = first + 2 * k;
        mesh.add_triangle(v, v + 1, v + 2);
        mesh.add_triangle(v + 1, v + 3, v + 2);
    }
}

/// How far `p` is outside the rect `r` with corners rounded to `radius`
/// (negative inside), for tests.
#[cfg(test)]
pub(crate) fn rounded_distance(p: Pos2, r: Rect, radius: f32) -> f32 {
    let d = (p - r.center()).abs() - (r.size() / 2.0 - eframe::egui::Vec2::splat(radius));
    eframe::egui::Vec2::new(d.x.max(0.0), d.y.max(0.0)).length() + d.x.max(d.y).min(0.0) - radius
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icon::{LOGO_FROM, LOGO_TO};
    use eframe::egui::Vec2;

    fn rgb(c: [u8; 3]) -> Color32 {
        Color32::from_rgb(c[0], c[1], c[2])
    }

    /// Twice the signed area of a triangle.
    fn cross(a: Pos2, b: Pos2, c: Pos2) -> f32 {
        (b - a).x * (c - a).y - (b - a).y * (c - a).x
    }

    fn triangles(mesh: &Mesh) -> impl Iterator<Item = [Pos2; 3]> + '_ {
        mesh.indices.chunks(3).map(|t| [0, 1, 2].map(|i| mesh.vertices[t[i] as usize].pos))
    }

    /// Coral meets coral where a turn ends, so the cone has no seam, and every
    /// colour in between is a blend of the two logo colours.
    #[test]
    fn the_palette_is_seamless_and_stays_on_the_logo_colours() {
        assert_eq!(palette(0.0), LOGO_FROM);
        assert_eq!(palette(0.5), LOGO_TO);
        assert_eq!(palette(1.0), LOGO_FROM);
        assert_eq!(palette(-1.0), LOGO_FROM);
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            // A whole turn on is the same colour (to the rounding of a level).
            for other in [palette(t + 1.0), palette(t - 1.0)] {
                assert!((0..3).all(|c| palette(t)[c].abs_diff(other[c]) <= 1), "{:?} vs {other:?} at {t}", palette(t));
            }
            // Every colour lies on the line from coral to violet.
            let c = palette(t);
            let s = (c[2] as f32 - LOGO_FROM[2] as f32) / (LOGO_TO[2] as f32 - LOGO_FROM[2] as f32);
            assert!((-0.01..=1.01).contains(&s), "{c:?} at {t}");
            for ch in 0..3 {
                let on_line = LOGO_FROM[ch] as f32 + (LOGO_TO[ch] as f32 - LOGO_FROM[ch] as f32) * s;
                assert!((c[ch] as f32 - on_line).abs() <= 1.5, "{c:?} off the logo line at {t}");
            }
        }
        // Seamless in time too: a whole turn later looks the same.
        assert_eq!(phase(0.0), 0.0);
        assert_eq!(phase(TURN_SECS / 2.0), 0.5);
        assert!(phase(TURN_SECS) < 1e-6);
        assert!((phase(-TURN_SECS / 4.0) - 0.75).abs() < 1e-6);
    }

    /// Coral starts straight up and moves round clockwise as the phase grows.
    #[test]
    fn the_gradient_turns_clockwise() {
        let c = Pos2::new(500.0, 300.0);
        let [top, right, bottom, left] = [Vec2::new(0.0, -100.0), Vec2::new(100.0, 0.0), Vec2::new(0.0, 100.0), Vec2::new(-100.0, 0.0)];
        assert_eq!(color_at(c + top, c, 0.0), rgb(LOGO_FROM));
        assert_eq!(color_at(c + bottom, c, 0.0), rgb(LOGO_TO));
        // A quarter turn later coral is on the right.
        assert_eq!(color_at(c + right, c, 0.25), rgb(LOGO_FROM));
        assert_eq!(color_at(c + left, c, 0.25), rgb(LOGO_TO));
        assert_eq!(color_at(c + bottom, c, 0.5), rgb(LOGO_FROM));
        assert_eq!(color_at(c + left, c, 0.75), rgb(LOGO_FROM));
    }

    /// `shape` as egui draws it at `pixels_per_point`, edges smoothed or not.
    fn tessellate(shape: Shape, pixels_per_point: f32, feathering: bool) -> Mesh {
        use eframe::egui::epaint::{TessellationOptions, Tessellator};
        let options = TessellationOptions { feathering, ..Default::default() };
        let mut mesh = Mesh::default();
        Tessellator::new(pixels_per_point, options, [1, 1], vec![]).tessellate_shape(shape, &mut mesh);
        mesh
    }

    /// The outline is a band just outside the selection, rounded at the
    /// corners, that leaves no gaps and stays a small mesh for tiny, thin and
    /// huge rects.
    #[test]
    fn the_ring_lies_just_outside_the_rect_with_rounded_corners() {
        for r in [
            Rect::from_min_size(Pos2::new(218.0, 150.0), Vec2::new(494.0, 328.0)),
            Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(4.0, 4.0)),
            Rect::from_min_size(Pos2::new(10.5, 20.25), Vec2::new(1000.0, 4.0)),
            Rect::from_min_size(Pos2::new(-5000.0, 0.0), Vec2::new(20000.0, 9000.0)),
        ] {
            let (inner, outer) = radii(r);
            assert!((outer - inner - WIDTH).abs() < 1e-6 && outer <= RADIUS + 1e-6, "{inner}, {outer} for {r:?}");
            // At 2 pixels a point egui smooths each edge over half a point.
            let mesh = tessellate(ring(r, 0.3), 2.0, true);
            // Bounded however big the rect: MAX_STEPS points a side at most,
            // plus the corners' arcs, each smoothed into four vertices.
            assert!(mesh.vertices.len() <= 4 * (4 * MAX_STEPS + 128), "{} vertices for {r:?}", mesh.vertices.len());
            let solid = |c: Color32| c.a() == 255;
            for v in &mesh.vertices {
                let (beyond, within) = (rounded_distance(v.pos, r.expand(WIDTH), outer), rounded_distance(v.pos, r, inner));
                let slack = if solid(v.color) { 1e-3 } else { 0.5 + 1e-3 };
                assert!(beyond <= slack && within >= -slack, "{:?} off the band of {r:?}", v.pos);
            }
            // The outer corners are round: nothing reaches into the square's
            // corner beyond the arc.
            if outer >= RADIUS {
                let gap = outer * (std::f32::consts::SQRT_2 - 1.0) - 0.5;
                let corners = { let q = r.expand(WIDTH); [q.left_top(), q.right_top(), q.right_bottom(), q.left_bottom()] };
                for v in &mesh.vertices {
                    for c in corners {
                        assert!((v.pos - c).length() >= gap - 1e-3, "{:?} in the corner of {r:?}", v.pos);
                    }
                }
            }
            // No gaps or overlaps: unsmoothed, the triangles cover the band,
            // whose round corners egui draws as short straight runs.
            let rounded_area = |q: Rect, radius: f32| q.area() - (4.0 - std::f32::consts::PI) * radius * radius;
            let band = rounded_area(r.expand(WIDTH), outer) - rounded_area(r, inner);
            let area: f32 = triangles(&tessellate(ring(r, 0.3), 2.0, false)).map(|[a, b, c]| cross(a, b, c).abs() / 2.0).sum();
            assert!((area - band).abs() <= band * 0.01 + 1.0, "{area} vs {band} for {r:?}");
        }
    }

    /// Time only recolours the outline: its shape stays put.
    #[test]
    fn the_ring_changes_colour_with_time_but_not_shape() {
        let r = Rect::from_min_size(Pos2::new(100.0, 80.0), Vec2::new(640.0, 360.0));
        let (a, b) = (tessellate(ring(r, phase(0.0)), 2.0, true), tessellate(ring(r, phase(0.8)), 2.0, true));
        assert_eq!(a.indices, b.indices);
        assert!(a.vertices.iter().zip(&b.vertices).all(|(a, b)| a.pos == b.pos));
        let moved = a.vertices.iter().zip(&b.vertices).filter(|(a, b)| a.color != b.color).count();
        assert!(moved > a.vertices.len() / 4, "only {moved} of {} vertices changed colour", a.vertices.len());
        // Every solid vertex shows the gradient at its own spot.
        for v in b.vertices.iter().filter(|v| v.color.a() == 255) {
            assert_eq!(v.color, color_at(v.pos, r.center(), phase(0.8)));
        }
    }
}
