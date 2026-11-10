# jxlrsWIC

A pure-Rust Windows Imaging Component (WIC) decoder for JPEG XL images, backed by
[`jxl-rs`](https://github.com/libjxl/jxl-rs).

Once installed, Windows File Explorer, Windows Photo Viewer, Microsoft Photos, and any
WIC-capable application can open, preview, and thumbnail `.jxl` files directly. There is
no C or C++ runtime dependency: the decoder is a single `jxlrs_wic.dll` built from the
Rust standard library and the Windows API bindings.

## Install

1. Download the zip for your architecture from the
   [releases page](https://github.com/YRSB/jxlrsWIC/releases):

   | Windows                         | Download                               |
   | ------------------------------- | -------------------------------------- |
   | Intel / AMD 64-bit              | `jxlrs_wic-x86_64-pc-windows-msvc.zip` |
   | ARM64 (Surface Pro, Snapdragon) | `jxlrs_wic-aarch64-pc-windows-msvc.zip` |

   The bitness must match the process that decodes the image. 64-bit Explorer cannot load
   a 32-bit DLL, and vice versa.

2. Verify the download against the published SHA-256:

   ```pwsh
   Get-FileHash .\jxlrs_wic-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\jxlrs_wic-x86_64-pc-windows-msvc.zip.sha256
   ```

3. Extract the zip, then open a terminal **as administrator** and register the decoder:

   ```pwsh
   regsvr32 jxlrs_wic.dll
   ```

   Registration without elevation fails with `0x8002801C`, because the decoder writes to
   `HKEY_CLASSES_ROOT`.

4. Registration broadcasts `SHChangeNotify(SHCNE_ASSOCCHANGED)`, so the shell drops cached
   "unknown type" icons for `.jxl`. If an already-open File Explorer window still shows them,
   restart it:

   ```pwsh
   taskkill /f /im explorer.exe; start explorer.exe
   ```

## Uninstall

In an administrator terminal, from the directory containing the DLL:

```pwsh
regsvr32 /u jxlrs_wic.dll
```

## Upgrading

The DLL cannot be replaced while it is loaded. Find the processes holding it and close
them before installing a new version:

```pwsh
tasklist /m jxlrs_wic.dll
```

## What is supported

| Feature                | Status                                                   |
| ---------------------- | -------------------------------------------------------- |
| Still images           | Yes                                                      |
| Alpha channel          | Yes, straight alpha                                      |
| Output formats         | `32bppBGRA`, `64bppRGBA`, `64bppRGBAHalf`                 |
| Embedded ICC profile   | Returned verbatim                                        |
| CICP / parametric gamut | Synthesised into a WIC colour context                    |
| Containers             | ISO BMFF (`0000000cJXL `) and bare codestream (`ff0a`)   |
| Animated JPEG XL       | Not yet; frame 0 of an animation decodes                 |
| Encoding               | Out of scope                                             |

Decoding is bounded to protect the host process from malformed input: streams larger than
256 MiB and images larger than 16,384 × 16,384 pixels are rejected during
`IWICBitmapDecoder::Initialize`. Panics raised inside the bitstream parser are caught at the
COM boundary and reported as `WINCODEC_ERR_BADIMAGE`, so a corrupt file cannot take down the
application that opened it.

## Threading

Frames decode across all available cores. The worker count defaults to the machine's logical
processor count and can be overridden per process with the `JXLRS_WIC_THREADS` environment
variable, which is useful when the host application already parallelises over images and you
would rather not have the decoder compete for cores:

```pwsh
$env:JXLRS_WIC_THREADS = "4"
```

The value is clamped to `1..=64`; anything that is not a positive integer falls back to the
default. Setting `1` makes decoding single-threaded.

Only the first `CopyPixels` for a frame performs the decode; every later call on that Frame
Decode reads the Cached Raster, so the threads are not touched again.

## Building from source

```pwsh
cargo build --release
```

The cdylib lands in `target/release/jxlrs_wic.dll`. Registration can be verified without a
graphical shell:

```pwsh
regsvr32 /s target\release\jxlrs_wic.dll
pwsh -File .\tests\verify_wic_registration.ps1 -DllPath (Resolve-Path target\release\jxlrs_wic.dll)
```

All builds and tests run in CI on `windows-latest` for `x86_64-pc-windows-msvc`,
`aarch64-pc-windows-msvc`, and `i686-pc-windows-msvc`. The `x86_64` job registers the
freshly built DLL, runs the WIC COM end-to-end suite through `CoCreateInstance`, then
unregisters it. Pushing a `v*` tag builds the three architecture archives and publishes
them to the releases page with SHA-256 checksums.

## Design

The codebase is split along two documented seams, described in
[`GLOSSARY.md`](GLOSSARY.md) and the ADRs under [`docs/adr/`](docs/adr):

- **Decoder Seam** — a pure-Rust trait boundary between the COM interface state machines
  and `jxl-rs`, so bitstream decoding is testable without any COM dependency.
- **Cached Raster** — pixels are decoded lazily on the first `CopyPixels` and kept in a
  `OnceLock`, so partial and tiled reads are stride-aware blits instead of re-decodes.

## Licence

BSD-3-Clause. See [LICENSE](LICENSE).
