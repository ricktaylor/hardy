//! The keyed BIB operations, with the `bpsec` feature: the verifier, the
//! check of an operation over its target and the key it requires.

#[cfg(feature = "rfc9173")]
use crate::bpsec::rfc9173;
use crate::{
    bpsec::{Error, bcb, key},
    eid,
    reader::Reader,
};

use super::{Operation, OperationArgs, OperationSet};

/// Incremental verifier for one BIB operation. Context-dispatching wrapper
/// over the per-context verifiers; the single verification engine — the
/// all-in-one [`Operation::verify`] is a thin resident-target wrapper over
/// it, and the streaming ingress drain feeds it a non-resident payload
/// target segment by segment.
///
/// Owns everything it needs, so it is `Send` and may cross `await` points
/// and task boundaries. That includes keyed state: the HMAC-SHA2 verifier
/// holds the MAC state derived from the key (see the per-context verifier
/// docs for what is copied and what is zeroized).
///
/// Contract for future security contexts: a verifier carries the *minimum
/// derived state* across the drain — a running digest, never a raw key
/// larger than the digest state. Resolve key material from the
/// [`KeySource`](crate::bpsec::key::KeySource) inside `begin_verify`'s sync scope;
/// a context that instead needs the key at settle (a hash-then-verify
/// signature scheme, say) should extend [`finish`](Self::finish) to take a
/// `KeySource` — the settle site is sync and can re-resolve — rather than
/// store the key in the verifier.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[must_use = "an unfinished verifier is an unchecked integrity statement — call finish()"]
pub enum Verifier {
    /// HMAC-SHA2 incremental verification (RFC 9173).
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2(rfc9173::bib_hmac_sha2::Verifier),
}

impl Verifier {
    /// Absorb the next run of the target's block-type-specific data.
    #[cfg_attr(not(feature = "rfc9173"), allow(unused_variables))]
    pub fn update(&mut self, bytes: &[u8]) {
        // Match on the place (`*self`), not the reference: with no security
        // context compiled in the enum is empty, and an empty enum's place
        // needs no arms, where a reference to it still needs one.
        match *self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(ref mut v) => v.update(bytes),
        }
    }

    /// Settle the operation once every byte has been absorbed. Fails with
    /// [`Error::IntegrityCheckFailed`] on tag mismatch.
    pub fn finish(self) -> Result<(), Error> {
        // By-value match: with no security context compiled in the enum is
        // empty, so the match is exhaustive with no arms.
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(v) => v.finish(),
        }
    }
}

impl Operation {
    /// Begin verifying this operation over a target whose block-type-specific
    /// data streams past, against bundle headers that need not hold it: the
    /// returned [`Verifier`] absorbs the data through [`Verifier::update`]
    /// and settles at [`Verifier::finish`]. A resident target takes the
    /// all-in-one [`verify`](Self::verify) instead. The caller feeds exactly
    /// `target_len` bytes: the received data, or the plaintext of a target
    /// this node decrypted (RFC 9172 §3.9: the BCB is evaluated first).
    ///
    /// # Errors
    ///
    /// [`Error::NoKey`] when `key_source` holds no key for the operation;
    /// [`Error::InvalidBIBTarget`] for the primary block, whose integrity
    /// input is its canonical form rather than streamed data;
    /// [`Error::UnrecognisedContext`] for a context this node does not
    /// support.
    #[cfg_attr(not(feature = "rfc9173"), allow(unused_variables))]
    pub fn begin_verify<K>(
        &self,
        key_source: &K,
        args: &OperationArgs,
        target_len: u64,
    ) -> Result<Verifier, Error>
    where
        K: key::KeySource + ?Sized,
    {
        // The streamed integrity input frames the target's raw data, which
        // is not a primary-block target's canonical form.
        if args.target == 0 {
            return Err(Error::InvalidBIBTarget);
        }
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o
                .begin_verify(key_source, args, target_len)
                .map(Verifier::HMAC_SHA2),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    /// Verifies the integrity of a fully-resident target block. The
    /// all-in-one counterpart to the crate's streaming `begin_verify`;
    /// both share the per-context IPPT/MAC primitives. The target's CRC, if
    /// it carries one, is no part of the check (RFC 9173 §3.7).
    /// [`Error::NoKey`] is the caller's policy skip.
    #[cfg_attr(not(feature = "rfc9173"), allow(unused_variables))]
    pub fn verify<K>(&self, key_source: &K, args: OperationArgs) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.verify(key_source, &args),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    /// Whether `key_source` holds a key this operation can verify with, for
    /// the security source `source`, without verifying: the check of
    /// an acceptor that must not take a bundle it cannot process.
    ///
    /// # Errors
    ///
    /// [`Error::NoKey`] when it holds none, and
    /// [`Error::UnrecognisedContext`] for a context this node does not
    /// implement. A key that is present but wrong still fails at
    /// [`verify`](Self::verify).
    #[cfg_attr(not(feature = "rfc9173"), allow(unused_variables))]
    pub fn require_key<K>(&self, key_source: &K, source: &eid::Eid) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.require_key(key_source, source),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }
}

impl OperationSet {
    /// The checks a BCB-encrypted BIB gets once decrypted, by whichever
    /// operation decrypted it: [`check`](Self::check) on its targets, and
    /// RFC 9172 §3.8's rule that `bcb`, the BCB that encrypted it, must
    /// share a target with it when its security context lets one BCB
    /// encrypt several targets (BCB-AES-GCM cannot).
    ///
    /// # Panics
    ///
    /// As [`check`](Self::check).
    pub(crate) fn check_decrypted<'a, B>(
        &self,
        bib_block_number: u64,
        bcb: &bcb::OperationSet,
        blocks: &'a B,
    ) -> Result<(), Error>
    where
        B: Reader<'a> + ?Sized,
    {
        self.check(bib_block_number, blocks)?;
        if bcb.can_share()
            && !self
                .operations
                .keys()
                .any(|target| bcb.operations.contains_key(target))
        {
            return Err(Error::BCBMustShareTarget);
        }
        Ok(())
    }
}
