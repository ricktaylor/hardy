//! The frozen registration state: per-hook filter chains and the node-wide
//! payload peek, produced from the packs at `build()` and executed by the
//! engine.

use super::{FilterPack, Pending};
use crate::{
    Arc,
    filter::{Classifier, Rewriter, Verifier},
};

/// One frozen [`Verifier`] registration: the pack-prefixed diagnostic
/// label and the filter.
pub struct VerifierEntry {
    pub label: Arc<str>,
    pub verifier: Box<dyn Verifier>,
}

/// One frozen [`Classifier`] registration.
pub struct ClassifierEntry {
    pub label: Arc<str>,
    pub classifier: Box<dyn Classifier>,
}

/// One frozen [`Rewriter`] registration.
pub struct RewriterEntry {
    pub label: Arc<str>,
    pub rewriter: Box<dyn Rewriter>,
}

/// An input hook's frozen chain: Verifiers (parallel) and Classifiers
/// (sequential).
pub struct InputChain {
    pub verifiers: Box<[VerifierEntry]>,
    pub classifiers: Box<[ClassifierEntry]>,
}

/// The Deliver hook's frozen chain: Rewriters (sequential) then Verifiers
/// (parallel).
pub struct DeliverChain {
    pub rewriters: Box<[RewriterEntry]>,
    pub verifiers: Box<[VerifierEntry]>,
}

/// The four per-hook chains frozen by
/// [`build()`](crate::builder::BpaBuilder::build), plus the node-wide
/// payload peek.
pub struct FilterChains {
    pub ingress: InputChain,
    pub originate: InputChain,
    /// The Egress hook's frozen chain: Rewriters only (sequential).
    pub egress: Box<[RewriterEntry]>,
    pub deliver: DeliverChain,
    /// P: the maximum payload peek declared across every input-hook
    /// registration.
    #[allow(dead_code)] // consumed by the pre-drain Ingress seat (Phase 3)
    pub max_peek: usize,
}

impl FilterChains {
    /// Splices the per-hook chains in registration order and computes the
    /// node-wide peek.
    pub fn freeze(packs: Vec<FilterPack>) -> Self {
        let mut max_peek = 0;
        let mut ingress_verifiers = Vec::new();
        let mut originate_verifiers = Vec::new();
        let mut deliver_verifiers = Vec::new();
        let mut ingress_classifiers = Vec::new();
        let mut originate_classifiers = Vec::new();
        let mut egress_rewriters = Vec::new();
        let mut deliver_rewriters = Vec::new();

        for pack in packs {
            drain_pending(
                pack.ingress_verifiers,
                &mut ingress_verifiers,
                &mut max_peek,
            );
            drain_pending(
                pack.originate_verifiers,
                &mut originate_verifiers,
                &mut max_peek,
            );
            drain_pending(
                pack.ingress_classifiers,
                &mut ingress_classifiers,
                &mut max_peek,
            );
            drain_pending(
                pack.originate_classifiers,
                &mut originate_classifiers,
                &mut max_peek,
            );
            deliver_verifiers.extend(pack.deliver_verifiers);
            egress_rewriters.extend(pack.egress_rewriters);
            deliver_rewriters.extend(pack.deliver_rewriters);
        }

        Self {
            ingress: InputChain {
                verifiers: ingress_verifiers.into_boxed_slice(),
                classifiers: ingress_classifiers.into_boxed_slice(),
            },
            originate: InputChain {
                verifiers: originate_verifiers.into_boxed_slice(),
                classifiers: originate_classifiers.into_boxed_slice(),
            },
            egress: egress_rewriters.into_boxed_slice(),
            deliver: DeliverChain {
                rewriters: deliver_rewriters.into_boxed_slice(),
                verifiers: deliver_verifiers.into_boxed_slice(),
            },
            max_peek,
        }
    }
}

// The one peek-fold: every input-hook pending list drains through here, so
// a future hook that forgets the fold is a missing call, not a silently
// dropped peek declaration.
fn drain_pending<E>(pending: Vec<Pending<E>>, out: &mut Vec<E>, max_peek: &mut usize) {
    for p in pending {
        let (peek, entry) = p.into_parts();
        *max_peek = (*max_peek).max(peek);
        out.push(entry);
    }
}
