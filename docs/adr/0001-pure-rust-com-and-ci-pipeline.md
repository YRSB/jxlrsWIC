# Pure Rust COM In-Process Server and CI-First Pipeline

## Context and Decision

Windows Imaging Component (WIC) decoders are traditionally implemented in C++ using MSVC COM tooling. We decided to implement `jxlrsWIC` as a 100% pure Rust in-process COM server (`cdylib`) using the official `windows` crate, backed by `jxl-rs` (`libjxl/jxl-rs`), with all building, testing, and registration verification running through GitHub Actions CI.

## Rationale and Trade-offs

- **Memory safety**: WIC decoders run inside host processes (such as Windows Explorer and Windows Photo Viewer) and process untrusted image streams from the web. Pure Rust decoder + COM bindings eliminate memory corruption vulnerabilities common in image codecs.
- **Single toolchain**: Avoids mixed C++/Rust build dependencies or MSVC C++ runtime redistribution issues.
- **CI-first verification**: Since the local development environment may lack specific toolchains, all compilation, unit testing, and end-to-end WIC registration (`regsvr32` + `CoCreateInstance` decoding test) are automated in GitHub Actions on `windows-latest`.
