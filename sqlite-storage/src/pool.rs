//! A small pool of SQLite read connections with a single serialized write
//! lock, avoiding SQLite busy errors.

use std::path::PathBuf;

use rusqlite::Connection;
use tokio::sync::MutexGuard;
use trace_err::*;

pub struct ConnectionPool {
    path: PathBuf,
    connections: std::sync::Mutex<Vec<Connection>>,
    pub write_lock: tokio::sync::Mutex<()>,
}

impl ConnectionPool {
    pub fn new(path: PathBuf, connection: Connection) -> Self {
        Self {
            path,
            connections: std::sync::Mutex::new(vec![connection]),
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn new_connection<'a>(&'a self, guard: Option<&MutexGuard<'a, ()>>) -> Connection {
        let conn = Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .trace_expect("Failed to open connection");

        // We need a guard here, if we don't already have one, because we are writing to the DB
        let guard = if guard.is_none() {
            Some(self.write_lock.lock().await)
        } else {
            None
        };

        // journal_mode cannot be changed inside a transaction (migrations run
        // in one), so WAL is applied per connection here, not in the schema.
        // synchronous is a per-connection pragma; NORMAL under WAL fsyncs at
        // checkpoint rather than per commit. A crash loses at most the
        // un-checkpointed tail of commits, never consistency — and bundle data
        // storage is ground truth: restart replay re-ingests anything whose
        // metadata the tail forgot.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA optimize = 0x10002;",
        )
        .trace_expect("Failed to optimize");

        rusqlite::vtab::array::load_module(&conn).trace_expect("Failed to load array module");

        drop(guard);
        conn
    }

    pub async fn get<'a>(&'a self, guard: Option<&MutexGuard<'a, ()>>) -> Connection {
        if let Some(conn) = self
            .connections
            .lock()
            .trace_expect("Failed to lock mutex")
            .pop()
        {
            conn
        } else {
            self.new_connection(guard).await
        }
    }

    pub fn put(&self, conn: Connection) {
        self.connections
            .lock()
            .trace_expect("Failed to lock mutex")
            .push(conn)
    }
}
