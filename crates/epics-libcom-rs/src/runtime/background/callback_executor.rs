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
//! This module keeps that structure but with **plain `std` threads** and
//! boxed closures instead of C function pointers, so it carries **no
//! tokio-runtime dependency** and runs on RTEMS (armv7-rtems-eabihf). The OS
//! thread priority per band is applied best-effort via the existing [`apply_to_current_thread`](crate::runtime::task::apply_to_current_thread) abstraction in
//! [`crate::runtime::task`] — this module does **not** duplicate that logic.
//!
//! ## The band has no lock (epics-base PR #996)
//!
//! C's band is a mutex around an `epicsRingPointer` plus one event every
//! worker of the band waits on, so a push wakes whoever the OS picks and
//! every worker contends on the one lock. epics-base PR #996 replaces both:
//! a lock-free inbox a worker takes *whole*, and a wake-up token per worker.
//! This port does the same, and two of that PR's parts have no counterpart
//! here. Its generation-tagged free list is unnecessary because take-all is
//! the only consumer operation, which makes the push CAS ABA-free on its own
//! (see [`PriorityQueue::inbox`]); its futex `epicsEvent` is unnecessary
//! because `std::thread::park`/`unpark` is already a per-thread futex token.
//!
//! What C reports about a band is unchanged — `callbackQueueStatus`'s `size`,
//! `numUsed`, `maxUsed` and `numOverflow`, the overflow latch cleared on
//! every pop, and the `S_db_bufFull` rejection — so the accounting moved from
//! fields under the lock to atomics, not away.
//!
//! One semantic does change, and only for a band widened by
//! `callbackParallelThreads`: a worker takes the whole inbox, so a second
//! worker's later batch can run ahead of the first worker's tail. With C's
//! default of one worker per band the order is still strict FIFO; a band with
//! parallel workers had no cross-worker ordering guarantee to begin with.
//!
//! ## Overflow hysteresis (`callback.c:365-374`, `:227`)
//!
//! C sets a per-band `queueOverflow` flag when a push finds the ring full; a
//! subsequent `callbackRequest` returns `S_db_bufFull` *immediately*
//! (`callback.c:365`) without even attempting a push, until a worker pops an
//! entry and clears the flag (`callback.c:227`). We reproduce that exact
//! latch: once `overflow` is set, `request` rejects until a worker drains one
//! entry.

use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, LazyLock, OnceLock};
use std::thread::{JoinHandle, Thread};

use super::facility::{run_facility_loop, run_isolated};
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

/// One FIFO entry of a band.
enum Queued {
    /// A `callbackRequest` — holds one of the ring's `capacity` slots.
    Ring(Callback),
    /// A spawned future's run-queue entry. A task has at most one at a time
    /// (its `SCHEDULED` state is the claim), so these are bounded by the live
    /// task count and take no ring slot: a wake that the ring could reject
    /// would strand a long-lived task forever.
    Task(Callback),
}

/// One queued entry as the inbox links it. The node owns both the link and
/// the item, because `Queued` is a boxed closure and has nowhere to carry an
/// intrusive pointer of its own.
struct Node {
    next: *mut Node,
    item: Queued,
}

/// A chain of nodes a worker has taken out of the inbox, in push order.
///
/// The take is a `swap`, so the chain becomes this worker's property the
/// instant it leaves the inbox: nothing else can free it, and nothing else
/// will. Every exit from the drain therefore has to free the remainder — the
/// normal end, an early `return`, a panic unwinding through the loop — which
/// is what this type is for and what a bare `*mut Node` walked by hand would
/// not give.
struct Chain(*mut Node);

impl Chain {
    fn pop(&mut self) -> Option<Queued> {
        if self.0.is_null() {
            return None;
        }
        // SAFETY: the pointer came from `Box::into_raw` in
        // `PriorityQueue::push` and was handed over exclusively by the `swap`
        // in `take_all`, so this is the one owner and the box is
        // reconstituted exactly once.
        let node = unsafe { Box::from_raw(self.0) };
        self.0 = node.next;
        Some(node.item)
    }

    fn is_empty(&self) -> bool {
        self.0.is_null()
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        while self.pop().is_some() {}
    }
}

/// Awake and bound for the inbox. A pusher that sees this state needs to wake
/// nobody — see [`PriorityQueue::wake_one`].
const WORKER_IDLE: u32 = 0;
/// Running callbacks out of a chain it already took. It will look at the
/// inbox again before it parks, so nothing queued now can be lost — but not
/// until its whole batch has run, which is why a parked worker is woken in
/// preference to leaving the work to this one (PR #996's work sharing).
const WORKER_BUSY: u32 = 1;
/// Parked on its own token, and will not look at the inbox again until
/// somebody moves it out of this state.
const WORKER_PARKED: u32 = 2;

/// One worker's park state and wake-up token. C's single
/// `cbQueueSet.semWakeUp` (`callback.c:54`) became one event per worker in
/// epics-base PR #996; in Rust the token is the thread's own `park`/`unpark`
/// pair, so there is nothing here to allocate or destroy.
struct WorkerSlot {
    state: AtomicU32,
    /// Published once by the worker itself, before its first park. A slot
    /// whose thread is still unset belongs to a worker that has not reached
    /// its first inbox read, so it needs no wake-up either.
    thread: OnceLock<Thread>,
}

/// One priority band: a lock-free inbox, the ring accounting C reports on it,
/// and one wake-up slot per worker. Mirrors C `cbQueueSet`
/// (`callback.c:53-62`) as epics-base PR #996 leaves it.
struct PriorityQueue {
    capacity: usize,
    /// Treiber-stack head. Pushers CAS a node on; a worker takes the entire
    /// chain with one `swap`.
    ///
    /// Take-all being the only consumer operation is what makes the push CAS
    /// ABA-free: a node is either in the inbox or gone from it forever, so a
    /// CAS that succeeds against an observed head succeeded against that same
    /// live node. This is why the generation tag C's free list carries has no
    /// counterpart here — it is the take-all shape that buys it, not the
    /// pointer width.
    inbox: AtomicPtr<Node>,
    /// Ring slots in use — `Queued::Ring` entries pushed but not yet run.
    /// This, not the inbox length, is what C's bounded ring measures: a
    /// task's run-queue entry shares the inbox but holds no ring slot, and an
    /// entry a worker has taken into its chain but not yet run still occupies
    /// C's ring.
    ring_used: AtomicUsize,
    /// C `epicsRingPointerGetHighWaterMark` on the band's ring — the deepest
    /// the queue has ever been. `callbackQueueShow` reports it and
    /// `callbackQueueStatus(reset=1)` clears it (`callback.c:115-139`), so it
    /// is not derivable after the fact and has to be latched on every push.
    high_water: AtomicUsize,
    /// C `cbQueueSet.queueOverflow` — latched full flag (`callback.c:56`).
    overflow: AtomicBool,
    /// C `cbQueueSet.queueOverflows` — lifetime overflow count
    /// (`callback.c:57`).
    overflows: AtomicU64,
    shutdown: AtomicBool,
    /// One slot per worker of this band, fixed at construction: a band's
    /// worker count is `threadsConfigured` (`callback.c:60`) and never
    /// changes while the band runs.
    workers: Vec<WorkerSlot>,
}

impl PriorityQueue {
    fn new(capacity: usize, workers: usize) -> Self {
        PriorityQueue {
            capacity,
            inbox: AtomicPtr::new(std::ptr::null_mut()),
            ring_used: AtomicUsize::new(0),
            high_water: AtomicUsize::new(0),
            overflow: AtomicBool::new(false),
            overflows: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            workers: (0..workers.max(1))
                .map(|_| WorkerSlot {
                    state: AtomicU32::new(WORKER_IDLE),
                    thread: OnceLock::new(),
                })
                .collect(),
        }
    }

    /// Publish this worker's thread so pushers can unpark it. Called by the
    /// worker itself before its first inbox read.
    fn register_worker(&self, idx: usize) {
        let _ = self.workers[idx].thread.set(std::thread::current());
    }

    /// Push one entry and make sure somebody will come for it.
    ///
    /// # Invariant
    ///
    /// **After a push returns, at least one worker of the band is awake or
    /// has been unparked.** Nothing else in this module is allowed to move a
    /// worker out of [`WORKER_PARKED`] except [`wake_one`](Self::wake_one),
    /// [`wake_all`](Self::wake_all) and the worker itself, which is what
    /// keeps the invariant checkable in one place.
    fn push(&self, item: Queued) {
        let node = Box::into_raw(Box::new(Node {
            next: std::ptr::null_mut(),
            item,
        }));
        let mut head = self.inbox.load(Ordering::Relaxed);
        loop {
            // SAFETY: the node is this thread's alone until the CAS below
            // publishes it, so writing its link is unsynchronized by right.
            unsafe { (*node).next = head };
            match self
                .inbox
                .compare_exchange_weak(head, node, Ordering::SeqCst, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(seen) => head = seen,
            }
        }
        self.wake_one();
    }

    /// Take the whole inbox, reversed back into push order.
    fn take_all(&self) -> Chain {
        let mut lifo = self.inbox.swap(std::ptr::null_mut(), Ordering::SeqCst);
        let mut fifo: *mut Node = std::ptr::null_mut();
        while !lifo.is_null() {
            // SAFETY: the swap took the whole chain exclusively; no other
            // thread can reach these nodes.
            let next = unsafe { (*lifo).next };
            unsafe { (*lifo).next = fifo };
            fifo = lifo;
            lifo = next;
        }
        Chain(fifo)
    }

    /// Wake one worker for the entry just pushed, or nobody if a worker is
    /// already on its way to the inbox.
    ///
    /// This is C `callbackRequest`'s `epicsEventSignal` (`callback.c:375`)
    /// after PR #996: signalling the band's one event woke a worker per push
    /// whether or not one was already running, and every woken worker then
    /// fought for the band lock. An awake worker reads the inbox before it
    /// parks, so the push it has not seen yet is a push it is about to see,
    /// and the wake-up would buy a syscall and nothing else.
    ///
    /// The handshake with [`park_for_work`](Self::park_for_work) is Dekker's:
    /// the push CAS precedes these loads and the worker's `WORKER_PARKED`
    /// store precedes its last inbox read, all `SeqCst`. So of the two
    /// orders, either this sees `WORKER_PARKED` and unparks, or the worker
    /// sees the node and does not park — never both missing.
    fn wake_one(&self) {
        let mut parked: Option<&WorkerSlot> = None;
        for w in &self.workers {
            match w.state.load(Ordering::SeqCst) {
                WORKER_IDLE => return,
                WORKER_PARKED if parked.is_none() => parked = Some(w),
                _ => {}
            }
        }
        let Some(w) = parked else {
            // Every worker is busy draining a chain, and a busy worker reads
            // the inbox again before it parks.
            return;
        };
        if w.state
            .compare_exchange(
                WORKER_PARKED,
                WORKER_IDLE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            if let Some(t) = w.thread.get() {
                t.unpark();
            }
        }
        // A lost CAS means the worker left `WORKER_PARKED` itself, which it
        // only does on its way to the inbox — the invariant holds without a
        // second candidate being woken.
    }

    /// Wake every worker of the band. Shutdown's counterpart to
    /// [`wake_one`](Self::wake_one) — C broadcast one event
    /// (`callback.c:270`), and with a token per worker every token has to be
    /// handed out.
    fn wake_all(&self) {
        for w in &self.workers {
            let _ = w.state.compare_exchange(
                WORKER_PARKED,
                WORKER_IDLE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            if let Some(t) = w.thread.get() {
                t.unpark();
            }
        }
    }

    /// Park worker `idx` until there is work or the band is stopping — C
    /// `callbackTask`'s `epicsEventWait` (`callback.c:220`) on this worker's
    /// own token.
    fn park_for_work(&self, idx: usize) {
        let me = &self.workers[idx];
        me.state.store(WORKER_PARKED, Ordering::SeqCst);
        // Announce the park *before* the last look at the inbox. See
        // `wake_one` for why that order is the whole of the handshake.
        if !self.inbox.load(Ordering::SeqCst).is_null() || self.shutdown.load(Ordering::SeqCst) {
            let _ = me.state.compare_exchange(
                WORKER_PARKED,
                WORKER_IDLE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            return;
        }
        while me.state.load(Ordering::SeqCst) == WORKER_PARKED {
            // A stale token left by an earlier wake-up only costs this one
            // spurious return; the state is what says whether to go back.
            std::thread::park();
        }
    }

    /// Port of `callbackRequest` for a single band (`callback.c:341-377`).
    fn request(&self, name: &str, cb: Callback) -> Result<(), CallbackError> {
        if self.shutdown.load(Ordering::SeqCst) {
            // Pool stopped: C drops late callbackRequests after callbackStop
            // without surfacing an error (`callback.c:237-284`). Drop `cb`
            // (deallocated here, never invoked) and report success. This also
            // absorbs the teardown race where the delayed timer fires into a
            // pool that has just been dropped.
            drop(cb);
            tracing::trace!(
                target: "epics_base_rs::runtime::callback",
                band = name,
                "callbackRequest after shutdown dropped"
            );
            return Ok(());
        }
        // callback.c:365 — reject immediately while the overflow latch is set.
        if self.overflow.load(Ordering::Acquire) {
            return Err(CallbackError::QueueFull);
        }
        // callback.c:367-374 — claim a ring slot; on a full ring, latch
        // overflow and count. The claim is the CAS, so `ring_used` can never
        // pass `capacity` however many threads push at once.
        let mut used = self.ring_used.load(Ordering::Acquire);
        loop {
            if used >= self.capacity {
                // The latch is the gate on counting too, not just on
                // rejecting: C sets the flag and counts under the band lock,
                // so concurrent full pushes report one overflow episode, not
                // one each (`callback.c:367-371`).
                if self
                    .overflow
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.overflows.fetch_add(1, Ordering::AcqRel);
                    // callback.c:370 — `fullMessage[priority]`, printed once
                    // per overflow episode.
                    tracing::error!(
                        target: "epics_base_rs::runtime::callback",
                        band = name,
                        "callbackRequest: ERROR {} ring buffer full",
                        name
                    );
                }
                return Err(CallbackError::QueueFull);
            }
            match self.ring_used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(seen) => used = seen,
            }
        }
        // The ring's high-water mark moves on the push that made it deepest,
        // exactly where `epicsRingPointer` moves its own.
        self.high_water.fetch_max(used + 1, Ordering::AcqRel);
        self.push(Queued::Ring(cb));
        Ok(())
    }

    /// Queue a spawned future's run-queue entry. Never refused for capacity —
    /// see [`Queued::Task`]. After shutdown `cb` is dropped un-run, which is
    /// how the task learns it was cancelled.
    fn schedule_task(&self, cb: Callback) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        self.push(Queued::Task(cb));
    }

    /// C `callbackQueueStatus` for one band (`callback.c:115-139`):
    /// sample size/used/high-water/overflows, and clear the high-water mark
    /// when `reset` is set.
    fn stats(&self, reset: bool) -> CallbackQueueStats {
        let num_used = self.ring_used.load(Ordering::Acquire);
        let out = CallbackQueueStats {
            size: self.capacity,
            num_used,
            // C samples both under the band lock, so `maxUsed >= numUsed`
            // always holds for its caller. Here the claim and the latch are
            // two atomics, and a push caught between them would otherwise
            // show a depth deeper than the mark it is in the middle of
            // setting.
            max_used: self.high_water.load(Ordering::Acquire).max(num_used),
            num_overflow: self.overflows.load(Ordering::Acquire),
        };
        if reset {
            self.high_water.store(0, Ordering::Release);
        }
        out
    }
}

impl Drop for PriorityQueue {
    /// A request that lost the shutdown race pushed into an inbox no worker
    /// will read again; the band owns those nodes and frees them here.
    fn drop(&mut self) {
        drop(self.take_all());
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

/// Port of `callbackTask` for one band (`callback.c:210-235`), as worker
/// `idx` of that band.
fn worker_loop(pq: &PriorityQueue, idx: usize) {
    pq.register_worker(idx);
    loop {
        // callback.c:223 — take the queued work. One `swap` where C popped
        // one entry under the band lock.
        let mut chain = pq.take_all();
        if chain.is_empty() {
            if pq.shutdown.load(Ordering::SeqCst) {
                // Empty *and* shutdown — drain complete, exit.
                return;
            }
            // callback.c:220-221 — sleep while the band has nothing to run.
            pq.park_for_work(idx);
            continue;
        }
        pq.workers[idx].state.store(WORKER_BUSY, Ordering::SeqCst);
        while let Some(item) = chain.pop() {
            let cb = match item {
                Queued::Ring(cb) => {
                    // The entry leaves C's ring here, not when the chain was
                    // taken: `numUsed` counts what has not run yet.
                    pq.ring_used.fetch_sub(1, Ordering::AcqRel);
                    // callback.c:227 — clear the overflow latch on every pop.
                    pq.overflow.store(false, Ordering::Release);
                    cb
                }
                Queued::Task(cb) => cb,
            };
            // callback.c:228 — run the callback with nothing of the band held.
            run_isolated(FACILITY, cb);
        }
        pq.workers[idx].state.store(WORKER_IDLE, Ordering::SeqCst);
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

    /// Queue a spawned future's run-queue entry on `priority`. Unlike
    /// [`request`](Self::request) this cannot fail: the entry takes no ring
    /// slot. The caller guarantees at most one such entry per task.
    pub(super) fn schedule_task(&self, priority: CallbackPriority, cb: Callback) {
        self.queues[priority.index()].schedule_task(cb);
    }

    /// Lifetime overflow count for a band — C `queueOverflows`
    /// (`callback.c:57`).
    pub fn overflow_count(&self, priority: CallbackPriority) -> u64 {
        self.queues[priority.index()]
            .overflows
            .load(Ordering::Acquire)
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
        let queues: [Arc<PriorityQueue>; NUM_CALLBACK_PRIORITIES] =
            CallbackPriority::ALL.map(|p| {
                Arc::new(PriorityQueue::new(
                    capacity,
                    threads_per_priority[p.index()],
                ))
            });

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
                        || worker_loop(&pq, j),
                        || pq.shutdown.store(true, Ordering::SeqCst),
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
        self.queues[priority.index()]
            .overflows
            .load(Ordering::Acquire)
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
            pq.shutdown.store(true, Ordering::SeqCst);
            pq.wake_all();
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

/// A task executor of a server's own, at a priority the server chooses.
///
/// # Why this exists beside [`CallbackPool`]
///
/// `CallbackPool` is the port of `callback.c`, so its three bands carry C's
/// thread priorities and nothing else: `epicsThreadPriorityScanLow - 1`, `+ 4`,
/// `epicsThreadPriorityScanHigh + 1` — 59, 64, 71. That ladder is parity, not a
/// preference, and it must not become configurable.
///
/// A network server's band is set against the *other servers* in the IOC, not
/// against record processing: pvxs runs its TCP reactor at `CAServerLow-2` =
/// 18, CA's rsrv its own ladder from `caservertask.c`. Before this type the
/// only executor a future could be spawned onto was the callback pool, so
/// every server's connection future ran at 64 — in the band C reserves for
/// deferred record processing, sharing its rings. Two consequences, and the
/// second is the one that bites: the pvxs band layout was not reproduced, and
/// a slow connection and a deferred record tail could starve each other.
///
/// # One ring, whatever band is named
///
/// A dedicated executor has one priority by construction, so the
/// [`CallbackPriority`] a caller names on [`handle`](Self::handle) selects
/// nothing — all three slots are the same ring. That is deliberate: it keeps
/// the handle type shared with the callback pool, so
/// [`spawn_future`](crate::runtime::background::spawn_future) needs no second
/// form, and it makes naming a band here impossible to get wrong rather than
/// silently routing work to a ring with no worker.
pub struct DedicatedExecutor {
    queue: Arc<PriorityQueue>,
    workers: Vec<JoinHandle<()>>,
}

impl DedicatedExecutor {
    /// Start `threads` workers named `name` at `priority`.
    ///
    /// Fallible, unlike [`CallbackPool`]'s `MandatoryThread` workers: this
    /// executor belongs to one server, so a thread that cannot start is that
    /// server's `bind` failing, not the process aborting. `threads` is clamped
    /// to at least 1 — an executor with no worker is a queue whose tasks never
    /// run.
    pub fn new(name: &str, priority: ThreadPriority, threads: usize) -> std::io::Result<Self> {
        let threads = threads.max(1);
        let queue = Arc::new(PriorityQueue::new(
            CONFIGURED_QUEUE_SIZE.load(Ordering::Relaxed).max(1),
            threads,
        ));
        let mut workers = Vec::with_capacity(threads);
        for j in 0..threads {
            // `callback.c:324-327`'s naming rule, applied to this executor's
            // own name: bare when single, `-<n>` when parallel.
            let worker_name = if threads > 1 {
                format!("{name}-{j}")
            } else {
                name.to_string()
            };
            let pq = Arc::clone(&queue);
            let watched_name = worker_name.clone();
            let spawned = crate::runtime::task::spawn_dedicated_thread(
                worker_name,
                priority,
                StackSizeClass::Big,
                move || {
                    let _watched = crate::runtime::taskwd::taskwd_insert(
                        watched_name,
                        crate::runtime::taskwd::CheckIn::Unbounded,
                        None,
                    );
                    run_facility_loop(
                        FACILITY,
                        || worker_loop(&pq, j),
                        || pq.shutdown.store(true, Ordering::SeqCst),
                    );
                },
            );
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(e) => {
                    // The workers already started have to go before the error
                    // leaves, or they outlive the executor nobody now holds.
                    let mut partial = DedicatedExecutor { queue, workers };
                    partial.shutdown();
                    return Err(e);
                }
            }
        }
        Ok(DedicatedExecutor { queue, workers })
    }

    /// A submission handle. Every band names the same ring — see the type doc.
    pub fn handle(&self) -> CallbackHandle {
        CallbackHandle {
            queues: [
                Arc::clone(&self.queue),
                Arc::clone(&self.queue),
                Arc::clone(&self.queue),
            ],
        }
    }

    /// Stop the workers and join them. Idempotent; [`Drop`] calls it.
    pub fn shutdown(&mut self) {
        self.queue.shutdown.store(true, Ordering::SeqCst);
        self.queue.wake_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl std::fmt::Debug for DedicatedExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DedicatedExecutor")
            .field("workers", &self.workers.len())
            .finish()
    }
}

impl Drop for DedicatedExecutor {
    fn drop(&mut self) {
        self.shutdown();
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

    /// The property the type exists for: work submitted to a dedicated
    /// executor runs on a thread at the priority its owner asked for, not on
    /// C's `cbMedium` band.
    #[test]
    fn a_dedicated_executor_runs_its_work_at_the_priority_it_was_given() {
        let exec = DedicatedExecutor::new("TESTEXEC", ThreadPriority::Custom(18), 1)
            .expect("executor starts");
        let (tx, rx) = mpsc::channel();
        exec.handle()
            .request(
                CallbackPriority::Medium,
                Box::new(move || {
                    let name = std::thread::current()
                        .name()
                        .unwrap_or_default()
                        .to_string();
                    tx.send(name).expect("send");
                }),
            )
            .expect("enqueue");
        assert_eq!(rx.recv_timeout(T).expect("callback ran"), "TESTEXEC");
    }

    /// Naming a band on a dedicated executor selects nothing — all three reach
    /// the one ring. A band that silently had no worker would hang instead.
    #[test]
    fn every_band_on_a_dedicated_executor_names_the_same_ring() {
        let exec = DedicatedExecutor::new("BANDEXEC", ThreadPriority::Custom(18), 1)
            .expect("executor starts");
        for band in CallbackPriority::ALL {
            let (tx, rx) = mpsc::channel();
            exec.handle()
                .request(band, Box::new(move || tx.send(()).expect("send")))
                .expect("enqueue");
            rx.recv_timeout(T)
                .unwrap_or_else(|_| panic!("{band:?} reached a worker"));
        }
    }

    /// Parallel workers get C's `callbackTask` naming rule, and all of them
    /// drain the one ring.
    #[test]
    fn parallel_workers_share_the_ring_and_are_numbered() {
        let exec = DedicatedExecutor::new("PAREXEC", ThreadPriority::Custom(18), 2)
            .expect("executor starts");
        let (tx, rx) = mpsc::channel();
        for _ in 0..8 {
            let tx = tx.clone();
            exec.handle()
                .request(
                    CallbackPriority::Medium,
                    Box::new(move || {
                        let name = std::thread::current()
                            .name()
                            .unwrap_or_default()
                            .to_string();
                        tx.send(name).expect("send");
                    }),
                )
                .expect("enqueue");
        }
        drop(tx);
        let names: Vec<String> = rx.iter().take(8).collect();
        assert_eq!(names.len(), 8, "every task ran");
        for name in &names {
            assert!(
                name == "PAREXEC-0" || name == "PAREXEC-1",
                "unexpected worker {name}"
            );
        }
    }

    /// `shutdown` is what `Drop` calls, so a second call must not hang on
    /// workers that are already joined.
    #[test]
    fn shutting_a_dedicated_executor_down_twice_is_a_no_op() {
        let mut exec = DedicatedExecutor::new("DUPEXEC", ThreadPriority::Custom(18), 1)
            .expect("executor starts");
        exec.shutdown();
        exec.shutdown();
        // A request after shutdown is dropped, not an error — C's rule for a
        // stopped pool (`callback.c:237-284`).
        assert!(
            exec.handle()
                .request(CallbackPriority::Medium, Box::new(|| {}))
                .is_ok()
        );
    }

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

    /// Park/unpark boundary: `ring_used == 0` at park time. The band has
    /// certainly parked (nothing queued, nothing running), so this is the
    /// `wake_one` → `unpark` path and nothing else.
    #[test]
    fn a_push_into_a_parked_band_wakes_it() {
        let pool = CallbackPool::with_config(16, 1);
        for round in 0..4u32 {
            let (tx, rx) = mpsc::channel();
            pool.request(
                CallbackPriority::Low,
                Box::new(move || tx.send(round).unwrap()),
            )
            .unwrap();
            assert_eq!(
                rx.recv_timeout(T).unwrap(),
                round,
                "round {round} never ran: the push did not wake the parked band"
            );
            // The worker is now draining nothing and on its way back to a
            // park, so the next round starts from the parked state again.
            assert_eq!(pool.stats(CallbackPriority::Low, false).num_used, 0);
        }
    }

    /// Park/unpark boundary: `ring_used > 0` at the push. The worker is busy
    /// inside a callback, so `wake_one` must wake nobody and the busy worker
    /// has to pick the new entry up on its own next look at the inbox.
    #[test]
    fn work_pushed_at_a_busy_band_runs_without_a_wake_up() {
        let mut pool = CallbackPool::with_config(16, 1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();

        let (tx, rx) = mpsc::channel();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || tx.send(7u32).unwrap()),
        )
        .unwrap();
        gate_tx.send(()).unwrap();
        assert_eq!(
            rx.recv_timeout(T).unwrap(),
            7,
            "the entry pushed while the worker was busy was never taken"
        );
        pool.shutdown();
    }

    /// The latch clears on the first entry a worker takes out of the ring
    /// (`callback.c:227`) — the recovery half of
    /// [`full_ring_latches_overflow_then_recovers`].
    #[test]
    fn one_drained_entry_clears_the_overflow_latch() {
        let mut pool = CallbackPool::with_config(1, 1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();

        let (ran_tx, ran_rx) = mpsc::channel();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || ran_tx.send(1u32).unwrap()),
        )
        .unwrap();
        assert_eq!(
            pool.request(CallbackPriority::Low, Box::new(|| {})),
            Err(CallbackError::QueueFull)
        );

        gate_tx.send(()).unwrap();
        // The worker clears the latch when it takes the entry out of the
        // ring, before it runs it, so this signal is proof the latch is
        // already clear — the next push has to be accepted.
        assert_eq!(ran_rx.recv_timeout(T).unwrap(), 1);
        let (tx, rx) = mpsc::channel();
        assert_eq!(
            pool.request(
                CallbackPriority::Low,
                Box::new(move || tx.send(2u32).unwrap())
            ),
            Ok(()),
            "the latch was still set after a worker drained an entry"
        );
        assert_eq!(rx.recv_timeout(T).unwrap(), 2);
        assert_eq!(pool.overflow_count(CallbackPriority::Low), 1);
        pool.shutdown();
    }

    /// Concurrent full pushes are one overflow *episode*, as C's lock made
    /// them: the latch gates the count, not just the rejection.
    #[test]
    fn concurrent_full_pushes_count_one_overflow() {
        let mut pool = CallbackPool::with_config(1, 1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();
        pool.request(CallbackPriority::Low, Box::new(|| {}))
            .unwrap();

        let h = pool.handle();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let h = h.clone();
                std::thread::spawn(move || {
                    for _ in 0..64 {
                        let _ = h.request(CallbackPriority::Low, Box::new(|| {}));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(
            pool.overflow_count(CallbackPriority::Low),
            1,
            "512 rejected pushes against one latched ring counted more than one episode"
        );
        gate_tx.send(()).unwrap();
        pool.shutdown();
    }

    /// `numUsed` counts entries that have not run, not entries still in the
    /// inbox: a batch a worker has taken is still in C's ring until each
    /// entry runs.
    #[test]
    fn num_used_counts_the_entries_that_have_not_run_yet() {
        let mut pool = CallbackPool::with_config(10, 1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();

        for _ in 0..6 {
            pool.request(CallbackPriority::Medium, Box::new(|| {}))
                .unwrap();
        }
        let st = pool.stats(CallbackPriority::Medium, false);
        assert_eq!(st.size, 10);
        assert_eq!(
            st.num_used, 6,
            "queued-but-not-run entries are the ring's use"
        );
        assert_eq!(st.max_used, 6);

        // The seventh has to be claimed while the band is still gated, or the
        // worker may already have drained some of the six and the mark would
        // never reach seven.
        let (tx, rx) = mpsc::channel();
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || tx.send(()).unwrap()),
        )
        .unwrap();
        assert_eq!(pool.stats(CallbackPriority::Medium, false).num_used, 7);

        // One worker, so FIFO makes the seventh the last to run and its own
        // slot the last to be released.
        gate_tx.send(()).unwrap();
        rx.recv_timeout(T).unwrap();
        let st = pool.stats(CallbackPriority::Medium, true);
        assert_eq!(st.num_used, 0, "every entry ran, so the ring is empty");
        assert_eq!(
            st.max_used, 7,
            "the mark holds the deepest the ring ever was"
        );
        assert_eq!(
            pool.stats(CallbackPriority::Medium, false).max_used,
            0,
            "callbackQueueStatus(reset=1) must clear the high-water mark"
        );
        pool.shutdown();
    }

    /// FIFO, which the Treiber stack only gives back because `take_all`
    /// reverses the chain. 300 entries span both cases: the batch the busy
    /// worker's take picks up whole, and the pushes that arrive after it.
    #[test]
    fn a_single_worker_band_runs_entries_in_push_order() {
        let mut pool = CallbackPool::with_config(2000, 1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::High,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();

        let (tx, rx) = mpsc::channel();
        for i in 0..200u32 {
            let tx = tx.clone();
            pool.request(
                CallbackPriority::High,
                Box::new(move || tx.send(i).unwrap()),
            )
            .unwrap();
        }
        gate_tx.send(()).unwrap();
        for i in 200..300u32 {
            let tx = tx.clone();
            pool.request(
                CallbackPriority::High,
                Box::new(move || tx.send(i).unwrap()),
            )
            .unwrap();
        }
        drop(tx);
        let seen: Vec<u32> = rx.iter().take(300).collect();
        assert_eq!(
            seen,
            (0..300).collect::<Vec<u32>>(),
            "entries ran out of order"
        );
        pool.shutdown();
    }

    /// A band widened by `callbackParallelThreads` has a wake-up slot per
    /// worker, so every parked worker has to be reachable: with four workers
    /// and four pushers, no entry may be left in an inbox nobody reads.
    #[test]
    fn a_parallel_band_loses_no_entry_however_its_workers_are_parked() {
        let mut pool = CallbackPool::with_per_priority_config(4000, [4, 1, 1]);
        let h = pool.handle();
        let (tx, rx) = mpsc::channel();
        let pushers: Vec<_> = (0..4)
            .map(|_| {
                let h = h.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for _ in 0..250 {
                        let tx = tx.clone();
                        h.request(
                            CallbackPriority::Low,
                            Box::new(move || tx.send(()).unwrap()),
                        )
                        .expect("a 4000-slot ring takes 1000 entries");
                        std::thread::yield_now();
                    }
                })
            })
            .collect();
        for t in pushers {
            t.join().unwrap();
        }
        drop(tx);
        for i in 0..1000 {
            rx.recv_timeout(T)
                .unwrap_or_else(|e| panic!("only {i} of 1000 entries ran: {e}"));
        }
        assert_eq!(pool.stats(CallbackPriority::Low, false).num_used, 0);
        pool.shutdown();
    }

    /// A task entry takes no ring slot, so a latched ring must not refuse it
    /// — a refused wake would strand the task forever.
    #[test]
    fn a_latched_ring_still_takes_a_task_entry() {
        let mut pool = CallbackPool::with_config(1, 1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();
        pool.request(CallbackPriority::Low, Box::new(|| {}))
            .unwrap();
        assert_eq!(
            pool.request(CallbackPriority::Low, Box::new(|| {})),
            Err(CallbackError::QueueFull)
        );

        let (tx, rx) = mpsc::channel();
        pool.handle().schedule_task(
            CallbackPriority::Low,
            Box::new(move || tx.send(99u32).unwrap()),
        );
        gate_tx.send(()).unwrap();
        assert_eq!(
            rx.recv_timeout(T).unwrap(),
            99,
            "a task entry was lost on a band whose ring was full"
        );
        pool.shutdown();
    }

    /// A push that lost the shutdown race lands in an inbox no worker will
    /// read again; the band owns those nodes, so dropping it must free them.
    #[test]
    fn dropping_a_band_frees_what_is_left_in_its_inbox() {
        struct Tell(Arc<AtomicBool>);
        impl Drop for Tell {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let tell = Tell(Arc::clone(&dropped));
        let pq = PriorityQueue::new(4, 1);
        pq.push(Queued::Ring(Box::new(move || {
            let _ = &tell;
        })));
        assert!(!dropped.load(Ordering::SeqCst));
        drop(pq);
        assert!(
            dropped.load(Ordering::SeqCst),
            "a node left in the inbox leaked when its band was dropped"
        );
    }
}
