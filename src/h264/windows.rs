//! Media Foundation backend: the H.264 encoder and decoder that ship with
//! Windows, both synchronous transforms (MFTs) driven on the calling thread.
//! The encoder is Microsoft's software one: GPU encoders are asynchronous MFTs,
//! with an event-driven model of their own.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, ensure};
use image::RgbaImage;
use windows::Win32::Foundation::{E_OUTOFMEMORY, E_POINTER, ERROR_MOD_NOT_FOUND, HMODULE, REGDB_E_CLASSNOTREG, RPC_E_CHANGED_MODE};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CLSID_MSH264EncoderMFT, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncH264CABACEnable,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFMediaBuffer, IMFMediaType, IMFSample,
    IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_VERSION, MFMediaType_Video, MFNominalRange_16_235,
    MFSTARTUP_LITE, MFSampleExtension_CleanPoint, MFSampleExtension_Discontinuity, MFT_INPUT_STREAM_INFO, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoArea, MFVideoInterlace_MixedInterlaceOrProgressive, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    eAVEncCommonRateControlMode_UnconstrainedVBR, eAVEncH264VProfile_High, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, HRESULT, Interface, PCSTR, s, w};

use super::nv12::{nv12_to_rgba, rgba_to_nv12};
use super::{KEYFRAME_SECONDS, Replay, annexb_nals, avcc_to_annexb, nal_type, push_nal, starts_picture};

/// What to tell the user when Windows has no H.264 codec: N and KN editions
/// ship without one.
const NO_CODEC: &str = "Video needs the Windows Media Feature Pack (Settings › Apps › Optional features › Add a feature › Media Feature Pack)";

/// Whether Windows has Media Foundation: N and KN editions have no mfplat.dll
/// until the Media Feature Pack is installed. Only loads the DLL; COM and the
/// codecs wait for the recording.
pub fn available() -> Result<()> {
    Mf::get().map(drop)
}

/// Step between the decoder's input timestamps, in 100 ns units. Any increasing
/// times do: they only pair each picture with the sample it came from.
const TICK: i64 = 333_333;

pub struct Encoder {
    transform: Transform,
    width: usize,
    height: usize,
    fps: u64,
    /// Frames given to the encoder so far.
    frames: u64,
    /// Pictures it has returned.
    pictures: u64,
    /// The latest SPS and PPS, for keyframes that come without them.
    sets: Vec<Vec<u8>>,
}

impl Encoder {
    /// `_realtime` changes nothing here: the encoder runs in low-latency mode,
    /// which keeps up live and loses nothing in exports either.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, _realtime: bool) -> Result<Self> {
        let raw = width as usize * height as usize * 3 / 2;
        let transform = Transform::new(&CLSID_MSH264EncoderMFT, "H.264 encoder", MFVideoFormat_H264, raw)?;
        configure(&transform.mft, fps, bitrate)?;
        let mf = transform.runtime.mf;

        // The output type first: the encoder takes no input type until it has one.
        let output = video_type(mf, &MFVideoFormat_H264, width, height, fps).context("describing the video")?;
        // SAFETY: plain COM call on our own media type.
        unsafe { output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate) }.context("describing the video")?;
        // High profile compresses best; Windows 7's encoder only had Main.
        let mut set = Ok(());
        for profile in [eAVEncH264VProfile_High, eAVEncH264VProfile_Main] {
            // SAFETY: plain COM calls.
            set = unsafe { output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32).and_then(|()| transform.mft.SetOutputType(0, &output, 0)) };
            if set.is_ok() {
                break;
            }
        }
        set.with_context(|| format!("the H.264 encoder can't make {width}×{height} video at {fps} fps"))?;

        let input = video_type(mf, &MFVideoFormat_NV12, width, height, fps).context("describing the video")?;
        // SAFETY: plain COM calls. The rows are packed tightly, as `encode` writes them.
        unsafe { input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width).and_then(|()| transform.mft.SetInputType(0, &input, 0)) }
            .with_context(|| format!("the H.264 encoder won't take {width}×{height} NV12 frames"))?;
        transform.start()?;
        Ok(Self { transform, width: width as usize, height: height as usize, fps: fps.into(), frames: 0, pictures: 0, sets: Vec::new() })
    }

    pub fn encode(&mut self, rgba: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let (w, h) = (self.width, self.height);
        let sample = self.transform.input_sample(w * h * 3 / 2, |nv12| {
            let (y, uv) = nv12.split_at_mut(w * h);
            rgba_to_nv12(rgba, w, h, y, w, uv, w);
        })?;
        // From the frame number, so they don't drift; 100 ns units.
        let time = |frame: u64| (frame * 10_000_000 / self.fps) as i64;
        let (start, end) = (time(self.frames), time(self.frames + 1));
        // SAFETY: plain COM calls on our own sample.
        unsafe { sample.SetSampleTime(start).and_then(|()| sample.SetSampleDuration(end - start)) }.context("timing a video frame")?;
        let (pictures, sets) = (&mut self.pictures, &mut self.sets);
        self.transform.process(&sample, &mut |picture: &IMFSample, mft: &IMFTransform| {
            *pictures += append(picture, mft, sets, out)?;
            Ok(())
        })?;
        self.frames += 1;
        Ok(())
    }

    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        // SAFETY: plain COM call.
        unsafe { self.transform.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0) }.context("ending the H.264 stream")?;
        let (pictures, sets) = (&mut self.pictures, &mut self.sets);
        self.transform.drain(&mut |picture: &IMFSample, mft: &IMFTransform| {
            *pictures += append(picture, mft, sets, out)?;
            Ok(())
        })?;
        // Only logged: failing here would lose the whole recording, while a
        // missing picture only shifts the ones after it by a frame.
        if self.pictures != self.frames {
            log::warn!("the H.264 encoder returned {} pictures for {} frames", self.pictures, self.frames);
        }
        Ok(())
    }
}

/// Codec settings, made before the media types: the encoder reads some of them
/// (the B-frame count, the GOP, the entropy coding) only when its output type
/// is set.
fn configure(mft: &IMFTransform, fps: u32, bitrate: u32) -> Result<()> {
    let codec: ICodecAPI = mft.cast().context("the H.264 encoder has no codec settings")?;
    // SAFETY: CODECAPI properties, each with the VARIANT type its docs give.
    let set = |key: &GUID, value: VARIANT| unsafe { codec.SetValue(key, &value) };
    // B-frames would reorder pictures, which the MP4 writer can't time; the GOP
    // length is what keeps seeking in the player fast.
    set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32)).context("the H.264 encoder won't turn B-frames off")?;
    set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * KEYFRAME_SECONDS)).context("the H.264 encoder won't take a keyframe interval")?;
    // Worth having, not worth failing over. CABAC, which this encoder leaves off
    // unless asked, codes Main and High profile video about 10% more tightly
    // than CAVLC: better pictures for the same bit rate. The rest are its
    // defaults anyway: a mean bit rate (MF_MT_AVG_BITRATE asks for it too), and
    // low latency, which promises one picture out per frame in, never reordered.
    let rate_control = VARIANT::from(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32);
    for (key, value, what) in [
        (CODECAPI_AVEncH264CABACEnable, VARIANT::from(true), "CABAC"),
        (CODECAPI_AVEncCommonRateControlMode, rate_control, "rate control"),
        (CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate), "bit rate"),
        (CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low-latency mode"),
    ] {
        if let Err(e) = set(&key, value) {
            log::warn!("the H.264 encoder ignored its {what} setting: {e}");
        }
    }
    Ok(())
}

/// Append an encoded picture (MFVideoFormat_H264: Annex B, one picture) to
/// `out`, SPS and PPS first if it's a keyframe. `sets` keeps the latest SPS and
/// PPS. Returns how many pictures it held (one, normally).
fn append(sample: &IMFSample, mft: &IMFTransform, sets: &mut Vec<Vec<u8>>, out: &mut Vec<u8>) -> Result<u64> {
    // SAFETY: plain COM calls on the encoder's sample.
    let (buffer, clean_point) = unsafe { (sample.ConvertToContiguousBuffer(), sample.GetUINT32(&MFSampleExtension_CleanPoint)) };
    let (buffer, clean_point) = (buffer.context("reading an encoded picture")?, clean_point.is_ok_and(|v| v != 0));
    let locked = Locked::new(&buffer)?;
    let mut nals = annexb_nals(locked.bytes());
    let is_set = |nal: &[u8]| matches!(nal_type(nal), 7 | 8);
    let has = |nals: &[&[u8]], t: u8| nals.iter().any(|nal| nal_type(nal) == t);
    if has(&nals, 7) && has(&nals, 8) {
        *sets = nals.iter().filter(|nal| is_set(nal)).map(|nal| nal.to_vec()).collect();
    } else if clean_point || has(&nals, 5) {
        // SPS and PPS before every keyframe, as `Encoder::encode` promises, so
        // the stream can be decoded from any of them (the MP4 writer stores
        // them once and ignores the repeats). The encoder sends them in-band,
        // and puts them in its output type too.
        if sets.is_empty() {
            let header = sequence_header(mft).context("the H.264 encoder gave no SPS and PPS")?;
            *sets = annexb_nals(&header).iter().filter(|nal| is_set(nal)).map(|nal| nal.to_vec()).collect();
        }
        let types: Vec<u8> = sets.iter().map(|nal| nal_type(nal)).collect();
        ensure!(types.contains(&7) && types.contains(&8), "the H.264 encoder gave no SPS and PPS");
        for nal in sets.iter() {
            push_nal(out, nal);
        }
        nals.retain(|nal| !is_set(nal));
    }
    let mut pictures = 0;
    for nal in nals {
        pictures += u64::from(starts_picture(nal));
        push_nal(out, nal);
    }
    Ok(pictures)
}

/// The SPS and PPS the encoder adds to its output type, as Annex B.
fn sequence_header(mft: &IMFTransform) -> windows::core::Result<Vec<u8>> {
    // SAFETY: plain COM calls; the blob is copied into a buffer of its size.
    unsafe {
        let format = mft.GetOutputCurrentType(0)?;
        let len = format.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)?;
        let mut header = vec![0; len as usize];
        format.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)?;
        Ok(header)
    }
}

pub struct Decoder {
    transform: Transform,
    width: usize,
    height: usize,
    /// SPS and PPS as Annex B, put before the first sample and every keyframe.
    parameter_sets: Vec<u8>,
    /// The samples fed since the last keyframe, for replaying after a drain.
    gop: Replay,
    /// The next input sample's time.
    time: i64,
    /// Whether `feed` drained the decoder, which resets it, since the last input.
    drained: bool,
}

impl Decoder {
    pub fn new(sps: &[u8], pps: &[u8], width: u32, height: u32) -> Result<Self> {
        let transform = Transform::new(&CLSID_MSH264DecoderMFT, "H.264 decoder", MFVideoFormat_NV12, 0)?;
        // SAFETY: plain COM calls. The decoder takes low-latency mode, which gives
        // each picture as soon as its sample is in rather than a few samples later,
        // as a UINT32 attribute.
        unsafe { transform.mft.GetAttributes().and_then(|attributes| attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1)) }
            .context("the H.264 decoder has no low-latency mode")?;
        // SAFETY: plain COM call with our own media type.
        let input = h264_type(transform.runtime.mf, width, height).and_then(|input| unsafe { transform.mft.SetInputType(0, &input, 0) });
        input.with_context(|| format!("the H.264 decoder won't take {width}×{height} video"))?;
        // A placeholder until the first picture brings the real format.
        transform.select_output()?;
        transform.start()?;
        let mut parameter_sets = Vec::new();
        for set in [sps, pps] {
            parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
            parameter_sets.extend_from_slice(set);
        }
        Ok(Self { transform, width: width as usize, height: height as usize, parameter_sets, gop: Replay::default(), time: 0, drained: false })
    }

    pub fn decode(&mut self, sample: &[u8], keyframe: bool, want: bool) -> Result<Option<RgbaImage>> {
        if keyframe {
            self.gop.restart();
        } else if self.drained {
            // `feed` drained the decoder, which resets it: bring it back to this
            // point of the GOP first. One too long to have kept is an error, on
            // which the caller starts again at its keyframe.
            ensure!(!self.gop.lost, "the H.264 decoder was reset in a GOP too long to replay");
            let gop = std::mem::take(&mut self.gop);
            for earlier in &gop.samples {
                self.feed(earlier, false)?;
            }
            self.gop = gop;
        }
        let mut data = Vec::with_capacity(self.parameter_sets.len() + sample.len() + 16);
        if self.gop.is_empty() {
            data.extend_from_slice(&self.parameter_sets);
        }
        avcc_to_annexb(sample, &mut data);
        if data.is_empty() {
            // The sample holds no NAL units: nothing to decode.
            return Ok(None);
        }
        let picture = self.feed(&data, want)?;
        self.gop.push(data);
        Ok(picture)
    }

    /// Decode one sample (Annex B); returns its picture if `want`.
    fn feed(&mut self, data: &[u8], want: bool) -> Result<Option<RgbaImage>> {
        let time = self.time;
        self.time += TICK;
        let sample = self.transform.input_sample(data.len(), |memory| memory.copy_from_slice(data))?;
        // The first sample after a drain must say so.
        let discontinuity = std::mem::take(&mut self.drained);
        // SAFETY: plain COM calls on our own sample.
        unsafe {
            sample
                .SetSampleTime(time)
                .and_then(|()| sample.SetSampleDuration(TICK))
                .and_then(|()| if discontinuity { sample.SetUINT32(&MFSampleExtension_Discontinuity, 1) } else { Ok(()) })
        }
        .context("timing a video sample")?;

        let (width, height) = (self.width, self.height);
        let mut picture = None;
        self.transform.process(&sample, &mut |out: &IMFSample, mft: &IMFTransform| {
            if want && picture.is_none() {
                picture = picture_at(out, mft, time, width, height)?;
            }
            Ok(())
        })?;
        if want && picture.is_none() {
            // Low-latency mode should rule this out, but if the decoder still holds
            // the picture, a drain gets it out, at the cost of a reset.
            self.transform.drain(&mut |out: &IMFSample, mft: &IMFTransform| {
                if picture.is_none() {
                    picture = picture_at(out, mft, time, width, height)?;
                }
                Ok(())
            })?;
            self.drained = true;
        }
        Ok(picture)
    }
}

/// The decoder's input type: H.264, at the size the MP4 gives.
fn h264_type(mf: &Mf, width: u32, height: u32) -> windows::core::Result<IMFMediaType> {
    let format = mf.media_type()?;
    // SAFETY: plain COM calls on our own media type.
    unsafe {
        format.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        format.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        format.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        // What the decoder's docs advise: H.264 can switch per picture.
        format.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_MixedInterlaceOrProgressive.0 as u32)?;
    }
    Ok(format)
}

/// `out`'s picture, if it's the one made from the sample fed at `time`: the
/// decoder gives each picture its sample's time.
fn picture_at(out: &IMFSample, mft: &IMFTransform, time: i64, width: usize, height: usize) -> Result<Option<RgbaImage>> {
    // SAFETY: plain COM call.
    if unsafe { out.GetSampleTime() }.ok() != Some(time) {
        return Ok(None);
    }
    to_rgba(out, mft, width, height).map(Some)
}

/// A decoded NV12 picture as RGBA: the stream's display area (what its SPS
/// crops to, as the decoder reports it), at most `width × height`.
fn to_rgba(sample: &IMFSample, mft: &IMFTransform, width: usize, height: usize) -> Result<RgbaImage> {
    // SAFETY: plain COM calls on the decoder and its output type.
    let (format, size, stride) = unsafe {
        let format = mft.GetOutputCurrentType(0).context("reading the H.264 decoder's output format")?;
        let size = format.GetUINT64(&MF_MT_FRAME_SIZE).context("the H.264 decoder's output has no size")?;
        let stride = format.GetUINT32(&MF_MT_DEFAULT_STRIDE);
        (format, size, stride)
    };
    // The coded size, which may be padded (1920×1088 for 1080p); the planes follow it.
    let (w, h) = ((size >> 32) as usize, size as u32 as usize);
    // A contiguous buffer (what Lock returns) has rows this far apart.
    let stride = stride.map_or(w, |stride| (stride as i32).unsigned_abs() as usize);
    let (x0, y0, shown_w, shown_h) = aperture(&format).filter(|&(x, y, aw, ah)| aw > 0 && ah > 0 && x + aw <= w && y + ah <= h).unwrap_or((0, 0, w, h));
    let (width, height) = (width.min(shown_w), height.min(shown_h));
    ensure!(width > 0 && height > 0 && w <= stride, "a {w}×{h} decoded picture can't be shown at {width}×{height}");

    // SAFETY: plain COM call on the decoder's sample.
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }.context("reading a decoded picture")?;
    let locked = Locked::new(&buffer)?;
    let bytes = locked.bytes();
    // The Y plane, then the interleaved UV plane at half height.
    let need = stride.checked_mul(h + h.div_ceil(2));
    ensure!(need.is_some_and(|need| bytes.len() >= need), "a {w}×{h} decoded picture came in only {} bytes", bytes.len());
    let (y, uv) = bytes.split_at(stride * h);
    // H.264 crops 4:2:0 pictures in steps of 2, so the offsets stay on chroma samples.
    let (y, uv) = (&y[y0 * stride + x0..], &uv[y0 / 2 * stride + x0 / 2 * 2..]);
    Ok(nv12_to_rgba(y, stride, uv, stride, width, height))
}

/// The decoder's minimum display aperture (offset and size), if it gives one.
fn aperture(format: &IMFMediaType) -> Option<(usize, usize, usize, usize)> {
    let mut area = MFVideoArea::default();
    // SAFETY: the attribute is an MFVideoArea blob, copied into the bytes of
    // one (plain data, so any bytes are a valid value).
    unsafe {
        let bytes = std::slice::from_raw_parts_mut((&raw mut area).cast::<u8>(), size_of::<MFVideoArea>());
        format.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None)
    }
    .ok()?;
    let at = |v: i16| usize::try_from(v).ok();
    let side = |v: i32| usize::try_from(v).ok();
    Some((at(area.OffsetX.value)?, at(area.OffsetY.value)?, side(area.Area.cx)?, side(area.Area.cy)?))
}

/// A synchronous MFT with one input and one output stream, both ID 0: the
/// H.264 MFTs have fixed streams, numbered from 0.
struct Transform {
    mft: IMFTransform,
    /// For messages: "H.264 encoder" or "H.264 decoder".
    name: &'static str,
    /// The output subtype to take again when the MFT changes its output format.
    subtype: GUID,
    /// The least output buffer to allocate, whatever the MFT asks for.
    min_size: usize,
    /// Declared last, so dropped after the MFT.
    runtime: Runtime,
}

impl Transform {
    fn new(clsid: &GUID, name: &'static str, subtype: GUID, min_size: usize) -> Result<Self> {
        let runtime = Runtime::start()?;
        // SAFETY: COM is running on this thread.
        let mft: IMFTransform = unsafe { CoCreateInstance(clsid, None, CLSCTX_INPROC_SERVER) }.map_err(|e| {
            // Not registered, or its DLL is gone: an N or KN edition.
            if e.code() == REGDB_E_CLASSNOTREG || e.code() == HRESULT::from_win32(ERROR_MOD_NOT_FOUND.0) {
                anyhow!(NO_CODEC)
            } else {
                anyhow::Error::new(e).context(format!("Windows can't create its {name}"))
            }
        })?;
        Ok(Self { mft, name, subtype, min_size, runtime })
    }

    /// Take the first output type the MFT offers in `subtype`.
    fn select_output(&self) -> Result<()> {
        let mut index = 0;
        loop {
            // SAFETY: plain COM calls on the MFT and the types it offers.
            unsafe {
                let format = self.mft.GetOutputAvailableType(0, index).with_context(|| format!("the {} offers no output format to take", self.name))?;
                if format.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|subtype| subtype == self.subtype) {
                    return self.mft.SetOutputType(0, &format, 0).with_context(|| format!("the {} refused its own output format", self.name));
                }
            }
            index += 1;
        }
    }

    /// Tell the MFT streaming starts, once its media types are set.
    fn start(&self) -> Result<()> {
        // SAFETY: plain COM calls.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).and_then(|()| self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)) }
            .with_context(|| format!("the {} won't start", self.name))
    }

    /// Give the MFT `sample`, then hand `each` all the output it has ready.
    fn process(&mut self, sample: &IMFSample, each: &mut impl FnMut(&IMFSample, &IMFTransform) -> Result<()>) -> Result<()> {
        loop {
            // SAFETY: plain COM call; the MFT may keep a reference to the sample.
            match unsafe { self.mft.ProcessInput(0, sample, 0) } {
                Ok(()) => break,
                // Full: it has output to give first. (An MFT that takes no input
                // and gives no output is broken; don't spin on it.)
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    ensure!(self.pull(each)? > 0, "the {} takes no input and gives no output", self.name);
                }
                Err(e) => return Err(e).with_context(|| format!("the {} rejected a frame", self.name)),
            }
        }
        self.pull(each).map(drop)
    }

    /// Have the MFT finish everything it holds and hand that to `each`. A
    /// decoder treats what comes next as a new stream.
    fn drain(&mut self, each: &mut impl FnMut(&IMFSample, &IMFTransform) -> Result<()>) -> Result<()> {
        // SAFETY: plain COM call.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.with_context(|| format!("the {} won't drain", self.name))?;
        self.pull(each).map(drop)
    }

    /// Hand `each` every output sample the MFT has ready, until it needs more
    /// input. Returns how many there were.
    fn pull(&mut self, each: &mut impl FnMut(&IMFSample, &IMFTransform) -> Result<()>) -> Result<usize> {
        let (mut count, mut changes) = (0, 0);
        loop {
            let mut output =
                MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(self.output_sample()?), dwStatus: 0, pEvents: ManuallyDrop::new(None) };
            let mut status = 0;
            // SAFETY: one buffer for the one output stream.
            let result = unsafe { self.mft.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) };
            // Ours to release whatever happened, once each: the sample, and any
            // events (which the H.264 MFTs don't send).
            let sample = output.pSample.take();
            drop(output.pEvents.take());
            match result {
                Ok(()) => {
                    changes = 0;
                    let empty = MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32;
                    if let Some(sample) = sample.filter(|_| (output.dwStatus & empty) != empty) {
                        each(&sample, &self.mft)?;
                        count += 1;
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(count),
                // A new output format (the decoder's first picture brings the real
                // size): take it and ask again. Changing again and again is a bug.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE && changes < 2 => {
                    changes += 1;
                    self.select_output()?;
                }
                Err(e) => return Err(e).with_context(|| format!("the {} failed", self.name)),
            }
        }
    }

    /// A sample for ProcessOutput to fill, unless the MFT makes its own. A new
    /// one each time, so no attributes are left over from the last picture.
    fn output_sample(&self) -> Result<Option<IMFSample>> {
        // SAFETY: plain COM call.
        let info = unsafe { self.mft.GetOutputStreamInfo(0) }.with_context(|| format!("the {} has no output stream", self.name))?;
        let makes_own = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        if info.dwFlags & makes_own != 0 {
            return Ok(None);
        }
        let size = (info.cbSize as usize).max(self.min_size);
        ensure!(size > 0, "the {} wants no room for its output", self.name);
        let mf = self.runtime.mf;
        mf.buffer(size, info.cbAlignment).and_then(|buffer| mf.sample(&buffer)).map(Some).context("allocating a video frame")
    }

    /// A sample for ProcessInput: one `len`-byte buffer, filled by `fill`.
    fn input_sample(&self, len: usize, fill: impl FnOnce(&mut [u8])) -> Result<IMFSample> {
        let mut info = MFT_INPUT_STREAM_INFO::default();
        // SAFETY: plain COM call.
        unsafe { self.mft.GetInputStreamInfo(0, &mut info) }.with_context(|| format!("the {} has no input stream", self.name))?;
        let mf = self.runtime.mf;
        let buffer = mf.buffer(len, info.cbAlignment).context("allocating a video frame")?;
        {
            let mut locked = Locked::new(&buffer)?;
            let memory = locked.bytes_mut();
            ensure!(memory.len() >= len, "a {len}-byte media buffer came {} bytes long", memory.len());
            fill(&mut memory[..len]);
        }
        // SAFETY: plain COM call on our own buffer, which `Mf::buffer` checked `len` fits.
        unsafe { buffer.SetCurrentLength(len as u32) }.context("filling a video frame")?;
        mf.sample(&buffer).context("allocating a video frame")
    }
}

/// A media buffer's memory, locked until dropped.
struct Locked<'a> {
    buffer: &'a IMFMediaBuffer,
    data: *mut u8,
    max: usize,
    len: usize,
}

impl<'a> Locked<'a> {
    fn new(buffer: &'a IMFMediaBuffer) -> Result<Self> {
        let (mut data, mut max, mut len) = (ptr::null_mut(), 0, 0);
        // SAFETY: out-pointers for the memory and its sizes; unlocked on drop.
        unsafe { buffer.Lock(&mut data, Some(&mut max), Some(&mut len)) }.context("can't read a video frame")?;
        let locked = Self { buffer, data, max: max as usize, len: len.min(max) as usize };
        ensure!(!locked.data.is_null(), "a video frame has no memory");
        Ok(locked)
    }

    /// The bytes holding data.
    fn bytes(&self) -> &[u8] {
        // SAFETY: Lock gave `len` readable bytes at `data`, until Unlock.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }

    /// All of the buffer, to fill.
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: Lock gave `max` writable bytes at `data`, until Unlock.
        unsafe { std::slice::from_raw_parts_mut(self.data, self.max) }
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: locked in `new`.
        let _ = unsafe { self.buffer.Unlock() };
    }
}

/// A progressive, square-pixel video type of `subtype`, `width × height` at
/// `fps`, tagged BT.709 video range like `nv12`'s conversion.
fn video_type(mf: &Mf, subtype: &GUID, width: u32, height: u32, fps: u32) -> windows::core::Result<IMFMediaType> {
    let format = mf.media_type()?;
    // SAFETY: plain COM calls on our own media type.
    unsafe {
        format.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        format.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        format.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        format.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        format.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        format.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        format.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        format.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        format.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        format.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
    }
    Ok(format)
}

/// Two UINT32s in one UINT64 attribute (MFSetAttributeSize / MFSetAttributeRatio).
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// COM and Media Foundation on the current thread, stopped when dropped.
struct Runtime {
    mf: &'static Mf,
    _com: Com,
}

impl Runtime {
    fn start() -> Result<Self> {
        let mf = Mf::get()?;
        let com = Com::start()?;
        // SAFETY: plain call, balanced in `drop`.
        unsafe { (mf.startup)(MF_VERSION, MFSTARTUP_LITE) }.ok().context("Media Foundation won't start")?;
        Ok(Self { mf, _com: com })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: balances the MFStartup in `start`; COM stops after this.
        let _ = unsafe { (self.mf.shutdown)() };
    }
}

/// COM on the current thread, stopped when dropped if it was started here.
struct Com(bool);

impl Com {
    fn start() -> Result<Self> {
        // SAFETY: plain call, balanced in `drop` when it succeeds. A thread that's
        // already in a single-threaded apartment (RPC_E_CHANGED_MODE) stays in it:
        // the MFTs work there too, as they're only used on this thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ensure!(hr.is_ok() || hr == RPC_E_CHANGED_MODE, "COM won't start on this thread ({hr})");
        Ok(Self(hr.is_ok()))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances a successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// The mfplat.dll functions used here, looked up at run time: Windows N
/// editions have no mfplat.dll until the Media Feature Pack is installed, and
/// linked to it, rega.exe wouldn't start there at all.
struct Mf {
    startup: unsafe extern "system" fn(u32, u32) -> HRESULT,
    shutdown: unsafe extern "system" fn() -> HRESULT,
    create_media_type: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT,
    create_sample: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT,
    create_aligned_memory_buffer: unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT,
}

impl Mf {
    /// Loaded on first use, and never unloaded.
    fn get() -> Result<&'static Mf> {
        static MF: OnceLock<Option<Mf>> = OnceLock::new();
        MF.get_or_init(|| {
            // SAFETY: System32's mfplat.dll, whose exports have these signatures
            // (mfapi.h).
            unsafe {
                let dll = LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
                Some(Mf {
                    startup: export(dll, s!("MFStartup"))?,
                    shutdown: export(dll, s!("MFShutdown"))?,
                    create_media_type: export(dll, s!("MFCreateMediaType"))?,
                    create_sample: export(dll, s!("MFCreateSample"))?,
                    create_aligned_memory_buffer: export(dll, s!("MFCreateAlignedMemoryBuffer"))?,
                })
            }
        })
        .as_ref()
        .context(NO_CODEC)
    }

    fn media_type(&self) -> windows::core::Result<IMFMediaType> {
        let mut raw = ptr::null_mut();
        // SAFETY: on success, a new reference through `raw`.
        unsafe { (self.create_media_type)(&mut raw).ok().and_then(|()| owned(raw)) }
    }

    /// A `len`-byte memory buffer, aligned as an MFT asks (`alignment` bytes, 0
    /// for no need) and to at least 64 bytes.
    fn buffer(&self, len: usize, alignment: u32) -> windows::core::Result<IMFMediaBuffer> {
        let len = u32::try_from(len).map_err(|_| E_OUTOFMEMORY)?;
        // MF_64_BYTE_ALIGNMENT to MF_512_BYTE_ALIGNMENT: one less than the alignment.
        let mask = alignment.clamp(64, 512).next_power_of_two() - 1;
        let mut raw = ptr::null_mut();
        // SAFETY: as above.
        unsafe { (self.create_aligned_memory_buffer)(len, mask, &mut raw).ok().and_then(|()| owned(raw)) }
    }

    /// A sample holding `buffer`.
    fn sample(&self, buffer: &IMFMediaBuffer) -> windows::core::Result<IMFSample> {
        let mut raw = ptr::null_mut();
        // SAFETY: as above.
        let sample: IMFSample = unsafe { (self.create_sample)(&mut raw).ok().and_then(|()| owned(raw)) }?;
        // SAFETY: plain COM call.
        unsafe { sample.AddBuffer(buffer) }?;
        Ok(sample)
    }
}

/// `dll`'s export `name`, as the function pointer type `F`.
///
/// # Safety
///
/// `F` must be the export's type, and `dll` must stay loaded while it's used.
unsafe fn export<F>(dll: HMODULE, name: PCSTR) -> Option<F> {
    const { assert!(size_of::<F>() == size_of::<usize>()) };
    // SAFETY: see above.
    unsafe { GetProcAddress(dll, name).map(|f| std::mem::transmute_copy(&f)) }
}

/// Own the new reference a creation function returned.
///
/// # Safety
///
/// `raw` must be null or a new reference to a `T`.
unsafe fn owned<T: Interface>(raw: *mut c_void) -> windows::core::Result<T> {
    if raw.is_null() {
        return Err(E_POINTER.into());
    }
    // SAFETY: see above.
    Ok(unsafe { T::from_raw(raw) })
}
