use std::str::Utf8Error;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Not enough data for encoded value")]
    NotEnoughData,

    #[error("Invalid minor-type value {0}")]
    InvalidMinorValue(u8),

    #[error("Tags with no following value")]
    JustTags,

    #[error("Incorrect type")]
    IncorrectType,

    #[error("Chunked string contains an invalid chunk")]
    InvalidChunk,
}

pub trait FromCBOR {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error>
    where
        Self: Sized;
}

pub enum Value<'a> {
    End(usize),
    Uint(u64),
    Bytes(&'a [u8], bool),
    Text(&'a str, bool),
    Array(Array<'a>),
}

pub struct Array<'a> {
    data: &'a [u8],
    count: Option<usize>,
    offset: &'a mut usize,
    idx: usize,
}

impl<'a> Array<'a> {
    fn new(data: &'a [u8], count: Option<usize>, offset: &'a mut usize) -> Self {
        Self {
            data,
            count,
            offset,
            idx: 0,
        }
    }

    pub fn count(&self) -> Option<usize> {
        self.count
    }

    fn check_for_end(&mut self) -> Result<Option<usize>, anyhow::Error> {
        // Check for end of array
        if let Some(count) = self.count {
            if self.idx >= count {
                panic!("Read past end of array!")
            }
        }
        if *self.offset >= self.data.len() {
            return Err(Error::NotEnoughData.into());
        }

        match self.count {
            Some(count) if self.idx == count => {
                self.idx += 1;
                Ok(Some(*self.offset))
            }
            None if self.data[*self.offset] == 0xFF => {
                self.idx += 1;
                *self.offset += 1;
                self.count = Some(self.idx);
                Ok(Some(*self.offset - 1))
            }
            _ => Ok(None),
        }
    }

    pub fn try_parse_item<T, F>(&mut self, f: F) -> Result<T, anyhow::Error>
    where
        F: FnOnce(Value, usize, &[u64]) -> Result<T, anyhow::Error>,
    {
        // Check for end of array
        if let Some(end_start) = self.check_for_end()? {
            return f(Value::End(*self.offset), end_start, &[]);
        }

        // Parse sub-item
        let item_start = *self.offset;
        parse_value(&self.data[*self.offset..], |value, tags| {
            f(value, item_start, tags)
        })
        .map(|(r, o)| {
            self.idx += 1;
            *self.offset += o;
            r
        })
    }

    pub fn parse_end_or_else<F>(&mut self, f: F) -> Result<usize, anyhow::Error>
    where
        F: FnOnce() -> anyhow::Error,
    {
        self.try_parse_item(|value, _, _| {
            if let Value::End(end) = value {
                Ok(end)
            } else {
                Err(f())
            }
        })
    }

    pub fn try_parse<T>(&mut self) -> Result<Option<(T, usize, Vec<u64>)>, anyhow::Error>
    where
        T: FromCBOR,
    {
        // Check for end of array
        if self.check_for_end()?.is_some() {
            Ok(None)
        } else {
            // Parse sub-item
            let item_start = *self.offset;
            let (v, o, tags) = T::from_cbor(&self.data[*self.offset..])?;
            *self.offset += o;
            self.idx += 1;
            Ok(Some((v, item_start, tags)))
        }
    }

    pub fn parse<T>(&mut self) -> Result<(T, usize, Vec<u64>), anyhow::Error>
    where
        T: FromCBOR,
    {
        match self.try_parse::<T>()? {
            None => Err(Error::NotEnoughData.into()),
            Some(r) => Ok(r),
        }
    }
}

fn parse_tags(data: &[u8]) -> Result<(Vec<u64>, usize), Error> {
    let mut tags = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        match (data[offset] >> 5, data[offset] & 0x1F) {
            (6, minor) => {
                offset += 1;
                let (tag, o) = parse_uint_minor(minor, &data[offset..])?;
                tags.push(tag);
                offset += o;
            }
            _ => break,
        }
    }
    Ok((tags, offset))
}

fn parse_uint_minor(minor: u8, data: &[u8]) -> Result<(u64, usize), Error> {
    match minor {
        24 => {
            if data.is_empty() {
                Err(Error::NotEnoughData)
            } else {
                Ok((data[0] as u64, 1))
            }
        }
        25 => {
            if data.len() < 2 {
                Err(Error::NotEnoughData)
            } else {
                Ok((((data[0] as u64) << 8) | (data[1] as u64), 2))
            }
        }
        26 => {
            if data.len() < 4 {
                Err(Error::NotEnoughData)
            } else {
                Ok((
                    ((data[0] as u64) << 24)
                        | ((data[1] as u64) << 16)
                        | ((data[2] as u64) << 8)
                        | (data[3] as u64),
                    4,
                ))
            }
        }
        27 => {
            if data.len() < 8 {
                Err(Error::NotEnoughData)
            } else {
                Ok((
                    ((data[0] as u64) << 56)
                        | ((data[1] as u64) << 48)
                        | ((data[2] as u64) << 40)
                        | ((data[3] as u64) << 32)
                        | ((data[4] as u64) << 24)
                        | ((data[5] as u64) << 16)
                        | ((data[6] as u64) << 8)
                        | (data[7] as u64),
                    8,
                ))
            }
        }
        val if val < 24 => Ok((val as u64, 0)),
        _ => Err(Error::InvalidMinorValue(minor)),
    }
}

fn parse_data_minor(minor: u8, data: &[u8]) -> Result<(&[u8], usize), Error> {
    let (len, o) = parse_uint_minor(minor, data)?;
    if (o as u64) + len >= data.len() as u64 {
        Err(Error::NotEnoughData)
    } else {
        let end = ((o as u64) + len) as usize;
        Ok((&data[o..end], end))
    }
}

fn parse_data_chunked(major: u8, data: &[u8]) -> Result<(Vec<&[u8]>, usize), Error> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    loop {
        if data.is_empty() {
            return Err(Error::NotEnoughData);
        }

        if data[offset] >> 5 != major {
            return Err(Error::InvalidChunk);
        }

        let minor = data[offset] & 0x1F;
        offset += 1;
        if minor == 31 {
            break Ok((chunks, offset));
        }

        let (chunk, o) = parse_data_minor(minor, &data[offset..])?;
        chunks.push(chunk);
        offset += o;
    }
}

pub fn parse_value<T, F>(data: &[u8], f: F) -> Result<(T, usize), anyhow::Error>
where
    F: FnOnce(Value, &[u64]) -> Result<T, anyhow::Error>,
{
    let (tags, mut offset) = parse_tags(data)?;
    if offset >= data.len() {
        if !tags.is_empty() {
            return Err(Error::JustTags.into());
        } else {
            return Err(Error::NotEnoughData.into());
        }
    }

    match (data[offset] >> 5, data[offset] & 0x1F) {
        (0, minor) => {
            let (v, o) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(Value::Uint(v), &tags)
        }
        (1, _) => todo!(),
        (2, 31) => {
            /* Indefinite length byte string */
            let (c, o) = parse_data_chunked(3, &data[offset + 1..])?;
            let v = c.into_iter().try_fold(Vec::new(), |mut v, b| {
                v.extend_from_slice(b);
                Ok::<Vec<u8>, anyhow::Error>(v)
            })?;
            offset += o + 1;
            f(Value::Bytes(&v, true), &tags)
        }
        (2, minor) => {
            /* Known length byte string */
            let (t, o) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(Value::Bytes(t, false), &tags)
        }
        (3, 31) => {
            /* Indefinite length text string */
            let (c, o) = parse_data_chunked(3, &data[offset + 1..])?;
            let s = c.into_iter().try_fold(String::new(), |mut s, b| {
                s.push_str(std::str::from_utf8(b)?);
                Ok::<String, Utf8Error>(s)
            })?;
            offset += o + 1;
            f(Value::Text(&s, true), &tags)
        }
        (3, minor) => {
            /* Known length text string */
            let (t, o) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(Value::Text(std::str::from_utf8(t)?, false), &tags)
        }
        (4, 31) => {
            /* Indefinite length array */
            offset += 1;
            f(Value::Array(Array::new(data, None, &mut offset)), &tags)
        }
        (4, minor) => {
            /* Known length array */
            let (len, o) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(
                Value::Array(Array::new(data, Some(usize::try_from(len)?), &mut offset)),
                &tags,
            )
        }
        (5, _) => todo!(),
        (7, _) => todo!(),
        (_, _) => unreachable!(),
    }
    .map(|r| (r, offset))
}

pub fn parse<T>(data: &[u8]) -> Result<(T, usize, Vec<u64>), anyhow::Error>
where
    T: FromCBOR,
{
    T::from_cbor(data)
}

impl FromCBOR for u64 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error>
    where
        Self: Sized,
    {
        parse_value(data, |value, tags| match (value, tags) {
            (Value::Uint(value), tags) => Ok((value, tags.to_vec())),
            _ => Err(Error::IncorrectType.into()),
        })
        .map(|((val, tags), o)| (val, o, tags))
    }
}
