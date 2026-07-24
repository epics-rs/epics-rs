//! Explicit-handle wrappers around the convenience PvaClient operations.
//!
//! Rust's async/await covers most of pvxs `client::Operation`'s job
//! implicitly (drop a future to cancel, `.await` to wait), but a
//! handle-style API is occasionally useful when:
//!
//! - You want to start an operation now and `.wait(timeout)` for it
//!   later from a different task.
//! - You want a `cancel()` that mirrors pvxs `Operation::cancel`: it
//!   reports whether the operation was still active and acts as a
//!   synchronization point, blocking until the spawned task has fully
//!   torn down before it returns (pvxs `client.h:127-130`).
//! - You want a thread-safe `interrupt()` that wakes a `wait()`
//!   without cancelling the underlying operation, mirroring pvxs
//!   `Operation::interrupt`.
//!
//! The handle is constructed from any future that returns
//! `PvaResult<T>`.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::TaskHandle;
use tokio::sync::{Notify, oneshot, watch};

use crate::error::{PvaError, PvaResult};

/// Outcome of a single `wait` body, computed while the result receiver
/// is borrowed; the `done` flag is updated afterwards so the borrow
/// never overlaps the `&mut self` write.
enum WaitOutcome<T> {
    /// The operation completed and produced a result.
    Completed(PvaResult<T>),
    /// The sender was dropped without sending (task aborted).
    Aborted,
    /// `interrupt()` woke the waiter; the operation keeps running.
    Interrupted,
    /// `cancel()` fired.
    Cancelled,
}

/// Handle to an in-flight operation. Pairs with the operation type
/// returned by `PvaClient::start_*` async methods.
pub struct PvaOperation<T: Send + 'static> {
    /// Spawned task running the underlying op. The seam's handle type
    /// (`tokio::task::JoinHandle` hosted, the runtime-free mirror on RTEMS) —
    /// it is what [`crate::client_native::operation`]'s `spawn` hands back and
    /// the only three methods used here (`abort`, `is_finished`, `await`) are
    /// the ones the mirror reproduces.
    join: TaskHandle<()>,
    /// Receiver for the op's final result. held by value (not
    /// `take`-n out per wait) and polled by `&mut`, so a `wait` that
    /// times out or is interrupted leaves the receiver in place and a
    /// later `wait` can still collect the result — matching pvxs
    /// `Operation::wait(timeout)`, which can be retried after a timeout.
    result_rx: oneshot::Receiver<PvaResult<T>>,
    /// `true` once a result has been successfully consumed; further
    /// `wait` calls then report "already consumed" (single-consumer
    /// final-result policy). A timeout/interrupt does NOT set this.
    done: bool,
    /// Pulsed by [`Self::interrupt`]; `wait*` selects on this and
    /// returns `PvaError::Interrupted` — a distinct variant
    /// from `PvaError::Timeout` so callers can tell an operator-driven
    /// wake-up from a real deadline.
    interrupt: Arc<Notify>,
    /// One-shot cancellation flag. When set, `wait*` short-circuits
    /// returning the abort error and the spawned task is aborted.
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Set exactly once, by [`ResultPublisher::publish`] inside the task, at
    /// the instant the operation produces its result — the **same event the
    /// caller observes** when `wait()` hands that result back. This, not the
    /// termination guard, is what "the operation completed" means; see
    /// [`Self::reached_terminal_state`].
    completed: Arc<std::sync::atomic::AtomicBool>,
    /// Receiver whose paired `watch::Sender` lives **inside** the spawned
    /// task. The sender is the operation's RAII termination guard: when
    /// the task's future stops running for any reason (normal completion,
    /// `abort()`, or panic-unwind) the sender drops and this receiver
    /// observes the channel as closed.
    ///
    /// Its **one** job is to be the synchronization point [`Self::cancel`]
    /// awaits, giving pvxs's "blocks until the in-progress callback has
    /// completed" guarantee — plus, as a corollary, evidence that a task
    /// which ended *without* a result (abort, panic-unwind) is over. It is
    /// deliberately NOT the answer to "did this operation complete": the
    /// result is published strictly before this channel closes, so a closed
    /// channel lags completion. `watch` is lost-wakeup-safe: a closure that
    /// races registration is reported by the next `changed()`/`has_changed()`
    /// rather than missed.
    terminated_rx: watch::Receiver<()>,
}

/// The spawned task's body: the operation future and the RAII termination
/// guard bound into **one value whose drop order the language fixes**.
///
/// This type exists because an `async move` block does not have *a* drop
/// order — it has two, and they are opposites:
///
/// - A generator that has been polled at least once drops its **saved
///   locals** in reverse declaration order. Writing `let _guard = guard;`
///   ahead of `fut.await` therefore drops the operation future *first* and
///   the guard *second* — the order the contract needs.
/// - A generator that has **never been polled** still holds its captures as
///   **upvars**, and upvars drop in *capture* order. The same source text
///   then drops the guard *first* and the operation future *second*.
///
/// The exec-model backend takes that second path routinely: `abort()` is
/// latched and the task scheduled, and the flag is observed at the top of
/// the task's *first* `run()` — so the generator is dropped un-started
/// (`future_exec.rs` `Task::run`'s abort arm, and `Entry::drop` for a ring
/// entry dropped un-run). The guard then closed the watch channel while the
/// operation future and everything it captured were still alive, waking
/// [`PvaOperation::cancel`] early and breaking its synchronization-point
/// contract. Hosted, the task is almost always polled before the abort
/// lands, which is why the same code passes on tokio.
///
/// Struct fields drop in declaration order, and that rule holds in **every**
/// state, polled or not. `fut` is declared first and `_guard` second, so
/// "the operation future has been dropped" and "the channel has closed" are
/// the same event by construction — one uniform rule instead of a
/// per-state one.
struct Terminating {
    /// Dropped **first**. Boxed so this struct is `Unpin` and [`Future::poll`]
    /// needs no unsafe pin projection.
    fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    /// Dropped **last**, closing the watch channel [`PvaOperation::cancel`]
    /// awaits as its synchronization point. Never read directly — its `Drop`
    /// is the entire point, so it must stay the final field. Note this
    /// instant is strictly *later* than the result publish inside `fut`, so
    /// it is not what "the operation completed" means; see
    /// [`PvaOperation::reached_terminal_state`].
    _guard: watch::Sender<()>,
}

/// The one and only way the spawned task publishes its result.
///
/// Publishing the result and recording "this operation completed" have to be
/// a single, inseparable act, or the two drift apart and callers disagree
/// about whether the operation finished. [`Self::publish`] consumes `self`,
/// so the `oneshot::Sender` cannot be reached without also setting the flag —
/// the illegal state "result delivered but not marked completed" is not
/// constructible rather than merely avoided by review.
///
/// The flag is stored *before* the value is handed over, which fixes the
/// direction that matters: any observer that can see the result can also see
/// the completion. The converse gap is unobservable — [`PvaOperation::wait`]
/// takes `&mut self` while [`PvaOperation::cancel`] takes `&self`, so no two
/// callers can inspect one handle concurrently.
struct ResultPublisher<T> {
    tx: oneshot::Sender<PvaResult<T>>,
    completed: Arc<std::sync::atomic::AtomicBool>,
}

impl<T> ResultPublisher<T> {
    fn publish(self, v: PvaResult<T>) {
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.tx.send(v);
    }
}

impl std::future::Future for Terminating {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        // `Pin<Box<_>>` and `watch::Sender` are both `Unpin`, so `Self` is
        // `Unpin` and `get_mut` is safe: the boxed future owns its own
        // stable address, independent of where this struct lives.
        self.get_mut().fut.as_mut().poll(cx)
    }
}

impl<T: Send + 'static> PvaOperation<T> {
    /// Spawn `fut` and return a handle. Dropping the handle aborts the
    /// spawned task — pvxs RAII `~Operation` performs the same implied
    /// cancel (client.cpp:314-320). [`Self::cancel`] is the explicit,
    /// awaitable form that also reports whether the operation was still
    /// active and blocks until the task has terminated.
    pub fn spawn<F>(fut: F) -> Self
    where
        F: std::future::Future<Output = PvaResult<T>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        // The sender is the task's RAII termination guard: it drops exactly
        // when the future stops running (completion, abort, or panic-unwind),
        // closing the channel. It is paired with the operation future inside
        // `Terminating` rather than bound by a `let` inside an async block,
        // because only a struct gives the two a drop order that holds whether
        // or not the task was ever polled — see `Terminating`'s docs.
        let (term_tx, terminated_rx) = watch::channel(());
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let publisher = ResultPublisher {
            tx,
            completed: completed.clone(),
        };
        let join = epics_base_rs::runtime::task::spawn(Terminating {
            // The result publish stays *inside* the guarded future, so a
            // normal completion still publishes strictly before the channel
            // closes — the ordering the `Terminating` fix established, and
            // exactly why completion cannot be inferred from the guard.
            fut: Box::pin(async move {
                let v = fut.await;
                publisher.publish(v);
            }),
            _guard: term_tx,
        });
        Self {
            join,
            result_rx: rx,
            done: false,
            interrupt: Arc::new(Notify::new()),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            completed,
            terminated_rx,
        }
    }

    /// **The single owner of "has this operation finished".**
    ///
    /// Every caller-visible terminal question — [`Self::cancel`]'s activeness
    /// answer and [`Self::is_done`] — routes through here, so no site invents
    /// its own rule. An operation is terminal when either
    ///
    /// - it **produced a result** (`completed`, set inseparably with the
    ///   publish by [`ResultPublisher::publish`]) — the same event `wait()`
    ///   reports to the caller; or
    /// - its **task is torn down without one** (the termination guard closed),
    ///   which is how an abort or a panic-unwind ends the operation.
    ///
    /// The guard alone is NOT this question, and using it as one was the
    /// defect: the result publishes strictly *before* the guard drops, so
    /// between those two instants an operation that every caller can already
    /// see as finished still had an open channel. Monotonic — neither
    /// disjunct ever goes back to false.
    fn reached_terminal_state(&self) -> bool {
        self.completed.load(std::sync::atomic::Ordering::Acquire)
            || self.terminated_rx.has_changed().is_err()
    }

    /// Block until the operation completes (matching pvxs
    /// `Operation::wait()`). Times out per-call: pass `None` to wait
    /// forever, or `Some(d)` for a deadline. Returns
    /// `PvaError::Timeout` when the deadline expires and the distinct
    /// `PvaError::Interrupted` when woken by [`Self::interrupt`]
    ///. Neither consumes the result — a later `wait` can
    /// still collect it.
    pub async fn wait(&mut self, timeout: Option<Duration>) -> PvaResult<T> {
        if self.done {
            return Err(PvaError::Protocol(
                "Operation result already consumed".into(),
            ));
        }
        // A produced result is terminal and wins over a later cancellation.
        // pvxs treats a completed `Operation` as final: `cancel()` of it
        // returns false and leaves the buffered reply intact. `cancel()`
        // here sets the `cancelled` flag before it can observe that the task
        // already terminated, so a cancel issued as idempotent cleanup after
        // completion must not replace the already-buffered Ok/Err result
        // with `Operation cancelled`. Read the buffered result *before*
        // honouring the cancellation flag.
        match self.result_rx.try_recv() {
            Ok(r) => {
                self.done = true;
                return r;
            }
            // `Empty`: still running. `Closed`: the task ended without
            // sending (aborted — possibly by `cancel()`). Either way, fall
            // through to the cancellation check and the awaiting select.
            Err(oneshot::error::TryRecvError::Empty | oneshot::error::TryRecvError::Closed) => {}
        }
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PvaError::Protocol("Operation cancelled".into()));
        }

        let interrupt = self.interrupt.clone();
        let cancelled = self.cancelled.clone();
        // Borrow the receiver by `&mut` so a timeout/interrupt leaves it
        // in place for a later `wait`. `oneshot::Receiver`
        // is `Unpin`, so `&mut rx` is itself a `Future`.
        let rx = &mut self.result_rx;
        let body = async {
            tokio::select! {
                v = &mut *rx => match v {
                    Ok(r) => WaitOutcome::Completed(r),
                    Err(_) => WaitOutcome::Aborted,
                },
                _ = interrupt.notified() => WaitOutcome::Interrupted,
                _ = wait_for_cancel(cancelled) => WaitOutcome::Cancelled,
            }
        };
        let outcome = match timeout {
            // On timeout the body (and its `&mut` borrow of `result_rx`)
            // is dropped, leaving the receiver intact in `self`.
            Some(d) => match epics_base_rs::runtime::task::timeout(d, body).await {
                Ok(o) => o,
                Err(_) => return Err(PvaError::Timeout),
            },
            None => body.await,
        };
        match outcome {
            WaitOutcome::Completed(r) => {
                self.done = true;
                r
            }
            // Interrupt/timeout do not consume; a later `wait` retries.
            // interrupt is its own variant, not Timeout.
            WaitOutcome::Interrupted => Err(PvaError::Interrupted),
            WaitOutcome::Cancelled => Err(PvaError::Protocol("Operation cancelled".into())),
            WaitOutcome::Aborted => Err(PvaError::Protocol("Operation aborted".into())),
        }
    }

    /// Cancel the operation. Idempotent. Mirrors pvxs `Operation::cancel`
    /// (client.h:127-130, clientget.cpp:173-203):
    ///
    /// - Returns whether the operation was **still active** — `true` iff
    ///   it had neither already completed nor already been cancelled.
    /// - Acts as a **synchronization point**: it `.await`s until the
    ///   spawned task has fully torn down, so once it returns no callback
    ///   captured by the operation future can still be running. A later
    ///   [`Self::wait`] then reports `PvaError::Protocol("Operation
    ///   cancelled")` via the cancellation flag.
    pub async fn cancel(&self) -> bool {
        // `swap` makes the "was active" decision exactly once across
        // repeated cancels: only the first cancel that finds the flag clear
        // AND the operation not yet terminal reports active.
        //
        // Terminality comes from `reached_terminal_state`, never from the
        // guard directly: the guard closes strictly *after* the result is
        // published, so reading it here used to report "still active" for an
        // operation whose result `wait()` had already returned.
        let already_cancelled = self
            .cancelled
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        let was_active = !already_cancelled && !self.reached_terminal_state();
        self.join.abort();
        // Synchronization point: wait until the task's `watch::Sender` has
        // dropped (channel closed), so cancel() returning means the
        // operation future is provably no longer running. We never send a
        // value, so `changed()` resolves only with `Err` on close.
        let mut rx = self.terminated_rx.clone();
        while rx.changed().await.is_ok() {}
        was_active
    }

    /// Wake a pending [`Self::wait`] without cancelling the operation
    /// — the wait returns `PvaError::Interrupted` and the
    /// underlying op keeps running. Mirrors pvxs `Operation::interrupt`.
    pub fn interrupt(&self) {
        self.interrupt.notify_waiters();
    }

    /// True iff the operation has reached a terminal state — it produced a
    /// result, or its task was torn down without one (abort, panic-unwind).
    ///
    /// This is deliberately the **same question** [`Self::cancel`] negates to
    /// answer "was it still active", and it is answered in the one place both
    /// share, [`Self::reached_terminal_state`]. So `is_done()` is true exactly
    /// when a `cancel()` would report not-active, and after `wait()` returns a
    /// value it is true immediately — the result *is* the terminal event.
    ///
    /// It is **not** "the spawned task's future has been dropped". That is a
    /// strictly later instant (the result publishes before the termination
    /// guard closes), it is `cancel()`'s synchronization point rather than a
    /// caller-visible state, and reading it here would make `is_done()`
    /// briefly false for an operation whose result the caller already holds.
    /// `join.is_finished()` is rejected for the older reason, unchanged:
    /// under the exec-model seam the join handle's finished flag flips at yet
    /// another instant, so it agrees with neither.
    pub fn is_done(&self) -> bool {
        self.reached_terminal_state()
    }
}

impl<T: Send + 'static> Drop for PvaOperation<T> {
    fn drop(&mut self) {
        // Drop without cancel still aborts the task to avoid orphan
        // background work. pvxs's RAII `~Operation` does the same
        // (calls cancel internally).
        self.join.abort();
    }
}

async fn wait_for_cancel(flag: Arc<std::sync::atomic::AtomicBool>) {
    while !flag.load(std::sync::atomic::Ordering::Acquire) {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[epics_macros_rs::epics_test]
    async fn wait_returns_value() {
        let mut op = PvaOperation::spawn(async { Ok::<i32, _>(42) });
        let v = op.wait(Some(Duration::from_secs(1))).await.unwrap();
        assert_eq!(v, 42);
        assert!(op.is_done());
    }

    #[epics_macros_rs::epics_test]
    async fn wait_times_out() {
        let mut op = PvaOperation::<()>::spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        let r = op.wait(Some(Duration::from_millis(50))).await;
        assert!(matches!(r, Err(PvaError::Timeout)));
    }

    #[tokio::test]
    async fn interrupt_wakes_waiter_op_continues() {
        let mut op = PvaOperation::<i32>::spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
            Ok(7)
        });
        let interrupter = op.interrupt.clone();
        tokio::spawn(async move {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
            interrupter.notify_waiters();
        });
        let r = op.wait(Some(Duration::from_secs(5))).await;
        // interrupt is its own variant, not Timeout.
        assert!(matches!(r, Err(PvaError::Interrupted)));
        assert!(
            !matches!(r, Err(PvaError::Timeout)),
            "interrupt must not be reported as a deadline timeout"
        );
        // interrupt does NOT consume the result. The op
        // completes and a second wait still collects its value.
        let v = op.wait(Some(Duration::from_secs(1))).await.unwrap();
        assert_eq!(v, 7);
        assert!(op.is_done());
    }

    /// a real deadline expiry and an explicit interrupt must
    /// surface as distinct variants so timeout-specific caller logic
    /// does not match an interrupt.
    #[tokio::test]
    async fn timeout_and_interrupt_are_distinct_variants() {
        // Real deadline: op never completes within the window.
        let mut slow = PvaOperation::<()>::spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        let deadline = slow.wait(Some(Duration::from_millis(30))).await;
        assert!(matches!(deadline, Err(PvaError::Timeout)));
        assert!(!matches!(deadline, Err(PvaError::Interrupted)));

        // Interrupt path: woken before completion.
        let mut op = PvaOperation::<i32>::spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
            Ok(1)
        });
        let interrupter = op.interrupt.clone();
        tokio::spawn(async move {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
            interrupter.notify_waiters();
        });
        let interrupted = op.wait(Some(Duration::from_secs(5))).await;
        assert!(matches!(interrupted, Err(PvaError::Interrupted)));
        assert!(!matches!(interrupted, Err(PvaError::Timeout)));
    }

    #[epics_macros_rs::epics_test]
    async fn cancel_aborts_op() {
        let mut op = PvaOperation::<i32>::spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_secs(60)).await;
            Ok(0)
        });
        // Cancelling an in-flight op reports it was active and, as a
        // synchronization point, returns only after the task has stopped.
        let was_active = op.cancel().await;
        assert!(
            was_active,
            "cancel of a running op must report it was active"
        );
        assert!(
            op.is_done(),
            "cancel() must not return until the spawned task has terminated"
        );
        let r = op.wait(Some(Duration::from_secs(1))).await;
        assert!(matches!(r, Err(PvaError::Protocol(_))));
    }

    /// pvxs `Operation::cancel()` returns `false` once the operation has
    /// already completed (clientget.cpp:173-184 reports the prior active
    /// state). A second cancel is also `false` (idempotent).
    #[epics_macros_rs::epics_test]
    async fn cancel_after_completion_reports_not_active() {
        let mut op = PvaOperation::spawn(async { Ok::<i32, _>(11) });
        assert_eq!(op.wait(Some(Duration::from_secs(1))).await.unwrap(), 11);
        // Already completed → not active.
        assert!(
            !op.cancel().await,
            "cancel after a completed op must report not-active"
        );
        // Idempotent: a second cancel is also not-active.
        assert!(!op.cancel().await, "repeated cancel must be idempotent");
    }

    /// A published result is terminal **immediately**, without waiting for
    /// the termination guard to close — asserted deterministically in the
    /// exact window that used to be decided the other way.
    ///
    /// The result is published strictly before the guard drops (the ordering
    /// `Terminating` preserves on purpose), so between those two instants an
    /// operation is finished from every caller-visible angle while its
    /// channel is still open. Deciding activeness from the guard there
    /// reported "still active" for a completed operation, which is what made
    /// `cancel_after_completion_reports_not_active` fail intermittently
    /// feature-ON. The state is built by hand so the window is held open for
    /// as long as the assertions need, rather than raced for.
    #[epics_macros_rs::epics_test]
    async fn a_published_result_is_terminal_while_the_guard_is_still_open() {
        use std::sync::atomic::AtomicBool;

        let (tx, result_rx) = oneshot::channel::<PvaResult<i32>>();
        let (term_tx, terminated_rx) = watch::channel(());
        let completed = Arc::new(AtomicBool::new(false));
        let publisher = ResultPublisher {
            tx,
            completed: completed.clone(),
        };
        let op = PvaOperation::<i32> {
            join: epics_base_rs::runtime::task::spawn(std::future::pending::<()>()),
            result_rx,
            done: false,
            interrupt: Arc::new(Notify::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
            completed,
            terminated_rx,
        };

        // Nothing has happened yet: neither terminal event.
        assert!(!op.is_done(), "an un-finished operation is not terminal");

        // The operation produces its result. `term_tx` is still held here —
        // this IS the window between publish and task teardown.
        publisher.publish(Ok(11));
        assert!(
            op.terminated_rx.has_changed().is_ok(),
            "the termination guard must still be open — that is the window under test"
        );
        assert!(
            op.is_done(),
            "a published result is terminal even though the guard is still open"
        );

        // `cancel()` decides activeness before it parks on the guard, so poll
        // it once while the guard is still held: the answer it computed in
        // the window is the one it must return.
        let mut cancel = Box::pin(op.cancel());
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            cancel.as_mut().poll(&mut cx).is_pending(),
            "cancel() parks on the still-open termination guard"
        );
        // Release the guard so the synchronization point can complete.
        drop(term_tx);
        assert!(
            !cancel.await,
            "cancel of an operation that already published its result must \
             report not-active, even though the guard was open when it was called"
        );
    }

    /// cancel() is a synchronization point: a resource held by the
    /// operation future must be observably released by the time cancel()
    /// returns (pvxs "blocks until any in-progress callback has finished").
    #[epics_macros_rs::epics_test]
    async fn cancel_is_a_sync_point_resource_released() {
        use std::sync::Arc as StdArc;
        let held = StdArc::new(());
        let inner = held.clone();
        let op = PvaOperation::<()>::spawn(async move {
            // Keep the Arc alive until the task is dropped/aborted.
            let _inner = inner;
            epics_base_rs::runtime::task::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        assert_eq!(StdArc::strong_count(&held), 2, "task holds the resource");
        let was_active = op.cancel().await;
        assert!(was_active);
        // After cancel() returns, the task future (and its captured Arc)
        // is gone — strong count is back to 1.
        assert_eq!(
            StdArc::strong_count(&held),
            1,
            "cancel() must block until the operation future has been dropped"
        );
    }

    /// The drop-order invariant `cancel()` rests on, asserted directly and
    /// backend-independently: **the operation future must be dropped while the
    /// termination channel is still open**, so the channel closing is proof
    /// the future is already gone.
    ///
    /// Both generator states are covered, because they used to disagree. A
    /// bare `async move { let _guard = guard; fut.await }` gets this right
    /// only once it has been polled — un-started, its upvars drop in capture
    /// order and the guard goes first. That un-started case is the one the
    /// exec-model backend hits when `abort()` is observed before the first
    /// poll, and it is what made
    /// `cancel_is_a_sync_point_resource_released` fail intermittently.
    #[test]
    fn guard_closes_only_after_the_operation_future_is_dropped() {
        use std::sync::atomic::{AtomicBool, Ordering};

        /// Reports, from inside the operation future's own drop, whether the
        /// termination channel was still open at that instant.
        struct Witness(watch::Receiver<()>, Arc<AtomicBool>);
        impl Drop for Witness {
            fn drop(&mut self) {
                self.1.store(self.0.has_changed().is_ok(), Ordering::SeqCst);
            }
        }

        // Build exactly what `spawn` builds, and drop it in a given state.
        fn probe(poll_first: bool) -> (bool, bool) {
            let (term_tx, terminated_rx) = watch::channel(());
            let open_at_future_drop = Arc::new(AtomicBool::new(false));
            let witness = Witness(terminated_rx.clone(), open_at_future_drop.clone());
            let mut body = Terminating {
                fut: Box::pin(async move {
                    let _witness = witness;
                    std::future::pending::<()>().await
                }),
                _guard: term_tx,
            };
            if poll_first {
                let waker = futures_util::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                let polled = std::pin::Pin::new(&mut body).poll(&mut cx);
                assert!(polled.is_pending(), "the probe future never completes");
            }
            drop(body);
            (
                open_at_future_drop.load(Ordering::SeqCst),
                terminated_rx.has_changed().is_err(),
            )
        }

        // Un-started — the state the exec backend drops an aborted-before-
        // first-poll task in.
        let (open_at_drop, closed_after) = probe(false);
        assert!(
            open_at_drop,
            "un-started: the operation future must be dropped while the \
             termination channel is still open"
        );
        assert!(
            closed_after,
            "un-started: the guard must close the channel once the future is gone"
        );

        // Polled once — the state the hosted backend almost always drops in.
        let (open_at_drop, closed_after) = probe(true);
        assert!(
            open_at_drop,
            "polled: the operation future must be dropped while the \
             termination channel is still open"
        );
        assert!(
            closed_after,
            "polled: the guard must close the channel once the future is gone"
        );
    }

    /// Dropping an in-flight handle aborts the spawned task (pvxs RAII
    /// `~Operation`). The captured resource is released after the task is
    /// scheduled off — assert the abort actually happens.
    #[epics_macros_rs::epics_test]
    async fn drop_aborts_in_flight_op() {
        use std::sync::Arc as StdArc;
        let held = StdArc::new(());
        let inner = held.clone();
        let op = PvaOperation::<()>::spawn(async move {
            let _inner = inner;
            epics_base_rs::runtime::task::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        assert_eq!(StdArc::strong_count(&held), 2);
        drop(op);
        // Give the runtime a moment to process the abort + drop the future.
        for _ in 0..100 {
            if StdArc::strong_count(&held) == 1 {
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            StdArc::strong_count(&held),
            1,
            "dropping the handle must abort the task and release its resources"
        );
    }

    /// Regression: a `wait` that times out while the op is
    /// still in-progress must leave the result recoverable by a later
    /// `wait` (pvxs `Operation::wait` is retriable after a timeout).
    #[epics_macros_rs::epics_test]
    async fn timeout_then_wait_again_recovers_result() {
        let mut op = PvaOperation::<i32>::spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(120)).await;
            Ok(99)
        });
        // First wait deadline expires before the op completes.
        let r1 = op.wait(Some(Duration::from_millis(30))).await;
        assert!(matches!(r1, Err(PvaError::Timeout)), "first wait times out");
        // Second wait (no deadline) collects the eventual result.
        let v = op.wait(None).await.unwrap();
        assert_eq!(v, 99, "result survives the earlier timeout");
    }

    /// Repeated short timeouts (polling pattern) must not consume the
    /// result; only actual completion does.
    #[epics_macros_rs::epics_test]
    async fn repeated_timeouts_do_not_consume() {
        let mut op = PvaOperation::<i32>::spawn(async {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;
            Ok(5)
        });
        for _ in 0..3 {
            assert!(matches!(
                op.wait(Some(Duration::from_millis(20))).await,
                Err(PvaError::Timeout)
            ));
        }
        let v = op.wait(Some(Duration::from_secs(1))).await.unwrap();
        assert_eq!(v, 5);
    }

    /// `cancel()` used as idempotent cleanup after an operation has already
    /// completed (but before its result is consumed) must report not-active
    /// and must NOT poison the buffered `Ok` result for a later `wait()`.
    #[epics_macros_rs::epics_test]
    async fn cancel_after_complete_preserves_ok_result() {
        let mut op = PvaOperation::spawn(async { Ok::<i32, _>(42) });
        // Let the task fully terminate without consuming the result: once
        // it is finished the result has been sent and is buffered.
        while !op.is_done() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
        }
        // Completed → cancel reports not-active and is a no-op for the result.
        assert!(
            !op.cancel().await,
            "cancel of a completed op must report not-active"
        );
        assert_eq!(
            op.wait(Some(Duration::from_secs(1))).await.unwrap(),
            42,
            "cancel after completion must not replace the buffered Ok result"
        );
    }

    /// The same precedence holds for an operation that completed with an
    /// `Err`: the real error survives a post-completion `cancel()`.
    #[epics_macros_rs::epics_test]
    async fn cancel_after_complete_preserves_err_result() {
        let mut op = PvaOperation::<i32>::spawn(async { Err(PvaError::Protocol("boom".into())) });
        while !op.is_done() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
        }
        assert!(!op.cancel().await);
        let r = op.wait(Some(Duration::from_secs(1))).await;
        assert!(
            matches!(r, Err(PvaError::Protocol(ref m)) if m.contains("boom")),
            "cancel after completion must surface the real error, not `Operation cancelled`, got {r:?}"
        );
    }

    /// After one successful result read the single-consumer policy
    /// holds: a second `wait` reports the result already consumed.
    #[epics_macros_rs::epics_test]
    async fn second_wait_after_success_is_already_consumed() {
        let mut op = PvaOperation::spawn(async { Ok::<i32, _>(1) });
        assert_eq!(op.wait(Some(Duration::from_secs(1))).await.unwrap(), 1);
        let r2 = op.wait(Some(Duration::from_secs(1))).await;
        assert!(
            matches!(r2, Err(PvaError::Protocol(ref m)) if m.contains("already consumed")),
            "second wait after success must report already-consumed, got {r2:?}"
        );
    }
}
