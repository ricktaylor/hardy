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

    #[error("Incorrect type, expecting {0}, found {1}")]
    IncorrectType(String, String),

    #[error("Chunked string contains an invalid chunk")]
    InvalidChunk,

    #[error("Invalid simple type {0}")]
    InvalidSimpleType(u8),

    #[error("Map has key but no value")]
    PartialMap,

    #[error("Too many levels of recursion")]
    Recursion,

    #[error(transparent)]
    InvalidUtf8(#[from] Utf8Error),

    #[error(transparent)]
    TryFromIntError(#[from] std::num::TryFromIntError),

    #[error("Loss of floating-point precision")]
    PrecisionLoss,
}

pub trait FromCbor: Sized {
    type Error;

    #[allow(clippy::type_complexity)]
    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error>;

    fn from_cbor_tagged(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error>
    where
        Self::Error: From<self::Error>,
    {
        Self::try_from_cbor_tagged(data)?.ok_or(Error::NotEnoughData.into())
    }

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        Self::try_from_cbor_tagged(data).map(|r| r.map(|(v, len, _)| (v, len)))
    }

    fn from_cbor(data: &[u8]) -> Result<(Self, usize), Self::Error>
    where
        Self::Error: From<self::Error>,
    {
        Self::from_cbor_tagged(data).map(|(v, len, _)| (v, len))
    }
}

#[derive(Debug)]
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
    pub fn type_name(&self) -> String {
        match &self {
            Value::UnsignedInteger(_) => "UnsignedInteger".to_string(),
            Value::NegativeInteger(_) => "NegativeInteger".to_string(),
            Value::Bytes(_, true) => "Definite-length Byte String".to_string(),
            Value::Bytes(_, false) => "Indefinite-length Byte String".to_string(),
            Value::Text(_, true) => "Definite-length Text String".to_string(),
            Value::Text(_, false) => "Indefinite-length Text String".to_string(),
            Value::Array(_) => "Array".to_string(),
            Value::Map(_) => "Map".to_string(),
            Value::False => "False".to_string(),
            Value::True => "True".to_string(),
            Value::Null => "Null".to_string(),
            Value::Undefined => "Undefined".to_string(),
            Value::Simple(v) => format!("Simple Value {v}"),
            Value::Float(_) => "Float".to_string(),
        }
    }
}

pub struct Sequence<'a, const D: usize> {
    data: &'a [u8],
    count: Option<usize>,
    offset: &'a mut usize,
    idx: usize,
    max_recursion: Option<usize>,
}

impl<'a, const D: usize> std::fmt::Debug for Sequence<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut offset = 0;
        {
            let max_recursion = if let Some(max_recursion) = self.max_recursion {
                if max_recursion == 1 {
                    return f.write_str("Too many levels of recursion");
                }
                Some(max_recursion - 1)
            } else {
                None
            };
            let mut self_cloned = Sequence::<D> {
                data: &self.data[*self.offset..],
                count: self.count,
                offset: &mut offset,
                idx: self.idx,
                max_recursion,
            };
            if D == 1 {
                let mut l = f.debug_list();
                loop {
                    let v = self_cloned
                        .try_parse_value(|value, _, _| {
                            l.entry(&value);
                            Ok::<(), Error>(())
                        })
                        .unwrap_or(None);
                    if v.is_none() {
                        break;
                    }
                }
                l.finish()
            } else {
                let mut s = f.debug_map();
                loop {
                    let v = self_cloned
                        .try_parse_value(|value, _, _| {
                            s.key(&value);
                            Ok::<(), Error>(())
                        })
                        .unwrap_or(None);
                    if v.is_none() {
                        break;
                    }

                    let v = self_cloned
                        .try_parse_value(|value, _, _| {
                            s.value(&value);
                            Ok::<(), Error>(())
                        })
                        .unwrap_or(None);
                    if v.is_none() {
                        s.value(&"<Missing>");
                        break;
                    }
                }
                s.finish()
            }
        }
    }
}

pub type Array<'a> = Sequence<'a, 1>;
pub type Map<'a> = Sequence<'a, 2>;

impl<'a, const D: usize> Sequence<'a, D> {
    fn new(
        data: &'a [u8],
        count: Option<usize>,
        offset: &'a mut usize,
        max_recursion: Option<usize>,
    ) -> Result<Self, Error> {
        Ok(Self {
            data,
            count,
            offset,
            idx: 0,
            max_recursion: if let Some(max_recursion) = max_recursion {
                if max_recursion == 1 {
                    return Err(Error::Recursion.into());
                }
                Some(max_recursion - 1)
            } else {
                None
            },
        })
    }

    pub fn count(&self) -> Option<usize> {
        self.count.map(|c| c / D)
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
        // Parse and discard any remaining items
        while let Some((_, _)) = self.try_parse_value(|_, _, _| Ok::<_, Error>(()))? {}
        Ok(())
    }

    fn try_parse_value_inner<T, F, E>(
        &mut self,
        max_recursion: Option<usize>,
        f: F,
    ) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(Value, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        // Check for end of array
        if self.check_for_end()? {
            Ok(None)
        } else {
            // Parse sub-item
            let max_recursion = match (max_recursion, self.max_recursion) {
                (Some(c), Some(d)) => Some(c.min(d)),
                (Some(c), None) => Some(c),
                (None, Some(d)) => Some(d),
                (None, None) => None,
            };
            let item_start = *self.offset;
            try_parse_value_inner(&self.data[item_start..], max_recursion, |value, tags| {
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

    #[inline]
    pub fn try_parse_value_checked<T, F, E>(
        &mut self,
        max_recursion: usize,
        f: F,
    ) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(Value, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value_inner(Some(max_recursion), f)
    }

    #[inline]
    pub fn try_parse_value<T, F, E>(&mut self, f: F) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(Value, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value_inner(None, f)
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
            T::from_cbor(&self.data[*self.offset..]).map(|(value, len)| {
                self.idx += 1;
                *self.offset += len;
                Some(value)
            })
        }
    }

    pub fn parse<T>(&mut self) -> Result<T, T::Error>
    where
        T: FromCbor,
        T::Error: From<self::Error>,
    {
        let r: Result<Option<T>, <T as FromCbor>::Error> = self.try_parse();
        r?.ok_or(Error::NotEnoughData.into())
    }

    pub fn try_parse_array<T, F, E>(&mut self, f: F) -> Result<Option<(T, usize)>, E>
    where
        F: FnOnce(&mut Array, usize, Vec<u64>) -> Result<T, E>,
        E: From<Error>,
    {
        self.try_parse_value(|value, start, tags| match value {
            Value::Array(a) => f(a, start, tags),
            _ => Err(Error::IncorrectType("Array".to_string(), value.type_name()).into()),
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
            _ => Err(Error::IncorrectType("Map".to_string(), value.type_name()).into()),
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

fn try_parse_value_inner<T, F, E>(
    data: &[u8],
    max_recursion: Option<usize>,
    f: F,
) -> Result<Option<(T, usize)>, E>
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
            let mut a = Array::new(data, None, &mut offset, max_recursion)?;
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
            let mut a = Array::new(data, Some(count as usize), &mut offset, max_recursion)?;
            let r = f(Value::Array(&mut a), tags)?;
            a.complete().map(|_| r).map_err(Into::into)
        }
        (5, 31) => {
            /* Indefinite length map */
            offset += 1;
            let mut m = Map::new(data, None, &mut offset, max_recursion)?;
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
            let mut m = Map::new(data, Some((count * 2) as usize), &mut offset, max_recursion)?;
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
pub fn try_parse_value_checked<T, F, E>(
    data: &[u8],
    max_recursion: usize,
    f: F,
) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(Value, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value_inner(data, Some(max_recursion), f)
}

#[inline]
pub fn try_parse_value<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(Value, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value_inner(data, None, f)
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
        _ => Err(Error::IncorrectType("Array".to_string(), value.type_name()).into()),
    })
}

pub fn parse_array<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Array, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    parse_value(data, |value, tags| match value {
        Value::Array(a) => f(a, tags),
        _ => Err(Error::IncorrectType("Array".to_string(), value.type_name()).into()),
    })
}

pub fn try_parse_map<T, F, E>(data: &[u8], f: F) -> Result<Option<(T, usize)>, E>
where
    F: FnOnce(&mut Map, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    try_parse_value(data, |value, tags| match value {
        Value::Map(m) => f(m, tags),
        _ => Err(Error::IncorrectType("Map".to_string(), value.type_name()).into()),
    })
}

pub fn parse_map<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Map, Vec<u64>) -> Result<T, E>,
    E: From<Error>,
{
    parse_value(data, |value, tags| match value {
        Value::Map(m) => f(m, tags),
        _ => Err(Error::IncorrectType("Map".to_string(), value.type_name()).into()),
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
    T::from_cbor(data).map(|(v, _)| v)
}

impl FromCbor for u8 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = u64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u16 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = u64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u32 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = u64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for usize {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = u64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for u64 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::UnsignedInteger(n) => Ok((n, tags)),
            value => Err(Error::IncorrectType(
                "UnsignedInteger".to_string(),
                value.type_name(),
            )),
        })
        .map(|r| r.map(|((v, tags), len)| (v, len, tags)))
    }
}

impl FromCbor for i8 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = i64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i16 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = i64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i32 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = i64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for isize {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = i64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for i64 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::UnsignedInteger(n) => Ok((i64::try_from(n)?, tags)),
            Value::NegativeInteger(n) => Ok((-1i64 - i64::try_from(n)?, tags)),
            value => Err(Error::IncorrectType(
                "Integer".to_string(),
                value.type_name(),
            )),
        })
        .map(|r| r.map(|((v, tags), len)| (v, len, tags)))
    }
}

impl FromCbor for f32 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = f64::try_from_cbor_tagged(data)? {
            Ok(Some((
                f32::from_f64(v).ok_or(Error::PrecisionLoss)?,
                len,
                tags,
            )))
        } else {
            Ok(None)
        }
    }
}

impl FromCbor for f64 {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::Float(f) => Ok((f, tags)),
            value => Err(Error::IncorrectType("Float".to_string(), value.type_name())),
        })
        .map(|r| r.map(|((v, tags), len)| (v, len, tags)))
    }
}

impl FromCbor for bool {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::False => Ok((false, tags)),
            Value::True => Ok((true, tags)),
            value => Err(Error::IncorrectType(
                "Boolean".to_string(),
                value.type_name(),
            )),
        })
        .map(|r| r.map(|((v, tags), len)| (v, len, tags)))
    }
}

impl FromCbor for Vec<u8> {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::Bytes(v, _) => Ok((v.to_vec(), tags)),
            value => Err(Error::IncorrectType(
                "Byte String".to_string(),
                value.type_name(),
            )),
        })
        .map(|r| r.map(|((v, tags), len)| (v, len, tags)))
    }
}

impl FromCbor for String {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        try_parse_value(data, |value, tags| match value {
            Value::Text(v, _) => Ok((v.to_string(), tags)),
            value => Err(Error::IncorrectType(
                "Text String".to_string(),
                value.type_name(),
            )),
        })
        .map(|r| r.map(|((v, tags), len)| (v, len, tags)))
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        let (tags, offset) = parse_tags(data)?;
        if offset >= data.len() {
            if !tags.is_empty() {
                return Err(Error::JustTags.into());
            } else {
                return Ok(None);
            }
        }
        if data[offset] == (7 << 5) | 23 {
            Ok(Some((None, offset + 1, tags)))
        } else {
            let (v, len) = T::from_cbor(&data[offset + 1..])?;
            Ok(Some((Some(v), len + offset + 1, tags)))
        }
    }
}
