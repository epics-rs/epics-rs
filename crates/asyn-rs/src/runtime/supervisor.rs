// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — all 2 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1081/1081). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::time::Instant;

use super::config::SupervisionPolicy;

/// Outcome of supervision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisionOutcome {
    /// Normal shutdown (actor returned without error).
    Normal,
    /// Max restarts exceeded within window.
    MaxRestartsExceeded { count: usize },
}

/// Generic supervision loop.
///
/// Calls `factory` to create a future, runs it, and restarts on panic/error
/// according to `policy`. Returns when the actor completes normally or
/// max restarts are exceeded.
pub async fn supervise<F, Fut>(
    name: &str,
    policy: SupervisionPolicy,
    factory: F,
) -> SupervisionOutcome
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut restart_times: Vec<Instant> = Vec::new();

    loop {
        let fut = factory();
        // Spawned rather than awaited in place so a panic in the supervised
        // future arrives as a join error instead of unwinding the supervisor.
        // The seam's task handle carries the same `Result<_, JoinError>` on
        // both backends, so the restart policy below reads identically.
        let result = crate::runtime::task::spawn(fut).await;

        match result {
            Ok(()) => {
                // Normal completion
                return SupervisionOutcome::Normal;
            }
            Err(e) => {
                // Task panicked or was cancelled
                tracing::error!("runtime {name} failed: {e}, restarting...");

                let now = Instant::now();
                // Purge old restart times outside the window
                restart_times.retain(|t| now.duration_since(*t) < policy.restart_window);
                restart_times.push(now);

                if restart_times.len() > policy.max_restarts {
                    tracing::error!(
                        "runtime {name} exceeded max restarts ({} in {:?})",
                        policy.max_restarts,
                        policy.restart_window
                    );
                    return SupervisionOutcome::MaxRestartsExceeded {
                        count: restart_times.len(),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn normal_completion() {
        let outcome = supervise("test", SupervisionPolicy::default(), || async {}).await;
        assert_eq!(outcome, SupervisionOutcome::Normal);
    }

    #[tokio::test]
    async fn restart_on_panic() {
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        let outcome = supervise(
            "panicker",
            SupervisionPolicy {
                max_restarts: 2,
                restart_window: Duration::from_secs(10),
            },
            move || {
                let c = count2.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n < 3 {
                        panic!("intentional panic #{n}");
                    }
                    // After 3 panics, complete normally
                }
            },
        )
        .await;

        // Should exceed max_restarts (2) because we panic 3 times
        assert_eq!(
            outcome,
            SupervisionOutcome::MaxRestartsExceeded { count: 3 }
        );
    }
}
