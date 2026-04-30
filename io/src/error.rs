use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("unexpected end of input")]
    UnexpectedEof,

    #[error("I/O error")]
    #[cfg(not(feature = "std"))]
    Io,

    #[error(transparent)]
    #[cfg(feature = "std")]
    Io(#[from] std::io::Error),
}
