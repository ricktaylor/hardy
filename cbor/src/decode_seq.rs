use super::decode::*;
use thiserror::Error;

pub struct Sequence<'a, const D: usize> {
    data: &'a [u8],
    count: Option<usize>,
    offset: &'a mut usize,
    parsed: usize,
}

impl<'a, const D: usize> Sequence<'a, D> {
    pub(super) fn new(data: &'a [u8], count: Option<usize>, offset: &'a mut usize) -> Self {
        Self {
            data,
            count,
            offset,
            parsed: 0,
        }
    }

    pub fn len(&self) -> Option<usize> {
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

    pub(super) fn complete(mut self) -> Result<(), Error> {
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

impl<const D: usize> std::fmt::Debug for Sequence<'_, D> {
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
