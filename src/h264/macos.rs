//! VideoToolbox backend: the H.264 encoder and decoder that ship with macOS
//! (hardware where the Mac has one, Apple's software codec otherwise).

use std::ffi::{c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, anyhow, ensure};
use image::RgbaImage;
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFGetTypeID, CFNumber, CFRetained, CFString, CFType, Type};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMBlockBufferAssureMemoryNowFlag, kCMSampleAttachmentKey_NotSync, kCMTimeInvalid,
    kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetTypeID, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_H264_Main_AutoLevel, kVTPropertyNotSupportedErr, kVTPropertyReadOnlyErr,
};

use super::{KEYFRAME_SECONDS, avcc_nals, nal_type, push_nal};

type OSStatus = i32;

/// VideoToolbox ships with every macOS Rega runs on.
pub fn available() -> Result<()> {
    Ok(())
}

/// Frames the encoder may hold before `encode` makes it finish them. Fed faster
/// than real time (catching up after a capture stall, trimming), the hardware
/// encoder queues a dozen, and Apple's software encoder holds as many to look
/// ahead: each a full BGRA frame, 33 MB at 4K.
const MAX_PENDING: u64 = 4;

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    /// Filled by the output callback on a VideoToolbox thread; `Drop` tears the
    /// session down before this is freed.
    output: Arc<Output>,
    /// For pixel buffers made without the session's pool.
    attributes: CFRetained<CFDictionary<CFString, CFType>>,
    width: usize,
    height: usize,
    fps: i32,
    /// Frames handed to the session so far.
    frames: u64,
}

/// Pictures the session has finished, in decode order, which is input order:
/// frame reordering (B-frames) is off.
#[derive(Default)]
struct Output(Mutex<Pictures>);

#[derive(Default)]
struct Pictures {
    /// Annex B, not yet handed to the caller.
    stream: Vec<u8>,
    /// Frames the session is done with: encoded or dropped.
    done: u64,
    /// Frames a real-time session dropped to keep up.
    dropped: u64,
    /// The first failure. It sticks: the session is unusable after it.
    error: Option<String>,
}

impl Output {
    fn lock(&self) -> MutexGuard<'_, Pictures> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, realtime: bool) -> Result<Self> {
        let attributes = pixel_buffer_attributes(width, height);
        let output = Arc::<Output>::default();
        let mut raw = ptr::null_mut();
        // SAFETY: `encoded` matches the callback type, and its refcon (`output`)
        // stays alive until `Drop` has torn the session down. No encoder
        // specification: VideoToolbox picks hardware, else software.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                None,
                Some(attributes.as_opaque()),
                None,
                Some(encoded),
                Arc::as_ptr(&output).cast_mut().cast(),
                NonNull::from(&mut raw),
            )
        };
        // SAFETY: Create rule.
        let session = unsafe { created(status, raw) }
            .map_err(|status| anyhow!("VideoToolbox can't make a {width}×{height} H.264 encoder (OSStatus {status})"))?;
        let encoder = Self { session, output, attributes, width: width as usize, height: height as usize, fps: fps as i32, frames: 0 };
        encoder.configure(fps, bitrate, realtime)?;
        Ok(encoder)
    }

    fn configure(&self, fps: u32, bitrate: u32, realtime: bool) -> Result<()> {
        let session: &CFType = &self.session;
        let number = |n: u32| CFNumber::new_i64(n.into());
        // SAFETY: VideoToolbox's own keys, each with a value of the type it takes.
        unsafe {
            // Live recording must keep up: Apple's software encoder (Macs and VMs
            // without a hardware one) runs at background priority otherwise, and
            // on a busy machine falls ever further behind. A real-time encoder
            // may drop a frame instead: that frame is lost, and as the recorder
            // times pictures by their count, the ones after it play a frame
            // early. Exports the user waits on can take their time and lose
            // nothing.
            hint(session, kVTCompressionPropertyKey_RealTime, CFBoolean::new(realtime))?;
            // No B-frames: the MP4 writer stores no composition offsets.
            set(session, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false))?;
            set(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel)
                .or_else(|_| set(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel))?;
            set(session, kVTCompressionPropertyKey_AverageBitRate, &number(bitrate))?;
            set(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, &number(fps.saturating_mul(KEYFRAME_SECONDS)))?;
            set(session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &number(KEYFRAME_SECONDS))?;
            hint(session, kVTCompressionPropertyKey_ExpectedFrameRate, &number(fps))?;
            // BT.709, the matrix VideoToolbox then converts with, tagged so that
            // players (QuickTime included) convert back with it too.
            hint(session, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2)?;
            hint(session, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2)?;
            hint(session, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2)?;
            let status = self.session.prepare_to_encode_frames();
            ensure!(status == 0, "VideoToolbox can't start the H.264 encoder (OSStatus {status})");
        }
        Ok(())
    }

    pub fn encode(&mut self, rgba: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let submitted = self.submit(rgba);
        // Hand over what's finished even when this frame failed.
        self.take(out).and(submitted)
    }

    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let completed = self.complete(None);
        // As in `encode`: a failure doesn't cost the pictures finished before it.
        self.take(out).and(completed)?;
        let pictures = self.output.lock();
        // Frames never reported back were dropped too.
        let dropped = pictures.dropped + self.frames.saturating_sub(pictures.done);
        if dropped > 0 {
            log::warn!("VideoToolbox dropped {dropped} of {} frames to keep up", self.frames);
        }
        Ok(())
    }

    /// Give the session the next frame (`width × height` RGBA), then have it
    /// finish frames until no more than [`MAX_PENDING`] are in flight.
    fn submit(&mut self, rgba: &[u8]) -> Result<()> {
        let buffer = self.pixel_buffer()?;
        write_bgra(&buffer, rgba, self.width, self.height)?;
        let frame = self.frames;
        let mut flags = VTEncodeInfoFlags::empty();
        // SAFETY: a filled, unlocked pixel buffer of the session's size; the
        // refcon is the frame number, not a pointer.
        let status = unsafe {
            self.session.encode_frame(
                &buffer,
                time(frame as i64, self.fps),
                time(1, self.fps),
                None,
                ptr::without_provenance_mut(frame as usize),
                &mut flags,
            )
        };
        // A frame dropped here is accounted for by the callback, or as a gap
        // before the next frame it reports.
        ensure!(status == 0, "VideoToolbox failed to encode frame {frame} (OSStatus {status})");
        self.frames += 1;

        if self.frames.saturating_sub(self.output.lock().done) > MAX_PENDING {
            self.complete(Some(self.frames - 1 - MAX_PENDING))?;
        }
        Ok(())
    }

    /// Have the session finish the frames up to frame `until`, or all of them.
    /// Their pictures wait in `output` for [`Encoder::take`].
    fn complete(&self, until: Option<u64>) -> Result<()> {
        // SAFETY: a live session; an invalid time means all pending frames.
        let status = unsafe { self.session.complete_frames(until.map_or(kCMTimeInvalid, |frame| time(frame as i64, self.fps))) };
        ensure!(status == 0, "VideoToolbox failed to finish encoding (OSStatus {status})");
        Ok(())
    }

    /// Move the finished pictures to `out`, then report a failure the callback
    /// recorded: the pictures before it are still good video.
    fn take(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut pictures = self.output.lock();
        out.append(&mut pictures.stream);
        match &pictures.error {
            Some(e) => Err(anyhow!("{e}")),
            None => Ok(()),
        }
    }

    /// A pixel buffer from the session's pool (IOSurface-backed, so the
    /// hardware encoder reads it without a copy), or a new one should the
    /// session have no pool.
    fn pixel_buffer(&self) -> Result<CFRetained<CVPixelBuffer>> {
        let mut raw = ptr::null_mut();
        // SAFETY: a valid pool or attributes, and an out-pointer.
        let status = unsafe {
            match self.session.pixel_buffer_pool() {
                Some(pool) => CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)),
                None => CVPixelBufferCreate(
                    None,
                    self.width,
                    self.height,
                    kCVPixelFormatType_32BGRA,
                    Some(self.attributes.as_opaque()),
                    NonNull::from(&mut raw),
                ),
            }
        };
        // SAFETY: both follow the Create rule.
        unsafe { created(status, raw) }.map_err(|status| anyhow!("can't allocate a {}×{} video frame (CVReturn {status})", self.width, self.height))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: completing first leaves no frame in flight, and once the
        // session is invalidated no callback can reach `output`.
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
    }
}

/// The encoder's output callback, on a VideoToolbox thread. `output` is the
/// encoder's [`Output`]; `frame` is the frame number `encode` passed.
unsafe extern "C-unwind" fn encoded(output: *mut c_void, frame: *mut c_void, status: OSStatus, flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer) {
    // SAFETY: the `Output` the session was made with outlives it.
    let Some(output) = (unsafe { output.cast::<Output>().as_ref() }) else { return };
    let frame = frame.addr() as u64;
    // A panic must not unwind into VideoToolbox.
    let picture = catch_unwind(AssertUnwindSafe(|| {
        ensure!(status == 0, "VideoToolbox failed to encode frame {frame} (OSStatus {status})");
        if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return Ok(None);
        }
        // SAFETY: VideoToolbox passes a valid sample buffer, or null, for the
        // duration of the call.
        let sample = unsafe { sample.as_ref() }.with_context(|| format!("VideoToolbox returned no data for frame {frame}"))?;
        // SAFETY: as above.
        unsafe { annexb(sample) }.map(Some).with_context(|| format!("reading frame {frame} from VideoToolbox"))
    }))
    .unwrap_or_else(|_| Err(anyhow!("reading frame {frame} from VideoToolbox panicked")));
    output.lock().add(frame, picture);
}

impl Pictures {
    /// Record frame `frame`'s outcome: its picture, `None` if it was dropped.
    fn add(&mut self, frame: u64, picture: Result<Option<Vec<u8>>>) {
        if self.error.is_some() {
            return;
        }
        if frame < self.done {
            // Pictures out of order would play out of order.
            self.error = Some(format!("VideoToolbox returned frame {frame} after frame {}", self.done - 1));
            return;
        }
        // Frames skipped over were dropped without a callback of their own.
        self.dropped += frame - self.done;
        self.done = frame + 1;
        match picture {
            Ok(Some(bytes)) => self.stream.extend_from_slice(&bytes),
            Ok(None) => self.dropped += 1,
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }
}

/// One encoded picture as Annex B: SPS and PPS first if it's a keyframe, then
/// its slices.
///
/// # Safety
///
/// `sample` must be a valid sample buffer from an H.264 compression session.
unsafe fn annexb(sample: &CMSampleBuffer) -> Result<Vec<u8>> {
    // SAFETY: reads the sample's own format and data. Parameter set pointers
    // point into `format`, which is retained while they're read.
    unsafe {
        let format = sample.format_description().context("no format description")?;
        let (mut sets, mut len_size): (usize, c_int) = (0, 0);
        let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&format, 0, ptr::null_mut(), ptr::null_mut(), &mut sets, &mut len_size);
        ensure!(status == 0, "no H.264 parameter sets (OSStatus {status})");

        let mut out = Vec::new();
        if is_keyframe(sample) {
            for i in 0..sets {
                let (mut nal, mut size) = (ptr::null(), 0);
                let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&format, i, &mut nal, &mut size, ptr::null_mut(), ptr::null_mut());
                ensure!(status == 0 && !nal.is_null(), "can't read H.264 parameter set {i} (OSStatus {status})");
                push_nal(&mut out, std::slice::from_raw_parts(nal, size));
            }
        }

        // Copied, as the block buffer needn't be contiguous.
        let block = sample.data_buffer().context("no data")?;
        let mut data = vec![0u8; block.data_length()];
        if !data.is_empty() {
            let status = block.copy_data_bytes(0, data.len(), NonNull::from(data.as_mut_slice()).cast());
            ensure!(status == 0, "can't copy the picture's data (OSStatus {status})");
        }
        let mut slices = 0;
        for nal in avcc_nals(&data, len_size.max(0) as usize) {
            slices += usize::from(matches!(nal_type(nal), 1 | 5));
            push_nal(&mut out, nal);
        }
        ensure!(slices > 0, "the picture has no coded slices");
        Ok(out)
    }
}

/// Whether the sample is a sync sample: its attachments don't set NotSync.
///
/// # Safety
///
/// `sample` must be a valid sample buffer.
unsafe fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    // SAFETY: sample attachments are an array of CFString-keyed dictionaries.
    unsafe {
        let Some(attachments) = sample.sample_attachments_array(false) else { return true };
        let attachments = CFRetained::cast_unchecked::<CFArray<CFDictionary<CFString, CFType>>>(attachments);
        let not_sync = attachments.get(0).and_then(|first| first.get(kCMSampleAttachmentKey_NotSync));
        !not_sync.and_then(|v| v.downcast::<CFBoolean>().ok()).is_some_and(|v| v.as_bool())
    }
}

pub struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    width: usize,
    height: usize,
}

/// One `decode` call's request to the output callback, and its answer.
struct Frame {
    want: bool,
    width: usize,
    height: usize,
    result: Mutex<Option<Result<Option<RgbaImage>>>>,
}

impl Decoder {
    pub fn new(sps: &[u8], pps: &[u8], width: u32, height: u32) -> Result<Self> {
        let sets = [NonNull::from(sps).cast::<u8>(), NonNull::from(pps).cast::<u8>()];
        let sizes = [sps.len(), pps.len()];
        let mut raw = ptr::null();
        // SAFETY: two parameter sets with their sizes, and an out-pointer.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                sets.len(),
                NonNull::from(&sets).cast(),
                NonNull::from(&sizes).cast(),
                4,
                NonNull::from(&mut raw),
            )
        };
        // SAFETY: Create rule.
        let format = unsafe { created(status, raw.cast_mut()) }
            .map_err(|status| anyhow!("VideoToolbox can't read the video's H.264 parameter sets (OSStatus {status})"))?;

        // SAFETY: CoreVideo's own key.
        let key = unsafe { kCVPixelBufferPixelFormatTypeKey };
        let bgra: &CFType = &CFNumber::new_i64(kCVPixelFormatType_32BGRA.into());
        let attributes = CFDictionary::<CFString, CFType>::from_slices(&[key], &[bgra]);
        // No session refcon: each `decode` passes its own `Frame`.
        let callback = VTDecompressionOutputCallbackRecord { decompressionOutputCallback: Some(decoded), decompressionOutputRefCon: ptr::null_mut() };
        let mut raw = ptr::null_mut();
        // SAFETY: a valid format and attributes, a callback record VideoToolbox
        // copies, and an out-pointer.
        let status = unsafe {
            VTDecompressionSession::create(None, &format, None, Some(attributes.as_opaque()), &callback, NonNull::from(&mut raw))
        };
        // SAFETY: Create rule.
        let session = unsafe { created(status, raw) }
            .map_err(|status| anyhow!("VideoToolbox can't make an H.264 decoder for {width}×{height} video (OSStatus {status})"))?;
        Ok(Self { session, format, width: width as usize, height: height as usize })
    }

    pub fn decode(&mut self, sample: &[u8], _keyframe: bool, want: bool) -> Result<Option<RgbaImage>> {
        let sample = self.sample_buffer(sample)?;
        let frame = Frame { want, width: self.width, height: self.height, result: Mutex::new(None) };
        // A frame that's only a reference for later ones needn't be converted
        // to BGRA at all.
        let flags = if want { VTDecodeFrameFlags::empty() } else { VTDecodeFrameFlags::Frame_DoNotOutputFrame };
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: a sample buffer in the session's format, decoded synchronously;
        // waiting guarantees the callback is done with `frame` before it's dropped.
        let (status, waited) = unsafe {
            let status = self.session.decode_frame(&sample, flags, ptr::from_ref(&frame).cast_mut().cast(), &mut info);
            (status, self.session.wait_for_asynchronous_frames())
        };
        ensure!(status == 0, "VideoToolbox failed to decode a frame (OSStatus {status})");
        ensure!(waited == 0, "VideoToolbox failed to finish decoding a frame (OSStatus {waited})");
        match frame.result.into_inner().unwrap_or_else(PoisonError::into_inner) {
            Some(result) => result,
            None if want && info.contains(VTDecodeInfoFlags::FrameDropped) => Err(anyhow!("VideoToolbox dropped a frame")),
            None => Ok(None),
        }
    }

    /// The sample, copied into memory CoreMedia owns, as a sample buffer.
    fn sample_buffer(&self, sample: &[u8]) -> Result<CFRetained<CMSampleBuffer>> {
        let len = sample.len();
        let mut raw = ptr::null_mut();
        // SAFETY: CoreMedia allocates `len` bytes up front (the flag) and the
        // sample is copied in; out-pointers for Create-rule objects.
        unsafe {
            let status = CMBlockBuffer::create_with_memory_block(
                None,
                ptr::null_mut(),
                len,
                None,
                ptr::null(),
                0,
                len,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut raw),
            );
            let block = created(status, raw).map_err(|status| anyhow!("can't allocate a {len}-byte video sample (OSStatus {status})"))?;
            let status = CMBlockBuffer::replace_data_bytes(NonNull::from(sample).cast(), &block, 0, len);
            ensure!(status == 0, "can't copy a video sample (OSStatus {status})");
            let mut raw = ptr::null_mut();
            let status = CMSampleBuffer::create_ready(None, Some(&block), Some(&self.format), 1, 0, ptr::null(), 1, &len, NonNull::from(&mut raw));
            created(status, raw).map_err(|status| anyhow!("can't wrap a video sample (OSStatus {status})"))
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: a live session with no frame in flight (`decode` waits).
        unsafe { self.session.invalidate() };
    }
}

/// The decoder's output callback. `frame` is the [`Frame`] that `decode`
/// passed, and waits on.
unsafe extern "C-unwind" fn decoded(
    _: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _: CMTime,
    _: CMTime,
) {
    // SAFETY: see above.
    let Some(frame) = (unsafe { frame.cast::<Frame>().as_ref() }) else { return };
    // A panic must not unwind into VideoToolbox.
    let result = catch_unwind(AssertUnwindSafe(|| {
        ensure!(status == 0, "VideoToolbox failed to decode a frame (OSStatus {status})");
        // SAFETY: VideoToolbox passes a valid image buffer, or null, for the
        // duration of the call.
        match unsafe { image.as_ref() } {
            Some(image) if frame.want => to_rgba(image, frame.width, frame.height).map(Some),
            None if frame.want && flags.contains(VTDecodeInfoFlags::FrameDropped) => Err(anyhow!("VideoToolbox dropped a frame")),
            _ => Ok(None),
        }
    }))
    .unwrap_or_else(|_| Err(anyhow!("reading a decoded frame from VideoToolbox panicked")));
    *frame.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
}

/// A 32BGRA pixel buffer's memory, mapped for the CPU until dropped.
struct Locked<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
    base: *mut u8,
    stride: usize,
}

impl<'a> Locked<'a> {
    /// Lock `buffer`, checking it's 32BGRA and holds `width × height` pixels.
    fn new(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags, width: usize, height: usize) -> Result<Self> {
        ensure!(CFGetTypeID(Some(buffer)) == CVPixelBufferGetTypeID(), "VideoToolbox returned an image that isn't a pixel buffer");
        let format = CVPixelBufferGetPixelFormatType(buffer);
        ensure!(format == kCVPixelFormatType_32BGRA, "VideoToolbox returned pixel format {format:#010x}, not BGRA");
        let (w, h) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer));
        ensure!(w >= width && h >= height && height > 0, "a {w}×{h} video frame can't hold {width}×{height}");
        // SAFETY: a valid pixel buffer, unlocked with the same flags on drop.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        ensure!(status == kCVReturnSuccess, "can't map a video frame (CVReturn {status})");
        let locked = Self { buffer, flags, base: CVPixelBufferGetBaseAddress(buffer).cast(), stride: CVPixelBufferGetBytesPerRow(buffer) };
        ensure!(!locked.base.is_null() && locked.stride >= width * 4, "a video frame has no memory or short rows");
        Ok(locked)
    }

    /// Bytes from the first pixel to the last of the top-left `width × height`.
    fn span(&self, width: usize, height: usize) -> usize {
        self.stride * (height - 1) + width * 4
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: locked in `new` with these flags.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

/// Copy tightly packed RGBA into a 32BGRA pixel buffer.
fn write_bgra(buffer: &CVPixelBuffer, rgba: &[u8], width: usize, height: usize) -> Result<()> {
    let locked = Locked::new(buffer, CVPixelBufferLockFlags::empty(), width, height)?;
    // SAFETY: locked for writing, and `Locked::new` checked the size.
    let dst = unsafe { std::slice::from_raw_parts_mut(locked.base, locked.span(width, height)) };
    for (src, dst) in rgba.chunks_exact(width * 4).zip(dst.chunks_mut(locked.stride)) {
        for (s, d) in src.as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<4>().0) {
            *d = [s[2], s[1], s[0], s[3]];
        }
    }
    Ok(())
}

/// The top-left `width × height` of a 32BGRA pixel buffer, as opaque RGBA, or
/// less if the buffer is smaller. VideoToolbox already crops to the stream's
/// own display size (1080 rows for 1080p, not the coded 1088), which may be
/// smaller than what the MP4 claims.
fn to_rgba(buffer: &CVPixelBuffer, width: usize, height: usize) -> Result<RgbaImage> {
    let (width, height) = (width.min(CVPixelBufferGetWidth(buffer)), height.min(CVPixelBufferGetHeight(buffer)));
    let locked = Locked::new(buffer, CVPixelBufferLockFlags::ReadOnly, width, height)?;
    // SAFETY: locked for reading, and `Locked::new` checked the size.
    let src = unsafe { std::slice::from_raw_parts(locked.base, locked.span(width, height)) };
    let mut rgba = vec![0u8; width * height * 4];
    for (dst, src) in rgba.chunks_exact_mut(width * 4).zip(src.chunks(locked.stride)) {
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], 255];
        }
    }
    RgbaImage::from_raw(width as u32, height as u32, rgba).context("sized buffer")
}

/// The encoder's source frames: 32BGRA in IOSurfaces, which the hardware
/// encoder reads without a copy.
fn pixel_buffer_attributes(width: u32, height: u32) -> CFRetained<CFDictionary<CFString, CFType>> {
    let format = CFNumber::new_i64(kCVPixelFormatType_32BGRA.into());
    let (w, h) = (CFNumber::new_i64(width.into()), CFNumber::new_i64(height.into()));
    let surface = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: CoreVideo's own keys.
    let keys = unsafe { [kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey] };
    let values: [&CFType; 4] = [&format, &w, &h, &surface];
    CFDictionary::from_slices(&keys, &values)
}

/// Set a compression property the encoder must take.
///
/// # Safety
///
/// `key` must be a property of `session` that takes `value`'s type.
unsafe fn set(session: &CFType, key: &CFString, value: &CFType) -> Result<()> {
    // SAFETY: see above.
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    ensure!(status == 0, "VideoToolbox rejected {key} (OSStatus {status})");
    Ok(())
}

/// Set a compression property that only helps, which an encoder may not
/// support or may fix itself (some insist on their own colorimetry).
///
/// # Safety
///
/// As [`set`].
unsafe fn hint(session: &CFType, key: &CFString, value: &CFType) -> Result<()> {
    // SAFETY: see above.
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    ensure!([0, kVTPropertyNotSupportedErr, kVTPropertyReadOnlyErr].contains(&status), "VideoToolbox rejected {key} (OSStatus {status})");
    Ok(())
}

/// Own the object a Create-rule function returned through an out-pointer, if
/// it succeeded; else its status.
///
/// # Safety
///
/// `raw` must be null or a +1 reference to a `T`.
unsafe fn created<T: Type>(status: OSStatus, raw: *mut T) -> Result<CFRetained<T>, OSStatus> {
    // SAFETY: see above.
    let object = NonNull::new(raw).map(|raw| unsafe { CFRetained::from_raw(raw) });
    object.filter(|_| status == 0).ok_or(status)
}

/// `value / timescale` seconds.
fn time(value: i64, timescale: i32) -> CMTime {
    CMTime { value, timescale, flags: CMTimeFlags::Valid, epoch: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::split_pictures;

    /// Frame `i` of a `w × h` test video: a gradient that shifts each frame.
    fn frame(w: u32, h: u32, i: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x + i * 8) as u8, y as u8, 128, 255]))
    }

    /// Fed faster than real time, the hardware encoder would queue a dozen
    /// full-size frames; dropping it mid-stream must still tear down cleanly.
    #[test]
    fn bounds_frames_in_flight() {
        let (w, h) = (640, 480);
        let mut enc = Encoder::new(w, h, 30, 2_000_000, false).unwrap();
        let mut out = Vec::new();
        for i in 0..30 {
            enc.encode(frame(w, h, i).as_raw(), &mut out).unwrap();
            assert!(enc.frames - enc.output.lock().done <= MAX_PENDING);
        }
    }

    /// A session that fails mid-stream (here, invalidated under the encoder)
    /// costs only what comes after: `encode` and `finish` still hand over the
    /// pictures it finished before, then report the failure.
    #[test]
    fn keeps_finished_pictures_when_the_session_fails() {
        let (w, h, frames) = (320, 240, 8);
        for fail_in_finish in [false, true] {
            let mut enc = Encoder::new(w, h, 30, 1_000_000, false).unwrap();
            let mut out = Vec::new();
            for i in 0..frames - 1 {
                enc.encode(frame(w, h, i).as_raw(), &mut out).unwrap();
            }
            // The last frame goes in without handing anything over, so its
            // picture at least is still waiting however fast the others came.
            enc.submit(frame(w, h, frames - 1).as_raw()).unwrap();
            // The frames in flight finish, but aren't handed over, before the
            // session is torn down under the encoder.
            // SAFETY: a live session; no callback runs once it's invalidated.
            unsafe {
                assert_eq!(enc.session.complete_frames(kCMTimeInvalid), 0);
                enc.session.invalidate();
            }
            let finished = enc.output.lock().stream.clone();
            assert!(!finished.is_empty(), "pictures waiting to be handed over");

            let handed_over = out.len();
            let failed = match fail_in_finish {
                true => enc.finish(&mut out),
                false => enc.encode(frame(w, h, frames).as_raw(), &mut out),
            };
            let call = if fail_in_finish { "finish" } else { "encode" };
            assert!(failed.is_err(), "{call} on an invalidated session fails");
            let gained = out.len() - handed_over;
            assert!(out[handed_over..] == finished, "{call} hands over the finished pictures first: {gained} of {} bytes", finished.len());
            assert_eq!(split_pictures(&out).1.len(), frames as usize, "{call}: one picture per frame before the failure");
        }
    }

    /// A real-time session may drop frames: they're counted, the pictures
    /// around them kept, and only reordering or a failure is an error.
    #[test]
    fn counts_dropped_frames() {
        let mut pictures = Pictures::default();
        pictures.add(0, Ok(Some(vec![1])));
        pictures.add(1, Ok(None));
        // Frame 2 never came back (dropped when submitted).
        pictures.add(3, Ok(Some(vec![3])));
        assert_eq!((pictures.done, pictures.dropped, pictures.stream.as_slice()), (4, 2, &[1, 3][..]));
        assert!(pictures.error.is_none());
        pictures.add(2, Ok(Some(vec![2])));
        assert!(pictures.error.is_some(), "a picture out of order");
        let mut failed = Pictures::default();
        failed.add(0, Err(anyhow!("boom")));
        failed.add(1, Ok(Some(vec![1])));
        assert!(failed.error.is_some() && failed.stream.is_empty(), "failures stick");
    }

    /// Counting the frames never reported back can't underflow, even if the
    /// callback were to report a frame that was never submitted.
    #[test]
    fn counts_unreported_frames_safely() {
        let mut enc = Encoder::new(64, 64, 30, 1_000_000, false).unwrap();
        enc.output.lock().add(3, Ok(None));
        enc.finish(&mut Vec::new()).unwrap();
    }
}
