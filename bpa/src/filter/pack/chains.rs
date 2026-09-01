//! The frozen registration state: per-hook filter chains and the node-wide
//! payload peek, produced from the packs at `build()` and executed by the
//! engine.

use super::{Error, FilterPack, Result};
use crate::{
    Arc,
    filter::{
        Classifier, Rewriter, Verifier,
        slots::state::{SlotRegistry, SlotTable},
    },
};

/// One frozen [`Verifier`] registration: the pack-prefixed diagnostic
/// label and the filter.
#[allow(dead_code)] // read by the engine when the C3 swap lands
pub struct VerifierEntry {
    pub label: Arc<str>,
    pub verifier: Box<dyn Verifier>,
}

/// One frozen [`Classifier`] registration.
#[allow(dead_code)] // read by the engine when the C3 swap lands
pub struct ClassifierEntry {
    pub label: Arc<str>,
    pub classifier: Box<dyn Classifier>,
}

/// One frozen [`Rewriter`] registration.
#[allow(dead_code)] // read by the engine when the C3 swap lands
pub struct RewriterEntry {
    pub label: Arc<str>,
    pub rewriter: Box<dyn Rewriter>,
}

/// An input hook's frozen chain: Verifiers (parallel) and Classifiers
/// (sequential).
#[allow(dead_code)] // read by the engine when the C3 swap lands
pub struct InputChain {
    pub verifiers: Box<[VerifierEntry]>,
    pub classifiers: Box<[ClassifierEntry]>,
}

/// An output hook's frozen chain: Rewriters (sequential) then Verifiers
/// (parallel).
#[allow(dead_code)] // read by the engine when the C3 swap lands
pub struct OutputChain {
    pub rewriters: Box<[RewriterEntry]>,
    pub verifiers: Box<[VerifierEntry]>,
}

/// The four per-hook chains frozen by
/// [`build()`](crate::builder::BpaBuilder::build), plus the node-wide
/// payload peek.
#[allow(dead_code)] // read by the engine when the C3 swap lands
pub struct FilterChains {
    pub ingress: InputChain,
    pub originate: InputChain,
    pub egress: OutputChain,
    pub deliver: OutputChain,
    /// P: the maximum payload peek declared across every input-hook
    /// registration.
    pub max_peek: usize,
}

impl FilterChains {
    /// Validates each pack's name, merges and freezes every pack's slot
    /// registrations, splices the per-hook chains in registration order,
    /// and computes the node-wide peek.
    pub fn freeze(packs: Vec<FilterPack>) -> Result<(Self, SlotTable)> {
        let mut slots = SlotRegistry::default();
        let mut max_peek = 0;
        let mut ingress_verifiers = Vec::new();
        let mut originate_verifiers = Vec::new();
        let mut egress_verifiers = Vec::new();
        let mut deliver_verifiers = Vec::new();
        let mut ingress_classifiers = Vec::new();
        let mut originate_classifiers = Vec::new();
        let mut egress_rewriters = Vec::new();
        let mut deliver_rewriters = Vec::new();

        for pack in packs {
            if pack.name.is_empty() {
                return Err(Error::EmptyName);
            }
            if pack.name.contains('.') {
                return Err(Error::DottedName(pack.name));
            }
            slots.merge(pack.slots);

            for v in pack.ingress_verifiers {
                max_peek = max_peek.max(v.peek);
                ingress_verifiers.push(v.entry);
            }
            for v in pack.originate_verifiers {
                max_peek = max_peek.max(v.peek);
                originate_verifiers.push(v.entry);
            }
            for c in pack.ingress_classifiers {
                max_peek = max_peek.max(c.peek);
                ingress_classifiers.push(c.entry);
            }
            for c in pack.originate_classifiers {
                max_peek = max_peek.max(c.peek);
                originate_classifiers.push(c.entry);
            }
            egress_verifiers.extend(pack.egress_verifiers);
            deliver_verifiers.extend(pack.deliver_verifiers);
            egress_rewriters.extend(pack.egress_rewriters);
            deliver_rewriters.extend(pack.deliver_rewriters);
        }

        let slot_table = slots.freeze()?;

        Ok((
            Self {
                ingress: InputChain {
                    verifiers: ingress_verifiers.into_boxed_slice(),
                    classifiers: ingress_classifiers.into_boxed_slice(),
                },
                originate: InputChain {
                    verifiers: originate_verifiers.into_boxed_slice(),
                    classifiers: originate_classifiers.into_boxed_slice(),
                },
                egress: OutputChain {
                    rewriters: egress_rewriters.into_boxed_slice(),
                    verifiers: egress_verifiers.into_boxed_slice(),
                },
                deliver: OutputChain {
                    rewriters: deliver_rewriters.into_boxed_slice(),
                    verifiers: deliver_verifiers.into_boxed_slice(),
                },
                max_peek,
            },
            slot_table,
        ))
    }
}
