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
    /// The picture has been marked up (arrows, blur, …): what it was before,
    /// and the marks, so it can be edited again. `shot` is the result.
    pub edit: Option<Arc<ShotEdit>>,
}

/// A step's picture before it was marked up in the editor, and the marks.
pub struct ShotEdit {
    pub original: Arc<Shot>,
    pub project: crate::editor::model::Project,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edit: Option<EditFile>,
}

/// A marked-up step: the file of the picture as it was, and the marks.
#[derive(Serialize, Deserialize)]
struct EditFile {
    original: String,
    project: crate::editor::model::Project,
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

/// The guide as its sidecar's `guide.json` describes it.
fn guide_file(guide: &Guide) -> GuideFile {
    GuideFile {
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
                edit: s.edit.as_ref().map(|e| EditFile { original: e.original.file_name(), project: e.project.clone() }),
            })
            .collect(),
    }
}

/// Every picture a guide keeps, once each: the steps' own, and the ones
/// marked-up steps were made from.
fn shots_of(guide: &Guide) -> Vec<&Arc<Shot>> {
    let mut seen = HashSet::new();
    guide
        .steps
        .iter()
        .flat_map(|s| [Some(&s.shot), s.edit.as_ref().map(|e| &e.original)])
        .flatten()
        .filter(|shot| seen.insert(shot.id))
        .collect()
}

/// A plain file name: what `guide.json` says must not reach outside its folder.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && !name.starts_with('.')
}

/// The guide `file` describes, its pictures fetched by name with `read`.
fn guide_from(file: GuideFile, mut read: impl FnMut(&str) -> Result<Vec<u8>>) -> Result<Guide> {
    let mut shots: HashMap<String, Arc<Shot>> = HashMap::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for s in file.steps {
        if !plain_name(&s.shot) {
            continue;
        }
        let mut shot_named = |name: &str| -> Result<Arc<Shot>> {
            if let Some(shot) = shots.get(name) {
                return Ok(shot.clone());
            }
            let shot = Shot::from_png(read(name).with_context(|| format!("screenshot {name} is missing"))?)?;
            shots.insert(name.to_owned(), shot.clone());
            Ok(shot)
        };
        let shot = shot_named(&s.shot)?;
        // Without the picture as it was, the step is still whole: only its
        // marks can't be edited any more.
        let edit = s.edit.filter(|e| plain_name(&e.original)).and_then(|e| {
            let original = shot_named(&e.original).ok()?;
            Some(Arc::new(ShotEdit { original, project: e.project }))
        });
        steps.push(Step { shot, action: s.action, marker: s.marker, marker_hidden: s.marker_hidden, title: s.title, note: s.note, edit });
    }
    let mut style = file.style.unwrap_or_default();
    if let Some(seconds) = file.seconds {
        style.seconds = seconds;
    }
    style.seconds = style.seconds.clamp(1.0, 30.0);
    Ok(Guide { title: file.title, style, steps })
}

/// Write the sidecar: new screenshots are added, ones no step uses any more removed.
pub fn save_sidecar(html: &Path, guide: &Guide) -> Result<()> {
    let dir = sidecar_dir(html);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut keep = HashSet::new();
    for shot in shots_of(guide) {
        let name = shot.file_name();
        let path = dir.join(&name);
        if !path.exists() {
            write_atomic(&path, &shot.png)?;
        }
        keep.insert(name);
    }
    write_atomic(&dir.join("guide.json"), &serde_json::to_vec_pretty(&guide_file(guide))?)?;
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
    guide_from(file, |name| Ok(std::fs::read(dir.join(name))?))
}

// ---------- one file to pass on ----------

/// The extension of a guide packed into one file for someone else to edit.
pub const PACKAGE_EXT: &str = "regaguide";
const PACKAGE_MAGIC: &[u8] = b"REGA-GUIDE 1\n";

/// Is `path` a guide packed into one file (by its name)?
pub fn is_package(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(PACKAGE_EXT))
}

/// Pack the whole editable guide into one file: what the sidecar folder
/// holds (the steps, their pictures at full quality, the marks on them), to
/// send to someone who opens it in their Rega and carries on editing.
///
/// The file is the magic line, then entry after entry: the name's length
/// (u32, little-endian), the name, the data's length (u64), the data.
/// `guide.json` comes first, then each picture.
pub fn write_package(dest: &Path, guide: &Guide) -> Result<()> {
    let mut out = PACKAGE_MAGIC.to_vec();
    let mut entry = |name: &str, data: &[u8]| {
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(data);
    };
    entry("guide.json", &serde_json::to_vec(&guide_file(guide))?);
    for shot in shots_of(guide) {
        entry(&shot.file_name(), &shot.png);
    }
    write_atomic(dest, &out)
}

/// Unpack a guide written by [`write_package`].
pub fn read_package(src: &Path) -> Result<Guide> {
    let bytes = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    let mut rest = bytes.strip_prefix(PACKAGE_MAGIC).context("this isn't a Rega guide file")?;
    let mut take = |n: usize| -> Result<&[u8]> {
        anyhow::ensure!(n <= rest.len(), "the guide file is cut short");
        let (head, tail) = rest.split_at(n);
        rest = tail;
        Ok(head)
    };
    let mut entries: HashMap<String, &[u8]> = HashMap::new();
    loop {
        let Ok(len) = take(4) else { break };
        let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
        anyhow::ensure!(len <= 255, "the guide file is damaged");
        let name = String::from_utf8(take(len)?.to_vec()).context("the guide file is damaged")?;
        let size = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let data = take(usize::try_from(size).context("the guide file is damaged")?)?;
        entries.insert(name, data);
    }
    let json = entries.get("guide.json").context("the guide file has no guide in it")?;
    let file: GuideFile = serde_json::from_slice(json).context("reading the guide")?;
    let guide = guide_from(file, |name| entries.get(name).map(|d| d.to_vec()).context("not in the file"))?;
    anyhow::ensure!(!guide.steps.is_empty(), "this guide has no steps");
    Ok(guide)
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
            edit: None,
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

    /// A guide packed into one file comes out whole on the other side: its
    /// steps, its pictures bit for bit, the marks on a marked-up step and
    /// the picture under them. A file that isn't one, or is cut short, says so.
    #[test]
    fn a_packed_guide_opens_whole() {
        let (plain, other) = (Shot::encode(&screen(64, 40, 1)).unwrap(), Shot::encode(&screen(64, 40, 2)).unwrap());
        let mut steps = vec![step(&plain, "One"), step(&other, "Two"), step(&plain, "Three")];
        steps[1].note = "Mind the gap".into();
        steps[1].marker_hidden = true;
        let mut project = crate::editor::model::Project::default();
        project.annotations.push(crate::editor::model::Annotation {
            kind: crate::editor::model::Kind::Rect { a: crate::editor::model::P::new(4.0, 4.0), b: crate::editor::model::P::new(20.0, 12.0) },
            color: [255, 0, 0, 255],
            width: 2.0,
            filled: true,
            rotation: 0.0,
        });
        steps[2].edit = Some(Arc::new(ShotEdit { original: plain.clone(), project }));
        steps[2].shot = Shot::encode(&screen(64, 40, 3)).unwrap();
        let mut guide = Guide::new("Reset a password".into(), steps);
        guide.style.seconds = 7.0;

        let file = std::env::temp_dir().join(format!("rega-packed-{}.{PACKAGE_EXT}", std::process::id()));
        assert!(is_package(&file) && !is_package(Path::new("guide.html")));
        write_package(&file, &guide).unwrap();
        let back = read_package(&file).unwrap();
        assert_eq!((back.title.as_str(), back.style.seconds, back.steps.len()), ("Reset a password", 7.0, 3));
        for (a, b) in guide.steps.iter().zip(&back.steps) {
            assert_eq!((&a.title, &a.note, a.marker, a.marker_hidden), (&b.title, &b.note, b.marker, b.marker_hidden));
            assert_eq!(a.shot.png, b.shot.png);
        }
        assert!(Arc::ptr_eq(&back.steps[0].shot, &back.steps[2].edit.as_ref().unwrap().original), "one picture, read once");
        assert_eq!(back.steps[2].edit.as_ref().unwrap().project.annotations.len(), 1);

        let bytes = std::fs::read(&file).unwrap();
        std::fs::write(&file, &bytes[..bytes.len() - 20]).unwrap();
        assert!(read_package(&file).is_err(), "cut short");
        std::fs::write(&file, b"<!doctype html>").unwrap();
        assert!(read_package(&file).is_err(), "not a guide file");
        let _ = std::fs::remove_file(&file);
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
