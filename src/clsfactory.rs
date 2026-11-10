//! Minimal COM class factory for jxlrsWIC decoder.

#![allow(missing_docs)]

use crate::decoder::JxlWicDecoder;
use core::ffi::c_void;
use windows::core::{implement, IUnknown, Interface, Result, GUID};
use windows::Win32::Foundation::{BOOL, CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::Graphics::Imaging::IWICBitmapDecoder;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};

/// Class factory for creating [`JxlWicDecoder`] instances.
#[implement(IClassFactory)]
pub struct JxlWicClassFactory;

impl JxlWicClassFactory {
    /// Creates a new class factory instance.
    pub fn new() -> Self {
        Self
    }
}

impl Default for JxlWicClassFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl IClassFactory_Impl for JxlWicClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Option<&IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppvobject = std::ptr::null_mut() };

        if punkouter.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }

        let decoder: IWICBitmapDecoder = JxlWicDecoder::new().into();
        unsafe { decoder.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        if flock.as_bool() {
            crate::inc_server_lock();
        } else {
            crate::dec_server_lock();
        }
        Ok(())
    }
}
