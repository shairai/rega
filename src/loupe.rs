//! The colour-picking loupe, shared by the editor's eyedropper and the
//! screen colour picker.

use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};
use image::RgbaImage;

/// "#AE4E4A": how a picked colour is shown and copied.
pub fn hex(color: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}

/// Pixels across the eyedropper's loupe; odd, so the picked one is in the middle.
pub const CELLS: i32 = 15;
/// Points per enlarged pixel in the loupe.
pub const CELL: f32 = 8.0;

/// The loupe, centred on the pointer at `at`: a circle of the pixels of
/// `img` around (`cx`, `cy`) enlarged, the one picked outlined in the
/// middle and `label` (its hex code) on a pill below it.
pub fn paint(painter: &egui::Painter, img: &RgbaImage, at: Pos2, (cx, cy): (u32, u32), label: &str) {
    let radius = CELLS as f32 * CELL / 2.0;
    let disc = Rect::from_center_size(at, Vec2::splat(radius * 2.0));
    painter.add(
        egui::epaint::Shadow { offset: [0, 4], blur: 18, spread: 0, color: Color32::from_black_alpha(90) }
            .as_shape(disc, CornerRadius::same(radius as u8)),
    );

    // One flat mesh (no anti-aliasing, so no seams between pixels), each
    // pixel cut to the circle; the ring drawn after covers the jagged edge.
    let circle: Vec<Pos2> = (0..96).map(|i| at + Vec2::angled(i as f32 * std::f32::consts::TAU / 96.0) * radius).collect();
    let inscribed = radius * (std::f32::consts::PI / 96.0).cos();
    let mut mesh = egui::Mesh::default();
    let half = CELLS / 2;
    for dy in -half..=half {
        for dx in -half..=half {
            let (x, y) = (cx as i32 + dx, cy as i32 + dy);
            let c = if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
                let p = img.get_pixel(x as u32, y as u32);
                Color32::from_rgb(p[0], p[1], p[2])
            } else {
                crate::theme::BG
            };
            let cell = Rect::from_center_size(at + Vec2::new(dx as f32, dy as f32) * CELL, Vec2::splat(CELL));
            let corners = vec![cell.left_top(), cell.right_top(), cell.right_bottom(), cell.left_bottom()];
            let far = corners.iter().map(|p| p.distance(at)).fold(0.0, f32::max);
            let poly = if far <= inscribed { corners } else { clip_convex(corners, &circle) };
            if poly.len() < 3 {
                continue;
            }
            let first = mesh.vertices.len() as u32;
            for &p in &poly {
                mesh.colored_vertex(p, c);
            }
            for k in 1..poly.len() as u32 - 1 {
                mesh.add_triangle(first, first + k, first + k + 1);
            }
        }
    }
    painter.add(mesh);

    // The picked pixel: a light square with a dark edge, seen on any color.
    let center = Rect::from_center_size(at, Vec2::splat(CELL));
    painter.rect_stroke(center, 0.0, Stroke::new(1.0, Color32::from_white_alpha(230)), StrokeKind::Outside);
    painter.rect_stroke(center.expand(1.0), 0.0, Stroke::new(1.0, Color32::from_black_alpha(140)), StrokeKind::Outside);

    painter.circle_stroke(at, radius, Stroke::new(2.0, Color32::from_gray(48)));
    painter.circle_stroke(at, radius - 1.5, Stroke::new(1.0, Color32::from_white_alpha(70)));

    let galley = painter.layout_no_wrap(label.to_owned(), FontId::proportional(12.0), Color32::WHITE);
    let pill = Rect::from_center_size(at + Vec2::new(0.0, radius * 0.45), galley.size() + Vec2::new(16.0, 6.0));
    painter.rect_filled(pill, CornerRadius::same((pill.height() / 2.0) as u8), Color32::from_rgba_unmultiplied(30, 30, 34, 215));
    painter.galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// The part of convex polygon `poly` inside convex polygon `clip`, whose
/// points go counter-clockwise in x/y (Sutherland–Hodgman).
fn clip_convex(mut poly: Vec<Pos2>, clip: &[Pos2]) -> Vec<Pos2> {
    for (i, &a) in clip.iter().enumerate() {
        let b = clip[(i + 1) % clip.len()];
        let side = |p: Pos2| (b - a).x * (p - a).y - (b - a).y * (p - a).x;
        let input = std::mem::take(&mut poly);
        for (j, &p) in input.iter().enumerate() {
            let q = input[(j + 1) % input.len()];
            let (sp, sq) = (side(p), side(q));
            if sp >= 0.0 {
                poly.push(p);
            }
            if (sp >= 0.0) != (sq >= 0.0) {
                poly.push(p + (q - p) * (sp / (sp - sq)));
            }
        }
        if poly.is_empty() {
            break;
        }
    }
    poly
}
