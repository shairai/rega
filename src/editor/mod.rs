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
use model::{Annotation, BlurStyle, Effects, Kind, P, Project, Rgba, Stamp, StampSymbol, arrow_geometry, contrast_on, ellipse_points, norm, stamp_symbol};

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
    Stamp,
    Magnify,
    Blur,
    Spotlight,
    Fill,
    Eraser,
    Crop,
}

impl Tool {
    const ALL: [Tool; 16] = [
        Tool::Select,
        Tool::Arrow,
        Tool::Line,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Pen,
        Tool::Highlight,
        Tool::Text,
        Tool::Step,
        Tool::Stamp,
        Tool::Magnify,
        Tool::Blur,
        Tool::Spotlight,
        Tool::Fill,
        Tool::Eraser,
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
            // No icon-font glyph: drawn by `paint_tool_icon`.
            Tool::Stamp => ("", "Stamp", Key::K),
            Tool::Magnify => (icon::MAGNIFIER, "Magnifier", Key::M),
            Tool::Blur => (icon::BLUR, "Blur", Key::B),
            Tool::Spotlight => (icon::SPOTLIGHT, "Spotlight", Key::S),
            Tool::Fill => ("", "Fill", Key::F),
            Tool::Eraser => ("", "Eraser", Key::X),
            Tool::Crop => (icon::CROP, "Crop", Key::C),
        }
    }

    /// The tool that makes this kind of mark (`None` for pasted images).
    fn for_kind(kind: &Kind) -> Option<Tool> {
        Some(match kind {
            Kind::Arrow { .. } => Tool::Arrow,
            Kind::Line { .. } => Tool::Line,
            Kind::Rect { .. } => Tool::Rect,
            Kind::Ellipse { .. } => Tool::Ellipse,
            Kind::Pen { .. } => Tool::Pen,
            Kind::Highlight { .. } => Tool::Highlight,
            Kind::Text { .. } => Tool::Text,
            Kind::Step { .. } => Tool::Step,
            Kind::Stamp { .. } => Tool::Stamp,
            Kind::Magnify { .. } => Tool::Magnify,
            Kind::Blur { .. } => Tool::Blur,
            Kind::Spotlight { .. } => Tool::Spotlight,
            Kind::Fill { .. } => Tool::Fill,
            Kind::Image { .. } => return None,
        })
    }

    /// Tools that draw by dragging: a plain click on a mark selects it instead.
    fn draws_by_drag(self) -> bool {
        matches!(
            self,
            Tool::Arrow | Tool::Line | Tool::Rect | Tool::Ellipse | Tool::Pen | Tool::Highlight | Tool::Magnify | Tool::Blur | Tool::Spotlight
        )
    }
}

/// Which properties the side panel shows for a tool (or a mark made by it).
#[derive(Default)]
struct Props {
    color: bool,
    width: Option<&'static str>,
    fill: bool,
    text: bool,
    step: bool,
    stamp: bool,
    zoom: bool,
    tolerance: bool,
    blur: bool,
    hint: Option<&'static str>,
}

impl Props {
    fn of(tool: Tool) -> Self {
        let color = true;
        match tool {
            Tool::Select => Props { hint: Some("Click a mark to see and change its properties."), ..Default::default() },
            Tool::Arrow | Tool::Line | Tool::Pen => Props { color, width: Some("Line width"), ..Default::default() },
            Tool::Rect | Tool::Ellipse => Props { color, width: Some("Line width"), fill: true, ..Default::default() },
            Tool::Highlight => Props { color, width: Some("Thickness"), ..Default::default() },
            Tool::Text => Props { color, text: true, ..Default::default() },
            Tool::Step => Props { color, step: true, ..Default::default() },
            Tool::Stamp => Props { color, stamp: true, ..Default::default() },
            Tool::Magnify => Props { color, width: Some("Ring width"), zoom: true, ..Default::default() },
            // Color and hint depend on the style; see `tools_panel`.
            Tool::Blur => Props { blur: true, ..Default::default() },
            Tool::Spotlight => Props { hint: Some("Drag around what matters; the rest is dimmed."), ..Default::default() },
            Tool::Fill => Props { color, tolerance: true, hint: Some("Click an area to flood it with the color."), ..Default::default() },
            Tool::Eraser => Props { width: Some("Eraser size"), hint: Some("Click or drag over marks to remove them."), ..Default::default() },
            Tool::Crop => Props { hint: Some("Drag over the area to keep."), ..Default::default() },
        }
    }
}

/// Icons for tools the icon font has no glyph for (about 23 pt across).
fn paint_tool_icon(tool: Tool, p: &egui::Painter, c: Pos2, color: Color32) {
    let s = Stroke::new(2.0, color);
    let v = Vec2::new;
    match tool {
        Tool::Stamp => {
            p.circle_stroke(c, 9.5, s);
            p.add(Shape::line(vec![c + v(-4.5, 0.5), c + v(-1.2, 3.8), c + v(4.8, -3.2)], s));
        }
        Tool::Fill => {
            // A tipped bucket, half full, and a drop.
            let pts = [c + v(-9.0, -1.0), c + v(-2.0, -8.0), c + v(5.5, -0.5), c + v(-1.5, 6.5)];
            p.add(Shape::convex_polygon(vec![pts[0], pts[2], pts[3]], color, Stroke::NONE));
            p.add(Shape::closed_line(pts.to_vec(), s));
            p.add(Shape::convex_polygon(vec![c + v(8.0, 0.5), c + v(10.6, 5.2), c + v(5.4, 5.2)], color, Stroke::NONE));
            p.circle_filled(c + v(8.0, 6.0), 2.6, color);
        }
        Tool::Eraser => {
            // A block tilted 45°, its tip filled, on a baseline.
            let k = std::f32::consts::FRAC_1_SQRT_2;
            let rot = |x: f32, y: f32| c + v((x + y) * k, (-x + y) * k) + v(0.0, -1.5);
            p.add(Shape::convex_polygon(vec![rot(-9.0, -4.5), rot(-2.0, -4.5), rot(-2.0, 4.5), rot(-9.0, 4.5)], color, Stroke::NONE));
            p.add(Shape::closed_line(vec![rot(-9.0, -4.5), rot(9.0, -4.5), rot(9.0, 4.5), rot(-9.0, 4.5)], s));
            p.line_segment([c + v(-9.0, 9.5), c + v(9.0, 9.5)], Stroke::new(1.6, color));
        }
        _ => {}
    }
}

/// A stamp badge at `center`, `diameter` across, on screen (also used for the picker).
fn paint_stamp(painter: &egui::Painter, center: Pos2, diameter: f32, stamp: Stamp, color: Color32) {
    painter.circle_filled(center, diameter / 2.0, color);
    let ink = to_c32(contrast_on(from_c32(color)));
    let at = |q: &P| Pos2::new(q.x, q.y);
    let fan = |pts: &[P], c: Color32| {
        // Fan from the first point (the outline is star-shaped around it), plus a
        // thin outline for anti-aliased edges.
        let mut mesh = egui::Mesh::default();
        for q in pts {
            mesh.colored_vertex(at(q), c);
        }
        for i in 1..pts.len() as u32 - 1 {
            mesh.add_triangle(0, i, i + 1);
        }
        painter.add(Shape::mesh(mesh));
        painter.add(Shape::closed_line(pts[1..].iter().map(at).collect(), Stroke::new(1.0, c)));
    };
    for part in stamp_symbol(stamp, P::new(center.x, center.y), diameter) {
        match part {
            StampSymbol::Strokes(lines, w) => {
                for line in lines {
                    let pts: Vec<Pos2> = line.iter().map(at).collect();
                    // Round caps and joins.
                    for q in &pts {
                        painter.circle_filled(*q, w / 2.0, ink);
                    }
                    painter.add(Shape::line(pts, Stroke::new(w, ink)));
                }
            }
            StampSymbol::Fill(pts) => fan(&pts, ink),
            StampSymbol::Cut(pts) => fan(&pts, color),
            StampSymbol::Glyph(g, size) => {
                painter.text(center, Align2::CENTER_CENTER, g, FontId::new(size, FontFamily::Name(ANNOT_FONT.into())), ink);
            }
        }
    }
}

/// How close (screen points) the pointer must be to grab a resize handle.
const HANDLE_GRAB: f32 = 10.0;
/// Gap (screen points) between a selected mark and its dashed outline.
const SELECTION_PAD: f32 = 4.0;

/// A corner resize handle: a small white square with an accent border.
fn paint_resize_handle(painter: &egui::Painter, c: Pos2, hot: bool) {
    let r = Rect::from_center_size(c, Vec2::splat(if hot { 11.0 } else { 9.0 }));
    painter.rect_filled(r.translate(Vec2::new(0.0, 0.5)).expand(1.0), 3.0, Color32::from_black_alpha(35));
    painter.rect_filled(r, 2.0, if hot { crate::theme::ACCENT } else { Color32::WHITE });
    painter.rect_stroke(r, 2.0, Stroke::new(1.5, if hot { Color32::WHITE } else { crate::theme::ACCENT }), StrokeKind::Inside);
}

/// Above this many pixels, moving a blur/spotlight re-rasters on release only.
const LIVE_RASTER_PIXELS: u64 = 8_000_000;

/// What `render::apply_raster` depends on, to tell whether the canvas texture is stale.
fn raster_key(anns: &[Annotation]) -> String {
    let raster: Vec<&Annotation> = anns.iter().filter(|a| a.is_raster()).collect();
    serde_json::to_string(&raster).unwrap_or_default()
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
    stamp: Stamp,
    stamp_size: f32,
    /// Magnifier enlargement for new loupes.
    loupe_zoom: f32,
    /// Paint-bucket tolerance, 0–255.
    fill_tolerance: f32,
    blur_style: BlurStyle,
    /// 1–100 for new blurs.
    blur_strength: f32,
    /// Picker hue, kept separately so greys (no hue) don't reset it.
    picker_hue: f32,
    /// The hex field while it is being typed in.
    hex_edit: Option<String>,

    zoom: f32,
    fit: bool,
    /// The flattened canvas as GPU tiles (image-pixel rect each): GPUs cap a
    /// texture side at 8192–16384 px and scrolling captures reach 40000.
    tiles: Vec<(TextureHandle, Rect)>,
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
    /// The `Weak` guards against a freed buffer's address being reused by a new paste.
    layer_textures: HashMap<usize, (std::sync::Weak<RgbaImage>, TextureHandle)>,
    /// The mark whose corner handle is being dragged, and which corner
    /// (0–3 clockwise from top-left, as in `resize_handles`).
    resizing: Option<(usize, usize)>,
    /// The mark right-clicked to open the canvas menu.
    menu_target: Option<usize>,
    dirty: bool,
}

fn to_c32(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
}

fn from_c32(c: Color32) -> Rgba {
    c.to_srgba_unmultiplied()
}

/// "#RRGGBB" or "RRGGBB" (also 3-digit "#RGB").
fn parse_hex(text: &str) -> Option<Color32> {
    let hex = text.trim().trim_start_matches('#');
    let digits: Vec<u8> = hex.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<_>>()?;
    match digits.as_slice() {
        [r, g, b] => Some(Color32::from_rgb(r * 17, g * 17, b * 17)),
        [r1, r2, g1, g2, b1, b2] => Some(Color32::from_rgb(r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2)),
        _ => None,
    }
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
            stamp: Stamp::Check,
            stamp_size: (unit * 14.0).round(),
            loupe_zoom: 2.0,
            // ~5%: UI borders are often within 10% of the background they enclose.
            fill_tolerance: 12.0,
            blur_style: BlurStyle::Pixelate,
            blur_strength: model::DEFAULT_BLUR_STRENGTH,
            picker_hue: 0.0,
            hex_edit: None,
            zoom: 1.0,
            fit: true,
            tiles: Vec::new(),
            texture_dirty: true,
            drawing: None,
            crop_drag: None,
            selected: None,
            moving: None,
            menu_target: None,
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

    /// For screenshot tests: pick a tool (by its shortcut letter) and a selected mark.
    #[cfg(test)]
    pub(crate) fn set_demo_state(&mut self, tool_key: Key, selected: Option<usize>) {
        if let Some(t) = Tool::ALL.into_iter().find(|t| t.info().2 == tool_key) {
            self.tool = t;
        }
        self.select(selected);
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
        let base_differs = !Arc::ptr_eq(&s.base, &self.base);
        if base_differs {
            self.base_changed = true;
            self.fit = true;
        }
        // Re-rastering a large canvas is costly; skip it when only vector shapes changed.
        if base_differs || raster_key(&s.anns) != raster_key(&self.anns) {
            self.texture_dirty = true;
        }
        self.base = s.base;
        self.anns = s.anns;
        self.effects = s.effects;
        self.next_step = s.next_step;
        self.selected = None;
        self.editing_text = None;
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
        // Picking another tool shows that tool's properties: drop a selection
        // the new tool can't edit, so the panel's changes apply to what's drawn next.
        if tool != Tool::Select
            && self.selected.and_then(|i| self.anns.get(i)).is_some_and(|a| Tool::for_kind(&a.kind) != Some(tool))
        {
            self.selected = None;
        }
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

    /// Bake marks `..=upto` into the background pixels and remove them, so
    /// pixel tools (blur, fill, magnifier, eyedropper) also work on what they
    /// covered, such as a pasted picture. The canvas looks the same afterwards;
    /// the parts of a mark outside the canvas are cut off.
    fn merge_down(&mut self, upto: usize) {
        if upto >= self.anns.len() {
            return;
        }
        self.push_undo();
        let merged: Vec<Annotation> = self.anns.drain(..=upto).collect();
        self.base = Arc::new(render::render(&self.base, &merged, &Effects::default()));
        self.base_changed = true;
        self.selected = None;
        self.texture_dirty = true;
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

    /// Enlarge the canvas so the image layer `i`, just dragged past its edge, stays fully visible.
    /// Only the dragged layer counts, so layers the user deliberately cropped stay cropped.
    fn grow_canvas_to_layer(&mut self, i: usize) {
        let Some(Kind::Image { pos, w, h, .. }) = self.anns.get(i).map(|a| &a.kind) else { return };
        let (cw, ch) = (self.base.width() as f32, self.base.height() as f32);
        // Round rather than floor/ceil: a sub-pixel overhang is not worth a new base image.
        let x0 = pos.x.round().min(0.0);
        let y0 = pos.y.round().min(0.0);
        let x1 = (pos.x + w).round().max(cw);
        let y1 = (pos.y + h).round().max(ch);
        let (nw, nh) = ((x1 - x0) as u32, (y1 - y0) as u32);
        if nw == self.base.width() && nh == self.base.height() {
            return;
        }
        if nw > 16_000 || nh > 40_000 {
            log::warn!("not growing canvas to {nw}x{nh}: over the size limit");
            return;
        }
        let mut grown = RgbaImage::from_pixel(nw, nh, self.background_fill());
        image::imageops::replace(&mut grown, &*self.base, (-x0) as i64, (-y0) as i64);
        self.base = Arc::new(grown);
        self.base_changed = true;
        for a in &mut self.anns {
            a.translate(-x0, -y0);
        }
        self.texture_dirty = true;
        self.dirty = true;
    }

    /// The canvas colour when all four corners agree (a blank canvas), else transparent,
    /// so a screenshot's title bar colour is not smeared into the new area.
    fn background_fill(&self) -> image::Rgba<u8> {
        let (w, h) = (self.base.width(), self.base.height());
        let c = *self.base.get_pixel(0, 0);
        let corners = [(w - 1, 0), (0, h - 1), (w - 1, h - 1)];
        if corners.iter().all(|&(x, y)| *self.base.get_pixel(x, y) == c) {
            c
        } else {
            image::Rgba([0, 0, 0, 0])
        }
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

    /// Right-click menu on the canvas.
    fn canvas_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(200.0);
        let target = self.menu_target.filter(|&i| i < self.anns.len());
        if let Some(i) = target
            && matches!(self.anns[i].kind, Kind::Image { .. })
            && ui
                .button("Flatten image")
                .on_hover_text("Merge this picture, and the marks under it, into the screenshot so blur, fill and the other pixel tools work on it")
                .clicked()
        {
            self.merge_down(i);
            ui.close();
        }
        if ui
            .add_enabled(!self.anns.is_empty(), egui::Button::new("Flatten all"))
            .on_hover_text("Merge every mark into the screenshot")
            .clicked()
        {
            self.merge_down(self.anns.len() - 1);
            ui.close();
        }
        if target.is_some() {
            ui.separator();
            if ui.button(egui::RichText::new("Delete").color(crate::theme::DANGER)).clicked() {
                self.delete_selected();
                ui.close();
            }
        }
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
            Tool::Blur => Kind::Blur { a: p, b: p, style: self.blur_style, strength: self.blur_strength },
            Tool::Spotlight => Kind::Spotlight { a: p, b: p },
            Tool::Magnify => Kind::Magnify { a: p, b: p, zoom: self.loupe_zoom },
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
        // Resize handles work with any tool, e.g. right after placing a stamp.
        if let Some((i, corners)) = self.resize_handles()
            && let Some(k) = (0..4).find(|&k| corners[k].dist(p) <= HANDLE_GRAB / self.zoom)
        {
            self.push_undo();
            self.resizing = Some((i, k));
            return;
        }
        match self.tool {
            Tool::Select => {
                self.select(self.hit_test(p));
                if self.selected.is_some() {
                    self.moving = Some(p);
                    self.moved = false;
                }
            }
            Tool::Crop => self.crop_drag = Some((p, p)),
            Tool::Eraser => self.erase_at(p),
            Tool::Text | Tool::Step | Tool::Stamp | Tool::Fill => {}
            _ => {
                self.selected = None;
                self.drawing = self.new_annotation(p);
            }
        }
    }

    fn drag_update(&mut self, p: P, constrain: bool) {
        if self.tool == Tool::Eraser {
            self.erase_at(p);
            return;
        }
        if let Some((i, k)) = self.resizing {
            let large = self.base.width() as u64 * self.base.height() as u64 > LIVE_RASTER_PIXELS;
            let Some(ann) = self.anns.get_mut(i) else { return };
            // The opposite corner stays put; `left`/`top` say which way the dragged corner lies.
            let (lo, hi) = ann.bounds();
            let (left, top) = (k == 0 || k == 3, k < 2);
            let anchor = P::new(if left { hi.x } else { lo.x }, if top { hi.y } else { lo.y });
            let toward = |len: f32, left_or_top: bool| if left_or_top { -len } else { len };
            match &mut ann.kind {
                Kind::Image { pos, w, h, img } => {
                    // Keep the aspect ratio.
                    let aspect = img.height() as f32 / img.width().max(1) as f32;
                    *w = (p.x - anchor.x).abs().max((p.y - anchor.y).abs() / aspect).max(8.0);
                    *h = *w * aspect;
                    *pos = P::new(anchor.x + toward(*w, left).min(0.0), anchor.y + toward(*h, top).min(0.0));
                }
                // Grows or shrinks around its centre.
                Kind::Stamp { pos, size, .. } => *size = (2.0 * (p.x - pos.x).abs().max((p.y - pos.y).abs())).max(12.0),
                Kind::Magnify { a, b, .. } => {
                    let side = (p.x - anchor.x).abs().max((p.y - anchor.y).abs()).max(8.0);
                    (*a, *b) = norm(anchor, P::new(anchor.x + toward(side, left), anchor.y + toward(side, top)));
                    // Large canvases re-raster on release (see `drag_end`).
                    if !large {
                        self.texture_dirty = true;
                    }
                }
                _ => return,
            }
            self.dirty = true;
            return;
        }
        if let (Some(last), Some(i)) = (self.moving, self.selected) {
            if !self.moved {
                self.push_undo();
                self.moved = true;
            }
            let large = self.base.width() as u64 * self.base.height() as u64 > LIVE_RASTER_PIXELS;
            let ann = &mut self.anns[i];
            ann.translate(p.x - last.x, p.y - last.y);
            // Large canvases re-raster once on release (see `drag_end`), not every frame.
            if ann.is_raster() && !large {
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
            Kind::Rect { a, b } | Kind::Ellipse { a, b } | Kind::Blur { a, b, .. } | Kind::Spotlight { a, b } => {
                *b = if constrain { square(*a, p) } else { p }
            }
            // A loupe is always round.
            Kind::Magnify { a, b, .. } => *b = square(*a, p),
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
        let dragged = self.resizing.map(|(i, _)| i).or(self.selected.filter(|_| self.moved && self.moving.is_some()));
        if let Some(i) = dragged
            && self.anns.get(i).is_some_and(|a| a.is_raster())
        {
            self.texture_dirty = true;
        }
        self.moving = None;
        self.resizing = None;
        if let Some(i) = dragged {
            self.grow_canvas_to_layer(i);
        }
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
        let hit = self.hit_test(p);
        // A click on an existing mark selects it and switches to Select & move.
        // Click-to-place tools only do this for their own kind of mark, so new
        // steps, stamps and fills can still go on top of other marks.
        let same_kind = hit.is_some_and(|i| Tool::for_kind(&self.anns[i].kind) == Some(self.tool));
        if hit.is_some() && (self.tool.draws_by_drag() || (matches!(self.tool, Tool::Step | Tool::Stamp | Tool::Fill) && same_kind)) {
            self.select(hit);
            self.set_tool(Tool::Select);
            return;
        }
        match self.tool {
            Tool::Select => self.select(hit),
            Tool::Eraser => self.erase_at(p),
            Tool::Stamp => {
                self.push_undo();
                self.anns.push(Annotation {
                    kind: Kind::Stamp { pos: p, size: self.stamp_size, stamp: self.stamp },
                    color: from_c32(self.color),
                    width: self.width,
                    filled: true,
                });
                self.selected = Some(self.anns.len() - 1);
            }
            Tool::Fill => {
                let Some(area) = self.fill_area(self.anns.len(), p, self.fill_tolerance) else { return };
                self.push_undo();
                self.anns.push(Annotation {
                    kind: Kind::Fill { seed: p, tolerance: self.fill_tolerance, area },
                    color: from_c32(self.color),
                    width: self.width,
                    filled: true,
                });
                self.texture_dirty = true;
                self.selected = Some(self.anns.len() - 1);
            }
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
        self.select(Some(i));
        self.focus_text = true;
    }

    /// Eraser: remove every mark under `p` (one undo step per gesture).
    fn erase_at(&mut self, p: P) {
        self.commit_text();
        let r = self.width.max(2.0) * 1.5 + 4.0 / self.zoom;
        while let Some(i) = self.anns.iter().rposition(|a| a.hit(p, r)) {
            self.gesture_undo();
            if self.anns.remove(i).is_raster() {
                self.texture_dirty = true;
            }
            self.selected = None;
        }
    }

    /// Bounds of the region a paint-bucket click at `seed` floods, as the image
    /// looks under the first `upto` marks.
    fn fill_area(&self, upto: usize, seed: P, tolerance: f32) -> Option<(P, P)> {
        let img = render::apply_raster(&self.base, &self.anns[..upto.min(self.anns.len())]);
        let (_, (x0, y0, x1, y1)) = render::flood_region(&img, seed, tolerance)?;
        Some((P::new(x0 as f32, y0 as f32), P::new(x1 as f32, y1 as f32)))
    }

    /// Select an existing element and show its style in the side panel, so the
    /// color swatch, sliders and switches reflect what is selected.
    fn select(&mut self, i: Option<usize>) {
        self.selected = i;
        let Some(ann) = i.and_then(|i| self.anns.get(i)) else { return };
        match &ann.kind {
            // A pasted image's color/width are unused placeholders.
            Kind::Image { .. } => return,
            Kind::Text { size, background, .. } => {
                self.font_size = *size;
                self.text_bg = *background;
            }
            Kind::Rect { .. } | Kind::Ellipse { .. } => {
                self.width = ann.width;
                self.filled = ann.filled;
            }
            Kind::Step { radius, .. } => self.font_size = (radius / 0.75).round(),
            Kind::Stamp { size, stamp, .. } => {
                self.stamp_size = *size;
                self.stamp = *stamp;
            }
            Kind::Magnify { zoom, .. } => {
                self.loupe_zoom = *zoom;
                self.width = ann.width;
            }
            Kind::Fill { tolerance, .. } => self.fill_tolerance = *tolerance,
            Kind::Blur { style, strength, .. } => {
                self.blur_style = *style;
                self.blur_strength = *strength;
                return;
            }
            // No color or width of its own.
            Kind::Spotlight { .. } => return,
            _ => self.width = ann.width,
        }
        self.color = to_c32(ann.color);
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) {
        if !self.texture_dirty && !self.tiles.is_empty() {
            return;
        }
        let img = render::apply_raster(&self.base, &self.anns);
        let (w, h) = img.dimensions();
        let side = (ctx.input(|i| i.max_texture_side) as u32).max(256);
        let mut old = std::mem::take(&mut self.tiles).into_iter();
        for y in (0..h).step_by(side as usize) {
            for x in (0..w).step_by(side as usize) {
                let (tw, th) = (side.min(w - x), side.min(h - y));
                let color = if (tw, th) == (w, h) {
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw())
                } else {
                    let tile = image::imageops::crop_imm(&img, x, y, tw, th).to_image();
                    egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], tile.as_raw())
                };
                let handle = match old.next() {
                    Some((mut t, _)) => {
                        t.set(color, egui::TextureOptions::LINEAR);
                        t
                    }
                    None => ctx.load_texture("editor-image", color, egui::TextureOptions::LINEAR),
                };
                let rect = Rect::from_min_size(Pos2::new(x as f32, y as f32), Vec2::new(tw as f32, th as f32));
                self.tiles.push((handle, rect));
            }
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
            if self.anns[i].is_raster() {
                self.texture_dirty = true;
            }
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

    /// Corner resize handles of a selected image layer, stamp or magnifier, in
    /// image coordinates, clockwise from top-left. They sit on the selection
    /// outline, `SELECTION_PAD` screen points outside the mark.
    fn resize_handles(&self) -> Option<(usize, [P; 4])> {
        let i = self.selected.filter(|&i| i < self.anns.len())?;
        if !matches!(self.anns[i].kind, Kind::Image { .. } | Kind::Stamp { .. } | Kind::Magnify { .. }) {
            return None;
        }
        let (a, b) = self.anns[i].bounds();
        let pad = SELECTION_PAD / self.zoom;
        let (a, b) = (P::new(a.x - pad, a.y - pad), P::new(b.x + pad, b.y + pad));
        Some((i, [a, P::new(b.x, a.y), b, P::new(a.x, b.y)]))
    }

    fn ensure_layer_textures(&mut self, ctx: &egui::Context) {
        let mut live = Vec::new();
        for ann in &self.anns {
            if let Kind::Image { img, .. } = &ann.kind {
                let key = Arc::as_ptr(img) as usize;
                live.push(key);
                let stale = self.layer_textures.get(&key).is_none_or(|(weak, _)| !weak.upgrade().is_some_and(|w| Arc::ptr_eq(&w, img)));
                if stale {
                    let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                    let tex = ctx.load_texture("image-layer", color, egui::TextureOptions::LINEAR);
                    self.layer_textures.insert(key, (Arc::downgrade(img), tex));
                }
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
            if theme::toolbar_item(ui, icon::PASTE, "Paste", ItemStyle::Normal, true)
                .on_hover_text(format!("Paste an image from the clipboard onto the canvas ({}V)", crate::platform::CMD))
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
                .on_hover_text(format!("Save to library ({}S)", crate::platform::CMD))
                .clicked()
            {
                action = self.save_action();
            }
            if theme::toolbar_item(ui, icon::COPY, "Copy", ItemStyle::Primary, true)
                .on_hover_text(format!("Copy image ({}C)", crate::platform::CMD))
                .clicked()
            {
                action = self.copy_action();
            }
        });
        action
    }

    /// Right-hand panel: tools, then the properties of the selected mark, or of
    /// the current tool when nothing is selected. Only what applies is shown.
    pub fn tools_panel(&mut self, ui: &mut egui::Ui) {
        use crate::theme;
        theme::section(ui, "Tools");
        egui::Grid::new("tool_grid").num_columns(4).spacing([8.0, 8.0]).show(ui, |ui| {
            for (n, tool) in Tool::ALL.into_iter().enumerate() {
                let (glyph, tip, key) = tool.info();
                let tip = format!("{tip}   {}", key.name());
                let active = self.tool == tool;
                let resp = if glyph.is_empty() {
                    theme::tool_tile_painted(ui, active, &tip, |p, c, color| paint_tool_icon(tool, p, c, color))
                } else {
                    theme::tool_tile(ui, glyph, active, &tip)
                };
                if resp.clicked() {
                    self.set_tool(tool);
                }
                if n % 4 == 3 {
                    ui.end_row();
                }
            }
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new(Tool::info(self.tool).1).font(theme::medium(13.0)).color(theme::TEXT_MUTED));

        let selected_idx = self.selected.filter(|&i| i < self.anns.len());
        let subject = match selected_idx {
            Some(i) => Tool::for_kind(&self.anns[i].kind),
            None => Some(self.tool),
        };
        let mut props = subject.map(Props::of).unwrap_or_default();
        if props.blur {
            props.hint = Some(match self.blur_style {
                BlurStyle::Pixelate => "Drag over anything private to pixelate it.",
                BlurStyle::Gaussian => "Drag over anything private to blur it.",
            });
        }
        if props.color {
            ui.add_space(14.0);
            self.color_section(ui, selected_idx.is_some());
        }

        ui.add_space(14.0);
        let title = match (selected_idx, subject) {
            (Some(_), Some(t)) => format!("{} · selection", t.info().1),
            (Some(_), None) => "Image · selection".to_string(),
            (None, Some(t)) => t.info().1.to_string(),
            (None, None) => String::new(),
        };
        theme::section(ui, &title);
        if let Some(hint) = props.hint {
            ui.label(egui::RichText::new(hint).small().color(theme::TEXT_FAINT));
            ui.add_space(4.0);
        }
        // A filled shape has no outline to size.
        if let Some(label) = props.width.filter(|_| !(props.fill && self.filled)) {
            let mut width = self.width;
            let label = ui.label(egui::RichText::new(label).color(theme::TEXT_MUTED));
            ui.add(egui::Slider::new(&mut width, 1.0..=40.0).max_decimals(0)).labelled_by(label.id);
            if width != self.width {
                self.width = width;
                if let Some(i) = selected_idx {
                    self.gesture_undo();
                    self.anns[i].width = width;
                }
            }
        }
        if props.fill {
            let mut filled = self.filled;
            if theme::switch(ui, &mut filled, "Fill shape").changed() {
                self.filled = filled;
                if let Some(i) = selected_idx {
                    self.push_undo();
                    self.anns[i].filled = filled;
                }
            }
        }
        if props.text {
            let mut size = self.font_size;
            let label = ui.label(egui::RichText::new("Text size").color(theme::TEXT_MUTED));
            ui.add(egui::Slider::new(&mut size, 8.0..=300.0).logarithmic(true).max_decimals(0)).labelled_by(label.id);
            if size != self.font_size {
                self.font_size = size;
                if let Some(i) = selected_idx {
                    self.gesture_undo();
                    if let Kind::Text { size: s, .. } = &mut self.anns[i].kind {
                        *s = size;
                    }
                }
            }
            ui.add_space(6.0);
            let mut text_bg = self.text_bg;
            if theme::switch(ui, &mut text_bg, "Callout box").on_hover_text("Text on a colored box").changed() {
                self.text_bg = text_bg;
                if let Some(i) = selected_idx {
                    self.push_undo();
                    if let Kind::Text { background, .. } = &mut self.anns[i].kind {
                        *background = text_bg;
                    }
                }
            }
        }
        if props.step {
            let mut size = self.font_size;
            let label = ui.label(egui::RichText::new("Size").color(theme::TEXT_MUTED));
            ui.add(egui::Slider::new(&mut size, 8.0..=300.0).logarithmic(true).max_decimals(0)).labelled_by(label.id);
            if size != self.font_size {
                self.font_size = size;
                if let Some(i) = selected_idx {
                    self.gesture_undo();
                    if let Kind::Step { radius, .. } = &mut self.anns[i].kind {
                        *radius = size * 0.75;
                    }
                }
            }
        }
        if props.stamp {
            let label = ui.label(egui::RichText::new("Symbol").color(theme::TEXT_MUTED));
            let mut picked = None;
            ui.horizontal_wrapped(|ui| {
                // Six across fit the 220 pt panel.
                ui.spacing_mut().item_spacing = Vec2::splat(4.0);
                for stamp in Stamp::ALL {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
                    if stamp == self.stamp {
                        ui.painter().rect_stroke(rect, 9.0, Stroke::new(2.0, theme::ACCENT), StrokeKind::Inside);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 9.0, theme::SURFACE_3);
                    }
                    paint_stamp(ui.painter(), rect.center(), 22.0, stamp, self.color);
                    if resp.on_hover_text(stamp.label()).clicked() {
                        picked = Some(stamp);
                    }
                }
            })
            .response
            .labelled_by(label.id);
            if let Some(stamp) = picked {
                self.stamp = stamp;
                if let Some(i) = selected_idx {
                    self.push_undo();
                    if let Kind::Stamp { stamp: s, .. } = &mut self.anns[i].kind {
                        *s = stamp;
                    }
                }
            }
            let mut size = self.stamp_size;
            let label = ui.label(egui::RichText::new("Size").color(theme::TEXT_MUTED));
            ui.add(egui::Slider::new(&mut size, 16.0..=600.0).logarithmic(true).max_decimals(0)).labelled_by(label.id);
            if size != self.stamp_size {
                self.stamp_size = size;
                if let Some(i) = selected_idx {
                    self.gesture_undo();
                    if let Kind::Stamp { size: s, .. } = &mut self.anns[i].kind {
                        *s = size;
                    }
                }
            }
        }
        if props.zoom {
            let mut zoom = self.loupe_zoom;
            let label = ui.label(egui::RichText::new("Zoom").color(theme::TEXT_MUTED));
            ui.add(egui::Slider::new(&mut zoom, 1.5..=4.0).step_by(0.5).max_decimals(1).suffix("×")).labelled_by(label.id);
            if zoom != self.loupe_zoom {
                self.loupe_zoom = zoom;
                if let Some(i) = selected_idx {
                    self.gesture_undo();
                    if let Kind::Magnify { zoom: z, .. } = &mut self.anns[i].kind {
                        *z = zoom;
                    }
                    self.texture_dirty = true;
                }
            }
        }
        if props.tolerance {
            let mut pct = (self.fill_tolerance / 2.55).round();
            let label = ui.label(egui::RichText::new("Tolerance").color(theme::TEXT_MUTED));
            ui.add(egui::Slider::new(&mut pct, 0.0..=100.0).max_decimals(0).suffix("%"))
                .labelled_by(label.id)
                .on_hover_text("How different a color can be and still get filled");
            let tolerance = pct * 2.55;
            if (tolerance - self.fill_tolerance).abs() > 0.5 {
                self.fill_tolerance = tolerance;
                if let Some(i) = selected_idx
                    && let Kind::Fill { seed, .. } = self.anns[i].kind
                {
                    self.gesture_undo();
                    let area = self.fill_area(i, seed, tolerance);
                    if let Kind::Fill { tolerance: t, area: a, .. } = &mut self.anns[i].kind {
                        *t = tolerance;
                        if let Some(area) = area {
                            *a = area;
                        }
                    }
                    self.texture_dirty = true;
                }
            }
        }
        if props.blur {
            let mut style = self.blur_style;
            let options: Vec<(BlurStyle, &str)> = BlurStyle::ALL.iter().map(|s| (*s, s.label())).collect();
            if theme::segmented(ui, &mut style, &options) {
                self.blur_style = style;
                if let Some(i) = selected_idx {
                    self.push_undo();
                    if let Kind::Blur { style: s, .. } = &mut self.anns[i].kind {
                        *s = style;
                    }
                    self.texture_dirty = true;
                }
            }
            ui.add_space(6.0);
            let mut strength = self.blur_strength;
            let label = ui.label(egui::RichText::new("Strength").color(theme::TEXT_MUTED));
            ui.add(egui::Slider::new(&mut strength, 1.0..=100.0).max_decimals(0)).labelled_by(label.id);
            if strength != self.blur_strength {
                self.blur_strength = strength;
                if let Some(i) = selected_idx {
                    self.gesture_undo();
                    if let Kind::Blur { strength: s, .. } = &mut self.anns[i].kind {
                        *s = strength;
                    }
                    self.texture_dirty = true;
                }
            }
        }
        if selected_idx.is_some() {
            ui.add_space(14.0);
            if theme::danger_button(ui, Some(theme::icon::DELETE), "Delete").on_hover_text("⌫").clicked() {
                self.delete_selected();
            }
        }
    }

    /// Palette, then a color chip (click: picker popup), hex field and
    /// eyedropper. Applies to the selection, if any.
    fn color_section(&mut self, ui: &mut egui::Ui, has_selection: bool) {
        use crate::theme;
        theme::section(ui, if has_selection { "Color · selection" } else { "Color" });
        let width = ui.available_width();

        // Palette: one row of small swatches.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for c in PALETTE.map(to_c32) {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width / PALETTE.len() as f32, 24.0), Sense::click());
                let center = rect.center();
                if c == self.color {
                    ui.painter().circle_stroke(center, 11.0, Stroke::new(2.0, theme::ACCENT));
                }
                ui.painter().circle_filled(center, 8.0, c);
                if c == Color32::from_rgb(24, 24, 24) || c == Color32::WHITE {
                    ui.painter().circle_stroke(center, 8.0, Stroke::new(1.0, theme::BORDER_STRONG));
                }
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    self.pick_color(c, true);
                }
            }
        });
        ui.add_space(4.0);

        // Current color (click for the picker), hex field, eyedropper.
        ui.horizontal(|ui| {
            let (chip, chip_resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
            let hovered = chip_resp.hovered();
            ui.painter().rect_filled(chip, 7.0, self.color);
            let ring = if hovered { theme::ACCENT } else { theme::BORDER_STRONG };
            ui.painter().rect_stroke(chip, 7.0, Stroke::new(if hovered { 2.0 } else { 1.0 }, ring), StrokeKind::Inside);
            chip_resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Custom color"));
            let chip_resp = chip_resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Custom color");
            // Stays open while dragging inside; closes on a click elsewhere.
            egui::Popup::from_toggle_button_response(&chip_resp)
                // To the left, over the canvas, so the panel's own settings stay visible.
                .align(egui::RectAlign::LEFT_START)
                .gap(10.0)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_width(220.0);
                    self.hsv_picker(ui, 220.0);
                });
            let c = self.color;
            let shown = format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b());
            let mut text = self.hex_edit.clone().unwrap_or(shown);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(width - 30.0 - 30.0 - 2.0 * ui.spacing().item_spacing.x - 8.0)
                    .char_limit(7),
            );
            if resp.has_focus() {
                self.hex_edit = Some(text.clone());
                if let Some(parsed) = parse_hex(&text) {
                    self.pick_color(parsed, false);
                }
            } else {
                self.hex_edit = None;
            }
            let picking = self.picking.is_some();
            let tip = if picking { "Cancel (Esc)" } else { "Pick a color from the image (I)" };
            let eyedropper = theme::icon_button_sized(ui, theme::icon::EYEDROPPER, tip, 30.0, true);
            if picking {
                ui.painter().rect_stroke(eyedropper.rect, 8.0, Stroke::new(1.5, theme::ACCENT), StrokeKind::Inside);
            }
            if eyedropper.clicked() {
                self.toggle_picking();
            }
        });
        if self.picking.is_some() {
            ui.label(egui::RichText::new("Click anywhere on the image to pick its color").small().color(theme::ACCENT));
        }
    }

    /// Set the color from the panel, keeping the picker's hue in step.
    fn pick_color(&mut self, color: Color32, discrete: bool) {
        let hsv = egui::ecolor::HsvaGamma::from(color);
        if hsv.s > 0.01 && hsv.v > 0.01 {
            self.picker_hue = hsv.h;
        }
        if color != self.color {
            self.set_color(color, discrete);
        }
    }

    /// Saturation/brightness square over a hue strip, `width` wide.
    fn hsv_picker(&mut self, ui: &mut egui::Ui, width: f32) {
        use egui::ecolor::HsvaGamma;
        let current = HsvaGamma::from(self.color);
        if current.s > 0.01 && current.v > 0.01 {
            self.picker_hue = current.h;
        }
        let hue = self.picker_hue;
        let at = |h: f32, s: f32, v: f32| Color32::from(HsvaGamma { h, s, v, a: 1.0 });

        // Square: x = saturation, y = brightness. A vertex grid fakes the 2D gradient.
        let (square, sq) = ui.allocate_exact_size(Vec2::new(width, 130.0), Sense::click_and_drag());
        let n = 16;
        let mut mesh = egui::Mesh::default();
        for j in 0..=n {
            for i in 0..=n {
                let (fx, fy) = (i as f32 / n as f32, j as f32 / n as f32);
                mesh.colored_vertex(square.lerp_inside(Vec2::new(fx, fy)), at(hue, fx, 1.0 - fy));
            }
        }
        for j in 0..n {
            for i in 0..n {
                let k = j * (n + 1) + i;
                mesh.add_triangle(k, k + 1, k + n + 1);
                mesh.add_triangle(k + 1, k + n + 2, k + n + 1);
            }
        }
        let p = ui.painter();
        p.add(Shape::mesh(mesh));
        p.rect_stroke(square, 0.0, Stroke::new(1.0, crate::theme::BORDER_STRONG), StrokeKind::Inside);
        let knob = square.lerp_inside(Vec2::new(current.s, 1.0 - current.v));
        p.circle_stroke(knob, 6.0, Stroke::new(2.5, Color32::from_black_alpha(120)));
        p.circle_stroke(knob, 6.0, Stroke::new(1.5, Color32::WHITE));
        if let Some(pos) = sq.interact_pointer_pos().filter(|_| sq.is_pointer_button_down_on()) {
            let s = ((pos.x - square.min.x) / square.width()).clamp(0.0, 1.0);
            let v = 1.0 - ((pos.y - square.min.y) / square.height()).clamp(0.0, 1.0);
            self.set_color(at(hue, s, v), false);
        }

        ui.add_space(6.0);
        let (strip, st) = ui.allocate_exact_size(Vec2::new(width, 12.0), Sense::click_and_drag());
        let segs = 36;
        let mut mesh = egui::Mesh::default();
        for i in 0..=segs {
            let f = i as f32 / segs as f32;
            let c = at(f, 1.0, 1.0);
            mesh.colored_vertex(Pos2::new(strip.min.x + f * strip.width(), strip.min.y), c);
            mesh.colored_vertex(Pos2::new(strip.min.x + f * strip.width(), strip.max.y), c);
        }
        for i in 0..segs as u32 {
            mesh.add_triangle(2 * i, 2 * i + 1, 2 * i + 2);
            mesh.add_triangle(2 * i + 1, 2 * i + 3, 2 * i + 2);
        }
        let p = ui.painter();
        p.add(Shape::mesh(mesh));
        let x = strip.min.x + hue * strip.width();
        let marker = Rect::from_center_size(Pos2::new(x, strip.center().y), Vec2::new(5.0, strip.height() + 6.0));
        p.rect_filled(marker, 2.0, Color32::WHITE);
        p.rect_stroke(marker, 2.0, Stroke::new(1.0, Color32::from_black_alpha(140)), StrokeKind::Outside);
        if let Some(pos) = st.interact_pointer_pos().filter(|_| st.is_pointer_button_down_on()) {
            let h = ((pos.x - strip.min.x) / strip.width()).clamp(0.0, 0.999);
            self.picker_hue = h;
            // A grey keeps its brightness but takes on the hue.
            let (s, v) = if current.s < 0.05 { (1.0, current.v.max(0.6)) } else { (current.s, current.v) };
            self.set_color(at(h, s, v), false);
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
            Tool::Stamp => "Click to place a stamp",
            Tool::Magnify => "Drag a circle over a detail to enlarge it",
            Tool::Fill => "Click an area to fill it with the color",
            Tool::Eraser => "Click or drag over marks to erase them",
            _ if cfg!(target_os = "macos") => "Drag to draw · hold ⇧ to constrain",
            _ => "Drag to draw · hold Shift to constrain",
        };
        ui.label(egui::RichText::new(hint).color(theme::TEXT_FAINT));
    }

    pub fn canvas(&mut self, ui: &mut egui::Ui) {
        self.ensure_texture(ui.ctx());
        self.ensure_layer_textures(ui.ctx());
        if self.tiles.is_empty() {
            return;
        }
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
                if resp.secondary_clicked() {
                    self.commit_text();
                    self.menu_target = resp.interact_pointer_pos().and_then(|p| self.hit_test(to_img(p)));
                    self.select(self.menu_target);
                }
                resp.context_menu(|ui| self.canvas_menu(ui));
            }
            if self.picking.is_none()
                && let Some(hover) = resp.hover_pos()
            {
                let over_mark = self.hit_test(to_img(hover)).is_some();
                let icon = match self.tool {
                    Tool::Select if over_mark => CursorIcon::Move,
                    Tool::Select => CursorIcon::Default,
                    Tool::Text => CursorIcon::Text,
                    // A click selects the mark (see `click`).
                    t if t.draws_by_drag() && over_mark => CursorIcon::PointingHand,
                    Tool::Eraser => CursorIcon::None,
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
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            for (tex, r) in &self.tiles {
                let screen = Rect::from_min_size(origin + r.min.to_vec2() * zoom, r.size() * zoom);
                painter.image(tex.id(), screen, uv, Color32::WHITE);
            }
            let layers = &self.layer_textures;
            let layer_tex = |img: &Arc<RgbaImage>| layers.get(&(Arc::as_ptr(img) as usize)).map(|(_, t)| t.id());
            for (i, ann) in self.anns.iter().enumerate() {
                let editing = self.editing_text == Some(i);
                paint_annotation(&painter, ann, &to_screen, zoom, editing, &layer_tex);
            }
            if self.tool == Tool::Eraser
                && let Some(h) = resp.hover_pos()
            {
                let r = (self.width.max(2.0) * 1.5) * zoom + 4.0;
                painter.circle_stroke(h, r, Stroke::new(3.0, Color32::from_black_alpha(140)));
                painter.circle_stroke(h, r, Stroke::new(1.5, Color32::WHITE));
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
                dashed_rect(&painter, Rect::from_two_pos(to_screen(a), to_screen(b)).expand(SELECTION_PAD));
            }
            if let Some((_, corners)) = self.resize_handles() {
                let hover = resp.hover_pos().filter(|_| self.resizing.is_none());
                for (k, corner) in corners.into_iter().enumerate() {
                    let c = to_screen(corner);
                    let hot = self.resizing.is_some_and(|(_, r)| r == k) || hover.is_some_and(|h| h.distance(c) <= HANDLE_GRAB);
                    paint_resize_handle(&painter, c, hot);
                    if hot {
                        ui.ctx().set_cursor_icon(if k % 2 == 0 { CursorIcon::ResizeNwSe } else { CursorIcon::ResizeNeSw });
                    }
                }
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
        let input_id = egui::Id::unique("rega-text-input");
        egui::Area::new(egui::Id::unique("rega-text-edit"))
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
    // Only the visible part: a zoomed tall capture would otherwise be millions of dots.
    let rect = rect.intersect(painter.clip_rect());
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
        Kind::Magnify { a, b, .. } => {
            // The ring; the enlarged pixels are in the canvas texture.
            painter.add(Shape::closed_line(ellipse_points(*a, *b, 96).into_iter().map(to_screen).collect(), stroke));
        }
        Kind::Stamp { pos, size, stamp } => paint_stamp(painter, to_screen(*pos), size * zoom, *stamp, color),
        Kind::Blur { .. } | Kind::Spotlight { .. } | Kind::Fill { .. } => {}
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
    // Start at the first visible row, keeping the pattern anchored to the image.
    let vis = rect.intersect(painter.clip_rect());
    let first = ((vis.min.y - rect.min.y) / cell).floor().max(0.0);
    let mut y = rect.min.y + first * cell;
    let mut row = first as i64;
    let rect = Rect::from_min_max(rect.min, Pos2::new(rect.max.x, vis.max.y));
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
    fn parses_hex_colors() {
        assert_eq!(parse_hex("#EB3B34"), Some(Color32::from_rgb(0xEB, 0x3B, 0x34)));
        assert_eq!(parse_hex("eb3b34"), Some(Color32::from_rgb(0xEB, 0x3B, 0x34)));
        assert_eq!(parse_hex("#f80"), Some(Color32::from_rgb(0xFF, 0x88, 0x00)));
        assert_eq!(parse_hex("#EB3B3"), None);
        assert_eq!(parse_hex("#GG0000"), None);
    }

    fn blank_editor() -> Editor {
        Editor::new(PathBuf::from("unused.png"), RgbaImage::from_pixel(400, 300, image::Rgba([255, 255, 255, 255])), None, true)
    }

    #[test]
    fn clicking_a_mark_with_a_drawing_tool_selects_it_for_moving() {
        let mut ed = blank_editor();
        ed.tool = Tool::Arrow;
        ed.drag_begin(P::new(50.0, 50.0));
        ed.drag_update(P::new(200.0, 50.0), false);
        ed.drag_end();
        ed.selected = None;
        ed.tool = Tool::Rect;
        ed.click(P::new(120.0, 50.0));
        assert_eq!(ed.selected, Some(0));
        assert_eq!(ed.tool, Tool::Select);
        // A click on empty canvas with a drawing tool selects nothing.
        ed.tool = Tool::Rect;
        ed.click(P::new(300.0, 250.0));
        assert_eq!(ed.tool, Tool::Rect);
    }

    #[test]
    fn steps_and_stamps_still_land_on_other_marks() {
        let mut ed = blank_editor();
        ed.tool = Tool::Blur;
        ed.drag_begin(P::new(20.0, 20.0));
        ed.drag_update(P::new(200.0, 200.0), false);
        ed.drag_end();
        ed.tool = Tool::Stamp;
        ed.click(P::new(100.0, 100.0));
        assert_eq!(ed.anns.len(), 2, "stamp placed on top of the blur");
        assert!(matches!(ed.anns[1].kind, Kind::Stamp { .. }));
        // Clicking the stamp again with the stamp tool selects it instead.
        ed.click(P::new(100.0, 100.0));
        assert_eq!(ed.anns.len(), 2);
        assert_eq!((ed.selected, ed.tool), (Some(1), Tool::Select));
    }

    #[test]
    fn eraser_removes_marks_in_one_undo_step() {
        let mut ed = blank_editor();
        ed.tool = Tool::Stamp;
        ed.click(P::new(60.0, 60.0));
        ed.click(P::new(300.0, 60.0));
        assert_eq!(ed.anns.len(), 2);
        ed.tool = Tool::Eraser;
        ed.drag_begin(P::new(60.0, 60.0));
        ed.drag_update(P::new(300.0, 60.0), false);
        ed.drag_end();
        assert!(ed.anns.is_empty());
        ed.undo();
        assert_eq!(ed.anns.len(), 2, "one gesture, one undo");
    }

    #[test]
    fn flattening_a_pasted_image_bakes_it_and_the_marks_under_it() {
        let mut ed = blank_editor();
        ed.tool = Tool::Stamp;
        ed.click(P::new(60.0, 60.0));
        let red = Arc::new(RgbaImage::from_pixel(40, 40, image::Rgba([255, 0, 0, 255])));
        ed.anns.push(Annotation { kind: Kind::Image { pos: P::new(100.0, 100.0), w: 40.0, h: 40.0, img: red }, color: [0; 4], width: 0.0, filled: false });
        ed.click(P::new(300.0, 60.0));
        let before = ed.flatten();
        ed.merge_down(1);
        assert_eq!(ed.anns.len(), 1, "only the stamp above the image stays a mark");
        assert_eq!(ed.base.get_pixel(120, 120).0, [255, 0, 0, 255], "the picture is now background pixels");
        assert_eq!(ed.flatten(), before, "flattening doesn't change the look");
        ed.undo();
        assert_eq!(ed.anns.len(), 3);
        assert_ne!(ed.base.get_pixel(120, 120).0, [255, 0, 0, 255]);
    }

    #[test]
    fn picking_another_tool_shows_its_properties() {
        let mut ed = blank_editor();
        ed.tool = Tool::Stamp;
        ed.click(P::new(60.0, 60.0));
        assert_eq!(ed.selected, Some(0), "a new stamp is selected");
        ed.set_tool(Tool::Stamp);
        assert_eq!(ed.selected, Some(0), "same tool keeps it");
        ed.set_tool(Tool::Rect);
        assert_eq!(ed.selected, None, "the panel now edits rectangles, not the stamp");
    }

    #[test]
    fn images_resize_from_any_corner_keeping_the_opposite_one() {
        let mut ed = blank_editor();
        let img = Arc::new(RgbaImage::new(20, 10));
        ed.anns.push(Annotation { kind: Kind::Image { pos: P::new(100.0, 100.0), w: 40.0, h: 20.0, img }, color: [0; 4], width: 0.0, filled: false });
        ed.selected = Some(0);
        let (_, corners) = ed.resize_handles().unwrap();
        ed.drag_begin(corners[0]);
        ed.drag_update(P::new(60.0, 80.0), false);
        ed.drag_end();
        // Dragged the top-left corner out to 80 wide; the bottom-right stays at (140, 120).
        assert!(matches!(ed.anns[0].kind, Kind::Image { pos, w, h, .. } if w == 80.0 && h == 40.0 && pos == P::new(60.0, 80.0)));
    }

    #[test]
    fn stamps_resize_from_their_corner() {
        let mut ed = blank_editor();
        ed.stamp_size = 40.0;
        ed.tool = Tool::Stamp;
        ed.click(P::new(100.0, 100.0));
        // Still on the Stamp tool: the handle of the stamp just placed works.
        let (i, corners) = ed.resize_handles().expect("stamp has resize handles");
        let pad = SELECTION_PAD / ed.zoom;
        assert_eq!(corners[2], P::new(120.0 + pad, 120.0 + pad));
        ed.drag_begin(corners[2]);
        assert_eq!(ed.resizing, Some((i, 2)));
        ed.drag_update(P::new(150.0, 130.0), false);
        ed.drag_end();
        assert!(matches!(ed.anns[0].kind, Kind::Stamp { size, .. } if size == 100.0));
    }

    #[test]
    fn exports_every_format() {
        let dir = std::env::temp_dir().join(format!("rega-export-{}", std::process::id()));
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
