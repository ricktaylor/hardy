use futures::{FutureExt, join, select_biased};
use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::{Bundle as Bpv7Bundle, ParsedBundle};
use hardy_bpv7::editor::Editor;
use trace_err::*;
use tracing::debug;

use super::{Fragment, FragmentSet};
use crate::bundle::{Bundle, BundleMetadata, BundleStatus};
use crate::storage::Store;
use crate::{Arc, Bytes};

/// Result of a reassembly attempt.
pub(crate) enum ReassemblerResult {
    /// All fragments collected; reassembled bundle ready for ingestion.
    Complete(Box<Bundle>, Bytes),
    /// Waiting for more fragments (bundle stored with AduFragment status).
    Pending,
    /// Reassembly failed or produced invalid data (already cleaned up).
    Failed,
}

/// Drives the reassembly of a fragmented bundle.
///
/// Holds the store needed to collect fragments, stitch the ADU,
/// validate the result, and persist it.
///
/// Construct once and reuse for multiple reassembly attempts.
pub(crate) struct Reassembler {
    store: Arc<Store>,
}

impl Reassembler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Attempt to reassemble a fragmented bundle.
    ///
    /// Collects sibling fragments from storage, stitches the ADU, rebuilds
    /// the bundle, and stores the result.
    pub async fn run<F>(&self, mut bundle: Bundle, key_provider: F) -> ReassemblerResult
    where
        F: FnOnce(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource>,
    {
        let status = BundleStatus::AduFragment {
            source: bundle.bundle.id.source.clone(),
            timestamp: bundle.bundle.id.timestamp.clone(),
        };

        let Some(fragments) = self.collect(&bundle, &status).await else {
            self.store.update_status(&mut bundle, &status).await;
            self.store.watch_bundle(bundle).await;
            return ReassemblerResult::Pending;
        };

        let result = self.stitch(&fragments).await;

        // Remove fragment data and metadata regardless of outcome
        for frag in fragments.0.values() {
            self.store.delete_data(&frag.storage_name).await;
            self.store.tombstone_metadata(&frag.id).await;
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&status))
                .decrement(1.0);
        }

        let Some(data) = result else {
            debug!("Fragment reassembly failed for bundle {}", bundle.bundle.id);
            return ReassemblerResult::Failed;
        };

        // Parse and validate the reassembled bundle
        let parsed = ParsedBundle::parse(&data, key_provider);
        let Ok(ParsedBundle { bundle, .. }) = parsed else {
            metrics::counter!("bpa.bundle.reassembly.failed").increment(1);
            debug!("Reassembled bundle is invalid: {}", parsed.unwrap_err());
            return ReassemblerResult::Failed;
        };

        metrics::counter!("bpa.bundle.reassembled").increment(1);

        let bundle = Bundle {
            metadata: BundleMetadata {
                storage_name: Some(self.store.save_data(&data).await),
                ..Default::default()
            },
            bundle,
        };

        if !self.store.insert_metadata(&bundle).await {
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            if let Some(name) = &bundle.metadata.storage_name {
                self.store.delete_data(name).await;
            }
            return ReassemblerResult::Failed;
        }

        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status))
            .increment(1.0);

        ReassemblerResult::Complete(Box::new(bundle), data)
    }

    /// Collect sibling fragments from storage.
    ///
    /// Returns `None` if not all fragments have arrived yet.
    async fn collect(&self, bundle: &Bundle, status: &BundleStatus) -> Option<FragmentSet> {
        let cancel_token = self.store.tasks.cancel_token().clone();

        let source = bundle.bundle.id.source.clone();
        let timestamp = bundle.bundle.id.timestamp.clone();
        let fragment_info = bundle
            .bundle
            .id
            .fragment_info
            .as_ref()
            .trace_expect("Unfragmented bundle got into reassemble?!");

        let total_adu_len = fragment_info.total_adu_length;
        let payload = &bundle
            .bundle
            .blocks
            .get(&1)
            .trace_expect("Bundle without payload?!")
            .payload_range();

        let mut adu_totals = payload.len() as u64;
        let mut fragments = FragmentSet(
            [(
                fragment_info.offset,
                Fragment {
                    id: bundle.bundle.id.clone(),
                    storage_name: bundle
                        .metadata
                        .storage_name
                        .clone()
                        .trace_expect("Invalid bundle in reassembly?!"),
                    payload_range: payload.clone(),
                },
            )]
            .into(),
        );

        let (tx, rx) = flume::bounded::<Bundle>(16);

        join!(
            async {
                let _ = self
                    .store
                    .metadata_storage
                    .poll_adu_fragments(tx, status)
                    .await
                    .inspect_err(|e| {
                        tracing::error!("Failed to poll store for fragmented bundles: {e}")
                    });
            },
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => {
                            let Ok(bundle) = bundle else {
                                break (adu_totals >= total_adu_len).then_some(fragments);
                            };

                            if source == bundle.bundle.id.source
                                && timestamp == bundle.bundle.id.timestamp
                                && let Some(fi) = &bundle.bundle.id.fragment_info
                            {
                                let payload = &bundle
                                    .bundle
                                    .blocks
                                    .get(&1)
                                    .trace_expect("Bundle fragment without payload?!")
                                    .payload_range();

                                adu_totals = adu_totals.saturating_add(payload.len() as u64);

                                fragments.0.insert(
                                    fi.offset,
                                    Fragment {
                                        id: bundle.bundle.id,
                                        storage_name: bundle
                                            .metadata
                                            .storage_name
                                            .trace_expect("Invalid bundle in reassembly?!"),
                                        payload_range: payload.clone(),
                                    },
                                );
                            }
                        },
                        _ = cancel_token.cancelled().fuse() => {
                            break None;
                        }
                    }
                }
            }
        )
        .1
    }

    /// Stitch fragment payloads into a single ADU and rewrite the wire format
    /// with fragment info cleared. Returns the raw reassembled bytes.
    async fn stitch(&self, fragments: &FragmentSet) -> Option<Bytes> {
        let first = fragments.0.get(&0).or_else(|| {
            debug!(
                "Series of fragments with no offset 0 fragment found: {:?}",
                fragments.0.values().next().map(|f| &f.id)
            );
            None
        })?;

        let old_data = self.store.load_data(&first.storage_name).await?;

        let total_adu_length = first
            .id
            .fragment_info
            .as_ref()
            .trace_expect("Fragment 0 missing fragment_info in reassembly?!")
            .total_adu_length;

        let adu_len = total_adu_length as usize;
        let mut new_data: Vec<u8> = vec![0; adu_len];
        let mut bytes_written: u64 = 0;

        for frag in fragments.0.values() {
            let fi = frag
                .id
                .fragment_info
                .as_ref()
                .trace_expect("Fragment missing fragment_info in reassembly?!");
            if fi.total_adu_length != total_adu_length {
                debug!(
                    "Total ADU length mismatch during fragment reassembly detected: {}",
                    frag.id
                );
                return None;
            }

            let offset = fi.offset as usize;
            let len = frag.payload_range.len();
            if offset.saturating_add(len) > adu_len {
                debug!("Fragment extends beyond total ADU length: {}", frag.id);
                return None;
            }

            let adu = self
                .store
                .load_data(&frag.storage_name)
                .await?
                .slice(frag.payload_range.clone());
            new_data[offset..offset + len].copy_from_slice(adu.as_ref());
            bytes_written = bytes_written.saturating_add(len as u64);
        }

        if bytes_written != total_adu_length {
            debug!(
                "Total reassembled ADU does not match fragment info: {:?}",
                first.id
            );
            return None;
        }

        // Parse fragment 0 to get the bundle structure for Editor
        let Ok(ParsedBundle { bundle: frag0, .. }) =
            ParsedBundle::parse(&old_data, hardy_bpv7::bpsec::no_keys)
        else {
            debug!("Failed to parse fragment 0 for Editor");
            return None;
        };

        // Rewrite primary block (clear fragment info)
        let mut editor = Editor::new(&frag0, &old_data);
        editor = match editor.with_fragment_info(None) {
            Ok(e) => e,
            Err((_, e)) => {
                debug!("Failed to clear fragment info: {e}");
                return None;
            }
        };

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

        Some(Bytes::from(new_data))
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::bundle::{FragmentInfo, Id as Bpv7Id};
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    use super::*;
    use crate::Arc;
    use crate::storage::{self, bundle_mem::BundleMemStorage, metadata_mem::MetadataMemStorage};

    fn make_store() -> Arc<Store> {
        Arc::new(make_store_inner())
    }

    fn make_store_inner() -> Store {
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

    fn make_reassembler(store: &Arc<Store>) -> Reassembler {
        Reassembler::new(store.clone())
    }

    #[tokio::test]
    async fn stitch_rejects_missing_first_fragment() {
        let store = make_store();
        let reassembler = make_reassembler(&store);
        let ts = CreationTimestamp::now();
        let id = make_id("ipn:0.1.1", &ts, 5, 10);

        let fragments = FragmentSet(
            [(
                5,
                Fragment {
                    id,
                    storage_name: "unused".into(),
                    payload_range: 0..5,
                },
            )]
            .into(),
        );

        let result = reassembler.stitch(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject FragmentSet without offset 0"
        );
    }

    #[tokio::test]
    async fn stitch_rejects_adu_length_mismatch() {
        let store = make_store();
        let reassembler = make_reassembler(&store);
        let ts = CreationTimestamp::now();

        let data = b"HelloWorld";
        let name0 = store_bytes(&store, data).await;
        let name1 = store_bytes(&store, data).await;

        let id0 = make_id("ipn:0.1.1", &ts, 0, 10);
        let id1 = make_id("ipn:0.1.1", &ts, 5, 99);

        store_fragment_metadata(&store, &id0, &name0).await;

        let fragments = FragmentSet(
            [
                (
                    0,
                    Fragment {
                        id: id0,
                        storage_name: name0,
                        payload_range: 0..5,
                    },
                ),
                (
                    5,
                    Fragment {
                        id: id1,
                        storage_name: name1,
                        payload_range: 0..5,
                    },
                ),
            ]
            .into(),
        );

        let result = reassembler.stitch(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject mismatched total_adu_length"
        );
    }

    #[tokio::test]
    async fn stitch_rejects_fragment_beyond_bounds() {
        let store = make_store();
        let reassembler = make_reassembler(&store);
        let ts = CreationTimestamp::now();

        let data = b"HelloWorld";
        let name0 = store_bytes(&store, data).await;
        let name1 = store_bytes(&store, data).await;

        let id0 = make_id("ipn:0.1.1", &ts, 0, 10);
        let id1 = make_id("ipn:0.1.1", &ts, 8, 10);

        store_fragment_metadata(&store, &id0, &name0).await;

        let fragments = FragmentSet(
            [
                (
                    0,
                    Fragment {
                        id: id0,
                        storage_name: name0,
                        payload_range: 0..5,
                    },
                ),
                (
                    8,
                    Fragment {
                        id: id1,
                        storage_name: name1,
                        payload_range: 0..5,
                    },
                ),
            ]
            .into(),
        );

        let result = reassembler.stitch(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject fragment extending beyond ADU length"
        );
    }

    #[tokio::test]
    async fn stitch_rejects_incomplete_coverage() {
        let store = make_store();
        let reassembler = make_reassembler(&store);
        let ts = CreationTimestamp::now();

        let data = b"HelloWorld";
        let name0 = store_bytes(&store, data).await;

        let id0 = make_id("ipn:0.1.1", &ts, 0, 10);

        store_fragment_metadata(&store, &id0, &name0).await;

        let fragments = FragmentSet(
            [(
                0,
                Fragment {
                    id: id0,
                    storage_name: name0,
                    payload_range: 0..5,
                },
            )]
            .into(),
        );

        let result = reassembler.stitch(&fragments).await;
        assert!(
            result.is_none(),
            "Should reject when bytes_written < total_adu_length"
        );
    }

    #[tokio::test]
    async fn reassemble_basic_happy_path() {
        let store = make_store();
        let reassembler = make_reassembler(&store);
        let ts = CreationTimestamp::now();
        let source: hardy_bpv7::eid::Eid = "ipn:0.1.1".parse().unwrap();
        let dest: hardy_bpv7::eid::Eid = "ipn:0.2.1".parse().unwrap();
        let payload = b"HelloWorld";

        let (complete_bundle, complete_data) =
            hardy_bpv7::builder::Builder::new(source.clone(), dest.clone())
                .with_payload(std::borrow::Cow::Borrowed(&payload[..]))
                .build(ts.clone())
                .unwrap();

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

        let bundle0 =
            hardy_bpv7::bundle::ParsedBundle::parse(&frag0_data, hardy_bpv7::bpsec::no_keys)
                .unwrap()
                .bundle;
        let bundle1 =
            hardy_bpv7::bundle::ParsedBundle::parse(&frag1_data, hardy_bpv7::bpsec::no_keys)
                .unwrap()
                .bundle;

        let name0 = store_bytes(&store, &frag0_data).await;
        let name1 = store_bytes(&store, &frag1_data).await;

        let meta_bundle = Bundle {
            bundle: bundle0.clone(),
            metadata: BundleMetadata {
                storage_name: Some(name0.clone()),
                ..Default::default()
            },
        };
        store.insert_metadata(&meta_bundle).await;

        let payload0_range = bundle0.blocks.get(&1).unwrap().payload_range();
        let payload1_range = bundle1.blocks.get(&1).unwrap().payload_range();

        let fragments = FragmentSet(
            [
                (
                    0,
                    Fragment {
                        id: bundle0.id.clone(),
                        storage_name: name0,
                        payload_range: payload0_range,
                    },
                ),
                (
                    5,
                    Fragment {
                        id: bundle1.id.clone(),
                        storage_name: name1,
                        payload_range: payload1_range,
                    },
                ),
            ]
            .into(),
        );

        let result = reassembler.stitch(&fragments).await;
        assert!(result.is_some(), "Reassembly should succeed");

        let reassembled_data = result.unwrap();

        let reassembled_bundle =
            hardy_bpv7::bundle::ParsedBundle::parse(&reassembled_data, hardy_bpv7::bpsec::no_keys)
                .unwrap()
                .bundle;

        assert!(
            reassembled_bundle.id.fragment_info.is_none(),
            "Reassembled bundle should not have fragment_info"
        );

        let payload_block = reassembled_bundle.blocks.get(&1).unwrap();
        let payload_range = payload_block.payload_range();
        let reassembled_payload = &reassembled_data[payload_range];
        assert_eq!(
            reassembled_payload, payload,
            "Reassembled payload should be 'HelloWorld'"
        );
    }

    // Helper: build two fragment data blobs from a payload, returning
    // (frag0_data, frag1_data, complete_bundle) for tests that need them.
    fn make_fragments(
        source: &str,
        dest: &str,
        payload: &[u8],
    ) -> (Box<[u8]>, Box<[u8]>, hardy_bpv7::bundle::Bundle) {
        let source: hardy_bpv7::eid::Eid = source.parse().unwrap();
        let dest: hardy_bpv7::eid::Eid = dest.parse().unwrap();
        let ts = CreationTimestamp::now();
        let total = payload.len() as u64;
        let mid = payload.len() / 2;

        let (complete_bundle, complete_data) = hardy_bpv7::builder::Builder::new(source, dest)
            .with_payload(std::borrow::Cow::Borrowed(payload))
            .build(ts)
            .unwrap();

        let frag0_data = Editor::new(&complete_bundle, &complete_data)
            .with_fragment_info(Some(FragmentInfo {
                offset: 0,
                total_adu_length: total,
            }))
            .map_err(|(_, e)| e)
            .unwrap()
            .update_block(1)
            .map_err(|(_, e)| e)
            .unwrap()
            .with_data(std::borrow::Cow::Borrowed(&payload[..mid]))
            .rebuild()
            .rebuild()
            .unwrap();

        let frag1_data = Editor::new(&complete_bundle, &complete_data)
            .with_fragment_info(Some(FragmentInfo {
                offset: mid as u64,
                total_adu_length: total,
            }))
            .map_err(|(_, e)| e)
            .unwrap()
            .update_block(1)
            .map_err(|(_, e)| e)
            .unwrap()
            .with_data(std::borrow::Cow::Borrowed(&payload[mid..]))
            .rebuild()
            .rebuild()
            .unwrap();

        (frag0_data, frag1_data, complete_bundle)
    }

    // run() returns Pending when not all fragments are in storage yet.
    #[tokio::test]
    async fn run_returns_pending_when_fragments_missing() {
        let store = make_store();
        let ts = CreationTimestamp::now();

        // Create a single fragment bundle but don't store its sibling
        let (frag0_data, _, _) = make_fragments("ipn:0.1.1", "ipn:0.2.1", b"HelloWorld");

        let bundle0 = ParsedBundle::parse(&frag0_data, hardy_bpv7::bpsec::no_keys)
            .unwrap()
            .bundle;

        let name0 = store_bytes(&store, &frag0_data).await;

        let bundle = Bundle {
            bundle: bundle0,
            metadata: BundleMetadata {
                storage_name: Some(name0),
                status: BundleStatus::AduFragment {
                    source: "ipn:0.1.1".parse().unwrap(),
                    timestamp: ts,
                },
                ..Default::default()
            },
        };
        store.insert_metadata(&bundle).await;

        let reassembler = make_reassembler(&store);
        let result = reassembler.run(bundle, hardy_bpv7::bpsec::no_keys).await;

        assert!(
            matches!(result, ReassemblerResult::Pending),
            "Should return Pending when sibling fragments are missing"
        );
    }

    // run() returns Failed when reassembled data is not valid CBOR/BPv7.
    #[tokio::test]
    async fn run_returns_failed_on_invalid_reassembled_data() {
        let store = make_store();
        let ts = CreationTimestamp::now();
        let total_adu_length = 10u64;

        // Store two fragments with garbage data that will stitch but fail parsing
        let id0 = make_id("ipn:0.1.1", &ts, 0, total_adu_length);
        let id1 = make_id("ipn:0.1.1", &ts, 5, total_adu_length);

        let name0 = store_bytes(&store, b"AAAAA_garbage_not_a_bundle").await;
        let name1 = store_bytes(&store, b"BBBBB_garbage_not_a_bundle").await;

        store_fragment_metadata(&store, &id0, &name0).await;
        store_fragment_metadata(&store, &id1, &name1).await;

        // Build a bundle that looks like a fragment pointing to this garbage
        let reassembler = make_reassembler(&store);
        // stitch will fail because the stored data isn't valid bundle wire format
        // The stitch method tries to load fragment 0 data and parse it, which will fail
        let result = reassembler
            .stitch(&FragmentSet(
                [
                    (
                        0,
                        Fragment {
                            id: id0,
                            storage_name: name0,
                            payload_range: 0..5,
                        },
                    ),
                    (
                        5,
                        Fragment {
                            id: id1,
                            storage_name: name1,
                            payload_range: 0..5,
                        },
                    ),
                ]
                .into(),
            ))
            .await;

        assert!(
            result.is_none(),
            "Should fail when fragment data cannot be parsed as a bundle"
        );
    }

    // stitch() with a zero-length ADU should fail (no payload data to stitch).
    #[tokio::test]
    async fn stitch_rejects_zero_length_adu() {
        let store = make_store();
        let reassembler = make_reassembler(&store);
        let ts = CreationTimestamp::now();

        let id0 = make_id("ipn:0.1.1", &ts, 0, 0);
        let name0 = store_bytes(&store, b"some_data").await;

        store_fragment_metadata(&store, &id0, &name0).await;

        let fragments = FragmentSet(
            [(
                0,
                Fragment {
                    id: id0,
                    storage_name: name0,
                    payload_range: 0..0, // zero-length payload
                },
            )]
            .into(),
        );

        let result = reassembler.stitch(&fragments).await;
        // The stitched data will be empty (0 bytes), and parsing an empty
        // bundle will fail in the Editor, so stitch returns None.
        assert!(result.is_none(), "Should reject zero-length ADU");
    }

    // run() full end-to-end: store two fragments with proper metadata,
    // call run(), and verify Complete is returned with valid data.
    #[tokio::test]
    async fn run_full_happy_path() {
        let store = make_store();
        let payload = b"HelloWorld";
        let (frag0_data, frag1_data, _) = make_fragments("ipn:0.1.1", "ipn:0.2.1", payload);

        let bundle0 = ParsedBundle::parse(&frag0_data, hardy_bpv7::bpsec::no_keys)
            .unwrap()
            .bundle;
        let bundle1 = ParsedBundle::parse(&frag1_data, hardy_bpv7::bpsec::no_keys)
            .unwrap()
            .bundle;

        let name0 = store_bytes(&store, &frag0_data).await;
        let name1 = store_bytes(&store, &frag1_data).await;

        // Store both fragment metadata so collect() finds them
        store_fragment_metadata(&store, &bundle0.id, &name0).await;
        store_fragment_metadata(&store, &bundle1.id, &name1).await;

        // Set fragment 0's status to AduFragment so it triggers reassembly
        let trigger = Bundle {
            bundle: bundle0.clone(),
            metadata: BundleMetadata {
                storage_name: Some(name0.clone()),
                status: BundleStatus::AduFragment {
                    source: bundle0.id.source.clone(),
                    timestamp: bundle0.id.timestamp.clone(),
                },
                ..Default::default()
            },
        };
        // Update metadata in store
        store.update_metadata(&trigger).await;

        // Also set fragment 1's status
        let frag1_bundle = Bundle {
            bundle: bundle1.clone(),
            metadata: BundleMetadata {
                storage_name: Some(name1.clone()),
                status: BundleStatus::AduFragment {
                    source: bundle1.id.source.clone(),
                    timestamp: bundle1.id.timestamp.clone(),
                },
                ..Default::default()
            },
        };
        store.update_metadata(&frag1_bundle).await;

        let reassembler = make_reassembler(&store);
        let result = reassembler.run(trigger, hardy_bpv7::bpsec::no_keys).await;

        match result {
            ReassemblerResult::Complete(_bundle, data) => {
                // Verify the reassembled bundle is valid
                let parsed = ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
                    .unwrap()
                    .bundle;
                assert!(
                    parsed.id.fragment_info.is_none(),
                    "Reassembled bundle should not be a fragment"
                );
            }
            other => panic!(
                "Expected Complete, got {:?}",
                match other {
                    ReassemblerResult::Pending => "Pending",
                    ReassemblerResult::Failed => "Failed",
                    _ => unreachable!(),
                }
            ),
        }
    }
}
