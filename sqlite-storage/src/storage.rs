use super::*;
use hardy_bpa::{async_trait, storage};
use rusqlite::OptionalExtension;
use std::{cell::RefCell, path::PathBuf};
use thiserror::Error;

thread_local! {
    static CONNECTION: RefCell<Option<rusqlite::Connection>> = const { RefCell::new(None) };
}

pub struct Storage {
    path: PathBuf,
    timeout: std::time::Duration,
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
                INSERT OR IGNORE INTO unconfirmed_bundles (bundle_id) SELECT id FROM bundles WHERE bundle IS NOT NULL;",
            )
            .trace_expect("Failed to prepare metadata store database");

        Self {
            path: file_path,
            timeout: config.timeout,
        }
    }

    async fn pooled_connection<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let path = self.path.clone();
        let timeout = self.timeout;
        tokio::task::spawn_blocking(move || {
            CONNECTION.with_borrow_mut(|v| {
                if v.is_none() {
                    let conn = rusqlite::Connection::open_with_flags(
                        &path,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )?;
                    conn.busy_timeout(timeout)?;
                    *v = Some(conn);
                }
                f(v.as_mut().unwrap())
            })
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
        while let Some(bundles) = self
            .pooled_connection(move |conn| {
                let trans =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let mut ids = Vec::new();
                let mut bundles = Vec::new();
                for r in trans
                    .prepare_cached(
                        "SELECT quote(id),bundle FROM bundles 
                            JOIN unconfirmed_bundles ON id = bundle_id
                            LIMIT 32",
                    )?
                    .query_map((), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })?
                {
                    let (id, bundle) = r?;
                    ids.push(id);
                    if let Some(bundle) = bundle {
                        let bundle = serde_json::from_str::<hardy_bpa::bundle::Bundle>(&bundle)?;
                        bundles.push(bundle);
                    }
                }
                if ids.is_empty() {
                    return Ok(None);
                }
                let ids = ids.join(",");

                trans
                    .prepare_cached("UPDATE bundles SET bundle = NULL WHERE id IN (?1)")?
                    .execute((&ids,))?;

                trans
                    .prepare_cached("DELETE FROM unconfirmed_bundles WHERE bundle_id IN (?1)")?
                    .execute((&ids,))?;

                trans.commit()?;

                Ok(Some(bundles))
            })
            .await?
        {
            for bundle in bundles {
                if tx.send(bundle).await.is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
