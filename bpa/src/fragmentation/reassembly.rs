//! Progressive fragment reassembly.
//!
//! Each fragment writes its payload directly to the final ADU position
//! in storage. Coverage is tracked in the metadata store. When all bytes
//! are covered, the ADU is finalized into a complete bundle.

use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::bundle::ParsedBundle;
use hardy_bpv7::editor::Editor;
use tracing::debug;

use crate::Bytes;
use crate::bundle::{Bundle, BundleMetadata};
use crate::storage::Store;

use super::{FragmentDescriptor, ReassemblyStatus};

/// Result of processing a fragment.
pub(crate) enum FragmentResult {
    /// Fragment recorded, waiting for more.
    Pending,
    /// All fragments received. Contains the reassembled bundle and its data.
    Complete(Box<Bundle>, Bytes),
    /// Reassembly failed (parse error, validation error, etc.).
    Failed,
}

/// Process a single fragment for progressive reassembly.
///
/// Writes the fragment's payload directly to its ADU offset in storage,
/// updates coverage tracking, and finalizes when all bytes are present.
pub(crate) async fn process_fragment<F>(
    bundle: &Bundle,
    data: &Bytes,
    store: &Store,
    key_provider: F,
) -> FragmentResult
where
    F: FnOnce(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource>,
{
    let Some(fi) = &bundle.bundle.id.fragment_info else {
        debug!("Non-fragment bundle passed to process_fragment");
        return FragmentResult::Failed;
    };

    let Some(payload_block) = bundle.bundle.blocks.get(&1) else {
        debug!("Fragment has no payload block");
        return FragmentResult::Failed;
    };

    let offset = fi.offset;
    let total_adu_length = fi.total_adu_length;
    let payload = data.slice(payload_block.payload_range());

    if offset + payload.len() as u64 > total_adu_length {
        debug!(
            "Fragment extends beyond ADU: offset={offset}, len={}, total={total_adu_length}",
            payload.len()
        );
        return FragmentResult::Failed;
    }

    // Fragment 0 carries all original extension blocks.
    // Store its full wire data for later primary block reconstruction.
    let frag0_data = (offset == 0).then(|| data.clone());

    let descriptor = FragmentDescriptor {
        source: &bundle.bundle.id.source,
        timestamp: &bundle.bundle.id.timestamp,
        total_adu_length,
        offset,
        length: payload.len() as u64,
        extension_blocks: frag0_data.as_ref(),
    };

    let status = store.receive_fragment(&descriptor, &payload).await;

    match status {
        ReassemblyStatus::Pending => FragmentResult::Pending,
        ReassemblyStatus::Complete {
            storage_name,
            extension_blocks,
        } => {
            let result = finalize(store, &storage_name, extension_blocks, key_provider).await;

            store
                .delete_reassembly(&bundle.bundle.id.source, &bundle.bundle.id.timestamp)
                .await;

            result
        }
    }
}

/// Finalize a complete ADU into a stored bundle.
async fn finalize<F>(
    store: &Store,
    adu_storage_name: &str,
    frag0_wire_data: Option<Bytes>,
    key_provider: F,
) -> FragmentResult
where
    F: FnOnce(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource>,
{
    let Some(adu_data) = store.load_data(adu_storage_name).await else {
        debug!("Failed to load reassembled ADU data");
        return FragmentResult::Failed;
    };

    let Some(frag0_data) = frag0_wire_data else {
        debug!("Fragment 0 wire data not available for finalization");
        return FragmentResult::Failed;
    };

    // Parse fragment 0 to get the original bundle structure
    let frag0 = match ParsedBundle::parse(&frag0_data, hardy_bpv7::bpsec::no_keys) {
        Ok(p) => p.bundle,
        Err(e) => {
            debug!("Failed to parse fragment 0: {e}");
            return FragmentResult::Failed;
        }
    };

    // Rebuild: clear fragment info, replace payload with complete ADU
    let editor = match Editor::new(&frag0, &frag0_data).with_fragment_info(None) {
        Ok(e) => e,
        Err((_, e)) => {
            debug!("Failed to clear fragment info: {e}");
            return FragmentResult::Failed;
        }
    };

    let final_data = match editor.update_block(1) {
        Ok(b) => match b.with_data(adu_data.to_vec().into()).rebuild().rebuild() {
            Ok(d) => Bytes::from(d),
            Err(e) => {
                debug!("Failed to rebuild bundle: {e}");
                return FragmentResult::Failed;
            }
        },
        Err((_, e)) => {
            debug!("Missing payload block: {e}");
            return FragmentResult::Failed;
        }
    };

    // Validate and decrypt via BPSec
    let bundle = match ParsedBundle::parse(&final_data, key_provider) {
        Ok(p) => p.bundle,
        Err(e) => {
            metrics::counter!("bpa.bundle.reassembly.failed").increment(1);
            debug!("Reassembled bundle is invalid: {e}");
            return FragmentResult::Failed;
        }
    };

    metrics::counter!("bpa.bundle.reassembled").increment(1);

    // Persist the final bundle
    let final_name = store.save_data(final_data.clone()).await;
    let bundle = Bundle {
        metadata: BundleMetadata {
            storage_name: Some(final_name.clone()),
            ..Default::default()
        },
        bundle,
    };

    if !store.insert_metadata(&bundle).await {
        metrics::counter!("bpa.bundle.received.duplicate").increment(1);
        store.delete_data(&final_name).await;
        return FragmentResult::Failed;
    }

    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status))
        .increment(1.0);

    // Clean up the intermediate ADU object
    store.delete_data(adu_storage_name).await;

    FragmentResult::Complete(Box::new(bundle), final_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arc;
    use crate::bundle::BundleMetadata;
    use crate::storage::{BundleMemStorage, MetadataMemStorage};
    use hardy_bpv7::bundle::FragmentInfo;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;
    use hardy_bpv7::editor::Editor;

    fn make_store() -> Store {
        Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(MetadataMemStorage::new(&Default::default())),
            Arc::new(BundleMemStorage::new(&Default::default())),
        )
    }

    /// Build two fragment bundles from a payload.
    fn make_fragment_bundles(payload: &[u8]) -> (Bundle, Bytes, Bundle, Bytes) {
        let source: hardy_bpv7::eid::Eid = "ipn:0.1.1".parse().unwrap();
        let dest: hardy_bpv7::eid::Eid = "ipn:0.2.1".parse().unwrap();
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

        let frag0_data = Bytes::from(frag0_data);
        let frag1_data = Bytes::from(frag1_data);

        let b0 = hardy_bpv7::bundle::ParsedBundle::parse(&frag0_data, hardy_bpv7::bpsec::no_keys)
            .unwrap()
            .bundle;
        let b1 = hardy_bpv7::bundle::ParsedBundle::parse(&frag1_data, hardy_bpv7::bpsec::no_keys)
            .unwrap()
            .bundle;

        let bundle0 = Bundle {
            bundle: b0,
            metadata: BundleMetadata::default(),
        };
        let bundle1 = Bundle {
            bundle: b1,
            metadata: BundleMetadata::default(),
        };

        (bundle0, frag0_data, bundle1, frag1_data)
    }

    #[tokio::test]
    async fn first_fragment_returns_pending() {
        let store = make_store();
        let (bundle0, data0, _, _) = make_fragment_bundles(b"HelloWorld");

        let result = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;

        assert!(
            matches!(result, FragmentResult::Pending),
            "First fragment should return Pending"
        );
    }

    #[tokio::test]
    async fn two_fragments_complete() {
        let store = make_store();
        let (bundle0, data0, bundle1, data1) = make_fragment_bundles(b"HelloWorld");

        // First fragment
        let result = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(matches!(result, FragmentResult::Pending));

        // Second fragment completes the ADU
        let result = process_fragment(&bundle1, &data1, &store, hardy_bpv7::bpsec::no_keys).await;
        match result {
            FragmentResult::Complete(_bundle, data) => {
                // Verify the reassembled bundle is valid
                let parsed =
                    hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
                        .unwrap()
                        .bundle;

                assert!(
                    parsed.id.fragment_info.is_none(),
                    "Reassembled bundle should not be a fragment"
                );
            }
            other => panic!(
                "Expected Complete, got {}",
                match other {
                    FragmentResult::Pending => "Pending",
                    FragmentResult::Failed => "Failed",
                    _ => unreachable!(),
                }
            ),
        }
    }

    #[tokio::test]
    async fn reverse_order_completes() {
        let store = make_store();
        let (bundle0, data0, bundle1, data1) = make_fragment_bundles(b"HelloWorld");

        // Send fragment 1 first, then fragment 0
        let result = process_fragment(&bundle1, &data1, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(matches!(result, FragmentResult::Pending));

        let result = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(
            matches!(result, FragmentResult::Complete(_, _)),
            "Should complete when last fragment arrives regardless of order"
        );
    }

    #[tokio::test]
    async fn duplicate_fragment_still_pending() {
        let store = make_store();
        let (bundle0, data0, _, _) = make_fragment_bundles(b"HelloWorld");

        let result = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(matches!(result, FragmentResult::Pending));

        // Same fragment again
        let result = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(
            matches!(result, FragmentResult::Pending),
            "Duplicate fragment should still be Pending"
        );
    }

    #[tokio::test]
    async fn non_fragment_returns_failed() {
        let store = make_store();
        let source: hardy_bpv7::eid::Eid = "ipn:0.1.1".parse().unwrap();
        let dest: hardy_bpv7::eid::Eid = "ipn:0.2.1".parse().unwrap();
        let ts = CreationTimestamp::now();

        let (bpv7, data) = hardy_bpv7::builder::Builder::new(source, dest)
            .with_payload(std::borrow::Cow::Borrowed(b"HelloWorld"))
            .build(ts)
            .unwrap();

        let data = Bytes::from(data);
        let bundle = Bundle {
            bundle: bpv7,
            metadata: BundleMetadata::default(),
        };

        let result = process_fragment(&bundle, &data, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(
            matches!(result, FragmentResult::Failed),
            "Non-fragment bundle should return Failed"
        );
    }

    /// Build N equal-sized fragment bundles from a payload.
    fn make_n_fragments(payload: &[u8], n: usize) -> Vec<(Bundle, Bytes)> {
        let source: hardy_bpv7::eid::Eid = "ipn:0.1.1".parse().unwrap();
        let dest: hardy_bpv7::eid::Eid = "ipn:0.2.1".parse().unwrap();
        let ts = CreationTimestamp::now();
        let total = payload.len() as u64;
        let chunk_size = payload.len() / n;

        let (complete_bundle, complete_data) = hardy_bpv7::builder::Builder::new(source, dest)
            .with_payload(std::borrow::Cow::Borrowed(payload))
            .build(ts)
            .unwrap();

        (0..n)
            .map(|i| {
                let offset = i * chunk_size;
                let end = if i == n - 1 {
                    payload.len()
                } else {
                    (i + 1) * chunk_size
                };
                let chunk = &payload[offset..end];

                let frag_data = Editor::new(&complete_bundle, &complete_data)
                    .with_fragment_info(Some(FragmentInfo {
                        offset: offset as u64,
                        total_adu_length: total,
                    }))
                    .map_err(|(_, e)| e)
                    .unwrap()
                    .update_block(1)
                    .map_err(|(_, e)| e)
                    .unwrap()
                    .with_data(std::borrow::Cow::Borrowed(chunk))
                    .rebuild()
                    .rebuild()
                    .unwrap();

                let frag_data = Bytes::from(frag_data);
                let bpv7 =
                    hardy_bpv7::bundle::ParsedBundle::parse(&frag_data, hardy_bpv7::bpsec::no_keys)
                        .unwrap()
                        .bundle;

                (
                    Bundle {
                        bundle: bpv7,
                        metadata: BundleMetadata::default(),
                    },
                    frag_data,
                )
            })
            .collect()
    }

    // Reassemble 5 fragments in order.
    #[tokio::test]
    async fn five_fragments_in_order() {
        let store = make_store();
        let payload = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij";
        let fragments = make_n_fragments(payload, 5);

        for (i, (bundle, data)) in fragments.iter().enumerate() {
            let result = process_fragment(bundle, data, &store, hardy_bpv7::bpsec::no_keys).await;
            if i < 4 {
                assert!(
                    matches!(result, FragmentResult::Pending),
                    "Fragment {i} should be Pending"
                );
            } else {
                assert!(
                    matches!(result, FragmentResult::Complete(_, _)),
                    "Last fragment should Complete"
                );
            }
        }
    }

    // Reassemble 5 fragments in reverse order.
    #[tokio::test]
    async fn five_fragments_reverse_order() {
        let store = make_store();
        let payload = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij";
        let fragments = make_n_fragments(payload, 5);

        for (i, (bundle, data)) in fragments.iter().rev().enumerate() {
            let result = process_fragment(bundle, data, &store, hardy_bpv7::bpsec::no_keys).await;
            if i < 4 {
                assert!(
                    matches!(result, FragmentResult::Pending),
                    "Fragment {i} (reverse) should be Pending"
                );
            } else {
                assert!(
                    matches!(result, FragmentResult::Complete(_, _)),
                    "Last fragment (reverse) should Complete"
                );
            }
        }
    }

    // Verify reassembled payload matches original.
    #[tokio::test]
    async fn reassembled_payload_matches_original() {
        let store = make_store();
        let payload = b"The quick brown fox jumps over the lazy dog";
        let (bundle0, data0, bundle1, data1) = make_fragment_bundles(payload);

        let _ = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;
        let result = process_fragment(&bundle1, &data1, &store, hardy_bpv7::bpsec::no_keys).await;

        match result {
            FragmentResult::Complete(_, data) => {
                let parsed =
                    hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
                        .unwrap()
                        .bundle;

                let payload_block = parsed.blocks.get(&1).unwrap();
                let payload_range = payload_block.payload_range();
                let reassembled = &data[payload_range];
                assert_eq!(
                    reassembled, payload,
                    "Reassembled payload should match original"
                );
            }
            _ => panic!("Expected Complete"),
        }
    }

    // Two independent bundles reassembling concurrently don't interfere.
    #[tokio::test]
    async fn two_bundles_independent() {
        let store = make_store();

        let payload_a = b"AAAAAAAAAA";
        let payload_b = b"BBBBBBBBBB";

        let (a0, a0d, a1, a1d) = make_fragment_bundles(payload_a);
        let (b0, b0d, b1, b1d) = make_fragment_bundles(payload_b);

        // Interleave: A0, B0, A1, B1
        let r = process_fragment(&a0, &a0d, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(matches!(r, FragmentResult::Pending));

        let r = process_fragment(&b0, &b0d, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(matches!(r, FragmentResult::Pending));

        let r = process_fragment(&a1, &a1d, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(
            matches!(r, FragmentResult::Complete(_, _)),
            "A should complete"
        );

        let r = process_fragment(&b1, &b1d, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(
            matches!(r, FragmentResult::Complete(_, _)),
            "B should complete"
        );
    }

    // Reassemble 10 fragments arriving in random order.
    #[tokio::test]
    async fn ten_fragments_shuffled() {
        let store = make_store();
        let payload = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!@";
        let fragments = make_n_fragments(payload, 10);

        // Shuffle: odds first, then evens
        let order = [1, 3, 5, 7, 9, 0, 2, 4, 6, 8];
        let mut last_result = None;

        for &idx in &order {
            let (bundle, data) = &fragments[idx];
            let result = process_fragment(bundle, data, &store, hardy_bpv7::bpsec::no_keys).await;
            last_result = Some(result);
        }

        assert!(
            matches!(last_result, Some(FragmentResult::Complete(_, _))),
            "Should complete after all 10 fragments"
        );
    }

    // After completion, verify the reassembly tracker is cleaned up.
    #[tokio::test]
    async fn cleanup_after_completion() {
        let store = make_store();
        let (bundle0, data0, bundle1, data1) = make_fragment_bundles(b"HelloWorld");

        let _ = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;
        let result = process_fragment(&bundle1, &data1, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(matches!(result, FragmentResult::Complete(_, _)));

        // Sending the same fragments again should start a NEW reassembly (Pending),
        // proving the old tracker was deleted.
        let result = process_fragment(&bundle0, &data0, &store, hardy_bpv7::bpsec::no_keys).await;
        assert!(
            matches!(result, FragmentResult::Pending),
            "After cleanup, same fragments should start fresh (Pending)"
        );
    }
}
