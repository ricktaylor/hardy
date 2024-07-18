pub mod app_registry;
pub mod cla_registry;
pub mod dispatcher;
pub mod fib;
pub mod ingress;
pub mod utils;

// This is the generic Error type used almost everywhere
type Error = Box<dyn std::error::Error + Send + Sync>;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

// This is the effective prelude
use hardy_bpa_api::metadata;
use hardy_bpv7::prelude as bpv7;
use trace_err::*;
use tracing::{error, info, instrument, trace, warn};

// Mock storage module
pub mod store {
    use super::*;
    use hardy_bpa_api::storage;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    static NEXT_NAME: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone, Default)]
    pub struct Store {
        bundles: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
        metadata: Arc<Mutex<HashMap<bpv7::BundleId, metadata::Bundle>>>,
    }

    impl Store {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn hash(&self, data: &[u8]) -> Vec<u8> {
            vec![0u8; 16]
        }

        pub async fn load_data(
            &self,
            storage_name: &str,
        ) -> Result<Option<storage::DataRef>, Error> {
            match self.bundles.lock().unwrap().get(storage_name) {
                None => Ok(None),
                Some(v) => Ok(Some(v.clone())),
            }
        }

        pub async fn store_data(&self, data: Vec<u8>) -> Result<String, Error> {
            let storage_name = format!(
                "bundle{}",
                NEXT_NAME.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );

            self.bundles
                .lock()
                .unwrap()
                .insert(storage_name.clone(), Arc::new(data));

            Ok(storage_name)
        }

        pub async fn store_metadata(
            &self,
            metadata: &metadata::Metadata,
            bundle: &bpv7::Bundle,
        ) -> Result<bool, Error> {
            todo!()
        }

        pub async fn load(
            &self,
            bundle_id: &bpv7::BundleId,
        ) -> Result<Option<metadata::Bundle>, Error> {
            todo!()
        }

        pub async fn store(
            &self,
            bundle: &bpv7::Bundle,
            data: Vec<u8>,
            status: metadata::BundleStatus,
            received_at: Option<time::OffsetDateTime>,
        ) -> Result<Option<metadata::Metadata>, Error> {
            todo!()
        }

        pub async fn get_waiting_bundles(
            &self,
            limit: time::OffsetDateTime,
        ) -> Result<Vec<(metadata::Bundle, time::OffsetDateTime)>, Error> {
            Ok(self
                .metadata
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, bundle)| {
                    if let metadata::BundleStatus::Waiting(until) = bundle.metadata.status {
                        if until <= limit {
                            Some((bundle.clone(), until))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect())
        }

        pub async fn poll_for_collection(
            &self,
            _destination: bpv7::Eid,
        ) -> Result<Vec<(String, time::OffsetDateTime)>, Error> {
            unimplemented!()
        }

        pub async fn replace_data(
            &self,
            metadata: &metadata::Metadata,
            data: Vec<u8>,
        ) -> Result<metadata::Metadata, Error> {
            todo!()
        }

        pub async fn check_status(
            &self,
            storage_name: &str,
        ) -> Result<Option<metadata::BundleStatus>, Error> {
            todo!()
        }

        pub async fn set_status(
            &self,
            storage_name: &str,
            status: &metadata::BundleStatus,
        ) -> Result<(), Error> {
            todo!()
        }

        pub async fn delete(&self, storage_name: &str) -> Result<(), Error> {
            todo!()
        }

        pub async fn remove(&self, storage_name: &str) -> Result<(), Error> {
            self.set_status(storage_name, &metadata::BundleStatus::Tombstone)
                .await
        }
    }
}

mod services {
    use super::*;

    pub fn from_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, Error> {
        Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)
            .map_err(time::error::ComponentRange::from)?
            + time::Duration::nanoseconds(t.nanos.into()))
    }

    pub fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
        let t = t - time::OffsetDateTime::UNIX_EPOCH;
        prost_types::Timestamp {
            seconds: t.whole_seconds(),
            nanos: t.subsec_nanoseconds(),
        }
    }
}
