use alloc::sync::Arc;
use bytes::Bytes;
use core::ops::Range;
use futures::{FutureExt, join, select_biased};
use hardy_bpv7::bundle::Id as Bpv7Id;
use hardy_bpv7::editor::Editor;
use time::OffsetDateTime;
use trace_err::*;
use tracing::{debug, error};

use super::{HashMap, Store};
use crate::bundle::{Bundle, BundleStatus};

pub enum ReassemblyResult {
    /// Not all sibling fragments have arrived; fragment data is still in storage.
    /// Caller should transition the bundle to `AduFragment` and wait.
    NotReady,
    /// All fragments arrived and the ADU was successfully reassembled.
    Done(Arc<str>, Bytes),
    /// All fragments arrived but reassembly failed (corrupt/misaligned data).
    /// Fragment data has already been deleted; caller should drop the trigger bundle.
    Failed,
}

struct FragmentSet {
    received_at: OffsetDateTime,
    adus: HashMap<u64, (Bpv7Id, Arc<str>, Range<usize>)>,
}

impl Store {
    pub async fn adu_reassemble(&self, bundle: &Bundle) -> ReassemblyResult {
        let status = BundleStatus::AduFragment {
            source: bundle.bundle.id.source.clone(),
            timestamp: bundle.bundle.id.timestamp.clone(),
        };

        let Some(fragments) = self.poll_fragments(bundle, &status).await else {
            return ReassemblyResult::NotReady;
        };

        let result = self.reassemble(&fragments).await;

        // Remove the fragments from bundle_storage even if we failed to fully reassemble
        for (bundle_id, storage_name, _) in fragments.adus.values() {
            self.delete_data(storage_name).await;
            self.tombstone_metadata(bundle_id).await;
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&status)).decrement(1.0);
        }

        // TODO: It would be good to capture the aggregate received at value across all the fragments, and use that as the received_at for the reassembled bundle

        match result {
            Some((storage_name, data)) => ReassemblyResult::Done(storage_name, data),
            None => ReassemblyResult::Failed,
        }
    }

    async fn poll_fragments(&self, bundle: &Bundle, status: &BundleStatus) -> Option<FragmentSet> {
        // Poll the store for the other fragments
        let cancel_token = self.tasks.cancel_token().clone();

        let source = bundle.bundle.id.source.clone();
        let timestamp = bundle.bundle.id.timestamp.clone();
        let fragment_info = bundle
            .bundle
            .id
            .fragment_info
            .as_ref()
            .trace_expect("Unfragmented bundle got into adu_reassemble?!");

        let total_adu_len = fragment_info.total_adu_length;
        let payload = &bundle
            .bundle
            .blocks
            .get(&1)
            .trace_expect("Bundle without payload?!")
            .payload_range();

        let mut adu_totals = payload.len() as u64;
        let mut results = FragmentSet {
            received_at: bundle.metadata.read_only.received_at,
            adus: [(
                fragment_info.offset,
                (
                    bundle.bundle.id.clone(),
                    bundle
                        .metadata
                        .storage_name
                        .clone()
                        .trace_expect("Invalid bundle in reassembly?!"),
                    payload.clone(),
                ),
            )]
            .into(),
        };

        let (tx, rx) = flume::bounded::<Bundle>(16);

        join!(
            // Producer: poll for fragment bundles
            async {
                let _ = self
                    .metadata_storage
                    .poll_adu_fragments(tx, status)
                    .await
                    .inspect_err(|e| error!("Failed to poll store for fragmented bundles: {e}"));
                // When tx is dropped, consumer will see channel close and return result
            },
            // Consumer: collect fragments
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => {
                            let Ok(bundle) = bundle else {
                                // Done (>= is just so we can capture invalid bundles and handle them at re-dispatch)
                                break (adu_totals >= total_adu_len).then_some(results);
                            };

                            if source == bundle.bundle.id.source &&
                                timestamp == bundle.bundle.id.timestamp &&
                                let Some(fragment_info) = &bundle.bundle.id.fragment_info
                            {
                                let payload = &bundle
                                    .bundle
                                    .blocks
                                    .get(&1)
                                    .trace_expect("Bundle fragment without payload?!")
                                    .payload_range();

                                adu_totals = adu_totals.saturating_add(payload.len() as u64);

                                results.received_at = results.received_at.min(bundle.metadata.read_only.received_at);
                                results.adus.insert(fragment_info.offset,
                                    (
                                        bundle.bundle.id,
                                        bundle.metadata
                                            .storage_name
                                            .trace_expect("Invalid bundle in reassembly?!"),
                                        payload.clone()
                                    )
                                );
                            }
                        },
                        _ = cancel_token.cancelled().fuse() => {
                            break None;
                        }
                    }
                }
            }
        ).1
    }

    async fn reassemble(&self, results: &FragmentSet) -> Option<(Arc<str>, Bytes)> {
        let first = results.adus.get(&0).or_else(|| {
            debug!(
                "Series of fragments with no offset 0 fragment found: {:?}",
                results.adus.values().next().map(|v| &v.0)
            );
            None
        })?;

        let bundle = self.get_metadata(&first.0).await?;
        let old_data = self
            .load_data(
                bundle
                    .metadata
                    .storage_name
                    .as_ref()
                    .trace_expect("Invalid bundle in reassembly?!"),
            )
            .await?;

        let total_adu_length = first
            .0
            .fragment_info
            .as_ref()
            .trace_expect("Fragment 0 missing fragment_info in reassembly?!")
            .total_adu_length;

        // Reassemble payload by writing each fragment at its ADU offset.
        // Iteration order does not matter. Each fragment is placed by offset.
        // TODO: There's a lot of mem copies going on here!
        let adu_len = total_adu_length as usize;
        let mut new_data: Vec<u8> = vec![0; adu_len];
        let mut bytes_written: u64 = 0;

        for (bundle_id, storage_name, payload) in results.adus.values() {
            let fi = bundle_id
                .fragment_info
                .as_ref()
                .trace_expect("Fragment missing fragment_info in reassembly?!");
            if fi.total_adu_length != total_adu_length {
                debug!(
                    "Total ADU length mismatch during fragment reassembly detected: {bundle_id}"
                );
                return None;
            }

            let offset = fi.offset as usize;
            let len = payload.len();
            if offset.saturating_add(len) > adu_len {
                debug!("Fragment extends beyond total ADU length: {bundle_id}");
                return None;
            }

            let adu = self.load_data(storage_name).await?.slice(payload.clone());
            new_data[offset..offset + len].copy_from_slice(adu.as_ref());
            bytes_written = bytes_written.saturating_add(len as u64);
        }

        if bytes_written != total_adu_length {
            debug!(
                "Total reassembled ADU does not match fragment info: {:?}",
                first.0
            );
            return None;
        }

        // Rewrite primary block
        let mut editor = Editor::new(&bundle.bundle, &old_data);
        editor = match editor.with_fragment_info(None) {
            Ok(e) => e,
            Err((_, e)) => {
                debug!("Failed to clear fragment info: {e}");
                return None;
            }
        };

        // Now rebuild
        let new_data = match editor.update_block(1) {
            Err((_, e)) => {
                debug!("Missing payload block?: {e}");
                return None;
            }
            Ok(b) => match b.with_data(new_data.into()).rebuild().rebuild() {
                Err(e) => {
                    debug!("Failed to rebuild bundle: {e}");
                    return None;
                }
                Ok(new_data) => new_data,
            },
        };

        // Write the rewritten bundle now for safety
        let new_data = Bytes::from(new_data);
        let new_storage_name = self.save_data(&new_data).await;
        Some((new_storage_name, new_data))
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::bundle::FragmentInfo;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    use super::*;
    use crate::bundle::BundleMetadata;
    use crate::storage::{self, bundle_mem::BundleMemStorage, metadata_mem::MetadataMemStorage};

    fn make_store() -> Store {
        Store::new(
            None,
            storage::DEFAULT_MAX_CACHED_BUNDLE_SIZE,
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(MetadataMemStorage::new(&Default::default())),
            Arc::new(BundleMemStorage::new(&Default::default())),
        )
    }

    fn make_id(source: &str, ts: &CreationTimestamp, offset: u64, total_adu_length: u64) -> Bpv7Id {
        Bpv7Id {
            source: source.parse().unwrap(),
            timestamp: ts.clone(),
            fragment_info: Some(FragmentInfo {
                offset,
                total_adu_length,
            }),
        }
    }

    async fn store_bytes(store: &Store, data: &[u8]) -> Arc<str> {
        store.save_data(&Bytes::from(data.to_vec())).await
    }

    async fn store_fragment_metadata(store: &Store, id: &Bpv7Id, storage_name: &Arc<str>) {
        let bundle = Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: id.clone(),
                destination: "ipn:0.2.1".parse().unwrap(),
                lifetime: core::time::Duration::from_secs(3600),
                ..Default::default()
            },
            metadata: BundleMetadata {
                storage_name: Some(storage_name.clone()),
                ..Default::default()
            },
        };
        store.insert_metadata(&bundle).await;
    }

    #[tokio::test]
    async fn reassemble_rejects_missing_first_fragment() {
        let store = make_store();
        let ts = CreationTimestamp::now();
        let id = make_id("ipn:0.1.1", &ts, 5, 10);

        let fragments = FragmentSet {
            received_at: OffsetDateTime::now_utc(),
            adus: [(5, (id, "unused".into(), 0..5))].into(),
        };

        let result = store.reassemble(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject FragmentSet without offset 0"
        );
    }

    #[tokio::test]
    async fn reassemble_rejects_adu_length_mismatch() {
        let store = make_store();
        let ts = CreationTimestamp::now();

        let data = b"HelloWorld";
        let name0 = store_bytes(&store, data).await;
        let name1 = store_bytes(&store, data).await;

        let id0 = make_id("ipn:0.1.1", &ts, 0, 10);
        let id1 = make_id("ipn:0.1.1", &ts, 5, 99); // mismatched total

        store_fragment_metadata(&store, &id0, &name0).await;

        let fragments = FragmentSet {
            received_at: OffsetDateTime::now_utc(),
            adus: [(0, (id0, name0, 0..5)), (5, (id1, name1, 0..5))].into(),
        };

        let result = store.reassemble(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject mismatched total_adu_length"
        );
    }

    #[tokio::test]
    async fn reassemble_rejects_fragment_beyond_bounds() {
        let store = make_store();
        let ts = CreationTimestamp::now();

        let data = b"HelloWorld";
        let name0 = store_bytes(&store, data).await;
        let name1 = store_bytes(&store, data).await;

        // Fragment 1 at offset 8 with 5 bytes → 8+5=13 > 10
        let id0 = make_id("ipn:0.1.1", &ts, 0, 10);
        let id1 = make_id("ipn:0.1.1", &ts, 8, 10);

        store_fragment_metadata(&store, &id0, &name0).await;

        let fragments = FragmentSet {
            received_at: OffsetDateTime::now_utc(),
            adus: [(0, (id0, name0, 0..5)), (8, (id1, name1, 0..5))].into(),
        };

        let result = store.reassemble(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject fragment extending beyond ADU length"
        );
    }

    #[tokio::test]
    async fn reassemble_rejects_incomplete_coverage() {
        let store = make_store();
        let ts = CreationTimestamp::now();

        let data = b"HelloWorld";
        let name0 = store_bytes(&store, data).await;

        // Only 5 bytes written but total is 10
        let id0 = make_id("ipn:0.1.1", &ts, 0, 10);

        store_fragment_metadata(&store, &id0, &name0).await;

        let fragments = FragmentSet {
            received_at: OffsetDateTime::now_utc(),
            adus: [(0, (id0, name0, 0..5))].into(),
        };

        let result = store.reassemble(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject when bytes_written < total_adu_length"
        );
    }

    /// Two fragments covering the full ADU reassemble into a complete bundle.
    #[tokio::test]
    async fn reassemble_basic_happy_path() {
        let store = make_store();
        let ts = CreationTimestamp::now();
        let source: hardy_bpv7::eid::Eid = "ipn:0.1.1".parse().unwrap();
        let dest: hardy_bpv7::eid::Eid = "ipn:0.2.1".parse().unwrap();
        let payload = b"HelloWorld"; // 10 bytes, split into 5+5

        // Build a complete bundle
        let (complete_bundle, complete_data) =
            hardy_bpv7::builder::Builder::new(source.clone(), dest.clone())
                .with_payload(std::borrow::Cow::Borrowed(&payload[..]))
                .build(ts.clone())
                .unwrap();

        // Create fragment 0: offset=0, total=10, payload="Hello"
        let frag0_data = Editor::new(&complete_bundle, &complete_data)
            .with_fragment_info(Some(FragmentInfo {
                offset: 0,
                total_adu_length: 10,
            }))
            .map_err(|(_, e)| e)
            .unwrap()
            .update_block(1)
            .map_err(|(_, e)| e)
            .unwrap()
            .with_data(std::borrow::Cow::Borrowed(&b"Hello"[..]))
            .rebuild()
            .rebuild()
            .unwrap();

        // Create fragment 1: offset=5, total=10, payload="World"
        let frag1_data = Editor::new(&complete_bundle, &complete_data)
            .with_fragment_info(Some(FragmentInfo {
                offset: 5,
                total_adu_length: 10,
            }))
            .map_err(|(_, e)| e)
            .unwrap()
            .update_block(1)
            .map_err(|(_, e)| e)
            .unwrap()
            .with_data(std::borrow::Cow::Borrowed(&b"World"[..]))
            .rebuild()
            .rebuild()
            .unwrap();

        // Parse fragments back to get Bundle structs with correct block ranges
        let bundle0 =
            hardy_bpv7::bundle::ParsedBundle::parse(&frag0_data, hardy_bpv7::bpsec::no_keys)
                .unwrap()
                .bundle;
        let bundle1 =
            hardy_bpv7::bundle::ParsedBundle::parse(&frag1_data, hardy_bpv7::bpsec::no_keys)
                .unwrap()
                .bundle;

        // Store fragment data
        let name0 = store_bytes(&store, &frag0_data).await;
        let name1 = store_bytes(&store, &frag1_data).await;

        // Store fragment 0 metadata with the full parsed Bundle (reassemble
        // passes it to Editor::new which needs the blocks map, not just the ID)
        let meta_bundle = Bundle {
            bundle: bundle0.clone(),
            metadata: BundleMetadata {
                storage_name: Some(name0.clone()),
                ..Default::default()
            },
        };
        store.insert_metadata(&meta_bundle).await;

        // Get payload ranges from the parsed bundles
        let payload0_range = bundle0.blocks.get(&1).unwrap().payload_range();
        let payload1_range = bundle1.blocks.get(&1).unwrap().payload_range();

        let fragments = FragmentSet {
            received_at: OffsetDateTime::now_utc(),
            adus: [
                (0, (bundle0.id.clone(), name0, payload0_range)),
                (5, (bundle1.id.clone(), name1, payload1_range)),
            ]
            .into(),
        };

        let result = store.reassemble(&fragments).await;
        assert!(result.is_some(), "Reassembly should succeed");

        let (_, reassembled_data) = result.unwrap();

        // Parse the reassembled bundle and verify
        let reassembled_bundle =
            hardy_bpv7::bundle::ParsedBundle::parse(&reassembled_data, hardy_bpv7::bpsec::no_keys)
                .unwrap()
                .bundle;

        // Should no longer be a fragment
        assert!(
            reassembled_bundle.id.fragment_info.is_none(),
            "Reassembled bundle should not have fragment_info"
        );

        // Extract and verify the payload
        let payload_block = reassembled_bundle.blocks.get(&1).unwrap();
        let payload_range = payload_block.payload_range();
        let reassembled_payload = &reassembled_data[payload_range];
        assert_eq!(
            reassembled_payload, payload,
            "Reassembled payload should be 'HelloWorld'"
        );
    }
}
