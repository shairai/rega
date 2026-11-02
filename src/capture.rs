//! Still captures. Coordinates are logical points; images are physical pixels.

use anyhow::{Context, Result, anyhow};
use image::RgbaImage;
use xcap::{Monitor, Window};

/// A window outline in monitor-local logical points, front-most first.
#[derive(Clone, Debug)]
pub struct WinRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
}

/// A frozen picture of one monitor, plus what the overlay needs to select from it.
pub struct Snapshot {
    pub image: RgbaImage,
    /// Monitor's top-left in global logical points.
    pub origin: (i32, i32),
    /// Monitor size in logical points.
    pub size: (u32, u32),
    /// Physical pixels per logical point.
    pub scale: f32,
    pub windows: Vec<WinRect>,
}

impl Snapshot {
    /// Crop a monitor-local logical rect out of the snapshot.
    pub fn crop(&self, x: f32, y: f32, w: f32, h: f32) -> RgbaImage {
        let s = self.scale;
        let px = ((x * s).round().max(0.0) as u32).min(self.image.width().saturating_sub(1));
        let py = ((y * s).round().max(0.0) as u32).min(self.image.height().saturating_sub(1));
        let pw = ((w * s).round() as u32).clamp(1, self.image.width() - px);
        let ph = ((h * s).round() as u32).clamp(1, self.image.height() - py);
        image::imageops::crop_imm(&self.image, px, py, pw, ph).to_image()
    }
}

pub fn cursor_position() -> (i32, i32) {
    use enigo::{Enigo, Mouse, Settings};
    let settings = Settings {
        open_prompt_to_get_permissions: false,
        ..Default::default()
    };
    Enigo::new(&settings)
        .ok()
        .and_then(|e| e.location().ok())
        .unwrap_or((0, 0))
}

pub fn monitor_at_cursor() -> Result<Monitor> {
    let (x, y) = cursor_position();
    Monitor::from_point(x, y)
        .or_else(|_| {
            Monitor::all()?
                .into_iter()
                .find(|m| m.is_primary().unwrap_or(false))
                .ok_or(xcap::XCapError::new("no primary monitor"))
        })
        .context("no monitor found")
}

pub fn snapshot_at_cursor() -> Result<Snapshot> {
    let monitor = monitor_at_cursor()?;
    let origin = (monitor.x()?, monitor.y()?);
    let size = (monitor.width()?, monitor.height()?);
    let image = monitor
        .capture_image()
        .context("screen capture failed — is Screen Recording permission granted?")?;
    if size.0 == 0 {
        return Err(anyhow!("monitor reports zero width"));
    }
    let scale = image.width() as f32 / size.0 as f32;
    let windows = list_windows(origin, size).unwrap_or_default();
    Ok(Snapshot {
        image,
        origin,
        size,
        scale,
        windows,
    })
}

const SYSTEM_APPS: &[&str] = &[
    "Window Server",
    "Dock",
    "Control Center",
    "Notification Center",
    "SystemUIServer",
    "Spotlight",
    "WindowManager",
];

fn list_windows(origin: (i32, i32), size: (u32, u32)) -> Result<Vec<WinRect>> {
    let own_pid = std::process::id();
    let (mx, my) = (origin.0 as f32, origin.1 as f32);
    let (mw, mh) = (size.0 as f32, size.1 as f32);

    let mut out = Vec::new();
    for w in Window::all()? {
        if w.pid().ok() == Some(own_pid) || w.is_minimized().unwrap_or(true) {
            continue;
        }
        let app = w.app_name().unwrap_or_default();
        if SYSTEM_APPS.contains(&app.as_str()) {
            continue;
        }
        let (Ok(x), Ok(y), Ok(ww), Ok(wh)) = (w.x(), w.y(), w.width(), w.height()) else {
            continue;
        };
        if ww < 40 || wh < 40 {
            continue;
        }
        // Clip to this monitor and convert to monitor-local coordinates.
        let x0 = (x as f32 - mx).max(0.0);
        let y0 = (y as f32 - my).max(0.0);
        let x1 = (x as f32 + ww as f32 - mx).min(mw);
        let y1 = (y as f32 + wh as f32 - my).min(mh);
        if x1 - x0 < 20.0 || y1 - y0 < 20.0 {
            continue;
        }
        let title = w.title().unwrap_or_default();
        let label = if title.is_empty() {
            app
        } else {
            format!("{app} — {title}")
        };
        out.push(WinRect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
            label,
        });
    }
    Ok(out)
}
