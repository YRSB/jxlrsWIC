//! Headless end-to-end tests that drive the *registered* WIC decoder through public COM APIs.
//!
//! These tests exercise the same path Windows Explorer and Direct2D take: memory stream in,
//! `CoCreateInstance` (or `IWICImagingFactory::CreateDecoderFromStream`), decoded pixels out.
//!
//! CI registers `jxlrs_wic.dll` with `regsvr32` before running the suite and sets
//! `JXLRS_REQUIRE_COM=1`, which turns a missing registration into a failure instead of a skip.
#![cfg(windows)]

use std::path::PathBuf;

// `Interface` is needed for the `IClassFactory::IID` associated constant.
use windows::core::{IUnknown, Interface, GUID};
use windows::Win32::Graphics::Imaging::{
    IWICBitmapDecoder, IWICBitmapFrameDecode, IWICImagingFactory,
    WICBitmapDecoderCapabilityCanDecodeAllImages, WICDecodeMetadataCacheOnDemand, WICRect,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IStream, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::SHCreateMemStream;

use jxlrs_wic::guids::{
    CLSID_JxlrsWICDecoder, CLSID_WICImagingFactory, GUID_ContainerFormatJxl,
    GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat64bppRGBA, GUID_WICPixelFormat64bppRGBAHalf,
};

const WINCODEC_ERR_CODECNOTHUMBNAIL: i32 = 0x88982F44_u32 as i32;
const WINCODEC_ERR_PALETTEUNAVAILABLE: i32 = 0x88982F45_u32 as i32;
const WINCODEC_ERR_UNSUPPORTEDOPERATION: i32 = 0x88982F81_u32 as i32;
const WINCODEC_ERR_WRONGSTATE: i32 = 0x88982F04_u32 as i32;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("missing fixture {}: {err}", path.display()))
}

/// Creates an in-memory COM stream over `bytes`.
fn memory_stream(bytes: &[u8]) -> IStream {
    // SAFETY: `bytes` outlives the call and SHCreateMemStream copies the buffer.
    unsafe { SHCreateMemStream(Some(bytes)) }.expect("SHCreateMemStream failed")
}

/// Instantiates a fresh decoder through `CoCreateInstance`, skipping if it is not registered.
fn create_decoder() -> Option<IWICBitmapDecoder> {
    // SAFETY: standard COM bootstrap on the current thread.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    // SAFETY: the CLSID is fixed and the interface is owned by the returned pointer.
    match unsafe {
        CoCreateInstance::<Option<&IUnknown>, IWICBitmapDecoder>(
            &CLSID_JxlrsWICDecoder,
            None,
            CLSCTX_INPROC_SERVER,
        )
    } {
        Ok(decoder) => Some(decoder),
        Err(err) => {
            let message = format!(
                "CLSID_JxlrsWICDecoder is not registered ({err}). \
                 Run `regsvr32 target\\<profile>\\jxlrs_wic.dll` (elevated) before the tests."
            );
            assert!(std::env::var_os("JXLRS_REQUIRE_COM").is_none(), "{message}");
            eprintln!("skipping COM end-to-end test: {message}");
            None
        }
    }
}

/// Creates WIC's own imaging factory.
fn create_factory() -> IWICImagingFactory {
    // SAFETY: WIC's factory is a stable in-process server.
    unsafe {
        CoCreateInstance::<Option<&IUnknown>, IWICImagingFactory>(
            &CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )
    }
    .expect("CoCreateInstance(IWICImagingFactory)")
}

/// Opens a decoder over a named fixture, asserting the container contract along the way.
fn open_fixture(decoder: &IWICBitmapDecoder, name: &str) -> IWICBitmapFrameDecode {
    let bytes = fixture(name);
    let stream = memory_stream(&bytes);

    // SAFETY: the stream stays alive for the duration of the call; the decoder buffers the
    // bitstream itself, so the stream may be released afterwards.
    unsafe {
        decoder
            .Initialize(&stream, WICDecodeMetadataCacheOnDemand)
            .unwrap_or_else(|err| panic!("Initialize({name}) failed: {err}"));
        assert_eq!(
            decoder.GetContainerFormat().expect("GetContainerFormat"),
            GUID_ContainerFormatJxl
        );
        assert_eq!(decoder.GetFrameCount().expect("GetFrameCount"), 1);
        decoder.GetFrame(0).expect("GetFrame(0)")
    }
}

/// Reads every pixel of `frame` into a tightly packed buffer of the frame's own stride.
fn read_pixels(frame: &IWICBitmapFrameDecode) -> (u32, u32, GUID, Vec<u8>) {
    let mut width = 0u32;
    let mut height = 0u32;
    // SAFETY: out-parameters are valid for the duration of the call.
    let format = unsafe {
        frame.GetSize(&mut width, &mut height).expect("GetSize");
        frame.GetPixelFormat().expect("GetPixelFormat")
    };
    let bytes_per_pixel = match format {
        f if f == GUID_WICPixelFormat32bppBGRA => 4,
        f if f == GUID_WICPixelFormat64bppRGBA || f == GUID_WICPixelFormat64bppRGBAHalf => 8,
        other => panic!("unexpected pixel format {other:?}"),
    };
    let stride = width as usize * bytes_per_pixel;
    let mut pixels = vec![0u8; stride * height as usize];
    // SAFETY: the buffer is sized for exactly `height` rows of `stride` bytes.
    unsafe {
        frame
            .CopyPixels(std::ptr::null(), stride as u32, &mut pixels)
            .expect("CopyPixels");
    }
    (width, height, format, pixels)
}

#[test]
fn co_create_instance_decodes_eight_bit_bgra() {
    let Some(decoder) = create_decoder() else {
        return;
    };
    let frame = open_fixture(&decoder, "srgb8_lossless.jxl");

    let (width, height, format, pixels) = read_pixels(&frame);
    assert_eq!((width, height), (3, 3));
    assert_eq!(format, GUID_WICPixelFormat32bppBGRA);
    assert_eq!(pixels[..4], [255, 0, 0, 255], "opaque blue in BGRA order");
    for pixel in pixels.as_chunks::<4>().0 {
        assert_eq!(pixel[3], 255, "opaque images must report full alpha");
    }

    let mut dpi_x = 0.0;
    let mut dpi_y = 0.0;
    // SAFETY: out-parameters are valid for the duration of the call.
    unsafe {
        frame
            .GetResolution(&mut dpi_x, &mut dpi_y)
            .expect("GetResolution")
    };
    assert!(dpi_x > 0.0 && dpi_y > 0.0);
}

#[test]
fn preserves_straight_alpha_from_the_bitstream() {
    let Some(decoder) = create_decoder() else {
        return;
    };
    let frame = open_fixture(&decoder, "alpha_lossless.jxl");

    let (_, _, format, pixels) = read_pixels(&frame);
    assert_eq!(format, GUID_WICPixelFormat32bppBGRA);
    assert_eq!(&pixels[4..8], &[0, 255, 0, 128], "green at 50% alpha");
    assert_eq!(pixels[11], 0, "third pixel is fully transparent");
}

#[test]
fn factory_create_decoder_from_stream_finds_the_decoder() {
    let Some(_) = create_decoder() else {
        return;
    };
    let factory = create_factory();

    let bytes = fixture("srgb8_lossless.jxl");
    let stream = memory_stream(&bytes);
    // SAFETY: the stream outlives the decoder, which is initialized before the stream drops.
    let decoder = unsafe {
        factory
            .CreateDecoderFromStream(
                Some(&stream),
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            )
            .expect("WIC could not find a decoder for JPEG XL")
    };
    // SAFETY: the decoder was created successfully by WIC.
    unsafe {
        assert_eq!(
            decoder.GetContainerFormat().expect("GetContainerFormat"),
            GUID_ContainerFormatJxl
        );
        let frame = decoder.GetFrame(0).expect("GetFrame(0)");
        let (width, height, format, _) = read_pixels(&frame);
        assert_eq!(
            (width, height, format),
            (3, 3, GUID_WICPixelFormat32bppBGRA)
        );
    }
}

#[test]
fn copy_pixels_honours_sub_rectangles_and_padded_strides() {
    let Some(decoder) = create_decoder() else {
        return;
    };
    let frame = open_fixture(&decoder, "srgb8_lossless.jxl");

    // One pixel at (1, 0) with a stride 4 bytes wider than the pixel itself.
    let rect = WICRect {
        X: 1,
        Y: 0,
        Width: 1,
        Height: 1,
    };
    let mut stride_buf = [0u8; 8];
    // SAFETY: the buffer holds one full stride of writable bytes.
    unsafe {
        frame
            .CopyPixels(&rect, 8, &mut stride_buf)
            .expect("CopyPixels(sub-rect)");
    }
    assert_eq!(&stride_buf[..4], &[0, 255, 0, 255], "pixel (1,0) is green");
    assert_eq!(&stride_buf[4..], &[0; 4], "the padding must be untouched");

    // Out-of-bounds rectangles must be rejected rather than clipped silently.
    let outside = WICRect {
        X: 2,
        Y: 0,
        Width: 2,
        Height: 1,
    };
    // SAFETY: the buffer is valid for the call.
    let outside_result = unsafe { frame.CopyPixels(&outside, 8, &mut stride_buf) };
    assert!(outside_result.is_err(), "out-of-bounds rectangle must fail");

    // A buffer shorter than one row of 12 bytes must be reported as insufficient.
    let mut too_small = [0u8; 8];
    // SAFETY: the buffer is deliberately undersized.
    let short_result = unsafe { frame.CopyPixels(std::ptr::null(), 12, &mut too_small) };
    assert!(short_result.is_err(), "undersized buffer must fail");
}

#[test]
fn decodes_sixteen_bit_and_half_float_formats() {
    let Some(decoder) = create_decoder() else {
        return;
    };
    let frame = open_fixture(&decoder, "rgb16_lossless.jxl");
    let (_, _, format, pixels) = read_pixels(&frame);
    assert_eq!(format, GUID_WICPixelFormat64bppRGBA);
    assert_eq!(
        u16::from_le_bytes([pixels[0], pixels[1]]),
        u16::MAX,
        "top-left pixel is opaque red at full 16-bit precision"
    );

    // A decoder instance accepts one stream, so the HDR fixture gets its own instance.
    let Some(decoder) = create_decoder() else {
        return;
    };
    let frame = open_fixture(&decoder, "hdr_pq.jxl");
    let (_, _, format, pixels) = read_pixels(&frame);
    assert_eq!(format, GUID_WICPixelFormat64bppRGBAHalf);
    let alpha = u16::from_le_bytes([pixels[6], pixels[7]]);
    assert_eq!(alpha, 0x3C00, "opaque alpha is binary16(1.0)");
    let red = u16::from_le_bytes([pixels[0], pixels[1]]);
    let green = u16::from_le_bytes([pixels[2], pixels[3]]);
    let blue = u16::from_le_bytes([pixels[4], pixels[5]]);
    assert!(
        red > green && red > blue,
        "PQ fixture starts with saturated red"
    );
}

#[test]
fn exposes_color_contexts_that_wic_accepts() {
    let Some(decoder) = create_decoder() else {
        return;
    };
    let factory = create_factory();
    let frame = open_fixture(&decoder, "icc_embedded.jxl");

    let mut count = 0u32;
    // SAFETY: a null array with a zero count asks only for the number of contexts.
    unsafe {
        frame
            .GetColorContexts(&mut [], &mut count)
            .expect("GetColorContexts(count only)");
    }
    assert_eq!(
        count, 1,
        "pcuactualcount must report the single ICC profile"
    );

    let mut contexts = [None];
    let mut written = 0u32;
    // SAFETY: the array is large enough for the single reported context.
    unsafe {
        frame
            .GetColorContexts(&mut contexts, &mut written)
            .expect("GetColorContexts");
    }
    assert_eq!(written, 1);
    let context = contexts[0].take().expect("color context");

    let mut required = 0u32;
    // SAFETY: an empty buffer asks for the profile length only.
    unsafe {
        context
            .GetProfileBytes(&mut [], &mut required)
            .expect("GetProfileBytes(size)");
    }
    assert!(required > 128, "ICC profile is suspiciously short");
    let mut profile = vec![0u8; required as usize];
    // SAFETY: the buffer is sized exactly as reported.
    unsafe {
        context
            .GetProfileBytes(&mut profile, &mut required)
            .expect("GetProfileBytes(data)");
    }
    assert_eq!(&profile[36..40], b"acsp", "ICC header magic");

    // WIC itself must accept the profile bytes.
    let fresh = unsafe { factory.CreateColorContext() }.expect("CreateColorContext");
    // SAFETY: the slice is valid for its own length.
    unsafe {
        fresh
            .InitializeFromMemory(&profile)
            .expect("WIC rejected the ICC profile");
    }
}

#[test]
fn decoder_get_color_contexts_delegates_to_the_frame() {
    let Some(decoder) = create_decoder() else {
        return;
    };
    let _frame = open_fixture(&decoder, "icc_embedded.jxl");

    let mut count = 0u32;
    // SAFETY: a null array with a zero count asks only for the number of contexts.
    unsafe {
        decoder
            .GetColorContexts(&mut [], &mut count)
            .expect("GetColorContexts(count only) on the decoder");
    }
    assert_eq!(count, 1, "the decoder must report the frame's ICC profile");
}

#[test]
fn describes_itself_as_a_bitmap_decoder() {
    let Some(decoder) = create_decoder() else {
        return;
    };

    // SAFETY: the call only reads decoder state and returns an owned interface.
    let info = unsafe { decoder.GetDecoderInfo() }.expect("GetDecoderInfo");

    // Every accessor below is answered by WIC from the registry keys our `DllRegisterServer`
    // wrote, so this test also guards that registration and the component info agree.
    let mut name_buffer = [0u16; 128];
    let mut name_length = 0u32;
    // SAFETY: the buffer and out-parameter are valid for the call.
    unsafe { info.GetFriendlyName(&mut name_buffer, &mut name_length) }.expect("GetFriendlyName");
    let friendly_name = String::from_utf16_lossy(&name_buffer[..name_length as usize]);
    assert!(
        friendly_name.contains("jxlrsWIC"),
        "unexpected FriendlyName: {friendly_name}"
    );

    // SAFETY: read-only accessor.
    let container = unsafe { info.GetContainerFormat() }.expect("GetContainerFormat");
    assert_eq!(container, GUID_ContainerFormatJxl);

    // `Formats` is what registration writes; a decoder info with none would fail WIC's own
    // format negotiation before our decoder is ever instantiated.
    //
    // Pass a real buffer rather than the documented NULL size query: the Windows binding models
    // the parameter as `&mut [GUID]`, so `&mut []` becomes a dangling non-NULL pointer, and WIC
    // answers 0 for that instead of the count it would report for NULL.
    let mut formats = [GUID::from_u128(0); 16];
    let mut format_count = 0u32;
    // SAFETY: the buffer and out-parameter are valid for the call.
    unsafe { info.GetPixelFormats(&mut formats, &mut format_count) }.expect("GetPixelFormats");
    assert!(format_count > 0, "decoder info advertises no pixel formats");

    let formats = &formats[..format_count as usize];
    assert!(
        formats.contains(&GUID_WICPixelFormat32bppBGRA),
        "32bppBGRA is missing from the registered formats: {formats:?}"
    );

    // `SupportLossless` is registered as 1, so this must agree.
    // SAFETY: read-only accessor.
    assert!(unsafe { info.DoesSupportLossless() }
        .expect("DoesSupportLossless")
        .as_bool());
}

#[test]
fn reports_capabilities_and_rejects_unsupported_operations() {
    let Some(decoder) = create_decoder() else {
        return;
    };

    let bytes = fixture("srgb8_lossless.jxl");
    let stream = memory_stream(&bytes);
    // SAFETY: the stream is alive for the call.
    let capability = unsafe { decoder.QueryCapability(Some(&stream)) }.expect("QueryCapability");
    assert_eq!(
        capability,
        WICBitmapDecoderCapabilityCanDecodeAllImages.0 as u32
    );

    let frame = open_fixture(&decoder, "srgb8_lossless.jxl");
    // SAFETY: these calls only read decoder and frame state.
    unsafe {
        assert_eq!(
            decoder.GetThumbnail().err().map(|err| err.code().0),
            Some(WINCODEC_ERR_CODECNOTHUMBNAIL)
        );
        assert_eq!(
            decoder.GetPreview().err().map(|err| err.code().0),
            Some(WINCODEC_ERR_UNSUPPORTEDOPERATION)
        );
        assert_eq!(
            decoder.CopyPalette(None).err().map(|err| err.code().0),
            Some(WINCODEC_ERR_PALETTEUNAVAILABLE)
        );
        assert!(decoder.GetMetadataQueryReader().is_err());

        assert_eq!(
            frame.GetThumbnail().err().map(|err| err.code().0),
            Some(WINCODEC_ERR_CODECNOTHUMBNAIL)
        );
        assert_eq!(
            frame.GetMetadataQueryReader().err().map(|err| err.code().0),
            Some(WINCODEC_ERR_UNSUPPORTEDOPERATION)
        );
        assert_eq!(
            frame.CopyPalette(None).err().map(|err| err.code().0),
            Some(WINCODEC_ERR_PALETTEUNAVAILABLE)
        );
    }
}

#[test]
fn queries_signatures_without_claiming_foreign_streams() {
    let Some(decoder) = create_decoder() else {
        return;
    };

    let foreign = b"not a jpeg xl stream at all".to_vec();
    let stream = memory_stream(&foreign);
    // SAFETY: the stream is alive for the call.
    let capability = unsafe { decoder.QueryCapability(Some(&stream)) }.expect("QueryCapability");
    assert_eq!(capability, 0, "foreign streams must not be claimed");

    for signature in [
        vec![
            0x00u8, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        ],
        vec![0xFFu8, 0x0A],
    ] {
        let stream = memory_stream(&signature);
        // SAFETY: the stream is alive for the call.
        let capability =
            unsafe { decoder.QueryCapability(Some(&stream)) }.expect("QueryCapability");
        assert_eq!(
            capability, WICBitmapDecoderCapabilityCanDecodeAllImages.0 as u32,
            "both JPEG XL signatures must be recognised"
        );
    }
}

#[test]
fn returns_wrong_state_before_initialize() {
    let Some(decoder) = create_decoder() else {
        return;
    };
    // SAFETY: `GetFrameCount` only reads decoder state.
    let result = unsafe { decoder.GetFrameCount() };
    assert_eq!(
        result.err().map(|err| err.code().0),
        Some(WINCODEC_ERR_WRONGSTATE),
        "an uninitialized decoder must report WINCODEC_ERR_WRONGSTATE"
    );
}

#[test]
fn class_object_is_created_for_the_registered_clsid() {
    if create_decoder().is_none() {
        return;
    }
    let mut factory: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `DllGetClassObject` is a stable export and the out-pointer is valid.
    let hr = unsafe {
        jxlrs_wic::DllGetClassObject(
            &CLSID_JxlrsWICDecoder,
            &windows::Win32::System::Com::IClassFactory::IID,
            &mut factory,
        )
    };
    assert_eq!(hr, windows::Win32::Foundation::S_OK);
    assert!(!factory.is_null());
}

/// Ensures the test binary sees the same GUIDs WIC uses, catching accidental GUID drift.
#[test]
fn guids_match_the_published_contract() {
    assert_eq!(
        GUID_WICPixelFormat32bppBGRA,
        GUID::from_u128(0x6FDDC324_4E03_4BFE_B185_3D77768DC90F)
    );
    assert_eq!(
        GUID_WICPixelFormat64bppRGBA,
        GUID::from_u128(0x6FDDC324_4E03_4BFE_B185_3D77768DC916)
    );
    assert_eq!(
        GUID_WICPixelFormat64bppRGBAHalf,
        GUID::from_u128(0x6FDDC324_4E03_4BFE_B185_3D77768DC93A)
    );
    assert_eq!(
        GUID_ContainerFormatJxl,
        GUID::from_u128(0xFEC14E3F_427A_4736_AAE6_27ED84F69322)
    );
}
