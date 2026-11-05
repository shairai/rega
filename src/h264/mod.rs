//! H.264 through the operating system's own codec: VideoToolbox on macOS and
//! Media Foundation on Windows. Apple and Microsoft license the H.264 patents
//! for the codecs they ship, so Rega carries no H.264 encoder or decoder of its
//! own. This module only turns pictures into H.264 and back: the recorder
//! muxes MP4 with lieff's minimp4 (C, through minimp4-sys), and the player
//! demuxes it with the `mp4` crate.

use std::sync::OnceLock;

use anyhow::{Result, ensure};
use image::RgbaImage;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as backend;

mod nv12;

/// Seconds between keyframes: short, so seeking in the player stays fast.
const KEYFRAME_SECONDS: u32 = 2;

/// The smallest side the system encoders accept; the recorder scales smaller
/// regions up to it.
pub const MIN_SIDE: u32 = 64;

/// The largest picture both system encoders accept (Media Foundation's H.264
/// encoder tops out at 4096×2304); the recorder scales larger regions down.
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 2304;

/// Whether the system has an H.264 codec, checked before a recording starts so
/// that a missing one (Windows N editions) is reported up front. Cheap enough
/// for the UI thread: no codec is started.
pub fn available() -> Result<()> {
    backend::available()
}

/// Start the system encoder once in the background, so the first recording
/// doesn't lose its first frames to the codec's start-up. Returns at once; only
/// the first call does anything.
pub fn warm_up() {
    warming();
}

/// The thread [`warm_up`] runs on: started by the first call, then the same
/// one for every call after (`None` if it couldn't be started). It encodes a
/// frame with a small encoder, which loads the codec and starts it up the
/// way a recording's would: on macOS that takes ~100 ms once per process,
/// and later encoders start in ~10.
fn warming() -> Option<&'static std::thread::JoinHandle<()>> {
    static WARMING: OnceLock<Option<std::thread::JoinHandle<()>>> = OnceLock::new();
    WARMING
        .get_or_init(|| {
            let spawned = std::thread::Builder::new().name("H.264 warm-up".into()).spawn(|| {
                let mut out = Vec::new();
                let warmed = Encoder::new(MIN_SIDE, MIN_SIDE, 30, 1_000_000, true).and_then(|mut encoder| {
                    encoder.encode(&vec![0; (MIN_SIDE * MIN_SIDE * 4) as usize], &mut out)?;
                    encoder.finish(&mut out)
                });
                if let Err(e) = warmed {
                    log::debug!("couldn't warm up the H.264 encoder: {e:#}");
                }
            });
            spawned.inspect_err(|e| log::debug!("couldn't start warming up the H.264 encoder: {e}")).ok()
        })
        .as_ref()
}

/// Whether an MP4 sample (4-byte NAL lengths) holds an IDR slice: a picture
/// decoding can start from. Sync samples from other apps may not (recovery
/// points).
pub(crate) fn has_idr(sample: &[u8]) -> bool {
    avcc_nals(sample, 4).iter().any(|nal| nal_type(nal) == 5)
}

/// Encodes RGBA frames at a constant frame rate into an H.264 Annex B stream.
pub struct Encoder {
    backend: backend::Encoder,
    /// Bytes in a frame: width × height × 4.
    frame_len: usize,
}

impl Encoder {
    /// `width` and `height` must be even and at least [`MIN_SIDE`]. `realtime`
    /// is for a live recording, which must keep up with the screen; otherwise
    /// (an export) the encoder may take its time.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, realtime: bool) -> Result<Self> {
        ensure!(
            width.is_multiple_of(2)
                && height.is_multiple_of(2)
                && (MIN_SIDE..=MAX_WIDTH).contains(&width)
                && (MIN_SIDE..=MAX_HEIGHT).contains(&height),
            "can't encode a {width}×{height} video: sides must be even, at least {MIN_SIDE} px and at most {MAX_WIDTH}×{MAX_HEIGHT}"
        );
        let backend = backend::Encoder::new(width, height, fps.max(1), bitrate, realtime)?;
        Ok(Self { backend, frame_len: width as usize * height as usize * 4 })
    }

    /// Encode the next frame: `width × height` RGBA pixels, shown for 1/fps s.
    /// Appends the whole pictures that are ready to `out` as Annex B: slices and
    /// parameter sets only, SPS and PPS before every keyframe. May append
    /// nothing while the system encoder holds frames back; [`Encoder::finish`]
    /// flushes them. Pictures come out in input order, one per frame, unless a
    /// live encoder had to drop one to keep up (logged). On an error, `out` may
    /// still have gained whole pictures: they're good video.
    pub fn encode(&mut self, rgba: &[u8], out: &mut Vec<u8>) -> Result<()> {
        ensure!(rgba.len() == self.frame_len, "a frame takes {} bytes of RGBA here, got {}", self.frame_len, rgba.len());
        self.backend.encode(rgba, out)
    }

    /// Flush the frames the encoder still holds into `out` (also on an error,
    /// as with [`Encoder::encode`]).
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        self.backend.finish(out)
    }
}

/// Decodes H.264 samples, as stored in MP4, into pictures.
pub struct Decoder(backend::Decoder);

impl Decoder {
    /// `sps` and `pps` are bare NAL units (no start code or length prefix).
    /// `width` × `height` is the display size the MP4 gives; the coded picture
    /// may be larger (1920×1088 for 1080p) and is cropped to it, or to the
    /// stream's own display size where that's smaller.
    pub fn new(sps: &[u8], pps: &[u8], width: u32, height: u32) -> Result<Self> {
        ensure!(!sps.is_empty() && !pps.is_empty(), "the video has no H.264 parameter sets");
        ensure!(width > 0 && height > 0, "the video is {width}×{height}");
        Ok(Self(backend::Decoder::new(sps, pps, width, height)?))
    }

    /// Decode the next sample in decode order: NAL units, each after a 4-byte
    /// big-endian length. `keyframe` marks samples that start a GOP. When `want`
    /// is false the sample is only decoded as a reference for later ones and no
    /// picture is made. Returns `None` when the decoder produced no picture for
    /// this sample (an empty one never gets to the decoder).
    pub fn decode(&mut self, sample: &[u8], keyframe: bool, want: bool) -> Result<Option<RgbaImage>> {
        if sample.is_empty() {
            return Ok(None);
        }
        self.0.decode(sample, keyframe, want)
    }
}

/// NAL unit type: the low 5 bits of the first byte.
pub(crate) fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// Whether `nal` is a slice that starts a picture: its first_mb_in_slice, the
/// first field of the slice header, is 0 (ue(v) "1").
fn starts_picture(nal: &[u8]) -> bool {
    matches!(nal_type(nal), 1 | 5) && nal.get(1).is_some_and(|b| b & 0x80 != 0)
}

/// Append `nal` (no start code) to an Annex B stream if it's a coded slice (1,
/// 5) or a parameter set (7, 8): all that an MP4's samples and its track header
/// need. The rest (SEI, access unit delimiters, filler) is dropped.
fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    if matches!(nal_type(nal), 1 | 5 | 7 | 8) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
}

/// The NAL units of an Annex B stream, without their start codes.
fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let end = starts.get(k + 1).map_or(data.len(), |&next| next - 3);
            // Trailing zeros belong to the next 4-byte start code (or are padding).
            let mut nal = &data[start..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            nal
        })
        .filter(|nal| !nal.is_empty())
        .collect()
}

/// The NAL units of an MP4 sample, each stored after a `len_size`-byte
/// big-endian length.
fn avcc_nals(data: &[u8], len_size: usize) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    while len_size > 0 && i + len_size <= data.len() {
        let len = data[i..i + len_size].iter().fold(0usize, |n, &b| n << 8 | b as usize);
        i += len_size;
        let end = i.saturating_add(len).min(data.len());
        if end > i {
            nals.push(&data[i..end]);
        }
        i = end;
    }
    nals
}

/// One coded picture from an encoder: its slices as an MP4 sample (each NAL
/// unit after a 4-byte length).
pub(crate) struct Picture {
    pub keyframe: bool,
    pub sample: Vec<u8>,
}

/// Split an encoder's Annex B output (whole pictures) into its parameter sets
/// and pictures, a new one at each slice that [starts one](starts_picture).
/// Only slices and parameter sets are expected (see [`push_nal`]).
pub(crate) fn split_pictures(stream: &[u8]) -> (Vec<&[u8]>, Vec<Picture>) {
    let (mut sets, mut pictures) = (Vec::new(), Vec::<Picture>::new());
    for nal in annexb_nals(stream) {
        match nal_type(nal) {
            7 | 8 => sets.push(nal),
            t @ (1 | 5) => {
                if starts_picture(nal) || pictures.is_empty() {
                    pictures.push(Picture { keyframe: false, sample: Vec::new() });
                }
                let picture = pictures.last_mut().expect("a picture");
                picture.keyframe |= t == 5;
                picture.sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                picture.sample.extend_from_slice(nal);
            }
            _ => {}
        }
    }
    (sets, pictures)
}

/// Rewrite an MP4 sample stored with `len_size`-byte NAL lengths to 4-byte
/// lengths, the form [`Decoder::decode`] takes.
pub(crate) fn avcc_to_4_byte_lengths(data: &[u8], len_size: usize) -> Vec<u8> {
    if len_size == 4 {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in avcc_nals(data, len_size) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// An MP4 sample (4-byte lengths) as Annex B, appended to `out`.
#[cfg_attr(not(windows), allow(dead_code))]
fn avcc_to_annexb(data: &[u8], out: &mut Vec<u8>) {
    for nal in avcc_nals(data, 4) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
}

/// The most bytes of a GOP that [`Replay`] keeps.
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_REPLAY: usize = 64 << 20;

/// The samples a decoder was fed since the last keyframe, as fed, to bring it
/// back to the same point after a drain resets it (Media Foundation's). At
/// most [`MAX_REPLAY`] bytes: a video whose only IDR is its first frame (x264
/// intra refresh) would otherwise keep a copy of everything played. Past that
/// the GOP is let go, and decoding has to start again at its keyframe.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Default)]
struct Replay {
    samples: Vec<Vec<u8>>,
    bytes: usize,
    /// Samples were let go: there's no replaying until the next keyframe.
    lost: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Replay {
    /// Start again at a keyframe.
    fn restart(&mut self) {
        self.samples.clear();
        (self.bytes, self.lost) = (0, false);
    }

    /// Whether nothing was fed since the keyframe.
    fn is_empty(&self) -> bool {
        self.samples.is_empty() && !self.lost
    }

    /// Keep `sample`, the next one fed.
    fn push(&mut self, sample: Vec<u8>) {
        if self.lost || self.bytes + sample.len() > MAX_REPLAY {
            *self = Self { lost: true, ..Self::default() };
        } else {
            self.bytes += sample.len();
            self.samples.push(sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn splits_annexb_and_avcc_streams() {
        // 4- and 3-byte start codes, with a trailing zero after the second unit.
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5];
        let nals = annexb_nals(&stream);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);

        let sample = [0, 0, 0, 2, 0x41, 9, 0, 0, 0, 1, 0x06];
        assert_eq!(avcc_nals(&sample, 4), vec![&[0x41, 9][..], &[0x06][..]]);
        let two_byte = [0, 2, 0x41, 9, 0, 1, 0x06];
        assert_eq!(avcc_to_4_byte_lengths(&two_byte, 2), sample.to_vec());
        // A truncated length doesn't read past the end.
        assert_eq!(avcc_nals(&[0, 0, 0, 9, 0x41], 4), vec![&[0x41][..]]);

        let mut annexb = Vec::new();
        avcc_to_annexb(&sample, &mut annexb);
        assert_eq!(annexb, [0, 0, 0, 1, 0x41, 9, 0, 0, 0, 1, 0x06]);
    }

    #[test]
    fn splits_encoder_output_into_pictures() {
        // An IDR in two slices (the second's first_mb_in_slice isn't 0), then a P picture.
        let mut stream = Vec::new();
        for nal in [&[0x67, 1][..], &[0x68, 2], &[0x65, 0x88, 1], &[0x65, 0x20, 2], &[0x41, 0x9a, 3]] {
            push_nal(&mut stream, nal);
        }
        let (sets, pictures) = split_pictures(&stream);
        assert_eq!(sets, vec![&[0x67, 1][..], &[0x68, 2][..]]);
        assert_eq!(pictures.len(), 2);
        assert!(pictures[0].keyframe && !pictures[1].keyframe);
        assert_eq!(pictures[0].sample, [0, 0, 0, 3, 0x65, 0x88, 1, 0, 0, 0, 3, 0x65, 0x20, 2]);
        assert_eq!(pictures[1].sample, [0, 0, 0, 3, 0x41, 0x9a, 3]);
    }

    #[test]
    fn finds_the_slices_that_start_pictures() {
        // first_mb_in_slice 0 is the top bit after the NAL header byte.
        assert!(starts_picture(&[0x65, 0x88]) && starts_picture(&[0x41, 0x9a]));
        assert!(!starts_picture(&[0x65, 0x20]) && !starts_picture(&[0x41, 0x7f]), "a picture's later slices");
        assert!(!starts_picture(&[0x67, 0x80]) && !starts_picture(&[0x06, 0x80]), "not slices");
        assert!(!starts_picture(&[0x65]) && !starts_picture(&[]), "no slice header");
    }

    #[test]
    fn keeps_only_slices_and_parameter_sets() {
        let mut out = Vec::new();
        for nal in [&[0x67, 1][..], &[0x68, 2], &[0x06, 3], &[0x09, 0x10], &[0x65, 4], &[0x41, 5], &[0x0c, 0xff]] {
            push_nal(&mut out, nal);
        }
        let types: Vec<u8> = annexb_nals(&out).iter().map(|n| nal_type(n)).collect();
        assert_eq!(types, [7, 8, 5, 1]);
    }

    fn psnr(a: &RgbaImage, b: &RgbaImage) -> f64 {
        let mut sum = 0.0;
        let mut count = 0.0;
        for (p, q) in a.pixels().zip(b.pixels()) {
            for c in 0..3 {
                let d = p[c] as f64 - q[c] as f64;
                sum += d * d;
                count += 1.0;
            }
        }
        10.0 * (255.0 * 255.0 / (sum / count).max(1e-9)).log10()
    }

    /// A gradient with a white square moving across it.
    fn test_frame(w: u32, h: u32, i: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let (sx, sy) = (8 + i * 3 % (w - 24), h / 3);
            if (sx..sx + 16).contains(&x) && (sy..sy + 16).contains(&y) {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 96, 255])
            }
        })
    }

    /// SPS, PPS, and the pictures of an encoder's output.
    fn into_samples(stream: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<Picture>) {
        let nals = annexb_nals(stream);
        assert!(nals.iter().all(|nal| matches!(nal_type(nal), 1 | 5 | 7 | 8)), "only slices and parameter sets");
        let (sets, pictures) = split_pictures(stream);
        let set = |t| sets.iter().find(|nal| nal_type(nal) == t).map_or_else(Vec::new, |nal| nal.to_vec());
        (set(7), set(8), pictures)
    }

    #[test]
    fn round_trips_through_the_system_codec() {
        // 330×250 isn't a multiple of 16, so the decoder must crop; 64×64 is the minimum.
        for (w, h, realtime) in [(320, 240, true), (330, 250, false), (MIN_SIDE, MIN_SIDE, true)] {
            let (fps, n) = (30, 75);
            let mut enc = Encoder::new(w, h, fps, 2_000_000, realtime).unwrap();
            let mut stream = Vec::new();
            for i in 0..n {
                enc.encode(test_frame(w, h, i).as_raw(), &mut stream).unwrap();
            }
            enc.finish(&mut stream).unwrap();

            let (sps, pps, samples) = into_samples(&stream);
            assert!(!sps.is_empty() && !pps.is_empty(), "{w}×{h}: SPS and PPS");
            assert_eq!(samples.len(), n as usize, "{w}×{h}: one picture per frame, none dropped");
            assert!(samples[0].keyframe, "{w}×{h}: starts with a keyframe");
            let keyframes = samples.iter().filter(|s| s.keyframe).count();
            assert!(keyframes >= 2, "{w}×{h}: a keyframe at least every {KEYFRAME_SECONDS} s, got {keyframes}");

            // A container that overstates the size gets the stream's own.
            let mut overstated = Decoder::new(&sps, &pps, w + 16, h + 16).unwrap();
            let first = overstated.decode(&samples[0].sample, true, true).unwrap().expect("first picture");
            assert_eq!(first.dimensions(), (w, h), "{w}×{h}: cropped to the stream's size");

            let mut dec = Decoder::new(&sps, &pps, w, h).unwrap();
            for (i, Picture { keyframe, sample }) in samples.iter().enumerate() {
                let want = i % 10 == 0 || i + 1 == samples.len();
                let picture = dec.decode(sample, *keyframe, want).unwrap();
                if want {
                    let picture = picture.unwrap_or_else(|| panic!("{w}×{h}: no picture for frame {i}"));
                    assert_eq!(picture.dimensions(), (w, h));
                    let db = psnr(&picture, &test_frame(w, h, i as u32));
                    assert!(db > 30.0, "{w}×{h} frame {i}: {db:.1} dB");
                }
            }
        }
    }

    #[test]
    fn rejects_odd_tiny_and_huge_sizes() {
        for (w, h) in [(321, 240), (MIN_SIDE - 2, 240), (MAX_WIDTH + 2, 240), (640, MAX_HEIGHT + 2)] {
            assert!(Encoder::new(w, h, 30, 1_000_000, false).is_err(), "{w}×{h}");
        }
    }

    /// The checks both backends rely on are made here, once: a frame of the
    /// wrong size, missing parameter sets or size, and empty samples.
    #[test]
    fn checks_what_it_hands_the_system_codec() {
        let (w, h) = (MIN_SIDE, MIN_SIDE);
        let mut enc = Encoder::new(w, h, 30, 1_000_000, false).unwrap();
        let mut stream = Vec::new();
        for len in [0, (w * h * 4 - 4) as usize, (w * h * 4 + 4) as usize] {
            assert!(enc.encode(&vec![0; len], &mut stream).is_err(), "a {len}-byte frame");
        }
        for i in 0..3 {
            enc.encode(test_frame(w, h, i).as_raw(), &mut stream).unwrap();
        }
        enc.finish(&mut stream).unwrap();
        let (sps, pps, samples) = into_samples(&stream);
        assert_eq!(samples.len(), 3, "the rejected frames left no trace");

        assert!(Decoder::new(&[], &pps, w, h).is_err() && Decoder::new(&sps, &[], w, h).is_err(), "no parameter sets");
        assert!(Decoder::new(&sps, &pps, 0, h).is_err() && Decoder::new(&sps, &pps, w, 0).is_err(), "no size");
        let mut dec = Decoder::new(&sps, &pps, w, h).unwrap();
        assert!(dec.decode(&[], true, true).unwrap().is_none(), "an empty sample has no picture");
        let first = dec.decode(&samples[0].sample, true, true).unwrap();
        assert_eq!(first.map(|p| p.dimensions()), Some((w, h)), "and leaves the decoder as it was");
    }

    #[test]
    fn the_system_has_a_codec() {
        available().unwrap();
    }

    /// Warming up returns at once, and starts one encoder however often it's
    /// called.
    #[test]
    fn warms_up_once_in_the_background() {
        let started = std::time::Instant::now();
        for _ in 0..3 {
            warm_up();
        }
        let returned = started.elapsed();
        assert!(returned < std::time::Duration::from_millis(50), "returned after {returned:?}");
        let thread = warming().expect("a warm-up thread");
        while !thread.is_finished() {
            assert!(started.elapsed() < std::time::Duration::from_secs(30), "still warming up");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        warm_up();
        assert!(warming().is_some_and(|again| std::ptr::eq(again, thread) && again.is_finished()), "no second warm-up");
    }

    /// What warming up saves a recording: a fresh process's first encoder (at
    /// a recording's size) made and fed its first frame, with and without a
    /// warm-up before. Each run is a process of its own, as the codec starts
    /// up once per process. `cargo test --release warm_up_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn warm_up_speed() {
        use std::time::{Duration, Instant};
        const CHILD: &str = "REGA_TEST_WARM_UP";
        let Some(warm) = std::env::var_os(CHILD) else {
            let name = format!("{}::warm_up_speed", module_path!().split_once("::").unwrap().1);
            for warm in ["no", "yes"].repeat(5) {
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([name.as_str(), "--exact", "--ignored", "--nocapture"])
                    .env(CHILD, warm)
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&child.stdout).lines().filter(|line| line.starts_with("warm-up")).for_each(|line| println!("{line}"));
            }
            return;
        };
        let mut before = "skipped".to_string();
        if warm == "yes" {
            let started = Instant::now();
            warm_up();
            while warming().is_some_and(|thread| !thread.is_finished()) {
                std::thread::sleep(Duration::from_millis(1));
            }
            before = format!("first, {:.0?}", started.elapsed());
        }
        let (w, h) = (1920, 1246);
        let frame = test_frame(w, h, 0);
        let started = Instant::now();
        let mut enc = Encoder::new(w, h, 30, 10_000_000, true).unwrap();
        let made = started.elapsed();
        enc.encode(frame.as_raw(), &mut Vec::new()).unwrap();
        let first = started.elapsed() - made;
        println!("warm-up {before}: {w}×{h} encoder made in {made:.1?}, first frame in {first:.1?}");
    }

    /// A GOP is kept for replaying up to its limit; past it, it's let go
    /// until the next keyframe.
    #[test]
    fn keeps_gops_for_replay_up_to_a_limit() {
        let mut gop = Replay::default();
        assert!(gop.is_empty());
        // Zeroed allocations: no memory is touched.
        for _ in 0..4 {
            gop.push(vec![0; MAX_REPLAY / 4]);
        }
        assert!(!gop.lost && gop.samples.len() == 4 && gop.bytes == MAX_REPLAY);
        gop.push(vec![0; 1]);
        assert!(gop.lost && gop.samples.is_empty() && gop.bytes == 0, "let go once over");
        gop.push(vec![0; 1]);
        assert!(gop.lost && gop.samples.is_empty() && !gop.is_empty(), "until the next keyframe");
        gop.restart();
        assert!(gop.is_empty() && !gop.lost);
        gop.push(vec![0; 9]);
        assert!(!gop.lost && gop.samples.len() == 1 && gop.bytes == 9 && !gop.is_empty());
    }
}
