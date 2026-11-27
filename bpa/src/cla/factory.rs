//! Constructing a CLA against the terms the BPA settles for it.

use core::{
    fmt::{self, Debug, Formatter},
    num::{NonZeroU32, NonZeroU64},
};

use alloc::boxed::Box;

use super::{Cla, ClaInit, Result, Sink};
use crate::{Arc, node_ids::NodeIds};

/// What the BPA settled against a CLA's [`ClaInit`] declarations.
///
/// Fixed for the life of the registration: a CLA whose circumstances
/// change unregisters and registers again.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Terms {
    /// The lanes the CLA transmits on, indexed `0..n`, as it declared
    /// them; `None` is the infinite pool, where every transfer travels on
    /// a lane of its own.
    pub lane_count: Option<NonZeroU32>,

    /// The effective receive limit: the smaller of the BPA's configured
    /// cap and the CLA's own, or `None` where neither declared one.
    ///
    /// The same value [`Cla::on_register`] receives. A CLA propagates a
    /// `Some` limit into its own transfer limits, such as an advertised
    /// transfer MRU, so an over-limit transfer is never offered.
    pub max_bundle_size: Option<NonZeroU64>,
}

impl Terms {
    // Settles a CLA's declarations against the BPA's own receive limit:
    // the smaller limit wins, and a side that declared none defers to the
    // other. Lanes are the CLA's own count, so they pass through.
    pub(crate) fn negotiate(init: &ClaInit, max_bundle_size: Option<NonZeroU64>) -> Self {
        Self {
            lane_count: init.lane_count,
            max_bundle_size: match (max_bundle_size, init.max_bundle_size) {
                (Some(bpa), Some(cla)) => Some(bpa.min(cla)),
                (bpa, cla) => bpa.or(cla),
            },
        }
    }
}

/// Everything the BPA hands a [`Factory`] when it builds the CLA.
pub struct Context {
    /// The channel back to the BPA.
    ///
    /// The same channel [`Cla::on_register`] delivers: a CLA keeps
    /// whichever handle suits it and may drop the other. It unregisters
    /// when it drops the last of them. The sink is not live until the
    /// build returns, so [`Factory::build`] must not use it.
    pub sink: Arc<dyn Sink>,

    /// The BPA's own node identifiers.
    pub node_ids: Arc<NodeIds>,

    /// What the BPA settled against the CLA's declarations.
    pub terms: Terms,
}

impl Context {
    /// Mints a context. The BPA does this; a CLA's tests mint one to build
    /// the CLA under the terms they exercise.
    pub fn new(sink: Arc<dyn Sink>, node_ids: Arc<NodeIds>, terms: Terms) -> Self {
        Self {
            sink,
            node_ids,
            terms,
        }
    }
}

impl Debug for Context {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("node_ids", &self.node_ids)
            .field("terms", &self.terms)
            .finish_non_exhaustive()
    }
}

/// Builds a CLA when the BPA registers it.
///
/// [`BpaBuilder::cla`](crate::builder::BpaBuilder::cla) takes a factory,
/// and the BPA runs it at [`Bpa::start`](crate::bpa::Bpa::start): it reads
/// [`request`](Self::request), settles the [`Terms`], and calls
/// [`build`](Self::build) with the sink in hand. Then
/// [`Cla::on_register`] runs as for any CLA.
///
/// Anything that can fail on the environment, such as binding a listener
/// or loading TLS material, happens before the factory is handed over, so
/// the embedder sees the error where it configures the CLA. An already
/// built CLA is therefore a factory whose `build` returns it: `Tcpclv4`
/// and the file CLA are, and [`Registration`] pairs any other built CLA
/// with its declarations.
pub trait Factory: Send {
    /// The CLA's set-once declarations, read once, before the build.
    fn request(&self) -> ClaInit {
        ClaInit::default()
    }

    /// Builds the CLA against the terms the BPA settled.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of `cx.sink`.
    /// `Err` means the CLA cannot work under the settled terms.
    fn build(self, cx: Context) -> Result<Arc<dyn Cla>>;
}

/// A built CLA and its set-once declarations: the [`Factory`] for a CLA
/// that needs nothing from the [`Context`].
///
/// There is deliberately no conversion from a bare CLA, which would
/// register it with `ClaInit::default()` and silently drop whatever it
/// declares.
pub struct Registration {
    /// The CLA implementation.
    pub cla: Arc<dyn Cla>,

    /// The CLA's declarations, snapshotted at registration.
    pub init: ClaInit,
}

impl Registration {
    /// A registration with explicit declarations.
    pub fn new(cla: Arc<dyn Cla>, init: ClaInit) -> Self {
        Self { cla, init }
    }
}

impl Factory for Registration {
    fn request(&self) -> ClaInit {
        self.init
    }

    /// Returns the CLA, which takes its sink in [`Cla::on_register`].
    fn build(self, _cx: Context) -> Result<Arc<dyn Cla>> {
        Ok(self.cla)
    }
}

// A factory with its declarations read and its type erased, so the
// registries can hold factories of different types.
pub struct BoxFactory {
    pub init: ClaInit,
    build: Box<dyn FnOnce(Context) -> Result<Arc<dyn Cla>> + Send>,
}

impl BoxFactory {
    pub fn new<F: Factory + 'static>(factory: F) -> Self {
        Self {
            init: factory.request(),
            build: Box::new(|cx| factory.build(cx)),
        }
    }

    pub fn build(self, cx: Context) -> Result<Arc<dyn Cla>> {
        (self.build)(cx)
    }
}
