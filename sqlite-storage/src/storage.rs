use super::*;
use hardy_bpa::{async_trait, bundle::BundleStatus, storage};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct ConnectionPool {
    path: PathBuf,
    connections: Mutex<Vec<rusqlite::Connection>>,
    write_lock: tokio::sync::Mutex<()>,
}

impl ConnectionPool {
    fn new(path: PathBuf, connection: rusqlite::Connection) -> Self {
        Self {
            path,
            connections: Mutex::new(vec![connection]),
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn new_connection<'a>(
        &'a self,
        guard: Option<&tokio::sync::MutexGuard<'a, ()>>,
    ) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .trace_expect("Failed to open connection");

        // conn.busy_timeout(std::time::Duration::ZERO)
        //     .trace_expect("Failed to set timeout");

        // We need a guard here, if we don't already have one, because we are writing to the DB
        let guard = if guard.is_none() {
            Some(self.write_lock.lock().await)
        } else {
            None
        };

        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
            PRAGMA optimize = 0x10002;",
        )
        .trace_expect("Failed to optimize");

        rusqlite::vtab::array::load_module(&conn).trace_expect("Failed to load array module");

        drop(guard);
        conn
    }

    async fn get<'a>(
        &'a self,
        guard: Option<&tokio::sync::MutexGuard<'a, ()>>,
    ) -> rusqlite::Connection {
        if let Some(conn) = self
            .connections
            .lock()
            .trace_expect("Failed to lock mutex")
            .pop()
        {
            conn
        } else {
            self.new_connection(guard).await
        }
    }

    fn put(&self, conn: rusqlite::Connection) {
        self.connections
            .lock()
            .trace_expect("Failed to lock mutex")
            .push(conn)
    }
}

/// SQLite-backed implementation of [`MetadataStorage`](storage::MetadataStorage).
///
/// Manages a pool of read connections and a single serialized write lock to
/// avoid SQLite busy errors. Bundle metadata is stored as JSON blobs alongside
/// typed status columns for efficient status-based queries.
pub struct Storage {
    pool: Arc<ConnectionPool>,
}

impl Storage {
    /// Opens or creates the SQLite database and runs schema migrations.
    ///
    /// If the database file does not exist it is created and `upgrade` is
    /// forced to `true`. When `upgrade` is `true`, pending schema migrations
    /// are applied.
    pub fn new(config: &Config, mut upgrade: bool) -> Self {
        // Ensure directory exists
        std::fs::create_dir_all(&config.db_dir).trace_expect(&format!(
            "Failed to create metadata store directory {}",
            config.db_dir.display()
        ));

        // Compose DB name
        let path = config.db_dir.join(&config.db_name);

        info!("Using database: {}", path.display());

        // Attempt to open existing database first
        let mut connection = match rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::CannotOpen,
                    ..
                },
                _,
            )) => {
                // Create database
                upgrade = true;
                rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
            }
            r => r,
        }
        .trace_expect("Failed to open metadata store database");

        // connection
        //     .busy_timeout(std::time::Duration::ZERO)
        //     .trace_expect("Failed to set timeout");

        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                PRAGMA optimize = 0x10002;",
            )
            .trace_expect("Failed to prepare metadata store database");

        rusqlite::vtab::array::load_module(&connection).trace_expect("Failed to load array module");

        // Migrate the database to the latest schema
        migrate::migrate(&mut connection, upgrade)
            .trace_expect("Failed to migrate metadata store database");

        Self {
            pool: Arc::new(ConnectionPool::new(path, connection)),
        }
    }

    async fn read<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let mut conn = self.pool.get(None).await;
        let r = f(&mut conn);
        self.pool.put(conn);
        r
    }

    async fn write<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let guard = self.pool.write_lock.lock().await;
        let mut conn = self.pool.get(Some(&guard)).await;
        let r = f(&mut conn);
        drop(guard);
        self.pool.put(conn);
        r
    }
}

// status_code layout:
//
// 0 = New
// 1 = Waiting
// 2 = ForwardPending(peer, queue)
// 3 = AduFragment(timestamp, seq, source)
// 4 = Dispatching
// 5 = WaitingForService(source)
fn from_status(status: &BundleStatus) -> (i64, Option<i64>, Option<i64>, Option<String>) {
    match status {
        BundleStatus::New => (0, None, None, None),
        BundleStatus::Waiting => (1, None, None, None),
        BundleStatus::ForwardPending { peer, queue } => {
            (2, Some(*peer as i64), queue.map(|q| q as i64), None)
        }
        BundleStatus::AduFragment { source, timestamp } => (
            3,
            Some(
                timestamp
                    .creation_time()
                    .map_or(0i64, |t| t.millisecs() as i64),
            ),
            Some(timestamp.sequence_number() as i64),
            Some(source.to_string()),
        ),
        BundleStatus::Dispatching => (4, None, None, None),
        BundleStatus::WaitingForService { service } => (5, None, None, Some(service.to_string())),
    }
}

fn to_status(
    code: i64,
    param1: Option<i64>,
    param2: Option<i64>,
    param3: Option<String>,
) -> Option<BundleStatus> {
    match code {
        0 => Some(BundleStatus::New),
        1 => Some(BundleStatus::Waiting),
        2 => Some(BundleStatus::ForwardPending {
            peer: param1? as u32,
            queue: param2.map(|q| q as u32),
        }),
        3 => {
            let source: hardy_bpv7::eid::Eid = param3?.parse().ok()?;
            let creation_time = param1
                .filter(|&ms| ms != 0)
                .map(|ms| hardy_bpv7::dtn_time::DtnTime::new(ms as u64));
            let sequence_number = param2? as u64;
            let timestamp = hardy_bpv7::creation_timestamp::CreationTimestamp::from_parts(
                creation_time,
                sequence_number,
            );
            Some(BundleStatus::AduFragment { source, timestamp })
        }
        4 => Some(BundleStatus::Dispatching),
        5 => Some(BundleStatus::WaitingForService {
            service: param3?.parse().ok()?,
        }),
        _ => None,
    }
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn get(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::Bundle>> {
        let id = serde_json::to_vec(bundle_id)?;
        let Some((bundle, status_code, p1, p2, p3)) = self
            .read(move |conn| {
                conn
                    .prepare_cached(
                        "SELECT bundle, status_code, status_param1, status_param2, status_param3 FROM bundles WHERE bundle_id = ?1 AND bundle IS NOT NULL LIMIT 1",
                    )?
                    .query_row((&id,), |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    })
                    .optional().map_err(Into::into)
            })
            .await?
        else {
            return Ok(None);
        };

        let mut bundle: hardy_bpa::bundle::Bundle = serde_json::from_slice(&bundle)?;
        if let Some(status) = to_status(status_code, p1, p2, p3) {
            bundle.metadata.status = status;
            Ok(Some(bundle))
        } else {
            warn!("Failed to unpack metadata status: code = {status_code}");
            Ok(None)
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    async fn insert(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<bool> {
        let expiry = bundle.expiry();
        let received_at = bundle.metadata.read_only.received_at;
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.metadata.status);
        let id = serde_json::to_vec(&bundle.bundle.id)?;
        let bundle = serde_json::to_vec(bundle)?;
        self.write(move |conn| {
            // Insert bundle
            conn.prepare_cached(
                "INSERT OR IGNORE INTO bundles (bundle_id,bundle,expiry,received_at,status_code,status_param1,status_param2,status_param3) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?
            .execute((id,bundle,expiry,received_at,status_code,status_param1,status_param2,status_param3))
            .map(|c| c == 1)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    async fn replace(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        let expiry = bundle.expiry();
        let received_at = bundle.metadata.read_only.received_at;
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.metadata.status);
        let id = serde_json::to_vec(&bundle.bundle.id)?;
        let bundle = serde_json::to_vec(bundle)?;
        if self
            .write(move |conn| {
                // Update bundle
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = ?2, expiry = ?3, received_at = ?4, status_code = ?5, status_param1 = ?6, status_param2 = ?7, status_param3 = ?8 WHERE bundle_id = ?1",
                )?
                .execute((id,bundle,expiry,received_at,status_code,status_param1,status_param2,status_param3))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            error!("Failed to replace bundle!");
        }
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    async fn update_status(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.metadata.status);
        let id = serde_json::to_vec(&bundle.bundle.id)?;
        if self
            .write(move |conn| {
                conn.prepare_cached(
                    "UPDATE bundles SET status_code = ?2, status_param1 = ?3, status_param2 = ?4, status_param3 = ?5 WHERE bundle_id = ?1",
                )?
                .execute((id, status_code, status_param1, status_param2, status_param3))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            error!("Failed to update bundle status!");
        }
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        let id = serde_json::to_vec(bundle_id)?;
        if self
            .write(move |conn| {
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = NULL, status_code = NULL, status_param1 = NULL, status_param2 = NULL, status_param3 = NULL WHERE bundle_id = ?1",
                )?
                .execute((id,))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            error!("Failed to tombstone bundle!");
        }
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn start_recovery(&self) {
        self
            .write(move |conn| {
                conn.execute_batch("INSERT OR IGNORE INTO unconfirmed_bundles (id) SELECT id FROM bundles WHERE bundle IS NOT NULL")
                .map_err(Into::into)
            })
            .await.unwrap_or_else(|e|
        {
            error!("Failed to mark unconfirmed bundles!: {e}");
        })
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::BundleMetadata>> {
        let id = serde_json::to_vec(bundle_id)?;
        let Some((bundle, status_code, p1, p2, p3))  = self
            .write(move |conn| {
                conn.prepare_cached(
                    "DELETE FROM unconfirmed_bundles WHERE id = (SELECT id FROM bundles WHERE bundle_id = ?1)",
                )?
                .execute((&id,))?;

                conn.prepare_cached(
                    "SELECT bundle, status_code, status_param1, status_param2, status_param3 FROM bundles WHERE bundle_id = ?1 LIMIT 1",
                )?
                .query_row((id,), |row| {
                     Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                })
                .optional()
                .map_err(Into::into)
            })
            .await? else {
            return Ok(None);
        };

        match serde_json::from_slice::<hardy_bpa::bundle::Bundle>(&bundle) {
            Ok(mut bundle) => {
                if let Some(status) = to_status(status_code, p1, p2, p3) {
                    bundle.metadata.status = status;
                    Ok(Some(bundle.metadata))
                } else {
                    error!("Failed to unpack metadata status: code = {status_code}");
                    self.tombstone(bundle_id).await.map(|_| None)
                }
            }
            Err(e) => {
                warn!("Garbage bundle found in metadata: {e}");
                self.tombstone(bundle_id).await.map(|_| None)
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn remove_unconfirmed(
        &self,
        tx: flume::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        loop {
            let bundles = self
                .write(move |conn| {
                    let trans =
                        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                    let ids = trans
                        .prepare_cached(
                            "DELETE FROM unconfirmed_bundles
                            WHERE id IN (SELECT id FROM unconfirmed_bundles LIMIT 64)
                            RETURNING id",
                        )?
                        .query_map([], |row| row.get(0))?
                        .collect::<Result<Vec<i64>, _>>()?;

                    if ids.is_empty() {
                        return Ok(Vec::new());
                    }

                    let id_values = std::rc::Rc::new(
                        ids.into_iter()
                            .map(rusqlite::types::Value::from)
                            .collect::<Vec<_>>(),
                    );

                    let bundles = trans
                        .prepare_cached(
                            "UPDATE bundles SET bundle = NULL, status_code = NULL, status_param1 = NULL, status_param2 = NULL, status_param3 = NULL WHERE id IN rarray(?1) AND bundle IS NOT NULL RETURNING bundle",
                        )?
                        .query_map([id_values], |row| row.get(0))?
                        .collect::<Result<Vec<Vec<u8>>, _>>()?;

                    trans.commit()?;

                    Ok(bundles)
                })
                .await?;

            if bundles.is_empty() {
                return Ok(());
            }

            for bundle in bundles {
                match serde_json::from_slice(&bundle) {
                    Ok(bundle) => {
                        if tx.send_async(bundle).await.is_err() {
                            // The other end is shutting down - get out
                            return Ok(());
                        }
                    }
                    Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn reset_peer_queue(&self, peer: u32) -> storage::Result<u64> {
        // Ensure status codes match
        debug_assert!(
            from_status(&BundleStatus::Waiting).0 == 1,
            "Status code mismatch"
        );
        debug_assert!(
            from_status(&BundleStatus::ForwardPending {
                peer,
                queue: Some(0)
            }) == (2, Some(peer as i64), Some(0), None),
            "Status code mismatch"
        );

        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = 1, status_param1 = NULL, status_param2 = NULL WHERE status_code = 2 AND status_param1 = ?1",
            )?
            .execute((Some(peer),))
            .map(|c| c as u64)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, tx)))]
    async fn poll_expiry(
        &self,
        tx: flume::Sender<hardy_bpa::bundle::Bundle>,
        limit: usize,
    ) -> storage::Result<()> {
        debug_assert!(
            from_status(&BundleStatus::New).0 == 0,
            "Status code mismatch"
        ); // Ensure status codes match

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle, status_code, status_param1, status_param2, status_param3 FROM bundles
                        WHERE bundle IS NOT NULL AND status_code != 0
                        ORDER BY expiry ASC
                        LIMIT ?1",
                )?
                .query_map((limit as isize,), |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for (bundle, status_code, p1, p2, p3) in bundles {
            match serde_json::from_slice::<hardy_bpa::bundle::Bundle>(&bundle) {
                Ok(mut bundle) => {
                    if let Some(status) = to_status(status_code, p1, p2, p3) {
                        bundle.metadata.status = status;
                        if tx.send_async(bundle).await.is_err() {
                            // The other end is shutting down - get out
                            break;
                        }
                    } else {
                        warn!("Failed to unpack metadata status: code = {status_code}");
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn poll_waiting(
        &self,
        tx: flume::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        debug_assert!(
            from_status(&BundleStatus::Waiting).0 == 1,
            "Status code mismatch"
        ); // Ensure status codes match

        // Refresh the waiting queue
        self.write(move |conn| {
            conn.execute_batch(
                "INSERT OR IGNORE INTO waiting_queue (id,received_at) SELECT id,received_at FROM bundles WHERE status_code = 1",
            )
            .map_err(Into::into)
        }).await?;

        loop {
            let bundles = self
                .write(move |conn| {
                    let trans =
                        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                    let ids = trans
                        .prepare_cached(
                            "DELETE FROM waiting_queue 
                            WHERE id IN (SELECT id FROM waiting_queue ORDER BY received_at ASC LIMIT 64)
                            RETURNING id",
                        )?
                        .query_map([], |row| row.get(0))?
                        .collect::<Result<Vec<i64>, _>>()?;

                    if ids.is_empty() {
                        return Ok(Vec::new()); // No bundles to process
                    }

                    let id_values = std::rc::Rc::new(
                        ids.into_iter()
                            .map(rusqlite::types::Value::from)
                            .collect::<Vec<_>>(),
                    );

                    let bundles = trans
                        .prepare_cached("SELECT bundle FROM bundles WHERE id IN rarray(?1) AND bundle IS NOT NULL ORDER BY received_at ASC")?
                        .query_map([id_values], |row| row.get::<_, Vec<u8>>(0))?
                        .collect::<Result<Vec<Vec<u8>>, _>>()?;

                    trans.commit()?;

                    Ok(bundles)
                })
                .await?;

            if bundles.is_empty() {
                return Ok(());
            }

            for bundle in bundles {
                match serde_json::from_slice::<hardy_bpa::bundle::Bundle>(&bundle) {
                    Ok(mut bundle) => {
                        bundle.metadata.status = BundleStatus::Waiting;
                        if tx.send_async(bundle).await.is_err() {
                            // The other end is shutting down - get out
                            return Ok(());
                        }
                    }
                    Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, tx)))]
    async fn poll_service_waiting(
        &self,
        source: hardy_bpv7::eid::Eid,
        tx: flume::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        debug_assert!(
            from_status(&BundleStatus::WaitingForService {
                service: source.clone()
            })
            .0 == 5,
            "Status code mismatch"
        ); // Ensure status codes match

        let source_str = source.to_string();
        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles
                        WHERE bundle IS NOT NULL AND status_code = 5 AND status_param3 = ?1
                        ORDER BY received_at ASC",
                )?
                .query_map((source_str,), |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<hardy_bpa::bundle::Bundle>(&bundle) {
                Ok(mut bundle) => {
                    bundle.metadata.status = BundleStatus::WaitingForService {
                        service: source.clone(),
                    };
                    if tx.send_async(bundle).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, tx)))]
    async fn poll_adu_fragments(
        &self,
        tx: flume::Sender<hardy_bpa::bundle::Bundle>,
        status: &BundleStatus,
    ) -> storage::Result<()> {
        let (status_code, status_param1, status_param2, status_param3) = from_status(status);

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles
                        WHERE bundle IS NOT NULL AND status_code = ?1 AND status_param1 IS ?2 AND status_param2 IS ?3 AND status_param3 IS ?4
                        ORDER BY received_at ASC",
                )?
                .query_map((status_code, status_param1, status_param2,status_param3), |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<hardy_bpa::bundle::Bundle>(&bundle) {
                Ok(mut bundle) => {
                    bundle.metadata.status = status.clone();
                    if tx.send_async(bundle).await.is_err() {
                        // The other end is shutting down - get out
                        break;
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, tx)))]
    async fn poll_pending(
        &self,
        tx: flume::Sender<hardy_bpa::bundle::Bundle>,
        status: &BundleStatus,
        limit: usize,
    ) -> storage::Result<()> {
        let (status_code, status_param1, status_param2, status_param3) = from_status(status);

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles
                        WHERE bundle IS NOT NULL AND status_code = ?1 AND status_param1 IS ?2 AND status_param2 IS ?3 AND status_param3 IS ?4
                        ORDER BY received_at ASC
                        LIMIT ?5",
                )?
                .query_map((status_code, status_param1, status_param2,status_param3, limit as isize), |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<hardy_bpa::bundle::Bundle>(&bundle) {
                Ok(mut bundle) => {
                    bundle.metadata.status = status.clone();
                    if tx.send_async(bundle).await.is_err() {
                        // The other end is shutting down - get out
                        break;
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardy_bpa::storage::MetadataStorage;

    fn make_config(dir: &std::path::Path) -> crate::Config {
        crate::Config {
            db_dir: dir.to_path_buf(),
            db_name: "test.db".into(),
        }
    }

    fn make_bundle(dest_service: u64) -> hardy_bpa::bundle::Bundle {
        use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp, eid::Eid};

        let source: Eid = "ipn:1.0".parse().unwrap();
        let dest: Eid = format!("ipn:2.{dest_service}").parse().unwrap();
        let (_bundle, data) = Builder::new(source, dest)
            .with_payload(b"test".to_vec().into())
            .build(CreationTimestamp::now())
            .unwrap();

        let parsed =
            hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys).unwrap();

        hardy_bpa::bundle::Bundle {
            bundle: parsed.bundle,
            metadata: hardy_bpa::bundle::BundleMetadata::default(),
        }
    }

    // SQL-01: Database is created at the configured path.
    #[tokio::test]
    async fn test_configuration_custom_db_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let _store = Storage::new(&config, true);

        let db_path = dir.path().join("test.db");
        assert!(
            db_path.exists(),
            "database file should be created at configured path"
        );
    }

    // SQL-04: Concurrent writers do not panic or deadlock.
    #[tokio::test]
    async fn test_concurrency_no_sqlite_busy() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let store = Arc::new(Storage::new(&config, true));

        // Create all bundles upfront so we can capture their IDs for verification
        let bundles: Vec<_> = (0..10).map(make_bundle).collect();
        let ids: Vec<_> = bundles.iter().map(|b| b.bundle.id.clone()).collect();

        let mut handles = Vec::new();
        for bundle in bundles {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store.insert(&bundle).await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // Verify all 10 were inserted by reading them back
        for (i, id) in ids.iter().enumerate() {
            let result = store.get(id).await.unwrap();
            assert!(result.is_some(), "bundle {i} should exist");
        }
    }

    // SQL-05: Corrupt data in the DB does not panic.
    //
    // `get()` returns an error on corrupt blob data (deserialization failure).
    // `confirm_exists()` handles it gracefully by tombstoning the entry.
    #[tokio::test]
    async fn test_corrupt_data_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let store = Storage::new(&config, true);

        // Insert a valid bundle
        let bundle = make_bundle(0);
        let id_bytes = serde_json::to_vec(&bundle.bundle.id).unwrap();
        assert!(store.insert(&bundle).await.unwrap());

        // Corrupt the bundle blob directly in the DB
        {
            let db_path = dir.path().join("test.db");
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE bundles SET bundle = X'DEADBEEF' WHERE bundle_id = ?1",
                [&id_bytes],
            )
            .unwrap();
        }

        // get() returns Err (deserialization failure), not panic
        let result = store.get(&bundle.bundle.id).await;
        assert!(result.is_err(), "get() should return Err for corrupt data");

        // confirm_exists() handles it gracefully — tombstones the entry
        store.start_recovery().await;
        let result = store.confirm_exists(&bundle.bundle.id).await.unwrap();
        assert!(
            result.is_none(),
            "confirm_exists should return None for corrupt data"
        );

        // Entry should now be tombstoned
        let result = store.get(&bundle.bundle.id).await.unwrap();
        assert!(result.is_none(), "tombstoned entry should return None");
    }

    // SQL-06: Waiting queue is invalidated when bundle status changes.
    #[tokio::test]
    async fn test_waiting_queue_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let store = Storage::new(&config, true);

        // Insert a bundle with Waiting status
        let mut bundle = make_bundle(0);
        bundle.metadata.status = BundleStatus::Waiting;
        assert!(store.insert(&bundle).await.unwrap());

        // Poll waiting — should return the bundle (populates waiting_queue)
        let (tx, rx) = flume::unbounded();
        store.poll_waiting(tx).await.unwrap();
        let polled: Vec<_> = rx.drain().collect();
        assert_eq!(polled.len(), 1, "should poll 1 waiting bundle");

        // Update status to Dispatching
        bundle.metadata.status = BundleStatus::Dispatching;
        store.replace(&bundle).await.unwrap();

        // Poll waiting again — should return nothing
        let (tx, rx) = flume::unbounded();
        store.poll_waiting(tx).await.unwrap();
        let polled: Vec<_> = rx.drain().collect();
        assert_eq!(
            polled.len(),
            0,
            "waiting queue should be empty after status change"
        );
    }
}
