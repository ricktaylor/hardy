use num_traits::FromPrimitive;
use std::str::Utf8Error;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Not enough data for encoded value")]
    NotEnoughData,

    #[error("Additional items to be read")]
    AdditionalItems,

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

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error>;
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
    parsed: usize,
}

pub type Structure<'a> = Sequence<'a, 0>;
pub type Array<'a> = Sequence<'a, 1>;
pub type Map<'a> = Sequence<'a, 2>;

impl<'a, const D: usize> Sequence<'a, D> {
    fn new(data: &'a [u8], count: Option<usize>, offset: &'a mut usize) -> Self {
        Self {
            data,
            count,
            offset,
            parsed: 0,
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
            if self.parsed >= count {
                Ok(true)
            } else {
                Ok(false)
            }
        } else if *self.offset >= self.data.len() {
            if D == 0 && *self.offset == self.data.len() {
                self.count = Some(self.parsed);
                Ok(true)
            } else {
                Err(Error::NotEnoughData)
            }
        } else if D > 0 && self.data[*self.offset] == 0xFF {
            if self.parsed % D == 1 {
                Err(Error::PartialMap)
            } else {
                *self.offset += 1;
                self.count = Some(self.parsed);
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }

    pub fn offset(&self) -> usize {
        *self.offset
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
            return Err(Error::AdditionalItems);
        }
        Ok(())
    }

    pub fn skip_value(&mut self, max_recursion: usize) -> Result<Option<(bool, usize)>, Error> {
        self.try_parse_value(|mut value, shortest, tags| {
            value
                .skip(max_recursion)
                .map(|s| s && shortest && tags.is_empty())
        })
    }

    pub fn skip_to_end(&mut self, max_recursion: usize) -> Result<bool, Error> {
        let mut shortest = true;
        while self
            .try_parse_value(|mut value, s, tags| {
                shortest = value.skip(max_recursion)? && shortest && s && tags.is_empty();
                Ok::<_, Error>(())
            })?
            .is_some()
        {
            if D == 2 {
                self.parse_value(|mut value, s, tags| {
                    shortest = value.skip(max_recursion)? && shortest && s && tags.is_empty();
                    Ok::<_, Error>(())
                })?;
            }
        }
        Ok(shortest)
    }

    pub fn try_parse_value<T, F, E>(&mut self, f: F) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(Value, bool, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        // Check for end of array
        if self.check_for_end()? {
            Ok(None)
        } else {
            // Parse sub-item
            let item_start = *self.offset;
            let r = try_parse_value(&self.data[item_start..], |value, shortest, tags| {
                f(value, shortest, tags)
            });
            if let Ok(Some((_, len))) = r {
                self.parsed += 1;
                *self.offset += len;
            }
            r
        }
    }

    #[inline]
    pub fn parse_value<T, F, E>(&mut self, f: F) -> Result<(T, usize), E>
    where
        F: FnOnce(Value, bool, Vec<u64>) -> Result<T, E>,
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
                Some((value, _, len)) => {
                    self.parsed += 1;
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
        F: FnOnce(&mut Array, bool, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value(|value, shortest, tags| match value {
            Value::Array(a) => f(a, shortest, tags),
            _ => Err(
                Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into(),
            ),
        })
    }

    pub fn parse_array<T, F, E>(&mut self, f: F) -> Result<(T, usize), E>
    where
        F: FnOnce(&mut Array, bool, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_array(f)?.ok_or(Error::NotEnoughData.into())
    }

    pub fn try_parse_map<T, F, E>(&mut self, f: F) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(&mut Map, bool, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value(|value, shortest, tags| match value {
            Value::Map(m) => f(m, shortest, tags),
            _ => Err(
                Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into(),
            ),
        })
    }

    pub fn parse_map<T, F, E>(&mut self, f: F) -> Result<(T, usize), E>
    where
        F: FnOnce(&mut Map, bool, Vec<u64>) -> Result<T, E>,
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
    _shortest: bool,
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
    if D == 2 {
        let mut items = Vec::new();
        loop {
            match sequence.try_parse_value(|value, shortest, tags| {
                debug_fmt(value, shortest, tags, max_recursion - 1)
            }) {
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
                    match sequence.parse_value(|value, shortest, tags| {
                        debug_fmt(value, shortest, tags, max_recursion - 1)
                    }) {
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
    } else {
        let mut items = Vec::new();
        loop {
            match sequence.try_parse_value(|value, shortest, tags| {
                debug_fmt(value, shortest, tags, max_recursion - 1)
            }) {
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
                parsed: self.parsed,
            };

            match sequence_debug_fmt(&mut self_cloned, 16) {
                Ok(s) => write!(f, "{s:?}"),
                Err(DebugError::Rollup(s)) => write!(f, "{s:?}"),
                Err(DebugError::Decode(e)) => write!(f, "<Error: {e}>"),
            }
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
        std::cmp::Ordering::Less => Err(Error::NotEnoughData),
        std::cmp::Ordering::Equal => Ok(data.try_into().unwrap()),
        std::cmp::Ordering::Greater => Ok(data[0..N].try_into().unwrap()),
    }
}

fn parse_uint_minor(minor: u8, data: &[u8]) -> Result<(u64, bool, usize), Error> {
    match minor {
        24 => {
            if data.is_empty() {
                Err(Error::NotEnoughData)
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

fn parse_data_minor(minor: u8, data: &[u8]) -> Result<(&[u8], bool, usize), Error> {
    let (data_len, shortest, len) = parse_uint_minor(minor, data)?;
    if let Some(sum) = (len as u64).checked_add(data_len) {
        if sum > data.len() as u64 {
            Err(Error::NotEnoughData)
        } else {
            let end = ((len as u64) + data_len) as usize;
            Ok((&data[len..end], shortest, end))
        }
    } else {
        Err(Error::NotEnoughData)
    }
}

fn parse_data_chunked(major: u8, data: &[u8]) -> Result<(Vec<&[u8]>, bool, usize), Error> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut shortest = true;
    loop {
        if offset >= data.len() {
            break Err(Error::NotEnoughData);
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
        chunks.push(chunk);
        shortest = shortest && s;
        offset += chunk_len;
    }
}

pub fn try_parse_value<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(Value, bool, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    let (tags, shortest, mut offset) = parse_tags(data)?;
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
            let (c, s, len) = parse_data_chunked(2, &data[offset + 1..])?;
            let v = c.into_iter().try_fold(Vec::new(), |mut v, b| {
                v.extend_from_slice(b);
                Ok::<_, Error>(v)
            })?;
            offset += len + 1;
            f(Value::Bytes(&v, true), shortest && s, tags)
        }
        (2, minor) => {
            /* Known length byte string */
            let (t, s, len) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(Value::Bytes(t, false), shortest && s, tags)
        }
        (3, 31) => {
            /* Indefinite length text string */
            let (c, sh, len) = parse_data_chunked(3, &data[offset + 1..])?;
            let s = c.into_iter().try_fold(String::new(), |mut s, b| {
                s.push_str(std::str::from_utf8(b)?);
                Ok(s)
            })?;
            offset += len + 1;
            f(Value::Text(&s, true), shortest && sh, tags)
        }
        (3, minor) => {
            /* Known length text string */
            let (t, s, len) = parse_data_minor(minor, &data[offset + 1..])?;
            offset += len + 1;
            f(
                Value::Text(std::str::from_utf8(t).map_err(Into::into)?, false),
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
                return Err(Error::NotEnoughData.into());
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
                return Err(Error::NotEnoughData.into());
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
            if data.len() <= offset + 1 {
                return Err(Error::NotEnoughData.into());
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
            if shortest {
                // TODO: Check for shortest form
            }
            f(Value::Float(v.into()), shortest, tags)
        }
        (7, 26) => {
            /* FP32 */
            let v = f32::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 5;
            if shortest {
                // TODO: Check for shortest form
            }
            f(Value::Float(v.into()), shortest, tags)
        }
        (7, 27) => {
            /* FP64 */
            let v = f64::from_be_bytes(to_array(&data[offset + 1..])?);
            offset += 9;
            if shortest {
                // TODO: Check for shortest form
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
    try_parse_value(data, f)?.ok_or(Error::NotEnoughData.into())
}

pub fn try_parse_structure<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(&mut Structure) -> Result<T, E>,
    E: From<Error>,
{
    if data.is_empty() {
        return Ok(None);
    }

    let mut offset = 0;
    let mut s = Structure::new(data, None, &mut offset);
    let r = f(&mut s)?;
    s.complete().map(|_| Some((r, offset))).map_err(Into::into)
}

#[inline]
pub fn parse_structure<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Structure) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_structure(data, f)?.ok_or(Error::NotEnoughData.into())
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
    parse_value(data, |value, shortest, tags| match value {
        Value::Array(a) => f(a, shortest, tags),
        _ => {
            Err(Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into())
        }
    })
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
    parse_value(data, |value, shortest, tags| match value {
        Value::Map(m) => f(m, shortest, tags),
        _ => Err(Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into()),
    })
}

pub fn try_parse<T>(data: &[u8]) -> Result<Option<T>, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    T::try_from_cbor(data).map(|r| r.map(|(v, _, _)| v))
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

impl FromCbor for Box<[u8]> {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        try_parse_value(data, |value, shortest, tags| match value {
            Value::Bytes(v, chunked) => Ok((v.into(), shortest && !chunked && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Byte String".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

impl FromCbor for String {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        try_parse_value(data, |value, shortest, tags| match value {
            Value::Text(v, chunked) => Ok((v.to_string(), shortest && !chunked && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Text String".to_string(),
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
