//! Blocking work that must not occupy a callback band.
//!
//! `runtime::task::spawn_blocking` is the callback pool on `exec_backend` and
//! `tokio::task::spawn_blocking` on a hosted build, so the same call either
//! parks a band worker for as long as the work blocks or panics when awaited
//! from a band future, which has no tokio runtime. A file read or a name lookup
//! awaited from a server task needs neither: it needs a thread of its own that
//! any executor can wait on. [`OffloadPool`] is that thread, kept per role.
//!
//! The shape is [`DialPool`](super::blocking_io::DialPool)'s, for the same
//! reason: a thread per job is a creation per job, and every creation leaks
//! its TLS key on RTEMS, so the workers are permanent and reused.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::runtime::task::{StackSizeClass, ThreadPriority, spawn_dedicated_thread};

/// How many threads one [`OffloadPool`] may ever create. Past it, jobs queue.
pub const MAX_OFFLOAD_WORKERS: usize = 4;

/// A job runs `release` once its work is done and before it replies, so the
/// caller the reply wakes sees this worker as available (see
/// `DialQueue::busy`).
type OffloadJob = Box<dyn FnOnce(&mut dyn FnMut()) + Send>;

struct OffloadQueue {
    pending: VecDeque<OffloadJob>,
    busy: usize,
    workers: usize,
}

/// A bounded, permanent set of threads for one role's reactor-free blocking
/// work. A job may block for as long as it likes; it holds only its own worker.
pub struct OffloadPool {
    /// OS thread-name stem; workers are `"{name_prefix} {index}"`.
    name_prefix: &'static str,
    priority: ThreadPriority,
    queue: Mutex<OffloadQueue>,
    work: Condvar,
}

impl OffloadPool {
    pub const fn new(name_prefix: &'static str, priority: ThreadPriority) -> Self {
        Self {
            name_prefix,
            priority,
            queue: Mutex::new(OffloadQueue {
                pending: VecDeque::new(),
                busy: 0,
                workers: 0,
            }),
            work: Condvar::new(),
        }
    }

    /// Threads this pool has created.
    pub fn worker_count(&self) -> usize {
        self.lock().workers
    }

    /// Run `f` on one of this pool's workers. The error is a thread-creation
    /// failure, returned before anything is queued.
    pub fn run<F, R>(&'static self, f: F) -> io::Result<Offloaded<R>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (reply, rx) = oneshot::channel();
        let job: OffloadJob = Box::new(move |release| {
            // A caller that gave up while this sat in the queue gets no work.
            if reply.is_closed() {
                return release();
            }
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            release();
            let _ = reply.send(out);
        });

        let mut q = self.lock();
        if q.pending.len() + q.busy < q.workers || q.workers >= MAX_OFFLOAD_WORKERS {
            q.pending.push_back(job);
            drop(q);
            self.work.notify_one();
            return Ok(Offloaded(rx));
        }
        let index = q.workers;
        q.workers += 1;
        drop(q);
        if let Err(e) = spawn_dedicated_thread(
            format!("{} {index}", self.name_prefix),
            self.priority,
            StackSizeClass::Small,
            move || self.worker_loop(),
        ) {
            self.lock().workers -= 1;
            return Err(e);
        }
        self.lock().pending.push_back(job);
        self.work.notify_one();
        Ok(Offloaded(rx))
    }

    fn worker_loop(&self) -> ! {
        loop {
            let job = {
                let mut q = self.lock();
                loop {
                    if let Some(job) = q.pending.pop_front() {
                        q.busy += 1;
                        break job;
                    }
                    q = self.work.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            // The job catches its own panic, so the worker outlives it.
            job(&mut || self.lock().busy -= 1);
        }
    }

    fn lock(&self) -> MutexGuard<'_, OffloadQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The result of [`OffloadPool::run`]. A panic in the job resumes in the
/// awaiting task, as `std::thread::JoinHandle::join` would report it.
pub struct Offloaded<R>(oneshot::Receiver<std::thread::Result<R>>);

impl<R> Future for Offloaded<R> {
    type Output = R;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(Ok(Ok(value))) => Poll::Ready(value),
            Poll::Ready(Ok(Err(panic))) => std::panic::resume_unwind(panic),
            Poll::Ready(Err(_)) => unreachable!("an offload job always replies"),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::task::{CallbackPriority, spawn_background, timeout};
    use std::time::Duration;

    /// A job blocked on its worker leaves the band that awaits it free: a
    /// second cbMedium future runs while the first is still waiting.
    #[epics_macros_rs::epics_test]
    async fn a_blocked_job_leaves_its_awaiting_band_free() {
        static POOL: OffloadPool = OffloadPool::new("test-offld", ThreadPriority::Low);
        let (release, held) = std::sync::mpsc::channel::<()>();
        let waiting = spawn_background(CallbackPriority::Medium, async move {
            POOL.run(move || held.recv_timeout(Duration::from_secs(5)).is_ok())
                .unwrap()
                .await
        });
        let beside = spawn_background(CallbackPriority::Medium, async { 7 });
        let ran = timeout(Duration::from_secs(3), beside).await;
        release.send(()).unwrap();
        assert_eq!(
            ran.expect("cbMedium was parked by an offloaded job")
                .unwrap(),
            7
        );
        assert!(waiting.await.unwrap(), "the job outlived its release");
    }

    /// Sequential jobs borrow one worker rather than creating one each.
    #[epics_macros_rs::epics_test]
    async fn sequential_jobs_reuse_one_worker() {
        static POOL: OffloadPool = OffloadPool::new("test-offld", ThreadPriority::Low);
        for i in 0..8 {
            assert_eq!(POOL.run(move || i).unwrap().await, i);
            assert_eq!(POOL.worker_count(), 1, "job {i} created a thread");
        }
    }
}
