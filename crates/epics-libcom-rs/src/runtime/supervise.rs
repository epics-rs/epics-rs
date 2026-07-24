//! Auto-restart supervisor — sliding-window NRESTARTS pattern.
//!
//! Shared by every component that needs "relaunch this thing if it
//! exits, but give up after too many restarts in too short a time":
//!
//! - `epics-bridge-rs::ca_gateway::master` — wraps the gateway daemon
//! - `epics-tools-rs::procserv` — wraps the supervised child process
//!
//! Mirrors the C ca-gateway master semantics (NRESTARTS=10,
//! RESTART_INTERVAL=600s, RESTART_DELAY=10s — `gateway.cc:22-24`)
//! and the C procServ `holdoffTime` floor.
//!
//! ## Other restart shapes
//!
//! Note that some workspace components use different restart shapes
//! (exponential-backoff retry instead of sliding-window — e.g.
//! `epics-pva-rs` upstream-monitor restart, `epics-ca-rs` name-server
//! reconnect). Those are NOT subsumed by this module — exponential
//! backoff is a different policy with different semantics. This
//! module is exclusively for the sliding-window pattern.

use std::time::{Duration, Instant};

/// Policy: at most `max_restarts` attempts inside `window`,
/// pausing `delay` between consecutive restarts.
#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    /// Maximum number of launch attempts within `window`, counted at the
    /// launch boundary. C ca-gateway's master records each child's start
    /// time and refuses the next fork once `NRESTARTS` starts already sit
    /// inside `RESTART_INTERVAL` (`gateway.cc:1506-1539`), so at most this
    /// many launches occur per window and the next is refused. The initial
    /// launch consumes the first slot — this is the total launch count,
    /// not "restarts after the first".
    pub max_restarts: u32,
    /// Sliding window over which `max_restarts` is counted.
    pub window: Duration,
    /// Delay between restart attempts. Doubles as a "min holdoff
    /// between consecutive child launches" floor.
    pub delay: Duration,
}

impl Default for RestartPolicy {
    /// Defaults match C ca-gateway: 10 restarts in 600s, 10s delay.
    fn default() -> Self {
        Self {
            max_restarts: 10,
            window: Duration::from_secs(600),
            delay: Duration::from_secs(10),
        }
    }
}

/// In-memory bookkeeping for the sliding window. Construct fresh per
/// supervised target (gateway, procserv child, etc.).
#[derive(Debug, Default)]
pub struct RestartTracker {
    timestamps: Vec<Instant>,
}

impl RestartTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `Ok(())` if a fresh restart fits inside `policy`, in
    /// which case the current timestamp is appended to the window.
    /// Returns `Err((max, window_secs))` if the limit was hit.
    pub fn try_record(&mut self, policy: &RestartPolicy) -> Result<(), (u32, u64)> {
        let now = Instant::now();
        // Drop entries outside the window.
        self.timestamps
            .retain(|t| now.duration_since(*t) < policy.window);
        if self.timestamps.len() as u32 >= policy.max_restarts {
            return Err((policy.max_restarts, policy.window.as_secs()));
        }
        self.timestamps.push(now);
        Ok(())
    }

    /// Most recent restart timestamp, if any.
    pub fn last(&self) -> Option<Instant> {
        self.timestamps.last().copied()
    }

    /// Reset the window — used by callers that explicitly want to
    /// "forget" past failures (e.g. operator re-enabled auto-restart).
    pub fn reset(&mut self) {
        self.timestamps.clear();
    }
}

/// Error returned by [`supervise`] when the policy refuses another
/// restart.
#[derive(Debug)]
pub enum SuperviseError<E> {
    /// Restart policy hit `max_restarts` inside `window`. Carries the
    /// final inner error that triggered the abandoned restart, so the
    /// caller does not lose the root cause — the C ca-gateway master
    /// loop likewise reports the last child-exit reason when it gives
    /// up (`gateway.cc` restart loop).
    TooManyRestarts {
        /// `max_restarts` from the policy that was exceeded.
        max_restarts: u32,
        /// `window` (seconds) the count was measured over.
        window_secs: u64,
        /// The inner-task error from the final failed attempt.
        last_error: E,
    },
}

impl<E: std::fmt::Display> std::fmt::Display for SuperviseError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyRestarts {
                max_restarts,
                window_secs,
                last_error,
            } => write!(
                f,
                "supervisor: too many restarts ({max_restarts} in {window_secs}s); \
                 last error: {last_error}"
            ),
        }
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for SuperviseError<E> {}

/// Supervise an async task with auto-restart. Returns `Ok` the
/// first time the task ever returns `Ok`. Returns
/// `Err(SuperviseError::TooManyRestarts)` once the policy refuses
/// another attempt.
///
/// The restart-window admission check runs at the **launch boundary** —
/// before each launch, not after a launch has failed — so a fast
/// crash-loop performs at most `policy.max_restarts` launches per window,
/// matching C ca-gateway's master, which checks `RESTART_INTERVAL` before
/// forking the next child (`gateway.cc:1506-1539`).
///
/// ```ignore
/// use epics_base_rs::runtime::supervise::{supervise, RestartPolicy};
///
/// supervise(RestartPolicy::default(), || async {
///     run_my_task().await
/// }).await
/// ```
pub async fn supervise<F, Fut, E>(
    policy: RestartPolicy,
    mut task_factory: F,
) -> Result<(), SuperviseError<E>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Debug,
{
    let mut tracker = RestartTracker::new();
    let mut attempt = 0u32;
    let mut last_error: Option<E> = None;

    loop {
        // Admission at the LAUNCH boundary. C ca-gateway's master records
        // each child's start time and, once the window already holds
        // `NRESTARTS` starts, prints "too many [N+1] restarts" and exits
        // *before* forking the next child (`gateway.cc:1506-1539`).
        // `try_record` admits iff fewer than `max_restarts` starts sit
        // inside `window`, recording this start on success — so the cap
        // counts launches and a fast crash-loop performs at most
        // `max_restarts` launches per window. (The pre-fix loop recorded
        // on failure *after* the launch, comparing the cap against
        // completed failures, which let one extra launch through.)
        if let Err((max, win)) = tracker.try_record(&policy) {
            match last_error {
                Some(last_error) => {
                    tracing::error!(max, window_secs = win, "supervise: too many restarts");
                    // Carry the final inner error so the caller sees the
                    // root cause of the abandoned supervision, not the cap.
                    return Err(SuperviseError::TooManyRestarts {
                        max_restarts: max,
                        window_secs: win,
                        last_error,
                    });
                }
                // `max_restarts == 0` refuses even the first start (no C
                // analog — C's NRESTARTS is a fixed `10`). The supervisor's
                // contract is to run the task at least once, so launch this
                // one time rather than return a TooManyRestarts with no
                // root-cause error to report.
                None => tracing::warn!(
                    max,
                    "supervise: max_restarts=0 is degenerate; running the task once"
                ),
            }
        }

        attempt += 1;
        if attempt > 1 {
            // Delay between consecutive launches (never before the first),
            // applied after passing the admission gate — C sleeps
            // RESTART_DELAY right before each refork (`gateway.cc:1532-1539`).
            tracing::info!(
                attempt,
                delay_ms = policy.delay.as_millis() as u64,
                "supervise: scheduling restart"
            );
            crate::runtime::task::sleep(policy.delay).await;
        }

        tracing::info!(attempt, "supervise: starting attempt");
        match task_factory().await {
            Ok(()) => {
                tracing::info!(attempt, "supervise: task exited normally");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(attempt, error = ?e, "supervise: task failed");
                last_error = Some(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn rate_limit_bails_after_max() {
        let policy = RestartPolicy {
            max_restarts: 3,
            window: Duration::from_secs(60),
            delay: Duration::ZERO,
        };
        let mut t = RestartTracker::new();
        assert!(t.try_record(&policy).is_ok());
        assert!(t.try_record(&policy).is_ok());
        assert!(t.try_record(&policy).is_ok());
        assert!(t.try_record(&policy).is_err());
    }

    #[test]
    fn reset_clears_window() {
        let policy = RestartPolicy {
            max_restarts: 2,
            window: Duration::from_secs(60),
            delay: Duration::ZERO,
        };
        let mut t = RestartTracker::new();
        t.try_record(&policy).unwrap();
        t.try_record(&policy).unwrap();
        assert!(t.try_record(&policy).is_err());
        t.reset();
        assert!(t.try_record(&policy).is_ok());
    }

    #[epics_macros_rs::epics_test]
    async fn supervise_immediate_success() {
        let policy = RestartPolicy {
            max_restarts: 3,
            window: Duration::from_secs(60),
            delay: Duration::from_millis(1),
        };
        let result: Result<(), SuperviseError<&str>> =
            supervise(policy, || async { Ok::<(), &str>(()) }).await;
        assert!(result.is_ok());
    }

    #[epics_macros_rs::epics_test]
    async fn supervise_eventual_success() {
        let count = Arc::new(AtomicU32::new(0));
        let policy = RestartPolicy {
            max_restarts: 5,
            window: Duration::from_secs(60),
            delay: Duration::from_millis(1),
        };
        let count_clone = count.clone();
        let result: Result<(), SuperviseError<&str>> = supervise(policy, || {
            let c = count_clone.clone();
            async move {
                let n = c.fetch_add(1, Ordering::Relaxed);
                if n < 2 {
                    Err::<(), &str>("not yet")
                } else {
                    Ok::<(), &str>(())
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(count.load(Ordering::Relaxed), 3);
    }

    #[epics_macros_rs::epics_test]
    async fn supervise_too_many_restarts() {
        let policy = RestartPolicy {
            max_restarts: 2,
            window: Duration::from_secs(60),
            delay: Duration::from_millis(1),
        };
        let result: Result<(), SuperviseError<&str>> =
            supervise(policy, || async { Err::<(), &str>("always fails") }).await;
        match result {
            Err(SuperviseError::TooManyRestarts {
                max_restarts,
                window_secs,
                last_error,
            }) => {
                assert_eq!(max_restarts, 2);
                assert_eq!(window_secs, 60);
                // L5: the final inner error is preserved, not discarded.
                assert_eq!(last_error, "always fails");
            }
            other => panic!("expected TooManyRestarts, got {other:?}"),
        }
    }

    /// C parity: a fast crash-loop performs at most `max_restarts`
    /// launches per window before the supervisor gives up — the admission
    /// check sits at the launch boundary, not after a failed launch. The
    /// pre-fix loop recorded on failure and admitted `max_restarts + 1`
    /// launches (one extra full gateway start vs C NRESTARTS).
    #[epics_macros_rs::epics_test]
    async fn supervise_launch_count_equals_max_restarts() {
        for max in [1u32, 2, 3, 10] {
            let launches = Arc::new(AtomicU32::new(0));
            let policy = RestartPolicy {
                max_restarts: max,
                window: Duration::from_secs(60),
                delay: Duration::ZERO,
            };
            let launches_clone = launches.clone();
            let result: Result<(), SuperviseError<&str>> = supervise(policy, || {
                let c = launches_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Err::<(), &str>("always fails")
                }
            })
            .await;
            assert!(
                matches!(result, Err(SuperviseError::TooManyRestarts { .. })),
                "max_restarts={max} must end in TooManyRestarts"
            );
            assert_eq!(
                launches.load(Ordering::Relaxed),
                max,
                "exactly max_restarts={max} launches must occur, not {} (the pre-fix off-by-one)",
                max + 1
            );
        }
    }

    /// `max_restarts == 0` has no C analog (NRESTARTS is a fixed 10). The
    /// supervisor's contract is to run the task at least once, so it must
    /// launch exactly once and then honor the cap — never panic on the
    /// missing root-cause error.
    #[epics_macros_rs::epics_test]
    async fn supervise_zero_max_runs_task_once() {
        let launches = Arc::new(AtomicU32::new(0));
        let policy = RestartPolicy {
            max_restarts: 0,
            window: Duration::from_secs(60),
            delay: Duration::ZERO,
        };
        let launches_clone = launches.clone();
        let result: Result<(), SuperviseError<&str>> = supervise(policy, || {
            let c = launches_clone.clone();
            async move {
                c.fetch_add(1, Ordering::Relaxed);
                Err::<(), &str>("boom")
            }
        })
        .await;
        match result {
            Err(SuperviseError::TooManyRestarts { last_error, .. }) => {
                assert_eq!(last_error, "boom");
            }
            other => panic!("expected TooManyRestarts, got {other:?}"),
        }
        assert_eq!(launches.load(Ordering::Relaxed), 1);
    }
}
