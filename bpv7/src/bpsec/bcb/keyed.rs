//! The keyed BCB operations, with the `bpsec` feature: the decryptor, the
//! decryption and encryption of an operation's target and the key it
//! requires.

use alloc::boxed::Box;

#[cfg(feature = "rfc9173")]
use crate::bpsec::rfc9173;
use crate::{
    bpsec::{Error, key},
    eid,
};

use super::{Operation, OperationArgs, OperationSet};

/// Incremental decryption of one BCB operation's target as its data
/// streams past: the decrypt stage of a security acceptor's drain.
/// Context-dispatching wrapper over the per-context decryptors; begun by
/// [`Operation::begin_decrypt`], which resolves the key before the first
/// byte.
///
/// Owns everything it needs, so it is `Send` and may cross `await` points.
/// That includes keyed state the context cannot avoid: see the
/// per-context decryptor's documentation for what it holds and how it is
/// zeroized. Under BCB-AES-GCM it collects the ciphertext and decrypts it
/// whole at [`finish`](Self::finish); an incremental AES-GCM, which checks
/// the tag in constant time before releasing any plaintext, can replace
/// that without changing this interface.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
#[must_use = "an unfinished decryptor is unauthenticated ciphertext — call finish()"]
pub enum Decryptor {
    /// AES-GCM incremental decryption (RFC 9173).
    #[cfg(feature = "rfc9173")]
    AES_GCM(rfc9173::bcb_aes_gcm::Decryptor),
}

impl Decryptor {
    /// The length of the plaintext [`finish`](Self::finish) returns, known
    /// before the first byte: the framing of the target's plaintext form.
    pub fn plaintext_len(&self) -> u64 {
        // Match on the place (`*self`), not the reference: with no security
        // context compiled in the enum is empty, and an empty enum's place
        // needs no arms, where a reference to it still needs one.
        match *self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(ref d) => d.plaintext_len(),
        }
    }

    /// Absorb the next run of the target's block-type-specific data, as
    /// received.
    #[cfg_attr(not(feature = "rfc9173"), allow(unused_variables))]
    pub fn update(&mut self, bytes: &[u8]) {
        match *self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(ref mut d) => d.update(bytes),
        }
    }

    /// Settle the operation once every byte of the target's data has been
    /// absorbed: the plaintext, authenticated, zeroized on drop. Fails with
    /// [`Error::DecryptionFailed`] when the ciphertext does not
    /// authenticate or its length is not the target's.
    pub fn finish(self) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error> {
        // By-value match: with no security context compiled in the enum is
        // empty, so the match is exhaustive with no arms.
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(d) => d.finish(),
        }
    }
}

impl Operation {
    /// Returns true if multiple security operations can share the same security
    /// context parameters (and thus be in the same BCB).
    ///
    /// BCB-AES-GCM (RFC 9173) returns false because each encryption requires a
    /// unique IV, which is stored in the context parameters.
    ///
    /// Future contexts (e.g., COSE-based) may return true if they store per-target
    /// IVs in the results rather than shared parameters. The operation instance
    /// can inspect its parameters to determine this.
    fn can_share(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(_) => false, // IV in context parameters requires separate blocks
            // Unknown contexts: assume they cannot share (conservative default)
            Self::Unrecognised(..) => false,
        }
    }

    /// Decrypts a fully-resident target block using the provided key
    /// source: the all-in-one counterpart to [`begin_decrypt`](Self::begin_decrypt),
    /// which it feeds the target's whole data. Fails as `begin_decrypt` and
    /// [`Decryptor::finish`] fail, and with [`Error::MissingSecurityTarget`]
    /// when the target's data is not resident.
    pub fn decrypt<K>(
        &self,
        key_source: &K,
        args: OperationArgs,
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error>
    where
        K: key::KeySource + ?Sized,
    {
        let data = args
            .blocks
            .block(args.target)
            .ok_or(Error::MissingSecurityTarget)?
            .1
            .available()
            .ok_or(Error::MissingSecurityTarget)?;
        let mut decryptor = self.begin_decrypt(key_source, &args, data.len() as u64)?;
        decryptor.update(data.as_ref());
        decryptor.finish()
    }

    /// Begin decrypting this operation's target as its block-type-specific
    /// data streams past, against bundle headers that need not hold it: the
    /// returned [`Decryptor`] absorbs the data through [`Decryptor::update`]
    /// and returns the authenticated plaintext at [`Decryptor::finish`],
    /// [`plaintext_len`](Decryptor::plaintext_len) long; the all-in-one
    /// [`decrypt`](Self::decrypt) feeds it a resident target's whole data.
    /// `target_len` is the length of the target's received data. Everything that needs
    /// the key happens here, before the first byte, so a caller learns now
    /// whether it can process the target.
    ///
    /// # Errors
    ///
    /// [`Error::NoKey`] when `key_source` holds no key for the operation;
    /// [`Error::UnrecognisedContext`] or [`Error::UnsupportedOperation`] for
    /// a context or variant this node does not support;
    /// [`Error::DecryptionFailed`] for a key that cannot serve, or
    /// data too short to carry the authentication tag it should.
    #[cfg_attr(not(feature = "rfc9173"), allow(unused_variables))]
    pub fn begin_decrypt<K>(
        &self,
        key_source: &K,
        args: &OperationArgs,
        target_len: u64,
    ) -> Result<Decryptor, Error>
    where
        K: key::KeySource + ?Sized,
    {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(op) => op
                .begin_decrypt(key_source, args, target_len)
                .map(Decryptor::AES_GCM),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    /// Whether `key_source` holds a key this operation can decrypt with, for
    /// the security source `source`, without decrypting: the check of
    /// an acceptor that must not take a bundle it cannot process.
    ///
    /// # Errors
    ///
    /// [`Error::NoKey`] when it holds none, and
    /// [`Error::UnrecognisedContext`] for a context this node does not
    /// implement. A key that is present but wrong still fails at
    /// [`decrypt`](Self::decrypt).
    #[allow(unused_variables)]
    pub fn require_key<K>(&self, key_source: &K, source: &eid::Eid) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(op) => op.require_key(key_source, source),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    /// Encrypts the target block using this operation's context parameters
    /// (e.g. AES-GCM scope flags), looking up the key on the bpsec source
    /// EID via `key_source`. The target's plaintext is read from
    /// `args.blocks.block(args.target)`. Returns a fresh operation carrying
    /// the newly generated per-context state (e.g. a new AES-GCM IV) along
    /// with the ciphertext; `self` is left untouched so the caller can
    /// decide whether to replace the original entry.
    ///
    /// Works equally on an operation just constructed in memory (with
    /// caller-chosen context parameters) and on one parsed from a wire
    /// BCB — the method only inspects this operation's parameters and
    /// the bytes in `args.blocks`.
    #[allow(unused_variables)]
    pub fn encrypt<K>(
        &self,
        key_source: &K,
        args: OperationArgs,
    ) -> Result<(Self, Box<[u8]>), Error>
    where
        K: key::KeySource + ?Sized,
    {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(op) => {
                let key = key_source
                    .key(args.source, &[key::Operation::Encrypt])
                    .ok_or(Error::NoKey)?;
                let (new_op, ciphertext) = rfc9173::bcb_aes_gcm::Operation::encrypt(
                    key,
                    op.parameters.flags.clone(),
                    args,
                )?;
                Ok((Self::AES_GCM(new_op), ciphertext))
            }
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }
}

impl OperationSet {
    /// Returns true if this BCB's context allows multiple targets to share
    /// security context parameters.
    pub(crate) fn can_share(&self) -> bool {
        // All operations in a set share the same context, so check any one
        self.operations
            .values()
            .next()
            .is_some_and(|op| op.can_share())
    }
}
