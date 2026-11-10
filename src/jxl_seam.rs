//! `jxl-rs` backed implementation of the [`DecoderSeam`] and [`FrameSeam`] traits.
//!
//! The bitstream is consumed entirely from memory: [`open`] reads the image header only, and the
//! full raster is decoded lazily on the first pixel request.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use jxl::api::{
    Endianness, Event, JxlBitDepth, JxlColorEncoding, JxlColorProfile, JxlColorType, JxlDataFormat,
    JxlDecoder, JxlDecoderOptions, JxlOutputBuffer, JxlPixelFormat, JxlTransferFunction,
};

use crate::decoder_seam::{
    CachedRaster, ContainerInfo, DecodeError, DecoderSeam, FrameInfo, FrameSeam, Rect,
    WicPixelFormat, MAX_DIMENSION,
};

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
}

/// Container seam backed by `jxl-rs`.
struct JxlDecoderSeam {
    info: ContainerInfo,
    bitstream: Arc<[u8]>,
    /// Frame 0 is the only visible frame; animations are out of scope.
    frame: FrameReadout,
}

/// Frame seam backed by `jxl-rs`, decoding its raster on first use.
struct JxlFrame {
    readout: FrameReadout,
    bitstream: Arc<[u8]>,
    cached_raster: OnceLock<Result<Arc<CachedRaster>, DecodeError>>,
}

impl DecoderSeam for JxlDecoderSeam {
    fn info(&self) -> &ContainerInfo {
        &self.info
    }

    fn frame(&self, index: usize) -> Result<Arc<dyn FrameSeam>, DecodeError> {
        if index >= self.info.frame_count {
            return Err(DecodeError::FrameIndexOutOfRange {
                index,
                frame_count: self.info.frame_count,
            });
        }
        Ok(Arc::new(JxlFrame {
            readout: self.frame.clone(),
            bitstream: Arc::clone(&self.bitstream),
            cached_raster: OnceLock::new(),
        }))
    }
}

impl FrameSeam for JxlFrame {
    fn info(&self) -> &FrameInfo {
        &self.readout.info
    }

    fn color_profile(&self) -> Option<&[u8]> {
        self.readout.color_profile.as_deref()
    }

    fn copy_pixels(
        &self,
        rect: Option<Rect>,
        stride: usize,
        out: &mut [u8],
    ) -> Result<(), DecodeError> {
        self.raster()?.blit(rect, stride, out)
    }
}

impl JxlFrame {
    /// Decodes (once) and returns the full raster for this frame.
    fn raster(&self) -> Result<Arc<CachedRaster>, DecodeError> {
        let info = &self.readout.info;
        match self.cached_raster.get_or_init(|| {
            decode_raster(&self.bitstream, info.width, info.height, info.pixel_format).map(Arc::new)
        }) {
            Ok(raster) => Ok(Arc::clone(raster)),
            Err(err) => Err(err.clone()),
        }
    }
}

/// Reads the image header of `bytes` and builds the container seam without decoding pixels.
#[allow(clippy::field_reassign_with_default)]
pub fn open(bytes: &[u8]) -> Result<Arc<dyn DecoderSeam>, DecodeError> {
    if !is_jpeg_xl(bytes) {
        return Err(DecodeError::InvalidBitstream(
            "missing JPEG XL container or codestream signature".to_string(),
        ));
    }

    let mut options = JxlDecoderOptions::default();
    options.alpha_mode = jxl::api::AlphaMode::Unpremultiplied;
    let mut decoder = JxlDecoder::new(options);
    let mut input = bytes;

    let basic_info = loop {
        match decoder
            .process(&mut input, None, None)
            .map_err(map_jxl_error)?
        {
            Event::BasicInfo => {
                break decoder
                    .basic_info()
                    .cloned()
                    .ok_or_else(|| DecodeError::InvalidBitstream("missing basic info".into()))?;
            }
            Event::NeedMoreInput { .. } => {
                return Err(DecodeError::InvalidBitstream(
                    "truncated before the image header".to_string(),
                ));
            }
            _ => continue,
        }
    };

    let (width, height) = basic_info.size;
    validate_dimensions(width, height)?;

    let embedded_profile = decoder.embedded_color_profile();
    let format = select_format(&basic_info.bit_depth, embedded_profile);
    let color_profile = extract_color_profile(embedded_profile, &basic_info);

    Ok(Arc::new(JxlDecoderSeam {
        info: ContainerInfo { frame_count: 1 },
        bitstream: Arc::from(bytes),
        frame: FrameReadout {
            info: FrameInfo {
                width: width as u32,
                height: height as u32,
                dpi_x: DEFAULT_DPI,
                dpi_y: DEFAULT_DPI,
                pixel_format: format,
            },
            color_profile,
        },
    }))
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

/// Runs a full decode of the first frame into a tightly packed raster.
#[allow(clippy::field_reassign_with_default)]
fn decode_raster(
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
            .process(&mut input, None, None)
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
            .process(&mut input, Some(&mut buffers), None)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder_seam::MAX_DIMENSION as LIMIT;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIXTURES}/{name}"))
            .unwrap_or_else(|err| panic!("missing test fixture {name}: {err}"))
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
            validate_dimensions(LIMIT as usize + 1, 10),
            Err(DecodeError::DimensionTooLarge)
        );
        assert_eq!(
            validate_dimensions(10, LIMIT as usize + 1),
            Err(DecodeError::DimensionTooLarge)
        );
        assert!(validate_dimensions(LIMIT as usize, LIMIT as usize).is_ok());
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
        let seam = open(&fixture("srgb8_lossless.jxl")).expect("open");
        assert_eq!(seam.frame_count(), 1);

        let frame = seam.frame(0).expect("frame");
        let info = frame.info();
        assert_eq!((info.width, info.height), (3, 3));
        assert_eq!(info.pixel_format, WicPixelFormat::Bgra32);
        assert_eq!(info.dpi_x, DEFAULT_DPI);
        assert_eq!(info.dpi_y, DEFAULT_DPI);
    }

    #[test]
    fn decodes_pixels_with_full_alpha() {
        let seam = open(&fixture("srgb8_lossless.jxl")).expect("open");
        let frame = seam.frame(0).expect("frame");

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
        let seam = open(&fixture("alpha_lossless.jxl")).expect("open");
        let frame = seam.frame(0).expect("frame");

        let mut pixels = vec![0u8; 3 * 3 * 4];
        frame.copy_pixels(None, 12, &mut pixels).expect("copy");

        // Second pixel is green at 50% alpha; third is fully transparent.
        assert_eq!(&pixels[4..8], &[0, 255, 0, 128]);
        assert_eq!(pixels[11], 0, "third pixel is fully transparent");
    }

    #[test]
    fn exposes_synthesized_icc_profile_for_srgb() {
        let seam = open(&fixture("srgb8_lossless.jxl")).expect("open");
        let frame = seam.frame(0).expect("frame");
        let profile = frame.color_profile().expect("synthesized profile");

        assert!(profile.len() > 100, "ICC profile is suspiciously short");
        assert_eq!(&profile[36..40], b"acsp", "ICC profile header magic");
    }

    #[test]
    fn returns_embedded_icc_profile_verbatim() {
        let seam = open(&fixture("icc_embedded.jxl")).expect("open");
        let frame = seam.frame(0).expect("frame");
        let profile = frame.color_profile().expect("embedded profile");
        let original = fixture("embedded.icc");

        assert_eq!(
            profile,
            original.as_slice(),
            "embedded ICC must be returned verbatim"
        );
    }

    #[test]
    fn rejects_frame_index_out_of_range() {
        let seam = open(&fixture("srgb8_lossless.jxl")).expect("open");
        assert_eq!(
            seam.frame(1).err(),
            Some(DecodeError::FrameIndexOutOfRange {
                index: 1,
                frame_count: 1
            })
        );
    }

    #[test]
    fn decodes_16bit_integer_pixels() {
        let seam = open(&fixture("rgb16_lossless.jxl")).expect("open");
        let frame = seam.frame(0).expect("frame");
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
        let seam = open(&fixture("hdr_pq.jxl")).expect("open");
        let frame = seam.frame(0).expect("frame");
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
        let seam = open(&fixture("srgb8_lossless.jxl")).expect("open");
        let frame = seam.frame(0).expect("frame");

        let mut pixel = vec![0u8; 4];
        frame
            .copy_pixels(Some(Rect::new(1, 1, 1, 1)), 4, &mut pixel)
            .expect("copy");
        assert_eq!(pixel[3], 255);
    }
}
