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
        let result = tokio::spawn(fut).await;

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
