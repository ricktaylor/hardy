//! The §5.6 reception-reporting facts a pass over an arrival establishes,
//! joined across the stages, and their one projection to the reception
//! report, made as the report is.

use core::ops::{BitOr, BitOrAssign};

use hardy_bpv7::{parse, status_report::ReasonCode};

/// The reception report the §5.6 facts ([`ReportFacts`]) project to, once,
/// as the report is made ([`reception_report_for`]): what reason the
/// reception assertion carries, and whether a block's own
/// `report_on_failure` flag demands the report be emitted regardless of the
/// bundle-level receipt flag (§5.6 Step 4's block-flag-alone trigger). The
/// nonsense states — a demanded "No additional information", a non-demanded
/// "Block unsupported" — are unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceptionReport {
    /// Nothing beyond §5.6 Step 2: reason "No additional information",
    /// emitted only when the bundle requests reception reports.
    Requested,
    /// A §5.1.1 failure-drop was scheduled: reason "Failed security
    /// operation", still bundle-flag-gated (RFC 9172 keeps failure
    /// reporting at requested-MAY level).
    FailureDropped,
    /// A block's `report_on_failure` flag demands the report: emitted even
    /// when the bundle-level receipt flag is clear. Carries "Block
    /// unsupported", "Missing security operation" or "Unknown security
    /// operation" — or "Failed security operation" when a failure-drop also
    /// fired and outranks them in the record's one reason slot. Never produced for an admin-record or
    /// anonymous bundle: the parser rejects the flag combination (§4.2.4).
    Demanded(ReasonCode),
}

impl ReceptionReport {
    /// The reason code the reception assertion carries.
    pub fn reason(&self) -> ReasonCode {
        match self {
            Self::Requested => ReasonCode::NoAdditionalInformation,
            Self::FailureDropped => ReasonCode::FailedSecurityOperation,
            Self::Demanded(reason) => *reason,
        }
    }

    /// §5.6 Step 4's block-flag-alone trigger: the report is emitted even
    /// when the bundle-level receipt flag is clear.
    pub fn demanded(&self) -> bool {
        matches!(self, Self::Demanded(_))
    }
}

/// The §5.6 reception-reporting facts a stage establishes: whether a §5.1.1
/// failure-drop was scheduled, and which block-flag demands fired, a block
/// under a security operation no valid bundle carries, one this node cannot
/// process (Step 4), a security operation it accepts in a context it does
/// not support, or a block that carries no operation of a service its
/// policy expected on it, confidentiality or integrity. Facts from several
/// stages combine with `|`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReportFacts {
    /// A target whose security operation failed is discarded: "Failed
    /// security operation", gated on the bundle's receipt-report flag.
    pub failure_dropped: bool,
    /// A block under a security operation no valid bundle carries, which
    /// this node cannot accept, demands the report: "Conflicting security
    /// operation" (RFC 9172 §7.1).
    pub conflicting: bool,
    /// A block this node cannot process demands the report: "Block
    /// unsupported".
    pub unsupported_block: bool,
    /// A security block whose operation this node accepts, in a context it
    /// does not support, demands the report: "Unknown security operation".
    pub unsupported_security: bool,
    /// A block that carries no operation of a service this node's policy
    /// expected on it, no BCB operation where it accepts or verifies its
    /// confidentiality, no BIB operation where it accepts or verifies its
    /// integrity, demands the report: "Missing security operation" (RFC
    /// 9172 §7.1).
    pub missing_security: bool,
}

impl ReportFacts {
    /// The facts the §A classification establishes at every receiver: an
    /// unrecognised block's demand. An unsupported security operation's is
    /// its acceptor's, so it is not among them.
    pub fn unrecognised(classification: &parse::Classification) -> Self {
        Self {
            unsupported_block: classification.report_unsupported_block,
            ..Self::default()
        }
    }
}

impl BitOr for ReportFacts {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self {
            failure_dropped: self.failure_dropped || other.failure_dropped,
            conflicting: self.conflicting || other.conflicting,
            unsupported_block: self.unsupported_block || other.unsupported_block,
            unsupported_security: self.unsupported_security || other.unsupported_security,
            missing_security: self.missing_security || other.missing_security,
        }
    }
}

impl BitOrAssign for ReportFacts {
    fn bitor_assign(&mut self, other: Self) {
        *self = *self | other;
    }
}

/// The reception report for `facts`. The RFC 9172 security codes outrank
/// the generic RFC 9171 block code when several fire: a dropped corrupt
/// operation is the most material event, then an operation no valid bundle
/// carries, then one this node cannot understand, then one it requires and
/// the bundle lacks, then a block it cannot process — but only the
/// block-flag facts make the report [`Demanded`](ReceptionReport::Demanded).
pub fn reception_report_for(facts: ReportFacts) -> ReceptionReport {
    let demanded = facts.conflicting
        || facts.unsupported_security
        || facts.missing_security
        || facts.unsupported_block;
    match (demanded, facts.failure_dropped) {
        (false, false) => ReceptionReport::Requested,
        (false, true) => ReceptionReport::FailureDropped,
        (true, _) => ReceptionReport::Demanded(if facts.failure_dropped {
            ReasonCode::FailedSecurityOperation
        } else if facts.conflicting {
            ReasonCode::ConflictingSecurityOperation
        } else if facts.unsupported_security {
            ReasonCode::UnknownSecurityOperation
        } else if facts.missing_security {
            ReasonCode::MissingSecurityOperation
        } else {
            ReasonCode::BlockUnsupported
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reception_report_precedence() {
        assert_eq!(
            reception_report_for(ReportFacts::default()),
            ReceptionReport::Requested
        );
        // A failure-drop alone reports, but is not block-demanded.
        assert_eq!(
            reception_report_for(ReportFacts {
                failure_dropped: true,
                ..Default::default()
            }),
            ReceptionReport::FailureDropped
        );
        assert_eq!(
            reception_report_for(ReportFacts {
                unsupported_block: true,
                ..Default::default()
            }),
            ReceptionReport::Demanded(ReasonCode::BlockUnsupported)
        );
        assert_eq!(
            reception_report_for(ReportFacts {
                unsupported_block: true,
                unsupported_security: true,
                ..Default::default()
            }),
            ReceptionReport::Demanded(ReasonCode::UnknownSecurityOperation)
        );
        // An operation this node requires and the bundle lacks outranks the
        // generic block code, and one it cannot understand outranks it.
        assert_eq!(
            reception_report_for(ReportFacts {
                unsupported_block: true,
                missing_security: true,
                ..Default::default()
            }),
            ReceptionReport::Demanded(ReasonCode::MissingSecurityOperation)
        );
        assert_eq!(
            reception_report_for(ReportFacts {
                unsupported_block: true,
                unsupported_security: true,
                missing_security: true,
                ..Default::default()
            }),
            ReceptionReport::Demanded(ReasonCode::UnknownSecurityOperation)
        );
        // A conflicting operation demands the report, and outranks the codes
        // for what this node cannot understand or process.
        assert_eq!(
            reception_report_for(ReportFacts {
                conflicting: true,
                ..Default::default()
            }),
            ReceptionReport::Demanded(ReasonCode::ConflictingSecurityOperation)
        );
        assert_eq!(
            reception_report_for(ReportFacts {
                conflicting: true,
                unsupported_block: true,
                unsupported_security: true,
                ..Default::default()
            }),
            ReceptionReport::Demanded(ReasonCode::ConflictingSecurityOperation)
        );
        // The failure-drop outranks in the reason slot without erasing the
        // block's demand.
        assert_eq!(
            reception_report_for(ReportFacts {
                failure_dropped: true,
                conflicting: true,
                unsupported_block: true,
                unsupported_security: true,
                ..Default::default()
            }),
            ReceptionReport::Demanded(ReasonCode::FailedSecurityOperation)
        );
    }

    // Facts from later stages join by `|`, and the one projection ranks
    // them: a failure-drop outranks in the reason slot, a block-flag
    // demand keeps the report demanded, and the security codes outrank the
    // generic block code.
    #[test]
    fn joined_facts_project_with_one_precedence() {
        use ReceptionReport::{Demanded, FailureDropped, Requested};

        let dropped = ReportFacts {
            failure_dropped: true,
            ..ReportFacts::default()
        };
        let block = ReportFacts {
            unsupported_block: true,
            ..ReportFacts::default()
        };
        let security = ReportFacts {
            unsupported_security: true,
            ..ReportFacts::default()
        };
        let missing = ReportFacts {
            missing_security: true,
            ..ReportFacts::default()
        };
        let conflicting = ReportFacts {
            conflicting: true,
            ..ReportFacts::default()
        };
        assert_eq!(reception_report_for(ReportFacts::default()), Requested);
        assert_eq!(reception_report_for(dropped), FailureDropped);
        assert_eq!(
            reception_report_for(block),
            Demanded(ReasonCode::BlockUnsupported)
        );
        assert_eq!(
            reception_report_for(block | security),
            Demanded(ReasonCode::UnknownSecurityOperation)
        );
        assert_eq!(
            reception_report_for(dropped | block),
            Demanded(ReasonCode::FailedSecurityOperation)
        );
        assert_eq!(
            reception_report_for(security | dropped),
            Demanded(ReasonCode::FailedSecurityOperation)
        );
        assert_eq!(
            reception_report_for(missing | block),
            Demanded(ReasonCode::MissingSecurityOperation)
        );
        assert_eq!(
            reception_report_for(missing | security),
            Demanded(ReasonCode::UnknownSecurityOperation)
        );
        assert_eq!(
            reception_report_for(conflicting | security | missing),
            Demanded(ReasonCode::ConflictingSecurityOperation)
        );
        let mut joined = block;
        joined |= dropped;
        assert_eq!(joined, dropped | block);
    }
}
