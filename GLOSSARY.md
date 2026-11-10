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
