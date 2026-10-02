//! Filter packs — the embedder's shipping unit for filter registration.
//!
//! A pack reifies a filter pair's common construction code: its hook
//! registrations are declared on one [`FilterPack`], whose name prefixes
//! their diagnostic labels (`"<pack>.<label>"`). The annotation slots a pair
//! shares are declared with [`slot!`](crate::slot), not registered.
//! [`BpaBuilder::add_filters`](crate::builder::BpaBuilder::add_filters)
//! splices packs into the per-hook chains — chain order is call order,
//! within a pack and across `add_filters` calls — and
//! [`build()`](crate::builder::BpaBuilder::build) freezes the chains and
//! fixes the node-wide payload peek as the maximum declared across every
//! registration.

use alloc::format;
use core::fmt::{self, Debug, Formatter};

use self::chains::{ClassifierEntry, RewriterEntry, VerifierEntry};
use crate::{
    Arc,
    filter::{Classifier, Rewriter, Verifier},
};

pub(crate) mod chains;

/// An embedder's filter registrations, shipped as one unit.
///
/// The pack is the common construction scope of a filter pair (typically an
/// input-hook [`Classifier`] and an output-hook [`Rewriter`]): per-bundle
/// state rides an annotation slot the pair declares with
/// [`slot!`](crate::slot), and cross-bundle node state rides a shared inner
/// (e.g. an `Arc<Mutex<...>>`) minted in the same scope.
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
/// node-wide payload peek at `build()`; the base methods declare 0. Until the
/// streaming ingress gate lands (Phase 3, `filter_subsystem_design.md`),
/// the declaration is recorded but has no effect: hooks run with the full
/// bundle resident and the peek is unconsumed.
#[must_use = "a filter pack registers nothing until it is handed to BpaBuilder::add_filters"]
pub struct FilterPack {
    name: Arc<str>,
    ingress_verifiers: Vec<Pending<VerifierEntry>>,
    originate_verifiers: Vec<Pending<VerifierEntry>>,
    egress_verifiers: Vec<VerifierEntry>,
    deliver_verifiers: Vec<VerifierEntry>,
    ingress_classifiers: Vec<Pending<ClassifierEntry>>,
    originate_classifiers: Vec<Pending<ClassifierEntry>>,
    egress_rewriters: Vec<RewriterEntry>,
    deliver_rewriters: Vec<RewriterEntry>,
}

// Manual impl: the filters are trait objects, so the pack shows its name
// and how many registrations each hook holds.
impl Debug for FilterPack {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterPack")
            .field("name", &self.name)
            .field(
                "ingress",
                &(self.ingress_verifiers.len() + self.ingress_classifiers.len()),
            )
            .field(
                "originate",
                &(self.originate_verifiers.len() + self.originate_classifiers.len()),
            )
            .field(
                "egress",
                &(self.egress_rewriters.len() + self.egress_verifiers.len()),
            )
            .field(
                "deliver",
                &(self.deliver_rewriters.len() + self.deliver_verifiers.len()),
            )
            .finish_non_exhaustive()
    }
}

impl FilterPack {
    /// Creates a pack named `name` — the prefix of its registrations'
    /// diagnostic labels, never required to be unique.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
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
    ///
    /// Recorded but inert until the Phase 3 streaming ingress gate lands
    /// (`filter_subsystem_design.md`): hooks currently run with the full
    /// bundle resident.
    pub fn ingress_verifier_with_peek(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
        peek: usize,
    ) -> &mut Self {
        let entry = self.verifier_entry(label, verifier);
        self.ingress_verifiers.push(Pending { peek, entry });
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
    ///
    /// Recorded but inert until the Phase 3 streaming ingress gate lands
    /// (`filter_subsystem_design.md`): hooks currently run with the full
    /// bundle resident.
    pub fn originate_verifier_with_peek(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
        peek: usize,
    ) -> &mut Self {
        let entry = self.verifier_entry(label, verifier);
        self.originate_verifiers.push(Pending { peek, entry });
        self
    }

    /// Appends a [`Verifier`] to the Egress chain. No peek variant: the
    /// bundle's bytes are resident at the output hooks.
    pub fn egress_verifier(&mut self, label: &str, verifier: impl Verifier + 'static) -> &mut Self {
        let entry = self.verifier_entry(label, verifier);
        self.egress_verifiers.push(entry);
        self
    }

    /// Appends a [`Verifier`] to the Deliver chain. No peek variant: the
    /// bundle's bytes are resident at the output hooks.
    pub fn deliver_verifier(
        &mut self,
        label: &str,
        verifier: impl Verifier + 'static,
    ) -> &mut Self {
        let entry = self.verifier_entry(label, verifier);
        self.deliver_verifiers.push(entry);
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
    ///
    /// Recorded but inert until the Phase 3 streaming ingress gate lands
    /// (`filter_subsystem_design.md`): hooks currently run with the full
    /// bundle resident.
    pub fn ingress_classifier_with_peek(
        &mut self,
        label: &str,
        classifier: impl Classifier + 'static,
        peek: usize,
    ) -> &mut Self {
        let entry = self.classifier_entry(label, classifier);
        self.ingress_classifiers.push(Pending { peek, entry });
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
    ///
    /// Recorded but inert until the Phase 3 streaming ingress gate lands
    /// (`filter_subsystem_design.md`): hooks currently run with the full
    /// bundle resident.
    pub fn originate_classifier_with_peek(
        &mut self,
        label: &str,
        classifier: impl Classifier + 'static,
        peek: usize,
    ) -> &mut Self {
        let entry = self.classifier_entry(label, classifier);
        self.originate_classifiers.push(Pending { peek, entry });
        self
    }

    /// Appends a [`Rewriter`] to the Egress chain.
    pub fn egress_rewriter(&mut self, label: &str, rewriter: impl Rewriter + 'static) -> &mut Self {
        let entry = self.rewriter_entry(label, rewriter);
        self.egress_rewriters.push(entry);
        self
    }

    /// Appends a [`Rewriter`] to the Deliver chain.
    pub fn deliver_rewriter(
        &mut self,
        label: &str,
        rewriter: impl Rewriter + 'static,
    ) -> &mut Self {
        let entry = self.rewriter_entry(label, rewriter);
        self.deliver_rewriters.push(entry);
        self
    }

    fn label(&self, suffix: &str) -> Arc<str> {
        format!("{}.{suffix}", self.name).into()
    }

    // The three labelled-entry constructors every registration body funnels
    // through — the one place each entry literal is spelled.
    fn verifier_entry(&self, label: &str, verifier: impl Verifier + 'static) -> VerifierEntry {
        VerifierEntry {
            label: self.label(label),
            verifier: Box::new(verifier),
        }
    }

    fn classifier_entry(
        &self,
        label: &str,
        classifier: impl Classifier + 'static,
    ) -> ClassifierEntry {
        ClassifierEntry {
            label: self.label(label),
            classifier: Box::new(classifier),
        }
    }

    fn rewriter_entry(&self, label: &str, rewriter: impl Rewriter + 'static) -> RewriterEntry {
        RewriterEntry {
            label: self.label(label),
            rewriter: Box::new(rewriter),
        }
    }
}

// Input-hook pending record: the declared peek rides beside the entry
// until freeze folds it into the node-wide P.
pub(super) struct Pending<E> {
    peek: usize,
    entry: E,
}

impl<E> Pending<E> {
    pub(super) fn into_parts(self) -> (usize, E) {
        (self.peek, self.entry)
    }
}
