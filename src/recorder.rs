//! Screen video recording. A capture thread grabs the region at the target
//! frame rate and hands frames to an encoder thread (H.264/MP4 or GIF).

use std::ffi::{c_int, c_void};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::RgbaImage;

use crate::config::VideoFormat;
use crate::resize::Resizer;

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
    out_path: PathBuf,
    handle: Option<JoinHandle<Result<Finished>>>,
}

/// How a recording ended.
#[derive(Debug)]
pub enum Finished {
    /// The video is at `path`. `stopped_early` is why recording stopped before
    /// it was asked to, if it did; the file has everything until then.
    Saved { path: PathBuf, stopped_early: Option<String> },
    /// Discarded on request: no file is left.
    Discarded,
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
        if cfg.format == VideoFormat::Mp4 {
            // Without an H.264 codec (Windows N) the recording could never be saved.
            crate::h264::available()?;
        }

        // Bounded: when the encoder falls behind (GIF quantisation, slow H.264), drop
        // frames instead of queueing ~8 MB each until memory runs out. MP4 fills the
        // gaps by timestamp and GIF stretches the previous frame's delay.
        let (tx, rx) = mpsc::sync_channel::<TimedFrame>(4);
        let (stop_c, paused_c) = (stop.clone(), paused.clone());
        let fps = cfg.fps.clamp(1, 60);
        let (rx_, ry, rw, rh) = cfg.region;
        let highlight = cfg.highlight_cursor;
        let max_width = cfg.max_width.max(320);
        let format = cfg.format;
        // MP4 takes the sizes H.264 can encode; GIF only has a width to fit.
        let fit = move |w, h| match format {
            VideoFormat::Mp4 => h264_fit(w, h, max_width, fps),
            VideoFormat::Gif => gif_fit(w, h, max_width),
        };

        let capture = thread::spawn(move || -> Result<()> {
            // Keeps the frame deadlines: no efficiency cores, no timer coalescing.
            crate::platform::make_thread_interactive();
            let monitor = crate::capture::monitor_with_origin(origin).context("recording monitor not found")?;
            // The region is in logical points; xcap wants its own units.
            let d = crate::capture::dpi(&monitor);
            let phys = |v: u32| (v as f32 * d).round() as u32;
            let (px, py, pw, ph) = (phys(rx_), phys(ry), phys(rw).max(2), phys(rh).max(2));
            let mut schedule = Schedule::new(fps, Instant::now());
            // Set up for the first frame, whose size fixes the recording's.
            let mut resizer: Option<Resizer> = None;
            let mut failures = 0;
            while !stop_c.load(Ordering::Relaxed) {
                if paused_c.load(Ordering::Relaxed) {
                    schedule.pause(Instant::now());
                    thread::sleep(Duration::from_millis(30));
                    continue;
                }
                schedule.resume(Instant::now());
                let wait = schedule.next_start().saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    // Then look at stop and pause again before capturing.
                    thread::sleep(wait);
                    continue;
                }
                let at = schedule.begin(Instant::now());

                // Ride out transient failures (display sleep, mode switch) for ~2 s:
                // the slots they take repeat a neighbouring frame.
                let mut img = match monitor.capture_region(px, py, pw, ph) {
                    Ok(img) => {
                        failures = 0;
                        img
                    }
                    Err(e) if failures < 2 * fps => {
                        failures += 1;
                        log::warn!("frame capture failed: {e}");
                        continue;
                    }
                    Err(e) => return Err(anyhow::Error::new(e).context("the screen could no longer be captured")),
                };
                let scale = img.width() as f32 / rw as f32;
                if highlight {
                    let (cx, cy) = crate::capture::to_logical(crate::capture::cursor_position());
                    let lx = (cx - origin.0 - rx_ as i32) as f32 * scale;
                    let ly = (cy - origin.1 - ry as i32) as f32 * scale;
                    draw_cursor_halo(&mut img, lx, ly, 18.0 * scale);
                }
                let resizer = resizer.get_or_insert_with(|| Resizer::new(img.dimensions(), fit(img.width(), img.height())));
                if img.dimensions() != resizer.target() {
                    img = resizer.resize(&img);
                }
                match tx.try_send(TimedFrame { image: img, at }) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
            Ok(())
        });

        let (stop_e, cancel_c) = (stop.clone(), cancel.clone());
        let gif_colors = cfg.gif_colors;
        let out_path = cfg.out_path.clone();
        let handle = thread::spawn(move || -> Result<Finished> {
            // Windows slows down a process it takes for background work.
            let _full_speed = crate::platform::full_speed();
            let encoded = match format {
                VideoFormat::Mp4 => encode_mp4_reporting(rx, fps, &out_path, &cancel_c, true),
                VideoFormat::Gif => encode_gif(rx, &out_path, &cancel_c, gif_colors, 0).map(|()| None),
            };
            // When the encoder stopped first (a full disk), the capture thread
            // would only notice on its next frame, which a pause can hold off.
            stop_e.store(true, Ordering::Relaxed);
            let captured = capture.join().unwrap_or_else(|_| Err(anyhow::anyhow!("capture thread panicked")));
            settle(captured, encoded, cancel_c.load(Ordering::Relaxed), out_path)
        });

        Ok(Self {
            stop,
            cancel,
            paused,
            started: Instant::now(),
            paused_total: Duration::ZERO,
            pause_started: None,
            out_path: cfg.out_path,
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

    /// Whether recording is over, asked to or not (no codec, a full disk, the
    /// screen gone): [`Recorder::poll_finished`] has the result.
    pub fn has_ended(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stop and block until the file is finalized (used on quit), giving up after `timeout`.
    pub fn finish_blocking(&mut self, timeout: Duration) -> Option<Result<Finished>> {
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

    /// Returns the result once encoding has finished. A discard always
    /// wins, also one that came after the recording had ended by itself.
    pub fn poll_finished(&mut self) -> Option<Result<Finished>> {
        if !self.has_ended() {
            return None;
        }
        let result = self.handle.take()?.join().unwrap_or_else(|_| Err(anyhow::anyhow!("encoder panicked")));
        if self.cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&self.out_path);
            return Some(Ok(Finished::Discarded));
        }
        Some(result)
    }
}

/// What a recording comes to, from how its capture and its encoding ended
/// (`Ok(Some(e))`: the file was finished early, after failure `e`) and
/// whether it was discarded meanwhile.
fn settle(captured: Result<()>, encoded: Result<Option<anyhow::Error>>, cancelled: bool, path: PathBuf) -> Result<Finished> {
    if cancelled {
        let _ = std::fs::remove_file(&path);
        return Ok(Finished::Discarded);
    }
    let saved = |early: Option<anyhow::Error>| Finished::Saved { path, stopped_early: early.map(|e| format!("{e:#}")) };
    match (captured, encoded) {
        // The file is complete up to the failure; keep it rather than hide it.
        (Err(e), Ok(_)) => Ok(saved(Some(e))),
        (Err(e), Err(_)) => Err(e),
        (Ok(()), encoded) => encoded.map(saved),
    }
}

/// When the capture thread takes each frame. Slot `n` is the recording time
/// [n, n + 1) / fps: its frame is captured as early in it as possible and
/// stamped with its middle, so rounding can't move it to a neighbour's.
/// Sleeping to absolute deadlines keeps timer slack from adding up, and a
/// capture that overran skips the slots it took (they repeat a neighbouring
/// frame); pauses are cut out by moving the schedule later.
struct Schedule {
    fps: u64,
    /// When slot 0 began, moved later by each pause.
    origin: Instant,
    /// The next slot to capture.
    next: u64,
    /// When the pause under way began.
    paused_at: Option<Instant>,
}

impl Schedule {
    fn new(fps: u32, now: Instant) -> Self {
        Self { fps: fps.max(1) as u64, origin: now, next: 0, paused_at: None }
    }

    /// `n / per_second` s, rounded up to whole nanoseconds.
    fn time(n: u64, per_second: u64) -> Duration {
        Duration::from_secs(n / per_second) + Duration::from_nanos((n % per_second * 1_000_000_000).div_ceil(per_second))
    }

    /// When the next slot starts: the next capture is due.
    fn next_start(&self) -> Instant {
        self.origin + Self::time(self.next, self.fps)
    }

    /// Take the frame captured from `now`: the slot that is under way then,
    /// or the next one if that was taken already. Returns the frame's
    /// recording time.
    fn begin(&mut self, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.origin);
        let current = (elapsed.as_nanos() * self.fps as u128 / 1_000_000_000) as u64;
        let slot = self.next.max(current);
        self.next = slot + 1;
        Self::time(2 * slot + 1, 2 * self.fps)
    }

    fn pause(&mut self, now: Instant) {
        self.paused_at.get_or_insert(now);
    }

    fn resume(&mut self, now: Instant) {
        if let Some(since) = self.paused_at.take() {
            self.origin += now.saturating_duration_since(since);
        }
    }
}

/// The most 16×16 macroblocks a second that the system's H.264 decoder is sure
/// to take, from the highest level it supports: 5.2 for VideoToolbox, and 5.1
/// for Media Foundation's, the most it's documented for. A bigger picture
/// needs a lower frame rate.
#[cfg(not(windows))]
const MAX_MACROBLOCK_RATE: u64 = 2_073_600;
#[cfg(windows)]
const MAX_MACROBLOCK_RATE: u64 = 983_040;

/// Fit within `max_width` and the encoders' limits, keeping both sides even
/// (required by H.264 4:2:0). Tiny regions are scaled up to the encoders'
/// minimum; a strip too thin to get there without overflowing the limits has
/// its short side stretched instead.
fn h264_fit(w: u32, h: u32, max_width: u32, fps: u32) -> (u32, u32) {
    use crate::h264::{MAX_HEIGHT, MAX_WIDTH, MIN_SIDE};
    // In pixels, kept a little under as sides round up to whole macroblocks.
    const MAX_PIXEL_RATE: f64 = MAX_MACROBLOCK_RATE as f64 * 256.0 * 0.97;
    // That margin needn't cost a size that's within the limits as it is.
    if w <= max_width && fits_h264(w, h, fps) {
        return (w, h);
    }
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let (max_w, max_h) = (max_width.clamp(MIN_SIDE, MAX_WIDTH) as f64, MAX_HEIGHT as f64);
    let rate = (MAX_PIXEL_RATE / fps.max(1) as f64 / (w * h)).sqrt();
    let ceiling = (max_w / w).min(max_h / h).min(rate);
    let grow = (MIN_SIDE as f64 / w).max(MIN_SIDE as f64 / h);
    let scale = ceiling.min(1.0).max(grow.min(ceiling));
    let side = |v: f64, max: f64| ((((v * scale).round()).min(max) as u32) & !1).max(MIN_SIDE);
    (side(w, max_w), side(h, max_h))
}

/// Whether the system encoders take `w × h` at `fps` as it is: even sides
/// within their limits, and at most [`MAX_MACROBLOCK_RATE`].
fn fits_h264(w: u32, h: u32, fps: u32) -> bool {
    use crate::h264::{MAX_HEIGHT, MAX_WIDTH, MIN_SIDE};
    w.is_multiple_of(2)
        && h.is_multiple_of(2)
        && (MIN_SIDE..=MAX_WIDTH).contains(&w)
        && (MIN_SIDE..=MAX_HEIGHT).contains(&h)
        && w.div_ceil(16) as u64 * h.div_ceil(16) as u64 * fps.max(1) as u64 <= MAX_MACROBLOCK_RATE
}

/// The size an MP4 of `w × h` frames is encoded at: theirs if the encoders
/// take it (a recording's frames are fitted already, and fitting again could
/// shave off a pixel and so resize every frame), else the nearest they do.
fn encoded_size((w, h): (u32, u32), fps: u32) -> (u32, u32) {
    if fits_h264(w, h, fps) { (w, h) } else { h264_fit(w, h, crate::h264::MAX_WIDTH, fps) }
}

/// Fit within `max_width`, keeping both sides even: GIF has none of H.264's
/// size limits.
fn gif_fit(w: u32, h: u32, max_width: u32) -> (u32, u32) {
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

/// Encode frames to an H.264 MP4 at a constant `fps`, with the system's encoder
/// (see `crate::h264`). Frames of a size the encoder can't take (a video from
/// another app being trimmed) are scaled to one it can.
///
/// `live` is for a recording: the encoder must keep pace, and if encoding
/// fails partway, the file is finished with what was encoded until then and
/// the failure only logged (a recording reports it, see
/// [`encode_mp4_reporting`]). Otherwise (an export) a failure leaves no file.
/// A file that can't play (nothing encoded, or a write failed: the index goes
/// in last) is never left behind.
pub(crate) fn encode_mp4(rx: Receiver<TimedFrame>, fps: u32, path: &Path, cancel: &AtomicBool, live: bool) -> Result<()> {
    if let Some(e) = encode_mp4_reporting(rx, fps, path, cancel, live)? {
        log::warn!("the video ends early: {e:#}");
    }
    Ok(())
}

/// [`encode_mp4`], returning why a live recording's video ends early when it
/// does: the failure after which the file was finished.
fn encode_mp4_reporting(rx: Receiver<TimedFrame>, fps: u32, path: &Path, cancel: &AtomicBool, live: bool) -> Result<Option<anyhow::Error>> {
    // Created on the first frame. Frames are written to disk as they are
    // encoded, so memory stays flat however long the recording runs.
    let mut out: Option<Mp4Out> = None;
    let encoded = (|| -> Result<()> {
        for frame in rx.iter() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let out = match &mut out {
                Some(out) => out,
                None => out.insert(Mp4Out::create(path, frame.image.dimensions(), fps, live)?),
            };
            out.push(frame, cancel)?;
        }
        Ok(())
    })();
    match out {
        Some(out) => out.close(path, encoded, live),
        None => encoded.and_then(|()| Err(anyhow::anyhow!("no frames were captured"))),
    }
}

/// The longest gap between two frames that is filled in. A longer one is cut
/// to this, and the frames after it move earlier with it, so a file with huge
/// gaps can't keep an export busy for days.
const MAX_REPEAT: Duration = Duration::from_secs(3600);

/// An MP4 being written: the system's H.264 encoder feeding minimp4.
struct Mp4Out {
    encoder: crate::h264::Encoder,
    file: Mp4File,
    /// The encoded size, which frames are scaled to if theirs differs.
    size: (u32, u32),
    fps: u32,
    /// Frames given to the encoder.
    frames: u64,
    /// Encoded pictures (Annex B) not yet written.
    nal: Vec<u8>,
    /// Pictures written.
    pictures: u64,
    /// Scales frames of another size, set up for the last one seen.
    resizer: Option<Resizer>,
    /// The encoder was flushed: nothing more comes out of it.
    flushed: bool,
    /// Frames cut from the timeline where a gap was cut to [`MAX_REPEAT`].
    skipped: u64,
}

impl Mp4Out {
    fn create(path: &Path, size: (u32, u32), fps: u32, live: bool) -> Result<Self> {
        let size = encoded_size(size, fps);
        let bps = ((size.0 * size.1) as f64 * fps as f64 * 0.15).clamp(1_000_000.0, 20_000_000.0) as u32;
        let encoder = crate::h264::Encoder::new(size.0, size.1, fps, bps, live)?;
        let file = Mp4File::create(path, size, fps)?;
        Ok(Self { encoder, file, size, fps, frames: 0, nal: Vec::new(), pictures: 0, resizer: None, flushed: false, skipped: 0 })
    }

    /// Encode `frame` as often as needed to reach its time: a capture stall
    /// is filled by repeating the frame that ends it, so wall-clock timing
    /// stays exact (a gap longer than [`MAX_REPEAT`] is cut to it). A frame a
    /// live encoder drops is not made up for (see [`Mp4Out::write`]). A long
    /// fill stops as soon as `cancel` is set.
    fn push(&mut self, frame: TimedFrame, cancel: &AtomicBool) -> Result<()> {
        let most = self.frames.saturating_add(MAX_REPEAT.as_secs() * self.fps as u64);
        let slot = ((frame.at.as_secs_f64() * self.fps as f64).floor() as u64).saturating_add(1);
        let mut due = slot.saturating_sub(self.skipped);
        if due > most {
            // Cut the rest of the gap, so the frames after it keep their spacing.
            self.skipped += due - most;
            due = most;
        }
        if due <= self.frames {
            return Ok(());
        }
        let image = if frame.image.dimensions() == self.size {
            frame.image
        } else {
            let from = frame.image.dimensions();
            let resizer = match &mut self.resizer {
                Some(resizer) if resizer.source() == from => resizer,
                resizer => resizer.insert(Resizer::new(from, self.size)),
            };
            resizer.resize(&frame.image)
        };
        while self.frames < due {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            // Whole pictures handed over with an error are still good video.
            let encoded = self.encoder.encode(image.as_raw(), &mut self.nal);
            self.frames += 1;
            self.write()?;
            encoded?;
        }
        Ok(())
    }

    /// Write the encoded pictures, each lasting 1/fps s: the encoder may hold
    /// some back for a while, but loses none unless it's live and had to drop
    /// one to keep up. Pictures are timed by count, so a dropped one shortens
    /// the video by a frame and shows every later picture a frame early. The
    /// file starts at a keyframe.
    fn write(&mut self) -> Result<()> {
        let (sets, pictures) = crate::h264::split_pictures(&self.nal);
        for set in sets {
            self.file.parameter_set(set)?;
        }
        for picture in pictures {
            if self.pictures == 0 && !picture.keyframe {
                continue;
            }
            self.file.picture(&picture.sample, picture.keyframe)?;
            self.pictures += 1;
        }
        self.nal.clear();
        Ok(())
    }

    /// Write out the pictures the encoder still holds.
    fn flush(&mut self) -> Result<()> {
        // After a failed write nothing more can reach the file.
        if std::mem::replace(&mut self.flushed, true) || self.file.write_error() {
            return Ok(());
        }
        let flushed = self.encoder.finish(&mut self.nal);
        self.write().and(flushed)
    }

    /// Flush the encoder and finish the file, then settle `encoded` (how
    /// encoding went) as `encode_mp4` describes: `Ok(Some(e))` is a live
    /// recording kept up to failure `e`.
    fn close(mut self, path: &Path, encoded: Result<()>, live: bool) -> Result<Option<anyhow::Error>> {
        let finished = self.flush();
        let closed = self.file.close();
        let playable = closed.is_ok() && self.pictures > 0;
        // A failed write explains whatever failed after it.
        match closed.and(encoded).and(finished) {
            Ok(()) if playable => Ok(None),
            Err(e) if playable && live => Ok(Some(e)),
            result => {
                let _ = std::fs::remove_file(path);
                Err(result.err().unwrap_or_else(|| anyhow::anyhow!("the video encoder returned no pictures")))
            }
        }
    }
}

/// An MP4 file written through minimp4's muxer, which stores each picture's
/// bytes as given and writes the index (moov) when closed. Each picture lasts
/// one tick of a 1/fps time scale: exact at any frame rate, and the index's
/// 32-bit durations, in ticks, hold 2³² pictures (a 90 kHz scale wrapped
/// after 13 h 15 min).
struct Mp4File {
    mux: *mut minimp4_sys::MP4E_mux_t,
    track: c_int,
    /// From `Box::into_raw`: minimp4 writes through it while one of our calls
    /// into it runs (see [`MuxWriter::write_at`]), so no `Box` may be moved or
    /// borrowed meanwhile. Freed once the muxer has ended, which closes the
    /// file; null then.
    writer: *mut MuxWriter,
}

impl Mp4File {
    fn create(path: &Path, size: (u32, u32), fps: u32) -> Result<Self> {
        let file = std::fs::File::create(path).context("creating the video file")?;
        Self::open(file, path, size, fps)
    }

    /// Start an MP4 in `file`, which is at `path` (removed again on failure).
    fn open(file: std::fs::File, path: &Path, (width, height): (u32, u32), fps: u32) -> Result<Self> {
        let writer = Box::into_raw(Box::new(MuxWriter { file, pos: 0, err: None }));
        // SAFETY: `writer` stays valid until the muxer has ended (`close` or `drop`).
        let mux = unsafe { minimp4_sys::MP4E_open(0, 0, writer.cast(), Some(MuxWriter::write_at)) };
        let mut file = Self { mux, track: 0, writer };
        if mux.is_null() {
            // Frees the writer, closing the file.
            drop(file);
            let _ = std::fs::remove_file(path);
            anyhow::bail!("can't start the video file (out of memory)");
        }
        // The header is written already: a full disk shows up here.
        let started = file.check(minimp4_sys::MP4E_STATUS_OK as c_int);
        let track = minimp4_sys::MP4E_track_t {
            object_type_indication: minimp4_sys::MP4_OBJECT_TYPE_AVC,
            language: *b"und\0",
            track_media_kind: minimp4_sys::track_media_kind_t_e_video,
            time_scale: fps.max(1),
            default_duration: 0,
            u: minimp4_sys::MP4E_track_t__bindgen_ty_1 {
                v: minimp4_sys::MP4E_track_t__bindgen_ty_1__bindgen_ty_2 { width: width as c_int, height: height as c_int },
            },
        };
        let added = started.and_then(|()| {
            // SAFETY: a live muxer; the track description is copied.
            let track = unsafe { minimp4_sys::MP4E_add_track(file.mux, &track) };
            anyhow::ensure!(track >= 0, "can't add a video track (minimp4 error {track})");
            Ok(track)
        });
        match added {
            Ok(track) => {
                file.track = track;
                Ok(file)
            }
            Err(e) => {
                // Closes the muxer and the file, which Windows needs before deleting it.
                drop(file);
                let _ = std::fs::remove_file(path);
                Err(e)
            }
        }
    }

    /// An SPS or PPS; repeats of one already stored are ignored.
    fn parameter_set(&mut self, nal: &[u8]) -> Result<()> {
        // SAFETY: a live muxer and track; the bytes are copied.
        let status = unsafe {
            match crate::h264::nal_type(nal) {
                7 => minimp4_sys::MP4E_set_sps(self.mux, self.track, nal.as_ptr().cast(), nal.len() as c_int),
                _ => minimp4_sys::MP4E_set_pps(self.mux, self.track, nal.as_ptr().cast(), nal.len() as c_int),
            }
        };
        self.check(status)
    }

    /// One picture, lasting a tick (1/fps s): its NAL units, each after a
    /// 4-byte length.
    fn picture(&mut self, sample: &[u8], keyframe: bool) -> Result<()> {
        let kind = if keyframe { minimp4_sys::MP4E_SAMPLE_RANDOM_ACCESS } else { minimp4_sys::MP4E_SAMPLE_DEFAULT };
        // SAFETY: a live muxer and track; the bytes are written out before it returns.
        let status =
            unsafe { minimp4_sys::MP4E_put_sample(self.mux, self.track, sample.as_ptr().cast(), sample.len() as c_int, 1, kind as c_int) };
        self.check(status)
    }

    fn write_error(&self) -> bool {
        self.writer().is_none_or(|w| w.err.is_some())
    }

    /// Write the index and close the file.
    fn close(&mut self) -> Result<()> {
        // SAFETY: a live muxer, ended here once (and freed by minimp4).
        let status = unsafe { minimp4_sys::MP4E_close(self.mux) };
        self.mux = std::ptr::null_mut();
        let result = self.check(status);
        self.free_writer();
        result
    }

    /// minimp4's status as a result, with the write error behind a failure.
    /// Writes never fail as far as minimp4 knows (see `MuxWriter::write_at`),
    /// so this is the only place they show.
    fn check(&self, status: c_int) -> Result<()> {
        if let Some(e) = self.writer().and_then(|w| w.err.as_ref()) {
            return Err(std::io::Error::new(e.kind(), e.to_string())).context("writing the video file");
        }
        anyhow::ensure!(status == minimp4_sys::MP4E_STATUS_OK as c_int, "writing the video file failed (minimp4 error {status})");
        Ok(())
    }

    /// The writer, until the file is closed.
    fn writer(&self) -> Option<&MuxWriter> {
        // SAFETY: null or the live writer from `open`. minimp4 only writes
        // through it during our calls into it, which take `&mut self`, so
        // never while this borrow lasts.
        unsafe { self.writer.as_ref() }
    }

    /// [`Mp4File::writer`], for tests to swap the file under it.
    #[cfg(test)]
    fn writer_mut(&mut self) -> Option<&mut MuxWriter> {
        // SAFETY: as in `writer`.
        unsafe { self.writer.as_mut() }
    }

    /// Free the writer, closing the file: only once the muxer has ended.
    fn free_writer(&mut self) {
        let writer = std::mem::replace(&mut self.writer, std::ptr::null_mut());
        if !writer.is_null() {
            // SAFETY: from `Box::into_raw` in `open`, freed once, and the
            // muxer that wrote through it has ended.
            drop(unsafe { Box::from_raw(writer) });
        }
    }
}

impl Drop for Mp4File {
    fn drop(&mut self) {
        if !self.mux.is_null() {
            // SAFETY: a live muxer not yet ended; this frees it.
            unsafe { minimp4_sys::MP4E_close(self.mux) };
        }
        self.free_writer();
    }
}

/// The file behind an [`Mp4File`], for minimp4's write callback.
struct MuxWriter {
    file: std::fs::File,
    /// Where the next write lands without a seek.
    pos: u64,
    /// The first failed write; later writes are skipped.
    err: Option<std::io::Error>,
}

impl MuxWriter {
    /// minimp4's write callback: write `size` bytes at `offset`. Always
    /// reports success, also for the write that fails: on a failed write
    /// minimp4 returns at once, skipping the frees after it (closing would
    /// leak the index it built), so the failure is kept for `Mp4File::check`
    /// to report instead, and the writes after it are skipped.
    extern "C" fn write_at(offset: i64, buffer: *const c_void, size: usize, token: *mut c_void) -> c_int {
        // SAFETY: `token` is the `MuxWriter` the muxer was opened with, alive
        // and not otherwise borrowed while minimp4 runs.
        let writer = unsafe { &mut *token.cast::<MuxWriter>() };
        if writer.err.is_some() || size == 0 {
            return 0;
        }
        // SAFETY: minimp4 passes `size` readable bytes.
        let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), size) };
        let offset = offset as u64;
        let written = (|| {
            if offset != writer.pos {
                writer.file.seek(SeekFrom::Start(offset))?;
            }
            writer.file.write_all(data)?;
            writer.pos = offset + size as u64;
            Ok(())
        })();
        if let Err(e) = written {
            writer.err = Some(e);
        }
        0
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
/// The GIF is the first frame's size: a frame of another size (a video whose
/// picture size changes midway) is scaled to it, and the first of a new size
/// is stored whole.
///
/// The last frame shows until `end` (recording time) when given, as a trimmed
/// copy's should; otherwise for half a second, so a recording pauses before it
/// loops.
pub(crate) fn encode_gif_until(
    rx: Receiver<TimedFrame>,
    path: &PathBuf,
    cancel: &AtomicBool,
    colors: usize,
    tolerance: u8,
    end: Option<Duration>,
) -> Result<()> {
    type GifFile = gif::Encoder<std::io::BufWriter<std::fs::File>>;
    // The GIF being written and its size.
    let mut gif: Option<(GifFile, (u32, u32))> = None;
    // What a viewer shows after the frames written so far.
    let mut shown: Option<RgbaImage> = None;
    // The size of the last frame written, before any scaling.
    let mut last_size: Option<(u32, u32)> = None;
    let mut resizer: Option<Resizer> = None;
    let mut pending: Option<TimedFrame> = None;
    // Delays from rounded timestamps, so rounding doesn't add up to drift.
    let centis = |d: Duration| (d.as_secs_f64() * 100.0).round() as u64;

    let mut write = |f: TimedFrame, until: u64| -> Result<()> {
        let size = f.image.dimensions();
        let (enc, screen) = match &mut gif {
            Some((enc, screen)) => (enc, *screen),
            None => {
                let file = std::io::BufWriter::new(std::fs::File::create(path).context("creating the GIF file")?);
                let mut enc = gif::Encoder::new(file, size.0 as u16, size.1 as u16, &[]).map_err(gif_error)?;
                enc.set_repeat(gif::Repeat::Infinite).map_err(gif_error)?;
                let (enc, screen) = gif.insert((enc, size));
                (enc, *screen)
            }
        };
        let delay = until.saturating_sub(centis(f.at)).clamp(2, u16::MAX as u64) as u16;
        let new_size = last_size.replace(size).is_some_and(|last| last != size);
        let image = if size == screen {
            f.image
        } else {
            let resizer = match &mut resizer {
                Some(resizer) if resizer.source() == size => resizer,
                resizer => resizer.insert(Resizer::new(size, screen)),
            };
            resizer.resize(&f.image)
        };
        let patch = match &mut shown {
            Some(prev) if !new_size => changes(prev, &image, tolerance, colors),
            _ => None,
        };
        let mut gf = match patch {
            Some(gf) => gf,
            None => {
                let gf = quantize(image.as_raw(), screen.0, screen.1, colors, None);
                shown = Some(image);
                gf
            }
        };
        gf.delay = delay;
        enc.write_frame(&gf).map_err(gif_error)
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
            let start = centis(last.at);
            let until = end.map(centis).filter(|&end| end > start).unwrap_or(start + 50);
            write(last, until)?;
        }
        None => anyhow::bail!("no frames were captured"),
    }
    // Finish the file here: dropped, the encoder and its buffer would write
    // their last bytes but ignore a failure (a full disk), leaving a GIF cut
    // short that was reported saved. No sync_all: on macOS that's F_FULLFSYNC,
    // which SMB shares don't support, so every GIF saved to one would fail.
    if let Some((enc, _)) = gif.take() {
        enc.into_inner().map_err(gif_error)?.flush().context("writing the GIF file")?;
    }
    Ok(())
}

/// [`encode_gif_until`], holding the last frame for half a second.
pub(crate) fn encode_gif(rx: Receiver<TimedFrame>, path: &PathBuf, cancel: &AtomicBool, colors: usize, tolerance: u8) -> Result<()> {
    encode_gif_until(rx, path, cancel, colors, tolerance, None)
}

/// The GIF encoder's error as a failure to write the file. One that wraps
/// another (a write that failed) gives that one: it shows the same message
/// as its source, which `{:#}` would print twice.
fn gif_error(e: gif::EncodingError) -> anyhow::Error {
    let e = match e {
        gif::EncodingError::Io(e) => anyhow::Error::new(e),
        gif::EncodingError::Format(e) => anyhow::Error::new(e),
        e => anyhow::Error::new(e),
    };
    e.context("writing the GIF file")
}

/// `image` as a frame of only what changed since `prev` (a viewer's picture of
/// the same size, updated to match), or `None` if it should be stored whole.
fn changes(prev: &mut RgbaImage, image: &RgbaImage, tolerance: u8, colors: usize) -> Option<gif::Frame<'static>> {
    let (w, h) = image.dimensions();
    let (x0, y0, x1, y1) = changed_rect(prev, image, tolerance).unwrap_or((0, 0, 1, 1));
    // A scroll or scene change touches most of the frame: store it whole
    // and exact, so no low-contrast detail within `tolerance` is left stale.
    if tolerance > 0 && (x1 - x0) as u64 * (y1 - y0) as u64 * 2 > w as u64 * h as u64 {
        let changed = image.pixels().zip(prev.pixels()).filter(|(a, b)| !same(&a.0, &b.0, tolerance)).count() as u64;
        if changed * 4 > w as u64 * h as u64 {
            return None;
        }
    }
    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut patch = Vec::with_capacity((cw * ch * 4) as usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let p = *image.get_pixel(x, y);
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
    Some(quantize(&patch, cw, ch, colors, Some((x0, y0))))
}

fn same(a: &[u8; 4], b: &[u8; 4], tolerance: u8) -> bool {
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
        let (pa, pb) = (ra.as_chunks::<4>().0, rb.as_chunks::<4>().0);
        let differs = |(p, q): (&[u8; 4], &[u8; 4])| !same(p, q, tolerance);
        let Some(first) = pa.iter().zip(pb).position(differs) else { continue };
        let last = pa.iter().zip(pb).rposition(differs).unwrap_or(first);
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
    let pixels = rgba.as_chunks::<4>().0;
    let mut opaque = Vec::with_capacity(rgba.len());
    for p in pixels {
        if p[3] != 0 {
            opaque.extend_from_slice(p);
        }
    }
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
    let nq = color_quant::NeuQuant::new(sample_factor(opaque.len() / 4), slots, &opaque);
    let mut palette = nq.color_map_rgb();
    let clear = (palette.len() / 3) as u8;
    let mut nearest = Nearest::new(&nq, opaque.len() / 4);
    let indices: Vec<u8> = pixels.iter().map(|p| if p[3] == 0 { clear } else { nearest.index_of(p) }).collect();
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

/// NeuQuant's sample factor for learning a palette from `pixels` pixels (it
/// learns from every n-th one): the usual speed/quality balance, 10, up to
/// 100k pixels, rising to its sparsest, 30, for a change of a whole frame.
/// That caps learning at about 10k samples up to 300k pixels, and makes a
/// 1280 px scroll learn three times faster for about 1.5 dB.
fn sample_factor(pixels: usize) -> i32 {
    (pixels / 10_000).clamp(10, 30) as i32
}

/// [`color_quant::NeuQuant::index_of`] remembered per colour: a screen has
/// few distinct colours, and every search walks the palette.
struct Nearest<'a> {
    nq: &'a color_quant::NeuQuant,
    /// A colour (RGBA as a `u32`) and its palette index per slot. Only opaque
    /// colours are looked up, so 0 marks an empty slot.
    slots: Vec<(u32, u8)>,
    shift: u32,
}

impl<'a> Nearest<'a> {
    /// 64K slots, or fewer for fewer `pixels`.
    fn new(nq: &'a color_quant::NeuQuant, pixels: usize) -> Self {
        let bits = pixels.next_power_of_two().trailing_zeros().clamp(8, 16);
        Self { nq, slots: vec![(0, 0); 1 << bits], shift: 32 - bits }
    }

    fn index_of(&mut self, pixel: &[u8; 4]) -> u8 {
        let key = u32::from_le_bytes(*pixel);
        let slot = &mut self.slots[(key.wrapping_mul(0x9e37_79b1) >> self.shift) as usize];
        if slot.0 != key {
            // At most 256 palette entries.
            *slot = (key, self.nq.index_of(pixel) as u8);
        }
        slot.1
    }
}

/// An MP4 of `n` frames at `size` and `fps`, for other modules' tests.
#[cfg(test)]
pub fn write_test_video(path: &std::path::Path, n: u32, fps: u32, size: (u32, u32)) {
    encode_mp4(tests::frames_sized(n, size.0, size.1, fps), fps, path, &AtomicBool::new(false), false).unwrap();
}

/// A small animated GIF with `n` frames, for other modules' tests.
#[cfg(test)]
pub fn write_test_gif(path: &std::path::Path, n: u32) {
    encode_gif(tests::frames_sized(n, 64, 48, 10), &path.to_path_buf(), &AtomicBool::new(false), 256, 0).unwrap();
}

#[cfg(test)]
impl Recorder {
    /// A recording without capture or encoding, for the app's tests: `run`
    /// stands in for its thread, given the stop flag, and gives the result.
    pub(crate) fn fake(out_path: PathBuf, run: impl FnOnce(&AtomicBool) -> Result<Finished> + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        Self {
            stop,
            cancel: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            paused_total: Duration::ZERO,
            pause_started: None,
            out_path,
            handle: Some(thread::spawn(move || run(&stop_t))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` frames at `fps`, each captured midway through its 1/fps slot as a
    /// live capture would be (exact slot boundaries would leave rounding to
    /// pick the slot).
    pub(super) fn frames_sized(n: u32, w: u32, h: u32, fps: u32) -> Receiver<TimedFrame> {
        let (tx, rx) = mpsc::channel();
        for i in 0..n {
            let image = RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x + i * 8) as u8, y as u8, 128, 255]));
            tx.send(TimedFrame { image, at: Duration::from_secs_f64((i as f64 + 0.5) / fps as f64) }).unwrap();
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

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rega-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The real file's handle swapped for a read-only one: every write from
    /// now on fails as on a full disk or a lost drive.
    fn make_read_only(file: &mut Mp4File, path: &Path) {
        file.writer_mut().unwrap().file = std::fs::File::open(path).unwrap();
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
        let dir = test_dir("gif-bench");
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
        encode_mp4(frames(1280, 15), 15, &mp4, &AtomicBool::new(false), false).unwrap();
        for q in GifQuality::ALL {
            let p = dir.join(format!("from-mp4-{:?}.gif", q));
            crate::player::export_trimmed(&mp4, &p, 0.0, 6.0, q).unwrap();
            println!("MP4 → GIF {:<9}: {} KB", q.label(), size(&p));
        }
    }

    /// Per-frame GIF encode times at each preset, for a screen recording
    /// with a moving cursor and for one whose every frame changes whole (a
    /// scroll), plus decoded-video-like noise. Set `GIF_DUMP_DIR` to keep the
    /// GIFs. `cargo test --release gif_encode_times -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn gif_encode_times() {
        use crate::config::GifQuality;
        let ui = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/images/editor.png")).unwrap().to_rgba8();
        let dir = std::env::var("GIF_DUMP_DIR").map(PathBuf::from).unwrap_or_else(|_| test_dir("gif-times"));
        std::fs::create_dir_all(&dir).unwrap();
        for q in GifQuality::ALL {
            let w = q.max_width();
            let h = w * 10 / 16;
            let base = image::imageops::resize(&ui, w, ui.height() * w / ui.width(), image::imageops::FilterType::Triangle);
            let n = 3 * q.fps();
            let at = |i: u32| Duration::from_secs_f64((i as f64 + 0.5) / q.fps() as f64);
            let cursor: Vec<TimedFrame> = (0..n)
                .map(|i| {
                    let mut img = image::imageops::crop_imm(&base, 0, 0, w, h).to_image();
                    let (x, y) = (w / 4 + i * w / (2 * n), w / 5);
                    for dy in 0..w / 60 {
                        for dx in 0..w / 60 {
                            img.put_pixel(x + dx, y + dy, image::Rgba([255, 214, 0, 255]));
                        }
                    }
                    TimedFrame { image: img, at: at(i) }
                })
                .collect();
            let scroll: Vec<TimedFrame> = (0..n)
                .map(|i| TimedFrame { image: image::imageops::crop_imm(&base, 0, (i * 7) % (base.height() - h), w, h).to_image(), at: at(i) })
                .collect();
            // Decoded video: noise within the tolerance on every frame, a
            // moving block, and from halfway a change in a third of the picture.
            let noisy: Vec<TimedFrame> = (0..n)
                .map(|i| {
                    let mut img = image::imageops::crop_imm(&base, 0, 0, w, h).to_image();
                    for (k, p) in img.pixels_mut().enumerate() {
                        let d = ((k as u32 ^ i.wrapping_mul(0x9e37_79b9)).wrapping_mul(2_654_435_761) >> 30) as u8;
                        p[0] = p[0].saturating_add(d);
                        let (x, y) = (k as u32 % w, k as u32 / w);
                        if i > n / 2 && x < w / 3 {
                            p[1] = p[1].wrapping_add(90);
                        }
                        if (w / 3 + i * 5..w / 3 + i * 5 + 24).contains(&x) && (h / 2..h / 2 + 24).contains(&y) {
                            *p = image::Rgba([250, 250, 250, 255]);
                        }
                    }
                    TimedFrame { image: img, at: at(i) }
                })
                .collect();
            for (name, frames, tolerance) in [("cursor", cursor, 0), ("scroll", scroll, 0), ("noisy", noisy, 4)] {
                let (tx, rx) = mpsc::channel();
                for f in frames {
                    tx.send(f).unwrap();
                }
                drop(tx);
                let path = dir.join(format!("{name}-{:?}.gif", q));
                let t = Instant::now();
                encode_gif(rx, &path, &AtomicBool::new(false), q.colors(), tolerance).unwrap();
                let ms = t.elapsed().as_secs_f64() * 1e3 / n as f64;
                println!("{:<9} {name:<7}: {ms:6.1} ms per frame, {} KB", q.label(), std::fs::metadata(&path).unwrap().len() / 1024);
            }
        }
    }
    /// Quantizing whole frames with the adaptive sample factor against the
    /// usual 10: PSNR and time. `cargo test --release gif_sample_factor -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints quality and timings"]
    fn gif_sample_factor() {
        use crate::config::GifQuality;
        let ui = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/images/editor.png")).unwrap().to_rgba8();
        let psnr = |a: &[u8], b: &[u8]| {
            let sum: f64 = a.iter().zip(b).map(|(&p, &q)| (p as f64 - q as f64).powi(2)).sum();
            10.0 * (255.0 * 255.0 / (sum / a.len() as f64).max(1e-9)).log10()
        };
        for q in GifQuality::ALL {
            let w = q.max_width();
            let base = image::imageops::resize(&ui, w, ui.height() * w / ui.width(), image::imageops::FilterType::Triangle);
            let frame = image::imageops::crop_imm(&base, 0, 40, w, w * 10 / 16).to_image();
            let opaque = frame.as_raw();
            for factor in [10, sample_factor(opaque.len() / 4)] {
                let t = Instant::now();
                let nq = color_quant::NeuQuant::new(factor, q.colors(), opaque);
                let ms = t.elapsed().as_secs_f64() * 1e3;
                let mut mapped = opaque.clone();
                for p in mapped.as_chunks_mut::<4>().0 {
                    nq.map_pixel(p);
                }
                println!("{:<9} {w} px, factor {factor:2}: learning {ms:5.1} ms, {:.2} dB", q.label(), psnr(&mapped, opaque));
            }
        }
    }

    /// minimp4 writes the index last, so after a failed write the file can't
    /// play: it's removed, and the write error is what's reported.
    #[test]
    fn a_failed_write_removes_the_video() {
        let path = test_dir("write").join("full.mp4");
        let mut out = Mp4Out::create(&path, (320, 240), 10, false).unwrap();
        for f in frames(5).iter() {
            out.push(f, &AtomicBool::new(false)).unwrap();
        }
        make_read_only(&mut out.file, &path);
        let encoded = frames(1).iter().try_for_each(|f| out.push(TimedFrame { at: Duration::from_secs(1), ..f }, &AtomicBool::new(false)));
        assert!(encoded.is_err(), "writing stops at the failure");
        // Even a live recording can't keep a file without its index.
        let err = out.close(&path, encoded, true).unwrap_err();
        assert!(format!("{err:#}").starts_with("writing the video file: "), "{err:#}");
        assert!(!path.exists());
    }

    /// Every picture is in, then writing the index fails: no file, the write
    /// error is reported, and minimp4 still frees the index it built (run
    /// under `leaks --atExit` to see).
    #[test]
    fn a_failed_index_write_removes_the_video() {
        let path = test_dir("index").join("index.mp4");
        let mut out = Mp4Out::create(&path, (320, 240), 10, true).unwrap();
        for f in frames(30).iter() {
            out.push(f, &AtomicBool::new(false)).unwrap();
        }
        out.flush().unwrap();
        assert_eq!(out.pictures, 30);
        make_read_only(&mut out.file, &path);
        let err = out.close(&path, Ok(()), true).unwrap_err();
        assert!(format!("{err:#}").starts_with("writing the video file: "), "{err:#}");
        assert!(!path.exists());
    }

    /// Opening a file whose first write fails leaves nothing behind.
    #[test]
    fn a_video_file_that_cant_be_written_is_not_started() {
        let path = test_dir("open").join("read-only.mp4");
        std::fs::write(&path, b"").unwrap();
        let read_only = std::fs::File::open(&path).unwrap();
        let err = Mp4File::open(read_only, &path, (320, 240), 10).err().expect("the header can't be written");
        assert!(format!("{err:#}").starts_with("writing the video file: "), "{err:#}");
        assert!(!path.exists());
    }

    /// When the encoder fails partway, a recording keeps what was encoded
    /// until then and says why it ends early; an export leaves nothing.
    #[test]
    fn an_encoder_failure_keeps_a_recording_but_not_an_export() {
        let dir = test_dir("partial");
        for live in [true, false] {
            let path = dir.join(format!("partial-{live}.mp4"));
            let mut out = Mp4Out::create(&path, (320, 240), 10, live).unwrap();
            for f in frames(15).iter() {
                out.push(f, &AtomicBool::new(false)).unwrap();
            }
            let result = out.close(&path, Err(anyhow::anyhow!("the encoder broke")), live);
            match result {
                Ok(early) => assert!(live && early.is_some_and(|e| e.to_string() == "the encoder broke")),
                Err(e) => assert!(!live && e.to_string() == "the encoder broke", "{e:#}"),
            }
            assert_eq!(path.exists(), live);
        }
        let media = crate::player::tests::frame_count(&dir.join("partial-true.mp4"));
        assert_eq!(media, 15, "the kept recording has every frame encoded before the failure");
    }

    /// How a recording's threads ended decides what the user is told.
    #[test]
    fn early_ends_are_reported_with_the_saved_file() {
        let path = test_dir("settle").join("rec.mp4");
        let saved = |r: Result<Finished>| match r {
            Ok(Finished::Saved { path: p, stopped_early }) => {
                assert_eq!(p, path);
                stopped_early
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(saved(settle(Ok(()), Ok(None), false, path.clone())), None);
        let lost = anyhow::anyhow!("display gone").context("the screen could no longer be captured");
        let reason = saved(settle(Err(lost), Ok(None), false, path.clone()));
        assert_eq!(reason.as_deref(), Some("the screen could no longer be captured: display gone"));
        // The encoder failed partway: a live recording keeps what it encoded until then.
        let broke = saved(settle(Ok(()), Ok(Some(anyhow::anyhow!("the H.264 encoder failed"))), false, path.clone()));
        assert_eq!(broke.as_deref(), Some("the H.264 encoder failed"));
        assert!(settle(Ok(()), Err(anyhow::anyhow!("no frames were captured")), false, path.clone()).is_err());
        assert!(settle(Err(anyhow::anyhow!("no monitor")), Err(anyhow::anyhow!("no frames")), false, path.clone())
            .is_err_and(|e| e.to_string() == "no monitor"));
        std::fs::write(&path, b"video").unwrap();
        assert!(matches!(settle(Ok(()), Ok(None), true, path.clone()), Ok(Finished::Discarded)));
        assert!(!path.exists());
    }

    /// Discard pressed after the recording had already ended by itself (and
    /// kept its file) still discards it.
    #[test]
    fn a_discard_after_an_early_end_removes_the_file() {
        let path = test_dir("discard").join("ended.mp4");
        std::fs::write(&path, b"video").unwrap();
        let kept = path.clone();
        let lost = Some("the screen could no longer be captured".into());
        let mut rec = Recorder::fake(path.clone(), move |_| Ok(Finished::Saved { path: kept, stopped_early: lost }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !rec.has_ended() {
            assert!(Instant::now() < deadline, "the recording should end by itself");
            thread::sleep(Duration::from_millis(5));
        }
        rec.discard();
        assert!(matches!(rec.poll_finished(), Some(Ok(Finished::Discarded))));
        assert!(!path.exists());
        assert!(rec.has_ended() && rec.poll_finished().is_none(), "the result is given once");
    }

    #[test]
    fn a_recording_runs_until_stopped() {
        let mut rec = Recorder::fake(PathBuf::new(), |stop| {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(2));
            }
            Ok(Finished::Discarded)
        });
        thread::sleep(Duration::from_millis(20));
        assert!(!rec.has_ended() && rec.poll_finished().is_none());
        assert!(matches!(rec.finish_blocking(Duration::from_secs(5)), Some(Ok(Finished::Discarded))));
    }

    #[test]
    fn encodes_mp4_and_gif() {
        let dir = test_dir("test");
        let cancel = AtomicBool::new(false);
        let mp4 = dir.join("t.mp4");
        encode_mp4(frames(20), 10, &mp4, &cancel, false).unwrap();
        assert!(std::fs::metadata(&mp4).unwrap().len() > 1000);
        let gif = dir.join("t.gif");
        encode_gif(frames(5), &gif, &cancel, 128, 0).unwrap();
        assert!(image::open(&gif).is_ok());
        assert_eq!(h264_fit(2881, 1801, 1920, 30), (1920, 1200));
        // Tiny regions are scaled up to the encoders' minimum, keeping the shape.
        assert_eq!(h264_fit(40, 20, 1920, 30), (128, 64));
        assert_eq!(h264_fit(1000, 30, 1920, 30), (1920, 64));
        // Taller than the encoders take: scaled down to fit, aspect kept.
        assert_eq!(h264_fit(1000, 3000, 1920, 30), (768, 2304));
        // The largest picture fits H.264 level 5.1 (and so 5.2) at 25 fps, but not at 60.
        assert_eq!(h264_fit(4096, 2304, 4096, 25), (4096, 2304));
        let (w, h) = h264_fit(4096, 2304, 4096, 60);
        assert!(fits_h264(w, h, 60) && w < 4096, "{w}×{h} at 60 fps");
        // GIF has no minimum or maximum height: only the width is fitted.
        assert_eq!(gif_fit(40, 20, 1920), (40, 20));
        assert_eq!(gif_fit(2881, 1801, 1920), (1920, 1200));
        assert_eq!(gif_fit(800, 3000, 960), (800, 3000));
    }

    /// A recording's frames are fitted when captured; the MP4 keeps that size
    /// rather than fitting again, which would shave a pixel off and resize
    /// every frame.
    #[test]
    fn fitted_sizes_are_kept() {
        for fps in [1, 15, 30, 60] {
            for (w, h, max) in [(3782, 2306, 3840), (5120, 2880, 4096), (4096, 2160, 4096), (2881, 1801, 1920), (41, 31, 1920), (1000, 30, 1920), (31, 4000, 1920), (10_000, 10, 4096)] {
                let once = h264_fit(w, h, max, fps);
                assert!(fits_h264(once.0, once.1, fps), "{w}×{h} → {once:?} at {fps} fps");
                assert_eq!(encoded_size(once, fps), once, "{w}×{h} → {once:?} at {fps} fps");
            }
        }
        // At 60 fps 3782×2306 is over the macroblock rate as well as the height.
        let once = h264_fit(3782, 2306, 3840, 60);
        assert!(once.0 < 3782 && once.1 <= crate::h264::MAX_HEIGHT);
        let path = test_dir("fitted").join("fitted.mp4");
        assert_eq!(Mp4Out::create(&path, once, 60, false).unwrap().size, once);
        // Sizes the encoders can't take are still fitted.
        assert_eq!(encoded_size((41, 31), 30), (84, 64));
        assert_eq!(encoded_size((4097, 2305), 30), h264_fit(4097, 2305, crate::h264::MAX_WIDTH, 30));
    }

    /// Frames of a size the encoder can't take are scaled to one it can.
    #[test]
    fn frames_of_other_sizes_are_scaled() {
        let path = test_dir("scaled").join("tiny.mp4");
        encode_mp4(frames_sized(12, 41, 31, 10), 10, &path, &AtomicBool::new(false), false).unwrap();
        assert_eq!(crate::player::first_frame(&path).unwrap().dimensions(), (84, 64));
        assert_eq!(crate::player::tests::frame_count(&path), 12);
    }

    /// A gap between two frames is filled for an hour at most, and the frames
    /// after it keep their spacing rather than each being held for an hour too.
    #[test]
    fn a_huge_time_gap_is_filled_for_an_hour_at_most() {
        let path = test_dir("gap").join("gap.mp4");
        let mut out = Mp4Out::create(&path, (64, 64), 1, false).unwrap();
        let frame = |at| TimedFrame { image: RgbaImage::from_pixel(64, 64, image::Rgba([9, 99, 199, 255])), at };
        let mut repeats = Vec::new();
        // Two hours between the first two frames.
        for at in [0.5, 7200.5, 7201.5, 7202.5, 7203.5] {
            let before = out.frames;
            out.push(frame(Duration::from_secs_f64(at)), &AtomicBool::new(false)).unwrap();
            repeats.push(out.frames - before);
        }
        assert_eq!(repeats, [1, MAX_REPEAT.as_secs(), 1, 1, 1]);
        // A stamp as late as can be is cut the same way, without overflowing
        // (cancelled, so nothing more is encoded).
        out.push(frame(Duration::MAX), &AtomicBool::new(true)).unwrap();
        assert_eq!(out.frames, 3604);
        assert!(out.close(&path, Ok(()), false).unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// Cancelling stops a long fill at once, not after an hour of repeats
    /// (which a damaged file's timing can ask for).
    #[test]
    fn a_cancel_cuts_a_long_fill_short() {
        let path = test_dir("cancel").join("cancel.mp4");
        let mut out = Mp4Out::create(&path, (64, 64), 30, false).unwrap();
        let frame = |at| TimedFrame { image: RgbaImage::from_pixel(64, 64, image::Rgba([9, 99, 199, 255])), at };
        let cancel = AtomicBool::new(false);
        out.push(frame(Duration::from_millis(10)), &cancel).unwrap();
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(20));
                cancel.store(true, Ordering::Relaxed);
            });
            out.push(frame(MAX_REPEAT), &cancel).unwrap();
        });
        assert!(out.frames < 10_000, "{} of {} repeats", out.frames - 1, 30 * MAX_REPEAT.as_secs());
        drop(out);
        let _ = std::fs::remove_file(&path);
    }

    /// The durations an MP4's header gives: the movie's and its track's.
    fn header_durations(path: &Path) -> (Duration, Duration) {
        let size = std::fs::metadata(path).unwrap().len();
        let mp4 = mp4::Mp4Reader::read_header(std::io::BufReader::new(std::fs::File::open(path).unwrap()), size).unwrap();
        let track = mp4.tracks().values().next().unwrap().duration();
        (mp4.duration(), track)
    }

    /// Each picture lasts one tick of a 1/fps time scale: durations are exact
    /// at any rate, and a long video's is right too. The header counts ticks
    /// in 32 bits, so at 90 kHz 50,000 s of 1 fps video claimed 38 minutes.
    #[test]
    fn videos_keep_their_duration_however_long() {
        let mut stream = Vec::new();
        let mut encoder = crate::h264::Encoder::new(64, 64, 1, 1_000_000, false).unwrap();
        encoder.encode(&[0; 64 * 64 * 4], &mut stream).unwrap();
        encoder.finish(&mut stream).unwrap();
        let (sets, pictures) = crate::h264::split_pictures(&stream);
        let dir = test_dir("durations");
        // The same picture over and over: the muxer doesn't look inside.
        let write = |fps: u32, n: u64| {
            let path = dir.join(format!("{fps}-{n}.mp4"));
            let mut file = Mp4File::create(&path, (64, 64), fps).unwrap();
            for set in &sets {
                file.parameter_set(set).unwrap();
            }
            for i in 0..n {
                file.picture(&pictures[0].sample, i == 0).unwrap();
            }
            file.close().unwrap();
            let durations = header_durations(&path);
            std::fs::remove_file(&path).unwrap();
            durations
        };
        for fps in [1, 7, 15, 30, 60] {
            let three = Duration::from_secs(3);
            assert_eq!(write(fps, 3 * fps as u64), (three, three), "{fps} fps");
        }
        // 13 h 53 min.
        let long = Duration::from_secs(50_000);
        assert_eq!(write(1, 50_000), (long, long));
    }

    /// A 14-hour recording at 1 fps, encoded for real: its header gives its
    /// whole length. `cargo test --release fourteen_hour -- --ignored`
    #[test]
    #[ignore = "slow: encodes 14 hours of 1 fps video, about a minute"]
    fn a_fourteen_hour_recording_keeps_its_length() {
        let path = test_dir("long").join("14h.mp4");
        let mut out = Mp4Out::create(&path, (64, 64), 1, false).unwrap();
        // A frame every 10 minutes, each filling the slots before it.
        for minute in (0..=14 * 60).step_by(10) {
            let image = RgbaImage::from_pixel(64, 64, image::Rgba([minute as u8, 99, 199, 255]));
            let at = Duration::from_secs(minute * 60) + Duration::from_millis(500);
            out.push(TimedFrame { image, at }, &AtomicBool::new(false)).unwrap();
        }
        assert!(out.close(&path, Ok(()), false).unwrap().is_none());
        let pictures = crate::player::tests::frame_count(&path);
        let durations = header_durations(&path);
        std::fs::remove_file(&path).unwrap();
        let length = Duration::from_secs(14 * 3600 + 1);
        assert_eq!((pictures, durations), (14 * 3600 + 1, (length, length)));
    }

    /// A GIF that can't be created says so, giving the reason once.
    #[test]
    fn a_gif_that_cant_be_created_says_why() {
        let path = test_dir("gif-create").join("missing").join("rec.gif");
        let err = encode_gif(frames(1), &path, &AtomicBool::new(false), 64, 0).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.starts_with("creating the GIF file: ") && message.matches("os error").count() == 1, "{message}");
    }

    /// A GIF that runs out of disk space is an error, with the reason given
    /// once, also when only its last bytes don't fit: the encoder writes them
    /// when it's done, and when dropped, ignores a failure. A file size limit
    /// stands in for a full disk; it holds for the whole process, so the test
    /// runs again in a child process of its own for it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_gif_that_doesnt_fit_on_the_disk_is_an_error() {
        const CHILD: &str = "REGA_TEST_FILE_SIZE_LIMIT";
        if std::env::var_os(CHILD).is_none() {
            // The test's name as the harness gives it, without the crate's.
            let name = format!("{}::a_gif_that_doesnt_fit_on_the_disk_is_an_error", module_path!().split_once("::").unwrap().1);
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([name.as_str(), "--exact", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let output = String::from_utf8_lossy(&child.stdout) + String::from_utf8_lossy(&child.stderr);
            assert!(child.status.success() && output.contains("1 passed"), "{output}");
            return;
        }
        // From <sys/resource.h> and <sys/signal.h>.
        #[repr(C)]
        struct Limit {
            cur: u64,
            max: u64,
        }
        unsafe extern "C" {
            fn getrlimit(resource: c_int, limit: *mut Limit) -> c_int;
            fn setrlimit(resource: c_int, limit: *const Limit) -> c_int;
            fn signal(signal: c_int, handler: usize) -> usize;
        }
        const RLIMIT_FSIZE: c_int = 1;
        const SIGXFSZ: c_int = 25;
        const SIG_IGN: usize = 1;
        let dir = test_dir("gif-limit");
        let gif = |frames, name: &str| encode_gif(frames_sized(frames, 64, 48, 10), &dir.join(name), &AtomicBool::new(false), 256, 0);
        gif(5, "whole.gif").unwrap();
        let size = std::fs::metadata(dir.join("whole.gif")).unwrap().len();
        let mut limit = Limit { cur: 0, max: 0 };
        // SAFETY: plain calls, in a process running only this test. A write
        // past the limit then fails (EFBIG) rather than ending the process.
        unsafe {
            signal(SIGXFSZ, SIG_IGN);
            assert_eq!(getrlimit(RLIMIT_FSIZE, &mut limit), 0);
            limit.cur = size - 1;
            assert_eq!(setrlimit(RLIMIT_FSIZE, &limit), 0);
        }
        // One byte short, so only the very last write fails; then a GIF that
        // fails while its frames are written.
        let short = gif(5, "short.gif").map_err(|e| format!("{e:#}"));
        let long = gif(60, "long.gif").map_err(|e| format!("{e:#}"));
        let _ = std::fs::remove_dir_all(&dir);
        for result in [short, long] {
            assert_eq!(result, Err("writing the GIF file: File too large (os error 27)".to_string()));
        }
    }

    /// A GIF is saved to a file the system can't flush to permanent storage:
    /// an SMB share takes writes but not F_FULLFSYNC (`sync_all` on macOS),
    /// and neither does /dev/null.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_gif_is_saved_where_the_disk_cant_be_synced() {
        encode_gif(frames_sized(2, 64, 48, 10), &PathBuf::from("/dev/null"), &AtomicBool::new(false), 64, 0).unwrap();
    }

    /// Sizes keep to what the system's decoder is sure to take: level 5.2 on
    /// macOS; on Windows 5.1, the most Media Foundation's is documented for.
    /// A size within it as it is stays as it is.
    #[test]
    fn sizes_keep_to_the_decoders_level() {
        assert_eq!(MAX_MACROBLOCK_RATE, if cfg!(windows) { 983_040 } else { 2_073_600 });
        // 4096×2160 is 34,560 macroblocks: kept at the highest frame rate the
        // level allows for it, scaled down above.
        let fps = (MAX_MACROBLOCK_RATE / 34_560) as u32;
        assert_eq!(h264_fit(4096, 2160, 4096, fps), (4096, 2160), "at {fps} fps");
        let (w, h) = h264_fit(4096, 2160, 4096, fps + 1);
        assert!(w < 4096 && fits_h264(w, h, fps + 1), "{w}×{h} at {} fps", fps + 1);
        // A 4K screen at 30 fps, the most the settings offer, is recorded whole.
        assert_eq!(h264_fit(3840, 2160, 3840, 30), (3840, 2160));
    }

    /// A frame of a new size (a source whose picture size changes midway) is
    /// scaled to the GIF's and stored whole, not compared with the last one.
    #[test]
    fn gif_frames_of_another_size_are_scaled_to_fit() {
        let (tx, rx) = mpsc::channel();
        let sizes = [(64, 48), (64, 48), (96, 40), (96, 40), (32, 24), (64, 48)];
        for (i, &(w, h)) in sizes.iter().enumerate() {
            let c = [40 * i as u8, 200 - 30 * i as u8, 90, 255];
            let image = RgbaImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgba(c) } else { image::Rgba([250, 250, 250, 255]) });
            tx.send(TimedFrame { image, at: Duration::from_millis(100 * i as u64) }).unwrap();
        }
        drop(tx);
        let path = test_dir("gif-sizes").join("sizes.gif");
        encode_gif(rx, &path, &AtomicBool::new(false), 64, 0).unwrap();
        let mut decoder = gif::DecodeOptions::new();
        decoder.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = decoder.read_info(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!((decoder.width(), decoder.height()), (64, 48));
        let mut written = Vec::new();
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            assert!(frame.left + frame.width <= 64 && frame.top + frame.height <= 48, "inside the GIF");
            written.push((frame.width, frame.height));
        }
        // Every frame differs from the one before; each new size is stored whole.
        assert_eq!(written.len(), sizes.len());
        for i in [0, 2, 4, 5] {
            assert_eq!(written[i], (64, 48), "frame {i}");
        }
        assert!(image::open(&path).is_ok());
    }

    /// The colour cache gives exactly NeuQuant's answer, collisions included.
    #[test]
    fn nearest_colours_are_remembered_exactly() {
        let pixels: Vec<u8> = (0..40_000u32).flat_map(|i| [(i * 7) as u8, ((i * 13) >> 3) as u8, (i >> 8) as u8, 255]).collect();
        let nq = color_quant::NeuQuant::new(10, 200, &pixels);
        // 256 slots for 40k colours collide all the time, 64K now and then.
        for sized_for in [10, 40_000] {
            let mut nearest = Nearest::new(&nq, sized_for);
            for _ in 0..2 {
                for p in pixels.as_chunks::<4>().0 {
                    assert_eq!(nearest.index_of(p) as usize, nq.index_of(p), "{p:?}");
                }
            }
        }
        assert_eq!(sample_factor(100), 10);
        assert_eq!(sample_factor(100_000), 10);
        assert_eq!(sample_factor(200_000), 20);
        assert_eq!(sample_factor(1280 * 800), 30);
    }

    /// Frames land in consecutive slots, stamped mid-slot, however late the
    /// thread wakes within its slot: oversleeping doesn't add up.
    #[test]
    fn frames_keep_to_the_schedule() {
        let t0 = Instant::now();
        let mut schedule = Schedule::new(30, t0);
        for n in 0..300u64 {
            let start = schedule.next_start();
            assert_eq!(start, t0 + Duration::from_nanos((n * 1_000_000_000).div_ceil(30)), "slot {n} starts on the grid");
            // Timer slack: woken 5 ms late, every time.
            let at = schedule.begin(start + Duration::from_millis(5));
            assert!(at.abs_diff(Duration::from_secs_f64((n as f64 + 0.5) / 30.0)) < Duration::from_micros(1), "slot {n}: {at:?}");
        }
        assert_eq!(Schedule::time(1, 30), Duration::from_nanos(33_333_334));
        assert_eq!(Schedule::time(61, 30), Duration::from_nanos(2_033_333_334));
    }

    /// A capture that takes longer than a slot skips the slots it used up.
    #[test]
    fn overruns_skip_the_slots_they_took() {
        let t0 = Instant::now();
        let mut schedule = Schedule::new(30, t0);
        let mut now = t0;
        let mut slots = Vec::new();
        for _ in 0..10 {
            now = now.max(schedule.next_start());
            let at = schedule.begin(now);
            slots.push((at.as_secs_f64() * 30.0) as u64);
            // 45 ms a capture.
            now += Duration::from_millis(45);
        }
        assert_eq!(slots, [0, 1, 2, 4, 5, 6, 8, 9, 10, 12]);
        // Woken a whole second late (at 1.45 s): the slots in between are skipped.
        let at = schedule.begin(now + Duration::from_secs(1));
        assert_eq!((at.as_secs_f64() * 30.0) as u64, 43);
    }

    /// Share of 1/fps slots that get a frame of their own with a capture
    /// taking `work`: the old loop (sleep for the rest of the interval, stamp
    /// with the capture time) against the schedule, with real sleeps.
    /// `cargo test --release capture_pacing -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn capture_pacing() {
        let run = Duration::from_secs(3);
        for fps in [30, 60] {
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            for work in [5, 12, 25, 40].map(Duration::from_millis) {
                let mut old = std::collections::BTreeSet::new();
                let (start, mut last, mut rec_time) = (Instant::now(), Instant::now(), Duration::ZERO);
                while start.elapsed() < run {
                    let tick = Instant::now();
                    rec_time += tick - last;
                    last = tick;
                    thread::sleep(work);
                    old.insert((rec_time.as_secs_f64() * fps as f64).floor() as u64);
                    if let Some(rest) = interval.checked_sub(tick.elapsed()) {
                        thread::sleep(rest);
                    }
                }
                let new = thread::spawn(move || {
                    crate::platform::make_thread_interactive();
                    let mut slots = std::collections::BTreeSet::new();
                    let mut schedule = Schedule::new(fps, Instant::now());
                    let start = Instant::now();
                    while start.elapsed() < run {
                        let wait = schedule.next_start().saturating_duration_since(Instant::now());
                        if !wait.is_zero() {
                            thread::sleep(wait);
                            continue;
                        }
                        let at = schedule.begin(Instant::now());
                        thread::sleep(work);
                        slots.insert((at.as_secs_f64() * fps as f64) as u64);
                    }
                    slots.len()
                })
                .join()
                .unwrap();
                let slots = (run.as_secs_f64() * fps as f64) as usize;
                println!(
                    "{fps} fps, {:2} ms captures: old loop {:5.1}% of slots, schedule {:5.1}%",
                    work.as_millis(),
                    100.0 * old.len() as f64 / slots as f64,
                    100.0 * new as f64 / slots as f64
                );
            }
        }
    }

    /// Paused time is cut out: recording time carries on where it stopped.
    #[test]
    fn pauses_move_the_schedule_later() {
        let t0 = Instant::now();
        let ms = |n: u64| Duration::from_millis(n);
        let mut schedule = Schedule::new(10, t0);
        assert_eq!(schedule.begin(t0), ms(50));
        assert_eq!(schedule.next_start(), t0 + ms(100));
        // Paused 30 ms into slot 1 for 2 s (looked at every 30 ms meanwhile).
        schedule.pause(t0 + ms(130));
        schedule.pause(t0 + ms(160));
        schedule.resume(t0 + ms(2130));
        schedule.resume(t0 + ms(2200));
        assert_eq!(schedule.next_start(), t0 + ms(2100), "slot 1 had started before the pause");
        assert_eq!(schedule.begin(t0 + ms(2130)), ms(150));
        assert_eq!(schedule.next_start(), t0 + ms(2200));
        assert_eq!(schedule.begin(t0 + ms(2200)), ms(250));
        // Paused right after a capture, until well into the slot after.
        schedule.pause(t0 + ms(2210));
        schedule.resume(t0 + ms(3000));
        assert_eq!(schedule.next_start(), t0 + ms(3090));
        assert_eq!(schedule.begin(t0 + ms(3090)), ms(350));
    }
}
