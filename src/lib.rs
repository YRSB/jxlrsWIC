//! Pure Rust Windows Imaging Component (WIC) decoder for JPEG XL.

#![deny(missing_docs)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{
    BOOL, CLASS_E_CLASSNOTAVAILABLE, E_POINTER, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::IClassFactory;

pub mod clsfactory;
pub mod decoder;
pub mod guids;
pub mod registry;

use clsfactory::JxlWicClassFactory;
use guids::CLSID_JxlrsWICDecoder;

static SERVER_LOCK_COUNT: AtomicIsize = AtomicIsize::new(0);
static MODULE_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Increments the in-process server lock count.
pub fn inc_server_lock() {
    SERVER_LOCK_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Decrements the in-process server lock count.
pub fn dec_server_lock() {
    SERVER_LOCK_COUNT.fetch_sub(1, Ordering::SeqCst);
}

/// DLL entry point.
///
/// # Safety
/// Called by the Windows operating system loader upon loading/unloading the DLL.
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    hinstance: HMODULE,
    reason: u32,
    _reserved: *mut c_void,
) -> BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE_HANDLE.store(hinstance.0, Ordering::SeqCst);
    }
    BOOL(1)
}

/// Retrieves the class factory object for the specified CLSID.
///
/// # Safety
/// Called by COM runtime / `CoGetClassObject`.
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

    if *rclsid != CLSID_JxlrsWICDecoder {
        return CLASS_E_CLASSNOTAVAILABLE;
    }

    let factory: IClassFactory = JxlWicClassFactory::new().into();
    factory.query(riid, ppv)
}

/// Determines whether the DLL can be unloaded by COM.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if SERVER_LOCK_COUNT.load(Ordering::SeqCst) <= 0 {
        S_OK
    } else {
        S_FALSE
    }
}

/// Registers COM class and WIC decoder entries in the Windows Registry.
///
/// # Safety
/// Called by `regsvr32.exe`.
#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> HRESULT {
    let mut raw_hmod = MODULE_HANDLE.load(Ordering::SeqCst);
    if raw_hmod.is_null() {
        use windows::core::PCWSTR;
        use windows::Win32::System::LibraryLoader::{
            GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };
        let mut resolved_hmod = HMODULE(std::ptr::null_mut());
        let _ = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(DllRegisterServer as *const () as *const u16),
            &mut resolved_hmod,
        );
        raw_hmod = resolved_hmod.0;
    }

    match registry::register_server(HMODULE(raw_hmod)) {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

/// Unregisters COM class and WIC decoder entries from the Windows Registry.
///
/// # Safety
/// Called by `regsvr32.exe /u`.
#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> HRESULT {
    match registry::unregister_server() {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

/// Returns the current crate package version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use guids::GUID_ContainerFormatJxl;
    use windows::Win32::Graphics::Imaging::IWICBitmapDecoder;

    #[test]
    fn test_version() {
        assert_eq!(version(), "0.1.0");
    }

    #[test]
    fn test_dll_get_class_object_and_create_instance() {
        unsafe {
            let mut factory_ptr: *mut c_void = std::ptr::null_mut();
            let hr =
                DllGetClassObject(&CLSID_JxlrsWICDecoder, &IClassFactory::IID, &mut factory_ptr);
            assert_eq!(hr, S_OK);
            assert!(!factory_ptr.is_null());

            let factory: IClassFactory = std::mem::transmute(factory_ptr);
            let mut decoder_ptr: *mut c_void = std::ptr::null_mut();
            let res = factory.CreateInstance(None, &IWICBitmapDecoder::IID, &mut decoder_ptr);
            assert!(res.is_ok());
            assert!(!decoder_ptr.is_null());

            let decoder: IWICBitmapDecoder = std::mem::transmute(decoder_ptr);
            let container_format = decoder
                .GetContainerFormat()
                .expect("GetContainerFormat failed");
            assert_eq!(container_format, GUID_ContainerFormatJxl);

            let frame_count = decoder.GetFrameCount().expect("GetFrameCount failed");
            assert_eq!(frame_count, 1);

            let caps = decoder
                .QueryCapability(None)
                .expect("QueryCapability failed");
            assert_eq!(caps, 0x4);
        }
    }
}
