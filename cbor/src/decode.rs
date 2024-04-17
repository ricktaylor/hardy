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

    #[error("Invalid simple type {0}")]
    InvalidSimpleType(u8),

    #[error("Map has key but no value")]
    PartialMap,
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
    Map(Map<'a>),
    False,
    True,
    Null,
    Undefined,
    Simple(u8),
    Float(f64),
}

pub struct Sequence<'a, const D: usize> {
    data: &'a [u8],
    count: Option<usize>,
    offset: &'a mut usize,
    idx: usize,
}

pub type Array<'a> = Sequence<'a, 1>;
pub type Map<'a> = Sequence<'a, 2>;

impl<'a, const D: usize> Sequence<'a, D> {
    fn new(data: &'a [u8], count: Option<usize>, offset: &'a mut usize) -> Self {
        Self {
            data,
            count: count.map(|c| c * D),
            offset,
            idx: 0,
        }
    }

    pub fn count(&self) -> Option<usize> {
        self.count.map(|c| c / D)
    }

    fn check_for_end(&mut self) -> Result<Option<(usize, usize)>, anyhow::Error> {
        if let Some(count) = self.count {
            match self.idx.cmp(&count) {
                std::cmp::Ordering::Greater => return Err(Error::NotEnoughData.into()),
                std::cmp::Ordering::Equal => {
                    self.idx += 1;
                    return Ok(Some((*self.offset, 0)));
                }
                _ => {}
            }
        } else if *self.offset >= self.data.len() {
            return Err(Error::NotEnoughData.into());
        } else if self.data[*self.offset] == 0xFF {
            if self.idx % D == 1 {
                return Err(Error::PartialMap.into());
            }
            self.count = Some(self.idx);
            self.idx += 1;
            *self.offset += 1;
            return Ok(Some((*self.offset - 1, 1)));
        }

        Ok(None)
    }

    pub fn end_or_else<F>(&mut self, f: F) -> Result<usize, anyhow::Error>
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

    pub fn try_parse_array<T, F>(&mut self, f: F) -> Result<Option<(T, usize)>, anyhow::Error>
    where
        F: FnOnce(Array, usize, &[u64]) -> Result<T, anyhow::Error>,
    {
        self.try_parse_value(|value, start, tags| match value {
            Value::Array(a) => f(a, start, tags),
            _ => Err(Error::IncorrectType.into()),
        })
    }

    pub fn parse_array<T, F>(&mut self, f: F) -> Result<(T, usize), anyhow::Error>
    where
        F: FnOnce(Array, usize, &[u64]) -> Result<T, anyhow::Error>,
    {
        self.try_parse_array(f)?.ok_or(Error::NotEnoughData.into())
    }
}

fn parse_tags(data: &[u8]) -> Result<(Vec<u64>, usize), Error> {
    let mut tags = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        match (data[offset] >> 5, data[offset] & 0x1F) {
            (6, minor) => {
                let (tag, o) = parse_uint_minor(minor, &data[offset + 1..])?;
                tags.push(tag);
                offset += o + 1;
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
    if (len as u64) + data_len > data.len() as u64 {
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
            break Err(Error::NotEnoughData);
        }

        let v = data[offset];
        offset += 1;

        if v == 0xFF {
            break Ok((chunks, offset));
        }

        if v >> 5 != major {
            break Err(Error::InvalidChunk);
        }

        let (chunk, chunk_len) = parse_data_minor(v & 0x1F, &data[offset..])?;
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
            let (c, len) = parse_data_chunked(2, &data[offset + 1..])?;
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
        (7, minor @ 0..=19) => {
            /* Unassigned simple type */
            offset += 1;
            f(Value::Simple(minor), &tags)
        }
        (7, 24) => {
            /* Unassigned simple type */
            if data.len() <= offset + 1 {
                return Err(Error::NotEnoughData.into());
            }
            let v = data[offset + 1];
            if v < 32 {
                return Err(Error::InvalidSimpleType(v).into());
            }
            offset += 2;
            f(Value::Simple(v), &tags)
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
        (7, minor) => {
            return Err(Error::InvalidSimpleType(minor).into());
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

pub fn parse_array<T, F>(data: &[u8], f: F) -> Result<(T, usize), anyhow::Error>
where
    F: FnOnce(Array, &[u64]) -> Result<T, anyhow::Error>,
{
    parse_value(data, |value, tags| match value {
        Value::Array(a) => f(a, tags),
        _ => Err(Error::IncorrectType.into()),
    })
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

impl FromCbor for Vec<u8> {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        parse_value(data, |value, tags| match (value, tags) {
            (Value::Bytes(v, _), tags) => Ok((v.to_vec(), tags.to_vec())),
            _ => Err(Error::IncorrectType.into()),
        })
        .map(|((val, tags), len)| (val, len, tags))
    }
}

impl FromCbor for String {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        parse_value(data, |value, tags| match (value, tags) {
            (Value::Text(v, _), tags) => Ok((v.to_string(), tags.to_vec())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use hex_literal::hex;

    #[test]
    fn rfc_tests() {
        // RFC 8949, Appendix A:
        // https://www.rfc-editor.org/rfc/rfc8949.html#section-appendix.a

        assert_eq!(0, parse(&hex!("00")).unwrap());
        assert_eq!(1, parse(&hex!("01")).unwrap());
        assert_eq!(10, parse(&hex!("0a")).unwrap());
        assert_eq!(23, parse(&hex!("17")).unwrap());
        assert_eq!(24, parse(&hex!("1818")).unwrap());
        assert_eq!(25, parse(&hex!("1819")).unwrap());
        assert_eq!(100, parse(&hex!("1864")).unwrap());
        assert_eq!(1000, parse(&hex!("1903e8")).unwrap());
        assert_eq!(1000000, parse(&hex!("1a000f4240")).unwrap());
        assert_eq!(
            1000000000000u64,
            parse(&hex!("1b000000e8d4a51000")).unwrap()
        );
        assert_eq!(
            18446744073709551615u64,
            parse(&hex!("1bffffffffffffffff")).unwrap()
        );
        assert!(parse::<u64>(&hex!("c249010000000000000000")).is_err());
        /*assert_eq!(
            18446744073709551616,
            parse(&hex!("c249 010000000000000000")).unwrap()
        );*/
        assert!(parse::<i64>(&hex!("3bffffffffffffffff")).is_err());
        /*assert_eq!(
            -18446744073709551616i128,
            parse(&hex!("3bffffffffffffffff")).unwrap()
        );*/
        assert!(parse::<i64>(&hex!("c349 010000000000000000")).is_err());
        /*assert_eq!(
            -18446744073709551617,
            parse(&hex!("c349 010000000000000000")).unwrap()
        );*/
        assert_eq!(-1, parse(&hex!("20")).unwrap());
        assert_eq!(-10, parse(&hex!("29")).unwrap());
        assert_eq!(-100, parse(&hex!("3863")).unwrap());
        assert_eq!(-1000, parse(&hex!("3903e7")).unwrap());
        assert_eq!(0.0, parse(&hex!("f90000")).unwrap());
        assert_eq!(-0.0, parse(&hex!("f98000")).unwrap());
        assert_eq!(1.0, parse(&hex!("f93c00")).unwrap());
        assert_eq!(1.1, parse(&hex!("fb3ff199999999999a")).unwrap());
        assert_eq!(1.5, parse(&hex!("f93e00")).unwrap());
        assert_eq!(65504.0, parse(&hex!("f97bff")).unwrap());
        assert_eq!(100000.0, parse(&hex!("fa47c35000")).unwrap());
        assert_eq!(3.4028234663852886e+38, parse(&hex!("fa7f7fffff")).unwrap());
        assert_eq!(1.0e+300, parse(&hex!("fb7e37e43c8800759c")).unwrap());
        assert_eq!(5.960464477539063e-8, parse(&hex!("f90001")).unwrap());
        assert_eq!(0.00006103515625, parse(&hex!("f90400")).unwrap());
        assert_eq!(-4.0, parse(&hex!("f9c400")).unwrap());
        assert_eq!(-4.1, parse(&hex!("fbc010666666666666")).unwrap());
        assert_eq!(f32::INFINITY, parse(&hex!("f97c00")).unwrap());
        assert!(parse::<f32>(&hex!("f97e00")).unwrap().is_nan());
        assert_eq!(f32::NEG_INFINITY, parse(&hex!("f9fc00")).unwrap());
        assert_eq!(f64::INFINITY, parse(&hex!("fa7f800000")).unwrap());
        assert!(parse::<f32>(&hex!("fa7fc00000")).unwrap().is_nan());
        assert_eq!(f64::NEG_INFINITY, parse(&hex!("faff800000")).unwrap());
        assert_eq!(f64::INFINITY, parse(&hex!("fb7ff0000000000000")).unwrap());
        assert!(parse::<f64>(&hex!("fb7ff8000000000000")).unwrap().is_nan());
        assert_eq!(
            f64::NEG_INFINITY,
            parse(&hex!("fbfff0000000000000")).unwrap()
        );
        assert_eq!(false, parse(&hex!("f4")).unwrap());
        assert_eq!(true, parse(&hex!("f5")).unwrap());
        assert!(
            parse_value(&hex!("f6"), |value, _| match value {
                Value::Null => Ok(true),
                _ => Ok(false),
            })
            .unwrap()
            .0
        );
        assert!(
            parse_value(&hex!("f7"), |value, _| match value {
                Value::Undefined => Ok(true),
                _ => Ok(false),
            })
            .unwrap()
            .0
        );
        assert!(
            parse_value(&hex!("f0"), |value, _| match value {
                Value::Simple(16) => Ok(true),
                _ => Ok(false),
            })
            .unwrap()
            .0
        );
        assert_eq!(
            (true, 2),
            parse_value(&hex!("f8ff"), |value, _| match value {
                Value::Simple(255) => Ok(true),
                _ => Ok(false),
            })
            .unwrap()
        );
        assert_eq!(
            (true, 22),
            parse_value(
                &hex!("c074323031332d30332d32315432303a30343a30305a"),
                |value, tags| match value {
                    Value::Text("2013-03-21T20:04:00Z", false) if tags == vec![0] => Ok(true),
                    _ => Ok(false),
                }
            )
            .unwrap()
        );
        assert_eq!(
            (1363896240, 6, vec![1]),
            parse_detail(&hex!("c11a514b67b0")).unwrap()
        );
        assert_eq!(
            (1363896240.5, 10, vec![1]),
            parse_detail(&hex!("c1fb41d452d9ec200000")).unwrap()
        );
        assert_eq!(
            (true, 6),
            parse_value(&hex!("d74401020304"), |value, tags| match value {
                Value::Bytes(v, false) if v == hex!("01020304") && tags == vec![23] => Ok(true),
                _ => Ok(false),
            })
            .unwrap()
        );
        assert_eq!(
            (true, 8),
            parse_value(&hex!("d818456449455446"), |value, tags| match value {
                Value::Bytes(v, false) if v == hex!("6449455446") && tags == vec![24] => Ok(true),
                _ => Ok(false),
            })
            .unwrap()
        );
        assert_eq!(
            (true, 25),
            parse_value(
                &hex!("d82076687474703a2f2f7777772e6578616d706c652e636f6d"),
                |value, tags| match value {
                    Value::Text(v, false) if v == "http://www.example.com" && tags == vec![32] =>
                        Ok(true),
                    _ => Ok(false),
                }
            )
            .unwrap()
        );
        assert!(parse::<Vec<u8>>(&hex!("40")).unwrap().is_empty());
        assert_eq!(
            hex!("01020304").to_vec(),
            parse::<Vec<u8>>(&hex!("4401020304")).unwrap()
        );
        assert!(parse::<String>(&hex!("60")).unwrap().is_empty());
        assert_eq!("a", &parse::<String>(&hex!("6161")).unwrap());
        assert_eq!("IETF", &parse::<String>(&hex!("6449455446")).unwrap());
        assert_eq!("\"\\", &parse::<String>(&hex!("62225c")).unwrap());
        assert_eq!("\u{00fc}", &parse::<String>(&hex!("62c3bc")).unwrap());
        assert_eq!("\u{6c34}", &parse::<String>(&hex!("63e6b0b4")).unwrap());
        assert_eq!(
            "\u{10151}", /* surrogate pair: \u{d800}\u{dd51} */
            &parse::<String>(&hex!("64f0908591")).unwrap()
        );
        assert_eq!(
            (0, 1),
            parse_array(&hex!("80"), |mut a, _| {
                a.end_or_else(|| Error::NotEnoughData.into())?;
                Ok(a.count().unwrap())
            })
            .unwrap()
        );
        assert_eq!(
            (vec![1, 2, 3], 4),
            parse_array(&hex!("83010203"), |mut a, _| {
                let v = vec![a.parse()?, a.parse()?, a.parse()?];
                a.end_or_else(|| Error::NotEnoughData.into())?;
                Ok(v)
            })
            .unwrap()
        );

        /*
        [1, 2, 3]	0x83010203
        [1, [2, 3], [4, 5]]	0x8301820203820405
         */

        assert_eq!(
            (
                vec![
                    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
                    23, 24, 25
                ],
                29
            ),
            parse_array(
                &hex!("98190102030405060708090a0b0c0d0e0f101112131415161718181819"),
                |mut a, _| {
                    let mut v = Vec::new();
                    for _ in 1..=25 {
                        v.push(a.parse()?);
                    }
                    a.end_or_else(|| Error::NotEnoughData.into())?;
                    Ok(v)
                }
            )
            .unwrap()
        );
        assert_eq!(
            (0, 1),
            parse_value(&hex!("a0"), |value, _| match value {
                Value::Map(mut m) => {
                    m.end_or_else(|| Error::NotEnoughData.into())?;
                    Ok(m.count().unwrap())
                }
                _ => Err(Error::IncorrectType.into()),
            })
            .unwrap()
        );
        assert_eq!(
            (vec![1, 2, 3, 4], 5),
            parse_value(&hex!("a201020304"), |value, _| match value {
                Value::Map(mut m) => {
                    let v = vec![m.parse()?, m.parse()?, m.parse()?, m.parse()?];
                    m.end_or_else(|| Error::NotEnoughData.into())?;
                    Ok(v)
                }
                _ => Err(Error::IncorrectType.into()),
            })
            .unwrap()
        );

        /*
        {"a": 1, "b": [2, 3]}	                            0xa26161016162820203
        ["a", {"b": "c"}]	                                0x826161a161626163
         */

        assert_eq!(
            (
                vec![
                    "a".to_string(),
                    "A".to_string(),
                    "b".to_string(),
                    "B".to_string(),
                    "c".to_string(),
                    "C".to_string(),
                    "d".to_string(),
                    "D".to_string(),
                    "e".to_string(),
                    "E".to_string()
                ],
                21
            ),
            parse_value(
                &hex!("a56161614161626142616361436164614461656145"),
                |value, _| match value {
                    Value::Map(mut m) => {
                        let mut v = Vec::new();
                        for _ in 1..=5 {
                            v.push(m.parse()?);
                            v.push(m.parse()?);
                        }
                        m.end_or_else(|| Error::NotEnoughData.into())?;
                        Ok(v)
                    }
                    _ => Err(Error::IncorrectType.into()),
                }
            )
            .unwrap()
        );
        assert_eq!(
            (true, 9),
            parse_value(&hex!("5f42010243030405ff"), |value, _| match value {
                Value::Bytes(v, true) if v == hex!("0102030405") => Ok(true),
                _ => Ok(false),
            })
            .unwrap()
        );
        assert_eq!(
            (true, 13),
            parse_value(
                &hex!("7f657374726561646d696e67ff"),
                |value, _| match value {
                    Value::Text(v, true) if v == "streaming" => Ok(true),
                    _ => Ok(false),
                }
            )
            .unwrap()
        );
        assert_eq!(
            (0, 2),
            parse_array(&hex!("9fff"), |mut a, _| {
                if a.count().is_some() {
                    return Err(anyhow::anyhow!("Expected indefinite length!"));
                }
                a.end_or_else(|| Error::NotEnoughData.into())?;
                Ok(a.count().unwrap())
            })
            .unwrap()
        );

        /*
        [_ 1, [2, 3], [_ 4, 5]]	0x9f018202039f0405ffff
        [_ 1, [2, 3], [4, 5]]	0x9f01820203820405ff
        [1, [2, 3], [_ 4, 5]]	0x83018202039f0405ff
        [1, [_ 2, 3], [4, 5]]	0x83019f0203ff820405
        */
        assert_eq!(
            (
                vec![
                    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
                    23, 24, 25
                ],
                29
            ),
            parse_array(
                &hex!("9f0102030405060708090a0b0c0d0e0f101112131415161718181819ff"),
                |mut a, _| {
                    if a.count().is_some() {
                        return Err(anyhow::anyhow!("Expected indefinite length!"));
                    }
                    let mut v = Vec::new();
                    for _ in 1..=25 {
                        v.push(a.parse()?);
                    }
                    a.end_or_else(|| Error::NotEnoughData.into())?;
                    Ok(v)
                }
            )
            .unwrap()
        );
        /* {_ "a": 1, "b": [_ 2, 3]}	0xbf61610161629f0203ffff
        ["a", {_ "b": "c"}]	0x826161bf61626163ff
        {_ "Fun": true, "Amt": -2}	0xbf6346756ef563416d7421ff
         */
    }
}
