//! A failure's fate (RFC 9172 §5.1.1, §5.1.2) a pass over an arrival
//! applies to what it finds, and the gate's one pass over the blocks the
//! Ingress chain announced.

use hardy_bpv7::{
    Bundle as Bpv7Bundle, block,
    bpsec::{self, Role, Roles, SecurityService, SecurityServices, checks::Checked},
    parse,
    status_report::ReasonCode,
};

use super::{has_type, report::ReportFacts};
use crate::{HashMap, HashSet};

/// What the fate its flags decide leaves the bundle, for the ingress rewrite and
/// the reception report.
#[derive(Debug, Default)]
pub struct Fate {
    /// The blocks the fate discards, with their security operations.
    pub discarded: HashSet<u64>,
    /// The reception-reporting facts it establishes.
    pub report: ReportFacts,
}

/// A fatal fate, the bundle's deletion: the error its deletion reason
/// comes from, and the report demands, which precede the deletion.
#[derive(Debug)]
pub struct Deletion {
    pub error: hardy_bpv7::Error,
    pub report: ReportFacts,
}

// What a check or an acceptance found, for the fate the flags decide.
#[derive(Debug, Default)]
pub(super) struct Findings {
    // Targets of BCB operations whose ciphertext did not authenticate.
    pub(super) bcb_failed: HashSet<u64>,
    // Targets of BIB operations whose integrity check failed.
    pub(super) bib_failed: HashSet<u64>,
    // Targets of an integrity operation inside an encrypted BIB this node
    // decrypted, which no valid bundle carries and it cannot accept.
    pub(super) conflicting: HashSet<u64>,
    // Blocks this node could not accept: for want of a key, an acceptance
    // it cannot complete, or an operation on them in a security context it
    // does not support.
    pub(super) unaccepted: HashSet<u64>,
    // Security blocks with an operation, in a context this node does not
    // support, on a block it accepts.
    pub(super) unsupported: HashSet<u64>,
    // Blocks this node could not accept but replaces, no failure: a Bundle
    // Age on a bundle with a creation time, whose replacement egress writes
    // from the clock.
    pub(super) soft: HashSet<u64>,
}

// A failure's fate (RFC 9172 §5.1.1, §5.1.2), shared by the header pass and
// the gate's acceptance. A target whose operation failed (Failed security
// operation, 15), that is under an integrity operation no valid bundle
// carries (Conflicting security operation, 16, RFC 9172 §7) or that this
// node could not accept (Block unsupported, 11, one it cannot process, RFC
// 9171 §5.6 Step 4) deletes the bundle where its flags say "delete bundle if
// block can't be processed", or any target where `every_failure_fatal` (a
// source refusing its service's bundle rather than altering it), and is
// otherwise discarded with its operations; a target takes one of those
// fates, in that order. A block this node replaces without accepting is
// discarded with its operations too, its flags unconsulted and nothing
// reported. Where a discarded block's removal would shrink an encrypted BIB
// that also covers a block that stays, which this node does not encrypt
// again, the ingress rewrite removes that BIB with it, whole, if its
// ciphertext did not authenticate or it is flagged to be discarded, as RFC
// 9172 §5.1.1 and RFC 9171 §5.6 Step 4 require, or if it is a per-hop block,
// which this node replaces or, as its acceptor, found failed; it otherwise
// keeps the block as received. The payload, of which a bundle has exactly
// one (RFC 9171 §4.1), deletes it whatever its flags, and the primary
// block's entry carries nominal flags (`Flags::primary`) that do. A security
// block with an operation it accepts in an unsupported context is itself one
// it cannot process (Unknown security operation, 13): its flags delete the
// bundle, or discard a BIB, which can go alone where a BCB cannot leave its
// ciphertext behind. A flagged block demands the reception report, except on
// an administrative record or anonymous bundle (RFC 9171 §4.2.3).
pub(super) fn decide_fate(
    bundle: &Bpv7Bundle,
    findings: Findings,
    every_failure_fatal: bool,
) -> Result<Fate, Deletion> {
    let Findings {
        bcb_failed,
        bib_failed,
        conflicting,
        unaccepted,
        unsupported,
        soft,
    } = findings;
    // A target meets one fate: failed, else conflicting, else unaccepted or
    // soft.
    let failed: HashSet<u64> = bcb_failed.union(&bib_failed).copied().collect();
    let conflicting: HashSet<u64> = conflicting.difference(&failed).copied().collect();
    let settled: HashSet<u64> = failed.union(&conflicting).copied().collect();
    let unaccepted: HashSet<u64> = unaccepted.difference(&settled).copied().collect();
    let soft: HashSet<u64> = soft.difference(&settled).copied().collect();

    let blocks = &bundle.blocks;
    let fatal = |target: &u64| fatal_loss(bundle, *target, every_failure_fatal);
    let demands = |set: &HashSet<u64>| demands_report(bundle, set);
    let report = ReportFacts {
        failure_dropped: false,
        conflicting: demands(&conflicting),
        unsupported_block: demands(&unaccepted),
        unsupported_security: demands(&unsupported),
        missing_security: false,
    };
    let deletion = |error: bpsec::Error| Deletion {
        error: error.into(),
        report,
    };
    if bcb_failed.iter().any(fatal) {
        return Err(deletion(bpsec::Error::DecryptionFailed));
    }
    if bib_failed.iter().any(fatal) {
        return Err(deletion(bpsec::Error::IntegrityCheckFailed));
    }
    if let Some(&target) = conflicting.iter().filter(|t| fatal(t)).min() {
        return Err(deletion(bpsec::Error::EncryptedBibCoversPerHopBlock(
            target,
        )));
    }
    if unsupported
        .iter()
        .any(|n| deletes_on_failure(blocks, *n, every_failure_fatal))
    {
        return Err(deletion(bpsec::Error::UnsupportedOperation));
    }
    if unaccepted.iter().any(fatal) {
        return Err(deletion(bpsec::Error::NoKey));
    }

    let failure_dropped = !bcb_failed.is_empty() || !bib_failed.is_empty();
    let mut discarded = bcb_failed;
    discarded.extend(bib_failed);
    discarded.extend(conflicting);
    discarded.extend(unaccepted);
    discarded.extend(soft);
    discarded.extend(unsupported.into_iter().filter(|n| {
        blocks.get(n).is_some_and(|b| {
            b.block_type == block::Type::BlockIntegrity && b.flags.delete_block_on_failure
        })
    }));
    Ok(Fate {
        discarded,
        report: ReportFacts {
            failure_dropped,
            ..report
        },
    })
}

// Whether losing `n` deletes the bundle: where its flags say "delete bundle
// if block can't be processed", or any block where `every_failure_fatal`.
fn deletes_on_failure(
    blocks: &HashMap<u64, block::Block>,
    n: u64,
    every_failure_fatal: bool,
) -> bool {
    every_failure_fatal
        || blocks
            .get(&n)
            .is_some_and(|b| b.flags.delete_bundle_on_failure)
}

// Whether losing `n` is fatal to the bundle: the payload, of which it has
// exactly one (RFC 9171 §4.1); the Bundle Age of a bundle without a
// creation time, its only expiry signal (§4.4.2); the primary block, whose
// entry carries nominal flags that say so; or any block whose flags do.
fn fatal_loss(bundle: &Bpv7Bundle, n: u64, every_failure_fatal: bool) -> bool {
    n == 1
        || (!bundle.primary.id.timestamp.is_clocked()
            && has_type(&bundle.blocks, n, block::Type::BundleAge))
        || deletes_on_failure(&bundle.blocks, n, every_failure_fatal)
}

// Whether a block in `set` demands the reception report by its
// `report_on_failure` flag, which no block of an administrative record or
// anonymous bundle does (RFC 9171 §4.2.3).
fn demands_report(bundle: &Bpv7Bundle, set: &HashSet<u64>) -> bool {
    !bundle.primary.forbids_report_on_failure()
        && set.iter().any(|n| {
            bundle
                .blocks
                .get(n)
                .is_some_and(|b| b.flags.report_on_failure)
        })
}

/// Why the gate's acceptance or verification of the blocks the Ingress
/// chain announced deletes the bundle.
#[derive(Debug, thiserror::Error)]
pub enum Rejected {
    /// A fatal fate: an announced block the bundle cannot lose that this node
    /// could not accept or verify deletes the bundle.
    #[error("{}", .0.error)]
    Fatal(Deletion),
    /// An announced block the bundle cannot lose that carries no operation
    /// of a service this node's policy expected on it, confidentiality or
    /// integrity: "Missing security operation" (RFC 9172 §7.1).
    #[error("block {block} carries no operation of a service this node's policy expects on it")]
    Missing {
        block: u64,
        /// The report demands, which precede the deletion.
        report: ReportFacts,
    },
}

impl Rejected {
    /// The reason the deletion is reported with.
    pub fn reason(&self) -> ReasonCode {
        match self {
            Self::Fatal(deletion) => ReasonCode::from(&deletion.error),
            Self::Missing { .. } => ReasonCode::MissingSecurityOperation,
        }
    }

    /// The report demands, which precede the deletion.
    pub fn report(&self) -> ReportFacts {
        match self {
            Self::Fatal(deletion) => deletion.report,
            Self::Missing { report, .. } => *report,
        }
    }
}

// A failure's fate, with the blocks policy expected an operation of some
// service on, confidentiality or integrity, and found none (`missing`):
// each is discarded, as RFC 9172 §5.1.1 and §5.1.2 recommend of a target
// the policy expected protected, or deletes
// the bundle where it is the payload or its flags say so, and its flags
// demand the reception report, "Missing security operation". The fate's
// own deletion comes first, carrying that demand.
fn fate_with_missing(
    bundle: &Bpv7Bundle,
    findings: Findings,
    missing: HashSet<u64>,
) -> Result<Fate, Rejected> {
    let demand = ReportFacts {
        missing_security: demands_report(bundle, &missing),
        ..ReportFacts::default()
    };
    let mut fate = decide_fate(bundle, findings, false).map_err(|Deletion { error, report }| {
        Rejected::Fatal(Deletion {
            error,
            report: report | demand,
        })
    })?;
    if let Some(&block) = missing
        .iter()
        .filter(|&&n| fatal_loss(bundle, n, false))
        .min()
    {
        return Err(Rejected::Missing {
            block,
            report: fate.report | demand,
        });
    }
    fate.discarded.extend(missing);
    fate.report |= demand;
    Ok(fate)
}

/// What the gate's pass over the blocks the Ingress chain announced leaves
/// for the ingress rewrite and the reception report.
#[derive(Debug, Default)]
pub struct Announced {
    /// The announced blocks whose operations this node accepted, with the
    /// services accepted on each.
    pub accepted: HashMap<u64, SecurityServices>,
    /// The targets of the BIB operations the pass checked and found good,
    /// those over blocks accepted for confidentiality alone among them,
    /// whose BIB stays.
    pub verified: HashSet<u64>,
    /// What the fate leaves.
    pub fate: Fate,
    /// Whether the payload's ciphertext is to be verified as it streams:
    /// this node its verifier for confidentiality, and the payload
    /// encrypted.
    pub verify_payload_ciphertext: bool,
}

/// The gate's one pass over the blocks the Ingress chain announced, in this
/// node's `roles` on each (RFC 9172 §5.1.1, §5.1.2), before anything is
/// spooled: as an acceptor, [`Parsed::check`](parse::Parsed::check)
/// processes the operations it accepts, and as a verifier, a role a node
/// that is not the acceptor "MAY attempt" and one announced is expected to
/// fill, it checks what it verifies, a block's ciphertext here, its
/// plaintext kept to the arrival's cache, or, for the payload, by the drain
/// as it streams ([`Announced::verify_payload_ciphertext`]), and a block's
/// integrity unless this arrival already checked it and found it good (the
/// header pass's `verified`, or the drain's for a payload it is checking,
/// `payload_verifying`). The BIB over a block accepted for confidentiality
/// alone is checked as a verifier checks it and stays, in plaintext;
/// `verified` reports it. The fate the header pass applies to the
/// per-hop blocks applies to what the pass finds, as a block this node
/// cannot process (RFC 9171 §5.6 Step 4): one it holds no key for, as an
/// acceptor or, for integrity, where it is named a verifier; one whose
/// integrity it is to accept or verify while the block is encrypted here,
/// its BIB encrypted with it (RFC 9172 §3.9) and unreadable without
/// accepting its BCB operation, since nothing is decrypted to reach a BIB;
/// one under an encrypted BIB it cannot read that may cover the block
/// ([`Maybe`](block::BibCoverage::Maybe)), where it is named for integrity;
/// one whose acceptance would take a BCB operation it cannot remove, on
/// such a BIB or on a block not accepted for confidentiality under an
/// encrypted BIB that also covers the accepted one (RFC 9172 §3.9); or one
/// under an operation in a security context it does not support ("Unknown
/// security operation", the security block being one it cannot process
/// too). A block whose BCB operation went with an accepted block's and
/// failed meets its fate too, as does one whose BIB fails, as does
/// ciphertext that does not authenticate ("Failed security operation"). A
/// Hop Count whose acceptance cannot be completed is no failure: its
/// increment is a SHOULD (RFC 9171 §4.4.3), and it stays as received. A
/// block that carries no operation of a service policy `expected` on it, to
/// accept or to verify, as against the blocks the destination accepts of
/// itself, is discarded, as RFC 9172 §5.1.1 and §5.1.2 recommend of a
/// target the policy expected protected, or deletes the bundle where it is
/// the payload or its flags say so ("Missing security operation"). A block
/// the bundle does not carry is passed over.
pub fn check_announced(
    roles: &HashMap<u64, Roles>,
    expected: &HashMap<u64, SecurityServices>,
    parsed: &mut parse::Parsed,
    keys: &dyn bpsec::key::KeySource,
    verified: &HashSet<u64>,
    payload_verifying: bool,
) -> Result<Announced, Rejected> {
    let blocks = &parsed.bundle.blocks;
    // The roles the check takes. A verifier of the payload's confidentiality
    // is the drain's; an integrity verification this arrival has made is
    // not made again; the BIB over a block accepted for confidentiality
    // alone is checked as a verifier checks it. A block whose integrity this
    // node is to accept or verify while the block stays encrypted here, its
    // confidentiality not its to accept, is unreadable, unless it has
    // verified the block already.
    let mut to_check: HashMap<u64, Roles> = HashMap::new();
    let mut unreadable: HashSet<u64> = HashSet::new();
    let mut verify_payload_ciphertext = false;
    for (&n, r) in roles {
        let Some(block) = blocks.get(&n) else {
            continue;
        };
        let checked_already = verified.contains(&n) || (n == 1 && payload_verifying);
        let confidentiality = match r.confidentiality {
            Some(Role::Verifier) if n == 1 => {
                verify_payload_ciphertext = block.bcb.is_some();
                None
            }
            role => role,
        };
        let integrity = match r.integrity {
            Some(Role::Acceptor) => Some(Role::Acceptor),
            Some(Role::Verifier) if !checked_already => Some(Role::Verifier),
            Some(Role::Verifier) => None,
            None if r.confidentiality == Some(Role::Acceptor) => Some(Role::Verifier),
            None => None,
        };
        let unread_for_integrity = match r.integrity {
            Some(Role::Acceptor) => r.confidentiality != Some(Role::Acceptor),
            Some(Role::Verifier) => r.confidentiality != Some(Role::Acceptor) && !checked_already,
            None => false,
        };
        if unread_for_integrity && block.bcb.is_some() {
            unreadable.insert(n);
        }
        let check = Roles {
            confidentiality,
            integrity,
        };
        if check.confidentiality.is_some() || check.integrity.is_some() {
            to_check.insert(n, check);
        }
    }
    let facts = parsed
        .check(keys, &to_check, Roles::default())
        .map_err(|error| {
            Rejected::Fatal(Deletion {
                error,
                report: ReportFacts::default(),
            })
        })?;
    let bundle = &parsed.bundle;
    let blocks = &bundle.blocks;
    // A block named for integrity whose coverage the check left `Maybe`
    // lies under an encrypted BIB this node cannot read: nothing was
    // checked, and nothing can be accepted.
    let unresolved: HashSet<u64> = roles
        .iter()
        .filter(|(n, r)| {
            r.integrity.is_some()
                && !verified.contains(n)
                && blocks
                    .get(n)
                    .is_some_and(|b| b.bib == block::BibCoverage::Maybe)
        })
        .map(|(&n, _)| n)
        .collect();
    let missing: HashSet<u64> = expected
        .iter()
        .filter(|(n, expects)| {
            blocks.get(n).is_some_and(|b| {
                (expects.confidentiality && b.bcb.is_none())
                    || (expects.integrity && b.bib == block::BibCoverage::None)
            })
        })
        .map(|(&n, _)| n)
        .collect();
    // A missing key for a BIB operation is a failed acceptance or
    // verification only where the block's integrity is this node's to
    // accept or verify, a verifier's skip otherwise; an incomplete or
    // unreadable Hop Count is no failure, its increment being a SHOULD.
    let findings = Findings {
        bcb_failed: facts
            .targets_of(SecurityService::Confidentiality, Checked::Failed)
            .collect(),
        bib_failed: facts
            .targets_of(SecurityService::Integrity, Checked::Failed)
            .collect(),
        unaccepted: facts
            .outcomes
            .iter()
            .filter(|o| {
                o.checked == Checked::NoKey
                    && (o.service == SecurityService::Confidentiality
                        || roles.get(&o.target).is_some_and(|r| r.integrity.is_some()))
            })
            .map(|o| o.target)
            .chain(
                facts
                    .incomplete
                    .iter()
                    .copied()
                    .chain(unreadable.iter().copied())
                    .filter(|&n| !has_type(blocks, n, block::Type::HopCount)),
            )
            .chain(unresolved.iter().copied())
            .chain(facts.targets(Checked::Unsupported))
            .collect(),
        unsupported: facts
            .outcomes
            .iter()
            .filter(|o| o.checked == Checked::Unsupported)
            .map(|o| o.security_block)
            .collect(),
        ..Default::default()
    };
    let fate = fate_with_missing(bundle, findings, missing)?;

    // The payload is accepted with its operations, which the drain processes
    // as it streams: a missing key or an unsupported operation on it has
    // already deleted the bundle, as its fate.
    let accepted = roles
        .iter()
        .filter(|(n, r)| {
            r.accepts().any()
                && !fate.discarded.contains(n)
                && !facts.incomplete.contains(n)
                && !unreadable.contains(n)
                && !unresolved.contains(n)
                && blocks
                    .get(n)
                    .is_some_and(|b| b.bcb.is_some() || b.bib != block::BibCoverage::None)
        })
        .map(|(&n, r)| (n, r.accepts()))
        .collect();
    let verified = facts
        .targets_of(SecurityService::Integrity, Checked::Verified)
        .collect();
    Ok(Announced {
        accepted,
        verified,
        fate,
        verify_payload_ciphertext,
    })
}
