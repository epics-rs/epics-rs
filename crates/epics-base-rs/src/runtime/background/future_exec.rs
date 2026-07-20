//! Runtime-free future executor over the callback pool — the RTEMS backend for
//! [`crate::runtime::task::spawn`] / [`crate::runtime::task::spawn_blocking`]
//! (decision A2, increment W3b).
//!
//! # Model
//!
//! A hosted build runs a spawned async *tail* as a tokio task. RTEMS has no
//! tokio runtime, so this module runs each spawned future as a **callback on the
//! [`CallbackPool`](super::callback_executor::CallbackPool)**: one pool worker
//! picks up the future and drives it to completion with
//! [`park_on_interruptible`](crate::runtime::task::park_on_interruptible) —
//! poll, then park the worker between polls, relying on the future's own
//! (runtime-agnostic) waker to unpark it, exactly as the sync bridge
//! `block_on_sync` already does. All primitives the future awaits must be
//! runtime-agnostic (`tokio::sync` locks/channels/notifies), which is precisely
//! the A2 precondition: every RTEMS async tail awaits only such primitives.
//!
//! The worker thread stays occupied for the whole life of the future — a
//! long-lived spawned task holds a callback-band worker until it ends. Sizing
//! the pool for the expected number of concurrent tails is a deployment concern
//! (C `callbackParallelThreads`), not handled here.
//!
//! # Handle surface
//!
//! [`JoinFuture<T>`] mirrors the subset of `tokio::task::JoinHandle` the CA/PVA
//! call sites actually use (see the W3b call-site map): `impl Future<Output =
//! Result<T, JoinError>>`, [`abort`](JoinFuture::abort),
//! [`is_finished`](JoinFuture::is_finished), and
//! [`abort_handle`](JoinFuture::abort_handle) returning a non-generic
//! [`AbortHandle`] with [`abort`](AbortHandle::abort) /
//! [`is_finished`](AbortHandle::is_finished). [`JoinError`] mirrors only
//! [`is_cancelled`](JoinError::is_cancelled) — the one `JoinError` method any
//! call site consumes.
//!
//! # Panic isolation
//!
//! C `callbackTask` (`callback.c:210-235`, cited in
//! [`super::callback_executor`]) is a bare drain loop: it calls each callback
//! and loops, with no exception machinery (C has none). A Rust callback *can*
//! unwind, and an unwmind out of the worker closure would tear down the band's
//! worker thread — breaking that drain-loop invariant. So the future poll is run
//! under [`catch_unwind`]: a panicking task is reported as a panicked
//! [`JoinError`] and the worker keeps draining, preserving the C loop's
//! "one callback never stops the worker" property.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::Thread;

use super::callback_executor::{Callback, CallbackHandle, CallbackPriority};
use crate::runtime::task::park_on_interruptible;

/// Default band for a general spawned tail. C routes general deferred work
/// through `callbackRequest` at `priorityMedium` (`callback.h:42`) — the middle
/// of the three bands (`callback.h:41-43`) — so a spawned async tail lands
/// there unless a caller picks another band.
pub const DEFAULT_SPAWN_PRIORITY: CallbackPriority = CallbackPriority::Medium;

/// Why awaiting a [`JoinFuture`] yielded an error instead of the task output —
/// the seam-owned mirror of `tokio::task::JoinError`.
///
/// Only [`is_cancelled`](Self::is_cancelled) is exposed: it is the one
/// `JoinError` method any seam call site consumes (the W3b map shows
/// `is_cancelled()` in ca/pva shutdown paths; no site calls `is_panic()` /
/// `into_panic()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinError {
    kind: JoinErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinErrorKind {
    /// The task was [`abort`](JoinFuture::abort)ed before it completed.
    Cancelled,
    /// The task's future (or blocking closure) panicked; the worker survived.
    Panicked,
}

impl JoinError {
    fn cancelled() -> Self {
        JoinError {
            kind: JoinErrorKind::Cancelled,
        }
    }

    fn panicked() -> Self {
        JoinError {
            kind: JoinErrorKind::Panicked,
        }
    }

    /// `true` when the task was aborted before completing — mirrors
    /// `tokio::task::JoinError::is_cancelled`. A panicked task returns `false`
    /// here (as tokio does).
    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind, JoinErrorKind::Cancelled)
    }
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            JoinErrorKind::Cancelled => f.write_str("task was cancelled"),
            JoinErrorKind::Panicked => f.write_str("task panicked"),
        }
    }
}

impl std::error::Error for JoinError {}

/// Non-generic control block shared by a [`JoinFuture`] and every
/// [`AbortHandle`] cloned from it. Split out from the generic result slot so
/// [`AbortHandle`] can be non-generic, exactly like `tokio::task::AbortHandle`
/// (which ca/pva store in `Vec<(_, AbortHandle)>` fields with no task type).
struct Control {
    /// Set by [`AbortHandle::abort`] / [`JoinFuture::abort`]; observed by the
    /// driving worker's cancel check.
    abort: AtomicBool,
    /// Set once the task has produced its result (completed, cancelled, or
    /// panicked). Backs `is_finished`.
    finished: AtomicBool,
    /// The worker thread currently driving the task, published while it runs so
    /// an abort can unpark a *parked* driver and make it re-check promptly.
    worker: Mutex<Option<Thread>>,
}

impl Control {
    fn new() -> Arc<Self> {
        Arc::new(Control {
            abort: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            worker: Mutex::new(None),
        })
    }
}

/// Request cancellation on a control block: latch the abort flag, then unpark
/// the driving worker (if any) so a parked driver wakes and observes it. A
/// no-op once the task has finished.
fn request_abort(control: &Control) {
    control.abort.store(true, Ordering::Release);
    if let Some(t) = control.worker.lock().unwrap().clone() {
        t.unpark();
    }
}

/// Generic result slot plus the joiner's waker.
struct Slot<T> {
    /// The task's outcome, taken by the first [`JoinFuture::poll`] that sees it.
    result: Option<Result<T, JoinError>>,
    /// Waker of the task awaiting this [`JoinFuture`], if it polled before the
    /// task finished.
    join_waker: Option<Waker>,
}

struct Shared<T> {
    control: Arc<Control>,
    slot: Mutex<Slot<T>>,
}

impl<T> Shared<T> {
    fn new() -> Arc<Self> {
        Arc::new(Shared {
            control: Control::new(),
            slot: Mutex::new(Slot {
                result: None,
                join_waker: None,
            }),
        })
    }

    /// Publish the task's outcome and wake any joiner. Sets the result under the
    /// slot lock, then the `finished` flag (still under the lock, so a concurrent
    /// `is_finished()` never observes `finished` before the result is visible to
    /// a `poll`), then wakes.
    fn finalize(&self, result: Result<T, JoinError>) {
        let waker = {
            let mut slot = self.slot.lock().unwrap();
            slot.result = Some(result);
            let waker = slot.join_waker.take();
            // Clear the worker under the same lock ordering the driver used.
            *self.control.worker.lock().unwrap() = None;
            self.control.finished.store(true, Ordering::Release);
            waker
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// A handle over a spawned task — the RTEMS-side mirror of
/// `tokio::task::JoinHandle`. `await` it for `Result<T, JoinError>`.
pub struct JoinFuture<T> {
    shared: Arc<Shared<T>>,
}

impl<T> JoinFuture<T> {
    /// Request cancellation — mirrors `tokio::task::JoinHandle::abort`.
    /// Best-effort: the task is dropped at its next suspension point (or before
    /// its first poll if not yet started). A task already inside a synchronous
    /// stretch runs to its next `await` before the cancel is observed.
    pub fn abort(&self) {
        request_abort(&self.shared.control);
    }

    /// `true` once the task has produced its result — mirrors
    /// `tokio::task::JoinHandle::is_finished`.
    pub fn is_finished(&self) -> bool {
        self.shared.control.finished.load(Ordering::Acquire)
    }

    /// A non-generic abort handle for this task — mirrors
    /// `tokio::task::JoinHandle::abort_handle`.
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            control: Arc::clone(&self.shared.control),
        }
    }
}

impl<T> Future for JoinFuture<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slot = self.shared.slot.lock().unwrap();
        match slot.result.take() {
            Some(result) => Poll::Ready(result),
            None => {
                // Re-register the latest waker (the joiner may have moved).
                slot.join_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// A cancellation handle detached from a [`JoinFuture`] — the mirror of
/// `tokio::task::AbortHandle`. Cloneable and non-generic, so it can be stored in
/// heterogeneous collections (as ca/pva store `AbortHandle`s).
#[derive(Clone)]
pub struct AbortHandle {
    control: Arc<Control>,
}

impl AbortHandle {
    /// Request cancellation — mirrors `tokio::task::AbortHandle::abort`.
    pub fn abort(&self) {
        request_abort(&self.control);
    }

    /// `true` once the task has finished — mirrors
    /// `tokio::task::AbortHandle::is_finished`.
    pub fn is_finished(&self) -> bool {
        self.control.finished.load(Ordering::Acquire)
    }
}

/// Spawn `fut` onto the callback pool behind `callbacks`, driven to completion
/// on a `priority`-band worker. Returns immediately with a [`JoinFuture`]; the
/// task runs on a background worker.
///
/// If the band's ring is full (C `S_db_bufFull`), the task cannot be enqueued;
/// the returned handle is pre-finished as cancelled and an error is logged —
/// mirroring the fact that a tokio spawn never fails while still giving the
/// caller a handle that resolves.
pub fn spawn_future<F>(
    callbacks: &CallbackHandle,
    priority: CallbackPriority,
    fut: F,
) -> JoinFuture<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let shared = Shared::new();
    let task_shared = Arc::clone(&shared);
    let callback: Callback = Box::new(move || run_future(&task_shared, fut));

    if callbacks.request(priority, callback).is_err() {
        // Ring full: the closure (and `fut`) was dropped by `request`; resolve
        // the handle as cancelled so a joiner is never stranded.
        tracing::error!(
            target: "epics_base_rs::runtime::future_exec",
            "spawn_future: callback ring full; task dropped, handle resolves cancelled"
        );
        shared.finalize(Err(JoinError::cancelled()));
    }
    JoinFuture { shared }
}

/// Drive `fut` to completion (or cancellation, or panic) on the current worker
/// thread, then publish the outcome.
fn run_future<T>(shared: &Shared<T>, fut: impl Future<Output = T>) {
    // Publish this worker so an abort can unpark us mid-park.
    *shared.control.worker.lock().unwrap() = Some(std::thread::current());

    let control = &shared.control;
    // Poll under catch_unwind: a panicking future must not tear down the worker
    // (C callbackTask drain-loop invariant, callback.c:210-235).
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        park_on_interruptible(fut, || control.abort.load(Ordering::Acquire))
    }));
    let result = match outcome {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err(JoinError::cancelled()),
        Err(_panic) => Err(JoinError::panicked()),
    };
    shared.finalize(result);
}

/// Run a blocking closure `f` on a callback-pool worker — the RTEMS backend for
/// [`crate::runtime::task::spawn_blocking`]. Returns a [`JoinFuture`] resolving
/// to `f`'s return value.
///
/// A blocking closure has no suspension point, so it cannot be aborted
/// mid-run (as `tokio::task::spawn_blocking` also cannot); `abort` before it
/// starts still cancels it. A panic is isolated exactly as for
/// [`spawn_future`].
pub fn spawn_blocking_on<F, R>(
    callbacks: &CallbackHandle,
    priority: CallbackPriority,
    f: F,
) -> JoinFuture<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let shared = Shared::new();
    let task_shared = Arc::clone(&shared);
    let callback: Callback = Box::new(move || {
        // Honor an abort that landed before we started running.
        if task_shared.control.abort.load(Ordering::Acquire) {
            task_shared.finalize(Err(JoinError::cancelled()));
            return;
        }
        let outcome = catch_unwind(AssertUnwindSafe(f));
        let result = match outcome {
            Ok(value) => Ok(value),
            Err(_panic) => Err(JoinError::panicked()),
        };
        task_shared.finalize(result);
    });

    if callbacks.request(priority, callback).is_err() {
        tracing::error!(
            target: "epics_base_rs::runtime::future_exec",
            "spawn_blocking_on: callback ring full; closure dropped, handle resolves cancelled"
        );
        shared.finalize(Err(JoinError::cancelled()));
    }
    JoinFuture { shared }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::background::callback_executor::CallbackPool;
    use crate::runtime::task::park_on_interruptible as drive;
    use std::sync::mpsc;
    use std::time::Duration;

    const T: Duration = Duration::from_secs(5);

    /// Block the test thread on a `JoinFuture`, returning its `Result`.
    fn join<T>(jf: JoinFuture<T>) -> Result<T, JoinError> {
        drive(jf, || false).expect("uncancelled join returned None")
    }

    #[test]
    fn future_runs_to_completion() {
        let pool = CallbackPool::new();
        let jf = spawn_future(&pool.handle(), DEFAULT_SPAWN_PRIORITY, async { 42u32 });
        assert_eq!(join(jf).unwrap(), 42);
    }

    #[test]
    fn future_awaiting_cross_thread_primitive_completes() {
        // The A2 precondition: a spawned tail that awaits a runtime-agnostic
        // tokio::sync primitive, woken from ANOTHER thread, must complete under
        // the park-driver with no tokio runtime present.
        let pool = CallbackPool::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<u32>();
        let jf = spawn_future(&pool.handle(), DEFAULT_SPAWN_PRIORITY, async move {
            rx.await.unwrap()
        });
        // Send from the test thread after the worker is already parked awaiting.
        std::thread::sleep(Duration::from_millis(20));
        tx.send(7).unwrap();
        assert_eq!(join(jf).unwrap(), 7);
    }

    #[test]
    fn panic_in_task_does_not_kill_the_worker() {
        // callback.c:210-235 drain-loop invariant: one bad callback must not
        // stop the band. The panicked task reports a non-cancelled JoinError,
        // and a subsequent task on the SAME pool still runs.
        let pool = CallbackPool::new();

        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async {
            panic!("boom");
        });
        let err = join(jf).unwrap_err();
        assert!(!err.is_cancelled(), "a panic is not a cancellation");

        // Same pool, same band — the worker survived and drains the next task.
        let jf2 = spawn_future(&pool.handle(), CallbackPriority::Medium, async { 99u32 });
        assert_eq!(join(jf2).unwrap(), 99);
    }

    #[test]
    fn abort_before_completion_cancels_cleanly() {
        let pool = CallbackPool::new();
        // A task that parks forever on a oneshot whose sender we keep.
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (ran_tx, ran_rx) = mpsc::channel();
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            ran_tx.send(()).unwrap();
            let _ = rx.await;
        });
        // Ensure the worker has started and parked on the await.
        ran_rx.recv_timeout(T).unwrap();
        std::thread::sleep(Duration::from_millis(20));

        jf.abort();
        let err = join(jf_reborrow(&jf)).unwrap_err();
        assert!(err.is_cancelled(), "aborted task must report cancelled");
        assert!(jf.is_finished());
    }

    // `JoinFuture` is single-await; the abort test needs both a method call and
    // a join. Re-expose the shared handle by cloning the Arc so the test can do
    // both without moving the handle into `join`.
    fn jf_reborrow<T>(jf: &JoinFuture<T>) -> JoinFuture<T> {
        JoinFuture {
            shared: Arc::clone(&jf.shared),
        }
    }

    #[test]
    fn abort_handle_cancels_the_task() {
        let pool = CallbackPool::new();
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            let _ = rx.await;
        });
        let ah = jf.abort_handle();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!ah.is_finished());
        ah.abort();
        assert!(join(jf).unwrap_err().is_cancelled());
        assert!(ah.is_finished());
    }

    #[test]
    fn spawn_blocking_returns_value_and_isolates_panic() {
        let pool = CallbackPool::new();
        let jf = spawn_blocking_on(&pool.handle(), CallbackPriority::Medium, || 123u32);
        assert_eq!(join(jf).unwrap(), 123);

        let jf = spawn_blocking_on(&pool.handle(), CallbackPriority::Medium, || {
            panic!("blocking boom")
        });
        assert!(!join::<()>(jf).unwrap_err().is_cancelled());

        // Worker survived the panic.
        let jf = spawn_blocking_on(&pool.handle(), CallbackPriority::Medium, || 5u32);
        assert_eq!(join(jf).unwrap(), 5);
    }
}
