//! The write-ahead log's counters, and the checkpoint passes and cycles
//! that copy it into the database file and sync it.

use std::{ffi::c_void, path::Path, ptr::null_mut};

use rusqlite::{
    Connection, Error as SqliteError, MAIN_DB, OpenFlags, Result as SqliteResult,
    ffi::{
        Error as SqliteCode, SQLITE_FCNTL_FILE_POINTER, SQLITE_MISUSE, SQLITE_OK,
        SQLITE_SYNC_NORMAL, sqlite3_file, sqlite3_file_control,
    },
    wal::CheckpointMode,
};

use super::retry::RETRY;

// Above the restart bound, a cycle keeps making passes until the writer is
// this few frames ahead: what is left is what the writer copies itself.
const SLACK_FRAMES: u32 = 64;

// The most passes one cycle makes while chasing the writer.
const MAX_PASSES: usize = 8;

// Opens the checkpoint thread's own connection to the writer's database.
pub fn open(path: &Path) -> SqliteResult<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // A connection opens its database's WAL only when it first reads the
    // database, and until then its checkpoints succeed without copying
    // anything. Reading the schema version is that first read.
    conn.query_row("PRAGMA schema_version", [], |_| Ok(()))?;
    Ok(conn)
}

// Copies the WAL into the database file and syncs it. A pass copies the WAL
// as it stood when the pass began. Above the restart bound, where the
// writer is about to copy the remainder itself, the cycle repeats for as
// long as that shrinks the remainder: a writer that outruns the passes is
// not chased.
fn cycle(conn: &Connection, restart: u32) -> SqliteResult<()> {
    let mut checkpointed = wal_size(conn)?.checkpointed;
    let mut remainder = u32::MAX;
    for _ in 0..MAX_PASSES {
        let wal = pass(conn)?;
        // A pass syncs the database file only if it copied the whole WAL,
        // which a pass racing the writer does not. Syncing here is what
        // leaves the writer's finishing pass little to flush. A pass that
        // copied nothing, held back by a reader's snapshot, left nothing to
        // flush.
        if wal.checkpointed != checkpointed {
            sync(conn)?;
        }
        if wal.frames < restart || wal.backlog() <= SLACK_FRAMES || wal.backlog() >= remainder {
            break;
        }
        checkpointed = wal.checkpointed;
        remainder = wal.backlog();
    }
    Ok(())
}

// Runs a cycle, and runs it again while it fails in a way that may clear,
// until the retry policy gives up.
pub fn retry_cycle(conn: &Connection, restart: u32) -> SqliteResult<()> {
    RETRY.run("Metadata checkpoint", || cycle(conn, restart))
}

// One passive pass: copies what it can without waiting for the writer or
// for any reader, and returns the WAL's size as the pass left it.
pub fn pass(conn: &Connection) -> SqliteResult<WalSize> {
    checkpoint(conn, CheckpointMode::PASSIVE)
}

// The WAL's size in frames, and how much of it a checkpoint has copied into
// the database file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalSize {
    pub frames: u32,
    // At most `frames`.
    pub checkpointed: u32,
}

impl WalSize {
    // The frames not yet checkpointed into the database file.
    pub fn backlog(self) -> u32 {
        self.frames - self.checkpointed
    }
}

// Reads the WAL's own counters, and copies nothing.
pub fn wal_size(conn: &Connection) -> SqliteResult<WalSize> {
    checkpoint(conn, CheckpointMode::NOOP)
}

// Runs a checkpoint in `mode`, returning the WAL's size after it.
fn checkpoint(conn: &Connection, mode: CheckpointMode) -> SqliteResult<WalSize> {
    let (frames, checkpointed) = conn.wal_checkpoint_v2(Some(MAIN_DB), mode)?;
    // Both are -1 unless the database is in WAL mode.
    let frames = frames.try_into().unwrap_or(0);
    Ok(WalSize {
        frames,
        checkpointed: checkpointed.try_into().unwrap_or(0).min(frames),
    })
}

// Syncs `conn`'s main database file, as SQLite does at the end of a
// checkpoint that copied the whole WAL, through the file object SQLite
// holds open for it.
//
// A second handle on the file, opened here, would not do: closing any
// descriptor of a file releases every POSIX lock the process holds on it,
// SQLite's own among them.
//
// rusqlite's safe `Connection::file_control` takes a closed enum with no
// variant for `SQLITE_FCNTL_FILE_POINTER`, so the file object is reached
// through the C API.
pub fn sync(conn: &Connection) -> SqliteResult<()> {
    let mut file: *mut sqlite3_file = null_mut();
    // SAFETY: `conn` is open for the whole call and, not being `Sync`,
    // is used by this thread alone. FILE_POINTER stores the main
    // database's file object through the out-pointer, which is valid
    // for the write.
    check(unsafe {
        sqlite3_file_control(
            conn.handle(),
            MAIN_DB.as_ptr(),
            SQLITE_FCNTL_FILE_POINTER,
            (&raw mut file).cast::<c_void>(),
        )
    })?;
    // SAFETY: on success `file` is the file object the connection keeps
    // open while it lives, and an open file object's method table is
    // set.
    let sync = unsafe { file.as_ref().and_then(|file| file.pMethods.as_ref()) }
        .and_then(|methods| methods.xSync)
        .ok_or_else(|| SqliteError::SqliteFailure(SqliteCode::new(SQLITE_MISUSE), None))?;
    // SAFETY: `sync` is that file object's own sync method, called on
    // it as SQLite calls it.
    check(unsafe { sync(file, SQLITE_SYNC_NORMAL) })
}

// Repeats rusqlite's own result-code check, which is not exported.
fn check(code: i32) -> SqliteResult<()> {
    if code == SQLITE_OK {
        Ok(())
    } else {
        Err(SqliteError::SqliteFailure(SqliteCode::new(code), None))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::retry::is_recoverable;

    // The checkpoint thread runs a cycle again while it fails in a way that
    // may clear, and returns the failure once the retry policy gives up. A
    // read-only connection fails every pass with an I/O error.
    #[test]
    fn a_failing_cycle_is_retried_until_the_policy_gives_up() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA wal_autocheckpoint = 0;
            CREATE TABLE rows (id INTEGER PRIMARY KEY);
            INSERT INTO rows DEFAULT VALUES;",
        )
        .unwrap();
        let read_only = open(Path::new(&format!("file:{}?mode=ro", path.display()))).unwrap();

        let error = retry_cycle(&read_only, u32::MAX).unwrap_err();
        assert!(is_recoverable(&error), "{error}");
        assert_eq!(wal_size(&conn).unwrap().checkpointed, 0);
    }

    #[test]
    fn the_database_file_syncs_through_its_own_handle() {
        let dir = tempdir().unwrap();
        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE rows (id INTEGER);")
            .unwrap();
        sync(&conn).unwrap();
    }
}
