//! The store's worker threads: joined when their handle drops, and fatal to
//! the process if they panic.

use std::{
    io,
    process::abort,
    thread::{Builder, JoinHandle, panicking},
};

use tracing::error;

// A thread the store cannot run without. It is joined when this drops, so
// that whatever it owns has closed by the time its owner has.
pub struct Worker(Option<JoinHandle<()>>);

impl Worker {
    // Spawns `work` on a thread called `name`. If `work` panics the process
    // aborts, as it does when a task on one of `hardy-async`'s task pools
    // panics: a store that cannot write or checkpoint must not leave the
    // rest of the process running against it.
    pub fn spawn(name: &'static str, work: impl FnOnce() + Send + 'static) -> io::Result<Self> {
        Builder::new()
            .name(name.into())
            .spawn(move || {
                let _fatal = AbortOnPanic(name);
                work();
            })
            .map(|thread| Self(Some(thread)))
    }
}

impl Drop for Worker {
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
// the panic hook before the unwind reaches this.
pub struct AbortOnPanic(pub &'static str);

impl Drop for AbortOnPanic {
    fn drop(&mut self) {
        if panicking() {
            error!("{} panicked, aborting process", self.0);
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
    };

    use super::*;

    // Set in a child process that is to run a scenario for real.
    const SCENARIO: &str = "HARDY_SQLITE_ABORT_SCENARIO";

    // Runs `scenario` in a child process and asserts that the child was
    // aborted. A scenario ends in a panic that must abort the process, so
    // it cannot run inside the test process: the abort would take every
    // other test down with it.
    //
    // The child is this test binary, re-run for the one test `test` (its
    // full path), with `SCENARIO` set. Called in the child, this runs the
    // scenario, and failing to abort is itself a panic, which exits with a
    // failure status that is not an abort.
    pub fn assert_aborts(test: &str, scenario: impl FnOnce()) {
        if var_os(SCENARIO).is_some() {
            scenario();
            panic!("the scenario returned without aborting the process");
        }
        let child = Command::new(current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(SCENARIO, "1")
            .output()
            .unwrap();
        assert!(
            aborted(child.status),
            "the child was not aborted: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[cfg(unix)]
    fn aborted(status: ExitStatus) -> bool {
        use std::os::unix::process::ExitStatusExt;

        // SIGABRT
        status.signal() == Some(6)
    }

    #[cfg(not(unix))]
    fn aborted(status: ExitStatus) -> bool {
        // An abort is neither a clean exit nor a failed test, which exits
        // with 101.
        !status.success() && status.code() != Some(101)
    }

    // A panic on a worker thread aborts the process.
    #[test]
    fn a_panicking_worker_aborts_the_process() {
        assert_aborts(
            "thread::tests::a_panicking_worker_aborts_the_process",
            || {
                let worker = Worker::spawn("test-worker", || panic!("the worker's work failed"));
                // Joins the thread, which the abort pre-empts.
                drop(worker.unwrap());
            },
        );
    }

    // A worker whose work returns is joined, and nothing aborts.
    #[test]
    fn a_worker_that_returns_is_joined() {
        let (done, finished) = channel();
        let worker = Worker::spawn("test-worker", move || done.send(()).unwrap()).unwrap();
        drop(worker);
        // The join has completed, so the work has run to its end.
        finished.try_recv().unwrap();
    }
}
