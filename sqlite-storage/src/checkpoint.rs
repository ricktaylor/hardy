//! Write-ahead log checkpoints, off the writer thread.
//!
//! A checkpoint copies committed pages from the WAL into the database file
//! and syncs it. Left to SQLite, it runs inside whichever commit crosses a
//! size threshold, and every write queued behind that commit waits for it.
//! Here the writer only looks at the WAL's backlog, the frames not yet
//! checkpointed. When the backlog is large enough, the writer wakes the
//! checkpoint thread, which copies it on a connection of its own, in
//! passive passes that never wait on the writer or on a reader, and syncs
//! the database file.
//!
//! A pass cannot empty a WAL that is still being written, and the WAL
//! restarts from its beginning only once all of it has been copied. So once
//! the WAL passes a size bound, the writer copies the remainder itself,
//! between two transactions: the few frames written since the last pass,
//! and a sync with little left to flush.

use std::{
    path::Path,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel},
    time::Duration,
};

use rusqlite::{
    Connection, ErrorCode, MAIN_DB, OpenFlags, Result as SqliteResult, wal::CheckpointMode,
};
use trace_err::*;
use tracing::{debug, warn};

use super::thread::JoinOnDrop;

// When checkpoints run, in WAL frames. A frame is one page: 4 KiB at
// SQLite's default page size.
#[derive(Clone, Copy)]
struct Thresholds {
    // The backlog at which a cycle is requested.
    start: u32,
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
    start: 1000,
    restart: 8000,
    limit: 16000,
};

// Above the restart bound, a cycle keeps making passes until the writer is
// this few frames ahead: what is left is what the writer copies itself.
const SLACK_FRAMES: u32 = 64;

// The most passes one cycle makes while chasing the writer.
const MAX_PASSES: usize = 8;

// The checkpoint thread stops only when its `Checkpointer` drops, and a
// panic on it aborts the process, so the writer never finds it gone.
const STOPPED_MESSAGE: &str = "The metadata checkpoint thread has stopped";

// How long the writer waits for a cycle at the limit: as long as the cycle
// takes. `recv_timeout` waits without a deadline when this one is past the
// clock's range.
#[cfg(not(test))]
const CYCLE_WAIT: Duration = Duration::MAX;

// A checkpoint thread that never completes its cycle fails the test instead
// of hanging the suite: the timeout only bounds a regression.
#[cfg(test)]
const CYCLE_WAIT: Duration = Duration::from_secs(30);

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
    // The WAL is at or past the limit and has not restarted since: the
    // bound is not holding, and the operator has been told once.
    over_limit: bool,
    // Declared before `_thread`: closing this channel stops the thread,
    // which is then joined.
    wake: Sender<()>,
    cycled: Receiver<()>,
    _thread: JoinOnDrop,
    // How many times the writer has set out to wait for a cycle: counted
    // as it starts waiting, whether or not the cycle had already finished.
    #[cfg(test)]
    waits: u32,
}

impl Checkpointer {
    // Takes over checkpointing of the database at `path`, which `writer`
    // has open: turns off SQLite's own checkpoints at its commits, and
    // starts the checkpoint thread.
    pub fn start(writer: &Connection, path: &Path) -> SqliteResult<Self> {
        Self::start_with_thresholds(writer, path, THRESHOLDS)
    }

    fn start_with_thresholds(
        writer: &Connection,
        path: &Path,
        thresholds: Thresholds,
    ) -> SqliteResult<Self> {
        writer.execute_batch("PRAGMA wal_autocheckpoint = 0;")?;

        let conn = open(path)?;
        let (wake, woken) = channel();
        let (done, cycled) = channel();
        let thread = JoinOnDrop::spawn("sqlite-checkpoint", move || {
            run(conn, woken, done, thresholds.restart);
        })
        .trace_expect("Failed to spawn the metadata checkpoint thread");

        Ok(Self {
            cycle: Cycle::Idle,
            thresholds,
            over_limit: false,
            wake,
            cycled,
            _thread: thread,
            #[cfg(test)]
            waits: 0,
        })
    }

    // Called by the writer between transactions, with its own connection.
    pub fn after_commit(&mut self, writer: &Connection) {
        let Some(wal) = defer_if_busy(wal_size(writer), "Failed to read the metadata WAL's size")
        else {
            return;
        };

        self.note_limit(wal.frames);
        if matches!(self.cycle, Cycle::Running) {
            self.cycle = if wal.frames >= self.thresholds.limit {
                self.wait_for_cycle()
            } else {
                self.try_cycle()
            };
        }
        match self.cycle {
            Cycle::Running => {}
            Cycle::Completed => {
                self.cycle = Cycle::Idle;
                if wal.frames >= self.thresholds.restart {
                    try_finish(writer);
                }
            }
            Cycle::Idle => {
                if wal.backlog() >= self.thresholds.start {
                    self.wake.send(()).trace_expect(STOPPED_MESSAGE);
                    self.cycle = Cycle::Running;
                }
            }
        }
    }

    // Warns once when the WAL reaches the limit, which the writer's wait
    // for each cycle should prevent: it means checkpoints are not copying
    // the WAL, because another process keeps winning the checkpoint lock or
    // a reader is holding an old snapshot open. Each one is otherwise logged
    // only at debug level, as a lock race.
    fn note_limit(&mut self, frames: u32) {
        if frames < self.thresholds.limit {
            self.over_limit = false;
        } else if !self.over_limit {
            self.over_limit = true;
            warn!(
                "Metadata WAL has reached {frames} pages, past its limit of {}: checkpoints are not keeping up",
                self.thresholds.limit
            );
        }
    }

    // Where the running cycle has got to, without waiting for it.
    fn try_cycle(&self) -> Cycle {
        cycle_state(self.cycled.try_recv())
    }

    // Waits for the running cycle to complete.
    fn wait_for_cycle(&mut self) -> Cycle {
        #[cfg(test)]
        {
            self.waits += 1;
        }
        let reported = self.cycled.recv_timeout(CYCLE_WAIT).map_err(|e| match e {
            RecvTimeoutError::Timeout => panic!("the checkpoint thread never completed its cycle"),
            RecvTimeoutError::Disconnected => TryRecvError::Disconnected,
        });
        cycle_state(reported)
    }

    // Called by the writer once it has committed its last batch: stops the
    // checkpoint thread, then copies what is left of the WAL on the
    // writer's own connection. A checkpoint is otherwise left to the last
    // connection to close, which is a pooled reader, on whichever thread
    // drops the store.
    pub fn stop(self, writer: &Connection) {
        drop(self);
        try_finish(writer);
    }
}

// The cycle's state, given what the checkpoint thread has reported.
fn cycle_state(reported: Result<(), TryRecvError>) -> Cycle {
    match reported {
        Ok(()) => Cycle::Completed,
        Err(TryRecvError::Empty) => Cycle::Running,
        Err(e @ TryRecvError::Disconnected) => Err(e).trace_expect(STOPPED_MESSAGE),
    }
}

// Copies what the background passes left, on the writer's own connection
// and between its transactions. With the whole WAL copied, the writer's
// next transaction restarts it from its beginning.
fn try_finish(writer: &Connection) {
    // Deferred, the WAL keeps growing until a later finish succeeds.
    _ = defer_if_busy(pass(writer), "Metadata checkpoint failed on the writer");
}

// What a checkpoint step returned, or `None` if it only lost a race for one
// of the WAL's locks: the step is then left for a later batch.
//
// Any other failure is the database failing to write. No caller is waiting
// on a checkpoint to be handed the error, and a sync that failed cannot be
// retried safely, because the kernel may have dropped the pages it could
// not write. So the failure is fatal here: the panic aborts the process.
fn defer_if_busy<T>(step: SqliteResult<T>, failure: &str) -> Option<T> {
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
fn open(path: &Path) -> SqliteResult<Connection> {
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
        // batch, while the backlog is large enough.
        _ = defer_if_busy(cycle(&conn, restart), "Metadata checkpoint failed");
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
            os::sync(conn)?;
        }
        if wal.frames < restart || wal.backlog() <= SLACK_FRAMES || wal.backlog() >= remainder {
            break;
        }
        checkpointed = wal.checkpointed;
        remainder = wal.backlog();
    }
    Ok(())
}

// One passive pass: copies what it can without waiting for the writer or
// for any reader, and returns the WAL's size as the pass left it.
fn pass(conn: &Connection) -> SqliteResult<WalSize> {
    checkpoint(conn, CheckpointMode::PASSIVE)
}

// The WAL's size in frames, and how much of it a checkpoint has copied into
// the database file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WalSize {
    frames: u32,
    // At most `frames`.
    checkpointed: u32,
}

impl WalSize {
    // The frames not yet checkpointed into the database file.
    fn backlog(self) -> u32 {
        self.frames - self.checkpointed
    }
}

// Reads the WAL's own counters, and copies nothing.
fn wal_size(conn: &Connection) -> SqliteResult<WalSize> {
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

// Syncing a database file through the file object SQLite holds open for it.
//
// A second handle on the file, opened here, would not do: closing any
// descriptor of a file releases every POSIX lock the process holds on it,
// SQLite's own among them.
//
// rusqlite's safe `Connection::file_control` takes a closed enum with no
// variant for `SQLITE_FCNTL_FILE_POINTER`, so the file object is reached
// through the C API. `check` repeats rusqlite's own, which is not exported.
mod os {
    use std::{ffi::c_void, ptr::null_mut};

    use rusqlite::{
        Connection, Error as SqliteError, MAIN_DB, Result as SqliteResult,
        ffi::{
            Error as SqliteCode, SQLITE_FCNTL_FILE_POINTER, SQLITE_MISUSE, SQLITE_OK,
            SQLITE_SYNC_NORMAL, sqlite3_file, sqlite3_file_control,
        },
    };

    fn check(code: i32) -> SqliteResult<()> {
        if code == SQLITE_OK {
            Ok(())
        } else {
            Err(SqliteError::SqliteFailure(SqliteCode::new(code), None))
        }
    }

    // Syncs `conn`'s main database file, as SQLite does at the end of a
    // checkpoint that copied the whole WAL.
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
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::{
        Error as SqliteError,
        ffi::{Error as SqliteCode, SQLITE_BUSY},
    };
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::thread::tests::{assert_aborts, test_path, wait_for_abort};

    // Thresholds no test's writes reach, for a test to lower the ones it
    // is about.
    const NEVER: Thresholds = Thresholds {
        start: u32::MAX,
        restart: u32::MAX,
        limit: u32::MAX,
    };

    impl Checkpointer {
        // Blocks until the cycle in flight, if any, has completed. A
        // checkpoint thread that never completes one would hang the suite,
        // so the wait fails instead: the timeout only bounds a regression.
        fn settle(&mut self) {
            if matches!(self.cycle, Cycle::Running) {
                self.cycled
                    .recv_timeout(CYCLE_WAIT)
                    .expect("the checkpoint thread never completed its cycle");
                self.cycle = Cycle::Completed;
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
        let thresholds = Thresholds { start: 8, ..NEVER };
        let mut checkpointer =
            Checkpointer::start_with_thresholds(&conn, &path, thresholds).unwrap();

        write(&conn, 16);
        let wal = wal_size(&conn).unwrap();
        let (frames, checkpointed) = (wal.frames, wal.checkpointed);
        assert!(frames >= 16);
        assert_eq!(checkpointed, 0);

        checkpointer.after_commit(&conn);
        checkpointer.settle();
        assert_eq!(
            wal_size(&conn).unwrap(),
            WalSize {
                frames,
                checkpointed: frames
            }
        );
    }

    // Short of the cycle threshold nothing is copied: SQLite's own
    // checkpoint, due at 1000 frames, is off.
    #[test]
    fn the_wal_is_left_alone_below_the_cycle_threshold() {
        let (_dir, path, conn) = database();
        let mut checkpointer = Checkpointer::start_with_thresholds(&conn, &path, NEVER).unwrap();

        write(&conn, 1200);
        checkpointer.after_commit(&conn);
        checkpointer.settle();
        let wal = wal_size(&conn).unwrap();
        let (frames, checkpointed) = (wal.frames, wal.checkpointed);
        assert!(frames >= 1200);
        assert_eq!(checkpointed, 0);
    }

    // Past the restart bound the writer copies what the background cycle
    // left, and its next transaction starts the WAL again from its
    // beginning.
    #[test]
    fn the_writer_finishes_a_cycle_past_the_restart_bound() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds {
            start: 8,
            restart: 24,
            ..NEVER
        };
        let mut checkpointer =
            Checkpointer::start_with_thresholds(&conn, &path, thresholds).unwrap();

        write(&conn, 16);
        let snapshot = reader(&path);
        write(&conn, 16);
        let frames = wal_size(&conn).unwrap().frames;

        // The cycle stops at the reader's snapshot.
        checkpointer.after_commit(&conn);
        checkpointer.settle();
        let checkpointed = wal_size(&conn).unwrap().checkpointed;
        assert!(checkpointed < frames);
        drop(snapshot);

        checkpointer.after_commit(&conn);
        assert_eq!(
            wal_size(&conn).unwrap(),
            WalSize {
                frames,
                checkpointed: frames
            }
        );

        write(&conn, 1);
        let restarted = wal_size(&conn).unwrap().frames;
        assert!(restarted < frames);
    }

    // Short of the restart bound the writer leaves the remainder to the
    // next cycle.
    #[test]
    fn the_writer_leaves_the_remainder_below_the_restart_bound() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds { start: 64, ..NEVER };
        let mut checkpointer =
            Checkpointer::start_with_thresholds(&conn, &path, thresholds).unwrap();

        write(&conn, 64);
        let snapshot = reader(&path);
        write(&conn, 16);
        let frames = wal_size(&conn).unwrap().frames;

        checkpointer.after_commit(&conn);
        checkpointer.settle();
        let checkpointed = wal_size(&conn).unwrap().checkpointed;
        assert!(checkpointed < frames);
        drop(snapshot);

        checkpointer.after_commit(&conn);
        assert_eq!(
            wal_size(&conn).unwrap(),
            WalSize {
                frames,
                checkpointed
            }
        );
    }

    // At the limit the writer waits for the cycle in flight, and returns
    // with it complete and finished.
    #[test]
    fn the_writer_waits_for_the_cycle_at_the_limit() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds {
            start: 8,
            restart: 8,
            limit: 8,
        };
        let mut checkpointer =
            Checkpointer::start_with_thresholds(&conn, &path, thresholds).unwrap();

        write(&conn, 16);
        let frames = wal_size(&conn).unwrap().frames;
        // Requests the cycle.
        checkpointer.after_commit(&conn);
        assert_eq!(checkpointer.waits, 0);

        // Finds the cycle requested, and waits for it.
        checkpointer.after_commit(&conn);
        assert_eq!(checkpointer.waits, 1);
        assert!(matches!(checkpointer.cycle, Cycle::Idle));
        assert_eq!(
            wal_size(&conn).unwrap(),
            WalSize {
                frames,
                checkpointed: frames
            }
        );
    }

    // Short of the limit the writer never waits for a cycle: it carries on
    // with its next batch while the cycle runs.
    #[test]
    fn the_writer_carries_on_below_the_limit() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds { start: 8, ..NEVER };
        let mut checkpointer =
            Checkpointer::start_with_thresholds(&conn, &path, thresholds).unwrap();

        write(&conn, 16);
        checkpointer.after_commit(&conn);
        checkpointer.after_commit(&conn);
        assert_eq!(checkpointer.waits, 0);
    }

    // Reaching the limit is noted once, and cleared once the WAL is back
    // under it, so a WAL that stays past the limit warns once, not on every
    // batch.
    #[test]
    fn the_limit_is_noted_once_until_the_wal_shrinks() {
        let (_dir, path, conn) = database();
        let thresholds = Thresholds {
            limit: 100,
            ..NEVER
        };
        let mut checkpointer =
            Checkpointer::start_with_thresholds(&conn, &path, thresholds).unwrap();

        checkpointer.note_limit(99);
        assert!(!checkpointer.over_limit);
        checkpointer.note_limit(100);
        assert!(checkpointer.over_limit);
        checkpointer.note_limit(150);
        assert!(checkpointer.over_limit);
        checkpointer.note_limit(10);
        assert!(!checkpointer.over_limit);
    }

    #[test]
    fn the_database_file_syncs_through_its_own_handle() {
        let (_dir, _path, conn) = database();
        os::sync(&conn).unwrap();
    }

    // A step that only lost a race for one of the WAL's locks is deferred.
    #[test]
    fn a_busy_checkpoint_step_is_deferred() {
        let busy = SqliteError::SqliteFailure(SqliteCode::new(SQLITE_BUSY), None);
        assert_eq!(defer_if_busy::<()>(Err(busy), "a checkpoint step"), None);
    }

    // A checkpoint that fails for any other reason takes the process down.
    // The checkpoint thread here opens the database read-only, so its first
    // pass fails to write the database file.
    #[test]
    fn a_failed_checkpoint_aborts_the_process() {
        assert_aborts(test_path!(a_failed_checkpoint_aborts_the_process), || {
            let (_dir, path, conn) = database();
            let read_only = PathBuf::from(format!("file:{}?mode=ro", path.display()));
            let thresholds = Thresholds { start: 8, ..NEVER };
            let mut checkpointer =
                Checkpointer::start_with_thresholds(&conn, &read_only, thresholds).unwrap();

            write(&conn, 16);
            checkpointer.after_commit(&conn);
            wait_for_abort();
        });
    }
}
