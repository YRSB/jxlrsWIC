# jxlrsWIC

A pure-Rust Windows Imaging Component (WIC) decoder for JPEG XL images, backed by
[`jxl-rs`](https://github.com/libjxl/jxl-rs).

Once installed, Windows File Explorer, Windows Photo Viewer, Microsoft Photos, and any
WIC-capable application can open, preview, and thumbnail `.jxl` files directly. There is
no C or C++ runtime dependency: the decoder is a single `jxlrs_wic.dll` built from the
Rust standard library and the Windows API bindings.

## Install

### Option A: MSI Installer (Recommended)

1. Download the `.msi` package for your architecture from the
   [releases page](https://github.com/YRSB/jxlrsWIC/releases):

   | Architecture                    | MSI Installer                          | Zip Archive (Manual)                   |
   | ------------------------------- | -------------------------------------- | -------------------------------------- |
   | Intel / AMD 64-bit (x64)        | `jxlrs_wic-x86_64-pc-windows-msvc.msi` | `jxlrs_wic-x86_64-pc-windows-msvc.zip` |
   | ARM64 (Surface Pro, Snapdragon) | `jxlrs_wic-aarch64-pc-windows-msvc.msi`| `jxlrs_wic-aarch64-pc-windows-msvc.zip`|
   | 32-bit (x86)                    | `jxlrs_wic-i686-pc-windows-msvc.msi`   | `jxlrs_wic-i686-pc-windows-msvc.zip`   |

2. Double-click the `.msi` installer to install, or run unattended from an elevated prompt:

   ```pwsh
   msiexec /i jxlrs_wic-x86_64-pc-windows-msvc.msi /qn
   ```

   The installer places `jxlrs_wic.dll` in `C:\Program Files\jxlrsWIC`, registers all COM/WIC
   decoder keys declaratively, and automatically notifies Windows Explorer to refresh its icon cache.

### Option B: Manual Registration (Zip)

1. Download and extract the `.zip` archive for your architecture.

2. Verify the download against the published SHA-256:

   ```pwsh
   Get-FileHash .\jxlrs_wic-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   Get-Content .\jxlrs_wic-x86_64-pc-windows-msvc.zip.sha256
   ```

3. Open a terminal **as administrator** and register the decoder:

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

If installed via MSI, uninstall via Windows **Installed Apps** / **Settings**, or run:

```pwsh
msiexec /x jxlrs_wic-x86_64-pc-windows-msvc.msi /qn
```

If registered manually via `regsvr32`, in an administrator terminal from the directory containing the DLL:

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

| Feature                 | Status                                                 |
| ----------------------- | ------------------------------------------------------ |
| Still images            | Yes                                                    |
| Alpha channel           | Yes, straight alpha                                    |
| Output formats          | `32bppBGRA`, `64bppRGBA`, `64bppRGBAHalf`               |
| Embedded ICC profile    | Returned verbatim                                      |
| CICP / parametric gamut | Synthesised into a WIC colour context                  |
| Containers              | ISO BMFF (`0000000cJXL `) and bare codestream (`ff0a`) |
| Animated JPEG XL        | Yes: `GetFrameCount` reports the visible frame count and each `GetFrame(i)` decodes its own frame; durations (`/grctlext/Delay`, centiseconds) and the loop count (`/appext`, `NETSCAPE2.0`, `0` = forever) are exposed through the metadata query readers, following the GIF precedent |
| Encoding                | Out of scope                                           |
| `GetDecoderInfo`        | Answered from the registration entries                 |

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
Decode reads the Cached Raster, so the threads are not touched again. Each animation frame
decodes this way exactly once, on first use, so hosts that only touch frame 0 pay for a
single frame; still images behave exactly as before. Frames decode across all available
cores through the same per-decode `ThreadRunner` the still path reuses.

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
