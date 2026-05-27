//! Explicit-handle wrappers around the convenience PvaClient operations.
//!
//! Rust's async/await covers most of pvxs `client::Operation`'s job
//! implicitly (drop a future to cancel, `.await` to wait), but a
//! handle-style API is occasionally useful when:
//!
//! - You want to start an operation now and `.wait(timeout)` for it
//!   later from a different task.
//! - You want a single thread-safe `cancel()` that unblocks the waiter
//!   from elsewhere (pvxs `Operation::cancel`).
//! - You want a thread-safe `interrupt()` that wakes a `wait()`
//!   without cancelling the underlying operation, mirroring pvxs
//!   `Operation::interrupt`.
//!
//! The handle is constructed from any future that returns
//! `PvaResult<T>`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

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
    /// Spawned task running the underlying op.
    join: JoinHandle<()>,
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
}

impl<T: Send + 'static> PvaOperation<T> {
    /// Spawn `fut` and return a handle. The future runs to completion
    /// regardless of handle drops unless [`Self::cancel`] is called
    /// explicitly. (Drop only loses the handle's view of the result;
    /// the spawned task continues. To make drop also cancel, call
    /// `cancel()` first.)
    pub fn spawn<F>(fut: F) -> Self
    where
        F: std::future::Future<Output = PvaResult<T>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let join = tokio::spawn(async move {
            let v = fut.await;
            let _ = tx.send(v);
        });
        Self {
            join,
            result_rx: rx,
            done: false,
            interrupt: Arc::new(Notify::new()),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
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
            Some(d) => match tokio::time::timeout(d, body).await {
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

    /// Cancel the operation. Safe to call from any task; idempotent.
    /// Mirrors pvxs `Operation::cancel`. Aborts the spawned task and
    /// causes any pending [`Self::wait`] to return `PvaError::Protocol("Operation cancelled")`.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.join.abort();
    }

    /// Wake a pending [`Self::wait`] without cancelling the operation
    /// — the wait returns `PvaError::Interrupted` and the
    /// underlying op keeps running. Mirrors pvxs `Operation::interrupt`.
    pub fn interrupt(&self) {
        self.interrupt.notify_waiters();
    }

    /// True iff the spawned task has finished.
    pub fn is_done(&self) -> bool {
        self.join.is_finished()
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_returns_value() {
        let mut op = PvaOperation::spawn(async { Ok::<i32, _>(42) });
        let v = op.wait(Some(Duration::from_secs(1))).await.unwrap();
        assert_eq!(v, 42);
        assert!(op.is_done());
    }

    #[tokio::test]
    async fn wait_times_out() {
        let mut op = PvaOperation::<()>::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        let r = op.wait(Some(Duration::from_millis(50))).await;
        assert!(matches!(r, Err(PvaError::Timeout)));
    }

    #[tokio::test]
    async fn interrupt_wakes_waiter_op_continues() {
        let mut op = PvaOperation::<i32>::spawn(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(7)
        });
        let interrupter = op.interrupt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
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
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        let deadline = slow.wait(Some(Duration::from_millis(30))).await;
        assert!(matches!(deadline, Err(PvaError::Timeout)));
        assert!(!matches!(deadline, Err(PvaError::Interrupted)));

        // Interrupt path: woken before completion.
        let mut op = PvaOperation::<i32>::spawn(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(1)
        });
        let interrupter = op.interrupt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            interrupter.notify_waiters();
        });
        let interrupted = op.wait(Some(Duration::from_secs(5))).await;
        assert!(matches!(interrupted, Err(PvaError::Interrupted)));
        assert!(!matches!(interrupted, Err(PvaError::Timeout)));
    }

    #[tokio::test]
    async fn cancel_aborts_op() {
        let mut op = PvaOperation::<i32>::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(0)
        });
        op.cancel();
        let r = op.wait(Some(Duration::from_secs(1))).await;
        assert!(matches!(r, Err(PvaError::Protocol(_))));
    }

    /// Regression: a `wait` that times out while the op is
    /// still in-progress must leave the result recoverable by a later
    /// `wait` (pvxs `Operation::wait` is retriable after a timeout).
    #[tokio::test]
    async fn timeout_then_wait_again_recovers_result() {
        let mut op = PvaOperation::<i32>::spawn(async {
            tokio::time::sleep(Duration::from_millis(120)).await;
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
    #[tokio::test]
    async fn repeated_timeouts_do_not_consume() {
        let mut op = PvaOperation::<i32>::spawn(async {
            tokio::time::sleep(Duration::from_millis(150)).await;
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

    /// After one successful result read the single-consumer policy
    /// holds: a second `wait` reports the result already consumed.
    #[tokio::test]
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
