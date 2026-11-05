//! Small OS integrations. Everything platform-specific outside of capture/OCR lives here.

use std::path::Path;
use std::process::Command;

pub fn open_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("cmd").args(["/C", "start", ""]).arg(path).spawn();
    if let Err(e) = result {
        log::warn!("failed to open {}: {e}", path.display());
    }
}

pub fn reveal_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = {
        use std::os::windows::process::CommandExt;
        // raw_arg: explorer wants `/select,"C:\a b\c.png"`, not a fully quoted argument.
        Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn()
    };
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
    if let Some(window) = view.window() {
        overlay_panel::leave(&window);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn enable_hover_when_inactive(_frame: &eframe::Frame) {}

/// Dock icon on (regular app) or off (menu-bar "accessory" app).
///
/// macOS only lets an accessory app's windows appear over another app's
/// full-screen Space, so Rega launches as one (see `main.rs`) and is a
/// regular app only while the main window is on screen.
#[cfg(target_os = "macos")]
pub fn set_dock_icon(visible: bool) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy as P};
    use objc2_foundation::MainThreadMarker;
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let policy = if visible { P::Regular } else { P::Accessory };
    if app.activationPolicy() != policy {
        app.setActivationPolicy(policy);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_icon(_visible: bool) {}

/// Make Rega the active app. A window `Focus` alone doesn't pull a menu-bar
/// app in front of the app the user was working in.
#[cfg(target_os = "macos")]
pub fn activate_app() {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;
    let Some(mtm) = MainThreadMarker::new() else { return };
    // `activate` needs macOS 14; Rega supports 12.3+.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

#[cfg(not(target_os = "macos"))]
pub fn activate_app() {}

/// The capture overlay and recording bar have to appear over whatever the user
/// is looking at, including another app's full-screen Space. macOS only allows
/// that for a non-activating panel, so for the duration of a capture the window
/// is turned into one (winit's window class only overrides the two methods
/// below, and NSPanel adds no instance storage, so swapping is safe), and turned
/// back afterwards.
#[cfg(target_os = "macos")]
mod overlay_panel {
    use std::sync::atomic::{AtomicPtr, Ordering};

    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{ClassType, MainThreadOnly, define_class};
    use objc2_app_kit::{NSPanel, NSResponder, NSWindow, NSWindowCollectionBehavior as B, NSWindowStyleMask};
    use objc2_foundation::NSObject;

    define_class!(
        // SAFETY: NSPanel has no subclassing requirements; no Drop impl.
        #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "RegaOverlayPanel"]
        struct OverlayPanel;

        impl OverlayPanel {
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key_window(&self) -> bool {
                true
            }

            #[unsafe(method(canBecomeMainWindow))]
            fn can_become_main_window(&self) -> bool {
                true
            }
        }
    );

    /// winit's window class, restored when the capture ends.
    static ORIGINAL_CLASS: AtomicPtr<AnyClass> = AtomicPtr::new(std::ptr::null_mut());

    fn set_class(window: &NSWindow, class: &AnyClass) -> *const AnyClass {
        let obj = window as *const NSWindow as *mut AnyObject;
        // SAFETY: both classes are NSWindow subclasses with the same instance
        // size and no ivars of their own, so the object stays valid.
        unsafe { objc2::ffi::object_setClass(obj, class) }
    }

    pub fn enter(window: &NSWindow) {
        let panel = OverlayPanel::class();
        if !std::ptr::eq(window.class(), panel) {
            let previous = set_class(window, panel);
            ORIGINAL_CLASS.store(previous as *mut AnyClass, Ordering::Relaxed);
        }
        window.setStyleMask(window.styleMask() | NSWindowStyleMask::NonactivatingPanel);
        window.setHidesOnDeactivate(false);
        window.setCollectionBehavior(B::CanJoinAllSpaces | B::FullScreenAuxiliary);
        // NSScreenSaverWindowLevel: above the menu bar and Dock.
        window.setLevel(1000);
    }

    pub fn leave(window: &NSWindow) {
        window.setStyleMask(window.styleMask() & !NSWindowStyleMask::NonactivatingPanel);
        let original = ORIGINAL_CLASS.load(Ordering::Relaxed);
        if !original.is_null() && std::ptr::eq(window.class(), OverlayPanel::class()) {
            // SAFETY: `original` came from `object_setClass` and is a live class.
            set_class(window, unsafe { &*original });
        }
        // Normal window: shown on whichever Space is active when ordered front.
        window.setCollectionBehavior(B::MoveToActiveSpace | B::FullScreenAuxiliary);
        window.setLevel(0);
    }
}

#[cfg(target_os = "macos")]
pub fn set_overlay_level(_ctx: &eframe::egui::Context, frame: &eframe::Frame, overlay: bool) {
    if let Some(window) = ns_window(frame) {
        if overlay {
            overlay_panel::enter(&window);
        } else {
            overlay_panel::leave(&window);
        }
    }
}

/// Show the overlay on the Space the user is on and give it the keyboard,
/// without activating Rega (activation would pull the user to another Space).
/// It starts fully transparent: until egui paints the frozen screenshot into it,
/// the window still holds its previous frame (the library UI), which would flash.
#[cfg(target_os = "macos")]
pub fn present_overlay(frame: &eframe::Frame) {
    if let Some(window) = ns_window(frame) {
        overlay_panel::enter(&window);
        window.setAlphaValue(0.0);
        window.orderFrontRegardless();
        window.makeKeyWindow();
    }
}

/// Window opacity, used to hide the window while it changes role (overlay ⇄
/// normal) so its stale previous frame never flashes on screen.
#[cfg(target_os = "macos")]
pub fn set_window_alpha(frame: &eframe::Frame, alpha: f64) {
    if let Some(window) = ns_window(frame) {
        window.setAlphaValue(alpha);
    }
}

/// Windows / Linux: a borderless always-on-top window covers the taskbar and panels.
#[cfg(not(target_os = "macos"))]
pub fn set_overlay_level(ctx: &eframe::egui::Context, _frame: &eframe::Frame, overlay: bool) {
    use eframe::egui::{ViewportCommand, WindowLevel};
    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if overlay { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal }));
}

#[cfg(not(target_os = "macos"))]
pub fn present_overlay(_frame: &eframe::Frame) {}

#[cfg(not(target_os = "macos"))]
pub fn set_window_alpha(_frame: &eframe::Frame, _alpha: f64) {}

/// Put the window behind every other app's windows (shown, but not in the way).
#[cfg(target_os = "macos")]
pub fn send_window_to_back(frame: &eframe::Frame) {
    if let Some(window) = ns_window(frame) {
        window.orderBack(None);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn send_window_to_back(_frame: &eframe::Frame) {}

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

/// A border drawn just outside the region being recorded, so the user can see
/// what is captured. Four thin click-through panels: outside the region (and
/// excluded from screen capture), on every Space including full-screen apps.
/// `rect` is (x, y, w, h) in global top-left logical points.
#[cfg(target_os = "macos")]
pub fn show_recording_frame(rect: (f32, f32, f32, f32)) {
    use objc2::MainThreadOnly;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSBackingStoreType, NSColor, NSPanel, NSScreen, NSWindowCollectionBehavior as B, NSWindowSharingType, NSWindowStyleMask,
    };
    use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

    hide_recording_frame();
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(primary) = NSScreen::screens(mtm).firstObject() else { return };
    let primary_h = primary.frame().size.height;
    const T: f32 = 3.0;
    let (x, y, w, h) = rect;
    let (from, to) = (crate::icon::LOGO_FROM, crate::icon::LOGO_TO);
    // Top and left in the logo's coral, bottom and right in its violet.
    let strips = [
        ((x - T, y - T, w + 2.0 * T, T), from),
        ((x - T, y, T, h), from),
        ((x - T, y + h, w + 2.0 * T, T), to),
        ((x + w, y, T, h), to),
    ];
    let panels: Vec<Retained<NSPanel>> = strips
        .into_iter()
        .map(|((sx, sy, sw, sh), c)| {
            let frame = NSRect::new(NSPoint::new(sx as f64, primary_h - (sy + sh) as f64), NSSize::new(sw as f64, sh as f64));
            let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                frame,
                NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                NSBackingStoreType::Buffered,
                false,
            );
            // SAFETY: we keep our own reference and never `close` it.
            unsafe { panel.setReleasedWhenClosed(false) };
            let color = |v: u8| v as f64 / 255.0;
            panel.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(color(c[0]), color(c[1]), color(c[2]), 1.0)));
            panel.setIgnoresMouseEvents(true);
            panel.setHidesOnDeactivate(false);
            panel.setSharingType(NSWindowSharingType::None);
            panel.setCollectionBehavior(B::CanJoinAllSpaces | B::FullScreenAuxiliary | B::Stationary);
            panel.setLevel(1000);
            panel.orderFrontRegardless();
            panel
        })
        .collect();
    RECORDING_FRAME.with(|f| *f.borrow_mut() = panels);
}

#[cfg(target_os = "macos")]
thread_local! {
    static RECORDING_FRAME: std::cell::RefCell<Vec<objc2::rc::Retained<objc2_app_kit::NSPanel>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(target_os = "macos")]
pub fn hide_recording_frame() {
    RECORDING_FRAME.with(|f| {
        for panel in f.borrow_mut().drain(..) {
            panel.orderOut(None);
        }
    });
}

/// Windows: the same border, drawn by four borderless click-through egui
/// viewports (see `paint_recording_frame`).
#[cfg(not(target_os = "macos"))]
pub fn show_recording_frame(rect: (f32, f32, f32, f32)) {
    *RECORDING_RECT.lock().unwrap() = Some(rect);
    // New windows, which have yet to be kept out of captures.
    FRAME_PROTECTED.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(target_os = "macos"))]
pub fn hide_recording_frame() {
    *RECORDING_RECT.lock().unwrap() = None;
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
static RECORDING_RECT: std::sync::Mutex<Option<(f32, f32, f32, f32)>> = std::sync::Mutex::new(None);

/// Which of the border's viewports were told to keep out of captures (bit
/// `i` for strip `i`): told once each, as every viewport command also
/// repaints its viewport, so telling it on every paint redraws it nonstop.
#[cfg_attr(target_os = "macos", allow(dead_code))]
static FRAME_PROTECTED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Call every frame: keeps the recording border's windows alive while a
/// recording runs (a viewport closes when it stops being shown). They are
/// excluded from capture, so they never appear in the video.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn paint_recording_frame(ctx: &eframe::egui::Context) {
    use eframe::egui::{self, Color32, Pos2, Vec2, ViewportBuilder, ViewportId};
    use std::sync::atomic::Ordering;
    let Some((x, y, w, h)) = *RECORDING_RECT.lock().unwrap() else { return };
    const T: f32 = 3.0;
    let (from, to) = (crate::icon::LOGO_FROM, crate::icon::LOGO_TO);
    let strips = [
        ((x - T, y - T, w + 2.0 * T, T), from),
        ((x - T, y, T, h), from),
        ((x - T, y + h, w + 2.0 * T, T), to),
        ((x + w, y, T, h), to),
    ];
    for (i, ((sx, sy, sw, sh), c)) in strips.into_iter().enumerate() {
        let color = Color32::from_rgb(c[0], c[1], c[2]);
        ctx.show_viewport_deferred(
            ViewportId::from_hash_of(("recording-frame", i)),
            ViewportBuilder::default()
                .with_title("Rega recording frame")
                .with_decorations(false)
                .with_resizable(false)
                .with_taskbar(false)
                .with_active(false)
                .with_always_on_top()
                .with_mouse_passthrough(true)
                .with_position(Pos2::new(sx, sy))
                .with_inner_size(Vec2::new(sw, sh)),
            move |ctx, _| {
                if FRAME_PROTECTED.fetch_or(1 << i, Ordering::Relaxed) & (1 << i) == 0 {
                    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::ContentProtected(true));
                }
                egui::CentralPanel::default().frame(egui::Frame::new().fill(color)).show(ctx, |_| {});
            },
        );
    }
}

/// Run the calling thread as user-interactive work, which macOS keeps on the
/// performance cores with its timers tight: the capture thread has frame
/// deadlines to keep.
#[cfg(target_os = "macos")]
pub fn make_thread_interactive() {
    // QOS_CLASS_USER_INTERACTIVE, from <sys/qos.h>.
    const USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: std::ffi::c_int) -> std::ffi::c_int;
    }
    // SAFETY: a plain call that changes only the calling thread's QoS class.
    let status = unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
    if status != 0 {
        log::debug!("couldn't make the thread interactive (error {status})");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn make_thread_interactive() {}

/// While alive, Windows runs Rega at full speed even when it judges it to be
/// background work (EcoQoS: efficiency cores, low clocks), which it may well
/// while Rega's only window is the small recording bar.
pub struct FullSpeed(());

pub fn full_speed() -> FullSpeed {
    #[cfg(windows)]
    set_speed_throttling(false);
    FullSpeed(())
}

impl Drop for FullSpeed {
    fn drop(&mut self) {
        #[cfg(windows)]
        set_speed_throttling(true);
    }
}

/// Opt the process out of execution speed throttling, or (`allow`) hand the
/// choice back to Windows.
#[cfg(windows)]
fn set_speed_throttling(allow: bool) {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        // A policy bit with its state bit clear turns throttling off; no
        // policy bits leave it to the system.
        ControlMask: if allow { 0 } else { PROCESS_POWER_THROTTLING_EXECUTION_SPEED },
        StateMask: 0,
    };
    // SAFETY: `state` is a PROCESS_POWER_THROTTLING_STATE of the size given,
    // alive for the call; the current-process pseudo handle needs no closing.
    let result = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if let Err(e) = result {
        log::debug!("couldn't change power throttling: {e}");
    }
}

/// A system permission Rega asks the user to approve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Permission {
    ScreenRecording,
    Accessibility,
}

impl Permission {
    pub fn title(self) -> &'static str {
        match self {
            Permission::ScreenRecording => "Screen Recording",
            Permission::Accessibility => "Accessibility",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Permission::ScreenRecording => "Lets Rega see your screen to take screenshots and record video. macOS may also ask you to confirm direct screen access: choose Allow.",
            Permission::Accessibility => "Lets Rega scroll a page for you during scrolling capture. Everything else works without it.",
        }
    }

    /// Optional permissions only unlock a single feature.
    pub fn optional(self) -> bool {
        self == Permission::Accessibility
    }
}

/// The permissions this platform asks for. Only macOS gates capture behind any.
pub fn permissions_needed() -> &'static [Permission] {
    #[cfg(target_os = "macos")]
    return &[Permission::ScreenRecording, Permission::Accessibility];
    #[cfg(not(target_os = "macos"))]
    &[]
}

#[cfg(target_os = "macos")]
mod mac_permissions {
    use std::ffi::c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGPreflightScreenCaptureAccess() -> bool;
        pub fn CGRequestScreenCaptureAccess() -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        pub fn AXIsProcessTrusted() -> bool;
        pub fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
        pub static kAXTrustedCheckOptionPrompt: *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFBooleanTrue: *const c_void;
        pub static kCFTypeDictionaryKeyCallBacks: c_void;
        pub static kCFTypeDictionaryValueCallBacks: c_void;
        pub fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        pub fn CFRelease(cf: *const c_void);
    }

    /// Ask for Accessibility with the system prompt, which also adds Rega
    /// to the list in System Settings so the user only has to flip the switch.
    pub fn prompt_accessibility() {
        // SAFETY: builds a one-entry CFDictionary from CoreFoundation constants,
        // passes it to AXIsProcessTrustedWithOptions, and releases it once.
        unsafe {
            let keys = [kAXTrustedCheckOptionPrompt];
            let values = [kCFBooleanTrue];
            let dict = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            if !dict.is_null() {
                AXIsProcessTrustedWithOptions(dict);
                CFRelease(dict);
            }
        }
    }
}

/// Is `permission` currently granted? Screen Recording is cached per process by
/// macOS: after the user grants it, Rega must restart to see it.
pub fn is_granted(permission: Permission) -> bool {
    #[cfg(target_os = "macos")]
    // SAFETY: plain argument-less queries.
    return unsafe {
        match permission {
            Permission::ScreenRecording => mac_permissions::CGPreflightScreenCaptureAccess(),
            Permission::Accessibility => mac_permissions::AXIsProcessTrusted(),
        }
    };
    #[cfg(not(target_os = "macos"))]
    {
        let _ = permission;
        true
    }
}

/// Show the system prompt (macOS only shows it the first time it sees the app).
pub fn request(permission: Permission) {
    #[cfg(target_os = "macos")]
    match permission {
        // SAFETY: plain argument-less call.
        Permission::ScreenRecording => unsafe {
            mac_permissions::CGRequestScreenCaptureAccess();
        },
        Permission::Accessibility => mac_permissions::prompt_accessibility(),
    }
    #[cfg(not(target_os = "macos"))]
    let _ = permission;
}

/// Open the System Settings pane where the user switches `permission` on.
pub fn open_settings(permission: Permission) {
    #[cfg(target_os = "macos")]
    {
        let pane = match permission {
            Permission::ScreenRecording => "Privacy_ScreenCapture",
            Permission::Accessibility => "Privacy_Accessibility",
        };
        // macOS 13+ address first, then the classic one.
        for base in [
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension",
            "x-apple.systempreferences:com.apple.preference.security",
        ] {
            if Command::new("open").arg(format!("{base}?{pane}")).status().is_ok_and(|s| s.success()) {
                return;
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = permission;
}

/// Relaunch Rega (needed for a newly granted permission to apply).
pub fn relaunch() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        // Inside Rega.app/Contents/MacOS/ → reopen the bundle so macOS
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

/// Register (or remove) Rega to start when the user logs in. Per-user
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
        .join("Library/LaunchAgents/app.rega.Rega.plist");
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
  <key>Label</key><string>app.rega.Rega</string>
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
        cmd.args(["add", KEY, "/v", "Rega", "/t", "REG_SZ", "/f", "/d"]).arg(format!("\"{}\"", exe.display()));
    } else {
        cmd.args(["delete", KEY, "/v", "Rega", "/f"]);
    }
    // Deleting a value that was never set exits non-zero; that's fine.
    let status = cmd.status()?;
    anyhow::ensure!(status.success() || !enabled, "reg exited with {status}");
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


/// The shortcut modifier as shown in tooltips: ⌘ on macOS, Ctrl+ on Windows.
pub const CMD: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };
/// Shift as shown in tooltips.
pub const SHIFT: &str = if cfg!(target_os = "macos") { "⇧" } else { "Shift+" };

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{self, ViewportCommand};

    /// The border's windows are told once each to keep out of captures: the
    /// command repaints its window, so repeating it would redraw them nonstop.
    #[test]
    fn the_recording_border_is_protected_once_per_window() {
        let ctx = egui::Context::default();
        *RECORDING_RECT.lock().unwrap() = Some((100.0, 100.0, 400.0, 300.0));
        FRAME_PROTECTED.store(0, std::sync::atomic::Ordering::Relaxed);
        let mut sent = Vec::new();
        for _ in 0..4 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| paint_recording_frame(ui.ctx()));
            out.textures_delta.clear();
            let commands = out.viewport_output.values().flat_map(|v| &v.commands);
            sent.push(commands.filter(|c| matches!(c, ViewportCommand::ContentProtected(true))).count());
        }
        *RECORDING_RECT.lock().unwrap() = None;
        assert_eq!(sent, [4, 0, 0, 0]);
    }
}
