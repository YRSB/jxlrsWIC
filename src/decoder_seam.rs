//! Pure-Rust decoding seam for JPEG XL, free of COM, Win32, `IStream`, `HRESULT` and GUID types.
//!
//! The COM layer in [`crate::decoder`] owns COM lifecycle, parameter marshalling and `HRESULT`
//! translation only. Bitstream parsing, pixel format selection, colour profile extraction and
//! raster storage live behind the concrete [`JxlContainer`] and zero-allocation borrowed
//! [`JxlFrame`] types defined here, so the whole decode pipeline can be exercised with plain
//! in-memory buffers.

use std::borrow::Cow;
use std::sync::OnceLock;

use jxl::api::{
    Endianness, Event, JxlBitDepth, JxlColorEncoding, JxlColorProfile, JxlColorType, JxlDataFormat,
    JxlDecoder, JxlDecoderOptions, JxlOutputBuffer, JxlPixelFormat, JxlTransferFunction,
    VisibleFrameSeekTarget,
};

/// Largest accepted image edge. Larger dimensions are rejected before any pixel allocation.
pub const MAX_DIMENSION: u32 = 16_384;

/// Largest accepted input stream, in bytes. Guards host processes against bitstream bombs.
pub const MAX_STREAM_BYTES: usize = 256 * 1024 * 1024;

/// Largest accepted visible frame count. Bounds scan state for pathological bitstreams that
/// declare thousands of frames; each lazily decoded raster is still bounded by
/// [`MAX_DIMENSION`], so multi-frame memory only grows with frames the host actually touches.
pub const MAX_FRAMES: usize = 4_096;

/// Default resolution reported for images whose bitstream carries no physical pixel size.
const DEFAULT_DPI: f64 = 72.0;

/// Signature bytes of an ISO BMFF (`JXL `) container box header.
const CONTAINER_SIGNATURE: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
];

/// Signature bytes of a bare JPEG XL codestream.
const CODESTREAM_SIGNATURE: [u8; 2] = [0xFF, 0x0A];

/// Returns `true` when `bytes` starts with either the ISO BMFF container signature or the bare
/// codestream signature.
pub fn is_jpeg_xl(bytes: &[u8]) -> bool {
    bytes.starts_with(&CONTAINER_SIGNATURE) || bytes.starts_with(&CODESTREAM_SIGNATURE)
}

/// Decoded container metadata readout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Number of decodable (visible) frames. Still images report 1.
    pub frame_count: usize,
}

/// Animation header of the container, copied verbatim from `JxlBasicInfo::animation`.
///
/// [`JxlContainer::animation`] returns `None` for still images; the COM layer then keeps the
/// legacy still-image behaviour (no duration or loop metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationInfo {
    /// Loop count; 0 means loop forever, matching the GIF NETSCAPE extension semantics.
    pub num_loops: u32,
    /// Ticks per second, numerator.
    pub tps_numerator: u32,
    /// Ticks per second, denominator.
    pub tps_denominator: u32,
    /// Whether the bitstream carries timecodes.
    pub have_timecodes: bool,
}

impl AnimationInfo {
    /// Ticks per second as a float; 0.0 when the header carries no usable rate.
    pub fn ticks_per_second(&self) -> f64 {
        if self.tps_numerator == 0 || self.tps_denominator == 0 {
            0.0
        } else {
            self.tps_numerator as f64 / self.tps_denominator as f64
        }
    }
}

/// Per-frame playback timing, copied verbatim from the upstream scan record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameTiming {
    /// Frame duration in animation ticks.
    pub duration_ticks: u32,
    /// Frame duration in milliseconds; 0.0 for still images and the last frame.
    pub duration_ms: f64,
    /// Whether decoding can start directly at this frame with no earlier decoder state.
    pub is_keyframe: bool,
    /// Whether this is the last visible frame.
    pub is_last: bool,
}

/// Everything the COM layer needs to answer `IWICBitmapSource` queries for a frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameInfo {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Horizontal resolution in dots per inch.
    pub dpi_x: f64,
    /// Vertical resolution in dots per inch.
    pub dpi_y: f64,
    /// Pixel format the frame reports through `GetPixelFormat`.
    pub pixel_format: WicPixelFormat,
}

/// Pixel formats exposed by the decoder, named after their WIC counterparts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WicPixelFormat {
    /// `GUID_WICPixelFormat32bppBGRA`, straight alpha.
    Bgra32,
    /// `GUID_WICPixelFormat64bppRGBA`, 16-bit unsigned integer channels.
    Rgba64,
    /// `GUID_WICPixelFormat64bppRGBAHalf`, IEEE 754 binary16 channels.
    RgbaHalf64,
}

impl WicPixelFormat {
    /// Bytes occupied by a single pixel in this format.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Bgra32 => 4,
            Self::Rgba64 | Self::RgbaHalf64 => 8,
        }
    }

    /// Bytes required for `width` pixels of this format.
    pub fn row_bytes(self, width: u32) -> usize {
        self.bytes_per_pixel() * width as usize
    }
}

/// Requested sub-rectangle of a frame, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Rectangle width; zero means "up to the right edge".
    pub width: u32,
    /// Rectangle height; zero means "up to the bottom edge".
    pub height: u32,
}

impl Rect {
    /// Builds a rectangle. Width and height of zero are interpreted as "to the edge".
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Resolves this (possibly zero-sized) rectangle against a frame of `frame_width` by
    /// `frame_height` pixels. Returns `None` when the rectangle is empty or escapes the frame.
    pub fn resolve(self, frame_width: u32, frame_height: u32) -> Option<Rect> {
        if self.x >= frame_width
            || self.y >= frame_height
            || self.width > frame_width - self.x
            || self.height > frame_height - self.y
        {
            return None;
        }
        let width = if self.width == 0 {
            frame_width - self.x
        } else {
            self.width
        };
        let height = if self.height == 0 {
            frame_height - self.y
        } else {
            self.height
        };
        Some(Rect {
            x: self.x,
            y: self.y,
            width,
            height,
        })
    }
}

/// Decoding failures, translated to WIC `HRESULT`s by the COM layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The input stream could not be read.
    #[error("stream read or ingestion error: {0}")]
    StreamError(String),
    /// The image is larger than [`MAX_DIMENSION`] on at least one edge.
    #[error("image dimensions exceed the safe limit of {MAX_DIMENSION}x{MAX_DIMENSION}")]
    DimensionTooLarge,
    /// The bitstream is not JPEG XL, or is corrupt.
    #[error("bitstream unsupported or corrupt: {0}")]
    InvalidBitstream(String),
    /// The requested rectangle is empty or escapes the frame bounds.
    #[error("copy rectangle is outside the frame bounds")]
    InvalidRect,
    /// The destination row stride is smaller than one packed row.
    #[error("row stride {stride} is smaller than the required minimum {min_stride}")]
    InvalidStride {
        /// Stride supplied by the caller.
        stride: usize,
        /// Minimum stride that would have sufficed.
        min_stride: usize,
    },
    /// The destination buffer cannot hold `height` rows of `stride` bytes.
    #[error("destination buffer holds {provided} bytes but {required} were required")]
    InsufficientBuffer {
        /// Bytes supplied by the caller.
        provided: usize,
        /// Bytes required for the requested rectangle and stride.
        required: usize,
    },
    /// The requested frame index does not exist.
    #[error("frame index {index} is out of range (frame count {frame_count})")]
    FrameIndexOutOfRange {
        /// Requested index.
        index: usize,
        /// Number of frames in the container.
        frame_count: usize,
    },
    /// A parser panic occurred while decoding the bitstream.
    #[error("bitstream parser panicked: {0}")]
    ParserPanic(String),
}

/// Resolved `CopyPixels` geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPlan {
    /// Rectangle that will actually be read, clipped to the frame.
    pub resolved: Rect,
    /// Destination row stride in bytes.
    pub stride: usize,
}

/// Owned decoded raster, stored lazily inside a [`JxlContainer`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedRaster {
    width: u32,
    height: u32,
    format: WicPixelFormat,
    bytes: Vec<u8>,
}

impl CachedRaster {
    fn new(width: u32, height: u32, format: WicPixelFormat, bytes: Vec<u8>) -> Self {
        Self {
            width,
            height,
            format,
            bytes,
        }
    }

    #[cfg(test)]
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn plan(
        &self,
        rect: Option<Rect>,
        stride: usize,
        buffer_len: usize,
    ) -> Result<CopyPlan, DecodeError> {
        let resolved = match rect {
            Some(rect) => rect
                .resolve(self.width, self.height)
                .ok_or(DecodeError::InvalidRect)?,
            None => Rect::new(0, 0, self.width, self.height),
        };
        let min_stride = self.format.row_bytes(resolved.width);
        if stride < min_stride {
            return Err(DecodeError::InvalidStride { stride, min_stride });
        }
        let required = stride.checked_mul(resolved.height as usize).ok_or(
            DecodeError::InsufficientBuffer {
                provided: buffer_len,
                required: usize::MAX,
            },
        )?;
        if buffer_len < required {
            return Err(DecodeError::InsufficientBuffer {
                provided: buffer_len,
                required,
            });
        }
        Ok(CopyPlan { resolved, stride })
    }

    fn blit(&self, rect: Option<Rect>, stride: usize, out: &mut [u8]) -> Result<(), DecodeError> {
        let plan = self.plan(rect, stride, out.len())?;
        let src_stride = self.format.row_bytes(self.width);
        let row_len = self.format.row_bytes(plan.resolved.width);

        let bytes_per_pixel = self.format.bytes_per_pixel();
        for (row, dest_row) in out
            .chunks_mut(stride)
            .take(plan.resolved.height as usize)
            .enumerate()
        {
            let src_start = (plan.resolved.y as usize + row) * src_stride
                + plan.resolved.x as usize * bytes_per_pixel;
            let src_end = src_start + row_len;
            match (
                self.bytes.get(src_start..src_end),
                dest_row.get_mut(..row_len),
            ) {
                (Some(src_row), Some(dest_row)) => dest_row.copy_from_slice(src_row),
                _ => return Err(DecodeError::InvalidRect),
            }
        }
        Ok(())
    }
}

/// Byte storage aligned for 16-bit and 32-bit channel formats, as required by `jxl-rs`.
struct AlignedRaster {
    words: Vec<u64>,
    len: usize,
}

impl AlignedRaster {
    fn zeroed(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(8)],
            len,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `words` owns at least `len` bytes of valid, 8-byte aligned memory.
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<u8>(), self.len) }
    }
}

/// Everything known about a Frame Decode before its pixels are needed.
#[derive(Debug, Clone)]
struct FrameReadout {
    info: FrameInfo,
    color_profile: Option<Box<[u8]>>,
    timing: FrameTiming,
    /// Scan checkpoint for a targeted decode. `None` only for frame 0 of a still image, which
    /// keeps the legacy sequential decode path bit-for-bit.
    seek_target: Option<VisibleFrameSeekTarget>,
}

/// One visible frame discovered by the container scan: playback timing plus the checkpoint a
/// targeted decode seeks to.
#[derive(Debug, Clone, Copy)]
struct ScannedFrame {
    timing: FrameTiming,
    seek_target: VisibleFrameSeekTarget,
}

/// A decoded JPEG XL image container.
///
/// Encapsulates container and frame metadata, bitstream bytes, and lazily decoded per-frame
/// in-memory [`CachedRaster`]s behind a concrete pure-Rust interface free of COM and Win32 types.
/// Each frame decodes on first use and is retained afterwards; still images (one visible frame,
/// no animation header) decode frame 0 through the original sequential path.
#[derive(Debug)]
pub struct JxlContainer {
    info: ContainerInfo,
    animation: Option<AnimationInfo>,
    bitstream: Box<[u8]>,
    frames: Box<[FrameReadout]>,
    rasters: Vec<OnceLock<Result<CachedRaster, DecodeError>>>,
}

/// Alias for [`JxlContainer`].
pub type JxlImage = JxlContainer;

/// A zero-allocation borrowed view of a decodable frame in a [`JxlContainer`].
#[derive(Debug, Clone, Copy)]
pub struct JxlFrame<'a> {
    container: &'a JxlContainer,
    index: usize,
}

impl JxlContainer {
    /// Reads the image header from `bytes` and builds the container without decoding pixels.
    pub fn open(bytes: &[u8]) -> Result<Self, DecodeError> {
        open(bytes)
    }

    /// Returns `true` when `bytes` starts with either the ISO BMFF container signature
    /// or the bare codestream signature.
    pub fn is_jpeg_xl(bytes: &[u8]) -> bool {
        is_jpeg_xl(bytes)
    }

    /// Container metadata, available immediately after construction.
    pub fn info(&self) -> &ContainerInfo {
        &self.info
    }

    /// Number of decodable frames in the container.
    pub fn frame_count(&self) -> usize {
        self.info.frame_count
    }

    /// Borrows the frame at `index`.
    pub fn frame(&self, index: usize) -> Result<JxlFrame<'_>, DecodeError> {
        if index >= self.info.frame_count {
            return Err(DecodeError::FrameIndexOutOfRange {
                index,
                frame_count: self.info.frame_count,
            });
        }
        Ok(JxlFrame {
            container: self,
            index,
        })
    }

    /// Animation header, or `None` for still images.
    pub fn animation(&self) -> Option<&AnimationInfo> {
        self.animation.as_ref()
    }

    /// Frame metadata for the primary frame.
    pub fn frame_info(&self) -> &FrameInfo {
        &self.frames[0].info
    }

    /// ICC profile bytes for the primary frame, if present.
    pub fn color_profile(&self) -> Option<&[u8]> {
        self.frames[0].color_profile.as_deref()
    }

    /// Copies the requested region of the primary frame into `out`.
    pub fn copy_pixels(
        &self,
        rect: Option<Rect>,
        stride: usize,
        out: &mut [u8],
    ) -> Result<(), DecodeError> {
        self.frame_raster(0)?.blit(rect, stride, out)
    }

    /// Decodes (once per frame) and returns a reference to the full raster of frame `index`.
    fn frame_raster(&self, index: usize) -> Result<&CachedRaster, DecodeError> {
        let readout = self.frames.get(index).ok_or(DecodeError::FrameIndexOutOfRange {
            index,
            frame_count: self.info.frame_count,
        })?;
        let slot = self.rasters.get(index).ok_or(DecodeError::FrameIndexOutOfRange {
            index,
            frame_count: self.info.frame_count,
        })?;
        match slot.get_or_init(|| decode_raster_indexed(&self.bitstream, readout)) {
            Ok(raster) => Ok(raster),
            Err(err) => Err(err.clone()),
        }
    }
}

impl<'a> JxlFrame<'a> {
    /// Zero-based index of this frame among the container's visible frames.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Playback timing of this frame, from the container scan.
    pub fn timing(&self) -> &'a FrameTiming {
        &self.container.frames[self.index].timing
    }

    /// Frame metadata, including dimensions and pixel format.
    pub fn info(&self) -> &'a FrameInfo {
        &self.container.frames[self.index].info
    }

    /// ICC color profile bytes for this frame, extracted verbatim or synthesized from CICP.
    pub fn color_profile(&self) -> Option<&'a [u8]> {
        self.container.frames[self.index].color_profile.as_deref()
    }

    /// Copies the requested region into `out`, honouring an arbitrary destination row stride.
    pub fn copy_pixels(
        &self,
        rect: Option<Rect>,
        stride: usize,
        out: &mut [u8],
    ) -> Result<(), DecodeError> {
        self.container.frame_raster(self.index)?.blit(rect, stride, out)
    }
}

/// Reads the image header of `bytes` and builds the container seam without decoding pixels.
///
/// Parsing is protected by panic containment so that malformed bitstreams triggering a panic
/// in `jxl-rs` return [`DecodeError::ParserPanic`] instead of unwinding into callers.
pub fn open(bytes: &[u8]) -> Result<JxlContainer, DecodeError> {
    if !is_jpeg_xl(bytes) {
        return Err(DecodeError::InvalidBitstream(
            "missing JPEG XL container or codestream signature".to_string(),
        ));
    }

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open_impl(bytes))) {
        Ok(result) => result,
        Err(payload) => Err(DecodeError::ParserPanic(panic_message(&*payload))),
    }
}

#[allow(clippy::field_reassign_with_default)]
fn open_impl(bytes: &[u8]) -> Result<JxlContainer, DecodeError> {
    let mut scan_options = JxlDecoderOptions::default();
    scan_options.scan_frames_only = true;
    let mut scanner = JxlDecoder::new(scan_options);
    let mut input = bytes;

    // The scan parses headers and TOCs only (`scan_frames_only` skips section decoding), so
    // `open` stays pixel-free while learning the true visible frame count up front.
    let mut header: Option<(jxl::api::JxlBasicInfo, Option<Box<[u8]>>, WicPixelFormat)> = None;
    loop {
        match scanner
            .process(&mut input, None, None)
            .map_err(map_jxl_error)?
        {
            Event::BasicInfo => {
                if header.is_none() {
                    let basic_info = scanner.basic_info().cloned().ok_or_else(|| {
                        DecodeError::InvalidBitstream("missing basic info".into())
                    })?;
                    let (width, height) = basic_info.size;
                    validate_dimensions(width, height)?;
                    let embedded_profile = scanner.embedded_color_profile();
                    let format = select_format(&basic_info.bit_depth, embedded_profile);
                    let color_profile = extract_color_profile(embedded_profile, &basic_info);
                    header = Some((basic_info, color_profile, format));
                }
            }
            Event::Complete => break,
            Event::NeedMoreInput { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "truncated before the end of the frame headers".to_string(),
                ));
            }
            _ => continue,
        }
    }

    let (basic_info, color_profile, format) =
        header.ok_or_else(|| DecodeError::InvalidBitstream("missing basic info".into()))?;

    let scans: Vec<ScannedFrame> = scanner
        .scanned_frames()
        .iter()
        .map(|visible| ScannedFrame {
            timing: FrameTiming {
                duration_ticks: visible.duration_ticks,
                duration_ms: visible.duration_ms,
                is_keyframe: visible.is_keyframe,
                is_last: visible.is_last,
            },
            seek_target: visible.seek_target,
        })
        .collect();
    if scans.is_empty() {
        return Err(DecodeError::InvalidBitstream(
            "image contains no visible frames".to_string(),
        ));
    }
    if scans.len() > MAX_FRAMES {
        return Err(DecodeError::InvalidBitstream(format!(
            "frame count {} exceeds the scan limit of {MAX_FRAMES}",
            scans.len()
        )));
    }

    let animation = basic_info.animation.clone().map(|upstream| AnimationInfo {
        num_loops: upstream.num_loops,
        tps_numerator: upstream.tps_numerator,
        tps_denominator: upstream.tps_denominator,
        have_timecodes: upstream.have_timecodes,
    });

    let (width, height) = basic_info.size;
    let info = FrameInfo {
        width: width as u32,
        height: height as u32,
        dpi_x: DEFAULT_DPI,
        dpi_y: DEFAULT_DPI,
        pixel_format: format,
    };
    // Frame 0 of a still image (one visible frame, no animation header) keeps the legacy
    // sequential decode; every other frame seeks directly to its decode start.
    let sequential_first = animation.is_none() && scans.len() == 1;
    let frames: Box<[FrameReadout]> = scans
        .iter()
        .enumerate()
        .map(|(index, scan)| FrameReadout {
            info: info.clone(),
            color_profile: color_profile.clone(),
            timing: scan.timing,
            seek_target: if sequential_first && index == 0 {
                None
            } else {
                Some(scan.seek_target)
            },
        })
        .collect();
    let rasters = (0..frames.len()).map(|_| OnceLock::new()).collect();

    Ok(JxlContainer {
        info: ContainerInfo {
            frame_count: frames.len(),
        },
        animation,
        bitstream: bytes.to_vec().into_boxed_slice(),
        frames,
        rasters,
    })
}

/// Rejects empty images and images beyond the [`MAX_DIMENSION`] decompression-bomb ceiling.
fn validate_dimensions(width: usize, height: usize) -> Result<(), DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::InvalidBitstream(
            "image has a zero dimension".to_string(),
        ));
    }
    if width > MAX_DIMENSION as usize || height > MAX_DIMENSION as usize {
        return Err(DecodeError::DimensionTooLarge);
    }
    Ok(())
}

/// How `jxl-rs` is asked to lay out a frame for a given WIC pixel format.
#[derive(Clone, Copy, Debug)]
struct OutputLayout {
    /// Colour type requested from `jxl-rs`.
    color_type: JxlColorType,
    /// Data format requested from `jxl-rs`.
    data_format: JxlDataFormat,
    /// Bytes per pixel of the buffer `jxl-rs` writes.
    staging_bytes_per_pixel: usize,
}

impl OutputLayout {
    fn for_format(format: WicPixelFormat) -> Self {
        let native_f32 = JxlDataFormat::F32 {
            endianness: Endianness::native(),
        };
        match format {
            WicPixelFormat::Bgra32 => Self {
                color_type: JxlColorType::Bgra,
                data_format: JxlDataFormat::U8 { bit_depth: 8 },
                staging_bytes_per_pixel: 4,
            },
            WicPixelFormat::Rgba64 => Self {
                color_type: JxlColorType::Rgba,
                data_format: JxlDataFormat::U16 {
                    endianness: Endianness::native(),
                    bit_depth: 16,
                },
                staging_bytes_per_pixel: 8,
            },
            // Staged as four native `f32` samples and narrowed by the seam afterwards.
            WicPixelFormat::RgbaHalf64 => Self {
                color_type: JxlColorType::Rgba,
                data_format: native_f32,
                staging_bytes_per_pixel: 16,
            },
        }
    }

    fn pixel_format(&self, extra_channel_count: usize) -> JxlPixelFormat {
        JxlPixelFormat {
            color_type: self.color_type,
            color_data_format: Some(self.data_format),
            extra_channel_format: vec![None; extra_channel_count],
        }
    }
}

/// Picks the WIC pixel format that preserves the source bit depth and HDR transfer.
fn select_format(bit_depth: &JxlBitDepth, profile: Option<&JxlColorProfile>) -> WicPixelFormat {
    match bit_depth {
        JxlBitDepth::Float { .. } => WicPixelFormat::RgbaHalf64,
        JxlBitDepth::Int { bits_per_sample } if is_hdr_transfer(profile) => {
            WicPixelFormat::RgbaHalf64
        }
        JxlBitDepth::Int { bits_per_sample } => {
            if *bits_per_sample <= 8 {
                WicPixelFormat::Bgra32
            } else {
                WicPixelFormat::Rgba64
            }
        }
    }
}

/// Returns `true` when the colour profile uses the PQ or HLG transfer function.
fn is_hdr_transfer(profile: Option<&JxlColorProfile>) -> bool {
    match profile {
        Some(JxlColorProfile::Simple(JxlColorEncoding::RgbColorSpace {
            transfer_function,
            ..
        })) => matches!(
            transfer_function,
            JxlTransferFunction::PQ | JxlTransferFunction::HLG
        ),
        _ => false,
    }
}

/// Returns the embedded ICC profile, or synthesizes one from the parametric colour encoding.
fn extract_color_profile(
    profile: Option<&JxlColorProfile>,
    info: &jxl::api::JxlBasicInfo,
) -> Option<Box<[u8]>> {
    let intensity_target = info.tone_mapping.intensity_target;
    match profile?.try_as_icc(intensity_target)? {
        Cow::Borrowed(bytes) => Some(bytes.to_vec().into_boxed_slice()),
        Cow::Owned(bytes) => Some(bytes.into_boxed_slice()),
    }
}

/// Narrows interleaved `f32` samples to little-endian IEEE 754 binary16.
fn narrow_to_half(source: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(source.len() / 2);
    for sample in source.as_chunks::<4>().0 {
        out.extend_from_slice(&half_from_f32(f32::from_ne_bytes(*sample)).to_le_bytes());
    }
    out
}

/// Converts an `f32` to IEEE 754 binary16 (round-to-nearest-even, saturating to infinity).
fn half_from_f32(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xFF) as i32;
    let mantissa = bits & 0x7F_FFFF;

    if exponent == 0xFF {
        // Infinity or NaN: keep the sign, force the largest half exponent.
        let payload = if mantissa == 0 { 0 } else { 0x200 };
        return sign | 0x7C00 | payload;
    }

    let unbiased = exponent - 127;
    if unbiased > 15 {
        return sign | 0x7C00;
    }
    if unbiased < -14 {
        // Subnormal half: a half subnormal is `n * 2^-24`, and the f32 value is
        // `significand * 2^(unbiased - 23)`, so `n = significand >> (-1 - unbiased)`.
        let shift = (-1 - unbiased) as u32;
        if shift > 23 {
            return sign;
        }
        let significand = mantissa | 0x80_0000;
        let half = (significand >> shift) as u16;
        return sign | half;
    }

    let half_exponent = ((unbiased + 15) as u16) << 10;
    let half_mantissa = (mantissa >> 13) as u16;
    sign | half_exponent | half_mantissa
}

/// Runs a full decode of the frame described by `readout`, wrapped in panic containment.
fn decode_raster_indexed(
    bytes: &[u8],
    readout: &FrameReadout,
) -> Result<CachedRaster, DecodeError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_raster_indexed_impl(bytes, readout)
    })) {
        Ok(result) => result,
        Err(payload) => Err(DecodeError::ParserPanic(panic_message(&*payload))),
    }
}

fn decode_raster_indexed_impl(
    bytes: &[u8],
    readout: &FrameReadout,
) -> Result<CachedRaster, DecodeError> {
    let info = &readout.info;
    match readout.seek_target {
        // Still images keep the original sequential decode bit-for-bit.
        None => decode_raster_impl(bytes, info.width, info.height, info.pixel_format),
        Some(seek_target) => decode_seek_raster_impl(
            bytes,
            info.width,
            info.height,
            info.pixel_format,
            seek_target,
        ),
    }
}

/// Decodes one visible frame by seeking a fresh decoder to the frame's decode start.
///
/// The seek target comes from the container scan, so dependencies (reference frames, LF
/// frames, patches) are reconstructed exactly as in a sequential decode; frames the target
/// does not need are skipped without notification and produce no events.
#[allow(clippy::field_reassign_with_default)]
fn decode_seek_raster_impl(
    bytes: &[u8],
    width: u32,
    height: u32,
    format: WicPixelFormat,
    seek_target: VisibleFrameSeekTarget,
) -> Result<CachedRaster, DecodeError> {
    let mut options = JxlDecoderOptions::default();
    options.alpha_mode = jxl::api::AlphaMode::Unpremultiplied;
    let mut decoder = JxlDecoder::new(options);
    // One runner per decode, as before: `jxl-rs` only parallelises the frame rendering pass.
    let mut runner = crate::parallel_runner::ThreadRunner::new();
    let mut input = bytes;

    // Image header only, then seek before any frame header is parsed. `BasicInfo` always
    // precedes frame headers, so any other event here just keeps the loop moving.
    let layout = OutputLayout::for_format(format);
    loop {
        match decoder
            .process(&mut input, None, Some(&mut runner))
            .map_err(map_jxl_error)?
        {
            Event::BasicInfo => {
                let extra_channels = decoder
                    .basic_info()
                    .map_or(0, |info| info.extra_channels.len());
                let pixel_format = layout.pixel_format(extra_channels);
                decoder
                    .set_pixel_format(pixel_format)
                    .map_err(map_jxl_error)?;
                break;
            }
            Event::NeedMoreInput { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "truncated before the image header".to_string(),
                ));
            }
            Event::Complete => {
                return Err(DecodeError::InvalidBitstream(
                    "no visible frame to decode".to_string(),
                ));
            }
            _ => continue,
        }
    }

    let offset = decoder
        .start_new_frame(seek_target)
        .map_err(map_jxl_error)?;
    let mut input: &[u8] = bytes
        .get(offset as usize..)
        .ok_or_else(|| {
            DecodeError::InvalidBitstream(
                "frame seek offset is beyond the end of input".to_string(),
            )
        })?;

    // Skipped frames stay silent, so the first `FrameHeader` after the seek is the target.
    loop {
        match decoder
            .process(&mut input, None, Some(&mut runner))
            .map_err(map_jxl_error)?
        {
            Event::FrameHeader => break,
            Event::FrameComplete { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "frame completed before its header".to_string(),
                ));
            }
            Event::Complete => {
                return Err(DecodeError::InvalidBitstream(
                    "no visible frame to decode".to_string(),
                ));
            }
            Event::NeedMoreInput { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "truncated frame data".to_string(),
                ));
            }
            _ => continue,
        }
    }

    let decode_row_bytes = layout.staging_bytes_per_pixel * width as usize;
    let mut raster = AlignedRaster::zeroed(decode_row_bytes * height as usize);

    let mut buffers = [JxlOutputBuffer::new(
        raster.as_mut_slice(),
        height as usize,
        decode_row_bytes,
    )];

    loop {
        match decoder
            .process(&mut input, Some(&mut buffers), Some(&mut runner))
            .map_err(map_jxl_error)?
        {
            Event::FrameComplete { .. } | Event::Complete => break,
            Event::NeedMoreInput { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "truncated frame data".to_string(),
                ));
            }
            _ => continue,
        }
    }

    let decoded = raster.as_mut_slice();
    let pixels = if format == WicPixelFormat::RgbaHalf64 {
        narrow_to_half(decoded)
    } else {
        decoded.to_vec()
    };

    Ok(CachedRaster::new(width, height, format, pixels))
}

#[allow(clippy::field_reassign_with_default)]
fn decode_raster_impl(
    bytes: &[u8],
    width: u32,
    height: u32,
    format: WicPixelFormat,
) -> Result<CachedRaster, DecodeError> {
    let mut options = JxlDecoderOptions::default();
    options.alpha_mode = jxl::api::AlphaMode::Unpremultiplied;
    // The dimension ceiling already bounds worst-case memory; `sample_limit` would be stricter
    // than WIC expects for wide-gamut images with several extra channels, so leave it unset.
    let mut decoder = JxlDecoder::new(options);

    // One runner per decode, shared by both passes: `jxl-rs` only parallelises the frame
    // rendering pass today, but handing it the same runner for the header pass keeps the
    // wiring uniform if that changes.
    let mut runner = crate::parallel_runner::ThreadRunner::new();
    let mut input = bytes;

    let layout = OutputLayout::for_format(format);
    let decode_row_bytes = layout.staging_bytes_per_pixel * width as usize;
    let mut raster = AlignedRaster::zeroed(decode_row_bytes * height as usize);

    let mut buffers = [JxlOutputBuffer::new(
        raster.as_mut_slice(),
        height as usize,
        decode_row_bytes,
    )];

    // Output buffers may only be supplied once the image header has been parsed, so the
    // header events drive a first pass and the frame is then decoded with buffers attached.
    loop {
        let event = decoder
            .process(&mut input, None, Some(&mut runner))
            .map_err(map_jxl_error)?;
        match event {
            Event::BasicInfo => {
                let extra_channels = decoder
                    .basic_info()
                    .map_or(0, |info| info.extra_channels.len());
                let pixel_format = layout.pixel_format(extra_channels);
                decoder
                    .set_pixel_format(pixel_format)
                    .map_err(map_jxl_error)?;
            }
            Event::FrameHeader => break,
            Event::Complete => {
                return Err(DecodeError::InvalidBitstream(
                    "no visible frame to decode".to_string(),
                ));
            }
            Event::NeedMoreInput { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "truncated before the first frame".to_string(),
                ));
            }
            _ => continue,
        }
    }

    loop {
        match decoder
            .process(&mut input, Some(&mut buffers), Some(&mut runner))
            .map_err(map_jxl_error)?
        {
            Event::FrameComplete { .. } | Event::Complete => break,
            Event::NeedMoreInput { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "truncated frame data".to_string(),
                ));
            }
            _ => continue,
        }
    }

    let decoded = raster.as_mut_slice();
    let pixels = if format == WicPixelFormat::RgbaHalf64 {
        narrow_to_half(decoded)
    } else {
        decoded.to_vec()
    };

    Ok(CachedRaster::new(width, height, format, pixels))
}

/// Maps a `jxl-rs` error onto the seam error type.
fn map_jxl_error(err: jxl::error::Error) -> DecodeError {
    DecodeError::InvalidBitstream(err.to_string())
}

/// Extracts a human-readable message from a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<&'static str>() {
        (*msg).to_string()
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else {
        "unknown parser panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIXTURES}/{name}"))
            .unwrap_or_else(|err| panic!("missing test fixture {name}: {err}"))
    }

    fn test_raster() -> CachedRaster {
        // 2x2 BGRA: red, green / blue, white.
        let bytes = vec![
            0, 0, 255, 255, // red
            0, 255, 0, 255, // green
            255, 0, 0, 255, // blue
            255, 255, 255, 255, // white
        ];
        CachedRaster::new(2, 2, WicPixelFormat::Bgra32, bytes)
    }

    #[test]
    fn blits_whole_frame_tightly_packed() {
        let mut out = vec![0u8; 16];
        test_raster().blit(None, 8, &mut out).unwrap();
        assert_eq!(out, test_raster().bytes());
    }

    #[test]
    fn blits_sub_rect_with_custom_stride() {
        let mut out = vec![0xAAu8; 16];
        // Bottom-right pixel only, with a stride of 8 and one row.
        test_raster()
            .blit(Some(Rect::new(1, 1, 1, 1)), 8, &mut out)
            .unwrap();
        assert_eq!(&out[..4], &[255, 255, 255, 255]);
        assert_eq!(&out[4..8], &[0xAA; 4]);
    }

    #[test]
    fn clips_zero_sized_rect_to_frame_edge() {
        let plan = test_raster()
            .plan(Some(Rect::new(1, 0, 0, 0)), 4, 8)
            .unwrap();
        assert_eq!(plan.resolved, Rect::new(1, 0, 1, 2));
    }

    #[test]
    fn rejects_out_of_bounds_rect() {
        assert_eq!(
            test_raster().plan(Some(Rect::new(2, 0, 1, 1)), 4, 4),
            Err(DecodeError::InvalidRect)
        );
        assert_eq!(
            test_raster().plan(Some(Rect::new(0, 0, 3, 1)), 12, 12),
            Err(DecodeError::InvalidRect)
        );
    }

    #[test]
    fn rejects_short_stride_and_small_buffer() {
        assert_eq!(
            test_raster().plan(None, 7, 64),
            Err(DecodeError::InvalidStride {
                stride: 7,
                min_stride: 8
            })
        );
        assert_eq!(
            test_raster().plan(None, 8, 15),
            Err(DecodeError::InsufficientBuffer {
                provided: 15,
                required: 16
            })
        );
    }

    #[test]
    fn sniffs_container_and_codestream_signatures() {
        assert!(is_jpeg_xl(&CONTAINER_SIGNATURE));
        assert!(is_jpeg_xl(&CODESTREAM_SIGNATURE));
        assert!(!is_jpeg_xl(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_jpeg_xl(b""));
    }

    #[test]
    fn rejects_non_jxl_input() {
        assert!(matches!(
            open(b"not a jpeg xl file"),
            Err(DecodeError::InvalidBitstream(_))
        ));
    }

    #[test]
    fn rejects_signature_without_a_bitstream() {
        // A valid signature followed by garbage exercises the post-sniff failure path.
        let mut bytes = CODESTREAM_SIGNATURE.to_vec();
        bytes.extend_from_slice(&[0u8; 32]);
        assert!(matches!(
            open(&bytes),
            Err(DecodeError::InvalidBitstream(_))
        ));
    }

    #[test]
    fn rejects_truncated_bitstream() {
        assert!(matches!(
            open(&CODESTREAM_SIGNATURE),
            Err(DecodeError::InvalidBitstream(_))
        ));
    }

    #[test]
    fn enforces_dimension_ceiling() {
        let zero = Err(DecodeError::InvalidBitstream(
            "image has a zero dimension".into(),
        ));
        assert_eq!(validate_dimensions(0, 10), zero);
        assert_eq!(validate_dimensions(10, 0), zero);
        assert_eq!(
            validate_dimensions(MAX_DIMENSION as usize + 1, 10),
            Err(DecodeError::DimensionTooLarge)
        );
        assert_eq!(
            validate_dimensions(10, MAX_DIMENSION as usize + 1),
            Err(DecodeError::DimensionTooLarge)
        );
        assert!(validate_dimensions(MAX_DIMENSION as usize, MAX_DIMENSION as usize).is_ok());
    }

    #[test]
    fn narrows_float_samples_to_binary16() {
        assert_eq!(half_from_f32(0.0), 0x0000);
        assert_eq!(half_from_f32(1.0), 0x3C00);
        assert_eq!(half_from_f32(-1.0), 0xBC00);
        assert_eq!(half_from_f32(-0.0), 0x8000);
        assert_eq!(half_from_f32(0.5), 0x3800);
        assert_eq!(half_from_f32(2.0), 0x4000);
        assert_eq!(half_from_f32(65504.0), 0x7BFF, "largest finite half");
        assert_eq!(
            half_from_f32(1.0e9),
            0x7C00,
            "overflow saturates to infinity"
        );
        assert_eq!(half_from_f32(f32::INFINITY), 0x7C00);
        assert_eq!(half_from_f32(f32::NEG_INFINITY), 0xFC00);
        assert_eq!(half_from_f32(f32::NAN) & 0x7C00, 0x7C00, "NaN stays NaN");
        assert_eq!(half_from_f32(1.0e-8), 0, "below the subnormal range");
        assert_eq!(half_from_f32(2.0f32.powi(-24)), 1, "one subnormal step");
    }

    #[test]
    fn narrows_interleaved_float_pixels() {
        let mut source = Vec::new();
        for sample in [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.5, 0.0, 1.0] {
            source.extend_from_slice(&sample.to_ne_bytes());
        }
        assert_eq!(
            narrow_to_half(&source),
            [0x00, 0x3C, 0, 0, 0, 0, 0x00, 0x3C, 0, 0, 0x00, 0x38, 0, 0, 0x00, 0x3C]
        );
    }

    #[test]
    fn reports_8bit_srgb_geometry_and_format() {
        let container = open(&fixture("srgb8_lossless.jxl")).expect("open");
        assert_eq!(container.frame_count(), 1);

        let frame = container.frame(0).expect("frame");
        let info = frame.info();
        assert_eq!((info.width, info.height), (3, 3));
        assert_eq!(info.pixel_format, WicPixelFormat::Bgra32);
        assert_eq!(info.dpi_x, DEFAULT_DPI);
        assert_eq!(info.dpi_y, DEFAULT_DPI);
    }

    #[test]
    fn decodes_pixels_with_full_alpha() {
        let container = open(&fixture("srgb8_lossless.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");

        let mut pixels = vec![0u8; 3 * 3 * 4];
        frame.copy_pixels(None, 12, &mut pixels).expect("copy");

        // The fixture's first pixel is fully saturated blue; 32bppBGRA stores it B-G-R-A.
        assert_eq!(
            &pixels[..4],
            &[255, 0, 0, 255],
            "top-left pixel is opaque blue in BGRA order"
        );
        for pixel in pixels.as_chunks::<4>().0 {
            assert_eq!(pixel[3], 255, "every opaque pixel must report full alpha");
        }
    }

    #[test]
    fn preserves_straight_alpha_from_the_bitstream() {
        let container = open(&fixture("alpha_lossless.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");

        let mut pixels = vec![0u8; 3 * 3 * 4];
        frame.copy_pixels(None, 12, &mut pixels).expect("copy");

        // Second pixel is green at 50% alpha; third is fully transparent.
        assert_eq!(&pixels[4..8], &[0, 255, 0, 128]);
        assert_eq!(pixels[11], 0, "third pixel is fully transparent");
    }

    #[test]
    fn exposes_synthesized_icc_profile_for_srgb() {
        let container = open(&fixture("srgb8_lossless.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");
        let profile = frame.color_profile().expect("synthesized profile");

        assert!(profile.len() > 100, "ICC profile is suspiciously short");
        assert_eq!(&profile[36..40], b"acsp", "ICC profile header magic");
    }

    #[test]
    fn returns_embedded_icc_profile_verbatim() {
        let container = open(&fixture("icc_embedded.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");
        let profile = frame.color_profile().expect("embedded profile");
        let original = fixture("embedded.icc");

        assert_eq!(
            profile,
            original.as_slice(),
            "embedded ICC must be returned verbatim"
        );
    }

    #[test]
    fn still_images_carry_no_animation_header() {
        let container = open(&fixture("srgb8_lossless.jxl")).expect("open");
        assert_eq!(container.frame_count(), 1);
        assert_eq!(container.animation(), None);
    }

    #[test]
    fn still_frame_zero_reports_still_timing() {
        let container = open(&fixture("srgb8_lossless.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");
        assert_eq!(frame.index(), 0);
        assert_eq!(
            frame.timing(),
            &FrameTiming {
                duration_ticks: 0,
                duration_ms: 0.0,
                is_keyframe: true,
                is_last: true,
            }
        );
    }

    #[test]
    fn rejects_frame_index_out_of_range() {
        let container = open(&fixture("srgb8_lossless.jxl")).expect("open");
        assert_eq!(
            container.frame(1).err(),
            Some(DecodeError::FrameIndexOutOfRange {
                index: 1,
                frame_count: 1
            })
        );
    }

    #[test]
    fn decodes_16bit_integer_pixels() {
        let container = open(&fixture("rgb16_lossless.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");
        assert_eq!(frame.info().pixel_format, WicPixelFormat::Rgba64);

        let mut pixels = vec![0u8; 3 * 3 * 8];
        frame.copy_pixels(None, 24, &mut pixels).expect("copy");

        let red = u16::from_le_bytes([pixels[0], pixels[1]]);
        assert_eq!(red, u16::MAX, "top-left pixel is opaque red");
        let alpha = u16::from_le_bytes([pixels[6], pixels[7]]);
        assert_eq!(alpha, u16::MAX, "opaque images must report full alpha");

        // Third pixel is pure blue, so the blue channel carries full scale.
        let blue = u16::from_le_bytes([pixels[20], pixels[21]]);
        assert_eq!(blue, u16::MAX, "third pixel is opaque blue");
    }

    #[test]
    fn decodes_hdr_half_float_pixels() {
        let container = open(&fixture("hdr_pq.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");
        assert_eq!(frame.info().pixel_format, WicPixelFormat::RgbaHalf64);

        let mut pixels = vec![0u8; 3 * 3 * 8];
        frame.copy_pixels(None, 24, &mut pixels).expect("copy");

        let alpha = u16::from_le_bytes([pixels[6], pixels[7]]);
        assert_eq!(alpha, 0x3C00, "binary16(1.0) is opaque");

        // The fixture's first pixel is fully saturated red in a Rec.2100 PQ encoding, so red
        // must be the strongest channel and the value must stay a finite half.
        let red = u16::from_le_bytes([pixels[0], pixels[1]]);
        let green = u16::from_le_bytes([pixels[2], pixels[3]]);
        let blue = u16::from_le_bytes([pixels[4], pixels[5]]);
        assert!(
            red > green && red > blue,
            "red must dominate at (0,0); halves: {pixels:02x?}"
        );
        assert!(red < 0x7C00, "PQ samples must stay finite, got {red:#06x}");
    }

    #[test]
    fn decodes_var_dct_and_modular_identically() {
        // Both fixtures were encoded losslessly (-d 0) from the same source, one with VarDCT
        // and one with the modular coder, so the decoded rasters must be byte-identical.
        let modular = open(&fixture("srgb8_lossless.jxl")).expect("open modular");
        let vardct = open(&fixture("srgb8_var_dct.jxl")).expect("open var-dct");

        let mut modular_pixels = vec![0u8; 3 * 3 * 4];
        let mut vardct_pixels = vec![0u8; 3 * 3 * 4];
        modular
            .frame(0)
            .expect("frame")
            .copy_pixels(None, 12, &mut modular_pixels)
            .expect("copy modular");
        vardct
            .frame(0)
            .expect("frame")
            .copy_pixels(None, 12, &mut vardct_pixels)
            .expect("copy var-dct");

        assert_eq!(modular_pixels, vardct_pixels);
    }

    #[test]
    fn decodes_partial_rectangle_within_frame() {
        let container = open(&fixture("srgb8_lossless.jxl")).expect("open");
        let frame = container.frame(0).expect("frame");

        let mut pixel = vec![0u8; 4];
        frame
            .copy_pixels(Some(Rect::new(1, 1, 1, 1)), 4, &mut pixel)
            .expect("copy");
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn panic_message_extracts_str_and_string() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("str error");
        assert_eq!(panic_message(&*str_payload), "str error");

        let string_payload: Box<dyn std::any::Any + Send> = Box::new("string error".to_string());
        assert_eq!(panic_message(&*string_payload), "string error");

        let unknown_payload: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(panic_message(&*unknown_payload), "unknown parser panic");
    }

    #[test]
    fn catches_panic_in_catch_unwind_boundary() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("parser exploded");
        }));
        let err = DecodeError::ParserPanic(panic_message(&*result.unwrap_err()));
        assert_eq!(err, DecodeError::ParserPanic("parser exploded".into()));
    }
}
