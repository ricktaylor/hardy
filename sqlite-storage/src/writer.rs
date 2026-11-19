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

use std::{
    sync::Arc,
    thread::{Builder, JoinHandle},
};

use hardy_bpa::storage::Result as StorageResult;
use rusqlite::{Connection, Error as SqliteError, Transaction, TransactionBehavior};
use thiserror::Error;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    oneshot::{Sender, channel},
};
use trace_err::*;

// The most writes one transaction commits. Every caller in a batch waits
// for the batch's commit, so the cap bounds how long the first waits on
// the rest.
const MAX_BATCH: usize = 64;

/// Why a write has no result of its own to report.
#[derive(Debug, Error)]
pub enum Error {
    /// The write's batch failed to begin or to commit its transaction, so
    /// none of the batch's writes took effect. Shared, because every caller
    /// in the batch is answered with the same failure.
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
    // Starts the writer thread on `conn`, the store's writing connection.
    pub fn new(conn: Connection) -> Self {
        let (queue, jobs) = unbounded_channel();
        let thread = Builder::new()
            .name("sqlite-writer".into())
            .spawn(move || serve(conn, jobs))
            .trace_expect("Failed to spawn the metadata writer thread");
        Self {
            queue,
            _thread: Joined(Some(thread)),
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
                write: Some(write),
                result: None,
                reply,
            }))
            .map_err(|_| Error::Stopped)?;
        result.await.map_err(|_| Error::Stopped)?
    }
}

// Joins the writer thread on drop, so the store's writing connection has
// closed by the time the store has.
struct Joined(Option<JoinHandle<()>>);

impl Drop for Joined {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            // A thread that panicked has already failed its callers with
            // `Stopped`; there is nothing left to answer.
            _ = thread.join();
        }
    }
}

// How a batch's transaction resolved.
type BatchResult = Result<(), Arc<SqliteError>>;

// A queued write, type-erased so one queue carries every result type.
trait Job: Send {
    // Runs the write in its own savepoint, holding its result for `answer`.
    fn run(&mut self, trans: &mut Transaction<'_>);

    // Answers the caller once the write's batch has resolved: with the
    // write's own result if the batch committed, else with the batch's
    // failure.
    fn answer(self: Box<Self>, batch: &BatchResult);
}

// A write and the caller awaiting its result.
struct Request<F, R> {
    write: Option<F>,
    result: Option<StorageResult<R>>,
    reply: Sender<StorageResult<R>>,
}

impl<F, R> Job for Request<F, R>
where
    F: FnOnce(&Connection) -> StorageResult<R> + Send + 'static,
    R: Send + 'static,
{
    fn run(&mut self, trans: &mut Transaction<'_>) {
        // A caller whose future dropped before its write's turn is not
        // written for: nobody would act on the result.
        if let Some(write) = self.write.take()
            && !self.reply.is_closed()
        {
            self.result = Some(atomically(trans, write));
        }
    }

    fn answer(self: Box<Self>, batch: &BatchResult) {
        let result = match batch {
            Ok(()) => self.result,
            Err(e) => Some(Err(Error::Transaction(e.clone()).into())),
        };
        if let Some(result) = result {
            _ = self.reply.send(result);
        }
    }
}

// The writer thread: commits whatever has queued as one batch, until the
// store drops and the queue closes.
fn serve(mut conn: Connection, mut jobs: UnboundedReceiver<Box<dyn Job>>) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    while jobs.blocking_recv_many(&mut batch, MAX_BATCH) > 0 {
        let committed = run_batch(&mut conn, &mut batch);
        for job in batch.drain(..) {
            job.answer(&committed);
        }
    }
}

// Runs a batch of writes in one transaction, returning how its commit
// resolved.
fn run_batch(conn: &mut Connection, batch: &mut [Box<dyn Job>]) -> BatchResult {
    let mut trans = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for job in batch {
        job.run(&mut trans);
    }
    Ok(trans.commit()?)
}

// Runs `write` in its own savepoint, so a failing write rolls back alone
// and the rest of its batch still commits.
fn atomically<F, R>(trans: &mut Transaction<'_>, write: F) -> StorageResult<R>
where
    F: FnOnce(&Connection) -> StorageResult<R>,
{
    let savepoint = trans.savepoint()?;
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

    use rusqlite::ErrorCode;
    use tempfile::{TempDir, tempdir};

    use super::*;

    // A writer over a scratch database: `rows` takes plain inserts, and
    // `refs` holds a deferred foreign key, checked only at commit.
    fn open() -> (TempDir, Writer) {
        let dir = tempdir().unwrap();
        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
            CREATE TABLE rows (id INTEGER PRIMARY KEY);
            CREATE TABLE refs (row INTEGER REFERENCES rows(id) DEFERRABLE INITIALLY DEFERRED);",
        )
        .unwrap();
        (dir, Writer::new(conn))
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
}
