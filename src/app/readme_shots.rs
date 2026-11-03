//! Renders the README screenshots off-screen, with made-up demo content.
//!
//! ```sh
//! cargo test --release readme_screenshots -- --ignored
//! ```
//! Images are written to `docs/images/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use image::RgbaImage;

use super::{Activity, RegaApp, Mode, SettingsTab};
use crate::capture::{Snapshot, WinRect};
use crate::editor::Editor;
use crate::editor::model::{Annotation, Kind, P, Project};
use crate::overlay::{Overlay, Purpose};
use crate::theme;

const PPP: f32 = 2.0;
const APP_SIZE: Vec2 = Vec2::new(1360.0, 900.0);

// ---------- demo content ----------

/// Render arbitrary painting to an image at Retina resolution.
fn render_ui(size: Vec2, mut draw: impl FnMut(&mut egui::Ui)) -> RgbaImage {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(PPP)
        .with_context_setup(|ctx| theme::install(ctx))
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
        ann(Kind::Blur { a: ea, b: eb }, red, 8.0),
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
    Project { annotations, effects: Default::default(), next_step: 4 }
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
        let images = app.library.iter().filter(|i| i.kind == crate::library::ItemKind::Image).count();
        if app.thumbs.len() >= images {
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
    let dir = std::env::temp_dir().join(format!("rega-demo-{}", std::process::id()));
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
    (dir, dashboard)
}

fn desktop(dashboard: &RgbaImage, code: &RgbaImage) -> (RgbaImage, Vec<WinRect>) {
    let size = Vec2::new(1440.0, 900.0);
    let dash_rect = Rect::from_min_size(Pos2::new(70.0, 70.0), Vec2::new(880.0, 550.0));
    let code_rect = Rect::from_min_size(Pos2::new(640.0, 330.0), Vec2::new(740.0, 471.0));
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

    let editor_for = |library: &Path, dashboard: &RgbaImage| {
        let path = library.join("Rega 2026-11-02 at 09.50.00.png");
        Editor::new(path, dashboard.clone(), Some(dashboard_project()), true)
    };

    // 1. Editor with annotations (hero).
    let (lib, ed) = (library.clone(), editor_for(&library, &dashboard));
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

    // 5. Settings (General and Shortcuts tabs).
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
}

/// Renders the permissions popup + banner; writes PNGs only when
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
