//! Filter packs — the embedder's shipping unit for filter registration.
//!
//! A pack reifies a filter pair's common construction code: annotation
//! slots and hook registrations are declared on one [`FilterPack`], whose
//! name prefixes both the at-rest slot names (`"<pack>.<slot>"`) and the
//! diagnostic labels (`"<pack>.<label>"`), so slot collisions between
//! distinct packs are unrepresentable.
//! [`BpaBuilder::add_filters`](crate::builder::BpaBuilder::add_filters)
//! splices packs into the per-hook chains — chain order is call order,
//! within a pack and across `add_filters` calls — and
//! [`build()`](crate::builder::BpaBuilder::build) validates pack names,
//! freezes the chains and the slot table, and fixes the node-wide payload
//! peek `P` as the maximum declared across every registration.

use core::num::NonZeroUsize;

use alloc::format;

use thiserror::Error;

use self::chains::{ClassifierEntry, RewriterEntry, VerifierEntry};
use crate::{
    Arc,
    filter::{
        Classifier, Rewriter, Verifier,
        slots::{self, SlotHandle, SlotValue, state::SlotRegistry},
    },
};

pub(crate) mod chains;

/// Errors from filter-pack registration, surfaced by
/// [`BpaBuilder::build()`](crate::builder::BpaBuilder::build).
#[derive(Debug, Error)]
pub enum Error {
    /// A pack was constructed with an empty name.
    #[error("Filter pack name must not be empty")]
    EmptyName,

    /// A pack name contains '.', which would make the `"<pack>.<slot>"`
    /// at-rest prefixing ambiguous between packs.
    #[error("Filter pack name '{0}' must not contain '.'")]
    DottedName(Arc<str>),

    /// A slot-name collision: two registrations within one pack, or two
    /// same-named packs registering the same slot.
    // Name collision with this module's own `Error`: the slots error stays
    // module-qualified.
    #[error(transparent)]
    Slots(#[from] slots::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

/// An embedder's filter registrations, shipped as one unit.
///
/// The pack is the common construction scope of a filter pair (typically an
/// input-hook [`Classifier`] and an output-hook [`Rewriter`]): register a
/// slot once with [`annotation_slot`](Self::annotation_slot) and hand the
/// returned handle to each filter's constructor — per-bundle state rides
/// the slot, and cross-bundle node state rides a shared inner (e.g. an
/// `Arc<Mutex<...>>`) minted in the same scope. Duplicate slot
/// registration is a [`build()`](crate::builder::BpaBuilder::build) error:
/// two writers should be sharing a handle, not a name.
///
/// Registered filters have no lifecycle: the BPA owns them from
/// [`add_filters`](crate::builder::BpaBuilder::add_filters) until shutdown,
/// with no unregistration and no early teardown — state needing
/// teardown-with-results belongs in a shared inner the embedder retains and
/// tears down after `shutdown()` returns. A filter that needs its own
/// lifecycle is a component, not a filter.
///
/// Hook registrations take a `label`, carried as `"<pack>.<label>"` in logs
/// and metrics — purely diagnostic, never unique. The `_with_peek` variants
/// at the input hooks declare a payload-prefix byte count folded into the
/// node-wide peek `P` at `build()`; the base methods declare 0.
pub struct FilterPack {
    name: Arc<str>,
    slots: SlotRegistry,
    ingress_verifiers: Vec<PendingVerifier>,
    originate_verifiers: Vec<PendingVerifier>,
    egress_verifiers: Vec<VerifierEntry>,
    deliver_verifiers: Vec<VerifierEntry>,
    ingress_classifiers: Vec<PendingClassifier>,
    originate_classifiers: Vec<PendingClassifier>,
    egress_rewriters: Vec<RewriterEntry>,
    deliver_rewriters: Vec<RewriterEntry>,
}

impl FilterPack {
    /// Creates a pack named `name` — the prefix minted into the pack's
    /// at-rest slot names and diagnostic labels.
    ///
    /// Name validity (non-empty, no `'.'`) is enforced by
    /// [`build()`](crate::builder::BpaBuilder::build). Two packs may share
    /// a name; they then share a prefix, so their slot names must stay
    /// disjoint.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            slots: SlotRegistry::default(),
            ingress_verifiers: Vec::new(),
            originate_verifiers: Vec::new(),
            egress_verifiers: Vec::new(),
            deliver_verifiers: Vec::new(),
            ingress_classifiers: Vec::new(),
            originate_classifiers: Vec::new(),
            egress_rewriters: Vec::new(),
            deliver_rewriters: Vec::new(),
        }
    }

    /// Registers an annotation slot named `"<pack>.<name>"` at rest, and
    /// returns its typed [`SlotHandle`] — the capability gating every read
    /// and write of the slot; a filter pair shares state by sharing the
    /// handle. `max_size` bounds the encoded value: larger writes are
    /// dropped (with a warning) when the delta is applied. Registering the
    /// same name twice in one pack is rejected loudly by
    /// [`build()`](crate::builder::BpaBuilder::build).
    pub fn annotation_slot<T: SlotValue>(
        &mut self,
        name: &str,
        max_size: NonZeroUsize,
    ) -> SlotHandle<T> {
        self.slots
            .register(&format!("{}.{name}", self.name), max_size)
    }

    /// Appends a [`Verifier`] to the Ingress chain, with no payload peek.
    pub fn ingress_verifier(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
    ) -> &mut Self {
        self.ingress_verifier_with_peek(label, verifier, 0)
    }

    /// Appends a [`Verifier`] to the Ingress chain, declaring a `peek`-byte
    /// payload prefix.
    pub fn ingress_verifier_with_peek(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
        peek: usize,
    ) -> &mut Self {
        self.ingress_verifiers.push(PendingVerifier {
            peek,
            entry: VerifierEntry {
                label: self.label(label),
                verifier: Box::new(verifier),
            },
        });
        self
    }

    /// Appends a [`Verifier`] to the Originate chain, with no payload peek.
    pub fn originate_verifier(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
    ) -> &mut Self {
        self.originate_verifier_with_peek(label, verifier, 0)
    }

    /// Appends a [`Verifier`] to the Originate chain, declaring a
    /// `peek`-byte payload prefix.
    pub fn originate_verifier_with_peek(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
        peek: usize,
    ) -> &mut Self {
        self.originate_verifiers.push(PendingVerifier {
            peek,
            entry: VerifierEntry {
                label: self.label(label),
                verifier: Box::new(verifier),
            },
        });
        self
    }

    /// Appends a [`Verifier`] to the Egress chain. No peek variant: the
    /// bundle's bytes are resident at the output hooks.
    pub fn egress_verifier(&mut self, label: &str, verifier: impl Verifier + 'static) -> &mut Self {
        self.egress_verifiers.push(VerifierEntry {
            label: self.label(label),
            verifier: Box::new(verifier),
        });
        self
    }

    /// Appends a [`Verifier`] to the Deliver chain. No peek variant: the
    /// bundle's bytes are resident at the output hooks.
    pub fn deliver_verifier(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
    ) -> &mut Self {
        self.deliver_verifiers.push(VerifierEntry {
            label: self.label(label),
            verifier: Box::new(verifier),
        });
        self
    }

    /// Appends a [`Classifier`] to the Ingress chain, with no payload peek.
    pub fn ingress_classifier(
        &mut self,
        label: &str,
        classifier: impl Classifier + 'static,
    ) -> &mut Self {
        self.ingress_classifier_with_peek(label, classifier, 0)
    }

    /// Appends a [`Classifier`] to the Ingress chain, declaring a
    /// `peek`-byte payload prefix.
    pub fn ingress_classifier_with_peek(
        &mut self,
        label: &str,
        classifier: impl Classifier + 'static,
        peek: usize,
    ) -> &mut Self {
        self.ingress_classifiers.push(PendingClassifier {
            peek,
            entry: ClassifierEntry {
                label: self.label(label),
                classifier: Box::new(classifier),
            },
        });
        self
    }

    /// Appends a [`Classifier`] to the Originate chain, with no payload
    /// peek.
    pub fn originate_classifier(
        &mut self,
        label: &str,
        classifier: impl Classifier + 'static,
    ) -> &mut Self {
        self.originate_classifier_with_peek(label, classifier, 0)
    }

    /// Appends a [`Classifier`] to the Originate chain, declaring a
    /// `peek`-byte payload prefix.
    pub fn originate_classifier_with_peek(
        &mut self,
        label: &str,
        classifier: impl Classifier + 'static,
        peek: usize,
    ) -> &mut Self {
        self.originate_classifiers.push(PendingClassifier {
            peek,
            entry: ClassifierEntry {
                label: self.label(label),
                classifier: Box::new(classifier),
            },
        });
        self
    }

    /// Appends a [`Rewriter`] to the Egress chain.
    pub fn egress_rewriter(&mut self, label: &str, rewriter: impl Rewriter + 'static) -> &mut Self {
        self.egress_rewriters.push(RewriterEntry {
            label: self.label(label),
            rewriter: Box::new(rewriter),
        });
        self
    }

    /// Appends a [`Rewriter`] to the Deliver chain.
    pub fn deliver_rewriter(
        &mut self,
        label: &str,
        rewriter: impl Rewriter + 'static,
    ) -> &mut Self {
        self.deliver_rewriters.push(RewriterEntry {
            label: self.label(label),
            rewriter: Box::new(rewriter),
        });
        self
    }

    fn label(&self, suffix: &str) -> Arc<str> {
        format!("{}.{suffix}", self.name).into()
    }
}

// Input-hook pending records: the declared peek rides beside the entry
// until freeze folds it into the node-wide P.
struct PendingVerifier {
    peek: usize,
    entry: VerifierEntry,
}

struct PendingClassifier {
    peek: usize,
    entry: ClassifierEntry,
}
