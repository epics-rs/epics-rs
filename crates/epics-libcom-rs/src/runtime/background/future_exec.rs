//! Runtime-free future executor over the callback pool — the RTEMS backend for
//! [`crate::runtime::task::spawn`] / [`crate::runtime::task::spawn_blocking`]
//! (decision A2, increment W3b).
//!
//! # Model — cooperative, not worker-per-future
//!
//! A hosted build runs a spawned async *tail* as a tokio task. RTEMS has no
//! tokio runtime, so this module runs each spawned future as a **task object
//! multiplexed over the [`CallbackPool`](super::callback_executor::CallbackPool)**:
//!
//! 1. `spawn` builds a `Task` and pushes it onto its band's ring.
//! 2. A band worker pops it and polls it **once**.
//! 3. `Ready` → the outcome is published and the task is done.
//! 4. `Pending` → the worker **releases the task and moves on**. The waker
//!    handed to the poll is the task itself: waking it pushes the task back
//!    onto the ring (at the tail), where some worker picks it up and polls it
//!    again.
//!
//! So a band's workers are held only for the duration of a single poll, and a
//! bounded worker set multiplexes an unbounded number of mostly-idle tails. All
//! primitives the future awaits must still be runtime-agnostic (`tokio::sync`
//! locks/channels/notifies, [`super::timer_sleep`]) — nothing here drives a
//! reactor or a timer wheel — which is precisely the A2 precondition.
//!
//! This replaces the original design, where a worker ran
//! `park_on_interruptible` for
//! the *whole life* of the future and stayed parked between polls. That model
//! had a structural defect: N concurrent long-lived tails exhausted the band's
//! N workers, after which every further task on that band starved until one
//! finished. Memory note `rtems-exec-worker-per-future-fragility`; it mattered
//! more once PVA started sharing this backend.
//!
//! ## Task state machine
//!
//! One [`AtomicU8`] is the single owner of "may this task be polled, and by
//! whom":
//!
//! | State | Meaning | Who leaves it |
//! |---|---|---|
//! | `IDLE` | not queued, not running — waiting for a wake | a waker, or an abort |
//! | `SCHEDULED` | sitting on a band ring | the worker that pops it |
//! | `RUNNING` | being polled right now | the polling worker |
//! | `RUNNING_NOTIFIED` | being polled, and a wake landed mid-poll | the polling worker (re-enqueues) |
//! | `DONE` | terminal | nobody |
//!
//! `RUNNING_NOTIFIED` is what makes a wake that races the poll safe: it is
//! never dropped, it is deferred to the end of the current poll and turned into
//! a re-enqueue. A wake arriving in any other state either enqueues (`IDLE`) or
//! is redundant (`SCHEDULED` — already queued; `DONE` — nothing to run).
//!
//! ## Abort
//!
//! [`JoinFuture::abort`] / [`AbortHandle::abort`] latch a flag and then
//! **schedule** the task, so an idle task is polled once more and observes the
//! flag at the top of that poll — before the future is polled again. That is
//! the same "cancel is observed at the next suspension point" contract the
//! previous park-driver had (there, the abort unparked the parked worker); only
//! the wake-up mechanism changed.
//!
//! ## Handle surface
//!
//! Unchanged. [`JoinFuture<T>`] mirrors the subset of `tokio::task::JoinHandle`
//! the CA/PVA call sites actually use (see the W3b call-site map): `impl
//! Future<Output = Result<T, JoinError>>`, [`abort`](JoinFuture::abort),
//! [`is_finished`](JoinFuture::is_finished), and
//! [`abort_handle`](JoinFuture::abort_handle) returning a non-generic
//! [`AbortHandle`] with [`abort`](AbortHandle::abort) /
//! [`is_finished`](AbortHandle::is_finished). [`JoinError`] mirrors only
//! [`is_cancelled`](JoinError::is_cancelled) — the one `JoinError` method any
//! call site consumes.
//!
//! ## Every handle resolves
//!
//! `Shared::finalize` is the single owner of "this task has an outcome", and
//! it is idempotent. Three paths reach it, covering every way a task can stop
//! existing: the poll produced `Ready`/cancel/panic; the queued entry was
//! dropped without ever running (ring full, or the band shut down under it);
//! or the task became unreachable — no queue entry and no live waker — and its
//! `Drop` ran. A [`JoinFuture`] therefore never strands.
//!
//! ## Panic isolation
//!
//! C `callbackTask` (`callback.c:210-235`, cited in
//! [`super::callback_executor`]) is a bare drain loop: it calls each callback
//! and loops, with no exception machinery (C has none). A Rust callback *can*
//! unwind, and an unwind out of the worker closure would tear down the band's
//! worker thread — breaking that drain-loop invariant. So each poll is run
//! under [`catch_unwind`]: a panicking task is reported as a panicked
//! [`JoinError`] and the worker keeps draining, preserving the C loop's
//! "one callback never stops the worker" property.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Wake, Waker};

use super::callback_executor::{Callback, CallbackHandle, CallbackPriority};

/// Default band for a general spawned tail. C routes general deferred work
/// through `callbackRequest` at `priorityMedium` (`callback.h:42`) — the middle
/// of the three bands (`callback.h:41-43`) — so a spawned async tail lands
/// there unless a caller picks another band.
pub const DEFAULT_SPAWN_PRIORITY: CallbackPriority = CallbackPriority::Medium;

// --- Task states (see the module docs' state table) -------------------------

/// Not queued and not running; only a wake or an abort moves it on.
const IDLE: u8 = 0;
/// Sitting on a band ring, waiting for a worker to pop it.
const SCHEDULED: u8 = 1;
/// Being polled right now by a band worker.
const RUNNING: u8 = 2;
/// Being polled, and a wake landed during the poll — re-enqueue when it ends.
const RUNNING_NOTIFIED: u8 = 3;
/// Terminal: the outcome has been (or is being) published.
const DONE: u8 = 4;

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

/// The non-generic half of a task, so [`AbortHandle`] can be non-generic —
/// exactly like `tokio::task::AbortHandle`, which ca/pva store in
/// `Vec<(_, AbortHandle)>` fields that name no task type.
trait Schedulable: Send + Sync {
    /// Push the task onto its band ring if it is not already queued or running.
    fn schedule(self: Arc<Self>);
}

/// Non-generic control block shared by a [`JoinFuture`] and every
/// [`AbortHandle`] cloned from it.
struct Control {
    /// Set by [`AbortHandle::abort`] / [`JoinFuture::abort`]; read at the top of
    /// every poll.
    abort: AtomicBool,
    /// Set once the task has produced its result (completed, cancelled, or
    /// panicked). Backs `is_finished`.
    finished: AtomicBool,
    /// The task, for waking it so it can observe an abort.
    ///
    /// **`Weak` on purpose.** A strong reference here would close the cycle
    /// task → shared → control → task and leak every task that ends without
    /// running. `Weak` costs nothing: the task is alive exactly while it can
    /// still run (a queue entry or a registered waker holds it), and if it is
    /// *not* alive its `Drop` has already resolved the handle as cancelled — so
    /// an abort that finds a dead `Weak` still leaves the joiner with
    /// `is_cancelled()`.
    task: Mutex<Option<Weak<dyn Schedulable>>>,
}

impl Control {
    fn new() -> Arc<Self> {
        Arc::new(Control {
            abort: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            task: Mutex::new(None),
        })
    }
}

/// Request cancellation on a control block: latch the abort flag, then schedule
/// the task so it is polled once more and observes the flag at the top of that
/// poll. A no-op once the task has finished (a `DONE` task ignores schedules)
/// or once it is gone (its `Drop` already finalized it as cancelled).
fn request_abort(control: &Control) {
    control.abort.store(true, Ordering::Release);
    // Clone out from under the lock: `schedule` reaches into the callback pool,
    // and no pool operation may run while a control lock is held.
    let task = control.task.lock().unwrap().clone();
    if let Some(task) = task.and_then(|w| w.upgrade()) {
        task.schedule();
    }
}

/// Generic result slot plus the joiner's waker.
struct Slot<T> {
    /// The task's outcome, taken by the first [`JoinFuture::poll`] that sees it.
    result: Option<Result<T, JoinError>>,
    /// Whether an outcome has ever been published. Distinct from
    /// `result.is_some()`, which goes back to `None` once the joiner takes it —
    /// this is what makes [`Shared::finalize`] idempotent.
    finalized: bool,
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
                finalized: false,
                join_waker: None,
            }),
        })
    }

    /// Publish the task's outcome and wake any joiner — **the single owner of
    /// the "task has an outcome" transition**, and idempotent: the first caller
    /// wins and every later one is a no-op.
    ///
    /// Sets the result under the slot lock, then the `finished` flag (still
    /// under the lock, so a concurrent `is_finished()` never observes
    /// `finished` before the result is visible to a `poll`), then wakes outside
    /// it — waking may run arbitrary code, including a re-entrant spawn.
    fn finalize(&self, result: Result<T, JoinError>) {
        let waker = {
            let mut slot = self.slot.lock().unwrap();
            if slot.finalized {
                return;
            }
            slot.finalized = true;
            slot.result = Some(result);
            let waker = slot.join_waker.take();
            self.control.finished.store(true, Ordering::Release);
            waker
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// A spawned future plus everything needed to re-schedule it: the executor's
/// unit of work, and the [`Waker`] handed to its own polls.
struct Task<T> {
    /// The state machine (see the module docs).
    state: AtomicU8,
    /// The future, taken out for the duration of a poll and put back on
    /// `Pending`. `None` means "consumed" — completed, cancelled, or panicked.
    future: Mutex<Option<Pin<Box<dyn Future<Output = T> + Send>>>>,
    shared: Arc<Shared<T>>,
    /// Where to push ourselves on wake.
    callbacks: CallbackHandle,
    priority: CallbackPriority,
    /// Test-only: how many times this task has been pushed onto a band ring.
    /// Backs the "a synchronously-completing future never round-trips the
    /// queue" boundary test. Not compiled into a production build.
    #[cfg(test)]
    enqueues: std::sync::atomic::AtomicUsize,
}

impl<T: Send + 'static> Task<T> {
    /// Push this task onto its band ring. The caller must have just claimed the
    /// `SCHEDULED` state, so exactly one ring entry exists per task at a time.
    fn enqueue(self: &Arc<Self>) {
        #[cfg(test)]
        self.enqueues.fetch_add(1, Ordering::Relaxed);

        // `Entry` finalizes the task if it is dropped without running — see its
        // `Drop`. Both `request` failure modes drop the callback, so the ring
        // rejecting us and the band shutting down under us are both covered.
        let mut entry = Entry {
            task: Some(Arc::clone(self)),
        };
        let cb: Callback = Box::new(move || entry.run());
        if self.callbacks.request(self.priority, cb).is_err() {
            self.state.store(DONE, Ordering::Release);
            tracing::error!(
                target: "epics_base_rs::runtime::future_exec",
                "spawn_future: callback ring full; task dropped, handle resolves cancelled"
            );
        }
    }

    /// Poll the task once on the calling worker, then either publish its
    /// outcome or release the worker.
    fn run(self: Arc<Self>) {
        // SCHEDULED → RUNNING. Anything else means the task was finalized out
        // from under this ring entry; there is nothing left to poll.
        if self
            .state
            .compare_exchange(SCHEDULED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let Some(mut fut) = self.future.lock().unwrap().take() else {
            // Already consumed by a terminal path.
            self.state.store(DONE, Ordering::Release);
            return;
        };

        // Cancel is checked here, before the poll — the same point the previous
        // park-driver checked it, so "the task is dropped at its next
        // suspension point" is unchanged. Dropping `fut` here runs its
        // destructors, exactly as a cancelled tokio task's does.
        if self.shared.control.abort.load(Ordering::Acquire) {
            drop(fut);
            self.state.store(DONE, Ordering::Release);
            self.shared.finalize(Err(JoinError::cancelled()));
            return;
        }

        // The waker IS the task: waking re-enqueues it (module docs).
        let waker = Waker::from(Arc::clone(&self));
        // callback.c:210-235 drain-loop invariant: a panicking future must not
        // tear down the band worker.
        let polled = catch_unwind(AssertUnwindSafe(|| {
            let mut cx = Context::from_waker(&waker);
            fut.as_mut().poll(&mut cx)
        }));

        match polled {
            Ok(Poll::Ready(value)) => {
                drop(fut);
                self.state.store(DONE, Ordering::Release);
                self.shared.finalize(Ok(value));
            }
            Err(_panic) => {
                drop(fut);
                self.state.store(DONE, Ordering::Release);
                self.shared.finalize(Err(JoinError::panicked()));
            }
            Ok(Poll::Pending) => {
                // Put the future back BEFORE announcing we are schedulable
                // again, or the worker that picks us up next could find an
                // empty slot.
                *self.future.lock().unwrap() = Some(fut);
                if self
                    .state
                    .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    // RUNNING_NOTIFIED: a wake (or an abort) landed mid-poll.
                    // It was deliberately not enqueued then — that is this
                    // path's job, and re-enqueueing at the tail is what keeps a
                    // self-waking task from monopolising the worker.
                    self.state.store(SCHEDULED, Ordering::Release);
                    self.enqueue();
                }
            }
        }
    }
}

impl<T: Send + 'static> Schedulable for Task<T> {
    fn schedule(self: Arc<Self>) {
        loop {
            match self.state.load(Ordering::Acquire) {
                IDLE => {
                    if self
                        .state
                        .compare_exchange(IDLE, SCHEDULED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        self.enqueue();
                        return;
                    }
                }
                RUNNING => {
                    // Defer to the poll that is in flight — it re-enqueues.
                    if self
                        .state
                        .compare_exchange(
                            RUNNING,
                            RUNNING_NOTIFIED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                // SCHEDULED: already on a ring. RUNNING_NOTIFIED: already
                // deferred. DONE: nothing to run. All redundant.
                _ => return,
            }
        }
    }
}

impl<T: Send + 'static> Wake for Task<T> {
    fn wake(self: Arc<Self>) {
        Schedulable::schedule(self);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        Schedulable::schedule(Arc::clone(self));
    }
}

impl<T> Drop for Task<T> {
    /// The task became unreachable — no ring entry, no live waker — so nothing
    /// will ever poll it again. Resolve the joiner rather than strand it.
    /// A no-op on the normal paths, where `finalize` has already run.
    fn drop(&mut self) {
        self.shared.finalize(Err(JoinError::cancelled()));
    }
}

/// One ring entry for a task, with the "ran or was dropped" bookkeeping.
///
/// A [`Callback`] is a `FnOnce` that the pool may drop instead of calling — on
/// a full ring, or when the band shuts down (`callback.c:237-284` semantics,
/// see [`super::callback_executor::CallbackHandle::request`]). Either way this
/// task's only scheduled run is gone and its state is stuck at `SCHEDULED`, so
/// no later wake would re-enqueue it. `Drop` closes that: an entry that is
/// dropped un-run finalizes its task as cancelled.
struct Entry<T> {
    task: Option<Arc<Task<T>>>,
}

impl<T: Send + 'static> Entry<T> {
    fn run(&mut self) {
        if let Some(task) = self.task.take() {
            task.run();
        }
    }
}

impl<T> Drop for Entry<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.state.store(DONE, Ordering::Release);
            // Drop the future here rather than leaving it to the task's own
            // `Drop`, which may not run promptly if a waker still holds a
            // reference. Its destructors are part of the cancel.
            let _ = task.future.lock().unwrap().take();
            task.shared.finalize(Err(JoinError::cancelled()));
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

// `tokio::task::AbortHandle` is `Debug`, and call sites rely on it: pva's
// `server_native::tcp::AbortOnDrop` is a `#[derive(Debug)]` newtype over
// whichever handle the seam selected. `Control` holds a `Mutex<Option<Weak<dyn
// Schedulable>>>` that cannot be derived, so the mirror is written out — the
// two flags are the whole observable state of the handle.
impl std::fmt::Debug for AbortHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbortHandle")
            .field("aborted", &self.control.abort.load(Ordering::Relaxed))
            .field("finished", &self.control.finished.load(Ordering::Acquire))
            .finish()
    }
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

/// Spawn `fut` onto the callback pool behind `callbacks`, polled on
/// `priority`-band workers. Returns immediately with a [`JoinFuture`].
///
/// The task occupies a worker only for the duration of each poll; between polls
/// it holds nothing (module docs). If the band's ring is full (C
/// `S_db_bufFull`) the task cannot be enqueued and the returned handle resolves
/// as cancelled with an error logged — mirroring the fact that a tokio spawn
/// never fails while still giving the caller a handle that resolves.
pub fn spawn_future<F>(
    callbacks: &CallbackHandle,
    priority: CallbackPriority,
    fut: F,
) -> JoinFuture<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_task(callbacks, priority, fut).0
}

/// [`spawn_future`], also handing back the task itself so a test can inspect
/// its scheduling. Dropping the extra `Arc` is harmless: the ring entry holds
/// its own reference (and if the enqueue was rejected, dropping the last
/// reference is exactly what resolves the handle).
fn spawn_task<F>(
    callbacks: &CallbackHandle,
    priority: CallbackPriority,
    fut: F,
) -> (JoinFuture<F::Output>, Arc<Task<F::Output>>)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let shared = Shared::new();
    let task = Arc::new(Task {
        // Claimed immediately: the spawn itself is the first schedule.
        state: AtomicU8::new(SCHEDULED),
        future: Mutex::new(Some(Box::pin(fut))),
        shared: Arc::clone(&shared),
        callbacks: callbacks.clone(),
        priority,
        #[cfg(test)]
        enqueues: std::sync::atomic::AtomicUsize::new(0),
    });
    *shared.control.task.lock().unwrap() = Some(Arc::downgrade(&task) as Weak<dyn Schedulable>);
    task.enqueue();
    (JoinFuture { shared }, task)
}

/// Run a blocking closure `f` on a callback-pool worker — the RTEMS backend for
/// [`crate::runtime::task::spawn_blocking`]. Returns a [`JoinFuture`] resolving
/// to `f`'s return value.
///
/// A blocking closure has no suspension point, so it cannot be aborted
/// mid-run (as `tokio::task::spawn_blocking` also cannot); `abort` before it
/// starts still cancels it. A panic is isolated exactly as for
/// [`spawn_future`]. It holds its worker for its whole run — that is what
/// "blocking" means, and unlike a spawned future there is nothing to yield at.
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
    // Same "the pool may drop a callback instead of calling it" hazard as a
    // spawned future's ring entry: resolve the handle either way.
    let mut guard = FinalizeOnDrop {
        shared: Some(Arc::clone(&shared)),
    };
    let callback: Callback = Box::new(move || {
        guard.defuse();
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
    }
    JoinFuture { shared }
}

/// Resolves a handle as cancelled unless [`defuse`](Self::defuse)d — the
/// blocking-closure counterpart of [`Entry`]'s `Drop`.
struct FinalizeOnDrop<R> {
    shared: Option<Arc<Shared<R>>>,
}

impl<R> FinalizeOnDrop<R> {
    fn defuse(&mut self) {
        self.shared = None;
    }
}

impl<R> Drop for FinalizeOnDrop<R> {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            shared.finalize(Err(JoinError::cancelled()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::background::callback_executor::{
        CallbackPool, DEFAULT_QUEUE_SIZE, DEFAULT_THREADS_PER_PRIORITY,
    };
    use crate::runtime::background::delayed_timer::DelayedTimer;
    use crate::runtime::background::timer_sleep::sleep;
    use crate::runtime::task::park_on_interruptible as drive;
    use std::sync::mpsc;
    use std::time::Duration;

    const T: Duration = Duration::from_secs(5);

    /// Block the test thread on a `JoinFuture`, returning its `Result`.
    fn join<T>(jf: JoinFuture<T>) -> Result<T, JoinError> {
        drive(jf, || false).expect("uncancelled join returned None")
    }

    // `JoinFuture` is single-await; several tests need both a method call and a
    // join. Re-expose the shared handle by cloning the Arc so the test can do
    // both without moving the handle into `join`.
    fn jf_reborrow<T>(jf: &JoinFuture<T>) -> JoinFuture<T> {
        JoinFuture {
            shared: Arc::clone(&jf.shared),
        }
    }

    /// Suspends, waking itself each poll, until the flag is set — the
    /// unbounded [`YieldN`]. For tests whose subject is *that* a task is
    /// suspended rather than how often: a fixed count makes the test thread
    /// race the worker, because the worker may exhaust every yield before the
    /// thread reaches its next statement.
    struct YieldUntil(Arc<AtomicBool>);

    impl Future for YieldUntil {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    /// Returns `Pending` `n` times, waking itself each time, then `Ready`.
    /// Exercises the wake-during-poll (`RUNNING_NOTIFIED`) edge on every step.
    struct YieldN(usize);

    impl Future for YieldN {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 == 0 {
                return Poll::Ready(());
            }
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    // -- basic execution ----------------------------------------------------

    #[test]
    fn future_runs_to_completion() {
        let pool = CallbackPool::new();
        let jf = spawn_future(&pool.handle(), DEFAULT_SPAWN_PRIORITY, async { 42u32 });
        assert_eq!(join(jf).unwrap(), 42);
    }

    #[test]
    fn future_awaiting_cross_thread_primitive_completes() {
        // The A2 precondition: a spawned tail that awaits a runtime-agnostic
        // tokio::sync primitive, woken from ANOTHER thread, must complete with
        // no tokio runtime present.
        let pool = CallbackPool::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<u32>();
        let jf = spawn_future(&pool.handle(), DEFAULT_SPAWN_PRIORITY, async move {
            rx.await.unwrap()
        });
        // Send from the test thread after the task has already yielded.
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
    fn panic_after_a_yield_is_isolated_too() {
        // Boundary partner to the above: the panic happens on a LATER poll, so
        // it unwinds out of a re-enqueued run rather than the first one.
        let pool = CallbackPool::new();
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async {
            YieldN(3).await;
            panic!("late boom");
        });
        assert!(!join::<()>(jf).unwrap_err().is_cancelled());

        let jf2 = spawn_future(&pool.handle(), CallbackPriority::Medium, async { 7u32 });
        assert_eq!(join(jf2).unwrap(), 7);
    }

    // -- invariant 4: a synchronous completion never round-trips the queue ---

    #[test]
    fn synchronously_ready_future_is_enqueued_exactly_once() {
        // Boundary: zero suspensions. The spawn itself is one enqueue; a task
        // that is Ready on its first poll must add no more.
        let pool = CallbackPool::new();
        let (jf, task) = spawn_task(&pool.handle(), CallbackPriority::Medium, async { 1u8 });
        assert_eq!(join(jf).unwrap(), 1);
        assert_eq!(
            task.enqueues.load(Ordering::Relaxed),
            1,
            "a future that completes on its first poll must not round-trip the ring"
        );
    }

    #[test]
    fn each_suspension_costs_exactly_one_re_enqueue() {
        // The other side of the same boundary: N suspensions → N re-enqueues,
        // i.e. the queue is used once per wake and never speculatively.
        let pool = CallbackPool::new();
        let (jf, task) = spawn_task(&pool.handle(), CallbackPriority::Medium, YieldN(3));
        join(jf).unwrap();
        assert_eq!(
            task.enqueues.load(Ordering::Relaxed),
            4,
            "expected 1 spawn + 3 wakes"
        );
    }

    // -- invariant 3: a released worker is reused; a woken task does not starve

    #[test]
    fn a_yielding_task_releases_the_worker_to_a_queued_task() {
        // Single worker, two tasks. `slow` stays suspended until this thread
        // releases it; `quick` is queued behind it. Under the old
        // park-a-worker design `quick` could not run until `slow` finished, so
        // it would never arrive at all.
        //
        // The gate is what makes the order an assertion rather than a race.
        // Holding `slow` for a fixed number of yields instead leaves the
        // executor free to burn all of them before this thread has enqueued
        // `quick` — then "slow" arrives first through no fault of the
        // executor, which is the flake this shape removes. Gated, `slow`
        // *cannot* finish before "quick" is received, so "quick" arriving is
        // itself the proof that a suspended task released the band's only
        // worker.
        assert_eq!(DEFAULT_THREADS_PER_PRIORITY, 1);
        let pool = CallbackPool::new();
        let (tx, rx) = mpsc::channel::<&'static str>();
        let gate = Arc::new(AtomicBool::new(false));

        let tx_slow = tx.clone();
        let gate_slow = Arc::clone(&gate);
        let slow = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            YieldUntil(gate_slow).await;
            tx_slow.send("slow").unwrap();
        });
        let quick = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            tx.send("quick").unwrap();
        });

        assert_eq!(
            rx.recv_timeout(T).unwrap(),
            "quick",
            "a suspended task must not hold the band's only worker"
        );
        gate.store(true, Ordering::Release);
        assert_eq!(
            rx.recv_timeout(T).unwrap(),
            "slow",
            "a repeatedly re-enqueued task must still make progress"
        );
        join(quick).unwrap();
        join(slow).unwrap();
    }

    #[test]
    fn many_self_waking_tasks_all_finish_on_one_worker() {
        // No-starvation under contention: 8 tasks, each waking itself 25 times,
        // multiplexed over a single worker. Tail re-enqueue makes this fair
        // enough that every one of them terminates.
        assert_eq!(DEFAULT_THREADS_PER_PRIORITY, 1);
        let pool = CallbackPool::new();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
                    YieldN(25).await;
                    i
                })
            })
            .collect();
        for (i, jf) in handles.into_iter().enumerate() {
            assert_eq!(join(jf).unwrap(), i);
        }
    }

    // -- invariant 1: the sleep-wake inline path ----------------------------

    #[test]
    fn spawned_future_awaiting_sleep_does_not_self_deadlock() {
        // Regression for the sleep-wake self-deadlock (bug_pattern
        // rtems-exec-sleep-wake-band-deadlock): a future SPAWNED ON THE POOL
        // that awaits `timer_sleep::sleep` must complete. This is the exact
        // `spawn(async { sleep().await })` shape that ODLY/SDLY async-record
        // reprocessing uses on the RTEMS backend.
        //
        // Why the existing `timer_sleep` unit tests did NOT catch it: they
        // `drive()` the Sleep on the TEST thread, leaving the pool's single
        // Medium worker free to run the wake callback. Here the future runs on
        // the pool worker (via `spawn_future`) — so with the old behavior, where
        // the sleep-wake was `sink.request(Medium, ...)`, the wake queued behind
        // the very worker parked on this future (one worker per band) and never
        // ran. We must OBSERVE completion via a channel, NOT `join()`/`drive()`
        // on the test thread, or we would re-introduce the free-worker escape
        // hatch and stop reproducing the deadlock.
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let th = timer.handle();

        let (done_tx, done_rx) = mpsc::channel::<()>();
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            sleep(&th, Duration::from_millis(50)).await;
            done_tx.send(()).unwrap();
        });

        // With the fix (inline wake on the timer thread) the sleep fires and the
        // future finishes well inside T. With the pre-fix band-dispatch wake this
        // recv times out because the wake starves behind the parked worker.
        done_rx.recv_timeout(T).expect(
            "spawned future awaiting sleep deadlocked (wake starved behind its own worker)",
        );
        assert!(join(jf).is_ok());
    }

    #[test]
    fn a_sleep_wake_needs_no_pool_worker() {
        // The inline-wake guarantee, stated directly: pin the band's ONLY
        // worker inside an unrelated blocking callback, and a sleeping task's
        // wake must still land. If the wake needed a worker it would sit behind
        // the pinned one and `fired` would stay false.
        assert_eq!(DEFAULT_THREADS_PER_PRIORITY, 1);
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());

        let (pinned_tx, pinned_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || {
                pinned_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }),
        )
        .unwrap();
        pinned_rx.recv_timeout(T).unwrap(); // the sole Medium worker is busy

        let (woke_tx, woke_rx) = mpsc::channel::<()>();
        // Drive the Sleep on the TEST thread — no pool worker involved at all,
        // so what this observes is purely "did the wake arrive".
        let th = timer.handle();
        let sleeper = std::thread::spawn(move || {
            drive(sleep(&th, Duration::from_millis(30)), || false).unwrap();
            woke_tx.send(()).unwrap();
        });
        woke_rx
            .recv_timeout(T)
            .expect("sleep wake did not arrive while the band's worker was pinned");

        release_tx.send(()).unwrap();
        sleeper.join().unwrap();
    }

    #[test]
    fn three_sleeping_tails_share_one_worker() {
        // The old failure mode, gone. DEFAULT_THREADS_PER_PRIORITY is 1, so
        // under the park-a-worker-per-future design these three tails would
        // have needed three workers: #1 would hold the only one for its whole
        // sleep, and #2 and #3 could not even START until it finished.
        //
        // The assertion is structural, not a stopwatch: every task announces
        // Started before it sleeps and Done after. All three Starteds must
        // arrive before the first Done — which is only possible if each task
        // handed the worker back at its suspension point.
        assert_eq!(DEFAULT_THREADS_PER_PRIORITY, 1);
        let pool = CallbackPool::with_config(DEFAULT_QUEUE_SIZE, DEFAULT_THREADS_PER_PRIORITY);
        let timer = DelayedTimer::new(pool.handle());

        #[derive(Debug, PartialEq, Eq)]
        enum Ev {
            Started,
            Done(u8),
        }

        let (tx, rx) = mpsc::channel::<Ev>();
        let handles: Vec<_> = (0..3u8)
            .map(|i| {
                let th = timer.handle();
                let tx = tx.clone();
                spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
                    tx.send(Ev::Started).unwrap();
                    sleep(&th, Duration::from_millis(120)).await;
                    tx.send(Ev::Done(i)).unwrap();
                    i
                })
            })
            .collect();
        drop(tx);

        for n in 0..3 {
            assert_eq!(
                rx.recv_timeout(T).unwrap(),
                Ev::Started,
                "task {n} had not started before the first task finished — the \
                 worker was held across a suspension"
            );
        }
        let mut done: Vec<u8> = (0..3)
            .map(|_| match rx.recv_timeout(T).unwrap() {
                Ev::Done(i) => i,
                Ev::Started => panic!("only three tasks exist"),
            })
            .collect();
        done.sort_unstable();
        assert_eq!(done, vec![0, 1, 2], "all three tails must complete");

        for (i, jf) in handles.into_iter().enumerate() {
            assert_eq!(join(jf).unwrap() as usize, i);
        }
    }

    // -- invariant 2: abort ------------------------------------------------

    #[test]
    fn abort_while_idle_cancels_cleanly() {
        // Boundary: the task is IDLE — parked between polls on a primitive that
        // will never fire, holding no worker. Nothing but the abort can wake
        // it, so this is what proves `request_abort` schedules.
        let pool = CallbackPool::new();
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (ran_tx, ran_rx) = mpsc::channel();
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            ran_tx.send(()).unwrap();
            let _ = rx.await;
        });
        // The task has run at least once and yielded.
        ran_rx.recv_timeout(T).unwrap();
        std::thread::sleep(Duration::from_millis(20));

        jf.abort();
        let err = join(jf_reborrow(&jf)).unwrap_err();
        assert!(err.is_cancelled(), "aborted task must report cancelled");
        assert!(jf.is_finished());
    }

    #[test]
    fn abort_before_the_first_poll_cancels_without_running() {
        // Boundary: the task is SCHEDULED but not yet RUNNING. Pin the band's
        // only worker so the task cannot start, abort it, then release. It must
        // resolve cancelled and its body must never have executed.
        assert_eq!(DEFAULT_THREADS_PER_PRIORITY, 1);
        let pool = CallbackPool::new();
        let (pinned_tx, pinned_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || {
                pinned_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }),
        )
        .unwrap();
        pinned_rx.recv_timeout(T).unwrap();

        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            flag.store(true, Ordering::SeqCst);
        });
        jf.abort();
        release_tx.send(()).unwrap();

        assert!(join(jf_reborrow(&jf)).unwrap_err().is_cancelled());
        assert!(
            !ran.load(Ordering::SeqCst),
            "an abort observed before the first poll must not run the future"
        );
    }

    #[test]
    fn abort_during_a_poll_is_observed_at_the_next_one() {
        // Boundary: the abort lands while the task is RUNNING, i.e. inside a
        // synchronous stretch. tokio's contract (and the previous park-driver's)
        // is that it is observed at the NEXT suspension point, not mid-poll —
        // so the current poll finishes and the following one cancels.
        let pool = CallbackPool::new();
        let (in_poll_tx, in_poll_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&polls);

        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async move {
            counter.fetch_add(1, Ordering::SeqCst);
            // Synchronous stretch: tell the test we are inside the poll and
            // block here until it has aborted us.
            in_poll_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            YieldN(1).await; // the suspension point the cancel is observed at
            counter.fetch_add(100, Ordering::SeqCst);
        });

        in_poll_rx.recv_timeout(T).unwrap();
        jf.abort(); // lands while the task is RUNNING
        go_tx.send(()).unwrap();

        assert!(join(jf_reborrow(&jf)).unwrap_err().is_cancelled());
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "the code after the suspension point must not have run"
        );
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
    fn abort_after_completion_does_not_rewrite_the_outcome() {
        // Boundary: DONE. `finalize` is idempotent, so a late abort must not
        // turn a delivered value into a cancellation.
        let pool = CallbackPool::new();
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async { 5u32 });
        while !jf.is_finished() {
            std::thread::sleep(Duration::from_millis(2));
        }
        jf.abort();
        assert_eq!(join(jf).unwrap(), 5);
    }

    // -- every handle resolves ---------------------------------------------

    #[test]
    fn full_ring_resolves_the_handle_as_cancelled() {
        // Boundary: the spawn never reaches a worker at all. C `S_db_bufFull`
        // (callback.c:373) — the task is dropped, so the handle must resolve
        // rather than strand its joiner.
        let pool = CallbackPool::with_config(1, 1);
        let (pinned_tx, pinned_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || {
                pinned_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }),
        )
        .unwrap();
        pinned_rx.recv_timeout(T).unwrap();

        // Fill the single ring slot, then latch overflow.
        pool.request(CallbackPriority::Medium, Box::new(|| {}))
            .unwrap();
        let jf = spawn_future(&pool.handle(), CallbackPriority::Medium, async { 1u32 });
        assert!(
            join(jf).unwrap_err().is_cancelled(),
            "a rejected spawn must resolve its handle, not strand it"
        );

        release_tx.send(()).unwrap();
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
