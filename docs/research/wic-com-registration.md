# Research: WIC COM Server Registration Requirements and GUID Topology

## 1. Executive Summary & Architecture Overview

Windows Imaging Component (WIC) is the extensible imaging platform introduced in Windows Vista and maintained through modern Windows 11. It provides a unified COM-based architecture for discovery, metadata extraction, format conversion, and pixel decoding/encoding across image formats.

To integrate `jxlrsWIC` as a native Windows image decoder, it must be compiled as an in-process COM server (`cdylib`) implementing standard COM lifecycle exports and the `IWICBitmapDecoder` interface. The operating system, Windows Explorer, and client applications discover and prioritize the decoder via structured Windows Registry entries under `HKEY_CLASSES_ROOT` (HKCR) and category tables defined by `CATID_WICBitmapDecoders`.

This document specifies the exact GUID topology, registry schema, bitstream sniffing patterns, Windows Explorer / Windows Photo Viewer shell bindings, and pure-Rust implementation blueprints required to support JPEG XL (`.jxl`) natively.

---

## 2. GUID Topology

COM and WIC rely on standard 128-bit globally unique identifiers (GUIDs) to decouple interface definitions, implementation classes, component categories, and container formats.

### 2.1 Core Component GUIDs

| Identifier | Symbolic Name | GUID String / Value | Description & Origin |
|---|---|---|---|
| **Category ID** | `CATID_WICBitmapDecoders` | `{7ED96837-96F0-4812-B211-F13C24117ED3}` | Standard WIC component category identifier for all bitmap decoders. Defined in `wincodec.h` / `windows::Win32::Graphics::Imaging::CATID_WICBitmapDecoders`. |
| **Container Format** | `GUID_ContainerFormatJxl` | `{FEC14E3F-427A-4736-AAE6-27ED84F69322}` | Official Microsoft WIC Container Format GUID for JPEG XL (introduced in Windows 11 `windowscodecs.dll`). |
| **Decoder CLSID** | `CLSID_JxlrsWICDecoder` | `{466C040F-E9A9-45F4-BE8A-BF0386B15E28}` | Implementation Class ID assigned to `jxlrsWIC`'s in-process decoder server. |
| **Vendor ID** | `GUID_VendorYRSB` | `{AE1ACFC8-AF90-46DA-A9E3-FC759EA12407}` | Vendor GUID assigned to YRSB for `jxlrsWIC`. |

### 2.2 Supported Output Pixel Format GUIDs

WIC decoders declare supported pixel formats under the `Formats` subkey of their CLSID. This allows WIC's format negotiation and converter pipeline (`IWICFormatConverter`) to query whether a frame can decode directly to a requested target:

| Symbolic Name | GUID String | Bit Depth / Layout |
|---|---|---|
| `GUID_WICPixelFormat24bppRGB` | `{6FDDC324-4E03-4BFE-B185-3D77768DC90D}` | 24 bpp, 8-bit unsigned RGB |
| `GUID_WICPixelFormat24bppBGR` | `{6FDDC324-4E03-4BFE-B185-3D77768DC90C}` | 24 bpp, 8-bit unsigned BGR |
| `GUID_WICPixelFormat32bppRGBA` | `{6FDDC324-4E03-4BFE-B185-3D77768DC916}` | 32 bpp, 8-bit unsigned RGBA |
| `GUID_WICPixelFormat32bppBGRA` | `{6FDDC324-4E03-4BFE-B185-3D77768DC90F}` | 32 bpp, 8-bit unsigned BGRA (standard Windows GDI/Direct2D) |
| `GUID_WICPixelFormat32bppRGBAFloat` | `{6FDDC324-4E03-4BFE-B185-3D77768DC920}` | 32 bpp (128 bpp total), 32-bit float RGBA (HDR workflows) |
| `GUID_WICPixelFormat48bppRGB` | `{6FDDC324-4E03-4BFE-B185-3D77768DC915}` | 48 bpp, 16-bit unsigned RGB |
| `GUID_WICPixelFormat64bppRGBA` | `{6FDDC324-4E03-4BFE-B185-3D77768DC917}` | 64 bpp, 16-bit unsigned RGBA |
| `GUID_WICPixelFormat8bppGray` | `{6FDDC324-4E03-4BFE-B185-3D77768DC908}` | 8 bpp grayscale |
| `GUID_WICPixelFormat16bppGray` | `{6FDDC324-4E03-4BFE-B185-3D77768DC90B}` | 16 bpp grayscale |

### 2.3 Shell Integration GUIDs

| Identifier | Symbolic Name | GUID String | Purpose |
|---|---|---|---|
| **Interface** | `IID_IThumbnailProvider` | `{E357FCCD-A995-4576-B01F-234630154E96}` | Windows Shell thumbnail generation interface. |
| **CLSID** | `CLSID_PhotoThumbnailProvider` | `{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}` | Microsoft standard Photo Thumbnail Provider; delegates directly to registered WIC decoders. |
| **Interface** | `IID_IExtractImage` | `{BB2E617C-0920-11D1-9A0B-00C04FC2D6C1}` | Legacy Windows Shell thumbnail extraction interface. |
| **CLSID** | `CLSID_PhotoExtractImage` | `{3F30C968-480A-4C6C-862D-EFC0897BB84B}` | Microsoft standard legacy photo extractor handler. |
| **CLSID** | `CLSID_PhotoViewerDropTarget` | `{FFE2A43C-56B9-4BF5-9A79-CC6D4285608A}` | Windows Photo Viewer image verb and drop-target handler. |

---

## 3. Bitstream Sniffing Patterns

WIC decoders do not merely bind to file extensions; WIC performs run-time stream identification (`IWICImagingFactory::CreateDecoderFromStream`) by inspecting raw stream byte sequences against registered `Patterns`.

Per ISO/IEC 18181 (JPEG XL specification), JPEG XL bitstreams exist in two primary encapsulations: **ISO BMFF Box Container** and **Bare / Naked Codestream**. Both must be registered.

```
       JPEG XL Stream Type Detection
       ┌───────────────────────────┐
       │   Examine first 24 bytes  │
       └─────────────┬─────────────┘
                     │
       ┌─────────────┴─────────────┐
       ▼                           ▼
[Pattern 0: ISO BMFF Box]     [Pattern 1: Naked Codestream]
Starts with:                  Starts with:
00 00 00 0C                   FF 0A
4A 58 4C 20 ("JXL ")
0D 0A 87 0A
XX XX XX XX (ftyp box size)
66 74 79 70 ("ftyp")
6A 78 6C 20 ("jxl ")
```

### Pattern 0: ISO Base Media File Format (BMFF) Container

Files formatted as ISO BMFF containers contain a 12-byte JPEG XL Signature Box followed immediately by a File Type Box (`ftyp`) declaring the major brand `jxl `.

* **Position**: `0` (offset from start of stream)
* **Length**: `24` bytes (0x18)
* **EndOfStream**: `0`
* **Pattern**:
  ```text
  00 00 00 0C 4A 58 4C 20 0D 0A 87 0A 00 00 00 00 66 74 79 70 6A 78 6C 20
  ```
  Breakdown:
  - `00 00 00 0C`: 4-byte box length = 12 bytes
  - `4A 58 4C 20`: ASCII `"JXL "` (Box Type)
  - `0D 0A 87 0A`: Box content (`\r\n\x87\n` — verifies binary integrity across text and 7-bit channels)
  - `00 00 00 00`: Wildcard 4-byte box length for the subsequent `ftyp` box
  - `66 74 79 70`: ASCII `"ftyp"`
  - `6A 78 6C 20`: ASCII `"jxl "` (Major Brand)
* **Mask**:
  ```text
  FF FF FF FF FF FF FF FF FF FF FF FF 00 00 00 00 FF FF FF FF FF FF FF FF
  ```
  *(Bytes 12..15 are masked with `00 00 00 00` because the `ftyp` box length varies depending on additional compatible brands.)*

### Pattern 1: Naked / Bare Codestream

JPEG XL images without ISO BMFF encapsulation start directly with the 2-byte codestream marker:

* **Position**: `0`
* **Length**: `2` bytes
* **EndOfStream**: `0`
* **Pattern**:
  ```text
  FF 0A
  ```
* **Mask**:
  ```text
  FF FF
  ```

---

## 4. Full Windows Registry Specification

WIC decoder registration requires writing to two distinct branches under `HKEY_CLASSES_ROOT`:
1. The decoder's own `CLSID` subkey defining component metadata, library path, supported formats, and sniffing patterns.
2. The WIC component category subkey (`CATID_WICBitmapDecoders`) registering the CLSID instance into WIC's runtime enumeration table.

### 4.1 Registry Key Topology Diagram

```
HKEY_CLASSES_ROOT
├── CLSID
│   ├── {466C040F-E9A9-45F4-BE8A-BF0386B15E28}              <-- CLSID_JxlrsWICDecoder
│   │   ├── (Default) = "jxlrsWIC JPEG XL Decoder"
│   │   ├── ArbitrationPriority = dword:00000000
│   │   ├── Author = "YRSB"
│   │   ├── ContainerFormat = "{FEC14E3F-427A-4736-AAE6-27ED84F69322}"
│   │   ├── Description = "jxlrsWIC JPEG XL WIC Bitmap Decoder"
│   │   ├── FileExtensions = ".jxl"
│   │   ├── FriendlyName = "jxlrsWIC JPEG XL Decoder"
│   │   ├── MimeTypes = "image/jxl"
│   │   ├── SpecVersion = "1.0.0.0"
│   │   ├── SupportAnimation = dword:00000001
│   │   ├── SupportLossless = dword:00000001
│   │   ├── SupportMultiframe = dword:00000001
│   │   ├── Vendor = "{AE1ACFC8-AF90-46DA-A9E3-FC759EA12407}"
│   │   ├── Version = "0.1.0.0"
│   │   ├── InprocServer32
│   │   │   ├── (Default) = "C:\path\to\jxlrsWIC.dll"
│   │   │   └── ThreadingModel = "Both"
│   │   ├── Formats
│   │   │   ├── {6FDDC324-4E03-4BFE-B185-3D77768DC90D}       <-- 24bppRGB
│   │   │   ├── {6FDDC324-4E03-4BFE-B185-3D77768DC90F}       <-- 32bppBGRA
│   │   │   └── ...
│   │   └── Patterns
│   │       ├── 0
│   │       │   ├── Position = dword:00000000
│   │       │   ├── Length = dword:00000018
│   │       │   ├── EndOfStream = dword:00000000
│   │       │   ├── Pattern = hex:00,00,00,0c,4a,58,4c,20,0d,0a,87,0a,00,00,00,00,66,74,79,70,6a,78,6c,20
│   │       │   └── Mask = hex:ff,ff,ff,ff,ff,ff,ff,ff,ff,ff,ff,ff,00,00,00,00,ff,ff,ff,ff,ff,ff,ff,ff
│   │       └── 1
│   │           ├── Position = dword:00000000
│   │           ├── Length = dword:00000002
│   │           ├── EndOfStream = dword:00000000
│   │           ├── Pattern = hex:ff,0a
│   │           └── Mask = hex:ff,ff
│   │
│   └── {7ED96837-96F0-4812-B211-F13C24117ED3}              <-- CATID_WICBitmapDecoders
│       └── Instance
│           └── {466C040F-E9A9-45F4-BE8A-BF0386B15E28}
│               ├── CLSID = "{466C040F-E9A9-45F4-BE8A-BF0386B15E28}"
│               └── FriendlyName = "jxlrsWIC JPEG XL Decoder"
│
├── .jxl
│   ├── (Default) = "jxlfile"
│   ├── Content Type = "image/jxl"
│   ├── PerceivedType = "image"
│   └── ShellEx
│       └── {e357fccd-a995-4576-b01f-234630154e96} = "{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}"
│
└── SystemFileAssociations
    └── .jxl
        ├── PerceivedType = "image"
        ├── ImageOptionFlags = dword:00000001
        └── ShellEx
            ├── {e357fccd-a995-4576-b01f-234630154e96} = "{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}"
            └── {BB2E617C-0920-11d1-9A0B-00C04FC2D6C1} = "{3F30C968-480A-4C6C-862D-EFC0897BB84B}"
```

### 4.2 Critical Key Semantics

- **`ArbitrationPriority` (`REG_DWORD`)**: Introduced in Windows 8. Values range from `0` (highest priority) to `10` (lowest). Microsoft's built-in stub decoder in Windows 11 registers with priority `10`. By registering `jxlrsWIC` with priority `0`, WIC guarantees our decoder takes precedence when parsing `.jxl` streams.
- **`ThreadingModel = "Both"`**: WIC decoders must support free-threaded and apartment-threaded callers. Starting in Windows 7, WIC rejects or degrades codecs not marked as `"Both"`.
- **`ContainerFormat`**: Must match `GUID_ContainerFormatJxl` (`{FEC14E3F-427A-4736-AAE6-27ED84F69322}`).

### 4.3 Reference `.reg` Script

```ini
Windows Registry Editor Version 5.00

; ==============================================================================
; 1. COM Class Registration
; ==============================================================================
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}]
@="jxlrsWIC JPEG XL Decoder"
"ArbitrationPriority"=dword:00000000
"Author"="YRSB"
"ColorManagementVersion"="1.0.0.0"
"ContainerFormat"="{FEC14E3F-427A-4736-AAE6-27ED84F69322}"
"Description"="jxlrsWIC JPEG XL WIC Bitmap Decoder"
"FileExtensions"=".jxl"
"FriendlyName"="jxlrsWIC JPEG XL Decoder"
"MimeTypes"="image/jxl"
"SpecVersion"="1.0.0.0"
"SupportAnimation"=dword:00000001
"SupportChromaKey"=dword:00000000
"SupportLossless"=dword:00000001
"SupportMultiframe"=dword:00000001
"Vendor"="{AE1ACFC8-AF90-46DA-A9E3-FC759EA12407}"
"Version"="0.1.0.0"

[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\InprocServer32]
@="C:\\Program Files\\jxlrsWIC\\jxlrsWIC.dll"
"ThreadingModel"="Both"

; Supported Pixel Formats
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Formats\{6FDDC324-4E03-4BFE-B185-3D77768DC90D}]
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Formats\{6FDDC324-4E03-4BFE-B185-3D77768DC90C}]
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Formats\{6FDDC324-4E03-4BFE-B185-3D77768DC916}]
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Formats\{6FDDC324-4E03-4BFE-B185-3D77768DC90F}]
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Formats\{6FDDC324-4E03-4BFE-B185-3D77768DC917}]
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Formats\{6FDDC324-4E03-4BFE-B185-3D77768DC915}]

; Pattern 0: ISO BMFF Box Container
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Patterns\0]
"Position"=dword:00000000
"Length"=dword:00000018
"EndOfStream"=dword:00000000
"Pattern"=hex:00,00,00,0c,4a,58,4c,20,0d,0a,87,0a,00,00,00,00,66,74,79,70,6a,78,6c,20
"Mask"=hex:ff,ff,ff,ff,ff,ff,ff,ff,ff,ff,ff,ff,00,00,00,00,ff,ff,ff,ff,ff,ff,ff,ff

; Pattern 1: Naked Codestream
[HKEY_CLASSES_ROOT\CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}\Patterns\1]
"Position"=dword:00000000
"Length"=dword:00000002
"EndOfStream"=dword:00000000
"Pattern"=hex:ff,0a
"Mask"=hex:ff,ff

; ==============================================================================
; 2. WIC Category Registration (CATID_WICBitmapDecoders)
; ==============================================================================
[HKEY_CLASSES_ROOT\CLSID\{7ED96837-96F0-4812-B211-F13C24117ED3}\Instance\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}]
"CLSID"="{466C040F-E9A9-45F4-BE8A-BF0386B15E28}"
"FriendlyName"="jxlrsWIC JPEG XL Decoder"

; ==============================================================================
; 3. File Extension & Shell Thumbnail Cache Registration
; ==============================================================================
[HKEY_CLASSES_ROOT\.jxl]
@="jxlfile"
"Content Type"="image/jxl"
"PerceivedType"="image"

[HKEY_CLASSES_ROOT\.jxl\ShellEx\{e357fccd-a995-4576-b01f-234630154e96}]
@="{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}"

[HKEY_CLASSES_ROOT\SystemFileAssociations\.jxl]
"PerceivedType"="image"
"ImageOptionFlags"=dword:00000001

[HKEY_CLASSES_ROOT\SystemFileAssociations\.jxl\ShellEx\{e357fccd-a995-4576-b01f-234630154e96}]
@="{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}"

[HKEY_CLASSES_ROOT\SystemFileAssociations\.jxl\ShellEx\{BB2E617C-0920-11d1-9A0B-00C04FC2D6C1}]
@="{3F30C968-480A-4C6C-862D-EFC0897BB84B}"
```

---

## 5. Shell Integration & Windows Photo Viewer

### 5.1 Windows Explorer Thumbnail Architecture

Windows Explorer does not decode images directly; it calls the Windows Shell Thumbnail Cache subsystem:

```
[Windows Explorer]
       │
       ▼
[IThumbnailCache / IThumbnailProvider]
       │ (queries .jxl ShellEx {e357fccd-...})
       ▼
[Microsoft Photo Thumbnail Provider ({C7657C4A-...})]
       │
       ▼
[IWICImagingFactory::CreateDecoderFromStream]
       │ (sniffs Pattern 0 / Pattern 1)
       ▼
[jxlrsWIC Decoder ({466C040F-...})]
       │
       ├──> Calls IWICBitmapDecoder::GetThumbnail(&pThumbnail)
       │    └── Fast path: Embedded preview/thumbnail frame
       │
       └──> Fallback if WINCODEC_ERR_CODECNOTHUMBNAIL:
            └── Calls IWICBitmapDecoder::GetFrame(0, &pFrame)
                 └── Scaled via IWICBitmapScaler to requested dimensions
```

1. **Automatic WIC Delegation**: By associating `.jxl` with `{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}` (`Photo Thumbnail Provider`), Windows Explorer delegates all thumbnail rendering directly to the WIC decoding stack. We do **not** need to author a separate `IThumbnailProvider` COM component.
2. **Fast Thumbnail Path**: When `IWICBitmapDecoder::GetThumbnail` returns an embedded thumbnail, Explorer displays it instantly without decoding full resolution frames. If no thumbnail exists, returning `WINCODEC_ERR_CODECNOTHUMBNAIL` causes Explorer to invoke `GetFrame(0)` and scale the full raster.
3. **Cache Flushing Requirement**: After modifying registry keys during installation, the installer or `DllRegisterServer` must call:
   ```cpp
   SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, NULL, NULL);
   ```
   This invalidates the shell's icon and thumbnail caches, forcing immediate visual updates for `.jxl` files without restarting `explorer.exe`.

### 5.2 Windows Photo Viewer Integration

Windows Photo Viewer (`photoviewer.dll`) is a built-in WIC host application. In modern Windows versions, it is hidden by default for non-TIFF formats. To register Windows Photo Viewer for `.jxl`:

1. Define the ProgID `PhotoViewer.FileAssoc.Jxl` in `HKCR`:
   ```ini
   [HKEY_CLASSES_ROOT\PhotoViewer.FileAssoc.Jxl]
   "ImageOptionFlags"=dword:00000001

   [HKEY_CLASSES_ROOT\PhotoViewer.FileAssoc.Jxl\DefaultIcon]
   @="%ProgramFiles%\\Windows Photo Viewer\\PhotoViewer.dll,1"

   [HKEY_CLASSES_ROOT\PhotoViewer.FileAssoc.Jxl\shell\open]
   "MuiVerb"="@%ProgramFiles%\\Windows Photo Viewer\\photoviewer.dll,-3043"

   [HKEY_CLASSES_ROOT\PhotoViewer.FileAssoc.Jxl\shell\open\command]
   @="\"%SystemRoot%\\System32\\rundll32.exe\" \"%ProgramFiles%\\Windows Photo Viewer\\PhotoViewer.dll\", ImageView_Fullscreen %1"

   [HKEY_CLASSES_ROOT\PhotoViewer.FileAssoc.Jxl\shell\open\DropTarget]
   "Clsid"="{FFE2A43C-56B9-4BF5-9A79-CC6D4285608A}"
   ```
2. Expose the capability to the Windows Default Apps control panel:
   ```ini
   [HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows Photo Viewer\Capabilities\FileAssociations]
   ".jxl"="PhotoViewer.FileAssoc.Jxl"
   ```
3. Expose the "Open with..." entry point:
   ```ini
   [HKEY_CLASSES_ROOT\.jxl\OpenWithProgids]
   "PhotoViewer.FileAssoc.Jxl"=""
   ```

---

## 6. Rust COM Implementation Blueprint (`windows` crate)

Per ADR-0001, `jxlrsWIC` is a 100% pure-Rust `cdylib` in-process COM server using the official Microsoft `windows` crate.

### 6.1 Required DLL COM Exports

Every in-process COM server must export the following standard C-ABI functions:

1. **`DllGetClassObject`**: Instantiates and returns an `IClassFactory` for a requested CLSID.
2. **`DllCanUnloadNow`**: Informs the host process whether any active COM references or server locks remain.
3. **`DllRegisterServer`**: Registers the COM server, WIC decoder, sniffing patterns, and shell associations.
4. **`DllUnregisterServer`**: Purges all created keys upon removal.

### 6.2 Complete Pure-Rust Implementation

```rust
//! lib.rs - COM In-Process Server Entry Points for jxlrsWIC

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::{implement, w, AsImpl, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOINTERFACE, E_POINTER,
    S_FALSE, S_OK,
};
use windows::Win32::Graphics::Imaging::CATID_WICBitmapDecoders;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT,
    KEY_WRITE, REG_BINARY, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

// GUID Constants
pub const CLSID_JXLRS_DECODER: GUID = GUID::from_u128(0x466c040f_e9a9_45f4_be8a_bf0386b15e28);
pub const GUID_CONTAINER_FORMAT_JXL: GUID = GUID::from_u128(0xfec14e3f_427a_4736_aae6_27ed84f69322);
pub const GUID_VENDOR_YRSB: GUID = GUID::from_u128(0xae1acfc8_af90_46da_a9e3_fc759ea12407);

// Global Module Reference Count
static MODULE_REF_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn inc_module_ref() {
    MODULE_REF_COUNT.fetch_add(1, Ordering::SeqCst);
}

pub fn dec_module_ref() {
    MODULE_REF_COUNT.fetch_sub(1, Ordering::SeqCst);
}

// -----------------------------------------------------------------------------
// IClassFactory Implementation
// -----------------------------------------------------------------------------

#[implement(IClassFactory)]
pub struct JxlClassFactory;

impl IClassFactory_Impl for JxlClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Option<&windows::core::IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        if punkouter.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppvobject = std::ptr::null_mut(); }

        // Here we instantiate our IWICBitmapDecoder implementation:
        // let decoder: windows::Win32::Graphics::Imaging::IWICBitmapDecoder = JxlBitmapDecoder::new()?.into();
        // decoder.query(riid, ppvobject)
        todo!("Instantiate JxlBitmapDecoder and query interface");
    }

    fn LockServer(&self, flock: windows::Win32::Foundation::BOOL) -> Result<()> {
        if flock.as_bool() {
            inc_module_ref();
        } else {
            dec_module_ref();
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// COM DLL Exports
// -----------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();

    if *rclsid != CLSID_JXLRS_DECODER {
        return CLASS_E_CLASSNOTAVAILABLE;
    }

    let factory: IClassFactory = JxlClassFactory.into();
    factory.query(riid, ppv)
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if MODULE_REF_COUNT.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

/// Dynamic path helper to get this DLL's file path on disk
fn get_current_module_path() -> Result<Vec<u16>> {
    let mut h_module = Default::default();
    unsafe {
        let addr = get_current_module_path as *const c_void;
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(addr as *const u16),
            &mut h_module,
        )?;

        let mut buf = vec![0u16; 512];
        let len = GetModuleFileNameW(Some(h_module.into()), &mut buf);
        if len == 0 {
            return Err(E_FAIL.into());
        }
        buf.truncate((len + 1) as usize); // null-terminated
        Ok(buf)
    }
}

#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    unsafe {
        if register_all_keys().is_err() {
            return E_FAIL;
        }
        // Invalidate Windows Shell caches
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
        S_OK
    }
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    unsafe {
        if unregister_all_keys().is_err() {
            return E_FAIL;
        }
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
        S_OK
    }
}

// -----------------------------------------------------------------------------
// Registry Helpers
// -----------------------------------------------------------------------------

unsafe fn set_reg_sz(hkey: HKEY, value_name: Option<PCWSTR>, value: &str) -> Result<()> {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
    RegSetValueExW(hkey, value_name.unwrap_or(PCWSTR::null()), 0, REG_SZ, Some(bytes)).ok()
}

unsafe fn set_reg_dword(hkey: HKEY, value_name: PCWSTR, val: u32) -> Result<()> {
    let bytes = val.to_ne_bytes();
    RegSetValueExW(hkey, value_name, 0, REG_DWORD, Some(&bytes)).ok()
}

unsafe fn set_reg_binary(hkey: HKEY, value_name: PCWSTR, val: &[u8]) -> Result<()> {
    RegSetValueExW(hkey, value_name, 0, REG_BINARY, Some(val)).ok()
}

unsafe fn create_key(root: HKEY, subkey: &str) -> Result<HKEY> {
    let wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey = HKEY::default();
    RegCreateKeyExW(
        root,
        PCWSTR(wide.as_ptr()),
        0,
        None,
        REG_OPTION_NON_VOLATILE,
        KEY_WRITE,
        None,
        &mut hkey,
        None,
    ).ok()?;
    Ok(hkey)
}

unsafe fn register_all_keys() -> Result<()> {
    let clsid_str = "{466C040F-E9A9-45F4-BE8A-BF0386B15E28}";
    let container_str = "{FEC14E3F-427A-4736-AAE6-27ED84F69322}";
    let vendor_str = "{AE1ACFC8-AF90-46DA-A9E3-FC759EA12407}";

    // 1. HKCR\CLSID\{CLSID}
    let k_clsid = create_key(HKEY_CLASSES_ROOT, &format!("CLSID\\{}", clsid_str))?;
    set_reg_sz(k_clsid, None, "jxlrsWIC JPEG XL Decoder")?;
    set_reg_dword(k_clsid, w!("ArbitrationPriority"), 0)?;
    set_reg_sz(k_clsid, Some(w!("Author")), "YRSB")?;
    set_reg_sz(k_clsid, Some(w!("ColorManagementVersion")), "1.0.0.0")?;
    set_reg_sz(k_clsid, Some(w!("ContainerFormat")), container_str)?;
    set_reg_sz(k_clsid, Some(w!("Description")), "jxlrsWIC JPEG XL WIC Bitmap Decoder")?;
    set_reg_sz(k_clsid, Some(w!("FileExtensions")), ".jxl")?;
    set_reg_sz(k_clsid, Some(w!("FriendlyName")), "jxlrsWIC JPEG XL Decoder")?;
    set_reg_sz(k_clsid, Some(w!("MimeTypes")), "image/jxl")?;
    set_reg_sz(k_clsid, Some(w!("SpecVersion")), "1.0.0.0")?;
    set_reg_dword(k_clsid, w!("SupportAnimation"), 1)?;
    set_reg_dword(k_clsid, w!("SupportLossless"), 1)?;
    set_reg_dword(k_clsid, w!("SupportMultiframe"), 1)?;
    set_reg_sz(k_clsid, Some(w!("Vendor")), vendor_str)?;
    set_reg_sz(k_clsid, Some(w!("Version")), "0.1.0.0")?;
    let _ = RegCloseKey(k_clsid);

    // 2. InprocServer32
    let dll_path = get_current_module_path()?;
    let k_inproc = create_key(HKEY_CLASSES_ROOT, &format!("CLSID\\{}\\InprocServer32", clsid_str))?;
    let path_bytes = std::slice::from_raw_parts(dll_path.as_ptr() as *const u8, dll_path.len() * 2);
    RegSetValueExW(k_inproc, PCWSTR::null(), 0, REG_SZ, Some(path_bytes)).ok()?;
    set_reg_sz(k_inproc, Some(w!("ThreadingModel")), "Both")?;
    let _ = RegCloseKey(k_inproc);

    // 3. Formats
    let formats = [
        "{6FDDC324-4E03-4BFE-B185-3D77768DC90D}", // 24bppRGB
        "{6FDDC324-4E03-4BFE-B185-3D77768DC90C}", // 24bppBGR
        "{6FDDC324-4E03-4BFE-B185-3D77768DC916}", // 32bppRGBA
        "{6FDDC324-4E03-4BFE-B185-3D77768DC90F}", // 32bppBGRA
    ];
    for fmt in formats {
        let k_fmt = create_key(HKEY_CLASSES_ROOT, &format!("CLSID\\{}\\Formats\\{}", clsid_str, fmt))?;
        let _ = RegCloseKey(k_fmt);
    }

    // 4. Pattern 0 (ISO BMFF)
    let k_p0 = create_key(HKEY_CLASSES_ROOT, &format!("CLSID\\{}\\Patterns\\0", clsid_str))?;
    set_reg_dword(k_p0, w!("Position"), 0)?;
    set_reg_dword(k_p0, w!("Length"), 24)?;
    set_reg_dword(k_p0, w!("EndOfStream"), 0)?;
    let p0_bytes: [u8; 24] = [
        0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        0x00, 0x00, 0x00, 0x00, 0x66, 0x74, 0x79, 0x70, 0x6A, 0x78, 0x6C, 0x20,
    ];
    let p0_mask: [u8; 24] = [
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ];
    set_reg_binary(k_p0, w!("Pattern"), &p0_bytes)?;
    set_reg_binary(k_p0, w!("Mask"), &p0_mask)?;
    let _ = RegCloseKey(k_p0);

    // 5. Pattern 1 (Naked Codestream)
    let k_p1 = create_key(HKEY_CLASSES_ROOT, &format!("CLSID\\{}\\Patterns\\1", clsid_str))?;
    set_reg_dword(k_p1, w!("Position"), 0)?;
    set_reg_dword(k_p1, w!("Length"), 2)?;
    set_reg_dword(k_p1, w!("EndOfStream"), 0)?;
    let p1_bytes: [u8; 2] = [0xFF, 0x0A];
    let p1_mask: [u8; 2] = [0xFF, 0xFF];
    set_reg_binary(k_p1, w!("Pattern"), &p1_bytes)?;
    set_reg_binary(k_p1, w!("Mask"), &p1_mask)?;
    let _ = RegCloseKey(k_p1);

    // 6. CATID_WICBitmapDecoders Instance Registration
    let catid_str = "{7ED96837-96F0-4812-B211-F13C24117ED3}";
    let k_cat = create_key(HKEY_CLASSES_ROOT, &format!("CLSID\\{}\\Instance\\{}", catid_str, clsid_str))?;
    set_reg_sz(k_cat, Some(w!("CLSID")), clsid_str)?;
    set_reg_sz(k_cat, Some(w!("FriendlyName")), "jxlrsWIC JPEG XL Decoder")?;
    let _ = RegCloseKey(k_cat);

    // 7. Shell Thumbnail Associations (.jxl)
    let k_ext = create_key(HKEY_CLASSES_ROOT, ".jxl")?;
    set_reg_sz(k_ext, None, "jxlfile")?;
    set_reg_sz(k_ext, Some(w!("Content Type")), "image/jxl")?;
    set_reg_sz(k_ext, Some(w!("PerceivedType")), "image")?;
    let _ = RegCloseKey(k_ext);

    let k_thumb = create_key(HKEY_CLASSES_ROOT, ".jxl\\ShellEx\\{e357fccd-a995-4576-b01f-234630154e96}")?;
    set_reg_sz(k_thumb, None, "{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}")?;
    let _ = RegCloseKey(k_thumb);

    let k_sys = create_key(HKEY_CLASSES_ROOT, "SystemFileAssociations\\.jxl")?;
    set_reg_sz(k_sys, Some(w!("PerceivedType")), "image")?;
    set_reg_dword(k_sys, w!("ImageOptionFlags"), 1)?;
    let _ = RegCloseKey(k_sys);

    let k_sys_thumb = create_key(HKEY_CLASSES_ROOT, "SystemFileAssociations\\.jxl\\ShellEx\\{e357fccd-a995-4576-b01f-234630154e96}")?;
    set_reg_sz(k_sys_thumb, None, "{C7657C4A-9F68-40FA-A4DF-96BC08EB3551}")?;
    let _ = RegCloseKey(k_sys_thumb);

    Ok(())
}

unsafe fn unregister_all_keys() -> Result<()> {
    let clsid_str = "{466C040F-E9A9-45F4-BE8A-BF0386B15E28}";
    let catid_str = "{7ED96837-96F0-4812-B211-F13C24117ED3}";

    // Delete in leaf-to-root order
    let keys_to_delete = [
        format!("CLSID\\{}\\Instance\\{}", catid_str, clsid_str),
        format!("CLSID\\{}\\InprocServer32", clsid_str),
        format!("CLSID\\{}\\Patterns\\0", clsid_str),
        format!("CLSID\\{}\\Patterns\\1", clsid_str),
        format!("CLSID\\{}\\Patterns", clsid_str),
        format!("CLSID\\{}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC90D}}", clsid_str),
        format!("CLSID\\{}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC90C}}", clsid_str),
        format!("CLSID\\{}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC916}}", clsid_str),
        format!("CLSID\\{}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC90F}}", clsid_str),
        format!("CLSID\\{}\\Formats", clsid_str),
        format!("CLSID\\{}", clsid_str),
        ".jxl\\ShellEx\\{e357fccd-a995-4576-b01f-234630154e96}".to_string(),
        "SystemFileAssociations\\.jxl\\ShellEx\\{e357fccd-a995-4576-b01f-234630154e96}".to_string(),
    ];

    for key in keys_to_delete {
        let wide: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = RegDeleteKeyW(HKEY_CLASSES_ROOT, PCWSTR(wide.as_ptr()));
    }

    Ok(())
}
```

---

## 7. Primary Source Citations & References

1. **Microsoft Learn: Windows Imaging Component Overview**
   - Win32 WIC architecture and extensibility model.
   - URL: `https://learn.microsoft.com/en-us/windows/win32/wic/-wic-about-windows-imaging-codec`
2. **Microsoft Learn: Codec Installation and Registration**
   - Detailed specification of required keys, category identifiers, and apartment threading rules.
   - URL: `https://learn.microsoft.com/en-us/windows/win32/wic/-wic-codecinstallandreg`
3. **Microsoft Learn: Decoder-Specific Registry Entries**
   - Registry definition of `CATID_WICBitmapDecoders` (`{7ED96837-96F0-4812-B211-F13C24117ED3}`) and `Patterns`.
   - URL: `https://learn.microsoft.com/en-us/windows/win32/wic/-wic-decoderspecificregistry`
4. **Microsoft Learn: General CODEC Registry Entries**
   - Specification for `ArbitrationPriority`, `ContainerFormat`, `Vendor`, and `Formats`.
   - URL: `https://learn.microsoft.com/en-us/windows/win32/wic/-wic-generalregistry`
5. **Windows 11 (Build 10.0.26100.1 / 24H2) Native Image Codecs**
   - Empirical registry dump of Microsoft's built-in JPEG XL registration under `windowscodecs.dll`:
     - Container Format: `{fec14e3f-427a-4736-aae6-27ed84f69322}`
     - Pattern 0 (ISO BMFF Box): 24 bytes `00 00 00 0C 4A 58 4C 20 0D 0A 87 0A 00 00 00 00 66 74 79 70 6A 78 6C 20`
     - Pattern 1 (Bare Codestream): 2 bytes `FF 0A`
6. **ISO/IEC 18181-1 / 18181-2 (JPEG XL Image Coding System)**
   - Codestream signatures, box headers, and MIME type registrations (`image/jxl`).
7. **Microsoft `windows-rs` Documentation (crates.io / docs.rs)**
   - `windows::core::implement`, `IClassFactory`, `IWICBitmapDecoder`, and Win32 registry bindings.
   - URL: `https://docs.rs/windows/latest/windows/`
