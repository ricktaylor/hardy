use super::{decode::*, *};
use thiserror::Error;

/// A stateful iterator for decoding a sequence of CBOR items, such as an array or a map.
///
/// `Series` provides a cursor-like interface to traverse and parse items within a
/// CBOR collection. It keeps track of the current position in the byte slice and
/// handles both definite and indefinite-length sequences.
///
/// The const generic `D` indicates the number of items per logical element (1 for arrays, 2 for maps).
pub struct Series<'a, const D: usize> {
    data: &'a [u8],
    count: Option<usize>,
    offset: &'a mut usize,
    parsed: usize,
}

impl<'a, const D: usize> Series<'a, D> {
    pub(super) fn new(data: &'a [u8], count: Option<usize>, offset: &'a mut usize) -> Self {
        Self {
            data,
            count,
            offset,
            parsed: 0,
        }
    }

    /// Returns the number of elements in the sequence, if it is definite-length.
    ///
    /// For an array, this is the number of items. For a map, it's the number of key-value pairs.
    /// Returns `None` for indefinite-length sequences until they have been fully parsed.
    #[inline]
    pub fn count(&self) -> Option<usize> {
        self.count.map(|c| c / D)
    }

    /// Returns `true` if the sequence has a definite length.
    #[inline]
    pub fn is_definite(&self) -> bool {
        self.count.is_some()
    }

    /// Checks if the end of the sequence has been reached.
    ///
    /// For definite-length sequences, this checks if the number of parsed items
    /// equals the declared count.
    ///
    /// For indefinite-length sequences, this checks for the `0xFF` break byte.
    ///
    /// For a top-level sequence (`D=0`), it checks if all bytes have been consumed.
    pub fn at_end(&mut self) -> Result<bool, Error> {
        if let Some(count) = self.count {
            Ok(self.parsed >= count)
        } else if D == 0 && *self.offset == self.data.len() {
            self.count = Some(self.parsed);
            Ok(true)
        } else if *self.offset >= self.data.len() {
            Err(Error::NeedMoreData(*self.offset + 1 - self.data.len()))
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

    /// Returns the current byte offset from the start of the containing data slice.
    #[inline]
    pub fn offset(&self) -> usize {
        *self.offset
    }

    pub(super) fn complete<T>(mut self, t: T) -> Result<T, Error> {
        if !self.at_end()? {
            return Err(Error::AdditionalItems);
        }
        Ok(t)
    }

    /// Parses and skips the next value in the sequence without fully decoding it.
    ///
    /// This is more efficient than parsing into a `Value` and then calling `skip`
    /// if you only need to advance the cursor.
    /// Returns a boolean indicating if the skipped value was in canonical form.
    pub fn skip_value(&mut self, max_recursion: usize) -> Result<bool, Error> {
        self.parse_value(|mut value, shortest, tags| {
            value
                .skip(max_recursion)
                .map(|s| s && shortest && tags.is_empty())
        })
    }

    /// Skips all remaining values until the end of the sequence is reached.
    ///
    /// Returns a boolean indicating if all skipped values were in canonical form.
    /// The `max_recursion` parameter prevents stack overflows on deeply nested structures.
    pub fn skip_to_end(&mut self, max_recursion: usize) -> Result<bool, Error> {
        let mut shortest = true;
        while !self.at_end()? {
            self.parse_value(|mut value, s, tags| {
                shortest = value.skip(max_recursion)? && shortest && s && tags.is_empty();
                Ok::<_, Error>(())
            })?;
            if D == 2 {
                self.parse_value(|mut value, s, tags| {
                    shortest = value.skip(max_recursion)? && shortest && s && tags.is_empty();
                    Ok::<_, Error>(())
                })?;
            }
        }
        Ok(shortest)
    }

    /// Tries to parse the next value in the sequence using a closure.
    ///
    /// If the end of the sequence is reached, it returns `Ok(None)`.
    /// Otherwise, it parses the next item and passes it as a [`Value`] to the
    /// closure `f`, returning `Ok(Some(result))`.
    pub fn try_parse_value<T, F, E>(&mut self, f: F) -> Result<Option<T>, E>
    where
        F: FnOnce(Value, bool, &[u64]) -> Result<T, E>,
        E: From<Error>,
    {
        // Check for end of array
        if self.at_end()? {
            Ok(None)
        } else {
            self.parse_value(f).map(Some)
        }
    }

    /// Parses the next value in the sequence using a closure.
    ///
    /// This is similar to `try_parse_value` but returns a `NoMoreItems`
    /// error if the end of the sequence has been reached, instead of `Ok(None)`.
    pub fn parse_value<T, F, E>(&mut self, f: F) -> Result<T, E>
    where
        F: FnOnce(Value, bool, &[u64]) -> Result<T, E>,
        E: From<Error>,
    {
        // Check for end of array
        if self.at_end()? {
            Err(Error::NoMoreItems.into())
        } else {
            // Parse sub-item
            let (value, len) = parse_value(&self.data[*self.offset..], |value, shortest, tags| {
                f(value, shortest, tags)
            })?;
            self.parsed += 1;
            *self.offset += len;
            Ok(value)
        }
    }

    /// Parses the next item in the sequence into a type that implements [`FromCbor`].
    ///
    /// This is a high-level convenience method. It will return a `NoMoreItems`
    /// error if the end of the sequence is reached. The `shortest` and `len`
    /// information from the `from_cbor` call is discarded.
    pub fn parse<T>(&mut self) -> Result<T, T::Error>
    where
        T: FromCbor,
        T::Error: From<self::Error>,
    {
        // Check for end of array
        if self.at_end()? {
            Err(Error::NoMoreItems.into())
        } else {
            // Parse sub-item
            let (value, _, len) = T::from_cbor(&self.data[*self.offset..])?;
            self.parsed += 1;
            *self.offset += len;
            Ok(value)
        }
    }

    /// Tries to parse the next item in the sequence into a type that implements [`FromCbor`].
    ///
    /// If the end of the sequence is reached, this returns `Ok(None)`. Otherwise,
    /// it attempts to parse the next item and returns `Ok(Some(value))`.
    pub fn try_parse<T>(&mut self) -> Result<Option<T>, T::Error>
    where
        T: FromCbor,
        T::Error: From<self::Error>,
    {
        // Check for end of array
        if self.at_end()? {
            Ok(None)
        } else {
            self.parse().map(Some)
        }
    }

    /// Parses the next item in the sequence, expecting it to be an array.
    ///
    /// This is a convenience wrapper that validates the item type and provides
    /// a nested [`Array`] to the closure `f` for processing. Returns an
    /// `IncorrectType` error if the next item is not an array.
    pub fn parse_array<T, F, E>(&mut self, f: F) -> Result<T, E>
    where
        F: FnOnce(&mut Array, bool, &[u64]) -> Result<T, E>,
        E: From<Error>,
    {
        self.parse_value(|value, shortest, tags| match value {
            Value::Array(a) => f(a, shortest, tags),
            _ => Err(
                Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into(),
            ),
        })
    }

    /// Parses the next item in the sequence, expecting it to be a map.
    ///
    /// This is a convenience wrapper that validates the item type and provides
    /// a nested [`Map`] to the closure `f` for processing. Returns an
    /// `IncorrectType` error if the next item is not a map.
    pub fn parse_map<T, F, E>(&mut self, f: F) -> Result<T, E>
    where
        F: FnOnce(&mut Map, bool, &[u64]) -> Result<T, E>,
        E: From<Error>,
    {
        self.parse_value(|value, shortest, tags| match value {
            Value::Map(m) => f(m, shortest, tags),
            _ => Err(
                Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into(),
            ),
        })
    }
}

enum SequenceDebugInfo {
    Unknown,
    Value(String),
    Array(Vec<SequenceDebugInfo>),
    Map(Vec<(SequenceDebugInfo, SequenceDebugInfo)>),
}

impl core::fmt::Debug for SequenceDebugInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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
    _tags: &[u64],
    max_recursion: usize,
) -> Result<SequenceDebugInfo, DebugError> {
    match value {
        Value::Array(a) => sequence_debug_fmt(a, max_recursion),
        Value::Map(m) => sequence_debug_fmt(m, max_recursion),
        value => Ok(SequenceDebugInfo::Value(format!("{value:?}"))),
    }
}

fn sequence_debug_fmt<const D: usize>(
    sequence: &mut Series<'_, D>,
    max_recursion: usize,
) -> Result<SequenceDebugInfo, DebugError> {
    if max_recursion == 0 {
        return Err(Error::MaxRecursion.into());
    }
    if D == 2 {
        let mut items = Vec::new();
        while !sequence.at_end()? {
            match sequence.parse_value(|value, shortest, tags| {
                debug_fmt(value, shortest, tags, max_recursion - 1)
            }) {
                Err(e) => {
                    let item = match e {
                        DebugError::Decode(e) => SequenceDebugInfo::Value(format!("<Error: {e}>")),
                        DebugError::Rollup(item) => item,
                    };
                    items.push((item, SequenceDebugInfo::Unknown));
                    return Err(DebugError::Rollup(SequenceDebugInfo::Map(items)));
                }
                Ok(key) => {
                    match sequence.parse_value(|value, shortest, tags| {
                        debug_fmt(value, shortest, tags, max_recursion - 1)
                    }) {
                        Ok(value) => items.push((key, value)),
                        Err(e) => {
                            let item = match e {
                                DebugError::Decode(e) => {
                                    SequenceDebugInfo::Value(format!("<Error: {e}>"))
                                }
                                DebugError::Rollup(item) => item,
                            };
                            items.push((key, item));
                            return Err(DebugError::Rollup(SequenceDebugInfo::Map(items)));
                        }
                    }
                }
            }
        }
        Ok(SequenceDebugInfo::Map(items))
    } else {
        let mut items = Vec::new();
        while !sequence.at_end()? {
            match sequence.parse_value(|value, shortest, tags| {
                debug_fmt(value, shortest, tags, max_recursion - 1)
            }) {
                Ok(item) => items.push(item),
                Err(e) => {
                    let item = match e {
                        DebugError::Decode(e) => SequenceDebugInfo::Value(format!("<Error: {e}>")),
                        DebugError::Rollup(item) => item,
                    };
                    items.push(item);
                    return Err(DebugError::Rollup(SequenceDebugInfo::Array(items)));
                }
            }
        }
        Ok(SequenceDebugInfo::Array(items))
    }
}

impl<const D: usize> core::fmt::Debug for Series<'_, D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut offset = 0;
        {
            let mut self_cloned = Series::<D> {
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
