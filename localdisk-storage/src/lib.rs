mod storage;

pub use storage::Storage;

pub const CONFIG_KEY: &str = "localdisk";

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}
