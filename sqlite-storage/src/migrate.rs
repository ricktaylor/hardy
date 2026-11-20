use rusqlite::OptionalExtension;
use thiserror::Error;

#[cfg(feature = "instrument")]
use tracing::instrument;

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

// A schema migration, embedded at build time from a file in `schemas/`.
struct Migration {
    seq: isize,
    file_name: &'static str,
    // The base64 SHA-1 of `sql`, recorded by the database when it runs.
    hash: &'static str,
    sql: &'static str,
}

// Every migration, in sequence order, generated from `schemas/` at build
// time.
const MIGRATIONS: &[Migration] = &include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

#[cfg_attr(feature = "instrument", instrument(skip(conn)))]
pub fn migrate(conn: &mut rusqlite::Connection, upgrade: bool) -> Result<(), Error> {
    migrate_to(conn, MIGRATIONS, upgrade)
}

// Brings the database to the schema `migrations` ends at: checks the ones
// it has already had against the list, then applies the rest if `upgrade`.
fn migrate_to(
    conn: &mut rusqlite::Connection,
    migrations: &[Migration],
    upgrade: bool,
) -> Result<(), Error> {
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

    // Get current max sequence number. `next` indexes the first migration
    // the database has not had.
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
        for migration in migrations {
            if migration.seq > current_max {
                break;
            }
            query.execute((migration.seq, migration.file_name, migration.hash))?;
            next += 1;
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
            for Migration {
                seq,
                file_name,
                hash,
                sql,
            } in &migrations[next..]
            {
                // Run the migration
                trans.execute_batch(sql)?;

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

#[cfg(test)]
mod tests {
    use super::{Error, MIGRATIONS, migrate, migrate_to};

    fn open_memory_db() -> rusqlite::Connection {
        rusqlite::Connection::open_in_memory().unwrap()
    }

    // SQL-02: Fresh migration creates schema and records version.
    #[test]
    fn test_migration_creates_schema() {
        let mut conn = open_memory_db();
        migrate(&mut conn, true).unwrap();

        // schema_versions table should exist and have at least one entry
        let count: i64 = conn
            .query_row("SELECT count(*) FROM schema_versions", [], |row| row.get(0))
            .unwrap();
        assert!(count > 0, "schema_versions should have migration records");

        // bundles table should exist
        let table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='bundles'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 1, "bundles table should exist");
    }

    // SQL-02: Re-running migration on an already-migrated DB is a no-op.
    #[test]
    fn test_migration_reopen_is_noop() {
        let mut conn = open_memory_db();
        migrate(&mut conn, true).unwrap();

        let count_before: i64 = conn
            .query_row("SELECT count(*) FROM schema_versions", [], |row| row.get(0))
            .unwrap();

        // Run again — should succeed without adding rows
        migrate(&mut conn, true).unwrap();

        let count_after: i64 = conn
            .query_row("SELECT count(*) FROM schema_versions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, count_after);
    }

    // SQL-02: Migration with upgrade=false on a fresh DB returns UpdateRequired.
    #[test]
    fn test_migration_upgrade_required() {
        let mut conn = open_memory_db();
        let result = migrate(&mut conn, false);
        assert!(
            matches!(result, Err(Error::UpdateRequired)),
            "should return UpdateRequired when upgrade=false on fresh DB"
        );
    }

    // SQL-03: Detect missing historic migration.
    //
    // Renaming the file_name in the DB means the code expects a file that
    // doesn't match any record — triggering MissingHistoric.
    #[test]
    fn test_migration_detects_missing_historic() {
        let mut conn = open_memory_db();
        migrate(&mut conn, true).unwrap();

        // Rename the migration record so the expected file_name no longer matches
        conn.execute(
            "UPDATE schema_versions SET file_name = 'renamed.sql' WHERE seq_no = (SELECT min(seq_no) FROM schema_versions)",
            [],
        )
        .unwrap();

        let result = migrate(&mut conn, true);
        assert!(
            matches!(result, Err(Error::MissingHistoric(_))),
            "should detect missing historic migration: {result:?}"
        );
    }

    // SQL-03: Detect extra historic migration.
    #[test]
    fn test_migration_detects_extra_historic() {
        let mut conn = open_memory_db();
        migrate(&mut conn, true).unwrap();

        // Insert a fake migration record
        conn.execute(
            "INSERT INTO schema_versions (seq_no, file_name, hash, timestamp) VALUES (999, 'fake.sql', X'00', datetime('now'))",
            [],
        )
        .unwrap();

        let result = migrate(&mut conn, true);
        assert!(
            matches!(result, Err(Error::ExtraHistoric(_))),
            "should detect extra historic migration: {result:?}"
        );
    }

    // SQL-03: Detect altered historic migration (hash mismatch).
    #[test]
    fn test_migration_detects_altered_historic() {
        let mut conn = open_memory_db();
        migrate(&mut conn, true).unwrap();

        // Corrupt the hash of the first migration
        conn.execute(
            "UPDATE schema_versions SET hash = X'DEADBEEF' WHERE seq_no = (SELECT min(seq_no) FROM schema_versions)",
            [],
        )
        .unwrap();

        let result = migrate(&mut conn, true);
        assert!(
            matches!(result, Err(Error::AlteredHistoric(_))),
            "should detect altered historic migration: {result:?}"
        );
    }

    // The migrations recorded as applied, by sequence number.
    fn applied(conn: &rusqlite::Connection) -> Vec<isize> {
        conn.prepare("SELECT seq_no FROM schema_versions ORDER BY seq_no")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    // The names of the indexes on `bundles` that the schema created.
    fn bundle_indexes(conn: &rusqlite::Connection) -> Vec<String> {
        conn.prepare(
            "SELECT name FROM pragma_index_list('bundles') WHERE origin = 'c' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    // A store behind by `pending` migrations: migrated with a build that
    // ended that many short of this one.
    fn store_behind_by(pending: usize) -> rusqlite::Connection {
        let mut conn = open_memory_db();
        let earlier = &MIGRATIONS[..MIGRATIONS.len() - pending];
        migrate_to(&mut conn, earlier, true).unwrap();
        assert_eq!(applied(&conn).len(), earlier.len());
        conn
    }

    // SQL-21: A store one migration behind is upgraded: the pending
    // migration runs and is recorded.
    #[test]
    fn test_migration_applies_the_pending_migration() {
        let mut conn = store_behind_by(1);
        let before = bundle_indexes(&conn);

        migrate(&mut conn, true).unwrap();

        let all: Vec<isize> = MIGRATIONS.iter().map(|m| m.seq).collect();
        assert_eq!(applied(&conn), all);
        // The newest migration reshapes the indexes on `bundles`, so having
        // run shows in the schema as well as in the record.
        assert_ne!(bundle_indexes(&conn), before);
        assert_eq!(bundle_indexes(&conn), bundle_indexes(&store_behind_by(0)));
    }

    // SQL-21: A store several migrations behind has every one applied.
    #[test]
    fn test_migration_applies_every_pending_migration() {
        let mut conn = store_behind_by(MIGRATIONS.len() - 1);

        migrate(&mut conn, true).unwrap();

        let all: Vec<isize> = MIGRATIONS.iter().map(|m| m.seq).collect();
        assert_eq!(applied(&conn), all);
    }

    // SQL-21: Without `upgrade`, a store one migration behind is refused,
    // and left as it was.
    #[test]
    fn test_migration_refuses_a_store_behind_without_upgrade() {
        let mut conn = store_behind_by(1);
        let before = applied(&conn);

        let result = migrate(&mut conn, false);

        assert!(
            matches!(result, Err(Error::UpdateRequired)),
            "should require an upgrade: {result:?}"
        );
        assert_eq!(applied(&conn), before);
    }
}
