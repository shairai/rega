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

/// Whether the user asked the OS for less motion on screen (macOS: Reduce
/// Motion; Windows: animation effects off). Rega's moving borders then hold
/// still. Read when an overlay opens or a recording starts, not every frame.
#[cfg(target_os = "macos")]
pub fn reduce_motion() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

#[cfg(windows)]
pub fn reduce_motion() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut animate = windows::core::BOOL(1);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL through the pointer,
    // which points at `animate` for the whole call.
    let read = unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some((&raw mut animate).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
    };
    read.is_ok() && !animate.as_bool()
}

/// The recording border's four strips around `rect`, each (x, y, w, h) in
/// global top-left points like `rect`: top and bottom span the corners and
/// are `corner` tall, so rounded corners of that outer radius fit in them;
/// left and right fit between. A `corner` of `border::WIDTH` gives square
/// corners and strips just the band's width.
fn border_strips((x, y, w, h): (f32, f32, f32, f32), corner: f32) -> [(f32, f32, f32, f32); 4] {
    const T: f32 = crate::border::WIDTH;
    let side = h + 2.0 * T - 2.0 * corner;
    [
        (x - T, y - T, w + 2.0 * T, corner),
        (x - T, y - T + corner, T, side),
        (x - T, y + h + T - corner, w + 2.0 * T, corner),
        (x + w, y - T + corner, T, side),
    ]
}

/// The outer corner radius of the recording border around `rect`.
#[cfg(target_os = "macos")]
fn border_corner((x, y, w, h): (f32, f32, f32, f32)) -> f32 {
    crate::border::radii(eframe::egui::Rect::from_min_size(eframe::egui::Pos2::new(x, y), eframe::egui::vec2(w, h))).1
}

/// A border drawn just outside the region being recorded, so the user can see
/// what is captured. Four thin click-through panels: outside the region (and
/// excluded from screen capture), on every Space including full-screen apps.
/// `rect` is (x, y, w, h) in global top-left logical points. The border's
/// gradient (see `crate::border`) starts turned by `phase`, carrying on from
/// the selection overlay, and Core Animation turns it in the window server:
/// Rega does no work for it while recording.
#[cfg(target_os = "macos")]
pub fn show_recording_frame(rect: (f32, f32, f32, f32), phase: f32) {
    use objc2::MainThreadOnly;
    use objc2_app_kit::{
        NSBackingStoreType, NSColor, NSPanel, NSScreen, NSWindowCollectionBehavior as B, NSWindowSharingType, NSWindowStyleMask,
    };
    use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

    hide_recording_frame();
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(primary) = NSScreen::screens(mtm).firstObject() else { return };
    let primary_h = primary.frame().size.height;
    // One start for all four strips keeps their slices of the gradient in step.
    let begin = (!reduce_motion()).then(|| objc2_quartz_core::CACurrentMediaTime());
    let panels = border_strips(rect, border_corner(rect))
        .into_iter()
        .map(|strip| {
            let (sx, sy, sw, sh) = strip;
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
            // Clear around the band: its rounded corners leave the corners of
            // the top and bottom strips empty.
            panel.setOpaque(false);
            panel.setBackgroundColor(Some(&NSColor::clearColor()));
            panel.setHasShadow(false);
            panel.setIgnoresMouseEvents(true);
            panel.setHidesOnDeactivate(false);
            panel.setSharingType(NSWindowSharingType::None);
            panel.setCollectionBehavior(B::CanJoinAllSpaces | B::FullScreenAuxiliary | B::Stationary);
            panel.setLevel(1000);
            let (root, gradient) = border_layer::strip(rect, strip, phase, begin, panel.backingScaleFactor());
            if let Some(view) = panel.contentView() {
                // Layer-hosting (layer first): the view shows `root` and leaves
                // its sublayers to us.
                view.setLayer(Some(&root));
                view.setWantsLayer(true);
            }
            panel.orderFrontRegardless();
            (panel, gradient)
        })
        .collect();
    RECORDING_FRAME.with(|f| *f.borrow_mut() = panels);
}

/// The recording border's layers on macOS. Each strip holds its slice of one
/// conic gradient laid over the whole border, so Core Animation can turn it
/// as one in the window server.
#[cfg(target_os = "macos")]
mod border_layer {
    use std::f64::consts::TAU;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGColor, CGMutablePath};
    use objc2_foundation::{NSArray, NSNumber, ns_string};
    use objc2_quartz_core::{
        CABasicAnimation, CAFrameRateRange, CAGradientLayer, CALayer, CAMediaTiming, CAMediaTimingFunction, CAShapeLayer,
        kCAFillRuleEvenOdd, kCAGradientLayerConic, kCAMediaTimingFunctionLinear,
    };

    use crate::border;

    /// Colour stops once around; Core Animation blends straight between them.
    const STOPS: usize = 24;

    /// The layers for `strip` of the border around `rect` (both (x, y, w, h)
    /// in global top-left points): a root layer the size of the strip and the
    /// gradient it holds. The gradient starts turned by `phase` and, given a
    /// `begin` time (`CACurrentMediaTime`), turns from then on. `scale` is the
    /// screen's pixels per point.
    pub fn strip(
        rect: (f32, f32, f32, f32),
        strip: (f32, f32, f32, f32),
        phase: f32,
        begin: Option<f64>,
        scale: f64,
    ) -> (Retained<CALayer>, Retained<CAGradientLayer>) {
        let (x, y, w, h) = (rect.0 as f64, rect.1 as f64, rect.2 as f64, rect.3 as f64);
        let (sx, sy, sw, sh) = (strip.0 as f64, strip.1 as f64, strip.2 as f64, strip.3 as f64);
        let gradient = CAGradientLayer::new();
        // SAFETY: reads a constant that Core Animation exports.
        gradient.setType(unsafe { kCAGradientLayerConic });
        // Core Animation's cone runs anticlockwise on screen and ours runs
        // clockwise, so its stop at u turns is ours at -u.
        let colors: Vec<CFRetained<CGColor>> = (0..=STOPS)
            .map(|k| {
                let [r, g, b] = border::palette(-(k as f32) / STOPS as f32);
                CGColor::new_srgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, 1.0)
            })
            .collect();
        let colors: Vec<&AnyObject> = colors
            .iter()
            .map(|c| {
                let c: &CFType = c;
                c.as_ref()
            })
            .collect();
        // SAFETY: `colors` takes an array of CGColors.
        unsafe { gradient.setColors(Some(&NSArray::from_slice(&colors))) };
        let locations: Vec<_> = (0..=STOPS).map(|k| NSNumber::new_f64(k as f64 / STOPS as f64)).collect();
        gradient.setLocations(Some(&NSArray::from_retained_slice(&locations)));
        // Square, as Core Animation lays the cone out in the unit square and a
        // longer side would squash it, and big enough to cover the border at
        // any angle.
        let t = border::WIDTH as f64;
        let side = (w + 2.0 * t).hypot(h + 2.0 * t) + 2.0;
        gradient.setBounds(CGRect::new(CGPoint::ZERO, CGSize::new(side, side)));
        // Centred on the region, in the strip's points (origin bottom-left).
        gradient.setPosition(CGPoint::new(x + w / 2.0 - sx, sy + sh - (y + h / 2.0)));
        gradient.setStartPoint(CGPoint::new(0.5, 0.5));
        // The end point is where the cone starts: `phase` clockwise from up.
        let a = phase as f64 * TAU;
        gradient.setEndPoint(CGPoint::new(0.5 + 0.5 * a.sin(), 0.5 + 0.5 * a.cos()));
        gradient.setContentsScale(scale);
        if let Some(begin) = begin {
            let spin = CABasicAnimation::animationWithKeyPath(Some(ns_string!("transform.rotation.z")));
            // Negative turns clockwise here, where y points up.
            let (from, to) = (NSNumber::new_f64(0.0), NSNumber::new_f64(-TAU));
            // SAFETY: a rotation takes numbers.
            unsafe {
                spin.setFromValue(Some(&from));
                spin.setToValue(Some(&to));
            }
            spin.setDuration(border::TURN_SECS);
            spin.setRepeatCount(f32::INFINITY);
            // SAFETY: reads a constant that Core Animation exports.
            spin.setTimingFunction(Some(&CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionLinear })));
            spin.setBeginTime(gradient.convertTime_fromLayer(begin, None));
            // About 30 frames a second, as on Windows: plenty for a slow turn,
            // and a ProMotion screen needn't run at 120 Hz for it.
            spin.setPreferredFrameRateRange(CAFrameRateRange { minimum: 24.0, maximum: 60.0, preferred: 30.0 });
            gradient.addAnimation_forKey(&spin, Some(ns_string!("spin")));
        }
        let root = CALayer::new();
        root.setBounds(CGRect::new(CGPoint::ZERO, CGSize::new(sw, sh)));
        root.setContentsScale(scale);
        root.addSublayer(&gradient);
        // Only the band shows, round at the corners; the rest is clear.
        let mask = CAShapeLayer::new();
        mask.setPath(Some(&band_path(rect, strip)));
        // SAFETY: reads a constant that Core Animation exports.
        mask.setFillRule(unsafe { kCAFillRuleEvenOdd });
        mask.setContentsScale(scale);
        // SAFETY: a mask mustn't have a superlayer, and this new one has none.
        unsafe { root.setMask(Some(&mask)) };
        (root, gradient)
    }

    /// The border's band around `rect`, in `strip`'s points (origin
    /// bottom-left): the outer rounded rect with the inner one cut out.
    fn band_path(rect: (f32, f32, f32, f32), strip: (f32, f32, f32, f32)) -> CFRetained<CGMutablePath> {
        let (x, y, w, h) = rect;
        let (inner, outer) = border::radii(eframe::egui::Rect::from_min_size(eframe::egui::pos2(x, y), eframe::egui::vec2(w, h)));
        let (sx, sy, sh) = (strip.0 as f64, strip.1 as f64, strip.3 as f64);
        let local = |gx: f32, gy: f32, gw: f32, gh: f32| {
            let (gx, gy, gw, gh) = (gx as f64, gy as f64, gw as f64, gh as f64);
            CGRect::new(CGPoint::new(gx - sx, sy + sh - (gy + gh)), CGSize::new(gw, gh))
        };
        let t = border::WIDTH;
        let path = CGMutablePath::new();
        // SAFETY: a null transform means none, and `radii` keeps each radius
        // within half its rect's sides.
        unsafe {
            CGMutablePath::add_rounded_rect(Some(&path), std::ptr::null(), local(x - t, y - t, w + 2.0 * t, h + 2.0 * t), outer as f64, outer as f64);
            CGMutablePath::add_rounded_rect(Some(&path), std::ptr::null(), local(x, y, w, h), inner as f64, inner as f64);
        }
        path
    }
}

#[cfg(target_os = "macos")]
thread_local! {
    /// The recording border's panels, each with the gradient it shows.
    static RECORDING_FRAME: std::cell::RefCell<
        Vec<(objc2::rc::Retained<objc2_app_kit::NSPanel>, objc2::rc::Retained<objc2_quartz_core::CAGradientLayer>)>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(target_os = "macos")]
pub fn hide_recording_frame() {
    RECORDING_FRAME.with(|f| {
        for (panel, gradient) in f.borrow_mut().drain(..) {
            gradient.removeAllAnimations();
            panel.orderOut(None);
        }
    });
}

/// Windows: the same border, drawn by four borderless click-through egui
/// viewports (see `paint_recording_frame`).
#[cfg(not(target_os = "macos"))]
pub fn show_recording_frame(rect: (f32, f32, f32, f32), phase: f32) {
    let animate = !reduce_motion();
    *RECORDING_BORDER.lock().unwrap() = Some(RecordingBorder { rect, phase, since: std::time::Instant::now(), animate });
    // New windows, which have yet to be kept out of captures.
    FRAME_PROTECTED.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(target_os = "macos"))]
pub fn hide_recording_frame() {
    *RECORDING_BORDER.lock().unwrap() = None;
}

/// Where the recording border is and how its gradient moves (Windows).
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Clone, Copy)]
struct RecordingBorder {
    rect: (f32, f32, f32, f32),
    /// The gradient's turn at `since`, carried on from the selection overlay.
    phase: f32,
    since: std::time::Instant,
    /// Off when the user asked the OS for reduced motion.
    animate: bool,
}

impl RecordingBorder {
    /// The gradient's turn at `now`.
    fn phase(&self, now: std::time::Instant) -> f32 {
        if self.animate {
            (self.phase + crate::border::phase(now.duration_since(self.since).as_secs_f64())).fract()
        } else {
            self.phase
        }
    }

    /// How long from `now` until the strips' next frame. Frames fall on one
    /// grid, about 30 a second from `since`, so the four strips repaint
    /// together: in step, and with one wake-up for all of them.
    fn until_next_frame(&self, now: std::time::Instant) -> std::time::Duration {
        const FRAME: std::time::Duration = std::time::Duration::from_millis(33);
        let into = now.duration_since(self.since).as_nanos() % FRAME.as_nanos();
        FRAME - std::time::Duration::from_nanos(into as u64)
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
static RECORDING_BORDER: std::sync::Mutex<Option<RecordingBorder>> = std::sync::Mutex::new(None);

/// Which of the border's viewports were told to keep out of captures (bit
/// `i` for strip `i`): told once each, as every viewport command also
/// repaints its viewport, so telling it on every paint redraws it nonstop.
#[cfg_attr(target_os = "macos", allow(dead_code))]
static FRAME_PROTECTED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Call every frame: keeps the recording border's windows alive while a
/// recording runs (a viewport closes when it stops being shown). They are
/// excluded from capture, so they never appear in the video. Each strip
/// paints its slice of the gradient by where it sits on screen, and repaints
/// itself about 30 times a second to turn it (never under reduced motion).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn paint_recording_frame(ctx: &eframe::egui::Context) {
    use eframe::egui::{self, Pos2, Vec2, ViewportBuilder, ViewportId};
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    let Some(border) = *RECORDING_BORDER.lock().unwrap() else { return };
    let (x, y, w, h) = border.rect;
    let center = Pos2::new(x + w / 2.0, y + h / 2.0);
    // Square corners here: a strip window can't be partly see-through.
    for (i, (sx, sy, sw, sh)) in border_strips(border.rect, crate::border::WIDTH).into_iter().enumerate() {
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
            move |ui, _| {
                if FRAME_PROTECTED.fetch_or(1 << i, Ordering::Relaxed) & (1 << i) == 0 {
                    ui.send_viewport_cmd(egui::ViewportCommand::ContentProtected(true));
                }
                let now = Instant::now();
                let mesh = crate::border::strip(ui.max_rect(), Pos2::new(sx, sy), center, border.phase(now));
                ui.painter().add(egui::Shape::mesh(mesh));
                if border.animate {
                    // Smooth enough for a slow turn; only the strips repaint.
                    ui.request_repaint_after(border.until_next_frame(now));
                }
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
    use std::time::{Duration, Instant};

    use super::*;
    use crate::border;
    use eframe::egui::{self, Color32, Mesh, Pos2, Rect, Vec2, ViewportCommand, ViewportId};

    /// The recording border's state is global, so the tests using it take turns.
    static BORDER_STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn recording(animate: bool) -> RecordingBorder {
        RecordingBorder { rect: (100.0, 100.0, 400.0, 300.0), phase: 0.25, since: Instant::now(), animate }
    }

    /// Run `frames` frames of the border's windows (embedded, as a plain
    /// context has no real viewports) and return what each frame asked for.
    fn paint_frames(border: RecordingBorder, frames: usize) -> Vec<egui::FullOutput> {
        let ctx = egui::Context::default();
        *RECORDING_BORDER.lock().unwrap() = Some(border);
        FRAME_PROTECTED.store(0, std::sync::atomic::Ordering::Relaxed);
        let outputs = (0..frames)
            .map(|_| {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| paint_recording_frame(ui.ctx()));
                out.textures_delta.clear();
                out
            })
            .collect();
        *RECORDING_BORDER.lock().unwrap() = None;
        outputs
    }

    /// The border's windows are told once each to keep out of captures: the
    /// command repaints its window, so repeating it would redraw them nonstop.
    #[test]
    fn the_recording_border_is_protected_once_per_window() {
        let _turn = BORDER_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let sent: Vec<usize> = paint_frames(recording(true), 4)
            .iter()
            .map(|out| {
                let commands = out.viewport_output.values().flat_map(|v| &v.commands);
                commands.filter(|c| matches!(c, ViewportCommand::ContentProtected(true))).count()
            })
            .collect();
        assert_eq!(sent, [4, 0, 0, 0]);
    }

    /// The strips repaint themselves about 30 times a second to turn the
    /// gradient, never back to back; under reduced motion they never repaint
    /// and the gradient holds still.
    #[test]
    fn the_recording_border_turns_at_30_fps_unless_motion_is_reduced() {
        let _turn = BORDER_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let next_repaint = |animate| {
            let outputs = paint_frames(recording(animate), 30);
            outputs.last().unwrap().viewport_output[&ViewportId::ROOT].repaint_delay
        };
        let turning = next_repaint(true);
        assert!(turning > Duration::ZERO && turning <= Duration::from_millis(33), "{turning:?}");
        assert_eq!(next_repaint(false), Duration::MAX);

        let start = Instant::now();
        let (turning, still) = (RecordingBorder { since: start, ..recording(true) }, recording(false));
        let half_a_turn = Duration::from_secs_f64(border::TURN_SECS / 2.0);
        assert!((turning.phase(start + half_a_turn) - 0.75).abs() < 1e-3);
        assert_eq!(still.phase(start + half_a_turn), 0.25);
        // Frames fall on one grid: however far into a frame a strip paints,
        // it asks for the same next one as the others.
        let ms = Duration::from_millis;
        assert_eq!(turning.until_next_frame(start), ms(33));
        for into in [0, 1, 12, 32] {
            let now = start + ms(5 * 33 + into);
            assert_eq!(now + turning.until_next_frame(now), start + ms(6 * 33));
        }
    }

    /// With real windows, as eframe gives them on Windows: each strip paints
    /// in its own pass and asks only for its own next frame, so the main
    /// window is never woken by the border. Past the two quick frames egui
    /// gives every new window, each strip asks once per frame, within 33 ms;
    /// under reduced motion none asks at all.
    #[test]
    fn only_the_border_windows_repaint_while_recording() {
        let _turn = BORDER_STATE.lock().unwrap_or_else(|e| e.into_inner());
        for animate in [true, false] {
            let ctx = egui::Context::default();
            ctx.set_embed_viewports(false);
            // A main window that has settled before the recording starts.
            for _ in 0..2 {
                ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
            }
            // What eframe listens to when it schedules each window's repaint.
            let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = asked.clone();
            ctx.set_request_repaint_callback(move |info| log.lock().unwrap().push((info.viewport_id, info.delay)));
            *RECORDING_BORDER.lock().unwrap() = Some(recording(animate));
            FRAME_PROTECTED.store(0, std::sync::atomic::Ordering::Relaxed);
            for round in 0..5 {
                asked.lock().unwrap().clear();
                // The main window's frame, which keeps the strips open, then each strip's.
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| paint_recording_frame(ui.ctx()));
                out.textures_delta.clear();
                let strips: Vec<_> = out.viewport_output.iter().filter_map(|(id, v)| Some((*id, v.viewport_ui_cb.clone()?))).collect();
                assert_eq!(strips.len(), 4);
                for (id, paint) in &strips {
                    let mut input = egui::RawInput { viewport_id: *id, ..Default::default() };
                    input.viewports.insert(*id, Default::default());
                    let mut out = ctx.run_ui(input, |ui| paint(ui));
                    out.textures_delta.clear();
                    let protect = out.viewport_output[id].commands.iter().filter(|c| matches!(c, ViewportCommand::ContentProtected(true)));
                    assert_eq!(protect.count(), usize::from(round == 0), "round {round}");
                }
                let asked = asked.lock().unwrap();
                assert!(asked.iter().all(|(id, _)| strips.iter().any(|(strip, _)| strip == id)), "round {round}: {asked:?}");
                if round >= 2 {
                    let next_frames = asked.iter().filter(|(_, delay)| *delay > Duration::ZERO && *delay <= Duration::from_millis(33));
                    assert_eq!((asked.len(), next_frames.count()), if animate { (4, 4) } else { (0, 0) }, "round {round}: {asked:?}");
                }
            }
            *RECORDING_BORDER.lock().unwrap() = None;
        }
    }

    /// The mesh's colour at `p`, blended from the triangle it falls in.
    fn sample(mesh: &Mesh, p: Pos2) -> Option<[f32; 3]> {
        let cross = |u: Vec2, v: Vec2| u.x * v.y - u.y * v.x;
        mesh.indices.chunks(3).find_map(|t| {
            let [a, b, c] = [0, 1, 2].map(|i| &mesh.vertices[t[i] as usize]);
            let area = cross(b.pos - a.pos, c.pos - a.pos);
            let wb = cross(p - a.pos, c.pos - a.pos) / area;
            let wc = cross(b.pos - a.pos, p - a.pos) / area;
            let wa = 1.0 - wb - wc;
            (wa >= -1e-4 && wb >= -1e-4 && wc >= -1e-4).then(|| {
                let channel = |f: fn(&Color32) -> u8| wa * f(&a.color) as f32 + wb * f(&b.color) as f32 + wc * f(&c.color) as f32;
                [channel(Color32::r), channel(Color32::g), channel(Color32::b)]
            })
        })
    }

    /// Each strip window paints its slice by where it sits on screen, so the
    /// colours run on across the joins, and the strips cover the ring around
    /// the region exactly.
    #[test]
    fn the_recording_border_strips_line_up() {
        let rect = (100.0, 80.0, 640.0, 360.0);
        let (x, y, w, h) = rect;
        let center = Pos2::new(x + w / 2.0, y + h / 2.0);
        let phase = 0.37;
        let strips: Vec<(Rect, Mesh)> = border_strips(rect, border::WIDTH)
            .into_iter()
            .map(|(sx, sy, sw, sh)| {
                let local = Rect::from_min_size(Pos2::ZERO, Vec2::new(sw, sh));
                (local.translate(Vec2::new(sx, sy)), border::strip(local, Pos2::new(sx, sy), center, phase))
            })
            .collect();
        let region = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h));
        let total: f32 = strips.iter().map(|(r, _)| r.area()).sum();
        assert_eq!(total, region.expand(border::WIDTH).area() - region.area());
        let mut joins = 0;
        for (i, (a, mesh)) in strips.iter().enumerate() {
            for v in &mesh.vertices {
                assert_eq!(v.color, border::color_at(a.min + v.pos.to_vec2(), center, phase));
            }
            for (b, other) in &strips[i + 1..] {
                let join = a.intersect(*b);
                assert!(join.area() <= 0.0, "{a:?} and {b:?} overlap");
                if join.width() < 0.0 || join.height() < 0.0 {
                    continue; // they don't touch
                }
                joins += 1;
                for k in 0..=6 {
                    let p = join.min.lerp(join.max, k as f32 / 6.0);
                    let here = sample(mesh, p - a.min.to_vec2()).expect("on the strip");
                    let there = sample(other, p - b.min.to_vec2()).expect("on the strip");
                    let gap = (0..3).map(|c| (here[c] - there[c]).abs()).fold(0.0, f32::max);
                    assert!(gap <= 2.0, "{gap} levels apart at {p:?}");
                }
            }
        }
        assert_eq!(joins, 4, "one join per corner");
    }

    /// Run `f`, which builds layers, in turn with the other layer tests, and
    /// commit its layer changes before handing over: Core Animation holds a
    /// thread's changed layers until a commit (the app's run loop makes one
    /// every turn), and a commit can reach layers another thread is still
    /// building.
    #[cfg(target_os = "macos")]
    fn with_layers<R>(f: impl FnOnce() -> R) -> R {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let result = objc2::rc::autoreleasepool(|_| f());
        objc2_quartz_core::CATransaction::flush();
        result
    }

    /// `root` drawn off-screen into an sRGB bitmap the size of `strip` at
    /// `scale` pixels per point, with Core Animation's premultiplied colours
    /// made plain.
    #[cfg(target_os = "macos")]
    fn render_layer(root: &objc2_quartz_core::CALayer, (_, _, sw, sh): (f32, f32, f32, f32), scale: f64) -> image::RgbaImage {
        use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, kCGColorSpaceSRGB};
        let (pw, ph) = ((sw as f64 * scale).round() as u32, (sh as f64 * scale).round() as u32);
        let mut img = image::RgbaImage::new(pw, ph);
        // SAFETY: reads a constant that Core Graphics exports.
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).expect("sRGB");
        // SAFETY: `img` holds `ph` rows of `pw` RGBA pixels, top row first as
        // Core Graphics lays them out, and outlives the context.
        let ctx = unsafe {
            CGBitmapContextCreate(
                img.as_mut_ptr().cast(),
                pw as usize,
                ph as usize,
                8,
                pw as usize * 4,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .expect("bitmap context");
        CGContext::scale_ctm(Some(&ctx), scale, scale);
        root.renderInContext(&ctx);
        drop(ctx);
        for px in img.pixels_mut() {
            if px[3] > 0 {
                let a = px[3] as f32;
                for c in 0..3 {
                    px[c] = (px[c] as f32 * 255.0 / a).round() as u8;
                }
            }
        }
        img
    }

    /// The border's layers, drawn off-screen strip by strip at Retina scale:
    /// every pixel shows the shared gradient at its spot on screen, so the
    /// strips meet seamlessly at the corners, both at the start and after the
    /// animation's own quarter (and more) of a turn, which moves it clockwise
    /// as on Windows and in the overlay. Set `BORDER_PREVIEW_DIR` to save the
    /// assembled border.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_recording_border_layers_show_the_shared_gradient() {
        use objc2_core_foundation::CGAffineTransform;
        use objc2_foundation::{NSNumber, ns_string};
        use objc2_quartz_core::CABasicAnimation;

        let rect = (130.0, 90.0, 320.0, 180.0);
        let (x, y, w, h) = rect;
        let center = Pos2::new(x + w / 2.0, y + h / 2.0);
        let (phase, scale, t) = (0.1, 2.0, border::WIDTH);
        let region = Rect::from_min_size(Pos2::new(x, y), egui::vec2(w, h));
        let (inner, outer) = border::radii(region);
        for turned in [0.0, 0.25, 0.6] {
            // The whole border, put together from its strips.
            let size = |len: f32| ((len + 2.0 * t) * scale as f32) as u32;
            let mut frame = image::RgbaImage::new(size(w), size(h));
            let mut worst = 0;
            for strip in border_strips(rect, border_corner(rect)) {
                let img = with_layers(|| {
                    let (root, gradient) = border_layer::strip(rect, strip, phase, Some(0.0), scale);
                    // SAFETY: "spin" is the CABasicAnimation `border_layer` adds.
                    let spin = unsafe { gradient.animationForKey(ns_string!("spin")) }.expect("turning");
                    let spin = spin.downcast::<CABasicAnimation>().unwrap();
                    let per_turn = spin.toValue().unwrap().downcast::<NSNumber>().unwrap().as_f64();
                    // Held where the animation has it `turned` turns on.
                    gradient.removeAllAnimations();
                    let a = per_turn * turned as f64;
                    gradient.setAffineTransform(CGAffineTransform { a: a.cos(), b: a.sin(), c: -a.sin(), d: a.cos(), tx: 0.0, ty: 0.0 });
                    render_layer(&root, strip, scale)
                });
                for (i, j, px) in img.enumerate_pixels() {
                    let p = Pos2::new(strip.0 + (i as f32 + 0.5) / scale as f32, strip.1 + (j as f32 + 0.5) / scale as f32);
                    // Clear outside the band, solid inside it; its smoothed
                    // edges (round the corners too) are in between.
                    let (beyond, within) = (border::rounded_distance(p, region.expand(t), outer), border::rounded_distance(p, region, inner));
                    if beyond > 0.5 || within < -0.5 {
                        assert!(px[3] <= 2, "showing at {p:?}, outside the band");
                        continue;
                    }
                    if beyond > -0.5 || within < 0.5 {
                        continue;
                    }
                    // Core Animation draws a cone at 254 of 255: opaque enough.
                    assert!(px[3] >= 254, "a hole at {p:?}");
                    let want = border::color_at(p, center, phase + turned);
                    let err = (0..3).map(|c| px[c].abs_diff([want.r(), want.g(), want.b()][c])).max().unwrap();
                    worst = worst.max(err);
                    let at = |v: f32, from: f32| ((v - from + t) * scale as f32) as u32;
                    frame.put_pixel(at(p.x, x), at(p.y, y), image::Rgba([px[0], px[1], px[2], 255]));
                }
            }
            assert!(worst <= 3, "{worst} levels off the gradient after {turned} of a turn");
            // No seams: neighbouring pixels anywhere on the border are alike.
            for (i, j, px) in frame.enumerate_pixels().filter(|(_, _, px)| px[3] > 0) {
                for (ni, nj) in [(i + 1, j), (i, j + 1)] {
                    if let Some(n) = frame.get_pixel_checked(ni, nj).filter(|n| n[3] > 0) {
                        let step = (0..3).map(|c| px[c].abs_diff(n[c])).max().unwrap();
                        assert!(step <= 3, "a seam of {step} levels at ({i}, {j})");
                    }
                }
            }
            if let Ok(dir) = std::env::var("BORDER_PREVIEW_DIR") {
                frame.save(std::path::Path::new(&dir).join(format!("recording-border-{turned}.png"))).unwrap();
            }
        }
    }

    /// Under reduced motion a strip's gradient holds still: no animation.
    /// Otherwise it turns clockwise, evenly and forever, a turn every
    /// `TURN_SECS` from the shared start.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_recording_border_layers_turn_unless_motion_is_reduced() {
        use objc2::rc::Retained;
        use objc2::runtime::AnyObject;
        use objc2_foundation::{NSNumber, ns_string};
        use objc2_quartz_core::{CABasicAnimation, CAMediaTiming, CAMediaTimingFunction, kCAMediaTimingFunctionLinear};

        with_layers(|| {
            let rect = (0.0, 0.0, 200.0, 100.0);
            let strip = border_strips(rect, border_corner(rect))[2];
            let (_, still) = border_layer::strip(rect, strip, 0.3, None, 2.0);
            assert!(still.animationKeys().is_none_or(|keys| keys.is_empty()));

            let (_, turning) = border_layer::strip(rect, strip, 0.3, Some(42.0), 2.0);
            assert_eq!(turning.animationKeys().map(|keys| keys.len()), Some(1));
            // SAFETY: "spin" is the CABasicAnimation `border_layer` adds.
            let spin = unsafe { turning.animationForKey(ns_string!("spin")) }.unwrap().downcast::<CABasicAnimation>().unwrap();
            let number = |v: Option<Retained<AnyObject>>| v.unwrap().downcast::<NSNumber>().unwrap().as_f64();
            assert_eq!(spin.keyPath().unwrap().to_string(), "transform.rotation.z");
            assert_eq!((number(spin.fromValue()), number(spin.toValue())), (0.0, -std::f64::consts::TAU));
            assert_eq!(spin.duration(), border::TURN_SECS);
            assert_eq!(spin.repeatCount(), f32::INFINITY);
            assert_eq!(spin.beginTime(), 42.0);
            assert_eq!(spin.preferredFrameRateRange().preferred, 30.0);
            // SAFETY: reads a constant that Core Animation exports.
            let linear = CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionLinear });
            assert_eq!(spin.timingFunction(), Some(linear));
        });
    }

    /// Nothing keeps a strip's layers alive once Rega lets go of them, not
    /// even their endless animation.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_recording_border_layers_are_released() {
        use objc2::rc::Weak;
        let rect = (10.0, 10.0, 300.0, 200.0);
        let (root, gradient) = with_layers(|| {
            let (root, gradient) = border_layer::strip(rect, border_strips(rect, border_corner(rect))[1], 0.0, Some(0.0), 2.0);
            (Weak::from_retained(&root), Weak::from_retained(&gradient))
        });
        assert!(root.load().is_none() && gradient.load().is_none());
    }
}
