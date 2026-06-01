#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to create directory '{path}': {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to watch outbox '{path}': {source}")]
    Watch { path: String, source: notify::Error },
}
