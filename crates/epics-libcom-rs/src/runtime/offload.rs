//! Blocking work that must not occupy a callback band.
//!
//! A file read or a name lookup awaited from a task needs a thread of its own
//! that any executor can wait on, not a callback band's worker, which every
//! other future on that band shares. [`OffloadPool`] is that thread, kept per
//! role; [`BLOCKING`] is the one behind `runtime::task::spawn_blocking` on
//! `exec_backend`.
//!
//! A thread per job is a creation per job, and every creation leaks its TLS
//! key on RTEMS, so the workers are permanent and reused.
//! [`DialPool`](super::blocking_io::DialPool) is one of these per dialing role.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::runtime::task::{InheritedRuntime, StackSizeClass, ThreadPriority, spawn_pool_worker};

/// How many threads one [`OffloadPool::new`] pool may ever create. Past it,
/// jobs queue.
pub const MAX_OFFLOAD_WORKERS: usize = 4;

/// A job runs `release` once its work is done and before it replies, so the
/// caller the reply wakes sees this worker as available (see
/// `OffloadQueue::busy`).
pub(crate) type OffloadJob = Box<dyn FnOnce(&mut dyn FnMut()) + Send>;

/// The process-global pool behind `runtime::task::spawn_blocking` on
/// `exec_backend`. Low, as C runs its blocking file and name work
/// (save_restore, `ipAddrToAsciiEngine`) at `epicsThreadPriorityLow`.
pub static BLOCKING: OffloadPool = OffloadPool::new("blocking", ThreadPriority::Low);

/// `workers - busy` are available, and a job is covered iff the available
/// ones outnumber the queue.
struct OffloadQueue {
    /// Each job with the runtime of the thread that submitted it: a worker
    /// outlives the runtime that created it, so it must never lend that one.
    pending: VecDeque<(InheritedRuntime, OffloadJob)>,
    /// Workers holding a job. Counting the busy ones rather than the parked
    /// ones is load-bearing: a worker between its job and its park is
    /// neither, and counting parked workers would make it read as unavailable,
    /// so a caller woken by that very worker's reply would create a second one.
    busy: usize,
    /// Workers created. A worker never exits.
    workers: usize,
}

/// A bounded, permanent set of threads for one role's reactor-free blocking
/// work. A job may block for as long as it likes; it holds only its own worker.
pub struct OffloadPool {
    /// OS thread-name stem; workers are `"{name_prefix} {index}"`.
    name_prefix: &'static str,
    priority: ThreadPriority,
    /// Threads this pool may ever create; past it, jobs queue.
    max_workers: usize,
    queue: Mutex<OffloadQueue>,
    work: Condvar,
}

impl OffloadPool {
    pub const fn new(name_prefix: &'static str, priority: ThreadPriority) -> Self {
        Self::with_max_workers(name_prefix, priority, MAX_OFFLOAD_WORKERS)
    }

    /// [`new`](Self::new) with a bound other than [`MAX_OFFLOAD_WORKERS`]. One
    /// worker serialises the role's jobs, as C's single-thread engines do.
    pub const fn with_max_workers(
        name_prefix: &'static str,
        priority: ThreadPriority,
        max_workers: usize,
    ) -> Self {
        assert!(max_workers > 0, "a pool with no workers never runs a job");
        Self {
            name_prefix,
            priority,
            max_workers,
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

    /// Jobs waiting for a worker, and workers inside a job.
    pub fn queue_depth(&self) -> (usize, usize) {
        let q = self.lock();
        (q.pending.len(), q.busy)
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

        self.submit(job)?;
        Ok(Offloaded(rx))
    }

    /// Run `f` on one of this pool's workers with nobody awaiting it — for
    /// work whose result lands somewhere shared, such as a cache. A panic in
    /// `f` ends `f` only. The error is a thread-creation failure.
    pub fn spawn<F>(&'static self, f: F) -> io::Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        self.submit(Box::new(move |release| {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            release();
        }))
    }

    /// Queue `job`, creating a worker if none is free. The error is a
    /// thread-creation failure; `job` is dropped unrun.
    ///
    /// The spawn happens under the queue lock, so no other submitter can count
    /// a worker that then fails to exist and queue behind it forever.
    pub(crate) fn submit(&'static self, job: OffloadJob) -> io::Result<()> {
        let mut q = self.lock();
        if q.pending.len() + q.busy >= q.workers && q.workers < self.max_workers {
            spawn_pool_worker(
                format!("{} {}", self.name_prefix, q.workers),
                self.priority,
                StackSizeClass::Small,
                move || self.worker_loop(),
            )?;
            q.workers += 1;
        }
        q.pending.push_back((InheritedRuntime::capture(), job));
        drop(q);
        self.work.notify_one();
        Ok(())
    }

    fn worker_loop(&self) -> ! {
        loop {
            let (ambient, job) = {
                let mut q = self.lock();
                loop {
                    if let Some(job) = q.pending.pop_front() {
                        q.busy += 1;
                        break job;
                    }
                    // Every worker re-reads `pending` under this lock before
                    // parking, so a job queued meanwhile is not missed.
                    q = self.work.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            // The job catches its own panic, so the worker outlives it.
            ambient.run(|| job(&mut || self.lock().busy -= 1));
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

    /// Jobs blocked at once each get a worker, up to the bound: a second job
    /// does not wait for the first to finish.
    #[test]
    fn concurrent_blocked_jobs_each_get_a_worker() {
        static POOL: OffloadPool = OffloadPool::new("test-offlc", ThreadPriority::Low);
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Arc::new(Mutex::new(held));
        let jobs: Vec<_> = (0..MAX_OFFLOAD_WORKERS)
            .map(|_| {
                let (started_tx, held) = (started_tx.clone(), held.clone());
                POOL.run(move || {
                    started_tx.send(()).unwrap();
                    // One holder at a time, but each job has already started.
                    let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(10));
                })
                .unwrap()
            })
            .collect();
        for i in 0..MAX_OFFLOAD_WORKERS {
            started
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|_| panic!("job {i} queued behind a blocked one"));
        }
        assert_eq!(POOL.worker_count(), MAX_OFFLOAD_WORKERS);
        assert_eq!(POOL.queue_depth(), (0, MAX_OFFLOAD_WORKERS));
        for _ in 0..MAX_OFFLOAD_WORKERS {
            release.send(()).unwrap();
        }
        for job in jobs {
            crate::runtime::task::park_on_interruptible(job, || false).unwrap();
        }
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

/// Two tokio runtimes in one process, so no `exec_backend` twin.
#[cfg(all(test, tokio_backend))]
mod runtime_tests {
    use super::*;
    use std::time::Duration;

    /// A worker created under one runtime runs a later job under the runtime
    /// that job came from, not the dead one that created the worker.
    #[test]
    fn a_job_runs_under_its_submitters_runtime() {
        static POOL: OffloadPool =
            OffloadPool::with_max_workers("test-offlr", ThreadPriority::Low, 1);
        for round in 0..2 {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (ran_tx, ran) = std::sync::mpsc::channel();
            rt.block_on(async {
                POOL.run(move || {
                    tokio::spawn(async move { ran_tx.send(()).unwrap() });
                })
                .unwrap()
                .await
            });
            ran.recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|_| panic!("round {round}: the job's task never ran"));
            drop(rt);
        }
        assert_eq!(
            POOL.worker_count(),
            1,
            "the second runtime got a fresh worker"
        );
        let job = POOL.run(|| tokio::runtime::Handle::try_current().is_err());
        assert!(
            crate::runtime::task::park_on_interruptible(job.unwrap(), || false).unwrap(),
            "a job from no runtime ran inside the worker's first one"
        );
    }
}
