//! General-purpose callback executor pool — RTEMS-safe port of
//! `modules/database/src/ioc/db/callback.c`.
//!
//! # C parity
//!
//! C `callback.c` runs `NUM_CALLBACK_PRIORITIES == 3` (`callback.h:40`)
//! independent priority bands — `priorityLow`/`priorityMedium`/`priorityHigh`
//! = 0/1/2 (`callback.h:41-43`) — each with its own bounded ring buffer, its
//! own wake-up event, and `callbackThreadsDefault == 1` worker thread(s)
//! (`callback.c:66`, sized by `threadsConfigured`). `callbackRequest`
//! (`callback.c:341`) pushes an `epicsCallback` onto the band's ring and
//! signals the band's event; `callbackTask` (`callback.c:210`) waits on the
//! event, drains the ring, and invokes each callback.
//!
//! This module keeps that structure but with **plain `std` threads +
//! `Mutex`/`Condvar`** and boxed closures instead of C function pointers, so
//! it carries **no tokio-runtime dependency** and runs on RTEMS
//! (armv7-rtems-eabihf). The OS thread priority per band is applied
//! best-effort via the existing [`apply_to_current_thread`](crate::runtime::task::apply_to_current_thread) abstraction in
//! [`crate::runtime::task`] — this module does **not** duplicate that logic.
//!
//! ## Overflow hysteresis (`callback.c:365-374`, `:227`)
//!
//! C sets a per-band `queueOverflow` flag when a push finds the ring full; a
//! subsequent `callbackRequest` returns `S_db_bufFull` *immediately*
//! (`callback.c:365`) without even attempting a push, until a worker pops an
//! entry and clears the flag (`callback.c:227`). We reproduce that exact
//! latch: once `overflow` is set, `request` rejects until a worker drains one
//! entry.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::JoinHandle;

use super::facility::{recover, run_facility_loop, run_isolated};
use crate::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};

/// A unit of deferred work. The C `epicsCallback` is a function pointer plus
/// user data; the Rust port boxes a `FnOnce` closure that already captures its
/// context.
pub type Callback = Box<dyn FnOnce() + Send + 'static>;

/// Number of callback priority bands — C `NUM_CALLBACK_PRIORITIES`
/// (`callback.h:40`).
pub const NUM_CALLBACK_PRIORITIES: usize = 3;

/// Default per-band ring capacity — C `callbackQueueSize` (`callback.c:51`).
pub const DEFAULT_QUEUE_SIZE: usize = 2000;

/// Default worker threads per band — C `callbackThreadsDefault`
/// (`callback.c:66`).
pub const DEFAULT_THREADS_PER_PRIORITY: usize = 1;

/// The sizing [`CallbackPool::new`] will use — C's `callbackQueueSize`
/// and `callbackQueue[i].threadsConfigured` file-statics
/// (`callback.c:51`, `:60`), which `callbackSetQueueSize` and
/// `callbackParallelThreads` write before `callbackInit` reads them.
///
/// They are module state for the same reason C's are: the pool is built
/// once, from a `OnceLock` initialiser that takes no arguments, and the
/// iocsh commands that size it run long before anything touches it.
/// Writing one after the pool exists changes nothing, which is why both
/// commands refuse once the pool is up.
static CONFIGURED_QUEUE_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_QUEUE_SIZE);
static CONFIGURED_THREADS: [AtomicUsize; NUM_CALLBACK_PRIORITIES] = [
    AtomicUsize::new(DEFAULT_THREADS_PER_PRIORITY),
    AtomicUsize::new(DEFAULT_THREADS_PER_PRIORITY),
    AtomicUsize::new(DEFAULT_THREADS_PER_PRIORITY),
];

/// C `callbackSetQueueSize` (`callback.c:101-113`) minus its two
/// diagnostics: the caller owns those, because C prints them from the
/// same function only because C has nowhere else to put them.
///
/// A size of zero or less is the caller's error to report; this clamps
/// to at least 1 so the pool can never be built with an unusable ring.
pub fn set_queue_size(size: usize) {
    CONFIGURED_QUEUE_SIZE.store(size.max(1), Ordering::Relaxed);
}

/// C `callbackParallelThreads(count, prio)` (`callback.c:160-208`) for
/// one band, or for all three when `priority` is `None` — C's
/// `NULL`/`""`/`"*"` case. `count` is clamped to at least 1 exactly as
/// `callback.c:171` does.
pub fn set_parallel_threads(count: usize, priority: Option<CallbackPriority>) {
    let count = count.max(1);
    match priority {
        Some(p) => CONFIGURED_THREADS[p.index()].store(count, Ordering::Relaxed),
        None => {
            for slot in &CONFIGURED_THREADS {
                slot.store(count, Ordering::Relaxed);
            }
        }
    }
}

/// C `epicsThreadGetCPUs()` (`osdThread.c`), the live processor count.
///
/// **Post-pin forward-port: this tracks epics-base HEAD, not R7.0.10.** At
/// the pin (`osdThread.c:1123-1137`) the function is `sysconf` of
/// `_SC_NPROCESSORS_ONLN`, then `_SC_NPROCESSORS_CONF`, then a hardcoded 1
/// — none of which consults the calling thread's CPU affinity mask, so a C
/// IOC pinned to 2 of 64 processors still sizes its callback pool for 64.
/// `556de06ff` ("avoid overreporting available CPUs", 2026-02-06, branch
/// 7.0, in no tag) puts a `sched_getaffinity` + `CPU_COUNT` arm ahead of
/// both `sysconf` calls. `std::thread::available_parallelism` is that
/// behaviour, so this has been carrying the fix rather than the pin.
/// Deliberately kept — reverting onto a number upstream itself calls
/// overreporting buys no parity worth having — and named here because
/// until now it was silent.
///
/// One divergence beyond that forward-port, stated rather than folded into
/// it: `available_parallelism` also clamps to the cgroup CPU quota, which
/// `556de06ff` does not. In a container limited to 2 CPUs with no affinity
/// mask set this returns 2 where even post-`556de06ff` C returns the host
/// count. Same direction as the upstream fix, so it stays.
///
/// Distinct from [`parallel_threads_default`], which is a settable knob
/// merely SEEDED from this. `callbackParallelThreads` reads the two on
/// different arms — a negative count is relative to the processor count,
/// a zero count means the knob (`callback.c:167-170`) — so collapsing
/// them into one accessor makes `var callbackParallelThreadsDefault N`
/// silently move the negative arm too.
pub fn cpu_count() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as i32
}

/// C `callbackParallelThreadsDefault` (`callback.c:69`) — the value
/// `callbackParallelThreads(0, ...)` resolves to (`callback.c:170`).
///
/// C declares it `2` and then overwrites it with `epicsThreadGetCPUs()`
/// in `dbIocRegister` (`dbIocRegister.c:638-639`), commented there
/// "Needed before callback system is initialized". That assignment runs
/// during registration, so `2` is never a value an IOC can observe and
/// this seeds the processor count directly rather than reproducing a
/// registration phase that has no counterpart here.
///
/// It is an `iocshVar` (`dbCore.dbd:32`, `variable(...,int)`), so a
/// startup script may write it, and C reads it at the point of use, not
/// at init. `i32` because C's is an `int`: a negative value is writable
/// and reaches `callback.c:171`'s floor, which `usize` could not carry.
static PARALLEL_THREADS_DEFAULT: LazyLock<AtomicI32> =
    LazyLock::new(|| AtomicI32::new(cpu_count()));

/// Read C `callbackParallelThreadsDefault`.
pub fn parallel_threads_default() -> i32 {
    PARALLEL_THREADS_DEFAULT.load(Ordering::Relaxed)
}

/// Write C `callbackParallelThreadsDefault` — the `var` command's setter.
pub fn set_parallel_threads_default(value: i32) {
    PARALLEL_THREADS_DEFAULT.store(value, Ordering::Relaxed);
}

/// Callback priority band — C `priorityLow`/`priorityMedium`/`priorityHigh`
/// (`callback.h:41-43`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CallbackPriority {
    /// `priorityLow` (0).
    Low,
    /// `priorityMedium` (1).
    Medium,
    /// `priorityHigh` (2).
    High,
}

impl CallbackPriority {
    /// All bands in index order — mirrors the `for (i = 0; i <
    /// NUM_CALLBACK_PRIORITIES; i++)` loops in `callback.c`.
    pub const ALL: [CallbackPriority; NUM_CALLBACK_PRIORITIES] = [
        CallbackPriority::Low,
        CallbackPriority::Medium,
        CallbackPriority::High,
    ];

    /// The `0..3` band index — C `priorityValue` (`callback.c:98`).
    pub fn index(self) -> usize {
        match self {
            CallbackPriority::Low => 0,
            CallbackPriority::Medium => 1,
            CallbackPriority::High => 2,
        }
    }

    /// The band a record's `PRIO` field selects — C `callbackSetPriority`
    /// (`callback.h:91-92`), which copies the record's `menuPriority` index
    /// (`menuPriority.dbd.pod:24-28` — `LOW`/`MEDIUM`/`HIGH` = 0/1/2, the same
    /// three values as `priorityLow`/`priorityMedium`/`priorityHigh`,
    /// `callback.h:41-43`) straight into `CALLBACK.priority`.
    ///
    /// C validates the copied value only when the callback is queued:
    /// `callbackRequest` drops it with "Bad priority" (`callback.c:355-357`)
    /// when it is outside `0..NUM_CALLBACK_PRIORITIES`. Dropping a record's
    /// deferred work loses that cycle outright, so an out-of-range `PRIO`
    /// lands on `Low` here instead — the band a record whose `PRIO` was never
    /// written already has.
    pub fn from_record_prio(prio: i16) -> CallbackPriority {
        match prio {
            1 => CallbackPriority::Medium,
            2 => CallbackPriority::High,
            _ => CallbackPriority::Low,
        }
    }

    /// Worker-thread name prefix — C `threadNamePrefix` (`callback.c:86-88`).
    pub fn name_prefix(self) -> &'static str {
        match self {
            CallbackPriority::Low => "cbLow",
            CallbackPriority::Medium => "cbMedium",
            CallbackPriority::High => "cbHigh",
        }
    }

    /// OS thread priority for this band — C `threadPriority`
    /// (`callback.c:93-97`): `epicsThreadPriorityScanLow - 1`,
    /// `epicsThreadPriorityScanLow + 4`, `epicsThreadPriorityScanHigh + 1`.
    /// Values are derived from [`ThreadPriority`] so the parity link to
    /// `epicsThread.h` stays in one place.
    pub fn os_priority(self) -> ThreadPriority {
        let scan_low = ThreadPriority::ScanLow.value(); // 60 (epicsThread.h:84)
        let scan_high = ThreadPriority::ScanHigh.value(); // 70 (epicsThread.h:85)
        match self {
            CallbackPriority::Low => ThreadPriority::Custom(scan_low - 1),
            CallbackPriority::Medium => ThreadPriority::Custom(scan_low + 4),
            CallbackPriority::High => ThreadPriority::Custom(scan_high + 1),
        }
    }
}

/// Why a [`CallbackHandle::request`] was rejected.
///
/// Note there is no `Shutdown` variant: a request arriving after the pool has
/// stopped is a silent no-op returning `Ok(())`, matching C — `callbackStop`
/// halts the queues and late `callbackRequest`s are simply dropped without
/// surfacing an error to the caller (`callback.c:237-284`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackError {
    /// The band's ring was full — C `S_db_bufFull` (`callback.c:373`). Either
    /// the push found the ring at capacity, or the overflow latch is still set
    /// from a prior full push (`callback.c:365`).
    QueueFull,
}

/// Mutable, lock-guarded state of one priority band's ring.
struct QueueState {
    queue: VecDeque<Callback>,
    /// C `epicsRingPointerGetHighWaterMark` on the band's ring — the
    /// deepest the queue has ever been. `callbackQueueShow` reports it
    /// and `callbackQueueStatus(reset=1)` clears it
    /// (`callback.c:115-139`), so it is not derivable from `queue.len()`
    /// after the fact and has to be latched on every push.
    high_water: usize,
    /// C `cbQueueSet.queueOverflow` — latched full flag (`callback.c:56`).
    overflow: bool,
    /// C `cbQueueSet.queueOverflows` — lifetime overflow count
    /// (`callback.c:57`).
    overflows: u64,
    shutdown: bool,
}

/// One priority band: a bounded ring plus its wake-up condvar. Mirrors C
/// `cbQueueSet` (`callback.c:53-62`).
struct PriorityQueue {
    capacity: usize,
    state: Mutex<QueueState>,
    /// C `cbQueueSet.semWakeUp` (`callback.c:54`).
    wake: Condvar,
}

impl PriorityQueue {
    fn new(capacity: usize) -> Self {
        PriorityQueue {
            capacity,
            state: Mutex::new(QueueState {
                queue: VecDeque::with_capacity(capacity.min(1024)),
                high_water: 0,
                overflow: false,
                overflows: 0,
                shutdown: false,
            }),
            wake: Condvar::new(),
        }
    }

    /// Port of `callbackRequest` for a single band (`callback.c:341-377`).
    fn request(&self, name: &str, cb: Callback) -> Result<(), CallbackError> {
        let mut st = recover(FACILITY, self.state.lock());
        if st.shutdown {
            // Pool stopped: C drops late callbackRequests after callbackStop
            // without surfacing an error (`callback.c:237-284`). Drop `cb`
            // (deallocated here, never invoked) and report success. This also
            // absorbs the teardown race where the delayed timer fires into a
            // pool that has just been dropped.
            drop(st);
            tracing::trace!(
                target: "epics_base_rs::runtime::callback",
                band = name,
                "callbackRequest after shutdown dropped"
            );
            return Ok(());
        }
        // callback.c:365 — reject immediately while the overflow latch is set.
        if st.overflow {
            return Err(CallbackError::QueueFull);
        }
        // callback.c:367-374 — push; on a full ring, latch overflow and count.
        if st.queue.len() >= self.capacity {
            st.overflow = true;
            st.overflows += 1;
            // callback.c:370 — `fullMessage[priority]`, printed once per
            // overflow episode (the latch above suppresses repeats).
            tracing::error!(
                target: "epics_base_rs::runtime::callback",
                band = name,
                "callbackRequest: ERROR {} ring buffer full",
                name
            );
            return Err(CallbackError::QueueFull);
        }
        st.queue.push_back(cb);
        // The ring's high-water mark moves on the push that made it
        // deepest, exactly where `epicsRingPointer` moves its own.
        st.high_water = st.high_water.max(st.queue.len());
        drop(st);
        // callback.c:375 — signal the band's wake-up event.
        self.wake.notify_one();
        Ok(())
    }

    /// C `callbackQueueStatus` for one band (`callback.c:115-139`):
    /// sample size/used/high-water/overflows, and clear the high-water
    /// mark when `reset` is set.
    fn stats(&self, reset: bool) -> CallbackQueueStats {
        let mut st = recover(FACILITY, self.state.lock());
        let out = CallbackQueueStats {
            size: self.capacity,
            num_used: st.queue.len(),
            max_used: st.high_water,
            num_overflow: st.overflows,
        };
        if reset {
            st.high_water = 0;
        }
        out
    }
}

/// One band's row of C's `callbackQueueStats` (`callback.h`), as
/// `callbackQueueShow` prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackQueueStats {
    /// Ring capacity — C `stats.size`.
    pub size: usize,
    /// Entries queued right now — C `stats.numUsed`.
    pub num_used: usize,
    /// Deepest the ring has been since the last reset — C `stats.maxUsed`.
    pub max_used: usize,
    /// Lifetime overflow count — C `stats.numOverflow`.
    pub num_overflow: u64,
}

/// What this facility is called when it has to report something about itself.
const FACILITY: &str = "callback band";

/// Port of `callbackTask` for one band (`callback.c:210-235`).
fn worker_loop(pq: &PriorityQueue) {
    loop {
        let mut st = recover(FACILITY, pq.state.lock());
        // callback.c:220-221 — sleep on the wake event while the ring is empty.
        while st.queue.is_empty() && !st.shutdown {
            st = recover(FACILITY, pq.wake.wait(st));
        }
        if st.queue.is_empty() {
            // Empty *and* shutdown — drain complete, exit.
            return;
        }
        // callback.c:223 — pop next entry.
        let cb = st.queue.pop_front().unwrap();
        // callback.c:227 — clear the overflow latch on every pop.
        st.overflow = false;
        drop(st);
        // callback.c:228 — run the callback with the ring lock released.
        run_isolated(FACILITY, cb);
    }
}

/// Cheap, clonable submission side of a [`CallbackPool`] — the seam route for
/// RTEMS synchronous-tail hand-offs (increment W3a, decision A2). Holds only
/// `Arc`s to the bands, so cloning is free and it can be handed to the delayed
/// timer, scanOnce worker, and future engine wiring.
#[derive(Clone)]
pub struct CallbackHandle {
    queues: [Arc<PriorityQueue>; NUM_CALLBACK_PRIORITIES],
}

impl CallbackHandle {
    /// Enqueue `cb` on `priority` — port of `callbackRequest`
    /// (`callback.c:341`). Returns immediately; a band worker runs the
    /// callback later. `Err` on a full ring (see [`CallbackError`]).
    pub fn request(&self, priority: CallbackPriority, cb: Callback) -> Result<(), CallbackError> {
        let pq = &self.queues[priority.index()];
        pq.request(priority.name_prefix(), cb)
    }

    /// Lifetime overflow count for a band — C `queueOverflows`
    /// (`callback.c:57`).
    pub fn overflow_count(&self, priority: CallbackPriority) -> u64 {
        recover(FACILITY, self.queues[priority.index()].state.lock()).overflows
    }

    /// One band's `callbackQueueStatus` row (`callback.c:115-139`);
    /// `reset` clears the high-water mark, as C's does.
    pub fn stats(&self, priority: CallbackPriority, reset: bool) -> CallbackQueueStats {
        self.queues[priority.index()].stats(reset)
    }
}

/// The callback executor pool: three independent priority bands, each with its
/// own bounded ring and worker thread(s). Port of the `callbackQueue[]` +
/// `callbackTask` machinery in `callback.c`.
///
/// Dropping the pool shuts every band down and joins its workers (parity with
/// `callbackStop`/`callbackCleanup`, `callback.c:237-284`).
pub struct CallbackPool {
    queues: [Arc<PriorityQueue>; NUM_CALLBACK_PRIORITIES],
    workers: Vec<JoinHandle<()>>,
}

impl CallbackPool {
    /// Build a pool with the C defaults: `callbackQueueSize` capacity per band
    /// (`callback.c:51`) and `callbackThreadsDefault` worker(s) per band
    /// (`callback.c:66`).
    pub fn new() -> Self {
        Self::with_per_priority_config(
            CONFIGURED_QUEUE_SIZE.load(Ordering::Relaxed),
            CallbackPriority::ALL.map(|p| CONFIGURED_THREADS[p.index()].load(Ordering::Relaxed)),
        )
    }

    /// Build a pool with an explicit ring capacity and worker count per band.
    /// `threads_per_priority` is clamped to at least 1 (C `callbackParallelThreads`
    /// forces `count >= 1`, `callback.c:171`).
    pub fn with_config(queue_size: usize, threads_per_priority: usize) -> Self {
        Self::with_per_priority_config(queue_size, [threads_per_priority; NUM_CALLBACK_PRIORITIES])
    }

    /// Build a pool whose bands may carry DIFFERENT worker counts — C
    /// `callbackQueue[i].threadsConfigured` is per band
    /// (`callback.c:60`, `:177`, `:205`), so
    /// `callbackParallelThreads(4, "HIGH")` widens one band only.
    pub fn with_per_priority_config(
        queue_size: usize,
        threads_per_priority: [usize; NUM_CALLBACK_PRIORITIES],
    ) -> Self {
        let capacity = queue_size.max(1);
        let threads_per_priority = threads_per_priority.map(|n| n.max(1));
        let queues: [Arc<PriorityQueue>; NUM_CALLBACK_PRIORITIES] = [
            Arc::new(PriorityQueue::new(capacity)),
            Arc::new(PriorityQueue::new(capacity)),
            Arc::new(PriorityQueue::new(capacity)),
        ];

        let mut workers = Vec::with_capacity(threads_per_priority.iter().sum::<usize>());
        for prio in CallbackPriority::ALL {
            let pq = &queues[prio.index()];
            let threads = threads_per_priority[prio.index()];
            for j in 0..threads {
                // callback.c:324-327 — `cbLow` when single, `cbLow-<n>` when
                // parallel.
                let name = if threads > 1 {
                    format!("{}-{}", prio.name_prefix(), j)
                } else {
                    prio.name_prefix().to_string()
                };
                let pq = Arc::clone(pq);
                let watched_name = name.clone();
                // A band with no worker is a band whose queued callbacks never
                // run again — deferred record processing, delayed callbacks,
                // monitor tails. There is no error path out of a constructor
                // reached through a `OnceLock` initialiser, so the failure is
                // fatal by `MandatoryThread` rather than a panic that would
                // unwind on whichever thread happened to touch the pool first.
                let handle = MandatoryThread::new(
                    name,
                    // callback.c:322 — `opts.priority = threadPriority[i]`,
                    // applied best-effort to this OS thread.
                    prio.os_priority(),
                    // callback.c:323 — `opts.stackSize = epicsThreadStackBig`.
                    StackSizeClass::Big,
                )
                .spawn(move || {
                    // C `callbackTask` registers itself and removes on the way
                    // out (`callback.c:215`, `:234`). Unbounded: a callback
                    // band with an empty queue is parked on its semaphore, and
                    // an idle IOC is not a fault.
                    let _watched = crate::runtime::taskwd::taskwd_insert(
                        watched_name,
                        crate::runtime::taskwd::CheckIn::Unbounded,
                        None,
                    );
                    run_facility_loop(
                        FACILITY,
                        || worker_loop(&pq),
                        || recover(FACILITY, pq.state.lock()).shutdown = true,
                    );
                });
                workers.push(handle);
            }
        }

        CallbackPool { queues, workers }
    }

    /// A cheap, clonable submission handle (see [`CallbackHandle`]).
    pub fn handle(&self) -> CallbackHandle {
        CallbackHandle {
            queues: self.queues.clone(),
        }
    }

    /// Enqueue `cb` on `priority` — convenience wrapper over
    /// [`CallbackHandle::request`].
    pub fn request(&self, priority: CallbackPriority, cb: Callback) -> Result<(), CallbackError> {
        self.queues[priority.index()].request(priority.name_prefix(), cb)
    }

    /// Lifetime overflow count for a band — C `queueOverflows`
    /// (`callback.c:57`).
    pub fn overflow_count(&self, priority: CallbackPriority) -> u64 {
        recover(FACILITY, self.queues[priority.index()].state.lock()).overflows
    }

    /// One band's `callbackQueueStatus` row (`callback.c:115-139`);
    /// `reset` clears the high-water mark, as C's does.
    pub fn stats(&self, priority: CallbackPriority, reset: bool) -> CallbackQueueStats {
        self.queues[priority.index()].stats(reset)
    }

    /// Stop every band and join its workers — port of the shutdown half of
    /// `callbackStop`/`callbackCleanup` (`callback.c:237-284`). Idempotent.
    pub fn shutdown(&mut self) {
        for pq in &self.queues {
            recover(FACILITY, pq.state.lock()).shutdown = true;
            pq.wake.notify_all();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Default for CallbackPool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CallbackPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const T: Duration = Duration::from_secs(5);

    /// Boundary: a callback that panics. It runs on the band's own worker, so
    /// before this one panicking callback silently retired the band and every
    /// later callback on it — deferred processing, delayed callbacks, monitor
    /// tails — simply never ran.
    #[test]
    fn a_panicking_callback_does_not_stop_the_band() {
        let pool = CallbackPool::new();
        pool.request(
            CallbackPriority::Medium,
            Box::new(|| panic!("a callback panicked on its band")),
        )
        .expect("enqueue the panicking callback");

        let (tx, rx) = mpsc::channel();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || tx.send(42u32).unwrap()),
        )
        .expect("enqueue the next callback");
        assert_eq!(
            rx.recv_timeout(T).unwrap(),
            42,
            "the callback after a panicking one never ran: the band worker died with it"
        );
    }

    #[test]
    fn enqueued_callback_runs() {
        let pool = CallbackPool::new();
        let (tx, rx) = mpsc::channel();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || tx.send(42u32).unwrap()),
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(T).unwrap(), 42);
    }

    #[test]
    fn priority_bands_are_independent() {
        // Invariant: a blocked Low worker MUST NOT stall the High band.
        let pool = CallbackPool::new();

        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        // Occupy the single Low worker and hold it inside the callback.
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap(); // Low worker is now blocked.

        // High must still run despite Low being wedged.
        let (high_tx, high_rx) = mpsc::channel();
        pool.request(
            CallbackPriority::High,
            Box::new(move || high_tx.send(()).unwrap()),
        )
        .unwrap();
        high_rx
            .recv_timeout(T)
            .expect("High band stalled behind a blocked Low worker");

        gate_tx.send(()).unwrap(); // release Low so shutdown can join.
    }

    #[test]
    fn full_ring_latches_overflow_then_recovers() {
        // Boundary: capacity-1 ring, worker pinned busy → the second live
        // entry fills the ring, the third latches overflow (callback.c:365).
        let mut pool = CallbackPool::with_config(1, 1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();

        // Worker picks this up and blocks; ring is now empty again.
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();

        // Fill the single ring slot (worker is busy, cannot drain).
        pool.request(CallbackPriority::Low, Box::new(|| {}))
            .unwrap();
        // Next push finds the ring full → QueueFull + overflow latched.
        assert_eq!(
            pool.request(CallbackPriority::Low, Box::new(|| {})),
            Err(CallbackError::QueueFull)
        );
        // While latched, even a would-fit push is rejected (callback.c:365).
        assert_eq!(
            pool.request(CallbackPriority::Low, Box::new(|| {})),
            Err(CallbackError::QueueFull)
        );
        assert_eq!(pool.overflow_count(CallbackPriority::Low), 1);

        gate_tx.send(()).unwrap(); // release the worker so it drains + clears.
        pool.shutdown();
    }

    #[test]
    fn request_after_shutdown_is_silent_noop() {
        // Boundary: a CallbackHandle that outlives the pool (the delayed-timer
        // teardown race) must get Ok(()) and the callback must never run.
        let pool = CallbackPool::new();
        let h = pool.handle();
        drop(pool); // sets shutdown on every band, joins workers.

        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        let res = h.request(
            CallbackPriority::High,
            Box::new(move || r.store(true, Ordering::SeqCst)),
        );
        assert_eq!(res, Ok(())); // silent no-op, not Err.
        assert!(
            !ran.load(Ordering::SeqCst),
            "callback ran after shutdown; it must be dropped, not invoked"
        );
    }

    /// `cpu_count()` must report the processors the calling thread may
    /// actually run on, not the host's — epics-base `556de06ff`, which the
    /// reference pin R7.0.10 does not carry (see [`cpu_count`]). At the pin
    /// this returns the host count for a pinned thread, which is exactly the
    /// overreporting that commit removed.
    ///
    /// The mask is set on a thread of this test's own: on Linux affinity is
    /// per-thread and `sched_getaffinity(0, ..)` — what
    /// `available_parallelism` calls — reads the caller's, so no other
    /// test's thread is disturbed.
    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_count_respects_the_threads_affinity_mask() {
        let host = cpu_count();
        if host < 2 {
            // Already pinned to one processor: nothing left to restrict, and
            // the assertion below would hold for the pin's behaviour too.
            return;
        }
        let pinned = std::thread::spawn(|| {
            // SAFETY: both calls address pid 0 (this thread) and a
            // `cpu_set_t` owned by this frame; nothing else is observed or
            // mutated.
            unsafe {
                let mut have: libc::cpu_set_t = std::mem::zeroed();
                if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut have) != 0 {
                    return None;
                }
                // Keep the lowest processor already permitted — CPU 0 need
                // not be in the mask this process inherited.
                let first = (0..libc::CPU_SETSIZE as usize).find(|&c| libc::CPU_ISSET(c, &have))?;
                let mut one: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_ZERO(&mut one);
                libc::CPU_SET(first, &mut one);
                if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &one) != 0 {
                    return None;
                }
            }
            Some(cpu_count())
        })
        .join()
        .expect("the pinned thread must not panic");
        let Some(pinned) = pinned else {
            // The sandbox forbids setting affinity; nothing measurable here.
            return;
        };
        assert_eq!(
            pinned, 1,
            "cpu_count() reported {host} for a thread pinned to one \
             processor — that is the pre-556de06ff sysconf behaviour"
        );
    }
}
