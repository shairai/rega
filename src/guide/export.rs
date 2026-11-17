//! The shared page: one HTML file with every screenshot inside, so it can be
//! mailed, uploaded or opened offline as it is.
//!
//! Size is what matters here. Consecutive steps mostly show the same screen
//! (typing changes one field, a click opens one menu), so a step is stored as
//! the tiles that changed since the step before, laid over the last full
//! frame; a new full frame starts when most of the screen changed, when the
//! size changed, or after a dozen patches. Each picture is stored as JPEG or
//! PNG, whichever is smaller (PNG when close: it keeps text crisp), at the
//! guide's quality; Lossless keeps every picture PNG.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, ensure};
use base64::Engine;
use image::{ImageEncoder, RgbaImage};
use serde::Serialize;

use super::style::Quality;
use super::{Action, Guide, Shot};

const PAGE: &str = include_str!("page.html");

/// Patches are found on a grid of tiles this many pixels square.
const TILE: u32 = 32;
/// Above this share of the frame, a new full frame is about as small as the patches.
const MAX_PATCH_SHARE: f64 = 0.5;
/// Patches stacked on one full frame before the next full frame.
const MAX_CHAIN: usize = 12;
/// More separate changed areas than this are stored as one patch around them all.
const MAX_PATCHES: usize = 24;

/// An encoded picture.
struct Blob {
    mime: &'static str,
    bytes: Vec<u8>,
}

/// Pictures already encoded, kept between saves: after an edit to a caption,
/// a save only re-assembles the page.
#[derive(Default)]
pub struct Cache {
    /// The quality the pictures below were encoded at.
    quality: Mutex<Option<Quality>>,
    full: Mutex<HashMap<u64, Arc<Blob>>>,
    /// (previous shot, shot) → its patches, or `None` when a full frame is better.
    patches: Mutex<HashMap<(u64, u64), Option<Patches>>>,
}

type Patches = Arc<Vec<Patch>>;

struct Patch {
    rect: [u32; 4],
    blob: Arc<Blob>,
}

/// One picture laid on a step's frame: (picture, x, y, w, h), in the frame's pixels.
type Layer = (Arc<Blob>, [u32; 4]);

#[derive(Serialize)]
struct PageData<'a> {
    title: &'a str,
    style: PageStyle,
    images: Vec<String>,
    steps: Vec<PageStep<'a>>,
}

/// How the page plays and draws its marker (see `Style`).
#[derive(Serialize)]
struct PageStyle {
    marker: &'static str,
    color: String,
    size: f32,
    seconds: f32,
    autoplay: bool,
    #[serde(rename = "loop")]
    repeat: bool,
    theme: &'static str,
    branding: bool,
}

#[derive(Serialize)]
struct PageStep<'a> {
    w: u32,
    h: u32,
    /// [image, x, y, w, h], bottom first.
    layers: Vec<[u64; 5]>,
    title: &'a str,
    note: &'a str,
    kind: &'static str,
    marker: Option<[f32; 2]>,
}

/// Decoding a screenshot takes a while; each is decoded at most once per export.
struct Decoder {
    last: Vec<(u64, Arc<RgbaImage>)>,
}

impl Decoder {
    fn get(&mut self, shot: &Shot) -> Result<Arc<RgbaImage>> {
        if let Some((_, img)) = self.last.iter().find(|(id, _)| *id == shot.id) {
            return Ok(img.clone());
        }
        let img = Arc::new(shot.decode()?);
        self.last.push((shot.id, img.clone()));
        if self.last.len() > 2 {
            self.last.remove(0);
        }
        Ok(img)
    }
}

/// The page for `guide`.
pub fn page(guide: &Guide, cache: &Cache) -> Result<String> {
    ensure!(!guide.steps.is_empty(), "the guide has no steps");
    let jpeg = guide.style.quality.jpeg();
    cache.use_quality(guide.style.quality);
    let mut decoder = Decoder { last: Vec::new() };
    let mut images: Vec<String> = Vec::new();
    let mut index: HashMap<*const Blob, usize> = HashMap::new();
    let mut steps = Vec::with_capacity(guide.steps.len());
    // The previous step's shot, its layers, and how many patches they stack.
    let mut prev: Option<(&Arc<Shot>, Vec<Layer>, usize)> = None;

    for step in &guide.steps {
        let shot = &step.shot;
        let (layers, chain) = match &prev {
            Some((p, layers, chain)) if p.id == shot.id => (layers.clone(), *chain),
            Some((p, layers, chain)) if (p.width, p.height) == (shot.width, shot.height) && *chain < MAX_CHAIN => {
                match cache.patches_between(p, shot, &mut decoder, jpeg)? {
                    Some(patches) => {
                        let mut layers = layers.clone();
                        layers.extend(patches.iter().map(|p| (p.blob.clone(), p.rect)));
                        (drop_hidden(layers), chain + 1)
                    }
                    None => (vec![(cache.full_frame(shot, &mut decoder, jpeg)?, [0, 0, shot.width, shot.height])], 0),
                }
            }
            _ => (vec![(cache.full_frame(shot, &mut decoder, jpeg)?, [0, 0, shot.width, shot.height])], 0),
        };
        let refs = layers
            .iter()
            .map(|(blob, [x, y, w, h])| {
                let n = *index.entry(Arc::as_ptr(blob)).or_insert_with(|| {
                    images.push(data_uri(blob));
                    images.len() - 1
                });
                [n as u64, *x as u64, *y as u64, *w as u64, *h as u64]
            })
            .collect();
        steps.push(PageStep {
            w: shot.width,
            h: shot.height,
            layers: refs,
            title: &step.title,
            note: &step.note,
            kind: match step.action {
                Action::Click { .. } => "click",
                Action::Type { .. } => "type",
                Action::Key { .. } => "key",
            },
            marker: step.visible_marker(),
        });
        prev = Some((shot, layers, chain));
    }

    let st = &guide.style;
    let style = PageStyle {
        marker: st.marker.css(),
        color: st.css_color(),
        size: st.size.scale(),
        seconds: st.seconds,
        autoplay: st.autoplay,
        repeat: st.repeat,
        theme: st.theme.css(),
        branding: st.branding,
    };
    let data = PageData { title: &guide.title, style, images, steps };
    // Inside <script>, a "</script>" or "<!--" in a caption would end or
    // derail the block; JSON can spell every "<" as an escape instead.
    let json = serde_json::to_string(&data)?.replace('<', "\\u003c");
    let (head, rest) = PAGE.split_once("{{TITLE}}").expect("title slot");
    let (before_site, rest) = rest.split_once("{{SITE}}").expect("site slot");
    let (middle, tail) = rest.split_once("{{DATA}}").expect("data slot");
    let title = if guide.title.trim().is_empty() { "Guide" } else { guide.title.as_str() };
    // "Made with Rega" links to the website.
    let site = escape_html(crate::config::SITE_URL);
    Ok([head, &escape_html(title), before_site, &site, middle, &json, tail].concat())
}

impl Cache {
    /// Pictures encoded at another quality are no use: start over.
    fn use_quality(&self, quality: Quality) {
        let mut current = self.quality.lock().unwrap();
        if *current != Some(quality) {
            self.full.lock().unwrap().clear();
            self.patches.lock().unwrap().clear();
            *current = Some(quality);
        }
    }

    fn full_frame(&self, shot: &Shot, decoder: &mut Decoder, jpeg: Option<u8>) -> Result<Arc<Blob>> {
        if let Some(blob) = self.full.lock().unwrap().get(&shot.id) {
            return Ok(blob.clone());
        }
        let blob = Arc::new(encode_smallest(&*decoder.get(shot)?, jpeg)?);
        self.full.lock().unwrap().insert(shot.id, blob.clone());
        Ok(blob)
    }

    fn patches_between(&self, from: &Shot, to: &Shot, decoder: &mut Decoder, jpeg: Option<u8>) -> Result<Option<Patches>> {
        let key = (from.id, to.id);
        if let Some(found) = self.patches.lock().unwrap().get(&key) {
            return Ok(found.clone());
        }
        let (a, b) = (decoder.get(from)?, decoder.get(to)?);
        let patches = match changed_rects(&a, &b) {
            None => None,
            Some(rects) => {
                let mut out = Vec::with_capacity(rects.len());
                for rect in rects {
                    let [x, y, w, h] = rect;
                    let crop = image::imageops::crop_imm(&*b, x, y, w, h).to_image();
                    out.push(Patch { rect, blob: Arc::new(encode_smallest(&crop, jpeg)?) });
                }
                Some(Arc::new(out))
            }
        };
        self.patches.lock().unwrap().insert(key, patches.clone());
        Ok(patches)
    }

    /// Forget pictures of shots no longer in `guide`.
    pub fn retain(&self, guide: &Guide) {
        let live: std::collections::HashSet<u64> = guide.steps.iter().map(|s| s.shot.id).collect();
        self.full.lock().unwrap().retain(|id, _| live.contains(id));
        self.patches.lock().unwrap().retain(|(a, b), _| live.contains(a) && live.contains(b));
    }
}

/// Layers wholly covered by a later one never show; leave them out.
fn drop_hidden(layers: Vec<Layer>) -> Vec<Layer> {
    let covers = |outer: [u32; 4], inner: [u32; 4]| {
        outer[0] <= inner[0] && outer[1] <= inner[1] && outer[0] + outer[2] >= inner[0] + inner[2] && outer[1] + outer[3] >= inner[1] + inner[3]
    };
    let keep: Vec<bool> = (0..layers.len()).map(|i| !layers[i + 1..].iter().any(|(_, r)| covers(*r, layers[i].1))).collect();
    layers.into_iter().zip(keep).filter(|(_, k)| *k).map(|(l, _)| l).collect()
}

/// Pixel rectangles ([x, y, w, h]) covering everywhere `b` differs from `a`
/// (same size), on the tile grid; empty when they're identical, `None` when
/// so much changed that a full frame is the better choice.
fn changed_rects(a: &RgbaImage, b: &RgbaImage) -> Option<Vec<[u32; 4]>> {
    let (w, h) = a.dimensions();
    if b.dimensions() != (w, h) {
        return None;
    }
    let (cols, rows) = (w.div_ceil(TILE) as usize, h.div_ceil(TILE) as usize);
    let stride = w as usize * 4;
    let (ra, rb) = (a.as_raw(), b.as_raw());
    let mut dirty = vec![false; cols * rows];
    for ty in 0..rows {
        let y0 = ty * TILE as usize;
        let y1 = (y0 + TILE as usize).min(h as usize);
        for tx in 0..cols {
            let x0 = tx * TILE as usize * 4;
            let x1 = ((tx + 1) * TILE as usize).min(w as usize) * 4;
            dirty[ty * cols + tx] = (y0..y1).any(|y| ra[y * stride + x0..y * stride + x1] != rb[y * stride + x0..y * stride + x1]);
        }
    }

    // Boxes of tiles ([x0, y0, x1, y1), in tiles): each changed tile starts
    // as one, and boxes that touch or nearly touch (one tile apart) merge,
    // so a changed area costs one picture rather than dozens.
    let mut boxes: Vec<[usize; 4]> = (0..rows)
        .flat_map(|ty| (0..cols).map(move |tx| (tx, ty)))
        .filter(|&(tx, ty)| dirty[ty * cols + tx])
        .map(|(tx, ty)| [tx, ty, tx + 1, ty + 1])
        .collect();
    let near = |a: &[usize; 4], b: &[usize; 4]| a[0] <= b[2] + 1 && b[0] <= a[2] + 1 && a[1] <= b[3] + 1 && b[1] <= a[3] + 1;
    loop {
        let mut merged = false;
        let mut i = 0;
        while i < boxes.len() {
            let mut j = i + 1;
            while j < boxes.len() {
                if near(&boxes[i], &boxes[j]) {
                    let o = boxes.swap_remove(j);
                    let b = &mut boxes[i];
                    *b = [b[0].min(o[0]), b[1].min(o[1]), b[2].max(o[2]), b[3].max(o[3])];
                    merged = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
        if !merged {
            break;
        }
    }
    if boxes.len() > MAX_PATCHES {
        let all = boxes.iter().fold([usize::MAX, usize::MAX, 0, 0], |a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]);
        boxes = vec![all];
    }
    let rects: Vec<[u32; 4]> = boxes
        .iter()
        .map(|b| {
            let (x0, y0) = (b[0] as u32 * TILE, b[1] as u32 * TILE);
            let (x1, y1) = ((b[2] as u32 * TILE).min(w), (b[3] as u32 * TILE).min(h));
            [x0, y0, x1 - x0, y1 - y0]
        })
        .collect();
    let area: u64 = rects.iter().map(|r| r[2] as u64 * r[3] as u64).sum();
    (area as f64 <= MAX_PATCH_SHARE * w as f64 * h as f64).then_some(rects)
}

/// PNG unless JPEG (at quality `jpeg`; `None` never) is clearly smaller:
/// screens of flat colour and text are often smaller as PNG, and stay sharp.
fn encode_smallest(img: &RgbaImage, jpeg: Option<u8>) -> Result<Blob> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Default,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)?;
    let Some(quality) = jpeg else { return Ok(Blob { mime: "image/png", bytes: png }) };
    let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality).encode_image(&rgb)?;
    Ok(if png.len() as f64 <= jpeg.len() as f64 * 1.15 {
        Blob { mime: "image/png", bytes: png }
    } else {
        Blob { mime: "image/jpeg", bytes: jpeg }
    })
}

fn data_uri(blob: &Blob) -> String {
    format!("data:{};base64,{}", blob.mime, base64::engine::general_purpose::STANDARD.encode(&blob.bytes))
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::{screen, step};
    use super::*;

    fn paint(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, c: [u8; 4]) {
        for yy in y..y + h {
            for xx in x..x + w {
                img.put_pixel(xx, yy, image::Rgba(c));
            }
        }
    }

    #[test]
    fn changes_are_found_as_few_tile_aligned_rects() {
        let a = screen(400, 300, 1);
        assert_eq!(changed_rects(&a, &a), Some(vec![]), "nothing changed");
        let mut b = a.clone();
        // A field being typed into, and a far-off clock.
        paint(&mut b, 70, 100, 90, 20, [0, 0, 0, 255]);
        paint(&mut b, 380, 5, 10, 10, [255, 0, 0, 255]);
        let mut rects = changed_rects(&a, &b).unwrap();
        rects.sort();
        assert_eq!(rects, vec![[64, 96, 96, 32], [352, 0, 48, 32]]);
        // Every changed pixel is inside a rect.
        for (x, y, p) in b.enumerate_pixels() {
            if a.get_pixel(x, y) != p {
                assert!(rects.iter().any(|r| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]), "{x},{y}");
            }
        }
        // A whole new screen is a full frame.
        assert_eq!(changed_rects(&a, &screen(400, 300, 4)), None);
    }

    #[test]
    fn layers_hidden_under_later_ones_are_dropped() {
        let blob = Arc::new(Blob { mime: "image/png", bytes: vec![] });
        let layers = vec![(blob.clone(), [0, 0, 100, 100]), (blob.clone(), [10, 10, 20, 20]), (blob.clone(), [0, 0, 50, 50])];
        let kept: Vec<[u32; 4]> = drop_hidden(layers).into_iter().map(|(_, r)| r).collect();
        assert_eq!(kept, vec![[0, 0, 100, 100], [0, 0, 50, 50]]);
    }

    /// The page holds every step; a step that only changed a little is a
    /// small patch over the frame before, and the same screen is stored once.
    #[test]
    fn page_stores_small_changes_as_patches() {
        let a = screen(640, 400, 1);
        let mut b = a.clone();
        paint(&mut b, 100, 100, 60, 20, [0, 0, 0, 255]);
        let c = screen(640, 400, 3);
        let (sa, sb, sc) = (Shot::encode(&a).unwrap(), Shot::encode(&b).unwrap(), Shot::encode(&c).unwrap());
        let mut guide = Guide::new("How to </script> <b>this</b>".into(), vec![step(&sa, "Open"), step(&sb, "Type"), step(&sb, "Again"), step(&sc, "New")]);
        guide.steps[3].note = "Done & dusted".into();
        let cache = Cache::default();
        let html = page(&guide, &cache).unwrap();
        assert!(html.contains("<title>How to &lt;/script&gt; &lt;b&gt;this&lt;/b&gt;</title>"));
        assert_eq!(html.matches("</script>").count(), 2, "a caption can't close the data block");
        let link = format!(r#"Made with <a href="{}" target="_blank" rel="noopener">Rega</a>"#, crate::config::SITE_URL);
        assert!(html.contains(&link), "\"Made with Rega\" links to the website");

        let start = html.find("id=\"guide-data\">").unwrap() + "id=\"guide-data\">".len();
        let end = start + html[start..].find("</script>").unwrap();
        let data: serde_json::Value = serde_json::from_str(&html[start..end]).unwrap();
        let steps = data["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(data["images"].as_array().unwrap().len(), 3, "one full frame, one patch, one full frame");
        assert_eq!(steps[0]["layers"], serde_json::json!([[0, 0, 0, 640, 400]]));
        assert_eq!(steps[1]["layers"], serde_json::json!([[0, 0, 0, 640, 400], [1, 96, 96, 64, 32]]));
        assert_eq!(steps[2]["layers"], steps[1]["layers"], "same screen, same layers");
        assert_eq!(steps[3]["layers"], serde_json::json!([[2, 0, 0, 640, 400]]));
        assert_eq!(steps[3]["note"], "Done & dusted");
        assert_eq!(steps[0]["marker"], serde_json::json!([0.5, 0.25]));

        assert_eq!(data["style"]["marker"], "ring");
        assert_eq!(data["style"]["loop"], false);

        // Saved again after a caption edit, nothing is re-encoded.
        assert_eq!(cache.full.lock().unwrap().len(), 2);
        guide.steps[0].title = "Open the app".into();
        let again = page(&guide, &cache).unwrap();
        assert_eq!(again.len(), html.len() + "Open the app".len() - "Open".len());

        // A hidden marker isn't on the page; another quality re-encodes.
        guide.steps[0].marker_hidden = true;
        guide.style.quality = Quality::Lossless;
        let lossless = page(&guide, &cache).unwrap();
        assert!(!lossless.contains("image/jpeg"));
        assert!(lossless.contains(r#""marker":null"#));
    }

    /// Writes `target/guide-sample.html` from the README's screenshots, a
    /// typing step among them, and prints the sizes, for looking at the page:
    /// `cargo test sample_page -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sample_page() {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/images");
        let load = |name: &str| super::super::fit_capture(image::open(docs.join(name)).unwrap().to_rgba8(), Some(1920));
        let welcome = load("welcome.png");
        let mut typed = welcome.clone();
        paint(&mut typed, 600, 300, 220, 26, [255, 255, 255, 255]);
        paint(&mut typed, 606, 306, 120, 14, [30, 30, 30, 255]);
        let shots: Vec<(RgbaImage, &str, Option<[f32; 2]>, Action)> = vec![
            (welcome.clone(), "Open Rega", Some([0.3, 0.2]), Action::Click { button: super::super::Button::Left, count: 1 }),
            (typed, "Type “Quarterly report”", Some([0.5, 0.42]), Action::Type { text: "Quarterly report".into(), then: Some("Enter".into()) }),
            (load("editor.png"), "Mark up the capture", Some([0.62, 0.35]), Action::Click { button: super::super::Button::Left, count: 1 }),
            (load("export-menu.png"), "Choose Export", Some([0.8, 0.12]), Action::Click { button: super::super::Button::Left, count: 1 }),
            (load("settings.png"), "Press ⌘, to open Settings", None, Action::Key { combo: "⌘,".into() }),
        ];
        let mut steps = Vec::new();
        let mut raw = 0;
        for (img, title, marker, action) in shots {
            let shot = Shot::encode(&img).unwrap();
            raw += shot.png.len();
            steps.push(super::super::Step { shot, action, marker, marker_hidden: false, title: title.into(), note: String::new() });
        }
        steps[2].note = "Arrows, text, blur and numbered steps are on the right.\nEverything stays editable.".into();
        let mut guide = Guide::new("Capture and share a screenshot".into(), steps);
        // GUIDE_MARKER=hand (etc.) and GUIDE_THEME=light try the other looks.
        if let Ok(m) = std::env::var("GUIDE_MARKER") {
            guide.style.marker = serde_json::from_value(serde_json::Value::String(m)).unwrap();
        }
        if let Ok(t) = std::env::var("GUIDE_THEME") {
            guide.style.theme = serde_json::from_value(serde_json::Value::String(t)).unwrap();
        }
        let t = std::time::Instant::now();
        let html = page(&guide, &Cache::default()).unwrap();
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/guide-sample.html");
        std::fs::write(&out, &html).unwrap();
        eprintln!("page {} KB (lossless screenshots {} KB) in {:?} → {}", html.len() / 1024, raw / 1024, t.elapsed(), out.display());
    }

    /// The patch reproduces the frame exactly: layered as the page lays them,
    /// the pictures give back the step's screenshot.
    #[test]
    fn layers_rebuild_the_screenshot() {
        let a = screen(320, 200, 1);
        let mut b = a.clone();
        paint(&mut b, 10, 150, 50, 30, [9, 9, 9, 255]);
        paint(&mut b, 250, 20, 40, 40, [200, 9, 9, 255]);
        let rects = changed_rects(&a, &b).unwrap();
        let mut rebuilt = a.clone();
        for [x, y, w, h] in rects {
            let patch = image::imageops::crop_imm(&b, x, y, w, h).to_image();
            image::imageops::replace(&mut rebuilt, &patch, x as i64, y as i64);
        }
        assert_eq!(rebuilt, b);
    }
}
