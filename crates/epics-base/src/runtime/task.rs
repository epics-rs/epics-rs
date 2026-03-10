use std::future::Future;
use std::time::Duration;
use tokio::task::JoinHandle;

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
}

pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

pub async fn sleep_until(deadline: std::time::Instant) {
    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
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
}
