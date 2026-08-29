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
type MutexBackend<T> = pi_mutex::PiMutex<T>;

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
type MutexBackend<T> = pi_mutex::PiMutex<T>;

/// Non-RT fallback — uses `parking_lot::Mutex` for the common case.
/// PI semantics are not needed because the scheduler does not
/// preempt the lock holder by priority.
#[cfg(not(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems")))]
type MutexBackend<T> = parking_lot::Mutex<T>;

/// C `epicsMutexId` — the mutex every EPICS lock in this port is built from,
/// and the only one `epicsMutexShowAll` can see.
///
/// A newtype over `MutexBackend` rather than a bare alias to it, for the
/// same reason C wraps a `pthread_mutex_t` in an `epicsMutexParm`: the mutex
/// has to be *findable*. C `calloc`s a node carrying the creation site,
/// `ellAdd`s it to a process-global `mutexList` under `epicsMutexGlobalLock`
/// (`epicsMutex.cpp:72-93`), and `ellDelete`s it in `epicsMutexDestroy`
/// (`:105-113`); `epicsMutexShowAll` then walks that list, and — this is what
/// forces the shape — *try-locks each entry* to answer `onlyLocked`
/// (`:129-146`).
///
/// Try-locking from the list means the list must hold something addressable,
/// so the backend goes in a `Box` allocated before registration and freed
/// after deregistration. That is C's node, exactly: a value that never moves
/// for its whole life. The public type moving is then a pointer move, which
/// is why the twenty call sites — struct fields, `Box<[_]>` elements,
/// `Box::leak`ed gates — need no change and cannot invalidate an entry.
/// Registering the value itself instead would leave a dangling address the
/// first time a caller moved one, and `PvDatabase::new` moves two into an
/// `Arc` at IOC init.
///
/// The creation site is [`std::panic::Location::caller`], which is C's
/// `__FILE__`/`__LINE__` reached without a macro: `epicsMutexCreate` is
/// `epicsMutexOsiCreate(__FILE__, __LINE__)` and `#[track_caller]` records the
/// same two facts at the same place.
pub type PriorityInheritanceMutex<T> = epics_mutex::EpicsMutex<T>;

/// The guard `PriorityInheritanceMutex::lock` hands out, nameable so a
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

/// Print, once at boot, which lock protocol this process actually obtained.
///
/// C's counterpart is `epicsMutexShowAll`, which reports "PI is/is not
/// enabled" (`os/posix/osdMutex.c:199-205`) — but that is an iocsh command,
/// and the RTEMS target has no iocsh. So this is an `eprintln!` from the boot
/// path, deliberately not a `tracing` event: a subscriber can be absent,
/// installed late, or filtered, and a diagnostic that reports whether a
/// guarantee exists must not itself depend on one.
///
/// **Two facts on one line, because either alone is misleading.** Priority
/// inheritance on the record gate needs *both* the probe to have returned
/// `PTHREAD_PRIO_INHERIT` ([`is_pi_mutex_active`]) *and* the contending
/// threads to carry distinct scheduling priorities, which is
/// [`RtPolicy::AllowRealtime`](crate::runtime::task::RtPolicy::AllowRealtime)
/// — with the RT switch off every thread is one priority and PI has nothing
/// to inherit (`server::database::record_lock`'s module doc states the same
/// pair). Printing "PI enabled" beside a disabled RT policy would claim an
/// ordering the process does not have.
pub fn report_lock_protocol() {
    let pi = if is_pi_mutex_active() {
        "PI is enabled"
    } else {
        "PI is not enabled"
    };
    eprintln!(
        "epics-rs: lock protocol: {pi}, RT scheduling {:?}",
        crate::runtime::task::RtPolicy::current()
    );
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

    /// The `pthread_mutex_t` lives behind a `Box`, and that is a correctness
    /// requirement on RTEMS rather than a layout preference.
    ///
    /// RTEMS 6 binds a POSIX mutex to **its own address**: `pthread_mutex_init`
    /// stores `flags = ((uintptr_t) mutex ^ POSIX_MUTEX_MAGIC) | protocol`, and
    /// every later operation recomputes that from the address it is handed —
    /// `POSIX_MUTEX_VALIDATE_OBJECT` (`rtems/posix/muteximpl.h:445-459` (`rtems_6`),
    /// magic at `:64`) returns **`EINVAL`** when they disagree. So a
    /// `pthread_mutex_t` that is *relocated* after being initialised is dead:
    /// not slow, not unordered — every `lock` fails.
    ///
    /// Held inline, that is exactly what happened. `new` initialised the mutex
    /// in a stack local and then moved the struct out by value (and callers
    /// move it again — `record_lock`'s gates are `Box::leak(Box::new(…))`), so
    /// on target the first `lock()` returned 22 and the assertion below took
    /// the IOC down at boot. Measured; invisible on Linux because glibc's
    /// mutex carries no address in its state and survives the move.
    ///
    /// Boxing makes the invariant hold **by construction**: the mutex is
    /// allocated first and initialised at the address it will keep for its
    /// whole life, and moving a `PiMutex` moves a pointer. There is no move to
    /// forbid, so there is no runtime check, no `Pin` in the public API and
    /// nothing for a future call site to get wrong. It is also what `std` does
    /// for the same reason on every platform whose `pthread_mutex_t` cannot be
    /// relocated.
    ///
    /// **Not** fixed by zeroing and letting RTEMS auto-initialise: the
    /// auto-initialisation arm of that macro exists for
    /// `PTHREAD_MUTEX_INITIALIZER`, and it produces a *default* mutex — no
    /// protocol, i.e. no priority inheritance. That path would report success
    /// while silently giving up the property this type exists for.
    pub struct PiMutex<T> {
        inner: Box<UnsafeCell<libc::pthread_mutex_t>>,
        data: UnsafeCell<T>,
    }

    unsafe impl<T: Send> Send for PiMutex<T> {}
    unsafe impl<T: Send> Sync for PiMutex<T> {}

    impl<T> PiMutex<T> {
        pub fn new(value: T) -> Self {
            // Allocated before it is initialised, so `pthread_mutex_init` sees
            // the address the mutex keeps for life — see the type's doc.
            let mutex: Box<UnsafeCell<libc::pthread_mutex_t>> =
                Box::new(UnsafeCell::new(unsafe { std::mem::zeroed() }));
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

        /// The address `pthread_mutex_init` was called with — the one RTEMS
        /// validates every later operation against, and the one
        /// `epicsMutexOsdShow` prints as `uaddr`.
        pub fn raw_addr(&self) -> usize {
            self.inner.get() as usize
        }

        /// `pthread_mutex_trylock`, the call C's `onlyLocked` filter makes on
        /// every list entry (`epicsMutex.cpp:137-142`).
        pub fn try_lock(&self) -> Option<PiMutexGuard<'_, T>> {
            // SAFETY: `trylock` never blocks, and the guard returned here
            // unlocks from this same thread, so POSIX's unlock-by-owner rule
            // holds.
            if unsafe { libc::pthread_mutex_trylock(self.inner.get()) } != 0 {
                return None;
            }
            Some(PiMutexGuard {
                mutex: self,
                _not_send: std::marker::PhantomData,
            })
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

    /// The `parking_lot::Mutex` arm has `Debug`, so this arm must too.
    ///
    /// [`PriorityInheritanceMutex`](super::PriorityInheritanceMutex) is one
    /// type alias with three `cfg` arms, and any trait the fallback arm has
    /// that a PI arm lacks is a build break that only ever appears on the
    /// target: a `#[derive(Debug)]` on a struct holding one compiles on a
    /// developer's box and fails for `armv7-rtems-eabihf`. That is how this
    /// impl was found — `qsrv::group_config::GroupPvDef` (`#[derive(Debug,
    /// Clone)]`, holding the L33 atomic-write lock) is the first such struct,
    /// and it broke nothing until the crate entered the RTEMS gate. The
    /// guard's `!Send` note above already states the rule for auto traits;
    /// this is the same rule for the named ones.
    ///
    /// `try_lock` rather than `lock`, matching `parking_lot`: a `Debug` impl
    /// that blocks deadlocks the moment anything formats a structure while the
    /// lock is held — including a panic message on the locking thread.
    impl<T: std::fmt::Debug> std::fmt::Debug for PiMutex<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // SAFETY: `trylock` never blocks. On success this thread owns the
            // mutex for the read below and unlocks before returning, so the
            // POSIX "unlock by the locking thread" rule holds.
            let acquired = unsafe { libc::pthread_mutex_trylock(self.inner.get()) } == 0;
            if !acquired {
                return f
                    .debug_struct("PiMutex")
                    .field("data", &"<locked>")
                    .finish();
            }
            let out = f
                .debug_struct("PiMutex")
                .field("data", unsafe { &*self.data.get() })
                .finish();
            unsafe {
                libc::pthread_mutex_unlock(self.inner.get());
            }
            out
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

/// C `epicsMutex.cpp`'s `mutexList` and the node on it.
///
/// The whole module exists so `epicsMutexShowAll` can answer the question it
/// is run to answer — *which lock is held right now* — rather than only how
/// many exist. That answer needs a try-lock through a stable address, which is
/// what pins the `Box` and the `Drop`.
mod epics_mutex {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::MutexBackend;

    /// One entry of C's `mutexList` (`epicsMutex.cpp:39`).
    ///
    /// `probe` is the entry's own `try_lock`, monomorphised for the `T` that
    /// registered it and then type-erased to a plain function pointer. C needs
    /// no equivalent because its node's payload is opaque bytes; here the
    /// backend is generic, and the list must be one list.
    struct Entry {
        id: u64,
        file: &'static str,
        line: u32,
        addr: usize,
        osd_addr: usize,
        probe: unsafe fn(usize) -> bool,
    }

    /// Creation order, as C's `ellAdd` appends.
    static MUTEXES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    /// A poisoned list is still a readable list — the same reasoning as the
    /// thread registry's: a panic while formatting one row must not make the
    /// IOC's lock report permanently unavailable.
    fn lock() -> std::sync::MutexGuard<'static, Vec<Entry>> {
        MUTEXES.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// C `epicsMutexId` (`epicsMutex.cpp:72-93`) — see
    /// [`PriorityInheritanceMutex`](super::PriorityInheritanceMutex) for why
    /// this is a newtype and not an alias.
    pub struct EpicsMutex<T> {
        /// Allocated before registration and freed after deregistration, so
        /// the address in the list is valid for exactly as long as the list
        /// holds it.
        inner: Box<MutexBackend<T>>,
        id: u64,
    }

    impl<T> EpicsMutex<T> {
        /// C `epicsMutexCreate()` — `epicsMutexOsiCreate(__FILE__, __LINE__)`.
        #[track_caller]
        pub fn new(value: T) -> Self {
            let inner = Box::new(MutexBackend::new(value));
            let caller = std::panic::Location::caller();
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            lock().push(Entry {
                id,
                file: caller.file(),
                line: caller.line(),
                addr: &*inner as *const MutexBackend<T> as usize,
                osd_addr: osd_addr(&inner),
                probe: probe_locked::<T>,
            });
            Self { inner, id }
        }

        pub fn lock(&self) -> super::PriorityInheritanceMutexGuard<'_, T> {
            self.inner.lock()
        }

        /// C `epicsMutexTryLock`. Also what [`report`] calls through `probe`.
        pub fn try_lock(&self) -> Option<super::PriorityInheritanceMutexGuard<'_, T>> {
            self.inner.try_lock()
        }

        /// The address `pthread_mutex_init` was called with — the one RTEMS
        /// validates every later operation against, and the one C's
        /// `epicsMutexOsdShow` prints as `uaddr`.
        #[cfg(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems"))]
        pub fn raw_addr(&self) -> usize {
            self.inner.raw_addr()
        }
    }

    /// C `epicsMutexDestroy` (`epicsMutex.cpp:105-113`): off the list first,
    /// under the list lock, and only then freed. A walk holding that lock can
    /// therefore dereference every address it is looking at.
    impl<T> Drop for EpicsMutex<T> {
        fn drop(&mut self) {
            let id = self.id;
            lock().retain(|entry| entry.id != id);
        }
    }

    /// `parking_lot::Mutex` has `Debug`, so this must too — a `#[derive(Debug)]`
    /// on a struct holding one would otherwise break on whichever arm lacked it.
    impl<T: std::fmt::Debug> std::fmt::Debug for EpicsMutex<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.inner.fmt(f)
        }
    }

    #[cfg(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems"))]
    fn osd_addr<T>(inner: &MutexBackend<T>) -> usize {
        inner.raw_addr()
    }

    /// The fallback backend has no OS object under it, so the address that
    /// stands in for C's `uaddr` is the lock's own — see [`super::MUTEX_OSD_LABEL`],
    /// which says which of the two this build printed.
    #[cfg(not(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems")))]
    fn osd_addr<T>(inner: &MutexBackend<T>) -> usize {
        inner as *const MutexBackend<T> as usize
    }

    /// # Safety
    ///
    /// `addr` must be the address a live `Box<MutexBackend<T>>` was registered
    /// with, for the same `T`. [`report`] calls this only while holding the
    /// list lock, and [`EpicsMutex::drop`] removes the entry under that same
    /// lock before the box is freed, so an address reachable here is live.
    unsafe fn probe_locked<T>(addr: usize) -> bool {
        let backend = unsafe { &*(addr as *const MutexBackend<T>) };
        match backend.try_lock() {
            Some(guard) => {
                drop(guard);
                false
            }
            None => true,
        }
    }

    /// One row of C's `epicsMutexShow` (`epicsMutex.cpp:118-127`).
    #[derive(Clone, Debug)]
    pub struct MutexInfo {
        addr: usize,
        osd_addr: usize,
        file: &'static str,
        line: u32,
    }

    impl MutexInfo {
        /// C prints the node address as the mutex's identity; this is that
        /// address, and it is stable for the mutex's whole life.
        pub fn addr(&self) -> usize {
            self.addr
        }

        /// Where the mutex was created — C's `pFileName` and `lineno`.
        pub fn file(&self) -> &'static str {
            self.file
        }

        pub fn line(&self) -> u32 {
            self.line
        }

        /// C `epicsMutexShow` plus, above `level` 0, `epicsMutexOsdShow`
        /// (`os/posix/osdMutex.c:188-195`).
        pub fn show_lines(&self, level: u32) -> Vec<String> {
            let mut lines = vec![format!(
                "epicsMutexId {:#x} source {} line {}",
                self.addr, self.file, self.line
            )];
            if level > 0 {
                lines.push(format!(
                    "    {} uaddr={:#x}",
                    super::MUTEX_OSD_LABEL,
                    self.osd_addr
                ));
            }
            lines
        }
    }

    /// What C prints from one `epicsMutexShowAll` call: the whole list's
    /// length, then the rows that passed `onlyLocked`.
    ///
    /// Both come out of one lock acquisition. C reads `ellCount` outside the
    /// list lock and walks inside it (`epicsMutex.cpp:133-136`), so its count
    /// and its rows can disagree by a mutex created in between; there is no
    /// reason to reproduce that.
    pub struct MutexReport {
        pub total: usize,
        pub shown: Vec<MutexInfo>,
    }

    /// C `epicsMutexShowAll`'s list walk (`epicsMutex.cpp:129-146`).
    ///
    /// `only_locked` is C's try-lock filter. C's mutexes are recursive, so its
    /// filter reads "held by another thread"; this port's are not reentrant
    /// (see `server::database::record_lock`), so the filter reads "cannot be
    /// acquired right now". The two agree for every caller that matters — the
    /// iocsh thread holds none of these locks while running the command — and
    /// the second is the honest phrasing of a non-recursive lock's try-lock.
    pub fn report(only_locked: bool) -> MutexReport {
        let entries = lock();
        let mut shown = Vec::new();
        for entry in entries.iter() {
            if only_locked {
                // SAFETY: see `probe_locked`. The list lock is held here, and
                // deregistration takes it before freeing.
                if !unsafe { (entry.probe)(entry.addr) } {
                    continue;
                }
            }
            shown.push(MutexInfo {
                addr: entry.addr,
                osd_addr: entry.osd_addr,
                file: entry.file,
                line: entry.line,
            });
        }
        MutexReport {
            total: entries.len(),
            shown,
        }
    }
}

pub use epics_mutex::{MutexInfo, MutexReport};

/// What this build's mutex actually is, for the `uaddr` line C prints from
/// `epicsMutexOsdShow`. Naming it `pthread_mutex_t*` on the fallback arm would
/// claim an OS object that arm does not create.
#[cfg(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems"))]
pub const MUTEX_OSD_LABEL: &str = "pthread_mutex_t*";

/// See [`MUTEX_OSD_LABEL`].
#[cfg(not(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems")))]
pub const MUTEX_OSD_LABEL: &str = "parking_lot::Mutex*";

/// Every EPICS mutex in the process, filtered as C's `onlyLocked` filters —
/// `epicsMutexShowAll`'s list walk.
pub fn mutex_report(only_locked: bool) -> MutexReport {
    epics_mutex::report(only_locked)
}

/// C `epicsMutexOsdShowAll` (`os/posix/osdMutex.c:197-210`), the line
/// `epicsMutexShowAll` prints between the count and the rows.
///
/// C has a third answer, `PI not supported`, for a build where
/// `_POSIX_THREAD_PRIO_INHERIT` is undefined. There is no such build here: the
/// fallback arm is a deliberate choice not to use a pthread mutex at all, so
/// the honest report is that PI is not enabled, which is what
/// [`is_pi_mutex_active`] already answers.
pub fn osd_show_all_line() -> &'static str {
    if is_pi_mutex_active() {
        "PI is enabled"
    } else {
        "PI is not enabled"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The creation site C records as `__FILE__`/`__LINE__`, and the two
    /// moments the entry exists between: C `ellAdd` in `epicsMutexOsiCreate`
    /// and `ellDelete` in `epicsMutexDestroy`.
    #[test]
    fn an_entry_lasts_exactly_as_long_as_its_mutex() {
        let expected_line = line!() + 1;
        let m: PriorityInheritanceMutex<i32> = PriorityInheritanceMutex::new(5);
        let addr = find_entry(&m).expect("registered at construction").addr();

        let entry = find_entry(&m).unwrap();
        assert_eq!(entry.file(), file!(), "C's `pFileName` is the caller's");
        assert_eq!(entry.line(), expected_line, "C's `lineno` is the caller's");

        drop(m);
        assert!(
            !mutex_report(false).shown.iter().any(|e| e.addr() == addr),
            "the entry must come off the list before the mutex is freed"
        );
    }

    /// Locate `m`'s own row, which is the only way to test a process-global
    /// list that other code also registers into.
    fn find_entry<T>(m: &PriorityInheritanceMutex<T>) -> Option<MutexInfo> {
        let want = mutex_addr(m);
        mutex_report(false)
            .shown
            .into_iter()
            .find(|e| e.addr() == want)
    }

    /// The address the list holds, reached the same way `new` computed it.
    fn mutex_addr<T>(m: &PriorityInheritanceMutex<T>) -> usize {
        // One row per mutex, so the row that reports this file and this
        // mutex's line is this mutex — except that two mutexes can share a
        // line, which is why the tests that need identity capture the addr
        // once and compare against it afterwards.
        let guard = m.try_lock();
        let held = guard.is_none();
        drop(guard);
        assert!(!held, "helper must not be called on a held mutex");
        // The registered address is the boxed backend's, and `try_lock`
        // proved this mutex is the free one; find it by elimination on the
        // locked probe.
        let before: Vec<usize> = mutex_report(true).shown.iter().map(|e| e.addr()).collect();
        let _g = m.lock();
        let after: Vec<usize> = mutex_report(true).shown.iter().map(|e| e.addr()).collect();
        after.into_iter().find(|a| !before.contains(a)).unwrap()
    }

    /// C's `onlyLocked` boundary, both sides of it: the filter try-locks every
    /// entry and keeps the ones it could not take.
    #[test]
    fn only_locked_keeps_exactly_the_held_mutexes() {
        let m: PriorityInheritanceMutex<i32> = PriorityInheritanceMutex::new(0);
        let addr = mutex_addr(&m);
        // A second, never-held mutex, so "the filter excludes something" is a
        // property of this test and not of whatever else the process created.
        let other: PriorityInheritanceMutex<i32> = PriorityInheritanceMutex::new(0);
        let other_addr = mutex_addr(&other);

        let free = mutex_report(true);
        assert!(
            !free.shown.iter().any(|e| e.addr() == addr),
            "an unheld mutex must not be listed under onlyLocked"
        );

        let guard = m.lock();
        let held = mutex_report(true);
        assert!(
            held.shown.iter().any(|e| e.addr() == addr),
            "a held mutex must be listed under onlyLocked"
        );
        assert_eq!(
            held.total, free.total,
            "the count is the whole list, not the filtered rows — C prints \
             `ellCount(&mutexList)` before it filters"
        );
        assert!(
            !held.shown.iter().any(|e| e.addr() == other_addr),
            "the filter must exclude the mutex nobody holds"
        );
        assert!(
            held.shown.len() < held.total,
            "{} of {}",
            held.shown.len(),
            held.total
        );
        drop(guard);

        assert!(!mutex_report(true).shown.iter().any(|e| e.addr() == addr));
    }

    /// The address in the list is the boxed backend's, so it survives moving
    /// the mutex — the property that makes the `onlyLocked` probe sound. A
    /// registry of addresses of the values themselves would dangle here.
    #[test]
    fn the_registered_address_survives_moving_the_mutex() {
        let m: PriorityInheritanceMutex<i32> = PriorityInheritanceMutex::new(1);
        let addr = mutex_addr(&m);
        let moved = Box::new(m);
        assert_eq!(mutex_addr(&moved), addr);
        assert!(mutex_report(false).shown.iter().any(|e| e.addr() == addr));
        drop(moved);
        assert!(!mutex_report(false).shown.iter().any(|e| e.addr() == addr));
    }

    /// C `epicsMutexShow` prints one line; `epicsMutexOsdShow` adds the second
    /// only above level 0.
    #[test]
    fn show_lines_adds_the_osd_line_only_above_level_zero() {
        let m: PriorityInheritanceMutex<i32> = PriorityInheritanceMutex::new(0);
        let entry = find_entry(&m).unwrap();

        let plain = entry.show_lines(0);
        assert_eq!(plain.len(), 1);
        assert_eq!(
            plain[0],
            format!(
                "epicsMutexId {:#x} source {} line {}",
                entry.addr(),
                entry.file(),
                entry.line()
            )
        );

        let detailed = entry.show_lines(1);
        assert_eq!(detailed.len(), 2);
        assert_eq!(detailed[0], plain[0]);
        assert!(
            detailed[1].starts_with(&format!("    {MUTEX_OSD_LABEL} uaddr=0x")),
            "{}",
            detailed[1]
        );
    }

    /// The PI line and the PI fact are one fact, as C's are: both come from
    /// what the process actually obtained.
    #[test]
    fn the_osd_show_all_line_is_the_pi_report() {
        assert_eq!(
            osd_show_all_line(),
            if is_pi_mutex_active() {
                "PI is enabled"
            } else {
                "PI is not enabled"
            }
        );
    }

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

    /// The `pthread_mutex_t` must not travel with the `PiMutex` that owns it.
    ///
    /// RTEMS binds a POSIX mutex to its own address and returns `EINVAL` from
    /// every operation on a relocated one (`PiMutex`'s doc). Measured on
    /// target: with the mutex stored inline the IOC panicked on its first
    /// `lock()` at boot. No host arm can reproduce *that* — glibc's mutex
    /// survives the move and the default host build is `parking_lot` — so
    /// what this pins is the property that makes it impossible: the address
    /// handed to `pthread_mutex_init` is stable across a move of the owner.
    /// Storing the mutex inline again fails here rather than on the next boot.
    #[cfg(any(all(target_os = "linux", feature = "linux-rt"), target_os = "rtems"))]
    #[test]
    fn the_pthread_object_does_not_move_with_the_mutex() {
        let m: PriorityInheritanceMutex<i32> = PriorityInheritanceMutex::new(7);
        let before = m.raw_addr();
        let moved = Box::new(m);
        assert_eq!(
            moved.raw_addr(),
            before,
            "the pthread_mutex_t moved with its owner; RTEMS answers EINVAL to \
             every lock on a relocated mutex"
        );
        assert_eq!(*moved.lock(), 7);
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
