//! Spinlock-based synchronization primitives for O(1) operations.
//!
//! This module provides lightweight spinlocks for hot paths where all operations
//! are O(1) and no blocking occurs. Use these only when:
//!
//! - All operations are O(1) (HashMap lookup/insert, state checks)
//! - No blocking, I/O, or syscalls while holding lock
//! - Lock is released before any async operations
//! - No nested lock acquisition
//!
//! # Platform Implementations
//!
//! - **std**: Wraps `spin::Mutex` / `spin::RwLock`
//! - **embassy** (future): Wraps `embassy_sync::mutex::Mutex<CriticalSectionRawMutex, T>`
//!
//! # When NOT to use these primitives
//!
//! - O(n) iteration while holding lock (use `std::sync::Mutex` instead)
//! - RNG, I/O, or any syscalls while holding lock
//! - Holding lock across await points
//! - Nested lock acquisition (can cause priority inversion)
//!
//! # Example
//!
//! ```
//! use hardy_async::sync::spin::Mutex;
//! use std::collections::HashMap;
//!
//! // Good: O(1) HashMap operations
//! let cache: Mutex<HashMap<String, i32>> = Mutex::new(HashMap::new());
//!
//! // Insert is O(1)
//! cache.lock().insert("key".to_string(), 42);
//!
//! // Lookup is O(1)
//! let value = cache.lock().get("key").cloned();
//! ```

// Re-export guard types from spin crate
pub use spin::MutexGuard;
pub use spin::RwLockReadGuard;
pub use spin::RwLockWriteGuard;

/// A spinlock-based one-time initialization cell for O(1) operations.
///
/// This is a thin wrapper around `spin::once::Once` that provides a consistent API
/// for potential future Embassy support.
///
/// # Platform Implementations
///
/// - **std**: Uses `spin::once::Once` (busy-wait spinlock)
/// - **embassy** (future): Will use appropriate single-init primitive
///
/// # Usage Guidelines
///
/// Use this for lazy one-time initialization where:
/// 1. Initialization is O(1) or happens once at startup
/// 2. Access after initialization is O(1)
/// 3. No blocking, I/O, or syscalls during initialization
///
/// # Example
///
/// ```
/// use hardy_async::sync::spin::Once;
///
/// struct Config {
///     value: i32,
/// }
///
/// let config: Once<Config> = Once::new();
///
/// // Initialize once
/// config.call_once(|| Config { value: 42 });
///
/// // Subsequent calls return the same value
/// assert_eq!(config.get().unwrap().value, 42);
/// ```
pub struct Once<T>(spin::once::Once<T>);

impl<T> Once<T> {
    /// Creates a new uninitialized `Once`.
    #[inline]
    pub const fn new() -> Self {
        Self(spin::once::Once::new())
    }

    /// Returns a reference to the inner value if initialized.
    ///
    /// Returns `None` if the cell has not been initialized yet.
    #[inline]
    pub fn get(&self) -> Option<&T> {
        self.0.get()
    }

    /// Initializes the cell with the result of `f` if not already initialized.
    ///
    /// If the cell is already initialized, returns the existing value.
    /// If multiple threads call this concurrently, only one will run `f`,
    /// and others will spin until initialization completes.
    ///
    /// Returns a reference to the initialized value.
    #[inline]
    pub fn call_once<F: FnOnce() -> T>(&self, f: F) -> &T {
        self.0.call_once(f)
    }

    /// Returns `true` if the cell has been initialized.
    #[inline]
    pub fn is_completed(&self) -> bool {
        self.0.is_completed()
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for Once<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.get() {
            Some(v) => f.debug_tuple("Once").field(v).finish(),
            None => f.write_str("Once(<uninitialized>)"),
        }
    }
}

/// A spinlock-based mutex for O(1) operations.
///
/// This is a thin wrapper around `spin::Mutex` that provides a consistent API
/// for potential future Embassy support.
///
/// # Platform Implementations
///
/// - **std**: Uses `spin::Mutex` (busy-wait spinlock)
/// - **embassy** (future): Will use `embassy_sync::mutex::Mutex<CriticalSectionRawMutex, T>`
///
/// # Usage Guidelines
///
/// Only use this when ALL of these conditions are met:
/// 1. All operations while holding the lock are O(1)
/// 2. No blocking, I/O, RNG, or syscalls while holding the lock
/// 3. Lock is released before any async operations
/// 4. No nested lock acquisition
///
/// For operations that may iterate (O(n)) or block, use `std::sync::Mutex` instead.
#[derive(Debug, Default)]
pub struct Mutex<T>(spin::Mutex<T>);

impl<T> Mutex<T> {
    /// Creates a new Mutex containing the given value.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self(spin::Mutex::new(value))
    }

    /// Acquires the lock, spinning until it becomes available.
    ///
    /// Returns a guard that releases the lock when dropped.
    #[inline]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock()
    }

    /// Attempts to acquire the lock without blocking.
    ///
    /// Returns `Some(guard)` if the lock was acquired, `None` if it was already held.
    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.0.try_lock()
    }

    /// Consumes the mutex and returns the inner value.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0.into_inner()
    }

    /// Returns a mutable reference to the inner value.
    ///
    /// This is safe because it requires exclusive access to the mutex.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

/// A spinlock-based read-write lock for O(1) operations with read-heavy access patterns.
///
/// This is a thin wrapper around `spin::RwLock` that provides a consistent API
/// for potential future Embassy support.
///
/// # Platform Implementations
///
/// - **std**: Uses `spin::RwLock` (busy-wait spinlock)
/// - **embassy** (future): Will degrade to `Mutex` (Embassy is single-core,
///   so RwLock provides no benefit)
///
/// # Usage Guidelines
///
/// Only use this when ALL of these conditions are met:
/// 1. All operations while holding the lock are O(1)
/// 2. Read operations significantly outnumber write operations
/// 3. No blocking, I/O, RNG, or syscalls while holding the lock
/// 4. Lock is released before any async operations
/// 5. No nested lock acquisition
///
/// For operations that may iterate (O(n)) or block, use `std::sync::RwLock` instead.
#[derive(Debug, Default)]
pub struct RwLock<T>(spin::RwLock<T>);

impl<T> RwLock<T> {
    /// Creates a new RwLock containing the given value.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self(spin::RwLock::new(value))
    }

    /// Acquires a read lock, spinning until it becomes available.
    ///
    /// Multiple readers can hold the lock simultaneously.
    /// Returns a guard that releases the lock when dropped.
    #[inline]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.0.read()
    }

    /// Acquires a write lock, spinning until it becomes available.
    ///
    /// Only one writer can hold the lock, and no readers can hold it.
    /// Returns a guard that releases the lock when dropped.
    #[inline]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.0.write()
    }

    /// Attempts to acquire a read lock without blocking.
    ///
    /// Returns `Some(guard)` if the lock was acquired, `None` if a writer holds it.
    #[inline]
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        self.0.try_read()
    }

    /// Attempts to acquire a write lock without blocking.
    ///
    /// Returns `Some(guard)` if the lock was acquired, `None` if it was already held.
    #[inline]
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.0.try_write()
    }

    /// Consumes the lock and returns the inner value.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0.into_inner()
    }

    /// Returns a mutable reference to the inner value.
    ///
    /// This is safe because it requires exclusive access to the lock.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn once_basic() {
        let once: Once<i32> = Once::new();
        assert!(once.get().is_none());
        assert!(!once.is_completed());

        let val = once.call_once(|| 42);
        assert_eq!(*val, 42);
        assert!(once.is_completed());
        assert_eq!(once.get(), Some(&42));
    }

    #[test]
    fn once_multiple_calls() {
        let once: Once<i32> = Once::new();

        let val1 = once.call_once(|| 42);
        let val2 = once.call_once(|| 100); // Should not run, returns existing

        assert_eq!(*val1, 42);
        assert_eq!(*val2, 42);
    }

    #[test]
    fn once_default() {
        let once: Once<i32> = Once::default();
        assert!(once.get().is_none());
    }

    #[test]
    fn once_debug() {
        let once: Once<i32> = Once::new();
        assert!(format!("{:?}", once).contains("uninitialized"));

        once.call_once(|| 42);
        assert!(format!("{:?}", once).contains("42"));
    }

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
}
