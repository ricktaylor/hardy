use super::*;
use std::{collections::HashMap, ops::Range};

struct ReassemblyResult {
    received_at: time::OffsetDateTime,
    adus: HashMap<u64, (hardy_bpv7::bundle::Id, Arc<str>, Range<usize>)>,
}

// struct Gather(VecDeque<Bytes>);

// impl Buf for Gather {
//     fn remaining(&self) -> usize {
//         self.0.iter().map(|b| b.len()).sum()
//     }

//     fn chunk(&self) -> &[u8] {
//         let Some(f) = self.0.front() else {
//             return &[];
//         };
//         f.as_ref()
//     }

//     fn chunks_vectored<'a>(&'a self, dst: &mut [std::io::IoSlice<'a>]) -> usize {
//         let mut total = 0;
//         for (b, d) in self.0.iter().zip(dst) {
//             total += b.len();
//             *d = std::io::IoSlice::new(b.as_ref());
//         }
//         total
//     }

//     fn advance(&mut self, mut cnt: usize) {
//         while cnt > 0 {
//             let Some(len) = self.0.front().map(|f| f.len()) else {
//                 break;
//             };
//             if cnt < len {
//                 self.0.front_mut().unwrap().advance(cnt);
//                 break;
//             }
//             cnt -= len;
//             self.0.pop_front();
//         }
//     }
// }

impl Store {
    pub async fn adu_reassemble(
        &self,
        bundle: &mut bundle::Bundle,
    ) -> Option<(metadata::BundleMetadata, Bytes)> {
        let status = metadata::BundleStatus::AduFragment {
            source: bundle.bundle.id.source.clone(),
            timestamp: bundle.bundle.id.timestamp.clone(),
        };

        // See if we can collect all the fragments
        let Some(results) = self.poll_fragments(bundle, &status).await else {
            bundle.metadata.status = status;
            self.update_metadata(bundle).await;
            return None;
        };

        // Now try to reassemble
        let result = self.reassemble(&results).await;

        // Remove the fragments from bundle_storage even if we failed to reassemble
        for (bundle_id, storage_name, _) in results.adus.values() {
            self.delete_data(storage_name).await;
            self.tombstone_metadata(bundle_id).await;
        }

        result.map(|(storage_name, data)| {
            let mut metadata = bundle.metadata.clone();
            metadata.storage_name = Some(storage_name);
            (metadata, data)
        })
    }

    async fn poll_fragments(
        &self,
        bundle: &bundle::Bundle,
        status: &metadata::BundleStatus,
    ) -> Option<ReassemblyResult> {
        // Poll the store for the other fragments
        let outer_cancel_token = self.tasks.child_token();
        let cancel_token = outer_cancel_token.clone();

        let source = bundle.bundle.id.source.clone();
        let timestamp = bundle.bundle.id.timestamp.clone();
        let fragment_info = bundle
            .bundle
            .id
            .fragment_info
            .as_ref()
            .trace_expect("Unfragmented bundle got into adu_reassemble?!");

        let total_adu_len = fragment_info.total_adu_length;

        // Initialize with bundle's ADU len, as we haven't set the status yet
        let payload = &bundle
            .bundle
            .blocks
            .get(&1)
            .trace_expect("Bundle without payload?!")
            .payload_range();

        let mut adu_totals = payload.len() as u64;
        let mut results = ReassemblyResult {
            received_at: bundle.metadata.received_at,
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

        let (tx, rx) = flume::bounded::<bundle::Bundle>(16);
        let task = async move {
            loop {
                tokio::select! {
                    bundle = rx.recv_async() => {
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

                            results.received_at = results.received_at.min(bundle.metadata.received_at);
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
                    _ = cancel_token.cancelled() => {
                        break None;
                    }
                }
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "poll_adu_fragments_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        if self
            .metadata_storage
            .poll_adu_fragments(tx, status)
            .await
            .inspect_err(|e| error!("Failed to poll store for fragmented bundles: {e}"))
            .is_err()
        {
            // Cancel the reader task
            outer_cancel_token.cancel();
        }

        h.await.unwrap_or(None)
    }

    async fn reassemble(&self, results: &ReassemblyResult) -> Option<(Arc<str>, Bytes)> {
        let first = results.adus.get(&0).or_else(|| {
            info!(
                "Series of fragments with no offset 0 fragment found: {:?}",
                &results.adus.iter().next().unwrap().1.0
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

        // TODO:  There's a lot of mem copies going on here!
        let mut new_data: Vec<u8> = old_data
            .slice(
                bundle
                    .bundle
                    .blocks
                    .get(&1)
                    .trace_expect("Bundle without payload?!")
                    .payload_range(),
            )
            .into();

        let mut next_offset = first.2.end as u64;
        let total_adu_length = first.0.fragment_info.as_ref().unwrap().total_adu_length;
        for (bundle_id, storage_name, payload) in results.adus.values() {
            let fi = bundle_id.fragment_info.as_ref().unwrap();
            if fi.total_adu_length != total_adu_length {
                info!(
                    "Total ADU length mismatch during fragment reassembly detected: {bundle_id:?}"
                );
                return None;
            }
            if fi.offset != next_offset {
                info!("Misalignment in offsets during fragment reassembly detected: {bundle_id:?}");
                return None;
            }

            next_offset = next_offset.saturating_add(payload.len() as u64);

            let adu = self.load_data(storage_name).await?.slice(payload.clone());
            new_data.extend_from_slice(adu.as_ref());
        }

        if next_offset
            != bundle
                .bundle
                .id
                .fragment_info
                .as_ref()
                .unwrap()
                .total_adu_length
        {
            info!(
                "Total reassembled ADU does not match fragment info: {:?}",
                first.0
            );
            return None;
        }

        // Rewrite primary block
        let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, &old_data);
        editor = editor.with_fragment_info(None);

        // Now rebuild
        let new_data = match editor.update_block(1) {
            Err(e) => {
                info!("Missing payload block?: {e}");
                return None;
            }
            Ok(b) => match b.with_data(new_data.into()).rebuild().rebuild() {
                Err(e) => {
                    info!("Failed to rebuild bundle: {e}");
                    return None;
                }
                Ok(new_data) => new_data,
            },
        };

        // Write the rewritten bundle now for safety
        let new_data: Bytes = new_data.into();
        let new_storage_name = self.save_data(new_data.clone()).await;

        Some((new_storage_name, new_data))
    }
}
