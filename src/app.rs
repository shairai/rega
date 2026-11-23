//! Application shell: library, capture flows, tray, global hotkeys, settings.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, Vec2,
    ViewportCommand,
};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use image::RgbaImage;

use crate::capture;
use crate::config::{APP_NAME, GifQuality, ImageFormat, Settings, VideoFormat};
use crate::editor::{Editor, EditorAction, Tool};
use crate::guide::editor::{GuideAction, GuideEditor};
use crate::guide::recorder::{GuideConfig, GuideRecorder};
use crate::theme::{self, ItemStyle, icon};
use crate::library::{self, Item, ItemKind};
use crate::ocr;
use crate::overlay::{Outcome, Overlay, Purpose};
use crate::platform;
use crate::player::{BackgroundSaves, Player, PlayerAction};
use crate::recorder::{Finished, RecordConfig, Recorder};
use crate::scroll::{self, ScrollMsg};
use crate::shortcuts::{self, Action};
use crate::update;

#[derive(Clone, Copy, Debug)]
pub enum AppEvent {
    Capture(Purpose),
    FullScreen,
    ShowMain,
    NewImage,
    OpenImage,
    CloseItem,
    DeleteItem,
    About,
    Quit,
}

enum Mode {
    Home,
    Editor(Box<Editor>),
    Video(Box<Player>),
    Guide(Box<GuideEditor>),
}

enum Activity {
    Idle,
    /// Window hidden, waiting for it to disappear (and for the user's delay).
    Pending { purpose: Purpose, full: bool, at: Instant, cursor: (i32, i32) },
    Selecting(Box<Overlay>),
    Recording(Recorder),
    Finishing(Recorder),
    Scrolling(Receiver<ScrollMsg>),
    /// Recording a guide: clicks and typing become steps.
    Guiding(Box<GuideRecorder>),
    /// A guide recording ended: its steps, or `None` when it was discarded.
    GuideFinished(Option<Vec<crate::guide::Step>>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Win {
    Normal,
    Hidden,
    Overlay,
    Bar,
}

const BAR_SIZE: Vec2 = Vec2::new(340.0, 52.0);

pub struct RegaApp {
    /// For background work (e.g. the video player) that needs to request repaints.
    ctx: egui::Context,
    settings: Settings,
    mode: Mode,
    activity: Activity,
    win: Win,
    hidden_by_user: bool,
    normal_outer: Option<Rect>,
    normal_inner: Option<Rect>,

    library: Vec<Item>,
    library_scanned: Instant,
    filter: String,
    lib_filter: LibFilter,
    thumbs: HashMap<PathBuf, (SystemTime, TextureHandle)>,
    thumbs_requested: HashSet<(PathBuf, SystemTime)>,
    thumb_jobs: Sender<(PathBuf, SystemTime)>,
    thumb_results: Receiver<(PathBuf, SystemTime, egui::ColorImage)>,

    events: Receiver<AppEvent>,
    hotkeys: Option<GlobalHotKeyManager>,
    hotkey_map: Arc<Mutex<HashMap<u32, AppEvent>>>,
    registered: Vec<HotKey>,
    recording_shortcut: Option<Action>,
    menu_items: Vec<(Action, tray_icon::menu::MenuItem)>,
    tray: Option<tray_icon::TrayIcon>,
    _menu_bar: Option<tray_icon::menu::Menu>,

    toast: Option<(String, Instant)>,
    /// Copies still being saved by players that have closed: they finish
    /// here, and are reported (see `report_background_saves`).
    saves: BackgroundSaves,
    ocr_rx: Option<Receiver<anyhow::Result<String>>>,
    ocr_text: Option<String>,
    show_settings: bool,
    show_about: bool,
    /// Library items picked with ⌘/⇧-click, for acting on several at once.
    lib_selection: HashSet<PathBuf>,
    /// Where a ⇧-click range starts: the last item clicked with or without ⌘.
    lib_anchor: Option<PathBuf>,
    /// Screen Recording permission is missing; captures would only show the wallpaper.
    /// macOS: overlay waiting to be shown natively on the next pass.
    overlay_to_present: Option<((i32, i32), (u32, u32))>,
    /// Passes left before the window, made transparent while it changed role, is
    /// shown again; see `hide_until_painted`.
    reveal_in: Option<u8>,
    /// When revealed, order the window behind other apps (shown in the background).
    reveal_behind: bool,
    show_permissions: bool,
    perm_status: Vec<(platform::Permission, bool)>,
    /// The UI zoom (⌘+/⌘−) to restore once a capture ends; see `sync_capture_zoom`.
    capture_zoom: Option<f32>,
    /// Smallest the main window may be; shrinks on small screens.
    min_size: Vec2,
    perm_polled: Option<Instant>,
    /// The user pressed Enable at least once: offer a restart.
    perm_opened: bool,
    hover_tracking: bool,
    new_canvas: NewCanvas,
    show_new_dialog: bool,
    settings_tab: SettingsTab,
    /// A newer version found on the website (see `update`).
    update: Option<update::Latest>,
    /// The update check running in the background.
    update_rx: Option<Receiver<anyhow::Result<Option<update::Latest>>>>,
    /// The running check was asked for in Settings: report how it went.
    update_manual: bool,
    /// When the daily check is next due, by the clock (so time asleep
    /// counts); `None` never checks (tests).
    update_due: Option<SystemTime>,
    /// An update being installed in place, and its step as last heard.
    installing: Option<(Receiver<update::Progress>, Installing)>,
    /// A checked new version, waiting for Rega to be idle to restart into it.
    ready_update: Option<update::Prepared>,
    /// Where this copy can update itself in place, worked out once (`Err`: why not).
    install_target: Option<Result<PathBuf, String>>,
}

/// What the update notice shows while an update is installed in place.
#[derive(Clone, Copy, PartialEq)]
enum Installing {
    Downloading(f32),
    Checking,
    Restarting,
}

impl RegaApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::build(&cc.egui_ctx, Settings::load(), true);
        // The window opens at launch, so show the Dock icon (we start as an agent).
        platform::set_dock_icon(true);
        // Dev builds would register a throwaway target/ binary.
        if !cfg!(debug_assertions) {
            platform::set_launch_at_login(app.settings.launch_at_login);
        }
        app.refresh_permissions(true);
        if app.missing_permissions() && !app.settings.permissions_prompt_shown {
            app.show_permissions = true;
            app.settings.permissions_prompt_shown = true;
            app.settings.save();
        }
        if cfg!(target_os = "macos")
            && platform::is_granted(platform::Permission::ScreenRecording)
            && !app.settings.screen_access_probed
        {
            app.settings.screen_access_probed = true;
            app.settings.save();
            std::thread::spawn(capture::probe_screen_access);
        }
        // Partial copies left by a save that a crash or a forced quit cut
        // short. Off the UI thread: the folder may be large, or on a network.
        let dir = app.settings.save_dir.clone();
        std::thread::spawn(move || {
            let removed = library::remove_stale_parts(&dir);
            if removed > 0 {
                log::info!("removed {removed} unfinished copies from {}", dir.display());
            }
        });
        let failed = app.apply_hotkeys();
        if !failed.is_empty() && app.toast.is_none() {
            app.toast(format!("Shortcut unavailable: {}", failed.join(", ")));
        }
        app
    }

    /// Rega without menus, tray, global hotkeys or permission prompts,
    /// showing `library` — used to render the README screenshots.
    #[cfg(test)]
    pub fn demo(ctx: &egui::Context, library: PathBuf) -> Self {
        let settings = Settings { save_dir: library, ..Settings::default() };
        Self::build(ctx, settings, false)
    }

    fn build(ctx: &egui::Context, settings: Settings, system: bool) -> Self {
        let ctx = ctx.clone();
        theme::install(&ctx);
        let _ = std::fs::create_dir_all(&settings.save_dir);

        let (event_tx, events) = mpsc::channel();
        let (hotkeys, hotkey_map, hotkey_error, tray, menu_bar, menu_items) = if system {
            let (hotkeys, map, err) = setup_hotkeys(&ctx, event_tx.clone());
            let (tray, menu_bar, items) = setup_menus(&ctx, event_tx);
            (hotkeys, map, err, tray, Some(menu_bar), items)
        } else {
            (None, HotkeyMap::default(), None, None, None, Vec::new())
        };

        let (thumb_jobs, thumb_results) = spawn_thumbnailer(ctx.clone());
        let library = library::scan(&settings.save_dir);

        Self {
            ctx: ctx.clone(),
            settings,
            mode: Mode::Home,
            activity: Activity::Idle,
            win: Win::Normal,
            hidden_by_user: false,
            normal_outer: None,
            normal_inner: None,
            library,
            library_scanned: Instant::now(),
            filter: String::new(),
            lib_filter: LibFilter::All,
            thumbs: HashMap::new(),
            thumbs_requested: HashSet::new(),
            thumb_jobs,
            thumb_results,
            events,
            hotkeys,
            hotkey_map,
            registered: Vec::new(),
            recording_shortcut: None,
            menu_items,
            tray,
            _menu_bar: menu_bar,
            toast: hotkey_error.map(|e| (e, Instant::now())),
            saves: BackgroundSaves::default(),
            ocr_rx: None,
            ocr_text: None,
            show_settings: false,
            show_about: false,
            lib_selection: HashSet::new(),
            lib_anchor: None,
            overlay_to_present: None,
            reveal_in: None,
            reveal_behind: false,
            show_permissions: false,
            perm_status: Vec::new(),
            capture_zoom: None,
            min_size: Vec2::new(1300.0, 640.0),
            perm_polled: None,
            perm_opened: false,
            hover_tracking: false,
            new_canvas: NewCanvas::default(),
            show_new_dialog: false,
            settings_tab: SettingsTab::General,
            update: None,
            update_rx: None,
            update_manual: false,
            // A few seconds after launch, so starting up isn't slowed.
            update_due: system.then(|| SystemTime::now() + Duration::from_secs(5)),
            installing: None,
            ready_update: None,
            install_target: None,
        }
    }

    /// (Re)register every global shortcut from the settings. Returns the
    /// labels of shortcuts the OS refused.
    fn apply_hotkeys(&mut self) -> Vec<String> {
        self.suspend_hotkeys();
        let Some(manager) = &self.hotkeys else { return Vec::new() };
        let mut map = HashMap::new();
        let mut failed = Vec::new();
        for action in Action::ALL {
            let Some(hk) = self.settings.shortcuts.hotkey(action) else { continue };
            match manager.register(hk) {
                Ok(()) => {
                    map.insert(hk.id(), action.event());
                    self.registered.push(hk);
                }
                Err(_) => failed.push(format!("{} ({})", action.label(), shortcuts::display(hk))),
            }
        }
        if let Ok(mut m) = self.hotkey_map.lock() {
            *m = map;
        }
        for (action, item) in &self.menu_items {
            item.set_text(menu_label(*action, &self.settings.shortcuts));
        }
        failed
    }

    /// Release all global shortcuts (while the user records a new one).
    fn suspend_hotkeys(&mut self) {
        if let Some(manager) = &self.hotkeys {
            let _ = manager.unregister_all(&self.registered);
        }
        self.registered.clear();
        if let Ok(mut m) = self.hotkey_map.lock() {
            m.clear();
        }
    }

    /// Display form of an action's shortcut, e.g. `⌃⌥C`.
    fn keys(&self, action: Action) -> String {
        self.settings.shortcuts.display(action)
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Capture geometry (overlay window, selection, recording region) is in screen
    /// points, which equal egui points only at zoom 1. Hold the zoom there, and keep
    /// ⌘+/⌘− from changing it, while a capture is under way; restore it afterwards.
    fn sync_capture_zoom(&mut self, ctx: &egui::Context) {
        let capturing = matches!(
            self.activity,
            Activity::Pending { .. } | Activity::Selecting(_) | Activity::Recording(_) | Activity::Scrolling(_) | Activity::Guiding(_)
        );
        match (capturing, self.capture_zoom) {
            (true, None) => {
                self.capture_zoom = Some(ctx.zoom_factor());
                ctx.set_zoom_factor(1.0);
                ctx.options_mut(|o| o.zoom_with_keyboard = false);
            }
            (false, Some(zoom)) => {
                self.capture_zoom = None;
                ctx.set_zoom_factor(zoom);
                ctx.options_mut(|o| o.zoom_with_keyboard = true);
            }
            _ => {}
        }
    }

    fn refresh_library(&mut self) {
        self.library = library::scan(&self.settings.save_dir);
        self.library_scanned = Instant::now();
        // Drop thumbnails of deleted files and of superseded versions (each autosave
        // changes the modified time), so the caches don't grow for the app's lifetime.
        let live: HashMap<&PathBuf, SystemTime> = self.library.iter().map(|i| (&i.path, i.modified)).collect();
        self.thumbs.retain(|p, _| live.contains_key(p));
        self.thumbs_requested.retain(|(p, m)| live.get(p) == Some(m));
    }

    fn set_tray_title(&self, title: Option<&str>) {
        if let Some(tray) = &self.tray {
            tray.set_title(title);
        }
    }

    // ---------- window states ----------

    fn hide_window(&mut self, ctx: &egui::Context) {
        if self.win == Win::Normal {
            self.normal_outer = ctx.input(|i| i.viewport().outer_rect);
            self.normal_inner = ctx.input(|i| i.viewport().inner_rect);
        }
        // A primed window (see `platform::prime_hidden_window`) is as good as
        // hidden already, and must stay on screen to run without delay.
        if !(self.win == Win::Hidden && platform::is_primed()) {
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
        platform::set_dock_icon(false);
        self.win = Win::Hidden;
    }

    fn show_normal(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        self.show_window(ctx, frame, true);
    }

    /// Show the main window (and the Dock icon). Without `focus` it stays behind
    /// the app the user is working in and Rega isn't activated.
    fn show_window(&mut self, ctx: &egui::Context, frame: &eframe::Frame, focus: bool) {
        self.reveal_behind = !focus;
        platform::set_dock_icon(true);
        platform::set_overlay_level(ctx, frame, false);
        if self.win != Win::Normal {
            // Coming from the overlay or recording bar, the window is resized and
            // re-decorated; hide it until it has painted the normal UI at its new size.
            platform::set_window_alpha(frame, 0.0);
            self.hide_until_painted(ctx, 2);
            ctx.send_viewport_cmd(ViewportCommand::ContentProtected(false));
            ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(ViewportCommand::Resizable(true));
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(self.min_size));
            let size = self.normal_inner.map(|r| r.size()).unwrap_or(Vec2::new(1360.0, 840.0));
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
            if let Some(outer) = self.normal_outer {
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(outer.min));
            }
        }
        if !focus {
            // Showing orders the window to the front; hide it until it can be sent back.
            platform::set_window_alpha(frame, 0.0);
            self.hide_until_painted(ctx, 2);
        }
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        if focus {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        self.win = Win::Normal;
        self.hidden_by_user = false;
    }

    /// A capture finished: bring Rega to the front on it, or (with "Focus Rega
    /// when done" off) show it in the background, with its Dock icon, behind the
    /// app the user is working in.
    fn after_capture(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let focus = self.settings.focus_after_capture;
        self.show_window(ctx, frame, focus);
        if focus {
            platform::activate_app();
        }
    }

    /// After a flow that produced nothing to show, go back to where the user was.
    fn return_to_rest(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.hidden_by_user {
            platform::set_overlay_level(ctx, frame, false);
            self.hide_window(ctx);
            self.hidden_by_user = true;
        } else {
            self.show_normal(ctx, frame);
        }
    }

    fn show_overlay(&mut self, ctx: &egui::Context, frame: &eframe::Frame, origin: (i32, i32), size: (u32, u32)) {
        ctx.send_viewport_cmd(ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
        // The main window's minimum size would stretch the overlay on small
        // (or highly scaled) monitors and misalign it with the cursor.
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::ZERO));
        // Size before position: macOS resizes around the bottom-left corner.
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(size.0 as f32, size.1 as f32)));
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(Pos2::new(origin.0 as f32, origin.1 as f32)));
        if cfg!(target_os = "macos") {
            // Shown natively on the next pass, once winit has applied the style
            // changes above (they would reset the panel style), see `present_overlay`.
            self.overlay_to_present = Some((origin, size));
            ctx.request_repaint();
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            platform::set_overlay_level(ctx, frame, true);
        }
        self.win = Win::Overlay;
    }

    /// macOS: show the overlay as a non-activating panel on the current Space
    /// (over full-screen apps too) instead of activating Rega.
    fn present_overlay(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        self.tick_reveal(ctx, frame);
        let Some((origin, size)) = self.overlay_to_present.take() else { return };
        platform::set_window_frame(frame, origin.0 as f32, origin.1 as f32, size.0 as f32, size.1 as f32);
        // Comes up transparent (see `platform::present_overlay`). The screenshot
        // matches the real screen, so the transparent moment is invisible.
        platform::present_overlay(frame);
        self.hide_until_painted(ctx, 1);
    }

    /// Keep the (already transparent) window hidden for `passes` more painted
    /// passes, so it reappears with fresh content rather than its stale last
    /// frame stretched to a new size, which flashes.
    fn hide_until_painted(&mut self, ctx: &egui::Context, passes: u8) {
        self.reveal_in = Some(passes);
        ctx.request_repaint();
    }

    fn tick_reveal(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        match self.reveal_in {
            Some(0) => {
                // Ordered back while still transparent, so it never flashes in front.
                if std::mem::take(&mut self.reveal_behind) {
                    platform::send_window_to_back(frame);
                }
                platform::set_window_alpha(frame, 1.0);
                self.reveal_in = None;
            }
            Some(n) => {
                self.reveal_in = Some(n - 1);
                ctx.request_repaint();
            }
            None => {}
        }
    }

    /// Show the bar with the recording's controls next to `region`; returns
    /// where it is, in global points.
    fn show_recording_bar(&mut self, ctx: &egui::Context, frame: &eframe::Frame, snap_origin: (i32, i32), monitor: (u32, u32), region: Rect) -> Rect {
        let (mw, mh) = (monitor.0 as f32, monitor.1 as f32);
        let gap = 12.0;
        let y = if region.max.y + gap + BAR_SIZE.y <= mh {
            region.max.y + gap
        } else if region.min.y - gap - BAR_SIZE.y >= 0.0 {
            region.min.y - gap - BAR_SIZE.y
        } else {
            mh - BAR_SIZE.y - 40.0
        };
        let x = (region.center().x - BAR_SIZE.x / 2.0).clamp(0.0, (mw - BAR_SIZE.x).max(0.0));
        let pos = Pos2::new(snap_origin.0 as f32 + x, snap_origin.1 as f32 + y);
        // The full-screen overlay shrinks into the bar; don't flash its stale frame.
        platform::set_window_alpha(frame, 0.0);
        self.hide_until_painted(ctx, 2);
        ctx.send_viewport_cmd(ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::ZERO));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(BAR_SIZE));
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos));
        // Keep the bar itself out of the recording.
        ctx.send_viewport_cmd(ViewportCommand::ContentProtected(true));
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        platform::set_overlay_level(ctx, frame, true);
        self.win = Win::Bar;
        Rect::from_min_size(pos, BAR_SIZE)
    }

    // ---------- capture flows ----------

    fn handle_event(&mut self, ev: AppEvent, ctx: &egui::Context, frame: &eframe::Frame) {
        match ev {
            AppEvent::Capture(p) => self.start_capture(ctx, p, false),
            AppEvent::FullScreen => self.start_capture(ctx, Purpose::Image, true),
            AppEvent::ShowMain => self.show_normal(ctx, frame),
            AppEvent::About => {
                self.show_normal(ctx, frame);
                self.show_about = true;
            }
            AppEvent::NewImage => {
                self.show_normal(ctx, frame);
                self.show_new_dialog = true;
            }
            AppEvent::OpenImage => {
                self.show_normal(ctx, frame);
                self.import_from_dialog();
            }
            AppEvent::CloseItem => self.close_current(ctx),
            AppEvent::DeleteItem => self.delete_current(),
            AppEvent::Quit => self.quit(ctx),
        }
    }

    fn quit(&mut self, ctx: &egui::Context) -> ! {
        self.prepare_to_quit();
        ctx.send_viewport_cmd(ViewportCommand::Close);
        std::process::exit(0);
    }

    /// Leave nothing half-written on quitting: save the open image, finish a
    /// recording, and stop the copies still being saved.
    fn prepare_to_quit(&mut self) {
        self.autosave_editor();
        if let Activity::Recording(rec) | Activity::Finishing(rec) = &mut self.activity {
            // Exiting mid-write would leave an unplayable video with no index.
            if rec.finish_blocking(Duration::from_secs(30)).is_none() {
                log::warn!("quit before the recording finished saving");
            }
        }
        // A guide being recorded is kept, as Stop would keep it.
        let steps = match std::mem::replace(&mut self.activity, Activity::Idle) {
            Activity::Guiding(rec) => Some(rec.finish()),
            Activity::GuideFinished(steps) => steps,
            other => {
                self.activity = other;
                None
            }
        };
        if let Some(steps) = steps.filter(|s| !s.is_empty())
            && let Some(mut ed) = self.new_guide(steps)
            && let Err(e) = ed.save_now()
        {
            log::warn!("saving the guide on quit: {e:#}");
        }
        // A copy could take minutes to finish; cancelled, it removes its
        // partial file. Closing the player hands its save over with the rest.
        self.mode = Mode::Home;
        crate::player::cancel_background_saves(&self.saves, Duration::from_secs(5));
    }

    fn start_capture(&mut self, ctx: &egui::Context, purpose: Purpose, full: bool) {
        match &self.activity {
            Activity::Idle => {}
            Activity::Recording(_) if purpose == Purpose::Video => {
                self.stop_recording(ctx);
                return;
            }
            Activity::Guiding(_) if purpose == Purpose::Guide => {
                self.stop_guide(ctx, false);
                return;
            }
            _ => return,
        }
        if !platform::is_granted(platform::Permission::ScreenRecording) {
            // Capturing now would only produce an empty desktop.
            self.refresh_permissions(true);
            self.show_permissions = true;
            platform::set_dock_icon(true);
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            self.win = Win::Normal;
            self.hidden_by_user = false;
            self.toast("Rega needs Screen Recording permission to capture");
            return;
        }
        if purpose == Purpose::Guide && !platform::is_granted(platform::Permission::Accessibility) {
            // Without it the guide would never see a click.
            platform::request(platform::Permission::Accessibility);
            self.refresh_permissions(true);
            self.show_permissions = true;
            platform::set_dock_icon(true);
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            self.win = Win::Normal;
            self.hidden_by_user = false;
            self.toast("Rega needs Accessibility permission to see your clicks and typing for a guide");
            return;
        }
        if let Mode::Editor(ed) = &mut self.mode {
            ed.commit_pending();
        }
        let was_hidden = self.win == Win::Hidden;
        // Started from a hotkey or the tray (our window isn't focused): cancelling
        // should leave the user where they were, not pop the app up.
        let from_outside = !ctx.input(|i| i.focused);
        // Windows fades a hiding window out over a few hundred ms (longer in a
        // VM); keep it out of the screenshot no matter how long that takes.
        // `show_normal` turns this off again.
        ctx.send_viewport_cmd(ViewportCommand::ContentProtected(true));
        self.hide_window(ctx);
        self.hidden_by_user = was_hidden || self.hidden_by_user || from_outside;
        // Only wait for our own window to leave the screen if it was on it.
        let settle = if was_hidden {
            Duration::ZERO
        } else if cfg!(target_os = "windows") {
            Duration::from_millis(450)
        } else {
            Duration::from_millis(120)
        };
        // The capture delay is for setting a screen up; a colour is picked at once.
        let wait = if purpose == Purpose::Color { 0 } else { self.settings.delay_secs as u64 };
        let delay = Duration::from_secs(wait).max(settle);
        self.activity = Activity::Pending {
            purpose,
            full,
            at: Instant::now() + delay,
            cursor: capture::cursor_position(),
        };
        ctx.request_repaint();
    }

    fn advance_activity(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        match &mut self.activity {
            Activity::Pending { purpose, full, at, cursor } => {
                let (purpose, full, at, cursor) = (*purpose, *full, *at, *cursor);
                let now = Instant::now();
                if now < at {
                    let left = at - now;
                    if self.settings.delay_secs > 0 && purpose != Purpose::Color {
                        let secs = left.as_secs_f32().ceil() as u32;
                        self.set_tray_title(Some(&secs.to_string()));
                    }
                    ctx.request_repaint_after(left.min(Duration::from_millis(100)));
                    return;
                }
                self.set_tray_title(None);
                let screens = capture::screens();
                match capture::snapshot_at(cursor) {
                    Err(e) => {
                        self.activity = Activity::Idle;
                        self.show_normal(ctx, frame);
                        self.toast(format!("Capture failed: {e:#}"));
                    }
                    Ok(snap) if full && screens.len() < 2 => {
                        self.activity = Activity::Idle;
                        self.finish_image(ctx, frame, snap.image);
                    }
                    Ok(snap) => {
                        if purpose == Purpose::Video && self.settings.video_format == VideoFormat::Mp4 {
                            // The codec starts up while the user picks the region,
                            // not on the recording's first frames.
                            crate::h264::warm_up();
                        }
                        let (origin, size) = (snap.origin, snap.size);
                        let mut overlay = Overlay::new(ctx, snap, purpose);
                        if screens.len() > 1 {
                            // The others are frozen in the background; the overlay
                            // moves to whichever screen the pointer goes to.
                            let others = capture::snapshot_others(&screens, origin, cursor);
                            overlay = overlay.with_screens(screens, others);
                            if full {
                                overlay = overlay.picking_screen();
                            }
                        }
                        self.activity = Activity::Selecting(Box::new(overlay));
                        self.show_overlay(ctx, frame, origin, size);
                    }
                }
            }
            Activity::Recording(rec) => {
                if rec.has_ended() {
                    // It stopped by itself (no codec, a full disk, the screen
                    // gone): end it as Stop would and say how it went now.
                    self.stop_recording(ctx);
                    self.advance_activity(ctx, frame);
                    return;
                }
                let secs = rec.elapsed().as_secs();
                let title = format!("● {}:{:02}", secs / 60, secs % 60);
                self.set_tray_title(Some(&title));
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Activity::Finishing(rec) => match rec.poll_finished() {
                None => ctx.request_repaint_after(Duration::from_millis(100)),
                Some(result) => {
                    self.activity = Activity::Idle;
                    self.set_tray_title(None);
                    match result {
                        Ok(Finished::Saved { path, stopped_early }) => {
                            self.refresh_library();
                            if self.settings.focus_after_capture {
                                // Open Rega on the new video.
                                self.lib_filter = LibFilter::All;
                                self.open_video(path.clone());
                            }
                            self.after_capture(ctx, frame);
                            self.toast(match stopped_early {
                                Some(reason) => format!("Video saved, but the recording stopped early: {reason}"),
                                None => format!("Video saved: {}", path.file_name().unwrap_or_default().to_string_lossy()),
                            });
                        }
                        Ok(Finished::Discarded) => {
                            self.return_to_rest(ctx, frame);
                            self.toast("Recording discarded");
                        }
                        Err(e) => {
                            // Shown even when it was started from a shortcut or
                            // the tray: nothing else would say the video is lost.
                            self.show_normal(ctx, frame);
                            self.toast(format!("Recording failed: {e:#}"));
                        }
                    }
                }
            },
            Activity::Scrolling(rx) => match rx.try_recv() {
                Ok(ScrollMsg::Progress(msg)) => {
                    log::debug!("{msg}");
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
                Ok(ScrollMsg::Done(result)) => {
                    self.activity = Activity::Idle;
                    match result {
                        Ok(img) => self.finish_image(ctx, frame, img),
                        Err(e) => {
                            self.show_normal(ctx, frame);
                            self.toast(format!("Scrolling capture failed: {e:#}"));
                        }
                    }
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => {
                    self.activity = Activity::Idle;
                    self.show_normal(ctx, frame);
                }
            },
            Activity::Guiding(rec) => {
                let n = rec.step_count();
                self.set_tray_title(Some(&format!("● {n}")));
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Activity::GuideFinished(_) => {
                let Activity::GuideFinished(steps) = std::mem::replace(&mut self.activity, Activity::Idle) else { return };
                self.set_tray_title(None);
                match steps {
                    None => {
                        self.return_to_rest(ctx, frame);
                        self.toast("Guide discarded");
                    }
                    Some(steps) if steps.is_empty() => {
                        self.return_to_rest(ctx, frame);
                        self.toast("No steps were recorded — click inside the region while recording a guide");
                    }
                    Some(steps) => {
                        let n = steps.len();
                        match self.new_guide(steps) {
                            Some(mut ed) => {
                                ed.save_in_background(ctx);
                                self.autosave_editor();
                                self.lib_filter = LibFilter::All;
                                self.mode = Mode::Guide(Box::new(ed));
                                // A guide always opens: it's made to be edited before it's shared.
                                self.show_normal(ctx, frame);
                                platform::activate_app();
                                self.toast(format!("Guide recorded: {n} steps"));
                            }
                            None => self.show_normal(ctx, frame),
                        }
                    }
                }
            }
            Activity::Idle | Activity::Selecting(_) => {}
        }
    }

    fn on_selection(&mut self, ctx: &egui::Context, frame: &eframe::Frame, overlay: &Overlay, rect: Rect) {
        let snap = &overlay.snap;
        match overlay.purpose {
            // Picking a colour ends with `Outcome::Color`, not a selection.
            Purpose::Color => self.return_to_rest(ctx, frame),
            Purpose::Image => {
                let img = snap.crop(rect.min.x, rect.min.y, rect.width(), rect.height());
                self.finish_image(ctx, frame, img);
            }
            Purpose::Text => {
                let img = snap.crop(rect.min.x, rect.min.y, rect.width(), rect.height());
                self.show_normal(ctx, frame);
                self.start_ocr(ctx, img);
            }
            Purpose::Video => {
                let region = (
                    rect.min.x.max(0.0) as u32,
                    rect.min.y.max(0.0) as u32,
                    (rect.width() as u32).max(16),
                    (rect.height() as u32).max(16),
                );
                let region = (
                    region.0,
                    region.1,
                    region.2.min(snap.size.0 - region.0),
                    region.3.min(snap.size.1 - region.1),
                );
                let fmt = self.settings.video_format;
                // GIF recordings use the GIF quality preset's caps (smaller files).
                let gif = self.settings.gif_quality;
                let is_gif = fmt == VideoFormat::Gif;
                let cfg = RecordConfig {
                    monitor_origin: snap.origin,
                    region,
                    fps: if is_gif { self.settings.video_fps.min(gif.fps()) } else { self.settings.video_fps },
                    format: fmt,
                    max_width: if is_gif { self.settings.video_max_width.min(gif.max_width()) } else { self.settings.video_max_width },
                    gif_colors: gif.colors(),
                    highlight_cursor: self.settings.highlight_cursor,
                    out_path: library::new_capture_path(&self.settings.save_dir, fmt.ext()),
                };
                match Recorder::start(cfg) {
                    Ok(rec) => {
                        self.activity = Activity::Recording(rec);
                        let (x, y) = (snap.origin.0 as f32 + region.0 as f32, snap.origin.1 as f32 + region.1 as f32);
                        // The border's gradient carries on from the overlay's outline.
                        let phase = overlay.border_phase(ctx.input(|i| i.time));
                        platform::show_recording_frame((x, y, region.2 as f32, region.3 as f32), phase);
                        self.show_recording_bar(ctx, frame, snap.origin, snap.size, rect);
                    }
                    Err(e) => {
                        self.show_normal(ctx, frame);
                        self.toast(format!("Recording failed: {e:#}"));
                    }
                }
            }
            Purpose::Guide => {
                let region = (
                    rect.min.x.max(0.0) as u32,
                    rect.min.y.max(0.0) as u32,
                    (rect.width() as u32).max(16).min(snap.size.0),
                    (rect.height() as u32).max(16).min(snap.size.1),
                );
                let cfg = GuideConfig {
                    monitor_origin: snap.origin,
                    region,
                    ignore_shortcuts: Action::ALL.iter().map(|a| self.settings.shortcuts.get(*a).to_owned()).collect(),
                    max_side: self.settings.guide.style.quality.max_side(),
                    record_typing: self.settings.guide.record_typing,
                    typed_text: self.settings.guide.typed_text_in_captions,
                    record_keys: self.settings.guide.record_keys,
                };
                match GuideRecorder::start(cfg) {
                    Ok(rec) => {
                        let (x, y) = (snap.origin.0 as f32 + region.0 as f32, snap.origin.1 as f32 + region.1 as f32);
                        let phase = overlay.border_phase(ctx.input(|i| i.time));
                        platform::show_recording_frame((x, y, region.2 as f32, region.3 as f32), phase);
                        let bar = self.show_recording_bar(ctx, frame, snap.origin, snap.size, rect);
                        rec.set_exclude([bar.min.x, bar.min.y, bar.width(), bar.height()]);
                        self.activity = Activity::Guiding(Box::new(rec));
                    }
                    Err(e) => {
                        self.show_normal(ctx, frame);
                        self.toast(format!("Couldn't record a guide: {e:#}"));
                    }
                }
            }
            Purpose::Scroll => {
                platform::set_overlay_level(ctx, frame, false);
                self.hide_window(ctx);
                let origin = snap.origin;
                let region = (
                    rect.min.x.max(0.0) as u32,
                    rect.min.y.max(0.0) as u32,
                    rect.width().max(16.0) as u32,
                    rect.height().max(16.0) as u32,
                );
                let (tx, rx) = mpsc::channel();
                let repaint = ctx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(350));
                    let result = scroll::run(origin, region, &tx);
                    let _ = tx.send(ScrollMsg::Done(result));
                    repaint.request_repaint();
                });
                self.activity = Activity::Scrolling(rx);
            }
        }
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        if let Activity::Recording(rec) = std::mem::replace(&mut self.activity, Activity::Idle) {
            rec.stop();
            platform::hide_recording_frame();
            // The bar has nothing left to show; without this it lingers as a black box.
            self.hide_window(ctx);
            self.activity = Activity::Finishing(rec);
            self.toast("Saving video…");
            ctx.request_repaint();
        }
    }

    /// End a guide recording; `advance_activity` opens it (or says it was
    /// discarded) once the window can be shown.
    fn stop_guide(&mut self, ctx: &egui::Context, discard: bool) {
        if let Activity::Guiding(rec) = std::mem::replace(&mut self.activity, Activity::Idle) {
            platform::hide_recording_frame();
            self.hide_window(ctx);
            // Waits for the last screenshots to be compressed: a moment at most.
            let steps = rec.finish();
            self.activity = Activity::GuideFinished((!discard).then_some(steps));
            ctx.request_repaint();
        }
    }

    /// An editor for a newly recorded guide, saved under a new library name.
    fn new_guide(&mut self, steps: Vec<crate::guide::Step>) -> Option<GuideEditor> {
        let path = library::new_capture_path(&self.settings.save_dir, "html");
        let mut guide = crate::guide::Guide::new("Untitled guide".to_owned(), steps);
        guide.style = self.settings.guide.style;
        Some(GuideEditor::new(&self.ctx, path, guide))
    }

    /// A new image capture: save it to the library, copy it, open the editor.
    fn finish_image(&mut self, ctx: &egui::Context, frame: &eframe::Frame, img: RgbaImage) {
        let ext = self.settings.image_format.ext();
        let path = library::new_capture_path(&self.settings.save_dir, ext);
        if self.settings.copy_after_capture
            && let Err(e) = platform::copy_image(&img) {
                log::warn!("clipboard: {e}");
            }
        let mut ed = Editor::new(path, img, None, false);
        if let Err(e) = ed.save() {
            self.show_normal(ctx, frame);
            self.toast(format!("Could not save capture: {e:#}"));
            return;
        }
        self.refresh_library();
        // Focusing means showing the new capture, so it opens in the editor either way.
        if self.settings.open_editor_after_capture || self.settings.focus_after_capture {
            self.show_editor(ed);
        }
        self.after_capture(ctx, frame);
        let copied = if self.settings.copy_after_capture { " and copied" } else { "" };
        self.toast(format!("Captured{copied}"));
    }

    fn start_ocr(&mut self, ctx: &egui::Context, img: RgbaImage) {
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(ocr::recognize(&img));
            repaint.request_repaint();
        });
        self.ocr_rx = Some(rx);
        self.toast("Recognizing text…");
    }

    /// Show `ed` in the workspace, after saving whatever was open; it starts
    /// with the tool the user picked last.
    fn show_editor(&mut self, mut ed: Editor) {
        self.autosave_editor();
        if let Some(tool) = self.settings.editor_tool.as_deref().and_then(Tool::from_key) {
            ed.start_with(tool);
        }
        self.mode = Mode::Editor(Box::new(ed));
    }

    fn autosave_editor(&mut self) {
        match &mut self.mode {
            Mode::Editor(ed) if ed.is_dirty() => {
                if let Err(e) = ed.save() {
                    log::warn!("autosave failed: {e}");
                }
            }
            Mode::Guide(ed) => {
                if let Err(e) = ed.save_now() {
                    log::warn!("saving the guide failed: {e:#}");
                }
            }
            _ => {}
        }
    }

    fn open_item(&mut self, item: &Item) {
        match item.kind {
            ItemKind::Video => self.open_video(item.path.clone()),
            ItemKind::Guide => match GuideEditor::open(&self.ctx, item.path.clone()) {
                Ok(ed) => {
                    self.autosave_editor();
                    self.mode = Mode::Guide(Box::new(ed));
                }
                Err(e) => {
                    log::warn!("opening the guide: {e:#}");
                    platform::open_path(&item.path);
                }
            },
            ItemKind::Image => match Editor::open(item.path.clone()) {
                Ok(ed) => {
                    self.show_editor(ed);
                }
                Err(e) => self.toast(format!("{e:#}")),
            },
        }
    }

    /// Preview a video inside Rega; fall back to the system player.
    fn open_video(&mut self, path: PathBuf) {
        self.autosave_editor();
        match Player::open(&self.ctx, path.clone()) {
            Ok(mut player) => {
                // A save still running when it closes goes on; the app reports it.
                player.hand_saves_to(self.saves.clone());
                self.mode = Mode::Video(Box::new(player));
            }
            Err(e) => {
                log::warn!("in-app preview failed: {e:#}");
                platform::open_path(&path);
            }
        }
    }

    /// Say how the saves that outlived their player went. A new copy is
    /// listed but not opened: the user has moved on.
    fn report_background_saves(&mut self) {
        for action in crate::player::poll_background_saves(&self.saves) {
            match action {
                PlayerAction::None => {}
                PlayerAction::Saved(path) => {
                    self.refresh_library();
                    self.toast(format!("Saved {}", path.file_name().unwrap_or_default().to_string_lossy()));
                }
                PlayerAction::Failed(msg) => self.toast(msg),
            }
        }
    }

    /// Each image becomes its own library item; the last one opens.
    fn import_images(&mut self, imgs: Vec<RgbaImage>) {
        let n = imgs.len();
        for img in imgs {
            self.import_image(img);
        }
        if n > 1 {
            self.toast(format!("{n} images added to the library"));
        }
    }

    fn import_image(&mut self, img: RgbaImage) {
        let path = library::new_capture_path(&self.settings.save_dir, self.settings.image_format.ext());
        let mut ed = Editor::new(path, img, None, false);
        match ed.save() {
            Ok(()) => {
                self.show_editor(ed);
                self.refresh_library();
            }
            Err(e) => self.toast(format!("Import failed: {e:#}")),
        }
    }

    /// Start a blank canvas in the editor (saved to the library right away).
    fn create_blank(&mut self) {
        let NewCanvas { width, height, background, .. } = self.new_canvas;
        let img = RgbaImage::from_pixel(width.max(1), height.max(1), image::Rgba(background.rgba()));
        // A transparent canvas must stay PNG even if the default format is JPEG.
        let ext = if background == Background::Transparent { "png" } else { self.settings.image_format.ext() };
        let path = library::new_capture_path(&self.settings.save_dir, ext);
        let mut ed = Editor::new(path, img, None, false);
        match ed.save() {
            Ok(()) => {
                self.show_editor(ed);
                self.refresh_library();
                self.toast(format!("New canvas — press {}V to paste an image onto it", platform::CMD));
            }
            Err(e) => self.toast(format!("Couldn't create the canvas: {e:#}")),
        }
    }

    /// The File menu's shortcuts — ⌘N, ⌘O, ⌘W (close the open item), ⌘D
    /// (delete it) and on macOS ⌘Q — plus ⌘V on the welcome screen.
    fn home_shortcuts(&mut self, ctx: &egui::Context) {
        // On macOS the menu catches these first, except with a layout that
        // types something else there (Hebrew has ' on W and / on Q, see
        // `latin_shortcuts`); this covers that, and Windows, which has no menu.
        let cmd = egui::Modifiers::COMMAND;
        let [new, open, close, delete, quit] =
            ctx.input_mut(|i| [egui::Key::N, egui::Key::O, egui::Key::W, egui::Key::D, egui::Key::Q].map(|k| i.consume_key(cmd, k)));
        if quit && cfg!(target_os = "macos") {
            self.quit(ctx);
        }
        if new {
            self.show_new_dialog = true;
        }
        if open {
            self.import_from_dialog();
        }
        if delete {
            self.delete_current();
        } else if close {
            self.close_current(ctx);
        }
        if matches!(self.mode, Mode::Home) && ctx.memory(|m| m.focused().is_none()) {
            let pasted = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::PasteImage(img) => Some(platform::color_image_to_rgba(img)),
                    _ => None,
                })
            });
            if let Some(img) = pasted {
                // Files copied in Finder arrive as an icon; open the files instead.
                let mut imgs = platform::clipboard_file_images();
                if imgs.is_empty() {
                    imgs.push(img);
                }
                self.import_images(imgs);
            } else if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))) {
                self.import_from_clipboard();
            }
        }
    }

    /// Size and background picker; returns true when "Create" was pressed.
    fn new_canvas_form(&mut self, ui: &mut egui::Ui) -> bool {
        let mut create = false;
        ui.set_width(260.0);
        theme::section(ui, "Size");
        let nc = &mut self.new_canvas;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (w, h, name) in [(1280, 800, "Laptop"), (1920, 1080, "Full HD"), (1080, 1080, "Square"), (800, 600, "Small")] {
                if size_row(ui, nc.width == w && nc.height == h, name, &format!("{w} × {h}")).clicked() {
                    nc.width = w;
                    nc.height = h;
                    // A kept ratio becomes the chosen size's.
                    if nc.ratio.is_some() {
                        nc.ratio = Some((w, h));
                    }
                }
            }
        });
        theme::section(ui, "Custom size");
        custom_size(ui, nc);
        theme::section(ui, "Background");
        theme::segmented(
            ui,
            &mut nc.background,
            &[(Background::White, "White"), (Background::Transparent, "Transparent"), (Background::Dark, "Dark")],
        );
        ui.add_space(10.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::primary_button(ui, Some(icon::NEW), "Create").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                create = true;
            }
        });
        create
    }

    /// The New form as a dropdown under a button.
    fn new_canvas_popup(&mut self, anchor: &egui::Response) {
        let mut create = false;
        egui::Popup::menu(anchor).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            if self.new_canvas_form(ui) {
                create = true;
                ui.close();
            }
        });
        if create {
            self.create_blank();
        }
    }

    /// The New form as a centered dialog (⌘N).
    fn new_canvas_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_new_dialog {
            return;
        }
        let mut create = false;
        let modal = egui::Modal::new(egui::Id::unique("new-canvas")).show(ctx, |ui| {
            ui.label(egui::RichText::new("New image").font(theme::bold(17.0)).color(theme::TEXT));
            ui.add_space(4.0);
            create = self.new_canvas_form(ui);
        });
        if create {
            self.show_new_dialog = false;
            self.create_blank();
        } else if modal.should_close() {
            self.show_new_dialog = false;
        }
    }

    // ---------- UI ----------

    /// The main window: top bar, canvas + tools, and the library tray along the bottom.
    fn workspace_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut actions = Vec::new();
        let mut guide_actions = Vec::new();

        egui::Panel::top("topbar")
            .frame(theme::panel_frame(theme::SURFACE, egui::Margin { left: 16, right: 14, top: 8, bottom: 8 }))
            .show(ui, |ui| {
                theme::toolbar_row(ui, |ui| {
                    theme::toolbar_brand(ui, APP_NAME);
                    ui.add_space(18.0);

                    theme::toolbar_group(ui, |ui| {
                        for (glyph, label, action, purpose, full) in [
                            (icon::REGION, "Capture", Action::Image, Purpose::Image, false),
                            (icon::FULLSCREEN, "Screen", Action::FullScreen, Purpose::Image, true),
                            (icon::VIDEO, "Video", Action::Video, Purpose::Video, false),
                            (icon::SCROLL, "Scrolling", Action::Scroll, Purpose::Scroll, false),
                            (icon::GUIDE, "Guide", Action::Guide, Purpose::Guide, false),
                            (icon::OCR, "Text", Action::Text, Purpose::Text, false),
                            (icon::EYEDROPPER, "Color", Action::Color, Purpose::Color, false),
                        ] {
                            let style = if action == Action::Image { ItemStyle::Primary } else { ItemStyle::Normal };
                            let tip = format!("{}   {}", action.label(), self.keys(action));
                            if theme::toolbar_item(ui, glyph, label, style, true).on_hover_text(tip).clicked() {
                                self.start_capture(&ctx, purpose, full);
                            }
                        }
                        let new = theme::toolbar_item(ui, icon::NEW, "New", ItemStyle::Normal, true)
                            .on_hover_text("Start from a blank canvas, then paste images onto it");
                        self.new_canvas_popup(&new);
                        theme::toolbar_divider(ui);
                        let delay_label = match self.settings.delay_secs {
                            0 => "No delay".to_owned(),
                            s => format!("{s}s delay"),
                        };
                        let delay = theme::toolbar_item(ui, icon::TIMER, &delay_label, ItemStyle::Normal, true)
                            .on_hover_text("Wait before capturing");
                        egui::Popup::menu(&delay).show(|ui| {
                            ui.set_min_width(150.0);
                            for (secs, label) in [(0, "No delay"), (3, "3 seconds"), (5, "5 seconds"), (10, "10 seconds")] {
                                if ui.selectable_label(self.settings.delay_secs == secs, label).clicked() {
                                    self.settings.delay_secs = secs;
                                    self.settings.save();
                                    ui.close();
                                }
                            }
                        });
                    });

                    // Right-align the editor actions + settings using last frame's width.
                    let width_id = egui::Id::unique("topbar-right-width");
                    let right_w = ui.ctx().data(|d| d.get_temp::<f32>(width_id)).unwrap_or(0.0);
                    ui.add_space((ui.available_width() - right_w).max(12.0));
                    let right = ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), theme::GROUP_HEIGHT),
                        egui::Layout::left_to_right(egui::Align::Min),
                        |ui| {
                            if let Mode::Editor(ed) = &mut self.mode {
                                actions.push(ed.top_actions(ui));
                                ui.add_space(6.0);
                            }
                            if let Mode::Guide(ed) = &mut self.mode {
                                guide_actions.push(ed.top_actions(ui));
                                ui.add_space(6.0);
                            }
                            theme::toolbar_group(ui, |ui| {
                                if theme::toolbar_item(ui, icon::SETTINGS, "Settings", ItemStyle::Normal, true).clicked() {
                                    self.show_settings = !self.show_settings;
                                }
                            });
                            // Width of what was actually laid out (not the space offered).
                            ui.min_rect().width()
                        },
                    );
                    let w = right.inner;
                    if (w - right_w).abs() > 0.5 {
                        ui.ctx().data_mut(|d| d.insert_temp(width_id, w));
                        ui.ctx().request_repaint();
                    }
                });
            });

        if self.missing_permissions() {
            egui::Panel::top("permission_banner")
                .frame(theme::panel_frame(Color32::from_rgb(52, 30, 38), egui::Margin::symmetric(16, 10)))
                .show(ui, |ui| self.permission_banner(ui));
        }
        if let Some(latest) = self.update_to_announce() {
            egui::Panel::top("update_banner")
                .frame(theme::panel_frame(Color32::from_rgb(33, 29, 56), egui::Margin::symmetric(16, 10)))
                .show(ui, |ui| self.update_banner(ui, &latest));
        }

        egui::Panel::bottom("library_tray")
            .resizable(false)
            .exact_size(168.0)
            .frame(theme::panel_frame(theme::SURFACE, egui::Margin { left: 16, right: 16, top: 10, bottom: 12 }))
            .show(ui, |ui| self.tray_ui(ui));

        let mut player_action = PlayerAction::None;
        match &mut self.mode {
            Mode::Editor(ed) => {
                egui::Panel::right("tools")
                    .resizable(false)
                    .exact_size(256.0)
                    .frame(theme::panel_frame(theme::SURFACE, egui::Margin::same(18)))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| ed.tools_panel(ui));
                    });
                egui::Panel::bottom("status")
                    .frame(theme::panel_frame(theme::BG, egui::Margin::symmetric(12, 4)))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ed.status_bar(ui);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let (w, h) = ed.size();
                                ui.label(egui::RichText::new(format!("{w} × {h}")).color(theme::TEXT_FAINT));
                                ui.label(egui::RichText::new(ed.file_name()).color(theme::TEXT_MUTED));
                            });
                        });
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::BG))
                    .show(ui, |ui| ed.canvas(ui));
                actions.push(ed.end_frame(&ctx));
                actions.push(ed.autosave(&ctx));
                // The tool picked becomes the one new captures open with (not
                // Crop: that's a one-off).
                let tool = ed.tool();
                if tool != Tool::Crop && self.settings.editor_tool.as_deref() != Some(tool.key().as_str()) {
                    self.settings.editor_tool = Some(tool.key());
                    self.settings.save();
                }
            }
            Mode::Guide(ed) => {
                egui::Panel::left("guide-steps")
                    .resizable(false)
                    .exact_size(250.0)
                    .frame(theme::panel_frame(theme::SURFACE, egui::Margin::same(12)))
                    .show(ui, |ui| guide_actions.push(ed.steps_panel(ui)));
                egui::Panel::right("guide-properties")
                    .resizable(false)
                    .exact_size(crate::guide::editor::PROPERTIES_WIDTH)
                    .frame(theme::panel_frame(theme::SURFACE, egui::Margin::same(crate::guide::editor::PROPERTIES_MARGIN)))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| guide_actions.push(ed.properties_panel(ui)));
                    });
                egui::Panel::bottom("status")
                    .frame(theme::panel_frame(theme::BG, egui::Margin::symmetric(12, 4)))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ed.status_bar(ui);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(egui::RichText::new(ed.file_name()).color(theme::TEXT_MUTED));
                            });
                        });
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::BG))
                    .show(ui, |ui| ed.canvas(ui));
                guide_actions.push(ed.end_frame(&ctx));
            }
            Mode::Video(player) => {
                egui::Panel::bottom("status")
                    .frame(theme::panel_frame(theme::BG, egui::Margin::symmetric(12, 4)))
                    .show(ui, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(player.file_name()).color(theme::TEXT_MUTED));
                        });
                    });
                player_action = egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::BG))
                    .show(ui, |ui| player.ui(ui))
                    .inner;
            }
            Mode::Home => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::BG))
                    .show(ui, |ui| self.welcome_ui(ui));
            }
        }

        match player_action {
            PlayerAction::None => {}
            PlayerAction::Saved(path) => {
                self.refresh_library();
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.open_video(path);
                self.toast(format!("Saved {name}"));
            }
            PlayerAction::Failed(msg) => self.toast(msg),
        }

        for action in actions {
            match action {
                EditorAction::None => {}
                EditorAction::Toast(msg) => {
                    self.refresh_library();
                    self.toast(msg);
                }
                EditorAction::Ocr(img) => self.start_ocr(&ctx, img),
                EditorAction::Saved => self.refresh_library(),
            }
        }
        for action in guide_actions {
            match action {
                GuideAction::None => {}
                GuideAction::Saved => self.refresh_library(),
                GuideAction::SetDefaultStyle(style) => {
                    self.settings.guide.style = style;
                    self.settings.save();
                    self.toast("New guides will look like this one");
                }
                GuideAction::Toast(msg) => {
                    self.refresh_library();
                    self.toast(msg);
                }
            }
        }
    }

    fn permission_banner(&mut self, ui: &mut egui::Ui) {
        let missing: Vec<&str> = self.perm_status.iter().filter(|(_, ok)| !ok).map(|(p, _)| p.title()).collect();
        let screen_missing = self.perm_status.iter().any(|(p, ok)| *p == platform::Permission::ScreenRecording && !ok);
        // Buttons first (right-aligned) so the text wraps in the space left over.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::primary_button(ui, Some(icon::SETTINGS), "Review permissions").clicked() {
                self.show_permissions = true;
            }
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(icon::INFO).font(theme::icon_font(22.0)).color(theme::CORAL));
                ui.vertical(|ui| {
                    let title = if missing.len() == 1 { "1 permission missing" } else { "Permissions missing" };
                    ui.label(egui::RichText::new(title).font(theme::medium(14.0)).color(theme::TEXT));
                    let detail = if screen_missing {
                        format!("{} — without Screen Recording, captures come out blank.", missing.join(", "))
                    } else {
                        format!("{} — some features won't work.", missing.join(", "))
                    };
                    ui.add(egui::Label::new(egui::RichText::new(detail).color(theme::TEXT_MUTED)).wrap());
                });
            });
        });
    }

    /// The newer version to tell the user about, unless they closed its notice.
    fn update_to_announce(&self) -> Option<update::Latest> {
        self.update.clone().filter(|l| self.settings.dismissed_update.as_deref() != Some(l.version.as_str()))
    }

    fn update_banner(&mut self, ui: &mut egui::Ui, latest: &update::Latest) {
        let in_place = self.can_update_in_place(latest);
        let installing = self.installing.as_ref().map(|(_, step)| *step);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match installing {
                Some(step) => {
                    let (text, share) = match step {
                        Installing::Downloading(share) => (format!("Downloading… {:.0}%", share * 100.0), Some(share)),
                        Installing::Checking => ("Checking it…".to_owned(), None),
                        Installing::Restarting => ("Restarting when the capture is done…".to_owned(), None),
                    };
                    if let Some(share) = share {
                        ui.add(egui::ProgressBar::new(share).desired_width(140.0));
                    } else {
                        ui.spinner();
                    }
                    ui.label(egui::RichText::new(text).color(theme::TEXT_MUTED));
                }
                None => {
                    if theme::ghost_button(ui, None, "Not now").clicked() {
                        self.settings.dismissed_update = Some(latest.version.clone());
                        self.settings.save();
                    }
                    if in_place {
                        if theme::primary_button(ui, Some(icon::IMPORT), "Update now")
                            .on_hover_text("Rega saves your work, installs the new version and restarts")
                            .clicked()
                        {
                            self.update_now(latest);
                        }
                    } else if theme::primary_button(ui, Some(icon::OPEN_EXTERNAL), "Download").clicked() {
                        platform::open_url(&update::download_url());
                    }
                }
            }
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(icon::INFO).font(theme::icon_font(22.0)).color(theme::ACCENT));
                ui.vertical(|ui| {
                    let title = format!("{APP_NAME} {} is available", latest.version);
                    ui.label(egui::RichText::new(title).font(theme::medium(14.0)).color(theme::TEXT));
                    let detail = if latest.title.is_empty() {
                        format!("You have {}.", env!("CARGO_PKG_VERSION"))
                    } else {
                        format!("{} You have {}.", latest.title, env!("CARGO_PKG_VERSION"))
                    };
                    ui.add(egui::Label::new(egui::RichText::new(detail).color(theme::TEXT_MUTED)).wrap());
                });
            });
        });
    }

    /// Whether `latest` can be installed over this copy, here and now.
    fn can_update_in_place(&mut self, latest: &update::Latest) -> bool {
        latest.installer().is_some() && self.install_target.get_or_insert_with(update::install_target).is_ok()
    }

    /// "Update now": download, check and install `latest` over this copy,
    /// then restart into it; where that can't be done, the download page.
    fn update_now(&mut self, latest: &update::Latest) {
        if self.installing.is_some() {
            return;
        }
        let target = self.install_target.get_or_insert_with(update::install_target).clone();
        let (Some(installer), Ok(target)) = (latest.installer().cloned(), target) else {
            platform::open_url(&update::download_url());
            return;
        };
        let ctx = self.ctx.clone();
        let rx = update::prepare(latest.version.clone(), installer, target, move || ctx.request_repaint());
        self.installing = Some((rx, Installing::Downloading(0.0)));
    }

    /// An update in place: follow its steps, and once it's ready and nothing
    /// is under way (a recording, a capture), quit into the new version.
    fn tick_install(&mut self, ctx: &egui::Context) {
        let mut finished = None;
        if let Some((rx, step)) = &mut self.installing {
            while let Ok(progress) = rx.try_recv() {
                match progress {
                    update::Progress::Downloading(share) => *step = Installing::Downloading(share),
                    update::Progress::Checking => *step = Installing::Checking,
                    other => finished = Some(other),
                }
            }
        }
        match finished {
            Some(update::Progress::Ready(prepared)) => {
                self.ready_update = Some(prepared);
                if let Some((_, step)) = &mut self.installing {
                    *step = Installing::Restarting;
                }
            }
            Some(update::Progress::Failed(e)) => self.update_failed(&e),
            _ => {}
        }
        if self.ready_update.is_some() && matches!(self.activity, Activity::Idle) {
            let prepared = self.ready_update.take().expect("checked");
            match update::apply(&prepared) {
                Ok(()) => {
                    log::info!("restarting into the new version");
                    self.prepare_to_quit();
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                    std::process::exit(0);
                }
                Err(e) => self.update_failed(&format!("{e:#}")),
            }
        }
    }

    fn update_failed(&mut self, why: &str) {
        log::warn!("updating in place: {why}");
        self.installing = None;
        self.ready_update = None;
        self.toast(format!("Couldn't update in place ({why}). Opening the download page"));
        platform::open_url(&update::download_url());
    }

    /// Start a check now; `manual` (from Settings) reports the outcome in a toast.
    fn check_for_update(&mut self, manual: bool) {
        if self.update_rx.is_some() {
            self.update_manual |= manual;
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(update::check());
            ctx.request_repaint();
        });
        self.update_rx = Some(rx);
        self.update_manual = manual;
    }

    /// Daily update check (when on), and its result once it arrives.
    fn tick_updates(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.update_rx {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.update_rx = None,
                Ok(result) => {
                    self.update_rx = None;
                    let manual = std::mem::take(&mut self.update_manual);
                    match result {
                        Ok(Some(latest)) => {
                            log::info!("{APP_NAME} {} is available", latest.version);
                            if manual {
                                // Asked for: show it even if an earlier notice was closed.
                                self.settings.dismissed_update = None;
                                self.settings.save();
                                self.toast(format!("{APP_NAME} {} is available", latest.version));
                            }
                            self.update = Some(latest);
                        }
                        Ok(None) => {
                            self.update = None;
                            if manual {
                                self.toast(format!("{APP_NAME} {} is the latest version", env!("CARGO_PKG_VERSION")));
                            }
                        }
                        Err(e) => {
                            log::info!("update check failed: {e}");
                            if manual {
                                self.toast(format!("Couldn't check for updates: {}", truncate(&e.to_string(), 60)));
                            }
                        }
                    }
                }
            }
        }
        let Some(due) = self.update_due else { return };
        if !self.settings.check_for_updates {
            return;
        }
        let now = SystemTime::now();
        match due.duration_since(now) {
            // Wake an idle Rega (closed to the menu bar for days) when the next
            // check is due. Asleep, this timer stops, but the first frame after
            // waking sees by the clock that the check is overdue.
            Ok(left) if !left.is_zero() => ctx.request_repaint_after(left),
            _ => {
                self.update_due = Some(now + update::INTERVAL);
                self.check_for_update(false);
            }
        }
    }

    /// Re-read permission status (at most once per ~0.7 s unless `force`).
    fn refresh_permissions(&mut self, force: bool) {
        let stale = self.perm_polled.is_none_or(|t| t.elapsed() > Duration::from_millis(700));
        if force || stale {
            self.perm_status = platform::permissions_needed().iter().map(|&p| (p, platform::is_granted(p))).collect();
            self.perm_polled = Some(Instant::now());
        }
    }

    fn missing_permissions(&self) -> bool {
        self.perm_status.iter().any(|(_, ok)| !ok)
    }

    /// The permissions popup: shown on first launch and from the banner.
    fn permissions_window(&mut self, ctx: &egui::Context) {
        if !self.show_permissions {
            return;
        }
        let mut open = true;
        let mut done = false;
        let mut enable: Option<platform::Permission> = None;
        let all_granted = !self.missing_permissions();
        egui::Window::new(egui::RichText::new("Set up Rega").font(theme::bold(17.0)))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .fixed_size(Vec2::new(520.0, 0.0))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Rega needs your approval to work with your screen. Switch each one on in System Settings.")
                            .color(theme::TEXT_MUTED),
                    )
                    .wrap(),
                );
                ui.add_space(12.0);
                for &(permission, granted) in &self.perm_status {
                    egui::Frame::new()
                        .fill(theme::SURFACE_2)
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .corner_radius(12)
                        .inner_margin(egui::Margin::same(14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let glyph = match permission {
                                    platform::Permission::ScreenRecording => icon::FULLSCREEN,
                                    platform::Permission::Accessibility => icon::SCROLL,
                                };
                                ui.label(egui::RichText::new(glyph).font(theme::icon_font(26.0)).color(theme::ACCENT));
                                ui.add_space(8.0);
                                ui.vertical(|ui| {
                                    ui.set_width(ui.available_width() - 110.0);
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(permission.title()).font(theme::medium(14.5)).color(theme::TEXT));
                                        if permission.optional() {
                                            ui.label(egui::RichText::new("Optional").font(theme::medium(11.5)).color(theme::TEXT_FAINT));
                                        }
                                    });
                                    ui.add(egui::Label::new(egui::RichText::new(permission.description()).color(theme::TEXT_MUTED)).wrap());
                                    ui.add_space(2.0);
                                    let (g, text, color) = if granted {
                                        (icon::CHECK, "Enabled", theme::SUCCESS)
                                    } else {
                                        (icon::INFO, "Not enabled", theme::CORAL)
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(g).font(theme::icon_font(15.0)).color(color));
                                        ui.label(egui::RichText::new(text).font(theme::medium(12.5)).color(color));
                                    });
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if !granted && theme::primary_button(ui, None, "Enable").clicked() {
                                        enable = Some(permission);
                                    }
                                });
                            });
                        });
                    ui.add_space(8.0);
                }
                let screen_missing = self.perm_status.iter().any(|(p, ok)| *p == platform::Permission::ScreenRecording && !ok);
                if self.perm_opened && screen_missing {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("macOS applies Screen Recording only after Rega restarts.").color(theme::TEXT_MUTED),
                        )
                        .wrap(),
                    );
                    ui.add_space(4.0);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if all_granted {
                        if theme::primary_button(ui, Some(icon::CHECK), "Done").clicked() {
                            done = true;
                        }
                    } else {
                        if theme::secondary_button(ui, None, "Skip for now").clicked() {
                            done = true;
                        }
                        if self.perm_opened && theme::primary_button(ui, None, "Restart Rega").clicked() {
                            self.autosave_editor();
                            platform::relaunch();
                        }
                    }
                });
            });
        if let Some(permission) = enable {
            platform::request(permission);
            platform::open_settings(permission);
            self.perm_opened = true;
        }
        if done || !open {
            self.show_permissions = false;
        }
    }

    /// Empty state shown when no capture is open.
    fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let avail = ui.available_rect_before_wrap();
        let painter = ui.painter_at(avail);
        // Soft brand glow behind the card.
        let glow = Rect::from_center_size(avail.center() - Vec2::new(0.0, 20.0), Vec2::new(620.0, 300.0));
        theme::gradient_rect(&painter, glow, 150, Color32::from_white_alpha(14));

        let card_w = 600.0;
        let card = Rect::from_center_size(avail.center(), Vec2::new(card_w, 330.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
            ui.vertical_centered(|ui| {
                theme::logo(ui, 56.0);
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Capture anything").font(theme::bold(28.0)).color(theme::TEXT));
                ui.label(
                    egui::RichText::new(format!(
                        "Press {} anywhere, or choose a mode below. Your captures land in the library tray.",
                        self.keys(Action::Image)
                    ))
                        .color(theme::TEXT_MUTED),
                );
                ui.add_space(22.0);
                let tiles = [
                    (icon::REGION, "Region", self.keys(Action::Image), Purpose::Image, false),
                    (icon::FULLSCREEN, "Full screen", self.keys(Action::FullScreen), Purpose::Image, true),
                    (icon::VIDEO, "Video", self.keys(Action::Video), Purpose::Video, false),
                    (icon::SCROLL, "Scrolling", self.keys(Action::Scroll), Purpose::Scroll, false),
                    (icon::GUIDE, "Guide", self.keys(Action::Guide), Purpose::Guide, false),
                    (icon::OCR, "Grab text", self.keys(Action::Text), Purpose::Text, false),
                    (icon::EYEDROPPER, "Pick color", self.keys(Action::Color), Purpose::Color, false),
                ];
                // Wider than the card: centred on the screen, and smaller when
                // the window is too narrow for the whole row.
                let gap = 12.0;
                let n = tiles.len() as f32;
                let side = ((avail.width() - 48.0 - gap * (n - 1.0)) / n).clamp(84.0, 104.0);
                let tile = Vec2::splat(side);
                let row_w = tile.x * n + gap * (n - 1.0);
                let top = ui.cursor().min.y;
                let row = Rect::from_min_size(Pos2::new(avail.center().x - row_w / 2.0, top), Vec2::new(row_w, tile.y));
                ui.allocate_space(Vec2::new(0.0, tile.y));
                for (n, (glyph, label, keys, purpose, full)) in tiles.into_iter().enumerate() {
                    let r = Rect::from_min_size(row.min + Vec2::new(n as f32 * (tile.x + gap), 0.0), tile);
                    if mode_tile(ui, r, glyph, label, &keys).clicked() {
                        self.start_capture(&ctx, purpose, full);
                    }
                }
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.add_space((card_w - 470.0) / 2.0);
                    let new = theme::ghost_button(ui, Some(icon::NEW), "New blank image");
                    self.new_canvas_popup(&new);
                    if theme::ghost_button(ui, Some(icon::IMPORT), "Open image…").clicked() {
                        self.import_from_dialog();
                    }
                    if theme::ghost_button(ui, Some(icon::PASTE), "Paste").clicked() {
                        self.import_from_clipboard();
                    }
                });
            });
        });
    }

    /// Library filmstrip along the bottom of the window.
    fn tray_ui(&mut self, ui: &mut egui::Ui) {
        let mut open: Option<Item> = None;
        let mut delete: Option<Item> = None;
        let current = self.current_path().cloned();

        let needle = self.filter.to_lowercase();
        let items: Vec<Item> = self
            .library
            .iter()
            .filter(|i| match self.lib_filter {
                LibFilter::All => true,
                LibFilter::Images => i.kind == ItemKind::Image,
                LibFilter::Videos => i.kind == ItemKind::Video,
                LibFilter::Guides => i.kind == ItemKind::Guide,
            })
            .filter(|i| needle.is_empty() || i.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        // Items hidden by the filter or search can't be acted on unseen.
        self.lib_selection.retain(|p| items.iter().any(|i| &i.path == p));
        let mut delete_selection = false;
        // The selected images and videos, in tray order (guides are folders of pages).
        let copyable: Vec<Item> = items
            .iter()
            .filter(|i| self.lib_selection.contains(&i.path) && i.kind != ItemKind::Guide)
            .cloned()
            .collect();
        let mut copy_selection = false;
        // ⌘C with items selected copies them (the tray comes before the
        // editor, which would otherwise copy its own selection).
        if !self.lib_selection.is_empty() && ui.ctx().memory(|m| m.focused().is_none()) {
            copy_selection = ui.input_mut(|i| {
                let event = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
                i.events.retain(|e| !matches!(e, egui::Event::Copy));
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::C) || event
            });
        }

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Library").font(theme::medium(14.0)).color(theme::TEXT));
            let count = |kind| self.library.iter().filter(|i| i.kind == kind).count();
            let (images, videos, guides) = (count(ItemKind::Image), count(ItemKind::Video), count(ItemKind::Guide));
            ui.add_space(6.0);
            let all = format!("All {}", self.library.len());
            let imgs = format!("Images {images}");
            let vids = format!("Videos {videos}");
            let gds = format!("Guides {guides}");
            theme::segmented(
                ui,
                &mut self.lib_filter,
                &[(LibFilter::All, &all), (LibFilter::Images, &imgs), (LibFilter::Videos, &vids), (LibFilter::Guides, &gds)],
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // While items are selected, the header offers actions on them.
                if !self.lib_selection.is_empty() {
                    let n = self.lib_selection.len();
                    if theme::danger_button(ui, Some(icon::DELETE), &format!("Delete {n}")).clicked() {
                        delete_selection = true;
                    }
                    if !copyable.is_empty()
                        && theme::secondary_button(ui, Some(icon::COPY), &format!("Copy {}", copyable.len()))
                            .on_hover_text(format!("Copy to paste into a chat, an email or a folder ({}C)", platform::CMD))
                            .clicked()
                    {
                        copy_selection = true;
                    }
                    if theme::ghost_button(ui, None, "Clear").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.lib_selection.clear();
                    }
                    if theme::ghost_button(ui, None, "Select all").clicked() {
                        self.lib_selection = items.iter().map(|i| i.path.clone()).collect();
                    }
                    ui.label(egui::RichText::new(format!("{n} selected")).font(theme::medium(13.0)).color(theme::TEXT));
                    return;
                }
                if theme::icon_button_sized(ui, icon::FOLDER, "Show library folder", 30.0, true).clicked() {
                    platform::open_path(&self.settings.save_dir);
                }
                if theme::icon_button_sized(ui, icon::PASTE, "New from clipboard", 30.0, true).clicked() {
                    self.import_from_clipboard();
                }
                if theme::icon_button_sized(ui, icon::IMPORT, "Open image…", 30.0, true).clicked() {
                    self.import_from_dialog();
                }
                ui.add_space(6.0);
                search_box(ui, &mut self.filter);
            });
        });
        ui.add_space(8.0);

        if items.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                let msg = if self.library.is_empty() { "Your captures will appear here" } else { "Nothing matches" };
                ui.label(egui::RichText::new(msg).color(theme::TEXT_FAINT));
            });
            return;
        }
        egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                for item in &items {
                    let selected = current.as_ref() == Some(&item.path);
                    let checked = self.lib_selection.contains(&item.path);
                    let (resp, delete_clicked) = self.tray_thumb(ui, item, selected, checked);
                    let mods = ui.input(|i| i.modifiers);
                    if delete_clicked {
                        delete = Some(item.clone());
                    } else if resp.clicked() && mods.shift {
                        // Range from the anchor (or the open item) to here.
                        let from = self.lib_anchor.as_ref().or(current.as_ref());
                        let a = from.and_then(|f| items.iter().position(|i| &i.path == f));
                        let b = items.iter().position(|i| i.path == item.path);
                        if let (Some(a), Some(b)) = (a.or(b), b) {
                            for i in &items[a.min(b)..=a.max(b)] {
                                self.lib_selection.insert(i.path.clone());
                            }
                        }
                    } else if resp.clicked() && mods.command {
                        if !self.lib_selection.remove(&item.path) {
                            self.lib_selection.insert(item.path.clone());
                        }
                        self.lib_anchor = Some(item.path.clone());
                    } else if resp.clicked() {
                        self.lib_selection.clear();
                        self.lib_anchor = Some(item.path.clone());
                        if !selected {
                            open = Some(item.clone());
                        }
                    }
                    let many = checked && self.lib_selection.len() > 1;
                    resp.context_menu(|ui| {
                        ui.set_min_width(180.0);
                        if ui.button("Open").clicked() {
                            open = Some(item.clone());
                            ui.close();
                        }
                        if item.kind == ItemKind::Guide && ui.button("Open in browser").clicked() {
                            platform::open_path(&item.path);
                            ui.close();
                        }
                        if item.kind != ItemKind::Guide && ui.button("Open with default app").clicked() {
                            platform::open_path(&item.path);
                            ui.close();
                        }
                        if ui.button(reveal_label()).clicked() {
                            platform::reveal_path(&item.path);
                            ui.close();
                        }
                        if many && !copyable.is_empty() {
                            if ui.button(format!("Copy {} selected", copyable.len())).clicked() {
                                copy_selection = true;
                                ui.close();
                            }
                        } else if item.kind == ItemKind::Image && ui.button("Copy").clicked() {
                            if let Ok(img) = image::open(&item.path) {
                                let _ = platform::copy_image(&img.to_rgba8());
                            }
                            ui.close();
                        }
                        ui.separator();
                        let label = if many { format!("Delete {} selected", self.lib_selection.len()) } else { "Delete".into() };
                        if ui.button(egui::RichText::new(label).color(theme::DANGER)).clicked() {
                            if many {
                                delete_selection = true;
                            } else {
                                delete = Some(item.clone());
                            }
                            ui.close();
                        }
                    });
                }
            });
        });

        if let Some(item) = open {
            self.open_item(&item);
        }
        if let Some(item) = delete {
            self.confirm_delete(&item);
        }
        if delete_selection {
            self.delete_selection();
        }
        if copy_selection {
            self.copy_items(&copyable);
        }
    }

    /// Copy library items: one image as a picture, as before; several as
    /// files, the way Finder and Explorer copy them.
    fn copy_items(&mut self, items: &[Item]) {
        let result = match items {
            [] => {
                self.toast("Guides can't be copied: open one to export it");
                return;
            }
            [one] if one.kind == ItemKind::Image => {
                image::open(&one.path).map_err(anyhow::Error::from).and_then(|img| platform::copy_image(&img.to_rgba8()))
            }
            _ => platform::copy_files(&items.iter().map(|i| i.path.clone()).collect::<Vec<_>>()),
        };
        let what = match items.iter().filter(|i| i.kind == ItemKind::Image).count() {
            n if n == items.len() && n == 1 => "the image".to_owned(),
            n if n == items.len() => format!("{n} images"),
            _ if items.len() == 1 => "the video".to_owned(),
            _ => format!("{} files", items.len()),
        };
        match result {
            Ok(()) if items.len() > 1 => self.toast(format!("Copied {what}: paste them into a chat, an email or a folder")),
            Ok(()) => self.toast(format!("Copied {what}")),
            Err(e) => self.toast(format!("Couldn't copy {what}: {e:#}")),
        }
    }

    /// The path of the image or video open in the workspace, if any.
    fn current_path(&self) -> Option<&PathBuf> {
        match &self.mode {
            Mode::Editor(ed) => Some(&ed.path),
            Mode::Video(player) => Some(&player.path),
            Mode::Guide(ed) => Some(&ed.path),
            Mode::Home => None,
        }
    }

    /// ⌘W: close the open image or video; with nothing open, close the window
    /// (Rega keeps running in the menu bar, as with the window's close button).
    fn close_current(&mut self, ctx: &egui::Context) {
        if self.current_path().is_none() {
            self.close_window(ctx);
            return;
        }
        self.autosave_editor();
        self.mode = Mode::Home;
    }

    fn close_window(&mut self, ctx: &egui::Context) {
        self.autosave_editor();
        self.hide_window(ctx);
        self.hidden_by_user = true;
    }

    /// ⌘D: delete the selected library items, or else the open image or video,
    /// after confirming.
    fn delete_current(&mut self) {
        if !self.lib_selection.is_empty() {
            self.delete_selection();
            return;
        }
        let Some(path) = self.current_path().cloned() else { return };
        // Save first so a pending autosave can't recreate the file after the delete.
        self.autosave_editor();
        match self.library.iter().find(|i| i.path == path).cloned() {
            Some(item) => self.confirm_delete(&item),
            None => self.toast("This item isn't in the library"),
        }
    }

    /// Ask once, then permanently delete every selected library item.
    fn delete_selection(&mut self) {
        let items: Vec<Item> = self.library.iter().filter(|i| self.lib_selection.contains(&i.path)).cloned().collect();
        match items.len() {
            0 => return,
            1 => {
                self.confirm_delete(&items[0]);
                self.lib_selection.clear();
                return;
            }
            _ => {}
        }
        let n = items.len();
        let confirmed = rfd::MessageDialog::new()
            .set_title(format!("Delete {n} captures?"))
            .set_description(format!("{n} captures will be permanently deleted."))
            .set_buttons(rfd::MessageButtons::OkCancel)
            .show();
        if confirmed != rfd::MessageDialogResult::Ok {
            return;
        }
        // Save first so a pending autosave can't recreate a deleted file.
        self.autosave_editor();
        if self.current_path().is_some_and(|p| self.lib_selection.contains(p)) {
            self.mode = Mode::Home;
        }
        let failed = items.iter().filter(|i| library::delete(&i.path).is_err()).count();
        self.lib_selection.clear();
        self.lib_anchor = None;
        self.refresh_library();
        if failed == 0 {
            self.toast(format!("Deleted {n} captures"));
        } else {
            self.toast(format!("Deleted {} of {n} captures; {failed} could not be deleted", n - failed));
        }
    }

    /// Ask, then permanently delete a library item (closing it if it is open).
    fn confirm_delete(&mut self, item: &Item) {
        let confirmed = rfd::MessageDialog::new()
            .set_title("Delete capture?")
            .set_description(format!("“{}” will be permanently deleted.", item.name))
            .set_buttons(rfd::MessageButtons::OkCancel)
            .show();
        if confirmed == rfd::MessageDialogResult::Ok {
            if self.current_path() == Some(&item.path) {
                self.mode = Mode::Home;
            }
            match library::delete(&item.path) {
                Ok(()) => self.refresh_library(),
                Err(e) => self.toast(format!("Delete failed: {e}")),
            }
        }
    }

    /// One library thumbnail; returns its response and whether its hover
    /// delete button was clicked.
    fn tray_thumb(&mut self, ui: &mut egui::Ui, item: &Item, selected: bool, checked: bool) -> (egui::Response, bool) {
        let size = CARD;
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        if !ui.is_rect_visible(rect) {
            return (resp, false);
        }
        // Pointer anywhere over the card, including over its delete button.
        let pointer_on_card = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| rect.contains(p) && ui.clip_rect().contains(p));
        let over = resp.hovered() || resp.contains_pointer() || pointer_on_card;
        let hover = if over { 1.0 } else { 0.0 };
        let thumb = Rect::from_min_size(rect.min, Vec2::new(size.x, 100.0));
        let p = ui.painter();
        p.rect_filled(thumb, 10.0, Color32::from_rgb(11, 13, 17));

        // After an edit, the last picture stays up until the new one is made:
        // no blank card in between.
        let (shown, current) = match self.thumbs.get(&item.path) {
            Some((modified, tex)) => (Some(tex.clone()), *modified == item.modified),
            None => (None, false),
        };
        if shown.is_some() && !current {
            let key = (item.path.clone(), item.modified);
            if self.thumbs_requested.insert(key.clone()) {
                let _ = self.thumb_jobs.send(key);
            }
        }
        match (&item.kind, shown) {
            (ItemKind::Image | ItemKind::Guide, Some(tex)) => {
                // Cover-fit into the rounded frame.
                let ts = tex.size_vec2();
                let scale = (thumb.width() / ts.x).max(thumb.height() / ts.y);
                let vis = Vec2::new(thumb.width() / (ts.x * scale), thumb.height() / (ts.y * scale));
                let uv = Rect::from_center_size(Pos2::new(0.5, 0.5), vis);
                p.add(egui::epaint::RectShape::filled(thumb, 10, Color32::WHITE).with_texture(tex.id(), uv));
            }
            (ItemKind::Image | ItemKind::Guide, None) => {
                let key = (item.path.clone(), item.modified);
                if self.thumbs_requested.insert(key.clone()) {
                    let _ = self.thumb_jobs.send(key);
                }
                let glyph = if item.kind == ItemKind::Guide { icon::GUIDE } else { icon::IMAGE };
                p.text(thumb.center(), Align2::CENTER_CENTER, glyph, theme::icon_font(26.0), theme::TEXT_FAINT);
            }
            (ItemKind::Video, fresh) => {
                match fresh {
                    Some(tex) => {
                        let ts = tex.size_vec2();
                        let scale = (thumb.width() / ts.x).max(thumb.height() / ts.y);
                        let vis = Vec2::new(thumb.width() / (ts.x * scale), thumb.height() / (ts.y * scale));
                        let uv = Rect::from_center_size(Pos2::new(0.5, 0.5), vis);
                        p.add(egui::epaint::RectShape::filled(thumb, 10, Color32::WHITE).with_texture(tex.id(), uv));
                    }
                    None => {
                        let key = (item.path.clone(), item.modified);
                        if self.thumbs_requested.insert(key) {
                            let _ = self.thumb_jobs.send((item.path.clone(), item.modified));
                        }
                        theme::gradient_rect(p, thumb, 10, Color32::from_white_alpha(60));
                    }
                }
                p.circle_filled(thumb.center(), 20.0, Color32::from_black_alpha(120));
                p.text(thumb.center(), Align2::CENTER_CENTER, icon::PLAY, theme::icon_font(26.0), Color32::WHITE);
                let ext = item.path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
                let badge = Rect::from_min_size(thumb.left_top() + Vec2::new(8.0, 8.0), Vec2::new(36.0, 18.0));
                p.rect_filled(badge, 5.0, Color32::from_black_alpha(150));
                p.text(badge.center(), Align2::CENTER_CENTER, ext, theme::medium(10.0), Color32::WHITE);
            }
        }
        if item.kind == ItemKind::Guide {
            let badge = Rect::from_min_size(thumb.left_top() + Vec2::new(8.0, 8.0), Vec2::new(58.0, 18.0));
            theme::gradient_rect(p, badge, 5, Color32::WHITE);
            p.text(badge.center() - Vec2::new(14.0, 0.0), Align2::CENTER_CENTER, icon::GUIDE, theme::icon_font(12.0), Color32::WHITE);
            p.text(badge.center() + Vec2::new(8.0, 0.0), Align2::CENTER_CENTER, "GUIDE", theme::medium(10.0), Color32::WHITE);
        }
        if checked {
            p.rect_filled(thumb, 10.0, theme::ACCENT.gamma_multiply(0.18));
            p.rect_stroke(thumb, 10.0, Stroke::new(2.5, theme::ACCENT), StrokeKind::Outside);
            let c = thumb.left_bottom() + Vec2::new(18.0, -18.0);
            p.circle_filled(c, 11.0, theme::ACCENT);
            p.circle_stroke(c, 11.0, Stroke::new(1.5, Color32::WHITE));
            p.text(c, Align2::CENTER_CENTER, icon::CHECK, theme::icon_font(15.0), Color32::WHITE);
        } else if selected {
            p.rect_stroke(thumb, 10.0, Stroke::new(2.5, theme::ACCENT), StrokeKind::Outside);
        } else {
            let c = lerp(theme::BORDER, theme::BORDER_STRONG, hover);
            p.rect_stroke(thumb, 10.0, Stroke::new(1.0, c), StrokeKind::Inside);
        }
        let mut delete_clicked = false;
        if hover > 0.0 {
            let btn = Rect::from_center_size(thumb.right_top() + Vec2::new(-18.0, 18.0), Vec2::splat(28.0));
            let del = ui.interact(btn, resp.id.with("delete"), Sense::click());
            let p = ui.painter();
            let bg = if del.hovered() { Color32::from_rgb(255, 110, 126) } else { theme::DANGER };
            p.circle_filled(btn.center() + Vec2::new(0.0, 2.0), 15.0, Color32::from_black_alpha(90));
            p.circle_filled(btn.center(), 14.0, bg);
            p.circle_stroke(btn.center(), 14.0, Stroke::new(1.5, Color32::from_white_alpha(230)));
            p.text(btn.center(), Align2::CENTER_CENTER, icon::DELETE, theme::icon_font(17.0), Color32::WHITE);
            delete_clicked = del.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
        }
        (resp, delete_clicked)
    }

    /// Images dragged from Finder / Explorer onto the window: a new item from
    /// the library, a layer on the canvas in the editor. Other screens (a
    /// video, a guide) leave drops alone rather than lose what's open.
    fn handle_drops(&mut self, ctx: &egui::Context) {
        let editable = matches!(self.mode, Mode::Home | Mode::Editor(_));
        if editable && ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop_hint")));
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(150));
            painter.text(
                screen.center(),
                Align2::CENTER_CENTER,
                "Drop an image to open it",
                egui::FontId::proportional(22.0),
                Color32::WHITE,
            );
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() || !editable {
            return;
        }
        let mut imgs = Vec::new();
        for file in dropped {
            let path = file.path();
            match image::open(path) {
                Ok(img) => imgs.push(img.to_rgba8()),
                Err(e) => self.toast(format!("Cannot open {}: {e}", path.file_name().unwrap_or_default().to_string_lossy())),
            }
        }
        if imgs.is_empty() {
            return;
        }
        if let Mode::Editor(ed) = &mut self.mode {
            let toast = crate::editor::pasted_toast(imgs.len());
            ed.paste_images(imgs);
            self.toast(toast);
        } else {
            self.import_images(imgs);
        }
    }

    fn import_from_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico"])
            .pick_file()
        {
            match image::open(&p) {
                Ok(img) => self.import_image(img.to_rgba8()),
                Err(e) => self.toast(format!("Cannot open: {e}")),
            }
        }
    }

    fn import_from_clipboard(&mut self) {
        let imgs = platform::clipboard_images();
        if imgs.is_empty() {
            self.toast("No image on the clipboard");
        } else {
            self.import_images(imgs);
        }
    }

    fn recording_bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let stop_tip = format!("Stop and save   {}", self.keys(Action::Video));
        let Activity::Recording(rec) = &mut self.activity else { return };
        let mut stop = false;
        let mut discard = false;
        let frame = egui::Frame::new().fill(Color32::from_rgb(17, 19, 24)).inner_margin(egui::Margin::symmetric(14, 9));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let secs = rec.elapsed().as_secs();
                let pulse = ((ctx.input(|i| i.time) * 3.0).sin() * 0.5 + 0.5) as f32;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                if rec.is_paused() {
                    ui.painter().circle_filled(r.center(), 6.0, Color32::from_rgb(255, 196, 0));
                } else {
                    ui.painter().circle_filled(r.center(), 6.0 + 3.0 * pulse, theme::DANGER.gamma_multiply(0.25));
                    ui.painter().circle_filled(r.center(), 6.0, theme::DANGER);
                }
                ui.label(egui::RichText::new(format!("{}:{:02}", secs / 60, secs % 60)).font(FontId::monospace(16.0)).color(theme::TEXT));
                ui.label(egui::RichText::new(if rec.is_paused() { "Paused" } else { "Recording" }).color(theme::TEXT_MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, icon::CLOSE, "Discard").clicked() {
                        discard = true;
                    }
                    if theme::icon_button(ui, icon::STOP, &stop_tip).clicked() {
                        stop = true;
                    }
                    let (g, tip) = if rec.is_paused() { (icon::PLAY, "Resume") } else { (icon::PAUSE, "Pause") };
                    if theme::icon_button(ui, g, tip).clicked() {
                        rec.toggle_pause();
                    }
                });
            });
        });
        // The bar only shows whole seconds; don't compete with the capture/encode threads.
        ctx.request_repaint_after(Duration::from_millis(200));
        if discard {
            rec.discard();
            stop = true;
        }
        if stop {
            self.stop_recording(&ctx);
        }
    }

    /// The bar while a guide records: how many steps so far, Stop and Discard.
    fn guide_bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let stop_tip = format!("Stop and edit the guide   {}", self.keys(Action::Guide));
        let Activity::Guiding(rec) = &self.activity else { return };
        let (steps, secs) = (rec.step_count(), rec.elapsed().as_secs());
        let mut stop = None;
        let frame = egui::Frame::new().fill(Color32::from_rgb(17, 19, 24)).inner_margin(egui::Margin::symmetric(14, 9));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let pulse = ((ctx.input(|i| i.time) * 3.0).sin() * 0.5 + 0.5) as f32;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 6.0 + 3.0 * pulse, theme::CORAL.gamma_multiply(0.25));
                ui.painter().circle_filled(r.center(), 6.0, theme::CORAL);
                ui.label(egui::RichText::new(format!("{}:{:02}", secs / 60, secs % 60)).font(FontId::monospace(16.0)).color(theme::TEXT));
                let label = match steps {
                    0 => "Click to add steps".to_owned(),
                    1 => "1 step".to_owned(),
                    n => format!("{n} steps"),
                };
                ui.label(egui::RichText::new(label).color(theme::TEXT_MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, icon::CLOSE, "Discard").clicked() {
                        stop = Some(true);
                    }
                    if theme::icon_button(ui, icon::STOP, &stop_tip).clicked() {
                        stop = Some(false);
                    }
                });
            });
        });
        ctx.request_repaint_after(Duration::from_millis(200));
        if let Some(discard) = stop {
            self.stop_guide(&ctx, discard);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let before = serde_json::to_string(&self.settings).unwrap_or_default();
        egui::Window::new(egui::RichText::new("Settings").font(theme::bold(17.0)))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(440.0, 0.0))
            .show(ctx, |ui| {
                theme::segmented(
                    ui,
                    &mut self.settings_tab,
                    &[
                        (SettingsTab::General, "General"),
                        (SettingsTab::Video, "Video"),
                        (SettingsTab::Guide, "Guide"),
                        (SettingsTab::Shortcuts, "Shortcuts"),
                    ],
                );
                if self.settings_tab != SettingsTab::Shortcuts && self.recording_shortcut.take().is_some() {
                    self.apply_hotkeys();
                }
                ui.add_space(8.0);
                match self.settings_tab {
                    SettingsTab::General => {
                        theme::section(ui, "Library");
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(truncate(&self.settings.save_dir.display().to_string(), 44)).monospace().color(theme::TEXT_MUTED));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::secondary_button(ui, None, "Change…").clicked()
                                    && let Some(dir) = rfd::FileDialog::new().set_directory(&self.settings.save_dir).pick_folder()
                                {
                                    self.settings.save_dir = dir;
                                    self.refresh_library();
                                }
                            });
                        });
                        ui.add_space(8.0);
                        theme::section(ui, "Startup");
                        if theme::switch(ui, &mut self.settings.launch_at_login, "Launch at login").changed() {
                            platform::set_launch_at_login(self.settings.launch_at_login);
                        }
                        ui.add_space(8.0);
                        theme::section(ui, "After capture");
                        theme::switch(ui, &mut self.settings.copy_after_capture, "Copy to clipboard");
                        theme::switch(ui, &mut self.settings.open_editor_after_capture, "Open in the editor");
                        theme::switch(ui, &mut self.settings.focus_after_capture, "Focus Rega when done")
                            .on_hover_text("On: Rega comes to the front with the new capture.\nOff: it opens behind the app you're using.");
                        ui.horizontal(|ui| {
                            ui.label("Image format");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                theme::segmented(ui, &mut self.settings.image_format, &[(ImageFormat::Png, "PNG"), (ImageFormat::Jpeg, "JPEG")]);
                            });
                        });
                        ui.add_space(8.0);
                        theme::section(ui, "Updates");
                        theme::switch(ui, &mut self.settings.check_for_updates, "Check for updates daily")
                            .on_hover_text("Rega asks the website for the latest version number.\nNothing else is sent.");
                        ui.horizontal(|ui| {
                            let (status, color) = if self.update_rx.is_some() {
                                ("Checking…".to_owned(), theme::TEXT_MUTED)
                            } else if let Some(latest) = &self.update {
                                (format!("{APP_NAME} {} is available", latest.version), theme::TEXT)
                            } else {
                                (format!("You have {APP_NAME} {}", env!("CARGO_PKG_VERSION")), theme::TEXT_MUTED)
                            };
                            ui.label(egui::RichText::new(status).color(color));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if let Some(latest) = self.update.clone() {
                                    if self.installing.is_some() {
                                        ui.spinner();
                                    } else if self.can_update_in_place(&latest) {
                                        if theme::primary_button(ui, Some(icon::IMPORT), "Update now").clicked() {
                                            self.update_now(&latest);
                                        }
                                    } else if theme::primary_button(ui, Some(icon::OPEN_EXTERNAL), "Download").clicked() {
                                        platform::open_url(&update::download_url());
                                    }
                                } else if theme::secondary_button(ui, None, "Check now").clicked() {
                                    self.check_for_update(true);
                                }
                            });
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"))).color(theme::TEXT_FAINT));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.link("About Rega").clicked() {
                                    self.show_about = true;
                                }
                            });
                        });
                    }
                    SettingsTab::Video => {
                        theme::section(ui, "Video");
                        ui.horizontal(|ui| {
                            ui.label("Format");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                theme::segmented(ui, &mut self.settings.video_format, &[(VideoFormat::Mp4, "MP4"), (VideoFormat::Gif, "GIF")]);
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label("Frame rate");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add(egui::Slider::new(&mut self.settings.video_fps, 5..=30).suffix(" fps"));
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label("Max width");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add(egui::Slider::new(&mut self.settings.video_max_width, 640..=3840).suffix(" px"));
                            });
                        });
                        theme::switch(ui, &mut self.settings.highlight_cursor, "Highlight the cursor");
                        ui.add_space(8.0);
                        theme::section(ui, "GIF");
                        ui.horizontal(|ui| {
                            ui.label("Quality");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let options: Vec<(GifQuality, &str)> = GifQuality::ALL.iter().map(|q| (*q, q.label())).collect();
                                theme::segmented(ui, &mut self.settings.gif_quality, &options);
                            });
                        });
                        ui.label(
                            egui::RichText::new(format!(
                                "GIF recordings: up to {}. Smaller means lighter files.",
                                self.settings.gif_quality.details()
                            ))
                            .small()
                            .color(theme::TEXT_FAINT),
                        );
                    }
                    SettingsTab::Guide => {
                        use crate::guide::style;
                        let g = &mut self.settings.guide;
                        let hint = |ui: &mut egui::Ui, text: &str| {
                            ui.label(egui::RichText::new(text).small().color(theme::TEXT_FAINT));
                        };
                        // The window takes its height from what's in it; give the
                        // list a height of its own, as much as the screen allows.
                        let height = (ui.ctx().content_rect().height() - 240.0).clamp(240.0, 560.0);
                        egui::ScrollArea::vertical().max_height(height).min_scrolled_height(height).auto_shrink([false, true]).show(ui, |ui| {
                            theme::section(ui, "New guides");
                            hint(ui, "Each guide keeps its own look: these are what new guides start with.");
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label("Each step plays for");
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    style::seconds_control(ui, &mut g.style.seconds);
                                });
                            });
                            ui.add_space(6.0);
                            ui.label("Marker");
                            style::marker_controls(ui, &mut g.style);
                            ui.add_space(8.0);
                            theme::section(ui, "Shared page");
                            style::page_controls(ui, &mut g.style);
                            ui.add_space(8.0);
                            theme::section(ui, "Screenshots");
                            ui.label("Quality");
                            style::quality_control(ui, &mut g.style.quality);
                            ui.add_space(8.0);
                            theme::section(ui, "Recording");
                            theme::switch(ui, &mut g.record_typing, "Record typing");
                            if g.record_typing {
                                theme::switch(ui, &mut g.typed_text_in_captions, "Put typed text in captions");
                                hint(
                                    ui,
                                    if cfg!(target_os = "macos") {
                                        "Off: captions say “Type here” and what you type is never kept (screenshots still show the screen). Password fields are never seen."
                                    } else {
                                        "Off: captions say “Type here” and what you type is never kept (screenshots still show the screen)."
                                    },
                                );
                            }
                            theme::switch(ui, &mut g.record_keys, "Record shortcuts and key presses");
                            hint(ui, &format!("{}C, Esc or Enter on its own becomes a step.", platform::CMD));
                            ui.add_space(8.0);
                            if *g != style::GuideSettings::default() && theme::ghost_button(ui, Some(icon::UNDO), "Reset to defaults").clicked() {
                                *g = style::GuideSettings::default();
                            }
                        });
                    }
                    SettingsTab::Shortcuts => {
                        theme::section(ui, "Global shortcuts");
                        ui.label(
                            egui::RichText::new("Click a shortcut, then press the new keys. Esc cancels, ⌫ clears.")
                                .color(theme::TEXT_FAINT)
                                .small(),
                        );
                        ui.add_space(4.0);
                        for action in Action::ALL {
                            ui.horizontal(|ui| {
                                ui.label(action.label());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let is_default = self.settings.shortcuts.is_default(action);
                                    let reset = theme::icon_button_sized(ui, icon::UNDO, "Reset to default", 28.0, !is_default);
                                    if reset.clicked() && !is_default {
                                        self.set_shortcut(action, action.default_combo().to_owned());
                                    }
                                    let recording = self.recording_shortcut == Some(action);
                                    let text = if recording {
                                        "Press keys…".to_owned()
                                    } else {
                                        let k = self.keys(action);
                                        if k.is_empty() { "None".to_owned() } else { k }
                                    };
                                    if shortcut_chip(ui, &text, recording).clicked() {
                                        if recording {
                                            self.recording_shortcut = None;
                                            self.apply_hotkeys();
                                        } else {
                                            self.recording_shortcut = Some(action);
                                            self.suspend_hotkeys();
                                        }
                                    }
                                });
                            });
                        }
                        ui.add_space(6.0);
                        let all_default = self.settings.shortcuts == shortcuts::Shortcuts::default();
                        ui.add_enabled_ui(!all_default, |ui| {
                            if theme::ghost_button(ui, Some(icon::UNDO), "Reset all to defaults").clicked() {
                                self.settings.shortcuts = shortcuts::Shortcuts::default();
                                self.recording_shortcut = None;
                                let failed = self.apply_hotkeys();
                                self.report_hotkeys(failed, "Shortcuts reset to defaults");
                            }
                        });
                    }
                }
            });
        self.show_settings = open;
        if !open && self.recording_shortcut.take().is_some() {
            self.apply_hotkeys();
        }
        self.capture_shortcut_keys(ctx);
        if serde_json::to_string(&self.settings).unwrap_or_default() != before {
            self.settings.save();
        }
    }

    /// While a shortcut row is listening, turn the next key press into its shortcut.
    fn capture_shortcut_keys(&mut self, ctx: &egui::Context) {
        let Some(action) = self.recording_shortcut else { return };
        let presses: Vec<(egui::Key, egui::Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    // `key` is the logical key; egui already falls back to the
                    // physical key when ⌥ turns the letter into a symbol.
                    egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, mods) in presses {
            if mods.is_none() && key == egui::Key::Escape {
                self.recording_shortcut = None;
                self.apply_hotkeys();
                return;
            }
            if mods.is_none() && matches!(key, egui::Key::Backspace | egui::Key::Delete) {
                self.set_shortcut(action, String::new());
                return;
            }
            let Some(combo) = shortcuts::from_key_press(key, mods) else {
                if !mods.is_none() || key.name().len() == 1 {
                    self.toast("Use Control, Option or Command with a letter, digit or F-key");
                }
                continue;
            };
            let hotkey: HotKey = match combo.parse() {
                Ok(h) => h,
                Err(_) => continue,
            };
            if let Some(other) = self.settings.shortcuts.conflict(action, hotkey) {
                self.toast(format!("{} is already used by “{}”", shortcuts::display(hotkey), other.label()));
                return;
            }
            self.set_shortcut(action, combo);
            return;
        }
    }

    fn set_shortcut(&mut self, action: Action, combo: String) {
        self.settings.shortcuts.set(action, combo);
        self.settings.save();
        self.recording_shortcut = None;
        let failed = self.apply_hotkeys();
        let keys = self.keys(action);
        let msg = if keys.is_empty() {
            format!("{} has no shortcut now", action.label())
        } else {
            format!("{} is now {keys}", action.label())
        };
        self.report_hotkeys(failed, &msg);
    }

    fn report_hotkeys(&mut self, failed: Vec<String>, ok: &str) {
        if failed.is_empty() {
            self.toast(ok.to_owned());
        } else {
            self.toast(format!("Couldn't register (used by another app?): {}", failed.join(", ")));
        }
    }

    /// About Rega: what it is, version, what it's built with, credits, license.
    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        egui::Window::new(egui::RichText::new("About Rega").font(theme::bold(17.0)))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(400.0, 0.0))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    theme::logo(ui, 72.0);
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(APP_NAME).font(theme::bold(26.0)).color(theme::TEXT));
                    ui.label(egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(theme::TEXT_MUTED));
                    if let Some(latest) = &self.update
                        && ui.link(format!("{APP_NAME} {} is available — download", latest.version)).clicked()
                    {
                        platform::open_url(&update::download_url());
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Capture anything on your screen, mark it up, and share it — in seconds.")
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new("Screenshots, scrolling captures, screen recordings (MP4/GIF), annotation and OCR, in one native app.")
                            .small()
                            .color(theme::TEXT_MUTED),
                    );
                });
                ui.add_space(12.0);
                theme::section(ui, "Built with");
                // The video libraries on a line of their own: one line won't fit.
                ui.label(
                    egui::RichText::new("Rust · egui · xcap · tiny-skia · ab_glyph\nminimp4 · gif · pic-scale")
                        .color(theme::TEXT_MUTED),
                );
                ui.label(
                    egui::RichText::new("MP4 video uses the H.264 codec built into macOS and Windows.").small().color(theme::TEXT_FAINT),
                );
                ui.add_space(8.0);
                theme::section(ui, "Credits");
                for (what, credit) in [
                    ("Interface font", "Ubuntu · Ubuntu Font Licence 1.0"),
                    ("Hebrew text", "Noto Sans Hebrew · SIL OFL 1.1"),
                    ("Icons", "Material Symbols by Google · Apache 2.0"),
                    ("Key symbols", "Noto Sans Symbols · SIL OFL 1.1"),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(what).color(theme::TEXT));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(credit).color(theme::TEXT_MUTED));
                        });
                    });
                }
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("© 2026 Shai Raiten · MIT License").small().color(theme::TEXT_FAINT));
                });
            });
        self.show_about = open;
    }

    fn ocr_window(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.ocr_rx
            && let Ok(result) = rx.try_recv()
        {
            self.ocr_rx = None;
            match result {
                Ok(text) if text.trim().is_empty() => self.toast("No text found"),
                Ok(text) => {
                    let _ = platform::copy_text(&text);
                    self.toast("Text copied to clipboard");
                    self.ocr_text = Some(text);
                }
                Err(e) => self.toast(format!("{e:#}")),
            }
        }
        let Some(text) = &mut self.ocr_text else { return };
        let mut open = true;
        let mut copy = false;
        egui::Window::new(egui::RichText::new("Recognized text").font(theme::bold(17.0)))
            .open(&mut open)
            .collapsible(false)
            .default_width(540.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(Color32::from_rgb(11, 13, 17))
                    .stroke(Stroke::new(1.0, theme::BORDER))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(text)
                                    .desired_width(f32::INFINITY)
                                    .frame(egui::Frame::NONE)
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                    });
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    copy = theme::primary_button(ui, Some(icon::COPY), "Copy text").clicked();
                });
            });
        if copy {
            let _ = platform::copy_text(text);
            self.toast("Text copied to clipboard");
        }
        if !open {
            self.ocr_text = None;
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some((msg, at)) = &self.toast else { return };
        let age = at.elapsed().as_secs_f32();
        if age > 3.5 {
            self.toast = None;
            return;
        }
        let alpha = (age / 0.15).min(1.0).min((3.5 - age) / 0.3);
        let failed = msg.contains("fail") || msg.contains("Failed") || msg.contains("Cannot") || msg.contains("stopped early");
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -186.0 + (1.0 - alpha) * 10.0))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                egui::Frame::new()
                    .fill(theme::SURFACE_3)
                    .stroke(Stroke::new(1.0, theme::BORDER_STRONG))
                    .corner_radius(12)
                    .shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(120) })
                    .inner_margin(egui::Margin { left: 12, right: 16, top: 9, bottom: 9 })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (g, c) = if failed { (icon::INFO, theme::DANGER) } else { (icon::CHECK, theme::SUCCESS) };
                            ui.label(egui::RichText::new(g).font(theme::icon_font(18.0)).color(c));
                            ui.label(egui::RichText::new(msg.as_str()).font(theme::medium(13.5)).color(theme::TEXT));
                        });
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(30));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Video,
    Guide,
    Shortcuts,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Background {
    White,
    Transparent,
    Dark,
}

impl Background {
    fn rgba(self) -> [u8; 4] {
        match self {
            Background::White => [255, 255, 255, 255],
            Background::Transparent => [0, 0, 0, 0],
            Background::Dark => [24, 26, 32, 255],
        }
    }
}

#[derive(Clone, Copy)]
struct NewCanvas {
    width: u32,
    height: u32,
    background: Background,
    /// Width and height whose ratio editing either side keeps (the padlock is shut).
    ratio: Option<(u32, u32)>,
}

impl Default for NewCanvas {
    fn default() -> Self {
        Self { width: 1280, height: 800, background: Background::White, ratio: None }
    }
}

/// Sides a new canvas can have, in pixels.
const CANVAS_SIDES: std::ops::RangeInclusive<u32> = 16..=8000;

/// One preset size in the New form: its name, and its size on the right.
fn size_row(ui: &mut egui::Ui, selected: bool, name: &str, size: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, format!("{name} · {size}")));
    let fill = if selected {
        theme::ACCENT
    } else if resp.hovered() {
        theme::SURFACE_3
    } else {
        Color32::TRANSPARENT
    };
    let (text, faint) = if selected { (Color32::WHITE, Color32::from_white_alpha(210)) } else { (theme::TEXT, theme::TEXT_MUTED) };
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, fill);
    let font = FontId::proportional(14.0);
    painter.text(rect.left_center() + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, name, font.clone(), text);
    painter.text(rect.right_center() - Vec2::new(12.0, 0.0), Align2::RIGHT_CENTER, size, font, faint);
    resp
}

/// Width and height boxes for a size of your own, joined by a padlock that
/// keeps their ratio while it is shut.
fn custom_size(ui: &mut egui::Ui, nc: &mut NewCanvas) {
    const LOCK_W: f32 = 30.0;
    const LABEL_W: f32 = 52.0;
    let left = ui.cursor().min.x;
    let mut row_y = [0.0; 2];
    let mut changed = [false; 2];
    ui.horizontal(|ui| {
        ui.add_space(LOCK_W);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            for (i, name) in ["Width", "Height"].into_iter().enumerate() {
                ui.horizontal(|ui| {
                    let label = ui
                        .allocate_ui_with_layout(Vec2::new(LABEL_W, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_width(LABEL_W);
                            ui.label(egui::RichText::new(name).color(theme::TEXT_MUTED))
                        })
                        .inner;
                    let value = if i == 0 { &mut nc.width } else { &mut nc.height };
                    let field;
                    (field, changed[i]) = px_field(ui, value, name);
                    row_y[i] = field.labelled_by(label.id).rect.center().y;
                });
            }
        });
    });
    if let Some((w, h)) = nc.ratio {
        let scale = |v: u32, from: u32, to: u32| {
            ((v as f32 * to as f32 / from as f32).round() as u32).clamp(*CANVAS_SIDES.start(), *CANVAS_SIDES.end())
        };
        if changed[0] {
            nc.height = scale(nc.width, w, h);
        } else if changed[1] {
            nc.width = scale(nc.height, h, w);
        }
    }

    // The padlock between the rows, with a bracket out to each.
    let area = Rect::from_min_max(Pos2::new(left, row_y[0]), Pos2::new(left + LOCK_W, row_y[1]));
    let resp = ui.interact(area, ui.make_persistent_id("keep-ratio"), Sense::click());
    if resp.clicked() {
        nc.ratio = if nc.ratio.is_some() { None } else { Some((nc.width, nc.height)) };
    }
    let locked = nc.ratio.is_some();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, locked, "Keep the ratio"));
    let color = if locked {
        theme::ACCENT
    } else if resp.hovered() {
        theme::TEXT_MUTED
    } else {
        theme::TEXT_FAINT
    };
    let (x, mid, end) = (area.center().x, area.center().y, area.right() - 4.0);
    let stroke = Stroke::new(1.0, color);
    let painter = ui.painter();
    for (from, to) in [
        (Pos2::new(end, row_y[0]), Pos2::new(x, row_y[0])),
        (Pos2::new(x, row_y[0]), Pos2::new(x, mid - 12.0)),
        (Pos2::new(x, mid + 10.0), Pos2::new(x, row_y[1])),
        (Pos2::new(x, row_y[1]), Pos2::new(end, row_y[1])),
    ] {
        painter.line_segment([from, to], stroke);
    }
    padlock(painter, Pos2::new(x, mid), locked, color);
    resp.on_hover_text(if locked { "Keeping the ratio. Click to set each side on its own." } else { "Keep the ratio" });
}

/// A box to type one side of the canvas in pixels. The value follows every
/// valid keystroke; leaving the box brings an out-of-range number back in
/// range. Returns the box and whether the user changed the value.
fn px_field(ui: &mut egui::Ui, value: &mut u32, name: &str) -> (egui::Response, bool) {
    let id = ui.make_persistent_id(("px-field", name));
    // While typing, the box keeps its own text (it may be empty or too small yet).
    let mut text = match ui.memory(|m| m.has_focus(id)) {
        true => ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| value.to_string()),
        false => value.to_string(),
    };
    let bg = ui.painter().add(egui::Shape::Noop);
    let field = ui.add_sized(
        [ui.available_width(), 30.0],
        egui::TextEdit::singleline(&mut text)
            .id(id)
            // Room for the "px" painted on the right.
            .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 10, right: 30, top: 0, bottom: 0 }))
            .align(Align2::LEFT_CENTER)
            .char_limit(4)
            .hint_text(name),
    );
    let mut changed = false;
    if field.changed() {
        text.retain(|c| c.is_ascii_digit());
        if let Ok(n) = text.parse::<u32>()
            && CANVAS_SIDES.contains(&n)
            && n != *value
        {
            *value = n;
            changed = true;
        }
    }
    if field.lost_focus() {
        let n = text.parse::<u32>().unwrap_or(*value).clamp(*CANVAS_SIDES.start(), *CANVAS_SIDES.end());
        changed |= n != *value;
        *value = n;
    }
    if field.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(id));
    }
    let edge = if field.has_focus() {
        theme::ACCENT
    } else if field.hovered() {
        theme::TEXT_FAINT
    } else {
        theme::BORDER_STRONG
    };
    let fill = ui.visuals().extreme_bg_color;
    ui.painter().set(bg, egui::epaint::RectShape::new(field.rect, 8, fill, Stroke::new(1.0, edge), StrokeKind::Inside));
    ui.painter().text(field.rect.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, "px", FontId::proportional(13.0), theme::TEXT_FAINT);
    (field, changed)
}

/// A small padlock centred on `at`: shut, or open with its shackle raised.
fn padlock(painter: &egui::Painter, at: Pos2, shut: bool, color: Color32) {
    use std::f32::consts::PI;
    let body = Rect::from_center_size(at + Vec2::new(0.0, 3.0), Vec2::new(12.0, 9.5));
    painter.rect_filled(body, 2.0, color);
    let (r, top) = (3.6, body.top() - if shut { 3.0 } else { 5.5 });
    let mut shackle = vec![Pos2::new(at.x - r, body.top())];
    shackle.extend((0..=12).map(|k| {
        let a = PI + PI * k as f32 / 12.0;
        Pos2::new(at.x + r * a.cos(), top + r * a.sin())
    }));
    // Open, the right leg stops short of the body.
    shackle.push(Pos2::new(at.x + r, if shut { body.top() } else { top + 2.0 }));
    painter.add(egui::Shape::line(shackle, Stroke::new(1.6, color)));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LibFilter {
    All,
    Images,
    Videos,
    Guides,
}

/// Large capture-mode button for the welcome screen.
fn mode_tile(ui: &mut egui::Ui, rect: Rect, glyph: &str, label: &str, keys: &str) -> egui::Response {
    let resp = ui.interact(rect, ui.scope_id().with(("mode", label)), Sense::click());
    // Painted by hand, so name it for screen readers.
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    let lifted = rect.translate(Vec2::new(0.0, -2.0 * t));
    p.rect_filled(lifted, 14.0, lerp(theme::SURFACE, theme::SURFACE_2, t));
    p.rect_stroke(lifted, 14.0, Stroke::new(1.0, lerp(theme::BORDER, theme::ACCENT, t)), StrokeKind::Inside);
    let badge = Rect::from_center_size(lifted.center() - Vec2::new(0.0, 16.0), Vec2::splat(40.0));
    theme::gradient_rect(p, badge, 12, Color32::WHITE);
    p.text(badge.center(), Align2::CENTER_CENTER, glyph, theme::icon_font(22.0), Color32::WHITE);
    p.text(lifted.center() + Vec2::new(0.0, 20.0), Align2::CENTER_CENTER, label, theme::medium(13.0), theme::TEXT);
    p.text(lifted.center() + Vec2::new(0.0, 37.0), Align2::CENTER_CENTER, keys, FontId::proportional(11.0), theme::TEXT_FAINT);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Pill-shaped search field; icon and text share one vertical center.
fn search_box(ui: &mut egui::Ui, text: &mut String) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(220.0, 32.0), Sense::hover());
    let edit_rect = Rect::from_min_max(rect.min + Vec2::new(32.0, 0.0), rect.max - Vec2::new(28.0, 0.0));
    let id = ui.scope_id().with("library-search");
    let focused = ui.memory(|m| m.has_focus(id));
    // Paint the pill before adding the text field so it sits underneath.
    let p = ui.painter().clone();
    p.rect_filled(rect, 9.0, Color32::from_rgb(11, 13, 17));
    p.rect_stroke(
        rect,
        9.0,
        Stroke::new(if focused { 1.5 } else { 1.0 }, if focused { theme::ACCENT } else { theme::BORDER }),
        StrokeKind::Inside,
    );
    ui.put(
        edit_rect,
        egui::TextEdit::singleline(text)
            .id(id)
            .hint_text("Search")
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::ZERO)
            .align(Align2::LEFT_CENTER)
            .min_size(edit_rect.size()),
    );
    p.text(
        rect.left_center() + Vec2::new(17.0, 0.0),
        Align2::CENTER_CENTER,
        icon::SEARCH,
        theme::icon_font(18.0),
        if focused { theme::TEXT_MUTED } else { theme::TEXT_FAINT },
    );
    if !text.is_empty() {
        let clear = Rect::from_center_size(rect.right_center() - Vec2::new(15.0, 0.0), Vec2::splat(20.0));
        let c = ui.interact(clear, ui.scope_id().with("search-clear"), Sense::click());
        let color = if c.hovered() { theme::TEXT } else { theme::TEXT_FAINT };
        p.text(clear.center(), Align2::CENTER_CENTER, icon::CLOSE, theme::icon_font(16.0), color);
        if c.on_hover_text("Clear").clicked() {
            text.clear();
        }
    }
}

/// Keyboard-key looking chip that starts shortcut recording.
fn shortcut_chip(ui: &mut egui::Ui, text: &str, recording: bool) -> egui::Response {
    let font = theme::medium(12.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, theme::TEXT);
    let size = Vec2::new((galley.size().x + 24.0).max(76.0), 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    if recording {
        let t = ui.input(|i| i.time) as f32;
        let pulse = (t * 4.0).sin() * 0.5 + 0.5;
        p.rect_filled(rect, 7.0, theme::ACCENT.gamma_multiply(0.18 + 0.12 * pulse));
        p.rect_stroke(rect, 7.0, Stroke::new(1.5, theme::ACCENT), StrokeKind::Inside);
        // ~30 fps is plenty for the pulse; an unbounded repaint busy-loops while waiting.
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    } else {
        p.rect_filled(rect, 7.0, lerp(theme::SURFACE_3, Color32::from_rgb(46, 52, 66), hover));
        p.rect_stroke(rect, 7.0, Stroke::new(1.0, theme::BORDER_STRONG), StrokeKind::Inside);
        // Key-cap bottom edge.
        p.line_segment(
            [rect.left_bottom() + Vec2::new(6.0, -1.0), rect.right_bottom() + Vec2::new(-6.0, -1.0)],
            Stroke::new(1.0, Color32::from_black_alpha(90)),
        );
    }
    let color = if recording { Color32::WHITE } else { theme::TEXT };
    p.galley(rect.center() - galley.size() / 2.0, galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// ⌘-shortcuts by the key's place, when the keyboard layout types something
/// other than a letter there: the Hebrew layout has ' on W and / on Q, so ⌘W
/// would arrive as ⌘' and close nothing. (Keys that type a non-Latin letter
/// already arrive by their place; layouts that move letters about, such as
/// French AZERTY, keep their own letters.)
fn latin_shortcuts(raw: &mut egui::RawInput) {
    let is_letter = |k: egui::Key| matches!(k.name().as_bytes(), [b'A'..=b'Z']);
    for event in &mut raw.events {
        if let egui::Event::Key { key, physical_key: Some(place), modifiers, .. } = event
            && modifiers.command
            && is_letter(*place)
            && !is_letter(*key)
        {
            *key = *place;
        }
    }
}

impl eframe::App for RegaApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        latin_shortcuts(raw);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Before `advance_activity`, so it runs a pass after `show_overlay`.
        self.present_overlay(ctx, frame);
        while let Ok(ev) = self.events.try_recv() {
            self.handle_event(ev, ctx, frame);
        }
        self.sync_capture_zoom(ctx);
        while let Ok((path, modified, img)) = self.thumb_results.try_recv() {
            // Mipmapped: drawn smaller than it's made, it stays smooth.
            let options = egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
            let tex = ctx.load_texture(path.to_string_lossy(), img, options);
            self.thumbs.insert(path, (modified, tex));
        }
        self.report_background_saves();
        self.tick_updates(ctx);
        self.tick_install(ctx);
        self.advance_activity(ctx, frame);
        // Primed for a capture that isn't coming (no permission, a recording
        // under way, a full-screen shot that needed no overlay): as it was.
        if platform::is_primed() && !matches!(self.activity, Activity::Pending { .. }) && self.overlay_to_present.is_none() {
            platform::unprime_window(frame, self.win == Win::Hidden);
            if self.win == Win::Normal && self.reveal_in.is_none() {
                platform::set_window_alpha(frame, 1.0);
            }
        }
        self.sync_capture_zoom(ctx);

        if ctx.input(|i| i.viewport().close_requested()) {
            // Closing the window keeps Rega running in the menu bar.
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.close_window(ctx);
        }
        if self.win == Win::Normal && self.library_scanned.elapsed() > Duration::from_secs(5) {
            self.refresh_library();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.hover_tracking {
            platform::enable_hover_when_inactive(frame);
            self.hover_tracking = true;
            // On a small or highly scaled screen (e.g. 300%: ~1008x633 points)
            // the default window would be bigger than the monitor.
            if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
                let fit = Vec2::new(monitor.x.min(1360.0), (monitor.y - 80.0).min(840.0)).max(Vec2::new(640.0, 400.0));
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(fit.x.min(1300.0), fit.y.min(640.0))));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(fit));
                self.min_size = Vec2::new(fit.x.min(1300.0), fit.y.min(640.0));
            }
        }
        if let Activity::Selecting(overlay) = &mut self.activity {
            overlay.take_arrivals(&ctx);
            // The overlay must cover the whole monitor, Dock and menu bar included.
            let (ox, oy) = (overlay.snap.origin.0 as f32, overlay.snap.origin.1 as f32);
            let (sw, sh) = (overlay.snap.size.0 as f32, overlay.snap.size.1 as f32);
            let want = Rect::from_min_size(Pos2::new(ox, oy), Vec2::new(sw, sh));
            if let Some(have) = ctx.input(|i| i.viewport().outer_rect)
                && (have.min - want.min).length() + (have.size() - want.size()).length() > 1.0
            {
                platform::set_window_frame(frame, ox, oy, sw, sh);
                if !cfg!(target_os = "macos") {
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(want.size()));
                    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(want.min));
                }
            }
            let mut outcome = egui::CentralPanel::no_frame().show(ui, |ui| overlay.ui(ui)).inner;
            // Every other screen has an overlay of its own, in a window of its
            // own: nothing moves between screens, so nothing flashes or lags.
            let mut chosen = None;
            for (i, extra) in overlay.extras.iter_mut().enumerate() {
                let (o, s) = (extra.snap.origin, extra.snap.size);
                let title = format!("{APP_NAME} overlay {}", i + 2);
                let builder = egui::ViewportBuilder::default()
                    .with_title(title.clone())
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_position(Pos2::new(o.0 as f32, o.1 as f32))
                    .with_inner_size(Vec2::new(s.0 as f32, s.1 as f32))
                    .with_always_on_top()
                    .with_taskbar(false)
                    .with_active(false);
                let id = egui::ViewportId::from_hash_of(("rega-overlay", o));
                let extra_outcome = ctx.show_viewport_immediate(id, builder, |ui, _| egui::CentralPanel::no_frame().show(ui, |ui| extra.ui(ui)).inner);
                if !extra.presented {
                    // macOS: above the menu bar and Dock, on the Space in use, like the main one.
                    extra.presented = platform::present_extra_overlay(&title, o.0 as f32, o.1 as f32, s.0 as f32, s.1 as f32);
                }
                match extra_outcome {
                    Outcome::Pending => {}
                    Outcome::Cancel => outcome = Outcome::Cancel,
                    Outcome::Color(c) => outcome = Outcome::Color(c),
                    Outcome::Selected(rect) => chosen = Some((i, rect)),
                }
            }
            if !matches!((&outcome, &chosen), (Outcome::Pending, None)) {
                // The capture is over and the other screens' windows close with it.
                platform::release_extra_overlays();
            }
            match (outcome, chosen) {
                (Outcome::Cancel, _) => {
                    self.activity = Activity::Idle;
                    self.return_to_rest(&ctx, frame);
                }
                // Already on the clipboard: back to where the user was.
                (Outcome::Color(color), _) => {
                    self.activity = Activity::Idle;
                    self.return_to_rest(&ctx, frame);
                    self.toast(format!("Copied {}", crate::loupe::hex(color)));
                }
                (Outcome::Selected(rect), _) => {
                    if let Activity::Selecting(overlay) = std::mem::replace(&mut self.activity, Activity::Idle) {
                        self.on_selection(&ctx, frame, &overlay, rect);
                    }
                }
                (Outcome::Pending, Some((i, rect))) => {
                    if let Activity::Selecting(mut overlay) = std::mem::replace(&mut self.activity, Activity::Idle) {
                        let picked = overlay.extras.swap_remove(i);
                        self.on_selection(&ctx, frame, &picked, rect);
                    }
                }
                (Outcome::Pending, None) => {}
            }
            return;
        }
        if matches!(self.activity, Activity::Recording(_)) {
            platform::paint_recording_frame(&ctx);
            self.recording_bar_ui(ui);
            return;
        }
        if matches!(self.activity, Activity::Guiding(_)) {
            platform::paint_recording_frame(&ctx);
            self.guide_bar_ui(ui);
            return;
        }
        if self.win != Win::Normal {
            return;
        }
        self.refresh_permissions(false);
        if self.show_permissions || self.perm_status.iter().any(|(p, ok)| !ok && !p.optional()) {
            // Notice a grant made in System Settings without needing input. A skipped
            // optional permission must not keep the idle app redrawing every second.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        self.workspace_ui(ui);
        self.permissions_window(&ctx);
        self.home_shortcuts(&ctx);
        self.handle_drops(&ctx);
        self.new_canvas_dialog(&ctx);
        self.settings_window(&ctx);
        self.about_window(&ctx);
        self.ocr_window(&ctx);
        self.toast_ui(&ctx);
    }

    /// Quitting some other way than Rega's own Quit: from the Dock, or by
    /// logging out or shutting down.
    fn on_exit(&mut self) {
        self.prepare_to_quit();
    }
}

fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else {
        "Show in folder"
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

type HotkeyMap = Arc<Mutex<HashMap<u32, AppEvent>>>;

fn setup_hotkeys(ctx: &egui::Context, tx: Sender<AppEvent>) -> (Option<GlobalHotKeyManager>, HotkeyMap, Option<String>) {
    let map: HotkeyMap = Arc::default();
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => return (None, map, Some(format!("Global shortcuts unavailable: {e}"))),
    };
    let ctx = ctx.clone();
    let lookup = map.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
        if e.state() == HotKeyState::Pressed
            && let Some(ev) = lookup.lock().ok().and_then(|m| m.get(&e.id()).copied())
        {
            if matches!(ev, AppEvent::Capture(_) | AppEvent::FullScreen) {
                // From the menu bar, start at once (see the function).
                platform::prime_hidden_window();
            }
            let _ = tx.send(ev);
            ctx.request_repaint();
        }
    }));
    (Some(manager), map, None)
}

fn menu_label(action: Action, shortcuts: &shortcuts::Shortcuts) -> String {
    let title = match action {
        Action::Image => "Capture Image",
        Action::FullScreen => "Capture Full Screen",
        Action::Video => "Record Video",
        Action::Scroll => "Scrolling Capture",
        Action::Guide => "Record Guide",
        Action::Text => "Grab Text",
        Action::Color => "Pick Color",
    };
    let keys = shortcuts.display(action);
    if keys.is_empty() { title.to_owned() } else { format!("{title}    {keys}") }
}


/// The menu-bar status item (with the colored app icon) and the app's own
/// menu bar, both offering every capture mode.
fn setup_menus(
    ctx: &egui::Context,
    tx: Sender<AppEvent>,
) -> (Option<tray_icon::TrayIcon>, tray_icon::menu::Menu, Vec<(Action, tray_icon::menu::MenuItem)>) {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    let mut map: HashMap<String, AppEvent> = Action::ALL.iter().map(|a| (format!("{a:?}"), a.event())).collect();
    map.insert("show".into(), AppEvent::ShowMain);
    map.insert("about".into(), AppEvent::About);
    map.insert("quit".into(), AppEvent::Quit);
    // Labels are filled in by `apply_hotkeys`.
    let mut all_items = Vec::new();
    let mut capture_items = |prefix: &str| -> Vec<MenuItem> {
        Action::ALL
            .iter()
            .map(|a| {
                let item = MenuItem::with_id(format!("{prefix}{a:?}"), a.label(), true, None);
                all_items.push((*a, item.clone()));
                item
            })
            .collect()
    };

    // Status item menu
    let tray_menu = Menu::new();
    for item in capture_items("") {
        let _ = tray_menu.append(&item);
    }
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&MenuItem::with_id("show", "Open Rega", true, None));
    let _ = tray_menu.append(&MenuItem::with_id("about", "About Rega", true, None));
    let _ = tray_menu.append(&MenuItem::with_id("quit", "Quit Rega", true, None));

    // App menu bar: Rega | Capture | Window
    let menu_bar = Menu::new();
    let app_menu = Submenu::new(APP_NAME, true);
    let _ = app_menu.append_items(&[
        &MenuItem::with_id("about", "About Rega", true, None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::services(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::show_all(None),
        &PredefinedMenuItem::separator(),
        // This menu replaces winit's default one, so ⌘Q must be bound here.
        &MenuItem::with_id(
            "quit",
            "Quit Rega",
            true,
            Some(tray_icon::menu::accelerator::Accelerator::new(
                tray_icon::menu::accelerator::Modifiers::META,
                tray_icon::menu::accelerator::Code::KeyQ,
            )),
        ),
    ]);
    // No Edit menu on purpose: its ⌘C/⌘V key equivalents would swallow the
    // keystrokes before the editor and text fields see them.
    let capture_menu = Submenu::new("Capture", true);
    for item in capture_items("bar-") {
        let _ = capture_menu.append(&item);
    }
    let file_menu = Submenu::new("File", true);
    {
        use tray_icon::menu::accelerator::{Accelerator, Code, Modifiers};
        let cmd = |code| Some(Accelerator::new(Modifiers::META, code));
        let _ = file_menu.append_items(&[
            &MenuItem::with_id("new-image", "New Image…", true, cmd(Code::KeyN)),
            &MenuItem::with_id("open-image", "Open Image…", true, cmd(Code::KeyO)),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("close-item", "Close", true, cmd(Code::KeyW)),
            &MenuItem::with_id("delete-item", "Delete…", true, cmd(Code::KeyD)),
        ]);
    }
    map.insert("new-image".into(), AppEvent::NewImage);
    map.insert("open-image".into(), AppEvent::OpenImage);
    map.insert("close-item".into(), AppEvent::CloseItem);
    map.insert("delete-item".into(), AppEvent::DeleteItem);
    let window_menu = Submenu::new("Window", true);
    let _ = window_menu.append_items(&[
        &MenuItem::with_id("show", "Library", true, None),
        &PredefinedMenuItem::minimize(None),
    ]);
    let _ = menu_bar.append_items(&[&app_menu, &file_menu, &capture_menu, &window_menu]);
    #[cfg(target_os = "macos")]
    menu_bar.init_for_nsapp();

    let ctx2 = ctx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let id = e.id().as_ref();
        if let Some(ev) = map.get(id.strip_prefix("bar-").unwrap_or(id)) {
            let _ = tx.send(*ev);
            ctx2.request_repaint();
        }
    }));

    platform::prefer_status_item_on_right(APP_NAME);
    let (rgba, w, h) = crate::icon::app_icon(44);
    let tray = tray_icon::Icon::from_rgba(rgba, w, h).ok().and_then(|icon| {
        tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_icon(icon)
            .with_tooltip(APP_NAME)
            .with_autosave_name(APP_NAME)
            .build()
            .map_err(|e| log::warn!("tray icon: {e}"))
            .ok()
    });
    (tray, menu_bar, all_items)
}

type ThumbResult = (PathBuf, SystemTime, egui::ColorImage);

/// A library card's picture, in points.
const CARD: Vec2 = Vec2::new(156.0, 100.0);

/// What a library card shows: the part of `img` with the card's shape (the
/// top of a tall capture, the middle of a wide one), at 3× the card so it's
/// sharp on any screen, scaled smoothly. A small image isn't enlarged.
fn card_picture(img: &image::DynamicImage) -> RgbaImage {
    let (cw, ch) = ((CARD.x * 3.0) as u32, (CARD.y * 3.0) as u32);
    let (w, h) = (img.width().max(1), img.height().max(1));
    let (crop_w, crop_h) = if w as u64 * ch as u64 > h as u64 * cw as u64 {
        (((h as u64 * cw as u64) / ch as u64).max(1) as u32, h)
    } else {
        (w, ((w as u64 * ch as u64) / cw as u64).max(1) as u32)
    };
    let part = img.crop_imm((w - crop_w) / 2, 0, crop_w, crop_h);
    if crop_w <= cw {
        part.to_rgba8()
    } else {
        part.resize_exact(cw, ch, image::imageops::FilterType::CatmullRom).to_rgba8()
    }
}

fn spawn_thumbnailer(ctx: egui::Context) -> (Sender<(PathBuf, SystemTime)>, Receiver<ThumbResult>) {
    let (job_tx, job_rx) = mpsc::channel::<(PathBuf, SystemTime)>();
    let (res_tx, res_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (path, modified) in job_rx {
            let kind = library::kind_of(&path);
            let img = if kind == Some(ItemKind::Video) {
                match crate::player::first_frame(&path) {
                    Ok(frame) => image::DynamicImage::ImageRgba8(frame),
                    Err(_) => continue,
                }
            } else if kind == Some(ItemKind::Guide) {
                match crate::guide::first_shot(&path) {
                    Ok(shot) => image::DynamicImage::ImageRgba8(shot),
                    Err(_) => continue,
                }
            } else {
                // GIFs open as their first frame.
                let Ok(img) = image::open(&path) else { continue };
                img
            };
            let thumb = card_picture(&img);
            let color = egui::ColorImage::from_rgba_unmultiplied([thumb.width() as usize, thumb.height() as usize], thumb.as_raw());
            if res_tx.send((path, modified, color)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    (job_tx, res_rx)
}

#[cfg(test)]
mod readme_shots;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use eframe::egui;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::{Activity, RegaApp, Win};
    use crate::recorder::{Finished, Recorder};

    /// ⌘W and ⌘Q work with the Hebrew layout, which types ' and / on those
    /// keys; other layouts' letters, and keys pressed without ⌘, are left alone.
    #[test]
    fn shortcuts_follow_the_key_with_a_hebrew_layout() {
        use egui::{Event, Key, Modifiers};
        let press = |key, place, modifiers| Event::Key { key, physical_key: Some(place), pressed: true, repeat: false, modifiers };
        let mut raw = egui::RawInput {
            events: vec![
                press(Key::Quote, Key::W, Modifiers::COMMAND),
                press(Key::Slash, Key::Q, Modifiers::COMMAND),
                // French AZERTY: ⌘Q is where QWERTY has A, and stays ⌘Q.
                press(Key::Q, Key::A, Modifiers::COMMAND),
                // Without ⌘, a ' is a '.
                press(Key::Quote, Key::W, Modifiers::NONE),
                // ⌘/ on a US keyboard stays ⌘/.
                press(Key::Slash, Key::Slash, Modifiers::COMMAND),
            ],
            ..Default::default()
        };
        super::latin_shortcuts(&mut raw);
        let keys: Vec<Key> = raw.events.iter().filter_map(|e| if let Event::Key { key, .. } = e { Some(*key) } else { None }).collect();
        assert_eq!(keys, [Key::W, Key::Q, Key::Q, Key::Quote, Key::Slash]);
    }

    /// Which hover check works for a card inside the bottom tray's scroll area?
    #[test]
    fn hover_detection_in_tray() {
        let ctx = egui::Context::default();
        let mut results = (false, false);
        for frame in 0..8 {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
                time: Some(frame as f64 * 0.4),
                ..Default::default()
            };
            input.events.push(egui::Event::PointerMoved(egui::pos2(60.0, 520.0)));
            let mut out = ctx.run_ui(input, |ui| {
                egui::Panel::bottom("tray").exact_size(200.0).show(ui, |ui| {
                    egui::ScrollArea::horizontal().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(156.0, 128.0), egui::Sense::click());
                            if frame >= 2 {
                                results = (ui.rect_contains_pointer(rect), resp.contains_pointer());
                            }
                            resp.on_hover_text("name\ntime");
                        });
                    });
                });
                egui::CentralPanel::default().show(ui, |_| {});
            });
            out.textures_delta.clear();
        }
        assert!(results.1, "response hover check should see the pointer");
    }

    /// The app on its recording bar, with `rec` recording. `from_outside`: it
    /// was started from a shortcut or the tray, so ending it goes back to the
    /// app the user was in, unless there's something to show.
    fn recording(name: &str, rec: Recorder, from_outside: bool) -> Harness<'static, RegaApp> {
        let lib = std::env::temp_dir().join(format!("rega-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        Harness::builder()
            .with_size(super::BAR_SIZE)
            .with_max_steps(400)
            .with_accessibility_check(false)
            .wgpu()
            .build_eframe(move |cc| {
                let mut app = RegaApp::demo(&cc.egui_ctx, lib);
                // Opening the video would start the system player in a test.
                app.settings.focus_after_capture = false;
                app.activity = Activity::Recording(rec);
                app.win = Win::Bar;
                app.hidden_by_user = from_outside;
                app
            })
    }

    /// Step until the recording is over (its stand-in thread ends at once).
    fn until_idle(h: &mut Harness<'_, RegaApp>) {
        for _ in 0..200 {
            h.step();
            if matches!(h.state().activity, Activity::Idle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the recording should have ended");
    }

    fn toast(h: &Harness<'_, RegaApp>) -> Option<String> {
        h.state().toast.as_ref().map(|(msg, _)| msg.clone())
    }

    /// A newer version on the website shows a notice over the workspace,
    /// until the user closes it for that version.
    #[test]
    fn update_notice() {
        let lib = std::env::temp_dir().join(format!("rega-update-notice-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let mut h = Harness::builder()
            .with_size(egui::vec2(1360.0, 840.0))
            .with_accessibility_check(false)
            .wgpu()
            .build_eframe(move |cc| RegaApp::demo(&cc.egui_ctx, lib));
        h.step();
        // Tests never reach out to the website.
        assert!(h.state().update_due.is_none());
        let title = "Rega 9.0.0 is available";
        assert!(h.query_by_label(title).is_none());

        h.state_mut().update = Some(crate::update::Latest { version: "9.0.0".into(), title: "Big things.".into(), downloads: Default::default() });
        h.step();
        assert!(h.query_by_label(title).is_some());
        assert!(h.query_by_label("Big things. You have 0.4.1.".replace("0.4.1", env!("CARGO_PKG_VERSION")).as_str()).is_some());

        h.state_mut().settings.dismissed_update = Some("9.0.0".into());
        h.step();
        assert!(h.query_by_label(title).is_none());
    }

    /// The home screen's capture tiles, Pick color among them, all fit in the
    /// window side by side, shrinking in a narrow one. Set
    /// `HOME_PREVIEW_DIR` to save both.
    #[test]
    fn home_tiles_fit_the_window() {
        for width in [1360.0, 760.0] {
            let lib = std::env::temp_dir().join(format!("rega-home-tiles-{}-{width}", std::process::id()));
            std::fs::create_dir_all(&lib).unwrap();
            let mut h = Harness::builder()
                .with_size(egui::vec2(width, 760.0))
                .with_accessibility_check(false)
                .wgpu()
                .build_eframe(move |cc| RegaApp::demo(&cc.egui_ctx, lib));
            h.step();
            h.step();
            if let Ok(dir) = std::env::var("HOME_PREVIEW_DIR") {
                h.render().unwrap().save(std::path::Path::new(&dir).join(format!("home-{width}.png"))).unwrap();
            }
            // The first, second and last two tiles (the others share their
            // names with toolbar buttons).
            let tiles: Vec<egui::Rect> = ["Region", "Full screen", "Grab text", "Pick color"]
                .iter()
                .map(|label| h.get_by_label(label).rect())
                .collect();
            for (r, next) in tiles.iter().zip(tiles.iter().skip(1)) {
                assert!(r.max.x <= next.min.x, "tiles overlap at {width}: {r:?} {next:?}");
            }
            assert!(tiles[0].min.x >= 0.0 && tiles[3].max.x <= width, "off screen at {width}: {tiles:?}");
        }
    }

    /// A card's picture has the card's shape at 3× its size: the middle of a
    /// wide screenshot, the top of a tall scrolling capture, a small image as is.
    #[test]
    fn card_pictures_are_sharp_and_card_shaped() {
        use super::{CARD, card_picture};
        let (cw, ch) = ((CARD.x * 3.0) as u32, (CARD.y * 3.0) as u32);
        let wide = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(3024, 1964, |x, _| image::Rgba([(x / 12) as u8, 0, 0, 255])));
        let pic = card_picture(&wide);
        assert_eq!(pic.dimensions(), (cw, ch));
        // A tall capture: red on top, blue below; the card shows the top.
        let tall = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(1200, 20000, |_, y| {
            image::Rgba(if y < 400 { [255, 0, 0, 255] } else { [0, 0, 255, 255] })
        }));
        let pic = card_picture(&tall);
        assert_eq!(pic.dimensions(), (cw, ch));
        assert_eq!(pic.get_pixel(cw / 2, 10).0, [255, 0, 0, 255], "the top of the page");
        assert!(pic.height() < 20000 / 10, "not squeezed into a strip");
        let small = image::DynamicImage::ImageRgba8(image::RgbaImage::new(120, 80));
        assert_eq!(card_picture(&small).dimensions(), (120, 76), "cropped to the card's shape, not enlarged");
    }

    /// The New form: a preset sets both sides; with the padlock shut, typing
    /// one side scales the other; open, each side is its own; a size out of
    /// range comes back in range when its box loses focus. Set
    /// `NEW_FORM_PREVIEW_DIR` to save the form.
    #[test]
    fn new_canvas_sizes_and_kept_ratio() {
        use egui::accesskit::Role;
        let lib = std::env::temp_dir().join(format!("rega-new-form-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let mut h = Harness::builder()
            .with_size(egui::vec2(1360.0, 840.0))
            .with_pixels_per_point(2.0)
            .with_accessibility_check(false)
            .wgpu()
            .build_eframe(move |cc| RegaApp::demo(&cc.egui_ctx, lib));
        h.step();
        h.get_by_label("New").click();
        h.run();
        let size = |h: &Harness<'_, RegaApp>| (h.state().new_canvas.width, h.state().new_canvas.height);
        let set = |h: &mut Harness<'_, RegaApp>, side: &str, text: &str| {
            h.get_by_role_and_label(Role::TextInput, side).focus();
            h.run();
            h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
            h.get_by_role_and_label(Role::TextInput, side).type_text(text);
            h.run();
        };

        h.get_by_label("Full HD · 1920 × 1080").click();
        h.run();
        assert_eq!(size(&h), (1920, 1080));

        h.get_by_label("Keep the ratio").click();
        h.run();
        set(&mut h, "Width", "1280");
        assert_eq!(size(&h), (1280, 720), "the height follows at 16:9");
        set(&mut h, "Height", "1080");
        assert_eq!(size(&h), (1920, 1080), "and the width follows the height");
        if let Ok(dir) = std::env::var("NEW_FORM_PREVIEW_DIR") {
            h.hover_at(egui::pos2(1200.0, 300.0));
            h.run();
            h.render().unwrap().save(std::path::Path::new(&dir).join("new-form-locked.png")).unwrap();
        }

        h.get_by_label("Keep the ratio").click();
        h.run();
        set(&mut h, "Width", "1000");
        assert_eq!(size(&h), (1000, 1080), "open, the height stays");

        set(&mut h, "Height", "5");
        assert_eq!(size(&h), (1000, 1080), "5 px is too small to take while typing");
        h.key_press(egui::Key::Tab);
        h.run();
        assert_eq!(size(&h), (1000, 16), "leaving the box makes it the smallest side");
        if let Ok(dir) = std::env::var("NEW_FORM_PREVIEW_DIR") {
            h.render().unwrap().save(std::path::Path::new(&dir).join("new-form.png")).unwrap();
        }
    }

    fn video(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rega-{name}-videos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Rega 2026-11-04 at 10.00.00.mp4");
        crate::recorder::write_test_video(&path, 10, 10, (96, 64));
        path
    }

    /// A recording that stops by itself (the screen gone) leaves the bar at
    /// once and says why, rather than counting on until Stop.
    #[test]
    fn a_recording_that_stops_by_itself_ends_at_once() {
        let path = video("early");
        let saved = path.clone();
        let rec = Recorder::fake(path.clone(), move |_| {
            Ok(Finished::Saved { path: saved, stopped_early: Some("the screen could no longer be captured".into()) })
        });
        let mut h = recording("early", rec, false);
        until_idle(&mut h);
        assert_eq!(toast(&h).as_deref(), Some("Video saved, but the recording stopped early: the screen could no longer be captured"));
        assert!(h.state().win != Win::Bar, "the bar is gone");
        assert!(path.exists());
    }

    /// Discard pressed as the recording stopped by itself wins: no video.
    #[test]
    fn discard_wins_over_a_recording_that_stopped_by_itself() {
        let path = video("discard");
        let saved = path.clone();
        let rec = Recorder::fake(path.clone(), move |_| {
            Ok(Finished::Saved { path: saved, stopped_early: Some("the screen could no longer be captured".into()) })
        });
        while !rec.has_ended() {
            std::thread::sleep(Duration::from_millis(2));
        }
        rec.discard();
        let mut h = recording("discard", rec, false);
        until_idle(&mut h);
        assert_eq!(toast(&h).as_deref(), Some("Recording discarded"));
        assert!(!path.exists());
    }

    /// A failed recording says so on screen, also one started from a
    /// shortcut or the tray, whose window would otherwise stay hidden.
    #[test]
    fn a_failed_recording_says_so() {
        for from_outside in [false, true] {
            let rec = Recorder::fake(PathBuf::new(), |_| Err(anyhow::anyhow!("no frames were captured")));
            let mut h = recording(&format!("failed-{from_outside}"), rec, from_outside);
            until_idle(&mut h);
            h.step();
            assert_eq!(toast(&h).as_deref(), Some("Recording failed: no frames were captured"));
            assert!(
                h.query_by_label("Recording failed: no frames were captured").is_some(),
                "the failure is drawn (started from outside: {from_outside})"
            );
        }
    }

    const WINDOW: egui::Vec2 = egui::vec2(1360.0, 840.0);

    /// Rega's main window on the library folder `lib`.
    fn main_window(lib: PathBuf) -> Harness<'static, RegaApp> {
        Harness::builder()
            .with_size(WINDOW)
            .with_max_steps(400)
            .with_accessibility_check(false)
            .wgpu()
            .build_eframe(move |cc| RegaApp::demo(&cc.egui_ctx, lib))
    }

    /// An empty library folder for the test `name`.
    fn library_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rega-{name}-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The names of the files in `dir`, hidden ones included, sorted.
    fn files(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// Start saving a trimmed copy of the open video as the user would: I
    /// sets the In point, then a click on "Save trimmed copy", the rightmost
    /// button of the player's bar, level with its clock. Its thread is
    /// started on the last pass.
    fn start_saving(h: &mut Harness<'_, RegaApp>) {
        h.key_press(egui::Key::I);
        h.step();
        let clock = h.get_by_label_contains(" / ").rect();
        let save = egui::pos2(h.ctx.content_rect().max.x - 40.0, clock.center().y);
        h.hover_at(save);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: save, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        }
        h.step();
    }

    /// A copy still being saved when its player closes (here, for another
    /// video) is finished, listed and announced, but not opened: the user
    /// has moved on.
    #[test]
    fn a_save_outlives_its_player() {
        let lib = library_dir("save-on");
        let (first, second) = (lib.join("Rega 2026-11-04 at 10.00.00.mp4"), lib.join("Rega 2026-11-04 at 11.00.00.mp4"));
        crate::recorder::write_test_video(&first, 10, 10, (96, 64));
        std::fs::copy(&first, &second).unwrap();
        let mut h = main_window(lib.clone());
        h.state_mut().open_video(first);
        h.step();
        start_saving(&mut h);
        h.state_mut().open_video(second.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while h.query_by_label_contains("Saved ").is_none() {
            assert!(std::time::Instant::now() < deadline, "the copy is never reported; folder: {:?}", files(&lib));
            std::thread::sleep(Duration::from_millis(10));
            h.step();
        }
        let msg = toast(&h).unwrap();
        let copy = lib.join(msg.strip_prefix("Saved ").unwrap());
        assert!(h.state().library.iter().any(|i| i.path == copy), "{} is listed", copy.display());
        assert_eq!(h.state().current_path(), Some(&second), "the copy isn't opened");
        drop(h);
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// Quitting while a copy is being written stops it and waits for it to
    /// clean up: neither the copy nor its partial file is left behind.
    #[test]
    fn quitting_cancels_a_save() {
        let lib = library_dir("quit");
        let name = "Rega 2026-11-04 at 10.00.00.mp4";
        crate::recorder::write_test_video(&lib.join(name), 300, 30, (96, 64));
        let mut h = main_window(lib.clone());
        h.state_mut().open_video(lib.join(name));
        h.step();
        start_saving(&mut h);
        // Quit once the copy's partial file is on disk.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !files(&lib).iter().any(|n| n.ends_with(".part")) {
            assert!(std::time::Instant::now() < deadline, "the copy was never seen being written; folder: {:?}", files(&lib));
            std::thread::sleep(Duration::from_millis(1));
        }
        h.state_mut().prepare_to_quit();
        assert_eq!(files(&lib), [name]);
        drop(h);
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// Every library video gets a thumbnail from its first frame: MOV as
    /// well as MP4, whatever the extension's case.
    #[test]
    fn videos_get_thumbnails() {
        let dir = std::env::temp_dir().join(format!("rega-thumbs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let videos = ["a.mp4", "b.mov", "C.MOV"].map(|name| dir.join(name));
        crate::recorder::write_test_video(&videos[0], 3, 10, (96, 64));
        for copy in &videos[1..] {
            std::fs::copy(&videos[0], copy).unwrap();
        }
        let (jobs, results) = super::spawn_thumbnailer(egui::Context::default());
        for path in &videos {
            jobs.send((path.clone(), std::time::SystemTime::UNIX_EPOCH)).unwrap();
        }
        // With no jobs to come, the thumbnailer stops once these are done.
        drop(jobs);
        let thumbnailed: Vec<PathBuf> = results.iter().map(|(path, _, _)| path).collect();
        assert_eq!(thumbnailed, videos);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The bar only shows whole seconds and a slow pulse: it must not
    /// compete with the capture thread by redrawing more than 10 times a second.
    #[test]
    fn the_recording_bar_repaints_at_most_ten_times_a_second() {
        let rec = Recorder::fake(PathBuf::new(), |stop| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(Finished::Discarded)
        });
        let mut h = recording("repaint", rec, false);
        for _ in 0..4 {
            h.step();
            let delay = h.output().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            assert!(delay >= Duration::from_millis(100), "repaints after {delay:?}");
        }
        let Activity::Recording(rec) = &h.state().activity else { panic!("still recording") };
        rec.stop();
    }
}
