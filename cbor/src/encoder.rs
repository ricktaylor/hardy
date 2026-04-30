//! Streaming CBOR encoder.
//!
//! [`Encoder`] writes CBOR items to any [`Write`] sink with canonical
//! shortest-form encoding.

use hardy_io::Write;

use crate::error::Error;

/// Streaming CBOR encoder over any [`Write`] sink.
pub struct Encoder<W> {
    writer: W,
    pos: u64,
}

impl<W> Encoder<W> {
    /// Create a new encoder wrapping the given writer.
    pub fn new(writer: W) -> Self {
        Self { writer, pos: 0 }
    }

    /// Current byte position (total bytes written).
    #[inline]
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Borrow the underlying writer for direct I/O.
    ///
    /// After writing `n` bytes directly, call [`advance`](Self::advance).
    #[inline]
    pub fn inner(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Consume the encoder and return the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Manually advance the position counter by `n` bytes.
    #[inline]
    pub fn advance(&mut self, n: u64) {
        self.pos += n;
    }
}

impl<W: Write> Encoder<W> {
    #[inline]
    fn write_byte(&mut self, b: u8) -> Result<(), Error> {
        self.writer.write_all(&[b])?;
        self.pos += 1;
        Ok(())
    }

    #[inline]
    fn write_bytes(&mut self, data: &[u8]) -> Result<(), Error> {
        self.writer.write_all(data)?;
        self.pos += data.len() as u64;
        Ok(())
    }

    #[inline]
    fn write_head(&mut self, major: u8, value: u64) -> Result<(), Error> {
        let mut buf = [0u8; 9];
        let n = crate::head::encode_head(&mut buf, major, value);
        self.write_bytes(&buf[..n])
    }

    // -- major type 0 -------------------------------------------------------

    /// Write an unsigned integer.
    #[inline]
    pub fn write_uint(&mut self, v: u64) -> Result<(), Error> {
        self.write_head(0, v)
    }

    // -- major type 1 -------------------------------------------------------

    /// Write a negative integer from raw argument `n` (value = -1 - n).
    pub fn write_negint_raw(&mut self, n: u64) -> Result<(), Error> {
        self.write_head(1, n)
    }

    /// Write a signed integer.
    #[inline]
    pub fn write_int(&mut self, v: i64) -> Result<(), Error> {
        if v >= 0 {
            self.write_head(0, v as u64)
        } else {
            self.write_head(1, v.unsigned_abs() - 1)
        }
    }

    // -- major type 2 -------------------------------------------------------

    /// Write a complete byte string.
    #[inline]
    pub fn write_bstr(&mut self, data: &[u8]) -> Result<(), Error> {
        self.write_head(2, data.len() as u64)?;
        self.write_bytes(data)
    }

    /// Write a byte-string header only.
    ///
    /// After this, write exactly `len` bytes via [`inner()`](Self::inner)
    /// and call [`advance(len)`](Self::advance).
    #[inline]
    pub fn write_bstr_header(&mut self, len: u64) -> Result<(), Error> {
        self.write_head(2, len)
    }

    /// Start an indefinite-length byte string.
    pub fn write_indefinite_bstr(&mut self) -> Result<(), Error> {
        self.write_byte((2 << 5) | 31)
    }

    // -- major type 3 -------------------------------------------------------

    /// Write a complete text string.
    #[inline]
    pub fn write_tstr(&mut self, s: &str) -> Result<(), Error> {
        self.write_head(3, s.len() as u64)?;
        self.write_bytes(s.as_bytes())
    }

    /// Start an indefinite-length text string.
    pub fn write_indefinite_tstr(&mut self) -> Result<(), Error> {
        self.write_byte((3 << 5) | 31)
    }

    // -- major type 4 -------------------------------------------------------

    /// Write a definite-length array header.
    #[inline]
    pub fn write_array(&mut self, len: usize) -> Result<(), Error> {
        self.write_head(4, len as u64)
    }

    /// Start an indefinite-length array.
    pub fn write_indefinite_array(&mut self) -> Result<(), Error> {
        self.write_byte((4 << 5) | 31)
    }

    // -- major type 5 -------------------------------------------------------

    /// Write a definite-length map header.
    #[inline]
    pub fn write_map(&mut self, len: usize) -> Result<(), Error> {
        self.write_head(5, len as u64)
    }

    /// Start an indefinite-length map.
    pub fn write_indefinite_map(&mut self) -> Result<(), Error> {
        self.write_byte((5 << 5) | 31)
    }

    // -- major type 6 -------------------------------------------------------

    /// Write a semantic tag.
    #[inline]
    pub fn write_tag(&mut self, tag: u64) -> Result<(), Error> {
        self.write_head(6, tag)
    }

    // -- major type 7 -------------------------------------------------------

    /// Write a boolean.
    pub fn write_bool(&mut self, v: bool) -> Result<(), Error> {
        self.write_byte((7 << 5) | if v { 21 } else { 20 })
    }

    /// Write null.
    pub fn write_null(&mut self) -> Result<(), Error> {
        self.write_byte((7 << 5) | 22)
    }

    /// Write undefined.
    pub fn write_undefined(&mut self) -> Result<(), Error> {
        self.write_byte((7 << 5) | 23)
    }

    /// Write a simple value.
    pub fn write_simple(&mut self, v: u8) -> Result<(), Error> {
        self.write_head(7, v as u64)
    }

    /// Write an `f16`.
    pub fn write_f16(&mut self, v: half::f16) -> Result<(), Error> {
        self.write_byte((7 << 5) | 25)?;
        self.write_bytes(&v.to_be_bytes())
    }

    /// Write an `f32`.
    pub fn write_f32(&mut self, v: f32) -> Result<(), Error> {
        self.write_byte((7 << 5) | 26)?;
        self.write_bytes(&v.to_be_bytes())
    }

    /// Write an `f64`.
    pub fn write_f64(&mut self, v: f64) -> Result<(), Error> {
        self.write_byte((7 << 5) | 27)?;
        self.write_bytes(&v.to_be_bytes())
    }

    /// Write a float with canonical shortest-form encoding (f16 → f32 → f64).
    pub fn write_float_canonical(&mut self, v: f64) -> Result<(), Error> {
        let mut buf = [0u8; 9];
        let n = crate::head::encode_float_canonical(&mut buf, v);
        self.write_bytes(&buf[..n])
    }

    // -- structural ---------------------------------------------------------

    /// Write the break code (`0xFF`).
    pub fn write_break(&mut self) -> Result<(), Error> {
        self.write_byte(0xFF)
    }

    /// Write raw bytes without CBOR framing.
    #[inline]
    pub fn write_raw(&mut self, data: &[u8]) -> Result<(), Error> {
        self.write_bytes(data)
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.writer.flush()?;
        Ok(())
    }
}
