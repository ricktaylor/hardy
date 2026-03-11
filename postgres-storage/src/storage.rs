use super::*;
use hardy_bpa::{async_trait, storage};
use sqlx::{PgPool, Row};
use tracing::{error, warn};

#[cfg(feature = "tracing")]
use tracing::instrument;

pub struct Storage {
    pool: PgPool,
}

impl Storage {
    pub async fn new(config: &Config, upgrade: bool) -> Result<Self, sqlx::Error> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
            .idle_timeout(std::time::Duration::from_secs(
                config.idle_timeout_mins * 60,
            ))
            .connect(&config.database_url)
            .await?;

        if upgrade {
            sqlx::migrate!("./migrations").run(&pool).await?;
        } else {
            // Validate checksums of already-applied migrations; error if any are
            // unapplied. Prevents accidental schema changes during rolling restarts.
            sqlx::migrate!("./migrations").run(&pool).await?;
        }

        Ok(Self { pool })
    }
}

// Deserialize a bundle from JSONB and override its status from the pre-decoded typed columns.
// The JSONB blob is authoritative for all fields; typed columns are only for indexing.
// We still override status from typed columns to guard against any blob/column skew.
fn decode_bundle(
    bundle_json: serde_json::Value,
    status: Option<hardy_bpa::metadata::BundleStatus>,
) -> Option<hardy_bpa::bundle::Bundle> {
    let status = status.or_else(|| {
        warn!("Failed to decode metadata status");
        None
    })?;
    match serde_json::from_value::<hardy_bpa::bundle::Bundle>(bundle_json) {
        Ok(mut bundle) => {
            bundle.metadata.status = status;
            Some(bundle)
        }

        Err(e) => {
            warn!("Garbage bundle in metadata store: {e}");
            None
        }
    }
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    #[cfg_attr(feature = "tracing", instrument(skip_all, fields(bundle.id = %bundle_id)))]
    async fn get(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::Bundle>> {
        let id_bytes = serde_json::to_vec(bundle_id)?;

        let row = sqlx::query(
            "SELECT m.bundle, m.status::TEXT, m.peer_id, m.queue_id,
                    m.adu_source, m.adu_ts_ms, m.adu_ts_seq, m.service_eid
             FROM metadata m
             JOIN bundles b ON m.id = b.id
             WHERE b.bundle_id = $1",
        )
        .bind(id_bytes)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(|r| {
            decode_bundle(
                r.get(0),
                status::to_status(
                    r.get(1),
                    r.get(2),
                    r.get(3),
                    r.get(4),
                    r.get(5),
                    r.get(6),
                    r.get(7),
                ),
            )
        }))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    async fn insert(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<bool> {
        let id_bytes = serde_json::to_vec(&bundle.bundle.id)?;
        let bundle_json = serde_json::to_value(bundle)?;
        let received_at = bundle.metadata.read_only.received_at;
        let expiry = bundle.expiry();
        let sp = status::from_status(&bundle.metadata.status);

        // Atomic CTE: insert identity anchor then metadata child.
        // ON CONFLICT DO NOTHING on bundle_id means the outer INSERT sees an empty
        // ins_bundle CTE and inserts 0 rows — clean false return for duplicates.
        let rows_affected = sqlx::query(
            "WITH ins_bundle AS (
                 INSERT INTO bundles (bundle_id, received_at)
                 VALUES ($1, $2)
                 ON CONFLICT (bundle_id) DO NOTHING
                 RETURNING id
             )
             INSERT INTO metadata
                 (id, expiry, received_at, status,
                  peer_id, queue_id, adu_source, adu_ts_ms, adu_ts_seq, service_eid,
                  bundle)
             SELECT id, $3, $4, $5::bundle_status,
                    $6, $7, $8, $9, $10, $11, $12
             FROM ins_bundle",
        )
        .bind(id_bytes)
        .bind(received_at)
        .bind(expiry)
        .bind(received_at) // denormalized received_at in metadata
        .bind(sp.status)
        .bind(sp.peer_id)
        .bind(sp.queue_id)
        .bind(sp.adu_source)
        .bind(sp.adu_ts_ms)
        .bind(sp.adu_ts_seq)
        .bind(sp.service_eid)
        .bind(bundle_json)
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(rows_affected == 1)
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    async fn replace(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        let id_bytes = serde_json::to_vec(&bundle.bundle.id)?;
        let bundle_json = serde_json::to_value(bundle)?;
        let expiry = bundle.expiry();
        let sp = status::from_status(&bundle.metadata.status);

        let rows = sqlx::query(
            "UPDATE metadata
             SET status      = $2::bundle_status,
                 expiry      = $3,
                 peer_id     = $4,
                 queue_id    = $5,
                 adu_source  = $6,
                 adu_ts_ms   = $7,
                 adu_ts_seq  = $8,
                 service_eid = $9,
                 bundle      = $10
             WHERE id = (SELECT id FROM bundles WHERE bundle_id = $1)",
        )
        .bind(id_bytes)
        .bind(sp.status)
        .bind(expiry)
        .bind(sp.peer_id)
        .bind(sp.queue_id)
        .bind(sp.adu_source)
        .bind(sp.adu_ts_ms)
        .bind(sp.adu_ts_seq)
        .bind(sp.service_eid)
        .bind(bundle_json)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows != 1 {
            error!(
                bundle.id = %bundle.bundle.id,
                "replace() updated {rows} rows (expected 1)"
            );
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, fields(bundle.id = %bundle_id)))]
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        let id_bytes = serde_json::to_vec(bundle_id)?;

        // Delete the metadata row; bundles row is kept permanently so its UNIQUE
        // constraint blocks any future insert for the same bundle_id (tombstone semantic).
        sqlx::query(
            "DELETE FROM metadata WHERE id = (SELECT id FROM bundles WHERE bundle_id = $1)",
        )
        .bind(id_bytes)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn start_recovery(&self) {
        sqlx::query("INSERT INTO unconfirmed (id) SELECT id FROM metadata ON CONFLICT DO NOTHING")
            .execute(&self.pool)
            .await
            .unwrap_or_else(|e| {
                error!("Failed to mark unconfirmed bundles: {e}");
                Default::default()
            });
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, fields(bundle.id = %bundle_id)))]
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::metadata::BundleMetadata>> {
        let id_bytes = serde_json::to_vec(bundle_id)?;

        let row = sqlx::query(
            "SELECT m.id, m.bundle, m.status::TEXT, m.peer_id, m.queue_id,
                    m.adu_source, m.adu_ts_ms, m.adu_ts_seq, m.service_eid
             FROM metadata m
             JOIN bundles b ON m.id = b.id
             WHERE b.bundle_id = $1
             LIMIT 1",
        )
        .bind(id_bytes)
        .fetch_optional(&self.pool)
        .await?;

        let Some(r) = row else {
            return Ok(None);
        };

        let id: i64 = r.get(0);

        // Remove the unconfirmed marker for this id.
        sqlx::query("DELETE FROM unconfirmed WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(decode_bundle(
            r.get(1),
            status::to_status(
                r.get(2),
                r.get(3),
                r.get(4),
                r.get(5),
                r.get(6),
                r.get(7),
                r.get(8),
            ),
        )
        .map(|b| b.metadata))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn remove_unconfirmed(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        loop {
            // One atomic CTE: delete a batch from unconfirmed, snapshot the bundle blobs,
            // then tombstone the metadata rows. PostgreSQL evaluates all CTEs against the
            // same pre-statement snapshot, so snapshot sees rows that del then removes.
            let rows = sqlx::query(
                "WITH batch AS (
                     DELETE FROM unconfirmed
                     WHERE id IN (SELECT id FROM unconfirmed LIMIT 64)
                     RETURNING id
                 ),
                 snapshot AS (
                     SELECT m.bundle, m.status::TEXT, m.peer_id, m.queue_id,
                            m.adu_source, m.adu_ts_ms, m.adu_ts_seq, m.service_eid
                     FROM metadata m
                     JOIN batch ON m.id = batch.id
                 ),
                 del AS (
                     DELETE FROM metadata WHERE id IN (SELECT id FROM batch)
                 )
                 SELECT * FROM snapshot",
            )
            .fetch_all(&self.pool)
            .await?;

            if rows.is_empty() {
                return Ok(());
            }

            for r in rows {
                let Some(bundle) = decode_bundle(
                    r.get(0),
                    status::to_status(
                        r.get(1),
                        r.get(2),
                        r.get(3),
                        r.get(4),
                        r.get(5),
                        r.get(6),
                        r.get(7),
                    ),
                ) else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn reset_peer_queue(&self, peer: u32) -> storage::Result<bool> {
        let rows = sqlx::query(
            "UPDATE metadata
             SET status   = 'waiting'::bundle_status,
                 peer_id  = NULL,
                 queue_id = NULL
             WHERE status = 'forward_pending'::bundle_status
               AND peer_id = $1",
        )
        .bind(peer as i32)
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(rows > 0)
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, tx)))]
    async fn poll_expiry(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        limit: usize,
    ) -> storage::Result<()> {
        let mut txn = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *txn)
            .await?;

        // UNIX_EPOCH as the initial keyset cursor: all BIGSERIAL ids start at 1,
        // so (UNIX_EPOCH, 0) is strictly less than every real row.
        let mut last_expiry = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;
        let mut sent: usize = 0;

        loop {
            let page_limit = (limit - sent).min(64) as i64;
            let rows = sqlx::query(
                "SELECT id, expiry, bundle, status::TEXT, peer_id, queue_id,
                        adu_source, adu_ts_ms, adu_ts_seq, service_eid
                 FROM metadata
                 WHERE status != 'new'
                   AND (expiry, id) > ($1, $2)
                 ORDER BY expiry ASC, id ASC
                 LIMIT $3",
            )
            .bind(last_expiry)
            .bind(last_id)
            .bind(page_limit)
            .fetch_all(&mut *txn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in &rows {
                last_expiry = r.get(1);
                last_id = r.get(0);
            }

            for r in rows {
                let Some(bundle) = decode_bundle(
                    r.get(2),
                    status::to_status(
                        r.get(3),
                        r.get(4),
                        r.get(5),
                        r.get(6),
                        r.get(7),
                        r.get(8),
                        r.get(9),
                    ),
                ) else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    txn.rollback().await.ok();
                    return Ok(());
                }
                sent += 1;
            }

            if sent >= limit {
                break;
            }
        }

        txn.commit().await?;
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn poll_waiting(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        let mut txn = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *txn)
            .await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;

        loop {
            let rows = sqlx::query(
                "SELECT id, received_at, bundle
                 FROM metadata
                 WHERE status = 'waiting'
                   AND (received_at, id) > ($1, $2)
                 ORDER BY received_at ASC, id ASC
                 LIMIT 64",
            )
            .bind(last_received_at)
            .bind(last_id)
            .fetch_all(&mut *txn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in &rows {
                last_received_at = r.get(1);
                last_id = r.get(0);
            }

            for r in rows {
                let mut bundle: hardy_bpa::bundle::Bundle = match serde_json::from_value(r.get(2)) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("Garbage bundle in metadata store: {e}");
                        continue;
                    }
                };
                // Status is 'waiting' by the WHERE clause; set it explicitly so
                // the blob's status field (which may lag by one write) is consistent.
                bundle.metadata.status = hardy_bpa::metadata::BundleStatus::Waiting;
                if tx.send_async(bundle).await.is_err() {
                    txn.rollback().await.ok();
                    return Ok(());
                }
            }
        }

        txn.commit().await?;
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, tx)))]
    async fn poll_service_waiting(
        &self,
        source: hardy_bpv7::eid::Eid,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        let source_str = source.to_string();

        let mut txn = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *txn)
            .await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;

        loop {
            let rows = sqlx::query(
                "SELECT id, received_at, bundle
                 FROM metadata
                 WHERE status = 'waiting_for_service'
                   AND service_eid = $1
                   AND (received_at, id) > ($2, $3)
                 ORDER BY received_at ASC, id ASC
                 LIMIT 64",
            )
            .bind(&source_str)
            .bind(last_received_at)
            .bind(last_id)
            .fetch_all(&mut *txn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in &rows {
                last_received_at = r.get(1);
                last_id = r.get(0);
            }

            for r in rows {
                let mut bundle: hardy_bpa::bundle::Bundle = match serde_json::from_value(r.get(2)) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("Garbage bundle in metadata store: {e}");
                        continue;
                    }
                };
                bundle.metadata.status = hardy_bpa::metadata::BundleStatus::WaitingForService {
                    service: source.clone(),
                };
                if tx.send_async(bundle).await.is_err() {
                    txn.rollback().await.ok();
                    return Ok(());
                }
            }
        }

        txn.commit().await?;
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, tx)))]
    async fn poll_adu_fragments(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        status: &hardy_bpa::metadata::BundleStatus,
    ) -> storage::Result<()> {
        let sp = status::from_status(status);

        let mut txn = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *txn)
            .await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;

        loop {
            let rows = sqlx::query(
                "SELECT id, received_at, bundle, status::TEXT, peer_id, queue_id,
                        adu_source, adu_ts_ms, adu_ts_seq, service_eid
                 FROM metadata
                 WHERE status = $1::bundle_status
                   AND adu_source IS NOT DISTINCT FROM $2
                   AND adu_ts_ms  IS NOT DISTINCT FROM $3
                   AND adu_ts_seq IS NOT DISTINCT FROM $4
                   AND (received_at, id) > ($5, $6)
                 ORDER BY received_at ASC, id ASC
                 LIMIT 64",
            )
            .bind(sp.status)
            .bind(&sp.adu_source)
            .bind(sp.adu_ts_ms)
            .bind(sp.adu_ts_seq)
            .bind(last_received_at)
            .bind(last_id)
            .fetch_all(&mut *txn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in &rows {
                last_received_at = r.get(1);
                last_id = r.get(0);
            }

            for r in rows {
                let Some(bundle) = decode_bundle(
                    r.get(2),
                    status::to_status(
                        r.get(3),
                        r.get(4),
                        r.get(5),
                        r.get(6),
                        r.get(7),
                        r.get(8),
                        r.get(9),
                    ),
                ) else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    txn.rollback().await.ok();
                    return Ok(());
                }
            }
        }

        txn.commit().await?;
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, tx)))]
    async fn poll_pending(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        status: &hardy_bpa::metadata::BundleStatus,
        limit: usize,
    ) -> storage::Result<()> {
        let sp = status::from_status(status);

        let mut txn = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *txn)
            .await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;
        let mut sent: usize = 0;

        loop {
            let page_limit = (limit - sent).min(64) as i64;
            let rows = sqlx::query(
                "SELECT id, received_at, bundle, status::TEXT, peer_id, queue_id,
                        adu_source, adu_ts_ms, adu_ts_seq, service_eid
                 FROM metadata
                 WHERE status    = $1::bundle_status
                   AND peer_id     IS NOT DISTINCT FROM $2
                   AND queue_id    IS NOT DISTINCT FROM $3
                   AND adu_source  IS NOT DISTINCT FROM $4
                   AND adu_ts_ms   IS NOT DISTINCT FROM $5
                   AND adu_ts_seq  IS NOT DISTINCT FROM $6
                   AND service_eid IS NOT DISTINCT FROM $7
                   AND (received_at, id) > ($8, $9)
                 ORDER BY received_at ASC, id ASC
                 LIMIT $10",
            )
            .bind(sp.status)
            .bind(sp.peer_id)
            .bind(sp.queue_id)
            .bind(&sp.adu_source)
            .bind(sp.adu_ts_ms)
            .bind(sp.adu_ts_seq)
            .bind(&sp.service_eid)
            .bind(last_received_at)
            .bind(last_id)
            .bind(page_limit)
            .fetch_all(&mut *txn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in &rows {
                last_received_at = r.get(1);
                last_id = r.get(0);
            }

            for r in rows {
                let Some(bundle) = decode_bundle(
                    r.get(2),
                    status::to_status(
                        r.get(3),
                        r.get(4),
                        r.get(5),
                        r.get(6),
                        r.get(7),
                        r.get(8),
                        r.get(9),
                    ),
                ) else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    txn.rollback().await.ok();
                    return Ok(());
                }
                sent += 1;
            }

            if sent >= limit {
                break;
            }
        }

        txn.commit().await?;
        Ok(())
    }
}
