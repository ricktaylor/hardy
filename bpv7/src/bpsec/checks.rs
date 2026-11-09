/*!
The keyed BPSec check over a parsed bundle, a method on [`Parsed`]: the
security operations in this node's roles on each block, the acceptor's
decryption, the verifier's decryption of its own block and the integrity
checks in one pass ([`Parsed::check`]), with what it reports ([`Facts`]).
The keyless classification of the blocks this node cannot process is the
parser's ([`Classification`](crate::parse::Classification)).

The check is policy-free: it produces facts (decrypt/verify outcomes,
coverage stamps) and applies no policy of its own. Consumers — the BPA
ingress pipeline, the bpv7 CLI tools — name their roles and layer their own
policy on top. The structural parser lives in [`crate::parse`]; rewrite
application in [`crate::rewrite`].

The §A–§E pipeline the parser and this check implement — what each `A1` /
`A2` / `A3` / `B` / `B6` / `C7` / `C8` / `D` / `E` label means — is
documented in `bpv7/docs/parser_design.md`.
*/

use alloc::boxed::Box;

use hardy_cbor::decode::FromCbor;
use smallvec::SmallVec;
use zeroize::Zeroizing;

use crate::{
    Error, HashMap, HashSet, block,
    bpsec::{self, Role, Roles, SecurityService},
    eid::Eid,
    error::CaptureFieldErr,
    parse::Parsed,
    reader::{Availability, PlainReader, Reader},
};

// Whether `block` is one every forwarder replaces (RFC 9171 §5.4), a
// Previous Node or Bundle Age block, which no valid bundle has under an
// encrypted BIB. Compared by encoding, so an alias of either type counts.
fn always_replaced(block: &block::Block) -> bool {
    block.block_type == block::Type::PreviousNode || block.block_type == block::Type::BundleAge
}

// A reader over the bundle's resident bytes that serves a BCB-covered
// block's plaintext from the arrival's cache, where an earlier pass
// decrypted it.
struct CachedReader<'a> {
    blocks: &'a HashMap<u64, block::Block>,
    source_data: &'a [u8],
    decrypted: &'a HashMap<u64, Zeroizing<Box<[u8]>>>,
}

impl<'a> Reader<'a> for CachedReader<'a> {
    fn block(&'a self, block_number: u64) -> Option<(&'a block::Block, Availability<'a>)> {
        let block = self.blocks.get(&block_number)?;
        let payload = match self.decrypted.get(&block_number) {
            Some(plaintext) => Some(plaintext.as_ref()),
            None => block.payload(self.source_data),
        };
        Some((
            block,
            payload
                .map(block::Payload::Borrowed)
                .map_or(Availability::NotResident, Availability::Available),
        ))
    }

    fn block_header(&'a self, block_number: u64) -> Option<&'a block::Block> {
        self.blocks.get(&block_number)
    }
}

fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, Error>
where
    T: FromCbor,
    T::Error: From<hardy_cbor::decode::Error>,
    Error: From<T::Error>,
{
    hardy_cbor::decode::parse_exact::<T>(data).map_field_err(field)
}

/// What the keyed checks found, for the caller's policy: one [`Outcome`]
/// per security operation they met, and the blocks whose acceptance could
/// not be completed.
#[derive(Debug, Default)]
pub struct Facts {
    /// One entry per security operation met: the operation's security block
    /// and target, its service, and what the check made of it.
    pub outcomes: SmallVec<[Outcome; 8]>,
    /// Blocks whose acceptance [`Parsed::check`] could not complete: coupled,
    /// through an encrypted BIB, to a block this node does not accept, or
    /// whose BCB operation it could not process (no key, ciphertext that did
    /// not authenticate, an unsupported context); or under an encrypted BIB
    /// this node cannot read that may cover the block
    /// ([`Maybe`](block::BibCoverage::Maybe)). Nothing coupled to such a
    /// block is decrypted on its account.
    pub incomplete: SmallVec<[u64; 4]>,
}

/// What a check made of one security operation (RFC 9172 §1.1: a security
/// service applied to a target), named by the security block that carries
/// it, its target and its service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// The BIB or BCB that carries the operation.
    pub security_block: u64,
    /// The block the operation protects.
    pub target: u64,
    /// The service the operation applies.
    pub service: SecurityService,
    /// What the check made of the operation.
    pub checked: Checked,
}

/// What a check made of a security operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// The integrity check passed over the target's plaintext, or the
    /// ciphertext authenticated and its plaintext is cached.
    Verified,
    /// The integrity check failed, or the ciphertext did not authenticate
    /// (RFC 9172 §5.1.1, §5.1.2).
    Failed,
    /// This node holds no key for the operation.
    NoKey,
    /// A security context this node does not support: the operation was not
    /// processed.
    Unsupported,
    /// The payload's operation, not checked here, its key held: the
    /// payload's acceptor checks it as the payload streams, or, under a
    /// BCB, once it has decrypted it (RFC 9172 §3.9). Or an operation on a
    /// fragment, other than a per-hop block's, checked once the bundle is
    /// reassembled (RFC 9172 §5.2), no key looked up.
    Deferred,
    /// An integrity operation on a Previous Node or Bundle Age block inside
    /// an encrypted BIB [`Parsed::check`] read: a combination no valid bundle
    /// carries, since only the forwarder that inserted the block could have
    /// sourced an operation on it, and RFC 9172 §3.9 forbids it to sign and
    /// then encrypt one target. The block's acceptor, the next node, cannot
    /// accept it.
    Conflicting,
    /// An encrypted BIB this node cannot read that the bundle's structure
    /// proves covers this Previous Node or Bundle Age block: an operation
    /// no valid bundle carries, which the block's acceptor cannot accept
    /// without the BIB's key.
    PossiblyConflicting,
}

impl Facts {
    /// Whether any operation met was `checked`.
    pub fn any(&self, checked: Checked) -> bool {
        self.outcomes.iter().any(|o| o.checked == checked)
    }

    /// The targets of the operations `checked`, of either service, a target
    /// once for each of its operations.
    pub fn targets(&self, checked: Checked) -> impl Iterator<Item = u64> + '_ {
        self.outcomes
            .iter()
            .filter(move |o| o.checked == checked)
            .map(|o| o.target)
    }

    /// The targets of the operations of `service` that were `checked`, a
    /// target once for each of its operations.
    pub fn targets_of(
        &self,
        service: SecurityService,
        checked: Checked,
    ) -> impl Iterator<Item = u64> + '_ {
        self.outcomes
            .iter()
            .filter(move |o| o.service == service && o.checked == checked)
            .map(|o| o.target)
    }

    // Whether the operation has an outcome already.
    fn met(&self, security_block: u64, target: u64, service: SecurityService) -> bool {
        self.outcomes.iter().any(|o| {
            o.security_block == security_block && o.target == target && o.service == service
        })
    }

    // Records what a check made of one operation. An operation with an
    // outcome already keeps it: the first finding stands.
    fn record(
        &mut self,
        security_block: u64,
        target: u64,
        service: SecurityService,
        checked: Checked,
    ) {
        if !self.met(security_block, target, service) {
            self.outcomes.push(Outcome {
                security_block,
                target,
                service,
                checked,
            });
        }
    }

    // Records a block whose acceptance cannot be completed, once.
    fn incomplete(&mut self, block_number: u64) {
        if !self.incomplete.contains(&block_number) {
            self.incomplete.push(block_number);
        }
    }
}

impl Parsed {
    /// The keyed BPSec checks, in this node's roles on each block (RFC 9172
    /// §1.4): `roles` names its roles by block number, and `others` is its
    /// roles on every block the map does not name, [`Roles::default`] for
    /// none. One pass, in the one order that works, since the integrity
    /// checks read the plaintext the decryptions cache, each security
    /// operation met recorded as one [`Outcome`]:
    ///
    /// 1. The acceptor's confidentiality processing of each block whose
    ///    confidentiality role is [`Acceptor`](Role::Acceptor) (RFC 9172
    ///    §5.1.1), short of removing the operations: its BCB operation is
    ///    decrypted into the cache ([`decrypted`](Self::decrypted)).
    ///    Accepting a block's BCB operation takes with it the BCB operations
    ///    on the encrypted BIB that covers the block and on that BIB's other
    ///    targets, in whichever BCBs hold them, all or none (RFC 9172 §3.9),
    ///    and accepting an encrypted BIB's BCB operation takes its targets'.
    ///    Each of those targets must be one this node accepts too, and each
    ///    of those operations one it can process: a block coupled to one it
    ///    does not accept, or whose operation finds no key, fails to
    ///    authenticate or is in an unsupported context, is `incomplete`,
    ///    that operation's own outcome recorded, and nothing coupled to it
    ///    is decrypted on its account. So is a block under an encrypted BIB
    ///    this node cannot read that may cover it
    ///    ([`Maybe`](block::BibCoverage::Maybe)). An encrypted BIB is
    ///    decrypted by necessity, when an accepted block is BCB-covered
    ///    beside one that may cover it: one under a BCB from the same
    ///    security source as the block's own, since the node that encrypts
    ///    a signed block encrypts its BIB too (RFC 9172 §3.9), so no BIB
    ///    under another source's BCB covers it in a valid bundle. The
    ///    coverage it reveals is stamped on the parse's blocks, and one
    ///    whose ciphertext does not authenticate is recorded `Failed`. None
    ///    is read on a Previous Node's or Bundle Age's account, since no
    ///    valid bundle has an encrypted BIB over a block every forwarder
    ///    replaces (RFC 9171 §5.4); the Hop Count, which may validly be
    ///    under one, reads its own source's. An integrity operation on a
    ///    Previous Node or Bundle Age block among them inside an encrypted
    ///    BIB is a combination no valid bundle carries, which the block's
    ///    acceptor cannot accept: one this pass decrypted is recorded
    ///    `Conflicting`, and one the bundle's structure proves an unread BIB
    ///    to carry, `PossiblyConflicting` ([`Checked`]). The Hop Count is
    ///    exempt, its increment being a SHOULD (RFC 9171 §4.4.3), and one
    ///    whose acceptance cannot be completed is read all the same, as a
    ///    verifier reads it below, so the caller can hold the hop limit.
    /// 2. The verifier's check of the ciphertext of each BCB-covered block
    ///    whose confidentiality role is [`Verifier`](Role::Verifier): the
    ///    block is decrypted under `key_source`, which checks the
    ///    ciphertext's tag, and its plaintext goes into the cache and
    ///    nowhere else, the block travelling as received.
    /// 3. The verifier's integrity checks: every BIB operation on a block
    ///    with an integrity role, either role, since the acceptor and the
    ///    verifier compute the same check and the role decides the removal,
    ///    the caller's. A target is checked where it is resident and in
    ///    plaintext, or where the cache holds its plaintext; one under a BCB
    ///    the cache does not hold is passed over, since RFC 9172 §3.9
    ///    forbids checking a BIB against ciphertext and the BCB's acceptor,
    ///    which decrypts it, is the one to check it. An operation in a
    ///    security context this node does not support is recorded
    ///    `Unsupported`, never checked.
    ///
    /// The payload's data need not be resident, so its own operations are
    /// not processed here: its acceptor decrypts it as it streams
    /// ([`bcb::Operation::begin_decrypt`](bpsec::bcb::Operation::begin_decrypt))
    /// and checks a BIB over it against the plaintext
    /// ([`bib::Operation::begin_verify`](bpsec::bib::Operation::begin_verify)).
    /// With the payload's confidentiality accepted, the key that decrypt
    /// needs is required now, and the operation recorded `Deferred` or
    /// `NoKey`, or `Unsupported` where its context is; the blocks whose BCB
    /// operations go with the payload's, the encrypted BIB over it and that
    /// BIB's other targets, are decrypted like any accepted block's coupled
    /// blocks. A BIB operation on the payload is `Deferred` or `NoKey` the
    /// same way, unless the payload is resident in plaintext, and a verifier
    /// of the payload's confidentiality is passed over, its ciphertext
    /// streaming past the caller that checks it as it arrives. A block
    /// already in the cache is not decrypted again, and one it records as
    /// failed counts as failed.
    ///
    /// On a fragment, every operation but a per-hop block's is deferred: a
    /// Previous Node, Bundle Age or Hop Count block is the forwarder's
    /// write to the fragment itself (RFC 9171 §5.4), while every other
    /// operation covers the primary block as it was before fragmentation,
    /// or a part of the payload, and is checkable only once the bundle is
    /// reassembled (RFC 9172 §5.2): each in this node's roles is recorded
    /// `Deferred`, nothing of it is decrypted, and no key is looked up for
    /// it.
    ///
    /// Returns [`Facts`], to which the caller applies its failure policy,
    /// before removing the operations it accepted
    /// ([`remove_encryption`](crate::bpsec::edit::BPSecEditor::remove_encryption)
    /// with the plaintext). Any BPSec error other than a missing key or a
    /// failed check propagates as `Err`.
    pub fn check(
        &mut self,
        key_source: &dyn bpsec::key::KeySource,
        roles: &HashMap<u64, Roles>,
        others: Roles,
    ) -> Result<Facts, Error> {
        let Parsed {
            data,
            bundle,
            bcbs: bcb_ops,
            bibs: bib_ops,
            decrypted,
            ..
        } = self;
        let data: &[u8] = data;
        let blocks = &mut bundle.blocks;
        let mut facts = Facts::default();
        let named = |n: u64| roles.get(&n).copied().unwrap_or(others);

        // On a fragment, every operation but a per-hop block's is checked
        // after reassembly: each in this node's roles is deferred, no key
        // touched, and plays no part below, where the roles name only the
        // per-hop blocks.
        let fragment = bundle.primary.id.fragment_info.is_some();
        let per_hop: HashSet<u64> = blocks
            .iter()
            .filter(|(_, b)| {
                // Compared by encoding, so an alias of a type counts.
                b.block_type == block::Type::PreviousNode
                    || b.block_type == block::Type::BundleAge
                    || b.block_type == block::Type::HopCount
            })
            .map(|(&n, _)| n)
            .collect();
        if fragment {
            for (&bcb, ops) in bcb_ops.iter() {
                for &target in ops.operations().keys() {
                    if !per_hop.contains(&target) && named(target).confidentiality.is_some() {
                        facts.record(
                            bcb,
                            target,
                            SecurityService::Confidentiality,
                            Checked::Deferred,
                        );
                    }
                }
            }
            for (&bib, ops) in bib_ops.iter() {
                for &target in ops.operations().keys() {
                    if !per_hop.contains(&target) && named(target).integrity.is_some() {
                        facts.record(bib, target, SecurityService::Integrity, Checked::Deferred);
                    }
                }
            }
        }
        let roles_on = |n: u64| {
            if fragment && !per_hop.contains(&n) {
                Roles::default()
            } else {
                named(n)
            }
        };

        let mut acceptors: HashSet<u64> = HashSet::new();
        let mut verifiers: HashSet<u64> = HashSet::new();
        for &n in blocks.keys() {
            match roles_on(n).confidentiality {
                Some(Role::Acceptor) => {
                    acceptors.insert(n);
                }
                Some(Role::Verifier) => {
                    verifiers.insert(n);
                }
                None => {}
            }
        }

        // 1. The acceptors' decryption. An encrypted BIB beside an accepted
        // BCB-covered block, the payload among them, may cover it: decrypted
        // by necessity, so the coverage read below is known, and the BIB
        // over an encrypted payload is there for its acceptor to verify
        // against the plaintext. The BIB that covers a block lies under a
        // BCB from the same security source as the block's own, since the
        // node that encrypts a signed block encrypts its BIB too (RFC 9172
        // §3.9), so only those sources' BIBs are read, and none on a
        // Previous Node's or Bundle Age's account, which no valid bundle
        // has under an encrypted BIB. One whose ciphertext does not
        // authenticate is recorded, and the coverage it hides stays
        // unknown.
        let sources: HashSet<&Eid> = acceptors
            .iter()
            .filter_map(|n| blocks.get(n))
            .filter(|b| !always_replaced(b))
            .filter_map(|b| b.bcb)
            .filter_map(|bcb| bcb_ops.get(&bcb).map(|ops| &ops.source))
            .collect();
        if !sources.is_empty() {
            let failed = decrypt_and_validate_covered_bibs(
                data,
                key_source,
                blocks,
                bcb_ops,
                bib_ops,
                &sources,
                &mut decrypted.plaintext,
            )?;
            for (bcb, bib) in failed {
                facts.record(bcb, bib, SecurityService::Confidentiality, Checked::Failed);
                decrypted.failed.insert(bib);
            }
        }
        let blocks = &*blocks;
        let bib_ops = &*bib_ops;
        per_hop_integrity_in_encrypted_bibs(blocks, &acceptors, &decrypted.plaintext, &mut facts);
        let encrypted = |n: u64| blocks.get(&n).is_some_and(|b| b.bcb.is_some());

        // The payload's BCB: its key is resolved as its decryptor will resolve
        // it, without decrypting.
        if acceptors.contains(&1)
            && let Some(bcb) = blocks.get(&1).and_then(|payload| payload.bcb)
        {
            let checked = if unsupported_bcb_operation(blocks, bcb_ops, 1) {
                Checked::Unsupported
            } else {
                let ops = bcb_ops
                    .get(&bcb)
                    .expect("BCB referenced by encrypted block must be in bcb_ops");
                match ops
                    .operations
                    .get(&1)
                    .expect("BCB protects this target")
                    .require_key(key_source, &ops.source)
                {
                    Ok(()) => Checked::Deferred,
                    Err(bpsec::Error::NoKey) => Checked::NoKey,
                    Err(e) => return Err(e.into()),
                }
            };
            facts.record(bcb, 1, SecurityService::Confidentiality, checked);
        }

        for &number in &acceptors {
            let Some(block) = blocks.get(&number) else {
                continue;
            };

            // The blocks whose BCB operations go with this one's: itself, the
            // encrypted BIB over it, and the targets of an encrypted BIB among
            // them.
            let mut coupled: SmallVec<[u64; 4]> = SmallVec::new();
            if block.bcb.is_some() {
                coupled.push(number);
            }
            match block.bib {
                block::BibCoverage::Maybe => {
                    facts.incomplete(number);
                    continue;
                }
                block::BibCoverage::Some(bib) if encrypted(bib) => coupled.push(bib),
                _ => {}
            }
            let bibs: SmallVec<[u64; 2]> = coupled
                .iter()
                .copied()
                .filter(|n| {
                    blocks
                        .get(n)
                        .is_some_and(|b| b.block_type == block::Type::BlockIntegrity)
                })
                .collect();
            for bib in bibs {
                for &target in bib_ops
                    .get(&bib)
                    .into_iter()
                    .flat_map(|ops| ops.operations.keys())
                {
                    if encrypted(target) && !coupled.contains(&target) {
                        coupled.push(target);
                    }
                }
            }
            // The BCB operations on these blocks are removed all or none, so a
            // block coupled to one this node does not accept cannot be
            // accepted. The encrypted BIB itself is no target of acceptance:
            // its operations go with its targets'.
            if coupled.iter().any(|&n| {
                n != number
                    && !acceptors.contains(&n)
                    && blocks
                        .get(&n)
                        .is_some_and(|b| b.block_type != block::Type::BlockIntegrity)
            }) {
                facts.incomplete(number);
                continue;
            }

            for &target in &coupled {
                // The payload's own BCB is its decryptor's, settled above: its
                // ciphertext streams.
                if target == 1 {
                    continue;
                }
                let bcb = blocks
                    .get(&target)
                    .and_then(|b| b.bcb)
                    .expect("a coupled block is BCB-covered");
                let checked = if unsupported_bcb_operation(blocks, bcb_ops, target) {
                    Checked::Unsupported
                } else if decrypted.plaintext.contains_key(&target) {
                    Checked::Verified
                } else if decrypted.failed.contains(&target) {
                    Checked::Failed
                } else {
                    let result = decrypt_target(
                        &CachedReader {
                            blocks,
                            source_data: data,
                            decrypted: &decrypted.plaintext,
                        },
                        key_source,
                        bcb_ops,
                        target,
                        bcb,
                    );
                    match result {
                        Ok(plaintext) => {
                            decrypted.plaintext.insert(target, plaintext);
                            Checked::Verified
                        }
                        Err(bpsec::Error::NoKey) => Checked::NoKey,
                        Err(bpsec::Error::DecryptionFailed) => {
                            decrypted.failed.insert(target);
                            Checked::Failed
                        }
                        Err(e) => return Err(e.into()),
                    }
                };
                facts.record(bcb, target, SecurityService::Confidentiality, checked);
                // A coupled operation this node could not process leaves the
                // acceptance incomplete; the block's own failure is its outcome.
                if checked != Checked::Verified && target != number {
                    facts.incomplete(number);
                }
            }
        }

        // A Hop Count whose acceptance could not be completed is read as a
        // verifier reads it, its increment being a SHOULD.
        for &n in &facts.incomplete {
            if blocks
                .get(&n)
                .is_some_and(|b| b.block_type == block::Type::HopCount && b.bcb.is_some())
            {
                verifiers.insert(n);
            }
        }

        // 2. The verifiers' decryption, the payload passed over for the
        // caller that checks its ciphertext as it streams.
        for &target in &verifiers {
            if target == 1 {
                continue;
            }
            let Some(bcb) = blocks.get(&target).and_then(|b| b.bcb) else {
                continue;
            };
            let checked = if unsupported_bcb_operation(blocks, bcb_ops, target) {
                Checked::Unsupported
            } else if decrypted.plaintext.contains_key(&target) {
                Checked::Verified
            } else if decrypted.failed.contains(&target) {
                Checked::Failed
            } else {
                let result = decrypt_target(
                    &CachedReader {
                        blocks,
                        source_data: data,
                        decrypted: &decrypted.plaintext,
                    },
                    key_source,
                    bcb_ops,
                    target,
                    bcb,
                );
                match result {
                    Ok(plaintext) => {
                        decrypted.plaintext.insert(target, plaintext);
                        Checked::Verified
                    }
                    Err(bpsec::Error::NoKey) => Checked::NoKey,
                    Err(bpsec::Error::DecryptionFailed) => {
                        decrypted.failed.insert(target);
                        Checked::Failed
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            facts.record(bcb, target, SecurityService::Confidentiality, checked);
        }

        // 3. The integrity checks, over the plaintext just cached.
        let reader = CachedReader {
            blocks,
            source_data: data,
            decrypted: &decrypted.plaintext,
        };
        for (&bib_block_number, ops) in bib_ops {
            for (&target_number, op) in &ops.operations {
                if roles_on(target_number).integrity.is_none() {
                    continue;
                }
                let target_block = blocks
                    .get(&target_number)
                    .expect("OperationSet::check verified targets exist");
                let cached = decrypted.plaintext.contains_key(&target_number);
                let checked = if op.unsupported_error().is_some() {
                    Checked::Unsupported
                } else if target_number == 1
                    && (target_block.bcb.is_some() || target_block.payload(data).is_none())
                {
                    match op.require_key(key_source, &ops.source) {
                        Ok(()) => Checked::Deferred,
                        Err(bpsec::Error::NoKey) => Checked::NoKey,
                        Err(e) => return Err(e.into()),
                    }
                } else if target_block.bcb.is_some() && !cached {
                    continue;
                } else {
                    match op.verify(
                        key_source,
                        bpsec::bib::OperationArgs {
                            source: &ops.source,
                            target: target_number,
                            security_block: bib_block_number,
                            blocks: &reader,
                        },
                    ) {
                        Ok(()) => Checked::Verified,
                        Err(bpsec::Error::NoKey) => Checked::NoKey,
                        Err(bpsec::Error::IntegrityCheckFailed) => Checked::Failed,
                        Err(e) => return Err(e.into()),
                    }
                };
                facts.record(
                    bib_block_number,
                    target_number,
                    SecurityService::Integrity,
                    checked,
                );
            }
        }
        Ok(facts)
    }
}

// ===== Section B — decrypt-and-validate BCB-encrypted BIBs =====

/// §B: For every BCB-encrypted BIB under a BCB from one of `sources`, the
/// security sources of the BCBs over the accepted blocks, decrypt the body,
/// parse the `bib::OperationSet`, run §3.8 / §3.9 structural checks, stamp
/// BIB coverage on every target, stash the plaintext into `decrypted`, and
/// insert the freshly-decoded OperationSet into `bib_ops`.
///
/// Returns the BIBs whose ciphertext did not authenticate
/// (`DecryptionFailed`, RFC 9172 §5.1.1), each with the BCB over it. The
/// caller applies the failure policy: for a Verifier, failure-drop
/// (schedule for removal); for a strict acceptor, reject the bundle.
///
/// A BIB already in `decrypted` (decrypted by an earlier pass over the same
/// parse) is not decrypted again. One under a BCB from a source not in
/// `sources` is left unread, since the BIB that covers a block lies under a
/// BCB from the same security source as the block's own (RFC 9172 §3.9).
/// `NoKey` on any individual BIB is a soft skip (loop continues; that BIB's
/// `BibCoverage::Maybe` markers are left in place), as is an unsupported
/// BCB operation over it, which the parser's classification has recorded
/// ([`Classification`](crate::parse::Classification)). Any non-NoKey,
/// non-`DecryptionFailed` BPSec error fails the whole call. When every
/// covered BIB was read and authenticated, residual `BibCoverage::Maybe`
/// markers are collapsed to `None` before returning.
fn decrypt_and_validate_covered_bibs(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    blocks: &mut HashMap<u64, block::Block>,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
    sources: &HashSet<&Eid>,
    decrypted: &mut HashMap<u64, Zeroizing<Box<[u8]>>>,
) -> Result<SmallVec<[(u64, u64); 4]>, Error> {
    let mut unread = false;
    let mut failed: SmallVec<[(u64, u64); 4]> = SmallVec::new();

    // Snapshot the encrypted-BIB block-number list up front so the loop
    // body can mutate `blocks` (coverage stamping) without conflicting
    // with the iteration.
    let encrypted_bibs: SmallVec<[u64; 4]> = blocks
        .iter()
        .filter_map(|(&n, b)| {
            (matches!(b.block_type, block::Type::BlockIntegrity) && b.bcb.is_some()).then_some(n)
        })
        .collect();

    for bib_block_number in encrypted_bibs {
        if decrypted.contains_key(&bib_block_number) {
            continue;
        }
        let bcb_block_number = blocks
            .get(&bib_block_number)
            .expect("encrypted BIB filtered above")
            .bcb
            .expect("encrypted BIB filtered above");

        let bcb_op_set = bcb_ops
            .get(&bcb_block_number)
            .expect("BCB referenced by an encrypted BIB must be in bcb_ops");
        // A BIB under another source's BCB covers no accepted block in a
        // valid bundle: left unread, as one this node holds no key for is.
        if !sources.contains(&bcb_op_set.source) {
            unread = true;
            continue;
        }

        // Scope the reader so its `blocks` borrow ends before we mutate
        // for coverage stamping.
        let plaintext = {
            let block_set = CachedReader {
                blocks,
                source_data: data,
                decrypted,
            };
            let bcb_op = bcb_op_set
                .operations
                .get(&bib_block_number)
                .expect("BCB-protected BIB must have a BCB op for itself");
            // An unsupported security context leaves the BIB opaque, as a
            // missing key does: the BCB is the classification's.
            if bcb_op.is_unsupported() {
                unread = true;
                continue;
            }
            match bcb_op.decrypt(
                key_source,
                bpsec::bcb::OperationArgs {
                    source: &bcb_op_set.source,
                    target: bib_block_number,
                    security_block: bcb_block_number,
                    blocks: &block_set,
                },
            ) {
                Ok(p) => p,
                Err(bpsec::Error::NoKey) => {
                    unread = true;
                    continue;
                }
                Err(bpsec::Error::DecryptionFailed) => {
                    // RFC 9172 §5.1.1: ciphertext corrupt — surface as a
                    // failed fact. Caller applies failure-drop policy.
                    // Coverage stamps are NOT applied (targets remain Maybe).
                    failed.push((bcb_block_number, bib_block_number));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        };

        // `bib::OperationSet` is strict-canonical end-to-end, so the
        // shortest flag is always true and `parse_exact` handles the
        // trailing-garbage check.
        let bib_op_set =
            parse_exact::<bpsec::bib::OperationSet>(&plaintext, "BPSec integrity extension block")?;

        // Per-OperationSet structural rules — single source of truth
        // shared with the keyless parser. The §3.9 "if target is
        // BCB-encrypted then BIB must be BCB-encrypted" check inside
        // `OperationSet::check` is trivially satisfied here because we only
        // reach this branch for BCB-encrypted BIBs.
        bib_op_set.check_decrypted(
            bib_block_number,
            bcb_op_set,
            &PlainReader {
                blocks: &*blocks,
                source_data: data,
            },
        )?;

        // Stamp coverage. The parser stamped these targets as Maybe
        // because of this encrypted BIB; now we know the actual targets.
        for &target_number in bib_op_set.operations.keys() {
            blocks
                .get_mut(&target_number)
                .expect("OperationSet::check verified every target exists")
                .bib = block::BibCoverage::Some(bib_block_number);
        }

        decrypted.insert(bib_block_number, plaintext);
        bib_ops.insert(bib_block_number, bib_op_set);
    }

    // Collapse residual Maybe markers only when every covered BIB both
    // decrypted and authenticated. An unread BIB may still claim any Maybe
    // target; a failed BIB leaves its targets unknown until it is dropped.
    if !unread && failed.is_empty() {
        for block in blocks.values_mut() {
            if matches!(block.bib, block::BibCoverage::Maybe) {
                block.bib = block::BibCoverage::None;
            }
        }
    }

    Ok(failed)
}

// Whether the BCB operation on `target` is in a security context this node
// does not support, so is never processed.
fn unsupported_bcb_operation(
    blocks: &HashMap<u64, block::Block>,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    target: u64,
) -> bool {
    blocks
        .get(&target)
        .and_then(|b| b.bcb)
        .and_then(|bcb| bcb_ops.get(&bcb))
        .and_then(|ops| ops.operations.get(&target))
        .is_some_and(|op| op.unsupported_error().is_some())
}

// The plaintext of `target`, which the BCB `bcb` covers, decrypted under
// `key_source` over what `reader` reads.
fn decrypt_target(
    reader: &CachedReader<'_>,
    key_source: &dyn bpsec::key::KeySource,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    target: u64,
    bcb: u64,
) -> Result<Zeroizing<Box<[u8]>>, bpsec::Error> {
    let ops = bcb_ops
        .get(&bcb)
        .expect("BCB referenced by encrypted block must be in bcb_ops");
    ops.operations
        .get(&target)
        .expect("BCB protects this target")
        .decrypt(
            key_source,
            bpsec::bcb::OperationArgs {
                source: &ops.source,
                target,
                security_block: bcb,
                blocks: reader,
            },
        )
}

// The integrity operations on the Previous Node and Bundle Age blocks among
// `acceptors` inside an encrypted BIB, which no valid bundle carries (see
// `Checked::Conflicting`). Only the forwarder that inserted a per-hop block
// can source an operation on it: if it signed and then encrypted the block,
// RFC 9172 §3.9 forbids the pair to one source, and an earlier node's
// signature had to go when the block changed. The Hop Count is exempt,
// since its increment is a SHOULD (RFC 9171 §4.4.3): a signature on a count
// no node has incremented can stand.
//
// Two facts prove the operation. A BIB read by necessity, on another
// accepted block's account or by an earlier pass, stamps the block's
// coverage with it: those are recorded `Conflicting`, as (BIB, target).
// Without the BIB read, the structure proves it when every BCB-covered
// block other than a BIB is a Previous Node or Bundle Age block: an
// encrypted BIB must target a BCB-covered block (RFC 9172 §3.8) and cannot
// target a BIB or a BCB (§3.7), so it covers one of them. Those are recorded
// `PossiblyConflicting`, as (BIB, block) for each encrypted BIB not in
// `decrypted` and each such block it may cover. Anything short of proof is
// nothing, as a keyless node rightly treats a per-hop block in a valid
// bundle as covered by no encrypted BIB. Both are recorded in block-number
// order.
fn per_hop_integrity_in_encrypted_bibs(
    blocks: &HashMap<u64, block::Block>,
    acceptors: &HashSet<u64>,
    decrypted: &HashMap<u64, Zeroizing<Box<[u8]>>>,
    facts: &mut Facts,
) {
    let encrypted_bib =
        |b: &block::Block| b.block_type == block::Type::BlockIntegrity && b.bcb.is_some();

    let mut conflicting: SmallVec<[(u64, u64); 2]> = blocks
        .iter()
        .filter(|(n, b)| acceptors.contains(*n) && always_replaced(b))
        .filter_map(|(&n, b)| match b.bib {
            block::BibCoverage::Some(bib) if blocks.get(&bib).is_some_and(encrypted_bib) => {
                Some((bib, n))
            }
            _ => None,
        })
        .collect();
    conflicting.sort_unstable();
    for (bib, target) in conflicting {
        facts.record(
            bib,
            target,
            SecurityService::Integrity,
            Checked::Conflicting,
        );
    }

    let covered: SmallVec<[u64; 4]> = blocks
        .iter()
        .filter(|(_, b)| b.bcb.is_some() && b.block_type != block::Type::BlockIntegrity)
        .map(|(&n, _)| n)
        .collect();
    let mut possibly: SmallVec<[(u64, u64); 4]> = SmallVec::new();
    if !covered.is_empty()
        && covered
            .iter()
            .all(|n| blocks.get(n).is_some_and(always_replaced))
    {
        for (&bib, _) in blocks
            .iter()
            .filter(|(n, b)| encrypted_bib(b) && !decrypted.contains_key(*n))
        {
            possibly.extend(
                covered
                    .iter()
                    .filter(|n| acceptors.contains(*n))
                    .map(|&n| (bib, n)),
            );
        }
    }
    possibly.sort_unstable();
    for (bib, target) in possibly {
        facts.record(
            bib,
            target,
            SecurityService::Integrity,
            Checked::PossiblyConflicting,
        );
    }
}

// Per-OperationSet structural checks live as `check` methods on
// `bpsec::bib::OperationSet` / `bpsec::bcb::OperationSet`, the single
// source of truth shared with the structural parser.
