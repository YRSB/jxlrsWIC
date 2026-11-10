//! Minimal WIC bitmap decoder prototype implementation for JPEG XL.

#![allow(missing_docs)]

use crate::guids::GUID_ContainerFormatJxl;
use windows::core::{implement, Result, GUID, HRESULT};
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Graphics::Imaging::{
    IWICBitmapDecoder, IWICBitmapDecoderInfo, IWICBitmapDecoder_Impl, IWICBitmapFrameDecode,
    IWICBitmapSource, IWICColorContext, IWICMetadataQueryReader, IWICPalette, WICDecodeOptions,
};
use windows::Win32::System::Com::IStream;

// WIC Bitmap Decoder Capability flags
const WIC_BITMAP_DECODER_CAPABILITY_CAN_DECODE_ALL_IMAGES: u32 = 0x4;

// WIC error HRESULT definitions
const WINCODEC_ERR_PALETTEUNAVAILABLE: HRESULT = HRESULT(0x88982F0C_u32 as i32);
const WINCODEC_ERR_UNSUPPORTEDOPERATION: HRESULT = HRESULT(0x88982F8B_u32 as i32);
const WINCODEC_ERR_CODECNOTHUMBNAIL: HRESULT = HRESULT(0x88982F87_u32 as i32);

/// Minimal JPEG XL WIC decoder COM class.
#[implement(IWICBitmapDecoder)]
pub struct JxlWicDecoder;

impl JxlWicDecoder {
    /// Creates a new minimal decoder instance.
    pub fn new() -> Self {
        Self
    }
}

impl Default for JxlWicDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl IWICBitmapDecoder_Impl for JxlWicDecoder_Impl {
    fn QueryCapability(&self, _pistream: Option<&IStream>) -> Result<u32> {
        Ok(WIC_BITMAP_DECODER_CAPABILITY_CAN_DECODE_ALL_IMAGES)
    }

    fn Initialize(
        &self,
        _pistream: Option<&IStream>,
        _cacheoptions: WICDecodeOptions,
    ) -> Result<()> {
        Ok(())
    }

    fn GetContainerFormat(&self) -> Result<GUID> {
        Ok(GUID_ContainerFormatJxl)
    }

    fn GetDecoderInfo(&self) -> Result<IWICBitmapDecoderInfo> {
        Err(E_NOTIMPL.into())
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
        _ccount: u32,
        _ppicolorcontexts: *mut Option<IWICColorContext>,
        pcuactualcount: *mut u32,
    ) -> Result<()> {
        if !pcuactualcount.is_null() {
            unsafe { *pcuactualcount = 0 };
        }
        Ok(())
    }

    fn GetThumbnail(&self) -> Result<IWICBitmapSource> {
        Err(WINCODEC_ERR_CODECNOTHUMBNAIL.into())
    }

    fn GetFrameCount(&self) -> Result<u32> {
        Ok(1)
    }

    fn GetFrame(&self, _pindex: u32) -> Result<IWICBitmapFrameDecode> {
        Err(E_NOTIMPL.into())
    }
}
