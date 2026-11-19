//! Write-ahead log checkpoints, off the writer thread.
//!
//! A checkpoint copies committed pages from the WAL into the database file
//! and syncs it. Left to SQLite, it runs inside whichever commit crosses a
//! size threshold, and every write queued behind that commit waits for it.
//! Here the writer only looks at how much of the WAL is uncopied. When
//! enough is, it wakes the checkpoint thread, which copies it on a
//! connection of its own, in passive passes that never wait on the writer
//! or on a reader, and syncs the database file.
//!
//! A pass cannot empty a WAL that is still being written, and the WAL
//! restarts from its beginning only once all of it has been copied. So once
//! the WAL passes a size bound, the writer copies the remainder itself,
//! between two transactions: the few frames written since the last pass,
//! and a sync with little left to flush.

use std::{
    path::Path,
    sync::mpsc::{Receiver, Sender, TryRecvError, channel},
};

use rusqlite::{Connection, ErrorCode, MAIN_DB, OpenFlags, Result, wal::CheckpointMode};
use trace_err::*;
use tracing::debug;

use super::thread::Worker;

// When checkpoints run, in WAL frames. A frame is one page: 4 KiB at
// SQLite's default page size.
#[derive(Clone, Copy)]
struct Thresholds {
    // The uncopied frames at which a cycle is requested.
    cycle: u32,
    // The WAL size above which the writer finishes a cycle itself, so that
    // the WAL restarts. Below it the WAL is left to grow, and the writer
    // never pauses for a checkpoint.
    restart: u32,
    // The WAL size at which the writer waits for the cycle in flight. Up
    // to here a writer that outruns the checkpoint thread is left to; past
    // it the write rate is held to the checkpoint rate, which is what
    // bounds the WAL.
    limit: u32,
}

// A cycle per 4 MiB written, a WAL that restarts once past 32 MiB, and one
// that never much exceeds 64 MiB.
const THRESHOLDS: Thresholds = Thresholds {
    cycle: 1000,
    restart: 8000,
    limit: 16000,
};

// Above the restart bound, a cycle keeps making passes until the writer is
// this few frames ahead: what is left is what the writer copies itself.
const CAUGHT_UP_FRAMES: u32 = 64;

// The most passes one cycle makes while chasing the writer.
const MAX_PASSES: usize = 8;

// The checkpoint thread stops only when its `Checkpointer` drops, and a
// panic on it aborts the process, so the writer never finds it gone.
const STOPPED: &str = "The metadata checkpoint thread has stopped";

// Where the checkpoint thread is with the cycle last requested of it.
enum Cycle {
    Idle,
    Running,
    // Complete, and not yet acted on by the writer.
    Completed,
}

// The writer's handle on the checkpoint thread: between transactions it
// decides whether a checkpoint is due, and whose it is.
pub struct Checkpointer {
    cycle: Cycle,
    thresholds: Thresholds,
    // Declared before `_thread`: closing this channel stops the thread,
    // which is then joined.
    wake: Sender<()>,
    cycled: Receiver<()>,
    _thread: Worker,
}

impl Checkpointer {
    // Takes over checkpointing of the database at `path`, which `writer`
    // has open: turns off SQLite's own checkpoints at its commits, and
    // starts the checkpoint thread.
    pub fn start(writer: &Connection, path: &Path) -> Result<Self> {
        Self::start_with(writer, path, THRESHOLDS)
    }

    fn start_with(writer: &Connection, path: &Path, thresholds: Thresholds) -> Result<Self> {
        writer.execute_batch("PRAGMA wal_autocheckpoint = 0;")?;

        let conn = open(path)?;
        let (wake, woken) = channel();
        let (done, cycled) = channel();
        let thread = Worker::spawn("sqlite-checkpoint", move || {
            run(conn, woken, done, thresholds.restart);
        })
        .trace_expect("Failed to spawn the metadata checkpoint thread");

        Ok(Self {
            cycle: Cycle::Idle,
            thresholds,
            wake,
            cycled,
            _thread: thread,
        })
    }

    // Called by the writer between transactions, with its own connection.
    pub fn after_commit(&mut self, writer: &Connection) {
        let Some((frames, copied)) = settled(wal(writer), "Failed to read the metadata WAL's size")
        else {
            return;
        };

        if matches!(self.cycle, Cycle::Running) {
            self.cycle = self.poll_cycle(frames >= self.thresholds.limit);
        }
        match self.cycle {
            Cycle::Running => {}
            Cycle::Completed => {
                self.cycle = Cycle::Idle;
                if frames >= self.thresholds.restart {
                    finish(writer);
                }
            }
            Cycle::Idle => {
                if frames - copied >= self.thresholds.cycle {
                    self.wake.send(()).trace_expect(STOPPED);
                    self.cycle = Cycle::Running;
                }
            }
        }
    }

    // Where the running cycle has got to, waiting for it to complete if
    // `wait`.
    fn poll_cycle(&self, wait: bool) -> Cycle {
        let cycled = if wait {
            self.cycled.recv().map_err(Into::into)
        } else {
            self.cycled.try_recv()
        };
        match cycled {
            Ok(()) => Cycle::Completed,
            Err(TryRecvError::Empty) => Cycle::Running,
            Err(e @ TryRecvError::Disconnected) => Err(e).trace_expect(STOPPED),
        }
    }
}

// Copies what the background passes left, on the writer's own connection
// and between its transactions. With the whole WAL copied, the writer's
// next transaction restarts it from its beginning.
fn finish(writer: &Connection) {
    // Deferred, the WAL keeps growing until a later finish succeeds.
    _ = settled(pass(writer), "Metadata checkpoint failed on the writer");
}

// What a checkpoint step returned, or `None` if it only lost a race for one
// of the WAL's locks: the step is then left for a later batch.
//
// Any other failure is the database failing to write. No caller is waiting
// on a checkpoint to be handed the error, and a sync that failed cannot be
// retried safely, because the kernel may have dropped the pages it could
// not write. So the failure is fatal here: the panic aborts the process.
fn settled<T>(step: Result<T>, failure: &str) -> Option<T> {
    match step {
        Ok(value) => Some(value),
        Err(e) if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) => {
            debug!("Metadata checkpoint deferred: {e}");
            None
        }
        Err(e) => Some(Err(e).trace_expect(failure)),
    }
}

// Opens the checkpoint thread's own connection to the writer's database.
fn open(path: &Path) -> Result<Connection> {
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

// The checkpoint thread: one cycle per request, until the writer has gone.
fn run(conn: Connection, woken: Receiver<()>, done: Sender<()>, restart: u32) {
    while woken.recv().is_ok() {
        // A cycle deferred by a lock race, such as reading the WAL's header
        // during a commit, is asked for again after the writer's next
        // batch, while enough of the WAL is uncopied.
        _ = settled(cycle(&conn, restart), "Metadata checkpoint failed");
        if done.send(()).is_err() {
            break;
        }
    }
}

// Copies the WAL into the database file and syncs it. A pass copies the WAL
// as it stood when the pass began. Above the restart bound, where the
// writer is about to copy the remainder itself, the cycle repeats for as
// long as that shrinks the remainder: a writer that outruns the passes is
// not chased.
fn cycle(conn: &Connection, restart: u32) -> Result<()> {
    let mut remainder = u32::MAX;
    for _ in 0..MAX_PASSES {
        pass(conn)?;
        // A pass syncs the database file only if it copied the whole WAL,
        // which a pass racing the writer does not. Syncing here is what
        // leaves the writer's finishing pass little to flush.
        os::sync(conn)?;
        let (frames, copied) = wal(conn)?;
        let uncopied = frames - copied;
        if frames < restart || uncopied <= CAUGHT_UP_FRAMES || uncopied >= remainder {
            break;
        }
        remainder = uncopied;
    }
    Ok(())
}

// One passive pass: copies what it can without waiting for the writer or
// for any reader.
fn pass(conn: &Connection) -> Result<()> {
    conn.wal_checkpoint_v2(Some(MAIN_DB), CheckpointMode::PASSIVE)?;
    Ok(())
}

// The frames in the WAL, and how many of them have been copied into the
// database file. Reads the WAL's own counters, and copies nothing.
fn wal(conn: &Connection) -> Result<(u32, u32)> {
    let (frames, copied) = conn.wal_checkpoint_v2(Some(MAIN_DB), CheckpointMode::NOOP)?;
    // Both are -1 unless the database is in WAL mode.
    let frames = frames.try_into().unwrap_or(0);
    Ok((frames, copied.try_into().unwrap_or(0).min(frames)))
}

// Syncing a database file through the file object SQLite holds open for it.
//
// A second handle on the file, opened here, would not do: closing any
// descriptor of a file releases every POSIX lock the process holds on it,
// SQLite's own among them.
mod os {
    use std::{ffi::c_void, ptr::null_mut};

    use rusqlite::{
        Connection, Error, MAIN_DB, Result,
        ffi::{
            self, SQLITE_FCNTL_FILE_POINTER, SQLITE_MISUSE, SQLITE_OK, SQLITE_SYNC_NORMAL,
            sqlite3_file, sqlite3_file_control,
        },
    };

    fn check(code: i32) -> Result<()> {
        if code == SQLITE_OK {
            Ok(())
        } else {
            Err(Error::SqliteFailure(ffi::Error::new(code), None))
        }
    }

    // Syncs `conn`'s main database file, as SQLite does at the end of a
    // checkpoint that copied the whole WAL.
    pub fn sync(conn: &Connection) -> Result<()> {
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
            .ok_or_else(|| Error::SqliteFailure(ffi::Error::new(SQLITE_MISUSE), None))?;
        // SAFETY: `sync` is that file object's own sync method, called on
        // it as SQLite calls it.
        check(unsafe { sync(file, SQLITE_SYNC_NORMAL) })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::{
        Error as SqliteError,
        ffi::{self, SQLITE_BUSY},
    };
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::thread::tests::assert_aborts;

    // Thresholds no test's writes reach, for a test to lower the ones it
    // is about.
    const NEVER: Thresholds = Thresholds {
        cycle: u32::MAX,
        restart: u32::MAX,
        limit: u32::MAX,
    };

    impl Checkpointer {
        // Blocks until the cycle in flight, if any, has completed.
        fn settle(&mut self) {
            if matches!(self.cycle, Cycle::Running) {
                self.cycle = self.poll_cycle(true);
            }
        }
    }

    // A WAL-mode database with one table, and the writer's connection to it.
    fn database() -> (TempDir, PathBuf, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE rows (id INTEGER PRIMARY KEY, filler BLOB);",
        )
        .unwrap();
        (dir, path, conn)
    }

    // Commits one transaction that adds at least `frames` frames to the WAL.
    fn write(conn: &Connection, frames: u32) {
        conn.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
            INSERT INTO rows (filler) SELECT zeroblob(4000) FROM n",
            [frames],
        )
        .unwrap();
    }

    // A reader holding the snapshot it took when called: until it drops, no
    // checkpoint copies past that snapshot.
    fn reader(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("BEGIN; SELECT count(*) FROM rows;")
            .unwrap();
        conn
    }

    // The checkpoint thread copies the WAL into the database file, and the
    // writer does no checkpoint work of its own.
    #[test]
    fn a_cycle_copies_the_wal_in_the_background() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds { cycle: 8, ..NEVER };
        let mut checkpointer = Checkpointer::start_with(&conn, &path, thresholds).unwrap();

        write(&conn, 16);
        let (frames, copied) = wal(&conn).unwrap();
        assert!(frames >= 16);
        assert_eq!(copied, 0);

        checkpointer.after_commit(&conn);
        checkpointer.settle();
        assert_eq!(wal(&conn).unwrap(), (frames, frames));
    }

    // Short of the cycle threshold nothing is copied: SQLite's own
    // checkpoint, due at 1000 frames, is off.
    #[test]
    fn the_wal_is_left_alone_below_the_cycle_threshold() {
        let (_dir, path, conn) = database();
        let mut checkpointer = Checkpointer::start_with(&conn, &path, NEVER).unwrap();

        write(&conn, 1200);
        checkpointer.after_commit(&conn);
        checkpointer.settle();
        let (frames, copied) = wal(&conn).unwrap();
        assert!(frames >= 1200);
        assert_eq!(copied, 0);
    }

    // Past the restart bound the writer copies what the background cycle
    // left, and its next transaction starts the WAL again from its
    // beginning.
    #[test]
    fn the_writer_finishes_a_cycle_past_the_restart_bound() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds {
            cycle: 8,
            restart: 24,
            ..NEVER
        };
        let mut checkpointer = Checkpointer::start_with(&conn, &path, thresholds).unwrap();

        write(&conn, 16);
        let snapshot = reader(&path);
        write(&conn, 16);
        let (frames, _) = wal(&conn).unwrap();

        // The cycle stops at the reader's snapshot.
        checkpointer.after_commit(&conn);
        checkpointer.settle();
        let (_, copied) = wal(&conn).unwrap();
        assert!(copied < frames);
        drop(snapshot);

        checkpointer.after_commit(&conn);
        assert_eq!(wal(&conn).unwrap(), (frames, frames));

        write(&conn, 1);
        let (restarted, _) = wal(&conn).unwrap();
        assert!(restarted < frames);
    }

    // Short of the restart bound the writer leaves the remainder to the
    // next cycle.
    #[test]
    fn the_writer_leaves_the_remainder_below_the_restart_bound() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds { cycle: 64, ..NEVER };
        let mut checkpointer = Checkpointer::start_with(&conn, &path, thresholds).unwrap();

        write(&conn, 64);
        let snapshot = reader(&path);
        write(&conn, 16);
        let (frames, _) = wal(&conn).unwrap();

        checkpointer.after_commit(&conn);
        checkpointer.settle();
        let (_, copied) = wal(&conn).unwrap();
        assert!(copied < frames);
        drop(snapshot);

        checkpointer.after_commit(&conn);
        assert_eq!(wal(&conn).unwrap(), (frames, copied));
    }

    // At the limit the writer waits for the cycle in flight and finishes
    // it, where short of the limit it would have carried on writing.
    #[test]
    fn the_writer_waits_for_the_cycle_at_the_limit() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds {
            cycle: 8,
            restart: 8,
            limit: 8,
        };
        let mut checkpointer = Checkpointer::start_with(&conn, &path, thresholds).unwrap();

        write(&conn, 16);
        let (frames, _) = wal(&conn).unwrap();
        // Requests the cycle; the next call finds it running or complete,
        // and in either case returns with the whole WAL copied.
        checkpointer.after_commit(&conn);
        checkpointer.after_commit(&conn);
        assert_eq!(wal(&conn).unwrap(), (frames, frames));
        assert!(matches!(checkpointer.cycle, Cycle::Idle));
    }

    #[test]
    fn the_database_file_syncs_through_its_own_handle() {
        let (_dir, _path, conn) = database();
        os::sync(&conn).unwrap();
    }

    // A step that only lost a race for one of the WAL's locks is deferred.
    #[test]
    fn a_busy_checkpoint_step_is_deferred() {
        let busy = SqliteError::SqliteFailure(ffi::Error::new(SQLITE_BUSY), None);
        assert_eq!(settled::<()>(Err(busy), "a checkpoint step"), None);
    }

    // A checkpoint that fails for any other reason takes the process down.
    // The checkpoint thread here is given an in-memory database, which has
    // no file to sync, so its first cycle fails.
    #[test]
    fn a_failed_checkpoint_aborts_the_process() {
        assert_aborts(
            "checkpoint::tests::a_failed_checkpoint_aborts_the_process",
            || {
                let (_dir, _path, conn) = database();
                let thresholds = Thresholds { cycle: 8, ..NEVER };
                let mut checkpointer =
                    Checkpointer::start_with(&conn, Path::new(":memory:"), thresholds).unwrap();

                write(&conn, 16);
                checkpointer.after_commit(&conn);
                // Waits on a cycle that never completes: the abort ends
                // the process first.
                checkpointer.settle();
            },
        );
    }
}
