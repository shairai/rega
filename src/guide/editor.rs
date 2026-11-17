//! Editing a guide: the steps down the left (drag to reorder), the selected
//! step's screenshot in the middle (click it to move the marker), and its
//! caption on the right. Every change saves itself shortly after, rewriting
//! the shared page in the background.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Align2, Color32, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, Vec2};

use super::style::{self, Style};
use super::{Action, Button, Guide, Shot, Step, export};
use crate::theme::{self, ItemStyle, icon};

/// Changes are saved this long after the last one.
/// The properties column beside the step: its width and inner margin.
pub const PROPERTIES_WIDTH: f32 = 280.0;
pub const PROPERTIES_MARGIN: i8 = 18;

const AUTOSAVE_AFTER: Duration = Duration::from_millis(1200);
/// Full-size screenshots kept as textures; the rest are re-decoded when shown.
const FULL_TEXTURES: usize = 8;
const THUMB_WIDTH: u32 = 360;

pub enum GuideAction {
    None,
    Toast(String),
    /// The page was written: the library should look again.
    Saved,
    /// Make this guide's look the default for new guides.
    SetDefaultStyle(Style),
}

#[derive(Clone)]
struct Snapshot {
    title: String,
    style: Style,
    steps: Vec<Step>,
    selected: usize,
}

pub struct GuideEditor {
    pub path: PathBuf,
    guide: Guide,
    selected: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The text field whose typing makes up the current undo step.
    text_group: Option<egui::Id>,
    /// A step being dragged in the list: (its index, where it would go).
    dragging: Option<(usize, usize)>,
    /// The marker is being dragged on the screenshot.
    moving_marker: bool,
    dirty: bool,
    changed_at: Instant,
    saving: Option<Receiver<Result<u64>>>,
    save_thread: Option<JoinHandle<()>>,
    /// Size of the page as last saved.
    page_size: Option<u64>,
    cache: Arc<export::Cache>,
    pictures: Pictures,
    /// Scroll the step list to the selection on the next frame.
    reveal_selected: bool,
}

impl GuideEditor {
    pub fn new(ctx: &egui::Context, path: PathBuf, guide: Guide) -> Self {
        let page_size = std::fs::metadata(&path).ok().map(|m| m.len());
        Self {
            path,
            guide,
            selected: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            text_group: None,
            dragging: None,
            moving_marker: false,
            dirty: false,
            changed_at: Instant::now(),
            saving: None,
            save_thread: None,
            page_size,
            cache: Arc::new(export::Cache::default()),
            pictures: Pictures::new(ctx),
            reveal_selected: false,
        }
    }

    pub fn open(ctx: &egui::Context, path: PathBuf) -> Result<Self> {
        let guide = super::load(&path)?;
        anyhow::ensure!(!guide.steps.is_empty(), "this guide has no steps");
        Ok(Self::new(ctx, path, guide))
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    // ---------- saving ----------

    /// Write the sidecar and the page on a background thread.
    pub fn save_in_background(&mut self, ctx: &egui::Context) {
        if self.saving.is_some() {
            return;
        }
        let (guide, path, cache) = (self.guide.clone(), self.path.clone(), self.cache.clone());
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.save_thread = Some(thread::spawn(move || {
            let _ = tx.send(write(&path, &guide, &cache));
            ctx.request_repaint();
        }));
        self.saving = Some(rx);
        self.dirty = false;
    }

    /// Save now (closing, quitting): waits for a save under way, then writes
    /// what changed since.
    pub fn save_now(&mut self) -> Result<()> {
        if let Some(t) = self.save_thread.take() {
            let _ = t.join();
        }
        if let Some(rx) = self.saving.take()
            && let Ok(Ok(size)) = rx.try_recv()
        {
            self.page_size = Some(size);
        }
        if self.dirty || !self.path.exists() {
            self.page_size = Some(write(&self.path, &self.guide, &self.cache)?);
            self.dirty = false;
        }
        Ok(())
    }

    fn poll_save(&mut self) -> GuideAction {
        let Some(rx) = &self.saving else { return GuideAction::None };
        match rx.try_recv() {
            Err(TryRecvError::Empty) => GuideAction::None,
            Err(TryRecvError::Disconnected) => {
                self.saving = None;
                GuideAction::None
            }
            Ok(result) => {
                self.saving = None;
                self.save_thread = None;
                match result {
                    Ok(size) => {
                        self.page_size = Some(size);
                        GuideAction::Saved
                    }
                    Err(e) => {
                        self.dirty = true;
                        GuideAction::Toast(format!("Couldn't save the guide: {e:#}"))
                    }
                }
            }
        }
    }

    // ---------- edits ----------

    fn snapshot(&self) -> Snapshot {
        Snapshot { title: self.guide.title.clone(), style: self.guide.style, steps: self.guide.steps.clone(), selected: self.selected }
    }

    fn restore(&mut self, s: Snapshot) {
        self.guide.title = s.title;
        self.guide.style = s.style;
        self.guide.steps = s.steps;
        self.selected = s.selected.min(self.guide.steps.len().saturating_sub(1));
        self.reveal_selected = true;
        self.touch();
    }

    /// Remember the state before an edit, for undo.
    fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.text_group = None;
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.changed_at = Instant::now();
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(s);
            self.text_group = None;
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(s);
            self.text_group = None;
        }
    }

    fn select(&mut self, i: usize) {
        self.selected = i.min(self.guide.steps.len().saturating_sub(1));
        self.reveal_selected = true;
        self.text_group = None;
    }

    fn move_step(&mut self, from: usize, to: usize) {
        let n = self.guide.steps.len();
        if from >= n || to >= n || from == to {
            return;
        }
        self.checkpoint();
        let step = self.guide.steps.remove(from);
        self.guide.steps.insert(to, step);
        self.select(to);
        self.touch();
    }

    fn delete_step(&mut self, i: usize) -> GuideAction {
        if self.guide.steps.len() <= 1 {
            return GuideAction::Toast("A guide needs at least one step".into());
        }
        self.checkpoint();
        self.guide.steps.remove(i);
        self.select(if i >= self.guide.steps.len() { i - 1 } else { i });
        self.touch();
        GuideAction::Toast(format!("Step deleted — {}Z to undo", crate::platform::CMD))
    }

    fn duplicate_step(&mut self, i: usize) {
        self.checkpoint();
        let copy = self.guide.steps[i].clone();
        self.guide.steps.insert(i + 1, copy);
        self.select(i + 1);
        self.touch();
    }

    // ---------- per frame ----------

    /// Shortcuts, autosave and loading; call once per frame.
    pub fn end_frame(&mut self, ctx: &egui::Context) -> GuideAction {
        self.pictures.receive(ctx);
        let mut action = self.poll_save();
        let cmd = egui::Modifiers::COMMAND;
        let cmd_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        if ctx.input_mut(|i| i.consume_key(cmd, Key::S)) {
            self.save_in_background(ctx);
        }
        let typing = ctx.memory(|m| m.focused().is_some());
        if !typing {
            let (redo, undo) = ctx.input_mut(|i| (i.consume_key(cmd_shift, Key::Z), i.consume_key(cmd, Key::Z)));
            if redo {
                self.redo();
            } else if undo {
                self.undo();
            }
            let none = egui::Modifiers::NONE;
            let alt = egui::Modifiers::ALT;
            let (prev, next, up, down, delete) = ctx.input_mut(|i| {
                (
                    i.consume_key(none, Key::ArrowUp) | i.consume_key(none, Key::ArrowLeft),
                    i.consume_key(none, Key::ArrowDown) | i.consume_key(none, Key::ArrowRight),
                    i.consume_key(alt, Key::ArrowUp),
                    i.consume_key(alt, Key::ArrowDown),
                    i.consume_key(none, Key::Delete) | i.consume_key(none, Key::Backspace),
                )
            });
            let i = self.selected;
            if prev && i > 0 {
                self.select(i - 1);
            } else if next {
                self.select(i + 1);
            } else if up && i > 0 {
                self.move_step(i, i - 1);
            } else if down {
                self.move_step(i, i + 1);
            } else if delete {
                action = self.delete_step(i);
            }
        }
        if self.dirty && self.saving.is_none() {
            let wait = AUTOSAVE_AFTER.saturating_sub(self.changed_at.elapsed());
            if wait.is_zero() {
                self.save_in_background(ctx);
            } else {
                ctx.request_repaint_after(wait);
            }
        }
        action
    }

    /// Guide actions for the workspace top bar.
    pub fn top_actions(&mut self, ui: &mut egui::Ui) -> GuideAction {
        let mut action = GuideAction::None;
        theme::toolbar_group(ui, |ui| {
            if theme::toolbar_item(ui, icon::UNDO, "Undo", ItemStyle::Normal, !self.undo.is_empty())
                .on_hover_text(format!("Undo ({}Z)", crate::platform::CMD))
                .clicked()
            {
                self.undo();
            }
            if theme::toolbar_item(ui, icon::REDO, "Redo", ItemStyle::Normal, !self.redo.is_empty())
                .on_hover_text(format!("Redo ({}{}Z)", crate::platform::CMD, crate::platform::SHIFT))
                .clicked()
            {
                self.redo();
            }
        });
        ui.add_space(6.0);
        theme::toolbar_group(ui, |ui| {
            if theme::toolbar_item(ui, icon::EXPORT, "Export", ItemStyle::Normal, true)
                .on_hover_text("Save a copy of the page somewhere else")
                .clicked()
            {
                action = self.export_copy();
            }
            // No Save: changes save by themselves (⌘S still saves at once).
            if theme::toolbar_item(ui, icon::OPEN_EXTERNAL, "Preview", ItemStyle::Primary, true)
                .on_hover_text("Open the shared page in your browser")
                .clicked()
            {
                action = self.preview();
            }
        });
        action
    }

    fn preview(&mut self) -> GuideAction {
        match self.save_now() {
            Ok(()) => {
                crate::platform::open_path(&self.path);
                GuideAction::Saved
            }
            Err(e) => GuideAction::Toast(format!("Couldn't save the guide: {e:#}")),
        }
    }

    fn export_copy(&mut self) -> GuideAction {
        if let Err(e) = self.save_now() {
            return GuideAction::Toast(format!("Couldn't save the guide: {e:#}"));
        }
        let name = sanitize_file_name(&self.guide.title).unwrap_or_else(|| self.file_name());
        let name = if name.to_ascii_lowercase().ends_with(".html") { name } else { format!("{name}.html") };
        let Some(dest) = rfd::FileDialog::new().set_file_name(&name).add_filter("Web page", &["html"]).save_file() else {
            return GuideAction::None;
        };
        match std::fs::copy(&self.path, &dest) {
            Ok(_) => GuideAction::Toast(format!("Saved {}", dest.file_name().unwrap_or_default().to_string_lossy())),
            Err(e) => GuideAction::Toast(format!("Couldn't save the copy: {e}")),
        }
    }

    pub fn status_bar(&mut self, ui: &mut egui::Ui) {
        let n = self.guide.steps.len();
        let mut parts = vec![format!("{n} step{}", if n == 1 { "" } else { "s" })];
        if let Some(size) = self.page_size {
            parts.push(format!("{} page", human_size(size)));
        }
        let state = if self.saving.is_some() {
            "Saving…"
        } else if self.dirty {
            "Edited"
        } else {
            "Saved"
        };
        parts.push(state.to_owned());
        ui.label(egui::RichText::new(parts.join("  ·  ")).color(theme::TEXT_MUTED));
    }

    // ---------- step list ----------

    pub fn steps_panel(&mut self, ui: &mut egui::Ui) -> GuideAction {
        let mut action = GuideAction::None;
        theme::section(ui, "Steps");
        ui.add_space(4.0);
        let width = ui.available_width();
        let mut rows: Vec<Rect> = Vec::with_capacity(self.guide.steps.len());
        let mut clicked = None;
        let mut menu: Option<(usize, MenuChoice)> = None;
        let pointer = ui.input(|i| i.pointer.interact_pos());
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            for i in 0..self.guide.steps.len() {
                let step = &self.guide.steps[i];
                let shot = step.shot.clone();
                let thumb_h = (width - 16.0) * shot.height as f32 / shot.width.max(1) as f32;
                let thumb_h = thumb_h.clamp(60.0, 150.0);
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, thumb_h + 44.0), Sense::click_and_drag());
                rows.push(rect);
                let selected = i == self.selected;
                if selected && self.reveal_selected {
                    ui.scroll_to_rect(rect, None);
                    self.reveal_selected = false;
                }
                let p = ui.painter();
                let hovered = resp.hovered() && self.dragging.is_none();
                let bg = if selected {
                    theme::SURFACE_3
                } else if hovered {
                    theme::SURFACE_2
                } else {
                    Color32::TRANSPARENT
                };
                p.rect_filled(rect, 10.0, bg);
                if selected {
                    p.rect_stroke(rect, 10.0, Stroke::new(1.5, theme::ACCENT), StrokeKind::Inside);
                }
                let thumb = Rect::from_min_size(rect.min + Vec2::new(8.0, 8.0), Vec2::new(width - 16.0, thumb_h));
                p.rect_filled(thumb, 6.0, Color32::from_rgb(11, 13, 17));
                if let Some(tex) = self.pictures.thumb(&shot) {
                    let fit = fit_rect(thumb, tex.size_vec2());
                    p.image(tex.id(), fit, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    if let Some([mx, my]) = step.visible_marker() {
                        let c = fit.min + Vec2::new(mx * fit.width(), my * fit.height());
                        p.circle_filled(c, 5.0, self.guide.style.color32());
                        p.circle_stroke(c, 5.0, Stroke::new(1.5, Color32::WHITE));
                    }
                }
                // Number badge.
                let badge = Rect::from_min_size(thumb.min + Vec2::new(6.0, 6.0), Vec2::new(26.0, 20.0));
                theme::gradient_rect(p, badge, 10, Color32::WHITE);
                p.text(badge.center(), Align2::CENTER_CENTER, (i + 1).to_string(), theme::bold(11.5), Color32::WHITE);
                let glyph = match step.action {
                    Action::Click { .. } => icon::MOUSE,
                    Action::Type { .. } | Action::Key { .. } => icon::KEYBOARD,
                };
                let text_y = thumb.max.y + 18.0;
                p.text(Pos2::new(rect.min.x + 18.0, text_y), Align2::CENTER_CENTER, glyph, theme::icon_font(15.0), theme::TEXT_FAINT);
                let title = if step.title.trim().is_empty() { step.action.default_title() } else { step.title.clone() };
                let galley = p.layout(
                    title.lines().next().unwrap_or_default().to_owned(),
                    theme::medium(12.5),
                    if selected { theme::TEXT } else { theme::TEXT_MUTED },
                    width - 40.0,
                );
                let galley = if galley.rows.len() > 1 {
                    p.layout_no_wrap(truncate(&title, ((width - 40.0) / 6.8) as usize), theme::medium(12.5), theme::TEXT_MUTED)
                } else {
                    galley
                };
                p.galley(Pos2::new(rect.min.x + 32.0, text_y - galley.size().y / 2.0), galley, theme::TEXT);

                if resp.clicked() {
                    clicked = Some(i);
                }
                if resp.drag_started() {
                    self.dragging = Some((i, i));
                }
                resp.context_menu(|ui| {
                    ui.set_min_width(170.0);
                    for (choice, label, enabled) in [
                        (MenuChoice::Up, "Move up", i > 0),
                        (MenuChoice::Down, "Move down", i + 1 < self.guide.steps.len()),
                        (MenuChoice::Duplicate, "Duplicate", true),
                        (MenuChoice::Delete, "Delete", self.guide.steps.len() > 1),
                    ] {
                        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                            menu = Some((i, choice));
                            ui.close();
                        }
                    }
                });
            }
        });

        // Dragging: the step goes where the pointer is between rows.
        if let Some((from, _)) = self.dragging {
            let released = ui.input(|i| i.pointer.any_released() || !i.pointer.any_down());
            if let Some(pos) = pointer {
                let to = rows.iter().position(|r| pos.y < r.center().y).unwrap_or(rows.len());
                // Insertion index in the list without the dragged step.
                let to = if to > from { to - 1 } else { to };
                self.dragging = Some((from, to.min(rows.len().saturating_sub(1))));
                let line_y = if to >= from { rows.get(to + 1).map(|r| r.min.y - 4.0) } else { rows.get(to).map(|r| r.min.y - 4.0) }
                    .or_else(|| rows.last().map(|r| r.max.y + 4.0));
                if let (Some(y), Some(first)) = (line_y, rows.first())
                    && to != from
                {
                    let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, ui.scope_id().with("guide-drop")));
                    p.line_segment([Pos2::new(first.min.x + 4.0, y), Pos2::new(first.max.x - 4.0, y)], Stroke::new(3.0, theme::ACCENT));
                }
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            if released {
                if let Some((from, to)) = self.dragging.take() {
                    self.move_step(from, to);
                }
            } else {
                ui.ctx().request_repaint();
            }
        }
        if let Some(i) = clicked {
            self.select(i);
        }
        if let Some((i, choice)) = menu {
            match choice {
                MenuChoice::Up => self.move_step(i, i.saturating_sub(1)),
                MenuChoice::Down => self.move_step(i, i + 1),
                MenuChoice::Duplicate => self.duplicate_step(i),
                MenuChoice::Delete => action = self.delete_step(i),
            }
        }
        action
    }

    // ---------- properties ----------

    pub fn properties_panel(&mut self, ui: &mut egui::Ui) -> GuideAction {
        let mut action = GuideAction::None;
        let before = self.snapshot();
        let mut text_changed: Option<egui::Id> = None;
        // A slider drag is one undo step; each click another.
        let style_group = ui.make_persistent_id("guide-style");
        if self.text_group == Some(style_group) && !ui.input(|i| i.pointer.any_down()) {
            self.text_group = None;
        }
        let style_before = self.guide.style;

        theme::section(ui, "Guide");
        let title = ui.add(
            egui::TextEdit::singleline(&mut self.guide.title)
                .hint_text("Name this guide")
                .desired_width(f32::INFINITY)
                .font(theme::medium(14.0)),
        );
        if title.changed() {
            text_changed = Some(title.id);
        }
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Each step plays for").color(theme::TEXT_MUTED));
        style::seconds_control(ui, &mut self.guide.style.seconds);

        ui.add_space(14.0);
        let n = self.guide.steps.len();
        let i = self.selected.min(n - 1);
        theme::section(ui, &format!("Step {} of {n}", i + 1));
        let recorded = describe(&self.guide.steps[i].action);
        ui.horizontal(|ui| {
            let glyph = if matches!(self.guide.steps[i].action, Action::Click { .. }) { icon::MOUSE } else { icon::KEYBOARD };
            ui.label(egui::RichText::new(glyph).font(theme::icon_font(16.0)).color(theme::TEXT_FAINT));
            ui.label(egui::RichText::new(recorded).color(theme::TEXT_FAINT).size(12.0));
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Title").color(theme::TEXT_MUTED));
        let step = &mut self.guide.steps[i];
        let title = ui.add(egui::TextEdit::multiline(&mut step.title).desired_rows(2).desired_width(f32::INFINITY).font(theme::medium(14.0)));
        if title.changed() {
            text_changed = Some(title.id);
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Description").color(theme::TEXT_MUTED));
        let note = ui.add(
            egui::TextEdit::multiline(&mut step.note)
                .hint_text("Optional: more detail for this step")
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        if note.changed() {
            text_changed = Some(note.id);
        }
        ui.add_space(8.0);
        let mut show_marker = step.visible_marker().is_some();
        if theme::switch(ui, &mut show_marker, "Show marker").changed() {
            self.checkpoint();
            let step = &mut self.guide.steps[i];
            // Hidden, the marker keeps its place for when it's shown again.
            step.marker_hidden = !show_marker;
            if show_marker && step.marker.is_none() {
                step.marker = Some([0.5, 0.5]);
            }
            self.touch();
        }
        ui.label(egui::RichText::new("Click the screenshot to move the marker there.").color(theme::TEXT_FAINT).size(11.5));

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if theme::icon_button_sized(ui, icon::MOVE_UP, "Move up (⌥↑)", 34.0, i > 0).clicked() {
                self.move_step(i, i - 1);
            }
            if theme::icon_button_sized(ui, icon::MOVE_DOWN, "Move down (⌥↓)", 34.0, i + 1 < n).clicked() {
                self.move_step(i, i + 1);
            }
            if theme::icon_button_sized(ui, icon::COPY, "Duplicate", 34.0, true).clicked() {
                self.duplicate_step(i);
            }
            if theme::icon_button_sized(ui, icon::DELETE, "Delete step", 34.0, n > 1).clicked() {
                action = self.delete_step(i);
            }
        });

        ui.add_space(16.0);
        theme::section(ui, "Marker");
        style::marker_controls(ui, &mut self.guide.style);
        ui.add_space(12.0);
        // Unindented: the panel has no width to spare.
        egui::CollapsingHeader::new(egui::RichText::new("Page options").color(theme::TEXT_MUTED))
            .id_salt("guide-page-options")
            .show_unindented(ui, |ui| {
                style::page_controls(ui, &mut self.guide.style);
                ui.add_space(6.0);
                ui.label("Quality");
                style::quality_control(ui, &mut self.guide.style.quality);
                ui.label(egui::RichText::new("Screenshot sizes are set when recording; this sets how the page compresses them.").color(theme::TEXT_FAINT).size(11.5));
                ui.add_space(6.0);
                if theme::ghost_button(ui, None, "Use for new guides").on_hover_text("Make this look the default in Settings › Guide").clicked() {
                    action = GuideAction::SetDefaultStyle(self.guide.style);
                }
            });

        ui.add_space(16.0);
        theme::section(ui, "Share");
        ui.label(
            egui::RichText::new("One web page with every step inside: send it, upload it, or open it in any browser.")
                .color(theme::TEXT_MUTED)
                .size(12.0),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if theme::primary_button(ui, Some(icon::OPEN_EXTERNAL), "Preview").clicked() {
                action = self.preview();
            }
            if theme::secondary_button(ui, None, "Show file").clicked() {
                if let Err(e) = self.save_now() {
                    action = GuideAction::Toast(format!("Couldn't save the guide: {e:#}"));
                }
                crate::platform::reveal_path(&self.path);
            }
        });

        if self.guide.style != style_before {
            if self.text_group != Some(style_group) {
                self.undo.push(before.clone());
                self.redo.clear();
                self.text_group = Some(style_group);
            }
            self.touch();
        }
        // Typing in one field is one undo step, however many keys it takes.
        if let Some(id) = text_changed {
            if self.text_group != Some(id) {
                self.undo.push(before);
                self.redo.clear();
                self.text_group = Some(id);
            }
            self.touch();
        }
        action
    }

    // ---------- screenshot ----------

    pub fn canvas(&mut self, ui: &mut egui::Ui) {
        let i = self.selected.min(self.guide.steps.len() - 1);
        let shot = self.guide.steps[i].shot.clone();
        // Neighbours first, so stepping through doesn't wait.
        for j in [i + 1, i.saturating_sub(1)] {
            if let Some(s) = self.guide.steps.get(j) {
                self.pictures.request_full(&s.shot);
            }
        }
        let full = ui.available_rect_before_wrap();
        // Room below the screenshot for the step switcher.
        let avail = Rect::from_min_max(full.min, full.max - Vec2::new(0.0, 40.0)).shrink(24.0);
        let size = Vec2::new(shot.width as f32, shot.height as f32);
        let rect = fit_rect(avail, size);
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let p = ui.painter();
        p.add(theme::soft_shadow(rect, 10));
        match self.pictures.full(&shot) {
            Some(tex) => {
                p.add(egui::epaint::RectShape::filled(rect, 8, Color32::WHITE).with_texture(tex.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
            }
            None => {
                p.rect_filled(rect, 8.0, theme::SURFACE);
                match self.pictures.thumb(&shot) {
                    Some(tex) => {
                        p.add(egui::epaint::RectShape::filled(rect, 8, Color32::WHITE).with_texture(tex.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
                    }
                    None => {
                        p.text(rect.center(), Align2::CENTER_CENTER, "Loading…", FontId::proportional(13.0), theme::TEXT_FAINT);
                    }
                }
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            }
        }
        let to_fraction = |pos: Pos2| [((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0), ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0)];
        if let Some([mx, my]) = self.guide.steps[i].visible_marker() {
            let c = rect.min + Vec2::new(mx * rect.width(), my * rect.height());
            let animate = !crate::platform::reduce_motion();
            let pulse = animate.then(|| (ui.input(|inp| inp.time) % 1.6 / 1.6) as f32);
            style::paint_marker(p, rect, c, &self.guide.style, i + 1, pulse);
            if animate {
                ui.ctx().request_repaint_after(Duration::from_millis(33));
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        if (resp.drag_started() || resp.clicked())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.checkpoint();
            self.guide.steps[i].marker = Some(to_fraction(pos));
            self.guide.steps[i].marker_hidden = false;
            self.moving_marker = resp.drag_started();
            self.touch();
        } else if self.moving_marker
            && resp.dragged()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.guide.steps[i].marker = Some(to_fraction(pos));
            self.touch();
        }
        if resp.drag_stopped() {
            self.moving_marker = false;
        }
        // Back / Next under the screenshot, as the shared page has them.
        let n = self.guide.steps.len();
        let row = Rect::from_center_size(Pos2::new(rect.center().x, rect.max.y + 28.0), Vec2::new(240.0, 34.0));
        let mut go = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(row), |ui| {
            ui.horizontal_centered(|ui| {
                if theme::icon_button_sized(ui, icon::PREV, "Previous step (←)", 34.0, i > 0).clicked() {
                    go = Some(i - 1);
                }
                let label = egui::RichText::new(format!("Step {} of {n}", i + 1)).font(theme::medium(13.0)).color(theme::TEXT_MUTED);
                ui.add_sized(Vec2::new(140.0, 34.0), egui::Label::new(label));
                if theme::icon_button_sized(ui, icon::NEXT, "Next step (→)", 34.0, i + 1 < n).clicked() {
                    go = Some(i + 1);
                }
            });
        });
        if let Some(j) = go {
            self.select(j);
        }
    }
}

#[derive(Clone, Copy)]
enum MenuChoice {
    Up,
    Down,
    Duplicate,
    Delete,
}

fn write(path: &std::path::Path, guide: &Guide, cache: &export::Cache) -> Result<u64> {
    super::save_sidecar(path, guide)?;
    let page = export::page(guide, cache)?;
    super::write_atomic(path, page.as_bytes())?;
    cache.retain(guide);
    Ok(page.len() as u64)
}

fn describe(action: &Action) -> String {
    match action {
        Action::Click { button: Button::Right, .. } => "Recorded: right-click".into(),
        Action::Click { count: 2.., .. } => "Recorded: double-click".into(),
        Action::Click { .. } => "Recorded: click".into(),
        Action::Type { then: Some(key), .. } => format!("Recorded: typing, then {key}"),
        Action::Type { .. } => "Recorded: typing".into(),
        Action::Key { combo } => format!("Recorded: {combo}"),
    }
}

/// The largest rect of `size`'s aspect ratio centred in `area`.
fn fit_rect(area: Rect, size: Vec2) -> Rect {
    let s = (area.width() / size.x.max(1.0)).min(area.height() / size.y.max(1.0));
    Rect::from_center_size(area.center(), size * s)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_owned() } else { format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>()) }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// A file name from a guide's title, or `None` when nothing usable is left.
fn sanitize_file_name(title: &str) -> Option<String> {
    let name: String = title.chars().map(|c| if c.is_control() || r#"/\:*?"<>|"#.contains(c) { ' ' } else { c }).collect();
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty() && name != "Untitled guide").then_some(name)
}

// ---------- screenshots as textures ----------

/// Decodes screenshots on a worker thread: a thumbnail for every step, and
/// full-size textures for the few last shown.
struct Pictures {
    jobs: Sender<(Arc<Shot>, bool)>,
    results: Receiver<Decoded>,
    asked: HashSet<(u64, bool)>,
    thumbs: HashMap<u64, TextureHandle>,
    full: HashMap<u64, TextureHandle>,
    recent: VecDeque<u64>,
}

struct Decoded {
    id: u64,
    thumb: egui::ColorImage,
    full: Option<egui::ColorImage>,
}

impl Pictures {
    fn new(ctx: &egui::Context) -> Self {
        let (jobs, job_rx) = mpsc::channel::<(Arc<Shot>, bool)>();
        let (tx, results) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            for (shot, want_full) in job_rx {
                let Ok(img) = shot.decode() else { continue };
                let tw = THUMB_WIDTH.min(img.width());
                let th = ((img.height() as f32 * tw as f32 / img.width().max(1) as f32).round() as u32).max(1);
                let small = crate::resize::resize(&img, tw, th);
                let thumb = egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], small.as_raw());
                let full = want_full.then(|| egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()));
                if tx.send(Decoded { id: shot.id, thumb, full }).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self { jobs, results, asked: HashSet::new(), thumbs: HashMap::new(), full: HashMap::new(), recent: VecDeque::new() }
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(d) = self.results.try_recv() {
            self.asked.remove(&(d.id, false));
            self.asked.remove(&(d.id, true));
            self.thumbs.entry(d.id).or_insert_with(|| ctx.load_texture(format!("guide-thumb-{:x}", d.id), d.thumb, egui::TextureOptions::LINEAR));
            if let Some(full) = d.full {
                self.full.insert(d.id, ctx.load_texture(format!("guide-shot-{:x}", d.id), full, egui::TextureOptions::LINEAR));
                self.recent.retain(|id| *id != d.id);
                self.recent.push_back(d.id);
                while self.recent.len() > FULL_TEXTURES {
                    if let Some(old) = self.recent.pop_front() {
                        self.full.remove(&old);
                    }
                }
            }
        }
    }

    fn thumb(&mut self, shot: &Arc<Shot>) -> Option<TextureHandle> {
        if let Some(t) = self.thumbs.get(&shot.id) {
            return Some(t.clone());
        }
        if !self.asked.contains(&(shot.id, false)) && !self.asked.contains(&(shot.id, true)) {
            self.asked.insert((shot.id, false));
            let _ = self.jobs.send((shot.clone(), false));
        }
        None
    }

    fn full(&mut self, shot: &Arc<Shot>) -> Option<TextureHandle> {
        if let Some(t) = self.full.get(&shot.id) {
            let id = shot.id;
            self.recent.retain(|r| *r != id);
            self.recent.push_back(id);
            return Some(t.clone());
        }
        self.request_full(shot);
        None
    }

    fn request_full(&mut self, shot: &Arc<Shot>) {
        if !self.full.contains_key(&shot.id) && self.asked.insert((shot.id, true)) {
            let _ = self.jobs.send((shot.clone(), true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{screen, step};
    use super::*;

    fn editor(n: usize) -> GuideEditor {
        editor_in(&egui::Context::default(), n)
    }

    /// A guide of `n` steps, its pictures loading into `ctx`.
    fn editor_in(ctx: &egui::Context, n: usize) -> GuideEditor {
        let shot = Shot::encode(&screen(64, 40, 1)).unwrap();
        let steps = (0..n).map(|i| step(&shot, &format!("Step {i}"))).collect();
        let path = std::env::temp_dir().join(format!("rega-guide-editor-{}-{n}.html", std::process::id()));
        GuideEditor::new(ctx, path, Guide::new("Test".into(), steps))
    }

    /// The properties fit their column with Page options open: nothing is
    /// wider than the column, so nothing pushes past its edge (under the
    /// step). Set `GUIDE_PANEL_PREVIEW_DIR` to save it.
    #[test]
    fn properties_fit_their_column() {
        use egui_kittest::kittest::Queryable;
        #[derive(Default)]
        struct State {
            ed: Option<GuideEditor>,
            width: f32,
        }
        let mut h = egui_kittest::Harness::builder()
            .with_size(Vec2::new(600.0, 1100.0))
            .with_pixels_per_point(2.0)
            .with_accessibility_check(false)
            .with_context_setup(theme::install)
            .wgpu()
            .build_ui_state(
                |ui, s: &mut State| {
                    let ed = s.ed.get_or_insert_with(|| editor_in(ui.ctx(), 2));
                    egui::Panel::right("guide-properties")
                        .resizable(false)
                        .exact_size(PROPERTIES_WIDTH)
                        .frame(theme::panel_frame(theme::SURFACE, egui::Margin::same(PROPERTIES_MARGIN)))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                ed.properties_panel(ui);
                                s.width = ui.min_rect().width();
                            });
                        });
                },
                State::default(),
            );
        h.run();
        h.get_by_label("Page options").click();
        h.run();
        if let Ok(dir) = std::env::var("GUIDE_PANEL_PREVIEW_DIR") {
            h.render().unwrap().save(std::path::Path::new(&dir).join("guide-properties.png")).unwrap();
        }
        let room = PROPERTIES_WIDTH - 2.0 * PROPERTIES_MARGIN as f32;
        let width = h.state().width;
        assert!(width <= room + 0.5, "the properties are {width} points wide in a {room}-point column");
    }

    fn titles(ed: &GuideEditor) -> Vec<&str> {
        ed.guide.steps.iter().map(|s| s.title.as_str()).collect()
    }

    #[test]
    fn steps_move_delete_and_undo() {
        let mut ed = editor(3);
        ed.move_step(0, 2);
        assert_eq!(titles(&ed), ["Step 1", "Step 2", "Step 0"]);
        assert_eq!(ed.selected, 2, "the moved step stays selected");
        ed.delete_step(1);
        assert_eq!(titles(&ed), ["Step 1", "Step 0"]);
        ed.duplicate_step(0);
        assert_eq!(titles(&ed), ["Step 1", "Step 1", "Step 0"]);
        ed.undo();
        ed.undo();
        ed.undo();
        assert_eq!(titles(&ed), ["Step 0", "Step 1", "Step 2"]);
        ed.redo();
        assert_eq!(titles(&ed), ["Step 1", "Step 2", "Step 0"]);
        assert!(ed.dirty);
    }

    /// Switching the marker off and on again brings it back where it was.
    #[test]
    fn hiding_the_marker_keeps_its_place() {
        let mut ed = editor(1);
        ed.guide.steps[0].marker = Some([0.2, 0.7]);
        ed.guide.steps[0].marker_hidden = true;
        assert_eq!(ed.guide.steps[0].visible_marker(), None);
        ed.guide.steps[0].marker_hidden = false;
        assert_eq!(ed.guide.steps[0].visible_marker(), Some([0.2, 0.7]));
    }

    #[test]
    fn the_last_step_cannot_be_deleted() {
        let mut ed = editor(1);
        ed.delete_step(0);
        assert_eq!(ed.guide.steps.len(), 1);
        assert!(ed.undo.is_empty());
    }

    /// Saving writes a page that opens in a browser, and a sidecar that opens
    /// in the editor again.
    #[test]
    fn saves_and_reopens() {
        let mut ed = editor(2);
        ed.guide.steps[1].note = "Then this".into();
        ed.touch();
        ed.save_now().unwrap();
        let page = std::fs::read_to_string(&ed.path).unwrap();
        assert!(page.starts_with("<!doctype html>") && page.contains("Then this"));
        let again = GuideEditor::open(&egui::Context::default(), ed.path.clone()).unwrap();
        assert_eq!(titles(&again), ["Step 0", "Step 1"]);
        assert_eq!(again.page_size, Some(page.len() as u64));
        let _ = std::fs::remove_file(&ed.path);
        super::super::delete_sidecar(&ed.path);
    }

    #[test]
    fn file_names_come_from_titles() {
        assert_eq!(sanitize_file_name("Reset: your/password?").as_deref(), Some("Reset your password"));
        assert_eq!(sanitize_file_name("  "), None);
        assert_eq!(human_size(1536), "2 KB");
        assert_eq!(human_size(3 * 1024 * 1024 + 200_000), "3.2 MB");
    }
}
