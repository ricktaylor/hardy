//! The store's worker threads: joined when their handle drops, and fatal to
//! the process if they panic.

use std::{
    io,
    process::abort,
    thread::{Builder, JoinHandle, panicking},
};

use tracing::error;

// A thread the store cannot run without, joined when this drops, so that
// whatever it owns has closed by the time its owner has.
pub struct JoinOnDrop(Option<JoinHandle<()>>);

impl JoinOnDrop {
    // Spawns `work` on a thread called `name`. If `work` panics the process
    // aborts, as it does when a task on one of `hardy-async`'s task pools
    // panics: a store that cannot write or checkpoint must not leave the
    // rest of the process running against it.
    pub fn spawn(name: &'static str, work: impl FnOnce() + Send + 'static) -> io::Result<Self> {
        Builder::new()
            .name(name.into())
            .spawn(move || {
                let _guard = AbortOnPanicGuard::new(name);
                work();
            })
            .map(|thread| Self(Some(thread)))
    }
}

impl Drop for JoinOnDrop {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            // A panicking thread aborts the process before it can be
            // joined, so the join finds nothing to report.
            _ = thread.join();
        }
    }
}

// Aborts the process if dropped during a panic: held for the length of the
// work it guards. A guard, not `catch_unwind`, so the work need not be
// unwind-safe, and the panic's own message and backtrace are printed by
// the panic hook before the unwind reaches this. `std::panic::abort_unwind`
// does the same but is not stable, and would not name the thread.
//
// The abort comes when the unwind reaches the guard, after everything the
// work owns has been dropped, so for that interval the work's resources
// are released as on any unwind.
pub struct AbortOnPanicGuard {
    // What the guard covers, for the log line.
    name: &'static str,
}

impl AbortOnPanicGuard {
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

impl Drop for AbortOnPanicGuard {
    fn drop(&mut self) {
        if panicking() {
            error!("{} panicked, aborting process", self.name);
            abort()
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::{
        env::{current_exe, var_os},
        process::{Command, ExitStatus},
        sync::mpsc::channel,
        thread::park_timeout,
        time::{Duration, Instant},
    };

    use tempfile::tempdir;

    use super::*;

    // Set in a child process that is to run a scenario for real.
    const SCENARIO: &str = "HARDY_SQLITE_ABORT_SCENARIO";

    // The full path of the test named `$name`, in the module it is used in.
    macro_rules! test_path {
        ($name:ident) => {
            concat!(module_path!(), "::", stringify!($name))
        };
    }
    pub(crate) use test_path;

    // Runs `scenario` in a child process and asserts that the child was
    // aborted. A scenario ends in a panic that must abort the process, so
    // it cannot run inside the test process: the abort would take every
    // other test down with it.
    //
    // The child is this test binary, re-run for the one test `test`, with
    // `SCENARIO` set. `test` is the test's path as `module_path!()` gives
    // it, crate name included, as `test_path!` builds it. Called in the child, this runs the
    // scenario, and failing to abort is itself a panic, which exits with a
    // failure status that is not an abort.
    //
    // An aborted child runs no destructors, so anything it creates outlives
    // it. It runs in a scratch directory that the parent removes: the
    // scenario's files go there, by way of `tempdir`, and so does a core
    // dump on a host that writes them to the working directory.
    pub fn assert_aborts(test: &str, scenario: impl FnOnce()) {
        if var_os(SCENARIO).is_some() {
            scenario();
            panic!("the scenario returned without aborting the process");
        }
        let scratch = tempdir().unwrap();
        // The test harness names a test by its path within the crate.
        let (_crate, test) = test.split_once("::").unwrap();
        let child = Command::new(current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(SCENARIO, "1")
            // Where `tempdir` puts its directories, on each platform.
            .env("TMPDIR", scratch.path())
            .env("TMP", scratch.path())
            .env("TEMP", scratch.path())
            .current_dir(scratch.path())
            .output()
            .unwrap();
        assert!(
            is_aborted(child.status),
            "the child was not aborted: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
    }

    // Parks this thread while another aborts the process. Returns only if
    // nothing has after 30 seconds, so a scenario that fails to abort fails
    // its test: the timeout only bounds a regression. The loop rides out
    // the spurious wake-ups `park_timeout` is allowed.
    pub fn wait_for_abort() {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            park_timeout(left);
        }
    }

    #[cfg(unix)]
    fn is_aborted(status: ExitStatus) -> bool {
        use std::os::unix::process::ExitStatusExt;

        // SIGABRT
        status.signal() == Some(6)
    }

    #[cfg(not(unix))]
    fn is_aborted(status: ExitStatus) -> bool {
        // An abort is neither a clean exit nor a failed test, which exits
        // with 101.
        !status.success() && status.code() != Some(101)
    }

    // A panic on a worker thread aborts the process.
    #[test]
    fn a_panicking_worker_aborts_the_process() {
        assert_aborts(test_path!(a_panicking_worker_aborts_the_process), || {
            let worker = JoinOnDrop::spawn("test-worker", || panic!("the worker's work failed"));
            // Joins the thread, which the abort pre-empts.
            drop(worker.unwrap());
        });
    }

    // A worker whose work returns is joined, and nothing aborts.
    #[test]
    fn a_worker_that_returns_is_joined() {
        let (done, finished) = channel();
        let worker = JoinOnDrop::spawn("test-worker", move || done.send(()).unwrap()).unwrap();
        drop(worker);
        // The join has completed, so the work has run to its end.
        finished.try_recv().unwrap();
    }
}
