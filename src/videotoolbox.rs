//! VideoToolbox H.264 encoder, fed from `BitmapUpdate`s and producing
//! compressed H.264 sample buffers for the EGFX (AVC420) path.
//!
//! Scoped to "Phase 2 Step 1" of the H.264 plan: get a working
//! `VTCompressionSession`, push BGRA frames in, collect compressed bytes
//! out via the output callback. Wiring the output into
//! `Avc420BitmapStream` and the `GfxServerFactory` bridge is the next
//! step and lives in `src/h264.rs`.
//!
//! Conventions follow `src/auth.rs::pam_impl`: direct `extern "C"`,
//! no wrapper crate — the call surface here is small. macOS-only.
//!
//! Bitstream format note. VideoToolbox emits *AVCC*-style sample data:
//! NAL units prefixed with a 4-byte big-endian length, and SPS+PPS
//! carried out-of-band in the `CMFormatDescription`. MS-RDPRFX-AVC420
//! wants *Annex-B*: NAL units prefixed with `00 00 00 01` start codes,
//! with SPS/PPS prepended to each keyframe. The conversion is straight-
//! forward (rewrite length prefixes → start codes, prepend
//! parameter-set NALs to keyframes) but lives outside this module —
//! this file's job is purely to surface the encoded NALs.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::mpsc;

use anyhow::{anyhow, bail, Result};

/// One H.264 sample as emitted by VideoToolbox. The payload is AVCC-
/// formatted (length-prefixed NAL units, no start codes). When this is
/// a keyframe (`is_keyframe = true`), `parameter_sets` carries the
/// SPS/PPS NALs that the client needs to decode it — these come out of
/// the `CMFormatDescription` and are required only on keyframes for
/// Annex-B conversion downstream.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    #[allow(dead_code)]
    pub pts: i64,
    /// SPS / PPS NAL units (raw, no length prefix and no start code).
    /// Populated only on keyframes — empty on non-keyframes.
    pub parameter_sets: Vec<Vec<u8>>,
}

pub struct Encoder {
    inner: ffi::SessionGuard,
    /// VideoToolbox output channel. `Option` because the push pipeline
    /// (`h264.rs`) takes the receiver out via `take_receiver` and runs it on a
    /// dedicated ship thread; once taken, `drain`/`flush` are no-ops here. Tests
    /// keep it and use `flush`.
    rx: Option<mpsc::Receiver<EncodedFrame>>,
    /// Heap-allocated sender pointed at by VT's `outputCallbackRefCon`.
    /// Owned here so it outlives the session; freed when the session is
    /// invalidated in `Drop`.
    _tx_ctx: Box<mpsc::Sender<EncodedFrame>>,
    width: u16,
    height: u16,
    next_pts: i64,
    /// Frame duration as a `CMTime` ratio (numerator, denominator).
    /// VT uses this for rate control even when we drive PTS manually.
    fps: u32,
    /// When true (the default), convert BGRA → full-range (0-255) NV12 ourselves
    /// and feed VT a `420f` buffer, so the encoded stream is full-range. mstsc
    /// reads AVC420 luma as full-range regardless of the VUI flag, so
    /// video-range output (VT's default from a BGRA source) shows raised/washed
    /// blacks there. FreeRDP honors the range flag so it stays correct either
    /// way. Opt back out to video-range with `MACRDP_H264_FULL_RANGE=0`.
    full_range: bool,
}

impl Encoder {
    /// Create a new H.264 encoder. `bitrate_bps` is the target average
    /// bitrate; `fps` sets the frame-duration hint used by VT's rate
    /// controller. The first encoded frame will always be a keyframe.
    pub fn new(
        width: u16,
        height: u16,
        fps: u32,
        bitrate_bps: u32,
        keyframe_secs: f32,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<EncodedFrame>();
        // The callback receives the raw `*mut Sender` and clones it per
        // delivery — see `ffi::output_callback`. Keep the original Box
        // alive on the Encoder so the pointer stays valid.
        let tx_box = Box::new(tx);
        let tx_ptr = Box::as_ref(&tx_box) as *const mpsc::Sender<EncodedFrame> as *mut c_void;

        // Default to full-range NV12 — verified to fix mstsc's washed/lighter
        // image while keeping FreeRDP correct. Opt back out to video-range (let
        // VT convert from BGRA) with MACRDP_H264_FULL_RANGE=0 for debugging.
        let full_range = !matches!(
            std::env::var("MACRDP_H264_FULL_RANGE").as_deref(),
            Ok("0") | Ok("false") | Ok("FALSE")
        );
        // Keyframe interval is a frame count; derive it from the requested
        // seconds and the frame rate. At least 1 (every frame an IDR).
        let keyframe_frames = (f64::from(fps) * f64::from(keyframe_secs)).round().max(1.0) as u32;
        let session = ffi::create_session(width, height, bitrate_bps, keyframe_frames, tx_ptr)?;
        Ok(Self {
            inner: session,
            rx: Some(rx),
            _tx_ctx: tx_box,
            width,
            height,
            next_pts: 0,
            fps,
            full_range,
        })
    }

    /// Take the VideoToolbox output receiver, to run it on a dedicated ship
    /// thread (the push pipeline). After this, `drain`/`flush` on this encoder
    /// return nothing; the encoder is submit-only. Returns `None` if already
    /// taken.
    pub fn take_receiver(&mut self) -> Option<mpsc::Receiver<EncodedFrame>> {
        self.rx.take()
    }

    /// Submit a BGRA frame for encoding. `stride` is in bytes per row
    /// of the source buffer — VideoToolbox is told the source pixel
    /// format is `kCVPixelFormatType_32BGRA`, so each pixel is 4 bytes
    /// in B, G, R, A order, matching what `capture.rs` produces.
    ///
    /// `force_keyframe` requests an IDR via VT's frame-property dict.
    /// Use it after dropping frames (e.g. recovering from EGFX
    /// backpressure) so the next encoded output restarts the H.264
    /// reference chain cleanly — otherwise P-frames after a drop refer
    /// to data the client never received and a region of the surface
    /// freezes.
    ///
    /// Returns immediately; encoded output arrives asynchronously via
    /// `drain()`. VT calls our output callback on its own thread, so
    /// the channel decouples producer and consumer cleanly.
    pub fn encode_bgra(&mut self, bgra: &[u8], stride: usize, force_keyframe: bool) -> Result<()> {
        let expected = stride
            .checked_mul(self.height.into())
            .ok_or_else(|| anyhow!("stride*height overflows usize"))?;
        if bgra.len() < expected {
            bail!(
                "BGRA buffer too small: have {}, need {} (stride {} * height {})",
                bgra.len(),
                expected,
                stride,
                self.height
            );
        }
        let pts = self.next_pts;
        self.next_pts = self.next_pts.wrapping_add(1);
        ffi::encode_frame(
            &self.inner,
            bgra,
            self.width,
            self.height,
            stride,
            pts,
            self.fps,
            force_keyframe,
            self.full_range,
        )
    }

    /// Pull every encoded frame the callback has produced so far.
    /// Non-blocking; returns an empty vec if nothing is ready (or the receiver
    /// has been taken for the push pipeline).
    pub fn drain(&mut self) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        if let Some(rx) = self.rx.as_ref() {
            while let Ok(frame) = rx.try_recv() {
                out.push(frame);
            }
        }
        out
    }

    /// Block until every frame submitted so far has been encoded and
    /// delivered to the output callback. The live pipeline uses the
    /// non-blocking `drain()`; `flush` is kept for shutdown / deterministic
    /// tests (and exercised by the encoder round-trip test).
    #[allow(dead_code)]
    pub fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        ffi::complete_frames(&self.inner)?;
        Ok(self.drain())
    }
}

// VT's compression session is thread-safe according to Apple's docs
// (frames can be submitted from any thread). The Receiver is !Sync but
// that's fine — we only call `drain()` from the owning thread.
unsafe impl Send for Encoder {}

mod ffi {
    use super::EncodedFrame;
    use anyhow::{anyhow, bail, Result};
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::mpsc;

    pub(super) type OSStatus = i32;
    pub(super) type Boolean = u8;
    pub(super) type CFTypeRef = *const c_void;
    pub(super) type CFAllocatorRef = CFTypeRef;
    pub(super) type CFStringRef = CFTypeRef;
    pub(super) type CFNumberRef = CFTypeRef;
    pub(super) type CFBooleanRef = CFTypeRef;
    pub(super) type CFDictionaryRef = CFTypeRef;
    pub(super) type CFArrayRef = CFTypeRef;
    pub(super) type CVPixelBufferRef = CFTypeRef;
    pub(super) type CVImageBufferRef = CFTypeRef;
    pub(super) type CMSampleBufferRef = CFTypeRef;
    pub(super) type CMBlockBufferRef = CFTypeRef;
    pub(super) type CMFormatDescriptionRef = CFTypeRef;
    pub(super) type VTCompressionSessionRef = CFTypeRef;
    pub(super) type VTEncodeInfoFlags = u32;
    pub(super) type OSType = u32;

    // Four-char codes are big-endian-packed on macOS.
    // 'B' 'G' 'R' 'A' = 0x42 0x47 0x52 0x41
    pub(super) const K_CV_PIXEL_FORMAT_TYPE_32_BGRA: OSType = 0x4247_5241; // 'BGRA'
                                                                           // '4' '2' '0' 'f' — bi-planar (NV12) Y'CbCr 4:2:0, FULL range (luma 0-255).
                                                                           // Used when the full-range path is enabled so mstsc — which reads AVC420
                                                                           // luma as full-range and washes video-range content lighter — gets data
                                                                           // whose range matches its assumption.
    pub(super) const K_CV_PIXEL_FORMAT_TYPE_420F: OSType = 0x3432_3066; // '420f'
                                                                        // 'a' 'v' 'c' '1' = 0x61 0x76 0x63 0x31
    pub(super) const K_CM_VIDEO_CODEC_TYPE_H264: OSType = 0x6176_6331; // 'avc1'

    #[repr(C)]
    #[derive(Copy, Clone, Debug)]
    pub(super) struct CMTime {
        pub value: i64,
        pub timescale: i32,
        pub flags: u32,
        pub epoch: i64,
    }
    pub(super) const K_CM_TIME_FLAGS_VALID: u32 = 1;

    pub(super) const KCF_NUMBER_INT32_TYPE: i32 = 3;

    /// Opaque stand-in for `CFDictionaryKeyCallBacks` /
    /// `CFDictionaryValueCallBacks`. We never read these structs — we
    /// only take the address of the linker-resolved `kCFType*` globals
    /// and pass it to `CFDictionaryCreate`. Size is padded large enough
    /// to outlive any future struct changes; reads would be UB but we
    /// don't do any.
    #[repr(C)]
    pub(super) struct CFCallbacksOpaque {
        _padding: [u8; 64],
    }

    /// Output callback type per `<VideoToolbox/VTCompressionSession.h>`:
    /// `void (*VTCompressionOutputCallback)(void *outputCallbackRefCon,
    ///   void *sourceFrameRefCon, OSStatus status, VTEncodeInfoFlags,
    ///   CMSampleBufferRef sampleBuffer);`
    pub(super) type VTCompressionOutputCallback = unsafe extern "C" fn(
        output_callback_ref_con: *mut c_void,
        source_frame_ref_con: *mut c_void,
        status: OSStatus,
        info_flags: VTEncodeInfoFlags,
        sample_buffer: CMSampleBufferRef,
    );

    // Each framework needs its own `#[link]`; clippy's duplicated_attributes
    // lint objects to the repeated `kind = "framework"`, which is unavoidable.
    #[allow(clippy::duplicated_attributes)]
    #[link(name = "CoreFoundation", kind = "framework")]
    #[link(name = "CoreVideo", kind = "framework")]
    #[link(name = "CoreMedia", kind = "framework")]
    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        // CoreFoundation memory mgmt.
        pub(super) fn CFRelease(cf: CFTypeRef);
        pub(super) fn CFNumberCreate(
            allocator: CFAllocatorRef,
            the_type: i32,
            value_ptr: *const c_void,
        ) -> CFNumberRef;

        // Constant string handles for VT property keys + profile levels.
        // These are global symbols, dereferenced for the actual CFStringRef.
        pub(super) static kVTCompressionPropertyKey_RealTime: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_MaxFrameDelayCount: CFStringRef;
        pub(super) static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
        pub(super) static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
        // Color-space signaling so the encoded SPS VUI describes its color
        // space explicitly instead of leaving the decoder to guess (which is
        // why mstsc washed the image lighter while FreeRDP guessed right).
        pub(super) static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
        // BT.709 — the HD convention RDP H.264 clients expect. Symbols live in
        // CoreVideo (already linked below).
        pub(super) static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
        pub(super) static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
        pub(super) static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
        pub(super) static kCFBooleanTrue: CFBooleanRef;
        pub(super) static kCFBooleanFalse: CFBooleanRef;

        // CFDictionary creation for VT frame-properties payloads.
        // `CFDictionaryKeyCallBacks` / `CFDictionaryValueCallBacks` are
        // opaque structs here — we never read their contents, only take
        // the address of the global "CF type" instances and hand it to
        // CFDictionaryCreate. The byte sizes don't matter for that.
        pub(super) static kCFTypeDictionaryKeyCallBacks: CFCallbacksOpaque;
        pub(super) static kCFTypeDictionaryValueCallBacks: CFCallbacksOpaque;
        pub(super) fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;

        // CVPixelBuffer.
        pub(super) fn CVPixelBufferCreate(
            allocator: CFAllocatorRef,
            width: usize,
            height: usize,
            pixel_format_type: OSType,
            pixel_buffer_attributes: CFDictionaryRef,
            pixel_buffer_out: *mut CVPixelBufferRef,
        ) -> i32;
        pub(super) fn CVPixelBufferLockBaseAddress(
            pixel_buffer: CVPixelBufferRef,
            lock_flags: u64,
        ) -> i32;
        pub(super) fn CVPixelBufferUnlockBaseAddress(
            pixel_buffer: CVPixelBufferRef,
            unlock_flags: u64,
        ) -> i32;
        pub(super) fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut c_void;
        pub(super) fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
        pub(super) fn CVPixelBufferGetBaseAddressOfPlane(
            pixel_buffer: CVPixelBufferRef,
            plane_index: usize,
        ) -> *mut c_void;
        pub(super) fn CVPixelBufferGetBytesPerRowOfPlane(
            pixel_buffer: CVPixelBufferRef,
            plane_index: usize,
        ) -> usize;

        // CMSampleBuffer accessors used in the output callback.
        pub(super) fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
        pub(super) fn CMSampleBufferGetFormatDescription(
            sbuf: CMSampleBufferRef,
        ) -> CMFormatDescriptionRef;
        pub(super) fn CMSampleBufferGetSampleAttachmentsArray(
            sbuf: CMSampleBufferRef,
            create_if_necessary: Boolean,
        ) -> CFArrayRef;
        pub(super) fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
        pub(super) fn CMBlockBufferGetDataLength(bbuf: CMBlockBufferRef) -> usize;
        pub(super) fn CMBlockBufferCopyDataBytes(
            bbuf: CMBlockBufferRef,
            offset: usize,
            length: usize,
            destination: *mut c_void,
        ) -> OSStatus;
        pub(super) fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            fmt: CMFormatDescriptionRef,
            param_set_index: usize,
            param_set_pointer_out: *mut *const u8,
            param_set_size_out: *mut usize,
            param_set_count_out: *mut usize,
            nal_unit_header_length_out: *mut i32,
        ) -> OSStatus;

        // CFArray (used to test the "NotSync" keyframe attachment).
        pub(super) fn CFArrayGetCount(arr: CFArrayRef) -> isize;
        pub(super) fn CFArrayGetValueAtIndex(arr: CFArrayRef, idx: isize) -> CFTypeRef;
        pub(super) fn CFDictionaryGetValue(
            dict: CFDictionaryRef,
            key: *const c_void,
        ) -> *const c_void;
        pub(super) static kCMSampleAttachmentKey_NotSync: CFStringRef;

        // VTCompressionSession.
        pub(super) fn VTCompressionSessionCreate(
            allocator: CFAllocatorRef,
            width: i32,
            height: i32,
            codec_type: OSType,
            encoder_specification: CFDictionaryRef,
            source_image_buffer_attributes: CFDictionaryRef,
            compressed_data_allocator: CFAllocatorRef,
            output_callback: Option<VTCompressionOutputCallback>,
            output_callback_ref_con: *mut c_void,
            compression_session_out: *mut VTCompressionSessionRef,
        ) -> OSStatus;
        pub(super) fn VTSessionSetProperty(
            session: VTCompressionSessionRef,
            property_key: CFStringRef,
            property_value: CFTypeRef,
        ) -> OSStatus;
        pub(super) fn VTCompressionSessionPrepareToEncodeFrames(
            session: VTCompressionSessionRef,
        ) -> OSStatus;
        pub(super) fn VTCompressionSessionEncodeFrame(
            session: VTCompressionSessionRef,
            image_buffer: CVImageBufferRef,
            presentation_timestamp: CMTime,
            duration: CMTime,
            frame_properties: CFDictionaryRef,
            source_frame_ref_con: *mut c_void,
            info_flags_out: *mut VTEncodeInfoFlags,
        ) -> OSStatus;
        #[allow(dead_code)] // only reached via Encoder::flush (shutdown / tests)
        pub(super) fn VTCompressionSessionCompleteFrames(
            session: VTCompressionSessionRef,
            complete_until_presentation_timestamp: CMTime,
        ) -> OSStatus;
        pub(super) fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    }

    /// RAII guard for the `VTCompressionSession`. `CFRelease` is the
    /// counterpart to `VTCompressionSessionCreate`'s implicit retain.
    pub(super) struct SessionGuard {
        pub(super) session: VTCompressionSessionRef,
    }

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.session.is_null() {
                    VTCompressionSessionInvalidate(self.session);
                    CFRelease(self.session);
                }
            }
        }
    }
    // The session itself is documented as thread-safe.
    unsafe impl Send for SessionGuard {}

    pub(super) fn create_session(
        width: u16,
        height: u16,
        bitrate_bps: u32,
        keyframe_frames: u32,
        tx_ctx: *mut c_void,
    ) -> Result<SessionGuard> {
        let mut session: VTCompressionSessionRef = ptr::null();
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                i32::from(width),
                i32::from(height),
                K_CM_VIDEO_CODEC_TYPE_H264,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                Some(output_callback),
                tx_ctx,
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            bail!("VTCompressionSessionCreate failed: OSStatus {status}");
        }
        let guard = SessionGuard { session };

        // Real-time low-latency profile. Disable frame reordering so
        // every emitted frame is immediately decodable in order — RDP
        // does not tolerate B-frames or reorder delay.
        unsafe {
            set_bool(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)?;
            set_bool(
                session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
            )?;
            // Don't let the encoder hold frames waiting for future input — emit
            // each one as soon as it's coded. Without this VT buffers a frame
            // and only flushes it when the NEXT frame is submitted, so a single
            // change (keystroke / caret) renders "one behind" until you type
            // again. Best-effort: ignore if a VT version rejects the value.
            let _ = set_i32(session, kVTCompressionPropertyKey_MaxFrameDelayCount, 0);
            set_string(
                session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Baseline_AutoLevel,
            )?;
            // Tag the bitstream's color space explicitly (BT.709). Without this
            // VideoToolbox leaves the SPS VUI under-specified and each decoder
            // guesses: FreeRDP guessed right, mstsc guessed wrong and raised the
            // black level so the whole image looked lighter/washed out once the
            // H.264 stream took over from the legacy bitmap seed. Best-effort —
            // ignore on a VT version that rejects a key rather than fail encode.
            let _ = set_string(
                session,
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
            );
            let _ = set_string(
                session,
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
            );
            let _ = set_string(
                session,
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            );
            set_i32(
                session,
                kVTCompressionPropertyKey_AverageBitRate,
                bitrate_bps as i32,
            )?;
            // Keyframe (IDR) interval — a heal-vs-smoothness knob. The transport
            // is reliable TLS/TCP, so this isn't for packet loss; it's so a
            // client that hits a transient *decode* glitch can resync without
            // reconnecting. mstsc in particular shows lingering macroblock
            // garbage (very visible on text) until the next IDR, because over a
            // long GOP a single bad P-frame is never corrected — the symptom
            // that "heals on redraw / after a while". An IDR does cost a brief
            // hitch (a large intra frame can trip EGFX backpressure), so lower =
            // faster recovery, higher = smoother typing. The session's first
            // keyframe is still forced explicitly in encode_bgra(); this governs
            // only the periodic safety net. (Was fps*30 ≈ 30s, which let mstsc
            // corruption persist for half a minute.) Set via --keyframe-interval
            // (seconds), already converted to a frame count by the caller.
            set_i32(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                keyframe_frames.max(1) as i32,
            )?;
        }

        let prepared = unsafe { VTCompressionSessionPrepareToEncodeFrames(session) };
        if prepared != 0 {
            bail!("VTCompressionSessionPrepareToEncodeFrames failed: OSStatus {prepared}");
        }
        Ok(guard)
    }

    unsafe fn set_bool(
        session: VTCompressionSessionRef,
        key: CFStringRef,
        value: CFBooleanRef,
    ) -> Result<()> {
        let status = VTSessionSetProperty(session, key, value);
        if status != 0 {
            bail!("VTSessionSetProperty(bool) failed: OSStatus {status}");
        }
        Ok(())
    }

    unsafe fn set_string(
        session: VTCompressionSessionRef,
        key: CFStringRef,
        value: CFStringRef,
    ) -> Result<()> {
        let status = VTSessionSetProperty(session, key, value);
        if status != 0 {
            bail!("VTSessionSetProperty(str) failed: OSStatus {status}");
        }
        Ok(())
    }

    unsafe fn set_i32(
        session: VTCompressionSessionRef,
        key: CFStringRef,
        value: i32,
    ) -> Result<()> {
        let number = CFNumberCreate(
            ptr::null(),
            KCF_NUMBER_INT32_TYPE,
            &value as *const i32 as *const c_void,
        );
        if number.is_null() {
            bail!("CFNumberCreate returned null");
        }
        let status = VTSessionSetProperty(session, key, number);
        CFRelease(number);
        if status != 0 {
            bail!("VTSessionSetProperty(i32 {value}) failed: OSStatus {status}");
        }
        Ok(())
    }

    /// Convert a BGRA frame to **full-range** (luma 0-255) BT.709 NV12, writing
    /// into the caller-provided Y plane and interleaved-CbCr plane. Full-range
    /// because mstsc reads AVC420 luma as full-range regardless of the
    /// bitstream's VUI range flag, so video-range output shows washed/raised
    /// blacks there; FreeRDP honors the flag and is correct either way. BGRA
    /// byte order is (B, G, R, A). Chroma is 4:2:0, averaged over each 2x2 block.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bgra_to_nv12_full_range(
        bgra: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
        y_plane: &mut [u8],
        y_stride: usize,
        cbcr_plane: &mut [u8],
        cbcr_stride: usize,
    ) {
        const KR: f32 = 0.2126;
        const KG: f32 = 0.7152;
        const KB: f32 = 0.0722;
        // Luma: one sample per pixel.
        for y in 0..height {
            let src = &bgra[y * src_stride..];
            let yrow = &mut y_plane[y * y_stride..];
            for x in 0..width {
                let p = &src[x * 4..];
                let (b, g, r) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
                let luma = KR * r + KG * g + KB * b; // 0..255, full range
                yrow[x] = luma.round().clamp(0.0, 255.0) as u8;
            }
        }
        // Chroma: average each 2x2 block, store interleaved Cb then Cr.
        let cw = width / 2;
        let ch = height / 2;
        for cy in 0..ch {
            let crow = &mut cbcr_plane[cy * cbcr_stride..];
            for cx in 0..cw {
                let (mut rs, mut gs, mut bs) = (0.0f32, 0.0f32, 0.0f32);
                for dy in 0..2 {
                    let src = &bgra[(cy * 2 + dy) * src_stride..];
                    for dx in 0..2 {
                        let p = &src[(cx * 2 + dx) * 4..];
                        bs += f32::from(p[0]);
                        gs += f32::from(p[1]);
                        rs += f32::from(p[2]);
                    }
                }
                let (r, g, b) = (rs / 4.0, gs / 4.0, bs / 4.0);
                let luma = KR * r + KG * g + KB * b;
                // Full-range Cb/Cr: deviation / 2*(1-Kb|Kr), bias 128.
                let cb = (b - luma) / 1.8556 + 128.0;
                let cr = (r - luma) / 1.5748 + 128.0;
                crow[cx * 2] = cb.round().clamp(0.0, 255.0) as u8;
                crow[cx * 2 + 1] = cr.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    // --- vImage (Accelerate) accelerated BGRA -> full-range NV12 -------------
    //
    // The scalar `bgra_to_nv12_full_range` above is correct and fine at 1080p,
    // but at 4K/60 (or on weak CPUs) it exceeds the frame budget. vImage does
    // the same RGB->Y'CbCr math on the CPU's vector units (NEON on Apple
    // Silicon), typically several times faster. All struct layouts/enum values
    // are transcribed from the macOS SDK's vImage headers; the conversion
    // descriptor is generated once (it's constant) and reused.

    /// `vImage_Buffer`. `height`/`width` are `vImagePixelCount` (unsigned long),
    /// `row_bytes` is `size_t` — all `usize` on 64-bit.
    #[repr(C)]
    struct VImageBuffer {
        data: *mut c_void,
        height: usize,
        width: usize,
        row_bytes: usize,
    }

    /// `vImage_YpCbCrPixelRange` (8 × int32). Full-range 8-bit values per the
    /// header's own example `{0,128,255,255,255,0,255,0}` (Y' 0-255, no clamp).
    #[repr(C)]
    struct VImageYpCbCrPixelRange {
        yp_bias: i32,
        cbcr_bias: i32,
        yp_range_max: i32,
        cbcr_range_max: i32,
        yp_max: i32,
        yp_min: i32,
        cbcr_max: i32,
        cbcr_min: i32,
    }

    /// `vImage_ARGBToYpCbCr` — opaque 128-byte, 16-aligned conversion descriptor.
    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    struct VImageArgbToYpCbCr {
        opaque: [u8; 128],
    }

    /// `vImage_ARGBToYpCbCrMatrix` — opaque to us; we only hold a pointer to the
    /// framework-provided BT.709 constant, never construct or read it.
    #[repr(C)]
    struct VImageArgbToYpCbCrMatrix {
        _private: [u8; 0],
    }

    const K_VIMAGE_NO_FLAGS: u32 = 0;
    const K_VIMAGE_ARGB8888: u32 = 0; // vImageARGBType::kvImageARGB8888
    const K_VIMAGE_420YP8_CBCR8: u32 = 4; // vImageYpCbCrType::kvImage420Yp8_CbCr8

    #[link(name = "Accelerate", kind = "framework")]
    extern "C" {
        /// `const vImage_ARGBToYpCbCrMatrix *` — the symbol itself is a pointer.
        static kvImage_ARGBToYpCbCrMatrix_ITU_R_709_2: *const VImageArgbToYpCbCrMatrix;
        fn vImageConvert_ARGBToYpCbCr_GenerateConversion(
            matrix: *const VImageArgbToYpCbCrMatrix,
            pixel_range: *const VImageYpCbCrPixelRange,
            out_info: *mut VImageArgbToYpCbCr,
            in_argb_type: u32,
            out_ypcbcr_type: u32,
            flags: u32,
        ) -> isize; // vImage_Error = ssize_t
        fn vImageConvert_ARGB8888To420Yp8_CbCr8(
            src: *const VImageBuffer,
            dest_yp: *const VImageBuffer,
            dest_cbcr: *const VImageBuffer,
            info: *const VImageArgbToYpCbCr,
            permute_map: *const u8,
            flags: u32,
        ) -> isize;
    }

    /// The conversion descriptor (709 + full-range, ARGB8888 -> NV12). Constant,
    /// so generate once and share — vImage docs say these are reusable across
    /// threads. `None` if generation failed (then callers fall back to scalar).
    fn vimage_conv_info() -> Option<&'static VImageArgbToYpCbCr> {
        static INFO: std::sync::OnceLock<Option<VImageArgbToYpCbCr>> = std::sync::OnceLock::new();
        INFO.get_or_init(|| {
            let range = VImageYpCbCrPixelRange {
                yp_bias: 0,
                cbcr_bias: 128,
                yp_range_max: 255,
                cbcr_range_max: 255,
                yp_max: 255,
                yp_min: 0,
                cbcr_max: 255,
                cbcr_min: 0,
            };
            let mut info = VImageArgbToYpCbCr { opaque: [0u8; 128] };
            let err = unsafe {
                vImageConvert_ARGBToYpCbCr_GenerateConversion(
                    kvImage_ARGBToYpCbCrMatrix_ITU_R_709_2,
                    &range,
                    &mut info,
                    K_VIMAGE_ARGB8888,
                    K_VIMAGE_420YP8_CBCR8,
                    K_VIMAGE_NO_FLAGS,
                )
            };
            (err == 0).then_some(info)
        })
        .as_ref()
    }

    /// vImage equivalent of `bgra_to_nv12_full_range`. `Err` if vImage isn't
    /// usable for this frame (descriptor generation failed, or the convert call
    /// errored — e.g. odd dimensions); callers fall back to the scalar path.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bgra_to_nv12_full_range_vimage(
        bgra: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
        y_plane: &mut [u8],
        y_stride: usize,
        cbcr_plane: &mut [u8],
        cbcr_stride: usize,
    ) -> Result<()> {
        let info = vimage_conv_info().ok_or_else(|| anyhow!("vImage conv-info unavailable"))?;
        let src = VImageBuffer {
            data: bgra.as_ptr() as *mut c_void,
            height,
            width,
            row_bytes: src_stride,
        };
        let dest_yp = VImageBuffer {
            data: y_plane.as_mut_ptr() as *mut c_void,
            height,
            width,
            row_bytes: y_stride,
        };
        let dest_cbcr = VImageBuffer {
            data: cbcr_plane.as_mut_ptr() as *mut c_void,
            height: height / 2,
            width: width / 2,
            row_bytes: cbcr_stride,
        };
        // Source is BGRA (byte order B,G,R,A); vImage's ARGB8888 is A,R,G,B.
        // permuteMap[dest] = source index: A<-3, R<-2, G<-1, B<-0.
        let permute: [u8; 4] = [3, 2, 1, 0];
        let err = unsafe {
            vImageConvert_ARGB8888To420Yp8_CbCr8(
                &src,
                &dest_yp,
                &dest_cbcr,
                info,
                permute.as_ptr(),
                K_VIMAGE_NO_FLAGS,
            )
        };
        if err != 0 {
            bail!("vImageConvert_ARGB8888To420Yp8_CbCr8 failed: {err}");
        }
        Ok(())
    }

    // --- BGRA -> full-range YUV444 planar (AVC444 scoping benchmark) ---------
    //
    // Two reference implementations of full-resolution chroma conversion, kept
    // side-by-side for an A/B perf decision before committing to AVC444's
    // 2-encoder pipeline. Same BT.709 full-range matrix as the NV12 path.
    // Neither is wired into encode_frame; they exist for the bench in
    // videotoolbox::tests::bench_yuv444_full_range.
    //
    // Gated behind cfg(test) until AVC444 is actually wired up — they're
    // dead code in a production build today.

    /// Scalar BGRA -> three full-resolution planar Y/Cb/Cr buffers. The matrix
    /// matches `bgra_to_nv12_full_range` exactly — the only difference is no
    /// 2x2 averaging.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bgra_to_yuv444_planar_scalar(
        bgra: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
        y_plane: &mut [u8],
        y_stride: usize,
        cb_plane: &mut [u8],
        cb_stride: usize,
        cr_plane: &mut [u8],
        cr_stride: usize,
    ) {
        const KR: f32 = 0.2126;
        const KG: f32 = 0.7152;
        const KB: f32 = 0.0722;
        for y in 0..height {
            let src = &bgra[y * src_stride..];
            let yrow = &mut y_plane[y * y_stride..];
            let cbrow = &mut cb_plane[y * cb_stride..];
            let crrow = &mut cr_plane[y * cr_stride..];
            for x in 0..width {
                let p = &src[x * 4..];
                let (b, g, r) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
                let luma = KR * r + KG * g + KB * b;
                yrow[x] = luma.round().clamp(0.0, 255.0) as u8;
                // Full-range Cb/Cr: deviation / 2*(1-Kb|Kr), bias 128.
                let cb = (b - luma) / 1.8556 + 128.0;
                let cr = (r - luma) / 1.5748 + 128.0;
                cbrow[x] = cb.round().clamp(0.0, 255.0) as u8;
                crrow[x] = cr.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    #[cfg(test)]
    extern "C" {
        /// `vImageMatrixMultiply_ARGB8888ToPlanar8` (vImage Transform.h). One call
        /// per output plane. The matrix is `int16_t[4]` in source byte order; we
        /// give BGRA bytes (B=byte0, G=byte1, R=byte2, A=byte3) so each
        /// coefficient is placed accordingly.
        fn vImageMatrixMultiply_ARGB8888ToPlanar8(
            src: *const VImageBuffer,
            dest: *const VImageBuffer,
            matrix: *const i16, // length 4
            divisor: i32,
            pre_bias: *const i16, // null = no pre-bias
            post_bias: i32,
            flags: u32,
        ) -> isize;
    }

    /// BT.709 full-range matrix in Q14 fixed-point, indexed by source BGRA byte
    /// order (matrix[0] = B, [1] = G, [2] = R, [3] = A=0).
    ///
    /// Derived from the scalar reference:
    /// - Y  =  0.2126 R + 0.7152 G + 0.0722 B
    /// - Cb = -0.114572 R - 0.385428 G + 0.500000 B + 128
    /// - Cr =  0.500000 R - 0.454153 G - 0.045847 B + 128
    #[cfg(test)]
    const DIVISOR: i32 = 16384; // 2^14

    // Y plane coefficients: [B, G, R, A] * Q14, no post-bias.
    #[cfg(test)]
    const Y_MATRIX: [i16; 4] = [1183, 11718, 3483, 0]; // 0.0722, 0.7152, 0.2126, 0
    #[cfg(test)]
    const Y_POST_BIAS: i32 = DIVISOR / 2; // rounding offset

    // Cb plane coefficients + 128*Q14 bias to recenter.
    #[cfg(test)]
    const CB_MATRIX: [i16; 4] = [8192, -6315, -1877, 0]; // 0.5, -0.385428, -0.114572, 0
    #[cfg(test)]
    const CB_POST_BIAS: i32 = 128 * DIVISOR + DIVISOR / 2;

    // Cr plane.
    #[cfg(test)]
    const CR_MATRIX: [i16; 4] = [-751, -7440, 8192, 0]; // -0.045847, -0.454153, 0.5, 0
    #[cfg(test)]
    const CR_POST_BIAS: i32 = 128 * DIVISOR + DIVISOR / 2;

    /// vImage BGRA -> three full-resolution planar Y/Cb/Cr buffers via three
    /// `vImageMatrixMultiply_ARGB8888ToPlanar8` calls (one per plane).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bgra_to_yuv444_planar_vimage(
        bgra: &[u8],
        src_stride: usize,
        width: usize,
        height: usize,
        y_plane: &mut [u8],
        y_stride: usize,
        cb_plane: &mut [u8],
        cb_stride: usize,
        cr_plane: &mut [u8],
        cr_stride: usize,
    ) -> Result<()> {
        let src = VImageBuffer {
            data: bgra.as_ptr() as *mut c_void,
            height,
            width,
            row_bytes: src_stride,
        };
        for (plane, stride, matrix, post_bias) in [
            (&mut *y_plane, y_stride, &Y_MATRIX, Y_POST_BIAS),
            (&mut *cb_plane, cb_stride, &CB_MATRIX, CB_POST_BIAS),
            (&mut *cr_plane, cr_stride, &CR_MATRIX, CR_POST_BIAS),
        ] {
            let dest = VImageBuffer {
                data: plane.as_mut_ptr() as *mut c_void,
                height,
                width,
                row_bytes: stride,
            };
            let err = unsafe {
                vImageMatrixMultiply_ARGB8888ToPlanar8(
                    &src,
                    &dest,
                    matrix.as_ptr(),
                    DIVISOR,
                    std::ptr::null(),
                    post_bias,
                    K_VIMAGE_NO_FLAGS,
                )
            };
            if err != 0 {
                bail!("vImageMatrixMultiply_ARGB8888ToPlanar8 failed: {err}");
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // internal FFI helper; grouping into a struct adds no clarity
    pub(super) fn encode_frame(
        guard: &SessionGuard,
        bgra: &[u8],
        width: u16,
        height: u16,
        stride: usize,
        pts: i64,
        fps: u32,
        force_keyframe: bool,
        full_range: bool,
    ) -> Result<()> {
        // Full-range path feeds VT a `420f` (full-range NV12) buffer we fill
        // ourselves; otherwise hand VT BGRA and let it produce video-range YUV.
        let pixel_format = if full_range {
            K_CV_PIXEL_FORMAT_TYPE_420F
        } else {
            K_CV_PIXEL_FORMAT_TYPE_32_BGRA
        };
        let mut pbuf: CVPixelBufferRef = ptr::null();
        let status = unsafe {
            CVPixelBufferCreate(
                ptr::null(),
                width.into(),
                height.into(),
                pixel_format,
                ptr::null(),
                &mut pbuf,
            )
        };
        if status != 0 || pbuf.is_null() {
            bail!("CVPixelBufferCreate failed: {status}");
        }

        // Pixel buffer ownership: VTCompressionSessionEncodeFrame
        // retains the CVPixelBuffer for the duration of the encode, so
        // we release our reference right after submit. The actual
        // CFRelease lives in the cleanup path below regardless of
        // success/failure so we don't leak on errors.
        let result = unsafe {
            CVPixelBufferLockBaseAddress(pbuf, 0);
            if full_range {
                // Two planes: Y (full size) + interleaved CbCr (half size).
                let y_base = CVPixelBufferGetBaseAddressOfPlane(pbuf, 0) as *mut u8;
                let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pbuf, 0);
                let cbcr_base = CVPixelBufferGetBaseAddressOfPlane(pbuf, 1) as *mut u8;
                let cbcr_stride = CVPixelBufferGetBytesPerRowOfPlane(pbuf, 1);
                let y_plane =
                    std::slice::from_raw_parts_mut(y_base, y_stride * usize::from(height));
                let cbcr_plane = std::slice::from_raw_parts_mut(
                    cbcr_base,
                    cbcr_stride * (usize::from(height) / 2),
                );
                // Prefer the vImage (SIMD) path; fall back to the scalar
                // conversion if vImage can't handle this frame (descriptor
                // generation failed, or odd dimensions). Both produce full-range
                // BT.709 NV12, so the fallback is transparent.
                if bgra_to_nv12_full_range_vimage(
                    bgra,
                    stride,
                    width.into(),
                    height.into(),
                    y_plane,
                    y_stride,
                    cbcr_plane,
                    cbcr_stride,
                )
                .is_err()
                {
                    bgra_to_nv12_full_range(
                        bgra,
                        stride,
                        width.into(),
                        height.into(),
                        y_plane,
                        y_stride,
                        cbcr_plane,
                        cbcr_stride,
                    );
                }
            } else {
                let dst = CVPixelBufferGetBaseAddress(pbuf) as *mut u8;
                let dst_stride = CVPixelBufferGetBytesPerRow(pbuf);
                let row_bytes = usize::from(width) * 4;
                // CVPixelBuffer may have a different stride than the source
                // (alignment to 64 bytes is common). Copy row by row.
                for row in 0..usize::from(height) {
                    let src_offset = row * stride;
                    let dst_offset = row * dst_stride;
                    ptr::copy_nonoverlapping(
                        bgra.as_ptr().add(src_offset),
                        dst.add(dst_offset),
                        row_bytes,
                    );
                }
            }
            CVPixelBufferUnlockBaseAddress(pbuf, 0);

            let presentation = CMTime {
                value: pts,
                timescale: i32::try_from(fps).unwrap_or(30),
                flags: K_CM_TIME_FLAGS_VALID,
                epoch: 0,
            };
            let duration = CMTime {
                value: 1,
                timescale: i32::try_from(fps).unwrap_or(30),
                flags: K_CM_TIME_FLAGS_VALID,
                epoch: 0,
            };
            let frame_props: CFDictionaryRef = if force_keyframe {
                let key = kVTEncodeFrameOptionKey_ForceKeyFrame;
                let value = kCFBooleanTrue;
                CFDictionaryCreate(
                    ptr::null(),
                    &key as *const *const c_void,
                    &value as *const *const c_void,
                    1,
                    &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
                    &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
                )
            } else {
                ptr::null()
            };

            let mut info_flags: VTEncodeInfoFlags = 0;
            let encode_status = VTCompressionSessionEncodeFrame(
                guard.session,
                pbuf,
                presentation,
                duration,
                frame_props,
                ptr::null_mut(),
                &mut info_flags,
            );
            if !frame_props.is_null() {
                CFRelease(frame_props);
            }
            if encode_status != 0 {
                Err(anyhow!(
                    "VTCompressionSessionEncodeFrame failed: OSStatus {encode_status}"
                ))
            } else {
                Ok(())
            }
        };
        unsafe { CFRelease(pbuf) };
        result
    }

    #[allow(dead_code)] // only reached via Encoder::flush (shutdown / tests)
    pub(super) fn complete_frames(guard: &SessionGuard) -> Result<()> {
        // Passing an invalid CMTime tells VT "complete *all* pending frames".
        // This is the documented sentinel value for "no deadline".
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        };
        let status = unsafe { VTCompressionSessionCompleteFrames(guard.session, invalid) };
        if status != 0 {
            bail!("VTCompressionSessionCompleteFrames failed: OSStatus {status}");
        }
        Ok(())
    }

    /// VT output callback. Runs on a VT-internal thread; gets a pointer
    /// back to the `mpsc::Sender<EncodedFrame>` we stashed in
    /// `outputCallbackRefCon`. We clone the sender per delivery so its
    /// lifetime is tied to the Encoder, not the callback.
    unsafe extern "C" fn output_callback(
        output_callback_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: OSStatus,
        _info_flags: VTEncodeInfoFlags,
        sample_buffer: CMSampleBufferRef,
    ) {
        if status != 0 || sample_buffer.is_null() || output_callback_ref_con.is_null() {
            // TODO(phase-2-step-2): surface this via the channel as an
            // error variant so the EGFX bridge can react (drop session,
            // request keyframe, etc.). Silently dropping is fine for the
            // scaffold only.
            return;
        }
        let tx = &*(output_callback_ref_con as *const mpsc::Sender<EncodedFrame>);
        let Ok(frame) = extract_frame(sample_buffer) else {
            return;
        };
        let _ = tx.send(frame);
    }

    unsafe fn extract_frame(sbuf: CMSampleBufferRef) -> Result<EncodedFrame> {
        let bbuf = CMSampleBufferGetDataBuffer(sbuf);
        if bbuf.is_null() {
            bail!("CMSampleBufferGetDataBuffer returned null");
        }
        let len = CMBlockBufferGetDataLength(bbuf);
        let mut data = vec![0u8; len];
        let copy_status =
            CMBlockBufferCopyDataBytes(bbuf, 0, len, data.as_mut_ptr() as *mut c_void);
        if copy_status != 0 {
            bail!("CMBlockBufferCopyDataBytes failed: {copy_status}");
        }

        // Keyframe = the sample is NOT marked as "NotSync" in the
        // sample-attachments array. Per Apple docs, if the attachments
        // array is missing or empty, the sample is a sync (keyframe).
        let mut is_keyframe = true;
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sbuf, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let dict = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
            if !dict.is_null() {
                let not_sync = CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync);
                if !not_sync.is_null() {
                    // A present "NotSync" attachment means this is a
                    // non-keyframe. (Apple's API encodes the negation.)
                    is_keyframe = not_sync != kCFBooleanTrue;
                }
            }
        }

        // SPS/PPS come from the format description on keyframes.
        let mut parameter_sets = Vec::new();
        if is_keyframe {
            let fmt = CMSampleBufferGetFormatDescription(sbuf);
            if !fmt.is_null() {
                // Index 0 = SPS, 1 = PPS for H.264 (per Apple). We pull
                // until we run out, so the code still works if a future
                // SDK exposes more (e.g. SPS-Ext).
                let mut index = 0usize;
                loop {
                    let mut ptr_out: *const u8 = ptr::null();
                    let mut size_out: usize = 0;
                    let mut count_out: usize = 0;
                    let st = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        fmt,
                        index,
                        &mut ptr_out,
                        &mut size_out,
                        &mut count_out,
                        ptr::null_mut(),
                    );
                    if st != 0 || ptr_out.is_null() || size_out == 0 {
                        break;
                    }
                    let slice = std::slice::from_raw_parts(ptr_out, size_out);
                    parameter_sets.push(slice.to_vec());
                    index += 1;
                    if count_out > 0 && index >= count_out {
                        break;
                    }
                }
            }
        }

        let pts = CMSampleBufferGetPresentationTimeStamp(sbuf).value;
        Ok(EncodedFrame {
            data,
            is_keyframe,
            pts,
            parameter_sets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip sanity test. Creates a session, encodes a single
    /// solid-color BGRA frame, drains the channel, and asserts that
    /// the first emitted frame is a keyframe carrying SPS+PPS. Doesn't
    /// validate the bitstream content beyond "non-empty" — that needs
    /// a real H.264 decoder, which is overkill for the scaffold.
    #[test]
    fn nv12_full_range_black_and_white() {
        use super::ffi::bgra_to_nv12_full_range;
        // 2x2 frame; fill solid black, then solid white. Full-range luma should
        // hit the extremes (0 / 255) — the whole point of full-range vs the
        // 16/235 video-range floors/ceilings that wash out on mstsc.
        let w = 2usize;
        let h = 2usize;
        let stride = w * 4;
        let mut y = vec![0u8; w * h];
        let mut cbcr = vec![0u8; w]; // one 2x2 chroma sample = 2 bytes
                                     // Black (B=G=R=0).
        let black = vec![0u8; stride * h];
        bgra_to_nv12_full_range(&black, stride, w, h, &mut y, w, &mut cbcr, w);
        assert!(
            y.iter().all(|&v| v == 0),
            "full-range black luma should be 0"
        );
        assert_eq!((cbcr[0], cbcr[1]), (128, 128), "neutral chroma");
        // White (B=G=R=255).
        let white = vec![0xFFu8; stride * h];
        bgra_to_nv12_full_range(&white, stride, w, h, &mut y, w, &mut cbcr, w);
        assert!(
            y.iter().all(|&v| v == 255),
            "full-range white luma should be 255 (video-range would cap at 235)"
        );
        assert_eq!((cbcr[0], cbcr[1]), (128, 128), "neutral chroma");
    }

    /// The vImage path must produce the same full-range BT.709 NV12 as the
    /// scalar reference (validates the FFI wiring, the BGRA->ARGB permute, and
    /// the full-range matrix/range). Solid colors so 4:2:0 subsampling is exact
    /// regardless of vImage's downsample filter, isolating the color math.
    #[test]
    fn vimage_matches_scalar_full_range() {
        use super::ffi::{bgra_to_nv12_full_range, bgra_to_nv12_full_range_vimage};
        // BGRA byte order [B, G, R, A]. A red channel swap (bad permute) would
        // diverge here on the pure-red / pure-blue cases.
        let colors: [[u8; 4]; 6] = [
            [0, 0, 0, 255],       // black
            [255, 255, 255, 255], // white
            [0, 0, 255, 255],     // red
            [0, 255, 0, 255],     // green
            [255, 0, 0, 255],     // blue
            [128, 128, 128, 255], // gray
        ];
        let (w, h) = (4usize, 4usize);
        let stride = w * 4;
        for c in colors {
            let mut bgra = vec![0u8; stride * h];
            for px in bgra.chunks_exact_mut(4) {
                px.copy_from_slice(&c);
            }
            let (mut y_s, mut cbcr_s) = (vec![0u8; w * h], vec![0u8; w * (h / 2)]);
            let (mut y_v, mut cbcr_v) = (vec![0u8; w * h], vec![0u8; w * (h / 2)]);
            bgra_to_nv12_full_range(&bgra, stride, w, h, &mut y_s, w, &mut cbcr_s, w);
            bgra_to_nv12_full_range_vimage(&bgra, stride, w, h, &mut y_v, w, &mut cbcr_v, w)
                .expect("vImage conversion should succeed");
            let near = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs() <= 3;
            assert!(
                near(y_s[0], y_v[0]),
                "Y mismatch {c:?}: {} vs {}",
                y_s[0],
                y_v[0]
            );
            assert!(
                near(cbcr_s[0], cbcr_v[0]),
                "Cb mismatch {c:?}: {} vs {}",
                cbcr_s[0],
                cbcr_v[0]
            );
            assert!(
                near(cbcr_s[1], cbcr_v[1]),
                "Cr mismatch {c:?}: {} vs {}",
                cbcr_s[1],
                cbcr_v[1]
            );
        }
    }

    /// Microbenchmark: scalar vs vImage BGRA->full-range-NV12. Ignored by
    /// default; run in RELEASE (debug is ~10x slower and misleading):
    ///   cargo test --release bench_nv12_full_range -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark; run with --release -- --ignored --nocapture"]
    fn bench_nv12_full_range() {
        use super::ffi::{bgra_to_nv12_full_range, bgra_to_nv12_full_range_vimage};
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        fn time_ms(mut f: impl FnMut()) -> f64 {
            for _ in 0..5 {
                f();
            }
            let mut iters = 0u64;
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(400) {
                f();
                iters += 1;
            }
            t.elapsed().as_secs_f64() * 1000.0 / iters as f64
        }

        println!("\nBGRA->NV12 (full-range BT.709) ms/frame, single thread");
        println!(
            "{:>26}  {:>12}  {:>12}  {:>8}",
            "resolution", "scalar ms", "vImage ms", "speedup"
        );
        for &(w, h, label) in &[
            (1470usize, 956usize, "1470x956 (Air native)"),
            (1920, 1080, "1920x1080"),
            (2560, 1440, "2560x1440"),
            (3840, 2160, "3840x2160 (4K)"),
        ] {
            let stride = w * 4;
            let bgra = vec![0x80u8; stride * h];
            let mut y = vec![0u8; w * h];
            let mut cbcr = vec![0u8; w * (h / 2)];

            let scalar_ms = time_ms(|| {
                bgra_to_nv12_full_range(black_box(&bgra), stride, w, h, &mut y, w, &mut cbcr, w);
            });
            let vimage_ms = time_ms(|| {
                let _ = bgra_to_nv12_full_range_vimage(
                    black_box(&bgra),
                    stride,
                    w,
                    h,
                    &mut y,
                    w,
                    &mut cbcr,
                    w,
                );
            });
            black_box(y[0]);
            black_box(cbcr[0]);
            println!(
                "{label:>26}  {scalar_ms:12.3}  {vimage_ms:12.3}  {:7.1}x",
                scalar_ms / vimage_ms
            );
        }
    }

    /// Microbenchmark for AVC444 scoping: BGRA -> full-resolution YUV444 planar
    /// (Option A's per-frame conversion). Compares scalar reference, the
    /// 3 x vImageMatrixMultiply_ARGB8888ToPlanar8 path, AND the current
    /// production NV12 vImage path so the AVC444 overhead is directly visible.
    /// Ignored by default; run in RELEASE:
    ///   cargo test --release bench_yuv444_full_range -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark; run with --release -- --ignored --nocapture"]
    fn bench_yuv444_full_range() {
        use super::ffi::{
            bgra_to_nv12_full_range_vimage, bgra_to_yuv444_planar_scalar,
            bgra_to_yuv444_planar_vimage,
        };
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        fn time_ms(mut f: impl FnMut()) -> f64 {
            for _ in 0..5 {
                f();
            }
            let mut iters = 0u64;
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(400) {
                f();
                iters += 1;
            }
            t.elapsed().as_secs_f64() * 1000.0 / iters as f64
        }

        println!("\nBGRA conversion ms/frame, single thread (lower = better)");
        println!(
            "{:>26}  {:>10}  {:>10}  {:>10}  {:>10}",
            "resolution", "NV12 vIm", "Y444 vIm", "Y444 scal", "v444/NV12"
        );
        for &(w, h, label) in &[
            (1470usize, 956usize, "1470x956 (Air native)"),
            (1920, 1080, "1920x1080"),
            (1920, 1088, "1920x1088 (16-pad)"),
            (2560, 1440, "2560x1440"),
            (3840, 2160, "3840x2160 (4K)"),
        ] {
            let stride = w * 4;
            let bgra = vec![0x80u8; stride * h];
            let mut y = vec![0u8; w * h];
            let mut cb = vec![0u8; w * h];
            let mut cr = vec![0u8; w * h];
            let mut cbcr = vec![0u8; w * (h / 2)];

            let nv12_vim_ms = time_ms(|| {
                let _ = bgra_to_nv12_full_range_vimage(
                    black_box(&bgra),
                    stride,
                    w,
                    h,
                    &mut y,
                    w,
                    &mut cbcr,
                    w,
                );
            });
            let y444_vim_ms = time_ms(|| {
                let _ = bgra_to_yuv444_planar_vimage(
                    black_box(&bgra),
                    stride,
                    w,
                    h,
                    &mut y,
                    w,
                    &mut cb,
                    w,
                    &mut cr,
                    w,
                );
            });
            let y444_scal_ms = time_ms(|| {
                bgra_to_yuv444_planar_scalar(
                    black_box(&bgra),
                    stride,
                    w,
                    h,
                    &mut y,
                    w,
                    &mut cb,
                    w,
                    &mut cr,
                    w,
                );
            });
            black_box(y[0]);
            black_box(cb[0]);
            black_box(cr[0]);
            black_box(cbcr[0]);
            println!(
                "{label:>26}  {nv12_vim_ms:10.3}  {y444_vim_ms:10.3}  {y444_scal_ms:10.3}  {:9.2}x",
                y444_vim_ms / nv12_vim_ms
            );
        }
    }

    /// AVC444 scoping: does VideoToolbox actually run two concurrent
    /// `VTCompressionSession`s in parallel on the dedicated hardware encoder,
    /// or does it serialize them at the hardware-block level? Submits N frames
    /// to one session as a baseline, then N frames to each of two sessions
    /// concurrently. Reports throughput ratio.
    ///
    /// Interpretation:
    ///   ratio ≈ 1.0  → VT serializes (AVC444 effectively doubles encode time
    ///                  per output frame; total per-frame budget tightens)
    ///   ratio ≈ 2.0  → VT parallelizes (AVC444 has no extra encode latency
    ///                  beyond the conversion overhead measured above)
    ///   1.0 < x < 2.0 → partial overlap (some pipeline stages share)
    ///
    /// Ignored by default; run in RELEASE:
    ///   cargo test --release bench_dual_vt_sessions -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark; run with --release -- --ignored --nocapture"]
    fn bench_dual_vt_sessions() {
        use std::hint::black_box;
        use std::time::Instant;

        const W: u16 = 1920;
        const H: u16 = 1088;
        const FPS: u32 = 60;
        const BITRATE_BPS: u32 = 6_000_000;
        const KEYFRAME_S: f32 = 2.0;
        const N_FRAMES: usize = 60;

        let stride = usize::from(W) * 4;
        // Diagonal gradient — non-uniform content so the encoder actually does
        // motion-estimation work instead of short-circuiting on flat samples.
        let mut frame = vec![0u8; stride * usize::from(H)];
        for y in 0..usize::from(H) {
            for x in 0..usize::from(W) {
                let p = &mut frame[y * stride + x * 4..][..4];
                p[0] = ((x + y) & 0xff) as u8; // B
                p[1] = ((x ^ y) & 0xff) as u8; // G
                p[2] = ((x.wrapping_mul(7) + y) & 0xff) as u8; // R
                p[3] = 0xff;
            }
        }

        let drive_one = |enc: &mut Encoder| {
            for i in 0..N_FRAMES {
                enc.encode_bgra(black_box(&frame), stride, i == 0).unwrap();
            }
            let _ = enc.flush().unwrap();
        };

        // Warm up VT (first session boot is expensive; we don't want it skewing
        // the baseline).
        {
            let mut warm = Encoder::new(W, H, FPS, BITRATE_BPS, KEYFRAME_S).unwrap();
            drive_one(&mut warm);
        }

        // Baseline: single session, N frames.
        let mut e1 = Encoder::new(W, H, FPS, BITRATE_BPS, KEYFRAME_S).unwrap();
        let t = Instant::now();
        drive_one(&mut e1);
        let t_single = t.elapsed();

        // Dual: two sessions, N frames each, submissions interleaved, then both
        // flushed. If VT parallelizes, e_b's frames overlap with e_a's flush
        // wait and the second flush returns nearly instantly.
        let mut e_a = Encoder::new(W, H, FPS, BITRATE_BPS, KEYFRAME_S).unwrap();
        let mut e_b = Encoder::new(W, H, FPS, BITRATE_BPS, KEYFRAME_S).unwrap();
        let t = Instant::now();
        for i in 0..N_FRAMES {
            e_a.encode_bgra(black_box(&frame), stride, i == 0).unwrap();
            e_b.encode_bgra(black_box(&frame), stride, i == 0).unwrap();
        }
        let _ = e_a.flush().unwrap();
        let _ = e_b.flush().unwrap();
        let t_dual = t.elapsed();

        let single_fps = N_FRAMES as f64 / t_single.as_secs_f64();
        let dual_fps = (2 * N_FRAMES) as f64 / t_dual.as_secs_f64();
        let ratio = dual_fps / single_fps;

        println!("\nDual VTCompressionSession parallelism ({W}x{H}, {N_FRAMES} frames, {FPS}fps target, {} Mbps):", BITRATE_BPS / 1_000_000);
        println!(
            "  single session:  {:7.1} ms total → {:6.1} fps ({:5.2} ms/frame)",
            t_single.as_secs_f64() * 1000.0,
            single_fps,
            t_single.as_secs_f64() * 1000.0 / N_FRAMES as f64,
        );
        println!(
            "  dual sessions:   {:7.1} ms total → {:6.1} fps ({:5.2} ms/frame each, 2x frames)",
            t_dual.as_secs_f64() * 1000.0,
            dual_fps,
            t_dual.as_secs_f64() * 1000.0 / (2 * N_FRAMES) as f64,
        );
        println!(
            "  parallelism ratio: {ratio:.2}x  (1.0 = VT serializes; 2.0 = perfect parallelism)",
        );
    }

    /// Sanity check: vImage YUV444 matrix output must match the scalar
    /// reference on solid-color frames. Validates the Q14 matrix coefficients
    /// and BGRA byte-order assumption before we trust the benchmark numbers.
    #[test]
    fn vimage_yuv444_matches_scalar() {
        use super::ffi::{bgra_to_yuv444_planar_scalar, bgra_to_yuv444_planar_vimage};
        let colors: [[u8; 4]; 6] = [
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            [0, 0, 255, 255],
            [0, 255, 0, 255],
            [255, 0, 0, 255],
            [128, 128, 128, 255],
        ];
        let (w, h) = (4usize, 4usize);
        let stride = w * 4;
        for c in colors {
            let mut bgra = vec![0u8; stride * h];
            for px in bgra.chunks_exact_mut(4) {
                px.copy_from_slice(&c);
            }
            let mut y_s = vec![0u8; w * h];
            let mut cb_s = vec![0u8; w * h];
            let mut cr_s = vec![0u8; w * h];
            let mut y_v = vec![0u8; w * h];
            let mut cb_v = vec![0u8; w * h];
            let mut cr_v = vec![0u8; w * h];
            bgra_to_yuv444_planar_scalar(
                &bgra, stride, w, h, &mut y_s, w, &mut cb_s, w, &mut cr_s, w,
            );
            bgra_to_yuv444_planar_vimage(
                &bgra, stride, w, h, &mut y_v, w, &mut cb_v, w, &mut cr_v, w,
            )
            .expect("vImage YUV444 conversion should succeed");
            let near = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs() <= 3;
            assert!(
                near(y_s[0], y_v[0]),
                "Y mismatch {c:?}: {} vs {}",
                y_s[0],
                y_v[0]
            );
            assert!(
                near(cb_s[0], cb_v[0]),
                "Cb mismatch {c:?}: {} vs {}",
                cb_s[0],
                cb_v[0]
            );
            assert!(
                near(cr_s[0], cr_v[0]),
                "Cr mismatch {c:?}: {} vs {}",
                cr_s[0],
                cr_v[0]
            );
        }
    }

    #[test]
    fn encodes_a_keyframe() -> Result<()> {
        let w: u16 = 320;
        let h: u16 = 240;
        let stride = usize::from(w) * 4;
        let mut frame = vec![0u8; stride * usize::from(h)];
        for px in frame.chunks_exact_mut(4) {
            px[0] = 0x33; // B
            px[1] = 0x77; // G
            px[2] = 0xcc; // R
            px[3] = 0xff; // A
        }

        let mut enc = Encoder::new(w, h, 30, 4_000_000, 5.0)?;
        enc.encode_bgra(&frame, stride, false)?;
        let frames = enc.flush()?;
        assert!(!frames.is_empty(), "expected at least one encoded frame");
        let first = &frames[0];
        assert!(first.is_keyframe, "first frame should be a keyframe");
        assert!(
            !first.data.is_empty(),
            "encoded payload should be non-empty"
        );
        assert!(
            !first.parameter_sets.is_empty(),
            "keyframe should carry SPS/PPS parameter sets"
        );
        Ok(())
    }
}
