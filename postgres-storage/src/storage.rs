use super::*;
use hardy_bpa::{async_trait, storage};
use sqlx::{FromRow, PgPool, migrate::Migrate};
use tracing::{error, warn};

#[cfg(feature = "instrument")]
use tracing::instrument;

pub struct Storage {
    pool: PgPool,
    poll_page_size: i64,
}

impl Storage {
    pub async fn new(config: &Config, upgrade: bool) -> Result<Self, super::Error> {
        let database_url = if config.database_url.is_empty() {
            std::env::var("DATABASE_URL").unwrap_or_default()
        } else {
            config.database_url.clone()
        };
        if database_url.is_empty() {
            return Err(super::Error::Config(
                "database_url is required; set it in config or via DATABASE_URL env var".into(),
            ));
        }
        if config.poll_page_size == 0 {
            return Err(super::Error::Config("poll_page_size must be >= 1".into()));
        }

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
            .idle_timeout(std::time::Duration::from_secs(
                config.idle_timeout_mins * 60,
            ))
            .max_lifetime(std::time::Duration::from_secs(
                config.max_lifetime_mins * 60,
            ))
            .connect(&database_url)
            .await?;

        if upgrade {
            sqlx::migrate!("./migrations").run(&pool).await?;
        } else {
            // Non-upgrade path (rolling restarts): validate that every expected migration
            // is already applied with a matching checksum, and fail if any are pending.
            // This catches both schema drift (checksum mismatch) and a binary that is
            // ahead of the schema (missing applied row).
            let migrator = sqlx::migrate!("./migrations");
            let mut conn = pool.acquire().await?;
            conn.ensure_migrations_table().await?;
            let applied: std::collections::HashMap<i64, _> = conn
                .list_applied_migrations()
                .await?
                .into_iter()
                .map(|m| (m.version, m.checksum))
                .collect();
            for migration in migrator.migrations.iter() {
                match applied.get(&migration.version) {
                    None => {
                        return Err(super::Error::Migration(
                            sqlx::migrate::MigrateError::VersionMissing(migration.version),
                        ));
                    }
                    Some(checksum) if checksum.as_ref() != migration.checksum.as_ref() => {
                        return Err(super::Error::Migration(
                            sqlx::migrate::MigrateError::VersionMismatch(migration.version),
                        ));
                    }
                    _ => {}
                }
            }

            // Reject applied migrations not known to this binary: the DB schema is
            // newer than the binary (downgrade scenario).
            let known: std::collections::HashSet<i64> =
                migrator.migrations.iter().map(|m| m.version).collect();
            for version in applied.keys() {
                if !known.contains(version) {
                    return Err(super::Error::Downgrade(*version));
                }
            }
        }

        Ok(Self {
            pool,
            poll_page_size: config.poll_page_size as i64,
        })
    }
}

/// Acquire a connection and open a REPEATABLE READ READ ONLY transaction in one round trip,
/// saving the extra RTT that `pool.begin()` + `SET TRANSACTION` would require.
///
/// For read-only transactions an explicit ROLLBACK is unnecessary: returning the connection
/// to the pool will trigger sqlx's implicit rollback if the transaction is still open.
/// Callers must issue an explicit `COMMIT` on success.
async fn begin_snapshot(
    pool: &PgPool,
) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *conn)
        .await?;
    Ok(conn)
}

/// Projection of the `metadata` table (joined with `bundles` for point lookups).
/// Used by: `get`, `remove_unconfirmed`.
#[derive(FromRow)]
struct MetadataRow {
    bundle: Vec<u8>,
    #[sqlx(flatten)]
    status_fields: status::StatusFields,
}

/// Like `MetadataRow` but includes `id`. Used by: `confirm_exists`.
#[derive(FromRow)]
struct MetadataRowWithId {
    id: i64,
    bundle: Vec<u8>,
    #[sqlx(flatten)]
    status_fields: status::StatusFields,
}

/// Keyset cursor on `received_at`; status is fixed by the WHERE clause.
/// Used by `poll_waiting`, `poll_service_waiting`.
#[derive(FromRow)]
struct WaitingRow {
    id: i64,
    received_at: time::OffsetDateTime,
    bundle: Vec<u8>,
}

impl WaitingRow {
    fn decode(self, status: &hardy_bpa::bundle::BundleStatus) -> Option<hardy_bpa::bundle::Bundle> {
        decode_bundle(self.bundle, Some(status.clone()))
    }
}

/// Keyset cursor on `expiry` with full status breakdown.
/// Used by `poll_expiry`.
#[derive(FromRow)]
struct ExpiryRow {
    id: i64,
    expiry: time::OffsetDateTime,
    bundle: Vec<u8>,
    #[sqlx(flatten)]
    status_fields: status::StatusFields,
}

/// Keyset cursor on `received_at` with full status breakdown.
/// Used by `poll_adu_fragments`, `poll_pending`.
#[derive(FromRow)]
struct PendingRow {
    id: i64,
    received_at: time::OffsetDateTime,
    bundle: Vec<u8>,
    #[sqlx(flatten)]
    status_fields: status::StatusFields,
}

impl MetadataRow {
    fn decode(self) -> Option<hardy_bpa::bundle::Bundle> {
        decode_bundle(self.bundle, self.status_fields.into_bundle_status())
    }
}

impl MetadataRowWithId {
    fn decode(self) -> (i64, Option<hardy_bpa::bundle::Bundle>) {
        (
            self.id,
            decode_bundle(self.bundle, self.status_fields.into_bundle_status()),
        )
    }
}

impl ExpiryRow {
    fn decode(self) -> (i64, time::OffsetDateTime, Option<hardy_bpa::bundle::Bundle>) {
        (
            self.id,
            self.expiry,
            decode_bundle(self.bundle, self.status_fields.into_bundle_status()),
        )
    }
}

impl PendingRow {
    fn decode(self) -> (i64, time::OffsetDateTime, Option<hardy_bpa::bundle::Bundle>) {
        (
            self.id,
            self.received_at,
            decode_bundle(self.bundle, self.status_fields.into_bundle_status()),
        )
    }
}

// Deserialize a bundle from BYTEA and override its status from the pre-decoded typed columns.
// The BYTEA blob is authoritative for all fields; typed columns are only for indexing.
// We still override status from typed columns to guard against any blob/column skew.
fn decode_bundle(
    bundle_bytes: Vec<u8>,
    status: Option<hardy_bpa::bundle::BundleStatus>,
) -> Option<hardy_bpa::bundle::Bundle> {
    let Some(status) = status else {
        warn!("Failed to decode metadata status");
        return None;
    };
    match serde_json::from_slice::<hardy_bpa::bundle::Bundle>(&bundle_bytes) {
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
    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle_id)))]
    async fn get(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::Bundle>> {
        let bundle_key = bundle_id.to_key();

        let row = sqlx::query_as::<_, MetadataRow>(
            "SELECT m.bundle, m.status, m.peer_id, m.queue_id,
                    m.adu_source, m.adu_ts_ms, m.adu_ts_seq, m.service_eid
             FROM metadata m
             JOIN bundles b ON m.id = b.id
             WHERE b.bundle_id = $1",
        )
        .bind(bundle_key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(MetadataRow::decode))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    async fn insert(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<bool> {
        let bundle_key = bundle.bundle.id.to_key();
        let bundle_bytes = serde_json::to_vec(bundle)?;
        let received_at = bundle.metadata.read_only.received_at;
        let expiry = bundle.expiry();
        let sf = status::StatusFields::try_from(&bundle.metadata.status)?;

        // Atomic CTE: insert identity anchor then metadata child.
        // RETURNING id on the outer INSERT: Some = inserted, None = duplicate
        // (ON CONFLICT DO NOTHING on bundle_id leaves ins_bundle empty).
        let inserted = sqlx::query_scalar::<_, i64>(
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
             SELECT id, $3, $4, $5,
                    $6, $7, $8, $9, $10, $11, $12
             FROM ins_bundle
             RETURNING id",
        )
        .bind(bundle_key)
        .bind(received_at)
        .bind(expiry)
        .bind(received_at) // denormalized received_at in metadata
        .bind(sf.status)
        .bind(sf.peer_id)
        .bind(sf.queue_id)
        .bind(sf.adu_source)
        .bind(sf.adu_ts_ms)
        .bind(sf.adu_ts_seq)
        .bind(sf.service_eid)
        .bind(bundle_bytes)
        .fetch_optional(&self.pool)
        .await?;

        Ok(inserted.is_some())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    async fn replace(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        let bundle_key = bundle.bundle.id.to_key();
        let bundle_bytes = serde_json::to_vec(bundle)?;
        let expiry = bundle.expiry();
        let sf = status::StatusFields::try_from(&bundle.metadata.status)?;

        let rows = sqlx::query(
            "UPDATE metadata
             SET status      = $2,
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
        .bind(bundle_key)
        .bind(sf.status)
        .bind(expiry)
        .bind(sf.peer_id)
        .bind(sf.queue_id)
        .bind(sf.adu_source)
        .bind(sf.adu_ts_ms)
        .bind(sf.adu_ts_seq)
        .bind(sf.service_eid)
        .bind(bundle_bytes)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows == 0 {
            return Err(sqlx::Error::RowNotFound.into());
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    async fn update_status(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        let bundle_key = bundle.bundle.id.to_key();
        let sf = status::StatusFields::try_from(&bundle.metadata.status)?;

        let rows = sqlx::query(
            "UPDATE metadata
             SET status      = $2,
                 peer_id     = $3,
                 queue_id    = $4,
                 adu_source  = $5,
                 adu_ts_ms   = $6,
                 adu_ts_seq  = $7,
                 service_eid = $8
             WHERE id = (SELECT id FROM bundles WHERE bundle_id = $1)",
        )
        .bind(bundle_key)
        .bind(sf.status)
        .bind(sf.peer_id)
        .bind(sf.queue_id)
        .bind(sf.adu_source)
        .bind(sf.adu_ts_ms)
        .bind(sf.adu_ts_seq)
        .bind(sf.service_eid)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows == 0 {
            return Err(sqlx::Error::RowNotFound.into());
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle_id)))]
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        let bundle_key = bundle_id.to_key();

        // Delete the metadata row; bundles row is kept permanently so its UNIQUE
        // constraint blocks any future insert for the same bundle_id (tombstone semantic).
        sqlx::query(
            "DELETE FROM metadata WHERE id = (SELECT id FROM bundles WHERE bundle_id = $1)",
        )
        .bind(bundle_key)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn start_recovery(&self) {
        if let Err(e) = sqlx::query(
            "INSERT INTO unconfirmed (id) SELECT id FROM metadata ON CONFLICT DO NOTHING",
        )
        .execute(&self.pool)
        .await
        {
            error!("Failed to mark unconfirmed bundles: {e}");
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle_id)))]
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::BundleMetadata>> {
        let bundle_key = bundle_id.to_key();

        // Atomic: SELECT + DELETE in one transaction so a concurrent
        // remove_unconfirmed cannot race between the two operations.
        let mut txn = self.pool.begin().await?;

        let row = sqlx::query_as::<_, MetadataRowWithId>(
            "SELECT m.id, m.bundle, m.status, m.peer_id, m.queue_id,
                    m.adu_source, m.adu_ts_ms, m.adu_ts_seq, m.service_eid
             FROM metadata m
             JOIN bundles b ON m.id = b.id
             WHERE b.bundle_id = $1",
        )
        .bind(bundle_key)
        .fetch_optional(&mut *txn)
        .await?;

        let Some(r) = row else {
            // Nothing modified; drop triggers implicit rollback, no COMMIT RTT needed.
            return Ok(None);
        };

        let (id, bundle) = r.decode();

        let Some(bundle) = bundle else {
            // Corrupt blob: leave the unconfirmed entry so remove_unconfirmed
            // tombstones the metadata row during recovery cleanup.
            return Ok(None);
        };

        sqlx::query("DELETE FROM unconfirmed WHERE id = $1")
            .bind(id)
            .execute(&mut *txn)
            .await?;

        txn.commit().await?;
        Ok(Some(bundle.metadata))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn remove_unconfirmed(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        loop {
            // One atomic CTE: delete a batch from unconfirmed, snapshot the bundle blobs,
            // then tombstone the metadata rows. PostgreSQL evaluates all CTEs against the
            // same pre-statement snapshot, so snapshot sees rows that del then removes.
            let rows = sqlx::query_as::<_, MetadataRow>(
                "WITH batch AS (
                     DELETE FROM unconfirmed
                     WHERE id IN (SELECT id FROM unconfirmed ORDER BY id LIMIT $1)
                     RETURNING id
                 ),
                 snapshot AS (
                     SELECT m.bundle, m.status, m.peer_id, m.queue_id,
                            m.adu_source, m.adu_ts_ms, m.adu_ts_seq, m.service_eid
                     FROM metadata m
                     JOIN batch ON m.id = batch.id
                 ),
                 del AS (
                     DELETE FROM metadata WHERE id IN (SELECT id FROM batch)
                 )
                 SELECT * FROM snapshot",
            )
            .bind(self.poll_page_size)
            .fetch_all(&self.pool)
            .await?;

            if rows.is_empty() {
                return Ok(());
            }

            for r in rows {
                let Some(bundle) = r.decode() else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    // Consumer closed before the batch was fully delivered.
                    // The CTE already deleted these rows from the DB; log so
                    // the operator knows deletion reports may be missing.
                    warn!(
                        "Recovery consumer closed mid-batch; remaining orphaned bundles will not receive deletion reports"
                    );
                    return Ok(());
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn reset_peer_queue(&self, peer: u32) -> storage::Result<u64> {
        let rows = sqlx::query(
            "UPDATE metadata
             SET status   = $2,
                 peer_id  = NULL,
                 queue_id = NULL
             WHERE status = $3
               AND peer_id = $1",
        )
        .bind(i32::try_from(peer)?)
        .bind(status::BundleStatusKind::Waiting)
        .bind(status::BundleStatusKind::ForwardPending)
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(rows)
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, tx)))]
    async fn poll_expiry(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        limit: usize,
    ) -> storage::Result<()> {
        let mut conn = begin_snapshot(&self.pool).await?;

        // UNIX_EPOCH as the initial keyset cursor: all BIGSERIAL ids start at 1,
        // so (UNIX_EPOCH, 0) is strictly less than every real row.
        let mut last_expiry = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;
        let mut sent: usize = 0;

        loop {
            let page_limit = (limit.saturating_sub(sent) as i64).min(self.poll_page_size);
            let rows = sqlx::query_as::<_, ExpiryRow>(
                "SELECT id, expiry, bundle, status, peer_id, queue_id,
                        adu_source, adu_ts_ms, adu_ts_seq, service_eid
                 FROM metadata
                 WHERE status != $1
                   AND (expiry, id) > ($2, $3)
                 ORDER BY expiry ASC, id ASC
                 LIMIT $4",
            )
            .bind(status::BundleStatusKind::New)
            .bind(last_expiry)
            .bind(last_id)
            .bind(page_limit)
            .fetch_all(&mut *conn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in rows {
                let (id, expiry, bundle) = r.decode();
                last_expiry = expiry;
                last_id = id;
                let Some(bundle) = bundle else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    // conn dropped here; sqlx issues implicit ROLLBACK on return to pool
                    return Ok(());
                }
                sent += 1;
            }

            if sent >= limit {
                break;
            }
        }

        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn poll_waiting(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        let mut conn = begin_snapshot(&self.pool).await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;

        loop {
            let rows = sqlx::query_as::<_, WaitingRow>(
                "SELECT id, received_at, bundle
                 FROM metadata
                 WHERE status = $1
                   AND (received_at, id) > ($2, $3)
                 ORDER BY received_at ASC, id ASC
                 LIMIT $4",
            )
            .bind(status::BundleStatusKind::Waiting)
            .bind(last_received_at)
            .bind(last_id)
            .bind(self.poll_page_size)
            .fetch_all(&mut *conn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in rows {
                last_received_at = r.received_at;
                last_id = r.id;
                // Status is 'waiting' by the WHERE clause; override the blob's status
                // field (which may lag by one write) to keep them consistent.
                let Some(bundle) = r.decode(&hardy_bpa::bundle::BundleStatus::Waiting) else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    // conn dropped here; sqlx issues implicit ROLLBACK on return to pool
                    return Ok(());
                }
            }
        }

        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn poll_service_waiting(
        &self,
        source: hardy_bpv7::eid::Eid,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        let source_str = source.to_string();
        // Construct once; all bundles on this poll share the same WaitingForService status.
        let bundle_status = hardy_bpa::bundle::BundleStatus::WaitingForService { service: source };

        let mut conn = begin_snapshot(&self.pool).await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;

        loop {
            let rows = sqlx::query_as::<_, WaitingRow>(
                "SELECT id, received_at, bundle
                 FROM metadata
                 WHERE status = $1
                   AND service_eid = $2
                   AND (received_at, id) > ($3, $4)
                 ORDER BY received_at ASC, id ASC
                 LIMIT $5",
            )
            .bind(status::BundleStatusKind::WaitingForService)
            .bind(&source_str)
            .bind(last_received_at)
            .bind(last_id)
            .bind(self.poll_page_size)
            .fetch_all(&mut *conn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in rows {
                last_received_at = r.received_at;
                last_id = r.id;
                let Some(bundle) = r.decode(&bundle_status) else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    // conn dropped here; sqlx issues implicit ROLLBACK on return to pool
                    return Ok(());
                }
            }
        }

        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, tx)))]
    async fn poll_adu_fragments(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        status: &hardy_bpa::bundle::BundleStatus,
    ) -> storage::Result<()> {
        let sf = status::StatusFields::try_from(status)?;

        let mut conn = begin_snapshot(&self.pool).await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;

        loop {
            let rows = sqlx::query_as::<_, PendingRow>(
                "SELECT id, received_at, bundle, status, peer_id, queue_id,
                        adu_source, adu_ts_ms, adu_ts_seq, service_eid
                 FROM metadata
                 WHERE status = $1
                   AND adu_source IS NOT DISTINCT FROM $2
                   AND adu_ts_ms  IS NOT DISTINCT FROM $3
                   AND adu_ts_seq IS NOT DISTINCT FROM $4
                   AND (received_at, id) > ($5, $6)
                 ORDER BY received_at ASC, id ASC
                 LIMIT $7",
            )
            .bind(sf.status)
            .bind(&sf.adu_source)
            .bind(sf.adu_ts_ms)
            .bind(sf.adu_ts_seq)
            .bind(last_received_at)
            .bind(last_id)
            .bind(self.poll_page_size)
            .fetch_all(&mut *conn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in rows {
                let (id, received_at, bundle) = r.decode();
                last_received_at = received_at;
                last_id = id;
                let Some(bundle) = bundle else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    // conn dropped here; sqlx issues implicit ROLLBACK on return to pool
                    return Ok(());
                }
            }
        }

        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, tx)))]
    async fn poll_pending(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        status: &hardy_bpa::bundle::BundleStatus,
        limit: usize,
    ) -> storage::Result<()> {
        let sf = status::StatusFields::try_from(status)?;

        let mut conn = begin_snapshot(&self.pool).await?;

        let mut last_received_at = time::OffsetDateTime::UNIX_EPOCH;
        let mut last_id: i64 = 0;
        let mut sent: usize = 0;

        loop {
            let page_limit = (limit.saturating_sub(sent) as i64).min(self.poll_page_size);
            let rows = sqlx::query_as::<_, PendingRow>(
                "SELECT id, received_at, bundle, status, peer_id, queue_id,
                        adu_source, adu_ts_ms, adu_ts_seq, service_eid
                 FROM metadata
                 WHERE status    = $1
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
            .bind(sf.status)
            .bind(sf.peer_id)
            .bind(sf.queue_id)
            .bind(&sf.adu_source)
            .bind(sf.adu_ts_ms)
            .bind(sf.adu_ts_seq)
            .bind(&sf.service_eid)
            .bind(last_received_at)
            .bind(last_id)
            .bind(page_limit)
            .fetch_all(&mut *conn)
            .await?;

            if rows.is_empty() {
                break;
            }

            for r in rows {
                let (id, received_at, bundle) = r.decode();
                last_received_at = received_at;
                last_id = id;
                let Some(bundle) = bundle else {
                    continue;
                };
                if tx.send_async(bundle).await.is_err() {
                    // conn dropped here; sqlx issues implicit ROLLBACK on return to pool
                    return Ok(());
                }
                sent += 1;
            }

            if sent >= limit {
                break;
            }
        }

        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(())
    }
}
