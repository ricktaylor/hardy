//! Streaming CBOR decoder.
//!
//! [`Decoder`] reads CBOR items from any [`Read`] source. It supports
//! the full RFC 8949 type set and provides streaming primitives for
//! constant-memory processing of arbitrarily large payloads.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use hardy_io::Read;

use crate::error::Error;

/// Default maximum allocation size for `read_bstr` / `read_tstr` (64 MB).
const DEFAULT_MAX_ALLOC: u64 = 64 * 1024 * 1024;

/// Streaming CBOR decoder over any [`Read`] source.
///
/// Reads CBOR items one at a time. Position tracking accounts for all
/// bytes read, including those consumed directly via [`inner()`](Self::inner).
///
/// The decoder enforces a maximum allocation size ([`set_max_alloc`](Self::set_max_alloc))
/// to prevent denial-of-service from malicious length fields. Default: 64 MB.
pub struct Decoder<R> {
    reader: R,
    peeked: Option<u8>,
    pos: u64,
    max_alloc: u64,
}

impl<R> Decoder<R> {
    /// Create a new decoder wrapping the given reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            peeked: None,
            pos: 0,
            max_alloc: DEFAULT_MAX_ALLOC,
        }
    }

    /// Set the maximum allocation size for `read_bstr` and `read_tstr`.
    ///
    /// Any definite-length string larger than this will return
    /// [`Error::InvalidCbor`]. Default: 64 MB.
    pub fn set_max_alloc(&mut self, max: u64) {
        self.max_alloc = max;
    }

    /// Current byte position in the stream.
    #[inline]
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Borrow the underlying reader for direct I/O.
    ///
    /// After reading `n` bytes directly, call [`advance`](Self::advance).
    #[inline]
    pub fn inner(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Consume the decoder and return the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Manually advance the position counter by `n` bytes.
    #[inline]
    pub fn advance(&mut self, n: u64) {
        self.pos += n;
    }
}

impl<R: Read> Decoder<R> {
    // -- internal helpers ---------------------------------------------------

    #[inline]
    fn read_byte(&mut self) -> Result<u8, Error> {
        if let Some(b) = self.peeked.take() {
            self.pos += 1;
            return Ok(b);
        }
        let mut buf = [0u8; 1];
        self.reader.read_exact(&mut buf)?;
        self.pos += 1;
        Ok(buf[0])
    }

    #[inline]
    fn peek_byte(&mut self) -> Result<u8, Error> {
        if let Some(b) = self.peeked {
            return Ok(b);
        }
        let mut buf = [0u8; 1];
        self.reader.read_exact(&mut buf)?;
        self.peeked = Some(buf[0]);
        Ok(buf[0])
    }

    #[inline]
    fn read_bytes_into(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.reader.read_exact(buf)?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    #[inline]
    fn read_bytes<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut buf = [0u8; N];
        self.read_bytes_into(&mut buf)?;
        Ok(buf)
    }

    /// Decode the argument from additional info (0-27).
    #[inline]
    fn read_argument(&mut self, additional: u8) -> Result<u64, Error> {
        match additional {
            v @ 0..24 => Ok(v as u64),
            24 => Ok(self.read_byte()? as u64),
            25 => Ok(u16::from_be_bytes(self.read_bytes::<2>()?) as u64),
            26 => Ok(u32::from_be_bytes(self.read_bytes::<4>()?) as u64),
            27 => Ok(u64::from_be_bytes(self.read_bytes::<8>()?)),
            _ => Err(Error::InvalidCbor),
        }
    }

    /// Read a CBOR head: (major_type, argument).
    /// Indefinite-length items return `u64::MAX` as argument.
    #[inline]
    fn read_head(&mut self) -> Result<(u8, u64), Error> {
        let initial = self.read_byte()?;
        let major = initial >> 5;
        let additional = initial & 0x1F;
        if additional == 31 {
            return Ok((major, u64::MAX));
        }
        let argument = self.read_argument(additional)?;
        Ok((major, argument))
    }

    #[inline]
    fn expect_major(&mut self, expected_major: u8, name: &'static str) -> Result<u64, Error> {
        let (major, arg) = self.read_head()?;
        if major != expected_major {
            return Err(Error::UnexpectedType {
                expected: name,
                actual: major,
            });
        }
        Ok(arg)
    }

    // -- type inspection ----------------------------------------------------

    /// Peek at the major type of the next item without consuming it.
    pub fn peek_major(&mut self) -> Result<u8, Error> {
        Ok(self.peek_byte()? >> 5)
    }

    /// Check if the next byte is the break code (`0xFF`).
    pub fn is_break(&mut self) -> Result<bool, Error> {
        Ok(self.peek_byte()? == 0xFF)
    }

    /// Consume the break code. Errors if next byte is not `0xFF`.
    pub fn read_break(&mut self) -> Result<(), Error> {
        let b = self.read_byte()?;
        if b != 0xFF {
            Err(Error::InvalidCbor)
        } else {
            Ok(())
        }
    }

    // -- major type 0: unsigned integer -------------------------------------

    /// Read an unsigned integer (major type 0).
    #[inline]
    pub fn read_uint(&mut self) -> Result<u64, Error> {
        self.expect_major(0, "unsigned integer")
    }

    // -- major type 1: negative integer -------------------------------------

    /// Read a negative integer (major type 1), returning raw `n` (value = -1 - n).
    #[inline]
    pub fn read_negint_raw(&mut self) -> Result<u64, Error> {
        self.expect_major(1, "negative integer")
    }

    /// Read an integer (major type 0 or 1) as `i64`.
    #[inline]
    pub fn read_int(&mut self) -> Result<i64, Error> {
        let (major, arg) = self.read_head()?;
        match major {
            0 => i64::try_from(arg).map_err(|_| Error::InvalidCbor),
            1 => {
                let val = i64::try_from(arg).map_err(|_| Error::InvalidCbor)?;
                Ok(-1 - val)
            }
            _ => Err(Error::UnexpectedType {
                expected: "integer",
                actual: major,
            }),
        }
    }

    // -- major type 2: byte string ------------------------------------------

    /// Read a byte string into a `Vec<u8>`.
    pub fn read_bstr(&mut self) -> Result<Vec<u8>, Error> {
        let len = self.expect_major(2, "byte string")?;
        if len == u64::MAX {
            return self.read_indefinite_bstr();
        }
        if len > self.max_alloc {
            return Err(Error::InvalidCbor);
        }
        let mut buf = vec![0u8; len as usize];
        self.read_bytes_into(&mut buf)?;
        Ok(buf)
    }

    /// Read the byte-string header only, returning the data length.
    ///
    /// After this, read exactly `len` bytes from [`inner()`](Self::inner)
    /// and call [`advance(len)`](Self::advance).
    #[inline]
    pub fn read_bstr_header(&mut self) -> Result<u64, Error> {
        self.expect_major(2, "byte string")
    }

    fn read_indefinite_bstr(&mut self) -> Result<Vec<u8>, Error> {
        let mut result = Vec::new();
        loop {
            if self.is_break()? {
                self.read_break()?;
                return Ok(result);
            }
            let len = self.expect_major(2, "byte string chunk")?;
            if len == u64::MAX || len > self.max_alloc {
                return Err(Error::InvalidCbor);
            }
            let new_len = result.len().saturating_add(len as usize);
            if new_len as u64 > self.max_alloc {
                return Err(Error::InvalidCbor);
            }
            let start = result.len();
            result.resize(new_len, 0);
            self.read_bytes_into(&mut result[start..])?;
        }
    }

    // -- major type 3: text string ------------------------------------------

    /// Read a text string.
    pub fn read_tstr(&mut self) -> Result<String, Error> {
        let len = self.expect_major(3, "text string")?;
        if len == u64::MAX {
            return self.read_indefinite_tstr();
        }
        if len > self.max_alloc {
            return Err(Error::InvalidCbor);
        }
        let mut buf = vec![0u8; len as usize];
        self.read_bytes_into(&mut buf)?;
        String::from_utf8(buf).map_err(|_| Error::InvalidUtf8)
    }

    fn read_indefinite_tstr(&mut self) -> Result<String, Error> {
        let mut bytes = Vec::new();
        loop {
            if self.is_break()? {
                self.read_break()?;
                return String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8);
            }
            let len = self.expect_major(3, "text string chunk")?;
            if len == u64::MAX || len > self.max_alloc {
                return Err(Error::InvalidCbor);
            }
            let new_len = bytes.len().saturating_add(len as usize);
            if new_len as u64 > self.max_alloc {
                return Err(Error::InvalidCbor);
            }
            let start = bytes.len();
            bytes.resize(new_len, 0);
            self.read_bytes_into(&mut bytes[start..])?;
        }
    }

    // -- major type 4: array ------------------------------------------------

    /// Read a definite-length array header.
    #[inline]
    pub fn read_array_len(&mut self) -> Result<usize, Error> {
        let arg = self.expect_major(4, "array")?;
        if arg == u64::MAX {
            return Err(Error::UnexpectedType {
                expected: "definite-length array",
                actual: 4,
            });
        }
        Ok(arg as usize)
    }

    /// Read the start of an indefinite-length array (`0x9F`).
    pub fn read_indefinite_array_start(&mut self) -> Result<(), Error> {
        let (major, arg) = self.read_head()?;
        if major != 4 || arg != u64::MAX {
            return Err(Error::UnexpectedType {
                expected: "indefinite-length array",
                actual: major,
            });
        }
        Ok(())
    }

    // -- major type 5: map --------------------------------------------------

    /// Read a definite-length map header (number of key-value pairs).
    pub fn read_map_len(&mut self) -> Result<usize, Error> {
        let arg = self.expect_major(5, "map")?;
        if arg == u64::MAX {
            return Err(Error::UnexpectedType {
                expected: "definite-length map",
                actual: 5,
            });
        }
        Ok(arg as usize)
    }

    /// Read the start of an indefinite-length map (`0xBF`).
    pub fn read_indefinite_map_start(&mut self) -> Result<(), Error> {
        let (major, arg) = self.read_head()?;
        if major != 5 || arg != u64::MAX {
            return Err(Error::UnexpectedType {
                expected: "indefinite-length map",
                actual: major,
            });
        }
        Ok(())
    }

    // -- major type 6: tag --------------------------------------------------

    /// Read a semantic tag number.
    #[inline]
    pub fn read_tag(&mut self) -> Result<u64, Error> {
        self.expect_major(6, "tag")
    }

    // -- major type 7: simple values and floats -----------------------------

    /// Read a boolean.
    pub fn read_bool(&mut self) -> Result<bool, Error> {
        let (major, arg) = self.read_head()?;
        if major != 7 {
            return Err(Error::UnexpectedType {
                expected: "boolean",
                actual: major,
            });
        }
        match arg {
            20 => Ok(false),
            21 => Ok(true),
            _ => Err(Error::UnexpectedType {
                expected: "boolean",
                actual: major,
            }),
        }
    }

    /// Read null (simple value 22).
    pub fn read_null(&mut self) -> Result<(), Error> {
        let (major, arg) = self.read_head()?;
        if major != 7 || arg != 22 {
            return Err(Error::UnexpectedType {
                expected: "null",
                actual: major,
            });
        }
        Ok(())
    }

    /// Read a floating-point value as `f64`. Accepts f16, f32, and f64 encodings.
    pub fn read_float(&mut self) -> Result<f64, Error> {
        let initial = self.read_byte()?;
        let major = initial >> 5;
        let additional = initial & 0x1F;
        if major != 7 {
            return Err(Error::UnexpectedType {
                expected: "float",
                actual: major,
            });
        }
        match additional {
            25 => Ok(half::f16::from_be_bytes(self.read_bytes::<2>()?).into()),
            26 => Ok(f32::from_be_bytes(self.read_bytes::<4>()?) as f64),
            27 => Ok(f64::from_be_bytes(self.read_bytes::<8>()?)),
            _ => Err(Error::UnexpectedType {
                expected: "float",
                actual: major,
            }),
        }
    }

    /// Read a simple value (0-255, excluding floats).
    pub fn read_simple(&mut self) -> Result<u8, Error> {
        let initial = self.read_byte()?;
        let major = initial >> 5;
        let additional = initial & 0x1F;
        if major != 7 || additional >= 25 {
            return Err(Error::UnexpectedType {
                expected: "simple value",
                actual: major,
            });
        }
        self.read_argument(additional).map(|v| v as u8)
    }

    // -- combined -----------------------------------------------------------

    /// Read either a uint or a text string.
    #[inline]
    pub fn read_uint_or_tstr(&mut self) -> Result<UintOrString, Error> {
        let initial = self.read_byte()?;
        let major = initial >> 5;
        let additional = initial & 0x1F;
        match major {
            0 => {
                let v = self.read_argument(additional)?;
                Ok(UintOrString::Uint(v))
            }
            3 => {
                let len = self.read_argument(additional)?;
                if len > self.max_alloc {
                    return Err(Error::InvalidCbor);
                }
                let len = len as usize;
                let mut buf = vec![0u8; len];
                self.read_bytes_into(&mut buf)?;
                let s = String::from_utf8(buf).map_err(|_| Error::InvalidUtf8)?;
                Ok(UintOrString::Tstr(s))
            }
            _ => Err(Error::UnexpectedType {
                expected: "unsigned integer or text string",
                actual: major,
            }),
        }
    }

    // -- streaming utilities ------------------------------------------------

    /// Skip `len` bytes without buffering.
    pub fn skip(&mut self, len: u64) -> Result<(), Error> {
        let mut remaining = len;
        let mut buf = [0u8; 4096];
        while remaining > 0 {
            let chunk = (remaining as usize).min(buf.len());
            self.reader.read_exact(&mut buf[..chunk])?;
            remaining -= chunk as u64;
        }
        self.pos += len;
        Ok(())
    }

    /// Skip one complete CBOR value (recursively for containers).
    ///
    /// `max_depth` limits nesting to prevent stack overflow on
    /// deeply nested input. Use 128 as a safe default.
    pub fn skip_value(&mut self, max_depth: usize) -> Result<(), Error> {
        if max_depth == 0 {
            return Err(Error::InvalidCbor);
        }
        let (major, arg) = self.read_head()?;
        match major {
            0 | 1 | 7 => Ok(()),
            2 | 3 => {
                if arg == u64::MAX {
                    loop {
                        if self.is_break()? {
                            self.read_break()?;
                            return Ok(());
                        }
                        let chunk_len = self.expect_major(major, "string chunk")?;
                        if chunk_len == u64::MAX {
                            return Err(Error::InvalidCbor);
                        }
                        self.skip(chunk_len)?;
                    }
                } else {
                    self.skip(arg)
                }
            }
            4 => {
                if arg == u64::MAX {
                    loop {
                        if self.is_break()? {
                            self.read_break()?;
                            return Ok(());
                        }
                        self.skip_value(max_depth - 1)?;
                    }
                } else {
                    for _ in 0..arg {
                        self.skip_value(max_depth - 1)?;
                    }
                    Ok(())
                }
            }
            5 => {
                if arg == u64::MAX {
                    loop {
                        if self.is_break()? {
                            self.read_break()?;
                            return Ok(());
                        }
                        self.skip_value(max_depth - 1)?;
                        self.skip_value(max_depth - 1)?;
                    }
                } else {
                    for _ in 0..arg {
                        self.skip_value(max_depth - 1)?;
                        self.skip_value(max_depth - 1)?;
                    }
                    Ok(())
                }
            }
            6 => self.skip_value(max_depth - 1),
            _ => unreachable!(),
        }
    }

    /// Capture one complete CBOR value into a buffer.
    ///
    /// `max_depth` limits nesting (use 128 as a safe default).
    /// The returned bytes are valid CBOR and can be passed to the
    /// buffer decoder ([`buffer::parse_value`](crate::buffer::parse_value)).
    pub fn capture_value(&mut self, max_depth: usize) -> Result<Vec<u8>, Error> {
        let mut buf = Vec::new();
        self.capture_value_into(&mut buf, max_depth)?;
        Ok(buf)
    }

    fn capture_value_into(&mut self, buf: &mut Vec<u8>, max_depth: usize) -> Result<(), Error> {
        if max_depth == 0 {
            return Err(Error::InvalidCbor);
        }
        let initial = self.read_byte()?;
        buf.push(initial);
        let major = initial >> 5;
        let additional = initial & 0x1F;

        let argument = match additional {
            v @ 0..24 => v as u64,
            24 => {
                let b = self.read_byte()?;
                buf.push(b);
                b as u64
            }
            25 => {
                let bytes = self.read_bytes::<2>()?;
                buf.extend_from_slice(&bytes);
                u16::from_be_bytes(bytes) as u64
            }
            26 => {
                let bytes = self.read_bytes::<4>()?;
                buf.extend_from_slice(&bytes);
                u32::from_be_bytes(bytes) as u64
            }
            27 => {
                let bytes = self.read_bytes::<8>()?;
                buf.extend_from_slice(&bytes);
                u64::from_be_bytes(bytes)
            }
            31 => u64::MAX,
            _ => return Err(Error::InvalidCbor),
        };

        match major {
            0 | 1 | 7 => Ok(()),
            2 | 3 => {
                if argument == u64::MAX {
                    loop {
                        if self.peek_byte()? == 0xFF {
                            buf.push(self.read_byte()?);
                            return Ok(());
                        }
                        self.capture_value_into(buf, max_depth - 1)?;
                    }
                } else {
                    let new_len = buf.len().saturating_add(argument as usize);
                    if argument > self.max_alloc || new_len as u64 > self.max_alloc {
                        return Err(Error::InvalidCbor);
                    }
                    let start = buf.len();
                    buf.resize(new_len, 0);
                    self.read_bytes_into(&mut buf[start..])
                }
            }
            4 => {
                if argument == u64::MAX {
                    loop {
                        if self.peek_byte()? == 0xFF {
                            buf.push(self.read_byte()?);
                            return Ok(());
                        }
                        self.capture_value_into(buf, max_depth - 1)?;
                    }
                } else {
                    for _ in 0..argument {
                        self.capture_value_into(buf, max_depth - 1)?;
                    }
                    Ok(())
                }
            }
            5 => {
                if argument == u64::MAX {
                    loop {
                        if self.peek_byte()? == 0xFF {
                            buf.push(self.read_byte()?);
                            return Ok(());
                        }
                        self.capture_value_into(buf, max_depth - 1)?;
                        self.capture_value_into(buf, max_depth - 1)?;
                    }
                } else {
                    for _ in 0..argument {
                        self.capture_value_into(buf, max_depth - 1)?;
                        self.capture_value_into(buf, max_depth - 1)?;
                    }
                    Ok(())
                }
            }
            6 => self.capture_value_into(buf, max_depth - 1),
            _ => unreachable!(),
        }
    }
}

/// Result of [`Decoder::read_uint_or_tstr`].
pub enum UintOrString {
    Uint(u64),
    Tstr(String),
}
