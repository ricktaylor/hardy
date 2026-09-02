use alloc::{boxed::Box, string::String, vec::Vec};

use super::*;

macro_rules! impl_uint_from_cbor {
    ($($ty:ty),*) => {
        $(
            impl FromCbor for $ty {
                type Error = Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    let (v,shortest,len) = u64::from_cbor(data)?;
                    Ok((v.try_into()?, shortest, len))
                }
            }
        )*
    };
}

impl_uint_from_cbor!(u8, u16, u32, usize);

impl FromCbor for u64 {
    type Error = Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(Head, bool, usize)>(data)?;
        if let Marker::UnsignedInteger(v) = marker.marker {
            Ok((v, shortest && marker.tags.is_empty(), offset))
        } else {
            Err(Error::IncorrectType(
                "Untagged Unsigned Integer",
                marker.item_type(),
            ))
        }
    }
}

macro_rules! impl_int_from_cbor {
    ($($ty:ty),*) => {
        $(
            impl FromCbor for $ty {
                type Error = Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    let (v,shortest,len) = i64::from_cbor(data)?;
                    Ok((v.try_into()?, shortest, len))
                }
            }
        )*
    };
}

impl_int_from_cbor!(i8, i16, i32, isize);

impl FromCbor for i64 {
    type Error = Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(Head, bool, usize)>(data)?;
        match marker.marker {
            Marker::UnsignedInteger(v) => Ok((
                i64::try_from(v)?,
                shortest && marker.tags.is_empty(),
                offset,
            )),
            Marker::NegativeInteger(n) => Ok((
                -1i64 - i64::try_from(n)?,
                shortest && marker.tags.is_empty(),
                offset,
            )),
            _ => Err(Error::IncorrectType("Untagged Integer", marker.item_type())),
        }
    }
}

macro_rules! impl_float_from_cbor {
    ($(($ty:ty, $convert_expr:expr)),*) => {
        $(
            impl FromCbor for $ty {
                type Error = Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    let (v, shortest, len) = f64::from_cbor(data)?;
                    Ok((
                        $convert_expr(v).ok_or(Error::PrecisionLoss)?,
                        shortest,
                        len,
                    ))
                }
            }
        )*
    };
}

impl_float_from_cbor!(
    (half::f16, |v: f64| {
        <half::f16 as num_traits::FromPrimitive>::from_f64(v)
    }),
    (f32, f32::from_f64)
);

impl FromCbor for f64 {
    type Error = Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(Head, bool, usize)>(data)?;
        if let Marker::Float(v) = marker.marker {
            Ok((v, shortest && marker.tags.is_empty(), offset))
        } else {
            Err(Error::IncorrectType("Untagged Float", marker.item_type()))
        }
    }
}

impl FromCbor for bool {
    type Error = Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(Head, bool, usize)>(data)?;
        match marker.marker {
            Marker::False => Ok((false, shortest && marker.tags.is_empty(), offset)),
            Marker::True => Ok((true, shortest && marker.tags.is_empty(), offset)),
            _ => Err(Error::IncorrectType("Untagged Boolean", marker.item_type())),
        }
    }
}

// The two owned-container impls. Each copies by construction — the requested
// type announces the allocation at the call site, which is what keeps the
// codec's zero-copy discipline honest (see the `FromCbor` trait docs). On
// hot paths, borrow through `parse_value` and `Value::Text`/`Value::Bytes`
// instead.

impl FromCbor for String {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        parse_value(data, |value, shortest, tags| match value {
            Value::Text(t) => Ok((String::from(t), shortest && tags.is_empty())),
            // Indefinite-length text is RFC-permitted but never canonical;
            // the chunk gather is the copy the owned return type announces.
            Value::TextStream(chunks) => Ok((chunks.concat(), false)),
            value => Err(Error::IncorrectType(
                "Untagged Text String",
                value.item_type(tags),
            )),
        })
        .map(|((value, shortest), len)| (value, shortest, len))
    }
}

/// Decode-only by design: there is deliberately no matching `ToCbor` for
/// `Box<[u8]>`. The blanket `[T]` encode gives `[u8]` *array* semantics
/// (major type 4), so a byte-string impl here would make `x.to_cbor()` and
/// `(&*x).to_cbor()` emit different wire types for the same bytes. Encode
/// byte strings explicitly through [`encode::Bytes`](crate::encode::Bytes);
/// an owned blob type wraps that for encoding and delegates to this impl
/// for decoding.
impl FromCbor for Box<[u8]> {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        parse_value(data, |value, shortest, tags| match value {
            Value::Bytes(r) => Ok((Box::from(&data[r]), shortest && tags.is_empty())),
            // Indefinite-length bytes are RFC-permitted but never canonical;
            // the chunk gather is the copy the owned return type announces.
            Value::ByteStream(ranges) => {
                let mut gathered = Vec::with_capacity(ranges.iter().map(|r| r.len()).sum());
                for r in ranges {
                    gathered.extend_from_slice(&data[r]);
                }
                Ok((gathered.into_boxed_slice(), false))
            }
            value => Err(Error::IncorrectType(
                "Untagged Byte String",
                value.item_type(tags),
            )),
        })
        .map(|((value, shortest), len)| (value, shortest, len))
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
    T::Error: From<Error>,
{
    type Error = T::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        // Peek at the head only — far cheaper than parse_value, which
        // materialises chunk lists and constructs nested Series for the
        // common Some(T) case where we throw the parse away and call
        // T::from_cbor again. Any Undefined marker is treated as None,
        // tagged or not; the tag presence is folded into `shortest` so
        // callers can flag the non-canonical wrapping if they care.
        let (head, shortest, len) = parse::<(Head, bool, usize)>(data)?;
        if matches!(head.marker, Marker::Undefined) {
            Ok((None, shortest && head.tags.is_empty(), len))
        } else {
            T::from_cbor(data).map(|(v, s, len)| (Some(v), s, len))
        }
    }
}

macro_rules! impl_tuple_from_cbor {
    ($(($tuple_ty:ty, $map_expr:expr)),*) => {
        $(
            impl<T> FromCbor for $tuple_ty
            where
                T: FromCbor,
                T::Error: From<Error>,
            {
                type Error = T::Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    T::from_cbor(data).map(|(value, shortest, length)| ($map_expr(value, shortest, length), shortest, length))
                }
            }
        )*
    };
}

impl_tuple_from_cbor!(
    ((T, bool, usize), |value, shortest, length| (
        value, shortest, length
    )),
    ((T, bool), |value, shortest, _length| (value, shortest)),
    ((T, usize), |value, _shortest, length| (value, length))
);
