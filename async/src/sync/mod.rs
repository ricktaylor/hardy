//! Synchronization primitives with platform-appropriate implementations.
//!
//! This module provides synchronization primitives organized by their characteristics:
//!
//! # Top-level Primitives
//!
//! - [`Mutex`] - General-purpose mutex for operations that may block or iterate
//! - [`RwLock`] - General-purpose read-write lock for read-heavy patterns
//!
//! # Submodules
//!
//! - [`spin`] - Spinlock-based primitives for O(1) operations on hot paths
//!
//! # Choosing the Right Primitive
//!
//! | Use Case | Primitive |
//! |----------|-----------|
//! | O(1) ops, hot path, no blocking | [`spin::Mutex`] |
//! | O(1) ops, read-heavy, no blocking | [`spin::RwLock`] |
//! | O(n) iteration, may block | [`Mutex`] |
//! | O(n) iteration, read-heavy | [`RwLock`] |
//!
//! # Platform Implementations
//!
//! - **std**: Wraps `std::sync::Mutex` / `std::sync::RwLock`
//! - **embassy** (future): Wraps `embassy_sync::mutex::Mutex` / `embassy_sync::mutex::Mutex`
//!
//! # Error Handling
//!
//! The std primitives use `trace_expect()` to handle poison errors, which only occur
//! when another thread panicked while holding the lock. This provides a unified
//! interface matching Embassy (which has no poison concept) and spin locks.

#[cfg(feature = "std")]
use std::sync::{MutexGuard, RwLockReadGuard, RwLockWriteGuard};

#[cfg(feature = "std")]
use trace_err::*;

pub mod spin;

/// A general-purpose mutex for operations that may block or iterate.
///
/// Use this when:
/// - Operations may be O(n) (iteration, complex logic)
/// - Blocking, I/O, or syscalls occur while holding lock
/// - RNG calls occur while holding lock
///
/// For O(1) operations on hot paths, use [`spin::Mutex`] instead.
///
/// # Platform Implementations
///
/// - **std**: Wraps `std::sync::Mutex`
/// - **embassy** (future): Wraps `embassy_sync::mutex::Mutex<ThreadModeRawMutex, T>`
///
/// # Poison Handling
///
/// Lock poisoning (from panic in another thread) is handled via `trace_expect()`,
/// which logs and panics. This matches Embassy behavior (no poison concept) and
/// ensures a unified interface.
#[cfg(feature = "std")]
#[derive(Debug, Default)]
pub struct Mutex<T>(std::sync::Mutex<T>);

#[cfg(feature = "std")]
impl<T> Mutex<T> {
    /// Creates a new Mutex containing the given value.
    #[inline]
    pub fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }

    /// Acquires the lock, blocking until it becomes available.
    ///
    /// Returns a guard that releases the lock when dropped.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned (another thread panicked while holding the lock).
    #[inline]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().trace_expect("Mutex poisoned")
    }

    /// Attempts to acquire the lock without blocking.
    ///
    /// Returns `Some(guard)` if the lock was acquired, `None` if it was already held.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("Mutex poisoned")
            }
        }
    }

    /// Consumes the mutex and returns the inner value.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0.into_inner().trace_expect("Mutex poisoned")
    }

    /// Returns a mutable reference to the inner value.
    ///
    /// This is safe because it requires exclusive access to the mutex.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().trace_expect("Mutex poisoned")
    }
}

/// A general-purpose read-write lock for read-heavy operations that may block or iterate.
///
/// Use this when:
/// - Read operations significantly outnumber write operations
/// - Operations may be O(n) (iteration, complex logic)
/// - Blocking, I/O, or syscalls occur while holding lock
///
/// For O(1) operations on hot paths, use [`spin::RwLock`] instead.
///
/// # Platform Implementations
///
/// - **std**: Wraps `std::sync::RwLock`
/// - **embassy** (future): Degrades to `Mutex` (Embassy is single-core)
///
/// # Poison Handling
///
/// Lock poisoning (from panic in another thread) is handled via `trace_expect()`,
/// which logs and panics. This matches Embassy behavior (no poison concept) and
/// ensures a unified interface.
#[cfg(feature = "std")]
#[derive(Debug, Default)]
pub struct RwLock<T>(std::sync::RwLock<T>);

#[cfg(feature = "std")]
impl<T> RwLock<T> {
    /// Creates a new RwLock containing the given value.
    #[inline]
    pub fn new(value: T) -> Self {
        Self(std::sync::RwLock::new(value))
    }

    /// Acquires a read lock, blocking until it becomes available.
    ///
    /// Multiple readers can hold the lock simultaneously.
    /// Returns a guard that releases the lock when dropped.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[inline]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.0.read().trace_expect("RwLock poisoned")
    }

    /// Acquires a write lock, blocking until it becomes available.
    ///
    /// Only one writer can hold the lock, and no readers can hold it.
    /// Returns a guard that releases the lock when dropped.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[inline]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.0.write().trace_expect("RwLock poisoned")
    }

    /// Attempts to acquire a read lock without blocking.
    ///
    /// Returns `Some(guard)` if the lock was acquired, `None` if a writer holds it.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[inline]
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        match self.0.try_read() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("RwLock poisoned")
            }
        }
    }

    /// Attempts to acquire a write lock without blocking.
    ///
    /// Returns `Some(guard)` if the lock was acquired, `None` if it was already held.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[inline]
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        match self.0.try_write() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("RwLock poisoned")
            }
        }
    }

    /// Consumes the lock and returns the inner value.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0.into_inner().trace_expect("RwLock poisoned")
    }

    /// Returns a mutable reference to the inner value.
    ///
    /// This is safe because it requires exclusive access to the lock.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().trace_expect("RwLock poisoned")
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn mutex_basic() {
        let mutex = Mutex::new(42);
        assert_eq!(*mutex.lock(), 42);
        *mutex.lock() = 100;
        assert_eq!(*mutex.lock(), 100);
    }

    #[test]
    fn mutex_hashmap() {
        let cache: Mutex<HashMap<String, i32>> = Mutex::new(HashMap::new());
        cache.lock().insert("key".to_string(), 42);
        assert_eq!(cache.lock().get("key"), Some(&42));
    }

    #[test]
    fn rwlock_basic() {
        let lock = RwLock::new(42);

        // Multiple readers
        {
            let r1 = lock.read();
            let r2 = lock.read();
            assert_eq!(*r1, 42);
            assert_eq!(*r2, 42);
        }

        // Writer
        {
            *lock.write() = 100;
        }

        assert_eq!(*lock.read(), 100);
    }

    #[test]
    fn mutex_try_lock() {
        let mutex = Mutex::new(42);
        let guard = mutex.lock();
        assert!(mutex.try_lock().is_none());
        drop(guard);
        assert!(mutex.try_lock().is_some());
    }

    #[test]
    fn rwlock_try_locks() {
        let lock = RwLock::new(42);

        // Can get multiple read locks
        let r1 = lock.try_read();
        assert!(r1.is_some());
        let r2 = lock.try_read();
        assert!(r2.is_some());

        // Can't get write lock while readers exist
        assert!(lock.try_write().is_none());

        drop(r1);
        drop(r2);

        // Now can get write lock
        let w = lock.try_write();
        assert!(w.is_some());

        // Can't get read lock while writer exists
        assert!(lock.try_read().is_none());
    }

    #[test]
    fn mutex_into_inner() {
        let mutex = Mutex::new(42);
        assert_eq!(mutex.into_inner(), 42);
    }

    #[test]
    fn rwlock_into_inner() {
        let lock = RwLock::new(42);
        assert_eq!(lock.into_inner(), 42);
    }

    #[test]
    fn mutex_get_mut() {
        let mut mutex = Mutex::new(42);
        *mutex.get_mut() = 100;
        assert_eq!(*mutex.lock(), 100);
    }

    #[test]
    fn rwlock_get_mut() {
        let mut lock = RwLock::new(42);
        *lock.get_mut() = 100;
        assert_eq!(*lock.read(), 100);
    }
}
