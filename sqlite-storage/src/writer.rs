//! The metadata store's single writer: every write runs on one dedicated
//! thread, which owns the only connection that writes.
//!
//! Writes queue to the thread and run in arrival order. Whatever has queued
//! while the thread was busy commits as one batch, in a single transaction,
//! with each write in its own savepoint: a failing write rolls back alone,
//! and every caller is answered only once its batch's commit has resolved,
//! so no caller acts on a write that did not commit. An idle store commits
//! each write on its own, with no batching delay; under load the batches
//! grow by themselves.
//!
//! Some failures do not stop at a savepoint: on a full disk or an I/O
//! error, SQLite rolls the whole transaction back. The batch then stops
//! where it is and every write in it fails, the ones that had already
//! succeeded included, because none of them took effect.
//!
//! The writer does not checkpoint the write-ahead log as it commits: between
//! batches it hands that to the [`Checkpointer`].

use std::{mem, path::Path, sync::Arc, thread::Builder};

use hardy_bpa::storage::Result as StorageResult;
use rusqlite::{
    Connection, Error as SqliteError,
    ffi::{Error as SqliteCode, SQLITE_ABORT, SQLITE_MISUSE},
};
use thiserror::Error;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    oneshot::{Sender, channel},
};
use trace_err::*;

use super::{checkpoint::Checkpointer, joined::Joined};

// The most writes one transaction commits. Every caller in a batch waits
// for the batch's commit, so the cap bounds how long the first waits on
// the rest.
const MAX_BATCH: usize = 64;

/// Why a write has no result of its own to report.
#[derive(Debug, Error)]
pub enum Error {
    /// The write's batch lost its transaction, so none of the batch's
    /// writes took effect: the transaction failed to begin or to commit, or
    /// SQLite rolled it back when one of the writes failed. Shared, because
    /// every caller in the batch is answered with the same failure.
    #[error("Metadata write transaction failed: {0}")]
    Transaction(Arc<SqliteError>),

    /// The writer thread has stopped.
    #[error("Metadata writer thread has stopped")]
    Stopped,
}

pub struct Writer {
    // Declared before `_thread`: fields drop in declaration order, so the
    // queue closes, and the thread finishes and exits, before it is joined.
    queue: UnboundedSender<Box<dyn Job>>,
    _thread: Joined,
}

impl Writer {
    // Starts the writer thread on `conn`, the store's writing connection
    // to the database at `path`.
    pub fn new(conn: Connection, path: &Path) -> Self {
        let checkpointer = Checkpointer::start(&conn, path)
            .trace_expect("Failed to start the metadata checkpoint thread");
        let (queue, jobs) = unbounded_channel();
        let thread = Builder::new()
            .name("sqlite-writer".into())
            .spawn(move || serve(conn, jobs, checkpointer))
            .trace_expect("Failed to spawn the metadata writer thread");
        Self {
            queue,
            _thread: thread.into(),
        }
    }

    // Queues `write` to run atomically on the writer thread: its statements
    // commit together or not at all. Every caller awaits its own reply, so
    // the queue holds at most one write per caller and needs no bound.
    pub async fn write<F, R>(&self, write: F) -> StorageResult<R>
    where
        F: FnOnce(&Connection) -> StorageResult<R> + Send + 'static,
        R: Send + 'static,
    {
        let (reply, result) = channel();
        self.queue
            .send(Box::new(Request {
                stage: Stage::Queued(write),
                reply,
            }))
            .map_err(|_| Error::Stopped)?;
        result.await.map_err(|_| Error::Stopped)?
    }
}

// How a batch's transaction resolved.
type BatchResult = Result<(), Arc<SqliteError>>;

// A queued write, type-erased so one queue carries every result type.
trait Job: Send {
    // Runs the write in its own savepoint, holding its result for `answer`.
    // Returns the write's error if it left `conn` without a transaction:
    // SQLite rolled the whole batch back, not just this write.
    fn run(&mut self, conn: &Connection) -> Option<SqliteError>;

    // Answers the caller once the write's batch has resolved: with the
    // write's own result if the batch committed, else with the batch's
    // failure.
    fn answer(self: Box<Self>, batch: &BatchResult);
}

// How far a queued write has got.
enum Stage<F, R> {
    Queued(F),
    Ran(StorageResult<R>),
    // Not run: the caller had gone before the write's turn, or the batch
    // lost its transaction first.
    Skipped,
}

// A write and the caller awaiting its result.
struct Request<F, R> {
    stage: Stage<F, R>,
    reply: Sender<StorageResult<R>>,
}

impl<F, R> Job for Request<F, R>
where
    F: FnOnce(&Connection) -> StorageResult<R> + Send + 'static,
    R: Send + 'static,
{
    fn run(&mut self, conn: &Connection) -> Option<SqliteError> {
        let Stage::Queued(write) = mem::replace(&mut self.stage, Stage::Skipped) else {
            return None;
        };
        // A caller whose future dropped before its write's turn is not
        // written for: nobody would act on the result.
        if self.reply.is_closed() {
            return None;
        }
        let result = atomically(conn, write);
        let lost = conn.is_autocommit().then(|| transaction_lost(&result));
        self.stage = Stage::Ran(result);
        lost
    }

    fn answer(self: Box<Self>, batch: &BatchResult) {
        let result = match (batch, self.stage) {
            (Err(e), _) => Err(Error::Transaction(e.clone()).into()),
            (Ok(()), Stage::Ran(result)) => result,
            (Ok(()), Stage::Queued(_) | Stage::Skipped) => return,
        };
        _ = self.reply.send(result);
    }
}

// The error to fail a batch with, when the write that returned `result`
// left the connection without a transaction.
fn transaction_lost<R>(result: &StorageResult<R>) -> SqliteError {
    match result {
        // SQLite rolled the transaction back as the write failed. The
        // write's error is boxed, and not `Clone`, so the batch's copy
        // keeps what identifies it: the result code and the message.
        Err(e) => match e.downcast_ref::<SqliteError>() {
            Some(SqliteError::SqliteFailure(code, message)) => {
                SqliteError::SqliteFailure(*code, message.clone())
            }
            _ => SqliteError::SqliteFailure(SqliteCode::new(SQLITE_ABORT), Some(e.to_string())),
        },
        // The write ended the transaction itself, which no write may do.
        Ok(_) => SqliteError::SqliteFailure(
            SqliteCode::new(SQLITE_MISUSE),
            Some("a write ended its batch's transaction".into()),
        ),
    }
}

// The writer thread: commits whatever has queued as one batch, until the
// store drops and the queue closes.
fn serve(
    conn: Connection,
    mut jobs: UnboundedReceiver<Box<dyn Job>>,
    mut checkpointer: Checkpointer,
) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    while jobs.blocking_recv_many(&mut batch, MAX_BATCH) > 0 {
        let committed = run_batch(&conn, &mut batch);
        for job in batch.drain(..) {
            job.answer(&committed);
        }
        // Once the batch's callers are answered: a checkpoint here holds up
        // only the writes still queued.
        checkpointer.after_commit(&conn);
    }
}

// Runs a batch of writes in one transaction, returning how it resolved.
fn run_batch(conn: &Connection, batch: &mut [Box<dyn Job>]) -> BatchResult {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    for job in batch {
        if let Some(lost) = job.run(conn) {
            // SQLite has rolled the batch back. A write run now would
            // commit on its own, outside any batch, so none is.
            return Err(lost.into());
        }
    }
    conn.execute_batch("COMMIT").map_err(|e| {
        // A commit that fails can leave the transaction open; the next
        // batch must not join it.
        if !conn.is_autocommit() {
            _ = conn.execute_batch("ROLLBACK");
        }
        e.into()
    })
}

// Runs `write` in its own savepoint, so a failing write rolls back alone
// and the rest of its batch still commits.
fn atomically<F, R>(conn: &Connection, write: F) -> StorageResult<R>
where
    F: FnOnce(&Connection) -> StorageResult<R>,
{
    conn.execute_batch("SAVEPOINT write")?;
    let result = write(conn);
    if result.is_err() {
        // Fails only if SQLite has already rolled the whole transaction
        // back, which the caller checks for.
        _ = conn.execute_batch("ROLLBACK TO write");
    }
    // Likewise: with no transaction left there is no savepoint to release.
    _ = conn.execute_batch("RELEASE write");
    result
}

#[cfg(test)]
mod tests {
    use core::{
        pin::{Pin, pin},
        task::{Context, Waker},
    };

    use std::path::PathBuf;

    use rusqlite::ErrorCode;
    use tempfile::{TempDir, tempdir};

    use super::*;

    // A writer over a scratch database: `rows` takes plain inserts, and
    // `refs` holds a deferred foreign key, checked only at commit.
    fn open() -> (TempDir, Writer) {
        let (dir, path, conn) = database();
        (dir, Writer::new(conn, &path))
    }

    fn database() -> (TempDir, PathBuf, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
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
        writer
            .write(|conn| {
                conn.prepare("SELECT id FROM rows ORDER BY id")?
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<_, _>>()
                    .map_err(Into::into)
            })
            .await
            .unwrap()
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
        Sender<()>,
    ) {
        let (started, running) = channel();
        let (release, released) = channel::<()>();
        let mut held = Box::pin(writer.write(move |_| {
            started.send(()).unwrap();
            released.blocking_recv().unwrap();
            Ok(())
        }));
        enqueue(held.as_mut());
        running.await.unwrap();
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
        held.await.unwrap();
        first.await.unwrap();
        last.await.unwrap();
        // The write's own failure, not its batch's.
        let error = failing.await.unwrap_err();
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
        held.await.unwrap();
        for result in [inserted.await, dangling.await] {
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
        held.await.unwrap();
        assert_eq!(ids(&writer).await, Vec::<i64>::new());
    }

    // A write that fills the database makes SQLite roll the whole batch
    // back, not just that write. Every write in the batch fails with what
    // went wrong, the one that had already succeeded included, and the
    // writes after it are not run: left to run, they would commit on
    // their own.
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
        held.await.unwrap();
        for result in [before.await, filling.await, after.await] {
            let error = result.unwrap_err();
            let Some(Error::Transaction(e)) = error.downcast_ref::<Error>() else {
                panic!("expected the batch's failure, got {error:?}");
            };
            assert_eq!(e.sqlite_error_code(), Some(ErrorCode::DiskFull));
        }
        assert_eq!(ids(&writer).await, Vec::<i64>::new());

        // The writer carries on, in a transaction of its own.
        writer.write(|conn| insert(conn, 4)).await.unwrap();
        assert_eq!(ids(&writer).await, [4]);
    }
}
