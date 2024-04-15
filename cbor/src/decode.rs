use num_traits::FromPrimitive;
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

    #[error("Invalid Value type {0}")]
    InvalidValue(u8),
}

pub trait FromCbor: Sized {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error>;
}

pub enum Value<'a> {
    UnsignedInteger(u64),
    NegativeInteger(u64),
    Bytes(&'a [u8], bool),
    Text(&'a str, bool),
    Array(Array<'a>),
    Map(Map),
    False,
    True,
    Null,
    Undefined,
    Unassigned(u8),
    Float(f64),
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

    fn check_for_end(&mut self) -> Result<Option<(usize, usize)>, anyhow::Error> {
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
                Ok(Some((*self.offset, 0)))
            }
            None if self.data[*self.offset] == 0xFF => {
                self.idx += 1;
                *self.offset += 1;
                self.count = Some(self.idx);
                Ok(Some((*self.offset - 1, 1)))
            }
            _ => Ok(None),
        }
    }

    pub fn parse_end_or_else<F>(&mut self, f: F) -> Result<usize, anyhow::Error>
    where
        F: FnOnce() -> anyhow::Error,
    {
        if let Some((offset, len)) = self.check_for_end()? {
            Ok(offset + len)
        } else {
            Err(f())
        }
    }

    pub fn try_parse_value<T, F>(&mut self, f: F) -> Result<Option<(T, usize)>, anyhow::Error>
    where
        F: FnOnce(Value, usize, &[u64]) -> Result<T, anyhow::Error>,
    {
        // Check for end of array
        if self.check_for_end()?.is_some() {
            Ok(None)
        } else {
            // Parse sub-item
            let item_start = *self.offset;
            try_parse_value(&self.data[*self.offset..], |value, tags| {
                f(value, item_start, tags)
            })
            .map(|o| {
                o.map(|(r, len)| {
                    self.idx += 1;
                    *self.offset += len;
                    (r, len)
                })
            })
        }
    }

    pub fn parse_value<T, F>(&mut self, f: F) -> Result<(T, usize), anyhow::Error>
    where
        F: FnOnce(Value, usize, &[u64]) -> Result<T, anyhow::Error>,
    {
        self.try_parse_value(f)?.ok_or(Error::NotEnoughData.into())
    }

    pub fn try_parse<T>(&mut self) -> Result<Option<T>, anyhow::Error>
    where
        T: FromCbor,
    {
        // Check for end of array
        if self.check_for_end()?.is_some() {
            Ok(None)
        } else {
            // Parse sub-item
            T::from_cbor(&self.data[*self.offset..]).map(|(value, len, _)| {
                self.idx += 1;
                *self.offset += len;
                Some(value)
            })
        }
    }

    pub fn parse<T>(&mut self) -> Result<T, anyhow::Error>
    where
        T: FromCbor,
    {
        self.try_parse()?.ok_or(Error::NotEnoughData.into())
    }
}

pub struct Map {}

impl Map {
    fn new<'a>(_data: &'a [u8], _count: Option<usize>, _offset: &'a mut usize) -> Self {
        Self {}
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
        25 => Ok((
            u16::from_be_bytes(data.try_into().map_err(|_| Error::NotEnoughData)?) as u64,
            2,
        )),
        26 => Ok((
            u32::from_be_bytes(data.try_into().map_err(|_| Error::NotEnoughData)?) as u64,
            4,
        )),
        27 => Ok((
            u64::from_be_bytes(data.try_into().map_err(|_| Error::NotEnoughData)?),
            8,
        )),
        val if val < 24 => Ok((val as u64, 0)),
        _ => Err(Error::InvalidMinorValue(minor)),
    }
}

fn parse_data_minor(minor: u8, data: &[u8]) -> Result<(&[u8], usize), Error> {
    let (data_len, len) = parse_uint_minor(minor, data)?;
    if (len as u64) + data_len >= data.len() as u64 {
        Err(Error::NotEnoughData)
    } else {
        let end = ((len as u64) + data_len) as usize;
        Ok((&data[len..end], end))
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

        let (chunk, chunk_len) = parse_data_minor(minor, &data[offset..])?;
        chunks.push(chunk);
        offset += chunk_len;
    }
}

pub fn try_parse_value<T, F>(data: &[u8], f: F) -> Result<Option<(T, usize)>, anyhow::Error>
where
    F: FnOnce(Value, &[u64]) -> Result<T, anyhow::Error>,
{
    let (tags, mut offset) = parse_tags(data)?;
    if offset >= data.len() {
        if !tags.is_empty() {
            return Err(Error::JustTags.into());
        } else {
            return Ok(None);
        }
    }

    match (data[offset] >> 5, data[offset] & 0x1F) {
        (0, minor) => {
            let (v, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::UnsignedInteger(v), &tags)
        }
        (1, minor) => {
            let (v, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::NegativeInteger(v), &tags)
        }
        (2, 31) => {
            /* Indefinite length byte string */
            let (c, len) = parse_data_chunked(3, &data[offset + 1..])?;
            let v = c.into_iter().try_fold(Vec::new(), |mut v, b| {
                v.extend_from_slice(b);
                Ok::<Vec<u8>, anyhow::Error>(v)
            })?;
            offset += len + 1;
            f(Value::Bytes(&v, true), &tags)
        }
        (2, minor) => {
            /* Known length byte string */
            let (t, len) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::Bytes(t, false), &tags)
        }
        (3, 31) => {
            /* Indefinite length text string */
            let (c, len) = parse_data_chunked(3, &data[offset + 1..])?;
            let s = c.into_iter().try_fold(String::new(), |mut s, b| {
                s.push_str(std::str::from_utf8(b)?);
                Ok::<String, Utf8Error>(s)
            })?;
            offset += len + 1;
            f(Value::Text(&s, true), &tags)
        }
        (3, minor) => {
            /* Known length text string */
            let (t, len) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::Text(std::str::from_utf8(t)?, false), &tags)
        }
        (4, 31) => {
            /* Indefinite length array */
            offset += 1;
            f(Value::Array(Array::new(data, None, &mut offset)), &tags)
        }
        (4, minor) => {
            /* Known length array */
            let (count, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(
                Value::Array(Array::new(data, Some(usize::try_from(count)?), &mut offset)),
                &tags,
            )
        }
        (5, 31) => {
            /* Indefinite length map */
            offset += 1;
            f(Value::Map(Map::new(data, None, &mut offset)), &tags)
        }
        (5, minor) => {
            /* Known length array */
            let (count, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(
                Value::Map(Map::new(data, Some(usize::try_from(count)?), &mut offset)),
                &tags,
            )
        }
        (6, _) => unreachable!(),
        (7, 20) => {
            /* False */
            offset += 1;
            f(Value::False, &tags)
        }
        (7, 21) => {
            /* True */
            offset += 1;
            f(Value::True, &tags)
        }
        (7, 22) => {
            /* Null */
            offset += 1;
            f(Value::Null, &tags)
        }
        (7, 23) => {
            /* Undefined */
            offset += 1;
            f(Value::Undefined, &tags)
        }
        (7, 0..=19) => {
            /* Unassigned */
            offset += 1;
            f(Value::Unassigned(data[offset] & 0x1F), &tags)
        }
        (7, 24) => {
            /* Unassigned */
            if data.len() <= offset + 1 {
                return Err(Error::NotEnoughData.into());
            }
            let v = data[offset + 1];
            if v < 32 {
                return Err(Error::InvalidValue(v).into());
            }
            offset += 2;
            f(Value::Unassigned(v), &tags)
        }
        (7, 25) => {
            /* FP16 */
            let v = half::f16::from_be_bytes(
                data[offset + 1..]
                    .try_into()
                    .map_err(|_| Error::NotEnoughData)?,
            );
            offset += 3;
            f(Value::Float(v.into()), &tags)
        }
        (7, 26) => {
            /* FP32 */
            let v = f32::from_be_bytes(
                data[offset + 1..]
                    .try_into()
                    .map_err(|_| Error::NotEnoughData)?,
            );
            offset += 5;
            f(Value::Float(v.into()), &tags)
        }
        (7, 27) => {
            /* FP64 */
            let v = f64::from_be_bytes(
                data[offset + 1..]
                    .try_into()
                    .map_err(|_| Error::NotEnoughData)?,
            );
            offset += 9;
            f(Value::Float(v), &tags)
        }
        (7, _) => {
            return Err(Error::InvalidValue(data[offset] & 0x1F).into());
        }
        (8.., _) => unreachable!(),
    }
    .map(|r| Some((r, offset)))
}

pub fn parse_value<T, F>(data: &[u8], f: F) -> Result<(T, usize), anyhow::Error>
where
    F: FnOnce(Value, &[u64]) -> Result<T, anyhow::Error>,
{
    try_parse_value(data, f)?.ok_or(Error::NotEnoughData.into())
}

pub fn parse_detail<T>(data: &[u8]) -> Result<(T, usize, Vec<u64>), anyhow::Error>
where
    T: FromCbor,
{
    T::from_cbor(data)
}

pub fn parse<T>(data: &[u8]) -> Result<T, anyhow::Error>
where
    T: FromCbor,
{
    T::from_cbor(data).map(|(v, _, _)| v)
}

impl FromCbor for u8 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<u64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for u16 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<u64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for u32 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<u64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for usize {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<u64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for u64 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        parse_value(data, |value, tags| match (value, tags) {
            (Value::UnsignedInteger(value), tags) => Ok((value, tags.to_vec())),
            _ => Err(Error::IncorrectType.into()),
        })
        .map(|((val, tags), len)| (val, len, tags))
    }
}

impl FromCbor for i8 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<i64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for i16 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<i64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for i32 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<i64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for isize {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<i64>(data)?;
        Ok((v.try_into()?, len, tags))
    }
}

impl FromCbor for i64 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        parse_value(data, |value, tags| match (value, tags) {
            (Value::UnsignedInteger(value), tags) => {
                Ok((<u64 as TryInto<i64>>::try_into(value)?, tags.to_vec()))
            }
            (Value::NegativeInteger(value), tags) => Ok((
                -1i64 - <u64 as TryInto<i64>>::try_into(value)?,
                tags.to_vec(),
            )),
            _ => Err(Error::IncorrectType.into()),
        })
        .map(|((val, tags), len)| (val, len, tags))
    }
}

impl FromCbor for f32 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (v, len, tags) = parse_detail::<f64>(data)?;
        Ok((f32::from_f64(v).ok_or(Error::IncorrectType)?, len, tags))
    }
}

impl FromCbor for f64 {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        parse_value(data, |value, tags| match (value, tags) {
            (Value::Float(value), tags) => Ok((value, tags.to_vec())),
            _ => Err(Error::IncorrectType.into()),
        })
        .map(|((val, tags), len)| (val, len, tags))
    }
}

impl FromCbor for bool {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        parse_value(data, |value, tags| match (value, tags) {
            (Value::False, tags) => Ok((false, tags.to_vec())),
            (Value::True, tags) => Ok((true, tags.to_vec())),
            _ => Err(Error::IncorrectType.into()),
        })
        .map(|((val, tags), len)| (val, len, tags))
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
{
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (tags, offset) = parse_tags(data)?;
        if offset >= data.len() {
            if !tags.is_empty() {
                return Err(Error::JustTags.into());
            } else {
                return Err(Error::NotEnoughData.into());
            }
        }
        if data[offset] == (7 << 5) | 23 {
            Ok((None, offset + 1, tags))
        } else {
            let (v, len, _) = parse_detail::<T>(&data[offset + 1..])?;
            Ok((Some(v), len + offset + 1, tags))
        }
    }
}
