//! Retrying a store step whose failure may clear by itself.
//!
//! The store's threads keep trying while a failure may clear: a full disk,
//! a shortage of memory or of file descriptors, or an I/O error other than a
//! failed sync. Once the retry policy's time is up, and at once for any
//! other failure, the step's error stands, and the failure is treated as
//! one that will not clear.
//!
//! The pause between retries is fixed. Nothing retried here is contended,
//! so spacing the retries out would not help a failure clear; a short pause
//! notices it has cleared soon after it does.

use std::{
    thread::sleep,
    time::{Duration, Instant},
};

use rusqlite::{
    Error as SqliteError, ErrorCode, Result as SqliteResult,
    ffi::{SQLITE_IOERR_CORRUPTFS, SQLITE_IOERR_DATA, SQLITE_IOERR_DIR_FSYNC, SQLITE_IOERR_FSYNC},
};
use tracing::{debug, error, warn};

// How a store thread retries a step that failed in a way that may clear.
#[derive(Clone, Copy)]
pub struct Retry {
    // The pause before each retry: a failure that clears holds the step up
    // for at most this much longer than it lasts.
    pause: Duration,
    // How long the step may spend pausing before its failure stands. Time
    // the step itself takes to fail is not counted.
    budget: Duration,
}

// Pauses of 100 ms, for a minute.
#[cfg(not(test))]
pub const RETRY: Retry = Retry {
    pause: Duration::from_millis(100),
    budget: Duration::from_secs(60),
};

// Tests pause a millisecond at a time, for a few retries.
#[cfg(test)]
pub const RETRY: Retry = Retry {
    pause: Duration::from_millis(1),
    budget: Duration::from_millis(5),
};

impl Retry {
    // Runs `step` until it succeeds, fails in a way that will not clear, or
    // has spent its budget, and returns its last result. `what` names the
    // step in the log.
    pub fn run<T>(self, what: &str, mut step: impl FnMut() -> SqliteResult<T>) -> SqliteResult<T> {
        let mut paused = Duration::ZERO;
        loop {
            let e = match step() {
                Err(e) if is_recoverable(&e) => e,
                result => return result,
            };
            if paused >= self.budget {
                error!("{what} still failing after {paused:?}: {e}");
                return Err(e);
            }
            if paused.is_zero() {
                warn!("{what} failed, retrying for up to {:?}: {e}", self.budget);
            } else {
                debug!("{what} failed again: {e}");
            }
            let started = Instant::now();
            sleep(self.pause.min(self.budget - paused));
            paused += started.elapsed();
        }
    }
}

// Whether a step lost a race for one of the database's locks.
pub fn is_busy(e: &SqliteError) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
}

// Whether a failure may clear by itself, so that the step that hit it is
// worth running again.
//
// A failed sync is not: the kernel may have dropped the pages it could not
// write, so a later sync that succeeds proves nothing about them. Nor is
// corruption, or a failure a statement causes, such as a constraint
// violation, which fails the same way every time. Nor is a database that
// has become read-only, through a remount or a change of permissions,
// which takes an operator to undo.
//
// Nor is a lock another connection holds, or the WAL locking protocol
// failing, which SQLite has already retried for seconds before reporting.
// The store's own connections never wait on each other for a lock: only
// the writer thread writes, the readers never wait on it, and the
// checkpoint steps that race the writer for a lock are deferred by the
// caller instead of failing. Either failure means another process is using
// the store, which no retry makes safe.
pub fn is_recoverable(e: &SqliteError) -> bool {
    let Some(failure) = e.sqlite_error() else {
        return false;
    };
    match failure.code {
        ErrorCode::DiskFull | ErrorCode::OutOfMemory | ErrorCode::CannotOpen => true,
        ErrorCode::SystemIoFailure => !matches!(
            failure.extended_code,
            SQLITE_IOERR_FSYNC
                | SQLITE_IOERR_DIR_FSYNC
                | SQLITE_IOERR_CORRUPTFS
                | SQLITE_IOERR_DATA
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::ffi::{
        Error as SqliteCode, SQLITE_BUSY, SQLITE_CANTOPEN, SQLITE_CONSTRAINT_UNIQUE,
        SQLITE_CORRUPT, SQLITE_FULL, SQLITE_IOERR_WRITE, SQLITE_NOMEM, SQLITE_PROTOCOL,
        SQLITE_READONLY, SQLITE_READONLY_DBMOVED,
    };

    use super::*;

    fn failure(code: i32) -> SqliteError {
        SqliteError::SqliteFailure(SqliteCode::new(code), None)
    }

    // A step that fails in a way that may clear is run again, until it
    // succeeds.
    #[test]
    fn a_recoverable_failure_is_retried_until_it_clears() {
        let mut runs = 0;
        let result = RETRY.run("a step", || {
            runs += 1;
            if runs < 3 {
                Err(failure(SQLITE_FULL))
            } else {
                Ok(runs)
            }
        });
        assert_eq!(result.unwrap(), 3);
    }

    // A step still failing once the budget is spent returns its failure.
    #[test]
    fn a_failure_that_outlasts_the_budget_stands() {
        let mut runs = 0;
        let result = RETRY.run("a step", || -> SqliteResult<()> {
            runs += 1;
            Err(failure(SQLITE_FULL))
        });
        assert_eq!(
            result.unwrap_err().sqlite_error_code(),
            Some(ErrorCode::DiskFull)
        );
        assert!(runs > 1);
    }

    // A step that fails in a way that will not clear is run once.
    #[test]
    fn an_unrecoverable_failure_is_not_retried() {
        let mut runs = 0;
        let result = RETRY.run("a step", || -> SqliteResult<()> {
            runs += 1;
            Err(failure(SQLITE_CORRUPT))
        });
        assert_eq!(
            result.unwrap_err().sqlite_error_code(),
            Some(ErrorCode::DatabaseCorrupt)
        );
        assert_eq!(runs, 1);
    }

    // A full disk, a shortage of memory or file descriptors, and I/O errors
    // may clear; a failed sync, another process's lock or locking, a
    // read-only database, corruption and a failing statement do not.
    #[test]
    fn a_failed_sync_is_unrecoverable() {
        for code in [
            SQLITE_FULL,
            SQLITE_NOMEM,
            SQLITE_CANTOPEN,
            SQLITE_IOERR_WRITE,
        ] {
            assert!(is_recoverable(&failure(code)), "{code}");
        }
        for code in [
            SQLITE_BUSY,
            SQLITE_PROTOCOL,
            SQLITE_READONLY,
            SQLITE_READONLY_DBMOVED,
            SQLITE_IOERR_FSYNC,
            SQLITE_IOERR_DIR_FSYNC,
            SQLITE_CORRUPT,
            SQLITE_CONSTRAINT_UNIQUE,
        ] {
            assert!(!is_recoverable(&failure(code)), "{code}");
        }
        assert!(!is_recoverable(&SqliteError::InvalidQuery));
    }
}
