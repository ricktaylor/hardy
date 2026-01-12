use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error("Database does not contain historic migration '{0}'")]
    MissingHistoric(String),

    #[error("Database contains unexpected historic migration '{0}'")]
    ExtraHistoric(String),

    #[error("Historic migration '{0}' has a different hash")]
    AlteredHistoric(String),

    #[error("Database schema requires updating")]
    UpdateRequired,
}

#[cfg_attr(feature = "tracing", instrument(skip(conn)))]
pub fn migrate(conn: &mut rusqlite::Connection, upgrade: bool) -> Result<(), Error> {
    let migrations = include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

    let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?;

    // Ensure we have a migrations table
    trans.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS schema_versions (
            seq_no INTEGER UNIQUE NOT NULL,
            file_name TEXT UNIQUE NOT NULL,
            hash BLOB NOT NULL,
            timestamp TEXT NOT NULL
        )",
    )?;

    // Get current max sequence number
    let mut next = 0;
    if let Some(Some::<isize>(current_max)) = trans
        .query_row(r"SELECT max(seq_no) FROM schema_versions", [], |row| {
            row.get(0)
        })
        .optional()?
    {
        // Insert migrations expected to exist into temp table, so we can query within the database
        trans.execute_batch(
            r"
            CREATE TEMPORARY TABLE temp.schema_check (
                seq_no INTEGER UNIQUE NOT NULL,
                file_name TEXT NOT NULL,
                hash BLOB NOT NULL
            )",
        )?;

        let mut query = trans
            .prepare(r"INSERT INTO temp.schema_check (seq_no,file_name,hash) VALUES (?1,?2,?3)")?;
        for (i, (seq, file_name, hash, _)) in migrations.iter().enumerate() {
            next = i + 1;
            if *seq <= current_max {
                query.execute((seq, file_name, hash))?;
            } else {
                break;
            }
        }

        // Check for missing historic migrations
        if let Some(file_name) = trans
            .query_row(
                r"
            SELECT file_name FROM temp.schema_check AS sc 
            WHERE sc.file_name NOT IN (
                SELECT file_name FROM schema_versions 
            )",
                [],
                |row| row.get(0),
            )
            .optional()?
        {
            Err(Error::MissingHistoric(file_name))?;
        }

        // Check for extra historic migrations
        if let Some(file_name) = trans
            .query_row(
                r"
            SELECT file_name FROM schema_versions AS sv 
            WHERE sv.file_name NOT IN (
                SELECT file_name FROM temp.schema_check 
            )",
                [],
                |row| row.get(0),
            )
            .optional()?
        {
            Err(Error::ExtraHistoric(file_name))?;
        }

        // Check for altered historic migrations
        if let Some(file_name) = trans
            .query_row(
                r"
            SELECT sv.file_name FROM temp.schema_check AS sc 
            JOIN schema_versions AS sv ON sc.seq_no = sv.seq_no
            WHERE sc.hash != sv.hash OR sc.file_name != sv.file_name
            ",
                [],
                |row| row.get(0),
            )
            .optional()?
        {
            Err(Error::AlteredHistoric(file_name))?;
        }

        // Drop the temporary table
        trans.execute_batch("DROP TABLE temp.schema_check")?;
    }

    // Are there newer migrations
    if next < migrations.len() {
        if upgrade {
            // Now run any new migrations
            for (seq, file_name, hash, migration) in migrations[next..].iter() {
                // Run the migration
                trans.execute_batch(migration)?;

                // Update the metadata
                trans.execute(r"INSERT INTO schema_versions (seq_no,file_name,hash,timestamp) VALUES (?1,?2,?3,datetime('now'))",(seq, file_name, hash))?;
            }
        } else {
            Err(Error::UpdateRequired)?;
        }
    }

    // Commit the transaction
    trans.commit()?;

    Ok(())
}
