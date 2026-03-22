/*!
Host-side plugin loading infrastructure.

This module is only available when the `host` feature is enabled.
It provides functions for loading plugin shared libraries, verifying
their ABI tokens, and calling their factory entry points.

Used by `hardy-bpa-server` and `bp ping` (and any future tool that
needs to load CLA or other plugins at runtime).

## Apartment Pattern

`cdylib` plugins link their own copy of tokio with separate thread-local
storage. Calls from the plugin's runtime threads into the host's BPA code
(via trait method vtables) fail because `tokio::spawn` can't find the
host's runtime in the plugin's TLS.

This is solved with an apartment pattern inspired by Windows COM: trait
objects given to plugins are wrapped in channel-based proxies. Each method
call is serialized into a message, sent over a channel, and executed by a
dispatcher task on the host's runtime. The plugin sees a normal trait
object; the channel crossing is invisible.

Each trait family (CLA Sink, future Service Sink, etc.) has its own proxy
implementation in a submodule.
*/

mod cla;

use super::*;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::debug;

pub use libloading::Library;
use libloading::Symbol;

// Re-export the CLA loader as the public API
pub use cla::load_cla_plugin;

/// Load a shared library and verify its ABI token matches the host.
///
/// # Safety
///
/// Loads and executes arbitrary native code.
pub(crate) unsafe fn load_and_check(path: &Path) -> Result<Library, PluginLoadError> {
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
