use num_traits::FromPrimitive;
use std::str::Utf8Error;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Not enough data for encoded value")]
    NotEnoughData,

    #[error("More items to be read")]
    MoreItems,

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
    TryFromIntError(#[from] std::num::TryFromIntError),

    #[error("Loss of floating-point precision")]
    PrecisionLoss,
}

pub trait FromCbor: Sized {
    type Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error>;
}

pub enum Value<'a, 'b: 'a> {
    UnsignedInteger(u64),
    NegativeInteger(u64),
    Bytes(&'b [u8], bool),
    Text(&'b str, bool),
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
            Value::Bytes(_, true) => prefix + "Definite-length Byte String",
            Value::Bytes(_, false) => prefix + "Indefinite-length Byte String",
            Value::Text(_, true) => prefix + "Definite-length Text String",
            Value::Text(_, false) => prefix + "Indefinite-length Text String",
            Value::Array(_) => prefix + "Array",
            Value::Map(_) => prefix + "Map",
            Value::False => prefix + "False",
            Value::True => prefix + "True",
            Value::Null => prefix + "Null",
            Value::Undefined => prefix + "Undefined",
            Value::Simple(v) => format!("{prefix}Simple Value {v}"),
            Value::Float(_) => prefix + "Float",
        }
    }

    pub fn skip(&mut self, mut max_recursion: usize) -> Result<(), Error> {
        match self {
            Value::Array(a) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                a.skip_to_end(max_recursion)
            }
            Value::Map(m) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                m.skip_to_end(max_recursion)
            }
            _ => Ok(()),
        }
    }
}

impl<'a, 'b: 'a> std::fmt::Debug for Value<'a, 'b> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::UnsignedInteger(n) => write!(f, "{n:?}"),
            Value::NegativeInteger(n) => write!(f, "-{n:?}"),
            Value::Bytes(b, true) => write!(f, "{b:?}"),
            Value::Bytes(b, false) => write!(f, "{b:?} (chunked)"),
            Value::Text(s, true) => write!(f, "{s:?}"),
            Value::Text(s, false) => write!(f, "{s:?} (chunked)"),
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
            count,
            offset,
            idx: 0,
        }
    }

    pub fn count(&self) -> Option<usize> {
        self.count.map(|c| c / D)
    }

    pub fn is_definite(&self) -> bool {
        self.count.is_some()
    }

    fn check_for_end(&mut self) -> Result<bool, Error> {
        if let Some(count) = self.count {
            match self.idx.cmp(&count) {
                std::cmp::Ordering::Greater => Ok(true),
                std::cmp::Ordering::Equal => {
                    self.idx += 1;
                    Ok(true)
                }
                _ => Ok(false),
            }
        } else if *self.offset >= self.data.len() {
            Err(Error::NotEnoughData)
        } else if self.data[*self.offset] == 0xFF {
            if self.idx % D == 1 {
                Err(Error::PartialMap)
            } else {
                self.count = Some(self.idx);
                self.idx += 1;
                *self.offset += 1;
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }

    pub fn end(&mut self) -> Result<Option<usize>, Error> {
        if self.check_for_end()? {
            Ok(Some(*self.offset))
        } else {
            Ok(None)
        }
    }

    fn complete(mut self) -> Result<(), Error> {
        if !self.check_for_end()? {
            return Err(Error::MoreItems);
        }
        Ok(())
    }

    pub fn skip_value(&mut self, max_recursion: usize) -> Result<Option<(usize, usize)>, Error> {
        self.try_parse_value(|mut value, start, _| {
            value.skip(max_recursion)?;
            Ok(start)
        })
    }

    pub fn skip_to_end(&mut self, max_recursion: usize) -> Result<(), Error> {
        while self
            .try_parse_value(|mut value, _, _| value.skip(max_recursion))?
            .is_some()
        {
            if D == 2 {
                self.parse_value(|mut value, _, _| value.skip(max_recursion))?;
            }
        }
        Ok(())
    }

    pub fn try_parse_value<T, F, E>(&mut self, f: F) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(Value, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        // Check for end of array
        if self.check_for_end()? {
            Ok(None)
        } else {
            // Parse sub-item
            let item_start = *self.offset;
            let r = try_parse_value(&self.data[item_start..], |value, tags| {
                f(value, item_start, tags)
            });
            if let Ok(Some((_, len))) = r {
                self.idx += 1;
                *self.offset += len;
            }
            r
        }
    }

    #[inline]
    pub fn parse_value<T, F, E>(&mut self, f: F) -> Result<(T, usize), E>
    where
        F: FnOnce(Value, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value(f)?.ok_or(Error::NotEnoughData.into())
    }

    pub fn try_parse<T>(&mut self) -> Result<Option<T>, T::Error>
    where
        T: FromCbor,
        T::Error: From<self::Error>,
    {
        // Check for end of array
        if self.check_for_end()? {
            Ok(None)
        } else {
            // Parse sub-item
            match T::try_from_cbor(&self.data[*self.offset..])? {
                Some((value, len)) => {
                    self.idx += 1;
                    *self.offset += len;
                    Ok(Some(value))
                }
                None => Ok(None),
            }
        }
    }

    pub fn parse<T>(&mut self) -> Result<T, T::Error>
    where
        T: FromCbor,
        T::Error: From<self::Error>,
    {
        self.try_parse::<T>()?.ok_or(Error::NotEnoughData.into())
    }

    pub fn try_parse_array<T, F, E>(&mut self, f: F) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(&mut Array, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value(|value, start, tags| match value {
            Value::Array(a) => f(a, start, tags),
            _ => Err(
                Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into(),
            ),
        })
    }

    pub fn parse_array<T, F, E>(&mut self, f: F) -> Result<(T, usize), E>
    where
        F: FnOnce(&mut Array, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_array(f)?.ok_or(Error::NotEnoughData.into())
    }

    pub fn try_parse_map<T, F, E>(&mut self, f: F) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(&mut Map, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value(|value, start, tags| match value {
            Value::Map(m) => f(m, start, tags),
            _ => Err(
                Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into(),
            ),
        })
    }

    pub fn parse_map<T, F, E>(&mut self, f: F) -> Result<(T, usize), E>
    where
        F: FnOnce(&mut Map, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_map(f)?.ok_or(Error::NotEnoughData.into())
    }
}

enum SequenceDebugInfo {
    Unknown,
    Value(String),
    Array(Vec<SequenceDebugInfo>),
    Map(Vec<(SequenceDebugInfo, SequenceDebugInfo)>),
}

impl std::fmt::Debug for SequenceDebugInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("..."),
            Self::Value(s) => f.write_str(s),
            Self::Array(items) => f.debug_list().entries(items).finish(),
            Self::Map(items) => {
                let mut m = f.debug_map();
                for (k, v) in items {
                    m.entry(k, v);
                }
                m.finish()
            }
        }
    }
}

#[derive(Error, Debug)]
enum DebugError {
    #[error(transparent)]
    Decode(#[from] Error),

    #[error("{0:?}")]
    Rollup(SequenceDebugInfo),
}

fn debug_fmt(
    value: Value,
    _tags: Vec<u64>,
    max_recursion: usize,
) -> Result<SequenceDebugInfo, DebugError> {
    match value {
        Value::Array(a) => sequence_debug_fmt(a, max_recursion),
        Value::Map(m) => sequence_debug_fmt(m, max_recursion),
        value => Ok(SequenceDebugInfo::Value(format!("{value:?}"))),
    }
}

fn sequence_debug_fmt<const D: usize>(
    sequence: &mut Sequence<'_, D>,
    max_recursion: usize,
) -> Result<SequenceDebugInfo, DebugError> {
    if max_recursion == 0 {
        return Err(Error::MaxRecursion.into());
    }
    if D == 1 {
        let mut items = Vec::new();
        loop {
            match sequence
                .try_parse_value(|value, _, tags| debug_fmt(value, tags, max_recursion - 1))
            {
                Ok(None) => break Ok(SequenceDebugInfo::Array(items)),
                Err(e) => {
                    let item = match e {
                        DebugError::Decode(e) => SequenceDebugInfo::Value(format!("<Error: {e}>")),
                        DebugError::Rollup(item) => item,
                    };
                    items.push(item);
                    break Err(DebugError::Rollup(SequenceDebugInfo::Array(items)));
                }
                Ok(Some((item, _))) => items.push(item),
            }
        }
    } else {
        let mut items = Vec::new();
        loop {
            match sequence
                .try_parse_value(|value, _, tags| debug_fmt(value, tags, max_recursion - 1))
            {
                Ok(None) => break Ok(SequenceDebugInfo::Map(items)),
                Err(e) => {
                    let item = match e {
                        DebugError::Decode(e) => SequenceDebugInfo::Value(format!("<Error: {e}>")),
                        DebugError::Rollup(item) => item,
                    };
                    items.push((item, SequenceDebugInfo::Unknown));
                    break Err(DebugError::Rollup(SequenceDebugInfo::Map(items)));
                }
                Ok(Some((key, _))) => {
                    match sequence
                        .parse_value(|value, _, tags| debug_fmt(value, tags, max_recursion - 1))
                    {
                        Ok((value, _)) => items.push((key, value)),
                        Err(e) => {
                            let item = match e {
                                DebugError::Decode(e) => {
                                    SequenceDebugInfo::Value(format!("<Error: {e}>"))
                                }
                                DebugError::Rollup(item) => item,
                            };
                            items.push((key, item));
                            break Err(DebugError::Rollup(SequenceDebugInfo::Map(items)));
                        }
                    }
                }
            }
        }
    }
}

impl<'a, const D: usize> std::fmt::Debug for Sequence<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut offset = 0;
        {
            let mut self_cloned = Sequence::<D> {
                data: &self.data[*self.offset..],
                count: self.count,
                offset: &mut offset,
                idx: self.idx,
            };

            match sequence_debug_fmt(&mut self_cloned, 16) {
                Ok(s) => write!(f, "{s:?}"),
                Err(DebugError::Rollup(s)) => write!(f, "{s:?}"),
                Err(DebugError::Decode(e)) => write!(f, "<Error: {e}>"),
            }
        }
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

fn to_array<const N: usize>(data: &[u8]) -> Result<[u8; N], Error> {
    match data.len().cmp(&N) {
        std::cmp::Ordering::Less => Err(Error::NotEnoughData),
        std::cmp::Ordering::Equal => Ok(data.try_into().unwrap()),
        std::cmp::Ordering::Greater => Ok(data[0..N].try_into().unwrap()),
    }
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
        25 => Ok((u16::from_be_bytes(to_array(data)?) as u64, 2)),
        26 => Ok((u32::from_be_bytes(to_array(data)?) as u64, 4)),
        27 => Ok((u64::from_be_bytes(to_array(data)?), 8)),
        val if val < 24 => Ok((val as u64, 0)),
        _ => Err(Error::InvalidMinorValue(minor)),
    }
}

fn parse_data_minor(minor: u8, data: &[u8]) -> Result<(&[u8], usize), Error> {
    let (data_len, len) = parse_uint_minor(minor, data)?;
    if let Some(sum) = (len as u64).checked_add(data_len) {
        if sum > data.len() as u64 {
            Err(Error::NotEnoughData)
        } else {
            let end = ((len as u64) + data_len) as usize;
            Ok((&data[len..end], end))
        }
    } else {
        Err(Error::NotEnoughData)
    }
}

fn parse_data_chunked(major: u8, data: &[u8]) -> Result<(Vec<&[u8]>, usize), Error> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    loop {
        if offset >= data.len() {
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

pub fn try_parse_value<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(Value, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
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
            f(Value::UnsignedInteger(v), tags)
        }
        (1, minor) => {
            let (v, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::NegativeInteger(v), tags)
        }
        (2, 31) => {
            /* Indefinite length byte string */
            let (c, len) = parse_data_chunked(2, &data[offset + 1..])?;
            let v = c.into_iter().try_fold(Vec::new(), |mut v, b| {
                v.extend_from_slice(b);
                Ok::<_, Error>(v)
            })?;
            offset += len + 1;
            f(Value::Bytes(&v, true), tags)
        }
        (2, minor) => {
            /* Known length byte string */
            let (t, len) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::Bytes(t, false), tags)
        }
        (3, 31) => {
            /* Indefinite length text string */
            let (c, len) = parse_data_chunked(3, &data[offset + 1..])?;
            let s = c.into_iter().try_fold(String::new(), |mut s, b| {
                s.push_str(std::str::from_utf8(b)?);
                Ok(s)
            })?;
            offset += len + 1;
            f(Value::Text(&s, true), tags)
        }
        (3, minor) => {
            /* Known length text string */
            let (t, len) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(
                Value::Text(std::str::from_utf8(t).map_err(Into::into)?, false),
                tags,
            )
        }
        (4, 31) => {
            /* Indefinite length array */
            offset += 1;
            let mut a = Array::new(data, None, &mut offset);
            let r = f(Value::Array(&mut a), tags)?;
            a.complete().map(|_| r).map_err(Into::into)
        }
        (4, minor) => {
            /* Known length array */
            let (count, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            if count > usize::MAX as u64 {
                return Err(Error::NotEnoughData.into());
            }
            let mut a = Array::new(data, Some(count as usize), &mut offset);
            let r = f(Value::Array(&mut a), tags)?;
            a.complete().map(|_| r).map_err(Into::into)
        }
        (5, 31) => {
            /* Indefinite length map */
            offset += 1;
            let mut m = Map::new(data, None, &mut offset);
            let r = f(Value::Map(&mut m), tags)?;
            m.complete().map(|_| r).map_err(Into::into)
        }
        (5, minor) => {
            /* Known length array */
            let (count, len) = parse_uint_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            if count > (usize::MAX as u64) / 2 {
                return Err(Error::NotEnoughData.into());
            }
            let mut m = Map::new(data, Some((count * 2) as usize), &mut offset);
            let r = f(Value::Map(&mut m), tags)?;
            m.complete().map(|_| r).map_err(Into::into)
        }
        (6, _) => unreachable!(),
        (7, 20) => {
            /* False */
            offset += 1;
            f(Value::False, tags)
        }
        (7, 21) => {
            /* True */
            offset += 1;
            f(Value::True, tags)
        }
        (7, 22) => {
            /* Null */
            offset += 1;
            f(Value::Null, tags)
        }
        (7, 23) => {
            /* Undefined */
            offset += 1;
            f(Value::Undefined, tags)
        }
        (7, minor @ 0..=19) => {
            /* Unassigned simple type */
            offset += 1;
            f(Value::Simple(minor), tags)
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
            f(Value::Simple(v), tags)
        }
        (7, 25) => {
            /* FP16 */
            let v = half::f16::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 3;
            f(Value::Float(v.into()), tags)
        }
        (7, 26) => {
            /* FP32 */
            let v = f32::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 5;
            f(Value::Float(v.into()), tags)
        }
        (7, 27) => {
            /* FP64 */
            let v = f64::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 9;
            f(Value::Float(v), tags)
        }
        (7, minor) => {
            return Err(Error::InvalidSimpleType(minor).into());
        }
        (8.., _) => unreachable!(),
    }
    .map(|r| Some((r, offset)))
}

#[inline]
pub fn parse_value<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(Value, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value(data, f)?.ok_or(Error::NotEnoughData.into())
}

pub fn try_parse_array<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(&mut Array, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value(data, |value, tags| match value {
        Value::Array(a) => f(a, tags),
        _ => {
            Err(Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into())
        }
    })
}

pub fn parse_array<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Array, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    parse_value(data, |value, tags| match value {
        Value::Array(a) => f(a, tags),
        _ => {
            Err(Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into())
        }
    })
}

pub fn try_parse_map<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(&mut Map, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value(data, |value, tags| match value {
        Value::Map(m) => f(m, tags),
        _ => Err(Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into()),
    })
}

pub fn parse_map<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Map, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    parse_value(data, |value, tags| match value {
        Value::Map(m) => f(m, tags),
        _ => Err(Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into()),
    })
}

pub fn try_parse<T>(data: &[u8]) -> Result<Option<T>, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    T::try_from_cbor(data).map(|r| r.map(|(v, _)| v))
}

pub fn parse<T>(data: &[u8]) -> Result<T, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    try_parse::<T>(data)?.ok_or(Error::NotEnoughData.into())
}

impl FromCbor for u8 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u16 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u32 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for usize {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = u64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u64 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::UnsignedInteger(n) => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Unsigned Integer".to_string(),
                        "Tagged Unsigned Integer".to_string(),
                    ))
                } else {
                    Ok(n)
                }
            }
            value => Err(Error::IncorrectType(
                "Untagged Unsigned Integer".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
    }
}

impl FromCbor for i8 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i16 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i32 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for isize {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = i64::try_from_cbor(data)? {
            Ok(Some((v.try_into()?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i64 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::UnsignedInteger(n) => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Integer".to_string(),
                        "Tagged Integer".to_string(),
                    ))
                } else {
                    i64::try_from(n).map_err(Into::into)
                }
            }
            Value::NegativeInteger(n) => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Integer".to_string(),
                        "Tagged Integer".to_string(),
                    ))
                } else {
                    Ok(-1i64 - i64::try_from(n)?)
                }
            }
            value => Err(Error::IncorrectType(
                "Untagged Integer".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
    }
}

impl FromCbor for f32 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        if let Some((v, len)) = f64::try_from_cbor(data)? {
            Ok(Some((f32::from_f64(v).ok_or(Error::PrecisionLoss)?, len)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for f64 {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::Float(f) => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Float".to_string(),
                        "Tagged Float".to_string(),
                    ))
                } else {
                    Ok(f)
                }
            }
            value => Err(Error::IncorrectType(
                "Untagged Float".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
    }
}

impl FromCbor for bool {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::False => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Boolean".to_string(),
                        "Tagged Boolean".to_string(),
                    ))
                } else {
                    Ok(false)
                }
            }
            Value::True => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Boolean".to_string(),
                        "Tagged Boolean".to_string(),
                    ))
                } else {
                    Ok(true)
                }
            }
            value => Err(Error::IncorrectType(
                "Untagged Boolean".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
    }
}

impl FromCbor for Vec<u8> {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::Bytes(v, _) => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Byte String".to_string(),
                        "Tagged Byte String".to_string(),
                    ))
                } else {
                    Ok(v.to_vec())
                }
            }
            value => Err(Error::IncorrectType(
                "Untagged Byte String".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
    }
}

impl FromCbor for String {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::Text(v, _) => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Text String".to_string(),
                        "Tagged Text String".to_string(),
                    ))
                } else {
                    Ok(v.to_string())
                }
            }
            value => Err(Error::IncorrectType(
                "Untagged Text String".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        match try_parse_value(data, |value, tags| match value {
            Value::Undefined => {
                if !tags.is_empty() {
                    Err(Error::IncorrectType(
                        "Untagged Undefined".to_string(),
                        "Tagged Undefined".to_string(),
                    ))
                } else {
                    Ok(true)
                }
            }
            _ => Ok(false),
        })? {
            Some((true, len)) => Ok(Some((None, len))),
            Some((false, _)) => {
                let (v, len) = T::try_from_cbor(data)?.unwrap();
                Ok(Some((Some(v), len)))
            }
            None => Ok(None),
        }
    }
}
