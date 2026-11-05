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
use crate::editor::{Editor, EditorAction};
use crate::theme::{self, ItemStyle, icon};
use crate::library::{self, Item, ItemKind};
use crate::ocr;
use crate::overlay::{Outcome, Overlay, Purpose};
use crate::platform;
use crate::player::{BackgroundSaves, Player, PlayerAction};
use crate::recorder::{Finished, RecordConfig, Recorder};
use crate::scroll::{self, ScrollMsg};
use crate::shortcuts::{self, Action};

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
}

enum Activity {
    Idle,
    /// Window hidden, waiting for it to disappear (and for the user's delay).
    Pending { purpose: Purpose, full: bool, at: Instant, cursor: (i32, i32) },
    Selecting(Box<Overlay>),
    Recording(Recorder),
    Finishing(Recorder),
    Scrolling(Receiver<ScrollMsg>),
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
            Activity::Pending { .. } | Activity::Selecting(_) | Activity::Recording(_) | Activity::Scrolling(_)
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
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
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

    fn show_recording_bar(&mut self, ctx: &egui::Context, frame: &eframe::Frame, snap_origin: (i32, i32), monitor: (u32, u32), region: Rect) {
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
            AppEvent::Quit => {
                self.prepare_to_quit();
                ctx.send_viewport_cmd(ViewportCommand::Close);
                std::process::exit(0);
            }
        }
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
        let delay = Duration::from_secs(self.settings.delay_secs as u64).max(settle);
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
                    if self.settings.delay_secs > 0 {
                        let secs = left.as_secs_f32().ceil() as u32;
                        self.set_tray_title(Some(&secs.to_string()));
                    }
                    ctx.request_repaint_after(left.min(Duration::from_millis(100)));
                    return;
                }
                self.set_tray_title(None);
                match capture::snapshot_at(cursor) {
                    Err(e) => {
                        self.activity = Activity::Idle;
                        self.show_normal(ctx, frame);
                        self.toast(format!("Capture failed: {e:#}"));
                    }
                    Ok(snap) if full => {
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
                        self.activity = Activity::Selecting(Box::new(Overlay::new(ctx, snap, purpose)));
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
            Activity::Idle | Activity::Selecting(_) => {}
        }
    }

    fn on_selection(&mut self, ctx: &egui::Context, frame: &eframe::Frame, overlay: &Overlay, rect: Rect) {
        let snap = &overlay.snap;
        match overlay.purpose {
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
            self.autosave_editor();
            self.mode = Mode::Editor(Box::new(ed));
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

    fn autosave_editor(&mut self) {
        if let Mode::Editor(ed) = &mut self.mode
            && ed.is_dirty()
                && let Err(e) = ed.save() {
                    log::warn!("autosave failed: {e}");
                }
    }

    fn open_item(&mut self, item: &Item) {
        match item.kind {
            ItemKind::Video => self.open_video(item.path.clone()),
            ItemKind::Image => match Editor::open(item.path.clone()) {
                Ok(ed) => {
                    self.autosave_editor();
                    self.mode = Mode::Editor(Box::new(ed));
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

    fn import_image(&mut self, img: RgbaImage) {
        let path = library::new_capture_path(&self.settings.save_dir, self.settings.image_format.ext());
        let mut ed = Editor::new(path, img, None, false);
        match ed.save() {
            Ok(()) => {
                self.autosave_editor();
                self.mode = Mode::Editor(Box::new(ed));
                self.refresh_library();
            }
            Err(e) => self.toast(format!("Import failed: {e:#}")),
        }
    }

    /// Start a blank canvas in the editor (saved to the library right away).
    fn create_blank(&mut self) {
        let NewCanvas { width, height, background } = self.new_canvas;
        let img = RgbaImage::from_pixel(width.max(1), height.max(1), image::Rgba(background.rgba()));
        // A transparent canvas must stay PNG even if the default format is JPEG.
        let ext = if background == Background::Transparent { "png" } else { self.settings.image_format.ext() };
        let path = library::new_capture_path(&self.settings.save_dir, ext);
        let mut ed = Editor::new(path, img, None, false);
        match ed.save() {
            Ok(()) => {
                self.autosave_editor();
                self.mode = Mode::Editor(Box::new(ed));
                self.refresh_library();
                self.toast(format!("New canvas — press {}V to paste an image onto it", platform::CMD));
            }
            Err(e) => self.toast(format!("Couldn't create the canvas: {e:#}")),
        }
    }

    /// ⌘N everywhere (the File menu covers macOS), ⌘W closes and ⌘D deletes the
    /// open item, ⌘V on the welcome screen.
    fn home_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
            self.show_new_dialog = true;
        }
        // On macOS the File menu catches these first; this covers Windows.
        let (close, delete) = ctx.input_mut(|i| {
            (i.consume_key(egui::Modifiers::COMMAND, egui::Key::W), i.consume_key(egui::Modifiers::COMMAND, egui::Key::D))
        });
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
                self.import_image(img);
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
        for (w, h, label) in [
            (1280, 800, "Laptop · 1280 × 800"),
            (1920, 1080, "Full HD · 1920 × 1080"),
            (1080, 1080, "Square · 1080 × 1080"),
            (800, 600, "Small · 800 × 600"),
        ] {
            if ui.selectable_label(nc.width == w && nc.height == h, label).clicked() {
                nc.width = w;
                nc.height = h;
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut nc.width).range(16..=8000).suffix(" px"));
            ui.label("×");
            ui.add(egui::DragValue::new(&mut nc.height).range(16..=8000).suffix(" px"));
        });
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
                            (icon::OCR, "Text", Action::Text, Purpose::Text, false),
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
                    (icon::OCR, "Grab text", self.keys(Action::Text), Purpose::Text, false),
                ];
                let tile = Vec2::new(104.0, 104.0);
                let gap = 12.0;
                let row_w = tile.x * tiles.len() as f32 + gap * (tiles.len() - 1) as f32;
                let (row, _) = ui.allocate_exact_size(Vec2::new(row_w, tile.y), Sense::hover());
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
            })
            .filter(|i| needle.is_empty() || i.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        // Items hidden by the filter or search can't be acted on unseen.
        self.lib_selection.retain(|p| items.iter().any(|i| &i.path == p));
        let mut delete_selection = false;

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Library").font(theme::medium(14.0)).color(theme::TEXT));
            let images = self.library.iter().filter(|i| i.kind == ItemKind::Image).count();
            let videos = self.library.len() - images;
            ui.add_space(6.0);
            let all = format!("All {}", self.library.len());
            let imgs = format!("Images {images}");
            let vids = format!("Videos {videos}");
            theme::segmented(ui, &mut self.lib_filter, &[(LibFilter::All, &all), (LibFilter::Images, &imgs), (LibFilter::Videos, &vids)]);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // While items are selected, the header offers actions on them.
                if !self.lib_selection.is_empty() {
                    let n = self.lib_selection.len();
                    if theme::danger_button(ui, Some(icon::DELETE), &format!("Delete {n}")).clicked() {
                        delete_selection = true;
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
                        if ui.button("Open with default app").clicked() {
                            platform::open_path(&item.path);
                            ui.close();
                        }
                        if ui.button(reveal_label()).clicked() {
                            platform::reveal_path(&item.path);
                            ui.close();
                        }
                        if item.kind == ItemKind::Image && ui.button("Copy").clicked() {
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
    }

    /// The path of the image or video open in the workspace, if any.
    fn current_path(&self) -> Option<&PathBuf> {
        match &self.mode {
            Mode::Editor(ed) => Some(&ed.path),
            Mode::Video(player) => Some(&player.path),
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
        let size = Vec2::new(156.0, 100.0);
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

        let fresh = self.thumbs.get(&item.path).filter(|(m, _)| *m == item.modified).map(|(_, t)| t.clone());
        match (&item.kind, fresh) {
            (ItemKind::Image, Some(tex)) => {
                // Cover-fit into the rounded frame.
                let ts = tex.size_vec2();
                let scale = (thumb.width() / ts.x).max(thumb.height() / ts.y);
                let vis = Vec2::new(thumb.width() / (ts.x * scale), thumb.height() / (ts.y * scale));
                let uv = Rect::from_center_size(Pos2::new(0.5, 0.5), vis);
                p.add(egui::epaint::RectShape::filled(thumb, 10, Color32::WHITE).with_texture(tex.id(), uv));
            }
            (ItemKind::Image, None) => {
                let key = (item.path.clone(), item.modified);
                if self.thumbs_requested.insert(key.clone()) {
                    let _ = self.thumb_jobs.send(key);
                }
                p.text(thumb.center(), Align2::CENTER_CENTER, icon::IMAGE, theme::icon_font(26.0), theme::TEXT_FAINT);
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

    fn import_from_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg", "gif", "bmp", "webp"]).pick_file() {
            match image::open(&p) {
                Ok(img) => self.import_image(img.to_rgba8()),
                Err(e) => self.toast(format!("Cannot open: {e}")),
            }
        }
    }

    fn import_from_clipboard(&mut self) {
        match platform::clipboard_image() {
            Some(img) => self.import_image(img),
            None => self.toast("No image on the clipboard"),
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
                    &[(SettingsTab::General, "General"), (SettingsTab::Video, "Video"), (SettingsTab::Shortcuts, "Shortcuts")],
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
}

impl Default for NewCanvas {
    fn default() -> Self {
        Self { width: 1280, height: 800, background: Background::White }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LibFilter {
    All,
    Images,
    Videos,
}

/// Large capture-mode button for the welcome screen.
fn mode_tile(ui: &mut egui::Ui, rect: Rect, glyph: &str, label: &str, keys: &str) -> egui::Response {
    let resp = ui.interact(rect, ui.scope_id().with(("mode", label)), Sense::click());
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

impl eframe::App for RegaApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Before `advance_activity`, so it runs a pass after `show_overlay`.
        self.present_overlay(ctx, frame);
        while let Ok(ev) = self.events.try_recv() {
            self.handle_event(ev, ctx, frame);
        }
        self.sync_capture_zoom(ctx);
        while let Ok((path, modified, img)) = self.thumb_results.try_recv() {
            let tex = ctx.load_texture(path.to_string_lossy(), img, egui::TextureOptions::LINEAR);
            self.thumbs.insert(path, (modified, tex));
        }
        self.report_background_saves();
        self.advance_activity(ctx, frame);
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
            let outcome = egui::CentralPanel::no_frame().show(ui, |ui| overlay.ui(ui)).inner;
            match outcome {
                Outcome::Pending => {}
                Outcome::Cancel => {
                    self.activity = Activity::Idle;
                    self.return_to_rest(&ctx, frame);
                }
                Outcome::Selected(rect) => {
                    if let Activity::Selecting(overlay) = std::mem::replace(&mut self.activity, Activity::Idle) {
                        self.on_selection(&ctx, frame, &overlay, rect);
                    }
                }
            }
            return;
        }
        if matches!(self.activity, Activity::Recording(_)) {
            platform::paint_recording_frame(&ctx);
            self.recording_bar_ui(ui);
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
        Action::Text => "Grab Text",
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

fn spawn_thumbnailer(ctx: egui::Context) -> (Sender<(PathBuf, SystemTime)>, Receiver<ThumbResult>) {
    let (job_tx, job_rx) = mpsc::channel::<(PathBuf, SystemTime)>();
    let (res_tx, res_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (path, modified) in job_rx {
            let is_video = library::kind_of(&path) == Some(ItemKind::Video);
            let img = if is_video {
                match crate::player::first_frame(&path) {
                    Ok(frame) => image::DynamicImage::ImageRgba8(frame),
                    Err(_) => continue,
                }
            } else {
                // GIFs open as their first frame.
                let Ok(img) = image::open(&path) else { continue };
                img
            };
            let thumb = img.thumbnail(400, 270).to_rgba8();
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
