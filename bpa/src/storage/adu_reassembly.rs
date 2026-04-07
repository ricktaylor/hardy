use super::*;
use core::ops::Range;
use futures::{FutureExt, join, select_biased};

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
    pub async fn adu_reassemble(&self, bundle: &bundle::Bundle) -> ReassemblyResult {
        let status = bundle::BundleStatus::AduFragment {
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
        }

        // TODO: It would be good to capture the aggregate received at value across all the fragments, and use that as the received_at for the reassembled bundle

        match result {
            Some((storage_name, data)) => ReassemblyResult::Done(storage_name, data),
            None => ReassemblyResult::Failed,
        }
    }

    async fn poll_fragments(
        &self,
        bundle: &bundle::Bundle,
        status: &bundle::BundleStatus,
    ) -> Option<FragmentSet> {
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

        let (tx, rx) = flume::bounded::<bundle::Bundle>(16);

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
        let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, &old_data);
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
