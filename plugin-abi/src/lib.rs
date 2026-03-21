/*!
ABI boundary types and plugin loading for Hardy BPA plugins.

This crate provides the FFI mechanics for the Hardy plugin system. It has
two facets, controlled by the `host` feature flag:

- **Default** (plugin-side): [`PluginResult`], [`parse_config`],
  [`guard`], [`guard_factory`], [`ABI_TOKEN`]. Used by plugin crates
  (`cdylib`) to implement entry points safely.

- **`host` feature** (host-side): [`host::load_cla_plugin`] and friends.
  Used by binaries that load plugins (`hardy-bpa-server`, `bp ping`).

This crate does **not** re-export `hardy-bpa` trait types. Plugin crates
depend on `hardy-bpa` directly for trait definitions.
*/

use std::ffi::{CStr, c_char};

#[cfg(feature = "host")]
pub mod host;

mod client;

/// ABI version token embedding the crate version and Rust compiler version.
///
/// Both the server and plugin embed this token. The host checks that they
/// match before calling any other entry point, catching the most common
/// mistake (plugin compiled against a different Hardy release or Rust
/// toolchain) with a clear error instead of undefined behaviour.
///
/// Plugin crates export this as:
/// ```ignore
/// #[unsafe(no_mangle)]
/// pub static HARDY_ABI_TOKEN: &str = hardy_plugin_abi::ABI_TOKEN;
/// ```
pub const ABI_TOKEN: &str = concat!(env!("CARGO_PKG_VERSION"), "-", env!("RUSTC_VERSION"));

/// C-compatible result type for plugin factory entry points.
///
/// Used by entry points that return a trait object (e.g., `hardy_create_cla`).
/// The `#[repr(C)]` ensures the discriminant layout is predictable across
/// the FFI boundary (under the same-`rustc`-version constraint).
#[repr(C)]
pub enum PluginResult<T> {
    Ok(T),
    Err(i32),
}

/// Errors from plugin config parsing.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("null config pointer")]
    NullConfig,

    #[error("config string is not valid UTF-8")]
    InvalidUtf8,

    #[error("config JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Parse a C string config pointer into a deserialized Rust type.
///
/// Converts the `*const c_char` config pointer from a plugin entry point
/// into a deserialized `T`. If `ptr` is null, returns `PluginError::NullConfig`.
///
/// # Safety
///
/// `ptr` must be null or point to a valid, NUL-terminated C string.
pub unsafe fn parse_config<T: serde::de::DeserializeOwned>(
    ptr: *const c_char,
) -> Result<T, PluginError> {
    if ptr.is_null() {
        return Err(PluginError::NullConfig);
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    let s = cstr.to_str().map_err(|_| PluginError::InvalidUtf8)?;
    serde_json::from_str(s).map_err(PluginError::Json)
}

/// Wrap a registration entry point body with panic catching.
///
/// A panic unwinding across an `extern "C"` boundary is undefined behaviour.
/// This converts panics into error code -99.
///
/// Returns 0 on success, the error code on failure, or -99 on panic.
pub fn guard(f: impl FnOnce() -> Result<(), i32>) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(code)) => code,
        Err(_) => -99,
    }
}

/// Wrap a factory entry point body with panic catching.
///
/// Returns `PluginResult::Ok(value)` on success, `PluginResult::Err(code)`
/// on failure, or `PluginResult::Err(-99)` on panic.
pub fn guard_factory<T>(f: impl FnOnce() -> Result<T, i32>) -> PluginResult<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(val)) => PluginResult::Ok(val),
        Ok(Err(code)) => PluginResult::Err(code),
        Err(_) => PluginResult::Err(-99),
    }
}
