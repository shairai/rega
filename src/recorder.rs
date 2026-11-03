//! Screen video recording. A capture thread grabs the region at the target
//! frame rate and hands frames to an encoder thread (H.264/MP4 or GIF).

use std::cell::RefCell;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::RgbaImage;

use crate::config::VideoFormat;

pub struct RecordConfig {
    /// Monitor top-left in global logical points.
    pub monitor_origin: (i32, i32),
    /// Region in monitor-local logical points.
    pub region: (u32, u32, u32, u32),
    pub fps: u32,
    pub format: VideoFormat,
    pub max_width: u32,
    /// Palette size when `format` is GIF.
    pub gif_colors: usize,
    pub highlight_cursor: bool,
    pub out_path: PathBuf,
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    started: Instant,
    paused_total: Duration,
    pause_started: Option<Instant>,
    handle: Option<JoinHandle<Result<PathBuf>>>,
}

pub(crate) struct TimedFrame {
    pub(crate) image: RgbaImage,
    /// Recording time (pauses excluded) at which this frame was captured.
    pub(crate) at: Duration,
}

impl Recorder {
    pub fn start(cfg: RecordConfig) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let origin = cfg.monitor_origin;
        // Fail fast here; the capture thread looks the monitor up again because
        // `Monitor` holds a raw handle that is not `Send` on Windows.
        crate::capture::monitor_with_origin(origin).context("recording monitor not found")?;

        // Bounded: when the encoder falls behind (GIF quantisation, slow H.264), drop
        // frames instead of queueing ~8 MB each until memory runs out. MP4 fills the
        // gaps by timestamp and GIF stretches the previous frame's delay.
        let (tx, rx) = mpsc::sync_channel::<TimedFrame>(4);
        let (stop_c, paused_c) = (stop.clone(), paused.clone());
        let fps = cfg.fps.clamp(1, 60);
        let (rx_, ry, rw, rh) = cfg.region;
        let highlight = cfg.highlight_cursor;
        let max_width = cfg.max_width.max(320);

        let capture = thread::spawn(move || -> Result<()> {
            let monitor = crate::capture::monitor_with_origin(origin).context("recording monitor not found")?;
            // The region is in logical points; xcap wants its own units.
            let d = crate::capture::dpi(&monitor);
            let phys = |v: u32| (v as f32 * d).round() as u32;
            let (px, py, pw, ph) = (phys(rx_), phys(ry), phys(rw).max(2), phys(rh).max(2));
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            let mut rec_time = Duration::ZERO;
            let mut last = Instant::now();
            let mut target: Option<(u32, u32)> = None;
            let mut failures = 0;
            while !stop_c.load(Ordering::Relaxed) {
                let tick = Instant::now();
                if paused_c.load(Ordering::Relaxed) {
                    last = Instant::now();
                    thread::sleep(Duration::from_millis(30));
                    continue;
                }
                rec_time += tick - last;
                last = tick;

                // Ride out transient failures (display sleep, mode switch) for ~2 s.
                let mut img = match monitor.capture_region(px, py, pw, ph) {
                    Ok(img) => {
                        failures = 0;
                        img
                    }
                    Err(e) if failures < 2 * fps => {
                        failures += 1;
                        log::warn!("frame capture failed: {e}");
                        thread::sleep(interval);
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                let scale = img.width() as f32 / rw as f32;
                if highlight {
                    let (cx, cy) = crate::capture::to_logical(crate::capture::cursor_position());
                    let lx = (cx - origin.0 - rx_ as i32) as f32 * scale;
                    let ly = (cy - origin.1 - ry as i32) as f32 * scale;
                    draw_cursor_halo(&mut img, lx, ly, 18.0 * scale);
                }
                let (tw, th) = *target.get_or_insert_with(|| even_fit(img.width(), img.height(), max_width));
                if img.width() != tw || img.height() != th {
                    img = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
                }
                match tx.try_send(TimedFrame { image: img, at: rec_time }) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
                if let Some(rest) = interval.checked_sub(tick.elapsed()) {
                    thread::sleep(rest);
                }
            }
            Ok(())
        });

        let cancel_c = cancel.clone();
        let format = cfg.format;
        let gif_colors = cfg.gif_colors;
        let out_path = cfg.out_path;
        let handle = thread::spawn(move || -> Result<PathBuf> {
            let encoded = match format {
                VideoFormat::Mp4 => encode_mp4(rx, fps, &out_path, &cancel_c),
                VideoFormat::Gif => encode_gif(rx, &out_path, &cancel_c, gif_colors, 0),
            };
            let captured = capture.join().unwrap_or_else(|_| Err(anyhow::anyhow!("capture thread panicked")));
            if cancel_c.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&out_path);
                anyhow::bail!("Recording discarded");
            }
            match (captured, encoded) {
                // The file is complete up to the failure; keep it rather than hide it.
                (Err(e), Ok(())) => {
                    log::warn!("recording ended early: {e:#}");
                    Ok(out_path)
                }
                (Err(e), Err(_)) => Err(e),
                (Ok(()), encoded) => encoded.map(|()| out_path),
            }
        });

        Ok(Self {
            stop,
            cancel,
            paused,
            started: Instant::now(),
            paused_total: Duration::ZERO,
            pause_started: None,
            handle: Some(handle),
        })
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn toggle_pause(&mut self) {
        if let Some(since) = self.pause_started.take() {
            self.paused_total += since.elapsed();
            self.paused.store(false, Ordering::Relaxed);
        } else {
            self.pause_started = Some(Instant::now());
            self.paused.store(true, Ordering::Relaxed);
        }
    }

    pub fn elapsed(&self) -> Duration {
        let paused_now = self.pause_started.map(|s| s.elapsed()).unwrap_or_default();
        self.started.elapsed() - self.paused_total - paused_now
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn discard(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.stop();
    }

    /// Stop and block until the file is finalized (used on quit), giving up after `timeout`.
    pub fn finish_blocking(&mut self, timeout: Duration) -> Option<Result<PathBuf>> {
        self.stop();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(r) = self.poll_finished() {
                return Some(r);
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Returns the result once encoding has finished.
    pub fn poll_finished(&mut self) -> Option<Result<PathBuf>> {
        if self.handle.as_ref()?.is_finished() {
            let handle = self.handle.take()?;
            Some(handle.join().unwrap_or_else(|_| Err(anyhow::anyhow!("encoder panicked"))))
        } else {
            None
        }
    }
}

/// Fit within `max_width`, keeping both sides even (required by H.264 4:2:0).
fn even_fit(w: u32, h: u32, max_width: u32) -> (u32, u32) {
    let (w, h) = if w > max_width {
        (max_width, (h as f64 * max_width as f64 / w as f64).round() as u32)
    } else {
        (w, h)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

fn draw_cursor_halo(img: &mut RgbaImage, cx: f32, cy: f32, r: f32) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let r2 = r * r;
    for y in ((cy - r) as i32).max(0)..((cy + r) as i32 + 1).min(h) {
        for x in ((cx - r) as i32).max(0)..((cx + r) as i32 + 1).min(w) {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            if dx * dx + dy * dy <= r2 {
                let p = img.get_pixel_mut(x as u32, y as u32);
                // 35% yellow tint
                p[0] = (p[0] as f32 * 0.65 + 255.0 * 0.35) as u8;
                p[1] = (p[1] as f32 * 0.65 + 214.0 * 0.35) as u8;
                p[2] = (p[2] as f32 * 0.65) as u8;
            }
        }
    }
}

pub(crate) fn encode_mp4(rx: Receiver<TimedFrame>, fps: u32, path: &PathBuf, cancel: &AtomicBool) -> Result<()> {
    use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, UsageType};
    use openh264::formats::{RgbaSliceU8, YUVBuffer};

    // Created on the first frame. Frames are muxed to disk as they are encoded, so
    // memory stays flat however long the recording runs.
    let mut state: Option<(Encoder, Box<minimp4::Mp4Muxer<MuxWriter>>, YUVBuffer)> = None;
    let io_err = Rc::new(RefCell::new(None));
    let mut nal = Vec::new();
    let mut written: u64 = 0;

    for frame in rx.iter() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (w, h) = (frame.image.width() as usize, frame.image.height() as usize);
        let (enc, muxer, yuv) = match &mut state {
            Some((e, m, y)) => (e, m, y),
            None => {
                let file = std::fs::File::create(path)?;
                // Boxed before `init_video`: minimp4 keeps a pointer to the muxer.
                let mut muxer = Box::new(minimp4::Mp4Muxer::new(MuxWriter { file, err: io_err.clone() }));
                muxer.init_video(w as i32, h as i32, false, "Rega recording");
                let bps = ((w * h) as f64 * fps as f64 * 0.15).clamp(1_000_000.0, 20_000_000.0) as u32;
                let config = EncoderConfig::new()
                    .max_frame_rate(FrameRate::from_hz(fps as f32))
                    .bitrate(BitRate::from_bps(bps))
                    .usage_type(UsageType::ScreenContentRealTime)
                    // A keyframe every 2 s keeps seeking in the in-app player fast.
                    .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2));
                let enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?;
                let (e, m, y) = state.insert((enc, muxer, YUVBuffer::new(w, h)));
                (e, m, y)
            }
        };
        // Keep wall-clock timing exact: duplicate frames when capture fell behind.
        let due = (frame.at.as_secs_f64() * fps as f64).floor() as u64 + 1;
        if due <= written {
            continue;
        }
        yuv.read_rgb(RgbaSliceU8::new(frame.image.as_raw(), (w, h)));
        while written < due {
            nal.clear();
            enc.encode(&*yuv)?.write_vec(&mut nal);
            // minimp4's NAL splitter underflows on chunks shorter than a start code.
            if nal.len() >= 4 {
                muxer.write_video_with_fps(&nal, fps);
            }
            written += 1;
        }
    }

    let Some((_, muxer, _)) = state else { anyhow::bail!("no frames were captured") };
    muxer.close();
    if let Some(e) = io_err.borrow_mut().take() {
        return Err(e).context("writing the video file");
    }
    Ok(())
}

/// File sink for minimp4 that never reports failure: its C write callback `unwrap`s
/// seeks, and a panic across `extern "C"` aborts the whole app (e.g. on a full disk).
/// The first error is kept for `encode_mp4` to return instead.
struct MuxWriter {
    file: std::fs::File,
    err: Rc<RefCell<Option<std::io::Error>>>,
}

impl MuxWriter {
    fn record(&self, e: std::io::Error) {
        self.err.borrow_mut().get_or_insert(e);
    }
}

impl Write for MuxWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.err.borrow().is_none()
            && let Err(e) = self.file.write_all(buf)
        {
            self.record(e);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for MuxWriter {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self.file.seek(pos) {
            Ok(p) => Ok(p),
            Err(e) => {
                self.record(e);
                Ok(0)
            }
        }
    }
}

/// Encode frames as an animated GIF with at most `colors` palette entries.
///
/// Screen recordings are mostly still, so after the first frame each frame
/// stores only the rectangle that changed, with unchanged pixels inside it left
/// transparent (shown through from the previous frame), and frames identical to
/// the previous one are dropped by stretching that one's delay. A pixel counts
/// as unchanged when no channel moved by more than `tolerance`: 0 for exact
/// screen captures, a few levels for decoded video and its compression noise.
pub(crate) fn encode_gif(rx: Receiver<TimedFrame>, path: &PathBuf, cancel: &AtomicBool, colors: usize, tolerance: u8) -> Result<()> {
    let mut encoder: Option<gif::Encoder<std::io::BufWriter<std::fs::File>>> = None;
    // What a viewer shows after the frames written so far.
    let mut shown: Option<RgbaImage> = None;
    let mut pending: Option<TimedFrame> = None;
    // Delays from rounded timestamps, so rounding doesn't add up to drift.
    let centis = |d: Duration| (d.as_secs_f64() * 100.0).round() as u64;

    let mut write = |f: TimedFrame, until: u64| -> Result<()> {
        let (w, h) = (f.image.width(), f.image.height());
        let enc = match &mut encoder {
            Some(e) => e,
            None => {
                let file = std::io::BufWriter::new(std::fs::File::create(path)?);
                let mut e = gif::Encoder::new(file, w as u16, h as u16, &[])?;
                e.set_repeat(gif::Repeat::Infinite)?;
                encoder.insert(e)
            }
        };
        let delay = until.saturating_sub(centis(f.at)).clamp(2, u16::MAX as u64) as u16;
        let mut gf = match &mut shown {
            None => {
                let gf = quantize(f.image.as_raw(), w, h, colors, None);
                shown = Some(f.image);
                gf
            }
            Some(prev) => {
                let (x0, y0, x1, y1) = changed_rect(prev, &f.image, tolerance).unwrap_or((0, 0, 1, 1));
                // A scroll or scene change touches most of the frame: store it whole
                // and exact, so no low-contrast detail within `tolerance` is left stale.
                if tolerance > 0 && (x1 - x0) as u64 * (y1 - y0) as u64 * 2 > w as u64 * h as u64 {
                    let changed = f.image.pixels().zip(prev.pixels()).filter(|(a, b)| !same(&a.0, &b.0, tolerance)).count() as u64;
                    if changed * 4 > w as u64 * h as u64 {
                        let mut gf = quantize(f.image.as_raw(), w, h, colors, None);
                        gf.delay = delay;
                        *prev = f.image;
                        enc.write_frame(&gf)?;
                        return Ok(());
                    }
                }
                let (cw, ch) = (x1 - x0, y1 - y0);
                let mut patch = Vec::with_capacity((cw * ch * 4) as usize);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = *f.image.get_pixel(x, y);
                        let old = prev.get_pixel_mut(x, y);
                        if same(&old.0, &p.0, tolerance) {
                            patch.extend_from_slice(&[0, 0, 0, 0]);
                        } else {
                            // Track exactly what the viewer now shows, so skipped
                            // small differences can't add up.
                            *old = p;
                            patch.extend_from_slice(&p.0);
                        }
                    }
                }
                quantize(&patch, cw, ch, colors, Some((x0, y0)))
            }
        };
        gf.delay = delay;
        enc.write_frame(&gf)?;
        Ok(())
    };

    for frame in rx.iter() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(prev) = pending.take() {
            if tolerance == 0 && prev.image == frame.image {
                // Nothing changed: keep showing `prev` for longer.
                pending = Some(prev);
                continue;
            }
            write(prev, centis(frame.at))?;
        }
        pending = Some(frame);
    }
    match pending {
        Some(last) => {
            let end = centis(last.at) + 50;
            write(last, end)
        }
        None => anyhow::bail!("no frames were captured"),
    }
}

fn same(a: &[u8], b: &[u8], tolerance: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= tolerance)
}

/// Bounding box `(x0, y0, x1, y1)` (exclusive) of the pixels that differ by
/// more than `tolerance` in some channel.
fn changed_rect(a: &RgbaImage, b: &RgbaImage, tolerance: u8) -> Option<(u32, u32, u32, u32)> {
    let w = a.width() as usize;
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for (y, (ra, rb)) in a.as_raw().chunks_exact(w * 4).zip(b.as_raw().chunks_exact(w * 4)).enumerate() {
        if ra == rb {
            continue;
        }
        let differs = |(p, q): (&[u8], &[u8])| !same(p, q, tolerance);
        let Some(first) = ra.chunks_exact(4).zip(rb.chunks_exact(4)).position(differs) else { continue };
        let last = ra.chunks_exact(4).zip(rb.chunks_exact(4)).rposition(differs).unwrap_or(first);
        x0 = x0.min(first);
        x1 = x1.max(last + 1);
        y0 = y0.min(y);
        y1 = y + 1;
    }
    (y1 > 0).then_some((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
}

/// An indexed GIF frame from RGBA pixels with at most `colors` palette entries;
/// fully transparent pixels get their own transparent index.
fn quantize(rgba: &[u8], w: u32, h: u32, colors: usize, at: Option<(u32, u32)>) -> gif::Frame<'static> {
    let opaque: Vec<u8> = rgba.chunks_exact(4).filter(|p| p[3] != 0).flatten().copied().collect();
    let has_clear = opaque.len() < rgba.len();
    let mut frame = gif::Frame { width: w as u16, height: h as u16, ..Default::default() };
    if let Some((x, y)) = at {
        frame.left = x as u16;
        frame.top = y as u16;
    }
    if opaque.is_empty() {
        // Nothing changed in this patch: one transparent pixel colour.
        frame.palette = Some(vec![0, 0, 0]);
        frame.transparent = Some(0);
        frame.buffer = vec![0; (w * h) as usize].into();
        return frame;
    }
    let slots = if has_clear { colors - 1 } else { colors }.clamp(2, 256);
    // Sample factor 10: NeuQuant's usual speed/quality balance.
    let nq = color_quant::NeuQuant::new(10, slots, &opaque);
    let mut palette = nq.color_map_rgb();
    let clear = (palette.len() / 3) as u8;
    let indices: Vec<u8> = rgba.chunks_exact(4).map(|p| if p[3] == 0 { clear } else { nq.index_of(p) as u8 }).collect();
    if has_clear {
        palette.extend_from_slice(&[0, 0, 0]);
        frame.transparent = Some(clear);
    }
    // Keep earlier frames under the transparent pixels.
    frame.dispose = gif::DisposalMethod::Keep;
    frame.palette = Some(palette);
    frame.buffer = indices.into();
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn frames_sized(n: u32, w: u32, h: u32) -> Receiver<TimedFrame> {
        let (tx, rx) = mpsc::channel();
        for i in 0..n {
            let image = RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x + i * 8) as u8, y as u8, 128, 255]));
            tx.send(TimedFrame { image, at: Duration::from_millis(i as u64 * 100) }).unwrap();
        }
        rx
    }

    fn frames(n: u32) -> Receiver<TimedFrame> {
        let (tx, rx) = mpsc::channel();
        for i in 0..n {
            let image = RgbaImage::from_fn(320, 240, |x, y| image::Rgba([(x + i * 8) as u8, y as u8, 128, 255]));
            tx.send(TimedFrame { image, at: Duration::from_millis(i as u64 * 100) }).unwrap();
        }
        rx
    }

    /// GIF sizes for a simulated screen recording: the old full-frame encoding
    /// vs the change-only encoder at each preset. `cargo test --release gif_sizes -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints sizes"]
    fn gif_sizes() {
        use crate::config::GifQuality;
        let ui = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/images/editor.png")).unwrap().to_rgba8();
        let screen = |w: u32, scroll: u32| {
            let h = w * 10 / 16;
            let base = image::imageops::resize(&ui, w, ui.height() * w / ui.width(), image::imageops::FilterType::Triangle);
            image::imageops::crop_imm(&base, 0, scroll.min(base.height() - h), w, h).to_image()
        };
        // 6 s at 15 fps: a cursor-sized block moving, one scroll halfway.
        let frames = |w: u32, fps: u32| {
            let (tx, rx) = mpsc::channel();
            let n = 6 * fps;
            for i in 0..n {
                let mut img = screen(w, if i < n / 2 { 0 } else { w / 12 });
                let (x, y) = (w / 4 + i * w / (2 * n), w / 5);
                for dy in 0..(w / 60) {
                    for dx in 0..(w / 60) {
                        img.put_pixel(x + dx, y + dy, image::Rgba([255, 214, 0, 255]));
                    }
                }
                tx.send(TimedFrame { image: img, at: Duration::from_secs_f64(i as f64 / fps as f64) }).unwrap();
            }
            rx
        };
        let dir = std::env::temp_dir().join(format!("rega-gif-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let size = |p: &std::path::Path| std::fs::metadata(p).unwrap().len() / 1024;

        // Before: every frame in full, 256 colours, 1280 px, 15 fps.
        let old = dir.join("old.gif");
        {
            let mut enc = gif::Encoder::new(std::fs::File::create(&old).unwrap(), 1280, 800, &[]).unwrap();
            for mut f in frames(1280, 15).iter() {
                let mut gf = gif::Frame::from_rgba_speed(1280, 800, &mut f.image, 10);
                gf.delay = 7;
                enc.write_frame(&gf).unwrap();
            }
        }
        println!("before (full frames, 1280 px, 15 fps, 256 colors): {} KB", size(&old));
        for q in GifQuality::ALL {
            let p = dir.join(format!("{:?}.gif", q));
            encode_gif(frames(q.max_width(), q.fps()), &p, &AtomicBool::new(false), q.colors(), 0).unwrap();
            assert!(image::open(&p).is_ok());
            println!("{:<9} ({}): {} KB", q.label(), q.details(), size(&p));
        }
        // The same recording as MP4, then converted to GIF (decoded frames are noisy).
        let mp4 = dir.join("rec.mp4");
        encode_mp4(frames(1280, 15), 15, &mp4, &AtomicBool::new(false)).unwrap();
        for q in GifQuality::ALL {
            let p = dir.join(format!("from-mp4-{:?}.gif", q));
            crate::player::export_trimmed(&mp4, &p, 0.0, 6.0, q).unwrap();
            println!("MP4 → GIF {:<9}: {} KB", q.label(), size(&p));
        }
    }

    #[test]
    fn encodes_mp4_and_gif() {
        let dir = std::env::temp_dir().join(format!("rega-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = AtomicBool::new(false);
        let mp4 = dir.join("t.mp4");
        encode_mp4(frames(20), 10, &mp4, &cancel).unwrap();
        assert!(std::fs::metadata(&mp4).unwrap().len() > 1000);
        let gif = dir.join("t.gif");
        encode_gif(frames(5), &gif, &cancel, 128, 0).unwrap();
        assert!(image::open(&gif).is_ok());
        assert_eq!(even_fit(2881, 1801, 1920), (1920, 1200));
    }
}

/// An MP4 of `n` frames at `size` and `fps`, for other modules' tests.
#[cfg(test)]
pub fn write_test_video(path: &std::path::Path, n: u32, fps: u32, size: (u32, u32)) {
    encode_mp4(tests::frames_sized(n, size.0, size.1), fps, &path.to_path_buf(), &AtomicBool::new(false)).unwrap();
}

/// A small animated GIF with `n` frames, for other modules' tests.
#[cfg(test)]
pub fn write_test_gif(path: &std::path::Path, n: u32) {
    encode_gif(tests::frames_sized(n, 64, 48), &path.to_path_buf(), &AtomicBool::new(false), 256, 0).unwrap();
}
