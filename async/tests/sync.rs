//! Lock-contract tests for the `sync` primitives, exercised against both the
//! general-purpose (`std`) and spinlock implementations.

macro_rules! lock_contract_tests {
    ($mutex:ty, $rwlock:ty) => {
        #[test]
        fn mutex_basic() {
            let mutex = <$mutex>::new(42);
            assert_eq!(*mutex.lock(), 42);
            *mutex.lock() = 100;
            assert_eq!(*mutex.lock(), 100);
        }

        #[test]
        fn rwlock_basic() {
            let lock = <$rwlock>::new(42);

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
            let mutex = <$mutex>::new(42);
            let guard = mutex.lock();
            assert!(mutex.try_lock().is_none());
            drop(guard);
            assert!(mutex.try_lock().is_some());
        }

        #[test]
        fn rwlock_try_locks() {
            let lock = <$rwlock>::new(42);

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
            let mutex = <$mutex>::new(42);
            assert_eq!(mutex.into_inner(), 42);
        }

        #[test]
        fn rwlock_into_inner() {
            let lock = <$rwlock>::new(42);
            assert_eq!(lock.into_inner(), 42);
        }

        #[test]
        fn mutex_get_mut() {
            let mut mutex = <$mutex>::new(42);
            *mutex.get_mut() = 100;
            assert_eq!(*mutex.lock(), 100);
        }

        #[test]
        fn rwlock_get_mut() {
            let mut lock = <$rwlock>::new(42);
            *lock.get_mut() = 100;
            assert_eq!(*lock.read(), 100);
        }
    };
}

#[cfg(feature = "std")]
mod general {
    use hardy_async::sync::{Mutex, RwLock};

    lock_contract_tests!(Mutex<i32>, RwLock<i32>);
}

mod spin {
    use hardy_async::sync::spin::{Mutex, Once, RwLock};

    lock_contract_tests!(Mutex<i32>, RwLock<i32>);

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
    fn once_wait_returns_immediately_when_initialized() {
        let once: Once<i32> = Once::new();
        once.call_once(|| 42);
        assert_eq!(*once.wait(), 42);
    }

    /// The barrier lines both threads up so `wait()` genuinely races the
    /// initialisation. Whichever side wins, `wait()` must spin until the
    /// value exists and return it, never panic or return early.
    #[test]
    fn once_wait_returns_value_initialized_by_another_thread() {
        let once = std::sync::Arc::new(Once::new());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let waiter = {
            let once = once.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                *once.wait()
            })
        };

        barrier.wait();
        once.call_once(|| 42);
        assert_eq!(waiter.join().unwrap(), 42);
    }
}
