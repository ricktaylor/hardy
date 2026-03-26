use core::ops::Range;
use futures::{FutureExt, join, select_biased};

use super::*;
use crate::bundle::RawBundle;

struct CollectedFragments {
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
    pub async fn adu_reassemble(&self, bundle: &mut bundle::Bundle) -> Result<Option<RawBundle>> {
        let status = bundle::BundleStatus::AduFragment {
            source: bundle.bundle.id.source.clone(),
            timestamp: bundle.bundle.id.timestamp.clone(),
        };

        let Some(fragments) = self.poll_fragments(bundle, &status).await? else {
            self.update_status(bundle, status).await?;
            return Ok(None);
        };

        let raw_bundle = self.reassemble(&fragments).await?;

        // Remove the fragments from bundle_storage even if reassembly failed or returned None;
        // attempt all cleanups regardless of individual failures.
        for (bundle_id, storage_name, _) in fragments.adus.values() {
            if let Err(e) = self.delete_data(storage_name).await {
                warn!("Failed to delete fragment data {storage_name}: {e}");
            }
            if let Err(e) = self.tombstone_metadata(bundle_id).await {
                warn!("Failed to tombstone fragment metadata {bundle_id}: {e}");
            }
        }

        // TODO: It would be good to capture the aggregate received at value across all the fragments, and use that as the received_at for the reassembled bundle

        Ok(raw_bundle)
    }

    async fn poll_fragments(
        &self,
        bundle: &bundle::Bundle,
        status: &bundle::BundleStatus,
    ) -> Result<Option<CollectedFragments>> {
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

        // Initialize with bundle's ADU len, as we haven't set the status yet
        let payload = &bundle
            .bundle
            .blocks
            .get(&1)
            .trace_expect("Bundle without payload?!")
            .payload_range();

        let mut adu_totals = payload.len() as u64;
        let mut fragments = CollectedFragments {
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

        let (producer_result, consumer_result) = join!(
            // Producer: poll for fragment bundles; tx drop signals consumer to stop
            async { self.metadata_storage.poll_adu_fragments(tx, status).await },
            // Consumer: collect fragments
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => {
                            let Ok(bundle) = bundle else {
                                // Done (>= is just so we can capture invalid bundles and handle them at re-dispatch)
                                break (adu_totals >= total_adu_len).then_some(fragments);
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

                                fragments.received_at = fragments.received_at.min(bundle.metadata.read_only.received_at);
                                fragments.adus.insert(fragment_info.offset,
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
        );

        producer_result?;
        Ok(consumer_result)
    }

    async fn reassemble(&self, results: &CollectedFragments) -> Result<Option<RawBundle>> {
        let Some(first) = results.adus.get(&0) else {
            debug!(
                "Series of fragments with no offset 0 fragment found: {:?}",
                &results.adus.values().next().unwrap().0
            );
            return Ok(None);
        };

        let bundle = self.get_metadata(&first.0).await?;
        let Some(old_data) = self
            .load_data(
                bundle
                    .metadata
                    .storage_name
                    .as_ref()
                    .trace_expect("Invalid bundle in reassembly?!"),
            )
            .await?
        else {
            return Ok(None);
        };

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
                debug!(
                    "Total ADU length mismatch during fragment reassembly detected: {bundle_id}"
                );
                return Ok(None);
            }
            if fi.offset != next_offset {
                debug!("Misalignment in offsets during fragment reassembly detected: {bundle_id}");
                return Ok(None);
            }

            next_offset = next_offset.saturating_add(payload.len() as u64);

            let Some(data) = self.load_data(storage_name).await? else {
                return Err(Error::BundleDataMissing);
            };
            new_data.extend_from_slice(data.slice(payload.clone()).as_ref());
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
            debug!(
                "Total reassembled ADU does not match fragment info: {:?}",
                first.0
            );
            return Ok(None);
        }

        // Rewrite primary block
        let editor =
            hardy_bpv7::editor::Editor::new(&bundle.bundle, &old_data).with_fragment_info(None)?;
        let builder = editor.update_block(1)?;
        let new_data: Bytes = builder
            .with_data(new_data.into())
            .rebuild()
            .rebuild()?
            .into();

        // Write the rewritten bundle now for safety
        let new_storage_name = self.save_data(&new_data).await?;

        Ok(Some(RawBundle {
            storage_name: new_storage_name,
            data: new_data,
        }))
    }
}
