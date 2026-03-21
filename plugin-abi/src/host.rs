/*!
Host-side plugin loading infrastructure.

This module is only available when the `host` feature is enabled.
It provides functions for loading plugin shared libraries, verifying
their ABI tokens, and calling their factory entry points.

Used by `hardy-bpa-server` and `bp ping` (and any future tool that
needs to load CLA or other plugins at runtime).
*/

use super::*;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};

pub use libloading::Library;
use libloading::Symbol;

/// Entry point type for CLA factory functions.
///
/// `Arc<dyn Cla>` is not FFI-safe by Rust's definition, but is safe under
/// the same-rustc-version constraint enforced by the ABI token check.
#[allow(improper_ctypes_definitions)]
type ClaFactoryFn =
    unsafe extern "C" fn(*const std::ffi::c_char) -> PluginResult<Arc<dyn hardy_bpa::cla::Cla>>;

/// Load a shared library and verify its ABI token matches the host.
///
/// # Safety
///
/// Loads and executes arbitrary native code.
unsafe fn load_and_check(path: &Path) -> Result<Library, PluginLoadError> {
    let lib = unsafe {
        Library::new(path).map_err(|e| PluginLoadError::Load {
            path: path.to_path_buf(),
            source: e,
        })?
    };

    let token: Symbol<&&str> =
        unsafe { lib.get(b"HARDY_ABI_TOKEN") }.map_err(|_| PluginLoadError::MissingAbiToken {
            path: path.to_path_buf(),
        })?;

    if **token != ABI_TOKEN {
        return Err(PluginLoadError::AbiMismatch {
            path: path.to_path_buf(),
            host: ABI_TOKEN.to_string(),
            plugin: (**token).to_string(),
        });
    }

    debug!("Loaded plugin: {} (ABI OK)", path.display());
    Ok(lib)
}

/// Load a CLA plugin by file path and call its factory function.
///
/// Returns the `Library` handle (caller must keep it alive for the
/// lifetime of the returned CLA) and the CLA trait object.
///
/// # Safety
///
/// Loads and executes arbitrary native code from `path`.
pub unsafe fn load_cla_plugin(
    path: &Path,
    config_json: &str,
) -> Result<(Library, Arc<dyn hardy_bpa::cla::Cla>), PluginLoadError> {
    info!("Loading CLA plugin: {}", path.display());
    let lib = unsafe { load_and_check(path)? };

    let factory: Symbol<ClaFactoryFn> =
        unsafe { lib.get(b"hardy_create_cla") }.map_err(|_| PluginLoadError::MissingSymbol {
            path: path.to_path_buf(),
            symbol: "hardy_create_cla".to_string(),
        })?;

    let config_cstr = CString::new(config_json).map_err(|_| PluginLoadError::InvalidConfig {
        reason: "config JSON contains null byte".to_string(),
    })?;

    match unsafe { factory(config_cstr.as_ptr()) } {
        PluginResult::Ok(cla) => Ok((lib, cla)),
        PluginResult::Err(code) => Err(PluginLoadError::FactoryFailed {
            path: path.to_path_buf(),
            symbol: "hardy_create_cla".to_string(),
            code,
        }),
    }
}

/// Errors that can occur during plugin loading.
#[derive(Debug, Error)]
pub enum PluginLoadError {
    #[error("Failed to load plugin {}: {source}", path.display())]
    Load {
        path: PathBuf,
        source: libloading::Error,
    },

    #[error("{}: missing HARDY_ABI_TOKEN symbol", path.display())]
    MissingAbiToken { path: PathBuf },

    #[error("{}: ABI mismatch (host={host}, plugin={plugin})", path.display())]
    AbiMismatch {
        path: PathBuf,
        host: String,
        plugin: String,
    },

    #[error("{}: missing {symbol} symbol", path.display())]
    MissingSymbol { path: PathBuf, symbol: String },

    #[error("{}: {symbol} returned error code {code}", path.display())]
    FactoryFailed {
        path: PathBuf,
        symbol: String,
        code: i32,
    },

    #[error("Invalid plugin config: {reason}")]
    InvalidConfig { reason: String },
}
