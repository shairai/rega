//! In-app video preview. Rega's recordings (H.264 MP4 and GIF) are decoded on a
//! background thread; the UI only uploads the latest frame and draws controls.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui::{self, Align2, Color32, Pos2, Rect, Sense, TextureHandle, Vec2};
use image::RgbaImage;

use crate::platform;
use crate::theme::{self, icon};

/// One compressed MP4 sample, converted to Annex B so openh264 can take it.
struct Sample {
    time: f64,
    sync: bool,
    data: Vec<u8>,
}

enum Source {
    Mp4 { samples: Vec<Sample>, parameter_sets: Vec<u8> },
    Gif { frames: Vec<(f64, RgbaImage)> },
}

impl Source {
    fn len(&self) -> usize {
        match self {
            Source::Mp4 { samples, .. } => samples.len(),
            Source::Gif { frames } => frames.len(),
        }
    }

    fn time(&self, i: usize) -> f64 {
        match self {
            Source::Mp4 { samples, .. } => samples[i].time,
            Source::Gif { frames } => frames[i].0,
        }
    }
}

struct Media {
    source: Source,
    duration: f64,
}

fn open_media(path: &Path) -> Result<Media> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "gif" { open_gif(path) } else { open_mp4(path) }
}

fn open_gif(path: &Path) -> Result<Media> {
    use image::AnimationDecoder;
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let decoder = image::codecs::gif::GifDecoder::new(file)?;
    let mut t = 0.0;
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        let frame = frame?;
        let (num, den) = frame.delay().numer_denom_ms();
        let delay = num as f64 / den.max(1) as f64 / 1000.0;
        frames.push((t, frame.into_buffer()));
        t += delay.max(0.02);
    }
    anyhow::ensure!(!frames.is_empty(), "GIF has no frames");
    Ok(Media { source: Source::Gif { frames }, duration: t })
}

/// MP4 samples are length-prefixed NAL units; the decoder wants start codes.
fn avcc_to_annexb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        let end = (i + len).min(data.len());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..end]);
        i = end;
    }
    out
}

fn open_mp4(path: &Path) -> Result<Media> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).context("not a readable MP4")?;
    let track = reader
        .tracks()
        .values()
        .find(|t| matches!(t.track_type(), Ok(mp4::TrackType::Video)))
        .context("no video track")?;
    let (id, timescale) = (track.track_id(), track.timescale().max(1) as f64);
    let mut parameter_sets = Vec::new();
    for set in [track.sequence_parameter_set()?, track.picture_parameter_set()?] {
        parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
        parameter_sets.extend_from_slice(set);
    }
    let mut samples = Vec::new();
    let mut duration = 0.0;
    for n in 1..=reader.sample_count(id)? {
        let Some(s) = reader.read_sample(id, n)? else { continue };
        let time = s.start_time as f64 / timescale;
        duration = time + s.duration as f64 / timescale;
        samples.push(Sample { time, sync: s.is_sync, data: avcc_to_annexb(&s.bytes) });
    }
    anyhow::ensure!(!samples.is_empty(), "video has no frames");
    Ok(Media { source: Source::Mp4 { samples, parameter_sets }, duration })
}

/// Turns sample indices into pictures, decoding forward from a keyframe when
/// asked for a frame out of order.
struct Decoder {
    h264: Option<openh264::decoder::Decoder>,
    /// Index of the last sample fed to `h264`.
    last: Option<usize>,
}

impl Decoder {
    fn new() -> Self {
        Self { h264: None, last: None }
    }

    fn frame(&mut self, source: &Source, i: usize) -> Result<Option<RgbaImage>> {
        let (samples, parameter_sets) = match source {
            Source::Gif { frames } => return Ok(Some(frames[i].1.clone())),
            Source::Mp4 { samples, parameter_sets } => (samples, parameter_sets),
        };
        // Continue from where we are if that's on the way; else restart at the
        // keyframe at or before `i`.
        let keyframe = (0..=i).rev().find(|&k| samples[k].sync).unwrap_or(0);
        let start = match self.last {
            Some(last) if last < i && last >= keyframe && self.h264.is_some() => last + 1,
            _ => {
                let mut dec = openh264::decoder::Decoder::new()?;
                dec.decode(parameter_sets)?;
                self.h264 = Some(dec);
                keyframe
            }
        };
        let dec = self.h264.as_mut().expect("decoder");
        let mut picture = None;
        for (k, sample) in samples.iter().enumerate().take(i + 1).skip(start) {
            let decoded = dec.decode(&sample.data)?;
            self.last = Some(k);
            if k == i
                && let Some(yuv) = decoded
            {
                use openh264::formats::YUVSource;
                let (w, h) = yuv.dimensions();
                let mut rgba = vec![0; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                picture = RgbaImage::from_raw(w as u32, h as u32, rgba);
            }
        }
        Ok(picture)
    }
}

/// First frame of a video, for library thumbnails.
pub fn first_frame(path: &Path) -> Result<RgbaImage> {
    let media = open_media(path)?;
    Decoder::new().frame(&media.source, 0)?.context("could not decode the first frame")
}

/// Write the part of `src` between `start` and `end` (seconds) to `dst`,
/// re-encoded in the same format. The original is left untouched.
pub fn export_trimmed(src: &Path, dst: &Path, start: f64, end: f64) -> Result<()> {
    use std::sync::atomic::AtomicBool;

    use crate::recorder::{TimedFrame, encode_gif, encode_mp4};

    let media = open_media(src)?;
    let source = &media.source;
    let n = source.len();
    // The frame on screen at `start`, through the last one starting before `end`.
    let first = (0..n).rev().find(|&i| source.time(i) <= start).unwrap_or(0);
    let picked: Vec<usize> = (first..n).take_while(|&i| i == first || source.time(i) < end).collect();
    let fps = ((n as f64 / media.duration.max(0.001)).round() as u32).clamp(1, 60);
    let is_gif = matches!(source, Source::Gif { .. });
    let (tx, rx) = mpsc::channel::<TimedFrame>();
    let dst = dst.to_path_buf();
    std::thread::scope(|scope| {
        let decoding = scope.spawn(move || -> Result<()> {
            let mut decoder = Decoder::new();
            for i in picked {
                let Some(image) = decoder.frame(source, i)? else { continue };
                let at = Duration::from_secs_f64((source.time(i) - start).max(0.0));
                if tx.send(TimedFrame { image, at }).is_err() {
                    break;
                }
            }
            Ok(())
        });
        let cancel = AtomicBool::new(false);
        let encoded = if is_gif { encode_gif(rx, &dst, &cancel) } else { encode_mp4(rx, fps, &dst, &cancel) };
        decoding.join().unwrap_or_else(|_| Err(anyhow::anyhow!("decoder crashed")))?;
        encoded
    })
}

enum Command {
    Play,
    Pause,
    Seek(f64),
}

#[derive(Default)]
struct Shared {
    /// A newly decoded frame the UI hasn't uploaded yet.
    frame: Option<RgbaImage>,
    position: f64,
    playing: bool,
    error: Option<String>,
}

fn playback(media: Media, rx: Receiver<Command>, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let source = &media.source;
    let n = source.len();
    let mut decoder = Decoder::new();
    let publish = |decoder: &mut Decoder, i: usize, playing: bool| {
        let frame = decoder.frame(source, i);
        let mut s = shared.lock().expect("player state");
        match frame {
            Ok(Some(img)) => s.frame = Some(img),
            Ok(None) => {}
            Err(e) => s.error = Some(format!("{e:#}")),
        }
        s.position = source.time(i);
        s.playing = playing;
        drop(s);
        ctx.request_repaint();
    };
    let index_at = |t: f64| (0..n).rev().find(|&i| source.time(i) <= t).unwrap_or(0);

    let mut playing = false;
    // Wall-clock instant at which media time 0 would have played.
    let mut clock = Instant::now();
    publish(&mut decoder, 0, false);
    let mut next = 1;

    loop {
        let command = if playing {
            let due = clock + Duration::from_secs_f64(source.time(next.min(n - 1)));
            match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };
        // While scrubbing, only the latest seek matters.
        let command = command.map(|first| rx.try_iter().fold(first, |_, c| c));
        match command {
            Some(Command::Play) => {
                if next >= n {
                    publish(&mut decoder, 0, true);
                    next = 1;
                }
                playing = true;
                clock = Instant::now() - Duration::from_secs_f64(source.time(next.saturating_sub(1)));
                shared.lock().expect("player state").playing = true;
            }
            Some(Command::Pause) => {
                playing = false;
                shared.lock().expect("player state").playing = false;
            }
            Some(Command::Seek(t)) => {
                let i = index_at(t);
                publish(&mut decoder, i, playing);
                next = i + 1;
                clock = Instant::now() - Duration::from_secs_f64(source.time(i));
            }
            None => {
                if next >= n {
                    playing = false;
                    let mut s = shared.lock().expect("player state");
                    s.playing = false;
                    s.position = media.duration;
                    drop(s);
                    ctx.request_repaint();
                    continue;
                }
                publish(&mut decoder, next, true);
                next += 1;
            }
        }
    }
}

/// Something the app should react to.
pub enum PlayerAction {
    None,
    /// A trimmed copy was written here.
    Saved(PathBuf),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Handle {
    In,
    Out,
}

pub struct Player {
    pub path: PathBuf,
    ctx: egui::Context,
    duration: f64,
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Command>,
    texture: Option<TextureHandle>,
    /// Where the scrubber is being dragged to, if it is.
    scrubbing: Option<f64>,
    /// Trim range (in, out) in seconds, while trimming.
    trim: Option<(f64, f64)>,
    dragging_handle: Option<Handle>,
    /// A trimmed copy being written in the background.
    saving: Option<Receiver<Result<PathBuf>>>,
}

impl Player {
    pub fn open(ctx: &egui::Context, path: PathBuf) -> Result<Self> {
        let media = open_media(&path)?;
        let duration = media.duration;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        let (s, c) = (shared.clone(), ctx.clone());
        std::thread::spawn(move || playback(media, rx, s, c));
        let player = Self {
            path,
            ctx: ctx.clone(),
            duration,
            shared,
            tx,
            texture: None,
            scrubbing: None,
            trim: None,
            dragging_handle: None,
            saving: None,
        };
        player.send(Command::Play);
        Ok(player)
    }

    fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> PlayerAction {
        let ctx = ui.ctx().clone();
        let mut action = PlayerAction::None;
        if let Some(rx) = &self.saving
            && let Ok(result) = rx.try_recv()
        {
            self.saving = None;
            action = match result {
                Ok(path) => {
                    self.trim = None;
                    PlayerAction::Saved(path)
                }
                Err(e) => PlayerAction::Failed(format!("Trim failed: {e:#}")),
            };
        }
        let (position, playing, error) = {
            let mut s = self.shared.lock().expect("player state");
            if let Some(img) = s.frame.take() {
                let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                match &mut self.texture {
                    Some(t) => t.set(color, egui::TextureOptions::LINEAR),
                    None => self.texture = Some(ctx.load_texture("video-frame", color, egui::TextureOptions::LINEAR)),
                }
            }
            (s.position, s.playing, s.error.clone())
        };

        let keys_free = ctx.memory(|m| m.focused().is_none());
        if keys_free && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.send(if playing { Command::Pause } else { Command::Play });
        }
        // I / O set the trim points at the playhead (starting a trim if needed).
        if keys_free && self.saving.is_none() {
            let (set_in, set_out) = ctx.input(|i| (i.key_pressed(egui::Key::I), i.key_pressed(egui::Key::O)));
            if set_in || set_out {
                let (a, b) = self.trim.unwrap_or((0.0, self.duration));
                self.trim = Some(if set_in { (position.min(b - MIN_TRIM), b) } else { (a, position.max(a + MIN_TRIM)) });
            }
            if self.trim.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.trim = None;
            }
        }
        // While trimming, playback stays inside the selection.
        if let Some((a, b)) = self.trim
            && playing
            && self.scrubbing.is_none()
            && (position >= b || position < a - 0.05)
        {
            self.send(Command::Pause);
            self.send(Command::Seek(a));
        }

        let avail = ui.available_rect_before_wrap();
        let controls_h = 64.0;
        let stage = Rect::from_min_max(avail.min + Vec2::splat(24.0), Pos2::new(avail.max.x - 24.0, avail.max.y - controls_h - 8.0));
        let painter = ui.painter_at(avail);
        match (&self.texture, &error) {
            (_, Some(e)) => {
                painter.text(stage.center(), Align2::CENTER_CENTER, format!("Can't play this video: {e}"), theme::medium(14.0), theme::TEXT_MUTED);
            }
            (Some(tex), None) => {
                let size = tex.size_vec2();
                // Fit, but never blow a pixel up beyond one point.
                let scale = (stage.width() / size.x).min(stage.height() / size.y).min(1.0);
                let rect = Rect::from_center_size(stage.center(), size * scale);
                painter.rect_filled(rect.expand(1.0), 6.0, Color32::BLACK);
                painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            (None, None) => {
                painter.text(stage.center(), Align2::CENTER_CENTER, "Loading…", theme::medium(14.0), theme::TEXT_FAINT);
            }
        }

        let bar = Rect::from_min_max(Pos2::new(avail.min.x + 24.0, avail.max.y - controls_h), Pos2::new(avail.max.x - 24.0, avail.max.y - 8.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
            ui.horizontal_centered(|ui| {
                let (glyph, tip) = if playing { (icon::PAUSE, "Pause   Space") } else { (icon::PLAY, "Play   Space") };
                if theme::icon_button(ui, glyph, tip).clicked() {
                    self.send(if playing { Command::Pause } else { Command::Play });
                }
                let shown = self.scrubbing.unwrap_or(position);
                ui.label(egui::RichText::new(format!("{} / {}", clock(shown), clock(self.duration))).monospace().color(theme::TEXT_MUTED));
                ui.add_space(8.0);

                // Buttons on the right, scrubber fills what's left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match self.trim {
                        Some((a, b)) => {
                            if self.saving.is_some() {
                                ui.add(egui::Spinner::new());
                                ui.label(egui::RichText::new("Saving…").color(theme::TEXT_MUTED));
                            } else {
                                if theme::primary_button(ui, Some(icon::SAVE), "Save trimmed copy").clicked() {
                                    self.save_trimmed(a, b);
                                }
                                if theme::secondary_button(ui, None, "Cancel").clicked() {
                                    self.trim = None;
                                }
                            }
                            ui.label(
                                egui::RichText::new(format!("{} – {}  ({})", clock(a), clock(b), clock(b - a)))
                                    .monospace()
                                    .color(theme::TEXT),
                            );
                        }
                        None => {
                            if theme::icon_button(ui, icon::FOLDER, if cfg!(target_os = "macos") { "Reveal in Finder" } else { "Show in folder" }).clicked() {
                                platform::reveal_path(&self.path);
                            }
                            if theme::icon_button(ui, icon::EXPORT, "Open in default player").clicked() {
                                self.send(Command::Pause);
                                platform::open_path(&self.path);
                            }
                            if theme::icon_button(ui, icon::CROP, "Trim   I / O").clicked() {
                                self.trim = Some((0.0, self.duration));
                            }
                        }
                    }
                    ui.add_space(8.0);
                    self.scrubber(ui, shown);
                });
            });
        });
        action
    }

    fn save_trimmed(&mut self, start: f64, end: f64) {
        self.send(Command::Pause);
        let src = self.path.clone();
        let ext = src.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
        let dst = crate::library::new_capture_path(src.parent().unwrap_or(Path::new(".")), &ext);
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(export_trimmed(&src, &dst, start, end).map(|()| dst));
            ctx.request_repaint();
        });
        self.saving = Some(rx);
    }

    fn scrubber(&mut self, ui: &mut egui::Ui, position: f64) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click_and_drag());
        let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), 4.0));
        let frac = if self.duration > 0.0 { (position / self.duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let x_at = |t: f64| track.min.x + track.width() * (t / self.duration.max(0.001)).clamp(0.0, 1.0) as f32;
        let p = ui.painter();
        p.rect_filled(track, 2.0, theme::SURFACE_3);
        match self.trim {
            Some((a, b)) => {
                // The kept part in the brand gradient, handles at both ends.
                let kept = Rect::from_min_max(Pos2::new(x_at(a), track.min.y), Pos2::new(x_at(b), track.max.y));
                theme::gradient_rect(p, kept.expand2(Vec2::new(0.0, 2.0)), 2, Color32::WHITE);
                for (t, color) in [(a, crate::icon::LOGO_FROM), (b, crate::icon::LOGO_TO)] {
                    let handle = Rect::from_center_size(Pos2::new(x_at(t), track.center().y), Vec2::new(6.0, 22.0));
                    p.rect_filled(handle, 3.0, Color32::from_rgb(color[0], color[1], color[2]));
                    p.rect_stroke(handle, 3.0, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Outside);
                }
                p.line_segment(
                    [Pos2::new(x_at(position), rect.min.y), Pos2::new(x_at(position), rect.max.y)],
                    egui::Stroke::new(2.0, Color32::WHITE),
                );
            }
            None => {
                let done = Rect::from_min_max(track.min, Pos2::new(track.min.x + track.width() * frac, track.max.y));
                theme::gradient_rect(p, done, 2, Color32::WHITE);
                let knob = Pos2::new(done.max.x, track.center().y);
                p.circle_filled(knob, if resp.hovered() || resp.dragged() { 7.0 } else { 5.0 }, Color32::WHITE);
            }
        }

        let t_at = |x: f32| ((x - track.min.x) / track.width()).clamp(0.0, 1.0) as f64 * self.duration;
        // Grabbing near a trim handle moves that handle instead of seeking.
        if resp.drag_started()
            && let (Some((a, b)), Some(pos)) = (self.trim, resp.interact_pointer_pos())
        {
            let (da, db) = ((pos.x - x_at(a)).abs(), (pos.x - x_at(b)).abs());
            self.dragging_handle = match (da.min(db) < 10.0, da <= db) {
                (true, true) => Some(Handle::In),
                (true, false) => Some(Handle::Out),
                _ => None,
            };
        }
        if let (Some(handle), Some((a, b)), Some(pos)) = (self.dragging_handle, self.trim, resp.interact_pointer_pos()) {
            let t = t_at(pos.x);
            let t = match handle {
                Handle::In => t.min(b - MIN_TRIM).max(0.0),
                Handle::Out => t.max(a + MIN_TRIM).min(self.duration),
            };
            self.trim = Some(if handle == Handle::In { (t, b) } else { (a, t) });
            if self.scrubbing.is_none_or(|s| (s - t).abs() > 0.02) {
                self.send(Command::Seek(t));
            }
            self.scrubbing = Some(t);
            if resp.drag_stopped() {
                self.dragging_handle = None;
                self.scrubbing = None;
            }
            return;
        }

        if let Some(pos) = resp.interact_pointer_pos()
            && (resp.dragged() || resp.clicked() || resp.drag_started())
        {
            let t = t_at(pos.x);
            if self.scrubbing.is_none_or(|s| (s - t).abs() > 0.02) {
                self.send(Command::Seek(t));
            }
            self.scrubbing = Some(t);
        }
        if resp.drag_stopped() || (resp.clicked() && !resp.dragged()) {
            self.scrubbing = None;
        }
    }
}

/// Shortest trim the handles allow, in seconds.
const MIN_TRIM: f64 = 0.2;

fn clock(t: f64) -> String {
    let s = t.max(0.0).round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_recordings_and_seeks() {
        let dir = std::env::temp_dir().join(format!("rega-player-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("t.mp4");
        crate::recorder::write_test_video(&mp4, 30, 10, (64, 48));
        let media = open_mp4(&mp4).unwrap();
        assert_eq!(media.source.len(), 30);
        assert!((media.duration - 3.0).abs() < 0.2, "duration {}", media.duration);

        let mut dec = Decoder::new();
        let first = dec.frame(&media.source, 0).unwrap().expect("frame 0");
        assert_eq!(first.dimensions(), (64, 48));
        // In order, then a jump back and a jump forward (both need re-decoding).
        for i in [1, 2, 25, 3, 29] {
            assert!(dec.frame(&media.source, i).unwrap().is_some(), "frame {i}");
        }
        assert!(first_frame(&mp4).is_ok());

        // Trim 1.0–2.0 s of the 3 s video.
        let cut = dir.join("cut.mp4");
        export_trimmed(&mp4, &cut, 1.0, 2.0).unwrap();
        let trimmed = open_mp4(&cut).unwrap();
        assert!((trimmed.duration - 1.0).abs() < 0.2, "trimmed duration {}", trimmed.duration);

        let gif = dir.join("t.gif");
        crate::recorder::write_test_gif(&gif, 4);
        let media = open_gif(&gif).unwrap();
        assert_eq!(media.source.len(), 4);
    }
}
