#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

mod dispatcher;
mod rib;

pub mod bpa;
pub mod bundle;
pub mod cla;
pub mod config;
pub mod filters;
pub mod keys;
pub mod metadata;
pub mod node_ids;
pub mod policy;
pub mod routes;
pub mod services;
pub mod storage;

use alloc::sync::{Arc, Weak};
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "tracing")]
use tracing::instrument;

// Centralized collections for future no_std compatibility
// For no_std: HashMap/HashSet from hashbrown, BTreeMap/BTreeSet from alloc::collections
#[cfg(feature = "std")]
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map, hash_map};

#[cfg(not(feature = "std"))]
use hashbrown::{HashMap, HashSet, hash_map};

#[cfg(not(feature = "std"))]
use alloc::collections::{BTreeMap, BTreeSet, btree_map};

// Re-export for consistency
pub use bytes::Bytes;
pub use hardy_async::async_trait;
