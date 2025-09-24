use super::*;
use hardy_bpa::{async_trait, storage};
use rusqlite::OptionalExtension;
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
            PRAGMA optimize = 0x10002",
        )
        .trace_expect("Failed to optimize");

        drop(guard);
        conn
    }

    async fn get<'a>(
        &'a self,
        guard: Option<&tokio::sync::MutexGuard<'a, ()>>,
    ) -> rusqlite::Connection {
        if let Some(conn) = self.connections.lock().expect("Failed to lock mutex").pop() {
            conn
        } else {
            self.new_connection(guard).await
        }
    }

    fn put(&self, conn: rusqlite::Connection) {
        self.connections
            .lock()
            .expect("Failed to lock mutex")
            .push(conn)
    }
}

pub struct Storage {
    pool: Arc<ConnectionPool>,
    bincode_config: bincode::config::Configuration,
}

impl Storage {
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

        // Migrate the database to the latest schema
        migrate::migrate(&mut connection, upgrade)
            .trace_expect("Failed to migrate metadata store database");

        // connection
        //     .busy_timeout(std::time::Duration::ZERO)
        //     .trace_expect("Failed to set timeout");

        // Mark all existing non-Tombstone bundles as unconfirmed
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                PRAGMA optimize = 0x10002;",
            )
            .trace_expect("Failed to prepare metadata store database");

        Self {
            pool: Arc::new(ConnectionPool::new(path, connection)),
            bincode_config: bincode::config::standard(),
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

fn from_status(status: &hardy_bpa::metadata::BundleStatus) -> (i64, Option<u32>, Option<u32>) {
    match status {
        hardy_bpa::metadata::BundleStatus::Dispatching => (0, None, None),
        hardy_bpa::metadata::BundleStatus::Waiting => (1, None, None),
        hardy_bpa::metadata::BundleStatus::ForwardPending { peer, queue } => {
            (2, Some(*peer), Some(*queue))
        }
        hardy_bpa::metadata::BundleStatus::LocalPending { service } => (3, Some(*service), None),
    }
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn get(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::Bundle>> {
        let id = bincode::encode_to_vec(bundle_id, self.bincode_config)?;
        let Some(bundle) = self
            .read(move |conn| {
                conn
                    .prepare_cached(
                        "SELECT bundle FROM bundles WHERE bundle_id = ?1 AND bundle IS NOT NULL LIMIT 1",
                    )?
                    .query_row((&id,), |row| row.get::<_, Vec<u8>>(0))
                    .optional().map_err(Into::into)
            })
            .await?
        else {
            return Ok(None);
        };

        bincode::decode_from_slice(&bundle, self.bincode_config)
            .map(|(b, _)| Some(b))
            .map_err(Into::into)
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn insert(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<bool> {
        let expiry = bundle.expiry();
        let received_at = bundle.metadata.received_at;
        let (status_code, status_param1, status_param2) = from_status(&bundle.metadata.status);
        let id = bincode::encode_to_vec(&bundle.bundle.id, self.bincode_config)?;
        let bundle = bincode::encode_to_vec(bundle, self.bincode_config)?;
        self.write(move |conn| {
            // Insert bundle
            conn.prepare_cached(
                "INSERT OR IGNORE INTO bundles (bundle_id,bundle,expiry,received_at,status_code,status_param1,status_param2) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?
            .execute((id,bundle,expiry,received_at,status_code,status_param1,status_param2))
            .map(|c| c == 1)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn replace(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        let expiry = bundle.expiry();
        let received_at = bundle.metadata.received_at;
        let (status_code, status_param1, status_param2) = from_status(&bundle.metadata.status);
        let id = bincode::encode_to_vec(&bundle.bundle.id, self.bincode_config)?;
        let bundle = bincode::encode_to_vec(bundle, self.bincode_config)?;
        if self
            .write(move |conn| {
                // Update bundle
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = ?2, expiry = ?3, received_at = ?4, status_code = ?5, status_param1 = ?6, status_param2 = ?7 WHERE bundle_id = ?1",
                )?
                .execute((id,bundle,expiry,received_at,status_code,status_param1,status_param2))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            error!("Failed to replace bundle!");
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        let id = bincode::encode_to_vec(bundle_id, self.bincode_config)?;
        if self
            .write(move |conn| {
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = NULL, status_code = NULL, status_param1 = NULL, status_param2 = NULL WHERE bundle_id = ?1",
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

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn start_recovery(&self) {
        if let Err(e) = self
            .write(move |conn| {
                conn.execute_batch("INSERT OR IGNORE INTO unconfirmed_bundles (id) SELECT id FROM bundles WHERE bundle IS NOT NULL")
                .map_err(Into::into)
            })
            .await
        {
            error!("Failed to mark unconfirmed bundles!: {e}");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::metadata::BundleMetadata>> {
        let id = bincode::encode_to_vec(bundle_id, self.bincode_config)?;
        let Some((bundle, id)) = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle,unconfirmed_bundles.id FROM bundles 
                    LEFT OUTER JOIN unconfirmed_bundles ON bundles.id = unconfirmed_bundles.id 
                    WHERE bundle_id = ?1 LIMIT 1",
                )?
                .query_row((id,), |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                    ))
                })
                .optional()
                .map_err(Into::into)
            })
            .await?
        else {
            return Ok(None);
        };

        if let Some(id) = id {
            // Delete from unconfirmed_bundles
            self.write(move |conn| {
                conn.prepare_cached("DELETE FROM unconfirmed_bundles WHERE id = ?1")?
                    .execute((id,))
                    .map_err(Into::into)
            })
            .await?;
        }

        let Some(bundle) = bundle else {
            return Ok(None);
        };

        match bincode::decode_from_slice::<hardy_bpa::bundle::Bundle, _>(
            &bundle,
            self.bincode_config,
        ) {
            Ok((bundle, _)) => Ok(Some(bundle.metadata)),
            Err(e) => {
                warn!("Garbage bundle found in metadata: {e}");
                self.tombstone(bundle_id).await.map(|_| None)
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn remove_unconfirmed(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
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

                    let sql = (1..=ids.len())
                        .map(|i| format!("?{i}"))
                        .collect::<Vec<String>>()
                        .join(",");                                     

                    let bundles = trans
                        .prepare(&format!("SELECT bundle FROM bundles WHERE id IN ({sql}) AND bundle IS NOT NULL"))?
                        .query_map(rusqlite::params_from_iter(&ids), |row| row.get(0))?
                        .collect::<Result<Vec<Vec<u8>>, _>>()?;

                    trans.execute(
                        &format!("UPDATE bundles SET bundle = NULL, status_code = NULL, status_param1 = NULL, status_param2 = NULL WHERE id IN ({sql})"),
                        rusqlite::params_from_iter(&ids),
                    )?;

                    trans.commit()?;

                    Ok(bundles)
                })
                .await?;

            for bundle in bundles {
                match bincode::decode_from_slice(&bundle, self.bincode_config) {
                    Ok((bundle, _)) => {
                        if tx.send_async(bundle).await.is_err() {
                            // The other end is shutting down - get out
                            break;
                        }
                    }
                    Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
                }
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn reset_peer_queue(&self, peer: u32) -> storage::Result<bool> {
        // Ensure status codes match
        assert!(from_status(&hardy_bpa::metadata::BundleStatus::Waiting).0 == 1);
        assert!(
            from_status(&hardy_bpa::metadata::BundleStatus::ForwardPending { peer, queue: 0 })
                == (2, Some(peer), Some(0))
        );

        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = 1 WHERE status_code = 2 AND status_param1 = ?1",
            )?
            .execute((Some(peer),))
            .map(|c| c == 1)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn poll_expiry(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        limit: usize,
    ) -> storage::Result<()> {
        assert!(from_status(&hardy_bpa::metadata::BundleStatus::Dispatching).0 == 0); // Ensure status codes match

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles 
                        WHERE bundle IS NOT NULL AND status_code != 0
                        ORDER BY expiry ASC
                        LIMIT ?1",
                )?
                .query_map((limit,), |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match bincode::decode_from_slice(&bundle, self.bincode_config) {
                Ok((bundle, _)) => {
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn poll_waiting(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        assert!(from_status(&hardy_bpa::metadata::BundleStatus::Waiting).0 == 1); // Ensure status codes match

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

                    let sql = (1..=ids.len())
                        .map(|i| format!("?{i}"))
                        .collect::<Vec<String>>()
                        .join(",");

                    let bundles = trans
                        .prepare(&format!("SELECT bundle FROM bundles WHERE id IN ({sql}) AND bundle IS NOT NULL"))?
                        .query_map(rusqlite::params_from_iter(&ids), |row| row.get::<_, Vec<u8>>(0))?
                        .collect::<Result<Vec<Vec<u8>>, _>>()?;

                    trans.commit()?;

                    Ok(bundles)
                })
                .await?;

            if bundles.is_empty() {
                return Ok(());
            }

            for bundle in bundles {
                match bincode::decode_from_slice(&bundle, self.bincode_config) {
                    Ok((bundle, _)) => {
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn poll_pending(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
        state: &hardy_bpa::metadata::BundleStatus,
        limit: usize,
    ) -> storage::Result<()> {
        let (status, status_param1, status_param2) = from_status(state);

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles 
                        WHERE status_code = ?1 AND status_param1 IS ?2 AND status_param2 IS ?3
                        ORDER BY received_at ASC
                        LIMIT ?4",
                )?
                .query_map((status, status_param1, status_param2, limit), |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match bincode::decode_from_slice(&bundle, self.bincode_config) {
                Ok((bundle, _)) => {
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
