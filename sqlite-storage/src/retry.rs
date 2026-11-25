//! Retrying a store step whose failure may clear by itself.
//!
//! The store's threads keep trying while a failure may clear: a lock held
//! by another process, a full disk, a shortage of memory, or an I/O error
//! other than a failed sync. Past the retry policy's last retry, and at
//! once for any other failure, the step's error stands, and the failure is
//! treated as one that will not clear.

use std::{thread::sleep, time::Duration};

use rusqlite::{
    Error as SqliteError, ErrorCode, Result as SqliteResult,
    ffi::{
        SQLITE_IOERR_CORRUPTFS, SQLITE_IOERR_DATA, SQLITE_IOERR_DIR_FSYNC, SQLITE_IOERR_FSYNC,
        SQLITE_READONLY_DBMOVED,
    },
};
use tracing::{debug, error, warn};

// How a store thread retries a step that failed in a way that may clear.
#[derive(Clone, Copy)]
pub struct Retry {
    // The pause before the first retry. Each later pause doubles, up to
    // `max_pause`.
    first_pause: Duration,
    max_pause: Duration,
    // How many times the step is retried before its failure stands.
    retries: u32,
}

// Pauses of 10 ms doubling to 5 s: twenty retries span about a minute.
#[cfg(not(test))]
pub const RETRY: Retry = Retry {
    first_pause: Duration::from_millis(10),
    max_pause: Duration::from_secs(5),
    retries: 20,
};

// Tests retry without pausing, and give up soon.
#[cfg(test)]
pub const RETRY: Retry = Retry {
    first_pause: Duration::ZERO,
    max_pause: Duration::ZERO,
    retries: 2,
};

impl Retry {
    // Runs `step` until it succeeds, fails in a way `retryable` rejects, or
    // has used up its retries, and returns its last result. `what` names
    // the step in the log.
    pub fn run<T>(
        self,
        what: &str,
        retryable: impl Fn(&SqliteError) -> bool,
        mut step: impl FnMut() -> SqliteResult<T>,
    ) -> SqliteResult<T> {
        let mut pause = self.first_pause;
        for retry in 1..=self.retries {
            match step() {
                Err(e) if retryable(&e) => {
                    // Losing a race for a lock is routine.
                    if is_busy(&e) {
                        debug!("{what} busy, retry {retry} in {pause:?}: {e}");
                    } else {
                        warn!("{what} failed, retry {retry} in {pause:?}: {e}");
                    }
                    sleep(pause);
                    pause = (pause * 2).min(self.max_pause);
                }
                result => return result,
            }
        }
        step().inspect_err(|e| {
            if retryable(e) {
                error!("{what} still failing after {} retries: {e}", self.retries);
            }
        })
    }
}

// Whether a step only lost a race for one of the database's locks.
pub fn is_busy(e: &SqliteError) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
}

// Whether a failure may clear by itself, so that the step that hit it is
// worth running again.
//
// A failed sync is not: the kernel may have dropped the pages it could not
// write, so a later sync that succeeds proves nothing about them. Nor is
// corruption, a database file that has been moved or deleted, or a failure
// a statement causes, such as a constraint violation, which fails the same
// way every time.
pub fn is_recoverable(e: &SqliteError) -> bool {
    let Some(failure) = e.sqlite_error() else {
        return false;
    };
    match failure.code {
        ErrorCode::DatabaseBusy
        | ErrorCode::DatabaseLocked
        | ErrorCode::DiskFull
        | ErrorCode::OutOfMemory
        | ErrorCode::CannotOpen
        | ErrorCode::FileLockingProtocolFailed => true,
        ErrorCode::SystemIoFailure => !matches!(
            failure.extended_code,
            SQLITE_IOERR_FSYNC
                | SQLITE_IOERR_DIR_FSYNC
                | SQLITE_IOERR_CORRUPTFS
                | SQLITE_IOERR_DATA
        ),
        ErrorCode::ReadOnly => failure.extended_code != SQLITE_READONLY_DBMOVED,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::ffi::{
        Error as SqliteCode, SQLITE_BUSY, SQLITE_CONSTRAINT_UNIQUE, SQLITE_CORRUPT, SQLITE_FULL,
        SQLITE_IOERR_WRITE,
    };

    use super::*;

    const RETRIES: Retry = Retry {
        first_pause: Duration::ZERO,
        max_pause: Duration::ZERO,
        retries: 3,
    };

    fn failure(code: i32) -> SqliteError {
        SqliteError::SqliteFailure(SqliteCode::new(code), None)
    }

    // A step that fails in a way that may clear is run again, until it
    // succeeds.
    #[test]
    fn a_recoverable_failure_is_retried_until_it_clears() {
        let mut runs = 0;
        let result = RETRIES.run("a step", is_recoverable, || {
            runs += 1;
            if runs < 3 {
                Err(failure(SQLITE_FULL))
            } else {
                Ok(runs)
            }
        });
        assert_eq!(result.unwrap(), 3);
    }

    // A step still failing after its last retry returns its failure.
    #[test]
    fn a_failure_that_outlasts_the_retries_stands() {
        let mut runs = 0;
        let result = RETRIES.run("a step", is_recoverable, || -> SqliteResult<()> {
            runs += 1;
            Err(failure(SQLITE_BUSY))
        });
        assert_eq!(
            result.unwrap_err().sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy)
        );
        assert_eq!(runs, 4);
    }

    // A step that fails in a way that will not clear is run once.
    #[test]
    fn an_unrecoverable_failure_is_not_retried() {
        let mut runs = 0;
        let result = RETRIES.run("a step", is_recoverable, || -> SqliteResult<()> {
            runs += 1;
            Err(failure(SQLITE_CORRUPT))
        });
        assert_eq!(
            result.unwrap_err().sqlite_error_code(),
            Some(ErrorCode::DatabaseCorrupt)
        );
        assert_eq!(runs, 1);
    }

    // Locks, a full disk and I/O errors may clear; a failed sync,
    // corruption and a failing statement do not.
    #[test]
    fn a_failed_sync_is_unrecoverable() {
        for code in [SQLITE_BUSY, SQLITE_FULL, SQLITE_IOERR_WRITE] {
            assert!(is_recoverable(&failure(code)), "{code}");
        }
        for code in [
            SQLITE_IOERR_FSYNC,
            SQLITE_IOERR_DIR_FSYNC,
            SQLITE_CORRUPT,
            SQLITE_CONSTRAINT_UNIQUE,
            SQLITE_READONLY_DBMOVED,
        ] {
            assert!(!is_recoverable(&failure(code)), "{code}");
        }
        assert!(!is_recoverable(&SqliteError::InvalidQuery));
    }
}
