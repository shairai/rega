//! In-app video preview. Rega's recordings (H.264 MP4 and GIF) are decoded on a
//! background thread; the UI only uploads the latest frame and draws controls.
//!
//! Neither format is held decoded. An MP4 is indexed, and each picture is read
//! from the file when it's decoded; a GIF keeps its frames compressed, as
//! stored, with a composited picture every so often to seek from. Videos from
//! elsewhere may be damaged or hostile: reading one is an error at worst, never
//! a crash, a hang or a runaway allocation.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui::{self, Align2, Color32, Pos2, Rect, Sense, TextureHandle, Vec2};
use image::RgbaImage;

use crate::config::GifQuality;
use crate::library::part_path;
use crate::platform;
use crate::theme::{self, icon};

/// The most frames an MP4 may have: about 19 hours at 60 fps. The index takes
/// 24 bytes a frame, whatever count a damaged file claims.
const MAX_SAMPLES: u64 = 1 << 22;

/// The largest MP4 sample read: far more than any H.264 picture takes.
const MAX_SAMPLE_BYTES: u32 = 64 << 20;

/// The largest GIF picture (canvas or frame) decoded, in RGBA bytes.
const MAX_GIF_BYTES: u64 = 256 << 20;

/// The most frames read from a GIF: hours of animation. Each takes some 150
/// bytes besides its data, so a file of tiny frames can't claim much more
/// memory than its own size.
const MAX_GIF_FRAMES: usize = 1 << 20;

/// What the pictures a GIF keeps to seek from may take together.
const SNAPSHOT_BYTES: u64 = 128 << 20;

/// How often playback looks at a GIF that's still being read, so its timeline
/// grows on screen.
const SCAN_POLL: Duration = Duration::from_millis(50);

/// Lock `mutex` even if a thread panicked holding it: what these locks guard
/// stays consistent between statements.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `t` seconds as a duration; 0 for anything negative or not finite.
fn secs(t: f64) -> Duration {
    Duration::try_from_secs_f64(t).unwrap_or_default()
}

/// `job`'s result, or an error saying `what` crashed if it panicked: a damaged
/// file must not take a thread (the UI's, the thumbnailer's) down with it.
fn caught<T>(what: &str, job: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(job)).unwrap_or_else(|_| Err(anyhow::anyhow!("{what} crashed")))
}

enum Source {
    Mp4(Video),
    Gif(Gif),
}

impl Source {
    fn len(&self) -> usize {
        match self {
            Source::Mp4(video) => video.samples.len(),
            Source::Gif(gif) => lock(&gif.frames).frames.len(),
        }
    }

    /// When frame `i` is shown, in seconds from the start.
    fn time(&self, i: usize) -> f64 {
        match self {
            Source::Mp4(video) => video.samples.get(i).map_or(0.0, |s| s.time),
            Source::Gif(gif) => lock(&gif.frames).frames.get(i).map_or(0.0, |f| f.time),
        }
    }

    fn duration(&self) -> f64 {
        match self {
            Source::Mp4(video) => video.duration,
            Source::Gif(gif) => lock(&gif.frames).duration,
        }
    }

    /// How many frames are known, and whether that's all of them: a GIF's
    /// are still being read while the player already shows the first ones.
    /// Both at once, as reading may end between two looks.
    fn known(&self) -> (usize, bool) {
        match self {
            Source::Mp4(video) => (video.samples.len(), true),
            Source::Gif(gif) => {
                let table = lock(&gif.frames);
                (table.frames.len(), table.done)
            }
        }
    }

    /// The size the file gives its pictures. Decoded ones are never larger:
    /// the decoders crop to it, and a GIF's frames are drawn on its screen.
    fn size(&self) -> (u32, u32) {
        match self {
            Source::Mp4(video) => video.size,
            Source::Gif(gif) => (u32::from(gif.screen.width), u32::from(gif.screen.height)),
        }
    }

    /// The frame on screen at `t` seconds: the last one to start by then.
    fn index_at(&self, t: f64) -> usize {
        let after = match self {
            Source::Mp4(video) => video.samples.partition_point(|s| s.time <= t),
            Source::Gif(gif) => lock(&gif.frames).frames.partition_point(|f| f.time <= t),
        };
        after.saturating_sub(1)
    }
}

/// How much of a GIF `open_gif` reads before returning.
#[derive(Clone, Copy)]
enum Scan {
    /// The first frame only (a thumbnail).
    First,
    /// Every frame (an export, which then decodes them in order).
    All,
    /// The first frame, decoded to check it; a thread of its own reads the rest
    /// while the player already shows it.
    Background,
}

fn open_media(path: &Path, scan: Scan) -> Result<Source> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "gif" {
        // Under `catch_unwind`, as the `mp4` crate's work is.
        caught("the GIF reader", || open_gif(path, scan)).map(Source::Gif)
    } else {
        open_mp4(path).map(Source::Mp4)
    }
}

/// A video to play or trim frame by frame. The player shows and trims pictures
/// in the order they're stored, so one with B-frames (made by another app) is
/// left to the system's player; its first frame still makes a thumbnail.
fn open_sequential(path: &Path, scan: Scan) -> Result<Source> {
    let source = open_media(path, scan)?;
    anyhow::ensure!(
        !matches!(&source, Source::Mp4(video) if video.reordered),
        "this video stores its frames out of order (B-frames), which Rega's player doesn't support"
    );
    Ok(source)
}

// ---------- MP4 ----------

/// Where one MP4 sample (a picture's H.264 NAL units) is in the file.
#[derive(Clone, Copy)]
struct Sample {
    offset: u64,
    size: u32,
    /// Decode time, in seconds.
    time: f64,
    /// Marked a sync sample. Not every one holds an IDR picture to restart
    /// decoding at (see `Decoder::restart_point`).
    sync: bool,
}

/// An MP4's video track: an index of its samples, which are read from the
/// file only when they're decoded.
struct Video {
    file: Mutex<File>,
    samples: Vec<Sample>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    size: (u32, u32),
    /// Bytes before each NAL unit's length; Rega writes 4, other apps may not.
    len_size: usize,
    /// Pictures stored out of display order (B-frames, from other apps).
    reordered: bool,
    duration: f64,
}

impl Video {
    /// Sample `i` into `buf`, as the decoder takes it: each NAL unit after a
    /// 4-byte length.
    fn read(&self, i: usize, buf: &mut Vec<u8>) -> Result<()> {
        let sample = self.samples.get(i).with_context(|| format!("the video has no frame {i}"))?;
        buf.resize(sample.size as usize, 0);
        let mut file = lock(&self.file);
        file.seek(SeekFrom::Start(sample.offset))
            .and_then(|_| file.read_exact(buf))
            .with_context(|| format!("reading frame {i} of the video"))?;
        drop(file);
        if self.len_size != 4 {
            *buf = crate::h264::avcc_to_4_byte_lengths(buf, self.len_size);
        }
        Ok(())
    }
}

/// Open an MP4's video track. A damaged or hostile file is an error, never a
/// crash or a hang: `check_boxes` first rules out the box layouts that send the
/// `mp4` crate into an endless loop, all the crate's work runs under
/// `catch_unwind` (some damaged tables make it panic), and `sample_index`
/// checks every sample against the file. Only the index is read: the samples
/// stay on disk until they're decoded.
fn open_mp4(path: &Path) -> Result<Video> {
    let file = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut reader = Tracked::new(file);
    let parsed = caught("the MP4 reader", || {
        check_boxes(&mut reader, len)?;
        reader.rewind()?;
        let mp4 = mp4::Mp4Reader::read_header(&mut reader, len)?;
        let track = mp4
            .tracks()
            .values()
            .filter(|t| matches!(t.track_type(), Ok(mp4::TrackType::Video)))
            .min_by_key(|t| t.track_id())
            .context("no video track")?;
        let (sps, pps) = (track.sequence_parameter_set()?.to_vec(), track.picture_parameter_set()?.to_vec());
        let size = (track.width() as u32, track.height() as u32);
        let stbl = &track.trak.mdia.minf.stbl;
        let len_size = stbl.stsd.avc1.as_ref().map_or(4, |avc1| avc1.avcc.length_size_minus_one as usize + 1);
        let tables = SampleTables {
            sample_size: stbl.stsz.sample_size,
            sample_count: stbl.stsz.sample_count,
            sizes: &stbl.stsz.sample_sizes,
            times: stbl.stts.entries.iter().map(|e| (e.sample_count, e.sample_delta)).collect(),
            offsets: stbl.ctts.as_ref().map(|ctts| ctts.entries.iter().map(|e| (e.sample_count, e.sample_offset)).collect()),
            chunks: stbl.stsc.entries.iter().map(|e| (e.first_chunk, e.samples_per_chunk)).collect(),
            chunk_offsets: match (&stbl.stco, &stbl.co64) {
                (Some(stco), _) => ChunkOffsets::Short(&stco.entries),
                (None, Some(co64)) => ChunkOffsets::Long(&co64.entries),
                (None, None) => anyhow::bail!("the video has no chunk offsets"),
            },
            sync: stbl.stss.as_ref().map(|stss| stss.entries.as_slice()),
        };
        let index = sample_index(&tables, track.timescale(), len)?;
        Ok((index, sps, pps, size, len_size))
    })
    .context("not a readable MP4")?;
    let (index, sps, pps, size, len_size) = parsed;
    Ok(Video {
        file: Mutex::new(reader.into_inner()),
        samples: index.samples,
        sps,
        pps,
        size,
        len_size,
        reordered: index.reordered,
        duration: index.duration,
    })
}

/// A track's sample table (`stbl`), as plain lists: the `mp4` crate's box
/// types can't be named outside it.
struct SampleTables<'a> {
    /// Every sample's size when not 0 (`stsz`); else `sizes` lists them.
    sample_size: u32,
    sample_count: u32,
    sizes: &'a [u32],
    /// Runs of (samples, decode time delta) (`stts`).
    times: Vec<(u32, u32)>,
    /// Runs of (samples, composition offset) (`ctts`), with B-frames.
    offsets: Option<Vec<(u32, i32)>>,
    /// Runs of (first chunk, samples per chunk) (`stsc`).
    chunks: Vec<(u32, u32)>,
    chunk_offsets: ChunkOffsets<'a>,
    /// Sync sample numbers, from 1 (`stss`); without them, every sample is one.
    sync: Option<&'a [u32]>,
}

/// Where each chunk starts (`stco`, or `co64` in files past 4 GB).
enum ChunkOffsets<'a> {
    Short(&'a [u32]),
    Long(&'a [u64]),
}

struct SampleIndex {
    samples: Vec<Sample>,
    duration: f64,
    reordered: bool,
}

/// Index a track's samples from its sample tables, in one pass over them,
/// refusing tables that contradict each other or the `len`-byte file. (The
/// `mp4` crate's own sample lookup scans the tables again for every sample,
/// and panics on some damaged ones.)
fn sample_index(t: &SampleTables, timescale: u32, len: u64) -> Result<SampleIndex> {
    use anyhow::ensure;
    let count = u64::from(t.sample_count);
    ensure!(count > 0, "the video has no frames");
    ensure!(count <= len.min(MAX_SAMPLES), "the video's tables claim {count} frames, more than it can hold");
    let sizes = (t.sample_size == 0).then_some(t.sizes);
    ensure!(sizes.is_none_or(|sizes| sizes.len() as u64 >= count), "the video's size table is short");
    let total = match sizes {
        Some(sizes) => sizes.iter().map(|&s| u64::from(s)).sum(),
        None => u64::from(t.sample_size) * count,
    };
    ensure!(total <= len, "the video's frames add up to more than the file holds");
    let timed: u64 = t.times.iter().map(|&(n, _)| u64::from(n)).sum();
    ensure!(timed >= count, "the video's timing table covers {timed} of its {count} frames");
    let chunks = &t.chunks;
    ensure!(
        !chunks.is_empty()
            && chunks.iter().all(|&(first, per_chunk)| first >= 1 && per_chunk > 0)
            && chunks.windows(2).all(|pair| pair[0].0 <= pair[1].0),
        "the video's chunk table is damaged"
    );
    let offsets: Box<dyn Iterator<Item = u64>> = match t.chunk_offsets {
        ChunkOffsets::Short(offsets) => Box::new(offsets.iter().map(|&o| u64::from(o))),
        ChunkOffsets::Long(offsets) => Box::new(offsets.iter().copied()),
    };
    let sync = t.sync.map(|numbers| {
        let mut numbers = numbers.to_vec();
        numbers.sort_unstable();
        numbers
    });
    let mut deltas = per_sample(&t.times);
    let timescale = f64::from(timescale.max(1));

    let mut samples = Vec::with_capacity(count as usize);
    let (mut ticks, mut last_delta, mut entry) = (0u64, 0, 0);
    'chunks: for (c, chunk_offset) in offsets.enumerate() {
        let chunk = c as u64 + 1;
        while chunks.get(entry + 1).is_some_and(|&(first, _)| u64::from(first) <= chunk) {
            entry += 1;
        }
        let (first, per_chunk) = chunks[entry];
        // Chunks before the first run's hold nothing, as the `mp4` crate reads it.
        if u64::from(first) > chunk {
            continue;
        }
        let mut offset = chunk_offset;
        for _ in 0..per_chunk {
            let i = samples.len();
            if i as u64 == count {
                break 'chunks;
            }
            let size = sizes.map_or(t.sample_size, |sizes| sizes[i]);
            ensure!(size <= MAX_SAMPLE_BYTES, "frame {i} of the video is {size} bytes, more than any picture");
            let end = offset.checked_add(u64::from(size)).filter(|&end| end <= len);
            let end = end.with_context(|| format!("frame {i} of the video lies outside the file"))?;
            let number = i as u32 + 1;
            samples.push(Sample {
                offset,
                size,
                time: ticks as f64 / timescale,
                sync: sync.as_ref().is_none_or(|numbers| numbers.binary_search(&number).is_ok()),
            });
            last_delta = deltas.next().unwrap_or(0);
            ticks += u64::from(last_delta);
            offset = end;
        }
    }
    // Samples past the last chunk are left out, as the `mp4` crate does.
    let last = samples.last().context("the video has no frames")?;
    let duration = last.time + f64::from(last_delta) / timescale;
    // Presentation times, which only differ from decode order with B-frames.
    let reordered = t.offsets.as_ref().is_some_and(|offsets| {
        let starts = per_sample(&t.times).scan(0u64, |t, delta| {
            let start = *t;
            *t += u64::from(delta);
            Some(start)
        });
        let offsets = per_sample(offsets).chain(std::iter::repeat(0));
        is_reordered(starts.zip(offsets).take(samples.len()).map(|(start, offset)| start as i64 + i64::from(offset)))
    });
    Ok(SampleIndex { samples, duration, reordered })
}

/// The values of `(samples, value)` runs, one per sample.
fn per_sample<T: Copy>(runs: &[(u32, T)]) -> impl Iterator<Item = T> + '_ {
    runs.iter().flat_map(|&(n, value)| std::iter::repeat_n(value, n as usize))
}

/// Whether pictures, in the order they're stored, are shown out of that order.
fn is_reordered(presentation_times: impl IntoIterator<Item = i64>) -> bool {
    let mut times = presentation_times.into_iter();
    let Some(mut previous) = times.next() else { return false };
    times.any(|t| std::mem::replace(&mut previous, t) > t)
}

/// A buffered reader that knows where it is, so it can seek a short way (to
/// the next small box) within what it has read ahead, which a `BufReader`'s
/// own seek throws away: walking a file of many small boxes then costs a
/// system call per buffer, not a seek and a read per box.
struct Tracked<R> {
    inner: BufReader<R>,
    pos: u64,
}

impl<R: Read + Seek> Tracked<R> {
    /// Read `inner`, which is at its start.
    fn new(inner: R) -> Self {
        Self { inner: BufReader::new(inner), pos: 0 }
    }

    fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

impl<R: Read> Read for Tracked<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for Tracked<R> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(by) => self.pos.checked_add_signed(by),
            SeekFrom::End(_) => None,
        };
        match target.and_then(|at| Some((at, at.checked_signed_diff(self.pos)?))) {
            Some((at, by)) => {
                self.inner.seek_relative(by)?;
                self.pos = at;
            }
            None => self.pos = self.inner.seek(to)?,
        }
        Ok(self.pos)
    }
}

/// A box in the file: its type, where its content starts and where it ends.
struct BoxAt {
    kind: [u8; 4],
    content: u64,
    end: u64,
}

/// Read `buf.len()` bytes at `at`.
fn read_at<R: Read + Seek>(r: &mut R, at: u64, buf: &mut [u8]) -> Result<()> {
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(buf)?;
    Ok(())
}

/// The box at `pos`, which must fit before `end` (its parent's end). A size of
/// 0 is refused: the `mp4` crate loops forever on one inside another box.
fn box_at<R: Read + Seek>(r: &mut R, pos: u64, end: u64) -> Result<BoxAt> {
    let fits = |n: u64| pos.checked_add(n).is_some_and(|e| e <= end);
    anyhow::ensure!(fits(8), "a box at byte {pos} runs past its parent");
    let mut head = [0; 16];
    read_at(r, pos, &mut head[..8])?;
    let kind = [head[4], head[5], head[6], head[7]];
    let (header, size) = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
        1 => {
            anyhow::ensure!(fits(16), "a box at byte {pos} runs past its parent");
            read_at(r, pos + 8, &mut head[8..])?;
            (16, u64::from_be_bytes([head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15]]))
        }
        size => (8, u64::from(size)),
    };
    anyhow::ensure!(size != 0, "a box at byte {pos} has no size");
    anyhow::ensure!(size >= header, "a box at byte {pos} is smaller than its header");
    anyhow::ensure!(fits(size), "a box at byte {pos} runs past its parent");
    Ok(BoxAt { kind, content: pos + header, end: pos + size })
}

/// Walk the boxes `mp4` 0.14 parses, as it parses them, and refuse the ones
/// that break it: a child of size 0 (its child loops then read the same box
/// forever), one smaller than its own header, or one running past its parent
/// (which also lets a `meta` child claim any amount of memory). Fragmented
/// files are refused outright: the crate's sample lookup for them divides by
/// zero.
fn check_boxes<R: Read + Seek>(r: &mut R, len: u64) -> Result<()> {
    let mut pos = 0;
    while pos < len {
        let mut head = [0; 8];
        read_at(r, pos, &mut head)?;
        // Size 0 is "to the end of the file": the crate stops reading there.
        if head[..4] == [0; 4] {
            break;
        }
        let b = box_at(r, pos, u64::MAX)?;
        match &b.kind {
            b"moov" => {
                anyhow::ensure!(b.end <= len, "the movie box runs past the end of the file");
                check_children(r, &b)?;
            }
            b"moof" => anyhow::bail!("fragmented MP4s aren't supported"),
            // The crate seeks past others, mdat included, even past the end
            // (a cut-off file); its samples are checked when indexed.
            _ => {}
        }
        pos = b.end;
    }
    Ok(())
}

/// Check the children the `mp4` crate reads inside `b`, and theirs.
fn check_children<R: Read + Seek>(r: &mut R, b: &BoxAt) -> Result<()> {
    match &b.kind {
        // Lists of boxes, `ilst`'s items among them.
        b"moov" | b"trak" | b"mdia" | b"minf" | b"dinf" | b"stbl" | b"mvex" | b"udta" | b"ilst" | b"covr" | b"desc" | b"\xa9nam" | b"\xa9day" => {
            let mut pos = b.content;
            while pos < b.end {
                let child = box_at(r, pos, b.end)?;
                let parsed = match (&b.kind, &child.kind) {
                    (b"moov", b"trak" | b"mvex" | b"udta" | b"meta")
                    | (b"trak", b"edts" | b"mdia" | b"meta")
                    | (b"mdia", b"minf")
                    | (b"minf", b"dinf" | b"stbl")
                    | (b"dinf", b"dref")
                    | (b"stbl", b"stsd")
                    | (b"udta", b"meta") => true,
                    (b"ilst", item) => matches!(item, b"covr" | b"desc" | b"\xa9nam" | b"\xa9day"),
                    _ => false,
                };
                if parsed {
                    check_children(r, &child)?;
                }
                pos = child.end;
            }
            Ok(())
        }
        // Up to entry_count boxes, after the version, flags and count.
        b"dref" => {
            anyhow::ensure!(b.content + 8 <= b.end, "a data reference box is too small");
            let mut count = [0; 4];
            read_at(r, b.content + 4, &mut count)?;
            let mut pos = b.content + 8;
            for _ in 0..u32::from_be_bytes(count) {
                if pos >= b.end {
                    break;
                }
                pos = box_at(r, pos, b.end)?.end;
            }
            Ok(())
        }
        // Boxes the crate reads one child of, at a fixed place.
        b"edts" => one_child(r, b, 0).map(drop),
        b"stsd" => match one_child(r, b, 8)? {
            Some(entry) => check_children(r, &entry),
            None => Ok(()),
        },
        b"avc1" | b"hev1" | b"vp09" => one_child(r, b, 78).map(drop),
        b"mp4a" => one_child(r, b, 28).map(drop),
        b"meta" => check_meta(r, b),
        _ => Ok(()),
    }
}

/// The one child the crate reads `skip` bytes into `b`, if there's room for one.
fn one_child<R: Read + Seek>(r: &mut R, b: &BoxAt, skip: u64) -> Result<Option<BoxAt>> {
    let at = b.content + skip;
    if at + 8 > b.end {
        return Ok(None);
    }
    box_at(r, at, b.end).map(Some)
}

/// A `meta` box, as the crate reads it: after its version and flags (or
/// straight away in QuickTime's form, without them), every child, then `ilst`'s
/// under the `mdir` handler. Its loops don't check that children fit.
fn check_meta<R: Read + Seek>(r: &mut R, b: &BoxAt) -> Result<()> {
    anyhow::ensure!(b.content + 8 <= b.end, "a metadata box is too small");
    let mut head = [0; 8];
    read_at(r, b.content, &mut head)?;
    let first = match head {
        [0, 0, 0, 0, ..] => b.content + 4,
        [_, _, _, _, b'h', b'd', b'l', b'r'] => b.content,
        _ => anyhow::bail!("a metadata box has an unknown version"),
    };
    let mut handler = None;
    let mut pos = first;
    while pos < b.end {
        let child = box_at(r, pos, b.end)?;
        if &child.kind == b"hdlr" && child.content + 12 <= child.end {
            let mut kind = [0; 4];
            read_at(r, child.content + 8, &mut kind)?;
            handler = Some(kind);
        }
        pos = child.end;
    }
    if handler == Some(*b"mdir") {
        let mut pos = first;
        while pos < b.end {
            let child = box_at(r, pos, b.end)?;
            if &child.kind == b"ilst" {
                check_children(r, &child)?;
            }
            pos = child.end;
        }
    }
    Ok(())
}

// ---------- GIF ----------

/// One GIF frame as stored: `gif`'s frame with its LZW-compressed data, minimum
/// code size first (as `skip_frame_decoding` reads it).
struct GifFrame {
    /// When it's shown, in seconds from the start.
    time: f64,
    frame: Arc<gif::Frame<'static>>,
}

/// A GIF's frames read so far, and pictures to restart compositing from.
#[derive(Default)]
struct GifFrames {
    frames: Vec<GifFrame>,
    /// `(k, the canvas after frame k)`: what frame k + 1 is drawn over.
    snapshots: Vec<(usize, Arc<RgbaImage>)>,
    /// When the last frame read ends.
    duration: f64,
    /// Every frame has been read, or reading stopped at damage.
    done: bool,
}

/// What a GIF's frames are drawn on and decoded with.
#[derive(Clone)]
struct GifScreen {
    width: u16,
    height: u16,
    /// The global palette (RGB triples), if the file has one.
    palette: Vec<u8>,
}

struct Gif {
    screen: GifScreen,
    frames: Arc<Mutex<GifFrames>>,
}

/// Reads a GIF's frames, without decoding them, into `GifFrames`.
struct GifScan {
    reader: gif::Decoder<File>,
    /// When the next frame starts.
    time: f64,
}

impl GifScan {
    /// Read the next frame into `frames`; its index and the frame, or `None`
    /// at the end.
    fn next(&mut self, frames: &Mutex<GifFrames>) -> Result<Option<(usize, Arc<gif::Frame<'static>>)>> {
        anyhow::ensure!(lock(frames).frames.len() < MAX_GIF_FRAMES, "it has more than {MAX_GIF_FRAMES} frames");
        let Some(frame) = self.reader.read_next_frame()? else { return Ok(None) };
        let frame = Arc::new(frame.clone());
        let mut table = lock(frames);
        table.frames.push(GifFrame { time: self.time, frame: Arc::clone(&frame) });
        // Delays under 20 ms count as 20, so playback can't spin.
        self.time += (f64::from(frame.delay) / 100.0).max(0.02);
        table.duration = self.time;
        Ok(Some((table.frames.len() - 1, frame)))
    }
}

/// Open a GIF, reading as much of it as `scan` says.
fn open_gif(path: &Path, scan: Scan) -> Result<Gif> {
    let file = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let reader = options.read_info(file).context("not a readable GIF")?;
    let screen = GifScreen { width: reader.width(), height: reader.height(), palette: reader.global_palette().unwrap_or_default().to_vec() };
    let area = u64::from(screen.width) * u64::from(screen.height);
    anyhow::ensure!(area > 0 && area * 4 <= MAX_GIF_BYTES, "can't show a {}×{} GIF", screen.width, screen.height);
    let gif = Gif { screen, frames: Arc::default() };
    let mut reading = GifScan { reader, time: 0.0 };
    let (_, first) = reading.next(&gif.frames)?.context("the GIF has no frames")?;
    match scan {
        Scan::First => lock(&gif.frames).done = true,
        Scan::All => {
            loop {
                match reading.next(&gif.frames) {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!("{} is cut short: {e:#}", path.display());
                        break;
                    }
                }
            }
            lock(&gif.frames).done = true;
        }
        Scan::Background => {
            // Decoded now, while the app can still give the file to another viewer.
            let mut canvas = gif.screen.blank();
            composite(&mut canvas, &first, &gif.screen.pixels(&first)?, false);
            let (screen, frames) = (gif.screen.clone(), Arc::downgrade(&gif.frames));
            let policy = Snapshots::for_screen(&screen);
            let name = path.display().to_string();
            std::thread::spawn(move || {
                let table = frames.clone();
                read_rest(&table, &name, || scan_in_background(reading, screen, frames, canvas, policy, &name));
            });
        }
    }
    Ok(gif)
}

/// Run `read`, which reads the rest of a GIF into `frames`. However it ends,
/// with damage or a crash in the `gif` crate, the GIF then counts as read as
/// far as it got, so playback stops at its end instead of waiting on.
fn read_rest(frames: &Weak<Mutex<GifFrames>>, name: &str, read: impl FnOnce() -> Result<()>) {
    if let Err(e) = caught("the GIF reader", read) {
        log::warn!("{name} is cut short: {e:#}");
    }
    if let Some(table) = frames.upgrade() {
        lock(&table).done = true;
    }
}

/// The pictures a GIF's background scan keeps to seek from: one each time
/// `every` pixels' worth of frames were decoded since the last, so a seek
/// decodes at most about that much. Past `keep` of them, every other one goes
/// and the spacing doubles.
#[derive(Clone, Copy)]
struct Snapshots {
    every: u64,
    keep: usize,
}

impl Snapshots {
    fn for_screen(screen: &GifScreen) -> Self {
        let area = (u64::from(screen.width) * u64::from(screen.height)).max(1);
        Self { every: 8 * area, keep: (SNAPSHOT_BYTES / (4 * area)).clamp(2, 64) as usize }
    }
}

/// Decoding `frame`, in pixels: its area, plus a fixed cost for setting up.
fn frame_cost(frame: &gif::Frame) -> u64 {
    u64::from(frame.width) * u64::from(frame.height) + 4096
}

/// Read the rest of a GIF after its first frame, compositing each onto
/// `canvas` to keep `policy`'s pictures to seek from. Stops at the end, at
/// damage (an error), or once nothing holds `frames` any more: the player
/// was closed.
fn scan_in_background(
    mut reading: GifScan,
    screen: GifScreen,
    frames: Weak<Mutex<GifFrames>>,
    mut canvas: RgbaImage,
    policy: Snapshots,
    name: &str,
) -> Result<()> {
    let (mut every, mut work) = (policy.every, 0);
    loop {
        let Some(table) = frames.upgrade() else { return Ok(()) };
        let Some((k, frame)) = reading.next(&table)? else { return Ok(()) };
        match screen.pixels(&frame) {
            Ok(pixels) => drop(composite(&mut canvas, &frame, &pixels, false)),
            Err(e) => log::warn!("frame {k} of {name} can't be decoded: {e:#}"),
        }
        work += frame_cost(&frame);
        if work >= every {
            work = 0;
            let mut table = lock(&table);
            table.snapshots.push((k, Arc::new(canvas.clone())));
            if table.snapshots.len() > policy.keep {
                let mut odd = false;
                table.snapshots.retain(|_| {
                    odd = !odd;
                    !odd
                });
                every *= 2;
            }
        }
    }
}

impl GifScreen {
    fn blank(&self) -> RgbaImage {
        RgbaImage::new(u32::from(self.width), u32::from(self.height))
    }

    /// `frame`'s pixels: RGBA rows of its own rectangle, decoded by `gif` just
    /// as `image`'s GIF decoder has it decode them. The frame is written out as
    /// a one-frame GIF for that: gif 0.14's decoder for bare LZW data spins
    /// forever on a frame whose data ends early, where reading a file fails.
    fn pixels(&self, frame: &gif::Frame) -> Result<Vec<u8>> {
        let (w, h) = (u64::from(frame.width), u64::from(frame.height));
        anyhow::ensure!(w * h * 4 <= MAX_GIF_BYTES, "a {w}×{h} GIF frame is too large");
        let mut file = Vec::with_capacity(frame.buffer.len() + frame.buffer.len() / 255 + 1600);
        file.extend_from_slice(b"GIF89a");
        file.extend_from_slice(&self.width.to_le_bytes());
        file.extend_from_slice(&self.height.to_le_bytes());
        let flags = file.len();
        file.extend_from_slice(&[0, 0, 0]);
        if !self.palette.is_empty() {
            file[flags] = 0x80 | color_table(&mut file, &self.palette);
        }
        // Graphic control extension: disposal, transparency, delay.
        let control = (frame.dispose as u8) << 2 | u8::from(frame.needs_user_input) << 1 | u8::from(frame.transparent.is_some());
        file.extend_from_slice(&[0x21, 0xf9, 4, control]);
        file.extend_from_slice(&frame.delay.to_le_bytes());
        file.extend_from_slice(&[frame.transparent.unwrap_or(0), 0, 0x2c]);
        for v in [frame.left, frame.top, frame.width, frame.height] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        let flags = file.len();
        file.push(if frame.interlaced { 0x40 } else { 0 });
        if let Some(palette) = &frame.palette {
            file[flags] |= 0x80 | color_table(&mut file, palette);
        }
        // The minimum code size, the data in sub-blocks of up to 255 bytes, the trailer.
        let (&min_code_size, data) = frame.buffer.split_first().unwrap_or((&2, &[]));
        file.push(min_code_size);
        for block in data.chunks(255) {
            file.push(block.len() as u8);
            file.extend_from_slice(block);
        }
        file.extend_from_slice(&[0, 0x3b]);

        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(file.as_slice())?;
        decoder.next_frame_info()?.context("the GIF frame went missing")?;
        let mut pixels = vec![0; decoder.buffer_size()];
        decoder.read_into_buffer(&mut pixels)?;
        Ok(pixels)
    }
}

/// Append a color table holding `palette`; returns its size field. `gif` reads
/// tables of 2, 4, … 256 colors, so one it read fits exactly.
fn color_table(out: &mut Vec<u8>, palette: &[u8]) -> u8 {
    let colors = (palette.len() / 3).clamp(2, 256).next_power_of_two();
    let used = &palette[..palette.len().min(colors * 3)];
    out.extend_from_slice(used);
    out.resize(out.len() + colors * 3 - used.len(), 0);
    colors.trailing_zeros() as u8 - 1
}

/// Draw `frame` (`pixels`: RGBA rows of its rectangle) over `canvas` the way
/// `image`'s GIF decoder composites animations: where the frame is
/// transparent, what's below shows. Returns the picture shown, if `show`, and
/// leaves in `canvas` what the next frame is drawn over: the frame kept (Keep,
/// Any), its rectangle cleared (Background), or nothing of it (Previous).
fn composite(canvas: &mut RgbaImage, frame: &gif::Frame, pixels: &[u8], show: bool) -> Option<RgbaImage> {
    use gif::DisposalMethod::{Any, Background, Keep, Previous};
    let mut shown = show.then(|| canvas.clone());
    let (cw, ch) = (canvas.width() as usize, canvas.height() as usize);
    let (left, top, fw, fh) = (usize::from(frame.left), usize::from(frame.top), usize::from(frame.width), usize::from(frame.height));
    // The part of the frame on the canvas; the rest is never seen.
    let (x1, y1) = ((left + fw).min(cw), (top + fh).min(ch));
    if left >= x1 || pixels.len() < fw * fh * 4 {
        return shown;
    }
    let row = (x1 - left) * 4;
    for y in top..y1 {
        let src = pixels[(y - top) * fw * 4..][..row].as_chunks::<4>().0;
        let at = (y * cw + left) * 4;
        let below = (**canvas)[at..at + row].as_chunks_mut::<4>().0;
        let mut out = shown.as_mut().map(|s| (**s)[at..at + row].as_chunks_mut::<4>().0);
        for (x, (s, b)) in src.iter().zip(below.iter_mut()).enumerate() {
            let px = if s[3] == 0 { *b } else { *s };
            if let Some(out) = &mut out {
                out[x] = px;
            }
            match frame.dispose {
                Any | Keep => *b = px,
                Background => *b = [0; 4],
                Previous => {}
            }
        }
    }
    shown
}

/// Where compositing a GIF got to: the canvas after frame `at` (blank before
/// the first).
struct GifCursor {
    canvas: RgbaImage,
    at: Option<usize>,
}

// ---------- decoding ----------

/// A set of sample numbers, a bit each: at most 512 KB, however many
/// samples a video has (a hash set of them took over 100 MB).
#[derive(Default)]
struct SampleSet(Vec<u64>);

impl SampleSet {
    fn contains(&self, k: usize) -> bool {
        self.0.get(k / 64).is_some_and(|bits| bits >> (k % 64) & 1 == 1)
    }

    fn insert(&mut self, k: usize) {
        if k / 64 >= self.0.len() {
            self.0.resize(k / 64 + 1, 0);
        }
        self.0[k / 64] |= 1 << (k % 64);
    }
}

/// Turns frame indices into pictures. For an MP4 it keeps one system decoder
/// session going: the next frame just continues it, a seek restarts it at the
/// IDR picture at or before the frame, and only an error (or a video with no
/// IDR to restart at) costs a new session. For a GIF it composites from where
/// it last was, or from the closest picture kept to seek from.
struct Decoder {
    h264: Option<crate::h264::Decoder>,
    /// Index of the last sample fed to `h264`.
    last: Option<usize>,
    /// Sync samples found to hold no IDR picture, such as x264's intra-refresh
    /// recovery points: decoding can't restart there.
    not_idr: SampleSet,
    /// The last frame a restart point was found for, and that restart point:
    /// no frame after it, up to the frame, is one.
    walked: Option<(usize, Option<usize>)>,
    /// Samples that failed to decode on a fresh session. Frames that need one
    /// fail at once, instead of decoding up to it again and again.
    broken: BTreeSet<usize>,
    /// The sample last read, and which one it is.
    buf: Vec<u8>,
    loaded: Option<usize>,
    /// Sessions started, for tests.
    #[cfg(test)]
    sessions: usize,
    /// Samples looked at for a restart point, for tests.
    #[cfg(test)]
    steps: usize,
    gif: Option<GifCursor>,
}

impl Decoder {
    fn new() -> Self {
        Self {
            h264: None,
            last: None,
            not_idr: SampleSet::default(),
            walked: None,
            broken: BTreeSet::new(),
            buf: Vec::new(),
            loaded: None,
            #[cfg(test)]
            sessions: 0,
            #[cfg(test)]
            steps: 0,
            gif: None,
        }
    }

    fn frame(&mut self, source: &Source, i: usize) -> Result<Option<RgbaImage>> {
        match source {
            Source::Mp4(video) => self.video_frame(video, i),
            Source::Gif(gif) => self.gif_frame(gif, i).map(Some),
        }
    }

    fn video_frame(&mut self, video: &Video, i: usize) -> Result<Option<RgbaImage>> {
        let next = self.last.filter(|_| self.h264.is_some()).map(|last| last + 1);
        if next == Some(i) && self.broken.is_empty() {
            return self.feed(video, i, i, false);
        }
        let restart = self.restart_point(video, i)?;
        let from = restart.unwrap_or(0);
        if let Some(bad) = self.broken.range(from..=i).next() {
            anyhow::bail!("frame {i} can't be decoded: it depends on frame {bad}, which is damaged");
        }
        match next {
            // Further on, with no IDR to skip ahead to: decode the frames between.
            Some(next) if next <= i && restart.is_none_or(|k| k < next) => self.feed(video, next, i, false),
            _ => {
                // An IDR resets the live session; starting anywhere else needs a new one.
                let fresh = restart.is_none() || self.h264.is_none();
                if fresh {
                    // Drop the old session first: a failed restart must not reuse it.
                    self.h264 = None;
                    self.last = None;
                    self.h264 = Some(crate::h264::Decoder::new(&video.sps, &video.pps, video.size.0, video.size.1)?);
                    #[cfg(test)]
                    {
                        self.sessions += 1;
                    }
                }
                self.feed(video, from, i, fresh)
            }
        }
    }

    /// The latest sample at or before `i` that decoding can restart at: a sync
    /// sample holding an IDR picture. Each sync sample is checked the first
    /// time it's a candidate. The last answer is kept: the frames from its
    /// restart point up to its frame share it, and a later frame looks only
    /// at the samples after that frame, so playing on looks at one sample a
    /// frame, not at every one before it.
    fn restart_point(&mut self, video: &Video, i: usize) -> Result<Option<usize>> {
        let i = i.min(video.samples.len().saturating_sub(1));
        let (from, mut found) = match self.walked {
            Some((upto, found)) if i <= upto && found.is_none_or(|k| k <= i) => return Ok(found),
            Some((upto, found)) if i > upto => (upto + 1, found),
            _ => (0, None),
        };
        for k in (from..=i).rev() {
            #[cfg(test)]
            {
                self.steps += 1;
            }
            if !video.samples[k].sync || self.not_idr.contains(k) {
                continue;
            }
            self.load(video, k)?;
            if crate::h264::has_idr(&self.buf) {
                found = Some(k);
                break;
            }
            self.not_idr.insert(k);
        }
        self.walked = Some((i, found));
        Ok(found)
    }

    /// Have sample `i` in `buf`.
    fn load(&mut self, video: &Video, i: usize) -> Result<()> {
        if self.loaded != Some(i) {
            self.loaded = None;
            video.read(i, &mut self.buf)?;
            self.loaded = Some(i);
        }
        Ok(())
    }

    /// Decode samples `from..=i` on the live session, returning `i`'s picture.
    /// A sample the decoder fails on drops the session, so the next call
    /// starts a new one, and is marked broken if the session was `fresh`.
    fn feed(&mut self, video: &Video, from: usize, i: usize, fresh: bool) -> Result<Option<RgbaImage>> {
        let mut picture = None;
        for k in from..=i {
            self.load(video, k)?;
            let keyframe = crate::h264::has_idr(&self.buf);
            let Some(h264) = self.h264.as_mut() else { anyhow::bail!("the video decoder is gone") };
            match h264.decode(&self.buf, keyframe, k == i) {
                Ok(decoded) => {
                    self.last = Some(k);
                    if k == i {
                        picture = decoded;
                    }
                }
                Err(e) => {
                    if fresh {
                        self.broken.insert(k);
                    }
                    // Start over from a keyframe next time, not from a half-fed decoder.
                    self.h264 = None;
                    self.last = None;
                    return Err(e.context(format!("decoding frame {k}")));
                }
            }
        }
        Ok(picture)
    }

    /// GIF frame `i`, composited from the closest point at or before it: where
    /// the last call left off, a kept picture, or the start. A frame that
    /// fails to decode on the way is left out, as an export leaves it out.
    fn gif_frame(&mut self, gif: &Gif, i: usize) -> Result<RgbaImage> {
        let at = self.gif.as_ref().and_then(|c| c.at).filter(|&at| at < i);
        let (snapshot, from, frames) = {
            let table = lock(&gif.frames);
            anyhow::ensure!(i < table.frames.len(), "the GIF has no frame {i}");
            let snapshot = table.snapshots.iter().rev().find(|(k, _)| *k < i).filter(|(k, _)| at.is_none_or(|at| *k > at)).cloned();
            let from = snapshot.as_ref().map(|(k, _)| k + 1).or(at.map(|at| at + 1)).unwrap_or(0);
            (snapshot, from, table.frames[from..=i].iter().map(|f| Arc::clone(&f.frame)).collect::<Vec<_>>())
        };
        let blank = || GifCursor { canvas: gif.screen.blank(), at: None };
        let cursor = self.gif.get_or_insert_with(blank);
        match snapshot {
            Some((k, canvas)) => *cursor = GifCursor { canvas: (*canvas).clone(), at: Some(k) },
            None if at.is_none() => *cursor = blank(),
            None => {}
        }
        let mut shown = None;
        for (j, frame) in (from..).zip(&frames) {
            let pixels = gif.screen.pixels(frame);
            cursor.at = Some(j);
            match pixels {
                Ok(pixels) => shown = composite(&mut cursor.canvas, frame, &pixels, j == i),
                Err(e) if j == i => return Err(e.context(format!("decoding frame {i} of the GIF"))),
                Err(e) => log::warn!("skipping frame {j} of the GIF: {e:#}"),
            }
        }
        shown.context("the GIF frame went missing")
    }
}

/// First frame of a video, for library thumbnails. Only what that takes is
/// read: an MP4's index and first picture, a GIF's first frame.
pub fn first_frame(path: &Path) -> Result<RgbaImage> {
    caught("reading the video", || {
        let source = open_media(path, Scan::First)?;
        Decoder::new().frame(&source, 0)?.context("could not decode the first frame")
    })
}

/// Write the part of `src` between `start` and `end` (seconds) to `dst`,
/// encoded as GIF or MP4 according to `dst`'s extension. A video turned into a
/// GIF is scaled and thinned to `gif`'s width, frame rate and palette (a GIF of
/// a full-size 30 fps recording would be tens of megabytes). The original is
/// left untouched. (The player saves through [`export`], which it can cancel.)
#[cfg_attr(not(test), allow(dead_code))]
pub fn export_trimmed(src: &Path, dst: &Path, start: f64, end: f64, gif: GifQuality) -> Result<()> {
    export(src, dst, start, end, gif, &AtomicBool::new(false)).map(|_| ())
}

/// [`export_trimmed`], stopped once `cancel` is set. The copy is written next
/// to `dst` under a name the library doesn't list (see [`part_path`]) and
/// renamed when it's complete, so quitting or failing midway never leaves a
/// broken file in the library. Frames that fail to decode are left out (and
/// logged); only a video none of whose frames decode fails for it. Returns
/// where the copy went: `dst`, or a fresh name if a file took `dst` meanwhile.
fn export(src: &Path, dst: &Path, start: f64, end: f64, gif: GifQuality, cancel: &AtomicBool) -> Result<PathBuf> {
    use crate::recorder::{TimedFrame, encode_gif_until, encode_mp4};

    let source = open_sequential(src, Scan::All)?;
    let source = &source;
    let n = source.len();
    // The frame on screen at `start`, through the last one starting before `end`.
    let first = source.index_at(start);
    let picked: Vec<usize> = (first..n).take_while(|&i| i == first || source.time(i) < end).collect();
    let fps = ((n as f64 / source.duration().max(0.001)).round() as u32).clamp(1, 60);
    let to_gif = dst.extension().is_some_and(|e| e.eq_ignore_ascii_case("gif"));
    let from_video = matches!(source, Source::Mp4(_));
    let shrink = to_gif && from_video;
    // A GIF times its frames from `start`. MP4 frames are placed on the output's
    // 1/fps grid, timed from the first one and half a frame in, so rounding can't
    // tip a frame into the slot before its own. When the first frame has shown
    // since well before `start` (a long hold, in a video from another app), the
    // grid starts whole frames later, a frame or so before `start`, so the copy
    // doesn't open with all of that hold. (A frame of a constant-rate video
    // lasts about one slot, so its grid always starts at the first frame.)
    let origin = if to_gif {
        start
    } else {
        let (t, slot) = (source.time(first), 1.0 / fps as f64);
        let skipped = ((start - t) / slot - 0.5).floor().max(0.0);
        t + (skipped - 0.5) * slot
    };
    let part = part_path(dst);
    // Bounded, so decoding can't run ahead of encoding with full-size frames.
    let (tx, rx) = mpsc::sync_channel::<TimedFrame>(4);
    let written = std::thread::scope(|scope| {
        let decoding = scope.spawn(move || -> Result<()> {
            let mut decoder = Decoder::new();
            let mut fit = FrameFit::new(shrink.then(|| gif.max_width()));
            // Thin to the preset's fps on a fixed schedule (15 → 12 fps keeps 4 of
            // every 5 frames); the GIF encoder stretches the kept frames' delays.
            let step = 1.0 / gif.fps() as f64;
            let mut next_due = f64::NEG_INFINITY;
            let (mut sent, mut failure) = (0, None);
            for i in picked {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let t = source.time(i);
                if shrink && t + 1e-3 < next_due {
                    continue;
                }
                next_due = next_due.max(t - step) + step;
                if next_due <= t {
                    next_due = t + step;
                }
                let image = match decoder.frame(source, i) {
                    Ok(Some(image)) => image,
                    Ok(None) => continue,
                    Err(e) => {
                        log::warn!("leaving frame {i} of {} out of the copy: {e:#}", src.display());
                        failure = Some(e);
                        continue;
                    }
                };
                // An MP4 frame fills the output's slots up to its stamp:
                // stamped a slot before the next frame starts, a frame held
                // for long (in a video from another app) keeps its whole time.
                let next = if i + 1 < n { source.time(i + 1) } else { source.duration() };
                let at = if to_gif { secs(t - origin) } else { secs((next.min(end) - 1.0 / fps as f64).max(t) - origin) };
                if tx.send(TimedFrame { image: fit.apply(image), at }).is_err() {
                    break;
                }
                sent += 1;
            }
            match failure {
                Some(e) if sent == 0 => Err(e.context("none of the video's frames could be decoded")),
                _ => Ok(()),
            }
        });
        // Trimming a GIF keeps its full palette.
        let colors = if from_video { gif.colors() } else { 256 };
        // Decoded video frames carry compression noise; ignore tiny differences.
        let tolerance = if from_video { 4 } else { 0 };
        // The last frame shows until the selection ends, so the copy lasts
        // as long as it (and a GIF copied whole keeps its last delay).
        let until = secs(end.min(source.duration()) - origin);
        let encoded = if to_gif {
            encode_gif_until(rx, &part, cancel, colors, tolerance, Some(until))
        } else {
            encode_mp4(rx, fps, &part, cancel, false)
        };
        let decoded = decoding.join().unwrap_or_else(|_| Err(anyhow::anyhow!("decoding the video crashed")));
        decoded.and(encoded)
    });
    let saved = written.and_then(|()| {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "saving was cancelled");
        // Never over a file that has turned up under the name meanwhile, such
        // as a recording started the same second.
        let dst = match (dst.exists(), dst.parent(), dst.extension()) {
            (true, Some(dir), Some(ext)) => crate::library::new_capture_path(dir, &ext.to_string_lossy()),
            _ => dst.to_path_buf(),
        };
        std::fs::rename(&part, &dst).with_context(|| format!("can't save {}", dst.display()))?;
        Ok(dst)
    });
    if saved.is_err() {
        // A partial copy isn't what was asked for.
        let _ = std::fs::remove_file(&part);
    }
    saved
}

/// Brings an export's frames to one size: the first frame's, or narrower when
/// it's wider than `max_width`. Encoders take frames of a single size, and a
/// stream can change size midway.
struct FrameFit {
    max_width: Option<u32>,
    size: Option<(u32, u32)>,
    resizer: Option<crate::resize::Resizer>,
}

impl FrameFit {
    fn new(max_width: Option<u32>) -> Self {
        Self { max_width, size: None, resizer: None }
    }

    fn apply(&mut self, image: RgbaImage) -> RgbaImage {
        let (w, h) = image.dimensions();
        let size = *self.size.get_or_insert_with(|| match self.max_width {
            Some(max_w) if w > max_w => (max_w, (h as f64 * max_w as f64 / w as f64).round().max(1.0) as u32),
            _ => (w, h),
        });
        if image.dimensions() == size {
            return image;
        }
        self.resizer.get_or_insert_with(|| crate::resize::Resizer::new((w, h), size)).resize(&image)
    }
}

// ---------- playback ----------

#[derive(Debug, PartialEq)]
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
    /// Grows while a GIF is still being read.
    duration: f64,
}

/// The playback thread's state. (The system decoder in `decoder` stays on the
/// thread that made it.)
struct Playback {
    source: Source,
    decoder: Decoder,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
    /// The frame whose picture is on screen, so seeking to it decodes nothing.
    shown: Option<usize>,
}

impl Playback {
    /// Put frame `i` on screen, or the error decoding it; a later frame that
    /// decodes clears the error.
    fn show(&mut self, i: usize, playing: bool) {
        self.show_at(i, playing, self.source.time(i));
    }

    /// [`Playback::show`], with the playhead at `at`, within frame `i`: where
    /// a seek went, which may be well into the frame (the trim range is
    /// checked against it).
    fn show_at(&mut self, i: usize, playing: bool, at: f64) {
        let frame = if self.shown == Some(i) {
            Ok(None)
        } else {
            match catch_unwind(AssertUnwindSafe(|| self.decoder.frame(&self.source, i))) {
                Ok(frame) => frame,
                Err(_) => {
                    self.decoder = Decoder::new();
                    Err(anyhow::anyhow!("decoding frame {i} crashed"))
                }
            }
        };
        let mut s = lock(&self.shared);
        match frame {
            Ok(Some(img)) => {
                s.frame = Some(img);
                s.error = None;
                self.shown = Some(i);
            }
            Ok(None) => {}
            Err(e) => {
                s.error = Some(format!("{e:#}"));
                self.shown = None;
            }
        }
        s.position = at;
        s.playing = playing;
        drop(s);
        self.ctx.request_repaint();
    }

    fn set_playing(&self, playing: bool) {
        lock(&self.shared).playing = playing;
    }

    /// Show a GIF's timeline as far as it has been read.
    fn sync_duration(&self) {
        let duration = self.source.duration();
        let mut s = lock(&self.shared);
        if s.duration != duration {
            s.duration = duration;
            drop(s);
            self.ctx.request_repaint();
        }
    }
}

fn playback(source: Source, rx: Receiver<Command>, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let mut p = Playback { source, decoder: Decoder::new(), shared, ctx, shown: None };
    let mut playing = false;
    // Media time `clock.1` played at wall-clock instant `clock.0`.
    let mut clock = (Instant::now(), 0.0);
    let due = |clock: (Instant, f64), t: f64| clock.0 + secs(t - clock.1);
    p.show(0, false);
    let mut next = 1;

    loop {
        p.sync_duration();
        let (n, complete) = p.source.known();
        let wait = if playing && next < n {
            Some(due(clock, p.source.time(next)).saturating_duration_since(Instant::now()))
        } else if playing && complete {
            // Played to the end.
            Some(Duration::ZERO)
        } else if !complete {
            Some(SCAN_POLL)
        } else {
            None
        };
        let command = match wait {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        let Some(first) = command else {
            if playing && next < n {
                if Instant::now() >= due(clock, p.source.time(next)) {
                    p.show(next, true);
                    next += 1;
                }
            } else if playing && complete {
                playing = false;
                let mut s = lock(&p.shared);
                s.playing = false;
                s.position = p.source.duration();
                drop(s);
                p.ctx.request_repaint();
            } else if playing {
                // Caught up with a GIF still being read: play on from here once
                // its next frame is in, rather than rush to catch up.
                clock = (Instant::now(), p.source.time(next.saturating_sub(1)));
            }
            // Otherwise a GIF still being read: its timeline grows (synced above).
            continue;
        };
        for command in queued(first, &rx) {
            match command {
                Command::Play => {
                    if next >= n && complete {
                        p.show(0, true);
                        next = 1;
                    }
                    playing = true;
                    clock = (Instant::now(), p.source.time(next.saturating_sub(1)));
                    p.set_playing(true);
                }
                Command::Pause => {
                    playing = false;
                    p.set_playing(false);
                }
                Command::Seek(t) => {
                    let i = p.source.index_at(t);
                    p.show_at(i, playing, t.max(p.source.time(i)).min(p.source.duration()));
                    next = i + 1;
                    clock = (Instant::now(), p.source.time(i));
                }
            }
        }
    }
}

/// `first` and the commands queued after it, in order, with each run of
/// seeks cut down to its last: while scrubbing, only the latest seek
/// matters, but a Pause sent along with a seek must still pause.
fn queued(first: Command, rx: &Receiver<Command>) -> Vec<Command> {
    let mut commands = vec![first];
    for command in rx.try_iter() {
        if let (Command::Seek(_), Some(Command::Seek(_))) = (&command, commands.last()) {
            commands.pop();
        }
        commands.push(command);
    }
    commands
}

/// Something the app should react to.
pub enum PlayerAction {
    None,
    /// A new copy (trimmed, and/or converted to GIF) was written here.
    Saved(PathBuf),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Handle {
    In,
    Out,
}

/// A copy being written in the background: by its player, or by the app once
/// that player has closed (see [`BackgroundSaves`]).
pub struct SaveJob {
    result: Receiver<Result<PathBuf>>,
    /// Stops the export: set by Cancel, and when Rega quits.
    cancel: Arc<AtomicBool>,
    /// When the save started.
    started: Instant,
}

impl SaveJob {
    /// The outcome once the save has finished. One that stopped without a
    /// word (its thread gone) failed; one that was cancelled is nothing to
    /// report.
    fn poll(&self) -> Option<PlayerAction> {
        let result = match self.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("the export stopped unexpectedly")),
        };
        Some(match result {
            Ok(path) => PlayerAction::Saved(path),
            Err(_) if self.cancel.load(Ordering::Relaxed) => PlayerAction::None,
            Err(e) => PlayerAction::Failed(format!("Saving failed: {e:#}")),
        })
    }
}

/// Saves that outlive their player: closing a player while it saves hands the
/// save over here, where it finishes and the app reports how it went.
pub type BackgroundSaves = Arc<Mutex<Vec<SaveJob>>>;

/// How the background saves that have finished went, in no particular order.
pub fn poll_background_saves(saves: &BackgroundSaves) -> Vec<PlayerAction> {
    let mut done = Vec::new();
    lock(saves).retain(|job| match job.poll() {
        Some(action) => {
            done.push(action);
            false
        }
        None => true,
    });
    done
}

/// Cancel every background save and wait up to `timeout` in all for them to
/// remove their partial files: for quitting.
pub fn cancel_background_saves(saves: &BackgroundSaves, timeout: Duration) {
    let jobs = std::mem::take(&mut *lock(saves));
    for job in &jobs {
        job.cancel.store(true, Ordering::Relaxed);
    }
    let deadline = Instant::now() + timeout;
    for job in &jobs {
        let _ = job.result.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }
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
    saving: Option<SaveJob>,
    /// Where a save still running when the player closes goes on.
    background: Option<BackgroundSaves>,
}

impl Drop for Player {
    fn drop(&mut self) {
        // The user asked for the copy: it still gets made. Without anywhere
        // to report it, it just turns up in the library.
        if let (Some(job), Some(saves)) = (self.saving.take(), &self.background) {
            lock(saves).push(job);
        }
    }
}

impl Player {
    /// Open a video to play. Fails (and the app hands the file to the
    /// system's player) for one Rega can't read or show frame by frame. A GIF
    /// opens at once: its frames are read on while the first plays.
    pub fn open(ctx: &egui::Context, path: PathBuf) -> Result<Self> {
        let source = open_sequential(&path, Scan::Background)?;
        // Each picture goes to the GPU as one texture; a larger one than it
        // takes would crash the app.
        let ((w, h), max) = (source.size(), ctx.input(|i| i.max_texture_side));
        anyhow::ensure!(
            w as usize <= max && h as usize <= max,
            "this video is {w}×{h}, larger than the GPU can show ({max} px a side)"
        );
        let duration = source.duration();
        let shared = Arc::new(Mutex::new(Shared { duration, ..Shared::default() }));
        let (tx, rx) = mpsc::channel();
        let (s, c) = (shared.clone(), ctx.clone());
        std::thread::spawn(move || playback(source, rx, s, c));
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
            background: None,
        };
        player.send(Command::Play);
        Ok(player)
    }

    /// Hand a save still running when this player closes to `saves`.
    pub fn hand_saves_to(&mut self, saves: BackgroundSaves) {
        self.background = Some(saves);
    }

    fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }

    /// Pause, or play from the playhead at `position`: from In instead when
    /// trimming with the playhead outside the selection, so Play plays it.
    fn play_pause(&self, playing: bool, position: f64) {
        if playing {
            self.send(Command::Pause);
            return;
        }
        if let Some(a) = self.rewind_to(position) {
            self.send(Command::Seek(a));
        }
        self.send(Command::Play);
    }

    /// The In point, when trimming with the playhead at `position` outside
    /// the selection (more than a moment before In, or at or past Out).
    fn rewind_to(&self, position: f64) -> Option<f64> {
        let (a, b) = self.trim?;
        (position >= b || position < a - 0.05).then_some(a)
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// The save's outcome, once it has finished.
    fn poll_save(&mut self) -> PlayerAction {
        let Some(action) = self.saving.as_ref().and_then(SaveJob::poll) else { return PlayerAction::None };
        self.saving = None;
        if let PlayerAction::Saved(_) = action {
            self.trim = None;
        }
        action
    }

    /// Upload a new frame, in place when it's the texture's size.
    fn upload(&mut self, ctx: &egui::Context, img: &RgbaImage) {
        let size = [img.width() as usize, img.height() as usize];
        let color = color_image(img);
        match &mut self.texture {
            Some(t) if t.size() == size => t.set_partial([0, 0], color, egui::TextureOptions::LINEAR),
            Some(t) => t.set(color, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("video-frame", color, egui::TextureOptions::LINEAR)),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> PlayerAction {
        let ctx = ui.ctx().clone();
        let action = self.poll_save();
        let (frame, position, playing, error) = {
            let mut s = lock(&self.shared);
            self.duration = s.duration;
            (s.frame.take(), s.position, s.playing, s.error.clone())
        };
        if let Some(img) = frame {
            self.upload(&ctx, &img);
        }

        let keys_free = ctx.memory(|m| m.focused().is_none());
        if keys_free && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.play_pause(playing, position);
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
        // While trimming, playback stays inside the selection: past it, it
        // stops, back at In.
        if playing
            && self.scrubbing.is_none()
            && let Some(a) = self.rewind_to(position)
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
                    self.play_pause(playing, position);
                }
                let shown = self.scrubbing.unwrap_or(position);
                ui.label(egui::RichText::new(format!("{} / {}", clock(shown), clock(self.duration))).monospace().color(theme::TEXT_MUTED));
                ui.add_space(8.0);

                // Buttons on the right, scrubber fills what's left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match self.trim {
                        Some((a, b)) => {
                            if self.saving.is_some() {
                                self.saving_ui(ui, "Saving…");
                            } else {
                                if theme::primary_button(ui, Some(icon::SAVE), "Save trimmed copy").clicked() {
                                    self.save_trimmed(a, b, None);
                                }
                                if !self.is_gif()
                                    && let Some(q) = gif_menu(ui)
                                {
                                    self.save_trimmed(a, b, Some(q));
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
                        None if self.saving.is_some() => self.saving_ui(ui, "Saving GIF…"),
                        None => {
                            if !self.is_gif()
                                && let Some(q) = gif_menu(ui)
                            {
                                self.save_trimmed(0.0, self.duration, Some(q));
                            }
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

    /// A save in progress: a spinner, and Cancel. Cancel takes the place of
    /// Save, so it ignores clicks for a moment: the second click of a
    /// double-click on Save must not cancel what the first one started.
    fn saving_ui(&mut self, ui: &mut egui::Ui, label: &str) {
        let Some(saving) = &self.saving else { return };
        let cancelling = saving.cancel.load(Ordering::Relaxed);
        if !cancelling && theme::secondary_button(ui, None, "Cancel").clicked() && saving.started.elapsed() >= Duration::from_millis(500) {
            saving.cancel.store(true, Ordering::Relaxed);
        }
        ui.add(egui::Spinner::new());
        ui.label(egui::RichText::new(if cancelling { "Cancelling…" } else { label }).color(theme::TEXT_MUTED));
    }

    fn is_gif(&self) -> bool {
        self.path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gif"))
    }

    /// Save `start..end` as a new library item: same format, or a GIF at the
    /// given quality.
    fn save_trimmed(&mut self, start: f64, end: f64, as_gif: Option<GifQuality>) {
        self.send(Command::Pause);
        let src = self.path.clone();
        let ext = if as_gif.is_some() {
            "gif".into()
        } else {
            src.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into())
        };
        let dst = crate::library::new_capture_path(src.parent().unwrap_or(Path::new(".")), &ext);
        // Claim the name now, so a save started in the same second picks another.
        let _ = File::create(part_path(&dst));
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (ctx, stop) = (self.ctx.clone(), cancel.clone());
        std::thread::spawn(move || {
            // Even a crash must end the save, or "Saving…" would spin forever.
            let saved = caught("the export", || export(&src, &dst, start, end, as_gif.unwrap_or_default(), &stop));
            let _ = tx.send(saved);
            ctx.request_repaint();
        });
        self.saving = Some(SaveJob { result: rx, cancel, started: Instant::now() });
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

/// A frame as egui takes it. Frames are opaque (or, from a GIF, fully
/// transparent where nothing was drawn), so their RGBA is already
/// premultiplied: a straight copy, which `as_chunks` lets the compiler
/// vectorize (unlike `ColorImage::from_rgba_premultiplied`).
fn color_image(img: &RgbaImage) -> egui::ColorImage {
    let pixels = img.as_raw().as_chunks::<4>().0.iter().map(|&[r, g, b, a]| Color32::from_rgba_premultiplied(r, g, b, a)).collect();
    egui::ColorImage::new([img.width() as usize, img.height() as usize], pixels)
}

/// Shortest trim the handles allow, in seconds.
const MIN_TRIM: f64 = 0.2;

fn clock(t: f64) -> String {
    let s = t.max(0.0).round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// "Save as GIF" button that opens a menu of quality presets; returns the one picked.
fn gif_menu(ui: &mut egui::Ui) -> Option<GifQuality> {
    let resp = theme::secondary_button(ui, None, "Save as GIF");
    let mut picked = None;
    egui::Popup::menu(&resp).show(|ui| {
        ui.set_min_width(240.0);
        for q in GifQuality::ALL {
            let text = egui::RichText::new(format!("{}   {}", q.label(), q.details()));
            if ui.button(text).clicked() {
                picked = Some(q);
                ui.close();
            }
        }
    });
    picked
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// How many pictures an MP4 holds.
    pub(crate) fn frame_count(mp4: &Path) -> usize {
        open_mp4(mp4).unwrap().samples.len()
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rega-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `f()`, run on a thread of its own: fails the test if it takes longer
    /// than `limit` (a hang) or panics.
    fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(limit) {
            Ok(value) => value,
            Err(RecvTimeoutError::Timeout) => panic!("still running after {limit:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("panicked"),
        }
    }

    fn mp4_source(path: &Path) -> Source {
        Source::Mp4(open_mp4(path).unwrap())
    }

    fn video(source: &Source) -> &Video {
        match source {
            Source::Mp4(video) => video,
            Source::Gif(_) => panic!("not an MP4"),
        }
    }

    /// Where the box at `path` (types from the top, e.g. `moov/trak/mdia/mdhd`)
    /// starts in `data`, and its size.
    fn find_box(data: &[u8], path: &str) -> (usize, usize) {
        let (mut start, mut end, mut found) = (0, data.len(), (0, 0));
        for kind in path.split('/') {
            let mut pos = start;
            loop {
                assert!(pos + 8 <= end, "no {kind} in {path}");
                let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
                if &data[pos + 4..pos + 8] == kind.as_bytes() {
                    found = (pos, size);
                    break;
                }
                pos += size;
            }
            // Where the children start: after a sample description's version,
            // flags and count, and after a sample entry's fields.
            start = found.0 + 8 + match kind {
                "stsd" => 8,
                "avc1" => 78,
                _ => 0,
            };
            end = found.0 + found.1;
        }
        found
    }

    fn put_u32(data: &mut [u8], at: usize, value: u32) {
        data[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// A 30-frame, 10 fps recording (keyframes at 0 and 20) and its bytes.
    fn recording(dir: &Path) -> (PathBuf, Vec<u8>) {
        let mp4 = dir.join("recording.mp4");
        crate::recorder::write_test_video(&mp4, 30, 10, (96, 64));
        let data = std::fs::read(&mp4).unwrap();
        (mp4, data)
    }

    #[test]
    fn decodes_recordings_and_seeks() {
        let dir = test_dir("player");
        let mp4 = dir.join("t.mp4");
        crate::recorder::write_test_video(&mp4, 30, 10, (96, 64));
        let source = mp4_source(&mp4);
        assert_eq!(source.len(), 30);
        assert!((source.duration() - 3.0).abs() < 0.2, "duration {}", source.duration());

        let mut dec = Decoder::new();
        let first = dec.frame(&source, 0).unwrap().expect("frame 0");
        assert_eq!(first.dimensions(), (96, 64));
        // In order, then a jump back and a jump forward (both need re-decoding).
        for i in [1, 2, 25, 3, 29] {
            assert!(dec.frame(&source, i).unwrap().is_some(), "frame {i}");
        }
        assert!(first_frame(&mp4).is_ok());

        // Trim 1.0–2.0 s of the 3 s video.
        let cut = dir.join("cut.mp4");
        export_trimmed(&mp4, &cut, 1.0, 2.0, GifQuality::default()).unwrap();
        let trimmed = mp4_source(&cut);
        assert!((trimmed.duration() - 1.0).abs() < 0.2, "trimmed duration {}", trimmed.duration());

        // MP4 → GIF: 10 fps source stays under the 15 fps cap, so 10 frames.
        let as_gif = dir.join("cut.gif");
        export_trimmed(&mp4, &as_gif, 1.0, 2.0, GifQuality::default()).unwrap();
        assert_eq!(open_gif(&as_gif, Scan::All).unwrap().frames.lock().unwrap().frames.len(), 10);

        let gif = dir.join("t.gif");
        crate::recorder::write_test_gif(&gif, 4);
        assert_eq!(Source::Gif(open_gif(&gif, Scan::All).unwrap()).len(), 4);
        assert_eq!(first_frame(&gif).unwrap().dimensions(), (64, 48));
    }

    /// Which test-video frame each picture of `mp4` shows: frame `i` has red
    /// `x + 8i` (see `recorder::tests::frames_sized`), read at x = 10.
    fn frame_numbers(mp4: &Path) -> Vec<i32> {
        let source = mp4_source(mp4);
        let mut dec = Decoder::new();
        (0..source.len())
            .map(|i| {
                let picture = dec.frame(&source, i).unwrap().expect("picture");
                ((picture.get_pixel(10, 10)[0] as f64 - 10.0) / 8.0).round() as i32
            })
            .collect()
    }

    #[test]
    fn trimmed_copies_keep_every_frame_once() {
        let dir = test_dir("trim");
        let mp4 = dir.join("t.mp4");
        // 15 fps, the default, whose frame times don't land exactly on the grid.
        crate::recorder::write_test_video(&mp4, 30, 15, (96, 64));
        assert_eq!(frame_numbers(&mp4), (0..30).collect::<Vec<_>>());
        // In-points on a frame (the default, and what the I key sets) and between two.
        for (start, end, first, last) in [(0.0, 2.0, 0, 29), (1.0, 1.6, 15, 23), (1.03, 1.63, 15, 24)] {
            let cut = dir.join(format!("cut-{start}.mp4"));
            export_trimmed(&mp4, &cut, start, end, GifQuality::default()).unwrap();
            assert_eq!(frame_numbers(&cut), (first..=last).collect::<Vec<_>>(), "trim {start}–{end}");
        }
    }

    /// A copy of a video whose frames last different times (another app's
    /// recording of a screen that sat still for a while) shows each picture
    /// when the original does: a frame held for seconds isn't lost, what
    /// follows it isn't early, and a copy from partway through the hold
    /// doesn't open with all of it.
    #[test]
    fn copies_keep_the_timing_of_variable_frame_rate_videos() {
        let dir = test_dir("vfr");
        // Red for 1.04 s in 26 frames, green held 4.96 s, blue for 1.08 s in 27.
        let colors = [("red", [255, 0, 0], 26, 0.04, 1.04), ("green", [0, 255, 0], 1, 4.96, 4.96), ("blue", [0, 0, 255], 26, 0.04, 1.08)];
        let mut list = String::from("ffconcat version 1.0\n");
        for (name, [r, g, b], frames, each, _) in colors {
            RgbaImage::from_pixel(64, 48, image::Rgba([r, g, b, 255])).save(dir.join(format!("{name}.png"))).unwrap();
            list += &format!("file {name}.png\nduration {each}\n").repeat(frames);
        }
        // The last file again, as the concat demuxer wants: a 27th blue frame.
        list += "file blue.png\n";
        std::fs::write(dir.join("list.txt"), list).unwrap();
        let src = dir.join("vfr.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "concat", "-i"])
            .arg(dir.join("list.txt"))
            .args(["-fps_mode", "vfr", "-c:v", "libx264", "-bf", "0", "-pix_fmt", "yuv420p"])
            .arg(&src)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("skipping: no ffmpeg with libx264");
            return;
        }
        assert_eq!(frame_count(&src), 54, "a frame for each picture, not a constant rate");

        // Copy `start`..the end, and check that each colour shows in the copy
        // as long as `expected` says, to within a frame and a half of it.
        let copy_shows = |start: f64, expected: &[(&str, f64)]| {
            let copy = dir.join(format!("copy-{start}.mp4"));
            export_trimmed(&src, &copy, start, 99.0, GifQuality::default()).unwrap();
            let source = mp4_source(&copy);
            let frame = source.duration() / source.len() as f64;
            let mut dec = Decoder::new();
            // How long each colour shows in the copy, in order.
            let mut shown: Vec<(&str, f64)> = Vec::new();
            for i in 0..source.len() {
                let [r, g, b, _] = dec.frame(&source, i).unwrap().expect("picture").get_pixel(32, 24).0;
                let name = if r > g && r > b { "red" } else if g > b { "green" } else { "blue" };
                match shown.last_mut() {
                    Some((last, seconds)) if *last == name => *seconds += frame,
                    _ => shown.push((name, frame)),
                }
            }
            let close = shown.len() == expected.len() && shown.iter().zip(expected).all(|(a, b)| a.0 == b.0 && (a.1 - b.1).abs() <= 1.5 * frame);
            assert!(close, "the copy from {start} s shows {shown:?}, the original {expected:?}");
        };
        let whole: Vec<(&str, f64)> = colors.iter().map(|&(name, _, _, _, seconds)| (name, seconds)).collect();
        copy_shows(0.0, &whole);
        // From In inside the held frame, which the copy then starts with only
        // a frame or so before In.
        copy_shows(3.05, &[("green", 6.0 - 3.05), ("blue", 1.08)]);
    }

    #[test]
    fn spots_videos_stored_out_of_order() {
        assert!(!is_reordered([0, 3000, 6000]));
        // ffmpeg's constant composition offset alone isn't reordering.
        assert!(!is_reordered([1024, 1536, 2048]));
        // I P B B: shown 0, 3, 1, 2.
        assert!(is_reordered([0, 3000, 1000, 2000]));
        assert!(!is_reordered([]));
    }

    /// The index gives each sample exactly what the `mp4` crate reads for it.
    fn assert_indexes_like_the_mp4_crate(path: &Path) {
        let video = open_mp4(path).unwrap();
        let file = File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        let mut reader = mp4::Mp4Reader::read_header(BufReader::new(file), len).unwrap();
        let track = reader.tracks().values().find(|t| matches!(t.track_type(), Ok(mp4::TrackType::Video))).unwrap();
        let (id, timescale) = (track.track_id(), track.timescale() as f64);
        let mut shown = Vec::new();
        let mut buf = Vec::new();
        let mut duration = 0.0;
        assert_eq!(video.samples.len() as u32, reader.sample_count(id).unwrap(), "{}", path.display());
        for (i, sample) in video.samples.iter().enumerate() {
            let s = reader.read_sample(id, i as u32 + 1).unwrap().unwrap();
            video.read(i, &mut buf).unwrap();
            assert_eq!(buf, crate::h264::avcc_to_4_byte_lengths(&s.bytes, video.len_size), "{} sample {i}", path.display());
            assert_eq!(sample.time, s.start_time as f64 / timescale, "{} sample {i}", path.display());
            assert_eq!(sample.sync, s.is_sync, "{} sample {i}", path.display());
            shown.push(s.start_time as i64 + s.rendering_offset as i64);
            duration = sample.time + s.duration as f64 / timescale;
        }
        assert_eq!(video.duration, duration, "{}", path.display());
        assert_eq!(video.reordered, is_reordered(shown), "{}", path.display());
    }

    /// `ffmpeg` with `args` (after a test-pattern input) writing `out`; false
    /// without ffmpeg.
    fn ffmpeg(args: &[&str], out: &Path) -> bool {
        let input = ["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x96:rate=25:duration=3", "-pix_fmt", "yuv420p"];
        std::process::Command::new("ffmpeg").args(input).args(args).arg(out).status().is_ok_and(|s| s.success())
    }

    #[test]
    fn indexes_videos_as_the_mp4_crate_reads_them() {
        let dir = test_dir("index");
        let (mp4, _) = recording(&dir);
        assert_indexes_like_the_mp4_crate(&mp4);
        // Other apps' layouts, from x264: B-frames (composition offsets), a
        // baseline stream, and intra refresh (recovery points marked as sync
        // samples); each with all its samples in one chunk, an edit list and
        // metadata.
        let other = [
            ("bframes", &["-c:v", "libx264", "-bf", "2", "-g", "25"][..]),
            ("baseline", &["-c:v", "libx264", "-profile:v", "baseline", "-g", "12"][..]),
            ("intra-refresh", &["-c:v", "libx264", "-bf", "0", "-x264-params", "intra-refresh=1:keyint=20"][..]),
        ];
        for (name, args) in other {
            let path = dir.join(format!("{name}.mp4"));
            if !ffmpeg(args, &path) {
                eprintln!("skipping {name}: no ffmpeg with libx264");
                continue;
            }
            assert_indexes_like_the_mp4_crate(&path);
            let video = open_mp4(&path).unwrap();
            assert_eq!(video.reordered, name == "bframes", "{name}");
            // B-frame videos go to the system's player, but still make thumbnails.
            assert_eq!(open_sequential(&path, Scan::All).is_ok(), name != "bframes", "{name}");
            assert!(first_frame(&path).is_ok(), "{name}");
        }
    }

    /// The ways a damaged MP4 used to crash (panics in the `mp4` crate) or
    /// hang (a zero-size box) Rega, or make it read gigabytes, each refused
    /// quickly, with what's wrong.
    #[test]
    fn refuses_damaged_mp4s_quickly() {
        let dir = test_dir("damaged");
        let (_, data) = recording(&dir);
        let stbl = "moov/trak/mdia/minf/stbl";
        let (stts, _) = find_box(&data, &format!("{stbl}/stts"));
        let (stsc, _) = find_box(&data, &format!("{stbl}/stsc"));
        let (stsz, _) = find_box(&data, &format!("{stbl}/stsz"));
        let (stco, _) = find_box(&data, &format!("{stbl}/stco"));
        let (mdhd, _) = find_box(&data, "moov/trak/mdia/mdhd");
        let (stss, stss_size) = find_box(&data, &format!("{stbl}/stss"));
        let len = data.len() as u32;
        #[allow(clippy::type_complexity)]
        let cases: Vec<(&str, Box<dyn Fn(&mut Vec<u8>)>, &str)> = vec![
            ("no timing entries", Box::new(move |d| put_u32(d, stts + 12, 0)), "timing table"),
            ("no samples per chunk", Box::new(move |d| put_u32(d, stsc + 20, 0)), "chunk table"),
            ("chunk numbered 0", Box::new(move |d| put_u32(d, stsc + 16, 0)), "chunk table"),
            (
                "zero 64-bit size",
                Box::new(move |d| {
                    put_u32(d, mdhd, 1);
                    d[mdhd + 8..mdhd + 16].fill(0);
                }),
                "no size",
            ),
            ("zero size", Box::new(move |d| put_u32(d, stss, 0)), "no size"),
            ("smaller than its header", Box::new(move |d| put_u32(d, stss, 4)), "smaller than its header"),
            ("past its parent", Box::new(move |d| put_u32(d, stss, stss_size as u32 + 64)), "past its parent"),
            (
                "overlapping chunks",
                Box::new(move |d| {
                    // Every frame a whole half of the file, all at the same place.
                    put_u32(d, stsz + 12, len / 2);
                    let first = u32::from_be_bytes(d[stco + 16..stco + 20].try_into().unwrap());
                    for k in 0..30 {
                        put_u32(d, stco + 16 + 4 * k, first);
                    }
                }),
                "more than the file holds",
            ),
            ("frame past the end", Box::new(move |d| put_u32(d, stco + 16 + 4 * 29, len - 10)), "outside the file"),
            ("cut short", Box::new(|d| d.truncate(d.len() - 40)), ""),
        ];
        for (name, damage, why) in cases {
            let mut bad = data.clone();
            damage(&mut bad);
            let path = dir.join(format!("{}.mp4", name.replace(' ', "-")));
            std::fs::write(&path, &bad).unwrap();
            let p = path.clone();
            let (opened, first) = within(Duration::from_secs(10), move || (Player::open(&egui::Context::default(), p.clone()).err(), first_frame(&p).err()));
            let (opened, first) = (opened.expect(name), first.expect(name));
            assert!(format!("{opened:#}").contains(why), "{name}: {opened:#}");
            assert!(format!("{first:#}").contains(why), "{name}: {first:#}");
            let p = path.clone();
            assert!(within(Duration::from_secs(10), move || export_trimmed(&p, &p.with_extension("out.mp4"), 0.0, 9.0, GifQuality::default()).is_err()));
        }
    }

    /// `data` (a recording, whose last box is `moov`) with `boxes` added at the
    /// end of its `moov`.
    fn with_in_moov(mut data: Vec<u8>, boxes: &[u8]) -> Vec<u8> {
        let (moov, size) = find_box(&data, "moov");
        assert_eq!(moov + size, data.len(), "moov comes last");
        put_u32(&mut data, moov, (size + boxes.len()) as u32);
        data.extend_from_slice(boxes);
        data
    }

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&(8 + body.len() as u32).to_be_bytes()[..], &kind[..], body].concat()
    }

    /// QuickTime may end a user data box with four zero bytes. The `mp4` crate
    /// reads them as a box of size 0 and loops forever when more follows, as
    /// it does on one inside a metadata item; such files are refused instead.
    /// QuickTime's `meta` without version and flags is fine.
    #[test]
    fn handles_metadata_boxes() {
        let dir = test_dir("metadata");
        let (_, data) = recording(&dir);
        let hdlr = mp4_box(b"hdlr", &[&[0; 8][..], b"mdir", &[0; 13]].concat());
        let title = mp4_box(b"\xa9nam", &[&[0, 0, 0, 0][..], b"free", &[0; 8]].concat());
        let damaged = [
            ("terminated", [mp4_box(b"udta", &[0; 4]), mp4_box(b"free", &[])].concat(), "past its parent"),
            ("bad title", mp4_box(b"udta", &mp4_box(b"meta", &[&[0; 4][..], &hdlr, &mp4_box(b"ilst", &title)].concat())), "no size"),
        ];
        for (name, boxes, why) in damaged {
            let path = dir.join(format!("{name}.mp4"));
            std::fs::write(&path, with_in_moov(data.clone(), &boxes)).unwrap();
            let err = within(Duration::from_secs(10), move || first_frame(&path).unwrap_err());
            assert!(format!("{err:#}").contains(why), "{name}: {err:#}");
        }

        let meta = mp4_box(b"meta", &[hdlr, mp4_box(b"ilst", &[])].concat());
        let path = dir.join("quicktime-meta.mp4");
        std::fs::write(&path, with_in_moov(data, &mp4_box(b"udta", &meta))).unwrap();
        assert_eq!(frame_count(&path), 30);
        assert!(first_frame(&path).is_ok());
    }

    /// The index follows chunk runs, 64-bit chunk offsets and an unsorted sync
    /// table; it leaves out samples past the last chunk (as the `mp4` crate
    /// does), and refuses tables that contradict each other or the file.
    #[test]
    fn indexes_sample_tables() {
        let tables = |chunks: Vec<(u32, u32)>, offsets: &'static [u64]| SampleTables {
            sample_size: 0,
            sample_count: 6,
            sizes: &[10, 20, 30, 40, 50, 60],
            times: vec![(4, 100), (2, 50)],
            offsets: None,
            chunks,
            chunk_offsets: ChunkOffsets::Long(offsets),
            sync: Some(&[5, 1]),
        };
        let index = sample_index(&tables(vec![(1, 2), (3, 1)], &[1000, 2000, 3000, 4000]), 100, 10_000).unwrap();
        let at: Vec<_> = index.samples.iter().map(|s| (s.offset, s.size, s.time, s.sync)).collect();
        assert_eq!(
            at,
            [(1000, 10, 0.0, true), (1010, 20, 1.0, false), (2000, 30, 2.0, false), (2030, 40, 3.0, false), (3000, 50, 4.0, true), (4000, 60, 4.5, false)]
        );
        assert_eq!(index.duration, 5.0);
        // Chunks before the first run hold nothing; past the last chunk, samples are left out.
        let index = sample_index(&tables(vec![(2, 3)], &[1000, 2000, 3000]), 100, 10_000).unwrap();
        assert_eq!(index.samples.iter().map(|s| s.offset).collect::<Vec<_>>(), [2000, 2010, 2030, 3000, 3040, 3090]);
        let index = sample_index(&tables(vec![(1, 2)], &[1000, 2000]), 100, 10_000).unwrap();
        assert_eq!(index.samples.len(), 4);
        // Composition offsets that put pictures out of order (B-frames).
        let mut bframes = tables(vec![(1, 6)], &[0]);
        bframes.offsets = Some(vec![(1, 200), (2, 0), (3, 100)]);
        assert!(sample_index(&bframes, 100, 10_000).unwrap().reordered);
        bframes.offsets = Some(vec![(6, 100)]);
        assert!(!sample_index(&bframes, 100, 10_000).unwrap().reordered);

        for (name, t, why) in [
            ("a file too small", tables(vec![(1, 6)], &[0]), "more than the file holds"),
            ("samples past the file", tables(vec![(1, 6)], &[100]), "outside the file"),
            ("chunk 0", tables(vec![(0, 6)], &[0]), "chunk table"),
            ("decreasing chunks", tables(vec![(2, 1), (1, 1)], &[0, 100]), "chunk table"),
            ("no runs", tables(vec![], &[0]), "chunk table"),
        ] {
            let err = sample_index(&t, 100, if name == "a file too small" { 200 } else { 220 }).map(|_| ()).unwrap_err();
            assert!(format!("{err:#}").contains(why), "{name}: {err:#}");
        }
        let mut untimed = tables(vec![(1, 6)], &[0]);
        untimed.times = vec![(5, 100)];
        assert!(sample_index(&untimed, 100, 10_000).is_err());
        let mut huge = tables(vec![(1, 6)], &[0]);
        huge.sample_size = MAX_SAMPLE_BYTES + 1;
        huge.sample_count = 1;
        assert!(sample_index(&huge, 100, u64::MAX).is_err());
    }

    /// Fragmented MP4s (`moof`) made the `mp4` crate's sample lookup divide
    /// by zero; they go to the system's player.
    #[test]
    fn refuses_fragmented_mp4s() {
        let dir = test_dir("fragmented");
        let (_, mut data) = recording(&dir);
        // A movie fragment of three 100-byte samples in four track fragments:
        // more fragments than samples, which divided by zero.
        let traf = |samples: u32| {
            let sizes = (0..samples).flat_map(|_| 100u32.to_be_bytes());
            let trun = [&[0, 0, 2, 0][..], &samples.to_be_bytes(), &sizes.collect::<Vec<_>>()].concat();
            mp4_box(b"traf", &[mp4_box(b"tfhd", &[0, 0, 0, 0, 0, 0, 0, 1]), mp4_box(b"trun", &trun)].concat())
        };
        data.extend(mp4_box(b"moof", &[mp4_box(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]), traf(3), traf(0), traf(0), traf(0)].concat()));
        let path = dir.join("fragmented.mp4");
        std::fs::write(&path, &data).unwrap();
        let err = within(Duration::from_secs(10), move || first_frame(&path).unwrap_err());
        assert!(format!("{err:#}").contains("fragmented"), "{err:#}");
    }

    /// A panic inside the `mp4` crate (here an `emsg` box too short for its
    /// fields: a subtraction overflows) is an error, not a crash.
    #[test]
    fn survives_a_panic_in_the_mp4_crate() {
        let dir = test_dir("mp4-panic");
        let (_, mut data) = recording(&dir);
        let mut emsg = vec![0, 0, 0, 29, b'e', b'm', b's', b'g', 0, 0, 0, 0, 0, 0];
        emsg.resize(29, 1);
        data.extend(emsg);
        data.extend([0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        let path = dir.join("emsg.mp4");
        std::fs::write(&path, &data).unwrap();
        let err = within(Duration::from_secs(10), move || first_frame(&path).unwrap_err());
        assert!(format!("{err:#}").contains("crashed"), "{err:#}");
    }

    /// `R`, counting the reads and seeks that reach it: system calls, for a
    /// file.
    struct Counted<R>(R, usize);

    impl<R: Read> Read for Counted<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.1 += 1;
            self.0.read(buf)
        }
    }

    impl<R: Seek> Seek for Counted<R> {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            self.1 += 1;
            self.0.seek(to)
        }
    }

    /// Reading the index of an MP4 with many small boxes takes a read per
    /// buffer, not a seek and a read per box: a crafted file of millions of
    /// empty boxes froze the window for seconds.
    #[test]
    fn reads_many_small_boxes_in_few_reads() {
        let dir = test_dir("boxes");
        let (_, data) = recording(&dir);
        let free: Vec<u8> = (0..20_000).flat_map(|_| mp4_box(b"free", &[])).collect();
        let data = with_in_moov(data, &mp4_box(b"udta", &free));
        let len = data.len() as u64;
        let mut reader = Tracked::new(Counted(std::io::Cursor::new(&data), 0));
        check_boxes(&mut reader, len).unwrap();
        reader.rewind().unwrap();
        mp4::Mp4Reader::read_header(&mut reader, len).unwrap();
        let calls = reader.into_inner().1;
        assert!(calls < 200, "{calls} reads and seeks for a {} KB file", len / 1024);
    }

    /// Every one-byte change anywhere in an MP4's index (the `moov` box) gives
    /// a video or an error, in good time: never a crash or a hang.
    #[test]
    fn survives_any_one_byte_change_to_the_index() {
        let dir = test_dir("fuzz");
        let (_, data) = recording(&dir);
        let (moov, size) = find_box(&data, "moov");
        let path = dir.join("fuzzed.mp4");
        let (outcomes, decoded) = within(Duration::from_secs(120), move || {
            let (mut outcomes, mut decoded) = ([0; 2], 0);
            for at in moov..moov + size {
                for value in [0, 1, 0xff, data[at] ^ 0x80, data[at].wrapping_add(1)] {
                    let mut bad = data.clone();
                    bad[at] = value;
                    std::fs::write(&path, &bad).unwrap();
                    let opened = open_media(&path, Scan::First);
                    outcomes[opened.is_ok() as usize] += 1;
                    // Decode a sample of the ones that open: VideoToolbox gets damaged parameter sets too.
                    if let Ok(source) = opened
                        && at % 7 == 0
                    {
                        let _ = Decoder::new().frame(&source, 0);
                        decoded += 1;
                    }
                }
            }
            (outcomes, decoded)
        });
        assert!(outcomes[0] > 0 && outcomes[1] > 0, "refused {}, opened {}", outcomes[0], outcomes[1]);
        assert!(decoded > 0);
    }

    /// Samples are read from the file when decoded, not when it's opened.
    #[test]
    fn reads_samples_when_they_are_decoded() {
        let dir = test_dir("on-demand");
        let (mp4, mut data) = recording(&dir);
        let source = mp4_source(&mp4);
        let s = video(&source).samples[25];
        // Damage frame 25 on disk after opening (same file, written in place).
        data[s.offset as usize + 4..(s.offset + u64::from(s.size)) as usize].fill(0);
        std::fs::write(&mp4, &data).unwrap();
        let mut dec = Decoder::new();
        assert!(dec.frame(&source, 21).unwrap().is_some());
        assert!(dec.frame(&source, 25).is_err(), "frame 25 is read from the file as it is now");
    }

    /// One system decoder session plays a video through: across keyframes,
    /// and across seeks back and forth (each restarting at an IDR on the live
    /// session), with every picture exactly what a fresh decoder makes.
    #[test]
    fn keeps_one_decoder_session_across_keyframes_and_seeks() {
        let dir = test_dir("session");
        let mp4 = dir.join("gops.mp4");
        // Keyframes every 2 s: 0, 20, 40.
        crate::recorder::write_test_video(&mp4, 60, 10, (96, 64));
        let source = mp4_source(&mp4);
        let keyframes: Vec<usize> = (0..60).filter(|&i| video(&source).samples[i].sync).collect();
        assert_eq!(keyframes, [0, 20, 40]);
        let mut reused = Decoder::new();
        for i in (0..60).chain([45, 44, 5, 25, 21, 20, 59, 0, 39, 40, 41, 10, 10]) {
            let picture = reused.frame(&source, i).unwrap().expect("picture");
            let fresh = Decoder::new().frame(&source, i).unwrap().expect("picture");
            assert!(picture == fresh, "frame {i} differs from a fresh decoder's");
        }
        assert_eq!(reused.sessions, 1, "one session for everything");
    }

    /// x264's intra refresh marks recovery points as sync samples, but only
    /// its first frame is an IDR: seeking decodes from there, on one session,
    /// to exactly the pictures playing in order gives.
    #[test]
    fn seeks_in_x264_intra_refresh_videos() {
        let dir = test_dir("intra-refresh");
        let path = dir.join("intra-refresh.mp4");
        if !ffmpeg(&["-c:v", "libx264", "-bf", "0", "-x264-params", "intra-refresh=1:keyint=20"], &path) {
            eprintln!("skipping: no ffmpeg with libx264");
            return;
        }
        let source = mp4_source(&path);
        let mut buf = Vec::new();
        let recovery: Vec<usize> = (0..source.len())
            .filter(|&k| video(&source).samples[k].sync && {
                video(&source).read(k, &mut buf).unwrap();
                !crate::h264::has_idr(&buf)
            })
            .collect();
        assert!(!recovery.is_empty(), "no recovery points");
        let mut sequential = Decoder::new();
        let pictures: Vec<RgbaImage> = (0..source.len()).map(|i| sequential.frame(&source, i).unwrap().expect("picture")).collect();
        let mut seeking = Decoder::new();
        for i in [recovery[0] + 5, recovery[0], source.len() - 1, 3, recovery[recovery.len() - 1]] {
            assert!(seeking.frame(&source, i).unwrap().expect("picture") == pictures[i], "frame {i}");
        }
        assert_eq!(seeking.sessions, 1);
    }

    /// A sync sample that's no IDR (x264's intra-refresh recovery point) is
    /// never a restart point: decoding can't start there.
    #[test]
    fn never_restarts_at_a_recovery_point() {
        let dir = test_dir("recovery");
        let (_, mut data) = recording(&dir);
        // Mark frame 10 (a P-frame) as a sync sample instead of frame 20 (an IDR).
        let (stss, _) = find_box(&data, "moov/trak/mdia/minf/stbl/stss");
        assert_eq!(u32::from_be_bytes(data[stss + 20..stss + 24].try_into().unwrap()), 21);
        put_u32(&mut data, stss + 20, 11);
        let mp4 = dir.join("recovery.mp4");
        std::fs::write(&mp4, &data).unwrap();
        let source = mp4_source(&mp4);
        assert!(video(&source).samples[10].sync && !video(&source).samples[20].sync);
        let mut sequential = Decoder::new();
        let pictures: Vec<RgbaImage> = (0..30).map(|i| sequential.frame(&source, i).unwrap().expect("picture")).collect();
        let mut seeking = Decoder::new();
        for i in [15, 12, 25, 10, 11, 29, 3, 19] {
            assert!(seeking.frame(&source, i).unwrap().expect("picture") == pictures[i], "frame {i}");
        }
        assert!(seeking.not_idr.contains(10));
        assert_eq!(seeking.sessions, 1);
    }

    /// A video of `n` samples of one NAL unit each, in a file in `dir`: an
    /// IDR picture where `idr` says, another slice elsewhere; sync samples
    /// where `sync` says.
    fn nal_video(dir: &Path, n: usize, sync: impl Fn(usize) -> bool, idr: impl Fn(usize) -> bool) -> Video {
        let path = dir.join("nals.h264");
        std::fs::write(&path, (0..n).flat_map(|k| [0, 0, 0, 1, if idr(k) { 0x65 } else { 0x41 }]).collect::<Vec<u8>>()).unwrap();
        Video {
            file: Mutex::new(File::open(&path).unwrap()),
            samples: (0..n).map(|k| Sample { offset: 5 * k as u64, size: 5, time: k as f64 / 30.0, sync: sync(k) }).collect(),
            sps: Vec::new(),
            pps: Vec::new(),
            size: (16, 16),
            len_size: 4,
            reordered: false,
            duration: n as f64 / 30.0,
        }
    }

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Restart points are the latest sync sample holding an IDR at or before
    /// each frame, in whatever order frames are asked for.
    #[test]
    fn finds_the_restart_point_of_any_frame() {
        let dir = test_dir("restart-points");
        let n = 400;
        let sync = |k: usize| k.is_multiple_of(7) || k % 11 == 3;
        // Sync samples without an IDR, and an IDR that isn't a sync sample.
        let idr = |k: usize| (k.is_multiple_of(7) && !k.is_multiple_of(3)) || k == 150;
        let video = nal_video(&dir, n, sync, idr);
        let mut dec = Decoder::new();
        let (mut rng, mut i) = (7, 0);
        for _ in 0..3000 {
            let r = xorshift(&mut rng);
            let step = (r >> 8) as usize % 40;
            i = match r % 4 {
                0 => i + 1,
                1 => (r >> 8) as usize % (n + 20),
                2 => i.saturating_sub(step),
                _ => i + step,
            };
            let expected = (0..=i.min(n - 1)).rev().find(|&k| sync(k) && idr(k));
            assert_eq!(dec.restart_point(&video, i).unwrap(), expected, "frame {i}");
        }
    }

    /// Finding where decoding can restart walks each sample once: frames up
    /// to the last one asked about share its answer, and a later frame walks
    /// only the samples after it. A video whose samples are all sync but none
    /// an IDR (a damaged one, without a sync table) walked every sample
    /// before a frame, for every frame asked for.
    #[test]
    fn finds_restart_points_without_walking_again() {
        let dir = test_dir("restart-walk");
        let n = 20_000;
        let video = nal_video(&dir, n, |_| true, |_| false);
        let mut dec = Decoder::new();
        assert_eq!(dec.restart_point(&video, n - 40).unwrap(), None);
        let walked = dec.steps;
        assert!(walked <= n);
        // Playing on, then seeking back and forth.
        for i in (n - 39..n).chain([n / 2, 3, n - 1, n - 20]) {
            assert_eq!(dec.restart_point(&video, i).unwrap(), None);
        }
        assert!(dec.steps - walked <= 40, "{} more steps", dec.steps - walked);
    }

    /// `data` with frame `i`'s slice data zeroed: the decoder refuses it.
    fn damage_frame(data: &mut [u8], video: &Video, i: usize) {
        let s = video.samples[i];
        data[s.offset as usize + 5..(s.offset + u64::from(s.size)) as usize].fill(0);
    }

    /// A recording whose frame 5 is damaged (keyframes at 0 and 20).
    fn damaged_recording(dir: &Path) -> PathBuf {
        let (mp4, mut data) = recording(dir);
        damage_frame(&mut data, &open_mp4(&mp4).unwrap(), 5);
        let bad = dir.join("damaged-frame.mp4");
        std::fs::write(&bad, &data).unwrap();
        bad
    }

    /// A damaged frame spoils only the frames that need it, up to the next
    /// keyframe: after it, and when seeking around, frames decode again.
    #[test]
    fn a_damaged_frame_spoils_only_its_own_gop() {
        let dir = test_dir("damaged-gop");
        let source = mp4_source(&damaged_recording(&dir));
        let mut dec = Decoder::new();
        let results: Vec<bool> = (0..30).map(|i| dec.frame(&source, i).is_ok_and(|p| p.is_some())).collect();
        if results[5] {
            eprintln!("this system's decoder conceals the damage; nothing to check");
            return;
        }
        assert_eq!(results, (0..30).map(|i| !(5..20).contains(&i)).collect::<Vec<_>>());
        let err = dec.frame(&source, 12).unwrap_err();
        assert!(format!("{err:#}").contains("depends on frame 5"), "{err:#}");
        assert!(dec.frame(&source, 3).unwrap().is_some() && dec.frame(&source, 25).unwrap().is_some());
    }

    /// The player shows a damaged frame's error only until a frame decodes.
    #[test]
    fn playback_clears_an_error_once_a_frame_decodes() {
        let dir = test_dir("playback-error");
        let source = mp4_source(&damaged_recording(&dir));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        let s = shared.clone();
        std::thread::spawn(move || playback(source, rx, s, egui::Context::default()));
        let wait_for = |what: &str, done: &dyn Fn(&Shared) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(&lock(&shared)) {
                assert!(Instant::now() < deadline, "never {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for("showed frame 0", &|s| s.frame.is_some());
        lock(&shared).frame = None;
        tx.send(Command::Seek(0.55)).unwrap();
        wait_for("showed an error", &|s| s.error.is_some() || s.frame.is_some());
        if lock(&shared).frame.is_some() {
            eprintln!("this system's decoder conceals the damage; nothing to check");
            return;
        }
        tx.send(Command::Seek(2.55)).unwrap();
        wait_for("cleared the error", &|s| s.error.is_none() && s.frame.is_some());
    }

    /// An export leaves out the frames that don't decode; it fails only when
    /// none do.
    #[test]
    fn exports_leave_out_frames_that_dont_decode() {
        let dir = test_dir("export-damaged");
        let bad = damaged_recording(&dir);
        let cut = dir.join("cut.mp4");
        export_trimmed(&bad, &cut, 0.0, 3.0, GifQuality::default()).unwrap();
        let shown = frame_numbers(&cut);
        if shown == (0..30).collect::<Vec<_>>() {
            eprintln!("this system's decoder conceals the damage; nothing left out");
        } else {
            assert_eq!(shown[..5], [0, 1, 2, 3, 4]);
            assert!(shown.iter().all(|i| !(5..20).contains(i)), "{shown:?}");
            assert!(shown.ends_with(&(20..30).collect::<Vec<_>>()), "{shown:?}");
        }

        let (mp4, mut data) = recording(&dir);
        let index = open_mp4(&mp4).unwrap();
        for i in 0..30 {
            damage_frame(&mut data, &index, i);
        }
        let ruined = dir.join("ruined.mp4");
        std::fs::write(&ruined, &data).unwrap();
        let out = dir.join("ruined-cut.mp4");
        if Decoder::new().frame(&mp4_source(&ruined), 0).is_err() {
            assert!(export_trimmed(&ruined, &out, 0.0, 3.0, GifQuality::default()).is_err());
            assert!(!out.exists() && !part_path(&out).exists());
        }
    }

    /// A GIF of `frames` on a `w`×`h` screen with `global` as its palette.
    fn write_gif(path: &Path, screen: (u16, u16), global: &[u8], frames: &[gif::Frame]) {
        write_gif_to(File::create(path).unwrap(), screen, global, frames);
    }

    fn write_gif_to(out: impl std::io::Write, (w, h): (u16, u16), global: &[u8], frames: &[gif::Frame]) {
        let mut encoder = gif::Encoder::new(out, w, h, global).unwrap();
        for frame in frames {
            encoder.write_frame(frame).unwrap();
        }
    }

    /// A frame at `(left, top)`, `w`×`h`, with palette indices from `index`.
    fn gif_frame_at((left, top, w, h): (u16, u16, u16, u16), index: impl Fn(u16, u16) -> u8) -> gif::Frame<'static> {
        let buffer = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| index(x, y)).collect::<Vec<_>>();
        gif::Frame { left, top, width: w, height: h, buffer: buffer.into(), delay: 4, ..gif::Frame::default() }
    }

    fn palette(colors: usize, seed: u8) -> Vec<u8> {
        (0..colors * 3).map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed)).collect()
    }

    /// Frames exercising what GIF compositing has: every disposal method,
    /// transparency, local palettes, interlacing, indices past the palette,
    /// and frames partly or wholly off the screen.
    fn tricky_frames() -> Vec<gif::Frame<'static>> {
        use gif::DisposalMethod::*;
        let mut frames = Vec::new();
        let mut add = |rect, index: &dyn Fn(u16, u16) -> u8, set: &dyn Fn(&mut gif::Frame)| {
            let mut frame = gif_frame_at(rect, index);
            set(&mut frame);
            frames.push(frame);
        };
        add((0, 0, 40, 30), &|x, y| ((x + y) % 4) as u8, &|f| f.dispose = Keep);
        add((5, 5, 10, 10), &|x, _| (x % 8) as u8, &|f| {
            f.palette = Some(palette(8, 3));
            f.transparent = Some(0);
            f.dispose = Background;
            f.delay = 1;
        });
        add((0, 0, 20, 15), &|x, y| ((x * y) % 4) as u8, &|f| {
            f.transparent = Some(1);
            f.dispose = Previous;
        });
        add((35, 25, 10, 10), &|x, y| ((x + 2 * y) % 4) as u8, &|f| f.dispose = Keep);
        add((10, 0, 20, 30), &|x, y| (x * 7 + y * 3) as u8, &|f| {
            f.palette = Some(palette(256, 9));
            f.interlaced = true;
            f.dispose = Any;
            f.delay = 0;
        });
        add((0, 0, 40, 30), &|x, y| ((x / 3 + y / 2) % 4) as u8, &|f| {
            f.transparent = Some(2);
            f.dispose = Background;
        });
        add((39, 29, 1, 1), &|_, _| 3, &|f| f.dispose = Previous);
        add((0, 0, 8, 8), &|x, y| ((x + y) % 4) as u8, &|f| {
            // Indices 2 and 3 are past this palette's 2 colors.
            f.palette = Some(palette(2, 200));
            f.dispose = Keep;
        });
        add((60, 40, 5, 5), &|_, _| 1, &|f| f.dispose = Background);
        add((3, 7, 30, 20), &|x, y| ((x ^ y) % 4) as u8, &|f| {
            f.transparent = Some(3);
            f.dispose = Previous;
            f.delay = 9;
        });
        add((0, 0, 40, 30), &|x, _| (x % 4) as u8, &|f| f.dispose = Keep);
        frames
    }

    /// Frames as `image` composites them, with when each starts, and the
    /// whole duration, as the player used to time them.
    fn image_frames(path: &Path) -> (Vec<(f64, RgbaImage)>, f64) {
        use image::AnimationDecoder;
        let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(File::open(path).unwrap())).unwrap();
        let mut t = 0.0;
        let frames = decoder
            .into_frames()
            .map(|frame| {
                let frame = frame.unwrap();
                let (num, den) = frame.delay().numer_denom_ms();
                let start = t;
                t += (num as f64 / den.max(1) as f64 / 1000.0).max(0.02);
                (start, frame.into_buffer())
            })
            .collect();
        (frames, t)
    }

    fn wait_until_read(gif: &Gif) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !lock(&gif.frames).done {
            assert!(Instant::now() < deadline, "the GIF was never read to the end");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Composited GIF frames are exactly `image`'s, played in order or reached
    /// by seeking (from the background scan's snapshots).
    #[test]
    fn gif_frames_match_the_image_crate() {
        let dir = test_dir("gif-oracle");
        let with_global = dir.join("global.gif");
        write_gif(&with_global, (40, 30), &palette(4, 0), &tricky_frames());
        // No global palette: every frame brings its own.
        let local_only = dir.join("local.gif");
        let local: Vec<_> = tricky_frames()
            .into_iter()
            .enumerate()
            .map(|(i, mut f)| {
                f.palette.get_or_insert_with(|| palette(4, i as u8 * 11));
                f
            })
            .collect();
        write_gif(&local_only, (40, 30), &[], &local);
        let mut gifs = vec![with_global, local_only];
        // ffmpeg's GIFs store each frame's changed rectangle, unchanged pixels transparent.
        let ffmpeg_gif = dir.join("ffmpeg.gif");
        if ffmpeg(&["-t", "1", "-vf", "split[a][b];[a]palettegen[p];[b][p]paletteuse"], &ffmpeg_gif) {
            gifs.push(ffmpeg_gif);
        } else {
            eprintln!("skipping ffmpeg's GIF: no ffmpeg");
        }

        for path in gifs {
            let (expected, duration) = image_frames(&path);
            let source = Source::Gif(open_gif(&path, Scan::All).unwrap());
            let mut dec = Decoder::new();
            for (i, (t, picture)) in expected.iter().enumerate() {
                assert_eq!(source.time(i), *t, "frame {i} time");
                assert!(dec.frame(&source, i).unwrap().unwrap() == *picture, "{}: frame {i} in order", path.display());
            }
            assert_eq!(source.duration(), duration);

            let gif = open_gif(&path, Scan::Background).unwrap();
            wait_until_read(&gif);
            assert!(!lock(&gif.frames).snapshots.is_empty(), "snapshots to seek from");
            let source = Source::Gif(gif);
            let mut dec = Decoder::new();
            for i in [7, 2, 9, 9, 0, 10, 4, 5, 1, 8, 3, 6] {
                let i = i * expected.len() / 11;
                assert!(dec.frame(&source, i).unwrap().unwrap() == expected[i].1, "{}: frame {i} seeking", path.display());
            }
        }
    }

    /// A long GIF keeps a bounded number of snapshots, and seeking from them
    /// still gives `image`'s frames.
    #[test]
    fn long_gifs_keep_few_snapshots() {
        let dir = test_dir("gif-long");
        let path = dir.join("long.gif");
        let frames: Vec<_> = (0..300u16)
            .map(|i| {
                let mut f = gif_frame_at((0, 0, 64, 64), move |x, y| ((x + y + i) % 4) as u8);
                f.transparent = Some((i % 4) as u8);
                f
            })
            .collect();
        write_gif(&path, (64, 64), &palette(4, 0), &frames);
        let (expected, _) = image_frames(&path);
        let gif = open_gif(&path, Scan::Background).unwrap();
        wait_until_read(&gif);
        let policy = Snapshots::for_screen(&gif.screen);
        let kept = lock(&gif.frames).snapshots.iter().map(|(k, _)| *k).collect::<Vec<_>>();
        assert!(kept.len() > policy.keep / 2 && kept.len() <= policy.keep, "{} snapshots, keeping {}", kept.len(), policy.keep);
        assert!(kept.windows(2).all(|pair| pair[0] < pair[1]));
        let source = Source::Gif(gif);
        let mut dec = Decoder::new();
        for i in [299, 150, 151, 3, 298, 77, 0, 200] {
            assert!(dec.frame(&source, i).unwrap().unwrap() == expected[i].1, "frame {i}");
        }
    }

    /// Opening a GIF reads only its first frame; the rest is read in the
    /// background (the timeline filling in), which stops when the player goes.
    #[test]
    fn gifs_open_at_their_first_frame() {
        let dir = test_dir("gif-open");
        let path = dir.join("t.gif");
        crate::recorder::write_test_gif(&path, 40);
        let first = open_gif(&path, Scan::First).unwrap();
        assert_eq!(lock(&first.frames).frames.len(), 1);
        let gif = open_gif(&path, Scan::Background).unwrap();
        wait_until_read(&gif);
        assert_eq!(lock(&gif.frames).frames.len(), 40);
        assert_eq!(lock(&gif.frames).duration, image_frames(&path).1);

        // Closed while still being read: the scan lets go of it.
        let long = dir.join("long.gif");
        let frames: Vec<_> = (0..3000u16).map(|i| gif_frame_at((0, 0, 256, 256), move |x, y| ((x ^ y ^ i) % 4) as u8)).collect();
        write_gif(&long, (256, 256), &palette(4, 0), &frames);
        let gif = open_gif(&long, Scan::Background).unwrap();
        let table = Arc::downgrade(&gif.frames);
        drop(gif);
        let deadline = Instant::now() + Duration::from_secs(10);
        while table.strong_count() > 0 {
            assert!(Instant::now() < deadline, "the background scan kept a closed GIF");
            std::thread::sleep(Duration::from_millis(1));
        }

        // A GIF cut short plays up to where it ends.
        let data = std::fs::read(&path).unwrap();
        let cut = dir.join("cut.gif");
        std::fs::write(&cut, &data[..data.len() * 3 / 4]).unwrap();
        let gif = open_gif(&cut, Scan::Background).unwrap();
        wait_until_read(&gif);
        let source = Source::Gif(gif);
        assert!((10..40).contains(&source.len()), "{} frames", source.len());
        let mut dec = Decoder::new();
        assert!((0..source.len()).all(|i| dec.frame(&source, i).is_ok()));
    }

    /// A frame whose data ends before its last pixel (which sends `gif`'s
    /// bare-LZW decoder into an endless loop) is an error; the frames around
    /// it still play.
    #[test]
    fn skips_a_gif_frame_whose_data_ends_early() {
        let dir = test_dir("gif-short");
        let colors = palette(4, 0);
        // A 4×4 frame's data, as `gif` reads it without decoding.
        let mut small = Vec::new();
        write_gif_to(&mut small, (4, 4), &colors, &[gif_frame_at((0, 0, 4, 4), |_, _| 1)]);
        let mut options = gif::DecodeOptions::new();
        options.skip_frame_decoding(true);
        let mut short = options.read_info(small.as_slice()).unwrap().read_next_frame().unwrap().unwrap().clone();
        (short.width, short.height) = (8, 8);
        let path = dir.join("short.gif");
        let mut file = File::create(&path).unwrap();
        let mut encoder = gif::Encoder::new(&mut file, 8, 8, &colors).unwrap();
        encoder.write_frame(&gif_frame_at((0, 0, 8, 8), |x, _| (x % 4) as u8)).unwrap();
        encoder.write_lzw_pre_encoded_frame(&short).unwrap();
        encoder.write_frame(&gif_frame_at((2, 2, 4, 4), |_, y| (y % 4) as u8)).unwrap();
        drop(encoder);

        let source = within(Duration::from_secs(10), move || {
            let gif = open_gif(&path, Scan::Background).unwrap();
            wait_until_read(&gif);
            Source::Gif(gif)
        });
        assert_eq!(source.len(), 3);
        let mut dec = Decoder::new();
        assert!(dec.frame(&source, 0).is_ok());
        let err = dec.frame(&source, 1).unwrap_err();
        assert!(format!("{err:#}").contains("truncated"), "{err:#}");
        assert!(dec.frame(&source, 2).is_ok());
    }

    /// A crash while a GIF is read in the background ends the reading there,
    /// as damage does: the GIF counts as read, rather than playing on to
    /// frames that never come.
    #[test]
    fn a_crash_reading_a_gif_ends_its_reading() {
        let frames = Arc::new(Mutex::new(GifFrames::default()));
        read_rest(&Arc::downgrade(&frames), "crash.gif", || panic!("a bug in the gif crate"));
        assert!(lock(&frames).done);
    }

    #[test]
    fn plays_gifs_while_they_are_read() {
        let dir = test_dir("gif-player");
        let path = dir.join("t.gif");
        crate::recorder::write_test_gif(&path, 40);
        let ctx = egui::Context::default();
        let player = Player::open(&ctx, path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock(&player.shared).duration < 3.9 || lock(&player.shared).frame.is_none() {
            assert!(Instant::now() < deadline, "the timeline never filled in");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// New frames replace the texture in place when they're its size.
    #[test]
    fn uploads_frames_in_place() {
        let dir = test_dir("upload");
        let (mp4, _) = recording(&dir);
        let ctx = egui::Context::default();
        let mut player = Player::open(&ctx, mp4).unwrap();
        let frame = |w, h| RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        // Where each change to the player's texture goes: `None` for a new image.
        let upload = |player: &mut Player, img: RgbaImage| {
            player.upload(&ctx, &img);
            let id = player.texture.as_ref().unwrap().id();
            let mut delta = ctx.tex_manager().write().take_delta();
            let positions = delta.set.get(&id).into_iter().flatten().map(|d| d.pos).collect::<Vec<_>>();
            delta.clear();
            positions
        };
        assert_eq!(upload(&mut player, frame(96, 64)), [None]);
        let id = player.texture.as_ref().unwrap().id();
        assert_eq!(upload(&mut player, frame(96, 64)), [Some([0, 0])]);
        assert_eq!(upload(&mut player, frame(48, 32)), [None]);
        assert_eq!(player.texture.as_ref().unwrap().id(), id);
        // Opaque pixels, and fully transparent ones, are the same premultiplied or not.
        let pixels = RgbaImage::from_raw(3, 1, vec![200, 100, 50, 255, 0, 0, 0, 0, 1, 2, 3, 255]).unwrap();
        assert!(color_image(&pixels) == egui::ColorImage::from_rgba_unmultiplied([3, 1], pixels.as_raw()));
    }

    /// Each picture goes to the GPU as one texture, and a bigger one than it
    /// takes crashed the app: such a video, wider or taller, is left to the
    /// system's player.
    #[test]
    fn leaves_videos_too_big_for_the_gpu_to_the_system() {
        let dir = test_dir("too-big");
        let (_, data) = recording(&dir);
        // The size the file gives, in its sample description (its pictures stay 96×64).
        let (avc1, _) = find_box(&data, "moov/trak/mdia/minf/stbl/stsd/avc1");
        let mp4 = |w: u16, h: u16| {
            let mut data = data.clone();
            data[avc1 + 32..avc1 + 36].copy_from_slice(&[w.to_be_bytes(), h.to_be_bytes()].concat());
            let path = dir.join(format!("{w}x{h}.mp4"));
            std::fs::write(&path, data).unwrap();
            path
        };
        let gif = |w: u16, h: u16| {
            let path = dir.join(format!("{w}x{h}.gif"));
            write_gif(&path, (w, h), &palette(4, 0), &[gif_frame_at((0, 0, 4, 4), |x, _| (x % 4) as u8)]);
            path
        };
        // What eframe's renderer reports; egui's own default is lower.
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput { max_texture_side: Some(8192), ..Default::default() }, |_| {}).textures_delta.clear();
        let cases = [(mp4(8192, 64), true), (mp4(8208, 64), false), (mp4(96, 8208), false), (gif(8192, 40), true), (gif(8208, 40), false), (gif(40, 8208), false)];
        for (path, fits) in cases {
            match Player::open(&ctx, path.clone()) {
                Ok(_) => assert!(fits, "{} opened", path.display()),
                Err(e) => {
                    assert!(!fits, "{}: {e:#}", path.display());
                    assert!(format!("{e:#}").contains("8192"), "{e:#}");
                }
            }
        }
    }

    /// A copy appears under its name only once it's complete: a cancelled or
    /// failed export leaves nothing behind, not even its partial file.
    #[test]
    fn exports_appear_only_when_complete() {
        let dir = test_dir("export-part");
        let mp4 = dir.join("long.mp4");
        crate::recorder::write_test_video(&mp4, 150, 30, (320, 240));
        let dst = dir.join("Rega copy.mp4");
        assert_eq!(part_path(&dst), dir.join(".Rega copy.mp4.part"));
        assert!(crate::library::kind_of(&part_path(&dst)).is_none(), "the library doesn't list partial files");

        for delay in [0, 5, 20] {
            let cancel = Arc::new(AtomicBool::new(delay == 0));
            let stop = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                stop.store(true, Ordering::Relaxed);
            });
            assert!(export(&mp4, &dst, 0.0, 5.0, GifQuality::default(), &cancel).is_err());
            assert!(!dst.exists() && !part_path(&dst).exists(), "cancelled after {delay} ms");
        }
        let nowhere = dir.join("missing").join("copy.gif");
        assert!(export_trimmed(&mp4, &nowhere, 0.0, 5.0, GifQuality::default()).is_err());
        assert!(!part_path(&nowhere).exists());

        export_trimmed(&mp4, &dst, 0.0, 5.0, GifQuality::default()).unwrap();
        assert!(dst.exists() && !part_path(&dst).exists());
        assert_eq!(frame_count(&dst), 150);

        // A file that took the name meanwhile (a recording started the same
        // second) is kept: the copy gets a name of its own.
        let taken = dir.join("Rega taken.mp4");
        std::fs::write(&taken, b"a recording").unwrap();
        let saved = export(&mp4, &taken, 0.0, 1.0, GifQuality::default(), &AtomicBool::new(false)).unwrap();
        assert_ne!(saved, taken);
        assert_eq!(std::fs::read(&taken).unwrap(), b"a recording");
        assert_eq!(frame_count(&saved), 30);
        assert!(!part_path(&taken).exists());
    }

    #[test]
    fn fits_export_frames_to_one_size() {
        let frame = |w, h| RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]));
        let mut fit = FrameFit::new(None);
        assert_eq!(fit.apply(frame(100, 60)).dimensions(), (100, 60));
        assert_eq!(fit.apply(frame(50, 30)).dimensions(), (100, 60), "a stream changing size midway");
        assert_eq!(fit.apply(frame(100, 60)).dimensions(), (100, 60));
        let mut shrink = FrameFit::new(Some(640));
        assert_eq!(shrink.apply(frame(1280, 721)).dimensions(), (640, 361));
        assert_eq!(shrink.apply(frame(1000, 1000)).dimensions(), (640, 361));
        let mut narrow = FrameFit::new(Some(640));
        assert_eq!(narrow.apply(frame(320, 200)).dimensions(), (320, 200));
    }

    /// A save whose thread died is a failure, never "Saving…" forever; one
    /// the user cancelled is no news; closing the player cancels its save.
    #[test]
    fn reports_how_a_save_ended() {
        let dir = test_dir("save-outcome");
        let (mp4, _) = recording(&dir);
        let mut player = Player::open(&egui::Context::default(), mp4).unwrap();
        let mut saving = |result: Option<Result<PathBuf>>, cancelled: bool| {
            let (tx, rx) = mpsc::channel();
            if let Some(result) = result {
                tx.send(result).unwrap();
            }
            drop(tx);
            let cancel = Arc::new(AtomicBool::new(cancelled));
            player.saving = Some(SaveJob { result: rx, cancel: cancel.clone(), started: Instant::now() });
            (player.poll_save(), player.saving.is_none(), cancel)
        };
        assert!(matches!(saving(None, false), (PlayerAction::Failed(msg), true, _) if msg.contains("stopped unexpectedly")));
        assert!(matches!(saving(Some(Err(anyhow::anyhow!("disk full"))), false), (PlayerAction::Failed(msg), true, _) if msg.contains("disk full")));
        assert!(matches!(saving(Some(Err(anyhow::anyhow!("saving was cancelled"))), true), (PlayerAction::None, true, _)));
        assert!(matches!(saving(Some(Ok(PathBuf::from("x.gif"))), false), (PlayerAction::Saved(_), true, _)));
        assert!(caught("the export", || -> Result<()> { panic!("boom") }).is_err_and(|e| e.to_string().contains("crashed")));

        // Closing the player hands a running save to the app, which reports it.
        let saves = BackgroundSaves::default();
        player.hand_saves_to(saves.clone());
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        player.saving = Some(SaveJob { result: rx, cancel: cancel.clone(), started: Instant::now() });
        drop(player);
        assert!(!cancel.load(Ordering::Relaxed), "closing the player keeps its save going");
        assert!(poll_background_saves(&saves).is_empty(), "still running");
        tx.send(Ok(PathBuf::from("x.gif"))).unwrap();
        assert!(matches!(poll_background_saves(&saves)[..], [PlayerAction::Saved(_)]));
        assert!(lock(&saves).is_empty());

        // Quitting cancels what is left, and waits for it to clean up.
        let (tx, rx) = mpsc::channel();
        lock(&saves).push(SaveJob { result: rx, cancel: cancel.clone(), started: Instant::now() });
        let (stop, cleaned) = (cancel.clone(), Arc::new(AtomicBool::new(false)));
        let cleaned_t = cleaned.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            cleaned_t.store(true, Ordering::Relaxed);
            tx.send(Err(anyhow::anyhow!("saving was cancelled"))).unwrap();
        });
        cancel_background_saves(&saves, Duration::from_secs(5));
        assert!(cleaned.load(Ordering::Relaxed), "waited for the cancelled export to clean up");
        assert!(lock(&saves).is_empty());
    }

    /// Closing the player while it saves hands the save to the app, which
    /// hears how it went; with nowhere to report to, the copy is still made.
    #[test]
    fn a_save_outlives_its_player() {
        let dir = test_dir("save-outlives");
        let (mp4, _) = recording(&dir);
        let ctx = egui::Context::default();
        let saves = BackgroundSaves::default();
        let mut player = Player::open(&ctx, mp4.clone()).unwrap();
        player.hand_saves_to(saves.clone());
        player.save_trimmed(0.0, 3.0, None);
        drop(player);
        assert_eq!(lock(&saves).len(), 1, "handed over");
        let deadline = Instant::now() + Duration::from_secs(30);
        let saved = loop {
            match poll_background_saves(&saves).pop() {
                Some(PlayerAction::Saved(path)) => break path,
                Some(PlayerAction::Failed(e)) => panic!("{e}"),
                _ => assert!(Instant::now() < deadline, "never saved"),
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(frame_count(&saved), 30);
        assert!(!part_path(&saved).exists());

        let mut player = Player::open(&ctx, mp4).unwrap();
        player.save_trimmed(0.0, 3.0, Some(GifQuality::Small));
        drop(player);
        let gif = loop {
            let found = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|e| e == "gif"));
            if let Some(gif) = found {
                break gif;
            }
            assert!(Instant::now() < deadline, "the GIF never turned up");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(Source::Gif(open_gif(&gif, Scan::All).unwrap()).len(), 30);
    }

    /// The player as the app shows it, in a 900×600 window, run one UI pass
    /// at a time with the input given.
    struct Window {
        ctx: egui::Context,
        player: Player,
        start: Instant,
    }

    impl Window {
        fn open(path: &Path) -> Self {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let player = Player::open(&ctx, path.to_path_buf()).unwrap();
            Self { ctx, player, start: Instant::now() }
        }

        fn pass(&mut self, events: Vec<egui::Event>) -> PlayerAction {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0))),
                time: Some(self.start.elapsed().as_secs_f64()),
                events,
                ..Default::default()
            };
            let (player, mut action) = (&mut self.player, PlayerAction::None);
            let mut output = self.ctx.run_ui(input, |ui| {
                action = egui::CentralPanel::default().frame(egui::Frame::new()).show(ui, |ui| player.ui(ui)).inner;
            });
            // Nothing is drawn.
            output.textures_delta.clear();
            action
        }

        /// The playhead, and whether it's playing.
        fn state(&self) -> (f64, bool) {
            let s = lock(&self.player.shared);
            (s.position, s.playing)
        }

        /// UI passes, as the app runs them while a video plays, until `done`.
        fn until(&mut self, what: &str, done: impl Fn(f64, bool) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.pass(vec![]);
                let (position, playing) = self.state();
                if done(position, playing) {
                    return;
                }
                assert!(Instant::now() < deadline, "never {what}: at {position:.3} s, playing {playing}");
                std::thread::sleep(Duration::from_millis(4));
            }
        }

        fn press(&mut self, key: egui::Key) {
            for pressed in [true, false] {
                self.pass(vec![egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Default::default() }]);
            }
        }

        fn click(&mut self, at: Pos2) {
            let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
            self.pass(vec![egui::Event::PointerMoved(at), button(true)]);
            self.pass(vec![button(false)]);
        }
    }

    /// Trimmed to `a`..`b`, with In inside the frame starting at `frame`,
    /// Play plays the selection, and at Out playback stops, back at In.
    fn plays_the_selection(path: &Path, frame: f64, a: f64, b: f64) {
        let mut window = Window::open(path);
        // A GIF is read on in the background: have it read past Out, as it
        // is by the time a handle can be dragged there.
        let deadline = Instant::now() + Duration::from_secs(5);
        while window.player.duration <= b {
            assert!(Instant::now() < deadline, "never read up to {b} s");
            window.pass(vec![]);
            std::thread::sleep(Duration::from_millis(4));
        }
        window.player.send(Command::Pause);
        window.player.trim = Some((a, b));
        window.player.send(Command::Seek(a));
        window.until("showed In", |position, playing| !playing && (frame..=a).contains(&position));
        window.press(egui::Key::Space);
        window.until("played on from In", |position, _| position > a);
        window.until("stopped back at In", |position, playing| !playing && position == a);
    }

    /// A selection whose In point lies more than 50 ms into a frame (15 fps
    /// video, Rega's GIFs) plays: it used to stay on In's frame, seeking back
    /// to it over and over.
    #[test]
    fn plays_a_selection_from_late_in_a_frame() {
        let dir = test_dir("trim-play");
        let mp4 = dir.join("t.mp4");
        crate::recorder::write_test_video(&mp4, 30, 15, (96, 64));
        // Frame 15 starts at 1 s and lasts 67 ms.
        plays_the_selection(&mp4, 1.0, 1.055, 1.3);
        let gif = dir.join("t.gif");
        crate::recorder::write_test_gif(&gif, 20);
        // 10 fps: frame 5 starts at 0.5 s.
        plays_the_selection(&gif, 0.5, 0.56, 0.8);
    }

    /// I and O set the selection at the playhead, on a frame; playing it
    /// stops at Out, back at In, and Play at Out plays it again from In.
    #[test]
    fn plays_a_selection_set_with_the_keys() {
        let dir = test_dir("trim-keys");
        let mp4 = dir.join("t.mp4");
        crate::recorder::write_test_video(&mp4, 30, 15, (96, 64));
        let source = mp4_source(&mp4);
        let (a, b) = (source.time(15), source.time(21));
        let mut window = Window::open(&mp4);
        window.player.send(Command::Pause);
        window.player.send(Command::Seek(b));
        window.until("showed Out", |position, playing| !playing && position == b);
        window.press(egui::Key::O);
        window.player.send(Command::Seek(a));
        window.until("showed In", |position, playing| !playing && position == a);
        window.press(egui::Key::I);
        assert_eq!(window.player.trim, Some((a, b)));
        window.press(egui::Key::Space);
        window.until("played on from In", |position, _| position > a);
        window.until("stopped back at In", |position, playing| !playing && position == a);
        window.player.send(Command::Seek(b));
        window.until("showed Out", |position, playing| !playing && position == b);
        window.press(egui::Key::Space);
        window.until("played the selection again", |position, playing| playing && position > a && position < b);
    }

    /// A Pause sent along with a seek (as the trim bar sends them at Out)
    /// pauses: only a run of seeks comes down to its last.
    #[test]
    fn a_pause_sent_with_a_seek_is_kept() {
        let dir = test_dir("pause-seek");
        let mp4 = dir.join("t.mp4");
        crate::recorder::write_test_video(&mp4, 60, 30, (96, 64));
        let source = mp4_source(&mp4);
        let time: Vec<f64> = (0..60).map(|i| source.time(i)).collect();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        let s = shared.clone();
        std::thread::spawn(move || playback(source, rx, s, egui::Context::default()));
        let wait_for = |what: &str, done: &dyn Fn(&Shared) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(&lock(&shared)) {
                assert!(Instant::now() < deadline, "never {what}");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        for (from, to) in [(40, 5), (50, 10), (45, 2)] {
            for command in [Command::Seek(time[from - 2]), Command::Seek(time[from]), Command::Play] {
                tx.send(command).unwrap();
            }
            wait_for("played", &|s| s.playing && s.position > time[from]);
            tx.send(Command::Pause).unwrap();
            tx.send(Command::Seek(time[to])).unwrap();
            wait_for("sought", &|s| s.position == time[to]);
            assert!(!lock(&shared).playing, "still playing after Pause and a seek to frame {to}");
        }
    }

    /// Commands queued for the playback thread keep their order, and each run
    /// of seeks comes down to its last.
    #[test]
    fn queued_commands_lose_only_superseded_seeks() {
        let (tx, rx) = mpsc::channel();
        for command in [Command::Seek(2.0), Command::Pause, Command::Seek(3.0), Command::Seek(4.0), Command::Play, Command::Pause, Command::Seek(5.0)] {
            tx.send(command).unwrap();
        }
        let expected = [Command::Seek(2.0), Command::Pause, Command::Seek(4.0), Command::Play, Command::Pause, Command::Seek(5.0)];
        assert_eq!(queued(Command::Seek(1.0), &rx), expected);
        assert_eq!(queued(Command::Play, &rx), [Command::Play]);
    }

    /// The second click of a double-click on Save lands on the Cancel that
    /// takes Save's place: it's ignored, so the save goes on. Cancel works
    /// once the save has been going a moment.
    #[test]
    fn a_double_click_on_save_doesnt_cancel_it() {
        let dir = test_dir("double-click");
        let mp4 = dir.join("t.mp4");
        crate::recorder::write_test_video(&mp4, 150, 30, (320, 192));
        let mut window = Window::open(&mp4);
        window.player.trim = Some((0.5, 4.5));
        window.pass(vec![]);
        // Near the right end of the bar along the bottom (24 points in, 64
        // high, 8 up): where Save sits, and then Cancel.
        let spot = Pos2::new(900.0 - 24.0 - 6.0, 600.0 - 8.0 - 28.0);
        window.click(spot);
        assert!(window.player.saving.is_some(), "the first click saves");
        window.pass(vec![]);
        window.click(spot);
        let saving = window.player.saving.as_mut().expect("still saving");
        assert!(!saving.cancel.load(Ordering::Relaxed), "the second click cancelled the save");
        saving.started -= Duration::from_secs(1);
        window.click(spot);
        assert!(window.player.saving.as_ref().expect("still saving").cancel.load(Ordering::Relaxed), "Cancel didn't cancel");
        let deadline = Instant::now() + Duration::from_secs(30);
        while window.player.saving.is_some() {
            assert!(matches!(window.pass(vec![]), PlayerAction::None), "a cancelled save is no news");
            assert!(Instant::now() < deadline, "the save never stopped");
            std::thread::sleep(Duration::from_millis(5));
        }
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["t.mp4"]);
    }

    /// The delays of a GIF's frames, in hundredths of a second.
    fn gif_delays(path: &Path) -> Vec<u16> {
        let mut options = gif::DecodeOptions::new();
        options.skip_frame_decoding(true);
        let mut reader = options.read_info(File::open(path).unwrap()).unwrap();
        std::iter::from_fn(|| reader.read_next_frame().unwrap().map(|f| f.delay)).collect()
    }

    /// A GIF copy lasts as long as its selection, as an MP4 copy does, at
    /// every preset: its last frame isn't held an extra half second. A GIF
    /// copied whole keeps its last frame's delay.
    #[test]
    fn gif_copies_last_as_long_as_their_selection() {
        let dir = test_dir("gif-length");
        let mp4 = dir.join("t.mp4");
        crate::recorder::write_test_video(&mp4, 90, 30, (96, 64));
        for q in GifQuality::ALL {
            let gif = dir.join(format!("{q:?}.gif"));
            export_trimmed(&mp4, &gif, 1.0, 2.0, q).unwrap();
            assert_eq!(gif_delays(&gif).iter().map(|&d| u32::from(d)).sum::<u32>(), 100, "{q:?}");
        }
        // A GIF that holds its last frame for 3 s.
        let frames: Vec<_> = (0..10u16)
            .map(|i| {
                let mut frame = gif_frame_at((0, 0, 16, 16), move |x, y| ((x + y + i) % 4) as u8);
                frame.delay = if i == 9 { 300 } else { 10 };
                frame
            })
            .collect();
        let held = dir.join("held.gif");
        write_gif(&held, (16, 16), &palette(4, 0), &frames);
        let whole = dir.join("whole.gif");
        export_trimmed(&held, &whole, 0.0, 3.9, GifQuality::default()).unwrap();
        assert_eq!(gif_delays(&whole), [10, 10, 10, 10, 10, 10, 10, 10, 10, 300]);
        let part = dir.join("part.gif");
        export_trimmed(&held, &part, 0.2, 0.5, GifQuality::default()).unwrap();
        assert_eq!(gif_delays(&part), [10, 10, 10]);
    }

    // ---------- benchmarks ----------

    /// Resident memory of this process, in MB.
    fn rss_mb() -> f64 {
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
    }

    fn min_median(mut times: Vec<f64>) -> (f64, f64) {
        times.sort_by(f64::total_cmp);
        (times[0], times[times.len() / 2])
    }

    /// A screen-recording-like GIF at `q`'s size and rate: the editor
    /// screenshot, a cursor-sized block moving across it, and a scroll every
    /// 10 s; or, `busy`, scrolling all the time, so every frame changes almost
    /// everywhere (the worst case for keeping frames compressed).
    fn screen_gif(path: &Path, q: GifQuality, seconds: u32, busy: bool) {
        let ui = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/images/editor.png")).unwrap().to_rgba8();
        let w = q.max_width();
        let h = w * 10 / 16;
        let page = image::imageops::resize(&ui, w, ui.height() * w / ui.width(), image::imageops::FilterType::Triangle);
        // Three pages, one under the other, to scroll through.
        let mut base = RgbaImage::new(w, page.height() * 3);
        for k in 0..3 {
            image::imageops::replace(&mut base, &page, 0, i64::from(k * page.height()));
        }
        let (tx, rx) = mpsc::channel();
        let n = seconds * q.fps();
        std::thread::spawn(move || {
            for i in 0..n {
                let scroll = if busy { i * 3 } else { (i / (10 * q.fps())) * w / 24 };
                let mut img = image::imageops::crop_imm(&base, 0, scroll % (base.height() - h), w, h).to_image();
                let (x, y) = (w / 8 + (i * 7) % (w * 3 / 4), h / 5 + (i * 3) % (h / 2));
                for dy in 0..w / 60 {
                    for dx in 0..w / 60 {
                        img.put_pixel(x + dx, y + dy, image::Rgba([255, 214, 0, 255]));
                    }
                }
                let at = Duration::from_secs_f64(i as f64 / q.fps() as f64);
                if tx.send(crate::recorder::TimedFrame { image: img, at }).is_err() {
                    break;
                }
            }
        });
        crate::recorder::encode_gif(rx, &path.to_path_buf(), &AtomicBool::new(false), q.colors(), 0).unwrap();
    }

    /// Opening a 30 s GIF: decoding every frame up front (before) against
    /// reading the first and the rest in the background (after).
    /// `cargo test --release --bin rega gif_open_benchmark -- --ignored --nocapture --test-threads 1`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn gif_open_benchmark() {
        use image::AnimationDecoder;
        let dir = std::env::temp_dir().join("rega-gif-bench");
        std::fs::create_dir_all(&dir).unwrap();
        let cases = [(GifQuality::Balanced, false), (GifQuality::High, false), (GifQuality::Balanced, true), (GifQuality::High, true)];
        std::thread::scope(|scope| {
            for (q, busy) in cases {
                let path = dir.join(format!("{q:?}-{busy}.gif"));
                if !path.exists() {
                    scope.spawn(move || screen_gif(&path, q, 30, busy));
                }
            }
        });
        for (q, busy) in cases {
            let path = dir.join(format!("{q:?}-{busy}.gif"));
            let size_mb = std::fs::metadata(&path).unwrap().len() as f64 / 1e6;
            let (mut before_t, mut before_mb, mut open_t, mut read_t, mut after_mb, mut kept_mb) = (vec![], vec![], vec![], vec![], vec![], vec![]);
            // The new way first: memory the old way frees stays with the
            // process, and would hide what the new way takes.
            for _ in 0..5 {
                let rss = rss_mb();
                let start = Instant::now();
                let gif = open_gif(&path, Scan::Background).unwrap();
                open_t.push(start.elapsed().as_secs_f64() * 1e3);
                wait_until_read(&gif);
                read_t.push(start.elapsed().as_secs_f64() * 1e3);
                after_mb.push(rss_mb() - rss);
                let table = lock(&gif.frames);
                let lzw: usize = table.frames.iter().map(|f| f.frame.buffer.len()).sum();
                let snapshots: usize = table.snapshots.iter().map(|(_, s)| s.len()).sum();
                kept_mb.push((lzw + snapshots) as f64 / 1e6);
                drop(table);
                drop(gif);
            }
            for _ in 0..5 {
                let rss = rss_mb();
                let start = Instant::now();
                let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(File::open(&path).unwrap())).unwrap();
                let frames: Vec<_> = decoder.into_frames().map(|f| f.unwrap().into_buffer()).collect();
                before_t.push(start.elapsed().as_secs_f64() * 1e3);
                before_mb.push(rss_mb() - rss);
                drop(frames);
            }
            let source = Source::Gif(open_gif(&path, Scan::Background).unwrap());
            if let Source::Gif(gif) = &source {
                wait_until_read(gif);
            }
            let n = source.len();
            let mut dec = Decoder::new();
            let mut step = vec![];
            for i in 0..n {
                let start = Instant::now();
                dec.frame(&source, i).unwrap();
                step.push(start.elapsed().as_secs_f64() * 1e3);
            }
            let mut seek = vec![];
            for k in 0..60 {
                let i = (k * 7919) % n;
                let start = Instant::now();
                dec.frame(&source, i).unwrap();
                seek.push(start.elapsed().as_secs_f64() * 1e3);
            }
            let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
            let first_rss = after_mb[0];
            let (b, bm, o, r, k) = (min_median(before_t), min_median(before_mb), min_median(open_t), min_median(read_t), min_median(kept_mb));
            let snapshots = match &source {
                Source::Gif(gif) => lock(&gif.frames).snapshots.len(),
                Source::Mp4(_) => 0,
            };
            println!("{q:?}{}: {n} frames, {size_mb:.1} MB file, {snapshots} snapshots", if busy { ", scrolling all the time" } else { "" });
            println!("  before (decode all on open): {:.0} / {:.0} ms (min / median), +{:.0} / {:.0} MB RSS", b.0, b.1, bm.0, bm.1);
            println!("  after: open {:.2} / {:.2} ms, fully read {:.0} / {:.0} ms, +{first_rss:.0} MB RSS the first time, {:.1} MB held", o.0, o.1, r.0, r.1, k.1);
            let (s, sk) = (min_median(step.clone()), min_median(seek.clone()));
            println!("  playing: {:.2} ms median, {:.2} ms max a frame; seeking: {:.2} ms median, {:.2} ms max", s.1, max(&step), sk.1, max(&seek));
        }
    }

    /// An MP4 of `n` changing frames, made a few at a time (the test videos
    /// queue them all up front, too much memory for a long one).
    fn write_long_video(path: &Path, n: u32, fps: u32, (w, h): (u32, u32)) {
        let (tx, rx) = mpsc::sync_channel(4);
        std::thread::spawn(move || {
            for i in 0..n {
                let image = RgbaImage::from_fn(w, h, |x, y| {
                    let inside = (x + i * 7) % w < 120 && (y + i * 3) % h < 90;
                    if inside { image::Rgba([255, 255, 255, 255]) } else { image::Rgba([(x + i) as u8, (y + 2 * i) as u8, (x ^ y) as u8, 255]) }
                });
                let at = Duration::from_secs_f64((i as f64 + 0.5) / fps as f64);
                if tx.send(crate::recorder::TimedFrame { image, at }).is_err() {
                    break;
                }
            }
        });
        crate::recorder::encode_mp4(rx, fps, path, &AtomicBool::new(false), false).unwrap();
    }

    /// Opening a long recording, and its first frame: reading every sample
    /// (before) against indexing (after).
    /// `cargo test --release --bin rega mp4_open_benchmark -- --ignored --nocapture --test-threads 1`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn mp4_open_benchmark() {
        let dir = std::env::temp_dir().join("rega-mp4-bench");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("long.mp4");
        if !path.exists() {
            // 4 minutes at 30 fps, 1280×720, every frame changing.
            write_long_video(&path, 7200, 30, (1280, 720));
        }
        let size_mb = std::fs::metadata(&path).unwrap().len() as f64 / 1e6;
        // What `open_mp4` did before: every sample read into memory.
        let old_open = |path: &Path| {
            let file = File::open(path).unwrap();
            let len = file.metadata().unwrap().len();
            let mut reader = mp4::Mp4Reader::read_header(BufReader::new(file), len).unwrap();
            let id = *reader.tracks().keys().next().unwrap();
            (1..=reader.sample_count(id).unwrap()).map(|n| reader.read_sample(id, n).unwrap().unwrap().bytes.to_vec()).collect::<Vec<_>>()
        };
        // What `first_frame` did before: all that, then decode the first.
        let old_first_frame = |path: &Path| {
            let samples = old_open(path);
            let video = open_mp4(path).unwrap();
            let mut dec = crate::h264::Decoder::new(&video.sps, &video.pps, video.size.0, video.size.1).unwrap();
            dec.decode(&samples[0], true, true).unwrap().unwrap()
        };
        let (mut before, mut before_mb, mut before_thumb, mut after, mut after_mb, mut thumb) = (vec![], vec![], vec![], vec![], vec![], vec![]);
        // The new way first: memory the old way frees stays with the process.
        for _ in 0..5 {
            let rss = rss_mb();
            let start = Instant::now();
            let video = open_mp4(&path).unwrap();
            after.push(start.elapsed().as_secs_f64() * 1e3);
            after_mb.push(rss_mb() - rss);
            drop(video);
            let start = Instant::now();
            first_frame(&path).unwrap();
            thumb.push(start.elapsed().as_secs_f64() * 1e3);
        }
        for _ in 0..5 {
            let rss = rss_mb();
            let start = Instant::now();
            let samples = old_open(&path);
            before.push(start.elapsed().as_secs_f64() * 1e3);
            before_mb.push(rss_mb() - rss);
            drop(samples);
            let start = Instant::now();
            old_first_frame(&path);
            before_thumb.push(start.elapsed().as_secs_f64() * 1e3);
        }
        // Memory from the first run of each: later ones reuse what it freed.
        let (b, bt, a, t) = (min_median(before), min_median(before_thumb), min_median(after), min_median(thumb));
        println!("{size_mb:.0} MB, 7200 frames");
        println!("  before: open {:.0} / {:.0} ms (min / median), +{:.0} MB RSS; first_frame {:.0} / {:.0} ms", b.0, b.1, before_mb[0], bt.0, bt.1);
        println!("  after: open {:.2} / {:.2} ms, +{:.1} MB RSS; first_frame {:.1} / {:.1} ms", a.0, a.1, after_mb[0], t.0, t.1);
    }

    /// Opening an MP4 crafted to freeze the window: a million empty boxes in
    /// its index (8 MB). The walk `check_boxes` makes, then the `mp4`
    /// crate's, through a `BufReader`, which drops its buffer at every seek
    /// (before), against through a `Tracked` reader (after); and all of
    /// `open_mp4`.
    /// `cargo test --release --bin rega box_walk_benchmark -- --ignored --nocapture --test-threads 1`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn box_walk_benchmark() {
        fn walk<R: Read + Seek>(mut reader: R, len: u64) -> (f64, f64) {
            let start = Instant::now();
            check_boxes(&mut reader, len).unwrap();
            let checked = start.elapsed().as_secs_f64() * 1e3;
            reader.rewind().unwrap();
            let start = Instant::now();
            mp4::Mp4Reader::read_header(&mut reader, len).unwrap();
            (checked, start.elapsed().as_secs_f64() * 1e3)
        }
        let dir = test_dir("box-bench");
        let (_, data) = recording(&dir);
        let free: Vec<u8> = (0..1_000_000).flat_map(|_| mp4_box(b"free", &[])).collect();
        let path = dir.join("boxes.mp4");
        std::fs::write(&path, with_in_moov(data, &mp4_box(b"udta", &free))).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let (mut before, mut after, mut open) = ((vec![], vec![]), (vec![], vec![]), vec![]);
        for _ in 0..3 {
            let (checked, read) = walk(BufReader::new(File::open(&path).unwrap()), len);
            before.0.push(checked);
            before.1.push(read);
            let (checked, read) = walk(Tracked::new(File::open(&path).unwrap()), len);
            after.0.push(checked);
            after.1.push(read);
            let start = Instant::now();
            assert_eq!(open_mp4(&path).unwrap().samples.len(), 30);
            open.push(start.elapsed().as_secs_f64() * 1e3);
        }
        std::fs::remove_file(&path).unwrap();
        let ms = |times: Vec<f64>| {
            let (min, median) = min_median(times);
            format!("{min:.0} / {median:.0} ms")
        };
        println!("{:.0} MB, a million empty boxes (min / median):", len as f64 / 1e6);
        println!("  before: check_boxes {}, mp4 crate {}", ms(before.0), ms(before.1));
        println!("  after: check_boxes {}, mp4 crate {}", ms(after.0), ms(after.1));
        println!("  open_mp4 {}", ms(open));
    }

    /// Seeking and playing across keyframes: a new decoder session each time
    /// (before) against one kept going (after).
    /// `cargo test --release --bin rega seek_benchmark -- --ignored --nocapture --test-threads 1`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn seek_benchmark() {
        let dir = std::env::temp_dir().join("rega-seek-bench");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seek.mp4");
        if !path.exists() {
            write_long_video(&path, 600, 30, (1920, 1080));
        }
        let source = mp4_source(&path);
        let keyframes: Vec<usize> = (0..600).filter(|&i| video(&source).samples[i].sync).collect();
        let (mut fresh, mut reused) = (vec![], vec![]);
        let mut dec = Decoder::new();
        dec.frame(&source, 0).unwrap();
        for &k in keyframes.iter().chain(keyframes.iter().rev()) {
            let start = Instant::now();
            Decoder::new().frame(&source, k).unwrap();
            fresh.push(start.elapsed().as_secs_f64() * 1e3);
            let start = Instant::now();
            dec.frame(&source, k).unwrap();
            reused.push(start.elapsed().as_secs_f64() * 1e3);
        }
        let (f, r) = (min_median(fresh), min_median(reused));
        println!("1080p seek to a keyframe: new session {:.2} / {:.2} ms (min / median), live session {:.2} / {:.2} ms", f.0, f.1, r.0, r.1);
    }

    /// Turning a decoded 1920×1246 frame into a texture image.
    /// `cargo test --release --bin rega upload_benchmark -- --ignored --nocapture --test-threads 1`
    #[test]
    #[ignore = "benchmark; prints timings"]
    fn upload_benchmark() {
        let img = RgbaImage::from_fn(1920, 1246, |x, y| image::Rgba([x as u8, y as u8, (x ^ y) as u8, 255]));
        let size = [1920, 1246];
        let time = |f: &dyn Fn() -> egui::ColorImage| {
            min_median(
                (0..40)
                    .map(|_| {
                        let start = Instant::now();
                        std::hint::black_box(f());
                        start.elapsed().as_secs_f64() * 1e3
                    })
                    .collect(),
            )
        };
        let before = time(&|| egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()));
        let premultiplied = time(&|| egui::ColorImage::from_rgba_premultiplied(size, img.as_raw()));
        let after = time(&|| color_image(&img));
        println!(
            "1920×1246 frame to ColorImage: unmultiplied {:.2} / {:.2} ms (min / median), from_rgba_premultiplied {:.2} / {:.2} ms, color_image {:.2} / {:.2} ms",
            before.0, before.1, premultiplied.0, premultiplied.1, after.0, after.1
        );
    }
}






