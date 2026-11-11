//! Guides: step-by-step walkthroughs recorded from the user's clicks and
//! typing. Each step is a screenshot, a marker on what was clicked or typed
//! into, and a caption. A guide is shared as one self-contained HTML page
//! (see `export`) and stays editable through a sidecar folder of lossless
//! screenshots and a JSON description, as images keep theirs under `.rega/`.

pub mod editor;
pub mod export;
pub mod input;
pub mod recorder;
pub mod style;

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use image::{ImageEncoder, RgbaImage};
use serde::{Deserialize, Serialize};

pub use style::Style;

/// One screenshot, held PNG-encoded: a raw full-screen frame is ~9 MB, and a
/// guide has dozens. Steps that show the same screen share one `Shot`.
pub struct Shot {
    /// Content hash: names the sidecar file and keys the export cache.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

impl Shot {
    pub fn encode(img: &RgbaImage) -> Result<Arc<Shot>> {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut png,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)
        .context("encoding a screenshot")?;
        Ok(Arc::new(Shot { id: content_id(&png), width: img.width(), height: img.height(), png }))
    }

    fn from_png(png: Vec<u8>) -> Result<Arc<Shot>> {
        let (width, height) = image::ImageReader::with_format(std::io::Cursor::new(&png), image::ImageFormat::Png)
            .into_dimensions()
            .context("reading a screenshot")?;
        Ok(Arc::new(Shot { id: content_id(&png), width, height, png }))
    }

    pub fn decode(&self) -> Result<RgbaImage> {
        Ok(image::load_from_memory_with_format(&self.png, image::ImageFormat::Png)
            .context("decoding a screenshot")?
            .to_rgba8())
    }

    fn file_name(&self) -> String {
        format!("{:016x}.png", self.id)
    }
}

fn content_id(bytes: &[u8]) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Shrink a capture to at most `max_side` on its long side (see
/// `Quality::max_side`: a Retina screen is ~3000 px across, and at 1920 text
/// stays sharp while each step costs a third of the memory and file size).
pub fn fit_capture(img: RgbaImage, max_side: Option<u32>) -> RgbaImage {
    let (w, h) = img.dimensions();
    let long = w.max(h);
    let Some(max_side) = max_side.filter(|m| long > *m) else { return img };
    let s = max_side as f32 / long as f32;
    let (tw, th) = (((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1));
    crate::resize::resize(&img, tw, th)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// What the user did on a step, as recorded. It only supplies the default
/// caption and the marker's look; the caption itself is the user's to edit.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Action {
    Click {
        button: Button,
        /// 2 for a double-click.
        count: u32,
    },
    Type {
        text: String,
        /// The key that ended the typing, when it was Enter or Tab.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        then: Option<String>,
    },
    /// A key or shortcut on its own, e.g. `⌘C` or `Esc`.
    Key { combo: String },
}

impl Action {
    pub fn default_title(&self) -> String {
        match self {
            Action::Click { button: Button::Right, .. } => "Right-click here".to_owned(),
            Action::Click { count: 2.., .. } => "Double-click here".to_owned(),
            Action::Click { .. } => "Click here".to_owned(),
            // Typed text left out of captions (Settings › Guide).
            Action::Type { text, then } if text.is_empty() => match then {
                Some(key) => format!("Type here and press {key}"),
                None => "Type here".to_owned(),
            },
            Action::Type { text, then } => {
                let shown = if text.chars().count() > 60 {
                    format!("{}…", text.chars().take(59).collect::<String>())
                } else {
                    text.clone()
                };
                match then {
                    Some(key) => format!("Type “{shown}” and press {key}"),
                    None => format!("Type “{shown}”"),
                }
            }
            Action::Key { combo } => format!("Press {combo}"),
        }
    }
}

#[derive(Clone)]
pub struct Step {
    pub shot: Arc<Shot>,
    pub action: Action,
    /// Where on the screenshot, as fractions of its width and height.
    pub marker: Option<[f32; 2]>,
    /// The marker is switched off; its place is kept for switching it back on.
    pub marker_hidden: bool,
    pub title: String,
    pub note: String,
}

impl Step {
    /// The marker as shown: none while it's hidden.
    pub fn visible_marker(&self) -> Option<[f32; 2]> {
        self.marker.filter(|_| !self.marker_hidden)
    }
}

#[derive(Clone)]
pub struct Guide {
    pub title: String,
    pub style: Style,
    pub steps: Vec<Step>,
}

impl Guide {
    pub fn new(title: String, steps: Vec<Step>) -> Self {
        Self { title, style: Style::default(), steps }
    }
}

// ---------- sidecar ----------

#[derive(Serialize, Deserialize)]
struct GuideFile {
    version: u32,
    title: String,
    /// Version 1 kept only the step time; version 2 the whole style.
    #[serde(default, skip_serializing)]
    seconds: Option<f32>,
    #[serde(default)]
    style: Option<Style>,
    steps: Vec<StepFile>,
}

#[derive(Serialize, Deserialize)]
struct StepFile {
    shot: String,
    action: Action,
    marker: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    marker_hidden: bool,
    title: String,
    #[serde(default)]
    note: String,
}

/// The folder that keeps a guide editable: `.rega/<name>.guide/`.
pub fn sidecar_dir(html: &Path) -> PathBuf {
    let name = html.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    html.parent().unwrap_or_else(|| Path::new(".")).join(".rega").join(format!("{name}.guide"))
}

fn sidecar_json(html: &Path) -> PathBuf {
    sidecar_dir(html).join("guide.json")
}

/// Is `path` a guide Rega can open (an HTML page with its sidecar)?
pub fn is_guide(html: &Path) -> bool {
    sidecar_json(html).is_file()
}

/// Write the sidecar: new screenshots are added, ones no step uses any more removed.
pub fn save_sidecar(html: &Path, guide: &Guide) -> Result<()> {
    let dir = sidecar_dir(html);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut keep = HashSet::new();
    for step in &guide.steps {
        let name = step.shot.file_name();
        let path = dir.join(&name);
        if keep.insert(name) && !path.exists() {
            write_atomic(&path, &step.shot.png)?;
        }
    }
    let file = GuideFile {
        version: 2,
        title: guide.title.clone(),
        seconds: None,
        style: Some(guide.style),
        steps: guide
            .steps
            .iter()
            .map(|s| StepFile {
                shot: s.shot.file_name(),
                action: s.action.clone(),
                marker: s.marker,
                marker_hidden: s.marker_hidden,
                title: s.title.clone(),
                note: s.note.clone(),
            })
            .collect(),
    };
    write_atomic(&dir.join("guide.json"), &serde_json::to_vec_pretty(&file)?)?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".png") && !keep.contains(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

pub fn load(html: &Path) -> Result<Guide> {
    let dir = sidecar_dir(html);
    let json = std::fs::read(sidecar_json(html)).context("this guide's editable copy is missing")?;
    let file: GuideFile = serde_json::from_slice(&json).context("reading the guide")?;
    let mut shots: HashMap<String, Arc<Shot>> = HashMap::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for s in file.steps {
        // Only a plain file name: the JSON must not reach outside its folder.
        if s.shot.contains(['/', '\\']) || s.shot.starts_with('.') {
            continue;
        }
        let shot = match shots.get(&s.shot) {
            Some(shot) => shot.clone(),
            None => {
                let png = std::fs::read(dir.join(&s.shot)).with_context(|| format!("screenshot {} is missing", s.shot))?;
                let shot = Shot::from_png(png)?;
                shots.insert(s.shot.clone(), shot.clone());
                shot
            }
        };
        steps.push(Step { shot, action: s.action, marker: s.marker, marker_hidden: s.marker_hidden, title: s.title, note: s.note });
    }
    let mut style = file.style.unwrap_or_default();
    if let Some(seconds) = file.seconds {
        style.seconds = seconds;
    }
    style.seconds = style.seconds.clamp(1.0, 30.0);
    Ok(Guide { title: file.title, style, steps })
}

/// The first step's screenshot, for the library thumbnail.
pub fn first_shot(html: &Path) -> Result<RgbaImage> {
    let json = std::fs::read(sidecar_json(html))?;
    let file: GuideFile = serde_json::from_slice(&json)?;
    let first = file.steps.first().context("the guide has no steps")?;
    anyhow::ensure!(!first.shot.contains(['/', '\\']), "bad screenshot name");
    Ok(image::open(sidecar_dir(html).join(&first.shot))?.to_rgba8())
}

pub fn delete_sidecar(html: &Path) {
    let _ = std::fs::remove_dir_all(sidecar_dir(html));
}

/// Write via a temporary file and a rename, so a reader (or a crash) never
/// sees half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = crate::library::part_path(path);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A screen-like test image: flat background, a few "widgets".
    pub fn screen(w: u32, h: u32, seed: u8) -> RgbaImage {
        let mut img = RgbaImage::from_pixel(w, h, image::Rgba([245, 246, 248, 255]));
        for y in 0..h {
            for x in 0..w {
                if (x / 40 + y / 30) % 7 == seed as u32 % 7 {
                    img.put_pixel(x, y, image::Rgba([40, 90, 200, 255]));
                }
            }
        }
        img
    }

    pub fn step(shot: &Arc<Shot>, title: &str) -> Step {
        Step {
            shot: shot.clone(),
            action: Action::Click { button: Button::Left, count: 1 },
            marker: Some([0.5, 0.25]),
            marker_hidden: false,
            title: title.to_owned(),
            note: String::new(),
        }
    }

    #[test]
    fn default_titles_describe_the_action() {
        let click = |button, count| Action::Click { button, count }.default_title();
        assert_eq!(click(Button::Left, 1), "Click here");
        assert_eq!(click(Button::Left, 2), "Double-click here");
        assert_eq!(click(Button::Right, 1), "Right-click here");
        let typed = Action::Type { text: "hello".into(), then: Some("Enter".into()) };
        assert_eq!(typed.default_title(), "Type “hello” and press Enter");
        assert_eq!(Action::Key { combo: "⌘C".into() }.default_title(), "Press ⌘C");
        assert_eq!(Action::Type { text: String::new(), then: Some("Tab".into()) }.default_title(), "Type here and press Tab");
    }

    /// Saved and loaded back, a guide is the same; shared screenshots are
    /// stored once, and ones no longer used are removed.
    #[test]
    fn sidecar_round_trip() {
        let dir = std::env::temp_dir().join(format!("rega-guide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let html = dir.join("Guide.html");
        let (a, b) = (Shot::encode(&screen(64, 48, 1)).unwrap(), Shot::encode(&screen(64, 48, 2)).unwrap());
        let mut guide = Guide::new("Set up".into(), vec![step(&a, "one"), step(&a, "two"), step(&b, "three")]);
        guide.steps[1].action = Action::Type { text: "hi".into(), then: None };
        guide.steps[2].note = "Then wait".into();
        guide.steps[2].marker = None;
        guide.steps[0].marker_hidden = true;
        guide.style.marker = style::MarkerShape::Spotlight;
        guide.style.seconds = 6.5;
        save_sidecar(&html, &guide).unwrap();
        assert!(is_guide(&html));
        let pngs = || std::fs::read_dir(sidecar_dir(&html)).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".png")).count();
        assert_eq!(pngs(), 2);

        let back = load(&html).unwrap();
        assert_eq!(back.title, "Set up");
        assert_eq!(back.steps.len(), 3);
        assert!(Arc::ptr_eq(&back.steps[0].shot, &back.steps[1].shot), "one shot for both");
        assert_eq!(back.steps[1].action, guide.steps[1].action);
        assert_eq!((back.steps[2].note.as_str(), back.steps[2].marker), ("Then wait", None));
        assert!(back.steps[0].marker_hidden && back.steps[0].marker == Some([0.5, 0.25]), "a hidden marker keeps its place");
        assert_eq!(back.steps[0].visible_marker(), None);
        assert_eq!(back.style, guide.style);
        assert_eq!(back.steps[2].shot.decode().unwrap(), screen(64, 48, 2));
        assert_eq!(first_shot(&html).unwrap(), screen(64, 48, 1));

        guide.steps.remove(2);
        save_sidecar(&html, &guide).unwrap();
        assert_eq!(pngs(), 1, "the unused screenshot is gone");
        delete_sidecar(&html);
        assert!(!is_guide(&html));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn captures_are_shrunk_to_the_size_cap() {
        assert_eq!(fit_capture(RgbaImage::new(3024, 1964), Some(1920)).dimensions(), (1920, 1247));
        assert_eq!(fit_capture(RgbaImage::new(800, 600), Some(1920)).dimensions(), (800, 600));
        assert_eq!(fit_capture(RgbaImage::new(3024, 1964), None).dimensions(), (3024, 1964));
    }

    /// A guide saved before styles existed opens with its step time kept.
    #[test]
    fn version_one_guides_still_open() {
        let file: GuideFile = serde_json::from_str(r#"{"version":1,"title":"Old","seconds":7.0,"steps":[]}"#).unwrap();
        assert_eq!((file.seconds, file.style), (Some(7.0), None));
    }
}
