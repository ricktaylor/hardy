use futures::{FutureExt, join, select_biased};
use hardy_bpv7::status_report::ReasonCode;
use trace_err::*;
use tracing::info;

use super::{Confirmed, Recovery};
use crate::bundle::Bundle;

impl Recovery<'_, Confirmed> {
    /// Phase 3: Purge orphaned metadata entries with no matching bundle data.
    pub(crate) async fn purge(self) {
        if self.store.is_cancelled() {
            return;
        }

        let cancel_token = self.store.cancel_token().clone();
        let (tx, rx) = flume::bounded::<Bundle>(16);

        join!(
            async {
                self.store
                    .purge_unconfirmed(tx)
                    .await
                    .trace_expect("Remove unconfirmed bundles failed");
            },
            async {
                loop {
                    select_biased! {
                        b = rx.recv_async().fuse() => match b {
                            Err(_) => break,
                            Ok(b) => {
                                metrics::counter!("bpa.restart.orphan").increment(1);
                                self.dispatcher
                                    .report_bundle_deletion(&b, ReasonCode::DepletedStorage)
                                    .await
                            }
                        },
                        _ = cancel_token.cancelled().fuse() => {
                            break;
                        }
                    }
                }
            }
        );

        if !self.store.is_cancelled() {
            info!("Store consistency check completed");
        }
    }
}
