use super::*;
use core::{ops::Range, str::Utf8Error};
use num_traits::{FromPrimitive, ToPrimitive};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("An encoded item requires more memory than available")]
    TooBig,

    #[error("Need at least {0} more bytes to decode value")]
    NeedMoreData(usize),

    #[error("Additional unread items in sequence")]
    AdditionalItems,

    #[error("No more items in sequence")]
    NoMoreItems,

    #[error("Invalid minor-type value {0}")]
    InvalidMinorValue(u8),

    #[error("Tags with no following value")]
    JustTags,

    #[error("Incorrect type, expecting {0}, found {1}")]
    IncorrectType(String, String),

    #[error("Chunked string contains an invalid chunk")]
    InvalidChunk,

    #[error("Invalid simple type {0}")]
    InvalidSimpleType(u8),

    #[error("Map has key but no value")]
    PartialMap,

    #[error("Maximum recursion depth reached")]
    MaxRecursion,

    #[error(transparent)]
    InvalidUtf8(#[from] Utf8Error),

    #[error(transparent)]
    TryFromIntError(#[from] core::num::TryFromIntError),

    #[error("Loss of floating-point precision")]
    PrecisionLoss,
}

pub trait FromCbor: Sized {
    type Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error>;
}

pub type Sequence<'a> = super::decode_seq::Series<'a, 0>;
pub type Array<'a> = super::decode_seq::Series<'a, 1>;
pub type Map<'a> = super::decode_seq::Series<'a, 2>;
pub use super::decode_seq::Series;

pub enum Value<'a, 'b: 'a> {
    UnsignedInteger(u64),
    NegativeInteger(u64),
    Bytes(Range<usize>),
    ByteStream(Vec<Range<usize>>),
    Text(&'b str),
    TextStream(&'a [&'b str]),
    Array(&'a mut Array<'b>),
    Map(&'a mut Map<'b>),
    False,
    True,
    Null,
    Undefined,
    Simple(u8),
    Float(f64),
}

impl<'a, 'b: 'a> Value<'a, 'b> {
    pub fn type_name(&self, tagged: bool) -> String {
        let prefix = if tagged { "Tagged " } else { "Untagged " }.to_string();
        match self {
            Value::UnsignedInteger(_) => prefix + "Unsigned Integer",
            Value::NegativeInteger(_) => prefix + "Negative Integer",
            Value::Bytes(_) => prefix + "Definite-length Byte String",
            Value::ByteStream(_) => prefix + "Indefinite-length Byte String",
            Value::Text(_) => prefix + "Definite-length Text String",
            Value::TextStream(_) => prefix + "Indefinite-length Text String",
            Value::Array(a) if a.is_definite() => prefix + "Definite-length Array",
            Value::Array(_) => prefix + "Indefinite-length Array",
            Value::Map(m) if m.is_definite() => prefix + "Definite-length Map",
            Value::Map(_) => prefix + "Indefinite-length Map",
            Value::False => prefix + "False",
            Value::True => prefix + "True",
            Value::Null => prefix + "Null",
            Value::Undefined => prefix + "Undefined",
            Value::Simple(v) => format!("{prefix}Simple Value {v}"),
            Value::Float(_) => prefix + "Float",
        }
    }

    pub fn skip(&mut self, mut max_recursion: usize) -> Result<bool, Error> {
        match self {
            Value::Array(a) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                a.skip_to_end(max_recursion).map(|s| s && a.is_definite())
            }
            Value::Map(m) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                m.skip_to_end(max_recursion).map(|s| s && m.is_definite())
            }
            _ => Ok(true),
        }
    }
}

impl<'a, 'b: 'a> core::fmt::Debug for Value<'a, 'b> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::UnsignedInteger(n) => write!(f, "{n:?}"),
            Value::NegativeInteger(n) => write!(f, "-{n:?}"),
            Value::Bytes(b) => write!(f, "{b:?}"),
            Value::ByteStream(b) => write!(f, "{b:?}"),
            Value::Text(s) => write!(f, "{s:?}"),
            Value::TextStream(s) => write!(f, "{s:?}"),
            Value::Array(a) => write!(f, "{a:?}"),
            Value::Map(m) => write!(f, "{m:?}"),
            Value::False => write!(f, "{:?}", false),
            Value::True => write!(f, "{:?}", true),
            Value::Null => f.write_str("null"),
            Value::Undefined => f.write_str("undefined"),
            Value::Simple(v) => write!(f, "simple value {v}"),
            Value::Float(v) => write!(f, "{v:?}"),
        }
    }
}

fn parse_tags(data: &[u8]) -> Result<(Vec<u64>, bool, usize), Error> {
    let mut tags = Vec::new();
    let mut offset = 0;
    let mut shortest = true;
    while offset < data.len() {
        match (data[offset] >> 5, data[offset] & 0x1F) {
            (6, minor) => {
                let (tag, s, o) = parse_uint_minor(minor, &data[offset + 1..])?;
                tags.push(tag);
                shortest = shortest && s;
                offset += o + 1;
            }
            _ => break,
        }
    }
    Ok((tags, shortest, offset))
}

fn to_array<const N: usize>(data: &[u8]) -> Result<[u8; N], Error> {
    match data.len().cmp(&N) {
        core::cmp::Ordering::Less => Err(Error::NeedMoreData(N - data.len())),
        core::cmp::Ordering::Equal => Ok(data.try_into().unwrap()),
        core::cmp::Ordering::Greater => Ok(data[0..N].try_into().unwrap()),
    }
}

fn parse_uint_minor(minor: u8, data: &[u8]) -> Result<(u64, bool, usize), Error> {
    match minor {
        24 => {
            if data.is_empty() {
                Err(Error::NeedMoreData(1))
            } else {
                Ok((data[0] as u64, data[0] > 23, 1))
            }
        }
        25 => {
            let v = u16::from_be_bytes(to_array(data)?);
            Ok((v as u64, v > u8::MAX as u16, 2))
        }
        26 => {
            let v = u32::from_be_bytes(to_array(data)?);
            Ok((v as u64, v > u16::MAX as u32, 4))
        }
        27 => {
            let v = u64::from_be_bytes(to_array(data)?);
            Ok((v, v > u32::MAX as u64, 8))
        }
        val if val < 24 => Ok((val as u64, true, 0)),
        _ => Err(Error::InvalidMinorValue(minor)),
    }
}

fn parse_data_minor(minor: u8, data: &[u8]) -> Result<(Range<usize>, bool, usize), Error> {
    let (data_len, shortest, len) = parse_uint_minor(minor, data)?;
    if (len as u64).checked_add(data_len).ok_or(Error::TooBig)? > data.len() as u64 {
        Err(Error::NeedMoreData(
            ((len as u64) + data_len - (data.len() as u64)) as usize,
        ))
    } else {
        let end = ((len as u64) + data_len) as usize;
        Ok((len..end, shortest, end))
    }
}

fn parse_data_chunked(major: u8, data: &[u8]) -> Result<(Vec<Range<usize>>, bool, usize), Error> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut shortest = true;
    loop {
        if offset >= data.len() {
            break Err(Error::NeedMoreData(offset + 1 - data.len()));
        }

        let v = data[offset];
        offset += 1;

        if v == 0xFF {
            break Ok((chunks, shortest, offset));
        }

        if v >> 5 != major {
            break Err(Error::InvalidChunk);
        }

        let (chunk, s, chunk_len) = parse_data_minor(v & 0x1F, &data[offset..])?;
        chunks.push(chunk.start + offset..chunk.end + offset);
        shortest = shortest && s;
        offset += chunk_len;
    }
}

pub fn try_parse_value<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(Value, bool, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    let (tags, mut shortest, mut offset) = parse_tags(data)?;
    if offset >= data.len() {
        if !tags.is_empty() {
            return Err(Error::JustTags.into());
        } else {
            return Ok(None);
        }
    }

    match (data[offset] >> 5, data[offset] & 0x1F) {
        (0, minor) => {
            let (v, s, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::UnsignedInteger(v), shortest && s, tags)
        }
        (1, minor) => {
            let (v, s, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::NegativeInteger(v), shortest && s, tags)
        }
        (2, 31) => {
            /* Indefinite length byte string */
            let (mut v, s, len) = parse_data_chunked(2, &data[offset + 1..])?;
            for t in v.iter_mut() {
                t.start += offset + 1;
                t.end += offset + 1;
            }
            offset += len + 1;
            f(Value::ByteStream(v), shortest && s, tags)
        }
        (2, minor) => {
            /* Known length byte string */
            let (t, s, len) = parse_data_minor(minor, &data[offset + 1..])?;
            let t = t.start + offset + 1..t.end + offset + 1;
            offset += len + 1;
            f(Value::Bytes(t), shortest && s, tags)
        }
        (3, 31) => {
            /* Indefinite length text string */
            let data = &data[offset + 1..];
            let (v, s, len) = parse_data_chunked(3, data)?;
            offset += len + 1;
            let mut t = Vec::new();
            for b in v {
                t.push(core::str::from_utf8(&data[b]).map_err(Into::into)?);
            }
            f(Value::TextStream(&t), shortest && s, tags)
        }
        (3, minor) => {
            /* Known length text string */
            let data = &data[offset + 1..];
            let (t, s, len) = parse_data_minor(minor, data)?;
            offset += len + 1;
            f(
                Value::Text(core::str::from_utf8(&data[t]).map_err(Into::into)?),
                shortest && s,
                tags,
            )
        }
        (4, 31) => {
            /* Indefinite length array */
            offset += 1;
            let mut a = Array::new(data, None, &mut offset);
            let r = f(Value::Array(&mut a), shortest, tags)?;
            a.complete().map(|_| r).map_err(Into::into)
        }
        (4, minor) => {
            /* Known length array */
            let (count, s, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            if count > usize::MAX as u64 {
                return Err(Error::TooBig.into());
            }
            let mut a = Array::new(data, Some(count as usize), &mut offset);
            let r = f(Value::Array(&mut a), shortest && s, tags)?;
            a.complete().map(|_| r).map_err(Into::into)
        }
        (5, 31) => {
            /* Indefinite length map */
            offset += 1;
            let mut m = Map::new(data, None, &mut offset);
            let r = f(Value::Map(&mut m), true, tags)?;
            m.complete().map(|_| r).map_err(Into::into)
        }
        (5, minor) => {
            /* Known length array */
            let (count, s, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            if count > (usize::MAX as u64) / 2 {
                return Err(Error::TooBig.into());
            }
            let mut m = Map::new(data, Some((count * 2) as usize), &mut offset);
            let r = f(Value::Map(&mut m), shortest && s, tags)?;
            m.complete().map(|_| r).map_err(Into::into)
        }
        (6, _) => unreachable!(),
        (7, 20) => {
            /* False */
            offset += 1;
            f(Value::False, shortest, tags)
        }
        (7, 21) => {
            /* True */
            offset += 1;
            f(Value::True, shortest, tags)
        }
        (7, 22) => {
            /* Null */
            offset += 1;
            f(Value::Null, shortest, tags)
        }
        (7, 23) => {
            /* Undefined */
            offset += 1;
            f(Value::Undefined, shortest, tags)
        }
        (7, minor @ 0..=19) => {
            /* Unassigned simple type */
            offset += 1;
            f(Value::Simple(minor), shortest, tags)
        }
        (7, 24) => {
            /* Unassigned simple type */
            if offset + 1 >= data.len() {
                return Err(Error::NeedMoreData(offset + 2 - data.len()).into());
            }
            let v = data[offset + 1];
            if v < 32 {
                return Err(Error::InvalidSimpleType(v).into());
            }
            offset += 2;
            f(Value::Simple(v), shortest, tags)
        }
        (7, 25) => {
            /* FP16 */
            let v = half::f16::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 3;
            f(Value::Float(v.into()), shortest, tags)
        }
        (7, 26) => {
            /* FP32 */
            let v = f32::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 5;
            if shortest {
                match v.classify() {
                    core::num::FpCategory::Nan
                    | core::num::FpCategory::Infinite
                    | core::num::FpCategory::Zero => {
                        // There is an FP16 representation that is shorter
                        shortest = false;
                    }
                    core::num::FpCategory::Subnormal | core::num::FpCategory::Normal => {
                        if let Some(v16) = <half::f16 as num_traits::FromPrimitive>::from_f32(v) {
                            if <half::f16 as num_traits::ToPrimitive>::to_f32(&v16) == Some(v) {
                                shortest = false;
                            }
                        }
                    }
                }
            }
            f(Value::Float(v.into()), shortest, tags)
        }
        (7, 27) => {
            /* FP64 */
            let v = f64::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 9;
            if shortest {
                match v.classify() {
                    core::num::FpCategory::Nan
                    | core::num::FpCategory::Infinite
                    | core::num::FpCategory::Zero => {
                        // There is an FP16 representation that is shorter
                        shortest = false;
                    }
                    core::num::FpCategory::Subnormal | core::num::FpCategory::Normal => {
                        if let Some(v32) = f32::from_f64(v) {
                            if v32.to_f64() == Some(v) {
                                shortest = false;
                            }
                        } else if let Some(v16) =
                            <half::f16 as num_traits::FromPrimitive>::from_f64(v)
                        {
                            if <half::f16 as num_traits::ToPrimitive>::to_f64(&v16) == Some(v) {
                                shortest = false;
                            }
                        }
                    }
                }
            }
            f(Value::Float(v), shortest, tags)
        }
        (7, minor) => {
            return Err(Error::InvalidSimpleType(minor).into());
        }
        _ => unreachable!(),
    }
    .map(|r| Some((r, offset)))
}

#[inline]
pub fn parse_value<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(Value, bool, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value(data, f)?.ok_or(Error::NeedMoreData(1).into())
}

pub fn try_parse_sequence<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(&mut Sequence) -> Result<T, E>,
    E: From<Error>,
{
    if data.is_empty() {
        return Ok(None);
    }

    let mut offset = 0;
    let mut s = Sequence::new(data, None, &mut offset);
    let r = f(&mut s)?;
    s.complete().map(|_| Some((r, offset))).map_err(Into::into)
}

#[inline]
pub fn parse_sequence<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Sequence) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_sequence(data, f)?.ok_or(Error::NeedMoreData(1).into())
}

pub fn try_parse_array<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(&mut Array, bool, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value(data, |value, shortest, tags| match value {
        Value::Array(a) => f(a, shortest, tags),
        _ => {
            Err(Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into())
        }
    })
}

pub fn parse_array<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Array, bool, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_array(data, f)?.ok_or(Error::NeedMoreData(1).into())
}

pub fn try_parse_map<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(&mut Map, bool, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value(data, |value, shortest, tags| match value {
        Value::Map(m) => f(m, shortest, tags),
        _ => Err(Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into()),
    })
}

pub fn parse_map<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Map, bool, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_map(data, f)?.ok_or(Error::NeedMoreData(1).into())
}

pub fn try_parse<T>(data: &[u8]) -> Result<Option<T>, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    T::try_from_cbor(data).map(|o| o.map(|v| v.0))
}

pub fn parse<T>(data: &[u8]) -> Result<T, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    try_parse::<T>(data)?.ok_or(Error::NeedMoreData(1).into())
}

impl FromCbor for u8 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u16 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u32 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for usize {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u64 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        try_parse_value(data, |value, shortest, tags| match value {
            Value::UnsignedInteger(n) => Ok((n, shortest && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Unsigned Integer".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

impl FromCbor for i8 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i16 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i32 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for isize {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i64 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        try_parse_value(data, |value, shortest, tags| match value {
            Value::UnsignedInteger(n) => Ok((i64::try_from(n)?, shortest && tags.is_empty())),
            Value::NegativeInteger(n) => {
                Ok((-1i64 - i64::try_from(n)?, shortest && tags.is_empty()))
            }
            value => Err(Error::IncorrectType(
                "Untagged Integer".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

impl FromCbor for half::f16 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = f64::try_from_cbor(data)? {
            Ok(Some((
                <half::f16 as num_traits::FromPrimitive>::from_f64(v)
                    .ok_or(Error::PrecisionLoss)?,
                shortest,
                len,
            )))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for f32 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = f64::try_from_cbor(data)? {
            Ok(Some((
                f32::from_f64(v).ok_or(Error::PrecisionLoss)?,
                shortest,
                len,
            )))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for f64 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        try_parse_value(data, |value, shortest, tags| match value {
            Value::Float(f) => Ok((f, shortest && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Float".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

impl FromCbor for bool {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        try_parse_value(data, |value, shortest, tags| match value {
            Value::False => Ok((false, shortest && tags.is_empty())),
            Value::True => Ok((true, shortest && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Boolean".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        match try_parse_value(data, |value, shortest, tags| match value {
            Value::Undefined => Ok(Some(shortest && tags.is_empty())),
            _ => Ok(None),
        })? {
            Some((Some(shortest), len)) => Ok(Some((None, shortest, len))),
            Some((None, _)) => {
                T::try_from_cbor(data).map(|o| o.map(|(v, shortest, len)| (Some(v), shortest, len)))
            }
            None => Ok(None),
        }
    }
}

impl<T> FromCbor for (T, bool, usize)
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        T::try_from_cbor(data).map(|o| {
            o.map(|(value, shortest, length)| ((value, shortest, length), shortest, length))
        })
    }
}

impl<T> FromCbor for (T, bool)
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        T::try_from_cbor(data)
            .map(|o| o.map(|(value, shortest, length)| ((value, shortest), shortest, length)))
    }
}

impl<T> FromCbor for (T, usize)
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        T::try_from_cbor(data)
            .map(|o| o.map(|(value, shortest, length)| ((value, length), shortest, length)))
    }
}
