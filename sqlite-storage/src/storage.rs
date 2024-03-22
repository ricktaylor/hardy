use super::*;
use hardy_bpa_core::{storage::MetadataStorage, *};
use hardy_cbor as cbor;
use std::{fs::create_dir_all, path::PathBuf, sync::Arc};

pub struct Storage {
    connection: tokio::sync::Mutex<rusqlite::Connection>,
}

impl Storage {
    pub fn init(config: &config::Config) -> Result<std::sync::Arc<Self>, anyhow::Error> {
        // Compose DB name
        let file_path = [&config.db_dir, "metadata.db"].iter().collect::<PathBuf>();

        // Ensure directory exists
        create_dir_all(file_path.parent().unwrap())?;

        // Create database
        let mut connection = rusqlite::Connection::open_with_flags(
            &file_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        // Migrate the database to the latest schema
        migrate::migrate(&mut connection)?;

        Ok(Arc::new(Storage {
            connection: tokio::sync::Mutex::new(connection),
        }))
    }
}

impl MetadataStorage for Storage {
    async fn store(
        &self,
        storage_name: &str,
        bundle: &bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let bundle_id = trans
            .prepare_cached(
                r#"
            INSERT INTO bundles (
                file_name,
                flags,
                destination,
                creation_time,
                creation_seq_num,
                lifetime,
                source,
                report_to)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8);"#,
            )?
            .insert((
                storage_name,
                bundle.primary.flags.as_u64(),
                cbor::encode::write(&bundle.primary.destination),
                bundle.primary.timestamp.0,
                bundle.primary.timestamp.1,
                bundle.primary.lifetime,
                if let bundle::Eid::Null = bundle.primary.source {
                    None
                } else {
                    Some(cbor::encode::write(&bundle.primary.source))
                },
                if let bundle::Eid::Null = bundle.primary.report_to {
                    None
                } else {
                    Some(cbor::encode::write(&bundle.primary.report_to))
                },
            ))?;

        let mut block_query = trans.prepare_cached(
            r#"
            INSERT INTO bundle_blocks (
                bundle_id,
                block_type,
                block_num,
                flags,
                data_offset)
            VALUES (?1,?2,?3,?4,?5);"#,
        )?;

        // Insert extension blocks
        for (block_num, block) in &bundle.extensions {
            block_query.execute((
                bundle_id,
                block.block_type.as_u64(),
                block_num,
                block.flags.as_u64(),
                block.data_offset,
            ))?;
        }

        // Insert fragments
        if let Some(fragment_info) = &bundle.primary.fragment_info {
            trans
                .prepare_cached(
                    r#"
                INSERT INTO bundle_fragments (
                    bundle_id,
                    offset,
                    total_len)
                VALUES (?1,?2,?3);"#,
                )?
                .execute((bundle_id, fragment_info.offset, fragment_info.total_len))?;
        }
        Ok(())
    }

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error> {
        todo!()
    }
}
