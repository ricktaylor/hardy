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
use fuzz_macros::instrument;
use hardy_bpa_api::metadata;
use hardy_bpv7::prelude as bpv7;
use trace_err::*;
use tracing::{error, info, trace, warn};

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
        index: Arc<Mutex<HashMap<bpv7::BundleId, String>>>,
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
            let mut index = self.index.lock().unwrap();
            if index.get(&bundle.id).is_some() {
                return Ok(false);
            }
            index.insert(bundle.id.clone(), metadata.storage_name.clone());

            self.metadata.lock().unwrap().insert(
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
            bundle: &bpv7::Bundle,
            data: Vec<u8>,
            status: metadata::BundleStatus,
            received_at: Option<time::OffsetDateTime>,
        ) -> Result<Option<metadata::Metadata>, Error> {
            // Calculate hash
            let hash = self.hash(&data);

            // Write to bundle storage
            let storage_name = self.store_data(data).await?;

            // Compose metadata
            let metadata = metadata::Metadata {
                status,
                storage_name,
                hash: hash.to_vec(),
                received_at,
            };

            // Write to metadata store
            match self.store_metadata(&metadata, bundle).await {
                Ok(true) => Ok(Some(metadata)),
                Ok(false) => {
                    // We have a duplicate, remove the duplicate from the bundle store
                    let _ = self.delete_data(&metadata.storage_name).await;
                    Ok(None)
                }
                Err(e) => {
                    // This is just bad, we can't really claim to have stored the bundle,
                    // so just cleanup and get out
                    let _ = self.delete_data(&metadata.storage_name).await;
                    Err(e)
                }
            }
        }

        pub async fn get_waiting_bundles(
            &self,
            limit: time::OffsetDateTime,
        ) -> Result<Vec<(metadata::Bundle, time::OffsetDateTime)>, Error> {
            let mut metadata = self.metadata.lock().unwrap();

            // Drop all tombstones and collect waiting
            let mut waiting = Vec::new();
            metadata.retain(|_, bundle| {
                match bundle.metadata.status {
                    metadata::BundleStatus::Tombstone
                        if bundle.expiry() + time::Duration::seconds(10)
                            < time::OffsetDateTime::now_utc() =>
                    {
                        self.index.lock().unwrap().remove(&bundle.bundle.id);
                        return false;
                    }
                    metadata::BundleStatus::Waiting(until) if until <= limit => {
                        waiting.push((bundle.clone(), until));
                    }
                    _ => {}
                }
                true
            });
            Ok(waiting)
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
            self.metadata
                .lock()
                .unwrap()
                .get_mut(storage_name)
                .unwrap()
                .metadata
                .status = status.clone();
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
