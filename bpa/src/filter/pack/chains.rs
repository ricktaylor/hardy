//! The frozen registration state: the per-hook filter chains, each input
//! chain with its payload peek, produced from the packs at `build()` and
//! executed by the engine.

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
/// (sequential), with the hook's payload peek.
pub struct InputChain {
    pub verifiers: Box<[VerifierEntry]>,
    pub classifiers: Box<[ClassifierEntry]>,
    /// P: the largest payload peek this chain's registrations declare, which
    /// the hook's door holds before the chain runs; 0 for an empty chain.
    pub peek: usize,
}

/// The Deliver hook's frozen chain: Rewriters (sequential) then Verifiers
/// (parallel).
pub struct DeliverChain {
    pub rewriters: Box<[RewriterEntry]>,
    pub verifiers: Box<[VerifierEntry]>,
}

/// The four per-hook chains frozen by
/// [`build()`](crate::builder::BpaBuilder::build).
pub struct FilterChains {
    pub ingress: InputChain,
    pub originate: InputChain,
    /// The Egress hook's frozen chain: Rewriters only (sequential).
    pub egress: Box<[RewriterEntry]>,
    pub deliver: DeliverChain,
}

impl FilterChains {
    /// Splices the per-hook chains in registration order and folds each
    /// input chain's peek.
    pub fn freeze(packs: Vec<FilterPack>) -> Self {
        let mut ingress_peek = 0;
        let mut originate_peek = 0;
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
                &mut ingress_peek,
            );
            drain_pending(
                pack.originate_verifiers,
                &mut originate_verifiers,
                &mut originate_peek,
            );
            drain_pending(
                pack.ingress_classifiers,
                &mut ingress_classifiers,
                &mut ingress_peek,
            );
            drain_pending(
                pack.originate_classifiers,
                &mut originate_classifiers,
                &mut originate_peek,
            );
            deliver_verifiers.extend(pack.deliver_verifiers);
            egress_rewriters.extend(pack.egress_rewriters);
            deliver_rewriters.extend(pack.deliver_rewriters);
        }

        Self {
            ingress: InputChain {
                verifiers: ingress_verifiers.into_boxed_slice(),
                classifiers: ingress_classifiers.into_boxed_slice(),
                peek: ingress_peek,
            },
            originate: InputChain {
                verifiers: originate_verifiers.into_boxed_slice(),
                classifiers: originate_classifiers.into_boxed_slice(),
                peek: originate_peek,
            },
            egress: egress_rewriters.into_boxed_slice(),
            deliver: DeliverChain {
                rewriters: deliver_rewriters.into_boxed_slice(),
                verifiers: deliver_verifiers.into_boxed_slice(),
            },
        }
    }
}

// The one peek-fold: every input-hook pending list drains through here into
// its own chain's peek, so a future hook that forgets the fold is a missing
// call, not a silently dropped peek declaration.
fn drain_pending<E>(pending: Vec<Pending<E>>, out: &mut Vec<E>, chain_peek: &mut usize) {
    for p in pending {
        let (peek, entry) = p.into_parts();
        *chain_peek = (*chain_peek).max(peek);
        out.push(entry);
    }
}
