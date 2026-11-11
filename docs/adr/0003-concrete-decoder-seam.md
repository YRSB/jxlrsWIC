# Concrete Decoder Seam and Unified In-Process Engine

## Context and Decision

In ADR-0002, we established a pure-Rust `DecoderSeam` trait to isolate decoding logic from COM interface state machines. While this boundary successfully isolated COM and Win32 dependencies, implementing the seam as two separate polymorphic traits (`DecoderSeam` and `FrameSeam`) across multiple files created artificial complexity:

1. **Hypothetical Seam**: The codebase contains exactly one adapter (`jxl-rs`). Dynamic dispatch (`Arc<dyn DecoderSeam>`, `Arc<dyn FrameSeam>`) and repeated heap allocations on `GetFrame(0)` added ceremony without polymorphism leverage.
2. **Scattered Implementation**: The `CachedRaster` engine was placed in `decoder_seam.rs` while the decoding pipeline lived in `jxl_seam.rs`, fragmenting the single logical concept of the decoding engine.
3. **Leaking Panic Risk**: Parser panics in `jxl-rs` were contained via shallow helper closures scattered across COM entrypoints in `decoder.rs`, rather than being absorbed at the seam.

We decided to:
1. Collapse `DecoderSeam`, `FrameSeam`, and `CachedRaster` into a single, concrete, pure-Rust deep module (`JxlContainer` / `JxlImage` and zero-allocation borrowed `JxlFrame<'a>`).
2. Maintain strict decoupling from COM: `JxlContainer` accepts in-memory byte slices (`&[u8]`) and returns pure Rust types (`FrameInfo`, `Rect`, `WicPixelFormat`, `DecodeError`), free of COM or Win32 headers.
3. Have `JxlContainer` directly own the single frame metadata and an internal `OnceLock<CachedRaster>`, eliminating per-call frame heap allocations and redundant `Arc` indirection.
4. Absorb parser panic containment directly inside `JxlContainer`'s public interface, ensuring the module never unwinds into callers.

## Rationale and Trade-offs

- **Locality and Depth**: All bitstream parsing, multi-threaded raster decoding, ICC color profile extraction, and stride-aware memory blits reside in one high-leverage module with a minimal public interface.
- **Zero Dynamic Dispatch Overhead**: Calling `open` returns a concrete `JxlContainer`. Frame lookups borrow directly from the container via zero-allocation `JxlFrame<'a>` without virtual dispatch or heap allocations.
- **Preserved Isolation**: COM and Win32 remain 100% separate from decoding logic. Unit tests continue to exercise the entire decoding pipeline using pure in-memory buffers without COM runtimes or elevation.
