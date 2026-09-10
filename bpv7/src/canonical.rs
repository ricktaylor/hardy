/*!
Shared canonical-decode plumbing.

Every wire field in bpv7 is decoded in its canonical (shortest) CBOR form,
with tag runs rejected from their first byte. The helpers here own that
discipline once, generic over the error domain, so the bundle grammar, the
BPSec ASB grammar, and the status-report grammar all share one
implementation.
*/

use hardy_cbor::decode::{Error as CborError, Untagged};

use crate::Error;

/// Trait for error types that can represent an invalid field error.
///
/// Implemented by each of the crate's error domains so the
/// [`CaptureFieldErr`] extension trait and the canonical-decode helpers can
/// wrap failures with a field label. The trait is public only within this
/// private module: external code cannot name it, so the set of implementors
/// is sealed to the crate by construction.
pub trait HasInvalidField: Sized {
    /// Wraps an already-domain-typed source error with a field label.
    fn invalid_field(field: &'static str, source: Self) -> Self;
}

/// Extension trait for `Result` that maps errors to an `InvalidField` variant.
///
/// This is useful for providing more context when a parsing error occurs.
/// The error type `E` is specified on the method, allowing turbofish syntax
/// (`.map_field_err::<Error>("field")`) when type inference is insufficient.
/// The source error is converted into the target domain at wrap time, so
/// the resulting chain is fully typed.
pub trait CaptureFieldErr<T, Err> {
    /// Maps the error to an `InvalidField` error with the given field name.
    fn map_field_err<E: HasInvalidField + From<Err>>(
        self,
        field: &'static str,
    ) -> core::result::Result<T, E>;
}

impl<T, Err> CaptureFieldErr<T, Err> for core::result::Result<T, Err> {
    fn map_field_err<E: HasInvalidField + From<Err>>(
        self,
        field: &'static str,
    ) -> core::result::Result<T, E> {
        self.map_err(|e| E::invalid_field(field, e.into()))
    }
}

/// Decode the next element of a CBOR series as `T`, rejecting any
/// non-shortest encoding with the caller's `not_canonical` error. Generic
/// over the error domain — each domain passes its own `NotCanonical`
/// variant, mirroring [`parse_canonical`] — and over the series arity, so
/// bpv7 array fields, BPSec ASB sequences, and status-report fields all
/// share the one implementation. Decodes through [`Untagged`], so a tag
/// run in front of the element is rejected from its first byte without
/// being read; the rejection surfaces as `not_canonical`, never as the
/// raw cbor `UnexpectedTag`.
pub fn require_canonical<T, E, const D: usize>(
    seq: &mut hardy_cbor::decode::Series<D>,
    field: &'static str,
    not_canonical: E,
) -> core::result::Result<T, E>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<CborError> + Into<E>,
    E: HasInvalidField,
{
    match seq.parse::<(Untagged<T>, bool)>() {
        // The wrap-time `Into<E>` conversion routes through the domain's
        // `From<CborError>` impl, which already translates the `Untagged`
        // rejection (`UnexpectedTag`) into the domain's own canonical
        // error, so no downcast sniffing is needed here.
        Err(e) => Err(E::invalid_field(field, e.into())),
        Ok((_, false)) => Err(E::invalid_field(field, not_canonical)),
        Ok((Untagged(t), true)) => Ok(t),
    }
}

/// Decode a `T` from the start of `data` in its canonical (shortest) form,
/// returning the value and the bytes consumed. A non-canonical encoding —
/// `T::from_cbor` reporting `shortest == false` — is rejected with
/// `not_canonical`. The whole-slice counterpart of [`require_canonical`] (which
/// decodes an array element), shared by leaf `FromCbor` impls whose wire form is
/// a single bare value: block/CRC type codes, DTN times, BPSec context and
/// variant ids. Generic over the caller's error so each keeps its own
/// `NotCanonical`. Decodes through [`Untagged`], so a tag run in front
/// of the value is rejected from its first byte without being read; the
/// rejection flows through `E`'s `From<T::Error>` conversion, which every
/// bpv7 error domain translates to its own `NotCanonical`.
pub fn parse_canonical<T, E>(data: &[u8], not_canonical: E) -> core::result::Result<(T, usize), E>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<CborError>,
    E: From<T::Error>,
{
    let (Untagged(value), shortest, len) =
        hardy_cbor::decode::parse::<(Untagged<T>, bool, usize)>(data)?;
    if shortest {
        Ok((value, len))
    } else {
        Err(not_canonical)
    }
}

/// Decode the next element of a bundle block array as `T`, wrapping any
/// failure with the field label and reporting whether the encoding was
/// canonical. The bundle-grammar sibling of [`require_canonical`], for
/// callers that accumulate the canonical flag instead of rejecting on it.
pub fn parse_item<T>(
    block: &mut hardy_cbor::decode::Array<'_>,
    field: &'static str,
) -> core::result::Result<(T, bool), Error>
where
    T: hardy_cbor::decode::FromCbor,
    <T as hardy_cbor::decode::FromCbor>::Error: From<CborError>,
    Error: From<<T as hardy_cbor::decode::FromCbor>::Error>,
{
    let (Untagged(v), s): (Untagged<T>, bool) = block.parse().map_field_err::<Error>(field)?;
    Ok((v, s))
}
