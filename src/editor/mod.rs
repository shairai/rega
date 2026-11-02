//! The image editor: annotate, crop, resize, add effects, export.

pub mod model;
pub mod render;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Key, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, TextureHandle, Vec2,
};
use image::RgbaImage;

use crate::library;
use model::{Annotation, Effects, Kind, P, Project, Rgba, arrow_geometry, contrast_on, ellipse_points, norm};

pub const ANNOT_FONT: &str = "annot";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Select,
    Arrow,
    Line,
    Rect,
    Ellipse,
    Pen,
    Highlight,
    Text,
    Step,
    Blur,
    Spotlight,
    Crop,
}

impl Tool {
    const ALL: [Tool; 12] = [
        Tool::Select,
        Tool::Arrow,
        Tool::Line,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Pen,
        Tool::Highlight,
        Tool::Text,
        Tool::Step,
        Tool::Blur,
        Tool::Spotlight,
        Tool::Crop,
    ];

    /// (icon, tooltip, shortcut)
    fn info(self) -> (&'static str, &'static str, Key) {
        use crate::theme::icon;
        match self {
            Tool::Select => (icon::SELECT, "Select & move", Key::V),
            Tool::Arrow => (icon::ARROW, "Arrow", Key::A),
            Tool::Line => (icon::LINE, "Line", Key::L),
            Tool::Rect => (icon::RECT, "Rectangle", Key::R),
            Tool::Ellipse => (icon::ELLIPSE, "Ellipse", Key::E),
            Tool::Pen => (icon::PEN, "Pen", Key::P),
            Tool::Highlight => (icon::MARKER, "Highlighter", Key::H),
            Tool::Text => (icon::TEXT, "Text & callout", Key::T),
            Tool::Step => (icon::STEP, "Step number", Key::N),
            Tool::Blur => (icon::BLUR, "Blur", Key::B),
            Tool::Spotlight => (icon::SPOTLIGHT, "Spotlight", Key::S),
            Tool::Crop => (icon::CROP, "Crop", Key::C),
        }
    }
}

const PALETTE: [Rgba; 8] = [
    [235, 59, 52, 255],
    [255, 140, 0, 255],
    [255, 214, 0, 255],
    [46, 184, 92, 255],
    [30, 136, 229, 255],
    [142, 68, 239, 255],
    [24, 24, 24, 255],
    [255, 255, 255, 255],
];

#[derive(Clone)]
struct State {
    base: Arc<RgbaImage>,
    anns: Vec<Annotation>,
    effects: Effects,
    next_step: u32,
}

pub enum EditorAction {
    None,
    Ocr(RgbaImage),
    Toast(String),
}

pub struct Editor {
    pub path: PathBuf,
    base: Arc<RgbaImage>,
    base_changed: bool,
    anns: Vec<Annotation>,
    effects: Effects,
    next_step: u32,
    undo: Vec<State>,
    redo: Vec<State>,

    tool: Tool,
    color: Color32,
    width: f32,
    font_size: f32,
    filled: bool,
    text_bg: bool,

    zoom: f32,
    fit: bool,
    texture: Option<TextureHandle>,
    texture_dirty: bool,

    drawing: Option<Annotation>,
    crop_drag: Option<(P, P)>,
    selected: Option<usize>,
    moving: Option<P>,
    moved: bool,
    editing_text: Option<usize>,
    focus_text: bool,
    gesture_open: bool,
    /// Eyedropper active: the flattened image to sample colors from.
    picking: Option<Arc<RgbaImage>>,
    /// GPU textures for pasted image layers, keyed by the layer's pixel buffer.
    layer_textures: HashMap<usize, TextureHandle>,
    /// Index of the image layer whose corner handle is being dragged.
    resizing: Option<usize>,
    dirty: bool,
}

fn to_c32(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
}

fn from_c32(c: Color32) -> Rgba {
    c.to_srgba_unmultiplied()
}

impl Editor {
    pub fn new(path: PathBuf, base: RgbaImage, project: Option<Project>, base_saved: bool) -> Self {
        let unit = (base.width().max(base.height()) as f32 / 250.0).clamp(3.0, 10.0);
        let project = project.unwrap_or_default();
        Self {
            path,
            base: Arc::new(base),
            base_changed: !base_saved,
            anns: project.annotations,
            effects: project.effects,
            next_step: project.next_step.max(1),
            undo: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Arrow,
            color: to_c32(PALETTE[0]),
            width: unit,
            font_size: (unit * 7.0).round(),
            filled: false,
            text_bg: false,
            zoom: 1.0,
            fit: true,
            texture: None,
            texture_dirty: true,
            drawing: None,
            crop_drag: None,
            selected: None,
            moving: None,
            moved: false,
            editing_text: None,
            focus_text: false,
            gesture_open: false,
            picking: None,
            layer_textures: HashMap::new(),
            resizing: None,
            dirty: false,
        }
    }

    /// Open a library item, restoring annotations when an editable sidecar exists.
    pub fn open(path: PathBuf) -> Result<Self> {
        let (json, orig) = library::project_paths(&path);
        if let (Ok(text), Ok(base)) = (std::fs::read_to_string(&json), image::open(&orig))
            && let Ok(project) = serde_json::from_str::<Project>(&text) {
                return Ok(Self::new(path, base.to_rgba8(), Some(project), true));
            }
        let img = image::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
        Ok(Self::new(path, img.to_rgba8(), None, false))
    }

    /// Finish any in-progress text edit (e.g. before a new capture hides the window).
    pub fn commit_pending(&mut self) {
        self.commit_text();
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn flatten(&self) -> RgbaImage {
        render::render(&self.base, &self.anns, &self.effects)
    }

    /// Write the flattened image plus the editable sidecar.
    pub fn save(&mut self) -> Result<()> {
        self.commit_text();
        let flat = self.flatten();
        write_image(&flat, &self.path)?;
        let (json, orig) = library::project_paths(&self.path);
        if let Some(dir) = json.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if self.base_changed || !orig.exists() {
            self.base.save(&orig)?;
            self.base_changed = false;
        }
        let project = Project {
            annotations: self.anns.clone(),
            effects: self.effects,
            next_step: self.next_step,
        };
        std::fs::write(&json, serde_json::to_string(&project)?)?;
        self.dirty = false;
        Ok(())
    }

    fn state(&self) -> State {
        State {
            base: self.base.clone(),
            anns: self.anns.clone(),
            effects: self.effects,
            next_step: self.next_step,
        }
    }

    fn restore(&mut self, s: State) {
        if !Arc::ptr_eq(&s.base, &self.base) {
            self.base_changed = true;
            self.fit = true;
        }
        self.base = s.base;
        self.anns = s.anns;
        self.effects = s.effects;
        self.next_step = s.next_step;
        self.selected = None;
        self.editing_text = None;
        self.texture_dirty = true;
        self.dirty = true;
    }

    fn push_undo(&mut self) {
        self.undo.push(self.state());
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.dirty = true;
    }

    /// One undo step per continuous gesture (slider drag, color pick…).
    fn gesture_undo(&mut self) {
        if !self.gesture_open {
            self.push_undo();
            self.gesture_open = true;
        }
    }

    fn undo(&mut self) {
        self.commit_text();
        if let Some(s) = self.undo.pop() {
            let cur = self.state();
            self.redo.push(cur);
            self.restore(s);
        }
    }

    fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            let cur = self.state();
            self.undo.push(cur);
            self.restore(s);
        }
    }

    fn set_tool(&mut self, tool: Tool) {
        self.commit_text();
        self.tool = tool;
    }

    fn commit_text(&mut self) {
        if let Some(i) = self.editing_text.take()
            && self.anns.get(i).is_some_and(|a| a.is_degenerate()) {
                self.anns.remove(i);
                self.selected = None;
            }
    }

    fn delete_selected(&mut self) {
        if let Some(i) = self.selected.take()
            && i < self.anns.len() {
                self.push_undo();
                if self.anns.remove(i).is_raster() {
                    self.texture_dirty = true;
                }
            }
    }

    fn crop(&mut self, a: P, b: P) {
        let (min, max) = norm(a, b);
        let (w, h) = (self.base.width() as f32, self.base.height() as f32);
        let (x0, y0) = (min.x.clamp(0.0, w).round(), min.y.clamp(0.0, h).round());
        let (x1, y1) = (max.x.clamp(0.0, w).round(), max.y.clamp(0.0, h).round());
        if x1 - x0 < 4.0 || y1 - y0 < 4.0 {
            return;
        }
        self.push_undo();
        let cropped = image::imageops::crop_imm(&*self.base, x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32).to_image();
        self.base = Arc::new(cropped);
        self.base_changed = true;
        for a in &mut self.anns {
            a.translate(-x0, -y0);
        }
        self.texture_dirty = true;
        self.fit = true;
    }

    fn resize(&mut self, factor: f32) {
        let w = ((self.base.width() as f32 * factor).round() as u32).max(1);
        let h = ((self.base.height() as f32 * factor).round() as u32).max(1);
        if w > 16_000 || h > 40_000 {
            return;
        }
        self.push_undo();
        self.base = Arc::new(image::imageops::resize(&*self.base, w, h, image::imageops::FilterType::Lanczos3));
        self.base_changed = true;
        for a in &mut self.anns {
            a.scale(factor);
        }
        self.texture_dirty = true;
        self.fit = true;
    }

    fn hit_test(&self, p: P) -> Option<usize> {
        let tol = 4.0 / self.zoom;
        self.anns.iter().rposition(|a| a.hit(p, tol))
    }

    fn new_annotation(&self, p: P) -> Option<Annotation> {
        let kind = match self.tool {
            Tool::Arrow => Kind::Arrow { a: p, b: p },
            Tool::Line => Kind::Line { a: p, b: p },
            Tool::Rect => Kind::Rect { a: p, b: p },
            Tool::Ellipse => Kind::Ellipse { a: p, b: p },
            Tool::Pen => Kind::Pen { pts: vec![p] },
            Tool::Highlight => Kind::Highlight { pts: vec![p] },
            Tool::Blur => Kind::Blur { a: p, b: p },
            Tool::Spotlight => Kind::Spotlight { a: p, b: p },
            _ => return None,
        };
        Some(Annotation {
            kind,
            color: from_c32(self.color),
            width: self.width,
            filled: self.filled,
        })
    }

    fn drag_begin(&mut self, p: P) {
        self.commit_text();
        match self.tool {
            Tool::Select => {
                if let Some((i, corner)) = self.resize_handle()
                    && corner.dist(p) <= 12.0 / self.zoom
                {
                    self.push_undo();
                    self.resizing = Some(i);
                    return;
                }
                self.selected = self.hit_test(p);
                if self.selected.is_some() {
                    self.moving = Some(p);
                    self.moved = false;
                }
            }
            Tool::Crop => self.crop_drag = Some((p, p)),
            Tool::Text | Tool::Step => {}
            _ => {
                self.selected = None;
                self.drawing = self.new_annotation(p);
            }
        }
    }

    fn drag_update(&mut self, p: P, constrain: bool) {
        if let Some(i) = self.resizing {
            if let Some(Kind::Image { pos, w, h, img }) = self.anns.get_mut(i).map(|a| &mut a.kind) {
                // Keep the aspect ratio; the top-left corner stays put.
                let aspect = img.height() as f32 / img.width().max(1) as f32;
                let new_w = (p.x - pos.x).max((p.y - pos.y) / aspect).max(8.0);
                *w = new_w;
                *h = new_w * aspect;
                self.dirty = true;
            }
            return;
        }
        if let (Some(last), Some(i)) = (self.moving, self.selected) {
            if !self.moved {
                self.push_undo();
                self.moved = true;
            }
            let ann = &mut self.anns[i];
            ann.translate(p.x - last.x, p.y - last.y);
            if ann.is_raster() {
                self.texture_dirty = true;
            }
            self.moving = Some(p);
            return;
        }
        if let Some((a, _)) = self.crop_drag {
            self.crop_drag = Some((a, p));
            return;
        }
        let Some(ann) = &mut self.drawing else { return };
        match &mut ann.kind {
            Kind::Arrow { a, b } | Kind::Line { a, b } => *b = if constrain { snap_45(*a, p) } else { p },
            Kind::Rect { a, b } | Kind::Ellipse { a, b } | Kind::Blur { a, b } | Kind::Spotlight { a, b } => {
                *b = if constrain { square(*a, p) } else { p }
            }
            Kind::Pen { pts } | Kind::Highlight { pts } => {
                if constrain {
                    // Straight stroke from the first point.
                    let first = pts[0];
                    *pts = vec![first, p];
                } else if pts.last().is_none_or(|l| l.dist(p) > 1.0) {
                    pts.push(p);
                }
            }
            _ => {}
        }
    }

    fn drag_end(&mut self) {
        self.moving = None;
        self.resizing = None;
        if let Some((a, b)) = self.crop_drag.take() {
            self.crop(a, b);
        }
        if let Some(ann) = self.drawing.take()
            && !ann.is_degenerate() {
                self.push_undo();
                if ann.is_raster() {
                    self.texture_dirty = true;
                }
                self.anns.push(ann);
                // Keep the new element selected so color/width changes apply to it.
                self.selected = Some(self.anns.len() - 1);
            }
    }

    fn click(&mut self, p: P) {
        if self.editing_text.is_some() {
            self.commit_text();
            return;
        }
        match self.tool {
            Tool::Select => self.selected = self.hit_test(p),
            Tool::Text => {
                if let Some(i) = self.hit_test(p).filter(|&i| matches!(self.anns[i].kind, Kind::Text { .. })) {
                    self.start_text_edit(i);
                    return;
                }
                self.push_undo();
                self.anns.push(Annotation {
                    kind: Kind::Text {
                        pos: p,
                        text: String::new(),
                        size: self.font_size,
                        background: self.text_bg,
                    },
                    color: from_c32(self.color),
                    width: self.width,
                    filled: false,
                });
                self.editing_text = Some(self.anns.len() - 1);
                self.selected = self.editing_text;
                self.focus_text = true;
            }
            Tool::Step => {
                self.push_undo();
                self.anns.push(Annotation {
                    kind: Kind::Step {
                        pos: p,
                        n: self.next_step,
                        radius: self.font_size * 0.75,
                    },
                    color: from_c32(self.color),
                    width: self.width,
                    filled: true,
                });
                self.selected = Some(self.anns.len() - 1);
                self.next_step += 1;
            }
            Tool::Pen | Tool::Highlight => {
                // A click leaves a dot.
                if let Some(ann) = self.new_annotation(p) {
                    self.push_undo();
                    self.anns.push(ann);
                    self.selected = Some(self.anns.len() - 1);
                }
            }
            _ => {}
        }
    }

    fn start_text_edit(&mut self, i: usize) {
        self.push_undo();
        self.editing_text = Some(i);
        self.selected = Some(i);
        self.focus_text = true;
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) {
        if !self.texture_dirty && self.texture.is_some() {
            return;
        }
        let img = render::apply_raster(&self.base, &self.anns);
        let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        match &mut self.texture {
            Some(t) => t.set(color, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("editor-image", color, egui::TextureOptions::LINEAR)),
        }
        self.texture_dirty = false;
    }

    /// Make `color` current and apply it to the selected element. `discrete`
    /// changes (a click) get their own undo step; continuous ones (dragging in
    /// the picker) share one per gesture.
    fn set_color(&mut self, color: Color32, discrete: bool) {
        self.color = color;
        if let Some(i) = self.selected.filter(|&i| i < self.anns.len()) {
            if discrete {
                self.push_undo();
            } else {
                self.gesture_undo();
            }
            self.anns[i].color = from_c32(color);
        }
    }

    fn toggle_picking(&mut self) {
        self.commit_text();
        self.picking = if self.picking.is_some() {
            None
        } else {
            // Sample what the user sees (annotations included, no border/shadow
            // so coordinates line up with the canvas).
            Some(Arc::new(render::render(&self.base, &self.anns, &Effects::default())))
        };
    }

    /// Drop an image onto the canvas as a movable, resizable layer.
    pub fn paste_image(&mut self, img: RgbaImage) {
        self.commit_text();
        let (cw, ch) = (self.base.width() as f32, self.base.height() as f32);
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        if iw < 1.0 || ih < 1.0 {
            return;
        }
        // Fit large pastes to 80% of the canvas; keep small ones at 1:1.
        let fit = (cw * 0.8 / iw).min(ch * 0.8 / ih).min(1.0);
        let (w, h) = (iw * fit, ih * fit);
        self.push_undo();
        self.anns.push(Annotation {
            kind: Kind::Image {
                pos: P::new(((cw - w) / 2.0).round(), ((ch - h) / 2.0).round()),
                w,
                h,
                img: Arc::new(img),
            },
            color: from_c32(self.color),
            width: self.width,
            filled: false,
        });
        self.selected = Some(self.anns.len() - 1);
        self.tool = Tool::Select;
    }

    fn paste_from_clipboard(&mut self) -> EditorAction {
        match crate::platform::clipboard_image() {
            Some(img) => {
                self.paste_image(img);
                EditorAction::Toast("Image pasted — drag to move, pull the corner to resize".into())
            }
            None => EditorAction::Toast("No image on the clipboard".into()),
        }
    }

    /// Bottom-right resize handle of a selected image layer (image coordinates).
    fn resize_handle(&self) -> Option<(usize, P)> {
        let i = self.selected.filter(|&i| i < self.anns.len())?;
        match &self.anns[i].kind {
            Kind::Image { pos, w, h, .. } => Some((i, P::new(pos.x + w, pos.y + h))),
            _ => None,
        }
    }

    fn ensure_layer_textures(&mut self, ctx: &egui::Context) {
        let mut live = Vec::new();
        for ann in &self.anns {
            if let Kind::Image { img, .. } = &ann.kind {
                let key = Arc::as_ptr(img) as usize;
                live.push(key);
                self.layer_textures.entry(key).or_insert_with(|| {
                    let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                    ctx.load_texture("image-layer", color, egui::TextureOptions::LINEAR)
                });
            }
        }
        // Keep undo history cheap: drop textures for layers no longer on the canvas.
        if self.layer_textures.len() > live.len() + 8 {
            self.layer_textures.retain(|k, _| live.contains(k));
        }
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.base.width(), self.base.height())
    }

    /// Keyboard shortcuts and per-frame bookkeeping; call once per frame.
    pub fn end_frame(&mut self, ctx: &egui::Context) -> EditorAction {
        let mut action = EditorAction::None;
        self.shortcuts(ctx, &mut action);
        if !ctx.input(|i| i.pointer.any_down()) {
            self.gesture_open = false;
        }
        action
    }

    /// Image actions for the workspace top bar, as toolbar groups (left to right).
    pub fn top_actions(&mut self, ui: &mut egui::Ui) -> EditorAction {
        use crate::theme::{self, ItemStyle, icon};
        let mut action = EditorAction::None;

        theme::toolbar_group(ui, |ui| {
            if theme::toolbar_item(ui, icon::UNDO, "Undo", ItemStyle::Normal, !self.undo.is_empty())
                .on_hover_text("Undo (⌘Z)")
                .clicked()
            {
                self.undo();
            }
            if theme::toolbar_item(ui, icon::REDO, "Redo", ItemStyle::Normal, !self.redo.is_empty())
                .on_hover_text("Redo (⌘⇧Z)")
                .clicked()
            {
                self.redo();
            }
        });
        ui.add_space(6.0);

        theme::toolbar_group(ui, |ui| {
            if theme::toolbar_item(ui, icon::PASTE, "Paste", ItemStyle::Normal, true)
                .on_hover_text("Paste an image from the clipboard onto the canvas (⌘V)")
                .clicked()
            {
                action = self.paste_from_clipboard();
            }
            if theme::toolbar_item(ui, icon::OCR, "Grab text", ItemStyle::Normal, true)
                .on_hover_text("Recognize text in this image")
                .clicked()
            {
                action = EditorAction::Ocr(self.flatten());
            }
            let resize = theme::toolbar_item(ui, icon::RESIZE, "Resize", ItemStyle::Normal, true).on_hover_text("Scale the image");
            egui::Popup::menu(&resize).show(|ui| {
                ui.set_min_width(150.0);
                for (label, f) in [("50%", 0.5), ("75%", 0.75), ("150%", 1.5), ("200%", 2.0)] {
                    if ui.button(label).clicked() {
                        self.resize(f);
                        ui.close();
                    }
                }
            });
        });
        ui.add_space(6.0);

        theme::toolbar_group(ui, |ui| {
            let export = theme::toolbar_item(ui, icon::EXPORT, "Export", ItemStyle::Normal, true)
                .on_hover_text("Save a copy in another format");
            egui::Popup::menu(&export).show(|ui| {
                theme::section(ui, "Export as");
                for fmt in ExportFormat::ALL {
                    let (name, hint) = fmt.describe();
                    if export_row(ui, name, hint).clicked() {
                        ui.close();
                        action = self.export_as(fmt);
                    }
                }
            });
            if theme::toolbar_item(ui, icon::SAVE, "Save", ItemStyle::Normal, true)
                .on_hover_text("Save to library (⌘S)")
                .clicked()
            {
                action = self.save_action();
            }
            if theme::toolbar_item(ui, icon::COPY, "Copy", ItemStyle::Primary, true)
                .on_hover_text("Copy image (⌘C)")
                .clicked()
            {
                action = self.copy_action();
            }
        });
        action
    }

    /// Right-hand panel: tools, then the style they use.
    pub fn tools_panel(&mut self, ui: &mut egui::Ui) {
        use crate::theme;
        theme::section(ui, "Tools");
        egui::Grid::new("tool_grid").num_columns(4).spacing([8.0, 8.0]).show(ui, |ui| {
            for (n, tool) in Tool::ALL.into_iter().enumerate() {
                let (glyph, tip, key) = tool.info();
                if theme::tool_tile(ui, glyph, self.tool == tool, &format!("{tip}   {}", key.name())).clicked() {
                    self.set_tool(tool);
                }
                if n % 4 == 3 {
                    ui.end_row();
                }
            }
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new(Tool::info(self.tool).1).font(theme::medium(13.0)).color(theme::TEXT_MUTED));

        ui.add_space(14.0);
        let selected_idx = self.selected.filter(|&i| i < self.anns.len());
        theme::section(ui, if selected_idx.is_some() { "Color · selection" } else { "Color" });
        let mut color = self.color;
        egui::Grid::new("palette").num_columns(4).spacing([16.0, 10.0]).show(ui, |ui| {
            for (n, c) in PALETTE.into_iter().enumerate() {
                let c = to_c32(c);
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                let active = c == color;
                if active {
                    ui.painter().circle_stroke(rect.center(), 14.5, Stroke::new(2.0, theme::ACCENT));
                }
                ui.painter().circle_filled(rect.center(), if active { 10.5 } else { 12.0 }, c);
                if c == Color32::from_rgb(24, 24, 24) || c == Color32::WHITE {
                    ui.painter().circle_stroke(rect.center(), if active { 10.5 } else { 12.0 }, Stroke::new(1.0, theme::BORDER_STRONG));
                }
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    color = c;
                }
                if n % 4 == 3 {
                    ui.end_row();
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            egui::color_picker::color_edit_button_srgba(ui, &mut color, egui::color_picker::Alpha::Opaque);
            let picking = self.picking.is_some();
            let tip = if picking { "Cancel (Esc)" } else { "Pick a color from the image (I)" };
            let resp = theme::icon_button_sized(ui, theme::icon::EYEDROPPER, tip, 30.0, true);
            if picking {
                ui.painter().rect_stroke(resp.rect, 8.0, Stroke::new(1.5, theme::ACCENT), StrokeKind::Inside);
            }
            if resp.clicked() {
                self.toggle_picking();
            }
            ui.label(egui::RichText::new(format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())).monospace().color(theme::TEXT_MUTED));
        });
        if self.picking.is_some() {
            ui.label(egui::RichText::new("Click anywhere on the image to pick its color").small().color(theme::ACCENT));
        }
        if color != self.color {
            self.set_color(color, false);
        }

        ui.add_space(14.0);
        theme::section(ui, "Style");
        let mut width = self.width;
        let label = ui.label(egui::RichText::new("Line width").color(theme::TEXT_MUTED));
        ui.add(egui::Slider::new(&mut width, 1.0..=40.0).max_decimals(0)).labelled_by(label.id);
        if width != self.width {
            self.width = width;
            if let Some(i) = selected_idx {
                self.gesture_undo();
                self.anns[i].width = width;
            }
        }
        let mut size = self.font_size;
        let label = ui.label(egui::RichText::new("Text size").color(theme::TEXT_MUTED));
        ui.add(egui::Slider::new(&mut size, 8.0..=300.0).logarithmic(true).max_decimals(0)).labelled_by(label.id);
        if size != self.font_size {
            self.font_size = size;
            if let Some(i) = selected_idx.filter(|&i| matches!(self.anns[i].kind, Kind::Text { .. })) {
                self.gesture_undo();
                if let Kind::Text { size: s, .. } = &mut self.anns[i].kind {
                    *s = size;
                }
            }
        }
        ui.add_space(6.0);
        let mut filled = self.filled;
        if theme::switch(ui, &mut filled, "Fill shapes").changed() {
            self.filled = filled;
            if let Some(i) = selected_idx.filter(|&i| matches!(self.anns[i].kind, Kind::Rect { .. } | Kind::Ellipse { .. })) {
                self.push_undo();
                self.anns[i].filled = filled;
            }
        }
        let mut text_bg = self.text_bg;
        if theme::switch(ui, &mut text_bg, "Callout box").on_hover_text("Text on a colored box").changed() {
            self.text_bg = text_bg;
            if let Some(i) = selected_idx.filter(|&i| matches!(self.anns[i].kind, Kind::Text { .. })) {
                self.push_undo();
                if let Kind::Text { background, .. } = &mut self.anns[i].kind {
                    *background = text_bg;
                }
            }
        }
        if selected_idx.is_some() {
            ui.add_space(14.0);
            theme::section(ui, "Selection");
            if theme::danger_button(ui, Some(theme::icon::DELETE), "Delete").on_hover_text("⌫").clicked() {
                self.delete_selected();
            }
        }
    }

    /// Zoom controls and hints, shown under the canvas.
    pub fn status_bar(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, icon};
        let ppp = ui.ctx().pixels_per_point();
        if theme::icon_button_sized(ui, icon::ZOOM_OUT, "Zoom out", 28.0, true).clicked() {
            self.zoom = (self.zoom / 1.25).max(0.02);
            self.fit = false;
        }
        ui.label(egui::RichText::new(format!("{:.0}%", self.zoom * ppp * 100.0)).font(theme::medium(12.5)).color(theme::TEXT));
        if theme::icon_button_sized(ui, icon::ZOOM_IN, "Zoom in", 28.0, true).clicked() {
            self.zoom = (self.zoom * 1.25).min(16.0);
            self.fit = false;
        }
        if theme::icon_button_sized(ui, icon::FIT, "Fit to window (0)", 28.0, true).clicked() {
            self.fit = true;
        }
        if ui.add(egui::Button::new(egui::RichText::new("1:1").font(theme::medium(12.0))).frame(false)).on_hover_text("Actual size").clicked() {
            self.zoom = 1.0 / ppp;
            self.fit = false;
        }
        ui.add_space(10.0);
        let hint = match self.tool {
            Tool::Select => "Click to select · drag to move · ⌫ deletes",
            Tool::Text => "Click to place text · Esc to finish",
            Tool::Step => "Click to drop the next number",
            Tool::Crop => "Drag over the area to keep",
            _ => "Drag to draw · hold ⇧ to constrain",
        };
        ui.label(egui::RichText::new(hint).color(theme::TEXT_FAINT));
    }

    pub fn canvas(&mut self, ui: &mut egui::Ui) {
        self.ensure_texture(ui.ctx());
        self.ensure_layer_textures(ui.ctx());
        let Some(tex_id) = self.texture.as_ref().map(|t| t.id()) else { return };
        let img_size = Vec2::new(self.base.width() as f32, self.base.height() as f32);
        let avail = ui.available_size();
        if self.fit {
            let ppp = ui.ctx().pixels_per_point();
            let margin = 64.0;
            self.zoom = ((avail.x - margin) / img_size.x)
                .min((avail.y - margin) / img_size.y)
                .min(1.0 / ppp * 2.0)
                .max(0.02);
        }
        if ui.rect_contains_pointer(ui.max_rect()) {
            let dz = ui.input(|i| i.zoom_delta());
            if dz != 1.0 {
                self.zoom = (self.zoom * dz).clamp(0.02, 16.0);
                self.fit = false;
            }
        }

        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let size = img_size * self.zoom;
            let area = size.max(ui.available_size());
            let (outer, resp) = ui.allocate_exact_size(area, Sense::click_and_drag());
            let origin = Pos2::new(
                if size.x < area.x { outer.center().x - size.x / 2.0 } else { outer.min.x },
                if size.y < area.y { outer.center().y - size.y / 2.0 } else { outer.min.y },
            );
            let img_rect = Rect::from_min_size(origin, size);
            let zoom = self.zoom;
            let to_img = |p: Pos2| P::new((p.x - origin.x) / zoom, (p.y - origin.y) / zoom);
            let to_screen = |p: P| Pos2::new(origin.x + p.x * zoom, origin.y + p.y * zoom);

            // Input
            let mut pick_preview: Option<(Pos2, Color32, (u32, u32))> = None;
            if let Some(snap) = self.picking.clone() {
                ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
                if let Some(h) = resp.hover_pos() {
                    let p = to_img(h);
                    if p.x >= 0.0 && p.y >= 0.0 && (p.x as u32) < snap.width() && (p.y as u32) < snap.height() {
                        let (x, y) = (p.x as u32, p.y as u32);
                        let px = snap.get_pixel(x, y);
                        pick_preview = Some((h, Color32::from_rgb(px[0], px[1], px[2]), (x, y)));
                    }
                }
                if resp.clicked()
                    && let Some((_, c, _)) = pick_preview
                {
                    self.set_color(c, true);
                    self.picking = None;
                }
            } else {
                let shift = ui.input(|i| i.modifiers.shift);
                if resp.drag_started()
                    && let Some(p) = ui.input(|i| i.pointer.press_origin()) {
                        self.drag_begin(to_img(p));
                    }
                if resp.dragged()
                    && let Some(p) = resp.interact_pointer_pos() {
                        self.drag_update(to_img(p), shift);
                    }
                if resp.drag_stopped() {
                    self.drag_end();
                }
                if resp.double_clicked() {
                    if let Some(i) = resp
                        .interact_pointer_pos()
                        .and_then(|p| self.hit_test(to_img(p)))
                        .filter(|&i| matches!(self.anns[i].kind, Kind::Text { .. }))
                    {
                        self.start_text_edit(i);
                    }
                } else if resp.clicked()
                    && let Some(p) = resp.interact_pointer_pos() {
                        self.click(to_img(p));
                    }
            }
            if self.picking.is_none()
                && let Some(hover) = resp.hover_pos()
            {
                let icon = match self.tool {
                    Tool::Select if self.hit_test(to_img(hover)).is_some() => CursorIcon::Move,
                    Tool::Select => CursorIcon::Default,
                    Tool::Text => CursorIcon::Text,
                    _ => CursorIcon::Crosshair,
                };
                ui.ctx().set_cursor_icon(icon);
            }
            if self.texture_dirty {
                ui.ctx().request_repaint();
            }

            // Paint
            let painter = ui.painter_at(outer);
            paint_backdrop(&painter, outer);
            painter.add(crate::theme::soft_shadow(img_rect, 4));
            if self.base.get_pixel(0, 0)[3] < 255 {
                paint_checkerboard(&painter, img_rect);
            }
            painter.image(tex_id, img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            let layers = &self.layer_textures;
            let layer_tex = |img: &Arc<RgbaImage>| layers.get(&(Arc::as_ptr(img) as usize)).map(|t| t.id());
            for (i, ann) in self.anns.iter().enumerate() {
                let editing = self.editing_text == Some(i);
                paint_annotation(&painter, ann, &to_screen, zoom, editing, &layer_tex);
            }
            if let Some((_, corner)) = self.resize_handle() {
                let c = to_screen(corner);
                painter.circle_filled(c, 7.0, Color32::WHITE);
                painter.circle_stroke(c, 7.0, Stroke::new(2.0, crate::theme::ACCENT));
                if resp.hover_pos().is_some_and(|h| h.distance(c) <= 12.0) || self.resizing.is_some() {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeNwSe);
                }
            }
            if let Some(ann) = &self.drawing {
                paint_annotation(&painter, ann, &to_screen, zoom, false, &layer_tex);
                if ann.is_raster() {
                    let (a, b) = ann.bounds();
                    dashed_rect(&painter, Rect::from_two_pos(to_screen(a), to_screen(b)));
                }
            }
            if let Some(i) = self.selected.filter(|&i| i < self.anns.len()) {
                let (a, b) = self.anns[i].bounds();
                dashed_rect(&painter, Rect::from_two_pos(to_screen(a), to_screen(b)).expand(4.0));
            }
            if let Some((a, b)) = self.crop_drag {
                let keep = Rect::from_two_pos(to_screen(a), to_screen(b)).intersect(img_rect);
                let shade = Color32::from_black_alpha(140);
                for r in [
                    Rect::from_min_max(img_rect.min, Pos2::new(img_rect.max.x, keep.min.y)),
                    Rect::from_min_max(Pos2::new(img_rect.min.x, keep.max.y), img_rect.max),
                    Rect::from_min_max(Pos2::new(img_rect.min.x, keep.min.y), Pos2::new(keep.min.x, keep.max.y)),
                    Rect::from_min_max(Pos2::new(keep.max.x, keep.min.y), Pos2::new(img_rect.max.x, keep.max.y)),
                ] {
                    painter.rect_filled(r, 0.0, shade);
                }
                painter.rect_stroke(keep, 0.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Outside);
            }

            if let (Some((at, color, (cx, cy))), Some(snap)) = (pick_preview, &self.picking) {
                paint_loupe(&painter, snap, at, (cx, cy), color, outer);
                ui.ctx().request_repaint();
            }

            self.text_editor(ui, &to_screen);
        });
    }

    fn text_editor(&mut self, ui: &mut egui::Ui, to_screen: &dyn Fn(P) -> Pos2) {
        let Some(i) = self.editing_text else { return };
        let zoom = self.zoom;
        let Some(ann) = self.anns.get_mut(i) else {
            self.editing_text = None;
            return;
        };
        let color = ann.color;
        let Kind::Text { pos, text, size, background } = &mut ann.kind else {
            self.editing_text = None;
            return;
        };
        let text_color = to_c32(if *background { contrast_on(color) } else { color });
        let font = FontId::new(*size * zoom, FontFamily::Name(ANNOT_FONT.into()));
        // Wide enough for the whole text plus room to type: committed text never
        // wraps, so the editor must not either.
        let min_width = (*size * zoom * 4.0).max(60.0).max((render::measure_text(text, *size).0 + *size * 2.0) * zoom);
        let screen = to_screen(*pos);
        let mut focus = std::mem::take(&mut self.focus_text);
        let mut finished = false;
        let input_id = egui::Id::unique("snapquill-text-input");
        egui::Area::new(egui::Id::unique("snapquill-text-edit"))
            .fixed_pos(screen)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                // egui's text box lays text out in typing order, which is wrong
                // for Hebrew. It stays as an invisible input layer (IME,
                // clipboard, selection all keep working) and the text, caret
                // and selection are painted below in visual order.
                let selection_fill = ui.visuals().selection.bg_fill;
                ui.visuals_mut().selection.bg_fill = Color32::TRANSPARENT;
                ui.visuals_mut().selection.stroke = Stroke::NONE;
                ui.visuals_mut().text_cursor.stroke = Stroke::NONE;

                rtl_arrow_keys(ui, input_id, text);

                let edit = egui::TextEdit::multiline(text)
                    .id(input_id)
                    .font(font.clone())
                    .text_color(Color32::TRANSPARENT)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .desired_rows(1)
                    .desired_width(min_width)
                    .clip_text(false);
                let out = edit.show(ui);
                let r = out.response;
                if focus {
                    r.request_focus();
                    focus = false;
                }
                let range = out.cursor_range.map(|c| {
                    let [a, b] = c.sorted_cursors();
                    (a.index.0, b.index.0, c.primary.index.0)
                });
                paint_visual_text(ui.painter(), out.galley_pos, text, *size, zoom, text_color, selection_fill, range);
                let (tw, th) = render::measure_text(text, *size);
                let outline = Rect::from_min_size(out.galley_pos, Vec2::new(tw.max(*size * 0.5) * zoom, th * zoom));
                ui.painter().rect_stroke(outline.expand(3.0), 2.0, Stroke::new(1.5, crate::theme::ACCENT), StrokeKind::Outside);
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    finished = true;
                }
            });
        self.focus_text = focus;
        self.dirty = true;
        if finished {
            ui.ctx().memory_mut(|m| m.stop_text_input());
            self.commit_text();
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, action: &mut EditorAction) {
        let typing = self.editing_text.is_some() || ctx.memory(|m| m.focused().is_some());
        // ⌘C arrives as a Copy event on most platforms, or as a plain key press.
        let copy_requested = ctx.input(|i| {
            i.events.iter().any(|e| matches!(e, egui::Event::Copy))
                || (i.modifiers.command && i.key_pressed(Key::C))
        });
        if typing {
            return;
        }
        if copy_requested {
            *action = self.copy_action();
        }
        // ⌘V: egui sends `PasteImage` when the clipboard holds only an image,
        // and `Paste` (text) otherwise — e.g. a file copied in Finder.
        let pasted_image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::PasteImage(img) => Some(crate::platform::color_image_to_rgba(img)),
                _ => None,
            })
        });
        let paste_requested = ctx.input(|i| {
            i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))
                || (i.modifiers.command && i.key_pressed(Key::V))
        });
        if let Some(img) = pasted_image {
            self.paste_image(img);
            *action = EditorAction::Toast("Image pasted — drag to move, pull the corner to resize".into());
        } else if paste_requested {
            *action = self.paste_from_clipboard();
        }
        let cmd = egui::Modifiers::COMMAND;
        let cmd_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        let (redo, undo, save, delete, backspace, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(cmd_shift, Key::Z),
                i.consume_key(cmd, Key::Z),
                i.consume_key(cmd, Key::S),
                i.consume_key(egui::Modifiers::NONE, Key::Delete),
                i.consume_key(egui::Modifiers::NONE, Key::Backspace),
                i.consume_key(egui::Modifiers::NONE, Key::Escape),
            )
        });
        if redo {
            self.redo();
        } else if undo {
            self.undo();
        }
        if save {
            *action = self.save_action();
        }
        if delete || backspace {
            self.delete_selected();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::I)) {
            self.toggle_picking();
        }
        if escape && self.picking.take().is_some() {
            return;
        }
        if escape {
            self.drawing = None;
            self.crop_drag = None;
            self.selected = None;
        }
        for tool in Tool::ALL {
            let key = tool.info().2;
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key)) {
                self.set_tool(tool);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Num0)) {
            self.fit = true;
        }
    }

    fn save_action(&mut self) -> EditorAction {
        match self.save() {
            Ok(()) => EditorAction::Toast("Saved to library".into()),
            Err(e) => EditorAction::Toast(format!("Save failed: {e}")),
        }
    }

    fn copy_action(&mut self) -> EditorAction {
        self.commit_text();
        match crate::platform::copy_image(&self.flatten()) {
            Ok(()) => EditorAction::Toast("Image copied to clipboard".into()),
            Err(e) => EditorAction::Toast(format!("Copy failed: {e}")),
        }
    }

    fn export_as(&mut self, fmt: ExportFormat) -> EditorAction {
        self.commit_text();
        let name = self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "capture".into());
        let (label, _) = fmt.describe();
        let Some(mut path) = rfd::FileDialog::new()
            .set_file_name(format!("{name}.{}", fmt.ext()))
            .add_filter(label, &[fmt.ext()])
            .save_file()
        else {
            return EditorAction::None;
        };
        if path.extension().is_none() {
            path.set_extension(fmt.ext());
        }
        match write_image(&self.flatten(), &path) {
            Ok(()) => EditorAction::Toast(format!("Exported {}", path.file_name().unwrap_or_default().to_string_lossy())),
            Err(e) => EditorAction::Toast(format!("Export failed: {e}")),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportFormat {
    Png,
    Jpeg,
    Pdf,
    Webp,
    Tiff,
    Bmp,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 6] =
        [ExportFormat::Png, ExportFormat::Jpeg, ExportFormat::Pdf, ExportFormat::Webp, ExportFormat::Tiff, ExportFormat::Bmp];

    pub fn ext(self) -> &'static str {
        match self {
            ExportFormat::Png => "png",
            ExportFormat::Jpeg => "jpg",
            ExportFormat::Pdf => "pdf",
            ExportFormat::Webp => "webp",
            ExportFormat::Tiff => "tiff",
            ExportFormat::Bmp => "bmp",
        }
    }

    /// (name, one-line hint)
    pub fn describe(self) -> (&'static str, &'static str) {
        match self {
            ExportFormat::Png => ("PNG", "Sharp, lossless, keeps transparency"),
            ExportFormat::Jpeg => ("JPEG", "Smallest files, good for photos"),
            ExportFormat::Pdf => ("PDF", "One-page document"),
            ExportFormat::Webp => ("WebP", "Lossless, small, for the web"),
            ExportFormat::Tiff => ("TIFF", "Lossless, for print & archives"),
            ExportFormat::Bmp => ("BMP", "Uncompressed bitmap"),
        }
    }
}

/// Image on a white background, for formats without transparency.
fn flatten_on_white(img: &RgbaImage) -> image::RgbImage {
    image::RgbImage::from_fn(img.width(), img.height(), |x, y| {
        let p = img.get_pixel(x, y);
        let a = p[3] as f32 / 255.0;
        let mix = |c: u8| (c as f32 * a + 255.0 * (1.0 - a)).round() as u8;
        image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])])
    })
}

/// Save by extension; formats without alpha are flattened onto white.
pub fn write_image(img: &RgbaImage, path: &std::path::Path) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "bmp" => flatten_on_white(img).save(path)?,
        "pdf" => std::fs::write(path, pdf_with_image(img)?)?,
        _ => img.save(path)?,
    }
    Ok(())
}

/// A minimal one-page PDF whose page is exactly the image (JPEG-compressed).
/// 1 image pixel = 0.75 pt, i.e. 96 dpi — the size you'd see on screen.
fn pdf_with_image(img: &RgbaImage) -> Result<Vec<u8>> {
    let rgb = flatten_on_white(img);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92).encode_image(&rgb)?;
    let (w, h) = (img.width(), img.height());
    let (pw, ph) = (w as f32 * 0.75, h as f32 * 0.75);
    let content = format!("q {pw:.2} 0 0 {ph:.2} 0 0 cm /Im0 Do Q");

    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    let mut obj = |out: &mut Vec<u8>, body: &[u8]| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };
    obj(&mut out, b"<< /Type /Catalog /Pages 2 0 R >>");
    obj(&mut out, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    obj(
        &mut out,
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] \
             /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
        )
        .as_bytes(),
    );
    let mut image_obj = format!(
        "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB \
         /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
        jpeg.len()
    )
    .into_bytes();
    image_obj.extend_from_slice(&jpeg);
    image_obj.extend_from_slice(b"\nendstream");
    obj(&mut out, &image_obj);
    obj(&mut out, format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).as_bytes());

    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", offsets.len() + 1).as_bytes());
    Ok(out)
}

/// A row in the Export menu: format name with a hint underneath.
fn export_row(ui: &mut egui::Ui, name: &str, hint: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(250.0, 42.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 8.0, crate::theme::SURFACE_3);
    }
    p.text(rect.left_top() + Vec2::new(12.0, 7.0), Align2::LEFT_TOP, name, crate::theme::medium(13.5), crate::theme::TEXT);
    p.text(rect.left_bottom() + Vec2::new(12.0, -7.0), Align2::LEFT_BOTTOM, hint, FontId::proportional(11.5), crate::theme::TEXT_MUTED);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn snap_45(a: P, p: P) -> P {
    let (dx, dy) = (p.x - a.x, p.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    let angle = (dy.atan2(dx) / std::f32::consts::FRAC_PI_4).round() * std::f32::consts::FRAC_PI_4;
    P::new(a.x + len * angle.cos(), a.y + len * angle.sin())
}

fn square(a: P, p: P) -> P {
    let side = (p.x - a.x).abs().max((p.y - a.y).abs());
    P::new(a.x + side * (p.x - a.x).signum(), a.y + side * (p.y - a.y).signum())
}

/// Magnified pixels around the cursor with the picked color and its hex code.
fn paint_loupe(painter: &egui::Painter, img: &RgbaImage, at: Pos2, (cx, cy): (u32, u32), color: Color32, bounds: Rect) {
    const CELLS: i32 = 11;
    const CELL: f32 = 10.0;
    let size = CELLS as f32 * CELL;
    let mut min = at + Vec2::new(22.0, 22.0);
    if min.x + size > bounds.max.x {
        min.x = at.x - 22.0 - size;
    }
    if min.y + size + 34.0 > bounds.max.y {
        min.y = at.y - 22.0 - size - 34.0;
    }
    let r = Rect::from_min_size(min, Vec2::splat(size));
    painter.add(crate::theme::soft_shadow(r, 10));
    painter.rect_filled(r.expand(3.0), 10.0, crate::theme::SURFACE);
    let half = CELLS / 2;
    for dy in -half..=half {
        for dx in -half..=half {
            let (x, y) = (cx as i32 + dx, cy as i32 + dy);
            let c = if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
                let p = img.get_pixel(x as u32, y as u32);
                Color32::from_rgb(p[0], p[1], p[2])
            } else {
                crate::theme::BG
            };
            let cell = Rect::from_min_size(
                r.min + Vec2::new((dx + half) as f32 * CELL, (dy + half) as f32 * CELL),
                Vec2::splat(CELL),
            );
            painter.rect_filled(cell, 0.0, c);
        }
    }
    let center = Rect::from_min_size(r.min + Vec2::splat(half as f32 * CELL), Vec2::splat(CELL));
    painter.rect_stroke(center, 0.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Outside);
    painter.rect_stroke(center.expand(1.5), 0.0, Stroke::new(1.0, Color32::BLACK), StrokeKind::Outside);
    painter.rect_stroke(r, 8.0, Stroke::new(1.0, crate::theme::BORDER_STRONG), StrokeKind::Outside);

    let label = Rect::from_min_size(Pos2::new(r.min.x, r.max.y + 8.0), Vec2::new(size, 26.0));
    painter.rect_filled(label, 8.0, crate::theme::SURFACE_3);
    let swatch = Rect::from_center_size(label.left_center() + Vec2::new(14.0, 0.0), Vec2::splat(14.0));
    painter.rect_filled(swatch, 4.0, color);
    painter.rect_stroke(swatch, 4.0, Stroke::new(1.0, Color32::from_white_alpha(60)), StrokeKind::Inside);
    painter.text(
        label.left_center() + Vec2::new(28.0, 0.0),
        Align2::LEFT_CENTER,
        format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b()),
        FontId::monospace(12.0),
        crate::theme::TEXT,
    );
}

/// Subtle dot grid behind the image.
fn paint_backdrop(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, crate::theme::BG);
    let step = 22.0;
    let dot = Color32::from_rgb(30, 34, 42);
    let mut y = rect.min.y - rect.min.y.rem_euclid(step);
    while y < rect.max.y {
        let mut x = rect.min.x - rect.min.x.rem_euclid(step);
        while x < rect.max.x {
            painter.circle_filled(Pos2::new(x, y), 1.0, dot);
            x += step;
        }
        y += step;
    }
}

fn dashed_rect(painter: &egui::Painter, r: Rect) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    painter.extend(Shape::dashed_line(&pts, Stroke::new(1.5, crate::theme::ACCENT), 6.0, 4.0));
    painter.extend(Shape::dashed_line_with_offset(&pts, Stroke::new(1.5, Color32::WHITE), &[4.0], &[6.0], 6.0));
}

fn paint_annotation(
    painter: &egui::Painter,
    ann: &Annotation,
    to_screen: &dyn Fn(P) -> Pos2,
    zoom: f32,
    editing: bool,
    layer_tex: &dyn Fn(&Arc<RgbaImage>) -> Option<egui::TextureId>,
) {
    let color = to_c32(ann.color);
    let w = (ann.width * zoom).max(1.0);
    let stroke = Stroke::new(w, color);
    match &ann.kind {
        Kind::Arrow { a, b } => {
            let (end, head) = arrow_geometry(*a, *b, ann.width);
            painter.line_segment([to_screen(*a), to_screen(end)], stroke);
            painter.add(Shape::convex_polygon(head.iter().map(|p| to_screen(*p)).collect(), color, Stroke::NONE));
        }
        Kind::Line { a, b } => {
            painter.line_segment([to_screen(*a), to_screen(*b)], stroke);
        }
        Kind::Rect { a, b } => {
            let r = Rect::from_two_pos(to_screen(*a), to_screen(*b));
            if ann.filled {
                painter.rect_filled(r, 0.0, color);
            } else {
                painter.rect_stroke(r, 0.0, stroke, StrokeKind::Middle);
            }
        }
        Kind::Ellipse { a, b } => {
            let pts: Vec<Pos2> = ellipse_points(*a, *b, 96).into_iter().map(to_screen).collect();
            if ann.filled {
                painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
            } else {
                painter.add(Shape::closed_line(pts, stroke));
            }
        }
        Kind::Pen { pts } => paint_stroke(painter, pts, to_screen, stroke),
        Kind::Highlight { pts } => {
            let c = Color32::from_rgba_unmultiplied(ann.color[0], ann.color[1], ann.color[2], render::HIGHLIGHT_ALPHA);
            paint_stroke(painter, pts, to_screen, Stroke::new(w * render::HIGHLIGHT_WIDTH_FACTOR, c));
        }
        Kind::Text { pos, text, size, background } => {
            let p = to_screen(*pos);
            let text_color = if *background {
                let pad = size * 0.35;
                let (tw, th) = render::measure_text(text, *size);
                let r = Rect::from_min_size(
                    to_screen(P::new(pos.x - pad, pos.y - pad)),
                    Vec2::new((tw.max(size * 0.5) + pad * 2.0) * zoom, (th + pad * 2.0) * zoom),
                );
                painter.rect_filled(r, CornerRadius::same((pad * zoom).min(255.0) as u8), color);
                to_c32(contrast_on(ann.color))
            } else {
                color
            };
            if !editing {
                paint_visual_text(painter, p, text, *size, zoom, text_color, Color32::TRANSPARENT, None);
            }
        }
        Kind::Step { pos, n, radius } => {
            let c = to_screen(*pos);
            painter.circle_filled(c, radius * zoom, color);
            let font = FontId::new(radius * 1.15 * zoom, FontFamily::Name(ANNOT_FONT.into()));
            painter.text(c, Align2::CENTER_CENTER, n.to_string(), font, to_c32(contrast_on(ann.color)));
        }
        Kind::Image { pos, w, h, img } => {
            let r = Rect::from_min_max(to_screen(*pos), to_screen(P::new(pos.x + w, pos.y + h)));
            if let Some(tex) = layer_tex(img) {
                painter.image(tex, r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
        }
        Kind::Blur { .. } | Kind::Spotlight { .. } => {}
    }
}

/// Hebrew lines: the left/right arrows move the caret by what is on screen,
/// not by typing order. Consumes the key before the text box sees it.
fn rtl_arrow_keys(ui: &egui::Ui, id: egui::Id, text: &str) {
    use egui::text::{CCursor, CCursorRange};
    let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), id) else { return };
    let Some(range) = state.cursor.char_range() else { return };
    let chars: Vec<char> = text.chars().collect();
    let p = range.primary.index.0.min(chars.len());
    let start = chars[..p].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
    let end = chars[p..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| p + i);
    let line: String = chars[start..end].iter().collect();
    if !render::visual_chars(&line).iter().any(|v| v.rtl) {
        return;
    }
    for (key, step) in [(Key::ArrowLeft, 1isize), (Key::ArrowRight, -1)] {
        for shift in [false, true] {
            let mods = if shift { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE };
            if ui.input_mut(|i| i.consume_key(mods, key)) {
                let np = (p as isize + step).clamp(start as isize, end as isize) as usize;
                let new = if shift { CCursorRange::two(range.secondary, CCursor::new(np)) } else { CCursorRange::one(CCursor::new(np)) };
                state.cursor.set_char_range(Some(new));
                state.store(ui.ctx(), id);
                return;
            }
        }
    }
}

/// Paint `text` line by line, glyph by glyph, from the same layout the export
/// uses (egui's own layout only reorders Hebrew per word). `range` = selection
/// start, end and caret as logical char indices; the caret and selection are
/// placed by logical position.
fn paint_visual_text(
    painter: &egui::Painter,
    origin: Pos2,
    text: &str,
    size: f32,
    zoom: f32,
    color: Color32,
    selection_fill: Color32,
    range: Option<(usize, usize, usize)>,
) {
    let font = FontId::new(size * zoom, FontFamily::Name(ANNOT_FONT.into()));
    let row_h = render::line_height(size) * zoom;
    let mut offset = 0;
    let mut y = origin.y;
    for line in text.split('\n') {
        let n = line.chars().count();
        let glyphs = render::layout_line(line, size).0;
        // Horizontal extent of each logical char.
        let mut spans = vec![(origin.x, origin.x, false); n];
        for g in &glyphs {
            let x0 = origin.x + g.x * zoom;
            spans[g.logical] = (x0, x0 + g.advance * zoom, g.rtl);
        }
        if let Some((a, b, _)) = range {
            for (j, &(x0, x1, _)) in spans.iter().enumerate() {
                if (a..b).contains(&(offset + j)) {
                    painter.rect_filled(Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, y + row_h)), 0.0, selection_fill);
                }
            }
        }
        for g in &glyphs {
            if !g.ch.is_whitespace() {
                painter.text(Pos2::new(origin.x + g.x * zoom, y), Align2::LEFT_TOP, g.ch, font.clone(), color);
            }
        }
        if let Some((_, _, caret)) = range
            && (offset..=offset + n).contains(&caret)
        {
            let i = caret - offset;
            // The caret hugs the character typed before it (or the first one).
            let x = match (i.checked_sub(1).map(|j| spans[j]), spans.first()) {
                (Some((x0, x1, rtl)), _) => if rtl { x0 } else { x1 },
                (None, Some(&(x0, x1, rtl))) => if rtl { x1 } else { x0 },
                (None, None) => origin.x,
            };
            painter.line_segment([Pos2::new(x, y), Pos2::new(x, y + row_h)], Stroke::new(1.5, color));
        }
        offset += n + 1;
        y += row_h;
    }
}

/// Grey/white squares that show where a canvas is transparent.
fn paint_checkerboard(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, Color32::from_gray(235));
    let cell = 10.0;
    let mut y = rect.min.y;
    let mut row = 0;
    while y < rect.max.y {
        let mut x = rect.min.x + if row % 2 == 0 { 0.0 } else { cell };
        while x < rect.max.x {
            let r = Rect::from_min_max(Pos2::new(x, y), Pos2::new((x + cell).min(rect.max.x), (y + cell).min(rect.max.y)));
            painter.rect_filled(r, 0.0, Color32::from_gray(200));
            x += cell * 2.0;
        }
        y += cell;
        row += 1;
    }
}

fn paint_stroke(painter: &egui::Painter, pts: &[P], to_screen: &dyn Fn(P) -> Pos2, stroke: Stroke) {
    if pts.len() == 1 {
        painter.circle_filled(to_screen(pts[0]), stroke.width / 2.0, stroke.color);
    } else {
        painter.add(Shape::line(pts.iter().map(|p| to_screen(*p)).collect(), stroke));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_every_format() {
        let dir = std::env::temp_dir().join(format!("snapquill-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = RgbaImage::from_fn(64, 40, |x, y| image::Rgba([x as u8 * 4, y as u8 * 6, 120, 255]));
        for fmt in ExportFormat::ALL {
            let path = dir.join(format!("out.{}", fmt.ext()));
            write_image(&img, &path).unwrap_or_else(|e| panic!("{fmt:?}: {e}"));
            if fmt == ExportFormat::Pdf {
                let bytes = std::fs::read(&path).unwrap();
                assert!(bytes.starts_with(b"%PDF-1.4"));
                assert!(bytes.ends_with(b"%%EOF\n"));
            } else {
                let back = image::open(&path).unwrap_or_else(|e| panic!("{fmt:?} unreadable: {e}"));
                assert_eq!((back.width(), back.height()), (64, 40), "{fmt:?}");
            }
        }
        println!("exported to {}", dir.display());
    }

    /// Typing Hebrew in the text tool: the box shows it in reading order and
    /// the arrow keys follow the screen, not typing order.
    #[test]
    fn hebrew_editing_is_visual() {
        let mut ed = Editor::new(PathBuf::from("unused.png"), RgbaImage::from_pixel(520, 160, image::Rgba([255, 255, 255, 255])), None, true);
        ed.anns.push(Annotation {
            kind: Kind::Text { pos: P::new(40.0, 40.0), text: String::new(), size: 36.0, background: false },
            color: [235, 40, 40, 255],
            width: 4.0,
            filled: false,
        });
        ed.editing_text = Some(0);
        ed.focus_text = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(Vec2::new(520.0, 160.0))
            .with_pixels_per_point(2.0)
            .with_accessibility_check(false)
            .with_context_setup(|ctx| crate::theme::install(ctx))
            .wgpu()
            .build_ui(move |ui| {
                ed.canvas(ui);
                // Expose the edited text to the test through egui memory.
                if let Some(Kind::Text { text, .. }) = ed.anns.first().map(|a| &a.kind) {
                    ui.data_mut(|d| d.insert_temp(egui::Id::unique("test-text"), text.clone()));
                }
            });
        for _ in 0..3 {
            harness.step();
        }
        harness.event(egui::Event::Text("שלום עולם".into()));
        for _ in 0..3 {
            harness.step();
        }
        let typed: String = harness.ctx.data(|d| d.get_temp(egui::Id::unique("test-text"))).unwrap_or_default();
        assert_eq!(typed, "שלום עולם");

        let shot = |h: &mut egui_kittest::Harness<'_>, name: &str| {
            if let Ok(dir) = std::env::var("HEBREW_PREVIEW_DIR") {
                h.render().unwrap().save(std::path::Path::new(&dir).join(name)).unwrap();
            }
        };
        shot(&mut harness, "edit-end.png");
        // Caret is at the logical end = visual left. Right arrow moves it
        // back toward the start of the word (visually right).
        for _ in 0..3 {
            harness.key_press(Key::ArrowRight);
            harness.step();
        }
        shot(&mut harness, "edit-moved.png");
    }

    /// ⌘V with only an image on the clipboard reaches us as `PasteImage`.
    #[test]
    fn paste_image_event_adds_a_layer() {
        let ctx = egui::Context::default();
        let mut ed = Editor::new(PathBuf::from("unused.png"), RgbaImage::new(400, 300), None, true);
        let pasted = egui::ColorImage::new([20, 10], vec![Color32::RED; 200]);
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::PasteImage(Arc::new(pasted)));
        let _ = ctx.run_ui(input, |ui| {
            ed.end_frame(ui.ctx());
        });
        assert_eq!(ed.anns.len(), 1);
        let Kind::Image { w, h, img, .. } = &ed.anns[0].kind else { panic!("expected an image layer") };
        assert_eq!((img.width(), img.height()), (20, 10));
        assert_eq!((*w, *h), (20.0, 10.0)); // small pastes stay 1:1
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(ed.selected, Some(0));
    }
}
