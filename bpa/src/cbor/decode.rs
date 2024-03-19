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

    pub fn try_parse_item<T, F>(&mut self, f: F) -> Result<T, anyhow::Error>
    where
        F: FnOnce(Value, usize, usize, Option<&[u64]>) -> Result<T, anyhow::Error>,
    {
        // Check for end of array
        if let Some(count) = self.count {
            if self.idx >= count {
                panic!("Read past end of array!")
            }
        }

        let item_start = *self.offset;
        match self.count {
            Some(count) => {
                if self.idx == count - 1 {
                    self.idx += 1;
                    return f(Value::End(*self.offset), self.idx, *self.offset, None);
                }
            }
            None => {
                if *self.offset >= self.data.len() {
                    return Err(Error::NotEnoughData.into());
                } else if self.data[*self.offset] == 0xFF {
                    *self.offset += 1;
                    self.idx += 1;
                    self.count = Some(self.idx);
                    return f(Value::End(*self.offset), self.idx, *self.offset - 1, None);
                }
            }
        }

        // Inc index
        let idx = self.idx + 1;

        // Parse sub-item
        parse(&self.data[*self.offset..], |value, tags| {
            f(value, idx, item_start, tags).map(|r| {
                self.idx = idx;
                r
            })
        })
        .map(|(r, o)| {
            *self.offset += o;
            r
        })
    }

    pub fn parse_uint(&mut self) -> Result<(u64, Option<Vec<u64>>), anyhow::Error> {
        self.try_parse_item(|value, _, _, tags| match (value, tags) {
            (Value::Uint(value), Some(tags)) => Ok((value, Some(tags.to_vec()))),
            (Value::Uint(value), None) => Ok((value, None)),
            _ => Err(Error::IncorrectType.into()),
        })
    }

    pub fn parse_end_or_else<F>(&mut self, f: F) -> Result<usize, anyhow::Error>
    where
        F: FnOnce() -> anyhow::Error,
    {
        self.try_parse_item(|value, _, _, _| {
            if let Value::End(end) = value {
                Ok(end)
            } else {
                Err(f())
            }
        })
    }
}

fn parse_tags(data: &[u8]) -> Result<(Option<Vec<u64>>, usize), Error> {
    let mut tags: Option<Vec<u64>> = None;
    let mut offset = 0;
    while offset < data.len() {
        match (data[offset] >> 5, data[offset] & 0x1F) {
            (6, minor) => {
                offset += 1;
                let (tag, o) = parse_uint_minor(minor, &data[offset..])?;
                if let Some(tags) = &mut tags {
                    tags.push(tag);
                } else {
                    tags = Some(vec![tag]);
                }
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

pub fn parse<T, F>(data: &[u8], f: F) -> Result<(T, usize), anyhow::Error>
where
    F: FnOnce(Value, Option<&[u64]>) -> Result<T, anyhow::Error>,
{
    let (tags, mut offset) = parse_tags(data)?;
    if offset >= data.len() {
        if tags.is_some() {
            return Err(Error::JustTags.into());
        } else {
            return Err(Error::NotEnoughData.into());
        }
    }

    match (data[offset] >> 5, data[offset] & 0x1F) {
        (0, minor) => {
            let (v, o) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(Value::Uint(v), tags.as_deref())
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
            f(Value::Bytes(&v, true), tags.as_deref())
        }
        (2, minor) => {
            /* Known length byte string */
            let (t, o) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(Value::Bytes(t, false), tags.as_deref())
        }
        (3, 31) => {
            /* Indefinite length text string */
            let (c, o) = parse_data_chunked(3, &data[offset + 1..])?;
            let s = c.into_iter().try_fold(String::new(), |mut s, b| {
                s.push_str(std::str::from_utf8(b)?);
                Ok::<String, Utf8Error>(s)
            })?;
            offset += o + 1;
            f(Value::Text(&s, true), tags.as_deref())
        }
        (3, minor) => {
            /* Known length text string */
            let (t, o) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(Value::Text(std::str::from_utf8(t)?, false), tags.as_deref())
        }
        (4, 31) => {
            /* Indefinite length array */
            offset += 1;
            f(
                Value::Array(Array::new(data, None, &mut offset)),
                tags.as_deref(),
            )
        }
        (4, minor) => {
            /* Known length array */
            let (len, o) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += o + 1;
            f(
                Value::Array(Array::new(data, Some(usize::try_from(len)?), &mut offset)),
                tags.as_deref(),
            )
        }
        (5, _) => todo!(),
        (7, _) => todo!(),
        (_, _) => unreachable!(),
    }
    .map(|r| (r, offset))
}
