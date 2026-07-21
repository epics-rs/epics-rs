use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;
use tokio::runtime::RuntimeFlavor;

pub use tokio::runtime::Handle as RuntimeHandle;

/// A synchronous caller asked to block on an async operation from a thread
/// where blocking cannot be made sound: a **current-thread** tokio runtime.
///
/// Parking that thread stops every task on that runtime, including whichever
/// one holds the state the awaited future is waiting for — so the block would
/// never be woken. No blocking mechanism can fix this; the caller has to `await`
/// the async operation instead of blocking on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotBlockable;

impl std::fmt::Display for NotBlockable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot block a current-thread runtime")
    }
}

impl std::error::Error for NotBlockable {}

/// A [`Waker`] that unparks the thread that built it. The single owner of the
/// "poll-then-park" wake mechanism in this crate: both [`park_on`] (the sync
/// bridge) and the RTEMS future executor
/// ([`crate::runtime::background::future_exec`]) drive a future by polling on a
/// thread and parking it between polls, so both build one of these on their own
/// thread and rely on the future's cross-thread waker to unpark them.
pub(crate) struct ThreadWaker(std::thread::Thread);

impl ThreadWaker {
    /// A waker over the *current* thread — call this on the thread that will
    /// park.
    pub(crate) fn for_current_thread() -> Waker {
        Waker::from(Arc::new(ThreadWaker(std::thread::current())))
    }
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drive `fut` to completion on this thread, parking between polls, and stop
/// early when `should_cancel` returns `true`.
///
/// Returns `Some(output)` when the future completed, or `None` when it was
/// cancelled before completing (the future is dropped in place on cancel,
/// running its destructors — the same "drop at the next suspension point"
/// semantics a cancelled tokio task has).
///
/// The future must only await runtime-agnostic primitives (`tokio::sync`
/// locks/channels/notifies): nothing here drives a reactor or a timer wheel, so
/// whoever wakes us must be running on some other thread. A cancel is observed
/// on the next wake — the caller that flips `should_cancel` must also
/// [`unpark`](std::thread::Thread::unpark) this thread so a *parked* driver
/// re-checks promptly rather than sleeping until the future's own waker fires.
pub(crate) fn park_on_interruptible<F: Future>(
    fut: F,
    mut should_cancel: impl FnMut() -> bool,
) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let waker = ThreadWaker::for_current_thread();
    let mut cx = Context::from_waker(&waker);
    loop {
        if should_cancel() {
            return None;
        }
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return Some(value);
        }
        std::thread::park();
    }
}

/// Drive `fut` to completion on this thread, parking between polls. Thin
/// uncancellable wrapper over [`park_on_interruptible`].
///
/// The future must only await runtime-agnostic primitives (`tokio::sync`
/// locks/channels/notifies): nothing here drives a reactor or a timer wheel, so
/// whoever wakes us must be running on some other thread.
fn park_on<F: Future>(fut: F) -> F::Output {
    // Never cancels, so `park_on_interruptible` always returns `Some`.
    park_on_interruptible(fut, || false).expect("uncancellable driver returned None")
}

/// Block the calling thread on `fut`, picking the mechanism that is sound for
/// the thread we are actually on.
///
/// This is the single owner of "sync call over async state" in this crate; the
/// three caller contexts are not interchangeable and picking one mechanism for
/// all of them is what makes such bridges panic:
///
/// - **No runtime entered** (a plain `std::thread`, an iocsh thread) — park the
///   thread. Nothing else runs here, so there is nothing to starve; the tasks
///   that will wake us live on some other runtime's threads.
/// - **Multi-thread runtime worker** — [`tokio::task::block_in_place`], which
///   hands this worker's remaining tasks to a sibling before it is parked.
/// - **Current-thread runtime** — [`Err(NotBlockable)`](NotBlockable). Parking
///   the only thread of that runtime halts every task on it, including the one
///   that would wake us. This is unsound for *any* blocking mechanism, so it is
///   reported to the caller instead of being panicked on (today) or deadlocked
///   on (the worse alternative).
pub fn block_on_sync<F: Future>(fut: F) -> Result<F::Output, NotBlockable> {
    match RuntimeHandle::try_current() {
        Ok(handle) => match handle.runtime_flavor() {
            RuntimeFlavor::CurrentThread => Err(NotBlockable),
            _ => Ok(tokio::task::block_in_place(|| handle.block_on(fut))),
        },
        Err(_) => Ok(park_on(fut)),
    }
}

// ---------------------------------------------------------------------------
// Platform-selected task handle types (decision A2 / B)
//
// The seam hands back one of these aliases from every spawn; call sites in this
// crate name only the alias, never a tokio handle. Hosted = the tokio handle
// types. RTEMS = the always-compiled, host-tested mirrors in
// `background::future_exec` (`JoinFuture`/`AbortHandle`/`JoinError`), which
// reproduce exactly the subset of the tokio surface the call sites use.
// ---------------------------------------------------------------------------

/// Handle to a spawned task — `await` for its result, `abort()` to cancel.
#[cfg(tokio_backend)]
pub type TaskHandle<T> = tokio::task::JoinHandle<T>;
/// Detached cancellation handle for a spawned task.
#[cfg(tokio_backend)]
pub type TaskAbortHandle = tokio::task::AbortHandle;
/// Error from awaiting a [`TaskHandle`] (cancelled or panicked).
#[cfg(tokio_backend)]
pub type TaskJoinError = tokio::task::JoinError;

#[cfg(exec_backend)]
pub type TaskHandle<T> = crate::runtime::background::future_exec::JoinFuture<T>;
#[cfg(exec_backend)]
pub type TaskAbortHandle = crate::runtime::background::future_exec::AbortHandle;
#[cfg(exec_backend)]
pub type TaskJoinError = crate::runtime::background::future_exec::JoinError;

// ---------------------------------------------------------------------------
// Process-global background executor (RTEMS spawn/timer backend)
//
// On RTEMS the seam routes every spawn/sleep/interval into one process-global
// `BackgroundExecutor` (callback pool + delayed timer + scanOnce worker). Two
// init paths, both landing on the same `OnceLock`:
//
//   * Explicit — `background_init()` from `IocApplication::run`, mirroring C's
//     `callbackInit` running early in `iocInit` (callback.c:286) so the
//     facilities exist before any record processing can defer a tail.
//   * Lazy fallback — the first `spawn`/`sleep`/`interval` on a path that
//     never went through `run` (a unit test, an embedded harness) initialises
//     it on demand via the same `get_or_init`.
//
// Compiled on RTEMS and under `cfg(test)` (so the wiring is host-exercised);
// on a hosted non-test build the tokio runtime is the backend and this is not
// compiled.
// ---------------------------------------------------------------------------

#[cfg(any(exec_backend, test))]
static BACKGROUND: std::sync::OnceLock<crate::runtime::background::BackgroundExecutor> =
    std::sync::OnceLock::new();

/// The process-global background executor, initialised on first use.
#[cfg(any(exec_backend, test))]
fn background() -> &'static crate::runtime::background::BackgroundExecutor {
    BACKGROUND.get_or_init(crate::runtime::background::BackgroundExecutor::new)
}

/// Eagerly start the process-global background executor — C `callbackInit`
/// parity (callback.c:286), called once from `IocApplication::run`. Idempotent:
/// a second call (or a prior lazy init) is a no-op, matching `callbackInit`'s
/// own re-entry guard (callback.c:292-295). Only the RTEMS build uses the
/// executor; hosted builds drive tails on the tokio runtime.
#[cfg(any(exec_backend, test))]
pub fn background_init() {
    let _ = background();
}

#[cfg(tokio_backend)]
pub fn spawn<F>(future: F) -> TaskHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

/// RTEMS: drive the tail on a callback-pool worker via the host-tested future
/// executor, at the default Medium band.
#[cfg(exec_backend)]
pub fn spawn<F>(future: F) -> TaskHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    use crate::runtime::background::future_exec::{DEFAULT_SPAWN_PRIORITY, spawn_future};
    spawn_future(
        &background().callbacks().handle(),
        DEFAULT_SPAWN_PRIORITY,
        future,
    )
}

#[cfg(tokio_backend)]
pub fn spawn_blocking<F, R>(f: F) -> TaskHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
}

/// RTEMS: run the blocking closure on a callback-pool worker at the default
/// Medium band via the host-tested future executor.
#[cfg(exec_backend)]
pub fn spawn_blocking<F, R>(f: F) -> TaskHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    use crate::runtime::background::future_exec::{DEFAULT_SPAWN_PRIORITY, spawn_blocking_on};
    spawn_blocking_on(
        &background().callbacks().handle(),
        DEFAULT_SPAWN_PRIORITY,
        f,
    )
}

#[cfg(tokio_backend)]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// RTEMS: sleep on the delayed-callback timer via the host-tested `Sleep`.
#[cfg(exec_backend)]
pub async fn sleep(duration: Duration) {
    crate::runtime::background::timer_sleep::sleep(&background().timer().handle(), duration).await;
}

#[cfg(tokio_backend)]
pub async fn sleep_until(deadline: std::time::Instant) {
    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
}

/// RTEMS: sleep-until on the delayed-callback timer via the host-tested `Sleep`.
#[cfg(exec_backend)]
pub async fn sleep_until(deadline: std::time::Instant) {
    crate::runtime::background::timer_sleep::sleep_until(&background().timer().handle(), deadline)
        .await;
}

/// Periodic ticker — the seam replacement for `tokio::time::interval`, so no
/// production site names `tokio::time` directly (decision A2). The hosted build
/// wraps `tokio::time::Interval`, preserving its default
/// `MissedTickBehavior::Burst` catch-up and immediate first tick; the RTEMS
/// build substitutes the runtime-free
/// [`crate::runtime::background::timer_sleep::TimerInterval`], which reproduces
/// the same semantics over the delayed-callback timer.
#[cfg(tokio_backend)]
pub struct Interval {
    inner: tokio::time::Interval,
}

#[cfg(tokio_backend)]
impl Interval {
    /// Complete at the next tick. The first tick is immediate (tokio parity);
    /// callers that want to skip it await `tick()` once up front.
    pub async fn tick(&mut self) {
        self.inner.tick().await;
    }
}

/// RTEMS: the periodic ticker is the runtime-free `TimerInterval` (same
/// immediate-first-tick + Burst catch-up semantics, same `tick()` surface).
#[cfg(exec_backend)]
pub type Interval = crate::runtime::background::timer_sleep::TimerInterval;

/// Build a periodic ticker firing every `period` — the seam replacement for
/// `tokio::time::interval`.
#[cfg(tokio_backend)]
pub fn interval(period: Duration) -> Interval {
    Interval {
        inner: tokio::time::interval(period),
    }
}

/// RTEMS: build the periodic ticker on the delayed-callback timer.
#[cfg(exec_backend)]
pub fn interval(period: Duration) -> Interval {
    crate::runtime::background::timer_sleep::interval(&background().timer().handle(), period)
}

pub fn runtime_handle() -> tokio::runtime::Handle {
    tokio::runtime::Handle::current()
}

// ---------------------------------------------------------------------------
// EPICS thread priority abstraction
//
// C parity: `modules/libcom/src/osi/epicsThread.h:73-92` defines an
// integer priority space `0..=99` (`epicsThreadPriorityMin/Max`) with a
// set of named levels, plus three stack-size classes.
// `osi/os/posix/osdThread.c` maps an EPICS priority `p` onto the OS
// SCHED_FIFO range with `oss = p * (max-min)/100 + min` and falls back
// to a non-RT (default-policy) thread when the process lacks permission
// to use SCHED_FIFO.
//
// The Rust port runs work as tokio tasks on a shared pool, so there is
// no per-task OS thread to re-prioritise for `spawn`. What is portably
// achievable is: (a) the priority enum + named levels as a first-class
// type, (b) a stack-size class with the C size table, and (c) a
// best-effort OS-scheduler priority applied to the *current* OS thread
// (used by dedicated `spawn_blocking` threads and the runtime's worker
// threads). `apply_to_current_thread` reports whether the OS actually
// honoured the request.
// ---------------------------------------------------------------------------

/// Minimum EPICS thread priority (`epicsThreadPriorityMin`).
pub const PRIORITY_MIN: u8 = 0;
/// Maximum EPICS thread priority (`epicsThreadPriorityMax`).
pub const PRIORITY_MAX: u8 = 99;

/// EPICS thread priority — an integer `0..=99` with the named levels
/// from `epicsThreadPriority*` (`epicsThread.h:73-83`). Lower values
/// are lower priority; the CA server bands sit below the scan bands so
/// scan threads preempt CA-server threads on a loaded IOC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThreadPriority {
    /// `epicsThreadPriorityLow` = 10.
    Low,
    /// `epicsThreadPriorityCAServerLow` = 20.
    CaServerLow,
    /// `epicsThreadPriorityCAServerHigh` = 40.
    CaServerHigh,
    /// `epicsThreadPriorityMedium` = 50.
    Medium,
    /// `epicsThreadPriorityScanLow` = 60.
    ScanLow,
    /// `epicsThreadPriorityScanHigh` = 70.
    ScanHigh,
    /// `epicsThreadPriorityHigh` = 90.
    High,
    /// `epicsThreadPriorityIocsh` = 91.
    Iocsh,
    /// An explicit priority value, clamped to `0..=99` on use.
    Custom(u8),
}

impl ThreadPriority {
    /// The raw EPICS priority value `0..=99`, matching the
    /// `epicsThreadPriority*` constants in `epicsThread.h`.
    pub fn value(self) -> u8 {
        let v = match self {
            ThreadPriority::Low => 10,
            ThreadPriority::CaServerLow => 20,
            ThreadPriority::CaServerHigh => 40,
            ThreadPriority::Medium => 50,
            ThreadPriority::ScanLow => 60,
            ThreadPriority::ScanHigh => 70,
            ThreadPriority::High => 90,
            ThreadPriority::Iocsh => 91,
            ThreadPriority::Custom(v) => v,
        };
        v.min(PRIORITY_MAX)
    }
}

/// Stack-size class — `epicsThreadStackSizeClass` (`epicsThread.h:91`).
///
/// The byte size is implementation-dependent in C; the values here
/// mirror the POSIX table `STACK_SIZE(f) = f * 0x10000 * sizeof(void*)`
/// (`osdThread.c:506-509`) for a 64-bit target (`sizeof(void*) == 8`):
/// Small = 1, Medium = 2, Big = 4 units of `0x10000 * 8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackSizeClass {
    Small,
    Medium,
    Big,
}

impl StackSizeClass {
    /// Stack size in bytes for this class, matching the POSIX
    /// `stackSizeTable` in `osdThread.c` on a 64-bit target.
    pub fn bytes(self) -> usize {
        // STACK_SIZE(f) = f * 0x10000 * sizeof(void*)
        let unit = 0x10000usize * std::mem::size_of::<usize>();
        match self {
            StackSizeClass::Small => unit,
            StackSizeClass::Medium => 2 * unit,
            StackSizeClass::Big => 4 * unit,
        }
    }
}

/// Outcome of a best-effort OS-scheduler priority change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityApplied {
    /// The OS scheduler honoured the requested priority (real-time
    /// SCHED_FIFO band applied).
    Realtime,
    /// The platform does not expose a portable scheduler priority API
    /// (e.g. Windows here, or a non-Unix target) — no change applied.
    Unsupported,
    /// The platform exposes the API but rejected the request (typically
    /// the process lacks `CAP_SYS_NICE`/root for SCHED_FIFO). C's
    /// `osdThread.c` makes the same best-effort fall back to a non-RT
    /// thread in this case (`osdThread.c:647` "Try again without
    /// SCHED_FIFO").
    BestEffortFailed,
}

impl PriorityApplied {
    /// `true` only when the OS actually applied a real-time priority.
    pub fn is_realtime(self) -> bool {
        matches!(self, PriorityApplied::Realtime)
    }
}

/// Apply an EPICS [`ThreadPriority`] to the **current OS thread**, best
/// effort.
///
/// C parity: mirrors `osdThread.c`'s SCHED_FIFO mapping
/// `oss = p * (max-min)/100 + min` over the kernel's
/// `sched_get_priority_min/max(SCHED_FIFO)` range, and the
/// EPERM-fallback to a non-RT thread.
///
/// Returns [`PriorityApplied`] describing what the platform allowed —
/// callers running in environments without RT permission still get a
/// running thread, just at the default policy, exactly as a C IOC does.
///
/// Note: tokio tasks spawned via [`spawn`] share worker threads, so
/// this is meaningful for [`spawn_blocking`] closures and for tuning
/// the runtime's worker threads at startup — not for individual async
/// tasks.
///
/// Platform support: the OS-scheduler change is wired on Linux (where
/// the crate links `libc`). On other targets the priority enum + API
/// surface still exist but `apply` reports [`PriorityApplied::Unsupported`]
/// — the platform allows no portable change here.
pub fn apply_to_current_thread(priority: ThreadPriority) -> PriorityApplied {
    apply_priority_impl(priority.value())
}

#[cfg(target_os = "linux")]
fn apply_priority_impl(epics_priority: u8) -> PriorityApplied {
    // SAFETY: sched_get_priority_min/max take only an int policy and
    // have no preconditions; pthread_setschedparam operates on the
    // calling thread with a stack-local sched_param.
    unsafe {
        let policy = libc::SCHED_FIFO;
        let min = libc::sched_get_priority_min(policy);
        let max = libc::sched_get_priority_max(policy);
        if min < 0 || max < 0 || max < min {
            return PriorityApplied::Unsupported;
        }
        // C `osdThread.c:138-139`: slope over a 0..100 EPICS range.
        let slope = (max - min) as f64 / 100.0;
        let mut oss = (epics_priority as f64 * slope) as i32 + min;
        if oss < min {
            oss = min;
        }
        if oss > max {
            oss = max;
        }
        let param = libc::sched_param {
            sched_priority: oss,
        };
        let rc = libc::pthread_setschedparam(libc::pthread_self(), policy, &param);
        if rc == 0 {
            PriorityApplied::Realtime
        } else {
            // EPERM (no RT permission) or EINVAL — C falls back to a
            // non-RT thread here. Leave the thread at the default
            // policy and report best-effort failure.
            tracing::debug!(
                target: "epics_base_rs::runtime",
                epics_priority,
                oss,
                errno = rc,
                "SCHED_FIFO priority not applied; thread stays at default policy"
            );
            PriorityApplied::BestEffortFailed
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_priority_impl(_epics_priority: u8) -> PriorityApplied {
    // The crate links `libc` only on Linux; no portable OS-scheduler
    // priority API is wired on other targets.
    PriorityApplied::Unsupported
}

/// Spawn a blocking closure on a dedicated thread and apply the given
/// EPICS [`ThreadPriority`] to that thread before running `f`.
///
/// The priority application is best effort (see
/// [`apply_to_current_thread`]); `f` runs regardless of whether the OS
/// honoured the request. This is the priority-aware counterpart of
/// [`spawn_blocking`] for IOC threads (CA server, scan) that a C IOC
/// would run in a distinct SCHED band.
#[cfg(tokio_backend)]
pub fn spawn_blocking_with_priority<F, R>(priority: ThreadPriority, f: F) -> TaskHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _ = apply_to_current_thread(priority);
        f()
    })
}

/// RTEMS: run the blocking closure on a callback-pool worker.
///
/// The requested EPICS [`ThreadPriority`] is **not** yet mapped onto a callback
/// band here: the pool workers are long-lived and shared, so re-prioritising the
/// running worker per task would leak that priority into the next callback it
/// drains. The closure runs at the pool's default Medium band; mapping
/// `ThreadPriority` to a `CallbackPriority` band is deferred to RTEMS bring-up.
#[cfg(exec_backend)]
pub fn spawn_blocking_with_priority<F, R>(_priority: ThreadPriority, f: F) -> TaskHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    use crate::runtime::background::future_exec::{DEFAULT_SPAWN_PRIORITY, spawn_blocking_on};
    spawn_blocking_on(
        &background().callbacks().handle(),
        DEFAULT_SPAWN_PRIORITY,
        f,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn() {
        let handle = spawn(async { 42 });
        assert_eq!(handle.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_spawn_blocking() {
        let handle = spawn_blocking(|| 123);
        assert_eq!(handle.await.unwrap(), 123);
    }

    #[tokio::test]
    async fn test_sleep() {
        let start = std::time::Instant::now();
        sleep(Duration::from_millis(10)).await;
        assert!(start.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn priority_named_levels_match_epics_thread_h() {
        // epicsThread.h:73-83 named-level constants.
        assert_eq!(ThreadPriority::Low.value(), 10);
        assert_eq!(ThreadPriority::CaServerLow.value(), 20);
        assert_eq!(ThreadPriority::CaServerHigh.value(), 40);
        assert_eq!(ThreadPriority::Medium.value(), 50);
        assert_eq!(ThreadPriority::ScanLow.value(), 60);
        assert_eq!(ThreadPriority::ScanHigh.value(), 70);
        assert_eq!(ThreadPriority::High.value(), 90);
        assert_eq!(ThreadPriority::Iocsh.value(), 91);
    }

    #[test]
    fn priority_ordering_ca_server_below_scan() {
        // Real-time invariant: scan threads must outrank CA-server
        // threads so scans preempt the CA server on a loaded IOC.
        assert!(ThreadPriority::CaServerHigh.value() < ThreadPriority::ScanLow.value());
        assert!(ThreadPriority::CaServerLow.value() < ThreadPriority::ScanLow.value());
    }

    #[test]
    fn priority_custom_clamps_to_max() {
        assert_eq!(ThreadPriority::Custom(200).value(), PRIORITY_MAX);
        assert_eq!(ThreadPriority::Custom(99).value(), 99);
        assert_eq!(ThreadPriority::Custom(0).value(), PRIORITY_MIN);
    }

    #[test]
    fn stack_size_classes_ordered() {
        // STACK_SIZE table is strictly increasing Small < Medium < Big.
        assert!(StackSizeClass::Small.bytes() < StackSizeClass::Medium.bytes());
        assert!(StackSizeClass::Medium.bytes() < StackSizeClass::Big.bytes());
        // Small = 0x10000 * sizeof(usize).
        assert_eq!(
            StackSizeClass::Small.bytes(),
            0x10000 * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn apply_priority_returns_a_defined_outcome() {
        // The result depends on the platform + permissions of the test
        // host; we only assert it is one of the defined outcomes and
        // does not panic. On a CI box without CAP_SYS_NICE this is
        // typically BestEffortFailed — which is C-parity behaviour.
        let outcome = apply_to_current_thread(ThreadPriority::ScanHigh);
        assert!(matches!(
            outcome,
            PriorityApplied::Realtime
                | PriorityApplied::Unsupported
                | PriorityApplied::BestEffortFailed
        ));
    }

    #[tokio::test]
    async fn spawn_blocking_with_priority_runs_closure() {
        let handle = spawn_blocking_with_priority(ThreadPriority::CaServerHigh, || 7);
        assert_eq!(handle.await.unwrap(), 7);
    }

    #[test]
    fn background_global_inits_and_runs_work() {
        // Host-exercises the OnceLock init path the RTEMS spawn/sleep/interval
        // arms rely on: background_init() forces creation, background() hands
        // back a usable executor whose callback pool runs submitted work.
        background_init();
        let exec = background();
        let (tx, rx) = std::sync::mpsc::channel();
        exec.callbacks()
            .handle()
            .request(
                crate::runtime::background::CallbackPriority::Medium,
                Box::new(move || tx.send(1u8).unwrap()),
            )
            .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    }
}
