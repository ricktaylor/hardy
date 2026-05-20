use super::*;

macro_rules! impl_uint_from_cbor {
    ($($ty:ty),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

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
    type Error = self::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(Head, bool, usize)>(data)?;
        if let Marker::UnsignedInteger(v) = marker.marker {
            Ok((v, shortest && marker.tags.is_empty(), offset))
        } else {
            Err(Error::IncorrectType(
                "Untagged Unsigned Integer".to_string(),
                marker.to_string(),
            ))
        }
    }
}

macro_rules! impl_int_from_cbor {
    ($($ty:ty),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

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
    type Error = self::Error;

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
            _ => Err(Error::IncorrectType(
                "Untagged Integer".to_string(),
                marker.to_string(),
            )),
        }
    }
}

macro_rules! impl_float_from_cbor {
    ($(($ty:ty, $convert_expr:expr)),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

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
    type Error = self::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(Head, bool, usize)>(data)?;
        if let Marker::Float(v) = marker.marker {
            Ok((v, shortest && marker.tags.is_empty(), offset))
        } else {
            Err(Error::IncorrectType(
                "Untagged Float".to_string(),
                marker.to_string(),
            ))
        }
    }
}

impl FromCbor for bool {
    type Error = self::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(Head, bool, usize)>(data)?;
        match marker.marker {
            Marker::False => Ok((false, shortest && marker.tags.is_empty(), offset)),
            Marker::True => Ok((true, shortest && marker.tags.is_empty(), offset)),
            _ => Err(Error::IncorrectType(
                "Untagged Boolean".to_string(),
                marker.to_string(),
            )),
        }
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        match parse_value(data, |value, shortest, tags| match value {
            Value::Undefined => Ok(Some(shortest && tags.is_empty())),
            _ => Ok(None),
        })? {
            (Some(shortest), len) => Ok((None, shortest, len)),
            (None, _) => T::from_cbor(data).map(|(v, shortest, len)| (Some(v), shortest, len)),
        }
    }
}

macro_rules! impl_tuple_from_cbor {
    ($(($tuple_ty:ty, $map_expr:expr)),*) => {
        $(
            impl<T> FromCbor for $tuple_ty
            where
                T: FromCbor,
                T::Error: From<self::Error>,
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
