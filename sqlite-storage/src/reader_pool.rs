//! The metadata store's reader pool: a bounded pool of query-only connections,
//! each read running on the blocking thread pool so that a slow query never
//! stalls an async worker.
//!
//! The pool is a semaphore over a list of idle connections.

use std::{
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::available_parallelism,
};

use hardy_bpa::storage::Result as StorageResult;
use rusqlite::{Connection, OpenFlags, vtab::array::load_module};
use tokio::{sync::Semaphore, task::spawn_blocking};
use trace_err::*;

use super::thread::AbortOnPanicGuard;

pub struct ReaderPool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    // One permit per connection the pool may hold: a read waits for a
    // permit rather than opening a connection beyond the bound.
    permits: Arc<Semaphore>,
}

impl ReaderPool {
    // A pool of up to one reader per hardware thread over the database at
    // `path`, each opened on first use.
    pub fn new(path: PathBuf) -> Self {
        let size = available_parallelism().map_or(1, NonZeroUsize::get);
        Self {
            path,
            idle: Mutex::new(Vec::with_capacity(size)),
            permits: Arc::new(Semaphore::new(size)),
        }
    }

    // Runs `read` on a pooled connection, off the async runtime. A read sees
    // every write whose caller has been answered: the writer answers only
    // once its batch has committed.
    pub async fn read<F, R>(self: &Arc<Self>, read: F) -> StorageResult<R>
    where
        F: FnOnce(&Connection) -> StorageResult<R> + Send + 'static,
        R: Send + 'static,
    {
        let permit = self.permits.clone().acquire_owned().await?;
        let pool = self.clone();
        spawn_blocking(move || {
            // The runtime would otherwise catch a panic here and hand it
            // to the one caller awaiting this read.
            let _guard = AbortOnPanicGuard::new("sqlite read");
            let conn = pool.acquire()?;
            let result = read(&conn);
            pool.release(conn);
            drop(permit);
            result
        })
        .await?
    }

    // A pooled connection, or a new one if every pooled one is in use.
    fn acquire(&self) -> StorageResult<Connection> {
        let idle = self
            .idle
            .lock()
            .trace_expect("Failed to lock the reader pool")
            .pop();
        idle.map_or_else(|| self.open(), Ok)
    }

    // Returns a connection to the pool.
    fn release(&self, conn: Connection) {
        self.idle
            .lock()
            .trace_expect("Failed to lock the reader pool")
            .push(conn);
    }

    fn open(&self) -> StorageResult<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // A reader never writes: `query_only` refuses any statement that
        // would. Journal mode persists in the database file, and the other
        // connection pragmas govern writes, so the writer alone sets them.
        conn.execute_batch("PRAGMA query_only = ON;")?;
        // Modules load per connection. No read uses `rarray` yet, but a
        // query moved here from the writer would otherwise fail only at
        // run time, with "no such table".
        load_module(&conn)?;
        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{Error as SqliteError, ErrorCode};
    use tempfile::tempdir;
    use tokio::runtime::Builder;

    use super::*;
    use crate::thread::tests::{assert_aborts, test_path};

    // A reader refuses to write, so a write routed through the readers
    // fails loudly rather than racing the writer thread.
    #[tokio::test]
    async fn a_reader_refuses_to_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE rows (id INTEGER);")
            .unwrap();
        let pool = Arc::new(ReaderPool::new(path));

        let error = pool
            .read(|conn| {
                conn.execute("INSERT INTO rows (id) VALUES (1)", [])?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<SqliteError>()
                .and_then(SqliteError::sqlite_error_code),
            Some(ErrorCode::ReadOnly)
        );
    }

    // A read that panics takes the process down with it, where the
    // runtime's blocking pool would otherwise catch the panic and hand it
    // to the read's one caller.
    #[test]
    fn a_panicking_read_aborts_the_process() {
        assert_aborts(test_path!(a_panicking_read_aborts_the_process), || {
            let dir = tempdir().unwrap();
            let path = dir.path().join("test.db");
            Connection::open(&path)
                .unwrap()
                .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE rows (id INTEGER);")
                .unwrap();
            let pool = Arc::new(ReaderPool::new(path));
            let runtime = Builder::new_current_thread().build().unwrap();
            _ = runtime.block_on(
                pool.read(|_| -> StorageResult<()> { panic!("the read failed an invariant") }),
            );
        });
    }
}
