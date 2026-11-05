//! Installer artwork (DMG background, NSIS bitmaps) and the multi-size Windows
//! icon, drawn from the same logo code as the app so there are no design files
//! to keep in sync. Used by the packaging scripts via `--write-branding`.

use std::path::Path;

use anyhow::Result;
use image::RgbaImage;
use tiny_skia::{Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect, SpreadMode, Stroke, Transform};

use crate::editor::render::{draw_text, measure_text};
use crate::icon::{LOGO_FROM, LOGO_TO, app_icon, draw_mark};

/// DMG window content size in points; the packaging script uses the same numbers.
const DMG_W: u32 = 660;
const DMG_H: u32 = 400;
/// Icon centers in the DMG window (must match `scripts/package-macos.sh`).
const DMG_APP_X: f32 = 180.0;
const DMG_LINK_X: f32 = 480.0;
const DMG_ICON_Y: f32 = 205.0;

const INK: [u8; 4] = [26, 28, 34, 255];
const INK_MUTED: [u8; 4] = [107, 112, 128, 255];

/// Write every branding file into `dir`.
pub fn write_all(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    dmg_background(1).save(dir.join("dmg-background.png"))?;
    dmg_background(2).save(dir.join("dmg-background@2x.png"))?;
    // NSIS needs 24-bit BMPs.
    image::DynamicImage::ImageRgba8(installer_welcome()).to_rgb8().save(dir.join("installer-welcome.bmp"))?;
    image::DynamicImage::ImageRgba8(installer_header()).to_rgb8().save(dir.join("installer-header.bmp"))?;
    Ok(())
}

/// A Windows .ico with every size Explorer and the taskbar ask for, so small
/// views get a sharp icon instead of a downscaled 256 px one.
pub fn write_ico(path: &Path) -> Result<()> {
    use image::codecs::ico::{IcoEncoder, IcoFrame};
    let frames = [16, 20, 24, 32, 40, 48, 64, 128, 256]
        .into_iter()
        .map(|s| {
            let (rgba, w, h) = app_icon(s);
            IcoFrame::as_png(&rgba, w, h, image::ExtendedColorType::Rgba8)
        })
        .collect::<Result<Vec<_>, _>>()?;
    IcoEncoder::new(std::fs::File::create(path)?).encode_images(&frames)?;
    Ok(())
}

fn logo_gradient(from: (f32, f32), to: (f32, f32)) -> Paint<'static> {
    Paint {
        anti_alias: true,
        shader: LinearGradient::new(
            Point::from_xy(from.0, from.1),
            Point::from_xy(to.0, to.1),
            vec![
                GradientStop::new(0.0, Color::from_rgba8(LOGO_FROM[0], LOGO_FROM[1], LOGO_FROM[2], 255)),
                GradientStop::new(1.0, Color::from_rgba8(LOGO_TO[0], LOGO_TO[1], LOGO_TO[2], 255)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .expect("gradient"),
        ..Paint::default()
    }
}

fn fill(pm: &mut Pixmap, paint: &Paint) {
    let rect = Rect::from_xywh(0.0, 0.0, pm.width() as f32, pm.height() as f32).expect("rect");
    pm.fill_rect(rect, paint, Transform::identity(), None);
}

fn text_centered(pm: &mut Pixmap, cx: f32, y: f32, text: &str, size: f32, color: [u8; 4]) {
    let (w, _) = measure_text(text, size);
    draw_text(pm, crate::editor::model::P::new(cx - w / 2.0, y), text, size, color);
}

/// Opaque pixmap → image (premultiplied and straight alpha agree at alpha 255).
fn into_image(pm: Pixmap) -> RgbaImage {
    let (w, h) = (pm.width(), pm.height());
    RgbaImage::from_raw(w, h, pm.take()).expect("pixmap dims")
}

fn overlay_icon(img: &mut RgbaImage, size: u32, x: f32, y: f32) {
    let (rgba, w, h) = app_icon(size);
    let icon = RgbaImage::from_raw(w, h, rgba).expect("icon dims");
    image::imageops::overlay(img, &icon, x.round() as i64, y.round() as i64);
}

/// Light background (Finder draws dark icon labels on it), title, hint and a
/// gradient arrow from the app icon to the Applications link.
fn dmg_background(k: u32) -> RgbaImage {
    let s = k as f32;
    let mut pm = Pixmap::new(DMG_W * k, DMG_H * k).expect("dmg size");
    let bg = Paint {
        shader: LinearGradient::new(
            Point::from_xy(0.0, 0.0),
            Point::from_xy(0.0, DMG_H as f32 * s),
            vec![
                GradientStop::new(0.0, Color::from_rgba8(252, 251, 255, 255)),
                GradientStop::new(1.0, Color::from_rgba8(240, 236, 252, 255)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .expect("gradient"),
        ..Paint::default()
    };
    fill(&mut pm, &bg);

    // Arrow between the two icons (icons are 128 pt wide, centered on their x).
    let (x0, x1, y) = ((DMG_APP_X + 84.0) * s, (DMG_LINK_X - 84.0) * s, DMG_ICON_Y * s);
    let paint = logo_gradient((x0, y), (x1, y));
    let head = 16.0 * s;
    let mut shaft = PathBuilder::new();
    shaft.move_to(x0, y);
    shaft.line_to(x1 - head * 0.6, y);
    if let Some(path) = shaft.finish() {
        let stroke = Stroke { width: 6.0 * s, line_cap: tiny_skia::LineCap::Round, ..Default::default() };
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    let mut tip = PathBuilder::new();
    tip.move_to(x1, y);
    tip.line_to(x1 - head * 1.3, y - head);
    tip.line_to(x1 - head * 1.3, y + head);
    tip.close();
    if let Some(path) = tip.finish() {
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    // Title row: logo + "Rega", centered together.
    let (title_size, logo) = (30.0 * s, 40.0 * s);
    let (tw, _) = measure_text("Rega", title_size);
    let gap = 12.0 * s;
    let left = (DMG_W as f32 * s - (logo + gap + tw)) / 2.0;
    let title_y = 44.0 * s;
    draw_text(&mut pm, crate::editor::model::P::new(left + logo + gap, title_y + 2.0 * s), "Rega", title_size, INK);
    text_centered(&mut pm, DMG_W as f32 * s / 2.0, 100.0 * s, "Drag Rega into Applications to install", 15.0 * s, INK_MUTED);

    let mut img = into_image(pm);
    overlay_icon(&mut img, logo as u32, left, title_y);
    img
}

/// NSIS welcome/finish side panel (164×314): logo gradient, white mark, name.
fn installer_welcome() -> RgbaImage {
    let (w, h) = (164u32, 314u32);
    let mut pm = Pixmap::new(w, h).expect("size");
    fill(&mut pm, &logo_gradient((0.0, 0.0), (w as f32, h as f32)));
    let mark = 84.0;
    let mut m = Pixmap::new(mark as u32, mark as u32).expect("mark");
    draw_mark(&mut m, mark, mark * 0.12, (255, 255, 255, 255));
    let x = ((w as f32 - mark) / 2.0) as i32;
    pm.draw_pixmap(x, 78, m.as_ref(), &tiny_skia::PixmapPaint::default(), Transform::identity(), None);
    let cx = w as f32 / 2.0;
    text_centered(&mut pm, cx, 182.0, "Rega", 30.0, [255, 255, 255, 255]);
    text_centered(&mut pm, cx, 222.0, "Capture · Mark up · Share", 10.5, [255, 255, 255, 215]);
    into_image(pm)
}

/// NSIS page header image (150×57, shown at the right): the app icon on white.
fn installer_header() -> RgbaImage {
    let (w, h) = (150u32, 57u32);
    let mut img = RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]));
    let size = 44;
    overlay_icon(&mut img, size, (w - size - 8) as f32, ((h - size) / 2) as f32);
    img
}
