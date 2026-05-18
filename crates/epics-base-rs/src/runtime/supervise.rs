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
    /// Maximum restart attempts within `window`.
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

    loop {
        attempt += 1;
        tracing::info!(attempt, "supervise: starting attempt");
        let result = task_factory().await;

        let last_error = match result {
            Ok(()) => {
                tracing::info!(attempt, "supervise: task exited normally");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(attempt, error = ?e, "supervise: task failed");
                e
            }
        };

        if let Err((max, win)) = tracker.try_record(&policy) {
            tracing::error!(max, window_secs = win, "supervise: too many restarts");
            // Carry the final inner error so the caller sees the root
            // cause of the abandoned supervision, not just the cap.
            return Err(SuperviseError::TooManyRestarts {
                max_restarts: max,
                window_secs: win,
                last_error,
            });
        }

        tracing::info!(
            attempt,
            delay_ms = policy.delay.as_millis() as u64,
            "supervise: scheduling restart"
        );
        tokio::time::sleep(policy.delay).await;
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
}
