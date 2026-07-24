// RTEMS-EXEC-MODEL-ALLOW(9): checked - these run and pass in the feature-ON suite.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;
use tokio::runtime::RuntimeFlavor;

pub use tokio::runtime::Handle as RuntimeHandle;

/// A synchronous caller asked to block on an async operation from a thread
/// where blocking cannot be made sound.
///
/// Both variants are the same defect seen through two executors: the calling
/// thread is one the awaited future needs in order to make progress, so parking
/// it parks the thing that would wake it. No blocking mechanism can fix that;
/// the caller has to `await` the async operation instead of blocking on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotBlockable {
    /// A **current-thread** tokio runtime is entered on this thread. Parking it
    /// stops every task on that runtime, including whichever one holds the
    /// state the awaited future is waiting for.
    CurrentThreadRuntime,
    /// This thread is a background-facility worker — a callback band, the
    /// delayed-callback timer, or the scanOnce worker
    /// ([`crate::runtime::background`]). Each facility has a bounded worker set
    /// and every unit of work it carries is enqueued for those workers, so a
    /// parked worker is waiting for work only it could have run. On RTEMS
    /// [`spawn`] routes here, which makes the callback bands the one other
    /// place where parking is unsound.
    BackgroundWorker,
}

impl std::fmt::Display for NotBlockable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotBlockable::CurrentThreadRuntime => {
                f.write_str("cannot block a current-thread runtime")
            }
            NotBlockable::BackgroundWorker => {
                f.write_str("cannot block a background-facility worker thread")
            }
        }
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
/// four caller contexts are not interchangeable and picking one mechanism for
/// all of them is what makes such bridges panic:
///
/// - **A background-facility worker** —
///   [`Err(BackgroundWorker)`](NotBlockable::BackgroundWorker), checked first,
///   because it is a property of the *thread* and holds whatever runtime is or
///   is not entered on it. See
///   [`background::facility::on_facility_thread`](crate::runtime::background)
///   for why parking one is unsound.
/// - **No runtime entered** (a plain `std::thread`, an iocsh thread) — park the
///   thread. Nothing else runs here, so there is nothing to starve; the tasks
///   that will wake us live on some other runtime's threads.
/// - **Multi-thread runtime worker** — [`tokio::task::block_in_place`], which
///   hands this worker's remaining tasks to a sibling before it is parked.
/// - **Current-thread runtime** —
///   [`Err(CurrentThreadRuntime)`](NotBlockable::CurrentThreadRuntime). Parking
///   the only thread of that runtime halts every task on it, including the one
///   that would wake us.
///
/// The two refusals are reported to the caller rather than panicked on (today)
/// or deadlocked on (the worse alternative) — an illegal blocking bridge is a
/// value the caller must handle, not a review item.
pub fn block_on_sync<F: Future>(fut: F) -> Result<F::Output, NotBlockable> {
    if crate::runtime::background::facility::on_facility_thread() {
        return Err(NotBlockable::BackgroundWorker);
    }
    match RuntimeHandle::try_current() {
        Ok(handle) => match handle.runtime_flavor() {
            RuntimeFlavor::CurrentThread => Err(NotBlockable::CurrentThreadRuntime),
            _ => Ok(tokio::task::block_in_place(|| handle.block_on(fut))),
        },
        Err(_) => Ok(park_on(fut)),
    }
}

/// A capability, captured where the backend's executor is reachable, to run
/// async work from a plain blocking thread (iocsh, a REPL, a script thread).
///
/// [`block_on_sync`] answers "may I block *here*, now?" per call and can only
/// use whatever runtime is visible on the calling thread. This type answers
/// the reachability question once, at [`capture`](Self::capture) time, and
/// carries the answer to a thread the runtime is otherwise invisible from: a
/// tokio handle is thread-local state, so a blocking thread spawned *before*
/// it exists has no way to find it. The exec backend's executor is
/// process-global, so there is nothing to carry and the bridge is a ZST —
/// which is what makes an API taking a `BlockingBridge` compile and work on
/// both backends, where one taking `tokio::runtime::Handle` pinned every
/// caller to tokio.
#[cfg(tokio_backend)]
#[derive(Clone)]
pub struct BlockingBridge {
    handle: tokio::runtime::Handle,
}

/// See the `tokio_backend` definition. The executor here is the
/// process-global background executor, reachable from any thread, so there is
/// no state to capture.
#[cfg(exec_backend)]
#[derive(Clone)]
pub struct BlockingBridge;

#[cfg(tokio_backend)]
impl BlockingBridge {
    /// Capture the current tokio runtime.
    ///
    /// # Panics
    /// Panics when no runtime is entered on this thread — call it on the
    /// async setup path (where the runtime is known), not on the blocking
    /// thread the bridge is being made for.
    pub fn capture() -> Self {
        Self {
            handle: tokio::runtime::Handle::current(),
        }
    }

    /// Drive `fut` to completion on this thread, with the captured runtime
    /// entered so the future may spawn and use the reactor.
    ///
    /// # Panics
    /// Panics on a runtime worker thread: blocking one parks tasks that may
    /// include the future's own wakers (the same refusal `block_on_sync`
    /// reports as a value).
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        assert!(
            RuntimeHandle::try_current().is_err(),
            "BlockingBridge::block_on must not be called from a runtime thread"
        );
        self.handle.block_on(fut)
    }

    /// Spawn `future` onto the captured runtime — [`spawn`] for a thread the
    /// runtime is not entered on.
    pub fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }
}

#[cfg(exec_backend)]
impl BlockingBridge {
    /// The exec backend's executor is process-global; capturing is a no-op
    /// and never panics.
    pub fn capture() -> Self {
        Self
    }

    /// Drive `fut` on this thread via [`park_on`]; whatever it spawns or
    /// sleeps on lands on the background executor.
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        park_on(fut)
    }

    /// [`spawn`] — the global executor needs no captured state.
    pub fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        spawn(future)
    }
}

/// Drive an async test body to completion — the driver behind
/// `#[epics_test]` (`epics-macros-rs`).
///
/// The point of the indirection is that the *backend* picks the driver, not
/// the test. On `tokio_backend` this builds exactly what `#[tokio::test]`
/// builds: a fresh current-thread runtime with IO and time enabled. On
/// `exec_backend` (the RTEMS target, or a host run with
/// `--features rtems-exec-model`) no tokio runtime exists to build, so the
/// test thread itself drives the future via [`park_on`], and everything the
/// body spawns or sleeps on lands on the process-global background executor
/// (lazily initialised on first use) — the same seam the RTEMS boot path
/// exercises. A test written with `#[epics_test]` therefore needs no
/// per-backend gating and no `RTEMS-EXEC-MODEL-ALLOW` census entry.
#[cfg(tokio_backend)]
pub fn test_block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio test runtime")
        .block_on(fut)
}

/// `exec_backend` twin of [`test_block_on`]: see the `tokio_backend` copy for
/// the contract. The body's awaits must reach only runtime-agnostic
/// primitives (`park_on`'s rule) — a body that touches `tokio::net` or
/// `tokio::time` directly belongs under `#[tokio::test]` with a backend gate
/// instead.
#[cfg(exec_backend)]
pub fn test_block_on<F: Future>(fut: F) -> F::Output {
    park_on(fut)
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

/// `true` when [`spawn`] lands the future on the tokio runtime, `false` when it
/// lands on the reactor-free background executor (`exec_backend` — the RTEMS
/// target, or a host build with `--features rtems-exec-model`).
///
/// # What this is for
///
/// It is the *exported* form of `build.rs`'s backend decision, and the reason
/// it is exported is that a spawned future's access to a tokio **reactor** is
/// decided here and consumed in other crates. A future handed to [`spawn`] on
/// `exec_backend` runs on a callback-pool worker with no reactor entered, so
/// every `tokio::net` socket it opens panics — *even in a process that has a
/// tokio runtime somewhere else*, because the runtime is not entered on that
/// worker.
///
/// `epics-ca-rs` and `epics-pva-rs` therefore have to make the same decision
/// this crate makes, for their own compilation, and they make it in their own
/// `build.rs` from the same two inputs (target OS, `rtems-exec-model` feature).
/// That is three copies of one rule, so each of them pins the copy against this
/// constant with a `const` assertion — a build where the two disagree (say,
/// `epics-base-rs/rtems-exec-model` enabled without `epics-ca-rs`'s) fails to
/// compile instead of panicking at boot.
pub const HAS_TOKIO_REACTOR: bool = cfg!(tokio_backend);

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

/// Yield the current task once — the seam replacement for
/// `tokio::task::yield_now`, so no call site names `tokio::task` directly.
#[cfg(tokio_backend)]
pub async fn yield_now() {
    tokio::task::yield_now().await;
}

/// Exec-backend yield: return `Pending` once with the waker already woken.
/// On the cooperative background executor that re-enqueues the task behind
/// whatever else is runnable; under a `park_on` driver the wake sets the
/// park token, so the driver re-polls immediately — both give one fair
/// scheduling point, which is all `yield_now` promises.
#[cfg(exec_backend)]
pub async fn yield_now() {
    struct YieldNow(bool);
    impl Future for YieldNow {
        type Output = ();
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
    YieldNow(false).await;
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

/// A set of spawned tasks, joined as they complete — the seam replacement for
/// `tokio::task::JoinSet`.
///
/// `JoinSet` is a **fourth spelling of `tokio::spawn`**, and the one no seam
/// guard caught: `JoinSet::spawn` calls `tokio::spawn` internally, so it panics
/// with *"there is no reactor running"* on any thread that is not inside a
/// tokio runtime — which on RTEMS is every callback-band worker. Measured on
/// target: the CA client's transport manager died on `cbMedium` at its first
/// connect (`doc/calink-rtems-design.md` §11.1). Naming it here means a call
/// site can express "spawn a set of tasks and reap them as they finish"
/// without reaching past the seam.
///
/// The three properties call sites depend on, all preserved:
///
/// * **Concurrency** — every member runs independently; joining one does not
///   block the others.
/// * **Pair-by-value** — [`Self::join_next`] yields whichever member finished
///   first, so a task that returns its own key can be matched to its state.
/// * **Abort on drop** — dropping the set cancels every member that has not
///   finished, which is the property that distinguishes a `JoinSet` from a bag
///   of detached `JoinHandle`s.
pub struct TaskSet<T> {
    tasks: Vec<TaskHandle<T>>,
}

impl<T> Default for TaskSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TaskSet<T> {
    /// An empty set.
    pub fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    /// Number of members that have not yet been joined.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// `true` when no member is outstanding.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

impl<T: Send + 'static> TaskSet<T> {
    /// Spawn `future` into the set — through [`spawn`], so the RTEMS build
    /// lands it on a callback band instead of demanding a tokio runtime.
    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = T> + Send + 'static,
    {
        self.tasks.push(spawn(future));
    }

    /// Wait for the next member to finish and return its result, removing it
    /// from the set. `None` when the set is empty — matching
    /// `JoinSet::join_next`, so a `select!` arm on it goes quiet rather than
    /// spinning once every task has been reaped.
    ///
    /// Cancel-safe: the returned future holds no state of its own, so a
    /// `select!` that drops it loses nothing.
    pub async fn join_next(&mut self) -> Option<Result<T, TaskJoinError>> {
        if self.tasks.is_empty() {
            return None;
        }
        std::future::poll_fn(|cx| {
            for i in 0..self.tasks.len() {
                // Both backends' handles are `Unpin` (tokio's `JoinHandle`,
                // and `JoinFuture`, whose only field is an `Arc`), so this
                // needs no pin projection. Polling every pending member
                // re-registers this waker with each — the shape both handles
                // document.
                if let std::task::Poll::Ready(result) =
                    std::pin::Pin::new(&mut self.tasks[i]).poll(cx)
                {
                    self.tasks.swap_remove(i);
                    return std::task::Poll::Ready(Some(result));
                }
            }
            std::task::Poll::Pending
        })
        .await
    }
}

impl<T> Drop for TaskSet<T> {
    /// Cancel every outstanding member — `JoinSet`'s drop behaviour, and the
    /// reason a call site reaches for a set rather than a `Vec` of handles.
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
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

/// The instant [`sleep_until`] measures deadlines against — **the backend's own
/// clock**, which is the whole point of naming it here.
///
/// A deadline is only meaningful in the clock the timer that waits on it runs
/// on. The hosted timer is tokio's, and under `#[tokio::test(start_paused =
/// true)]` tokio's clock is virtual and advances on `sleep`, not with the wall
/// — so a `std::time::Instant` deadline handed to a tokio timer is a deadline
/// in a *different* timeline, and the wait is wrong by however far the two have
/// diverged. The RTEMS timer runs on `std::time::Instant` (1-second-quantized
/// on target, `doc/calink-rtems-design.md` §5.5).
///
/// Taking the alias rather than a concrete instant type is what keeps a caller
/// from mixing them: `Instant::now() + timeout` is the deadline `sleep_until`
/// will actually honour, on both backends.
#[cfg(tokio_backend)]
pub type Instant = tokio::time::Instant;
/// See the hosted definition.
#[cfg(exec_backend)]
pub type Instant = std::time::Instant;

#[cfg(tokio_backend)]
pub async fn sleep_until(deadline: Instant) {
    tokio::time::sleep_until(deadline).await;
}

/// RTEMS: sleep-until on the delayed-callback timer via the host-tested `Sleep`.
#[cfg(exec_backend)]
pub async fn sleep_until(deadline: Instant) {
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

/// The timeout's error — "the deadline elapsed before the future completed".
/// Hosted this is tokio's own type so `timeout` composes with tokio-aware
/// callers; the exec backend substitutes a mirror (same Decision-B alias
/// pattern as `TaskJoinError` — the consumed surface is Debug/Display only).
#[cfg(tokio_backend)]
pub use tokio::time::error::Elapsed;

/// Exec-backend mirror of [`tokio::time::error::Elapsed`] (that type has no
/// public constructor, so the runtime-free `timeout` below cannot return it).
#[cfg(exec_backend)]
#[derive(Debug)]
pub struct Elapsed(());

#[cfg(exec_backend)]
impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline has elapsed")
    }
}

#[cfg(exec_backend)]
impl std::error::Error for Elapsed {}

/// Await `fut`, giving up after `duration` — the seam's only bounded wait.
///
/// It belongs here rather than at the call sites for the same reason [`sleep`]
/// does: a deadline needs a timer, and which timer that is depends on the
/// backend. Call sites that reach for `tokio::time::timeout` directly pin
/// themselves to the tokio timer wheel.
#[cfg(tokio_backend)]
pub async fn timeout<F: Future>(duration: Duration, fut: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout(duration, fut).await
}

/// RTEMS/exec: the same bounded wait raced against the delayed-callback
/// timer's [`sleep`] — no tokio timer wheel, no runtime.
#[cfg(exec_backend)]
pub async fn timeout<F: Future>(duration: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let mut sleep = std::pin::pin!(sleep(duration));
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(move |cx| {
        if let Poll::Ready(v) = fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(v));
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Elapsed(())));
        }
        Poll::Pending
    })
    .await
}

/// [`timeout`] against an absolute [`Instant`] instead of a duration — the
/// seam's `tokio::time::timeout_at`.
///
/// It exists because one deadline shared across several sequential awaits is a
/// different bound from a fresh duration per await: `caget_many` gives its
/// whole batch one deadline, so a slow first PV eats the budget the rest would
/// otherwise each get in full. Expressing that with `timeout` would need the
/// caller to do the subtraction, which is the arithmetic that drifts.
#[cfg(tokio_backend)]
pub async fn timeout_at<F: Future>(deadline: Instant, fut: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout_at(deadline, fut).await
}

/// RTEMS/exec: raced against [`sleep_until`], the absolute-deadline twin of
/// what [`timeout`] races against.
#[cfg(exec_backend)]
pub async fn timeout_at<F: Future>(deadline: Instant, fut: F) -> Result<F::Output, Elapsed> {
    let mut sleep = std::pin::pin!(sleep_until(deadline));
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(move |cx| {
        if let Poll::Ready(v) = fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(v));
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Elapsed(())));
        }
        Poll::Pending
    })
    .await
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
//
// (c) is opt-in and off by default — see `RT_PRIORITY_ENV`. C's switch
// (`EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING`) defaults to YES because
// a C IOC is deployed onto a machine chosen for it; this crate is just as
// often linked into a desktop tool, where a silent SCHED_FIFO request is
// either a guaranteed failure or a way to starve the box.
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
    ///
    /// `const` so a server can *derive* its band from the named one C derives
    /// it from — `CaServerLow - 2` rather than a bare `18` with a comment
    /// asserting the two are the same number. C builds exactly that ladder at
    /// `caservertask.c:562-575`; restating its output as a literal is how the
    /// ladder and its constants come to disagree.
    pub const fn value(self) -> u8 {
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
        // `Ord::min` is not `const`; the clamp is the same one.
        if v > PRIORITY_MAX { PRIORITY_MAX } else { v }
    }
}

/// Stack-size class — `epicsThreadStackSizeClass` (`epicsThread.h:91`).
///
/// The byte size is implementation-dependent in C. These mirror the POSIX
/// table `STACK_SIZE(f) = f * 0x10000 * sizeof(void*)`
/// (`libcom/src/osi/os/posix/osdThread.c:506-509`), pointer-width
/// parameterised exactly as the C macro is: Small = 1, Medium = 2, Big = 4
/// units of `0x10000 * sizeof(void*)`. On a 64-bit host that is
/// 512 KiB / 1 MiB / 2 MiB; on `armv7-rtems-eabihf` it is
/// **256 KiB / 512 KiB / 1 MiB**.
///
/// # This is the table a C IOC on RTEMS 6 uses too
///
/// Base has a second, much smaller table — 5000 / 8000 / 11000 bytes, floored
/// at `RTEMS_MINIMUM_STACK_SIZE` — in `os/RTEMS-score/osdThread.c:136-150`.
/// It does not apply here. `configure/toolchain.c:29-35` selects
/// `OS_API = posix` for `__RTEMS_MAJOR__ >= 5`, so an RTEMS 6 build searches
/// `os/RTEMS-posix` then `os/RTEMS`, neither of which contains an
/// `osdThread.c`, and lands on `os/posix/osdThread.c` — the file above. The
/// score table is what RTEMS **4/5** used.
///
/// Do not "align" these constants with 5000/8000/11000. That would be a
/// regression against the C IOC we are matching, not a correction: on RTEMS 6
/// the C IOC asks `pthread_attr_setstacksize` for the POSIX number
/// (`os/posix/osdThread.c:212-215`), which is the same call `std` makes for
/// us, with the same argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackSizeClass {
    Small,
    Medium,
    Big,
}

impl StackSizeClass {
    /// Stack size in bytes for this class, matching the POSIX
    /// `stackSizeTable` in `osdThread.c` on **every** target.
    ///
    /// C's `STACK_SIZE(f) = f * 0x10000 * sizeof(void*)` is parameterised by
    /// pointer width and so is this, so the two agree by construction rather
    /// than on one word size: 512 KiB / 1 MiB / 2 MiB on a 64-bit host, and
    /// 256 KiB / 512 KiB / 1 MiB on `armv7-rtems-eabihf` — which is exactly
    /// what a C IOC asks `pthread_attr_setstacksize` for on that target.
    ///
    /// Read "on a 64-bit target" here before: it was wrong in the direction
    /// that matters, because the only target this crate is portable *to* is
    /// 32-bit, and it invited a reader to assume the RTEMS numbers were
    /// unverified.
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
    /// Real-time scheduling was never requested: the opt-in switch
    /// [`RT_PRIORITY_ENV`] is off, so **no scheduler call was made at
    /// all** and the thread keeps the process default policy.
    Disabled,
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

/// Environment switch that opts this process in to real-time (SCHED_FIFO)
/// scheduling for the IOC threads that carry an EPICS priority.
///
/// The switch is read in both directions on every target; what differs is
/// what it defaults to when unset — see [`DEFAULT_POLICY`].
///
/// Accepted "on" values, case-insensitive: `YES`, `TRUE`, `ON`, `1`.
/// Any other *explicit* value is off.
///
/// # Relationship to the C switch
///
/// C base has the same concept under
/// `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING` (`envDefs.h:80`, read at
/// `osdThread.c:389`), and `envGetBoolConfigParam` (`envSubr.c:331`) accepts
/// only case-insensitive `yes`. We deliberately do **not** reuse that name:
/// its base default is `YES` on every target (`configure/CONFIG_ENV:57`)
/// while ours is `YES` only on RTEMS, so one name would carry two different
/// defaults on a hosted build depending on which implementation read it.
pub const RT_PRIORITY_ENV: &str = "EPICS_RS_ALLOW_RT_PRIORITY";

/// Whether this process may ask the OS for real-time scheduling.
///
/// Resolved from [`RT_PRIORITY_ENV`] exactly once per process by
/// [`RtPolicy::current`]. It is a *parameter* of
/// [`apply_to_current_thread_under`] rather than a check buried inside the
/// syscall wrapper, so "switch off ⟹ no scheduler call" is a property of
/// the call graph and not of a runtime branch some future caller can skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtPolicy {
    /// Never touch the OS scheduler.
    Disabled,
    /// Best-effort SCHED_FIFO, falling back to default scheduling.
    AllowRealtime,
}

/// What [`RT_PRIORITY_ENV`] means when it is **unset**, for the target this
/// was compiled for.
///
/// `AllowRealtime` on RTEMS, `Disabled` everywhere else. The asymmetry is
/// deliberate and is a property of the default itself — not a `setenv` the
/// boot shim performs before `main()`. A `setenv` is a runtime side effect any
/// later caller can undo or reorder, and a variable one component writes for
/// another to read is the dual-meaning shape this code keeps removing.
///
/// Why the two targets differ:
///
///   - Base's own equivalent switch,
///     `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING`, defaults to `YES`
///     (`configure/CONFIG_ENV:57`). An IOC that honours its priorities is
///     upstream's default posture, not an opt-in.
///   - The opt-in gate exists for RT-Linux, where asking for SCHED_FIFO needs
///     `CAP_SYS_NICE` or a non-zero `RLIMIT_RTPRIO` (so on a desktop the
///     request merely fails), and where a runaway RT band on a box that
///     *grants* it can wedge a developer's machine. Neither failure mode
///     exists on RTEMS: there is no RLIMIT_RTPRIO and no desktop to wedge.
///   - The band invariant is now a test rather than a hope —
///     `rtems_priority_map_stays_below_the_libbsd_network_band` proves every
///     u8 input lands in core 100..199, at or below libbsd's default band and
///     strictly less urgent than IRQS(96)/TIME(98).
///
/// An explicit value still wins in **both** directions on both targets, so
/// `EPICS_RS_ALLOW_RT_PRIORITY=NO` turns it off on RTEMS.
pub const DEFAULT_POLICY: RtPolicy = default_policy(cfg!(target_os = "rtems"));

/// [`DEFAULT_POLICY`] as a pure function of the one target fact it depends
/// on, so both arms are reachable from a host test run. A host CI will never
/// execute the RTEMS arm otherwise, and an untested default is exactly the
/// kind that drifts.
const fn default_policy(on_rtems: bool) -> RtPolicy {
    if on_rtems {
        RtPolicy::AllowRealtime
    } else {
        RtPolicy::Disabled
    }
}

impl RtPolicy {
    /// Parse a raw switch value (`None` = unset ⇒ [`DEFAULT_POLICY`]).
    pub fn from_env_value(raw: Option<&str>) -> RtPolicy {
        Self::resolve(raw, DEFAULT_POLICY)
    }

    /// [`Self::from_env_value`] with the unset-default injected, so a host
    /// test can ask what an RTEMS process would do with the same input.
    pub fn resolve(raw: Option<&str>, default: RtPolicy) -> RtPolicy {
        let Some(raw) = raw else {
            return default;
        };
        let v = raw.trim();
        let on = v.eq_ignore_ascii_case("yes")
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("on")
            || v == "1";
        if on {
            RtPolicy::AllowRealtime
        } else {
            RtPolicy::Disabled
        }
    }

    /// The process-wide policy, read from [`RT_PRIORITY_ENV`] on first use
    /// and cached. Caching matches C, which resolves its switch once in
    /// `epicsThreadInit` (`osdThread.c:389`), and keeps later `set_var`
    /// calls from changing the scheduling of threads already running.
    pub fn current() -> RtPolicy {
        static POLICY: std::sync::OnceLock<RtPolicy> = std::sync::OnceLock::new();
        *POLICY.get_or_init(|| {
            RtPolicy::from_env_value(std::env::var(RT_PRIORITY_ENV).ok().as_deref())
        })
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
/// Platform support: the OS-scheduler change is wired on Linux, via the
/// range-probed linear map of `os/posix/osdThread.c`, and on RTEMS, via the
/// fixed map of `os/RTEMS-score/osdThread.c` inverted into POSIX space (see
/// `map_epics_priority_rtems` — the two maps differ in shape, deliberately).
/// On other targets the priority enum + API surface still exist but `apply`
/// reports [`PriorityApplied::Unsupported`] — no band has been measured there.
///
/// Opt-in: real-time scheduling is only ever requested when
/// [`RT_PRIORITY_ENV`] is set (see [`RtPolicy`]). With the switch off this
/// returns [`PriorityApplied::Disabled`] without calling the OS at all.
pub fn apply_to_current_thread(priority: ThreadPriority) -> PriorityApplied {
    apply_to_current_thread_under(RtPolicy::current(), priority)
}

/// [`apply_to_current_thread`] with the real-time policy supplied by the
/// caller instead of read from the environment.
///
/// The single gate: [`RtPolicy::Disabled`] returns before any scheduler
/// call is reachable. Exposed so a caller that already owns its RT policy
/// (and the tests that must exercise both states in one process, since
/// [`RtPolicy::current`] is cached) does not have to mutate the environment.
pub fn apply_to_current_thread_under(
    policy: RtPolicy,
    priority: ThreadPriority,
) -> PriorityApplied {
    match policy {
        RtPolicy::Disabled => PriorityApplied::Disabled,
        RtPolicy::AllowRealtime => apply_priority_impl(priority.value()),
    }
}

/// The prologue an IOC thread runs as its first statement, when it takes on
/// its role: publish its name to the OS, then request its scheduling band.
///
/// Two things a thread owes the operator, and they have different gates.
/// The band is opt-in ([`RT_PRIORITY_ENV`]) and best effort. The **name** is
/// unconditional: a thread that cannot be identified in a task listing
/// cannot be diagnosed, and on RTEMS that listing is often the only
/// instrument there is — bring-up had to measure libbsd's priority band by
/// other means precisely because none of our threads carried a name the
/// kernel could show.
///
/// Use this rather than [`apply_to_current_thread`] at a thread's entry, so
/// naming cannot be forgotten by the next thread somebody adds. Call
/// [`apply_to_current_thread`] directly only when re-banding a thread that
/// is already named and running. A thread that deliberately takes no EPICS
/// band — the iocsh script runners — calls [`name_current_thread`] alone
/// rather than inventing a priority just to be visible.
///
/// The band this asks the OS for is also what orders the blocking locks in
/// `server::database::record_lock` and its siblings: they are
/// priority-inheritance mutexes, so the wait queue is the *kernel's* and it
/// is ranked by the scheduling priority requested here. With
/// [`RtPolicy::Disabled`] no scheduler call happens and there is no ordering
/// to have — the hosted default, where the locks still exclude but do not
/// prioritise ([`crate::runtime::sync::is_pi_mutex_active`]).
pub fn enter_ioc_thread(priority: ThreadPriority) -> PriorityApplied {
    name_current_thread();
    apply_to_current_thread(priority)
}

/// Push `std::thread::current().name()` down to the OS thread object.
///
/// No-op off RTEMS: `std` already calls the platform's `pthread_setname_np`
/// from `Builder::spawn` on every hosted target it supports. RTEMS is not in
/// that list, so a name set with `Builder::name` lives only in Rust's own
/// `Thread` struct and never reaches the kernel — which is what makes our
/// threads invisible to an RTEMS task listing.
#[cfg(not(target_os = "rtems"))]
pub fn name_current_thread() {}

/// RTEMS: `pthread_setname_np` (`cpukit/posix/src/pthreadsetnamenp.c`) into
/// `_Thread_Set_name`, which `strlcpy`s into `_Thread_Maximum_name_size`.
///
/// That size is `CONFIGURE_MAXIMUM_THREAD_NAME_SIZE`, default **16**
/// including the NUL (`rtems/score/thread.h:1079`,
/// `rtems/confdefs/threads.h:92-93`), and the boot shim does not override
/// it — so 15 usable bytes, the same budget `std` truncates to on Linux
/// (`TASK_COMM_LEN`). Truncating here rather than letting the kernel do it
/// keeps that existing rule and keeps the call's success unambiguous:
/// `_Thread_Set_name` still *sets* an over-long name, it just also returns
/// `STATUS_RESULT_TOO_LARGE` → `ERANGE`, so an untruncated call would report
/// failure for a name it had in fact applied.
#[cfg(target_os = "rtems")]
pub fn name_current_thread() {
    let current = std::thread::current();
    let Some(name) = current.name() else {
        return;
    };
    let Ok(c_name) = std::ffi::CString::new(truncate_thread_name(name)) else {
        // An interior NUL cannot come from `Builder::name`, which takes a
        // `String`; nothing to publish if one ever did.
        return;
    };
    // SAFETY: `pthread_setname_np` acts on the calling thread and reads a
    // NUL-terminated string that outlives the call.
    let rc =
        unsafe { rtems_sched::pthread_setname_np(rtems_sched::pthread_self(), c_name.as_ptr()) };
    if rc != 0 {
        tracing::debug!(
            target: "epics_base_rs::runtime",
            thread = name,
            errno = rc,
            "pthread_setname_np failed; thread stays unnamed in the task listing"
        );
    }
}

/// `CONFIGURE_MAXIMUM_THREAD_NAME_SIZE` (default 16) minus the NUL.
#[cfg(any(target_os = "rtems", test))]
const RTEMS_MAX_THREAD_NAME_BYTES: usize = 15;

/// Cut a thread name to what an RTEMS thread object can hold, on a UTF-8
/// boundary.
///
/// Byte budget, not character count — `_Thread_Set_name` `strlcpy`s bytes —
/// but never mid-codepoint, or the task listing shows invalid UTF-8. Kept as
/// a pure function so the rule is testable on the host, where the caller
/// that applies it does not exist.
#[cfg(any(target_os = "rtems", test))]
fn truncate_thread_name(name: &str) -> &str {
    let mut end = name.len().min(RTEMS_MAX_THREAD_NAME_BYTES);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// The SCHED_FIFO priority range this process may actually enter.
///
/// C parity: `find_pri_range` (`osdThread.c:259-314`). The kernel's
/// `sched_get_priority_max` reports the *policy's* range and ignores
/// `RLIMIT_RTPRIO`, so on an RT box with a restricted limit the nominal
/// range is wider than the usable one; C binary-searches for the real
/// ceiling and so do we.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RtRange {
    /// The kernel does not report a SCHED_FIFO range at all.
    Unsupported,
    /// The range exists but this process may not enter it — no
    /// `CAP_SYS_NICE` and `RLIMIT_RTPRIO` is 0. C's equivalent is
    /// `usePolicy == 0` (`osdThread.c:279-285`, `:331`), which makes it
    /// stop asking for SCHED_FIFO for the life of the process.
    Denied,
    /// Priorities `min..=max` are settable.
    Available { min: i32, max: i32 },
}

/// Probe once per process and cache. Only reachable on the
/// [`RtPolicy::AllowRealtime`] path, so a default (switch-off) process
/// never runs the probe and never makes a scheduler call.
#[cfg(target_os = "linux")]
fn permitted_fifo_range() -> RtRange {
    static RANGE: std::sync::OnceLock<RtRange> = std::sync::OnceLock::new();
    *RANGE.get_or_init(probe_fifo_range)
}

#[cfg(target_os = "linux")]
fn probe_fifo_range() -> RtRange {
    // SAFETY: sched_get_priority_min/max take only an int policy and have
    // no preconditions.
    let (min, max) = unsafe {
        (
            libc::sched_get_priority_min(libc::SCHED_FIFO),
            libc::sched_get_priority_max(libc::SCHED_FIFO),
        )
    };
    if min < 0 || max < 0 || max < min {
        return RtRange::Unsupported;
    }

    // The probe *changes the scheduling of the thread that runs it*, so it
    // runs on a throwaway thread — exactly why C hands `find_pri_range` to
    // its own `pthread_create`/`pthread_join` pair (`osdThread.c:316-334`).
    let probe = std::thread::Builder::new()
        .name("cbRtProbe".to_string())
        // Two `sched_get_priority_*` calls and a `sched_setscheduler`; nothing
        // recurses. Linux-only, so this is not the RTEMS ceiling — but there is
        // no reason for a throwaway probe to reserve 2 MiB on any target.
        .stack_size(StackSizeClass::Small.bytes())
        .spawn(move || {
            // `osdThread.c:277-287`: failing at the minimum means no
            // permission for SCHED_FIFO at all.
            if set_fifo_priority(min) != 0 {
                return RtRange::Denied;
            }
            // `osdThread.c:296-307`: binary-search the real ceiling.
            let (mut low, mut high) = (min, max);
            while low < high {
                let mid = (high + low) / 2;
                if set_fifo_priority(mid) != 0 {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }
            // `osdThread.c:310`: `max_pri = try_pri(max) ? max-1 : max`.
            let top = if set_fifo_priority(high) != 0 {
                high - 1
            } else {
                high
            };
            RtRange::Available { min, max: top }
        });
    match probe.map(std::thread::JoinHandle::join) {
        Ok(Ok(range)) => range,
        // Cannot spawn, or the probe died: treat as no RT rather than
        // guessing a range we have not shown to be settable.
        _ => RtRange::Denied,
    }
}

/// Map an EPICS priority `0..=99` onto the permitted SCHED_FIFO range.
///
/// C parity: `epicsThreadGetPosixPriority` (`osdThread.c:129-144`) — the
/// POSIX counterpart of the `epicsThreadGetOssPriorityValue` used on
/// RTEMS/vxWorks (`RTEMS-score/osdThread.c:94`, `vxWorks/osdThread.c:99`).
///
/// **Hosted only.** RTEMS deliberately does not use this map — see
/// [`map_epics_priority_rtems`] for the shape and the reason. The `test`
/// arm of the cfg exists so the two maps can be compared in one process
/// on the host; without it the divergence test would silently vanish.
#[cfg(any(target_os = "linux", test))]
fn map_epics_priority(epics_priority: u8, min: i32, max: i32) -> i32 {
    // `osdThread.c:133-134`: a degenerate range collapses to one level.
    if max == min {
        return max;
    }
    let slope = (max - min) as f64 / 100.0;
    let oss = epics_priority as f64 * slope + min as f64;
    // `ThreadPriority::value` caps at 99 and the slope is over 100, so this
    // cannot exceed `max`; the clamp guards the probed bounds, which are
    // runtime values rather than compile-time constants.
    (oss as i32).clamp(min, max)
}

/// The highest RTEMS *core* priority number, i.e. the least urgent level.
///
/// Measured on the bring-up guest (RTEMS 6 + libbsd, QEMU
/// `xilinx_zynq_a9`): `RTEMS_MAXIMUM_PRIORITY == 255`, the idle thread runs
/// at core 255, and `sched_get_priority_min/max(SCHED_FIFO)` report `1`/`254`.
/// The POSIX-to-core inversion `core = 255 - posix` was verified in both
/// directions on that guest.
#[cfg(any(target_os = "rtems", test))]
const RTEMS_MAXIMUM_PRIORITY: i32 = 255;

/// The RTEMS *core* priority an EPICS priority must land on.
///
/// Verbatim `epicsThreadGetOssPriorityValue` from EPICS's own RTEMS port,
/// `libcom/src/osi/os/RTEMS-score/osdThread.c:94-102`:
///
/// ```c
/// int epicsThreadGetOssPriorityValue(unsigned int osiPriority)
/// {
///     if (osiPriority > 99) { return 100; }
///     else { return (199 - (signed int)osiPriority); }
/// }
/// ```
///
/// Fixed offsets, not a range-scaled slope. The whole EPICS space therefore
/// occupies core `100..=199` and nothing else can be reached — which is the
/// property [`map_epics_priority_rtems`] is chosen for.
#[cfg(any(target_os = "rtems", test))]
const fn rtems_core_priority(epics_priority: u8) -> i32 {
    if epics_priority > 99 {
        100
    } else {
        199 - epics_priority as i32
    }
}

/// Map an EPICS priority onto an RTEMS **POSIX** SCHED_FIFO priority.
///
/// A distinct function from [`map_epics_priority`] on purpose: the two have
/// different *shapes*, not different endpoints. Expressing this one as the
/// hosted linear map with `min`/`max` retuned would re-introduce the linear
/// map the moment somebody adjusted a constant, and the linear map is the
/// thing this arm exists to avoid.
///
/// **Deliberate deviation from base-on-RTEMS-6.** EPICS base compiles
/// `os/posix/osdThread.c` on RTEMS 6 — `configure/toolchain.c:31-36` sets
/// `OS_API = posix` for `__RTEMS_MAJOR__ >= 5`, and `os/RTEMS-posix/` ships
/// no `osdThread.c` — so upstream applies the *linear* map
/// `oss = epics*(max-min)/100 + min` over `find_pri_range`'s result, which
/// on this guest is `min=1`/`max=254`, with
/// `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING` defaulting to `YES`
/// (`configure/CONFIG_ENV:57`). That places EPICS 91 (the CA server band) at
/// posix 231, i.e. **core 24** — far above libbsd's network threads. The
/// crossover is EPICS **63** (posix 160, core 95): every EPICS priority at or
/// above it outranks the interrupt server. Reproducing that would reproduce
/// the hazard, so this port takes EPICS's *own* RTEMS answer instead —
/// [`rtems_core_priority`] — and inverts it into the POSIX space we actually
/// set:
///
/// ```text
/// core = RTEMS_MAXIMUM_PRIORITY - posix   (measured)
/// core = 199 - epics                      (RTEMS-score/osdThread.c:94-102)
/// ⟹ posix = 255 - (199 - epics) = 56 + epics
/// ```
///
/// So EPICS 0 → posix 56 → core 199, EPICS 99 → posix 155 → core 100, and
/// anything above 99 clamps to posix 155. Every value is inside the guest's
/// settable `[1, 254]`. **Measured on target**, core 100 is also where
/// libbsd's own twelve default-band worker threads sit, so the map's most
/// urgent reachable value *ties* libbsd's default band there rather than
/// staying strictly below it — a boundary tie by construction, not a
/// collision-free image. It is still strictly below `IRQS`(96)/`TIME`(98);
/// see `rtems_priority_map_stays_below_the_libbsd_network_band`, which
/// asserts the non-strict `core >= 100` this tie actually produces.
#[cfg(any(target_os = "rtems", test))]
fn map_epics_priority_rtems(epics_priority: u8) -> i32 {
    RTEMS_MAXIMUM_PRIORITY - rtems_core_priority(epics_priority)
}

/// Ask the OS for SCHED_FIFO at `oss` on the **calling** thread. The single
/// place this crate touches the scheduler; returns the raw `pthread_*`
/// status (0 on success). Counting lives here so the "switch off ⟹ no
/// scheduler call" guarantee is observable on every target that has one.
#[cfg(any(target_os = "linux", target_os = "rtems"))]
fn set_fifo_priority(oss: i32) -> i32 {
    SCHED_CALLS_MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    SCHED_CALLS.with(|c| c.set(c.get() + 1));
    set_fifo_priority_raw(oss)
}

#[cfg(target_os = "linux")]
fn set_fifo_priority_raw(oss: i32) -> i32 {
    let param = libc::sched_param {
        sched_priority: oss,
    };
    // SAFETY: pthread_setschedparam operates on the calling thread with a
    // stack-local sched_param and a valid policy constant.
    unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) }
}

/// The RTEMS scheduler surface, declared here rather than taken from `libc`.
///
/// `libc`'s `newlib/rtems` module (0.2.188) declares neither `sched_param`,
/// `SCHED_FIFO`, `pthread_setschedparam` nor `pthread_self` — its sibling
/// newlib targets `vita` and `horizon` declare all of them, RTEMS does not.
/// The functions exist in the RTEMS 6 kernel
/// (`cpukit/posix/src/pthreadsetschedparam.c`) and in the toolchain headers;
/// only the Rust binding is missing.
///
/// `libc::timespec` is deliberately NOT used to describe the sporadic-server
/// tail: `libc` types `time_t` as `i32` for every newlib target except
/// `horizon`/`espidf` (`src/unix/newlib/mod.rs:55-64`), while the arm-rtems6
/// toolchain has `sizeof(time_t) == 8`. Its `timespec` is therefore half the
/// real width on this target, so the tail is carried as opaque bytes sized
/// from the target compiler instead.
#[cfg(target_os = "rtems")]
mod rtems_sched {
    use std::ffi::c_int;

    /// `sys/sched.h`: `#define SCHED_FIFO 1`.
    pub const SCHED_FIFO: c_int = 1;

    /// `struct sched_param` as arm-rtems6 lays it out.
    ///
    /// `sys/features.h:404-405` defines both `_POSIX_SPORADIC_SERVER` and
    /// `_POSIX_THREAD_SPORADIC_SERVER`, so `sys/sched.h` compiles the
    /// sporadic-server tail in. Measured with the target compiler
    /// (`arm-rtems6-gcc`, `sizeof`/`offsetof` via array-length symbols):
    ///
    /// | field | offset | size |
    /// |-------|--------|------|
    /// | `sched_priority`        |  0 |  4 |
    /// | `sched_ss_low_priority` |  4 |  4 |
    /// | `sched_ss_repl_period`  |  8 | 16 |
    /// | `sched_ss_init_budget`  | 24 | 16 |
    /// | `sched_ss_max_repl`     | 40 |  4 |
    ///
    /// total 48, align 8. `SCHED_FIFO` makes the kernel read only
    /// `sched_priority` (`_POSIX_Thread_Translate_sched_param` takes the
    /// sporadic branch for `SCHED_SPORADIC` alone), but the struct is
    /// declared at full width anyway so the kernel is never handed a pointer
    /// to less memory than its own header describes.
    #[repr(C, align(8))]
    pub struct SchedParam {
        pub sched_priority: c_int,
        /// Offsets 4..48 — the sporadic-server fields, unused under
        /// `SCHED_FIFO` and always zeroed.
        pub sporadic_tail: [u8; 44],
    }

    // The whole point of the opaque tail is that the width is right. If a
    // future edit reaches for `libc::timespec` here, this stops the build
    // instead of silently handing the kernel a short buffer.
    const _: () = {
        assert!(core::mem::size_of::<SchedParam>() == 48);
        assert!(core::mem::align_of::<SchedParam>() == 8);
    };

    unsafe extern "C" {
        pub fn pthread_self() -> libc::pthread_t;
        pub fn pthread_setschedparam(
            thread: libc::pthread_t,
            policy: c_int,
            param: *const SchedParam,
        ) -> c_int;
        /// `cpukit/posix/src/pthreadsetnamenp.c`. Also absent from `libc`'s
        /// `newlib/rtems` module.
        pub fn pthread_setname_np(thread: libc::pthread_t, name: *const std::ffi::c_char) -> c_int;
    }
}

#[cfg(target_os = "rtems")]
fn set_fifo_priority_raw(oss: i32) -> i32 {
    let param = rtems_sched::SchedParam {
        sched_priority: oss,
        sporadic_tail: [0u8; 44],
    };
    // SAFETY: `pthread_setschedparam` acts on the calling thread, is handed a
    // stack-local `sched_param` of the target's own width (asserted above),
    // and a policy constant taken from `sys/sched.h`.
    unsafe {
        rtems_sched::pthread_setschedparam(
            rtems_sched::pthread_self(),
            rtems_sched::SCHED_FIFO,
            &param,
        )
    }
}

/// The unprivileged-fallback message. Emitted **once per process**: the
/// denial is a property of the process, not of the thread that happened to
/// notice it first, and an IOC creates a thread per CA client.
#[cfg(target_os = "linux")]
fn warn_rt_denied_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        #[cfg(test)]
        DENIED_WARNINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            target: "epics_base_rs::runtime",
            switch = RT_PRIORITY_ENV,
            "{RT_PRIORITY_ENV} asked for real-time scheduling, but this process may not \
             use SCHED_FIFO (needs CAP_SYS_NICE or a non-zero RLIMIT_RTPRIO). Every IOC \
             thread stays at the default scheduling policy; timing is not real-time. \
             Logged once per process."
        );
    });
}

#[cfg(target_os = "linux")]
fn apply_priority_impl(epics_priority: u8) -> PriorityApplied {
    let (min, max) = match permitted_fifo_range() {
        RtRange::Unsupported => return PriorityApplied::Unsupported,
        RtRange::Denied => {
            warn_rt_denied_once();
            return PriorityApplied::BestEffortFailed;
        }
        RtRange::Available { min, max } => (min, max),
    };
    let oss = map_epics_priority(epics_priority, min, max);
    let rc = set_fifo_priority(oss);
    if rc == 0 {
        PriorityApplied::Realtime
    } else {
        // The probe proved this range settable, so a failure here is not
        // the permission case the warning covers — keep it at debug.
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

#[cfg(target_os = "rtems")]
fn apply_priority_impl(epics_priority: u8) -> PriorityApplied {
    // Without this, every IOC thread runs at one level just above idle:
    // `cpukit/posix/src/pthreadattrdefault.c:49-58` sets
    // `inheritsched = PTHREAD_INHERIT_SCHED` in the default attribute set and
    // `std` never calls `pthread_attr_setinheritsched`, so a thread inherits
    // its creator's parameters — and every IOC thread descends from
    // `POSIX_Init`, which the boot shim deliberately lowers to
    // `RTEMS_MAXIMUM_PRIORITY - 1`. The CA receiver/sender ordering that stops
    // a stalled client starving command dispatch does not hold at one level.
    //
    // No range probe, unlike Linux. The probe exists there because
    // `sched_get_priority_max` reports the *policy's* range while
    // `RLIMIT_RTPRIO`/`CAP_SYS_NICE` decide the usable one, so the settable
    // ceiling has to be searched for. RTEMS has no such permission gate —
    // `pthread_setschedparam` (`cpukit/posix/src/pthreadsetschedparam.c`)
    // performs no privilege check — and this map's image is a fixed
    // `[56, 155]`, inside the measured settable `[1, 254]` by construction.
    // There is nothing to discover, and a probe thread would itself need a
    // band to run in.
    let oss = map_epics_priority_rtems(epics_priority);
    let rc = set_fifo_priority(oss);
    if rc == 0 {
        PriorityApplied::Realtime
    } else {
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

#[cfg(not(any(target_os = "linux", target_os = "rtems")))]
fn apply_priority_impl(_epics_priority: u8) -> PriorityApplied {
    // No OS-scheduler priority API is wired on other targets. The two that
    // are wired each needed a *measured* target band before they could be:
    // Linux probes for its settable ceiling at runtime, and RTEMS's map is
    // pinned against libbsd's network-thread band measured on the bring-up
    // guest. Neither number is guessable, so a new target gets `Unsupported`
    // until somebody measures it rather than a plausible-looking range.
    PriorityApplied::Unsupported
}

/// How many times this process has asked the OS scheduler for SCHED_FIFO,
/// across every thread — including the one-off range probe.
///
/// The observable form of the opt-in guarantee: with [`RT_PRIORITY_ENV`]
/// unset, a process can run its whole life and this stays `0`. Also answers
/// "did this IOC ever actually try to go real-time?" from a log line.
///
/// Always `0` off Linux, where no scheduler call is wired at all.
pub fn sched_calls_made() -> usize {
    SCHED_CALLS_MADE.load(std::sync::atomic::Ordering::Relaxed)
}

static SCHED_CALLS_MADE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, any(target_os = "linux", target_os = "rtems")))]
thread_local! {
    /// Every scheduler call this crate makes passes through
    /// [`set_fifo_priority`], which bumps this. Tests assert the delta is
    /// zero with the switch off — the property "switch off ⟹ no sched
    /// calls" observed directly rather than inferred from a return value.
    ///
    /// Per-thread, not global: `pthread_setschedparam` acts on the calling
    /// thread, so a per-thread count is the exact quantity, and a test
    /// cannot be perturbed by a concurrent one (the unit tests share a
    /// process under plain `cargo test`).
    static SCHED_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times the once-per-process denial warning was emitted.
#[cfg(all(test, target_os = "linux"))]
static DENIED_WARNINGS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
        let _ = enter_ioc_thread(priority);
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

/// Spawn a **dedicated OS thread** that runs `f` at `priority` with a `stack`
/// of [`StackSizeClass`], plus whatever ambient async context
/// [`block_on_sync`] needs on this target.
///
/// # Why the stack class is a parameter and not a default
///
/// C creates an IOC thread with `epicsThreadCreate(name, priority, stackSize,
/// fn, arg)` — three attributes. This seam carried the first two and let the
/// third fall through to whatever `std` picks, which is **2 MiB on RTEMS**:
/// `std/src/sys/thread/unix.rs` gates its `DEFAULT_MIN_STACK_SIZE` on
/// `not(any(l4re, vxworks, espidf, nuttx))`, and vxWorks got a 256 KiB
/// carve-out where RTEMS did not.
///
/// That is invisible on the host, where a thread stack is lazily-committed
/// virtual address space, and decisive on the target, where it is carved
/// eagerly out of a fixed pool. Making it a parameter is what stops a new
/// per-connection thread from silently costing 2 MiB: there is no default to
/// inherit, so every caller states what the thread is for.
///
/// Not [`spawn_blocking_with_priority`], and the difference is the point.
/// That one hands the closure to a *pool*: tokio's blocking pool on the host,
/// and on RTEMS a shared callback-pool worker that also drops the priority
/// (see its `exec_backend` arm). Both are right for work that finishes. A
/// server thread that lives as long as its connection would occupy a pool
/// worker for that whole time, so the pool is the wrong home for it — an IOC
/// thread that a C IOC would create with `epicsThreadCreate` wants a thread of
/// its own, and the priority a C IOC gives it.
///
/// # Why the ambient context is part of this, and not the caller's problem
///
/// `block_on_sync` picks its mechanism from the thread it is called on, but it
/// cannot *create* the context a future needs. On the host a fresh
/// `std::thread` has no runtime, so a future that spawns tasks or arms timers
/// panics with "there is no reactor running" the moment it is polled — even
/// though `block_on_sync` itself was perfectly happy to park. On RTEMS the
/// exec backend is process-global (`background_init`), so a bare thread is
/// already complete. That asymmetry is a property of the two backends, so it
/// is resolved here, at the seam, rather than by a `cfg` in every server that
/// wants a thread.
///
/// The captured context is whatever the *calling* thread is running under, so
/// call this from the runtime the work should belong to. When there is none
/// (RTEMS always; on the host a caller that is itself outside a runtime) the
/// thread simply runs without one, which is exactly right for a future whose
/// awaits are all runtime-agnostic.
///
/// # A current-thread ambient is not inherited, and that is the rule
///
/// [`RuntimeHandle::try_current`] answers two different questions with one
/// value: *"am I running on this runtime's thread"* and *"has this thread
/// merely entered this handle"*. [`block_on_sync`] cannot distinguish them, so
/// it must assume the first and refuse to park under a `CurrentThread` flavor —
/// correct on that runtime's own thread, where parking halts the task that
/// would wake you, and wrong on a dedicated thread, where it halts nothing.
///
/// So the dual meaning is removed here, at the one place a dedicated thread's
/// context is decided, rather than left for `block_on_sync` to guess: a
/// `CurrentThread` ambient is **not** inherited, and the thread runs with no
/// runtime — the `park_on` arm, which is sound for it and is the only arm RTEMS
/// ever takes.
///
/// Nothing is lost by declining it. What inheriting buys is stated above —
/// `spawn` and the timer inside `block_on_sync` — and under a `CurrentThread`
/// ambient `block_on_sync` returns
/// [`Err(CurrentThreadRuntime)`](NotBlockable::CurrentThreadRuntime), so every one of those
/// powers is unreachable anyway. Inheriting it can only convert a thread that
/// would have worked into one that cannot block at all. Measured as exactly
/// that: the PVA client's blocking byte pumps
/// (`runtime::blocking_io::spawn_pump`) are dedicated threads whose bodies are
/// pure `tokio::sync` channel traffic, and every `#[tokio::test]` that drives
/// one is `CurrentThread` by default — inheritance made the reader pump exit on
/// its first chunk and the connection read as "server closed during handshake".
#[cfg(tokio_backend)]
pub fn spawn_dedicated_thread<F>(
    name: String,
    priority: ThreadPriority,
    stack: StackSizeClass,
    f: F,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    let ambient = InheritedRuntime::capture();
    std::thread::Builder::new()
        .name(name)
        .stack_size(stack.bytes())
        .spawn(move || {
            // Held for the whole body: it is what makes `tokio::spawn` and the
            // timer reachable from this thread, and therefore what lets a future
            // written for the hosted driver run unchanged under `block_on_sync`.
            ambient.run(move || {
                let _ = enter_ioc_thread(priority);
                f()
            })
        })
}

/// The ambient async context a worker body should run under — captured on the
/// thread that *submitted* the work, applied on the thread that runs it.
///
/// **One owner for the question `spawn_dedicated_thread`'s docs above answer at
/// length.** Two callers need it and they differ in *when* they capture:
/// `spawn_dedicated_thread` captures once, at spawn, because the thread it
/// creates serves exactly one body; `runtime::worker_pool` captures per **job**,
/// because a pooled worker outlives the runtime that first used it. A pooled
/// worker that inherited its ambient at creation would hold a `Handle` to a
/// runtime that has since been dropped — every `#[tokio::test]` builds and drops
/// its own — and enter it for every later connection.
///
/// The `CurrentThread` filter is the rule stated above and must not be
/// re-derived: a current-thread ambient is *not* inherited, because
/// `block_on_sync` cannot distinguish "I am that runtime's thread" from "I have
/// merely entered its handle" and must refuse to park under it.
#[cfg(tokio_backend)]
pub(crate) struct InheritedRuntime(Option<tokio::runtime::Handle>);

#[cfg(tokio_backend)]
impl InheritedRuntime {
    /// Capture the calling thread's runtime, if it is one a dedicated thread
    /// may enter.
    pub(crate) fn capture() -> Self {
        Self(
            tokio::runtime::Handle::try_current()
                .ok()
                .filter(|h| h.runtime_flavor() != RuntimeFlavor::CurrentThread),
        )
    }

    /// Run `f` with the captured context entered for its whole duration.
    pub(crate) fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        let _entered = self.0.as_ref().map(|h| h.enter());
        f()
    }
}

/// RTEMS: the exec backend's spawn pool and timer are process-global, so there
/// is no per-thread context to capture or enter. Same shape so the callers need
/// no `cfg` of their own.
#[cfg(exec_backend)]
pub(crate) struct InheritedRuntime;

#[cfg(exec_backend)]
impl InheritedRuntime {
    pub(crate) fn capture() -> Self {
        Self
    }

    pub(crate) fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        f()
    }
}

/// RTEMS: a plain thread is already complete — the exec backend's spawn pool
/// and timer are process-global, so there is no per-thread context to enter.
#[cfg(exec_backend)]
pub fn spawn_dedicated_thread<F>(
    name: String,
    priority: ThreadPriority,
    stack: StackSizeClass,
    f: F,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name)
        .stack_size(stack.bytes())
        .spawn(move || {
            let _ = enter_ioc_thread(priority);
            f()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything before the first column-0 `#[cfg(test)]` — the code that
    /// actually ships.
    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// Every thread this crate creates states a stack size.
    ///
    /// `std` gives RTEMS the generic 2 MiB `DEFAULT_MIN_STACK_SIZE`
    /// (`std/src/sys/thread/unix.rs`: the carve-out list names vxworks, l4re,
    /// espidf and nuttx — not rtems). On the host that is lazily-committed
    /// address space and costs nothing measurable; on the target it is carved
    /// eagerly out of a fixed pool, which is why an unset stack size is the
    /// first ceiling the IOC hits rather than a rounding error.
    ///
    /// `spawn_dedicated_thread` is enforced by its signature — the class is a
    /// parameter, so a caller cannot omit it. This covers the threads that
    /// still build a `std::thread::Builder` directly.
    ///
    /// It also bans the API that has no class to state:
    /// `std::thread::spawn` cannot express a stack size at all, so a site
    /// using it does not fail the `Builder` check above — it is invisible to
    /// it. Same defect, different anchor. (The six bare `thread::spawn` sites
    /// elsewhere in the workspace — `ca::repeater`, `ca::calink`,
    /// `ca::server::ca_server`, `pva::server::pva_server` — all sit behind
    /// `#[cfg(not(target_os = "rtems"))]` module gates and are therefore
    /// distinct: they are not in the RTEMS closure at all. Every file listed
    /// here is.)
    ///
    /// Fails today, on Linux, with no cross toolchain.
    #[test]
    fn every_thread_in_this_crate_states_a_stack_size() {
        // The list is this crate's files only. `epics-base-rs`'s two
        // thread-building files (`server/ioc_app.rs`, `server/scan.rs`) are
        // swept by the same three assertions in that crate's own
        // `tests/thread_census.rs`: `include_str!` must not cross a crate
        // boundary — a path outside the package directory does not survive
        // `cargo publish` — so the guard was split by subject, not weakened.
        // (file label, source) — every production `Builder` site in the crate.
        let files = [
            ("runtime/task.rs", include_str!("task.rs")),
            (
                "runtime/background/delayed_timer.rs",
                include_str!("background/delayed_timer.rs"),
            ),
            (
                "runtime/background/scan_once.rs",
                include_str!("background/scan_once.rs"),
            ),
            (
                "runtime/background/callback_executor.rs",
                include_str!("background/callback_executor.rs"),
            ),
            ("runtime/worker_pool.rs", include_str!("worker_pool.rs")),
        ];

        let mut unclassified = Vec::new();
        let mut checked = 0usize;
        for (label, src) in files {
            let prod = production_scope(src);
            for (n, after) in prod.split("thread::Builder::new()").skip(1).enumerate() {
                checked += 1;
                // The class must be set before the closure is handed over;
                // `.spawn(` ends the builder chain.
                let chain = after.split(".spawn(").next().unwrap_or("");
                if !chain.contains(".stack_size(") {
                    unclassified.push(format!("{label} (Builder #{})", n + 1));
                }
            }
            // The classless API. Split so this guard does not match its own
            // needle in the file it is written in.
            let bare = concat!("thread", "::spawn(");
            for (n, line) in prod.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") {
                    continue;
                }
                if t.contains(bare) && !t.contains("Builder") {
                    unclassified.push(format!("{label}:{} (bare spawn)", n + 1));
                }
            }
        }

        assert!(
            checked >= 7,
            "expected to find the crate's Builder sites, found {checked} — \
             did a file move? update this guard's file list"
        );
        assert!(
            unclassified.is_empty(),
            "these threads inherit std's 2 MiB default on RTEMS: {unclassified:?}"
        );
    }

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

    /// The property `spawn_dedicated_thread` exists for. A future written for
    /// the hosted driver — one that spawns a task and arms a timer — must run
    /// unchanged on the thread this hands back. On a plain `std::thread` it
    /// does not: it panics with "there is no reactor running" as soon as it is
    /// polled, however willing `block_on_sync` was to park.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dedicated_thread_carries_the_ambient_runtime() {
        let (tx, rx) = std::sync::mpsc::channel();
        let joined = spawn_dedicated_thread(
            "dedicated-with-runtime".into(),
            ThreadPriority::CaServerLow,
            StackSizeClass::Small,
            move || {
                let outcome = block_on_sync(async {
                    let inner = spawn(async { 7u32 }).await.expect("inner task");
                    sleep(Duration::from_millis(1)).await;
                    inner
                });
                let _ = tx.send((
                    std::thread::current().name().map(str::to_string),
                    outcome.ok(),
                ));
            },
        )
        .expect("dedicated thread spawned");

        let (name, value) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the dedicated thread must complete, not panic");
        assert_eq!(name.as_deref(), Some("dedicated-with-runtime"));
        assert_eq!(
            value,
            Some(7),
            "a spawn and a timer must both work on the dedicated thread"
        );
        joined.join().expect("dedicated thread joined");
    }

    /// The third boundary, and the one that was missing: a **current-thread**
    /// ambient runtime.
    ///
    /// The two neighbours below and above cover "multi-thread ambient" and "no
    /// ambient". This is the case between them, and inheriting the handle there
    /// is what made `block_on_sync` return `NotBlockable` on a thread that was
    /// perfectly able to park — silently, since every caller reads the refusal
    /// as "the connection ended". `#[tokio::test]` is `CurrentThread` by
    /// default, so this is also the flavor most of the workspace's tests hand a
    /// dedicated thread.
    ///
    /// The assertion is on `block_on_sync` succeeding, not on the absence of a
    /// handle, because being able to block is the property the thread is spawned
    /// for; how that is arranged is this function's business.
    #[tokio::test]
    async fn a_dedicated_thread_can_block_under_a_current_thread_ambient() {
        let (tx, rx) = std::sync::mpsc::channel();
        let joined = spawn_dedicated_thread(
            "dedicated-current-thread-ambient".into(),
            ThreadPriority::Low,
            StackSizeClass::Small,
            move || {
                // A runtime-agnostic await, the only kind a parking thread may
                // use — and the exact shape both blocking-io pumps run.
                let (ctx, crx) = tokio::sync::mpsc::channel::<u32>(1);
                let outcome = block_on_sync(async move {
                    ctx.send(9u32).await.expect("send into a depth-1 channel");
                    let mut crx = crx;
                    crx.recv().await
                });
                let _ = tx.send(outcome.ok().flatten());
            },
        )
        .expect("dedicated thread spawned");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5))
                .expect("the dedicated thread must complete, not panic"),
            Some(9),
            "a dedicated thread must be able to park under a current-thread \
             ambient runtime; inheriting that handle makes block_on_sync \
             refuse and every pump built on it exit at once"
        );
        joined.join().expect("dedicated thread joined");
    }

    /// The other boundary: no runtime to capture. The thread still runs, and a
    /// runtime-agnostic await still completes — that is `park_on`, and it is
    /// the only arm RTEMS ever takes.
    #[test]
    fn a_dedicated_thread_runs_without_an_ambient_runtime() {
        let (tx, rx) = std::sync::mpsc::channel();
        let joined = spawn_dedicated_thread(
            "dedicated-no-runtime".into(),
            ThreadPriority::Low,
            StackSizeClass::Small,
            move || {
                let _ = tx.send((
                    std::thread::current().name().map(str::to_string),
                    block_on_sync(async { 5u32 }).ok(),
                ));
            },
        )
        .expect("dedicated thread spawned");

        let (name, value) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the dedicated thread must run with no runtime to capture");
        assert_eq!(name.as_deref(), Some("dedicated-no-runtime"));
        assert_eq!(value, Some(5));
        joined.join().expect("dedicated thread joined");
    }

    // --- The band-blocking invariant (doc/pvalink-rtems-design.md §2.3) ------
    //
    // MUST NOT: work running on a background-facility worker thread — a
    // callback band, the delayed timer, the scanOnce worker — block that
    // thread on async progress. The gate is `block_on_sync`; the mark is set
    // by `background::facility::run_facility_loop`, the one function every
    // worker loop goes through.
    //
    // Each of the three cases below is written so that a *broken* gate fails
    // the test instead of hanging it: the awaited future is completable from
    // the test thread, so a worker that parked can always be released before
    // the assertion runs and the pool's `Drop` can still join it.

    /// The case the invariant exists for: a future spawned onto a callback
    /// band. On RTEMS this is exactly what [`spawn`] produces, and the band has
    /// one worker — parking it stops every deferred callback, every FLNK tail
    /// and every other monitor on that band.
    #[test]
    fn a_future_on_a_callback_band_is_refused_a_blocking_bridge() {
        use crate::runtime::background::callback_executor::CallbackPool;
        use crate::runtime::background::future_exec::{DEFAULT_SPAWN_PRIORITY, spawn_future};

        let pool = CallbackPool::new();
        // Held by the test: `recv()` never completes until we send, so a gate
        // that does not refuse leaves the worker parked here.
        let (release, mut park_here) = tokio::sync::mpsc::channel::<()>(1);
        let (report, outcome) = std::sync::mpsc::channel();

        let _handle = spawn_future(&pool.handle(), DEFAULT_SPAWN_PRIORITY, async move {
            let _ = report.send(block_on_sync(async move { park_here.recv().await }));
        });

        let got = outcome.recv_timeout(Duration::from_secs(5));
        // Release a worker the gate failed to protect, so the assertions below
        // report a failure instead of hanging `CallbackPool::drop`'s join.
        let _ = release.try_send(());

        match got {
            Ok(result) => assert_eq!(
                result.map(|v| v.is_some()),
                Err(NotBlockable::BackgroundWorker),
                "a band worker must be refused the blocking bridge, not given one"
            ),
            Err(_) => panic!(
                "the band worker parked inside block_on_sync instead of being \
                 refused — the band has one worker, so this is the deadlock the \
                 invariant exists to prevent"
            ),
        }
    }

    /// The same thread, reached the other way: `spawn_blocking` also lands on a
    /// band worker under the exec backend, and a blocking closure holds that
    /// worker for its whole run. The rule is a property of the thread, so it
    /// must not depend on which spawn put the work there.
    #[test]
    fn a_blocking_closure_on_a_callback_band_is_refused_too() {
        use crate::runtime::background::callback_executor::CallbackPool;
        use crate::runtime::background::future_exec::{DEFAULT_SPAWN_PRIORITY, spawn_blocking_on};

        let pool = CallbackPool::new();
        let (release, mut park_here) = tokio::sync::mpsc::channel::<()>(1);
        let (report, outcome) = std::sync::mpsc::channel();

        let _handle = spawn_blocking_on(&pool.handle(), DEFAULT_SPAWN_PRIORITY, move || {
            let _ = report.send(block_on_sync(async move { park_here.recv().await }));
        });

        let got = outcome.recv_timeout(Duration::from_secs(5));
        let _ = release.try_send(());

        match got {
            Ok(result) => assert_eq!(
                result.map(|v| v.is_some()),
                Err(NotBlockable::BackgroundWorker),
                "the refusal keys on the thread, not on how work reached it"
            ),
            Err(_) => panic!("the band worker parked instead of being refused"),
        }
    }

    /// The other side of the boundary, so the gate cannot be satisfied by
    /// refusing everything: an ordinary thread that merely *submits* to the
    /// pool still blocks. The mark covers the worker loop's own thread and
    /// nothing else.
    #[test]
    fn a_thread_that_only_submits_to_a_band_still_blocks() {
        use crate::runtime::background::callback_executor::{CallbackPool, CallbackPriority};

        let pool = CallbackPool::new();
        let (tx, rx) = std::sync::mpsc::channel();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || tx.send(1u32).unwrap()),
        )
        .expect("the band accepts the callback");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        assert_eq!(
            block_on_sync(async { 5u32 }),
            Ok(5),
            "the submitting thread runs no facility loop, so it may still park"
        );
    }

    #[tokio::test]
    async fn test_sleep() {
        let start = std::time::Instant::now();
        sleep(Duration::from_millis(10)).await;
        assert!(start.elapsed() >= Duration::from_millis(10));
    }

    // The two halves of `timeout`'s contract. They read as trivial against a
    // tokio delegation, and that is the point: they are what a later backend
    // swap has to keep true, on a seam whose whole purpose is to be
    // reimplemented.
    #[tokio::test]
    async fn timeout_yields_the_value_when_the_future_finishes_first() {
        let r = timeout(Duration::from_secs(30), async { 42 }).await;
        assert_eq!(r.unwrap(), 42);
    }

    #[tokio::test]
    async fn timeout_elapses_on_a_future_that_never_finishes() {
        let r = timeout(Duration::from_millis(10), std::future::pending::<()>()).await;
        assert!(r.is_err());
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

    /// The three classes against the C table, factor by factor.
    ///
    /// `STACK_SIZE(f) = f * 0x10000 * sizeof(void*)` with factors 1, 2, 4
    /// (`libcom/src/osi/os/posix/osdThread.c:506-509`) — the file a C IOC on
    /// RTEMS 6 compiles, because `configure/toolchain.c:29-35` picks
    /// `OS_API = posix` for `__RTEMS_MAJOR__ >= 5`. Pinning the factors
    /// separately from the unit is what makes a silent edit of one of them
    /// fail: `stack_size_classes_ordered` above is satisfied by any
    /// increasing triple.
    #[test]
    fn the_classes_are_the_c_posix_table_factor_for_factor() {
        let unit = 0x10000 * std::mem::size_of::<usize>();
        assert_eq!(StackSizeClass::Small.bytes(), unit);
        assert_eq!(StackSizeClass::Medium.bytes(), 2 * unit);
        assert_eq!(StackSizeClass::Big.bytes(), 4 * unit);
        // And what the same table yields on the target the RTEMS port builds
        // for (`sizeof(void*) == 4`), spelled out so a reader on a 64-bit
        // host does not have to re-derive it.
        const TARGET_UNIT: usize = 0x10000 * 4;
        assert_eq!(
            [TARGET_UNIT, 2 * TARGET_UNIT, 4 * TARGET_UNIT],
            [256 * 1024, 512 * 1024, 1024 * 1024],
            "armv7-rtems-eabihf: Small / Medium / Big in bytes"
        );
    }

    /// The stack a caller *states* is the stack the thread *reports*.
    ///
    /// The source guard above only proves a number reached the builder. This
    /// asks the running thread what it actually got, through
    /// `pthread_getattr_np`, and that is the property the RTEMS ceiling
    /// depends on: `std` gates its 2 MiB `DEFAULT_MIN_STACK_SIZE` on a
    /// carve-out list that omits rtems, so a size that fails to arrive is
    /// silently 2 MiB rather than an error.
    ///
    /// The mechanism this exercises is not host-specific: `std`'s
    /// `Thread::new` calls `pthread_attr_setstacksize(attr, max(stack,
    /// PTHREAD_STACK_MIN))` on every non-espidf/nuttx unix
    /// (`std/src/sys/thread/unix.rs`), and `libc` gives rtems
    /// `PTHREAD_STACK_MIN = 0`, so the `max` cannot raise our request there
    /// either. Glibc-only because `pthread_getattr_np` is the readback API.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dedicated_thread_reports_the_stack_it_was_asked_for() {
        fn reported_stack_bytes() -> usize {
            unsafe {
                let mut attr: libc::pthread_attr_t = std::mem::zeroed();
                assert_eq!(
                    libc::pthread_getattr_np(libc::pthread_self(), &mut attr),
                    0,
                    "pthread_getattr_np"
                );
                let mut addr: *mut libc::c_void = std::ptr::null_mut();
                let mut size: libc::size_t = 0;
                assert_eq!(
                    libc::pthread_attr_getstack(&attr, &mut addr, &mut size),
                    0,
                    "pthread_attr_getstack"
                );
                libc::pthread_attr_destroy(&mut attr);
                size
            }
        }

        for class in [
            StackSizeClass::Small,
            StackSizeClass::Medium,
            StackSizeClass::Big,
        ] {
            let (tx, rx) = std::sync::mpsc::channel();
            let joined = spawn_dedicated_thread(
                format!("stack-readback-{class:?}"),
                ThreadPriority::CaServerLow,
                class,
                move || {
                    let _ = tx.send(reported_stack_bytes());
                },
            )
            .expect("dedicated thread spawned");
            let got = rx.recv().expect("the thread reported its stack");
            joined.join().expect("thread joined");

            let asked = class.bytes();
            // The kernel rounds up to a page; it must never round *down*, and
            // it must not silently substitute something of a different order.
            assert!(
                got >= asked && got < asked + 64 * 1024,
                "{class:?}: asked for {asked} bytes, thread reports {got}"
            );
        }
    }

    /// The distinguishing half of the readback: a class below `std`'s default
    /// must land below it. Without this the test above would pass on a
    /// platform that ignored every request and handed out 2 MiB, for the two
    /// classes that happen to be smaller than that on a 64-bit host.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn the_small_classes_are_below_the_default_that_would_mask_a_failure() {
        const STD_DEFAULT_MIN_STACK_SIZE: usize = 2 * 1024 * 1024;
        assert!(StackSizeClass::Small.bytes() < STD_DEFAULT_MIN_STACK_SIZE);
        assert!(StackSizeClass::Medium.bytes() < STD_DEFAULT_MIN_STACK_SIZE);
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
                | PriorityApplied::Disabled
                | PriorityApplied::Unsupported
                | PriorityApplied::BestEffortFailed
        ));
    }

    /// Both defaults, and both override directions against each default.
    ///
    /// The RTEMS arm is unreachable from a host test run unless the default
    /// is a function of the target rather than a `cfg` block, which is why
    /// `default_policy`/`resolve` take their input explicitly.
    #[test]
    fn the_rt_default_is_on_for_rtems_and_off_for_hosted() {
        // (1) the two defaults themselves
        assert_eq!(
            default_policy(true),
            RtPolicy::AllowRealtime,
            "RTEMS honours its priorities by default, as base does \
             (EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING=YES, CONFIG_ENV:57)"
        );
        assert_eq!(
            default_policy(false),
            RtPolicy::Disabled,
            "hosted stays opt-in: RLIMIT_RTPRIO makes the request fail on a \
             desktop, and where it succeeds a runaway band wedges the machine"
        );

        // (2) the compiled-in default is wired to the target, not to a guess
        assert_eq!(DEFAULT_POLICY, default_policy(cfg!(target_os = "rtems")));
        assert_eq!(RtPolicy::from_env_value(None), DEFAULT_POLICY);

        // (3) an explicit value wins over EITHER default, in BOTH directions.
        //     The RTEMS-off case is the one an operator needs: turning RT
        //     scheduling off on a target that defaults to on.
        for default in [RtPolicy::AllowRealtime, RtPolicy::Disabled] {
            assert_eq!(
                RtPolicy::resolve(Some("NO"), default),
                RtPolicy::Disabled,
                "explicit NO must turn it off even where the default is {default:?}"
            );
            assert_eq!(
                RtPolicy::resolve(Some("YES"), default),
                RtPolicy::AllowRealtime,
                "explicit YES must turn it on even where the default is {default:?}"
            );
            assert_eq!(
                RtPolicy::resolve(None, default),
                default,
                "unset must resolve to the default and nothing else"
            );
        }
    }

    #[test]
    fn rt_switch_explicit_values_win_over_the_default() {
        // Unset takes the target's default; that is
        // `the_rt_default_is_on_for_rtems_and_off_for_hosted`'s subject.
        assert_eq!(RtPolicy::from_env_value(None), DEFAULT_POLICY);
        // C's `envGetBoolConfigParam` (envSubr.c:331) accepts only
        // case-insensitive "yes"; we also take the spellings a hand-written
        // startup script is likely to use.
        for on in ["YES", "yes", "Yes", "true", "TRUE", "on", "1", " yes "] {
            assert_eq!(
                RtPolicy::from_env_value(Some(on)),
                RtPolicy::AllowRealtime,
                "{on:?} should turn the switch on"
            );
        }
        // Everything else is off. Silence is the safe direction: a
        // misspelling must never grant a process RT scheduling.
        for off in ["", "NO", "no", "false", "off", "0", "y", "yes please", "2"] {
            assert_eq!(
                RtPolicy::from_env_value(Some(off)),
                RtPolicy::Disabled,
                "{off:?} should leave the switch off"
            );
        }
    }

    /// Switch off ⟹ the OS scheduler is never called.
    ///
    /// Mutation check: deleting the `RtPolicy::Disabled` arm in
    /// `apply_to_current_thread_under` (so it always calls
    /// `apply_priority_impl`) makes the `SCHED_CALLS` assertion fail.
    #[cfg(target_os = "linux")]
    #[test]
    fn switch_off_makes_no_scheduler_calls() {
        let before = SCHED_CALLS.with(std::cell::Cell::get);
        for p in [
            ThreadPriority::Low,
            ThreadPriority::CaServerLow,
            ThreadPriority::ScanHigh,
            ThreadPriority::Iocsh,
            ThreadPriority::Custom(0),
            ThreadPriority::Custom(99),
        ] {
            assert_eq!(
                apply_to_current_thread_under(RtPolicy::Disabled, p),
                PriorityApplied::Disabled
            );
        }
        assert_eq!(
            SCHED_CALLS.with(std::cell::Cell::get),
            before,
            "the switch is off, so nothing may reach pthread_setschedparam"
        );
    }

    /// Switch on: either the host grants SCHED_FIFO — and then the policy
    /// must actually be in force on the thread, at the mapped priority — or
    /// it does not, and the thread keeps running under the default policy.
    ///
    /// Runs on its own thread: on a host that *does* grant RT, leaving the
    /// test-harness thread in a real-time band would outlive the test.
    #[cfg(target_os = "linux")]
    #[test]
    fn switch_on_either_sticks_or_falls_back_without_killing_the_thread() {
        let outcome = std::thread::spawn(|| {
            // Resolve (and cache) the probe first, so what the call below is
            // expected to do is known rather than order-dependent.
            let range = permitted_fifo_range();
            let before = SCHED_CALLS.with(std::cell::Cell::get);
            let outcome =
                apply_to_current_thread_under(RtPolicy::AllowRealtime, ThreadPriority::ScanHigh);
            let calls = SCHED_CALLS.with(std::cell::Cell::get) - before;

            // Whatever the host allowed, this thread is still running.
            assert_eq!(2 + 2, 4);

            let mut policy = 0i32;
            let mut param = libc::sched_param { sched_priority: 0 };
            // SAFETY: reads the calling thread's own scheduling into
            // stack-local outputs.
            let rc = unsafe {
                libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut param)
            };
            assert_eq!(rc, 0, "pthread_getschedparam failed");

            match (range, outcome) {
                (RtRange::Available { min, max }, PriorityApplied::Realtime) => {
                    // The host permits FIFO — assert the policy stuck, at
                    // the C-mapped priority, off exactly one scheduler call.
                    assert_eq!(calls, 1, "one apply must be one scheduler call");
                    assert_eq!(policy, libc::SCHED_FIFO, "SCHED_FIFO did not stick");
                    assert_eq!(
                        param.sched_priority,
                        map_epics_priority(ThreadPriority::ScanHigh.value(), min, max),
                        "wrong OS priority for epicsThreadPriorityScanHigh"
                    );
                }
                (RtRange::Denied, PriorityApplied::BestEffortFailed) => {
                    // Unprivileged: the fallback leaves the thread at the
                    // default policy rather than failing the caller, and —
                    // the anti-spam property — asks the OS nothing further
                    // now that the probe has settled the question once.
                    assert_eq!(calls, 0, "a settled denial must not re-ask the OS");
                    assert_ne!(
                        policy,
                        libc::SCHED_FIFO,
                        "fallback reported but the thread is real-time scheduled"
                    );
                }
                (RtRange::Unsupported, PriorityApplied::Unsupported) => {
                    assert_eq!(calls, 0, "no SCHED_FIFO range means no scheduler call");
                }
                (range, outcome) => {
                    panic!("range {range:?} and outcome {outcome:?} disagree")
                }
            }
            outcome
        })
        .join()
        .expect("probe thread panicked");
        eprintln!("host RT outcome: {outcome:?}");
    }

    /// The unprivileged fallback is logged once, not once per thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn denial_is_reported_once_not_per_thread() {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..8 {
                        let _ = apply_to_current_thread_under(
                            RtPolicy::AllowRealtime,
                            ThreadPriority::Low,
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("worker panicked");
        }
        assert!(
            DENIED_WARNINGS.load(std::sync::atomic::Ordering::Relaxed) <= 1,
            "64 denied requests across 8 threads must not produce more than one warning"
        );
    }

    /// The mapping itself, against `epicsThreadGetPosixPriority`
    /// (`osdThread.c:129-144`).
    #[cfg(target_os = "linux")]
    #[test]
    fn epics_priority_maps_onto_the_permitted_fifo_range() {
        // Linux's nominal SCHED_FIFO range.
        let (min, max) = (1, 99);
        // oss = p * (max-min)/100 + min
        assert_eq!(map_epics_priority(0, min, max), 1);
        assert_eq!(map_epics_priority(20, min, max), 1 + (20.0 * 0.98) as i32);
        assert_eq!(map_epics_priority(99, min, max), 1 + (99.0 * 0.98) as i32);
        // Ordering is preserved: the CA server sits below the scan bands.
        assert!(
            map_epics_priority(ThreadPriority::CaServerHigh.value(), min, max)
                < map_epics_priority(ThreadPriority::ScanLow.value(), min, max)
        );
        // A range restricted by RLIMIT_RTPRIO still spans the whole EPICS
        // space rather than saturating at the top.
        assert_eq!(map_epics_priority(0, 1, 10), 1);
        assert_eq!(map_epics_priority(99, 1, 10), 1 + (99.0 * 0.09) as i32);
        // Degenerate range collapses (osdThread.c:133).
        assert_eq!(map_epics_priority(50, 7, 7), 7);
    }

    /// The RTEMS map's *image* is the whole point of choosing it, so assert
    /// the image, not sampled points: for **every** `u8` input the resulting
    /// RTEMS core priority lands in `100..=199`, and therefore below libbsd's
    /// network threads.
    ///
    /// Provenance of the band, measured on the bring-up guest (RTEMS 6 +
    /// libbsd, QEMU `xilinx_zynq_a9`) — lower core number is *more* urgent:
    ///
    /// | core | thread |
    /// |------|--------|
    /// | 96   | libbsd `IRQS` (interrupt server) |
    /// | 98   | libbsd `TIME` |
    /// | 100  | libbsd default — twelve further network threads |
    /// | 254  | DHCP, outside the band |
    /// | 255  | idle, `RTEMS_MAXIMUM_PRIORITY` |
    ///
    /// So `core >= 100` means: never more urgent than any libbsd network
    /// thread, and strictly less urgent than `IRQS`/`TIME`. That is a
    /// property of the map's construction (fixed offsets over a 100-wide
    /// EPICS space), not of the endpoints, which is why no input — including
    /// the out-of-range ones `ThreadPriority::value` cannot currently produce
    /// — can escape it.
    #[test]
    fn rtems_priority_map_stays_below_the_libbsd_network_band() {
        /// libbsd's most urgent network thread on the measured guest.
        const LIBBSD_IRQS_CORE: i32 = 96;
        /// libbsd's default band; twelve of its threads sit here.
        const LIBBSD_DEFAULT_CORE: i32 = 100;
        // The guest's settable SCHED_FIFO range.
        const POSIX_MIN: i32 = 1;
        const POSIX_MAX: i32 = 254;

        for epics in 0..=u8::MAX {
            let posix = map_epics_priority_rtems(epics);
            let core = RTEMS_MAXIMUM_PRIORITY - posix;
            assert!(
                (POSIX_MIN..=POSIX_MAX).contains(&posix),
                "EPICS {epics} maps to posix {posix}, outside the settable \
                 [{POSIX_MIN}, {POSIX_MAX}]"
            );
            assert!(
                core >= LIBBSD_DEFAULT_CORE,
                "EPICS {epics} maps to core {core}, more urgent than libbsd's \
                 default band ({LIBBSD_DEFAULT_CORE}) and its IRQS \
                 ({LIBBSD_IRQS_CORE})"
            );
            assert!(
                core <= RTEMS_MAXIMUM_PRIORITY - 56,
                "EPICS {epics} maps to core {core}, less urgent than the \
                 EPICS band's own floor of 199"
            );
        }
        // The two ends of the EPICS space, as `RTEMS-score/osdThread.c:94-102`
        // defines them, and the clamp above it.
        assert_eq!(map_epics_priority_rtems(0), 56);
        assert_eq!(map_epics_priority_rtems(99), 155);
        assert_eq!(map_epics_priority_rtems(100), 155);
        assert_eq!(map_epics_priority_rtems(u8::MAX), 155);
        // Ordering still holds: a higher EPICS priority is a more urgent core.
        assert!(
            RTEMS_MAXIMUM_PRIORITY - map_epics_priority_rtems(ThreadPriority::ScanLow.value())
                < RTEMS_MAXIMUM_PRIORITY
                    - map_epics_priority_rtems(ThreadPriority::CaServerHigh.value())
        );
    }

    /// The RTEMS map is not the hosted map with retuned endpoints, and the
    /// difference is exactly the reason the RTEMS arm exists.
    ///
    /// Stated as the hazard rather than as `assert_ne!` on a sample: over the
    /// EPICS space, feeding the *hosted linear* map the guest's own probed
    /// range (`min=1`, `max=254`) puts some priorities above libbsd's `IRQS`
    /// at core 96, and the fixed RTEMS map puts none there. The crossover is
    /// pinned at EPICS 63 because that number is the justification recorded in
    /// the commit message; if base's map or the measured band ever moves, this
    /// fails rather than the deviation quietly losing its reason.
    #[test]
    fn rtems_priority_map_is_not_the_hosted_linear_map() {
        const LIBBSD_IRQS_CORE: i32 = 96;
        // What `find_pri_range` yields on the guest (osdThread.c:295-311).
        let (min, max) = (1, 254);

        let hosted_core = |epics: u8| RTEMS_MAXIMUM_PRIORITY - map_epics_priority(epics, min, max);
        let rtems_core = |epics: u8| RTEMS_MAXIMUM_PRIORITY - map_epics_priority_rtems(epics);

        let hosted_above_irqs: Vec<u8> = (0..=99)
            .filter(|&e| hosted_core(e) < LIBBSD_IRQS_CORE)
            .collect();
        let rtems_above_irqs: Vec<u8> = (0..=99)
            .filter(|&e| rtems_core(e) < LIBBSD_IRQS_CORE)
            .collect();

        assert_eq!(
            rtems_above_irqs,
            Vec::<u8>::new(),
            "the RTEMS map must place no EPICS priority above libbsd's IRQS"
        );
        assert_eq!(
            hosted_above_irqs.first().copied(),
            Some(63),
            "base-on-RTEMS-6's posix map crosses IRQS at EPICS 63; that number \
             is the recorded reason for this deviation"
        );
        // And concretely at the CA server band the audit cares about.
        assert_eq!(
            hosted_core(91),
            24,
            "upstream posix map: EPICS 91 -> core 24"
        );
        assert_eq!(rtems_core(91), 108, "this port: EPICS 91 -> core 108");
        // Shapes, not endpoints: the hosted map spans the whole probed range,
        // this one spans exactly 100 levels wherever it is placed.
        assert_eq!(
            map_epics_priority_rtems(99) - map_epics_priority_rtems(0),
            99
        );
        assert_eq!(
            map_epics_priority(99, min, max) - map_epics_priority(0, min, max),
            250
        );
    }

    /// The name budget an RTEMS thread object actually has:
    /// `CONFIGURE_MAXIMUM_THREAD_NAME_SIZE` defaults to 16 *including* the
    /// NUL (`rtems/score/thread.h:1079`, `rtems/confdefs/threads.h:92-93`)
    /// and the boot shim does not override it, so 15 bytes — the same budget
    /// `std` truncates to on Linux.
    ///
    /// Truncating here rather than letting `_Thread_Set_name` do it is what
    /// keeps the call's result meaningful: that function `strlcpy`s and
    /// *still applies* the truncated name, but returns
    /// `STATUS_RESULT_TOO_LARGE` → `ERANGE`, so an untruncated call would log
    /// a failure for a name it had in fact set.
    #[test]
    fn thread_names_are_cut_to_the_rtems_budget_on_a_char_boundary() {
        assert_eq!(RTEMS_MAX_THREAD_NAME_BYTES, 15);
        // Short names pass through untouched.
        assert_eq!(truncate_thread_name("CAS-event"), "CAS-event");
        // Exactly at the budget.
        assert_eq!(truncate_thread_name("123456789012345"), "123456789012345");
        // Over it — a real per-client CA thread name.
        assert_eq!(
            truncate_thread_name("CAS-client-blocking 10.0.0.1:5064"),
            "CAS-client-blo"[..14].to_owned() + "c"
        );
        assert!(truncate_thread_name("CAS-client-blocking 10.0.0.1:5064").len() <= 15);
        // Never mid-codepoint: 'é' is two bytes, so a cut landing inside it
        // must step back rather than produce invalid UTF-8. 14 ASCII bytes
        // plus 'é' is 16 bytes; the budget cuts at 15, inside the 'é'.
        let mixed = "aaaaaaaaaaaaaaé";
        assert_eq!(mixed.len(), 16);
        assert_eq!(truncate_thread_name(mixed), "aaaaaaaaaaaaaa");
        // Empty stays empty rather than underflowing the boundary walk.
        assert_eq!(truncate_thread_name(""), "");
    }

    /// Every thread this crate starts publishes its name to the OS.
    ///
    /// `std` calls the platform `pthread_setname_np` from `Builder::spawn`
    /// on the hosted targets it supports, and RTEMS is not one of them — so
    /// there, a name set with `Builder::name` lives only in Rust's `Thread`
    /// struct and the kernel shows nothing. Bring-up had to measure libbsd's
    /// priority band by other means for exactly that reason.
    ///
    /// The defect is a call that is *absent*, so this is source inspection
    /// over every production `Builder` site in the crate — the same sweep
    /// shape as `every_thread_in_this_crate_states_a_stack_size`, and it
    /// fails the same way when a new thread forgets. Either prologue counts:
    /// `enter_ioc_thread` for a thread with an EPICS band, bare
    /// `name_current_thread` for one that deliberately has none.
    #[test]
    fn every_thread_in_this_crate_publishes_its_name() {
        // This crate's files only — see the note on the sweep above.
        let files = [
            ("runtime/task.rs", include_str!("task.rs")),
            (
                "runtime/background/delayed_timer.rs",
                include_str!("background/delayed_timer.rs"),
            ),
            (
                "runtime/background/scan_once.rs",
                include_str!("background/scan_once.rs"),
            ),
            (
                "runtime/background/callback_executor.rs",
                include_str!("background/callback_executor.rs"),
            ),
        ];
        // The one exemption, named rather than pattern-matched: the
        // SCHED_FIFO range probe is `#[cfg(target_os = "linux")]`, exists for
        // two `sched_*` calls and a join, and never runs on the target whose
        // task listing this guard is about.
        const EXEMPT: &str = ".name(\"cbRtProbe\".to_string())";

        let mut anonymous = Vec::new();
        let mut checked = 0usize;
        for (label, src) in files {
            for (n, after) in production_scope(src)
                .split("thread::Builder::new()")
                .skip(1)
                .enumerate()
            {
                let (chain, body) = after.split_once(".spawn(").unwrap_or((after, ""));
                if chain.contains(EXEMPT) {
                    continue;
                }
                checked += 1;
                // The prologue is the closure's first work, so look at the
                // closure, not the builder chain.
                if !body.contains("enter_ioc_thread(") && !body.contains("name_current_thread()") {
                    anonymous.push(format!("{label} (Builder #{})", n + 1));
                }
            }
        }

        assert!(
            checked >= 5,
            "expected to find the crate's Builder sites, found {checked} — \
             did a file move? update this guard's file list"
        );
        assert!(
            anonymous.is_empty(),
            "these threads are invisible in an RTEMS task listing: {anonymous:?}"
        );
    }

    /// The banding half of the prologue is not reachable without the naming
    /// half: nothing in this crate's production scope calls
    /// [`apply_to_current_thread`] except [`enter_ioc_thread`] itself.
    ///
    /// Separate from the sweep above because it catches the other direction —
    /// a thread that is named by `Builder` but takes its band directly, which
    /// the closure-body sweep would pass if the naming call happened to be
    /// somewhere else in the file.
    #[test]
    fn only_the_prologue_reaches_the_banding_call() {
        // This crate's files only — see the note on the sweep above.
        let files = [
            ("runtime/task.rs", include_str!("task.rs")),
            (
                "runtime/background/delayed_timer.rs",
                include_str!("background/delayed_timer.rs"),
            ),
            (
                "runtime/background/scan_once.rs",
                include_str!("background/scan_once.rs"),
            ),
            (
                "runtime/background/callback_executor.rs",
                include_str!("background/callback_executor.rs"),
            ),
        ];
        // Only the definition and the prologue's own delegation, both in
        // task.rs. Anywhere else is a thread banded without being named.
        let allowed = [
            "pub fn apply_to_current_thread(priority: ThreadPriority) -> PriorityApplied {",
            "apply_to_current_thread(priority)",
        ];
        let mut seen_definition = false;
        for (label, src) in files {
            let callers: Vec<&str> = production_scope(src)
                .lines()
                .map(str::trim)
                .filter(|l| l.contains("apply_to_current_thread("))
                .filter(|l| !l.starts_with("//"))
                .collect();
            seen_definition |= callers.contains(&allowed[0]);
            let strays: Vec<&&str> = callers.iter().filter(|l| !allowed.contains(l)).collect();
            assert!(
                strays.is_empty(),
                "{label}: only `enter_ioc_thread` may band a thread; \
                 everything else would band an OS-anonymous one — {strays:?}"
            );
        }
        assert!(
            seen_definition,
            "the banding function moved out of this file list; update the guard"
        );
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
