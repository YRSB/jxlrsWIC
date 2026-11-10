//! Windows Registry COM server and WIC decoder registration logic.

use std::slice;
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HMODULE};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT,
    KEY_WRITE, REG_BINARY, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

const CLSID_DECODER_STR: &str = "{466C040F-E9A9-45F4-BE8A-BF0386B15E28}";
const CATID_DECODERS_STR: &str = "{7ED96837-96F0-4812-B211-F13C24117ED3}";
const CONTAINER_FORMAT_JXL_STR: &str = "{FEC14E3F-427A-4736-AAE6-27ED84F69322}";
const VENDOR_YRSB_STR: &str = "{AE1ACFC8-AF90-46DA-A9E3-FC759EA12407}";

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct RegKey(HKEY);

impl RegKey {
    fn create(root: HKEY, subkey: &str) -> Result<Self> {
        let wide = to_wide(subkey);
        let mut hkey = HKEY::default();
        let status = unsafe {
            RegCreateKeyExW(
                root,
                PCWSTR(wide.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hkey,
                None,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(RegKey(hkey))
        } else {
            Err(status.to_hresult().into())
        }
    }

    fn set_string(&self, name: Option<&str>, value: &str) -> Result<()> {
        let wide_val = to_wide(value);
        let status = unsafe {
            match name {
                Some(n) => {
                    let wide_name = to_wide(n);
                    RegSetValueExW(
                        self.0,
                        PCWSTR(wide_name.as_ptr()),
                        0,
                        REG_SZ,
                        Some(slice::from_raw_parts(
                            wide_val.as_ptr() as *const u8,
                            wide_val.len() * 2,
                        )),
                    )
                }
                None => RegSetValueExW(
                    self.0,
                    PCWSTR::null(),
                    0,
                    REG_SZ,
                    Some(slice::from_raw_parts(
                        wide_val.as_ptr() as *const u8,
                        wide_val.len() * 2,
                    )),
                ),
            }
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(status.to_hresult().into())
        }
    }

    fn set_dword(&self, name: &str, value: u32) -> Result<()> {
        let wide_name = to_wide(name);
        let status = unsafe {
            RegSetValueExW(
                self.0,
                PCWSTR(wide_name.as_ptr()),
                0,
                REG_DWORD,
                Some(slice::from_raw_parts(&value as *const u32 as *const u8, 4)),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(status.to_hresult().into())
        }
    }

    fn set_binary(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let wide_name = to_wide(name);
        let status = unsafe {
            RegSetValueExW(
                self.0,
                PCWSTR(wide_name.as_ptr()),
                0,
                REG_BINARY,
                Some(bytes),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(status.to_hresult().into())
        }
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}

/// Retrieves the filesystem path of the module identified by `hmodule`.
pub fn get_module_path(hmodule: HMODULE) -> Result<String> {
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(hmodule, &mut buf) };
    if len == 0 {
        return Err(windows::core::Error::from_win32());
    }
    buf.truncate(len as usize);
    String::from_utf16(&buf).map_err(|_| windows::core::Error::from_win32())
}

/// Tells the shell that file associations changed, so cached thumbnails and icons for a
/// newly supported extension are discarded and re-queried against the decoders WIC now sees.
///
/// Without this, an Explorer window that already rendered `.jxl` files as "unknown type"
/// keeps showing those cached placeholders after the decoder is registered. The call has no
/// failure mode worth reporting: the registration itself already succeeded, and a stale
/// icon cache is cosmetic.
fn notify_shell_of_association_change() {
    // SAFETY: `SHCNF_IDLIST` with null item pointers is the documented "refresh everything"
    // form of this call; it dereferences neither argument.
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
}

/// Registers the WIC decoder in the Windows Registry (`HKEY_CLASSES_ROOT`).
pub fn register_server(hmodule: HMODULE) -> Result<()> {
    let dll_path = get_module_path(hmodule)?;

    // 1. CLSID\{CLSID_JxlrsWICDecoder}
    let k_clsid = RegKey::create(HKEY_CLASSES_ROOT, &format!("CLSID\\{CLSID_DECODER_STR}"))?;
    k_clsid.set_string(None, "jxlrsWIC JPEG XL Decoder")?;
    k_clsid.set_dword("ArbitrationPriority", 0)?;
    k_clsid.set_string(Some("Author"), "YRSB")?;
    k_clsid.set_string(Some("ColorManagementVersion"), "1.0.0.0")?;
    k_clsid.set_string(Some("ContainerFormat"), CONTAINER_FORMAT_JXL_STR)?;
    k_clsid.set_string(Some("Description"), "Pure Rust JPEG XL WIC Decoder")?;
    k_clsid.set_string(Some("FileExtensions"), ".jxl")?;
    k_clsid.set_string(Some("FriendlyName"), "jxlrsWIC JPEG XL Decoder")?;
    k_clsid.set_string(Some("MimeTypes"), "image/jxl")?;
    k_clsid.set_string(Some("SpecVersion"), "1.0.0.0")?;
    k_clsid.set_dword("SupportAnimation", 0)?;
    k_clsid.set_dword("SupportChromakey", 0)?;
    k_clsid.set_dword("SupportLossless", 1)?;
    k_clsid.set_dword("SupportMultiframe", 0)?;
    k_clsid.set_string(Some("Vendor"), VENDOR_YRSB_STR)?;
    k_clsid.set_string(Some("Version"), "0.1.0.0")?;

    // 2. InprocServer32
    let k_inproc = RegKey::create(
        HKEY_CLASSES_ROOT,
        &format!("CLSID\\{CLSID_DECODER_STR}\\InprocServer32"),
    )?;
    k_inproc.set_string(None, &dll_path)?;
    k_inproc.set_string(Some("ThreadingModel"), "Both")?;

    // 3. Formats
    let formats = [
        "{6FDDC324-4E03-4BFE-B185-3D77768DC90D}", // 24bpp RGB
        "{6FDDC324-4E03-4BFE-B185-3D77768DC90C}", // 24bpp BGR
        "{6FDDC324-4E03-4BFE-B185-3D77768DC916}", // 32bpp RGBA
        "{6FDDC324-4E03-4BFE-B185-3D77768DC90F}", // 32bpp BGRA
    ];
    for fmt in formats {
        let _ = RegKey::create(
            HKEY_CLASSES_ROOT,
            &format!("CLSID\\{CLSID_DECODER_STR}\\Formats\\{fmt}"),
        )?;
    }

    // 4. Pattern 0 (ISO BMFF Box Container)
    let k_p0 = RegKey::create(
        HKEY_CLASSES_ROOT,
        &format!("CLSID\\{CLSID_DECODER_STR}\\Patterns\\0"),
    )?;
    k_p0.set_dword("Position", 0)?;
    k_p0.set_dword("Length", 24)?;
    k_p0.set_dword("EndOfStream", 0)?;
    let p0_bytes: [u8; 24] = [
        0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A, 0x00, 0x00, 0x00,
        0x00, 0x66, 0x74, 0x79, 0x70, 0x6A, 0x78, 0x6C, 0x20,
    ];
    let p0_mask: [u8; 24] = [
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00,
        0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ];
    k_p0.set_binary("Pattern", &p0_bytes)?;
    k_p0.set_binary("Mask", &p0_mask)?;

    // 5. Pattern 1 (Bare Codestream)
    let k_p1 = RegKey::create(
        HKEY_CLASSES_ROOT,
        &format!("CLSID\\{CLSID_DECODER_STR}\\Patterns\\1"),
    )?;
    k_p1.set_dword("Position", 0)?;
    k_p1.set_dword("Length", 2)?;
    k_p1.set_dword("EndOfStream", 0)?;
    let p1_bytes: [u8; 2] = [0xFF, 0x0A];
    let p1_mask: [u8; 2] = [0xFF, 0xFF];
    k_p1.set_binary("Pattern", &p1_bytes)?;
    k_p1.set_binary("Mask", &p1_mask)?;

    // 6. CATID_WICBitmapDecoders Instance Registration
    let k_cat = RegKey::create(
        HKEY_CLASSES_ROOT,
        &format!("CLSID\\{CATID_DECODERS_STR}\\Instance\\{CLSID_DECODER_STR}"),
    )?;
    k_cat.set_string(Some("CLSID"), CLSID_DECODER_STR)?;
    k_cat.set_string(Some("FriendlyName"), "jxlrsWIC JPEG XL Decoder")?;

    notify_shell_of_association_change();
    Ok(())
}

/// Unregisters the WIC decoder, purging all created registry keys.
pub fn unregister_server() -> Result<()> {
    let keys = [
        format!("CLSID\\{CATID_DECODERS_STR}\\Instance\\{CLSID_DECODER_STR}"),
        format!("CLSID\\{CLSID_DECODER_STR}\\InprocServer32"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Patterns\\0"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Patterns\\1"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Patterns"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC90D}}"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC90C}}"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC916}}"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Formats\\{{6FDDC324-4E03-4BFE-B185-3D77768DC90F}}"),
        format!("CLSID\\{CLSID_DECODER_STR}\\Formats"),
        format!("CLSID\\{CLSID_DECODER_STR}"),
    ];

    for key in keys {
        let wide = to_wide(&key);
        let _ = unsafe { RegDeleteKeyW(HKEY_CLASSES_ROOT, PCWSTR(wide.as_ptr())) };
    }

    // Same reasoning as registration: without this, thumbnails rendered by the decoder linger
    // in the shell cache after the decoder is gone.
    notify_shell_of_association_change();
    Ok(())
}
