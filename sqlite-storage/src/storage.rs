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

        conn.busy_timeout(std::time::Duration::ZERO)
            .trace_expect("Failed to set timeout");

        // We need a guard here, if we don't already have one, because we are writing to the DB
        let guard = if guard.is_none() {
            Some(self.write_lock.lock().await)
        } else {
            None
        };

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            PRAGMA optimize=0x10002",
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

        connection
            .busy_timeout(std::time::Duration::ZERO)
            .trace_expect("Failed to set timeout");

        // Mark all existing non-Tombstone bundles as unconfirmed
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                PRAGMA optimize=0x10002;
                INSERT OR IGNORE INTO unconfirmed_bundles (id) SELECT id FROM bundles WHERE bundle IS NOT NULL",
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

#[async_trait]
impl storage::MetadataStorage for Storage {
    #[instrument(skip(self))]
    async fn get(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::Bundle>> {
        let id = bincode::encode_to_vec(bundle_id, self.bincode_config)?;
        let Some(bundle) = self
            .read(move |conn| {
                let r = conn
                    .prepare_cached(
                        "SELECT bundle FROM bundles WHERE bundle_id = ?1 AND bundle IS NOT NULL LIMIT 1",
                    )?
                    .query_row((&id,), |row| row.get::<_, Vec<u8>>(0))
                    .optional()?;
                Ok(r)
            })
            .await?
        else {
            return Ok(None);
        };

        bincode::decode_from_slice(&bundle, self.bincode_config)
            .map(|(b, _)| Some(b))
            .map_err(Into::into)
    }

    #[instrument(skip(self))]
    async fn insert(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<bool> {
        let expiry = bundle.expiry();
        let id = bincode::encode_to_vec(&bundle.bundle.id, self.bincode_config)?;
        let bundle = bincode::encode_to_vec(bundle, self.bincode_config)?;
        self.write(move |conn| {
            // Insert bundle
            conn.prepare_cached(
                "INSERT OR IGNORE INTO bundles (bundle_id,bundle,expiry) VALUES (?1,?2,?3)",
            )?
            .execute((&id, bundle, expiry))
            .map(|c| c == 1)
            .map_err(Into::into)
        })
        .await
    }

    #[instrument(skip(self))]
    async fn replace(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        let expiry = bundle.expiry();
        let id = bincode::encode_to_vec(&bundle.bundle.id, self.bincode_config)?;
        let bundle = bincode::encode_to_vec(bundle, self.bincode_config)?;
        if self
            .write(move |conn| {
                // Update bundle
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = ?2, expiry = ?3 WHERE bundle_id = ?1",
                )?
                .execute((&id, bundle, expiry))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            error!("Failed to replace bundle!");
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        let id = bincode::encode_to_vec(bundle_id, self.bincode_config)?;
        if self
            .write(move |conn| {
                conn.prepare_cached("UPDATE bundles SET bundle = NULL WHERE bundle_id = ?1")?
                    .execute((&id,))
                    .map_err(Into::into)
            })
            .await?
            != 1
        {
            error!("Failed to tombstone bundle!");
        }
        Ok(())
    }

    #[instrument(skip(self))]
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
                    WHERE bundle_id = ?1 AND bundle IS NOT NULL LIMIT 1",
                )?
                .query_row((&id,), |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?))
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
                    .execute((&id,))
                    .map_err(Into::into)
            })
            .await?;
        }

        if let Ok((bundle, _)) = bincode::decode_from_slice(&bundle, self.bincode_config) {
            Ok(Some(bundle))
        } else {
            warn!("Garbage bundle found in metadata!");
            self.tombstone(bundle_id).await.map(|_| None)
        }
    }

    #[instrument(skip_all)]
    async fn remove_unconfirmed(
        &self,
        tx: storage::Sender<hardy_bpa::bundle::Bundle>,
    ) -> storage::Result<()> {
        loop {
            let tx = tx.clone();
            let Some(bundle) = self.write(move |conn| {
                let trans =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let Some((id,bundle)) = trans
                    .prepare_cached(
                        "SELECT bundles.id,bundle FROM bundles JOIN unconfirmed_bundles ON unconfirmed_bundles.id = bundles.id LIMIT 1",
                    )?
                    .query_row((), |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
                    }).optional()? else {
                        return Ok(None);
                    };

                if trans
                    .prepare_cached("UPDATE bundles SET bundle = NULL WHERE id = ?1")?
                    .execute((&id,))? != 1 {
                        error!("Failed to tombstone unconfirmed bundle!");
                    }

                if trans
                    .prepare_cached("DELETE FROM unconfirmed_bundles WHERE id = ?1")?
                    .execute((&id,))? != 1 {
                        error!("Failed to delete unconfirmed bundle!");
                    }

                trans.commit()?;

                Ok(bundle)
            })
            .await? else {
                return Ok(());
            };

            match bincode::decode_from_slice(&bundle, self.bincode_config) {
                Ok((bundle, _)) => {
                    if tx.send(bundle).await.is_err() {
                        // The other end is shutting down - get out
                        return Ok(());
                    }
                }
                Err(e) => warn!("Garbage bundle found in metadata: {e}"),
            }
        }
    }
}
