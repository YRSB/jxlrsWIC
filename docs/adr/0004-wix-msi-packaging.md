# WiX Toolset MSI Installer Packaging

## Context and Decision

Prior to this decision, `jxlrsWIC` was distributed exclusively as `.zip` archives containing `jxlrs_wic.dll`. Installation required end-users or administrators to manually open an elevated terminal and invoke `regsvr32.exe jxlrs_wic.dll`. Uninstallation similarly required manual `regsvr32.exe /u` invocations.

While suitable for developers, manual registration has several drawbacks:
1. **Lack of Automated Lifecycle Management**: Windows does not track files installed or registered via `regsvr32`. There is no entries in "Installed Apps" / "Programs and Features" to modify, repair, or uninstall the decoder.
2. **Dangling Registry Hazards**: If a user deletes the directory containing `jxlrs_wic.dll` without first unregistering, Explorer and WIC host applications encounter broken in-process server entries.
3. **MSIX Limitations**: MSIX packages virtualize the Windows Registry into private hives. Because WIC is an in-process COM extension loaded dynamically by system processes (e.g. `explorer.exe`, `DllHost.exe`, and third-party imaging tools), system-wide decoder registration requires native `HKCR` visibility across the entire OS, making MSIX ill-suited for arbitrary host discovery.

We decided to:
1. Author a declarative WiX Toolset (v4+) MSI package in `installer/jxlrs_wic.wxs`.
2. Install `jxlrs_wic.dll`, `LICENSE`, and `README.md` to `ProgramFiles6432Folder\jxlrsWIC` (`C:\Program Files\jxlrsWIC` for 64-bit packages, `C:\Program Files (x86)\jxlrsWIC` for 32-bit packages).
3. Author all COM server and WIC decoder registrations declaratively in `HKCR` (`CLSID\{466C040F-E9A9-45F4-BE8A-BF0386B15E28}`, `InprocServer32`, `Formats`, `Patterns/0`, `Patterns/1`, and `CATID_WICBitmapDecoders\Instance`), strictly avoiding `SelfReg` (`DllRegisterServer`).
4. Broadcast `SHCNE_ASSOCCHANGED` via an immediate custom action scheduled after `InstallFinalize` upon both install and uninstall to force Windows Explorer to flush cached thumbnails and association icons without requiring a shell restart or reboot.
5. Provide a standalone PowerShell build helper `installer/build_msi.ps1` that packages `x86_64`, `aarch64`, and `i686` targets.
6. Integrate MSI packaging into GitHub Actions CI (`ci.yml`), including end-to-end headless testing of silent installation (`msiexec /i ... /qn`), registry verification, WIC decoding test execution against the installed decoder, and clean silent uninstallation (`msiexec /x ... /qn`).
7. Update the release workflow (`release.yml`) to publish `.msi` packages and `.msi.sha256` checksums alongside the distribution zip files.

## Rationale and Trade-offs

- **Declarative MSI vs SelfReg**: Windows Installer best practices strongly discourage `SelfReg` (flagged by ICE33). Declarative authoring enables MSI transaction rollback if installation fails midway, guarantees exact reference counting, and ensures 100% symmetric uninstallation with zero leftover registry keys or orphaned files.
- **Cross-Architecture Support**: Using WiX v4 allows building native `x64`, `arm64`, and `x86` MSI packages from a single unified `.wxs` source via the `-arch` switch without requiring separate Visual Studio `.wixproj` solutions.
- **Shell Notification Ergonomics**: Triggering `SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, 0, 0)` immediately updates Explorer file icons, matching the behavior of the internal `register_server()` function.
