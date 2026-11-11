# jxlrsWIC

A pure-Rust Windows Imaging Component (WIC) decoder for JPEG XL images based on `jxl-rs`.

## Language

**WIC Decoder**:
A Windows Imaging Component in-process COM server implementing `IWICBitmapDecoder` that enables Windows applications to discover, inspect, and decode image streams.
_Avoid_: Codec, image plugin, filter

**Decoder Seam**:
The internal abstraction boundary separating COM interface state machines from the underlying `jxl-rs` decoding engine.
_Avoid_: Wrapper, bridge, driver

**Frame Decode**:
The WIC representation (`IWICBitmapFrameDecode`) of an individual image frame providing dimensions, color contexts, and raster pixel data.
_Avoid_: Sub-image, page, layer

**Cached Raster**:
The decoded in-memory pixel buffer managed behind the Decoder Seam, used to serve subsequent `CopyPixels` requests without re-decoding the underlying bitstream.
_Avoid_: Frame buffer, pixel cache, decoded cache

**MSI Package**:
The declarative Windows Installer package (`.msi`) authored via the WiX Toolset that installs `jxlrs_wic.dll` to `Program Files` and manages COM/WIC registry entries and Explorer shell notifications without self-registration.
_Avoid_: Setup exe, MSIX bundle, installer script
