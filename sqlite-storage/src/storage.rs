use super::*;
use hardy_bpa::{async_trait, storage};
use rusqlite::OptionalExtension;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use thiserror::Error;

pub struct ConnectionPool {
    path: PathBuf,
    timeout: std::time::Duration,
    inner: Mutex<Vec<rusqlite::Connection>>,
}

impl ConnectionPool {
    fn get(&self) -> rusqlite::Connection {
        if let Some(conn) = self.inner.lock().expect("Failed to lock mutex").pop() {
            return conn;
        }

        let conn = rusqlite::Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("Failed to open connection");

        conn.busy_timeout(self.timeout)
            .expect("Failed to set timeout");

        conn.execute_batch("PRAGMA optimize=0x10002")
            .trace_expect("Failed to optimize");

        conn
    }

    fn put(&self, conn: rusqlite::Connection) {
        let mut conns = self.inner.lock().expect("Failed to lock mutex");
        if conns.len() < 16 {
            conns.push(conn);
        }
    }
}

pub struct Storage {
    pool: Arc<ConnectionPool>,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

impl Storage {
    pub fn new(config: &Config, mut upgrade: bool) -> Self {
        // Ensure directory exists
        std::fs::create_dir_all(&config.db_dir).trace_expect(&format!(
            "Failed to create metadata store directory {}",
            config.db_dir.display()
        ));

        // Compose DB name
        let file_path = config.db_dir.join(&config.db_name);

        info!("Using database: {}", file_path.display());

        // Attempt to open existing database first
        let mut connection = match rusqlite::Connection::open_with_flags(
            &file_path,
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
                    &file_path,
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

        // Mark all existing non-Tombstone bundles as unconfirmed
        connection
            .execute_batch(
            "PRAGMA optimize=0x10002;
                INSERT OR IGNORE INTO unconfirmed_bundles (bundle_id) SELECT id FROM bundles WHERE bundle IS NOT NULL",
            )
            .trace_expect("Failed to prepare metadata store database");

        Self {
            pool: Arc::new(ConnectionPool {
                path: file_path,
                timeout: config.timeout,
                inner: Mutex::new(vec![connection]),
            }),
        }
    }

    async fn pooled_connection<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get();
            let r = f(&mut conn);
            pool.put(conn);
            r
        })
        .await
        .trace_expect("Failed to spawn blocking thread")
    }
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    #[instrument(skip(self))]
    async fn load(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::Bundle>> {
        let id = serde_json::to_string(bundle_id)?;
        self.pooled_connection(move |conn| {
            if let Some(s) = conn
                .prepare_cached(
                    "SELECT bundle FROM bundles WHERE id = ?1 AND bundle IS NOT NULL LIMIT 1",
                )?
                .query_row((id,), |row| row.get::<_, String>(0))
                .optional()?
            {
                serde_json::from_str(&s).map_err(Into::into)
            } else {
                Ok(None)
            }
        })
        .await
    }

    #[instrument(skip(self))]
    async fn store(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<bool> {
        let expiry = bundle.expiry();
        let id = serde_json::to_string(&bundle.bundle.id)?;
        let bundle = serde_json::to_string(bundle)?;
        self.pooled_connection(move |conn| {
            // Insert bundle
            conn.prepare_cached(
                "INSERT OR IGNORE INTO bundles (id,bundle,expiry) VALUES (?1,?2,?3)",
            )?
            .execute((id, bundle, expiry))
            .map(|c| c == 1)
            .map_err(Into::into)
        })
        .await
    }

    #[instrument(skip(self))]
    async fn remove(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        let id = serde_json::to_string(bundle_id)?;
        self.pooled_connection(move |conn| {
            conn.prepare_cached("UPDATE bundles SET bundle = NULL WHERE id = ?1")?
                .execute((id,))
                .map(|count| count != 0)?
                .then_some(())
                .ok_or(Error::NotFound.into())
        })
        .await
    }

    #[instrument(skip(self))]
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<hardy_bpa::bundle::Bundle>> {
        let id = serde_json::to_string(bundle_id)?;
        self.pooled_connection(move |conn| {
            let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

            // Check if bundle exists
            let Some(s) = trans
                .prepare_cached(
                    "SELECT bundle FROM bundles WHERE id = ?1 AND bundle IS NOT NULL LIMIT 1",
                )?
                .query_row((&id,), |row| row.get::<_, String>(0))
                .optional()?
            else {
                return Ok(None);
            };

            // Remove from unconfirmed set
            if trans
                .prepare_cached("DELETE FROM unconfirmed_bundles WHERE bundle_id = ?1")?
                .execute((id,))?
                != 0
            {
                trans.commit()?;
            }

            // Unpack the bundle
            serde_json::from_str(&s).map(Some).map_err(Into::into)
        })
        .await
    }

    #[instrument(skip_all)]
    async fn remove_unconfirmed_bundles(&self, tx: storage::Sender) -> storage::Result<()> {
        self.pooled_connection(move |conn| {
            loop {
                let trans =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let mut ids = Vec::new();
                let mut bundles = Vec::new();
                for r in trans
                    .prepare_cached(
                        "SELECT quote(id),bundle FROM bundles 
                                JOIN unconfirmed_bundles ON id = bundle_id
                                LIMIT 256",
                    )?
                    .query_map((), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })?
                {
                    let (id, bundle) = r?;
                    ids.push(id);
                    if let Some(bundle) = bundle {
                        bundles.push(bundle);
                    }
                }
                if ids.is_empty() {
                    return Ok(());
                }
                let ids = ids.join(",");

                trans
                    .prepare_cached("UPDATE bundles SET bundle = NULL WHERE id IN (?1)")?
                    .execute((&ids,))?;

                trans
                    .prepare_cached("DELETE FROM unconfirmed_bundles WHERE bundle_id IN (?1)")?
                    .execute((&ids,))?;

                trans.commit()?;

                for bundle in bundles {
                    if tx
                        .blocking_send(serde_json::from_str::<hardy_bpa::bundle::Bundle>(&bundle)?)
                        .is_err()
                    {
                        // The other end is shutting down - get out
                        return Ok(());
                    }
                }
            }
        })
        .await
    }
}
