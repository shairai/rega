//! The website's demo video, rendered off-screen from the real UI like the
//! README screenshots: press ⌃⌥C, drag over a dashboard, mark it up with
//! one-key tools, copy it, and paste it into a chat.
//!
//! ```sh
//! cargo test --release demo_video -- --ignored
//! ```
//! Needs `ffmpeg`. Writes `site/media/demo.mp4` and its poster
//! `site/media/demo.jpg`, and sets the times of the steps under the video in
//! `site/index.html`.
//!
//! The overlay and the editor are the real app, driven by pointer and key
//! events; the pointer, click rings and key badges are drawn on top. Nothing
//! reaches the clipboard: copying is shown, not done.

use std::io::Write as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Instant;

use super::*;

const FPS: f32 = 30.0;
const SCREEN: Vec2 = Vec2::new(1440.0, 900.0);
/// The 2× render scaled down: sharp on the site, a few MB for the whole video.
const OUT: (u32, u32) = (1920, 1200);
/// The dashboard window on the demo desktop (see `desktop`).
const DASH_WIN: Rect = Rect { min: Pos2::new(70.0, 70.0), max: Pos2::new(950.0, 620.0) };
/// The region dragged on the desktop: the dashboard's cards, chart and table.
const SELECTION: Rect = Rect { min: Pos2::new(228.0, 150.0), max: Pos2::new(938.0, 612.0) };
/// Rega's window on the desktop: its content's top-left (below the title bar) and size.
const WIN_AT: Pos2 = Pos2::new(70.0, 68.0);
const WIN_SIZE: Vec2 = Vec2::new(1300.0, 800.0);
/// The chat window the capture is pasted into.
const CHAT_AT: Pos2 = Pos2::new(600.0, 168.0);
const CHAT_SIZE: Vec2 = Vec2::new(620.0, 620.0);
/// Title bar and shadow margin added by `window_chrome`, in points.
const BAR: f32 = 28.0;
const MARGIN: f32 = 36.0;

#[test]
#[ignore = "writes the website's demo video; run with --ignored (needs ffmpeg)"]
fn demo_video() {
    let site = Path::new(env!("CARGO_MANIFEST_DIR")).join("site");
    let media = site.join("media");
    std::fs::create_dir_all(&media).unwrap();

    let (library, dashboard) = demo_library();
    let code = render_ui(CODE, draw_code_editor);
    // The code window off to the side, clear of the region that gets captured.
    let (desk, windows) = desktop_with_code_at(&dashboard, &code, Pos2::new(985.0, 300.0));
    let mut d = Director::new(&media.join("demo.mp4"), Stage::Still(desk.clone()), Pos2::new(780.0, 560.0));

    // 1. Anywhere on the desktop: ⌃⌥C.
    d.mark(1);
    d.wait(0.5);
    d.glide(Pos2::new(470.0, 330.0), 0.7);
    d.key(None, "⌃⌥C");
    d.wait(0.35);

    // 2. The frozen screen: drag over the cards, chart and table.
    d.mark(2);
    let snap = Snapshot { image: desk.clone(), origin: (0, 0), size: (1440, 900), scale: PPP, windows, cursor: (470, 330) };
    let lib = library.clone();
    d.cut(Stage::Screen(live(SCREEN, move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.activity = Activity::Selecting(Box::new(Overlay::new(ctx, snap, Purpose::Image)));
        app
    })));
    d.crosshair = true;
    d.glide(SELECTION.min, 0.55);
    d.wait(0.15);
    d.drag(SELECTION.max, 1.5);
    d.wait(0.5);

    // 3. The capture opens in the editor: arrow, text, blur, steps, a stamp.
    d.mark(3);
    let (px0, px1) = ((SELECTION.min.to_vec2() * PPP).round(), (SELECTION.size() * PPP).round());
    let shot = image::imageops::crop_imm(&desk, px0.x as u32, px0.y as u32, px1.x as u32, px1.y as u32).to_image();
    let mut ed = Editor::new(library.join("Rega 2026-11-16 at 10.42.00.png"), shot, None, false);
    ed.save().unwrap();
    let lib = library.clone();
    let mut harness = live(WIN_SIZE, move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.mode = Mode::Editor(Box::new(ed));
        app
    });
    settle(&mut harness);
    d.crosshair = false;
    d.fade(Stage::window(harness, &desk), 0.3);
    d.toast("Captured and copied");
    d.wait(0.6);

    let dash = dash_layout();
    // A point on the dashboard (its own layout) to the capture's pixels.
    let k = DASH_WIN.width() / DASH.x;
    let at = |q: Pos2| P::new((DASH_WIN.min.x + q.x * k - SELECTION.min.x) * PPP, (DASH_WIN.min.y + q.y * k - SELECTION.min.y) * PPP);
    let chart = dash.chart;

    // Arrow from the chart to the revenue card.
    d.key(Some(egui::Key::A), "A");
    let tail = at(Pos2::new(chart.min.x + chart.width() * 0.34, chart.min.y + 74.0));
    d.glide_img(tail, 0.45);
    d.drag_img(at(Pos2::new(dash.cards[0].center().x + 34.0, dash.cards[0].max.y + 6.0)), 0.55);
    d.wait(0.2);

    // A callout next to it.
    d.key(Some(egui::Key::T), "T");
    d.glide_img(at(Pos2::new(chart.min.x + chart.width() * 0.36, chart.min.y + 40.0)), 0.35);
    d.click();
    d.wait(0.1);
    d.typ("Revenue up 12%", 2);
    d.wait(0.25);
    d.key(Some(egui::Key::Escape), "");
    d.wait(0.2);

    // Blur the email addresses.
    d.key(Some(egui::Key::B), "B");
    d.glide_img(at(dash.email_col.min), 0.5);
    d.drag_img(at(dash.email_col.max), 0.6);
    d.wait(0.25);

    // Number three cards.
    d.key(Some(egui::Key::N), "N");
    for card in &dash.cards[1..] {
        d.glide_img(at(Pos2::new(card.max.x - 20.0, card.min.y + 20.0)), 0.3);
        d.click();
        d.wait(0.08);
    }

    // A check mark on the best month.
    d.key(Some(egui::Key::K), "K");
    let bar = dash.bars[11];
    d.glide_img(at(Pos2::new(bar.center().x, bar.min.y - 16.0)), 0.4);
    d.click();
    d.wait(0.3);
    d.key(Some(egui::Key::Escape), "");
    d.wait(0.2);

    // 4. Copy, then paste it into a chat.
    d.mark(4);
    d.key(None, "⌘C");
    d.toast("Image copied to clipboard");
    d.wait(1.1);
    let poster = d.video.last.clone();
    let flat = match &d.stage {
        Stage::Window { h, .. } => match &h.state().mode {
            Mode::Editor(ed) => ed.flatten(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    d.toast_off();
    let (before, after) = (chat_on(&desk, &flat, false), chat_on(&desk, &flat, true));
    d.fade(Stage::Still(before), 0.3);
    d.glide(CHAT_AT + Vec2::new(250.0, CHAT_SIZE.y - 36.0), 0.5);
    d.click();
    d.wait(0.2);
    d.key(None, "⌘V");
    d.fade(Stage::Still(after), 0.2);
    d.wait(2.4);

    let marks = d.marks.clone();
    d.video.finish();
    let poster = image::imageops::resize(&poster, OUT.0, OUT.1, image::imageops::FilterType::Lanczos3);
    image::DynamicImage::ImageRgba8(poster).to_rgb8().save(media.join("demo.jpg")).unwrap();
    set_step_times(&site.join("index.html"), &marks);
    println!("wrote {} ({:.1} s), steps at {marks:?}", media.join("demo.mp4").display(), d.video.frames as f32 / FPS);
    let _ = std::fs::remove_dir_all(&library);
}

/// The app in a harness that steps at the video's frame rate.
fn live(size: Vec2, make: impl FnOnce(&egui::Context) -> RegaApp + 'static) -> Harness<'static, RegaApp> {
    Harness::builder()
        .with_size(size)
        .with_pixels_per_point(PPP)
        .with_step_dt(1.0 / FPS)
        .with_max_steps(400)
        .with_accessibility_check(false)
        .with_render_cursor(false)
        .wgpu()
        .build_eframe(move |cc| make(&cc.egui_ctx))
}

/// What's on screen.
enum Stage {
    Still(RgbaImage),
    /// The app full screen: the capture overlay.
    Screen(Harness<'static, RegaApp>),
    /// The app in a window on the desktop. `base` is the desktop with the
    /// window's frame and shadow; `alpha` is the content's rounded corners.
    Window { h: Harness<'static, RegaApp>, base: RgbaImage, alpha: Vec<u8> },
}

impl Stage {
    fn window(h: Harness<'static, RegaApp>, desk: &RgbaImage) -> Self {
        let (w, hh) = ((WIN_SIZE.x * PPP) as u32, (WIN_SIZE.y * PPP) as u32);
        let chrome = window_chrome(&RgbaImage::from_pixel(w, hh, image::Rgba([14, 16, 20, 255])));
        let (m, bar) = ((MARGIN * PPP) as u32, (BAR * PPP) as u32);
        let alpha = (0..hh).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| chrome.get_pixel(x + m, y + m + bar)[3]).collect();
        let mut base = desk.clone();
        blend_straight(&mut base, &chrome, ((WIN_AT.x - MARGIN) * PPP) as i64, ((WIN_AT.y - BAR - MARGIN) * PPP) as i64, 1.0);
        Stage::Window { h, base, alpha }
    }

    /// Screen points to the harness's own.
    fn local(&self, p: Pos2) -> Pos2 {
        match self {
            Stage::Window { .. } => p - WIN_AT.to_vec2(),
            _ => p,
        }
    }

    fn harness(&mut self) -> Option<&mut Harness<'static, RegaApp>> {
        match self {
            Stage::Still(_) => None,
            Stage::Screen(h) | Stage::Window { h, .. } => Some(h),
        }
    }

    fn frame(&mut self) -> RgbaImage {
        match self {
            Stage::Still(img) => img.clone(),
            Stage::Screen(h) => {
                h.step();
                h.render().unwrap()
            }
            Stage::Window { h, base, alpha } => {
                h.step();
                let content = h.render().unwrap();
                let mut out = base.clone();
                let (ox, oy) = ((WIN_AT.x * PPP) as u32, (WIN_AT.y * PPP) as u32);
                for (i, (x, y)) in (0..content.height()).flat_map(|y| (0..content.width()).map(move |x| (x, y))).enumerate() {
                    let a = alpha[i] as f32 / 255.0;
                    if a <= 0.0 {
                        continue;
                    }
                    let (src, dst) = (content.get_pixel(x, y), out.get_pixel_mut(x + ox, y + oy));
                    for c in 0..3 {
                        dst[c] = (src[c] as f32 * a + dst[c] as f32 * (1.0 - a)).round() as u8;
                    }
                }
                out
            }
        }
    }
}

/// Moves the pointer, presses keys and records the frames.
struct Director {
    video: Video,
    stage: Stage,
    pointer: Pos2,
    crosshair: bool,
    clicks: Vec<(Pos2, f32)>,
    badge: Option<(RgbaImage, f32)>,
    toast: Option<(String, f32)>,
    marks: Vec<(u32, f32)>,
}

impl Director {
    fn new(path: &Path, stage: Stage, pointer: Pos2) -> Self {
        Self { video: Video::start(path), stage, pointer, crosshair: false, clicks: Vec::new(), badge: None, toast: None, marks: Vec::new() }
    }

    fn t(&self) -> f32 {
        self.video.frames as f32 / FPS
    }

    fn mark(&mut self, step: u32) {
        self.marks.push((step, self.t()));
    }

    /// Render the stage, decorate it and add it to the video.
    fn frame(&mut self) {
        let t = self.t();
        if let (Some((msg, since)), Some(h)) = (&self.toast, self.stage.harness()) {
            h.state_mut().toast = Some((msg.clone(), Instant::now() - Duration::from_secs_f32(t - since)));
        }
        let base = self.stage.frame();
        self.push(base);
    }

    fn push(&mut self, mut img: RgbaImage) {
        let t = self.t();
        self.video.last_base = img.clone();
        self.clicks.retain(|(_, at)| t - at < 0.5);
        for (p, at) in &self.clicks {
            ring(&mut img, *p, (t - at) / 0.5);
        }
        if self.crosshair {
            crosshair(&mut img, self.pointer);
        } else {
            arrow(&mut img, self.pointer);
        }
        if let Some((badge, at)) = &self.badge {
            let age = t - at;
            let a = (age / 0.1).min(1.0).min((0.95 - age) / 0.2).max(0.0);
            let x = ((SCREEN.x * PPP) as i64 - badge.width() as i64) / 2;
            let y = ((SCREEN.y - 150.0) * PPP) as i64 - badge.height() as i64 / 2;
            blend_premultiplied(&mut img, badge, x, y, a);
        }
        self.video.push(img);
    }

    fn wait(&mut self, secs: f32) {
        for _ in 0..(secs * FPS).round() as u32 {
            self.frame();
        }
    }

    /// Move the pointer to `to` (screen points), easing in and out.
    fn glide(&mut self, to: Pos2, secs: f32) {
        let from = self.pointer;
        let n = ((secs * FPS).round() as u32).max(1);
        for i in 1..=n {
            let k = i as f32 / n as f32;
            let k = k * k * (3.0 - 2.0 * k);
            self.pointer = from + (to - from) * k;
            let local = self.stage.local(self.pointer);
            if let Some(h) = self.stage.harness() {
                h.hover_at(local);
            }
            self.frame();
        }
    }

    /// Where an image pixel is on screen in the editor.
    fn screen_of(&self, p: P) -> Pos2 {
        match &self.stage {
            Stage::Window { h, .. } => match &h.state().mode {
                Mode::Editor(ed) => ed.image_to_screen(p) + WIN_AT.to_vec2(),
                _ => unreachable!("not in the editor"),
            },
            _ => unreachable!("not in a window"),
        }
    }

    fn glide_img(&mut self, to: P, secs: f32) {
        let to = self.screen_of(to);
        self.glide(to, secs);
    }

    fn button(&mut self, pressed: bool) {
        let pos = self.stage.local(self.pointer);
        if pressed {
            self.clicks.push((self.pointer, self.t()));
        }
        if let Some(h) = self.stage.harness() {
            h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
        }
    }

    fn click(&mut self) {
        self.button(true);
        self.frame();
        self.button(false);
        self.frame();
    }

    fn drag(&mut self, to: Pos2, secs: f32) {
        self.button(true);
        self.frame();
        self.glide(to, secs);
        self.button(false);
        self.frame();
    }

    fn drag_img(&mut self, to: P, secs: f32) {
        let to = self.screen_of(to);
        self.drag(to, secs);
    }

    /// Press `key` in the app (if any) and show `label` as a key badge.
    fn key(&mut self, key: Option<egui::Key>, label: &str) {
        if let (Some(key), Some(h)) = (key, self.stage.harness()) {
            h.key_press(key);
        }
        if !label.is_empty() {
            self.badge = Some((key_badge(label), self.t()));
        }
        self.frame();
    }

    fn typ(&mut self, text: &str, frames_per_char: u32) {
        for ch in text.chars() {
            if let Some(h) = self.stage.harness() {
                h.event(egui::Event::Text(ch.to_string()));
            }
            for _ in 0..frames_per_char {
                self.frame();
            }
        }
    }

    fn toast(&mut self, msg: &str) {
        self.toast = Some((msg.into(), self.t()));
    }

    fn toast_off(&mut self) {
        self.toast = None;
        if let Some(h) = self.stage.harness() {
            h.state_mut().toast = None;
        }
    }

    /// Switch to `stage` at once.
    fn cut(&mut self, stage: Stage) {
        self.stage = stage;
        let local = self.stage.local(self.pointer);
        if let Some(h) = self.stage.harness() {
            h.hover_at(local);
        }
    }

    /// Cross-fade from the last frame to `stage`.
    fn fade(&mut self, stage: Stage, secs: f32) {
        let from = self.video.last_base.clone();
        self.cut(stage);
        let n = ((secs * FPS).round() as u32).max(1);
        for i in 1..=n {
            let to = self.stage.frame();
            let k = i as f32 / n as f32;
            let mixed = RgbaImage::from_fn(to.width(), to.height(), |x, y| {
                let (a, b) = (from.get_pixel(x, y), to.get_pixel(x, y));
                image::Rgba([0, 1, 2, 3].map(|c| (a[c] as f32 * (1.0 - k) + b[c] as f32 * k).round() as u8))
            });
            self.push(mixed);
        }
    }
}

/// Raw frames piped to ffmpeg.
struct Video {
    ffmpeg: Child,
    stdin: Option<ChildStdin>,
    frames: u32,
    /// The last frame as sent, and before the pointer and badges were drawn.
    last: RgbaImage,
    last_base: RgbaImage,
}

impl Video {
    fn start(path: &Path) -> Self {
        let size = format!("{}x{}", (SCREEN.x * PPP) as u32, (SCREEN.y * PPP) as u32);
        let mut ffmpeg = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &size, "-r", &FPS.to_string(), "-i", "-"])
            .args(["-vf", &format!("scale={}:{}:flags=lanczos,format=yuv420p", OUT.0, OUT.1)])
            .args(["-c:v", "libx264", "-preset", "slow", "-crf", "24", "-tune", "animation", "-movflags", "+faststart", "-an"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("ffmpeg must be installed to render the demo video");
        let stdin = ffmpeg.stdin.take();
        Self { ffmpeg, stdin, frames: 0, last: RgbaImage::new(1, 1), last_base: RgbaImage::new(1, 1) }
    }

    fn push(&mut self, img: RgbaImage) {
        self.stdin.as_mut().unwrap().write_all(img.as_raw()).unwrap();
        self.frames += 1;
        self.last = img;
    }

    fn finish(&mut self) {
        drop(self.stdin.take());
        assert!(self.ffmpeg.wait().unwrap().success(), "ffmpeg failed");
    }
}

// ---------- drawing on frames ----------

fn pixmap(img: &mut RgbaImage) -> tiny_skia::PixmapMut<'_> {
    let (w, h) = img.dimensions();
    tiny_skia::PixmapMut::from_bytes(img.as_mut(), w, h).unwrap()
}

fn paint(r: u8, g: u8, b: u8, a: u8) -> tiny_skia::Paint<'static> {
    let mut p = tiny_skia::Paint::default();
    p.set_color_rgba8(r, g, b, a);
    p.anti_alias = true;
    p
}

/// The macOS arrow pointer with its tip at `at`.
fn arrow(img: &mut RgbaImage, at: Pos2) {
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(0.0, 0.0);
    for (x, y) in [(0.0, 17.0), (4.2, 13.2), (7.0, 19.6), (9.6, 18.5), (6.9, 12.2), (12.4, 12.2)] {
        pb.line_to(x, y);
    }
    pb.close();
    let path = pb.finish().unwrap();
    let place = |dx: f32, dy: f32| tiny_skia::Transform::from_row(PPP, 0.0, 0.0, PPP, (at.x + dx) * PPP, (at.y + dy) * PPP);
    let mut pm = pixmap(img);
    let stroke = tiny_skia::Stroke { width: 1.6, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
    pm.fill_path(&path, &paint(0, 0, 0, 60), tiny_skia::FillRule::Winding, place(0.6, 1.4), None);
    pm.stroke_path(&path, &paint(255, 255, 255, 255), &stroke, place(0.0, 0.0), None);
    pm.fill_path(&path, &paint(18, 18, 20, 255), tiny_skia::FillRule::Winding, place(0.0, 0.0), None);
}

/// The selection crosshair centred on `at`.
fn crosshair(img: &mut RgbaImage, at: Pos2) {
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(-10.0, 0.0);
    pb.line_to(10.0, 0.0);
    pb.move_to(0.0, -10.0);
    pb.line_to(0.0, 10.0);
    let path = pb.finish().unwrap();
    let place = tiny_skia::Transform::from_row(PPP, 0.0, 0.0, PPP, at.x * PPP, at.y * PPP);
    let mut pm = pixmap(img);
    let line = |w| tiny_skia::Stroke { width: w, line_cap: tiny_skia::LineCap::Round, ..Default::default() };
    pm.stroke_path(&path, &paint(255, 255, 255, 255), &line(3.4), place, None);
    pm.stroke_path(&path, &paint(18, 18, 20, 255), &line(1.4), place, None);
}

/// A ring spreading out from a click; `k` runs 0 → 1.
fn ring(img: &mut RgbaImage, at: Pos2, k: f32) {
    let Some(path) = tiny_skia::PathBuilder::from_circle(at.x * PPP, at.y * PPP, (8.0 + 20.0 * k) * PPP) else { return };
    let mut pm = pixmap(img);
    let stroke = tiny_skia::Stroke { width: 3.0 * PPP * (1.0 - k * 0.6), ..Default::default() };
    pm.stroke_path(&path, &paint(124, 92, 255, (210.0 * (1.0 - k)) as u8), &stroke, tiny_skia::Transform::identity(), None);
}

/// A key cap like "⌃⌥C", on a transparent background.
fn key_badge(keys: &str) -> RgbaImage {
    let keys = keys.to_owned();
    let size = Vec2::new(360.0, 120.0);
    let mut h = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(PPP)
        .with_render_cursor(false)
        .with_context_setup(|ctx| {
            theme::install(ctx);
            ctx.all_styles_mut(|s| s.visuals.panel_fill = Color32::TRANSPARENT);
        })
        .wgpu()
        .build_ui(move |ui| {
            let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("badge")));
            let galley = p.layout_no_wrap(keys.clone(), theme::bold(40.0), Color32::WHITE);
            let pill = Rect::from_center_size(Pos2::new(size.x / 2.0, size.y / 2.0), galley.size() + Vec2::new(48.0, 28.0));
            p.add(egui::epaint::RectShape::filled(pill.translate(Vec2::new(0.0, 6.0)), 20.0, Color32::from_black_alpha(90)).with_blur_width(18.0));
            p.rect_filled(pill, 20.0, Color32::from_rgba_unmultiplied(28, 31, 40, 240));
            p.rect_stroke(pill, 20.0, Stroke::new(1.5, Color32::from_white_alpha(46)), StrokeKind::Inside);
            p.galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
        });
    h.step();
    h.render().unwrap()
}

/// `src` over `dst` at (x, y) in pixels, with straight alpha, faded by `fade`.
fn blend_straight(dst: &mut RgbaImage, src: &RgbaImage, x: i64, y: i64, fade: f32) {
    blend(dst, src, x, y, |s, d, a| s * a + d * (1.0 - a), fade);
}

/// The same with premultiplied `src`, as rendered on a transparent background.
fn blend_premultiplied(dst: &mut RgbaImage, src: &RgbaImage, x: i64, y: i64, fade: f32) {
    blend(dst, src, x, y, |s, d, a| s + d * (1.0 - a), fade);
}

fn blend(dst: &mut RgbaImage, src: &RgbaImage, x: i64, y: i64, mix: impl Fn(f32, f32, f32) -> f32, fade: f32) {
    if fade <= 0.0 {
        return;
    }
    for (sx, sy, s) in src.enumerate_pixels() {
        let (dx, dy) = (x + sx as i64, y + sy as i64);
        if dx < 0 || dy < 0 || dx >= dst.width() as i64 || dy >= dst.height() as i64 || s[3] == 0 {
            continue;
        }
        let a = s[3] as f32 / 255.0 * fade;
        let d = dst.get_pixel_mut(dx as u32, dy as u32);
        for c in 0..3 {
            d[c] = mix(s[c] as f32 * fade, d[c] as f32, a).round().clamp(0.0, 255.0) as u8;
        }
    }
}

// ---------- the chat the capture is pasted into ----------

/// The desktop with a team chat open; `pasted`: with the capture posted.
fn chat_on(desk: &RgbaImage, capture: &RgbaImage, pasted: bool) -> RgbaImage {
    let thumb = image::imageops::resize(capture, 880, (880.0 * capture.height() as f32 / capture.width() as f32) as u32, image::imageops::FilterType::Lanczos3);
    let thumb = egui::ColorImage::from_rgba_unmultiplied([thumb.width() as usize, thumb.height() as usize], thumb.as_raw());
    let mut texture: Option<egui::TextureHandle> = None;
    let content = render_ui(CHAT_SIZE, move |ui| {
        let tex = texture.get_or_insert_with(|| ui.ctx().load_texture("capture", thumb.clone(), egui::TextureOptions::LINEAR));
        let p = ui.painter();
        let (ink, muted, line) = (Color32::from_rgb(29, 28, 36), Color32::from_rgb(110, 112, 124), Color32::from_rgb(228, 229, 235));
        let full = Rect::from_min_size(Pos2::ZERO, CHAT_SIZE);
        p.rect_filled(full, 0.0, Color32::WHITE);
        text(p, Pos2::new(24.0, 16.0), "# design-review", theme::bold(17.0), ink);
        text(p, Pos2::new(24.0, 40.0), "Dana, Omer and you", theme::medium(12.5), muted);
        p.hline(0.0..=CHAT_SIZE.x, 66.0, Stroke::new(1.0, line));
        let message = |y: f32, who: &str, color: Color32, time: &str, body: &str| {
            let avatar = Rect::from_min_size(Pos2::new(24.0, y), Vec2::splat(38.0));
            p.rect_filled(avatar, 9.0, color);
            p.text(avatar.center(), Align2::CENTER_CENTER, &who[..1], theme::bold(17.0), Color32::WHITE);
            let name = text(p, Pos2::new(76.0, y - 1.0), who, theme::bold(15.0), ink);
            text(p, Pos2::new(name.max.x + 8.0, y + 1.0), time, theme::medium(12.0), muted);
            text(p, Pos2::new(76.0, y + 21.0), body, theme::medium(14.5), ink);
        };
        message(88.0, "Dana", Color32::from_rgb(52, 168, 113), "10:38", "Did revenue really jump this month?");
        message(150.0, "Omer", Color32::from_rgb(10, 132, 255), "10:39", "Someone grab the dashboard?");
        if pasted {
            message(212.0, "You", Color32::from_rgb(255, 94, 122), "10:42", "Yes, December. Here:");
            let w = 440.0;
            let r = Rect::from_min_size(Pos2::new(76.0, 260.0), Vec2::new(w, w * tex.size()[1] as f32 / tex.size()[0] as f32));
            p.add(egui::epaint::RectShape::filled(r, 8.0, Color32::WHITE).with_texture(tex.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
            p.rect_stroke(r, 8.0, Stroke::new(1.0, line), StrokeKind::Outside);
        }
        let composer = Rect::from_min_max(Pos2::new(20.0, CHAT_SIZE.y - 62.0), Pos2::new(CHAT_SIZE.x - 20.0, CHAT_SIZE.y - 14.0));
        p.rect_filled(composer, 10.0, Color32::WHITE);
        p.rect_stroke(composer, 10.0, Stroke::new(1.0, Color32::from_rgb(200, 202, 212)), StrokeKind::Inside);
        text(p, composer.left_center() + Vec2::new(16.0, -9.0), "Message #design-review", theme::medium(14.5), muted);
    });
    let mut out = desk.clone();
    blend_straight(&mut out, &window_chrome(&content), ((CHAT_AT.x - MARGIN) * PPP) as i64, ((CHAT_AT.y - BAR - MARGIN) * PPP) as i64, 1.0);
    out
}

/// Set `data-t` on the step list under the video to when each step starts.
fn set_step_times(page: &Path, marks: &[(u32, f32)]) {
    let mut html = std::fs::read_to_string(page).unwrap();
    for (step, t) in marks {
        let key = format!("data-step=\"{step}\" data-t=\"");
        let start = html.find(&key).unwrap_or_else(|| panic!("no {key} in {}", page.display())) + key.len();
        let end = start + html[start..].find('"').unwrap();
        html.replace_range(start..end, &format!("{t:.2}"));
    }
    std::fs::write(page, html).unwrap();
}

// ---------- the demo guide ----------

/// The website's demo guide: a real guide page, exported the way Rega exports
/// one, that also teaches the basics. Each step's screenshot is the real UI.
///
/// ```sh
/// cargo test --release site_guide -- --ignored
/// ```
/// Writes `site/guide-demo.html`, which the home page shows in a frame.
#[test]
#[ignore = "writes the website's demo guide; run with --ignored"]
fn site_guide() {
    use crate::guide::style::PageTheme;
    use crate::guide::{Action, Button, Guide, Shot, Step};

    let (library, dashboard) = demo_library();
    let code = render_ui(CODE, draw_code_editor);
    let click = Action::Click { button: Button::Left, count: 1 };
    // A spot in points on a screenshot of `size` points, as the page wants it.
    let spot = |p: Pos2, size: Vec2| Some([p.x / size.x, p.y / size.y]);
    let step = |img: RgbaImage, action: Action, marker: Option<[f32; 2]>, title: &str, note: &str| Step {
        shot: Shot::encode(&crate::guide::fit_capture(img, Some(1920))).unwrap(),
        action,
        marker,
        marker_hidden: false,
        title: title.into(),
        note: note.into(),
    };
    let mut steps = Vec::new();

    // 1. The home screen: the Region tile.
    let lib = library.clone();
    let mut h = app_harness(APP_SIZE, move |ctx| RegaApp::demo(ctx, lib));
    settle(&mut h);
    let region = h.get_by_label("Region").rect().center();
    steps.push(step(
        h.render().unwrap(),
        click.clone(),
        spot(region, APP_SIZE),
        "Click Region, or press ⌃⌥C in any app",
        "Rega waits in the menu bar, so the shortcut works wherever you are.",
    ));
    drop(h);

    // 2. The frozen screen, mid-drag over the dashboard.
    let (desk, windows) = desktop_with_code_at(&dashboard, &code, Pos2::new(985.0, 300.0));
    let lib = library.clone();
    let mut h = app_harness(SCREEN, move |ctx| {
        let snap = Snapshot { image: desk, origin: (0, 0), size: (1440, 900), scale: PPP, windows, cursor: (-1, -1) };
        let mut app = RegaApp::demo(ctx, lib);
        app.activity = Activity::Selecting(Box::new(Overlay::new(ctx, snap, Purpose::Image)));
        app
    });
    let (start, end) = (SELECTION.min, SELECTION.max);
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
    steps.push(step(
        h.render().unwrap(),
        click.clone(),
        spot(end, SCREEN),
        "Drag over what you want",
        "Or click a window to capture just that window. Esc cancels.",
    ));
    drop(h);

    // 3. The editor with marks; 4. copied; 5. the Export menu.
    let ed = Editor::new(library.join("Rega 2026-11-02 at 09.50.00.png"), dashboard.clone(), Some(dashboard_project()), true);
    let lib = library.clone();
    let mut h = app_harness(APP_SIZE, move |ctx| {
        let mut app = RegaApp::demo(ctx, lib);
        app.mode = Mode::Editor(Box::new(ed));
        app
    });
    settle(&mut h);
    let arrow = dashboard_project().annotations.iter().find_map(|a| match a.kind {
        Kind::Arrow { b, .. } => Some(b),
        _ => None,
    });
    let arrow = match &h.state().mode {
        Mode::Editor(ed) => ed.image_to_screen(arrow.expect("the demo project has an arrow")),
        _ => unreachable!(),
    };
    steps.push(step(
        h.render().unwrap(),
        click.clone(),
        spot(arrow, APP_SIZE),
        "Mark it up",
        "Arrows, text, blur, steps and stamps, one key each. Every mark stays editable.",
    ));

    // Shown fully faded in: toasts fade in over their first 0.15 s.
    h.state_mut().toast = Some(("Image copied to clipboard".into(), Instant::now() - Duration::from_secs(1)));
    for _ in 0..3 {
        h.step();
    }
    let copy = h.get_by_label("Copy").rect().center();
    steps.push(step(
        h.render().unwrap(),
        Action::Key { combo: "⌘C".into() },
        spot(copy, APP_SIZE),
        "Press ⌘C and paste it anywhere",
        "Slack, an email, a ticket: it's already on your clipboard.",
    ));
    h.state_mut().toast = None;

    h.get_by_label("Export").click();
    for _ in 0..6 {
        h.step();
    }
    let export = h.get_by_label("Export").rect().center();
    steps.push(step(
        h.render().unwrap(),
        click,
        spot(export, APP_SIZE),
        "Or choose Export for a file",
        "PNG, JPEG, PDF, WebP, TIFF or BMP. Every capture is also kept in the library at the bottom.",
    ));
    drop(h);

    let mut guide = Guide::new("Capture, mark up and share a screenshot".into(), steps);
    guide.style.theme = PageTheme::Dark;
    guide.style.repeat = true;
    guide.style.seconds = 5.0;
    let html = crate::guide::export::page(&guide, &crate::guide::export::Cache::default()).unwrap();
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("site/guide-demo.html");
    std::fs::write(&out, &html).unwrap();
    println!("wrote {} ({} KB, {} steps)", out.display(), html.len() / 1024, guide.steps.len());
    let _ = std::fs::remove_dir_all(&library);
}
