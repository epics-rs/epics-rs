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
/// RTEMS gets the same `pthread_mutex_t` construction on its own arm
/// below, unconditionally. On every other target — including a default
/// (non-`linux-rt`) Linux build — this is a transparent type alias for
/// [`parking_lot::Mutex`], which is
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

/// The RTEMS arm, and it is **not** behind a Cargo feature.
///
/// `linux-rt` exists because a desktop Linux build must opt in: the request
/// can fail on a box without `CAP_SYS_NICE`, and a runaway RT band on a box
/// that grants it wedges a developer's machine. Neither failure mode exists on
/// RTEMS, which is the same reasoning that makes
/// [`crate::runtime::task::DEFAULT_POLICY`] `AllowRealtime` there
/// (`task.rs:591-603`) — the target has no way to set an env var and no
/// privilege gate to fail.
///
/// The parity target is a POSIX PI mutex, not an RTEMS classic-API semaphore:
/// base-on-RTEMS-6 compiles the POSIX arm (`configure/toolchain.c:31-35`
/// selects `OS_API = posix` for `__RTEMS_MAJOR__ >= 5`, and
/// `os/RTEMS-posix/osdMutex.c:8` is a single `#include "../posix/osdMutex.c"`).
/// So this is the same construction on the same API as C — including C's
/// probe-and-fall-back, see `pi_mutex::protocol` below.
#[cfg(target_os = "rtems")]
pub type PriorityInheritanceMutex<T> = pi_mutex::PiMutex<T>;

/// Non-RT fallback — uses `parking_lot::Mutex` for the common case.
/// PI semantics are not needed because the scheduler does not
/// preempt the lock holder by priority.
#[cfg(not(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems")))]
pub type PriorityInheritanceMutex<T> = parking_lot::Mutex<T>;

/// The guard [`PriorityInheritanceMutex::lock`] hands out, nameable so a
/// caller can *store* one — the per-record write gate
/// (`server::database::record_lock`) holds a `'static` guard in a struct
/// field rather than a local.
///
/// `!Send` on every arm, and on the PI arms that is a correctness
/// requirement rather than a lint: POSIX requires a mutex to be unlocked by
/// the thread that locked it, so a guard that could migrate between threads
/// would eventually call `pthread_mutex_unlock` from a non-owner. It is also
/// what makes "no `.await` inside a lock window" a build error at every
/// spawn site instead of a review convention.
#[cfg(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems"))]
pub type PriorityInheritanceMutexGuard<'a, T> = pi_mutex::PiMutexGuard<'a, T>;

/// See [`PriorityInheritanceMutexGuard`] — `parking_lot`'s guard is already
/// `!Send` for the same reason.
#[cfg(not(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems")))]
pub type PriorityInheritanceMutexGuard<'a, T> = parking_lot::MutexGuard<'a, T>;

/// Diagnostic — `true` when the PI variant is active in this build.
///
/// On RTEMS this is **not** a `cfg!`: PI there is a probe, exactly as it is in
/// C (`posix/osdMutex.c:77-85`), so the answer is what the probe obtained
/// rather than what the build selected. See `pi_mutex::protocol` below.
pub fn is_pi_mutex_active() -> bool {
    #[cfg(target_os = "rtems")]
    {
        pi_mutex::protocol() == rtems_pi::PTHREAD_PRIO_INHERIT
    }
    #[cfg(not(target_os = "rtems"))]
    {
        cfg!(all(target_os = "linux", feature = "linux-rt"))
    }
}

/// The RTEMS priority-protocol surface, declared here rather than taken from
/// `libc`.
///
/// `libc` (the pinned fork, `Cargo.toml`'s `[patch.crates-io]`) declares
/// `pthread_mutexattr_setprotocol` only for cygwin/qurt/teeos/aix and the
/// musl/glibc/uclibc modules, and defines `PTHREAD_PRIO_NONE`/`_INHERIT`/
/// `_PROTECT` only for aix/vxworks/l4re/qurt/apple/hurd/linux — there is no
/// newlib/rtems arm for either. Both exist on the target: the prototype in
/// newlib's `pthread.h:189-206`, the constants in `sys/_pthreadtypes.h:81-83`,
/// and `sys/features.h:394-395` turns `_POSIX_THREAD_PRIO_INHERIT` on
/// unconditionally for `__rtems__`. So the symbols are there at link time and
/// only the Rust binding is missing — the identical situation
/// [`crate::runtime::task`]'s `rtems_sched` block (`task.rs:987-1056`) already
/// solved for `pthread_setschedparam`.
///
/// Deliberately **not** redeclared here: `pthread_mutex_t`,
/// `pthread_mutexattr_t`, and the `pthread_mutex_*` / `pthread_mutexattr_init`
/// / `_destroy` functions. `libc` carries all of them for RTEMS at the
/// target's own widths (`__SIZEOF_PTHREAD_MUTEX_T = 64`,
/// `__SIZEOF_PTHREAD_MUTEXATTR_T = 24`, `src/unix/newlib/mod.rs:283`, `:329`,
/// `:392-393`). Restating a struct layout locally is how the `timespec` and
/// `sockaddr` defects happened; this block declares a function and two
/// integers and no layout at all.
#[cfg(target_os = "rtems")]
mod rtems_pi {
    use std::ffi::c_int;

    /// `sys/_pthreadtypes.h:81-83` on the arm-rtems6 toolchain.
    pub const PTHREAD_PRIO_NONE: c_int = 0;
    /// `sys/_pthreadtypes.h:81-83` on the arm-rtems6 toolchain.
    pub const PTHREAD_PRIO_INHERIT: c_int = 1;

    unsafe extern "C" {
        /// `pthread.h:189-206`; implemented in
        /// `cpukit/posix/src/mutexattrsetprotocol.c`. Absent from `libc`'s
        /// `newlib/rtems` module.
        pub fn pthread_mutexattr_setprotocol(
            attr: *mut libc::pthread_mutexattr_t,
            protocol: c_int,
        ) -> c_int;
    }
}

/// `pthread_mutex_t` with the target's priority protocol — one implementation
/// for both PI targets.
///
/// Linux and RTEMS differ in exactly one thing — where the protocol constant
/// comes from (`protocol` below) — so they share the mutex, the guard and the
/// `unsafe` rather than carrying two copies that must be fixed twice.
#[cfg(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems"))]
mod pi_mutex {
    use std::cell::UnsafeCell;
    use std::ffi::c_int;
    use std::ops::{Deref, DerefMut};

    #[cfg(target_os = "rtems")]
    use super::rtems_pi;
    #[cfg(target_os = "rtems")]
    use super::rtems_pi::pthread_mutexattr_setprotocol;
    #[cfg(not(target_os = "rtems"))]
    use libc::pthread_mutexattr_setprotocol;

    /// The protocol every mutex in this process is built with.
    ///
    /// On Linux this is `PTHREAD_PRIO_INHERIT` unconditionally: the caller
    /// asked for `linux-rt`, glibc supports the protocol, and a failure is a
    /// misconfiguration worth panicking on.
    #[cfg(not(target_os = "rtems"))]
    pub fn protocol() -> c_int {
        libc::PTHREAD_PRIO_INHERIT
    }

    /// The protocol this process actually obtained, probed once.
    ///
    /// C does not assert PI on POSIX — it *probes* it. `globalAttrInit`
    /// (`posix/osdMutex.c:71-88`) sets `PTHREAD_PRIO_INHERIT` on the global
    /// attributes, builds one temporary mutex with them, and on failure
    /// silently downgrades both attributes to `PTHREAD_PRIO_NONE`
    /// (`:81-85`). `epicsMutexShowAll` then reports which it got
    /// (`:199-205`).
    ///
    /// We match that rather than the Linux arm's `assert_eq!`, and the reason
    /// is target-specific: the RTEMS IOC installs no `tracing` subscriber and
    /// has no iocsh, so a panic in a lock constructor during boot is the worst
    /// available failure mode — it is both fatal and silent. Degrading to a
    /// plain mutex loses priority inheritance and says so through
    /// [`is_pi_mutex_active`](super::is_pi_mutex_active); panicking loses the
    /// IOC.
    ///
    /// Probed once per process, so the answer is one fact and not a per-mutex
    /// race — the `OnceLock` is this file's analogue of C's `pthread_once`.
    #[cfg(target_os = "rtems")]
    pub fn protocol() -> c_int {
        static PROTOCOL: std::sync::OnceLock<c_int> = std::sync::OnceLock::new();
        *PROTOCOL.get_or_init(|| {
            // SAFETY: `attr` and `probe` are stack locals of libc's own RTEMS
            // widths, handed only to the pthread calls that own them, and each
            // is destroyed on every path that initialised it.
            unsafe {
                let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                if libc::pthread_mutexattr_init(&mut attr) != 0 {
                    return rtems_pi::PTHREAD_PRIO_NONE;
                }
                let mut obtained = rtems_pi::PTHREAD_PRIO_NONE;
                // C probes only when `setprotocol` itself succeeded
                // (`osdMutex.c:76`), and treats a failed temporary
                // `pthread_mutex_init` as "PI does not work here" (`:81`).
                if pthread_mutexattr_setprotocol(&mut attr, rtems_pi::PTHREAD_PRIO_INHERIT) == 0 {
                    let mut probe: libc::pthread_mutex_t = std::mem::zeroed();
                    if libc::pthread_mutex_init(&mut probe, &attr) == 0 {
                        libc::pthread_mutex_destroy(&mut probe);
                        obtained = rtems_pi::PTHREAD_PRIO_INHERIT;
                    }
                }
                libc::pthread_mutexattr_destroy(&mut attr);
                obtained
            }
        })
    }

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
                // On RTEMS `protocol()` is the probed value, so this call and
                // the `pthread_mutex_init` below are being made with exactly
                // the arguments the probe already proved acceptable. What
                // remains assertable here is the ENOMEM class, which is C's
                // `cantProceed` path (`osdMutex.c:98`), not the
                // PI-unavailable path C degrades on.
                let protocol = protocol();
                let r = pthread_mutexattr_setprotocol(&mut attr, protocol);
                assert_eq!(r, 0, "pthread_mutexattr_setprotocol({protocol}) failed");
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
            PiMutexGuard {
                mutex: self,
                _not_send: std::marker::PhantomData,
            }
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
        /// Makes the guard `!Send`, which `&PiMutex<T>` alone does not.
        ///
        /// Not a lint: POSIX requires `pthread_mutex_unlock` to be called by
        /// the thread that locked the mutex (and an RTEMS PI mutex enforces
        /// ownership), so a guard that could be moved to another thread and
        /// dropped there would unlock from a non-owner. `parking_lot`'s
        /// guard — the arm the host build compiles — is `!Send` for the same
        /// reason, so this also keeps the two arms' auto traits identical
        /// and the "no `.await` under a lock" build error target-independent.
        _not_send: std::marker::PhantomData<*const ()>,
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

    /// The report must follow the arm that was actually compiled, per target.
    /// Host CI only ever executes the last branch, which is the point: the
    /// default build must not claim PI it does not have.
    #[test]
    fn is_pi_mutex_active_matches_the_cfg_arm() {
        #[cfg(all(target_os = "linux", feature = "linux-rt"))]
        assert!(
            is_pi_mutex_active(),
            "linux-rt selects the pthread PI arm unconditionally"
        );

        // On RTEMS the answer is the probe's, not the cfg's — C degrades to
        // PTHREAD_PRIO_NONE when the target refuses PI
        // (`posix/osdMutex.c:77-85`) and so do we. What this pins is that the
        // report and the protocol actually obtained are one fact.
        #[cfg(target_os = "rtems")]
        assert_eq!(
            is_pi_mutex_active(),
            pi_mutex::protocol() == rtems_pi::PTHREAD_PRIO_INHERIT,
            "the RTEMS report must be the probe result, not the cfg"
        );

        #[cfg(not(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems")))]
        assert!(
            !is_pi_mutex_active(),
            "the parking_lot fallback arm has no priority inheritance"
        );
    }

    /// Whichever arm is compiled must be a mutex. On host CI this is the
    /// `parking_lot` fallback — the arm the other two tests can only assert
    /// *about* — so this is where the fallback's exclusion is actually
    /// exercised.
    #[test]
    fn pi_mutex_serialises_concurrent_writers() {
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 10_000;

        let m: Arc<PriorityInheritanceMutex<u64>> = Arc::new(PriorityInheritanceMutex::new(0));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let m = Arc::clone(&m);
                std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        *m.lock() += 1;
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().expect("worker panicked");
        }
        assert_eq!(*m.lock(), THREADS * PER_THREAD);
    }
}
