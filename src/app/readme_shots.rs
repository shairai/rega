//! Renders the README screenshots off-screen, with made-up demo content.
//!
//! ```sh
//! cargo test --release readme_screenshots -- --ignored
//! ```
//! Images are written to `docs/images/`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use image::RgbaImage;

use super::{Activity, RegaApp, Mode, SettingsTab};
use crate::capture::{Snapshot, WinRect};
use crate::editor::Editor;
use crate::editor::model::{Annotation, BlurStyle, DEFAULT_BLUR_STRENGTH, Kind, P, Project, Stamp};
use crate::overlay::{Overlay, Purpose};
use crate::theme;

mod demo;

const PPP: f32 = 2.0;
const APP_SIZE: Vec2 = Vec2::new(1360.0, 900.0);

// ---------- demo content ----------

/// Render arbitrary painting to an image at Retina resolution.
fn render_ui(size: Vec2, mut draw: impl FnMut(&mut egui::Ui)) -> RgbaImage {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(PPP)
        .with_context_setup(theme::install)
        .wgpu()
        .build_ui(move |ui| draw(ui));
    for _ in 0..4 {
        harness.step();
    }
    harness.render().expect("render")
}

fn text(p: &egui::Painter, pos: Pos2, s: &str, font: FontId, color: Color32) -> Rect {
    p.text(pos, Align2::LEFT_TOP, s, font, color)
}

fn window_dots(p: &egui::Painter, at: Pos2) {
    for (i, c) in [(255, 95, 87), (254, 188, 46), (40, 200, 64)].into_iter().enumerate() {
        p.circle_filled(at + Vec2::new(i as f32 * 20.0, 0.0), 6.0, Color32::from_rgb(c.0, c.1, c.2));
    }
}

/// Where things sit on the fake analytics dashboard (logical points).
struct Dash {
    nav: Vec<Rect>,
    cards: Vec<Rect>,
    chart: Rect,
    bars: Vec<Rect>,
    table: Rect,
    email_col: Rect,
    rows: Vec<Rect>,
}

const DASH: Vec2 = Vec2::new(1280.0, 800.0);

fn dash_layout() -> Dash {
    let (w, h) = (DASH.x, DASH.y);
    let top = 38.0;
    let nav = (0..5).map(|i| Rect::from_min_size(Pos2::new(16.0, top + 70.0 + i as f32 * 42.0), Vec2::new(188.0, 36.0))).collect();
    let x0 = 240.0;
    let cw = (w - x0 - 32.0 - 3.0 * 16.0) / 4.0;
    let cards = (0..4).map(|i| Rect::from_min_size(Pos2::new(x0 + i as f32 * (cw + 16.0), top + 92.0), Vec2::new(cw, 108.0))).collect();
    let chart = Rect::from_min_size(Pos2::new(x0, top + 218.0), Vec2::new((w - x0 - 32.0) * 0.64, 270.0));
    let bars = (0..12)
        .map(|i| {
            let heights = [0.42, 0.5, 0.46, 0.58, 0.55, 0.63, 0.6, 0.7, 0.66, 0.78, 0.74, 0.92];
            let bw = (chart.width() - 48.0) / 12.0;
            let bh = (chart.height() - 86.0) * heights[i];
            Rect::from_min_max(
                Pos2::new(chart.min.x + 24.0 + i as f32 * bw + 6.0, chart.max.y - 34.0 - bh),
                Pos2::new(chart.min.x + 24.0 + (i + 1) as f32 * bw - 6.0, chart.max.y - 34.0),
            )
        })
        .collect();
    let table = Rect::from_min_max(Pos2::new(x0, chart.max.y + 18.0), Pos2::new(w - 32.0, h - 24.0));
    let rows = (0..4).map(|i| Rect::from_min_size(Pos2::new(table.min.x, table.min.y + 76.0 + i as f32 * 40.0), Vec2::new(table.width(), 40.0))).collect();
    let email_col = Rect::from_min_max(Pos2::new(table.min.x + 230.0, table.min.y + 80.0), Pos2::new(table.min.x + 500.0, table.min.y + 76.0 + 4.0 * 40.0));
    Dash { nav, cards, chart, bars, table, email_col, rows }
}

fn draw_dashboard(ui: &mut egui::Ui) {
    let d = dash_layout();
    let p = ui.painter();
    let full = Rect::from_min_size(Pos2::ZERO, DASH);
    let ink = Color32::from_rgb(23, 26, 35);
    let muted = Color32::from_rgb(110, 117, 135);
    let line = Color32::from_rgb(229, 231, 238);
    let brand = Color32::from_rgb(79, 70, 229);
    p.rect_filled(full, 0.0, Color32::from_rgb(245, 246, 250));
    // title bar
    p.rect_filled(Rect::from_min_size(Pos2::ZERO, Vec2::new(DASH.x, 38.0)), 0.0, Color32::from_rgb(236, 237, 242));
    window_dots(p, Pos2::new(22.0, 19.0));
    p.text(Pos2::new(DASH.x / 2.0, 19.0), Align2::CENTER_CENTER, "app.acme-analytics.example", FontId::proportional(13.0), muted);
    // sidebar
    p.rect_filled(Rect::from_min_max(Pos2::new(0.0, 38.0), Pos2::new(220.0, DASH.y)), 0.0, Color32::WHITE);
    p.circle_filled(Pos2::new(36.0, 80.0), 14.0, brand);
    text(p, Pos2::new(58.0, 70.0), "Acme Analytics", theme::bold(17.0), ink);
    for (i, (r, label)) in d.nav.iter().zip(["Overview", "Customers", "Revenue", "Reports", "Settings"]).enumerate() {
        if i == 0 {
            p.rect_filled(*r, 8.0, Color32::from_rgb(238, 237, 253));
        }
        p.circle_stroke(r.left_center() + Vec2::new(20.0, 0.0), 6.0, Stroke::new(2.0, if i == 0 { brand } else { muted }));
        text(p, r.left_center() + Vec2::new(38.0, -9.0), label, theme::medium(14.5), if i == 0 { brand } else { ink });
    }
    // header
    text(p, Pos2::new(240.0, 62.0), "Overview", theme::bold(26.0), ink);
    text(p, Pos2::new(240.0, 98.0), "Last 30 days · updated 5 min ago", FontId::proportional(13.5), muted);
    let search = Rect::from_min_size(Pos2::new(DASH.x - 360.0, 66.0), Vec2::new(260.0, 38.0));
    p.rect_filled(search, 19.0, Color32::WHITE);
    p.rect_stroke(search, 19.0, Stroke::new(1.0, line), StrokeKind::Inside);
    text(p, search.left_center() + Vec2::new(18.0, -9.0), "Search reports…", FontId::proportional(14.0), muted);
    p.circle_filled(Pos2::new(DASH.x - 60.0, 85.0), 19.0, Color32::from_rgb(253, 186, 116));
    // stat cards
    for (r, (label, value, delta, up)) in d.cards.iter().zip([
        ("Revenue", "$48,210", "+12.4% vs last month", true),
        ("Active users", "3,214", "+4.1%", true),
        ("Conversion", "4.6%", "+0.3 pt", true),
        ("Churn", "1.2%", "-0.4 pt", false),
    ]) {
        p.rect_filled(*r, 14.0, Color32::WHITE);
        p.rect_stroke(*r, 14.0, Stroke::new(1.0, line), StrokeKind::Inside);
        text(p, r.min + Vec2::new(18.0, 16.0), label, theme::medium(13.5), muted);
        text(p, r.min + Vec2::new(18.0, 40.0), value, theme::bold(28.0), ink);
        let c = if up { Color32::from_rgb(22, 163, 74) } else { Color32::from_rgb(220, 38, 38) };
        text(p, r.min + Vec2::new(18.0, 80.0), delta, theme::medium(12.5), c);
    }
    // chart
    p.rect_filled(d.chart, 14.0, Color32::WHITE);
    p.rect_stroke(d.chart, 14.0, Stroke::new(1.0, line), StrokeKind::Inside);
    text(p, d.chart.min + Vec2::new(20.0, 16.0), "Monthly revenue", theme::bold(16.0), ink);
    for (i, b) in d.bars.iter().enumerate() {
        let c = if i == 11 { brand } else { Color32::from_rgb(199, 210, 254) };
        p.rect_filled(*b, 5.0, c);
        let m = ["J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D"][i];
        p.text(Pos2::new(b.center().x, d.chart.max.y - 18.0), Align2::CENTER_CENTER, m, FontId::proportional(12.0), muted);
    }
    // channels
    let side = Rect::from_min_max(Pos2::new(d.chart.max.x + 16.0, d.chart.min.y), Pos2::new(DASH.x - 32.0, d.chart.max.y));
    p.rect_filled(side, 14.0, Color32::WHITE);
    p.rect_stroke(side, 14.0, Stroke::new(1.0, line), StrokeKind::Inside);
    text(p, side.min + Vec2::new(20.0, 16.0), "Top channels", theme::bold(16.0), ink);
    for (i, (name, share)) in [("Organic search", 0.46), ("Newsletter", 0.27), ("Paid social", 0.17), ("Referral", 0.1)].into_iter().enumerate() {
        let y = side.min.y + 62.0 + i as f32 * 48.0;
        text(p, Pos2::new(side.min.x + 20.0, y), name, FontId::proportional(13.5), ink);
        p.text(Pos2::new(side.max.x - 20.0, y), Align2::RIGHT_TOP, format!("{:.0}%", share * 100.0), theme::medium(13.0), muted);
        let track = Rect::from_min_size(Pos2::new(side.min.x + 20.0, y + 24.0), Vec2::new(side.width() - 40.0, 6.0));
        p.rect_filled(track, 3.0, Color32::from_rgb(238, 239, 244));
        p.rect_filled(Rect::from_min_size(track.min, Vec2::new(track.width() * share, 6.0)), 3.0, brand);
    }
    // table
    p.rect_filled(d.table, 14.0, Color32::WHITE);
    p.rect_stroke(d.table, 14.0, Stroke::new(1.0, line), StrokeKind::Inside);
    text(p, d.table.min + Vec2::new(20.0, 16.0), "Newest customers", theme::bold(16.0), ink);
    let cols = [20.0, 230.0, 500.0, 640.0, 760.0];
    for (c, h) in cols.iter().zip(["Customer", "Email", "Plan", "MRR", "Status"]) {
        text(p, d.table.min + Vec2::new(*c, 50.0), h, theme::medium(12.0), muted);
    }
    for (r, row) in d.rows.iter().zip([
        ["Jane Cooper", "jane.cooper@example.com", "Business", "$249", "Active"],
        ["Wade Warren", "wade@northwind.example", "Starter", "$29", "Trial"],
        ["Esther Howard", "esther.h@contoso.example", "Business", "$249", "Active"],
        ["Cody Fisher", "cody.fisher@example.org", "Pro", "$99", "Past due"],
    ]) {
        p.line_segment([r.left_top() + Vec2::new(16.0, 0.0), r.right_top() - Vec2::new(16.0, 0.0)], Stroke::new(1.0, line));
        for (c, cell) in cols.iter().zip(row) {
            let pos = Pos2::new(d.table.min.x + c, r.center().y - 9.0);
            if c == &760.0 {
                let (bg, fg) = match cell {
                    "Active" => (Color32::from_rgb(220, 252, 231), Color32::from_rgb(21, 128, 61)),
                    "Trial" => (Color32::from_rgb(224, 231, 255), brand),
                    _ => (Color32::from_rgb(254, 226, 226), Color32::from_rgb(185, 28, 28)),
                };
                let pill = Rect::from_min_size(pos - Vec2::new(0.0, 3.0), Vec2::new(74.0, 24.0));
                p.rect_filled(pill, 12.0, bg);
                p.text(pill.center(), Align2::CENTER_CENTER, cell, theme::medium(12.0), fg);
            } else {
                text(p, pos, cell, FontId::proportional(14.0), ink);
            }
        }
    }
}

const CODE: Vec2 = Vec2::new(1100.0, 700.0);

fn draw_code_editor(ui: &mut egui::Ui) {
    let p = ui.painter();
    let full = Rect::from_min_size(Pos2::ZERO, CODE);
    p.rect_filled(full, 0.0, Color32::from_rgb(30, 32, 40));
    p.rect_filled(Rect::from_min_size(Pos2::ZERO, Vec2::new(CODE.x, 38.0)), 0.0, Color32::from_rgb(38, 40, 50));
    window_dots(p, Pos2::new(22.0, 19.0));
    p.text(Pos2::new(CODE.x / 2.0, 19.0), Align2::CENTER_CENTER, "checkout.rs — shop", FontId::proportional(13.0), Color32::from_gray(160));
    p.rect_filled(Rect::from_min_max(Pos2::new(0.0, 38.0), Pos2::new(220.0, CODE.y)), 0.0, Color32::from_rgb(25, 27, 34));
    for (i, f) in ["src", "  cart.rs", "  checkout.rs", "  payment.rs", "  main.rs", "tests", "Cargo.toml"].iter().enumerate() {
        let c = if i == 2 { Color32::WHITE } else { Color32::from_gray(150) };
        if i == 2 {
            p.rect_filled(Rect::from_min_size(Pos2::new(0.0, 56.0 + i as f32 * 28.0), Vec2::new(220.0, 28.0)), 0.0, Color32::from_rgb(45, 48, 62));
        }
        text(p, Pos2::new(20.0, 62.0 + i as f32 * 28.0), f, FontId::monospace(13.0), c);
    }
    let kw = Color32::from_rgb(198, 120, 221);
    let ty = Color32::from_rgb(229, 192, 123);
    let fun = Color32::from_rgb(97, 175, 239);
    let st = Color32::from_rgb(152, 195, 121);
    let pl = Color32::from_rgb(171, 178, 191);
    let cm = Color32::from_rgb(92, 99, 112);
    let lines: Vec<Vec<(&str, Color32)>> = vec![
        vec![("use ", kw), ("crate::cart::", pl), ("Cart", ty), (";", pl)],
        vec![],
        vec![("/// Charge the customer and create the order.", cm)],
        vec![("pub fn ", kw), ("checkout", fun), ("(cart: &", pl), ("Cart", ty), (", card: &", pl), ("Card", ty), (") -> ", pl), ("Result", ty), ("<", pl), ("Order", ty), ("> {", pl)],
        vec![("    let ", kw), ("total = cart.", pl), ("total", fun), ("();", pl)],
        vec![("    if ", kw), ("total == ", pl), ("0", ty), (" {", pl)],
        vec![("        return ", kw), ("Err", ty), ("(", pl), ("\"cart is empty\"", st), (".into());", pl)],
        vec![("    }", pl)],
        vec![("    let ", kw), ("payment = payment::", pl), ("charge", fun), ("(card, total)?;", pl)],
        vec![("    ", pl), ("Ok", ty), ("(", pl), ("Order", ty), ("::", pl), ("new", fun), ("(cart, payment))", pl)],
        vec![("}", pl)],
    ];
    for (i, line) in lines.iter().enumerate() {
        let y = 62.0 + i as f32 * 26.0;
        p.text(Pos2::new(262.0, y), Align2::RIGHT_TOP, (i + 1).to_string(), FontId::monospace(13.0), Color32::from_gray(90));
        let mut x = 284.0;
        for (seg, c) in line {
            let r = text(p, Pos2::new(x, y), seg, FontId::monospace(14.0), *c);
            x = r.max.x;
        }
    }
    // terminal
    let term = Rect::from_min_max(Pos2::new(220.0, CODE.y - 200.0), CODE.to_pos2());
    p.rect_filled(term, 0.0, Color32::from_rgb(24, 25, 32));
    text(p, term.min + Vec2::new(20.0, 14.0), "TERMINAL", theme::medium(11.0), Color32::from_gray(140));
    text(p, term.min + Vec2::new(20.0, 44.0), "$ cargo test", FontId::monospace(13.5), pl);
    text(p, term.min + Vec2::new(20.0, 70.0), "test checkout::empty_cart ... ok", FontId::monospace(13.5), st);
    text(p, term.min + Vec2::new(20.0, 96.0), "test checkout::declined_card ... FAILED", FontId::monospace(13.5), Color32::from_rgb(224, 108, 117));
}

const LANDING: Vec2 = Vec2::new(1200.0, 760.0);

fn draw_landing(ui: &mut egui::Ui) {
    let p = ui.painter();
    let full = Rect::from_min_size(Pos2::ZERO, LANDING);
    p.rect_filled(full, 0.0, Color32::from_rgb(12, 14, 30));
    theme::gradient_rect(p, Rect::from_center_size(Pos2::new(LANDING.x * 0.7, 260.0), Vec2::new(700.0, 420.0)), 210, Color32::from_white_alpha(70));
    p.rect_filled(Rect::from_min_size(Pos2::ZERO, Vec2::new(LANDING.x, 38.0)), 0.0, Color32::from_rgb(30, 32, 44));
    window_dots(p, Pos2::new(22.0, 19.0));
    text(p, Pos2::new(60.0, 66.0), "Nimbus", theme::bold(20.0), Color32::WHITE);
    for (i, l) in ["Product", "Pricing", "Docs", "Blog"].iter().enumerate() {
        text(p, Pos2::new(520.0 + i as f32 * 100.0, 70.0), l, theme::medium(14.0), Color32::from_gray(200));
    }
    text(p, Pos2::new(60.0, 200.0), "Ship faster with", theme::bold(54.0), Color32::WHITE);
    text(p, Pos2::new(60.0, 266.0), "fewer meetings.", theme::bold(54.0), Color32::from_rgb(196, 181, 253));
    text(p, Pos2::new(60.0, 350.0), "Async updates, decisions and docs in one calm place.", FontId::proportional(19.0), Color32::from_gray(190));
    let cta = Rect::from_min_size(Pos2::new(60.0, 410.0), Vec2::new(190.0, 52.0));
    theme::gradient_rect(p, cta, 26, Color32::WHITE);
    p.text(cta.center(), Align2::CENTER_CENTER, "Start free trial", theme::medium(16.0), Color32::WHITE);
    for (i, (t, d)) in [("Threads", "Every decision, findable"), ("Docs", "Specs next to the chat"), ("Digests", "One email, not fifty")].iter().enumerate() {
        let r = Rect::from_min_size(Pos2::new(60.0 + i as f32 * 370.0, 540.0), Vec2::new(340.0, 150.0));
        p.rect_filled(r, 16.0, Color32::from_rgb(24, 27, 48));
        p.rect_stroke(r, 16.0, Stroke::new(1.0, Color32::from_rgb(48, 52, 82)), StrokeKind::Inside);
        text(p, r.min + Vec2::new(24.0, 26.0), t, theme::bold(19.0), Color32::WHITE);
        text(p, r.min + Vec2::new(24.0, 62.0), d, FontId::proportional(15.0), Color32::from_gray(170));
    }
}

/// Logical-point rect on a dashboard image → image pixels.
fn px(r: Rect) -> (P, P) {
    (P::new(r.min.x * PPP, r.min.y * PPP), P::new(r.max.x * PPP, r.max.y * PPP))
}

fn green() -> [u8; 4] {
    [46, 184, 92, 255]
}

fn ann(kind: Kind, color: [u8; 4], width: f32) -> Annotation {
    Annotation { kind, color, width, filled: false }
}

/// The marked-up dashboard used for the hero shot.
fn dashboard_project() -> Project {
    let d = dash_layout();
    let red = [235, 59, 52, 255];
    let blue = [30, 136, 229, 255];
    let (ca, cb) = px(d.cards[0].expand(6.0));
    let (ha, hb) = px(d.bars[11].expand(10.0));
    let (ea, eb) = px(d.email_col);
    let row = d.rows[3];
    let mut annotations = vec![
        ann(Kind::pixelate(ea, eb), red, 8.0),
        ann(Kind::Rect { a: ca, b: cb }, red, 8.0),
        ann(Kind::Arrow { a: P::new(1180.0, 760.0), b: P::new(cb.x + 30.0, cb.y + 10.0) }, red, 12.0),
        ann(
            Kind::Text { pos: P::new(1200.0, 700.0), text: "Revenue up 12% this month".into(), size: 46.0, background: true },
            red,
            8.0,
        ),
        ann(Kind::Ellipse { a: ha, b: hb }, [46, 184, 92, 255], 8.0),
        ann(
            Kind::Highlight {
                pts: vec![P::new((row.min.x + 16.0) * PPP, row.center().y * PPP), P::new((row.min.x + 210.0) * PPP, row.center().y * PPP)],
            },
            [255, 214, 0, 255],
            10.0,
        ),
    ];
    for (n, r) in d.nav.iter().take(3).enumerate() {
        annotations.push(ann(
            Kind::Step { pos: P::new((r.max.x - 18.0) * PPP, r.center().y * PPP), n: n as u32 + 1, radius: 26.0 },
            blue,
            6.0,
        ));
    }
    // Stamps beside the status pills.
    for (row, stamp, color) in [(0, Stamp::Check, green()), (2, Stamp::Check, green()), (3, Stamp::Warning, red)] {
        let at = Pos2::new(d.table.min.x + 870.0, d.rows[row].center().y);
        annotations.push(ann(Kind::Stamp { pos: P::new(at.x * PPP, at.y * PPP), size: 30.0 * PPP, stamp }, color, 6.0));
    }
    Project { annotations, effects: Default::default(), next_step: 4 }
}

// ---------- README examples ----------
// Built the way a user would: crops pasted onto a blank canvas, then marked up
// with Rega's own tools and flattened by its export renderer.

const EX_MARGIN: f32 = 48.0;
const EX_BG: [u8; 4] = [236, 238, 244, 255];
const EX_INK: [u8; 4] = [23, 26, 35, 255];
const EX_MUTED: [u8; 4] = [110, 117, 135, 255];

/// A logical-point rect cut out of a Retina dashboard image.
fn crop_pts(img: &RgbaImage, r: Rect) -> RgbaImage {
    let (a, b) = px(r);
    image::imageops::crop_imm(img, a.x as u32, a.y as u32, (b.x - a.x) as u32, (b.y - a.y) as u32).to_image()
}

/// A pasted image layer at `pos`, with a thin frame around it.
fn pasted(img: RgbaImage, pos: P) -> [Annotation; 2] {
    let (w, h) = (img.width() as f32, img.height() as f32);
    [
        ann(Kind::Image { pos, w, h, img: Arc::new(img) }, [0; 4], 0.0),
        ann(Kind::Rect { a: P::new(pos.x - 2.0, pos.y - 2.0), b: P::new(pos.x + w + 2.0, pos.y + h + 2.0) }, [214, 218, 228, 255], 3.0),
    ]
}

fn label(pos: P, text: &str, size: f32, color: [u8; 4]) -> Annotation {
    ann(Kind::Text { pos, text: text.into(), size, background: false }, color, 0.0)
}

fn flatten(w: f32, h: f32, anns: &[Annotation]) -> RgbaImage {
    let canvas = RgbaImage::from_pixel(w.round() as u32, h.round() as u32, image::Rgba(EX_BG));
    crate::editor::render::render(&canvas, anns, &Default::default())
}

/// The customer table before and after blurring names and emails (`B`).
/// The dashboard with the customer table's names pixelated and emails blurred.
fn blurred_table(dashboard: &RgbaImage) -> RgbaImage {
    let d = dash_layout();
    // One blur per cell, as a user would drag them, so row dividers stay intact.
    let blur = |x0: f32, x1: f32, r: &Rect, style| {
        let (a, b) = px(Rect::from_min_max(Pos2::new(d.table.min.x + x0, r.center().y - 13.0), Pos2::new(d.table.min.x + x1, r.center().y + 13.0)));
        ann(Kind::Blur { a, b, style, strength: DEFAULT_BLUR_STRENGTH }, [0; 4], 0.0)
    };
    // Both styles: names pixelated, emails blurred.
    let marks: Vec<Annotation> =
        d.rows.iter().flat_map(|r| [blur(12.0, 150.0, r, BlurStyle::Pixelate), blur(222.0, 440.0, r, BlurStyle::Gaussian)]).collect();
    crate::editor::render::render(dashboard, &marks, &Default::default())
}

fn blur_example(dashboard: &RgbaImage) -> RgbaImage {
    let d = dash_layout();
    let blurred = blurred_table(dashboard);
    // Customer, email and plan columns.
    let region = Rect::from_min_max(d.table.min - Vec2::splat(12.0), Pos2::new(d.table.min.x + 620.0, d.table.max.y + 12.0));
    let (before, after) = (crop_pts(dashboard, region), crop_pts(&blurred, region));
    let (pw, ph) = (before.width() as f32, before.height() as f32);
    let (gap, title) = (64.0, 76.0);
    let top = EX_MARGIN + title;
    let right = EX_MARGIN + pw + gap;
    let mut anns = vec![
        label(P::new(EX_MARGIN, EX_MARGIN), "Before", 36.0, EX_MUTED),
        label(P::new(right, EX_MARGIN), "After: B, then Pixelate names and Blur emails", 36.0, EX_INK),
    ];
    anns.extend(pasted(before, P::new(EX_MARGIN, top)));
    anns.extend(pasted(after, P::new(right, top)));
    flatten(right + pw + EX_MARGIN, top + ph + EX_MARGIN, &anns)
}

/// Numbered steps (`N`) on the dashboard, with a legend typed with `T`.
fn steps_example(dashboard: &RgbaImage) -> RgbaImage {
    let d = dash_layout();
    let blue = [30, 136, 229, 255];
    let radius = 26.0;
    let search = Rect::from_min_size(Pos2::new(DASH.x - 360.0, 66.0), Vec2::new(260.0, 38.0));
    let spots = [
        (Pos2::new(d.nav[1].max.x - 18.0, d.nav[1].center().y), "Open Customers"),
        (Pos2::new(search.max.x - 20.0, search.center().y), "Search for a report"),
        (Pos2::new(d.cards[0].max.x - 22.0, d.cards[0].min.y + 22.0), "Check this month's revenue"),
        (Pos2::new(d.cards[3].max.x - 22.0, d.cards[3].min.y + 22.0), "Keep an eye on churn"),
    ];
    let step = |n: usize, pos: P| ann(Kind::Step { pos, n: n as u32 + 1, radius }, blue, 6.0);
    let marks: Vec<Annotation> = spots.iter().enumerate().map(|(n, (p, _))| step(n, P::new(p.x * PPP, p.y * PPP))).collect();
    let marked = crate::editor::render::render(dashboard, &marks, &Default::default());
    let shot = crop_pts(&marked, Rect::from_min_max(Pos2::new(0.0, 38.0), Pos2::new(DASH.x, 264.0)));
    let (pw, ph) = (shot.width() as f32, shot.height() as f32);

    let mut anns = pasted(shot, P::new(EX_MARGIN, EX_MARGIN)).to_vec();
    // Legend: two columns of "(n) what to do".
    let legend_top = EX_MARGIN + ph + 56.0;
    let (col_w, row_h) = (pw / 2.0, 84.0);
    for (n, (_, what)) in spots.iter().enumerate() {
        let x = EX_MARGIN + (n % 2) as f32 * col_w + radius;
        let y = legend_top + (n / 2) as f32 * row_h + radius;
        anns.push(step(n, P::new(x, y)));
        anns.push(label(P::new(x + radius + 22.0, y - 22.0), what, 38.0, EX_INK));
    }
    flatten(pw + 2.0 * EX_MARGIN, legend_top + 2.0 * row_h + EX_MARGIN - 20.0, &anns)
}

/// Review feedback on the dashboard: stamps (`K`) and a magnifier (`M`).
fn feedback_example(dashboard: &RgbaImage) -> RgbaImage {
    let d = dash_layout();
    let (red, blue, orange) = ([235, 59, 52, 255], [30, 136, 229, 255], [255, 140, 0, 255]);
    let at = |p: Pos2| P::new(p.x * PPP, p.y * PPP);
    let stamp = |p: Pos2, stamp, color| ann(Kind::Stamp { pos: at(p), size: 46.0 * PPP, stamp }, color, 6.0);
    let corner = |r: &Rect| Pos2::new(r.max.x - 28.0, r.min.y + 28.0);
    let side = Rect::from_min_max(Pos2::new(d.chart.max.x + 16.0, d.chart.min.y), Pos2::new(DASH.x - 32.0, d.chart.max.y));
    // The tiny "10%" share of the last channel, under a 2.5x loupe.
    let referral = Pos2::new(side.max.x - 34.0, side.min.y + 62.0 + 3.0 * 48.0 + 9.0);
    let marks = vec![
        stamp(corner(&d.cards[0]), Stamp::Check, green()),
        stamp(corner(&d.cards[1]), Stamp::ThumbsUp, blue),
        stamp(corner(&d.cards[2]), Stamp::Question, orange),
        stamp(corner(&d.cards[3]), Stamp::Warning, red),
        stamp(Pos2::new(d.bars[11].center().x, d.bars[11].min.y - 30.0), Stamp::Star, orange),
        ann(Kind::Magnify { a: at(referral - Vec2::splat(46.0)), b: at(referral + Vec2::splat(46.0)), zoom: 2.0 }, red, 6.0),
    ];
    let marked = crate::editor::render::render(dashboard, &marks, &Default::default());
    let shot = crop_pts(&marked, Rect::from_min_max(Pos2::new(220.0, 38.0), Pos2::new(DASH.x, d.chart.max.y + 14.0)));
    let (pw, ph) = (shot.width() as f32, shot.height() as f32);
    let title = 76.0;
    let mut anns = vec![label(P::new(EX_MARGIN, EX_MARGIN), "Review feedback: K for stamps, M for the magnifier", 36.0, EX_INK)];
    anns.extend(pasted(shot, P::new(EX_MARGIN, EX_MARGIN + title)));
    flatten(pw + 2.0 * EX_MARGIN, ph + title + 2.0 * EX_MARGIN, &anns)
}

/// The blur example for a small card on the website: before above after,
/// first two rows, so both fit a 4:3 box.
fn blur_card(dashboard: &RgbaImage) -> RgbaImage {
    let d = dash_layout();
    let blurred = blurred_table(dashboard);
    let region = Rect::from_min_max(d.table.min - Vec2::splat(12.0), Pos2::new(d.table.min.x + 620.0, d.rows[1].max.y + 6.0));
    let (before, after) = (crop_pts(dashboard, region), crop_pts(&blurred, region));
    let (pw, ph) = (before.width() as f32, before.height() as f32);
    let (title, gap) = (76.0, 44.0);
    let second = EX_MARGIN + title + ph + gap;
    let mut anns = vec![
        label(P::new(EX_MARGIN, EX_MARGIN), "Before", 36.0, EX_MUTED),
        label(P::new(EX_MARGIN, second), "After: B, then Pixelate names and Blur emails", 36.0, EX_INK),
    ];
    anns.extend(pasted(before, P::new(EX_MARGIN, EX_MARGIN + title)));
    anns.extend(pasted(after, P::new(EX_MARGIN, second + title)));
    flatten(pw + 2.0 * EX_MARGIN, second + title + ph + EX_MARGIN, &anns)
}

/// The steps example for a small card on the website: three steps that fit
/// one compact crop, with the legend under it.
fn steps_card(dashboard: &RgbaImage) -> RgbaImage {
    let d = dash_layout();
    let (blue, radius) = ([30, 136, 229, 255], 26.0);
    let spots = [
        (Pos2::new(d.nav[1].max.x - 18.0, d.nav[1].center().y), "Open Customers"),
        (Pos2::new(d.cards[0].max.x - 22.0, d.cards[0].min.y + 22.0), "Check this month's revenue"),
        (Pos2::new(d.cards[1].max.x - 22.0, d.cards[1].min.y + 22.0), "Compare it with active users"),
    ];
    let step = |n: usize, pos: P| ann(Kind::Step { pos, n: n as u32 + 1, radius }, blue, 6.0);
    let marks: Vec<Annotation> = spots.iter().enumerate().map(|(n, (p, _))| step(n, P::new(p.x * PPP, p.y * PPP))).collect();
    let marked = crate::editor::render::render(dashboard, &marks, &Default::default());
    let shot = crop_pts(&marked, Rect::from_min_max(Pos2::new(0.0, 38.0), Pos2::new(d.cards[1].max.x + 16.0, d.cards[1].max.y + 16.0)));
    let (pw, ph) = (shot.width() as f32, shot.height() as f32);
    let mut anns = pasted(shot, P::new(EX_MARGIN, EX_MARGIN)).to_vec();
    let (legend_top, row_h) = (EX_MARGIN + ph + 56.0, 84.0);
    for (n, (_, what)) in spots.iter().enumerate() {
        let (x, y) = (EX_MARGIN + radius, legend_top + n as f32 * row_h + radius);
        anns.push(step(n, P::new(x, y)));
        anns.push(label(P::new(x + radius + 22.0, y - 22.0), what, 38.0, EX_INK));
    }
    flatten(pw + 2.0 * EX_MARGIN, legend_top + spots.len() as f32 * row_h + EX_MARGIN - 20.0, &anns)
}

fn write_examples(dashboard: &RgbaImage) {
    save(&blur_example(dashboard), "example-blur.png");
    save(&steps_example(dashboard), "example-steps.png");
    save(&feedback_example(dashboard), "example-feedback.png");
    // Card-sized versions for the website's examples (converted to WebP in site/images).
    save(&blur_card(dashboard), "card-blur.png");
    save(&steps_card(dashboard), "card-steps.png");
}

#[test]
#[ignore = "writes README example images; run with --ignored"]
fn readme_examples() {
    write_examples(&render_ui(DASH, draw_dashboard));
}

// ---------- app shots ----------

/// Build the app inside the harness so it shares the harness's egui context.
fn app_harness(size: Vec2, make: impl FnOnce(&egui::Context) -> RegaApp + 'static) -> Harness<'static, RegaApp> {
    Harness::builder()
        .with_size(size)
        .with_pixels_per_point(PPP)
        .with_max_steps(400)
        .with_accessibility_check(false)
        .wgpu()
        .build_eframe(move |cc| make(&cc.egui_ctx))
}

/// Step until the library thumbnails have loaded.
fn settle(h: &mut Harness<'_, RegaApp>) {
    for _ in 0..80 {
        h.step();
        let app = h.state();
        use crate::library::ItemKind;
        let pictured = app.library.iter().filter(|i| matches!(i.kind, ItemKind::Image | ItemKind::Guide)).count();
        if app.thumbs.len() >= pictured {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    for _ in 0..6 {
        h.step();
    }
}

/// macOS-style window frame: title bar with traffic lights, rounded corners, soft shadow.
fn window_chrome(img: &RgbaImage) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let bar = (28.0 * PPP) as u32;
    let radius = 12.0 * PPP;
    let margin = (36.0 * PPP) as u32;
    let mut win = RgbaImage::from_pixel(w, h + bar, image::Rgba([33, 36, 44, 255]));
    image::imageops::replace(&mut win, img, 0, bar as i64);
    for (i, c) in [[255, 95, 87], [254, 188, 46], [40, 200, 64]].iter().enumerate() {
        let (cx, cy) = (20.0 * PPP + i as f32 * 20.0 * PPP, bar as f32 / 2.0);
        for y in 0..bar {
            for x in 0..(90.0 * PPP) as u32 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let a = (6.0 * PPP + 0.5 - d).clamp(0.0, 1.0);
                if a > 0.0 {
                    let p = win.get_pixel_mut(x, y);
                    for k in 0..3 {
                        p[k] = (p[k] as f32 * (1.0 - a) + c[k] as f32 * a) as u8;
                    }
                }
            }
        }
    }
    let (ww, wh) = (w as f32, (h + bar) as f32);
    let corner_alpha = |x: f32, y: f32| -> f32 {
        let cx = x.clamp(radius, ww - radius);
        let cy = y.clamp(radius, wh - radius);
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        (radius + 0.5 - d).clamp(0.0, 1.0)
    };
    let (cw, ch) = (w + margin * 2, h + bar + margin * 2);
    let mut mask = image::GrayImage::new(cw, ch);
    for y in 0..(h + bar) {
        for x in 0..w {
            let a = corner_alpha(x as f32 + 0.5, y as f32 + 0.5);
            mask.put_pixel(x + margin, y + margin + (10.0 * PPP) as u32 % ch, image::Luma([(a * 255.0) as u8]));
        }
    }
    let mask = imageproc::filter::gaussian_blur_f32(&mask, 16.0 * PPP);
    let mut out = RgbaImage::from_fn(cw, ch, |x, y| image::Rgba([0, 0, 0, (mask.get_pixel(x, y)[0] as f32 * 0.5) as u8]));
    for y in 0..(h + bar) {
        for x in 0..w {
            let a = corner_alpha(x as f32 + 0.5, y as f32 + 0.5);
            if a <= 0.0 {
                continue;
            }
            let src = win.get_pixel(x, y);
            let dst = out.get_pixel_mut(x + margin, y + margin);
            let da = dst[3] as f32 / 255.0;
            let oa = a + da * (1.0 - a);
            for k in 0..3 {
                dst[k] = ((src[k] as f32 * a + dst[k] as f32 * da * (1.0 - a)) / oa.max(1e-6)) as u8;
            }
            dst[3] = (oa * 255.0) as u8;
        }
    }
    out
}

fn save(img: &RgbaImage, name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/images");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    img.save(&path).unwrap();
    println!("wrote {}", path.display());
}

fn demo_library() -> (PathBuf, RgbaImage) {
    // One folder per call: tests run side by side in one process.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rega-demo-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dashboard = render_ui(DASH, draw_dashboard);
    let code = render_ui(CODE, draw_code_editor);
    let landing = render_ui(LANDING, draw_landing);
    let annotated = crate::editor::render::render(&dashboard, &dashboard_project().annotations, &Default::default());
    // Newest first in the tray: give them increasing timestamps.
    let items: [(&str, &RgbaImage); 4] = [
        ("Rega 2026-11-02 at 09.10.00.png", &landing),
        ("Rega 2026-11-02 at 09.20.00.png", &code),
        ("Rega 2026-11-02 at 09.40.00.png", &annotated),
        ("Rega 2026-11-02 at 09.50.00.png", &dashboard),
    ];
    for (name, img) in items {
        img.save(dir.join(name)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(dir.join("Rega 2026-11-02 at 09.30.00.mp4"), b"demo").unwrap();
    let mp4 = dir.join("Rega 2026-11-02 at 09.30.00.mp4");
    let older = std::time::SystemTime::now() - Duration::from_secs(60);
    let _ = std::fs::File::options().write(true).open(&mp4).and_then(|f| f.set_modified(older));
    std::thread::sleep(Duration::from_millis(20));
    write_demo_guide(&dir, &dashboard, &code);
    (dir, dashboard)
}

/// The demo library's guide (its newest item).
const DEMO_GUIDE: &str = "Rega 2026-11-02 at 10.00.00.html";

/// A three-step guide through the demo dashboard and code editor.
fn write_demo_guide(library: &Path, dashboard: &RgbaImage, code: &RgbaImage) {
    use crate::guide::editor::GuideEditor;
    use crate::guide::{Action, Button, Guide, Shot, Step};
    let mut typed = dashboard.clone();
    for (x, y) in (1500..1720).flat_map(|x| (60..84).map(move |y| (x, y))) {
        typed.put_pixel(x, y, image::Rgba([30, 30, 30, 255]));
    }
    let step = |img: &RgbaImage, action: Action, marker: Option<[f32; 2]>, title: &str| Step {
        shot: Shot::encode(&crate::guide::fit_capture(img.clone(), Some(1920))).unwrap(),
        action,
        marker,
        marker_hidden: false,
        title: title.into(),
        note: String::new(),
    };
    let click = Action::Click { button: Button::Left, count: 1 };
    let mut steps = vec![
        step(dashboard, click.clone(), Some([0.075, 0.21]), "Open Customers"),
        step(&typed, Action::Type { text: "Jane".into(), then: Some("Enter".into()) }, Some([0.80, 0.105]), "Type “Jane” and press Enter"),
        step(code, click, Some([0.5, 0.4]), "Click here"),
    ];
    steps[0].note = "It's in the sidebar, under Overview.".into();
    let mut guide = Guide::new("Find a customer".into(), steps);
    guide.style.seconds = 5.0;
    // GUIDE_MARKER=hand (etc.) shows the other shapes.
    if let Ok(m) = std::env::var("GUIDE_MARKER") {
        guide.style.marker = serde_json::from_value(serde_json::Value::String(m)).unwrap();
    }
    GuideEditor::new(&egui::Context::default(), library.join(DEMO_GUIDE), guide).save_now().unwrap();
}

fn desktop(dashboard: &RgbaImage, code: &RgbaImage) -> (RgbaImage, Vec<WinRect>) {
    desktop_with_code_at(dashboard, code, Pos2::new(640.0, 330.0))
}

/// The demo desktop with the code window's top-left at `code_at`.
fn desktop_with_code_at(dashboard: &RgbaImage, code: &RgbaImage, code_at: Pos2) -> (RgbaImage, Vec<WinRect>) {
    let size = Vec2::new(1440.0, 900.0);
    let dash_rect = Rect::from_min_size(Pos2::new(70.0, 70.0), Vec2::new(880.0, 550.0));
    let code_rect = Rect::from_min_size(code_at, Vec2::new(740.0, 471.0));
    let to_color = |img: &RgbaImage| egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
    let (dash_c, code_c) = (to_color(dashboard), to_color(code));
    let mut textures: Option<(egui::TextureHandle, egui::TextureHandle)> = None;
    let img = render_ui(size, move |ui| {
        let (dt, ct) = textures.get_or_insert_with(|| {
            (
                ui.ctx().load_texture("dash", dash_c.clone(), egui::TextureOptions::LINEAR),
                ui.ctx().load_texture("code", code_c.clone(), egui::TextureOptions::LINEAR),
            )
        });
        let p = ui.painter();
        let full = Rect::from_min_size(Pos2::ZERO, size);
        p.rect_filled(full, 0.0, Color32::from_rgb(20, 24, 54));
        theme::gradient_rect(p, Rect::from_center_size(Pos2::new(1100.0, 250.0), Vec2::new(1100.0, 700.0)), 255, Color32::from_white_alpha(90));
        theme::gradient_rect(p, Rect::from_center_size(Pos2::new(250.0, 800.0), Vec2::new(900.0, 500.0)), 250, Color32::from_white_alpha(50));
        p.rect_filled(Rect::from_min_size(Pos2::ZERO, Vec2::new(size.x, 26.0)), 0.0, Color32::from_black_alpha(90));
        text(p, Pos2::new(16.0, 5.0), "Finder   File   Edit   View   Go   Window", theme::medium(13.0), Color32::WHITE);
        p.text(Pos2::new(size.x - 16.0, 13.0), Align2::RIGHT_CENTER, "Mon 2 Nov  10:42", theme::medium(13.0), Color32::WHITE);
        for (r, t) in [(dash_rect, dt.id()), (code_rect, ct.id())] {
            p.add(theme::soft_shadow(r, 10));
            p.add(egui::epaint::RectShape::filled(r, 10, Color32::WHITE).with_texture(t, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
        }
        let dock = Rect::from_center_size(Pos2::new(size.x / 2.0, size.y - 40.0), Vec2::new(520.0, 64.0));
        p.rect_filled(dock, 18.0, Color32::from_white_alpha(40));
        for i in 0..8 {
            let c = [(255, 94, 98), (124, 92, 255), (52, 199, 89), (255, 159, 10), (10, 132, 255), (191, 90, 242), (255, 214, 10), (100, 210, 255)][i];
            let r = Rect::from_center_size(Pos2::new(dock.min.x + 40.0 + i as f32 * 62.0, dock.center().y), Vec2::splat(46.0));
            p.rect_filled(r, 11.0, Color32::from_rgb(c.0, c.1, c.2));
        }
    });
    let win = |r: Rect, label: &str| WinRect { x: r.min.x, y: r.min.y, w: r.width(), h: r.height(), label: label.into() };
    (img, vec![win(code_rect, "Code — checkout.rs"), win(dash_rect, "Acme Analytics — Overview")])
}

#[test]
#[ignore = "writes README screenshots; run with --ignored"]
fn readme_screenshots() {
    let (library, dashboard) = demo_library();
    let code = render_ui(CODE, draw_code_editor);
    write_examples(&dashboard);

    let editor_for = |library: &Path, dashboard: &RgbaImage| {
        let path = library.join("Rega 2026-11-02 at 09.50.00.png");
        Editor::new(path, dashboard.clone(), Some(dashboard_project()), true)
    };

    // 1. Editor with annotations (hero), on the Stamp tool to show its symbols.
    let (lib, mut ed) = (library.clone(), editor_for(&library, &dashboard));
    ed.set_demo_state(egui::Key::K, None);
    let mut h = app_harness(APP_SIZE, move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.mode = Mode::Editor(Box::new(ed));
        app
    });
    settle(&mut h);
    save(&window_chrome(&h.render().unwrap()), "editor.png");

    // 2. Export menu.
    h.get_by_label("Export").click();
    for _ in 0..6 {
        h.step();
    }
    save(&window_chrome(&h.render().unwrap()), "export-menu.png");
    drop(h);

    // 3. Welcome screen + library tray.
    let lib = library.clone();
    let mut h = app_harness(APP_SIZE, move |ctx| RegaApp::demo(ctx, lib));
    settle(&mut h);
    save(&window_chrome(&h.render().unwrap()), "welcome.png");

    // 4. New canvas dialog.
    h.state_mut().show_new_dialog = true;
    for _ in 0..6 {
        h.step();
    }
    save(&window_chrome(&h.render().unwrap()), "new-canvas.png");
    h.state_mut().show_new_dialog = false;

    // 5. Settings (General and Shortcuts tabs), showing the library where it
    // would be on a Mac rather than this run's temporary folder. Marking the
    // library freshly scanned keeps the tray from rescanning that path.
    h.state_mut().settings.save_dir = PathBuf::from("/Users/you/Pictures/Rega");
    h.state_mut().library_scanned = std::time::Instant::now();
    h.state_mut().show_settings = true;
    for _ in 0..6 {
        h.step();
    }
    save(&window_chrome(&h.render().unwrap()), "settings.png");
    h.state_mut().settings_tab = SettingsTab::Shortcuts;
    for _ in 0..6 {
        h.step();
    }
    save(&window_chrome(&h.render().unwrap()), "settings-shortcuts.png");
    drop(h);

    // 6. Capture overlay over a demo desktop, hovering a window.
    let (desk, windows) = desktop(&dashboard, &code);
    let lib = library.clone();
    let mut h = app_harness(Vec2::new(1440.0, 900.0), move |ctx| {
        let snap = Snapshot { image: desk, origin: (0, 0), size: (1440, 900), scale: PPP, windows, cursor: (-1, -1) };
        let mut app = RegaApp::demo(ctx, lib);
        app.activity = Activity::Selecting(Box::new(Overlay::new(ctx, snap, Purpose::Image)));
        app
    });
    // Mid-drag: a selection around the stat cards and chart, with the live size label.
    let (start, end) = (Pos2::new(218.0, 150.0), Pos2::new(712.0, 478.0));
    h.hover_at(start);
    h.step();
    h.event(egui::Event::PointerButton { pos: start, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    for i in 1..=10 {
        h.hover_at(start + (end - start) * (i as f32 / 10.0));
        h.step();
    }
    for _ in 0..4 {
        h.step();
    }
    save(&h.render().unwrap(), "capture-overlay.png");

    let _ = std::fs::remove_dir_all(&library);
    write_guide_shots();
}

/// Renders the permissions popup + banner; writes PNGs only when
/// The new marks (magnifier, stamps, paint-bucket fill) and the tool-specific
/// properties panel. Saved when `TOOLS_PREVIEW_DIR` is set.
#[test]
fn editor_tools_render() {
    use crate::editor::model::Stamp;
    let Ok(dir) = std::env::var("TOOLS_PREVIEW_DIR") else { return };
    let dashboard = render_ui(DASH, draw_dashboard);
    let d = dash_layout();
    let (ba, bb) = px(d.bars[11].expand(30.0));
    let card = d.cards[1];
    let seed = P::new((card.min.x + 8.0) * PPP, (card.max.y - 8.0) * PPP);
    let mut annotations = vec![
        ann(Kind::Fill { seed, tolerance: 12.0, area: px(card) }, [255, 236, 179, 255], 1.0),
        ann(Kind::Magnify { a: P::new(ba.x - 60.0, ba.y - 60.0), b: P::new(ba.x - 60.0 + 360.0, ba.y - 60.0 + 360.0), zoom: 2.0 }, [124, 92, 255, 255], 8.0),
    ];
    let _ = bb;
    for (n, (stamp, color)) in [(Stamp::Check, [46, 184, 92, 255]), (Stamp::Cross, [235, 59, 52, 255]), (Stamp::Warning, [255, 140, 0, 255]), (Stamp::Question, [30, 136, 229, 255]), (Stamp::Star, [255, 214, 0, 255]), (Stamp::Heart, [235, 59, 52, 255])].into_iter().enumerate() {
        annotations.push(ann(Kind::Stamp { pos: P::new(560.0 + n as f32 * 130.0, 1380.0), size: 104.0, stamp }, color, 6.0));
    }
    annotations.push(ann(Kind::Rect { a: P::new(1700.0, 1250.0), b: P::new(2400.0, 1500.0) }, [235, 59, 52, 255], 8.0));
    let project = Project { annotations, effects: Default::default(), next_step: 1 };
    for (name, key, selected) in [("tools-stamp.png", egui::Key::K, None), ("tools-rect-selected.png", egui::Key::V, Some(8)), ("tools-magnify-selected.png", egui::Key::V, Some(1)), ("tools-fill.png", egui::Key::F, None)] {
        let lib = std::env::temp_dir().join(format!("rega-tools-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let mut ed = Editor::new(lib.join("x.png"), dashboard.clone(), Some(project.clone()), true);
        ed.set_demo_state(key, selected);
        let mut h = app_harness(APP_SIZE, move |ctx| {
            let mut app = RegaApp::demo(ctx, lib);
            app.mode = Mode::Editor(Box::new(ed));
            app
        });
        settle(&mut h);
        h.render().unwrap().save(Path::new(&dir).join(name)).unwrap();
        if name == "tools-stamp.png" {
            // The custom color picker opens from the color chip.
            h.get_by_label("Custom color").click();
            settle(&mut h);
            h.render().unwrap().save(Path::new(&dir).join("tools-picker-open.png")).unwrap();
        }
    }
}

/// The two blur styles on the customer emails, and the blur panel.
/// Saved when `TOOLS_PREVIEW_DIR` is set.
#[test]
fn blur_styles_render() {
    use crate::editor::model::BlurStyle;
    let Ok(dir) = std::env::var("TOOLS_PREVIEW_DIR") else { return };
    let dashboard = render_ui(DASH, draw_dashboard);
    let d = dash_layout();
    let annotations: Vec<Annotation> = [BlurStyle::Pixelate, BlurStyle::Gaussian]
        .into_iter()
        .zip(&d.rows)
        .map(|(style, r)| {
            let cell = Rect::from_min_max(Pos2::new(d.table.min.x + 222.0, r.center().y - 13.0), Pos2::new(d.table.min.x + 440.0, r.center().y + 13.0));
            let (a, b) = px(cell);
            ann(Kind::Blur { a, b, style, strength: 50.0 }, [24, 24, 24, 255], 1.0)
        })
        .collect();
    let project = Project { annotations, effects: Default::default(), next_step: 1 };
    for (name, selected) in [("blur-pixelate-selected.png", 0), ("blur-gaussian-selected.png", 1)] {
        let lib = std::env::temp_dir().join(format!("rega-blur-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let mut ed = Editor::new(lib.join("x.png"), dashboard.clone(), Some(project.clone()), true);
        ed.set_demo_state(egui::Key::V, Some(selected));
        let mut h = app_harness(APP_SIZE, move |ctx| {
            let mut app = RegaApp::demo(ctx, lib);
            app.mode = Mode::Editor(Box::new(ed));
            app
        });
        settle(&mut h);
        h.render().unwrap().save(Path::new(&dir).join(name)).unwrap();
    }
}

/// Two selected library items: header shows the count and batch actions.
/// Saved when `SELECTION_PREVIEW_DIR` is set.
#[test]
fn library_selection_renders() {
    let (library, _) = demo_library();
    let picked: Vec<PathBuf> = ["Rega 2026-11-02 at 09.40.00.png", "Rega 2026-11-02 at 09.20.00.png"].iter().map(|n| library.join(n)).collect();
    let mut h = app_harness(Vec2::new(1360.0, 840.0), move |ctx| {
        let mut app = RegaApp::demo(ctx, library);
        app.lib_selection = picked.into_iter().collect();
        app
    });
    for _ in 0..8 {
        h.step();
    }
    h.get_by_label("2 selected");
    if let Ok(d) = std::env::var("SELECTION_PREVIEW_DIR") {
        h.render().unwrap().save(Path::new(&d).join("selection.png")).unwrap();
    }
}

/// The About window shows the name, version and what Rega is built with;
/// saved when `ABOUT_PREVIEW_DIR` is set.
#[test]
fn about_window_renders() {
    let lib = std::env::temp_dir().join(format!("rega-about-{}", std::process::id()));
    std::fs::create_dir_all(&lib).unwrap();
    let mut h = app_harness(Vec2::new(1360.0, 840.0), move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.show_about = true;
        app
    });
    for _ in 0..6 {
        h.step();
    }
    h.get_by_label(&format!("Version {}", env!("CARGO_PKG_VERSION")));
    // Every recorded frame is scaled with it.
    h.get_by_label_contains("pic-scale");
    if let Ok(d) = std::env::var("ABOUT_PREVIEW_DIR") {
        h.render().unwrap().save(Path::new(&d).join("about.png")).unwrap();
    }
}

/// `PERMISSIONS_PREVIEW_DIR` is set.
#[test]
fn permissions_popup_renders() {
    use crate::platform::Permission;
    let dir = std::env::var("PERMISSIONS_PREVIEW_DIR").ok();
    let lib = std::env::temp_dir().join(format!("rega-perm-{}", std::process::id()));
    std::fs::create_dir_all(&lib).unwrap();
    let mut h = app_harness(Vec2::new(1360.0, 840.0), move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.perm_status = vec![(Permission::ScreenRecording, false), (Permission::Accessibility, true)];
        // A poll time in the future keeps the live refresh from replacing the fake status.
        app.perm_polled = Some(std::time::Instant::now() + Duration::from_secs(3600));
        app.show_permissions = true;
        app
    });
    for _ in 0..6 {
        h.step();
    }
    if let Some(d) = &dir {
        h.render().unwrap().save(Path::new(d).join("permissions-popup.png")).unwrap();
    }
    // Skipping closes the popup but the missing-permissions banner stays.
    h.state_mut().show_permissions = false;
    for _ in 0..6 {
        h.step();
    }
    assert!(!h.state().show_permissions);
    assert!(h.state().missing_permissions());
    if let Some(d) = &dir {
        h.render().unwrap().save(Path::new(d).join("permissions-banner.png")).unwrap();
    }
}

/// Renders the capture overlay (hover + drag); writes PNGs only when
/// `OVERLAY_PREVIEW_DIR` is set.
#[test]
fn overlay_renders() {
    let Ok(dir) = std::env::var("OVERLAY_PREVIEW_DIR") else { return };
    let (desk, windows) = desktop(&render_ui(DASH, draw_dashboard), &render_ui(CODE, draw_code_editor));
    let lib = std::env::temp_dir().join(format!("rega-overlay-{}", std::process::id()));
    std::fs::create_dir_all(&lib).unwrap();
    let mut h = app_harness(Vec2::new(1440.0, 900.0), move |ctx| {
        let snap = Snapshot { image: desk, origin: (0, 0), size: (1440, 900), scale: PPP, windows, cursor: (-1, -1) };
        let mut app = RegaApp::demo(ctx, lib);
        app.activity = Activity::Selecting(Box::new(Overlay::new(ctx, snap, Purpose::Image)));
        app
    });
    h.hover_at(Pos2::new(900.0, 520.0));
    for _ in 0..4 {
        h.step();
    }
    h.render().unwrap().save(Path::new(&dir).join("overlay-hover.png")).unwrap();
    let (start, end) = (Pos2::new(218.0, 150.0), Pos2::new(712.0, 478.0));
    h.hover_at(start);
    h.step();
    h.event(egui::Event::PointerButton { pos: start, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    for i in 1..=10 {
        h.hover_at(start + (end - start) * (i as f32 / 10.0));
        h.step();
    }
    h.render().unwrap().save(Path::new(&dir).join("overlay-drag.png")).unwrap();
}

/// Renders the in-app video player; writes a PNG only when `PLAYER_PREVIEW_DIR` is set.
#[test]
fn video_player_renders() {
    let Ok(dir) = std::env::var("PLAYER_PREVIEW_DIR") else { return };
    let lib = std::env::temp_dir().join(format!("rega-player-shot-{}", std::process::id()));
    std::fs::create_dir_all(&lib).unwrap();
    let video = lib.join("Rega 2026-11-03 at 09.00.00.mp4");
    crate::recorder::write_test_video(&video, 40, 10, (960, 600));
    let mut h = app_harness(APP_SIZE, move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.refresh_library();
        app.open_video(video);
        app
    });
    for _ in 0..30 {
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.render().unwrap().save(Path::new(&dir).join("player.png")).unwrap();
    // Trim: in at the playhead now, out a moment later.
    h.key_press(egui::Key::I);
    for _ in 0..25 {
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.key_press(egui::Key::O);
    for _ in 0..4 {
        h.step();
    }
    h.render().unwrap().save(Path::new(&dir).join("player-trim.png")).unwrap();
}

/// A recorded guide open in the editor: steps on the left, the screenshot
/// with its marker, the caption on the right, and the guide in the library
/// with its badge. Writes `guide-editor.png` when `GUIDE_PREVIEW_DIR` is set.
#[test]
fn guide_editor_renders() {
    let (shot, _) = guide_editor_shot();
    if let Ok(d) = std::env::var("GUIDE_PREVIEW_DIR") {
        shot.save(Path::new(&d).join("guide-editor.png")).unwrap();
    }
}

/// The README's guide pictures: `docs/images/guide-editor.png`, and the
/// guide's page at `target/guide-readme.html`, for a browser to screenshot
/// as `docs/images/guide-page.png` (see the Development section).
#[test]
#[ignore = "writes README screenshots; run with --ignored"]
fn readme_guide_screenshots() {
    write_guide_shots();
}

fn write_guide_shots() {
    let (shot, page) = guide_editor_shot();
    save(&window_chrome(&shot), "guide-editor.png");
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/guide-readme.html");
    std::fs::copy(&page, &out).unwrap();
    println!("wrote {}", out.display());
}

/// The demo guide open in the editor, rendered, and where its page was saved.
fn guide_editor_shot() -> (RgbaImage, PathBuf) {
    use crate::guide::editor::GuideEditor;
    let (library, _) = demo_library();
    let path = library.join(DEMO_GUIDE);
    let page = path.clone();
    let mut h = app_harness(APP_SIZE, move |ctx| {
        let mut app = RegaApp::demo(ctx, library);
        app.mode = Mode::Guide(Box::new(GuideEditor::open(ctx, path).unwrap()));
        app
    });
    settle(&mut h);
    for _ in 0..20 {
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.get_by_label("Step 1 of 3");
    assert_eq!(h.state().library.iter().filter(|i| i.kind == crate::library::ItemKind::Guide).count(), 1, "listed in the library");
    (h.render().unwrap(), page)
}

/// Settings › Guide: defaults for new guides and what recording picks up.
/// Writes `guide-settings.png` when `GUIDE_PREVIEW_DIR` is set.
#[test]
fn guide_settings_render() {
    let lib = std::env::temp_dir().join(format!("rega-guide-settings-{}", std::process::id()));
    std::fs::create_dir_all(&lib).unwrap();
    let mut h = app_harness(Vec2::new(1360.0, 900.0), move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.show_settings = true;
        app.settings_tab = SettingsTab::Guide;
        app
    });
    for _ in 0..8 {
        h.step();
    }
    h.get_by_label("Guide");
    if let Ok(d) = std::env::var("GUIDE_PREVIEW_DIR") {
        h.render().unwrap().save(Path::new(&d).join("guide-settings.png")).unwrap();
    }
}
