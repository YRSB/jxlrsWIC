//! Windows Imaging Component (WIC) decoder COM server for JPEG XL.
//!
//! This layer owns COM lifecycle, parameter marshalling and `HRESULT` translation only. Every
//! byte of bitstream parsing and pixel work happens behind [`crate::decoder_seam`], so the COM
//! surface stays a thin, auditable adapter.

use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use windows::core::{implement, Interface, Result, GUID, HRESULT, PCWSTR, PROPVARIANT, PWSTR};
use windows::Win32::Foundation::{
    E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK,
};
use windows::Win32::Graphics::Imaging::{
    IWICBitmapDecoder, IWICBitmapDecoderInfo, IWICBitmapDecoder_Impl, IWICBitmapFrameDecode,
    IWICBitmapFrameDecode_Impl, IWICBitmapSource, IWICBitmapSource_Impl, IWICColorContext,
    IWICColorContext_Impl, IWICImagingFactory, IWICMetadataQueryReader,
    IWICMetadataQueryReader_Impl, IWICPalette, WICBitmapDecoderCapabilityCanDecodeAllImages,
    WICColorContextProfile, WICDecodeOptions, WICRect,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemAlloc, IEnumString, IEnumString_Impl, IStream, CLSCTX_INPROC_SERVER,
    STATFLAG_NONAME, STREAM_SEEK_CUR, STREAM_SEEK_SET,
};

use crate::decoder_seam::{
    DecodeError, JxlContainer, JxlFrame, Rect, WicPixelFormat, MAX_STREAM_BYTES,
};
use crate::guids::{
    CLSID_JxlrsWICDecoder, CLSID_WICImagingFactory, GUID_ContainerFormatJxl,
    GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat64bppRGBA, GUID_WICPixelFormat64bppRGBAHalf,
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
/// WIC error raised when a metadata query names a property the decoder does not carry.
const WINCODEC_ERR_PROPERTYNOTFOUND: HRESULT = HRESULT(0x88982F40_u32 as i32);
/// WIC error raised when the decoder carries no embedded thumbnail.
const WINCODEC_ERR_CODECNOTHUMBNAIL: HRESULT = HRESULT(0x88982F44_u32 as i32);

/// Converts a seam failure into the WIC `HRESULT` the shell expects.
fn to_hresult(err: &DecodeError) -> HRESULT {
    match err {
        DecodeError::StreamError(_) => E_UNEXPECTED,
        DecodeError::DimensionTooLarge
        | DecodeError::InvalidBitstream(_)
        | DecodeError::ParserPanic(_) => WINCODEC_ERR_BADIMAGE,
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

    Ok(crate::decoder_seam::is_jpeg_xl(&header[..read as usize]))
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

/// Frame-level metadata path reporting the frame delay in GIF-compatible centiseconds
/// (`VT_UI2`).
const FRAME_DELAY_PATH: &str = "/grctlext/Delay";
/// Decoder-level metadata path identifying the application extension.
const APP_EXTENSION_PATH: &str = "/appext/application";
/// Decoder-level metadata path carrying the loop count as a decimal `VT_LPWSTR` string.
const APP_DATA_PATH: &str = "/appext/data";
/// Application identifier reported through [`APP_EXTENSION_PATH`], matching the GIF
/// `NETSCAPE2.0` extension whose loop-count semantics (0 = loop forever) JXL shares.
const NETSCAPE_APPLICATION_ID: &str = "NETSCAPE2.0";

/// Converts a scan duration in milliseconds to the GIF-compatible `VT_UI2` centisecond delay.
///
/// Rounds to the nearest centisecond and saturates at `u16::MAX`; non-positive or non-finite
/// durations report zero ("no delay specified").
fn delay_centiseconds(duration_ms: f64) -> u16 {
    if !duration_ms.is_finite() || duration_ms <= 0.0 {
        return 0;
    }
    (duration_ms / 10.0).round().clamp(0.0, f64::from(u16::MAX)) as u16
}

/// Formats the animation loop count for [`APP_DATA_PATH`]: decimal, saturating at `u16::MAX`.
/// Zero passes through and means "loop forever", matching the GIF `NETSCAPE2.0` semantics.
fn loop_count_text(num_loops: u32) -> String {
    num_loops.min(u32::from(u16::MAX)).to_string()
}

/// Encodes `text` as NUL-terminated UTF-16 for [`IEnumString`] output.
fn wide_nul(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// Reads a metadata query name. Returns `None` for null or non-UTF-16 input so unknown names
/// degrade to `WINCODEC_ERR_PROPERTYNOTFOUND` instead of panicking across COM.
///
/// # Safety
///
/// The caller must guarantee `wzname` is either null or points at a readable NUL-terminated
/// UTF-16 string, which is exactly what the WIC query-reader contract promises.
unsafe fn read_query_name(wzname: &PCWSTR) -> Option<String> {
    if wzname.is_null() {
        return None;
    }
    unsafe { wzname.to_string().ok() }
}

/// Writes a looked-up metadata value into the caller's `PROPVARIANT` slot.
///
/// A missing value surfaces as `WINCODEC_ERR_PROPERTYNOTFOUND`, following the WIC query
/// protocol; this is not a decode failure, so [`to_hresult`] stays untouched.
fn write_metadata_value(value: Option<PROPVARIANT>, pvarvalue: *mut PROPVARIANT) -> Result<()> {
    if pvarvalue.is_null() {
        return Err(E_POINTER.into());
    }
    let value = value.ok_or(WINCODEC_ERR_PROPERTYNOTFOUND)?;
    // SAFETY: non-null per the check above; `ptr::write` initialises without dropping because
    // the slot arrives uninitialised from the caller.
    unsafe {
        std::ptr::write(pvarvalue, value);
    }
    Ok(())
}

/// Answers `GetLocation` for both metadata readers: every served path lives at the root.
fn reader_location(
    cchmaxlength: u32,
    wznamespace: &PWSTR,
    pcchactuallength: *mut u32,
) -> Result<()> {
    // The only location served is the root namespace: the empty string plus its NUL.
    const NEEDED: u32 = 1;
    if !pcchactuallength.is_null() {
        // SAFETY: the caller promised a writable `u32` slot.
        unsafe {
            *pcchactuallength = NEEDED;
        }
    }
    if cchmaxlength > 0 && !wznamespace.as_ptr().is_null() {
        // SAFETY: the caller promised at least `cchmaxlength` writable UTF-16 units, and
        // `cchmaxlength > 0` guarantees room for the single NUL written here.
        unsafe {
            *wznamespace.as_ptr() = 0;
        }
    }
    Ok(())
}

/// `IWICMetadataQueryReader` serving the GIF-compatible application-extension entries of an
/// animated container: [`APP_EXTENSION_PATH`] and [`APP_DATA_PATH`] (`NETSCAPE2.0` loop count).
///
/// Only constructed for containers whose [`JxlContainer::animation`] header is present; still
/// images keep the legacy `WINCODEC_ERR_UNSUPPORTEDOPERATION` from `GetMetadataQueryReader`.
#[implement(IWICMetadataQueryReader)]
struct JxlDecoderMetadataReader {
    /// Container holding the animation header.
    container: Arc<JxlContainer>,
}

impl JxlDecoderMetadataReader {
    /// Wraps the animation header of an already-gated container.
    fn new(container: Arc<JxlContainer>) -> Self {
        Self { container }
    }

    /// Resolves one query path to its value. `None` means "not carried" and surfaces as
    /// `WINCODEC_ERR_PROPERTYNOTFOUND`.
    fn lookup(&self, name: &str) -> Option<PROPVARIANT> {
        let animation = self.container.animation()?;
        if name == APP_EXTENSION_PATH {
            Some(PROPVARIANT::from(NETSCAPE_APPLICATION_ID))
        } else if name == APP_DATA_PATH {
            Some(PROPVARIANT::from(
                loop_count_text(animation.num_loops).as_str(),
            ))
        } else {
            None
        }
    }
}

impl IWICMetadataQueryReader_Impl for JxlDecoderMetadataReader_Impl {
    fn GetContainerFormat(&self) -> Result<GUID> {
        Ok(GUID_ContainerFormatJxl)
    }

    fn GetLocation(
        &self,
        cchmaxlength: u32,
        wznamespace: &PWSTR,
        pcchactuallength: *mut u32,
    ) -> Result<()> {
        reader_location(cchmaxlength, wznamespace, pcchactuallength)
    }

    fn GetMetadataByName(&self, wzname: &PCWSTR, pvarvalue: *mut PROPVARIANT) -> Result<()> {
        // SAFETY: `wzname` obeys the WIC query-reader contract (null or NUL-terminated UTF-16).
        let name = unsafe { read_query_name(wzname) }.ok_or(WINCODEC_ERR_PROPERTYNOTFOUND)?;
        write_metadata_value(self.lookup(&name), pvarvalue)
    }

    fn GetEnumerator(&self) -> Result<IEnumString> {
        Ok(
            JxlPathEnumerator::new(vec![wide_nul(APP_EXTENSION_PATH), wide_nul(APP_DATA_PATH)])
                .into(),
        )
    }
}

/// `IWICMetadataQueryReader` serving one animated frame's [`FRAME_DELAY_PATH`] entry (`VT_UI2`
/// centiseconds, converted from the scan's millisecond duration).
///
/// Only constructed when the container holds more than one frame; still images keep the legacy
/// `WINCODEC_ERR_UNSUPPORTEDOPERATION` from `GetMetadataQueryReader`.
#[implement(IWICMetadataQueryReader)]
struct JxlFrameMetadataReader {
    /// Container holding the frame timing.
    container: Arc<JxlContainer>,
    /// Zero-based index of the frame this reader describes.
    index: usize,
}

impl JxlFrameMetadataReader {
    /// Wraps one frame of an already-gated container.
    fn new(container: Arc<JxlContainer>, index: usize) -> Self {
        Self { container, index }
    }

    /// Resolves one query path to its value. `None` means "not carried" and surfaces as
    /// `WINCODEC_ERR_PROPERTYNOTFOUND`.
    fn lookup(&self, name: &str) -> Option<PROPVARIANT> {
        if name != FRAME_DELAY_PATH || self.container.frame_count() <= 1 {
            return None;
        }
        let frame = self.container.frame(self.index).ok()?;
        Some(PROPVARIANT::from(delay_centiseconds(
            frame.timing().duration_ms,
        )))
    }
}

impl IWICMetadataQueryReader_Impl for JxlFrameMetadataReader_Impl {
    fn GetContainerFormat(&self) -> Result<GUID> {
        Ok(GUID_ContainerFormatJxl)
    }

    fn GetLocation(
        &self,
        cchmaxlength: u32,
        wznamespace: &PWSTR,
        pcchactuallength: *mut u32,
    ) -> Result<()> {
        reader_location(cchmaxlength, wznamespace, pcchactuallength)
    }

    fn GetMetadataByName(&self, wzname: &PCWSTR, pvarvalue: *mut PROPVARIANT) -> Result<()> {
        // SAFETY: `wzname` obeys the WIC query-reader contract (null or NUL-terminated UTF-16).
        let name = unsafe { read_query_name(wzname) }.ok_or(WINCODEC_ERR_PROPERTYNOTFOUND)?;
        write_metadata_value(self.lookup(&name), pvarvalue)
    }

    fn GetEnumerator(&self) -> Result<IEnumString> {
        Ok(JxlPathEnumerator::new(vec![wide_nul(FRAME_DELAY_PATH)]).into())
    }
}

/// `IEnumString` over the fixed path list of one metadata reader.
///
/// Paths are stored NUL-terminated so `Next` can copy them straight into `CoTaskMemAlloc`
/// buffers, which the caller frees with `CoTaskMemFree` per the COM contract.
#[implement(IEnumString)]
struct JxlPathEnumerator {
    /// Served paths as NUL-terminated UTF-16 strings.
    paths: Vec<Vec<u16>>,
    /// Index of the next path `Next` will return.
    cursor: Mutex<usize>,
}

impl JxlPathEnumerator {
    /// Builds an enumerator positioned before the first path.
    fn new(paths: Vec<Vec<u16>>) -> Self {
        Self {
            paths,
            cursor: Mutex::new(0),
        }
    }

    /// Builds an enumerator resumed at `cursor` (for `Clone`); out-of-range positions clamp
    /// to the end so a poisoned or stale cursor reports `S_FALSE` instead of panicking.
    fn with_position(paths: Vec<Vec<u16>>, cursor: usize) -> Self {
        let cursor = cursor.min(paths.len());
        Self {
            paths,
            cursor: Mutex::new(cursor),
        }
    }
}

impl IEnumString_Impl for JxlPathEnumerator_Impl {
    fn Next(&self, celt: u32, rgelt: *mut PWSTR, pceltfetched: *mut u32) -> HRESULT {
        if celt > 0 && rgelt.is_null() {
            return E_POINTER;
        }
        let mut cursor = match self.cursor.lock() {
            Ok(cursor) => cursor,
            Err(_) => return E_UNEXPECTED,
        };
        let mut fetched = 0u32;
        while fetched < celt {
            let Some(path) = self.paths.get(*cursor + fetched as usize) else {
                break;
            };
            // SAFETY: `CoTaskMemAlloc` returns suitably aligned memory the caller frees with
            // `CoTaskMemFree`; the buffer holds exactly the NUL-terminated path.
            let mem = unsafe { CoTaskMemAlloc(path.len() * std::mem::size_of::<u16>()) };
            if mem.is_null() {
                if !pceltfetched.is_null() {
                    // SAFETY: the caller promised a writable `u32` slot.
                    unsafe {
                        *pceltfetched = fetched;
                    }
                }
                return E_OUTOFMEMORY;
            }
            // SAFETY: `mem` is non-null with room for the whole path, and `rgelt` holds at
            // least `celt` writable slots per the `IEnumString` contract.
            unsafe {
                mem.cast::<u16>()
                    .copy_from_nonoverlapping(path.as_ptr(), path.len());
                rgelt.add(fetched as usize).write(PWSTR(mem.cast()));
            }
            fetched += 1;
        }
        *cursor += fetched as usize;
        if !pceltfetched.is_null() {
            // SAFETY: the caller promised a writable `u32` slot.
            unsafe {
                *pceltfetched = fetched;
            }
        }
        if fetched == celt {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, celt: u32) -> HRESULT {
        let mut cursor = match self.cursor.lock() {
            Ok(cursor) => cursor,
            Err(_) => return E_UNEXPECTED,
        };
        let advance = (celt as usize).min(self.paths.len().saturating_sub(*cursor));
        *cursor += advance;
        if advance == celt as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Reset(&self) -> Result<()> {
        let mut cursor = self
            .cursor
            .lock()
            .map_err(|_| windows::core::Error::from(E_UNEXPECTED))?;
        *cursor = 0;
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumString> {
        let cursor = self
            .cursor
            .lock()
            .map_err(|_| windows::core::Error::from(E_UNEXPECTED))?;
        Ok(JxlPathEnumerator::with_position(self.paths.clone(), *cursor).into())
    }
}

/// WIC bitmap frame decoder backed by a concrete JPEG XL container.
///
/// Crate-private: hosts see it only through the IWICBitmapFrameDecode interface, and the
/// implement macro's generated companion type inherits this visibility.
#[implement(IWICBitmapFrameDecode)]
pub(crate) struct JxlFrameDecode {
    /// Container holding the decoded frame.
    container: Arc<JxlContainer>,
    /// Zero-based index of the frame this wrapper decodes.
    index: usize,
}

impl JxlFrameDecode {
    /// Wraps one frame of the container; the index was validated by `GetFrame`.
    pub(crate) fn new(container: Arc<JxlContainer>, index: usize) -> Self {
        Self { container, index }
    }

    fn frame(&self) -> Result<JxlFrame<'_>> {
        self.container
            .frame(self.index)
            .map_err(|err| windows::core::Error::from(to_hresult(&err)))
    }
}

impl IWICBitmapSource_Impl for JxlFrameDecode_Impl {
    fn GetSize(&self, puiwidth: *mut u32, puiheight: *mut u32) -> Result<()> {
        if puiwidth.is_null() || puiheight.is_null() {
            return Err(E_POINTER.into());
        }
        let frame = self.frame()?;
        let info = frame.info();
        // SAFETY: both pointers were checked for null.
        unsafe {
            *puiwidth = info.width;
            *puiheight = info.height;
        }
        Ok(())
    }

    fn GetPixelFormat(&self) -> Result<GUID> {
        let frame = self.frame()?;
        Ok(pixel_format_guid(frame.info().pixel_format))
    }

    fn GetResolution(&self, pdpix: *mut f64, pdpiy: *mut f64) -> Result<()> {
        if pdpix.is_null() || pdpiy.is_null() {
            return Err(E_POINTER.into());
        }
        let frame = self.frame()?;
        let info = frame.info();
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
        let frame = self.frame()?;
        frame
            .copy_pixels(rect, cbstride as usize, buffer)
            .map_err(|err| windows::core::Error::from(to_hresult(&err)))
    }
}

impl IWICBitmapFrameDecode_Impl for JxlFrameDecode_Impl {
    fn GetMetadataQueryReader(&self) -> Result<IWICMetadataQueryReader> {
        // Validates the stored index so a stale wrapper reports FRAMEMISSING, not metadata.
        self.frame()?;
        if self.container.frame_count() <= 1 {
            // Still images carry no frame metadata; the e2e suite pins this error.
            return Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into());
        }
        Ok(JxlFrameMetadataReader::new(self.container.clone(), self.index).into())
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
        let frame = self.frame()?;
        // SAFETY: delegated pointer contract, documented on the WIC interface.
        unsafe { slot.write(frame.color_profile()) }
    }

    fn GetThumbnail(&self) -> Result<IWICBitmapSource> {
        // The decoder carries no embedded thumbnail; WIC downscales frame 0 instead.
        Err(WINCODEC_ERR_CODECNOTHUMBNAIL.into())
    }
}

/// WIC bitmap decoder for JPEG XL.
///
/// `Initialize` publishes the decoded container exactly once; later calls report
/// `WINCODEC_ERR_WRONGSTATE`, matching how WIC hosts treat a re-used decoder instance.
///
/// Crate-private: hosts see it only through the `IWICBitmapDecoder` interface, and the
/// `implement` macro's generated companion type inherits this visibility.
#[implement(IWICBitmapDecoder)]
pub(crate) struct JxlWicDecoder {
    /// Decoding container published by Initialize.
    container: OnceLock<Arc<JxlContainer>>,
}

impl JxlWicDecoder {
    /// Creates a decoder with no attached stream.
    pub(crate) fn new() -> Self {
        Self {
            container: OnceLock::new(),
        }
    }

    fn container(&self) -> Result<Arc<JxlContainer>> {
        self.container
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
        let stream = pistream.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        // SAFETY: `stream` is a live COM interface pointer for the duration of the call.
        let matches = unsafe { sniff_signatures(stream) }?;
        if matches {
            Ok(WICBitmapDecoderCapabilityCanDecodeAllImages.0 as u32)
        } else {
            Ok(0)
        }
    }

    fn Initialize(
        &self,
        pistream: Option<&IStream>,
        _cacheoptions: WICDecodeOptions,
    ) -> Result<()> {
        let stream = pistream.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        // SAFETY: `stream` is a live COM interface pointer for the duration of the call.
        let bytes = unsafe { ingest_stream(stream) }?;
        let container = JxlContainer::open(&bytes)
            .map_err(|err| windows::core::Error::from(to_hresult(&err)))?;
        self.container
            .set(Arc::new(container))
            .map_err(|_| windows::core::Error::from(WINCODEC_ERR_WRONGSTATE))?;
        Ok(())
    }

    fn GetContainerFormat(&self) -> Result<GUID> {
        Ok(GUID_ContainerFormatJxl)
    }

    fn GetDecoderInfo(&self) -> Result<IWICBitmapDecoderInfo> {
        // Hosts call this before decoding anything (WPF's `BitmapDecoder.Create`, for one), so a
        // stub here breaks them outright rather than degrading gracefully: they receive
        // `0x88982F81` and cannot distinguish "unsupported" from "corrupt image".
        //
        // WIC can describe a decoder from its own registry entries, so the factory builds the
        // component info for our CLSID and we hand it back as the decoder-specific interface. No
        // decoder instance is created, so this cannot recurse into `CreateInstance`.
        // SAFETY: WIC's factory is a registered in-process server, and the CLSID is a
        // `'static` constant valid for the duration of the call.
        let factory: IWICImagingFactory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
        let info = unsafe { factory.CreateComponentInfo(&CLSID_JxlrsWICDecoder) }?;
        info.cast::<IWICBitmapDecoderInfo>()
    }

    fn CopyPalette(&self, _pipalette: Option<&IWICPalette>) -> Result<()> {
        Err(WINCODEC_ERR_PALETTEUNAVAILABLE.into())
    }

    fn GetMetadataQueryReader(&self) -> Result<IWICMetadataQueryReader> {
        let container = self.container()?;
        if container.animation().is_none() {
            // Still images carry no animation header, so there is no loop metadata to serve;
            // the e2e suite pins this error.
            return Err(WINCODEC_ERR_UNSUPPORTEDOPERATION.into());
        }
        Ok(JxlDecoderMetadataReader::new(container).into())
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
        let slot = ColorContextSlot {
            capacity: ccount,
            contexts: ppicolorcontexts,
            actual_count: pcactualcount,
        };
        // The container has no colour context of its own, so it delegates to frame 0.
        let container = self.container()?;
        let frame = container
            .frame(0)
            .map_err(|err| windows::core::Error::from(to_hresult(&err)))?;
        // SAFETY: delegated pointer contract, documented on the WIC interface.
        unsafe { slot.write(frame.color_profile()) }
    }

    fn GetThumbnail(&self) -> Result<IWICBitmapSource> {
        Err(WINCODEC_ERR_CODECNOTHUMBNAIL.into())
    }

    fn GetFrameCount(&self) -> Result<u32> {
        let container = self.container()?;
        Ok(container.frame_count() as u32)
    }

    fn GetFrame(&self, index: u32) -> Result<IWICBitmapFrameDecode> {
        let container = self.container()?;
        // Validate frame index exists before creating the COM frame decode wrapper.
        let _ = container
            .frame(index as usize)
            .map_err(|err| windows::core::Error::from(to_hresult(&err)))?;
        let decode: IWICBitmapFrameDecode = JxlFrameDecode::new(container, index as usize).into();
        Ok(decode)
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
            to_hresult(&DecodeError::ParserPanic("x".into())),
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
        assert_eq!(
            to_hresult(&DecodeError::FrameIndexOutOfRange {
                index: 1,
                frame_count: 1
            }),
            WINCODEC_ERR_FRAMEMISSING
        );
    }

    #[test]
    fn converts_durations_to_centisecond_delays() {
        assert_eq!(delay_centiseconds(0.0), 0);
        assert_eq!(delay_centiseconds(-3.0), 0);
        assert_eq!(delay_centiseconds(f64::NAN), 0);
        assert_eq!(delay_centiseconds(100.0), 10);
        assert_eq!(delay_centiseconds(105.0), 11);
        assert_eq!(delay_centiseconds(14.9), 1);
        assert_eq!(delay_centiseconds(1.0e9), u16::MAX);
    }

    #[test]
    fn formats_loop_counts_as_decimal_text() {
        assert_eq!(loop_count_text(0), "0");
        assert_eq!(loop_count_text(3), "3");
        assert_eq!(loop_count_text(u32::MAX), u16::MAX.to_string());
    }

    #[test]
    fn encodes_enumerator_paths_as_nul_terminated_utf16() {
        assert_eq!(wide_nul("AB"), vec![65, 66, 0]);
        assert_eq!(wide_nul(FRAME_DELAY_PATH).last(), Some(&0));
    }

    #[test]
    fn reads_well_formed_query_names() {
        assert!(unsafe { read_query_name(&PCWSTR::null()) }.is_none());
        let wide = wide_nul(FRAME_DELAY_PATH);
        let queried = unsafe { read_query_name(&PCWSTR(wide.as_ptr())) };
        assert_eq!(queried.as_deref(), Some(FRAME_DELAY_PATH));
    }
}
