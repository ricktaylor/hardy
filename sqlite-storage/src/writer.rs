//! The metadata store's single writer: every write runs on one dedicated
//! thread, which owns the only connection that writes.
//!
//! Writes queue to the thread and run in arrival order. Whatever has queued
//! while the thread was busy commits as one batch, in a single transaction,
//! with each write in its own savepoint: a failing write rolls back alone,
//! and every caller is answered only once its batch's commit has completed,
//! so no caller acts on a write that did not commit. An idle store commits
//! each write on its own, with no batching delay; under load the batches
//! grow by themselves.
//!
//! Some failures do not stop at a savepoint: on a full disk or an I/O
//! error, SQLite rolls the whole transaction back. The batch then stops
//! where it is. A failure that may clear by itself, such as a full disk or
//! a lock held by another process, rolls the batch back and runs it again,
//! writes and all, until the retry policy gives up. Then, or at once for a
//! failure that will not clear, every write in the batch fails, the ones
//! that had already succeeded included, because none of them took effect.
//!
//! The writer does not checkpoint the write-ahead log as it commits: between
//! batches it hands that to the [`Checkpointer`].

use std::{path::Path, sync::Arc};

use hardy_bpa::storage::{Error as StorageError, Result as StorageResult};
use rusqlite::{
    Connection, Error as SqliteError, Result as SqliteResult, Transaction, TransactionBehavior,
    ffi::{Error as SqliteCode, SQLITE_ABORT, SQLITE_MISUSE},
};
use thiserror::Error;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    oneshot::{Sender, channel},
};
use trace_err::*;

use super::{
    checkpoint::Checkpointer,
    retry::{RETRY, is_recoverable},
    thread::JoinOnDrop,
};

// The most writes one transaction commits. Every caller in a batch waits
// for the batch's commit, so the cap bounds how many writes the first
// waits on. It bounds their number, not their cost: a write that sweeps
// many rows, such as a peer queue's reset, holds up the rest of its batch.
const MAX_BATCH_SIZE: usize = 64;

// Why a write has no result of its own to report. Callers see it only
// through the storage error's `Display`: the type is not exported.
#[derive(Debug, Error)]
pub enum Error {
    // The write's batch lost its transaction, so none of the batch's writes
    // took effect: the transaction failed to begin or to commit, or SQLite
    // rolled it back when one of the writes failed, in a way that will not
    // clear or that outlasted the retries. Shared, because every caller in
    // the batch is answered with the same failure.
    #[error("Metadata write transaction failed: {0}")]
    Transaction(#[source] Arc<SqliteError>),

    // The writer thread has stopped: the store is shutting down, or the
    // writer thread has panicked and the process is about to abort. The
    // panic unwinds the thread before its guard aborts, so writes that were
    // queued, or arrive meanwhile, are answered with this first.
    #[error("Metadata writer thread has stopped")]
    Stopped,
}

pub struct Writer {
    // Declared before `_thread`: fields drop in declaration order, so the
    // queue closes, and the thread finishes and exits, before it is joined.
    // Dropping a `Writer` therefore blocks until the writes already queued
    // have committed and the WAL has been checkpointed.
    queue: UnboundedSender<Box<dyn Job>>,
    _thread: JoinOnDrop,
}

impl Writer {
    // Starts the writer thread on `conn`, the store's writing connection
    // to the database at `path`.
    pub fn new(conn: Connection, path: &Path) -> Self {
        let checkpointer = Checkpointer::start(&conn, path)
            .trace_expect("Failed to start the metadata checkpoint thread");
        let (queue, jobs) = unbounded_channel();
        let thread = JoinOnDrop::spawn("sqlite-writer", move || run(conn, jobs, checkpointer))
            .trace_expect("Failed to spawn the metadata writer thread");
        Self {
            queue,
            _thread: thread,
        }
    }

    // Queues `write` to run atomically on the writer thread: its statements
    // commit together or not at all. Every caller awaits its own reply, so
    // the queue holds at most one write per caller and needs no bound.
    //
    // `write` may run more than once: a batch that failed in a way that may
    // clear is rolled back and run again. Each run starts from the state
    // the rollback restored, so a write that does the same thing each time
    // it runs takes effect once.
    //
    // Dropping the returned future cancels the write only until the writer
    // thread starts it. A write already running commits with its batch, and
    // its result is discarded.
    pub async fn write<F, R>(&self, write: F) -> StorageResult<R>
    where
        F: FnMut(&Connection) -> StorageResult<R> + Send + 'static,
        R: Send + 'static,
    {
        let (reply, result) = channel();
        self.queue
            .send(Box::new(Request {
                write,
                result: None,
                reply,
            }))
            .map_err(|_| Error::Stopped)?;
        result.await.map_err(|_| Error::Stopped)?
    }
}

// How a batch's transaction ended.
type BatchResult = Result<(), Arc<SqliteError>>;

// A queued write, type-erased so one queue carries every result type.
trait Job: Send {
    // Runs the write in its own savepoint, holding its result for `reply`.
    // Fails if the batch must stop: SQLite rolled the whole batch back, not
    // just this write, or the write failed in a way that may clear, which
    // makes the batch worth running again. The write's other failures are
    // not this function's: they are held for the reply.
    fn run(&mut self, transaction: &mut Transaction<'_>) -> SqliteResult<()>;

    // Replies to the caller once the write's batch has completed: with the
    // write's own result if the batch committed, else with the batch's
    // failure.
    fn reply(self: Box<Self>, batch: &BatchResult);
}

// A write and the caller awaiting its result.
struct Request<F, R> {
    write: F,
    // The write's result from the batch's latest run, or `None` if it was
    // not run: the caller had gone before the write's turn, or the batch
    // stopped first.
    result: Option<StorageResult<R>>,
    reply: Sender<StorageResult<R>>,
}

impl<F, R> Job for Request<F, R>
where
    F: FnMut(&Connection) -> StorageResult<R> + Send + 'static,
    R: Send + 'static,
{
    fn run(&mut self, transaction: &mut Transaction<'_>) -> SqliteResult<()> {
        self.result = None;
        // A caller whose future dropped before its write's turn is not
        // written for: nobody would act on the result.
        if self.reply.is_closed() {
            return Ok(());
        }
        let result = with_savepoint(transaction, &mut self.write);
        let stop = if transaction.is_autocommit() {
            Some(transaction_lost(&result))
        } else {
            result
                .as_ref()
                .err()
                .and_then(sqlite_failure)
                .filter(is_recoverable)
        };
        self.result = Some(result);
        stop.map_or(Ok(()), Err)
    }

    fn reply(self: Box<Self>, batch: &BatchResult) {
        let result = match (batch, self.result) {
            (Err(e), _) => Err(Error::Transaction(e.clone()).into()),
            (Ok(()), Some(result)) => result,
            (Ok(()), None) => return,
        };
        _ = self.reply.send(result);
    }
}

// A copy of a write's error, if SQLite raised it. The write's error is
// boxed, and not `Clone`, so the copy keeps what identifies it: the result
// code and the message.
fn sqlite_failure(e: &StorageError) -> Option<SqliteError> {
    match e.downcast_ref::<SqliteError>() {
        Some(SqliteError::SqliteFailure(code, message)) => {
            Some(SqliteError::SqliteFailure(*code, message.clone()))
        }
        _ => None,
    }
}

// The error to fail a batch with, when the write that returned `result`
// left the connection without a transaction.
fn transaction_lost<R>(result: &StorageResult<R>) -> SqliteError {
    match result {
        // SQLite rolled the transaction back as the write failed.
        Err(e) => sqlite_failure(e).unwrap_or_else(|| {
            SqliteError::SqliteFailure(SqliteCode::new(SQLITE_ABORT), Some(e.to_string()))
        }),
        // The write ended the transaction itself, which no write may do.
        Ok(_) => SqliteError::SqliteFailure(
            SqliteCode::new(SQLITE_MISUSE),
            Some("a write ended its batch's transaction".into()),
        ),
    }
}

// The writer thread: commits whatever has queued as one batch, until the
// store drops and the queue closes, then checkpoints the WAL.
fn run(
    mut conn: Connection,
    mut jobs: UnboundedReceiver<Box<dyn Job>>,
    mut checkpointer: Checkpointer,
) {
    let mut batch = Vec::with_capacity(MAX_BATCH_SIZE);
    while jobs.blocking_recv_many(&mut batch, MAX_BATCH_SIZE) > 0 {
        let committed = RETRY
            .run("Metadata write batch", is_recoverable, || {
                run_batch(&mut conn, &mut batch)
            })
            .map_err(Arc::new);
        for job in batch.drain(..) {
            job.reply(&committed);
        }
        // Once the batch's callers are answered: a checkpoint here holds up
        // only the writes still queued.
        checkpointer.after_commit(&conn);
    }
    checkpointer.stop(&conn);
}

// Runs a batch of writes in one transaction, returning how it ended.
// Dropped without committing, the transaction rolls back, unless SQLite
// has already rolled it back: a commit that fails cannot leave it open for
// the next batch to join, and a batch that stops early needs no cleanup,
// so the batch can run again from the start.
fn run_batch(conn: &mut Connection, batch: &mut [Box<dyn Job>]) -> SqliteResult<()> {
    let mut transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for job in batch {
        // On a lost transaction, SQLite has rolled the batch back. A write
        // run now would commit on its own, outside any batch, so none is.
        job.run(&mut transaction)?;
    }
    transaction.commit()
}

// Runs `write` in its own savepoint, so a failing write rolls back alone
// and the rest of its batch still commits. Dropped without being released,
// the savepoint rolls back. If SQLite has rolled the whole transaction back
// instead, there is nothing left to roll back to, and the caller, finding
// the transaction gone, stops the batch. A savepoint whose release fails
// in a way that may clear also stops it, to be run again.
fn with_savepoint<F, R>(transaction: &mut Transaction<'_>, write: F) -> StorageResult<R>
where
    F: FnOnce(&Connection) -> StorageResult<R>,
{
    let savepoint = transaction.savepoint()?;
    let result = write(&savepoint)?;
    savepoint.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use core::{
        pin::{Pin, pin},
        task::{Context, Waker},
    };

    // std's channel, which blocks the writer thread where tokio's would
    // need a runtime; `super::*` brings in tokio's `Sender` and `channel`.
    use std::{
        path::PathBuf,
        sync::mpsc::{Sender as BlockingSender, channel as blocking_channel},
        time::Duration,
    };

    use rusqlite::{ErrorCode, ffi::SQLITE_IOERR_WRITE};
    use tempfile::{TempDir, tempdir};
    use tokio::time::timeout;

    use super::*;
    use crate::thread::tests::{assert_aborts, test_path, wait_for_abort};

    // A writer over a scratch database in WAL mode, as the store's is:
    // `rows` takes plain inserts, and `refs` holds a deferred foreign key,
    // checked only at commit.
    fn open() -> (TempDir, Writer) {
        let (dir, path, conn) = database();
        (dir, Writer::new(conn, &path))
    }

    fn database() -> (TempDir, PathBuf, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            CREATE TABLE rows (id INTEGER PRIMARY KEY, filler BLOB);
            CREATE TABLE refs (row INTEGER REFERENCES rows(id) DEFERRABLE INITIALLY DEFERRED);",
        )
        .unwrap();
        (dir, path, conn)
    }

    fn insert(conn: &Connection, id: i64) -> StorageResult<()> {
        conn.execute("INSERT INTO rows (id) VALUES (?1)", [id])?;
        Ok(())
    }

    async fn ids(writer: &Writer) -> Vec<i64> {
        within_timeout(writer.write(|conn| {
            conn.prepare("SELECT id FROM rows ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()
                .map_err(Into::into)
        }))
        .await
        .unwrap()
    }

    // Awaits something the writer thread answers. A writer that never
    // answers would hang the suite, so the wait fails instead: the timeout
    // only bounds a regression.
    async fn within_timeout<F: Future>(waiting: F) -> F::Output {
        timeout(Duration::from_secs(30), waiting)
            .await
            .expect("the writer thread never answered")
    }

    // Polls `write` once, which queues its job, and leaves it pending.
    fn enqueue<F: Future>(write: Pin<&mut F>) {
        let poll = write.poll(&mut Context::from_waker(Waker::noop()));
        assert!(poll.is_pending());
    }

    // Holds the writer thread inside a batch of its own until the returned
    // sender fires, so the writes queued meanwhile all land in the next
    // batch.
    async fn hold(
        writer: &Writer,
    ) -> (
        Pin<Box<impl Future<Output = StorageResult<()>> + '_>>,
        BlockingSender<()>,
    ) {
        let (started, running) = channel();
        let (release, released) = blocking_channel::<()>();
        let mut started = Some(started);
        let mut held = Box::pin(writer.write(move |_| {
            if let Some(started) = started.take() {
                started.send(()).unwrap();
                released.recv().unwrap();
            }
            Ok(())
        }));
        enqueue(held.as_mut());
        within_timeout(running).await.unwrap();
        (held, release)
    }

    // A write that fails part-way rolls back its own statements, and the
    // rest of its batch commits around it.
    #[tokio::test]
    async fn a_failing_write_rolls_back_alone() {
        let (_dir, writer) = open();
        let (held, release) = hold(&writer).await;

        let mut first = pin!(writer.write(|conn| insert(conn, 1)));
        enqueue(first.as_mut());
        let mut failing = pin!(writer.write(|conn| {
            insert(conn, 2)?;
            insert(conn, 1)
        }));
        enqueue(failing.as_mut());
        let mut last = pin!(writer.write(|conn| insert(conn, 3)));
        enqueue(last.as_mut());

        release.send(()).unwrap();
        within_timeout(held).await.unwrap();
        within_timeout(first).await.unwrap();
        within_timeout(last).await.unwrap();
        // The write's own failure, not its batch's.
        let error = within_timeout(failing).await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<SqliteError>()
                .and_then(SqliteError::sqlite_error_code),
            Some(ErrorCode::ConstraintViolation)
        );
        assert_eq!(ids(&writer).await, [1, 3]);
    }

    // A batch whose commit fails reports the failure to every write in it,
    // including those whose own statements succeeded, and none takes
    // effect; the writer carries on.
    #[tokio::test]
    async fn a_failed_commit_fails_its_whole_batch() {
        let (_dir, writer) = open();
        let (held, release) = hold(&writer).await;

        let mut inserted = pin!(writer.write(|conn| insert(conn, 1)));
        enqueue(inserted.as_mut());
        let mut dangling = pin!(writer.write(|conn| {
            conn.execute("INSERT INTO refs (row) VALUES (99)", [])?;
            Ok(())
        }));
        enqueue(dangling.as_mut());

        release.send(()).unwrap();
        within_timeout(held).await.unwrap();
        for result in [
            within_timeout(inserted).await,
            within_timeout(dangling).await,
        ] {
            let error = result.unwrap_err();
            let Some(Error::Transaction(e)) = error.downcast_ref::<Error>() else {
                panic!("expected the batch's failure, got {error:?}");
            };
            assert_eq!(e.sqlite_error_code(), Some(ErrorCode::ConstraintViolation));
        }
        assert_eq!(ids(&writer).await, Vec::<i64>::new());
    }

    // A write whose caller has gone before its turn is not run.
    #[tokio::test]
    async fn an_abandoned_write_is_not_run() {
        let (_dir, writer) = open();
        let (held, release) = hold(&writer).await;

        let mut abandoned = Box::pin(writer.write(|conn| insert(conn, 1)));
        enqueue(abandoned.as_mut());
        drop(abandoned);

        release.send(()).unwrap();
        within_timeout(held).await.unwrap();
        assert_eq!(ids(&writer).await, Vec::<i64>::new());
    }

    // A write that fails in a way that may clear rolls its whole batch back,
    // and the batch runs again, writes and all: each write takes effect
    // once, and every caller gets its own result.
    #[tokio::test]
    async fn a_batch_that_fails_until_the_failure_clears_runs_again() {
        let (_dir, writer) = open();
        let (held, release) = hold(&writer).await;

        let mut before = pin!(writer.write(|conn| insert(conn, 1)));
        enqueue(before.as_mut());
        let mut runs = 0;
        let mut flaky = pin!(writer.write(move |conn| {
            insert(conn, 2)?;
            runs += 1;
            if runs == 1 {
                Err(SqliteError::SqliteFailure(SqliteCode::new(SQLITE_IOERR_WRITE), None).into())
            } else {
                Ok(runs)
            }
        }));
        enqueue(flaky.as_mut());
        let mut after = pin!(writer.write(|conn| insert(conn, 3)));
        enqueue(after.as_mut());

        release.send(()).unwrap();
        within_timeout(held).await.unwrap();
        within_timeout(before).await.unwrap();
        assert_eq!(within_timeout(flaky).await.unwrap(), 2);
        within_timeout(after).await.unwrap();
        assert_eq!(ids(&writer).await, [1, 2, 3]);
    }

    // A write that fills the database makes SQLite roll the whole batch
    // back, not just that write. The batch runs again while the retry
    // policy allows; the disk stays full, so every write in the batch then
    // fails with what went wrong, the one that had already succeeded
    // included, and the writes after it are not run: left to run, they
    // would commit on their own.
    #[tokio::test]
    async fn a_write_that_loses_the_transaction_fails_its_whole_batch() {
        let (_dir, path, conn) = database();
        // Room for a few more pages, and no more.
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        conn.query_row(
            &format!("PRAGMA max_page_count = {}", pages + 8),
            [],
            |_| Ok(()),
        )
        .unwrap();
        let writer = Writer::new(conn, &path);
        let (held, release) = hold(&writer).await;

        let mut before = pin!(writer.write(|conn| insert(conn, 1)));
        enqueue(before.as_mut());
        let mut filling = pin!(writer.write(|conn| {
            conn.execute(
                "INSERT INTO rows (id, filler) VALUES (2, zeroblob(1000000))",
                [],
            )?;
            Ok(())
        }));
        enqueue(filling.as_mut());
        let mut after = pin!(writer.write(|conn| insert(conn, 3)));
        enqueue(after.as_mut());

        release.send(()).unwrap();
        within_timeout(held).await.unwrap();
        for result in [
            within_timeout(before).await,
            within_timeout(filling).await,
            within_timeout(after).await,
        ] {
            let error = result.unwrap_err();
            let Some(Error::Transaction(e)) = error.downcast_ref::<Error>() else {
                panic!("expected the batch's failure, got {error:?}");
            };
            assert_eq!(e.sqlite_error_code(), Some(ErrorCode::DiskFull));
        }
        assert_eq!(ids(&writer).await, Vec::<i64>::new());

        // The writer carries on, in a transaction of its own.
        within_timeout(writer.write(|conn| insert(conn, 4)))
            .await
            .unwrap();
        assert_eq!(ids(&writer).await, [4]);
    }

    // A write that panics takes the process down with it: the store must
    // not carry on, answering every later write with an error.
    #[test]
    fn a_panicking_write_aborts_the_process() {
        assert_aborts(test_path!(a_panicking_write_aborts_the_process), || {
            let (_dir, writer) = open();
            let mut write = pin!(
                writer.write(|_| -> StorageResult<()> { panic!("the write failed an invariant") })
            );
            enqueue(write.as_mut());
            wait_for_abort();
        });
    }
}
