//! Rega's design system: palette, typography, icons, and shared widgets.

use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Id, Margin, Rect, Response,
    Sense, Shadow, Stroke, StrokeKind, TextStyle, TextureHandle, Ui, Vec2, epaint,
};

use crate::editor::ANNOT_FONT;

// ---------- palette ----------

pub const BG: Color32 = Color32::from_rgb(14, 16, 20);
pub const SURFACE: Color32 = Color32::from_rgb(21, 24, 30);
pub const SURFACE_2: Color32 = Color32::from_rgb(28, 32, 40);
pub const SURFACE_3: Color32 = Color32::from_rgb(36, 41, 52);
pub const BORDER: Color32 = Color32::from_rgb(40, 45, 56);
pub const BORDER_STRONG: Color32 = Color32::from_rgb(56, 63, 78);
pub const TEXT: Color32 = Color32::from_rgb(236, 238, 243);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(139, 147, 167);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(93, 100, 118);
pub const ACCENT: Color32 = Color32::from_rgb(124, 92, 255);
pub const CORAL: Color32 = Color32::from_rgb(255, 94, 122);
pub const DANGER: Color32 = Color32::from_rgb(255, 84, 104);
pub const SUCCESS: Color32 = Color32::from_rgb(61, 214, 140);

// ---------- icons (Material Symbols Rounded) ----------

pub mod icon {
    pub const UNDO: &str = "\u{e166}";
    pub const REDO: &str = "\u{e15a}";
    pub const COPY: &str = "\u{e14d}";
    pub const SAVE: &str = "\u{e161}";
    pub const EXPORT: &str = "\u{e6b8}";
    pub const RESIZE: &str = "\u{e433}";
    pub const SELECT: &str = "\u{f82f}";
    pub const ARROW: &str = "\u{e941}";
    pub const LINE: &str = "\u{f108}";
    pub const RECT: &str = "\u{e3c6}";
    pub const ELLIPSE: &str = "\u{e836}";
    pub const PEN: &str = "\u{f097}";
    pub const MARKER: &str = "\u{e6d1}";
    pub const TEXT: &str = "\u{e264}";
    pub const STEP: &str = "\u{e400}";
    pub const BLUR: &str = "\u{e3a5}";
    pub const SPOTLIGHT: &str = "\u{e25f}";
    pub const CROP: &str = "\u{e3be}";
    pub const REGION: &str = "\u{f7d2}";
    pub const FULLSCREEN: &str = "\u{ec08}";
    pub const VIDEO: &str = "\u{e04b}";
    pub const SCROLL: &str = "\u{eb53}";
    pub const OCR: &str = "\u{e5fa}";
    pub const SETTINGS: &str = "\u{e8b8}";
    pub const FOLDER: &str = "\u{e2c8}";
    pub const PASTE: &str = "\u{e14f}";
    pub const SEARCH: &str = "\u{ef7a}";
    pub const IMAGE: &str = "\u{e3f4}";
    pub const DELETE: &str = "\u{e92e}";
    pub const PAUSE: &str = "\u{e034}";
    pub const PLAY: &str = "\u{e037}";
    pub const STOP: &str = "\u{e047}";
    pub const CLOSE: &str = "\u{e5cd}";
    pub const ZOOM_IN: &str = "\u{e8ff}";
    pub const ZOOM_OUT: &str = "\u{e900}";
    pub const FIT: &str = "\u{ea10}";
    pub const TIMER: &str = "\u{e425}";
    pub const CHECK: &str = "\u{e5ca}";
    pub const EYEDROPPER: &str = "\u{e3b8}";
    pub const NEW: &str = "\u{e89c}";
    pub const IMPORT: &str = "\u{e43e}";
    pub const INFO: &str = "\u{e88e}";
    pub const MAGNIFIER: &str = "\u{e8b6}";
    pub const GUIDE: &str = "\u{e762}";
    pub const MOVE_UP: &str = "\u{e5d8}";
    pub const MOVE_DOWN: &str = "\u{e5db}";
    pub const PREV: &str = "\u{e5cb}";
    pub const NEXT: &str = "\u{e5cc}";
    pub const KEYBOARD: &str = "\u{e312}";
    pub const MOUSE: &str = "\u{e323}";
    pub const OPEN_EXTERNAL: &str = "\u{e89e}";
}

pub const ICON_FONT: &str = "icons";
pub const MEDIUM: &str = "medium";
pub const BOLD: &str = "bold";

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICON_FONT.into()))
}
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

// ---------- setup ----------

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add("ubuntu", include_bytes!("../assets/fonts/Ubuntu-Regular.ttf"));
    add(MEDIUM, include_bytes!("../assets/fonts/Ubuntu-Medium.ttf"));
    add(BOLD, crate::editor::render::annotation_font_bytes());
    add(ICON_FONT, include_bytes!("../assets/fonts/RegaIcons.ttf"));
    add("hebrew", crate::editor::render::hebrew_font_bytes());
    // Keyboard symbols (⌃ ⌥ ⇧ ⌘ ⌫ …) from Noto Sans Symbols, subset to a few glyphs.
    add("keys-a", include_bytes!("../assets/fonts/KeySymbolsA.ttf"));
    add("keys-b", include_bytes!("../assets/fonts/KeySymbolsB.ttf"));

    let fallbacks = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let family = |primary: &str| {
        let mut v = vec![primary.to_owned(), ICON_FONT.to_owned(), "keys-a".to_owned(), "keys-b".to_owned(), "hebrew".to_owned()];
        v.extend(fallbacks.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, family("ubuntu"));
    fonts.families.insert(FontFamily::Name(MEDIUM.into()), family(MEDIUM));
    fonts.families.insert(FontFamily::Name(BOLD.into()), family(BOLD));
    fonts.families.insert(FontFamily::Name(ANNOT_FONT.into()), family(BOLD));
    fonts.families.insert(FontFamily::Name(ICON_FONT.into()), vec![ICON_FONT.to_owned()]);
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, bold(22.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, medium(13.5)),
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 8.0);
        s.button_padding = Vec2::new(12.0, 6.0);
        s.interact_size.y = 30.0;
        s.slider_width = 160.0;
        s.menu_margin = Margin::same(8);
        s.window_margin = Margin::same(18);

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = SURFACE;
        v.window_fill = SURFACE;
        v.extreme_bg_color = Color32::from_rgb(11, 13, 17);
        v.faint_bg_color = SURFACE_2;
        v.code_bg_color = SURFACE_2;
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_shadow = Shadow { offset: [0, 14], blur: 40, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(120) };
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.hyperlink_color = ACCENT;
        v.slider_trailing_fill = true;
        v.handle_shape = egui::style::HandleShape::Circle;

        let r = CornerRadius::same(8);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = SURFACE;
        w.noninteractive.weak_bg_fill = SURFACE;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = SURFACE_3;
        w.inactive.weak_bg_fill = SURFACE_2;
        w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = SURFACE_3;
        w.hovered.weak_bg_fill = SURFACE_3;
        w.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = ACCENT;
        w.active.weak_bg_fill = Color32::from_rgb(44, 50, 64);
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.5, TEXT);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;
        w.open.weak_bg_fill = SURFACE_3;
        w.open.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
        w.open.corner_radius = r;
    });

    // Gradient used for primary buttons, hero banner and accents.
    let n = 256;
    let pixels = (0..n)
        .map(|i| lerp_color(CORAL, ACCENT, i as f32 / (n - 1) as f32))
        .collect::<Vec<_>>();
    let tex = ctx.load_texture(
        "rega-gradient",
        egui::ColorImage::new([n, 1], pixels),
        egui::TextureOptions::LINEAR,
    );
    ctx.data_mut(|d| d.insert_temp(Id::unique("rega-gradient"), tex));

    let (rgba, w, h) = crate::icon::app_icon(128);
    let logo = ctx.load_texture(
        "rega-logo",
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba),
        egui::TextureOptions::LINEAR,
    );
    ctx.data_mut(|d| d.insert_temp(Id::unique("rega-logo"), logo));
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn texture(ctx: &egui::Context, name: &str) -> Option<TextureHandle> {
    ctx.data(|d| d.get_temp::<TextureHandle>(Id::unique(name)))
}

/// Paint a rounded rect filled with the brand gradient (left → right).
pub fn gradient_rect(painter: &egui::Painter, rect: Rect, radius: impl Into<CornerRadius>, tint: Color32) {
    if let Some(tex) = texture(painter.ctx(), "rega-gradient") {
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        painter.add(epaint::RectShape::filled(rect, radius, tint).with_texture(tex.id(), uv));
    } else {
        painter.rect_filled(rect, radius, ACCENT);
    }
}

pub fn logo(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    if let Some(tex) = texture(ui.ctx(), "rega-logo") {
        ui.painter().image(tex.id(), rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
    }
}

// ---------- widgets ----------

/// Tiny uppercase section label.
pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(text.to_uppercase()).font(medium(10.5)).color(TEXT_FAINT).extra_letter_spacing(1.2));
    ui.add_space(2.0);
}

fn hover_anim(ui: &Ui, id: Id, hovered: bool) -> f32 {
    ui.ctx().animate_bool_with_time(id.with("hover"), hovered, 0.12)
}

/// Square, borderless icon button.
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> Response {
    icon_button_sized(ui, glyph, tooltip, 34.0, true)
}

pub fn icon_button_sized(ui: &mut Ui, glyph: &str, tooltip: &str, size: f32, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), sense);
    let t = hover_anim(ui, resp.id, resp.hovered() && enabled);
    let p = ui.painter();
    if t > 0.0 {
        p.rect_filled(rect, 8.0, SURFACE_3.gamma_multiply(t));
    }
    let color = if !enabled {
        TEXT_FAINT.gamma_multiply(0.6)
    } else {
        lerp_color(TEXT_MUTED, TEXT, t)
    };
    p.text(rect.center(), Align2::CENTER_CENTER, glyph, icon_font(size * 0.58), color);
    if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) }
}

enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

fn button_impl(ui: &mut Ui, glyph: Option<&str>, text: &str, kind: Kind) -> Response {
    let font = medium(13.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, TEXT);
    let icon_w = if glyph.is_some() { 24.0 } else { 0.0 };
    let size = Vec2::new(galley.size().x + icon_w + 28.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = hover_anim(ui, resp.id, resp.hovered());
    let p = ui.painter();
    let pressed = resp.is_pointer_button_down_on();
    let (fg, icon_color) = match kind {
        Kind::Primary => {
            let tint = if pressed { Color32::from_gray(210) } else { lerp_color(Color32::from_gray(235), Color32::WHITE, t) };
            gradient_rect(p, rect, 9, tint);
            (Color32::WHITE, Color32::WHITE)
        }
        Kind::Secondary => {
            p.rect_filled(rect, 9.0, lerp_color(SURFACE_2, SURFACE_3, t));
            p.rect_stroke(rect, 9.0, Stroke::new(1.0, lerp_color(BORDER, BORDER_STRONG, t)), StrokeKind::Inside);
            (TEXT, TEXT_MUTED)
        }
        Kind::Ghost => {
            if t > 0.0 {
                p.rect_filled(rect, 9.0, SURFACE_3.gamma_multiply(t));
            }
            (TEXT, TEXT_MUTED)
        }
        Kind::Danger => {
            p.rect_filled(rect, 9.0, DANGER.gamma_multiply(0.12 + 0.1 * t));
            p.rect_stroke(rect, 9.0, Stroke::new(1.0, DANGER.gamma_multiply(0.5)), StrokeKind::Inside);
            (DANGER, DANGER)
        }
    };
    let mut x = rect.min.x + 14.0;
    if let Some(g) = glyph {
        p.text(egui::pos2(x + 9.0, rect.center().y), Align2::CENTER_CENTER, g, icon_font(19.0), icon_color);
        x += icon_w;
    }
    p.galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

pub fn primary_button(ui: &mut Ui, glyph: Option<&str>, text: &str) -> Response {
    button_impl(ui, glyph, text, Kind::Primary)
}
pub fn secondary_button(ui: &mut Ui, glyph: Option<&str>, text: &str) -> Response {
    button_impl(ui, glyph, text, Kind::Secondary)
}
pub fn ghost_button(ui: &mut Ui, glyph: Option<&str>, text: &str) -> Response {
    button_impl(ui, glyph, text, Kind::Ghost)
}
pub fn danger_button(ui: &mut Ui, glyph: Option<&str>, text: &str) -> Response {
    button_impl(ui, glyph, text, Kind::Danger)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ItemStyle {
    Normal,
    /// Brand gradient, for the main action of a group.
    Primary,
}

pub const TOOLBAR_HEIGHT: f32 = 50.0;

/// Top-bar tile: icon above a short label.
pub fn toolbar_item(ui: &mut Ui, glyph: &str, label: &str, style: ItemStyle, enabled: bool) -> Response {
    let label_galley = ui.painter().layout_no_wrap(label.to_owned(), medium(11.0), TEXT);
    let size = Vec2::new((label_galley.size().x + 16.0).max(50.0), TOOLBAR_HEIGHT);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let t = hover_anim(ui, resp.id, resp.hovered() && enabled);
    let pressed = resp.is_pointer_button_down_on();
    let p = ui.painter();
    let (icon_color, text_color) = match style {
        ItemStyle::Primary => {
            let tint = if pressed { Color32::from_gray(205) } else { lerp_color(Color32::from_gray(232), Color32::WHITE, t) };
            gradient_rect(p, rect, 11, tint);
            (Color32::WHITE, Color32::WHITE)
        }
        ItemStyle::Normal => {
            if t > 0.0 {
                p.rect_filled(rect, 11.0, SURFACE_3.gamma_multiply(t));
            }
            if !enabled {
                (TEXT_FAINT.gamma_multiply(0.5), TEXT_FAINT.gamma_multiply(0.6))
            } else {
                (lerp_color(Color32::from_rgb(196, 201, 214), TEXT, t), lerp_color(TEXT_MUTED, TEXT, t))
            }
        }
    };
    let lift = if pressed { 1.0 } else { 0.0 };
    p.text(rect.center_top() + Vec2::new(0.0, 17.0 + lift), Align2::CENTER_CENTER, glyph, icon_font(22.0), icon_color);
    p.galley(
        egui::pos2(rect.center().x - label_galley.size().x / 2.0, rect.max.y - label_galley.size().y - 5.0 + lift),
        label_galley,
        text_color,
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    resp.on_hover_cursor(if enabled { egui::CursorIcon::PointingHand } else { egui::CursorIcon::Default })
}

/// Rounded "dock" that groups related toolbar items.
pub fn toolbar_group<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(Color32::from_rgb(17, 20, 26))
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(15)
        .inner_margin(Margin::same(4))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                // Fix the row height first so every tile shares one center line.
                ui.set_min_height(TOOLBAR_HEIGHT);
                add(ui)
            })
            .inner
        })
        .inner
}

/// Height of a toolbar group: tile height plus the dock's margin and border.
pub const GROUP_HEIGHT: f32 = TOOLBAR_HEIGHT + 10.0;

/// A top-bar row. Items are top-aligned; since every dock has the same
/// height, that lines them all up exactly (centered rows in egui center each
/// item on the row height *so far*, which drifts).
pub fn toolbar_row<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), GROUP_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Min),
        add,
    )
}

/// Logo + app name, painted at the exact vertical center of the row.
pub fn toolbar_brand(ui: &mut Ui, name: &str) -> Rect {
    let galley = ui.painter().layout_no_wrap(name.to_owned(), bold(18.0), TEXT);
    let logo_size = 32.0;
    let width = logo_size + 10.0 + galley.size().x;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, GROUP_HEIGHT), Sense::hover());
    let cy = rect.center().y;
    let logo_rect = Rect::from_min_size(egui::pos2(rect.min.x, cy - logo_size / 2.0), Vec2::splat(logo_size));
    if let Some(tex) = texture(ui.ctx(), "rega-logo") {
        ui.painter().image(tex.id(), logo_rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
    }
    let text_pos = egui::pos2(logo_rect.max.x + 10.0, cy - galley.size().y / 2.0);
    let text_rect = Rect::from_min_size(text_pos, galley.size());
    ui.painter().galley(text_pos, galley, TEXT);
    text_rect
}

/// Thin vertical divider between items inside a group.
pub fn toolbar_divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, TOOLBAR_HEIGHT), Sense::hover());
    ui.painter().line_segment(
        [rect.center_top() + Vec2::new(0.0, 12.0), rect.center_bottom() - Vec2::new(0.0, 12.0)],
        Stroke::new(1.0, BORDER),
    );
}

/// Tool tile for the editor tool grid.
pub fn tool_tile(ui: &mut Ui, glyph: &str, selected: bool, tooltip: &str) -> Response {
    tool_tile_painted(ui, selected, tooltip, |p, c, color| {
        p.text(c, Align2::CENTER_CENTER, glyph, icon_font(23.0), color);
    })
}

/// A tool tile whose icon is drawn by `icon(painter, center, color)`, for tools
/// without a glyph in the icon font. Icons are about 23 pt across.
pub fn tool_tile_painted(ui: &mut Ui, selected: bool, tooltip: &str, icon: impl FnOnce(&egui::Painter, egui::Pos2, Color32)) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::click());
    let t = hover_anim(ui, resp.id, resp.hovered());
    let p = ui.painter();
    if selected {
        gradient_rect(p, rect, 11, Color32::WHITE);
    } else {
        p.rect_filled(rect, 11.0, lerp_color(SURFACE_2, SURFACE_3, t));
        p.rect_stroke(rect, 11.0, Stroke::new(1.0, lerp_color(BORDER, BORDER_STRONG, t)), StrokeKind::Inside);
    }
    let color = if selected { Color32::WHITE } else { lerp_color(TEXT_MUTED, TEXT, t) };
    icon(p, rect.center(), color);
    resp.on_hover_text(tooltip)
}

/// Segmented control, returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    let font = medium(12.5);
    let labels: Vec<f32> = options.iter().map(|(_, l)| ui.painter().layout_no_wrap(l.to_string(), font.clone(), TEXT).size().x).collect();
    // Roomy padding, tightened when the space is short (a narrow side panel).
    let pad = ((ui.available_width() - 6.0 - labels.iter().sum::<f32>()) / options.len().max(1) as f32).clamp(10.0, 22.0);
    let widths: Vec<f32> = labels.iter().map(|w| w + pad).collect();
    let total = widths.iter().sum::<f32>() + 6.0;
    // Its own id keys the choices, so two controls offering "Small" can sit together.
    let (rect, whole) = ui.allocate_exact_size(Vec2::new(total, 32.0), Sense::hover());
    let p = ui.painter().clone();
    p.rect_filled(rect, 9.0, Color32::from_rgb(11, 13, 17));
    p.rect_stroke(rect, 9.0, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    let mut x = rect.min.x + 3.0;
    for ((opt, label), w) in options.iter().zip(widths) {
        let r = Rect::from_min_size(egui::pos2(x, rect.min.y + 3.0), Vec2::new(w, 26.0));
        let resp = ui.interact(r, whole.id.with(("seg", label)), Sense::click());
        let selected = *value == *opt;
        if selected {
            p.rect_filled(r, 7.0, SURFACE_3);
            p.rect_stroke(r, 7.0, Stroke::new(1.0, BORDER_STRONG), StrokeKind::Inside);
        } else if resp.hovered() {
            p.rect_filled(r, 7.0, SURFACE_2);
        }
        p.text(r.center(), Align2::CENTER_CENTER, *label, font.clone(), if selected { TEXT } else { TEXT_MUTED });
        if resp.clicked() && !selected {
            *value = *opt;
            changed = true;
        }
        x += w;
    }
    changed
}

/// iOS-style switch with a label.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let width = ui.available_width();
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width, 28.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.15);
    let p = ui.painter();
    p.text(rect.left_center(), Align2::LEFT_CENTER, label, FontId::proportional(13.5), TEXT);
    let track = Rect::from_min_size(egui::pos2(rect.max.x - 36.0, rect.center().y - 10.0), Vec2::new(36.0, 20.0));
    if t > 0.5 {
        gradient_rect(p, track, 10, Color32::WHITE);
    } else {
        p.rect_filled(track, 10.0, SURFACE_3);
    }
    let knob_x = egui::lerp(track.min.x + 10.0..=track.max.x - 10.0, t);
    p.circle_filled(egui::pos2(knob_x, track.center().y), 8.0, Color32::WHITE);
    resp
}

/// A frame for full panels.
pub fn panel_frame(fill: Color32, margin: Margin) -> egui::Frame {
    egui::Frame::new().fill(fill).inner_margin(margin)
}

pub fn soft_shadow(rect: Rect, radius: u8) -> egui::Shape {
    Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(150) }
        .as_shape(rect, CornerRadius::same(radius))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Docks with different item counts, plus the logo block, share one row line.
    #[test]
    fn toolbar_groups_are_vertically_aligned() {
        let ctx = egui::Context::default();
        install(&ctx);
        let mut rects = Vec::new();
        for frame in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1400.0, 800.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                egui::Panel::top("bar").show(ui, |ui| {
                    toolbar_row(ui, |ui| {
                        let mut found = vec![];
                        found.push(toolbar_brand(ui, "Rega"));
                        for n in 0..3 {
                            let r = ui.scope(|ui| {
                                toolbar_group(ui, |ui| {
                                    for _ in 0..=n {
                                        toolbar_item(ui, icon::COPY, "Copy", ItemStyle::Normal, true);
                                    }
                                    toolbar_divider(ui);
                                })
                            });
                            found.push(r.response.rect);
                            ui.add_space(6.0);
                        }
                        if frame == 1 {
                            rects = found;
                        }
                    });
                });
            });
            out.textures_delta.clear();
        }
        let docks = &rects[1..];
        for r in docks {
            assert!((r.center().y - docks[0].center().y).abs() < 0.5, "docks misaligned: {rects:?}");
            assert!((r.height() - docks[0].height()).abs() < 0.5, "dock heights differ: {rects:?}");
        }
        assert!((rects[0].center().y - docks[0].center().y).abs() < 1.0, "title off-center: {rects:?}");
    }
}
