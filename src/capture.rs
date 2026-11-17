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
    /// Global cursor position at the moment the capture shortcut was pressed.
    pub cursor: (i32, i32),
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

/// Global cursor position in logical points, top-left origin. `None` if it
/// can't be read.
#[cfg(target_os = "macos")]
pub fn try_cursor_position() -> Option<(i32, i32)> {
    // Plain CoreGraphics: unlike enigo, this needs no Accessibility permission
    // (enigo refuses to start without it, which used to yield (0, 0) here).
    use std::ffi::c_void;
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }
    // SAFETY: CGEventCreate(NULL) returns an owned event (or null); we only read
    // its location and release it exactly once.
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return None;
        }
        let p = CGEventGetLocation(event);
        CFRelease(event);
        Some((p.x as i32, p.y as i32))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn try_cursor_position() -> Option<(i32, i32)> {
    use enigo::{Enigo, Mouse, Settings};
    thread_local! {
        static ENIGO: std::cell::RefCell<Option<Enigo>> = const { std::cell::RefCell::new(None) };
    }
    ENIGO.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Enigo::new(&Settings { open_prompt_to_get_permissions: false, ..Default::default() }).ok();
        }
        slot.as_ref()?.location().ok()
    })
}

/// Capture one pixel and throw it away. On recent macOS the first real capture
/// triggers the system's "directly access your screen" confirmation; doing it
/// right after setup means that prompt doesn't interrupt the user's first capture.
pub fn probe_screen_access() {
    if let Some(monitor) = Monitor::all().ok().and_then(|m| m.into_iter().next()) {
        let _ = monitor.capture_region(0, 0, 1, 1);
    }
}

/// Device pixels per logical point. macOS already reports logical points; on
/// Windows xcap reports physical pixels, so everything Rega draws and stores
/// (overlay, regions, window rects) is divided by this to stay in points,
/// which is what egui's window commands expect.
pub fn dpi(monitor: &Monitor) -> f32 {
    if cfg!(target_os = "windows") {
        monitor.scale_factor().ok().filter(|s| *s >= 1.0).unwrap_or(1.0)
    } else {
        1.0
    }
}

/// A monitor's origin in logical points.
fn logical_origin(monitor: &Monitor) -> Option<(i32, i32)> {
    let d = dpi(monitor);
    Some(((monitor.x().ok()? as f32 / d).round() as i32, (monitor.y().ok()? as f32 / d).round() as i32))
}

/// The monitor whose logical origin is `origin` (as stored in a `Snapshot`).
pub fn monitor_with_origin(origin: (i32, i32)) -> Option<Monitor> {
    Monitor::all().ok()?.into_iter().find(|m| logical_origin(m) == Some(origin))
}

/// Convert a global physical-pixel point (e.g. the cursor) to logical points.
pub fn to_logical(point: (i32, i32)) -> (i32, i32) {
    if !cfg!(target_os = "windows") {
        return point;
    }
    let d = Monitor::from_point(point.0, point.1).map(|m| dpi(&m)).unwrap_or(1.0);
    ((point.0 as f32 / d).round() as i32, (point.1 as f32 / d).round() as i32)
}

pub fn cursor_position() -> (i32, i32) {
    try_cursor_position().unwrap_or((0, 0))
}

fn monitor_at(cursor: (i32, i32)) -> Result<Monitor> {
    Monitor::from_point(cursor.0, cursor.1)
        .or_else(|_| {
            Monitor::all()?
                .into_iter()
                .find(|m| m.is_primary().unwrap_or(false))
                .ok_or(xcap::XCapError::new("no primary monitor"))
        })
        .context("no monitor found")
}

/// One screen, in global logical points.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub origin: (i32, i32),
    pub size: (u32, u32),
    /// What the system calls it ("Built-in Retina Display", "DELL U2720Q").
    pub name: String,
}

impl Screen {
    pub fn contains(&self, p: (i32, i32)) -> bool {
        let (x, y) = (p.0 - self.origin.0, p.1 - self.origin.1);
        x >= 0 && y >= 0 && (x as u32) < self.size.0 && (y as u32) < self.size.1
    }
}

/// Every screen, left to right (then top to bottom).
pub fn screens() -> Vec<Screen> {
    let mut out: Vec<Screen> = Monitor::all()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            let d = dpi(m);
            let size = ((m.width().ok()? as f32 / d).round() as u32, (m.height().ok()? as f32 / d).round() as u32);
            let name = m.friendly_name().ok().filter(|n| !n.trim().is_empty()).or_else(|| m.name().ok()).unwrap_or_default();
            (size.0 > 0 && size.1 > 0).then_some(Screen { origin: logical_origin(m)?, size, name })
        })
        .collect();
    out.sort_by_key(|s| s.origin);
    out
}

/// Freeze each of `screens` except the one at `except`, one after another on
/// a background thread; each picture arrives on the channel once it's taken.
pub fn snapshot_others(screens: &[Screen], except: (i32, i32), cursor: (i32, i32)) -> std::sync::mpsc::Receiver<Snapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    let origins: Vec<(i32, i32)> = screens.iter().map(|s| s.origin).filter(|&o| o != except).collect();
    std::thread::spawn(move || {
        for origin in origins {
            let snap = monitor_with_origin(origin).context("the screen is gone").and_then(|m| snapshot_of(&m, cursor));
            match snap {
                Ok(snap) => {
                    if tx.send(snap).is_err() {
                        break; // the capture is over
                    }
                }
                Err(e) => log::warn!("couldn't freeze the screen at {origin:?}: {e:#}"),
            }
        }
    });
    rx
}

/// Freeze the monitor under `cursor` (read when the shortcut was pressed, not
/// now, so a slow capture can't make the marker lag behind the mouse).
pub fn snapshot_at(cursor: (i32, i32)) -> Result<Snapshot> {
    snapshot_of(&monitor_at(cursor)?, cursor)
}

fn snapshot_of(monitor: &Monitor, cursor: (i32, i32)) -> Result<Snapshot> {
    let d = dpi(monitor);
    let phys_origin = (monitor.x()?, monitor.y()?);
    let origin = ((phys_origin.0 as f32 / d).round() as i32, (phys_origin.1 as f32 / d).round() as i32);
    let size = ((monitor.width()? as f32 / d).round() as u32, (monitor.height()? as f32 / d).round() as u32);
    let image = monitor
        .capture_image()
        .context("screen capture failed — is Screen Recording permission granted?")?;
    // A zero dimension would make every later crop/clamp panic.
    if size.0 == 0 || size.1 == 0 || image.width() == 0 || image.height() == 0 {
        return Err(anyhow!("monitor reports a zero size"));
    }
    let scale = image.width() as f32 / size.0 as f32;
    let windows = list_windows(phys_origin, (monitor.width()?, monitor.height()?), d).unwrap_or_default();
    let cursor = to_logical(cursor);
    Ok(Snapshot {
        image,
        origin,
        size,
        scale,
        windows,
        cursor,
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

/// `origin` and `size` are in xcap units; the result is in logical points (`d` = DPI scale).
fn list_windows(origin: (i32, i32), size: (u32, u32), d: f32) -> Result<Vec<WinRect>> {
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
            x: x0 / d,
            y: y0 / d,
            w: (x1 - x0) / d,
            h: (y1 - y0) / d,
            label,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn coregraphics_cursor_matches_enigo() {
        use enigo::{Enigo, Mouse, Settings};
        let ours = super::try_cursor_position().expect("cursor");
        let theirs = Enigo::new(&Settings { open_prompt_to_get_permissions: false, ..Default::default() })
            .unwrap()
            .location()
            .unwrap();
        eprintln!("CURSOR ours={ours:?} enigo={theirs:?}");
        assert!((ours.0 - theirs.0).abs() <= 3 && (ours.1 - theirs.1).abs() <= 3);
    }
}
