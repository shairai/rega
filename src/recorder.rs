//! Screen video recording. A capture thread grabs the region at the target
//! frame rate and hands frames to an encoder thread (H.264/MP4 or GIF).

use std::path::PathBuf;
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

struct TimedFrame {
    image: RgbaImage,
    /// Recording time (pauses excluded) at which this frame was captured.
    at: Duration,
}

impl Recorder {
    pub fn start(cfg: RecordConfig) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let point = (cfg.monitor_origin.0 + 1, cfg.monitor_origin.1 + 1);
        // Fail fast here; the capture thread looks the monitor up again because
        // `Monitor` holds a raw handle that is not `Send` on Windows.
        xcap::Monitor::from_point(point.0, point.1).context("recording monitor not found")?;

        let (tx, rx) = mpsc::channel::<TimedFrame>();
        let (stop_c, paused_c) = (stop.clone(), paused.clone());
        let fps = cfg.fps.clamp(1, 60);
        let (rx_, ry, rw, rh) = cfg.region;
        let origin = cfg.monitor_origin;
        let highlight = cfg.highlight_cursor;
        let max_width = cfg.max_width.max(320);

        let capture = thread::spawn(move || -> Result<()> {
            let monitor = xcap::Monitor::from_point(point.0, point.1).context("recording monitor not found")?;
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            let mut rec_time = Duration::ZERO;
            let mut last = Instant::now();
            let mut target: Option<(u32, u32)> = None;
            while !stop_c.load(Ordering::Relaxed) {
                let tick = Instant::now();
                if paused_c.load(Ordering::Relaxed) {
                    last = Instant::now();
                    thread::sleep(Duration::from_millis(30));
                    continue;
                }
                rec_time += tick - last;
                last = tick;

                let mut img = monitor.capture_region(rx_, ry, rw, rh)?;
                let scale = img.width() as f32 / rw as f32;
                if highlight {
                    let (cx, cy) = crate::capture::cursor_position();
                    let lx = (cx - origin.0 - rx_ as i32) as f32 * scale;
                    let ly = (cy - origin.1 - ry as i32) as f32 * scale;
                    draw_cursor_halo(&mut img, lx, ly, 18.0 * scale);
                }
                let (tw, th) = *target.get_or_insert_with(|| even_fit(img.width(), img.height(), max_width));
                if img.width() != tw || img.height() != th {
                    img = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
                }
                if tx.send(TimedFrame { image: img, at: rec_time }).is_err() {
                    break;
                }
                if let Some(rest) = interval.checked_sub(tick.elapsed()) {
                    thread::sleep(rest);
                }
            }
            Ok(())
        });

        let cancel_c = cancel.clone();
        let format = cfg.format;
        let out_path = cfg.out_path;
        let handle = thread::spawn(move || -> Result<PathBuf> {
            let encoded = match format {
                VideoFormat::Mp4 => encode_mp4(rx, fps, &out_path, &cancel_c),
                VideoFormat::Gif => encode_gif(rx, &out_path, &cancel_c),
            };
            let captured = capture.join().unwrap_or_else(|_| Err(anyhow::anyhow!("capture thread panicked")));
            if cancel_c.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&out_path);
                anyhow::bail!("Recording discarded");
            }
            captured?;
            encoded?;
            Ok(out_path)
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

fn encode_mp4(rx: Receiver<TimedFrame>, fps: u32, path: &PathBuf, cancel: &AtomicBool) -> Result<()> {
    use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType};
    use openh264::formats::{RgbaSliceU8, YUVBuffer};

    let mut encoder: Option<Encoder> = None;
    let mut h264 = Vec::new();
    let mut written: u64 = 0;
    let mut dims = (0, 0);

    for frame in rx.iter() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (w, h) = (frame.image.width() as usize, frame.image.height() as usize);
        let enc = match &mut encoder {
            Some(e) => e,
            None => {
                dims = (w, h);
                let bps = ((w * h) as f64 * fps as f64 * 0.15).clamp(1_000_000.0, 20_000_000.0) as u32;
                let config = EncoderConfig::new()
                    .max_frame_rate(FrameRate::from_hz(fps as f32))
                    .bitrate(BitRate::from_bps(bps))
                    .usage_type(UsageType::ScreenContentRealTime);
                encoder.insert(Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?)
            }
        };
        // Keep wall-clock timing exact: duplicate frames when capture fell behind.
        let due = (frame.at.as_secs_f64() * fps as f64).floor() as u64 + 1;
        if due <= written {
            continue;
        }
        let yuv = YUVBuffer::from_rgb_source(RgbaSliceU8::new(frame.image.as_raw(), (w, h)));
        while written < due {
            enc.encode(&yuv)?.write_vec(&mut h264);
            written += 1;
        }
    }

    if written == 0 {
        anyhow::bail!("no frames were captured");
    }
    let file = std::fs::File::create(path)?;
    let mut muxer = minimp4::Mp4Muxer::new(file);
    muxer.init_video(dims.0 as i32, dims.1 as i32, false, "Snapquill recording");
    muxer.write_video_with_fps(&h264, fps);
    muxer.close();
    Ok(())
}

fn encode_gif(rx: Receiver<TimedFrame>, path: &PathBuf, cancel: &AtomicBool) -> Result<()> {
    let mut encoder: Option<gif::Encoder<std::fs::File>> = None;
    let mut pending: Option<TimedFrame> = None;

    let write = |encoder: &mut Option<gif::Encoder<std::fs::File>>, mut f: TimedFrame, delay: Duration| -> Result<()> {
        let (w, h) = (f.image.width() as u16, f.image.height() as u16);
        let enc = match encoder {
            Some(e) => e,
            None => {
                let mut e = gif::Encoder::new(std::fs::File::create(path)?, w, h, &[])?;
                e.set_repeat(gif::Repeat::Infinite)?;
                encoder.insert(e)
            }
        };
        let mut gf = gif::Frame::from_rgba_speed(w, h, &mut f.image, 10);
        gf.delay = ((delay.as_millis() / 10) as u16).max(2);
        enc.write_frame(&gf)?;
        Ok(())
    };

    for frame in rx.iter() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(prev) = pending.take() {
            let delay = frame.at.saturating_sub(prev.at);
            write(&mut encoder, prev, delay)?;
        }
        pending = Some(frame);
    }
    match pending {
        Some(last) => write(&mut encoder, last, Duration::from_millis(500)),
        None => anyhow::bail!("no frames were captured"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: u32) -> Receiver<TimedFrame> {
        let (tx, rx) = mpsc::channel();
        for i in 0..n {
            let image = RgbaImage::from_fn(320, 240, |x, y| image::Rgba([(x + i * 8) as u8, y as u8, 128, 255]));
            tx.send(TimedFrame { image, at: Duration::from_millis(i as u64 * 100) }).unwrap();
        }
        rx
    }

    #[test]
    fn encodes_mp4_and_gif() {
        let dir = std::env::temp_dir().join(format!("snapquill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = AtomicBool::new(false);
        let mp4 = dir.join("t.mp4");
        encode_mp4(frames(20), 10, &mp4, &cancel).unwrap();
        assert!(std::fs::metadata(&mp4).unwrap().len() > 1000);
        let gif = dir.join("t.gif");
        encode_gif(frames(5), &gif, &cancel).unwrap();
        assert!(image::open(&gif).is_ok());
        assert_eq!(even_fit(2881, 1801, 1920), (1920, 1200));
    }
}
