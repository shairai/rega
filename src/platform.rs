//! Small OS integrations. Everything platform-specific outside of capture/OCR lives here.

use std::path::Path;
use std::process::Command;

pub fn open_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("cmd").args(["/C", "start", ""]).arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = result {
        log::warn!("failed to open {}: {e}", path.display());
    }
}

pub fn reveal_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    if let Err(e) = result {
        log::warn!("failed to reveal {}: {e}", path.display());
    }
}

/// Raise (or restore) the main window above the menu bar and dock, so the
/// selection overlay and recording bar cover everything.
#[cfg(target_os = "macos")]
fn ns_window(frame: &eframe::Frame) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = frame.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    // SAFETY: eframe hands us a valid NSView for the root window, and `ui`/`logic`
    // run on the main thread.
    let view: &NSView = unsafe { appkit.ns_view.cast().as_ref() };
    view.window()
}

/// winit only gets mouse-move events while its window is key, so hover
/// effects die whenever another app is focused. A tracking area with
/// `ActiveAlways` keeps them flowing (winit's view already handles
/// `mouseMoved:`).
#[cfg(target_os = "macos")]
pub fn enable_hover_when_inactive(frame: &eframe::Frame) {
    use objc2::AnyThread;
    use objc2_app_kit::{NSTrackingArea, NSTrackingAreaOptions, NSView};
    use objc2_foundation::NSRect;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = frame.window_handle() else { return };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return };
    // SAFETY: valid NSView from eframe, used on the main thread.
    let view: &NSView = unsafe { appkit.ns_view.cast().as_ref() };
    let options = NSTrackingAreaOptions::MouseMoved | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect;
    // SAFETY: the view owns the tracking area and outlives it; the rect is
    // ignored because of `InVisibleRect`.
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), NSRect::ZERO, options, Some(view), None)
    };
    view.addTrackingArea(&area);
}

#[cfg(not(target_os = "macos"))]
pub fn enable_hover_when_inactive(_frame: &eframe::Frame) {}

#[cfg(target_os = "macos")]
pub fn set_overlay_level(frame: &eframe::Frame, overlay: bool) {
    if let Some(window) = ns_window(frame) {
        // NSScreenSaverWindowLevel = 1000, NSNormalWindowLevel = 0
        window.setLevel(if overlay { 1000 } else { 0 });
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_overlay_level(_frame: &eframe::Frame, _overlay: bool) {}

/// Place the window exactly over a rect given in global top-left logical
/// points. Goes straight to AppKit so macOS can't nudge it off the Dock or
/// menu bar area.
#[cfg(target_os = "macos")]
pub fn set_window_frame(frame: &eframe::Frame, x: f32, y: f32, w: f32, h: f32) {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

    let (Some(window), Some(mtm)) = (ns_window(frame), MainThreadMarker::new()) else {
        return;
    };
    // Cocoa's origin is the bottom-left of the primary screen.
    let Some(primary) = NSScreen::screens(mtm).firstObject() else {
        return;
    };
    let primary_h = primary.frame().size.height;
    let rect = NSRect::new(
        NSPoint::new(x as f64, primary_h - (y + h) as f64),
        NSSize::new(w as f64, h as f64),
    );
    window.setFrame_display(rect, true);
}

#[cfg(not(target_os = "macos"))]
pub fn set_window_frame(_frame: &eframe::Frame, _x: f32, _y: f32, _w: f32, _h: f32) {}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Without Screen Recording permission macOS still "captures", but only the
/// wallpaper — so check before capturing. The answer is cached per process:
/// after the user grants access, Snapquill must restart.
pub fn has_screen_permission() -> bool {
    #[cfg(target_os = "macos")]
    // SAFETY: plain CoreGraphics query with no arguments.
    return unsafe { CGPreflightScreenCaptureAccess() };
    #[cfg(not(target_os = "macos"))]
    true
}

/// Shows the system prompt (only the first time macOS sees this app).
pub fn request_screen_permission() {
    #[cfg(target_os = "macos")]
    // SAFETY: plain CoreGraphics call with no arguments.
    unsafe {
        CGRequestScreenCaptureAccess();
    }
}

pub fn open_screen_recording_settings() {
    #[cfg(target_os = "macos")]
    {
        // macOS 13+ address first, then the classic one.
        for url in [
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_ScreenCapture",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        ] {
            if Command::new("open").arg(url).status().is_ok_and(|s| s.success()) {
                return;
            }
        }
    }
}

/// Relaunch Snapquill (needed for a newly granted permission to apply).
pub fn relaunch() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        // Inside Snapquill.app/Contents/MacOS/ → reopen the bundle so macOS
        // attributes permissions to it.
        let bundle = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf);
        let spawned = match bundle {
            Some(app) if cfg!(target_os = "macos") => Command::new("open").arg("-n").arg(app).spawn(),
            _ => Command::new(exe).spawn(),
        };
        if let Err(e) = spawned {
            log::warn!("relaunch failed: {e}");
        }
    }
    std::process::exit(0);
}

/// Register (or remove) Snapquill to start when the user logs in. Per-user
/// only: no admin rights needed, and it never touches system-wide settings.
pub fn set_launch_at_login(enabled: bool) {
    if let Err(e) = try_set_launch_at_login(enabled) {
        log::warn!("failed to {} launch at login: {e}", if enabled { "enable" } else { "disable" });
    }
}

#[cfg(target_os = "macos")]
fn try_set_launch_at_login(enabled: bool) -> anyhow::Result<()> {
    let plist = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?
        .join("Library/LaunchAgents/app.snapquill.Snapquill.plist");
    if !enabled {
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    // Launch the .app bundle (not the bare binary) so macOS attributes the
    // Screen Recording permission to it.
    let args = match exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")) {
        Some(app) => format!("<string>/usr/bin/open</string><string>-a</string><string>{}</string>", xml_escape(&app.display().to_string())),
        None => format!("<string>{}</string>", xml_escape(&exe.display().to_string())),
    };
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>app.snapquill.Snapquill</string>
  <key>ProgramArguments</key><array>{args}</array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#
    );
    if std::fs::read_to_string(&plist).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    std::fs::create_dir_all(plist.parent().expect("has parent"))?;
    std::fs::write(&plist, contents)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "windows")]
fn try_set_launch_at_login(enabled: bool) -> anyhow::Result<()> {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut cmd = Command::new("reg");
    if enabled {
        let exe = std::env::current_exe()?;
        cmd.args(["add", KEY, "/v", "Snapquill", "/t", "REG_SZ", "/f", "/d"]).arg(format!("\"{}\"", exe.display()));
    } else {
        cmd.args(["delete", KEY, "/v", "Snapquill", "/f"]);
    }
    // Deleting a value that was never set exits non-zero; that's fine.
    let status = cmd.status()?;
    anyhow::ensure!(status.success() || !enabled, "reg exited with {status}");
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn try_set_launch_at_login(enabled: bool) -> anyhow::Result<()> {
    let entry = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("no config directory"))?
        .join("autostart/snapquill.desktop");
    if !enabled {
        if entry.exists() {
            std::fs::remove_file(&entry)?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(entry.parent().expect("has parent"))?;
    std::fs::write(
        &entry,
        format!("[Desktop Entry]\nType=Application\nName=Snapquill\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\n", exe.display()),
    )?;
    Ok(())
}

/// On notched MacBooks, new status items land on the far left of the status
/// area and get hidden behind the notch. On first launch, ask macOS to place
/// ours next to Control Center instead; a later ⌘-drag by the user wins.
#[cfg(target_os = "macos")]
pub fn prefer_status_item_on_right(autosave_name: &str) {
    use objc2_foundation::{NSString, NSUserDefaults};
    let defaults = NSUserDefaults::standardUserDefaults();
    let key = NSString::from_str(&format!("NSStatusItem Preferred Position {autosave_name}"));
    if defaults.objectForKey(&key).is_none() {
        // Distance in points from the right edge of the screen.
        defaults.setDouble_forKey(120.0, &key);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn prefer_status_item_on_right(_autosave_name: &str) {}

pub fn copy_image(img: &image::RgbaImage) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_image(arboard::ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: std::borrow::Cow::Borrowed(img.as_raw()),
    })?;
    Ok(())
}

/// An image from the clipboard: copied pixels, or an image file copied in
/// Finder / Explorer.
pub fn clipboard_image() -> Option<image::RgbaImage> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    if let Ok(img) = clipboard.get_image() {
        return image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned());
    }
    let files = clipboard.get().file_list().ok()?;
    files.iter().find_map(|f| image::open(f).ok()).map(|img| img.to_rgba8())
}

/// egui hands over pasted images as `ColorImage`s.
pub fn color_image_to_rgba(img: &eframe::egui::ColorImage) -> image::RgbaImage {
    let [w, h] = img.size;
    let bytes = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    image::RgbaImage::from_raw(w as u32, h as u32, bytes).unwrap_or_default()
}

pub fn copy_text(text: &str) -> anyhow::Result<()> {
    arboard::Clipboard::new()?.set_text(text.to_owned())?;
    Ok(())
}
