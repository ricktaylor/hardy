use core::ops::Range;

use hardy_bpv7::bundle::Id as Bpv7Id;

use crate::{Arc, HashMap};

mod reassembler;

pub(crate) use reassembler::{Reassembler, ReassemblerResult};

/// A single collected fragment: its bundle ID, storage key, and payload byte range.
pub(crate) struct Fragment {
    pub id: Bpv7Id,
    pub storage_name: Arc<str>,
    pub payload_range: Range<usize>,
}

/// Collected fragments keyed by ADU offset.
pub(crate) struct FragmentSet(pub HashMap<u64, Fragment>);
