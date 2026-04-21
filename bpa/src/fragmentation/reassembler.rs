use futures::{FutureExt, join, select_biased};
use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::bundle::ParsedBundle;
use hardy_bpv7::editor::Editor;
use trace_err::*;
use tracing::debug;

use super::{Fragment, FragmentSet};
use crate::Bytes;
use crate::bundle::{Bundle, BundleStatus, Stored};
use crate::storage::Store;

/// Result of a reassembly attempt.
pub(crate) enum ReassemblerResult {
    /// All fragments collected and stored; ready for pipeline re-entry.
    Complete(Bundle<Stored>),
    /// Waiting for more fragments (bundle stored with AduFragment status).
    Pending,
    /// Reassembly failed or produced invalid data (already cleaned up).
    Failed,
}

/// Drives the reassembly of a fragmented bundle.
///
/// Holds the store reference and key provider needed to collect fragments,
/// stitch the ADU, validate the result, and persist it.
pub(crate) struct Reassembler<'a, F> {
    store: &'a Store,
    key_provider: F,
}

impl<'a, F> Reassembler<'a, F>
where
    F: Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource>,
{
    pub fn new(store: &'a Store, key_provider: F) -> Self {
        Self {
            store,
            key_provider,
        }
    }

    /// Attempt to reassemble a fragmented bundle.
    ///
    /// Collects sibling fragments from storage, stitches the ADU, rebuilds
    /// the bundle, and stores the result.
    pub async fn run(self, mut bundle: Bundle<Stored>) -> ReassemblerResult {
        let status = BundleStatus::AduFragment {
            source: bundle.id().source.clone(),
            timestamp: bundle.id().timestamp.clone(),
        };

        let Some(fragments) = self.collect(&bundle, &status).await else {
            let _ = self.store.update_status(&mut bundle, &status).await;
            self.store.watch_bundle(&bundle);
            return ReassemblerResult::Pending;
        };

        let result = self.stitch(&fragments).await;

        // Remove fragment data and metadata regardless of outcome
        for frag in fragments.0.values() {
            let _ = self.store.delete_data(&frag.storage_name).await;
            let _ = self.store.tombstone_metadata(&frag.id).await;
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&status))
                .decrement(1.0);
        }

        let Some(data) = result else {
            debug!("Fragment reassembly failed for bundle {}", bundle.id());
            return ReassemblerResult::Failed;
        };

        // Parse and validate the reassembled bundle
        let parsed = ParsedBundle::parse(data.as_ref(), self.key_provider);
        let Ok(ParsedBundle { bundle, .. }) = parsed else {
            metrics::counter!("bpa.bundle.reassembly.failed").increment(1);
            debug!("Reassembled bundle is invalid: {}", parsed.unwrap_err());
            return ReassemblerResult::Failed;
        };

        metrics::counter!("bpa.bundle.reassembled").increment(1);

        let idle = Bundle::new(bundle, data, None, None, None);

        let bundle = match idle.store(self.store).await {
            Ok(Some(bundle)) => bundle,
            Ok(None) => {
                metrics::counter!("bpa.bundle.received.duplicate").increment(1);
                return ReassemblerResult::Failed;
            }
            Err(_) => return ReassemblerResult::Failed,
        };

        ReassemblerResult::Complete(bundle)
    }

    /// Collect sibling fragments from storage.
    ///
    /// Returns `None` if not all fragments have arrived yet.
    async fn collect(&self, bundle: &Bundle<Stored>, status: &BundleStatus) -> Option<FragmentSet> {
        let cancel_token = self.store.cancel_token().clone();

        let source = bundle.id().source.clone();
        let timestamp = bundle.id().timestamp.clone();
        let fragment_info = bundle
            .id()
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
                    id: bundle.id().clone(),
                    storage_name: bundle.storage_name().clone(),
                    payload_range: payload.clone(),
                },
            )]
            .into(),
        );

        let (tx, rx) = flume::bounded::<Bundle<Stored>>(16);

        join!(async { let _ = self.store.poll_adu_fragments(tx, status).await; }, async {
            loop {
                select_biased! {
                    bundle = rx.recv_async().fuse() => {
                        let Ok(bundle) = bundle else {
                            break (adu_totals >= total_adu_len).then_some(fragments);
                        };

                        if source == bundle.id().source
                            && timestamp == bundle.id().timestamp
                            && let Some(fi) = &bundle.id().fragment_info
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
                                    id: bundle.id().clone(),
                                    storage_name: bundle.storage_name().clone(),
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
        })
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

        let old_data = self.store.load_data(&first.storage_name).await.ok().flatten()?;
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
                .await
                .ok()
                .flatten()?
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
    use hardy_bpv7::bundle::FragmentInfo;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    use super::*;
    use crate::storage::{bundle_mem::BundleMemStorage, metadata_mem::MetadataMemStorage};

    fn make_store() -> Store {
        Store::new(
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
        store.save_data(&Bytes::from(data.to_vec())).await.unwrap()
    }

    async fn store_fragment_metadata(store: &Store, id: &Bpv7Id) {
        let idle = Bundle::new(
            hardy_bpv7::bundle::Bundle {
                id: id.clone(),
                destination: "ipn:0.2.1".parse().unwrap(),
                lifetime: core::time::Duration::from_secs(3600),
                ..Default::default()
            },
            Bytes::new(),
            None,
            None,
            None,
        );
        idle.store(store).await.unwrap();
    }

    fn make_reassembler(
        store: &Store,
    ) -> Reassembler<
        '_,
        impl Fn(&hardy_bpv7::bundle::Bundle, &[u8]) -> Box<dyn hardy_bpv7::bpsec::key::KeySource>,
    > {
        Reassembler::new(store, hardy_bpv7::bpsec::no_keys)
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

        store_fragment_metadata(&store, &id0).await;

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

        store_fragment_metadata(&store, &id0).await;

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

        store_fragment_metadata(&store, &id0).await;

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

        let meta_bundle = Bundle::new(bundle0.clone(), Bytes::new(), None, None, None);
        meta_bundle.store(&store).await.unwrap();

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
}
