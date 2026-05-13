// Re-export tokio sync primitives through the runtime facade.
pub use std::sync::Arc;
pub use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc, oneshot};

/// Priority-inheritance mutex for real-time builds (epics-base 7-E
/// `5a8b6e41` "epicsMutex priority inheritance"). On Linux with the
/// `linux-rt` Cargo feature enabled this wraps a `pthread_mutex_t`
/// configured with `PTHREAD_PRIO_INHERIT` so a low-priority holder
/// inherits the priority of the highest-priority waiter — preventing
/// the classic priority-inversion deadlock that bit C epicsMutex on
/// PREEMPT_RT kernels.
///
/// On non-RT builds (the default) and on non-Linux platforms this is
/// a transparent type alias for [`parking_lot::Mutex`], which is
/// already non-poisoning, smaller, and faster than `std::sync::Mutex`
/// for typical IOC workloads. Callers see the same API surface — the
/// PI variant only matters when the OS scheduler can preempt a thread
/// holding the lock, i.e. when the runtime is configured for
/// real-time scheduling.
///
/// Note: tokio's async `Mutex` is unaffected — async tasks don't have
/// OS-level priorities to invert. `PriorityInheritanceMutex` is for
/// the rare blocking-Sync code paths (device-support callbacks, some
/// tracing sinks) where we hold a mutex while running with
/// SCHED_FIFO / SCHED_RR.
#[cfg(all(target_os = "linux", feature = "linux-rt"))]
pub type PriorityInheritanceMutex<T> = pi_mutex::PiMutex<T>;

/// Non-RT fallback — uses `parking_lot::Mutex` for the common case.
/// PI semantics are not needed because the scheduler does not
/// preempt the lock holder by priority.
#[cfg(not(all(target_os = "linux", feature = "linux-rt")))]
pub type PriorityInheritanceMutex<T> = parking_lot::Mutex<T>;

/// Diagnostic — `true` when the PI variant is active in this build.
pub fn is_pi_mutex_active() -> bool {
    cfg!(all(target_os = "linux", feature = "linux-rt"))
}

#[cfg(all(target_os = "linux", feature = "linux-rt"))]
mod pi_mutex {
    //! Linux-only `pthread_mutex_t` with `PTHREAD_PRIO_INHERIT`.

    use std::cell::UnsafeCell;
    use std::ops::{Deref, DerefMut};

    pub struct PiMutex<T> {
        inner: UnsafeCell<libc::pthread_mutex_t>,
        data: UnsafeCell<T>,
    }

    unsafe impl<T: Send> Send for PiMutex<T> {}
    unsafe impl<T: Send> Sync for PiMutex<T> {}

    impl<T> PiMutex<T> {
        pub fn new(value: T) -> Self {
            let mutex = UnsafeCell::new(unsafe { std::mem::zeroed() });
            unsafe {
                let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                let r = libc::pthread_mutexattr_init(&mut attr);
                assert_eq!(r, 0, "pthread_mutexattr_init failed");
                let r = libc::pthread_mutexattr_setprotocol(&mut attr, libc::PTHREAD_PRIO_INHERIT);
                assert_eq!(r, 0, "pthread_mutexattr_setprotocol PRIO_INHERIT failed");
                let r = libc::pthread_mutex_init(mutex.get(), &attr);
                assert_eq!(r, 0, "pthread_mutex_init failed");
                libc::pthread_mutexattr_destroy(&mut attr);
            }
            Self {
                inner: mutex,
                data: UnsafeCell::new(value),
            }
        }

        pub fn lock(&self) -> PiMutexGuard<'_, T> {
            unsafe {
                let r = libc::pthread_mutex_lock(self.inner.get());
                assert_eq!(r, 0, "pthread_mutex_lock failed");
            }
            PiMutexGuard { mutex: self }
        }
    }

    impl<T> Drop for PiMutex<T> {
        fn drop(&mut self) {
            unsafe {
                libc::pthread_mutex_destroy(self.inner.get());
            }
        }
    }

    pub struct PiMutexGuard<'a, T> {
        mutex: &'a PiMutex<T>,
    }

    impl<T> Deref for PiMutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            unsafe { &*self.mutex.data.get() }
        }
    }

    impl<T> DerefMut for PiMutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            unsafe { &mut *self.mutex.data.get() }
        }
    }

    impl<T> Drop for PiMutexGuard<'_, T> {
        fn drop(&mut self) {
            unsafe {
                libc::pthread_mutex_unlock(self.mutex.inner.get());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PI mutex API surface — works on both feature gates. Build path:
    /// confirms the type is constructible and the lock guard derefs.
    #[test]
    fn pi_mutex_lock_unlock() {
        let m: PriorityInheritanceMutex<i32> = PriorityInheritanceMutex::new(42);
        {
            let g = m.lock();
            assert_eq!(*g, 42);
        }
        // re-lock after drop
        let g = m.lock();
        assert_eq!(*g, 42);
    }

    #[test]
    fn is_pi_mutex_active_returns_bool() {
        let _b = is_pi_mutex_active();
    }
}
