//! How a guide looks and plays: its marker, timing, page options and
//! picture quality. Each guide keeps its own, so changing a default never
//! changes a page already shared; Settings › Guide holds the defaults new
//! guides start with, and what a recording picks up.

use eframe::egui::{self, Align2, Color32, Pos2, Rect, Stroke, Vec2};
use serde::{Deserialize, Serialize};

use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerShape {
    #[default]
    Ring,
    Dot,
    Pointer,
    Hand,
    /// Dims everything but a circle around the spot.
    Spotlight,
    /// The step's number, on the spot.
    Number,
}

impl MarkerShape {
    pub const ALL: [MarkerShape; 6] =
        [MarkerShape::Ring, MarkerShape::Dot, MarkerShape::Pointer, MarkerShape::Hand, MarkerShape::Spotlight, MarkerShape::Number];

    pub fn label(self) -> &'static str {
        match self {
            MarkerShape::Ring => "Ring",
            MarkerShape::Dot => "Dot",
            MarkerShape::Pointer => "Pointer",
            MarkerShape::Hand => "Hand",
            MarkerShape::Spotlight => "Spotlight",
            MarkerShape::Number => "Step number",
        }
    }

    /// The name the page's stylesheet knows it by.
    pub fn css(self) -> &'static str {
        match self {
            MarkerShape::Ring => "ring",
            MarkerShape::Dot => "dot",
            MarkerShape::Pointer => "pointer",
            MarkerShape::Hand => "hand",
            MarkerShape::Spotlight => "spotlight",
            MarkerShape::Number => "number",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl MarkerSize {
    pub fn scale(self) -> f32 {
        match self {
            MarkerSize::Small => 0.75,
            MarkerSize::Medium => 1.0,
            MarkerSize::Large => 1.4,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageTheme {
    /// Follows the viewer's system.
    #[default]
    Auto,
    Light,
    Dark,
}

impl PageTheme {
    pub fn css(self) -> &'static str {
        match self {
            PageTheme::Auto => "auto",
            PageTheme::Light => "light",
            PageTheme::Dark => "dark",
        }
    }
}

/// Screenshot size (when recording) and compression (when the page is written).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Small,
    #[default]
    Balanced,
    Sharp,
    Lossless,
}

impl Quality {
    pub const ALL: [Quality; 4] = [Quality::Small, Quality::Balanced, Quality::Sharp, Quality::Lossless];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Small => "Small",
            Quality::Balanced => "Balanced",
            Quality::Sharp => "Sharp",
            Quality::Lossless => "Lossless",
        }
    }

    /// Screenshots are shrunk to this on their long side; `None` keeps them whole.
    pub fn max_side(self) -> Option<u32> {
        match self {
            Quality::Small => Some(1440),
            Quality::Balanced => Some(1920),
            Quality::Sharp => Some(2560),
            Quality::Lossless => None,
        }
    }

    /// JPEG quality the page may use; `None` keeps every picture lossless (PNG).
    pub fn jpeg(self) -> Option<u8> {
        match self {
            Quality::Small => Some(72),
            Quality::Balanced => Some(85),
            Quality::Sharp => Some(92),
            Quality::Lossless => None,
        }
    }

    pub fn details(self) -> &'static str {
        match self {
            Quality::Small => "Up to 1440 px, compressed harder: the lightest pages.",
            Quality::Balanced => "Up to 1920 px: sharp text, small pages.",
            Quality::Sharp => "Up to 2560 px, light compression: for big screens and fine print.",
            Quality::Lossless => "Full resolution, never lossy: the largest pages.",
        }
    }
}

/// Marker colours offered as swatches (any colour can be picked too).
pub const MARKER_COLORS: [([u8; 3], &str); 6] = [
    ([255, 94, 98], "Coral"),
    ([124, 77, 255], "Violet"),
    ([47, 128, 255], "Blue"),
    ([34, 197, 94], "Green"),
    ([245, 158, 11], "Amber"),
    ([236, 72, 153], "Pink"),
];

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    pub marker: MarkerShape,
    pub color: [u8; 3],
    pub size: MarkerSize,
    /// How long the page shows each step before moving on (longer captions get longer).
    pub seconds: f32,
    /// The page starts playing on its own.
    pub autoplay: bool,
    /// After the last step the page starts over.
    #[serde(rename = "loop")]
    pub repeat: bool,
    pub theme: PageTheme,
    /// "Made with Rega" under the page.
    pub branding: bool,
    pub quality: Quality,
}

pub const DEFAULT_SECONDS: f32 = 4.0;

impl Default for Style {
    fn default() -> Self {
        Self {
            marker: MarkerShape::Ring,
            color: MARKER_COLORS[0].0,
            size: MarkerSize::Medium,
            seconds: DEFAULT_SECONDS,
            autoplay: true,
            repeat: false,
            theme: PageTheme::Auto,
            branding: true,
            quality: Quality::Balanced,
        }
    }
}

impl Style {
    pub fn color32(&self) -> Color32 {
        Color32::from_rgb(self.color[0], self.color[1], self.color[2])
    }

    pub fn css_color(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.color[0], self.color[1], self.color[2])
    }
}

/// Settings › Guide.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GuideSettings {
    /// What new guides start with; its quality also sizes the screenshots.
    pub style: Style,
    pub record_typing: bool,
    /// Put what was typed in the caption ("Type “hello”"); off, the text is
    /// never kept and the caption only says to type.
    pub typed_text_in_captions: bool,
    /// Shortcuts and lone key presses (⌘C, Esc, Enter) become steps.
    pub record_keys: bool,
}

impl Default for GuideSettings {
    fn default() -> Self {
        Self { style: Style::default(), record_typing: true, typed_text_in_captions: true, record_keys: true }
    }
}

// ---------- controls, shared by Settings and the guide editor ----------

/// "Each step plays for" slider.
pub fn seconds_control(ui: &mut egui::Ui, seconds: &mut f32) -> egui::Response {
    ui.add(egui::Slider::new(seconds, 2.0..=12.0).step_by(0.5).suffix(" s"))
}

/// Marker shape, colour and size; true when something changed.
pub fn marker_controls(ui: &mut egui::Ui, style: &mut Style) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(8.0);
        for shape in MarkerShape::ALL {
            let preview = Style { marker: shape, size: MarkerSize::Medium, ..*style };
            let selected = style.marker == shape;
            let resp = theme::tool_tile_painted(ui, selected, shape.label(), |p, c, _| {
                paint_marker_icon(p, Rect::from_center_size(c, Vec2::splat(30.0)), &preview);
            });
            if resp.clicked() && !selected {
                style.marker = shape;
                changed = true;
            }
        }
    });
    ui.label(egui::RichText::new(style.marker.label()).color(theme::TEXT_MUTED).size(12.0));
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (rgb, name) in MARKER_COLORS {
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(24.0), egui::Sense::click());
            let c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
            let p = ui.painter();
            p.circle_filled(rect.center(), 10.0, c);
            if style.color == rgb {
                p.circle_stroke(rect.center(), 12.0, Stroke::new(2.0, theme::TEXT));
            } else if resp.hovered() {
                p.circle_stroke(rect.center(), 11.5, Stroke::new(1.0, theme::TEXT_MUTED));
            }
            if resp.on_hover_text(name).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && style.color != rgb {
                style.color = rgb;
                changed = true;
            }
        }
        let mut rgb = style.color;
        if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).on_hover_text("Any colour").changed() {
            style.color = rgb;
            changed = true;
        }
    });
    ui.add_space(6.0);
    // Above its choices, not beside them: the side panel is too narrow for both.
    ui.label("Marker size");
    changed |= theme::segmented(ui, &mut style.size, &[(MarkerSize::Small, "Small"), (MarkerSize::Medium, "Medium"), (MarkerSize::Large, "Large")]);
    changed
}

/// Autoplay, loop, theme and branding of the shared page; true when something changed.
pub fn page_controls(ui: &mut egui::Ui, style: &mut Style) -> bool {
    let mut changed = false;
    changed |= theme::switch(ui, &mut style.autoplay, "Play automatically").changed();
    changed |= theme::switch(ui, &mut style.repeat, "Start over after the last step").changed();
    changed |= theme::switch(ui, &mut style.branding, "Show “Made with Rega”").changed();
    ui.horizontal(|ui| {
        ui.label("Theme");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            changed |= theme::segmented(ui, &mut style.theme, &[(PageTheme::Auto, "Auto"), (PageTheme::Light, "Light"), (PageTheme::Dark, "Dark")]);
        });
    });
    ui.label(egui::RichText::new("Auto follows the viewer's light or dark setting.").color(theme::TEXT_FAINT).size(11.5));
    changed
}

/// Quality preset and what it means; true when it changed.
pub fn quality_control(ui: &mut egui::Ui, quality: &mut Quality) -> bool {
    let options: Vec<(Quality, &str)> = Quality::ALL.iter().map(|q| (*q, q.label())).collect();
    let changed = theme::segmented(ui, quality, &options);
    ui.label(egui::RichText::new(quality.details()).color(theme::TEXT_FAINT).size(11.5));
    changed
}

// ---------- drawing the marker ----------

/// Where a pointer's tip and a hand's fingertip sit in their glyphs, as
/// fractions of the glyph box: the spot goes there.
const HAND_HOTSPOT: Vec2 = Vec2::new(0.346, 0.203);
/// The filled hand (Material "touch_app", filled), at a private code point.
const HAND_FILLED: &str = "\u{f8fe}";

/// The marker at `c` on a screenshot shown in `frame`, as the page draws it.
/// `number` is the step's (1-based); `pulse` in 0..1 animates the ring.
pub fn paint_marker(p: &egui::Painter, frame: Rect, c: Pos2, style: &Style, number: usize, pulse: Option<f32>) {
    let color = style.color32();
    let r = 16.0 * style.size.scale();
    let white = Color32::WHITE;
    let ping = |p: &egui::Painter, base: f32| {
        if let Some(t) = pulse {
            p.circle_stroke(c, base * (1.0 + 1.3 * t), Stroke::new(3.0, color.gamma_multiply(0.85 * (1.0 - t))));
        }
    };
    match style.marker {
        MarkerShape::Ring => {
            ping(p, r);
            p.circle_filled(c, r, color.gamma_multiply(0.2));
            p.circle_stroke(c, r + 2.5, Stroke::new(3.0, color));
            p.circle_stroke(c, r, Stroke::new(3.0, white));
        }
        MarkerShape::Dot => {
            ping(p, r * 0.5);
            p.circle_filled(c, r * 0.5, color);
            p.circle_stroke(c, r * 0.5, Stroke::new(2.5, white));
        }
        MarkerShape::Number => {
            ping(p, r);
            p.circle_filled(c, r, color);
            p.circle_stroke(c, r, Stroke::new(2.5, white));
            p.text(c, Align2::CENTER_CENTER, number.to_string(), theme::bold(r * 1.05), white);
        }
        MarkerShape::Spotlight => {
            let hole = r * 2.6;
            dim_outside(p, frame, c, hole);
            p.circle_stroke(c, hole, Stroke::new(3.0, color));
        }
        MarkerShape::Pointer => paint_pointer(p, c, r * 2.2, color),
        MarkerShape::Hand => {
            let size = r * 3.0;
            let at = c + Vec2::new(0.5 - HAND_HOTSPOT.x, 0.5 - HAND_HOTSPOT.y) * size;
            let font = theme::icon_font(size);
            // A white outline: the glyph drawn around itself.
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                p.text(at + Vec2::angled(a) * 2.0, Align2::CENTER_CENTER, HAND_FILLED, font.clone(), white);
            }
            p.text(at, Align2::CENTER_CENTER, HAND_FILLED, font, color);
        }
    }
}

/// Darken all of `frame` but a circle of `radius` around `c`: a ring from
/// the circle out past the frame's corners, clipped to the frame.
fn dim_outside(p: &egui::Painter, frame: Rect, c: Pos2, radius: f32) {
    let far = [frame.left_top(), frame.right_top(), frame.left_bottom(), frame.right_bottom()]
        .iter()
        .map(|q| q.distance(c))
        .fold(radius, f32::max)
        + 2.0;
    let shade = Color32::from_black_alpha(140);
    let mut mesh = egui::Mesh::default();
    const N: u32 = 96;
    for k in 0..=N {
        let dir = Vec2::angled(k as f32 / N as f32 * std::f32::consts::TAU);
        mesh.colored_vertex(c + dir * radius, shade);
        mesh.colored_vertex(c + dir * far, shade);
    }
    for k in 0..N {
        let i = 2 * k;
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
    p.with_clip_rect(frame.intersect(p.clip_rect())).add(egui::Shape::mesh(mesh));
}

/// A classic arrow cursor `height` tall with its tip at `tip`.
fn paint_pointer(p: &egui::Painter, tip: Pos2, height: f32, color: Color32) {
    let s = height / 15.6;
    let pt = |x: f32, y: f32| tip + Vec2::new(x, y) * s;
    let head = vec![pt(0.0, 0.0), pt(0.0, 15.6), pt(11.8, 10.3)];
    let tail = vec![pt(3.0, 10.6), pt(6.2, 9.8), pt(9.4, 16.6), pt(6.4, 18.0)];
    // White first, wider, so the two parts share one outline.
    for part in [&head, &tail] {
        p.add(egui::Shape::convex_polygon(part.clone(), Color32::WHITE, Stroke::new(4.0, Color32::WHITE)));
    }
    for part in [head, tail] {
        p.add(egui::Shape::convex_polygon(part, color, Stroke::NONE));
    }
}

/// A small picture of the marker shape, for the shape picker.
fn paint_marker_icon(p: &egui::Painter, area: Rect, style: &Style) {
    let c = area.center();
    let s = Style { size: MarkerSize::Small, ..*style };
    match style.marker {
        MarkerShape::Spotlight => {
            let frame = area.shrink(1.0);
            p.rect_filled(frame, 4.0, Color32::from_gray(150));
            dim_outside(p, frame, c, 7.0);
            p.circle_stroke(c, 7.0, Stroke::new(2.0, style.color32()));
        }
        MarkerShape::Pointer => paint_pointer(p, c - Vec2::new(5.0, 9.0), 18.0, style.color32()),
        MarkerShape::Hand => {
            p.text(c, Align2::CENTER_CENTER, HAND_FILLED, theme::icon_font(28.0), style.color32());
        }
        _ => paint_marker(p, area, c, &s, 1, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guides and settings saved before a field existed still load, with
    /// the default for what's missing.
    #[test]
    fn missing_fields_take_defaults() {
        let style: Style = serde_json::from_str(r#"{"marker":"hand","loop":true}"#).unwrap();
        assert_eq!((style.marker, style.repeat, style.seconds, style.autoplay), (MarkerShape::Hand, true, DEFAULT_SECONDS, true));
        let settings: GuideSettings = serde_json::from_str(r#"{"record_typing":false}"#).unwrap();
        assert!(!settings.record_typing && settings.record_keys && settings.typed_text_in_captions);
    }

    #[test]
    fn quality_sets_size_and_compression() {
        assert_eq!(Quality::Balanced.max_side(), Some(1920));
        assert_eq!(Quality::Lossless.jpeg(), None);
        assert!(Quality::Small.jpeg() < Quality::Sharp.jpeg());
        assert_eq!(Style::default().css_color(), "#ff5e62");
    }
}
