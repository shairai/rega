//! Full-screen selection overlay drawn over a frozen screenshot: drag a
//! region, click a window, or press Enter for the whole screen.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, Vec2,
};

use crate::capture::Snapshot;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Purpose {
    Image,
    Video,
    Scroll,
    Text,
}

impl Purpose {
    fn title(self) -> &'static str {
        match self {
            Purpose::Image => "Image capture",
            Purpose::Video => "Video recording",
            Purpose::Scroll => "Scrolling capture",
            Purpose::Text => "Grab text",
        }
    }
}

pub enum Outcome {
    Pending,
    Cancel,
    /// Selected rect in monitor-local logical points.
    Selected(Rect),
}

pub struct Overlay {
    pub snap: Snapshot,
    pub purpose: Purpose,
    texture: TextureHandle,
    drag_from: Option<Pos2>,
    drag_to: Option<Pos2>,
}

const ACCENT: Color32 = Color32::from_rgb(255, 94, 98);

impl Overlay {
    pub fn new(ctx: &egui::Context, snap: Snapshot, purpose: Purpose) -> Self {
        let img = &snap.image;
        let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        let texture = ctx.load_texture("overlay-snapshot", color, egui::TextureOptions::NEAREST);
        Self {
            snap,
            purpose,
            texture,
            drag_from: None,
            drag_to: None,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome {
        let ctx = ui.ctx().clone();
        // Where our window really sits relative to the monitor (macOS may nudge it).
        let inner = ctx.input(|i| i.viewport().inner_rect);
        let offset = inner
            .map(|r| r.min.to_vec2() - Vec2::new(self.snap.origin.0 as f32, self.snap.origin.1 as f32))
            .unwrap_or_default();
        let to_local = |p: Pos2| p + offset; // window → monitor-local
        let to_win = |p: Pos2| p - offset;

        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(self.snap.size.0 as f32, self.snap.size.1 as f32));
        let (_, resp) = ui.allocate_exact_size(ui.max_rect().size(), Sense::click_and_drag());
        let painter = ui.painter().clone();
        let full = Rect::from_min_size(to_win(Pos2::ZERO), screen.size());
        painter.image(self.texture.id(), full, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        ctx.set_cursor_icon(CursorIcon::Crosshair);

        let pointer = resp.hover_pos().or(ctx.input(|i| i.pointer.latest_pos())).map(to_local);

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            return Outcome::Cancel;
        }
        if ctx.input(|i| i.key_pressed(Key::Enter)) {
            return Outcome::Selected(screen);
        }
        if resp.drag_started() {
            self.drag_from = ctx.input(|i| i.pointer.press_origin()).map(to_local);
        }
        if resp.dragged() {
            self.drag_to = resp.interact_pointer_pos().map(to_local);
        }
        let hovered_window = pointer.and_then(|p| {
            self.snap
                .windows
                .iter()
                .find(|w| Rect::from_min_size(Pos2::new(w.x, w.y), Vec2::new(w.w, w.h)).contains(p))
        });
        let dragging = self.drag_from.zip(self.drag_to).map(|(a, b)| Rect::from_two_pos(a, b).intersect(screen));

        if resp.drag_stopped() {
            if let Some(r) = dragging.filter(|r| r.width() >= 4.0 && r.height() >= 4.0) {
                return Outcome::Selected(r);
            }
            self.drag_from = None;
            self.drag_to = None;
        }
        if resp.clicked() {
            let r = hovered_window
                .map(|w| Rect::from_min_size(Pos2::new(w.x, w.y), Vec2::new(w.w, w.h)))
                .unwrap_or(screen);
            return Outcome::Selected(r.intersect(screen));
        }

        // Dim everything except the focus rect.
        let focus = dragging.or_else(|| hovered_window.map(|w| Rect::from_min_size(Pos2::new(w.x, w.y), Vec2::new(w.w, w.h))));
        let shade = Color32::from_black_alpha(110);
        match focus {
            Some(f) => {
                let f = Rect::from_min_max(to_win(f.min), to_win(f.max));
                for r in [
                    Rect::from_min_max(full.min, Pos2::new(full.max.x, f.min.y)),
                    Rect::from_min_max(Pos2::new(full.min.x, f.max.y), full.max),
                    Rect::from_min_max(Pos2::new(full.min.x, f.min.y), Pos2::new(f.min.x, f.max.y)),
                    Rect::from_min_max(Pos2::new(f.max.x, f.min.y), Pos2::new(full.max.x, f.max.y)),
                ] {
                    painter.rect_filled(r, 0.0, shade);
                }
                painter.rect_stroke(f, 0.0, Stroke::new(2.0, ACCENT), StrokeKind::Outside);
                let label = match (dragging, hovered_window) {
                    (Some(d), _) => {
                        let s = self.snap.scale;
                        format!("{:.0} × {:.0}", d.width() * s, d.height() * s)
                    }
                    (None, Some(w)) => w.label.clone(),
                    _ => String::new(),
                };
                label_box(&painter, Pos2::new(f.min.x, (f.min.y - 28.0).max(full.min.y + 4.0)), &label);
            }
            None => {
                painter.rect_filled(full, 0.0, shade);
            }
        }

        if let Some(p) = pointer {
            let pw = to_win(p);
            if dragging.is_none() {
                let guide = Stroke::new(1.0, Color32::from_white_alpha(140));
                painter.line_segment([Pos2::new(full.min.x, pw.y), Pos2::new(full.max.x, pw.y)], guide);
                painter.line_segment([Pos2::new(pw.x, full.min.y), Pos2::new(pw.x, full.max.y)], guide);
            }
            self.magnifier(&painter, p, pw, full);
        }

        let hint = format!(
            "{}  ·  drag to select  ·  click a window  ·  Enter = full screen  ·  Esc = cancel",
            self.purpose.title()
        );
        let galley = painter.layout_no_wrap(hint, FontId::proportional(15.0), Color32::WHITE);
        let hint_rect = Align2::CENTER_CENTER.anchor_size(Pos2::new(full.center().x, full.min.y + 40.0), galley.size());
        painter.rect_filled(hint_rect.expand2(Vec2::new(14.0, 8.0)), 8.0, Color32::from_black_alpha(190));
        painter.galley(hint_rect.min, galley, Color32::WHITE);

        ctx.request_repaint();
        Outcome::Pending
    }

    /// Zoomed loupe with the pixel grid and the color under the cursor.
    fn magnifier(&self, painter: &egui::Painter, local: Pos2, win: Pos2, full: Rect) {
        let s = self.snap.scale;
        let (iw, ih) = (self.snap.image.width() as f32, self.snap.image.height() as f32);
        let (px, py) = ((local.x * s).floor(), (local.y * s).floor());
        if px < 0.0 || py < 0.0 || px >= iw || py >= ih {
            return;
        }
        let cells: f32 = 15.0;
        let size = 135.0;
        let mut pos = win + Vec2::new(24.0, 24.0);
        if pos.x + size > full.max.x {
            pos.x = win.x - 24.0 - size;
        }
        if pos.y + size + 30.0 > full.max.y {
            pos.y = win.y - 24.0 - size - 30.0;
        }
        let r = Rect::from_min_size(pos, Vec2::splat(size));
        let half = (cells / 2.0).floor();
        let uv = Rect::from_min_max(
            Pos2::new((px - half) / iw, (py - half) / ih),
            Pos2::new((px - half + cells) / iw, (py - half + cells) / ih),
        );
        painter.rect_filled(r.expand(2.0), 4.0, Color32::BLACK);
        painter.image(self.texture.id(), r, uv, Color32::WHITE);
        let cell = size / cells;
        let center = Rect::from_min_size(r.min + Vec2::splat(half * cell), Vec2::splat(cell));
        painter.rect_stroke(center, 0.0, Stroke::new(1.5, ACCENT), StrokeKind::Outside);
        painter.rect_stroke(r, 4.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Outside);

        let c = self.snap.image.get_pixel(px as u32, py as u32);
        let text = format!("#{:02X}{:02X}{:02X}   {:.0}, {:.0}", c[0], c[1], c[2], px, py);
        label_box(painter, Pos2::new(r.min.x, r.max.y + 6.0), &text);
    }
}

fn label_box(painter: &egui::Painter, at: Pos2, text: &str) {
    if text.is_empty() {
        return;
    }
    let font = FontId::monospace(12.0);
    let galley = painter.layout_no_wrap(text.to_owned(), font, Color32::WHITE);
    let rect = Rect::from_min_size(at, galley.size() + Vec2::new(12.0, 8.0));
    painter.rect_filled(rect, 4.0, Color32::from_black_alpha(200));
    painter.galley(at + Vec2::new(6.0, 4.0), galley, Color32::WHITE);
}
