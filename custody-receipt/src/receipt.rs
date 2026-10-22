//! Version 2 custody receipts and the Custody Receipt Block data that holds
//! them (draft-bsvkey-dtn-custody-receipt-block-01, Sections 3 and 4).
//!
//! A receipt is an untagged COSE_Sign1 (RFC 9052 §4.2) with EdDSA over
//! Ed25519 (RFC 9053 §2.2, alg -8), whose payload is a CBOR map with integer
//! keys. The block data is a CBOR array of receipts in the order they were
//! appended. Appending re-emits the array head and copies every earlier
//! receipt's bytes unchanged, so no earlier receipt is decoded and
//! re-encoded.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use thiserror::Error;

/// The experimental block-type code the Custody Receipt Block uses until a
/// code point is assigned (draft Section 4.1).
pub const BLOCK_TYPE: u64 = 224;

/// The most receipts one block may hold.
pub const MAX_RECEIPTS: usize = 64;

// `{1: -8}`: the protected header naming EdDSA.
const PROTECTED: [u8; 3] = [0xa1, 0x01, 0x27];

// The nesting depth a skipped item may reach; a receipt nests three deep.
const MAX_DEPTH: usize = 16;

/// Why existing Custody Receipt Block data cannot take another receipt.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The data is not a non-empty, definite-length CBOR array of
    /// well-formed, definite-length items with nothing after it.
    #[error("custody receipt block data is malformed")]
    Malformed,
    /// The block holds version 1 receipts (text strings), and a block holds
    /// one version only.
    #[error("custody receipt block holds version 1 receipts")]
    Version1,
    /// The block mixes receipt versions.
    #[error("custody receipt block mixes receipt versions")]
    Mixed,
    /// The block already holds [`MAX_RECEIPTS`] receipts.
    #[error("custody receipt block is full")]
    Full,
}

/// Why a receipt does not verify.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The item is not a COSE_Sign1 with an attached payload.
    #[error("not a COSE_Sign1")]
    NotCoseSign1,
    /// The protected header does not name EdDSA.
    #[error("unsupported algorithm")]
    UnsupportedAlg,
    /// The signature does not verify under the key.
    #[error("signature invalid")]
    SignatureInvalid,
}

pub type Result<T> = core::result::Result<T, Error>;

/// The BPv7 bundle identity a receipt names (RFC 9171 §4.3.1 and §5.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleIdentity<'a> {
    /// The source EID, in its text form.
    pub source: &'a str,
    /// The creation time in DTN milliseconds, 0 for a source without a clock.
    pub creation_time: u64,
    /// The creation sequence number.
    pub sequence_number: u64,
    /// The fragment offset and total ADU length, for a fragment.
    pub fragment: Option<(u64, u64)>,
}

/// The fields of one version 2 receipt (draft Section 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fields<'a> {
    /// Key 1: the EID of the node the custodian received the bundle from.
    pub prev_hop: &'a str,
    /// Key 2: the EID of the custodian.
    pub this_hop: &'a str,
    /// Key 3: the bundle (or fragment) as held.
    pub bundle: BundleIdentity<'a>,
    /// Key 4: SHA-256 of the payload block's block-type-specific data held.
    pub payload_hash: [u8; 32],
    /// Key 5: reception time, DTN milliseconds.
    pub received_at: u64,
    /// Key 6: forwarding time, DTN milliseconds.
    pub forwarded_at: u64,
    /// Key 7: an optional application identifier.
    pub subject: Option<&'a str>,
    /// Key 8: the RFC 9171 Hop Count as received and as released, so a
    /// verifier can size an unsigned stretch between two custodians.
    pub hop_count: Option<(u64, u64)>,
    /// Key 9: SHA-256 of the CBOR array of the receipts already in the block,
    /// which commits this receipt to every earlier one.
    pub chain: Option<[u8; 32]>,
}

impl Fields<'_> {
    /// The deterministic CBOR encoding of the receipt payload map, keys in
    /// ascending order.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        let optional = [self.subject.is_some(), self.hop_count.is_some(), self.chain.is_some()];
        head(&mut out, 5, 6 + optional.iter().filter(|x| **x).count() as u64);
        head(&mut out, 0, 1);
        text(&mut out, self.prev_hop);
        head(&mut out, 0, 2);
        text(&mut out, self.this_hop);
        head(&mut out, 0, 3);
        let id = &self.bundle;
        head(&mut out, 4, if id.fragment.is_some() { 4 } else { 2 });
        text(&mut out, id.source);
        head(&mut out, 4, 2);
        head(&mut out, 0, id.creation_time);
        head(&mut out, 0, id.sequence_number);
        if let Some((offset, total)) = id.fragment {
            head(&mut out, 0, offset);
            head(&mut out, 0, total);
        }
        head(&mut out, 0, 4);
        bytes(&mut out, &self.payload_hash);
        head(&mut out, 0, 5);
        head(&mut out, 0, self.received_at);
        head(&mut out, 0, 6);
        head(&mut out, 0, self.forwarded_at);
        if let Some(subject) = self.subject {
            head(&mut out, 0, 7);
            text(&mut out, subject);
        }
        if let Some((received, released)) = self.hop_count {
            head(&mut out, 0, 8);
            head(&mut out, 4, 2);
            head(&mut out, 0, received);
            head(&mut out, 0, released);
        }
        if let Some(chain) = &self.chain {
            head(&mut out, 0, 9);
            bytes(&mut out, chain);
        }
        out
    }
}

/// Signs `payload` as an untagged COSE_Sign1 and returns its CBOR encoding.
#[must_use]
pub fn sign(key: &SigningKey, payload: &[u8]) -> Vec<u8> {
    let signature = key.sign(&to_be_signed(&PROTECTED, payload));
    let mut out = Vec::with_capacity(payload.len() + 80);
    head(&mut out, 4, 4);
    bytes(&mut out, &PROTECTED);
    head(&mut out, 5, 0);
    bytes(&mut out, payload);
    bytes(&mut out, &signature.to_bytes());
    out
}

/// Verifies one encoded COSE_Sign1 receipt under `key`, returning its
/// payload bytes.
///
/// # Errors
///
/// A [`VerifyError`] naming why the receipt does not verify.
pub fn verify<'a>(
    key: &VerifyingKey,
    item: &'a [u8],
) -> core::result::Result<&'a [u8], VerifyError> {
    let mut cursor = Cursor::new(item);
    let not_cose = |_| VerifyError::NotCoseSign1;
    if cursor.head().map_err(not_cose)? != (4, 4) {
        return Err(VerifyError::NotCoseSign1);
    }
    let protected = cursor.byte_string().map_err(not_cose)?;
    if cursor.head().map_err(not_cose)?.0 != 5 {
        return Err(VerifyError::NotCoseSign1);
    }
    cursor.backtrack_and_skip().map_err(not_cose)?;
    let payload = cursor.byte_string().map_err(not_cose)?;
    let signature = cursor.byte_string().map_err(not_cose)?;
    if !cursor.is_empty() {
        return Err(VerifyError::NotCoseSign1);
    }
    if protected != PROTECTED {
        return Err(VerifyError::UnsupportedAlg);
    }
    let signature = Signature::from_slice(signature).map_err(|_| VerifyError::NotCoseSign1)?;
    key.verify(&to_be_signed(protected, payload), &signature)
        .map_err(|_| VerifyError::SignatureInvalid)?;
    Ok(payload)
}

/// Splits Custody Receipt Block data into its receipts' encoded bytes,
/// checking that every item is a version 2 receipt shape.
///
/// # Errors
///
/// [`Error::Malformed`], [`Error::Version1`] or [`Error::Mixed`].
pub fn items(data: &[u8]) -> Result<Vec<&[u8]>> {
    let mut cursor = Cursor::new(data);
    let (major, count) = cursor.head()?;
    if major != 4 || count == 0 {
        return Err(Error::Malformed);
    }
    let mut items = Vec::new();
    let (mut arrays, mut texts) = (0u64, 0u64);
    for _ in 0..count {
        let start = cursor.offset;
        match cursor.peek_major()? {
            4 => arrays += 1,
            3 => texts += 1,
            _ => return Err(Error::Mixed),
        }
        cursor.skip(0)?;
        items.push(&data[start..cursor.offset]);
    }
    if !cursor.is_empty() {
        return Err(Error::Malformed);
    }
    match (arrays, texts) {
        (_, 0) => Ok(items),
        (0, _) => Err(Error::Version1),
        _ => Err(Error::Mixed),
    }
}

/// The block data for a new Custody Receipt Block holding one receipt.
#[must_use]
pub fn new_block(receipt: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(receipt.len() + 1);
    head(&mut out, 4, 1);
    out.extend_from_slice(receipt);
    out
}

/// Appends `receipt` to existing block data, copying every earlier
/// receipt's bytes unchanged.
///
/// # Errors
///
/// Any [`items`] error, or [`Error::Full`].
pub fn append(data: &[u8], receipt: &[u8]) -> Result<Vec<u8>> {
    let items = items(data)?;
    if items.len() >= MAX_RECEIPTS {
        return Err(Error::Full);
    }
    let mut out = Vec::with_capacity(data.len() + receipt.len() + 1);
    head(&mut out, 4, items.len() as u64 + 1);
    for item in items {
        out.extend_from_slice(item);
    }
    out.extend_from_slice(receipt);
    Ok(out)
}

// The COSE Sig_structure: ["Signature1", protected, external_aad (empty), payload].
fn to_be_signed(protected: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 24);
    head(&mut out, 4, 4);
    text(&mut out, "Signature1");
    bytes(&mut out, protected);
    bytes(&mut out, &[]);
    bytes(&mut out, payload);
    out
}

// A shortest-form CBOR head (RFC 8949 §4.2.1).
fn head(out: &mut Vec<u8>, major: u8, arg: u64) {
    let m = major << 5;
    if arg < 24 {
        out.push(m | arg as u8);
    } else if let Ok(v) = u8::try_from(arg) {
        out.extend_from_slice(&[m | 24, v]);
    } else if let Ok(v) = u16::try_from(arg) {
        out.push(m | 25);
        out.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = u32::try_from(arg) {
        out.push(m | 26);
        out.extend_from_slice(&v.to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&arg.to_be_bytes());
    }
}

fn bytes(out: &mut Vec<u8>, b: &[u8]) {
    head(out, 2, b.len() as u64);
    out.extend_from_slice(b);
}

fn text(out: &mut Vec<u8>, s: &str) {
    head(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

// A reader over definite-length CBOR, enough to find item boundaries and to
// read the byte strings of a COSE_Sign1.
struct Cursor<'a> {
    data: &'a [u8],
    offset: usize,
    last_head: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            offset: 0,
            last_head: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.offset == self.data.len()
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8]> {
        let remaining = (self.data.len() - self.offset) as u64;
        if n > remaining {
            return Err(Error::Malformed);
        }
        let n = usize::try_from(n).map_err(|_| Error::Malformed)?;
        let slice = &self.data[self.offset..self.offset + n];
        self.offset += n;
        Ok(slice)
    }

    fn peek_major(&self) -> Result<u8> {
        self.data
            .get(self.offset)
            .map(|b| b >> 5)
            .ok_or(Error::Malformed)
    }

    // Reads one head: (major type, argument). Indefinite lengths and the
    // reserved additional-information values are refused.
    fn head(&mut self) -> Result<(u8, u64)> {
        self.last_head = self.offset;
        let initial = self.take(1)?[0];
        let (major, info) = (initial >> 5, initial & 0x1f);
        let arg = match info {
            0..=23 => u64::from(info),
            24 => u64::from(self.take(1)?[0]),
            25 => u64::from(u16::from_be_bytes(
                self.take(2)?.try_into().map_err(|_| Error::Malformed)?,
            )),
            26 => u64::from(u32::from_be_bytes(
                self.take(4)?.try_into().map_err(|_| Error::Malformed)?,
            )),
            27 => u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| Error::Malformed)?),
            _ => return Err(Error::Malformed),
        };
        Ok((major, arg))
    }

    fn backtrack_and_skip(&mut self) -> Result<()> {
        self.offset = self.last_head;
        self.skip(0)
    }

    fn byte_string(&mut self) -> Result<&'a [u8]> {
        match self.head()? {
            (2, len) => self.take(len),
            _ => Err(Error::Malformed),
        }
    }

    fn skip(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::Malformed);
        }
        let (major, arg) = self.head()?;
        match major {
            0 | 1 | 7 => Ok(()),
            2 | 3 => self.take(arg).map(|_| ()),
            4 => (0..arg).try_for_each(|_| self.skip(depth + 1)),
            5 => (0..arg).try_for_each(|_| {
                self.skip(depth + 1)?;
                self.skip(depth + 1)
            }),
            _ => self.skip(depth + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::{TryRng, rngs::SysRng};

    use super::*;

    fn key() -> SigningKey {
        let mut seed = [0u8; 32];
        SysRng.try_fill_bytes(&mut seed).unwrap();
        SigningKey::from_bytes(&seed)
    }

    fn signed(key: &SigningKey, this_hop: &str) -> Vec<u8> {
        let fields = Fields {
            prev_hop: "ipn:1.0",
            this_hop,
            bundle: BundleIdentity {
                source: "ipn:1.1",
                creation_time: 0,
                sequence_number: 3,
                fragment: None,
            },
            payload_hash: [7; 32],
            received_at: 1,
            forwarded_at: 2,
            subject: None,
            hop_count: None,
            chain: None,
        };
        sign(key, &fields.encode())
    }

    #[test]
    fn append_copies_earlier_receipts_unchanged() {
        let (a, b) = (key(), key());
        let first = signed(&a, "ipn:10.0");
        let second = signed(&b, "ipn:20.0");
        let block = append(&new_block(&first), &second).unwrap();
        assert_eq!(block[0], 0x82);
        assert_eq!(&block[1..1 + first.len()], &first[..]);
        assert_eq!(items(&block), Ok(vec![&first[..], &second[..]]));
        assert!(verify(&a.verifying_key(), &first).is_ok());
        assert_eq!(
            verify(&a.verifying_key(), &second),
            Err(VerifyError::SignatureInvalid)
        );
    }

    #[test]
    fn version_1_and_mixed_blocks_are_refused() {
        // ["x"], [["..."], "x"], 5, [] and a trailing byte.
        assert_eq!(append(&[0x81, 0x61, b'x'], &[0x80]), Err(Error::Version1));
        let r = signed(&key(), "ipn:20.0");
        let mut mixed = vec![0x82];
        mixed.extend_from_slice(&r);
        mixed.extend_from_slice(&[0x61, b'x']);
        assert_eq!(append(&mixed, &r), Err(Error::Mixed));
        assert_eq!(append(&[0x05], &r), Err(Error::Malformed));
        assert_eq!(append(&[0x80], &r), Err(Error::Malformed));
        let mut trailing = new_block(&r);
        trailing.push(0x00);
        assert_eq!(append(&trailing, &r), Err(Error::Malformed));
        assert_eq!(
            append(&new_block(&r[..r.len() - 1]), &r),
            Err(Error::Malformed)
        );
    }

    #[test]
    fn a_full_block_is_refused() {
        let r = signed(&key(), "ipn:20.0");
        let mut block = new_block(&r);
        for _ in 1..MAX_RECEIPTS {
            block = append(&block, &r).unwrap();
        }
        assert_eq!(items(&block).unwrap().len(), MAX_RECEIPTS);
        assert_eq!(append(&block, &r), Err(Error::Full));
    }
}
