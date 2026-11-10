# Decoder Seam and Raster Caching Architecture

## Context and Decision

Windows Imaging Component (WIC) decoders interact with COM through `IStream` and `IWICBitmapSource::CopyPixels`. To maintain high performance, apartment thread-safety, and testability, we decided to:
1. Isolate all decoding behind a pure-Rust `DecoderSeam` trait completely free of COM and Win32 dependencies.
2. Buffer incoming `IStream` data into a contiguous in-memory byte buffer on `IWICBitmapDecoder::Initialize`, eliminating multi-threading `IStream` contention and seek overhead.
3. Cache the full uncompressed frame raster in a `OnceLock` inside `FrameSeam` upon first access, fulfilling all subsequent `CopyPixels` requests (including sub-rectangles and arbitrary strides) via stride-aware memory blits.
4. Normalize output formats to canonical WIC formats (`32bppBGRA` straight alpha, `64bppRGBA`, `64bppRGBAHalf`) and synthesize compliant ICC color profile bytes on demand from embedded or CICP metadata.

## Rationale and Trade-offs

- **Thread-safety and Locality**: Reading `IStream` into memory upfront makes the entire decoding pipeline pure in-memory, allowing lock-free concurrent reads across apartment threads without COM `IStream` marshaling hurdles.
- **WIC Host Behavior**: Callers like Windows Explorer, Photo Viewer, and Direct2D often request metadata and pixels across multiple small rects or tiled `CopyPixels` calls. Caching the decoded raster avoids repeated `jxl-rs` decoding passes.
- **DoS Protection**: A hard dimension limit (max 16,384 x 16,384) guards host processes against memory exhaustion from decompression bombs.
