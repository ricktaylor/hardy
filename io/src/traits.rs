use alloc::vec::Vec;

use crate::Error;

/// Portable read trait.
pub trait Read {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error>;

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        let mut offset = 0;
        while offset < buf.len() {
            let n = self.read(&mut buf[offset..])?;
            if n == 0 {
                return Err(Error::UnexpectedEof);
            }
            offset += n;
        }
        Ok(())
    }

    fn read_to_end(&mut self, out: &mut Vec<u8>) -> Result<usize, Error> {
        let mut buf = [0u8; 4096];
        let mut total = 0;
        loop {
            let n = self.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            total += n;
        }
        Ok(total)
    }
}

/// Portable write trait.
pub trait Write {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
}

// -- std blanket impls -------------------------------------------------------

#[cfg(feature = "std")]
impl<R: std::io::Read> Read for R {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        std::io::Read::read(self, buf).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::UnexpectedEof
            } else {
                Error::Io(e)
            }
        })
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        std::io::Read::read_exact(self, buf).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::UnexpectedEof
            } else {
                Error::Io(e)
            }
        })
    }
}

#[cfg(feature = "std")]
impl<W: std::io::Write> Write for W {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
        std::io::Write::write_all(self, buf).map_err(Error::Io)
    }

    fn flush(&mut self) -> Result<(), Error> {
        std::io::Write::flush(self).map_err(Error::Io)
    }
}

// -- embedded-io blanket impls -----------------------------------------------

#[cfg(not(feature = "std"))]
impl<R: embedded_io::Read> Read for R {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        embedded_io::Read::read(self, buf).map_err(|_| Error::Io)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        embedded_io::Read::read_exact(self, buf).map_err(|e| match e {
            embedded_io::ReadExactError::UnexpectedEof => Error::UnexpectedEof,
            embedded_io::ReadExactError::Other(_) => Error::Io,
        })
    }
}

#[cfg(not(feature = "std"))]
impl<W: embedded_io::Write> Write for W {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
        embedded_io::Write::write_all(self, buf).map_err(|_| Error::Io)
    }

    fn flush(&mut self) -> Result<(), Error> {
        embedded_io::Write::flush(self).map_err(|_| Error::Io)
    }
}
