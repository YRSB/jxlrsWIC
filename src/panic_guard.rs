//! Containment for panics raised inside the decoder.
//!
//! A WIC Decoder runs in-process inside whatever application asked for it — File Explorer,
//! Photos, or a third-party viewer. An unwinding panic that reached the COM boundary would
//! unwind straight into `extern "system"` frames, which aborts the host process: opening one
//! malformed `.jxl` file would take the whole shell down with it.
//!
//! [`guard`] converts such a panic into an `HRESULT` instead, so WIC can report the image as
//! undecodable and move on.

use windows::core::{Result, HRESULT};
use windows::Win32::Foundation::E_UNEXPECTED;

/// WIC error raised when the bitstream cannot be decoded.
const WINCODEC_ERR_BADIMAGE: HRESULT = HRESULT(0x88982F60_u32 as i32);

/// Runs `body`, converting a panic into `WINCODEC_ERR_BADIMAGE`.
///
/// The panic payload is deliberately discarded: it originated in either `jxl-rs` parsing an
/// untrusted bitstream or in a decoder bug, and neither is something the host process can act
/// on beyond "this image did not decode". Reporting `WINCODEC_ERR_BADIMAGE` makes WIC fall
/// back cleanly (no thumbnail, no preview) rather than treating the file as valid.
pub(crate) fn guard<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_payload) => Err(WINCODEC_ERR_BADIMAGE.into()),
    }
}

/// Runs `body`, converting a panic into `E_UNEXPECTED`.
///
/// Used for calls that cannot fail because of the bitstream: if one of those panics, the
/// decoder itself is broken and the caller should not be told the image was at fault.
pub(crate) fn guard_unexpected<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_payload) => Err(E_UNEXPECTED.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_a_successful_result() {
        let value = guard(|| Ok(7u32)).expect("guard should not alter successful results");
        assert_eq!(value, 7);
    }

    #[test]
    fn passes_through_a_returned_error() {
        let err = guard::<()>(|| Err(E_UNEXPECTED.into())).expect_err("error should pass through");
        assert_eq!(err.code(), E_UNEXPECTED);
    }

    #[test]
    fn converts_a_panic_into_badimage() {
        let err = guard::<()>(|| panic!("bitstream parser exploded"))
            .expect_err("a panic must surface as an error");
        assert_eq!(err.code(), WINCODEC_ERR_BADIMAGE);
    }

    #[test]
    fn converts_a_panic_in_unexpected_guard_into_a_distinct_error() {
        let err = guard_unexpected::<()>(|| panic!("decoder bug"))
            .expect_err("a panic must surface as an error");
        assert_eq!(err.code(), E_UNEXPECTED);
    }
}
