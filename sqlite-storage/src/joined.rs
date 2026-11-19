//! A thread that is joined when its handle drops.

use std::thread::JoinHandle;

// Joins its thread on drop, so that whatever the thread owns has closed by
// the time its owner has.
pub struct Joined(Option<JoinHandle<()>>);

impl From<JoinHandle<()>> for Joined {
    fn from(thread: JoinHandle<()>) -> Self {
        Self(Some(thread))
    }
}

impl Drop for Joined {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            // A thread that panicked has nothing left to hand back.
            _ = thread.join();
        }
    }
}
