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
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct Store {
        bundles: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
        metadata: Arc<Mutex<HashMap<String, metadata::Bundle>>>,
    }

    impl Store {
        pub fn new() -> Self {
            Self::default()
        }

        fn adler32(data: &[u8]) -> u32 {
            // Very dumb Adler-32
            let mut a = 1u32;
            let mut b = 0u32;
            for d in data {
                a = a.wrapping_add(*d as u32) % 65521;
                b = b.wrapping_add(a) % 65521;
            }
            (b << 16) | a
        }

        pub fn hash(&self, data: &[u8]) -> Vec<u8> {
            Self::adler32(data).to_be_bytes().to_vec()
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
            let mut bundles = self.bundles.lock().unwrap();
            let metadata = self.metadata.lock().unwrap();
            let mut hash = Self::adler32(&data);
            loop {
                let storage_name = hash.to_string();
                if metadata.contains_key(&storage_name) || bundles.contains_key(&storage_name) {
                    hash += 1;
                    continue;
                }

                bundles.insert(storage_name.clone(), Arc::new(data));
                break Ok(storage_name);
            }
        }

        pub async fn store_metadata(
            &self,
            metadata: &metadata::Metadata,
            bundle: &bpv7::Bundle,
        ) -> Result<bool, Error> {
            let mut metadatas = self.metadata.lock().unwrap();
            if metadatas
                .iter()
                .find(|(_, b)| b.bundle.id == bundle.id)
                .is_some()
            {
                return Ok(false);
            }

            metadatas.insert(
                metadata.storage_name.clone(),
                metadata::Bundle {
                    metadata: metadata.clone(),
                    bundle: bundle.clone(),
                },
            );
            Ok(true)
        }

        pub async fn load(
            &self,
            _bundle_id: &bpv7::BundleId,
        ) -> Result<Option<metadata::Bundle>, Error> {
            todo!()
        }

        pub async fn store(
            &self,
            _bundle: &bpv7::Bundle,
            _data: Vec<u8>,
            _status: metadata::BundleStatus,
            _received_at: Option<time::OffsetDateTime>,
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
            _metadata: &metadata::Metadata,
            _data: Vec<u8>,
        ) -> Result<metadata::Metadata, Error> {
            todo!()
        }

        pub async fn check_status(
            &self,
            _storage_name: &str,
        ) -> Result<Option<metadata::BundleStatus>, Error> {
            todo!()
        }

        pub async fn set_status(
            &self,
            storage_name: &str,
            status: &metadata::BundleStatus,
        ) -> Result<(), Error> {
            let mut metadata = self.metadata.lock().unwrap();
            metadata.get_mut(storage_name).unwrap().metadata.status = status.clone();
            Ok(())
        }

        pub async fn delete_data(&self, storage_name: &str) -> Result<(), Error> {
            self.bundles.lock().unwrap().remove(storage_name);
            Ok(())
        }

        pub async fn remove(&self, storage_name: &str) -> Result<(), Error> {
            self.set_status(storage_name, &metadata::BundleStatus::Tombstone)
                .await?;

            self.delete_data(storage_name).await
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
