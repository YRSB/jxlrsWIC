//! Windows Imaging Component (WIC) decoder COM server for JPEG XL.
//!
//! This layer owns COM lifecycle, parameter marshalling and `HRESULT` translation only. Every
//! byte of bitstream parsing and pixel work happens behind [`crate::decoder_seam`], so the COM
//! surface stays a thin, auditable adapter.

use std::ffi::c_void;
use std::sync::{Arc, OnceLock, RwLock};

use windows::core::{implement, Result, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, E_UNEXPECTED};
use windows::Win32::Graphics::Imaging::{
    IWICBitmapDecoder, IWICBitmapDecoderInfo, IWICBitmapDecoder_Impl, IWICBitmapFrameDecode,
    IWICBitmapFrameDecode_Impl, IWICBitmapSource, IWICBitmapSource_Impl, IWICColorContext,
    IWICColorContext_Impl, IWICMetadataQueryReader, IWICPalette,
    WICBitmapDecoderCapabilityCanDecodeAllImages, WICColorContextProfile, WICDecodeOptions,
    WICRect,
};
use windows::Win32::System::Com::{IStream, STATFLAG_NONAME, STREAM_SEEK_CUR, STREAM_SEEK_SET};

use crate::decoder_seam::{
    DecodeError, DecoderSeam, FrameSeam, Rect, WicPixelFormat, MAX_STREAM_BYTES,
};
use crate::guids::{
    GUID_ContainerFormatJxl, GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat64bppRGBA,
    GUID_WICPixelFormat64bppRGBAHalf,
};

/// WIC error raised when the bitstream cannot be decoded.
const WINCODEC_ERR_BADIMAGE: HRESULT = HRESULT(0x88982F60_u32 as i32);
/// WIC error raised when a requested frame does not exist.
const WINCODEC_ERR_FRAMEMISSING: HRESULT = HRESULT(0x88982F62_u32 as i32);
/// WIC error raised when the image has not been initialized with a stream.
const WINCODEC_ERR_WRONGSTATE: HRESULT = HRESULT(0x88982F04_u32 as i32);
/// WIC error raised when a destination buffer is too small for the requested copy.
const WINCODEC_ERR_INSUFFICIENTBUFFER: HRESULT = HRESULT(0x88982F8C_u32 as i32);
/// WIC error raised when no palette is available for the image.
const WINCODEC_ERR_PALETTEUNAVAILABLE: HRESULT = HRESULT(0x88982F45_u32 as i32);
/// WIC error raised for unsupported operations, including missing previews.
const WINCODEC_ERR_UNSUPPORTEDOPERATION: HRESULT = HRESULT(0x88982F81_u32 as i32);
/// WIC error raised when the decoder carries no embedded thumbnail.
const WINCODEC_ERR_CODECNOTHUMBNAIL: HRESULT = HRESULT(0x88982F44_u32 as i32);

/// Converts a seam failure into the WIC `HRESULT` the shell expects.
fn to_hresult(err: &DecodeError) -> HRESULT {
    match err {
        DecodeError::StreamError(_) => E_UNEXPECTED,
        DecodeError::DimensionTooLarge | DecodeError::InvalidBitstream(_) => WINCODEC_ERR_BADIMAGE,
        DecodeError::InvalidRect | DecodeError::InvalidStride { .. } => E_INVALIDARG,
        DecodeError::InsufficientBuffer { .. } => WINCODEC_ERR_INSUFFICIENTBUFFER,
        DecodeError::FrameIndexOutOfRange { .. } => WINCODEC_ERR_FRAMEMISSING,
    }
}

/// Enforces the hard ceiling on accepted stream sizes.
fn ensure_within_stream_limit(length: u64) -> Result<()> {
    if length > MAX_STREAM_BYTES as u64 {
        Err(WINCODEC_ERR_BADIMAGE.into())
    } else {
        Ok(())
    }
}

/// Enforces the same ceiling while reading, for streams that cannot report a size upfront.
fn ensure_room_for(total: usize, next_read: usize) -> Result<()> {
    match total.checked_add(next_read) {
        Some(length) if length <= MAX_STREAM_BYTES => Ok(()),
        _ => Err(WINCODEC_ERR_BADIMAGE.into()),
    }
}

/// Copies the whole `IStream` into a contiguous buffer, refusing oversized inputs.
unsafe fn ingest_stream(stream: &IStream) -> Result<Vec<u8>> {
    let mut start = 0u64;
    stream.Seek(0, STREAM_SEEK_SET, Some(&mut start))?;

    let mut stat = std::mem::zeroed();
    let sized = stream.Stat(&mut stat, STATFLAG_NONAME).is_ok() && stat.cbSize > 0;
    let length = if sized { stat.cbSize } else { 0u64 };
    ensure_within_stream_limit(length)?;

    let mut bytes = if sized {
        vec![0u8; length as usize]
    } else {
        Vec::new()
    };
    let mut filled = 0usize;
    let mut chunk = [0u8; 64 * 1024];

    loop {
        let mut read = 0u32;
        // SAFETY: the destination slice is valid for `chunk.len()` writes.
        let hr = stream.Read(
            chunk.as_mut_ptr().cast::<c_void>(),
            chunk.len() as u32,
            Some(&mut read),
        );
        hr.ok()?;
        if read == 0 {
            break;
        }
        ensure_room_for(filled, read as usize)?;
        if sized {
            let end = (filled + read as usize).min(bytes.len());
            bytes[filled..end].copy_from_slice(&chunk[..end - filled]);
        } else {
            bytes.extend_from_slice(&chunk[..read as usize]);
        }
        filled += read as usize;
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// Reads the JPEG XL signature bytes from `IStream` without disturbing the caller's position.
unsafe fn sniff_signatures(stream: &IStream) -> Result<bool> {
    let mut original = 0u64;
    stream.Seek(0, STREAM_SEEK_CUR, Some(&mut original))?;
    stream.Seek(0, STREAM_SEEK_SET, Some(&mut original))?;

    let mut header = [0u8; 12];
    let mut read = 0u32;
    let result = stream
        .Read(
            header.as_mut_ptr().cast::<c_void>(),
            header.len() as u32,
            Some(&mut read),
        )
        .ok();

    let mut restored = 0u64;
    let _ = stream.Seek(original as i64, STREAM_SEEK_SET, Some(&mut restored));
    result?;

    Ok(crate::jxl_seam::is_jpeg_xl(&header[..read as usize]))
}

/// WIC `IWICColorContext` wrapper around the frame's ICC profile bytes.
#[implement(IWICColorContext)]
struct JxlColorContext {
    profile: RwLock<Vec<u8>>,
}

impl JxlColorContext {
    fn with_profile(profile: &[u8]) -> Self {
        Self {
            profile: RwLock::new(profile.to_vec()),
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.profile
            .read()
            .map(|profile| profile.clone())
            .unwrap_or_default()
    }
}

impl IWICColorContext_Impl for JxlColorContext_Impl {
    fn InitializeFromFilename(&self, _wzfilename: &windows::core::PCWSTR) -> Result<()> {
        Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into())
    }

    fn InitializeFromMemory(&self, pbbuffer: *const u8, cbbuffersize: u32) -> Result<()> {
        if pbbuffer.is_null() && cbbuffersize != 0 {
            return Err(E_POINTER.into());
        }
        let bytes = if cbbuffersize == 0 {
            Vec::new()
        } else {
            // SAFETY: the caller promised `cbbuffersize` readable bytes at `pbbuffer`.
            unsafe { std::slice::from_raw_parts(pbbuffer, cbbuffersize as usize) }.to_vec()
        };
        match self.profile.write() {
            Ok(mut profile) => {
                *profile = bytes;
                Ok(())
            }
            Err(_) => Err(E_UNEXPECTED.into()),
        }
    }

    fn InitializeFromExifColorSpace(&self, _value: u32) -> Result<()> {
        Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into())
    }

    fn GetType(&self) -> Result<windows::Win32::Graphics::Imaging::WICColorContextType> {
        Ok(WICColorContextProfile)
    }

    fn GetProfileBytes(&self, cbbuffer: u32, pbbuffer: *mut u8, pcbactual: *mut u32) -> Result<()> {
        let profile = self.snapshot();
        if !pcbactual.is_null() {
            // SAFETY: the caller supplied a writable `u32` pointer, as the WIC contract requires.
            unsafe { *pcbactual = profile.len() as u32 };
        }
        if cbbuffer == 0 || pbbuffer.is_null() {
            return Ok(());
        }
        if (cbbuffer as usize) < profile.len() {
            return Err(WINCODEC_ERR_INSUFFICIENTBUFFER.into());
        }
        // SAFETY: the caller promised `cbbuffer` writable bytes at `pbbuffer`, and the length
        // check above guarantees the profile fits.
        unsafe {
            std::ptr::copy_nonoverlapping(profile.as_ptr(), pbbuffer, profile.len());
        }
        Ok(())
    }

    fn GetExifColorSpace(&self) -> Result<u32> {
        Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into())
    }
}

/// The `GetColorContexts` out-parameters, as supplied by the COM caller.
#[derive(Clone, Copy)]
struct ColorContextSlot {
    /// Capacity of the caller's array.
    capacity: u32,
    /// Caller's array of interfaces.
    contexts: *mut Option<IWICColorContext>,
    /// Receives the number of profiles the decoder has.
    actual_count: *mut u32,
}

impl ColorContextSlot {
    /// Writes the frame's colour context, if any, into the caller's array.
    ///
    /// # Safety
    /// The caller must honour the `GetColorContexts` contract: `contexts` points at `capacity`
    /// writable slots, and `actual_count` is null or writable.
    unsafe fn write(self, profile: Option<&[u8]>) -> Result<()> {
        let available = usize::from(profile.is_some());
        if !self.actual_count.is_null() {
            *self.actual_count = available as u32;
        }
        if available == 0 || self.capacity == 0 || self.contexts.is_null() {
            return Ok(());
        }
        let profile = profile.expect("available implies a profile is present");
        let context: IWICColorContext = JxlColorContext::with_profile(profile).into();
        self.contexts.write(Some(context));
        Ok(())
    }
}

/// Converts a seam pixel format into the canonical WIC format GUID.
fn pixel_format_guid(format: WicPixelFormat) -> GUID {
    match format {
        WicPixelFormat::Bgra32 => GUID_WICPixelFormat32bppBGRA,
        WicPixelFormat::Rgba64 => GUID_WICPixelFormat64bppRGBA,
        WicPixelFormat::RgbaHalf64 => GUID_WICPixelFormat64bppRGBAHalf,
    }
}

/// WIC bitmap frame decoder backed by a frame seam.
///
/// Crate-private: hosts see it only through the IWICBitmapFrameDecode interface, and the
/// implement macro's generated companion type inherits this visibility.
#[implement(IWICBitmapFrameDecode)]
pub(crate) struct JxlFrameDecode {
    /// Decoded frame this COM object exposes.
    frame: Arc<dyn FrameSeam>,
}

impl JxlFrameDecode {
    /// Wraps a decoded frame.
    pub(crate) fn new(frame: Arc<dyn FrameSeam>) -> Self {
        Self { frame }
    }
}

impl IWICBitmapSource_Impl for JxlFrameDecode_Impl {
    fn GetSize(&self, puiwidth: *mut u32, puiheight: *mut u32) -> Result<()> {
        if puiwidth.is_null() || puiheight.is_null() {
            return Err(E_POINTER.into());
        }
        let info = self.frame.info();
        // SAFETY: both pointers were checked for null.
        unsafe {
            *puiwidth = info.width;
            *puiheight = info.height;
        }
        Ok(())
    }

    fn GetPixelFormat(&self) -> Result<GUID> {
        Ok(pixel_format_guid(self.frame.info().pixel_format))
    }

    fn GetResolution(&self, pdpix: *mut f64, pdpiy: *mut f64) -> Result<()> {
        if pdpix.is_null() || pdpiy.is_null() {
            return Err(E_POINTER.into());
        }
        let info = self.frame.info();
        // SAFETY: both pointers were checked for null.
        unsafe {
            *pdpix = info.dpi_x;
            *pdpiy = info.dpi_y;
        }
        Ok(())
    }

    fn CopyPalette(&self, _pipalette: Option<&IWICPalette>) -> Result<()> {
        Err(WINCODEC_ERR_PALETTEUNAVAILABLE.into())
    }

    fn CopyPixels(
        &self,
        prc: *const WICRect,
        cbstride: u32,
        cbbuffersize: u32,
        pbbuffer: *mut u8,
    ) -> Result<()> {
        // The first `CopyPixels` is the call that decodes the bitstream, so it is the one that
        // can trip a parser panic inside `jxl-rs`. Contain it rather than aborting the host.
        crate::panic_guard::guard(|| {
            if pbbuffer.is_null() {
                return Err(E_POINTER.into());
            }
            let rect = if prc.is_null() {
                None
            } else {
                // SAFETY: the caller passed a non-null WICRect pointer.
                let raw = unsafe { *prc };
                if raw.X < 0 || raw.Y < 0 || raw.Width < 0 || raw.Height < 0 {
                    return Err(E_INVALIDARG.into());
                }
                Some(Rect::new(
                    raw.X as u32,
                    raw.Y as u32,
                    raw.Width as u32,
                    raw.Height as u32,
                ))
            };

            // SAFETY: `pbbuffer` is non-null and the caller promised `cbbuffersize` writable bytes.
            let buffer = unsafe { std::slice::from_raw_parts_mut(pbbuffer, cbbuffersize as usize) };
            self.frame
                .copy_pixels(rect, cbstride as usize, buffer)
                .map_err(|err| windows::core::Error::from(to_hresult(&err)))
        })
    }
}

impl IWICBitmapFrameDecode_Impl for JxlFrameDecode_Impl {
    fn GetMetadataQueryReader(&self) -> Result<IWICMetadataQueryReader> {
        Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into())
    }

    fn GetColorContexts(
        &self,
        ccount: u32,
        ppicolorcontexts: *mut Option<IWICColorContext>,
        pcactualcount: *mut u32,
    ) -> Result<()> {
        let slot = ColorContextSlot {
            capacity: ccount,
            contexts: ppicolorcontexts,
            actual_count: pcactualcount,
        };
        // SAFETY: delegated pointer contract, documented on the WIC interface.
        unsafe { slot.write(self.frame.color_profile()) }
    }

    fn GetThumbnail(&self) -> Result<IWICBitmapSource> {
        // The decoder carries no embedded thumbnail; WIC downscales frame 0 instead.
        Err(WINCODEC_ERR_CODECNOTHUMBNAIL.into())
    }
}

/// WIC bitmap decoder for JPEG XL.
///
/// `Initialize` publishes the decoded seam exactly once; later calls report
/// `WINCODEC_ERR_WRONGSTATE`, matching how WIC hosts treat a re-used decoder instance.
///
/// Crate-private: hosts see it only through the `IWICBitmapDecoder` interface, and the
/// `implement` macro's generated companion type inherits this visibility.
#[implement(IWICBitmapDecoder)]
pub(crate) struct JxlWicDecoder {
    /// Decoding seam published by Initialize.
    seam: OnceLock<Arc<dyn DecoderSeam>>,
}

impl JxlWicDecoder {
    /// Creates a decoder with no attached stream.
    pub(crate) fn new() -> Self {
        Self {
            seam: OnceLock::new(),
        }
    }

    fn seam(&self) -> Result<Arc<dyn DecoderSeam>> {
        self.seam
            .get()
            .cloned()
            .ok_or_else(|| WINCODEC_ERR_WRONGSTATE.into())
    }
}

impl Default for JxlWicDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl IWICBitmapDecoder_Impl for JxlWicDecoder_Impl {
    fn QueryCapability(&self, pistream: Option<&IStream>) -> Result<u32> {
        crate::panic_guard::guard(|| {
            let stream = pistream.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            // SAFETY: `stream` is a live COM interface pointer for the duration of the call.
            let matches = unsafe { sniff_signatures(stream) }?;
            if matches {
                Ok(WICBitmapDecoderCapabilityCanDecodeAllImages.0 as u32)
            } else {
                Ok(0)
            }
        })
    }

    fn Initialize(
        &self,
        pistream: Option<&IStream>,
        _cacheoptions: WICDecodeOptions,
    ) -> Result<()> {
        crate::panic_guard::guard(|| {
            let stream = pistream.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            // SAFETY: `stream` is a live COM interface pointer for the duration of the call.
            let bytes = unsafe { ingest_stream(stream) }?;
            let seam = crate::jxl_seam::open(&bytes)
                .map_err(|err| windows::core::Error::from(to_hresult(&err)))?;
            self.seam
                .set(seam)
                .map_err(|_| windows::core::Error::from(WINCODEC_ERR_WRONGSTATE))?;
            Ok(())
        })
    }

    fn GetContainerFormat(&self) -> Result<GUID> {
        Ok(GUID_ContainerFormatJxl)
    }

    fn GetDecoderInfo(&self) -> Result<IWICBitmapDecoderInfo> {
        Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into())
    }

    fn CopyPalette(&self, _pipalette: Option<&IWICPalette>) -> Result<()> {
        Err(WINCODEC_ERR_PALETTEUNAVAILABLE.into())
    }

    fn GetMetadataQueryReader(&self) -> Result<IWICMetadataQueryReader> {
        Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into())
    }

    fn GetPreview(&self) -> Result<IWICBitmapSource> {
        Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into())
    }

    fn GetColorContexts(
        &self,
        ccount: u32,
        ppicolorcontexts: *mut Option<IWICColorContext>,
        pcactualcount: *mut u32,
    ) -> Result<()> {
        crate::panic_guard::guard(|| {
            let slot = ColorContextSlot {
                capacity: ccount,
                contexts: ppicolorcontexts,
                actual_count: pcactualcount,
            };
            // The container has no colour context of its own, so it delegates to frame 0.
            let seam = self.seam()?;
            let frame = seam
                .frame(0)
                .map_err(|err| windows::core::Error::from(to_hresult(&err)))?;
            // SAFETY: delegated pointer contract, documented on the WIC interface.
            unsafe { slot.write(frame.color_profile()) }
        })
    }

    fn GetThumbnail(&self) -> Result<IWICBitmapSource> {
        Err(WINCODEC_ERR_CODECNOTHUMBNAIL.into())
    }

    fn GetFrameCount(&self) -> Result<u32> {
        crate::panic_guard::guard_unexpected(|| Ok(self.seam()?.frame_count() as u32))
    }

    fn GetFrame(&self, index: u32) -> Result<IWICBitmapFrameDecode> {
        crate::panic_guard::guard(|| {
            let seam = self.seam()?;
            let frame = seam
                .frame(index as usize)
                .map_err(|err| windows::core::Error::from(to_hresult(&err)))?;
            let decode: IWICBitmapFrameDecode = JxlFrameDecode::new(frame).into();
            Ok(decode)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder_seam::MAX_STREAM_BYTES;

    #[test]
    fn accepts_streams_within_the_ceiling() {
        assert!(ensure_within_stream_limit(0).is_ok());
        assert!(ensure_within_stream_limit(MAX_STREAM_BYTES as u64).is_ok());
    }

    #[test]
    fn rejects_streams_above_the_ceiling() {
        let result = ensure_within_stream_limit(MAX_STREAM_BYTES as u64 + 1);
        assert_eq!(
            result.err().map(|err| err.code()),
            Some(WINCODEC_ERR_BADIMAGE)
        );
    }

    #[test]
    fn enforces_the_ceiling_while_reading_unmeasured_streams() {
        assert!(ensure_room_for(0, MAX_STREAM_BYTES).is_ok());
        assert!(ensure_room_for(MAX_STREAM_BYTES - 1, 1).is_ok());
        assert_eq!(
            ensure_room_for(MAX_STREAM_BYTES, 1)
                .err()
                .map(|err| err.code()),
            Some(WINCODEC_ERR_BADIMAGE)
        );
        assert_eq!(
            ensure_room_for(usize::MAX, 1).err().map(|err| err.code()),
            Some(WINCODEC_ERR_BADIMAGE),
            "an overflowing read must not wrap past the ceiling"
        );
    }

    #[test]
    fn maps_seam_errors_onto_wic_hresults() {
        assert_eq!(
            to_hresult(&DecodeError::DimensionTooLarge),
            WINCODEC_ERR_BADIMAGE
        );
        assert_eq!(
            to_hresult(&DecodeError::InvalidBitstream("x".into())),
            WINCODEC_ERR_BADIMAGE
        );
        assert_eq!(
            to_hresult(&DecodeError::InvalidStride {
                stride: 1,
                min_stride: 4
            }),
            E_INVALIDARG
        );
        assert_eq!(
            to_hresult(&DecodeError::InsufficientBuffer {
                provided: 1,
                required: 4
            }),
            WINCODEC_ERR_INSUFFICIENTBUFFER
        );
    }
}
