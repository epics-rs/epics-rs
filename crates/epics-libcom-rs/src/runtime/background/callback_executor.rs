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
//! This module keeps that structure but with **plain `std` threads and a
//! lock-free band queue** and boxed closures instead of C function pointers,
//! so it carries **no tokio-runtime dependency** and runs on RTEMS
//! (armv7-rtems-eabihf). The OS thread priority per band is applied
//! best-effort via the existing [`apply_to_current_thread`](crate::runtime::task::apply_to_current_thread) abstraction in
//! [`crate::runtime::task`] — this module does **not** duplicate that logic.
//!
//! ## No lock on the band (epics-base PR #996)
//!
//! A band is crossed by every priority in the IOC, so whatever guards it is a
//! priority-inversion site. C guards its ring with `epicsSpin`, which on Linux
//! is a priority-inheriting `pthread_mutex` whenever POSIX thread priority
//! scheduling is available (`osdSpin.c:126`); inheritance bounds the inversion
//! by boosting the holder. This port has nothing to boost on that path: a
//! `callbackRequest` is one CAS onto `callback_queue::BandQueue`'s inbox and
//! owns nothing, so a requester descheduled anywhere in it delays no one,
//! whatever its priority. Inside the band, one worker does briefly own the
//! root it is refilling — see that module for why that window is not an
//! inversion site, and for why the queue is addressed by arena index rather
//! than by pointer.
//!
//! C also signals the band's event on every push (`callback.c:375`) and
//! re-triggers it on every pop that leaves work behind (`callback.c:224`), so
//! a band pays a wake-up per entry whether or not a worker is actually asleep.
//! That is the other half of what epics-base PR #996 attacks, with a wake-up
//! token per worker. Here a push wakes a worker only while one is parked, and
//! the announce/poll pair that makes that safe is in
//! `callback_queue::Parking`.
//!
//! What PR #996 shows is worth taking is the take-all inbox; what it keeps
//! that this does not is the *private* batch. A batch that becomes the
//! property of the worker that took it strands its tail behind one callback
//! that blocks, even while other workers of the band sit idle — two callbacks
//! that have to meet then deadlock, which C's shared ring does not
//! (`a_blocked_callback_does_not_strand_its_neighbours`). The private batch
//! was implemented here, measured against that invariant, and withdrawn; the
//! band now takes the inbox all at once and publishes it to a stack every
//! worker pops from, so an entry becomes a worker's property only as that
//! worker takes it.
//!
//! ## A full band (`callback.c:874-877` as of epics-base PR #996)
//!
//! Whether a band is full is the slot supply's answer and nothing else's
//! (`callback_queue::Pool`): a request that gets no slot is refused, counted
//! against `queueOverflows` and named on the log, and the next request is
//! accepted the moment a slot comes back. There is no second cell recording
//! that the band *was* full.
//!
//! Older base keeps one, `cbQueueSet.queueOverflow`, and turns the next
//! `callbackRequest` away on it without attempting a push (`callback.c:365`
//! pre-#996), until a worker pops an entry and clears it (`:227`). That flag
//! is raised *after* the push it failed, so a worker draining the ring in
//! between clears it first and the raise lands on an empty ring — from then
//! on every `callbackRequest` is refused with every slot free, the only
//! writer of the clear is a pop, and a pop needs a push. The band never
//! recovers, and it goes quiet while dead, because the message sits past the
//! gate too. PR #996 drops the flag along with the ring; so does this.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;

use super::callback_queue::{BandQueue, Parking, ReadyCursor, Returns};
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
/// Note there is no `Shutdown` variant: a request arriving after
/// `callbackStop` is a silent no-op returning `Ok(())`, matching C, whose
/// ring outlives its workers until `callbackCleanup` (`callback.c:237-284`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackError {
    /// The band's ring was full — C `S_db_bufFull` (`callback.c:373`): the
    /// push found every slot of the ring taken.
    QueueFull,
    /// Before `callbackInit` or after `callbackCleanup` — C `S_db_notInit`
    /// (`callback.c:360-363`).
    NotInit,
}

/// C `cbState` (`callback.c:69-71`), with `callbackStop`'s drain as a state
/// of its own: the low bits of a band's [`PriorityQueue::word`], whose other
/// bits count the requests it was given. Only [`CallbackPool::init`],
/// [`CallbackPool::stop`] and [`CallbackPool::cleanup`] change it.
mod band_state {
    /// C `cbRun`.
    pub(super) const RUN: u64 = 0;
    /// Inside `callbackStop`: requests still run, until the band is found
    /// with all it was given run.
    pub(super) const STOPPING: u64 = 1;
    /// C `cbInit`: requests are refused with `S_db_notInit`.
    pub(super) const INIT: u64 = 2;
    /// C `cbStop`: a request is taken and never run.
    pub(super) const STOPPED: u64 = 3;
    /// The state's bits of the word.
    pub(super) const MASK: u64 = 3;
    /// The band's workers are told to exit: a request is dropped un-run.
    pub(super) const SHUTDOWN: u64 = 4;
    /// Every bit that refuses a request: `INIT`, `STOPPED` and `SHUTDOWN`,
    /// so admitting one is a single test of the word.
    pub(super) const REFUSE: u64 = 6;
    /// One request in the word's count.
    pub(super) const ONE: u64 = 8;
}

/// One worker's count of the requests it has run, on a line of its own so
/// the count costs its worker one store and nobody else anything.
#[repr(align(128))]
struct Finished(AtomicU64);

std::thread_local! {
    /// The band this thread works for, as its queue's address: the one band
    /// whose drain [`CallbackPool::stop`] cannot wait for from here.
    static WORKER_OF: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One FIFO entry of a band.
enum Queued {
    /// A `callbackRequest` — holds one of the ring's `capacity` slots.
    Ring(Callback),
    /// A spawned future's run-queue entry. A task has at most one at a time
    /// (its `SCHEDULED` state is the claim), so these are bounded by the live
    /// task count and take no ring slot: a wake that the ring could reject
    /// would strand a long-lived task forever.
    Task(TaskEntry),
    /// A [`CallbackCell`] request — holds a ring slot, as [`Queued::Ring`]
    /// does, but queues the requester's own cell instead of a closure.
    Cell(CellEntry),
}

/// What a [`CallbackCell`] runs — C `epicsCallback.callback` with its `user`
/// pointer already bound, since the body is the cell's own field.
pub trait CallbackBody: Send + Sync + 'static {
    /// One run of the request. The cell is idle again before this is called,
    /// so a request made from inside it queues the cell once more.
    fn run(&self);
}

/// A preallocated, re-armable callback request — C's `epicsCallback` in its
/// intrusive form (epics-base `proto/callback-intrusive`): the requester owns
/// it, a request queues the cell itself, and nothing is allocated per request.
///
/// A request while the cell is queued is taken by the run already pending and
/// returns `Ok`, as the prototype does (`callback.c:865-868`). This deviates
/// from released C, where an `epicsCallback` "may be queued multiple times"
/// (`callback.h:108`) and each request runs it once. The cell goes idle just
/// before its body runs (`:635-638`), so a request from the body, or one that
/// arrives while it runs, queues it again. Either way the run sees what the
/// requester wrote before requesting: the claim and the clear are both
/// read-modify-writes of one flag, so every request is ordered against the
/// clear.
pub struct CallbackCell<B> {
    queued: AtomicBool,
    body: B,
}

impl<B: CallbackBody> CallbackCell<B> {
    /// A cell running `body`, idle. Allocated here, once, by its owner.
    pub fn new(body: B) -> Arc<Self> {
        Arc::new(CallbackCell {
            queued: AtomicBool::new(false),
            body,
        })
    }

    /// The body, for the owner that keeps state in it.
    pub fn body(&self) -> &B {
        &self.body
    }

    /// Whether a run of this cell is pending on a band.
    pub fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Acquire)
    }
}

/// A cell as its band holds it, type-erased so every body shares one entry.
trait CellRun: Send + Sync {
    fn idle(&self);
    fn run_body(&self);
}

impl<B: CallbackBody> CellRun for CallbackCell<B> {
    fn idle(&self) {
        self.queued.swap(false, Ordering::AcqRel);
    }

    fn run_body(&self) {
        self.body.run();
    }
}

/// A queued cell. Dropped un-run — refused by a band that shut down under it,
/// or left in one that is torn down — it puts the cell back to idle, so a later
/// request is not taken by a run that will never come.
struct CellEntry(Option<Arc<dyn CellRun>>);

impl CellEntry {
    fn run(mut self) {
        if let Some(cell) = self.0.take() {
            cell.idle();
            cell.run_body();
        }
    }
}

impl Drop for CellEntry {
    fn drop(&mut self) {
        if let Some(cell) = self.0.take() {
            cell.idle();
        }
    }
}

/// A spawned task as its band holds it, so a wake queues the task's own `Arc`
/// rather than a closure it would have to allocate.
pub(super) trait Run: Send + Sync {
    /// Poll the task once.
    fn run(self: Arc<Self>);
    /// The entry was dropped without running: the band shut down under it.
    fn cancel_unrun(&self);
}

/// A task's run-queue entry. The task's state stays `SCHEDULED` while the
/// entry exists, so no later wake re-queues it: dropped un-run — queued past
/// shutdown, left in a band that is torn down, refused by an exhausted arena —
/// the entry cancels the task, or its handle would never resolve.
pub(super) struct TaskEntry(Option<Arc<dyn Run>>);

impl TaskEntry {
    pub(super) fn new(task: Arc<dyn Run>) -> Self {
        TaskEntry(Some(task))
    }

    fn run(mut self) {
        if let Some(task) = self.0.take() {
            task.run();
        }
    }
}

impl Drop for TaskEntry {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.cancel_unrun();
        }
    }
}

/// One priority band: a bounded lock-free FIFO plus the park slots its workers
/// sleep in. Mirrors C `cbQueueSet` (`callback.c:53-62`).
struct PriorityQueue {
    capacity: usize,
    queue: BandQueue<Queued>,
    /// The deepest the ring has been since the last reset — C
    /// `epicsRingPointerGetHighWaterMark`, which `callbackQueueShow` reports
    /// and `callbackQueueStatus(reset=1)` clears (`callback.c:115-139`). Not
    /// derivable after the fact, so it is latched on the pushes that deepen
    /// the ring; a push that does not deepen it only reads.
    high_water: AtomicUsize,
    /// C `cbQueueSet.queueOverflows` — lifetime overflow count
    /// (`callback.c:57`).
    overflows: AtomicU64,
    /// The band's [`band_state`] and shutdown bit and, above them, the requests taken onto the
    /// ring — counted before the push and given back when the push is
    /// refused or coalesced. One word, so no request is counted once the
    /// band is `STOPPED` and the band is `STOPPED` only with every counted
    /// request run.
    word: AtomicU64,
    /// What each worker has run of them; the count equal to their sum is a
    /// drained band.
    finished: Box<[Finished]>,
    /// C `cbQueueSet.semWakeUp` (`callback.c:54`), as one park slot per
    /// worker. Cache-line aligned, and kept out of `used`'s line — see
    /// `callback_queue::Parking`.
    parking: Parking,
}

impl PriorityQueue {
    fn new(capacity: usize, workers: usize, state: u64) -> Self {
        PriorityQueue {
            capacity,
            queue: BandQueue::with_capacity(capacity),
            high_water: AtomicUsize::new(0),
            overflows: AtomicU64::new(0),
            word: AtomicU64::new(state),
            finished: (0..workers).map(|_| Finished(AtomicU64::new(0))).collect(),
            parking: Parking::new(workers),
        }
    }

    /// The band's answer to a request: `Ok(true)` to push it, `Ok(false)`
    /// to take it and never run it (`cbStop`, or a band shut down), or C's
    /// "Callbacks not initialized" (`callback.c:360-363`). A pushed request
    /// is counted here.
    #[inline]
    fn admit(&self, name: &str) -> Result<bool, CallbackError> {
        // One RMW counts the request and reads the state it was counted in.
        // The count never carries into the low bits, and a refusal's passing
        // count cannot meet a seal: no band is sealed from `INIT`/`STOPPED`.
        let word = self.word.fetch_add(band_state::ONE, Ordering::AcqRel);
        if word & band_state::REFUSE == 0 {
            return Ok(true);
        }
        self.refuse(name, word)
    }

    /// [`Self::admit`]'s refusal, its count given back.
    #[cold]
    fn refuse(&self, name: &str, word: u64) -> Result<bool, CallbackError> {
        self.unadmit();
        if word & band_state::MASK == band_state::INIT {
            tracing::error!(
                target: "epics_base_rs::runtime::callback",
                band = name,
                "callbackRequest: ERROR Callbacks not initialized"
            );
            return Err(CallbackError::NotInit);
        }
        if word & band_state::SHUTDOWN != 0 {
            // Pool stopped: C drops late callbackRequests after callbackStop
            // without surfacing an error (`callback.c:237-284`). This also
            // absorbs the teardown race where the delayed timer fires into a
            // pool that has just been dropped.
            tracing::trace!(
                target: "epics_base_rs::runtime::callback",
                band = name,
                "callbackRequest after shutdown dropped"
            );
        }
        Ok(false)
    }

    /// The band's workers are told to exit.
    fn is_shut_down(&self, order: Ordering) -> bool {
        self.word.load(order) & band_state::SHUTDOWN != 0
    }

    /// Give back the count of a request [`Self::admit`] pushed and the
    /// ring did not keep.
    fn unadmit(&self) {
        self.word.fetch_sub(band_state::ONE, Ordering::AcqRel);
    }

    /// Move the band from state `from` to `to`, keeping its count; `false`
    /// when it was not in `from`.
    fn transition(&self, from: u64, to: u64) -> bool {
        let mut word = self.word.load(Ordering::Acquire);
        while word & band_state::MASK == from {
            match self.word.compare_exchange_weak(
                word,
                (word & !band_state::MASK) | to,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => word = now,
            }
        }
        false
    }

    #[cfg(test)]
    fn state(&self) -> u64 {
        self.word.load(Ordering::Acquire) & band_state::MASK
    }

    /// Worker `slot` ran one request.
    fn finish(&self, slot: usize) {
        let count = &self.finished[slot].0;
        count.store(count.load(Ordering::Relaxed) + 1, Ordering::Release);
    }

    /// `STOPPING` to `STOPPED`, if every request the band was given has
    /// run; true once the band is `STOPPED`, whoever sealed it. The finishes
    /// are read first: a request made from a callback is counted before that
    /// callback's finish, so it is seen with it; one counted after the read
    /// fails the exchange.
    fn seal(&self) -> bool {
        match self.word.load(Ordering::Acquire) & band_state::MASK {
            band_state::STOPPING => {}
            state => return state == band_state::STOPPED,
        }
        let finished: u64 = self
            .finished
            .iter()
            .map(|f| f.0.load(Ordering::Acquire))
            .sum();
        let draining = (finished * band_state::ONE) | band_state::STOPPING;
        let mut word = self.word.load(Ordering::Acquire);
        while word & !band_state::SHUTDOWN == draining {
            match self.word.compare_exchange_weak(
                word,
                (word & !band_state::MASK) | band_state::STOPPED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => word = now,
            }
        }
        word & band_state::MASK == band_state::STOPPED
    }

    /// Deepen the high-water mark if `depth` — the ring's depth as the push
    /// that just succeeded saw it — is the deepest yet.
    ///
    /// In the steady state one relaxed load: the mark only moves while the
    /// ring is reaching depths it has not reached since the last reset. The
    /// depth is the pusher's own, not a load, because by now the entry may
    /// already have run.
    fn deepen_high_water(&self, depth: usize) {
        if depth > self.high_water.load(Ordering::Relaxed) {
            self.high_water.fetch_max(depth, Ordering::AcqRel);
        }
    }

    /// Count the refused request and name it — C `callback.c:874-877` as of
    /// PR #996, which counts and prints once per refusal rather than once per
    /// episode. A band saturated for a second by a 1 kHz producer therefore
    /// reports a thousand, and says so a thousand times: the count is the
    /// requests that were lost, and no cell is kept to suppress the rest.
    fn report_full(&self, name: &str) -> CallbackError {
        self.overflows.fetch_add(1, Ordering::Relaxed);
        // `fullMessage[priority]`.
        tracing::error!(
            target: "epics_base_rs::runtime::callback",
            band = name,
            "callbackRequest: ERROR {} ring buffer full",
            name
        );
        CallbackError::QueueFull
    }

    /// Port of `callbackRequest` for a single band (`callback.c:341-377`).
    fn request(&self, name: &str, cb: Callback) -> Result<(), CallbackError> {
        if !self.admit(name)? {
            // Dropped here, deallocated and never invoked.
            return Ok(());
        }
        // callback.c:789-824 — the push that begins a batch recruits a worker
        // for it, and only that push can know (`BandQueue::publish`).
        let depth = match self
            .queue
            .push_ring(Queued::Ring(cb), || self.parking.wake_one())
        {
            Ok(depth) => depth,
            // No ring slot was free: the band is full, the one place that
            // decides it. Dropping the entry deallocates the callback that
            // was never queued.
            Err(entry) => {
                drop(entry);
                self.unadmit();
                return Err(self.report_full(name));
            }
        };
        self.deepen_high_water(depth);
        Ok(())
    }

    /// [`Self::request`] for a [`CallbackCell`]: the slot first, then the
    /// cell's claim, so a full band refuses either way and a cell already
    /// queued gives its slot back (`callback.c:855-868`, intrusive form).
    fn request_cell<B: CallbackBody>(
        &self,
        name: &str,
        cell: &Arc<CallbackCell<B>>,
    ) -> Result<(), CallbackError> {
        if !self.admit(name)? {
            return Ok(());
        }
        // A swap, not a compare-exchange: a coalesced request must still
        // write, so the run pending for it acquires what it published.
        let claim = || {
            (!cell.queued.swap(true, Ordering::AcqRel))
                .then(|| Queued::Cell(CellEntry(Some(Arc::clone(cell) as Arc<dyn CellRun>))))
        };
        match self
            .queue
            .push_ring_claimed(claim, || self.parking.wake_one())
        {
            Ok(Some(depth)) => self.deepen_high_water(depth),
            Ok(None) => self.unadmit(),
            Err(()) => {
                self.unadmit();
                return Err(self.report_full(name));
            }
        }
        Ok(())
    }

    /// Queue a spawned future's run-queue entry. Never refused for capacity —
    /// see [`Queued::Task`]. After shutdown `entry` is dropped un-run, which
    /// is how the task learns it was cancelled.
    fn schedule_task(&self, entry: TaskEntry) {
        if self.is_shut_down(Ordering::Acquire) {
            return;
        }
        if let Err(entry) = self
            .queue
            .push_task(Queued::Task(entry), || self.parking.wake_one())
        {
            // Only reachable with the whole 4 G index space queued; dropping
            // the entry finalizes its task rather than stranding it.
            drop(entry);
            tracing::error!(
                target: "epics_base_rs::runtime::callback",
                "callback band queue arena exhausted; task entry dropped"
            );
        }
    }

    /// C `callbackQueueStatus` for one band (`callback.c:115-139`):
    /// sample size/used/high-water/overflows, then reset the high-water mark
    /// when asked — in that order, as C does, so the row reports the mark the
    /// reset is about to drop.
    ///
    /// A reset leaves the mark at the entries still queued, not at zero:
    /// `epicsRingPointerResetHighWaterMark` is
    /// `highWaterMark = getUsedNoLock()` (`epicsRingPointer.h:339-343`). The
    /// mark is the deepest the ring has been *since the reset*, and the ring
    /// is already that deep at the moment the reset happens.
    fn stats(&self, reset: bool) -> CallbackQueueStats {
        // `used` and the mark are two words, so a row is sampled until they
        // agree — a push raises `used` before the mark and would otherwise be
        // caught between the two, reporting a mark below the depth. The retry
        // is on this side because it is `callbackQueueStatus`, run from iocsh,
        // and the alternative is a CAS loop on every request.
        let (used, mark) = loop {
            let used = self.queue.ring_used();
            let mark = self.high_water.load(Ordering::Acquire);
            if mark >= used {
                if !reset {
                    break (used, mark);
                }
                // The mark the reset drops is the one reported, and the one it
                // installs is the depth the ring is at right now
                // (`epicsRingPointer.h:339-343`).
                if self
                    .high_water
                    .compare_exchange_weak(mark, used, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    break (used, mark);
                }
            }
        };
        CallbackQueueStats {
            size: self.capacity,
            num_used: used,
            max_used: mark,
            num_overflow: self.overflows.load(Ordering::Relaxed),
        }
    }

    /// Lifetime overflow count — C `queueOverflows` (`callback.c:57`).
    fn overflow_count(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Every worker of this band is parked and the band holds nothing —
    /// queued, running, or run and not yet given back. C `testCallbackIdle`
    /// (`callback.c:996-1020`) asks the same of the worker states, `nAwake`,
    /// the sleeper bits and the in-use count; here a parked worker IS a
    /// sleeper and a running one is not, so the two loads answer all of it.
    ///
    /// Read in this order: the sleeper count last, so a worker that has just
    /// taken an entry out of the ring cannot be counted asleep against a ring
    /// reading that already excludes its entry.
    #[cfg(test)]
    fn is_idle(&self) -> bool {
        let empty = self.queue.is_empty() && self.queue.ring_used() == 0;
        empty && self.parking.sleepers() == self.parking.workers()
    }

    /// Stop the band and wake every worker so each re-tests its exit
    /// condition. Idempotent.
    fn request_shutdown(&self) {
        self.word.fetch_or(band_state::SHUTDOWN, Ordering::SeqCst);
        self.parking.wake_all();
    }
}

/// One band's row of C's `callbackQueueStats` (`callback.h`), as
/// `callbackQueueShow` prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackQueueStats {
    /// Ring capacity — C `stats.size`.
    pub size: usize,
    /// Entries **in use** right now — C `stats.numUsed`
    /// (`callback.h:78-81`): queued, running, or already run and not yet
    /// given back, because a worker of a multi-worker band returns its
    /// nodes in batches (`callback_queue::return_batch`). The
    /// band is full when all `size` of them are in use. A one-worker band
    /// — the `callbackParallelThreads` default — returns each node as it
    /// dequeues it, so there the count is exactly the entries queued.
    pub num_used: usize,
    /// Deepest the ring has been since the last reset — C `stats.maxUsed`.
    pub max_used: usize,
    /// Lifetime overflow count — C `stats.numOverflow`.
    pub num_overflow: u64,
}

/// What this facility is called when it has to report something about itself.
const FACILITY: &str = "callback band";

/// Run one worker of one band, with the band's own answer to whether its
/// workers chain the nodes they have run ([`BandQueue::returns`]) decided here
/// and nowhere else: the chain handle and the loop that fills it are picked
/// together, so a worker cannot run a loop that disagrees with its handle.
fn worker_loop(pq: &PriorityQueue, slot: usize) {
    // callback.c:563-570 — the nodes this worker has run and not yet given
    // back. Holding the handle is what guarantees they are given back.
    let returns = pq.queue.returns(pq.parking.workers());
    if returns.is_batched() {
        drain_band::<true>(pq, slot, returns);
    } else {
        drain_band::<false>(pq, slot, returns);
    }
}

/// C `callbackTask` (`callback.c:210-235`) for one worker of one band.
/// `slot` is the worker's ordinal within the band — its park slot.
///
/// `BATCHED` is the band's chain decision made constant, so the band that does
/// not chain pays nothing for the one that does: threading it through as a
/// runtime field instead costs a single worker's drain 11% (62.5 → 67.7 ns per
/// entry on this box), which is more than chaining ever saved it.
fn drain_band<const BATCHED: bool>(
    pq: &PriorityQueue,
    slot: usize,
    mut returns: Returns<'_, Queued>,
) {
    let parked = pq.parking.waiter(slot);
    // callback.c:537 — the root as this worker last left it.
    let mut at = ReadyCursor::new();
    loop {
        // callback.c:223 — take the next entry.
        let Some(popped) = pq.queue.pop_into::<BATCHED>(&mut returns, &mut at) else {
            // callback.c:220-221 — nothing to run: exit if the band has
            // stopped and is drained, otherwise sleep until a push arrives.
            // callback.c:574-581 — give the slots back before sleeping, so a
            // band that has caught up is holding none of its ring.
            returns.flush();
            // callback.c:217-226 — inside `callbackStop` a band is drained
            // when its worker finds it empty. Left to `stop`'s polling, a
            // steady requester could keep every sampled instant short of it.
            pq.seal();
            if pq.is_shut_down(Ordering::SeqCst) {
                return;
            }
            parked.park_until(|| !pq.queue.is_empty() || pq.is_shut_down(Ordering::SeqCst));
            // callback.c:607 — a sleep makes the carried root worthless: every
            // entry this worker could have taken from it was taken by whoever
            // emptied the band, so the next pop starts from a fresh read
            // rather than from a word whose compare-exchange can only fail.
            at = ReadyCursor::new();
            continue;
        };
        // callback.c:558-560 — a pop that leaves work behind wakes a sleeper,
        // so a second worker is not left asleep beside a queue that is not
        // empty. Recruiting the band is the workers' job, not the requester's
        // (see `Parking`), and this is where they do it. The answer comes out
        // of the pop's own CAS (`Popped::more`): reading the roots again here
        // instead costs a drain 7% at four workers and 11% at two.
        //
        // rt43 asks a wider question here, `sleepers && (more || !readyEmpty()
        // || inbox)`, because its requester declines the recruiting wake
        // whenever a worker looks ready (`Parking`). `more` already answers
        // its first two terms, from the CAS instead of a fresh load. Its third
        // is an entry pushed after this worker took the inbox — and an inbox
        // this worker emptied makes that push a batch starter, which recruits
        // on the requester's side whatever this worker does.
        if popped.more {
            pq.parking.wake_one();
        }
        // callback.c:228 — run the callback owning no band state.
        match popped.value {
            Queued::Ring(cb) => {
                run_isolated(FACILITY, cb);
                pq.finish(slot);
            }
            Queued::Task(entry) => {
                run_isolated(FACILITY, move || entry.run());
            }
            Queued::Cell(entry) => {
                run_isolated(FACILITY, move || entry.run());
                pq.finish(slot);
            }
        };
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

    /// Queue `cell` on `priority` — `callbackRequest` of a requester-owned
    /// `epicsCallback`. Allocates nothing. See [`CallbackCell`].
    pub fn request_cell<B: CallbackBody>(
        &self,
        priority: CallbackPriority,
        cell: &Arc<CallbackCell<B>>,
    ) -> Result<(), CallbackError> {
        self.queues[priority.index()].request_cell(priority.name_prefix(), cell)
    }

    /// Queue a spawned future's run-queue entry on `priority`. Unlike
    /// [`request`](Self::request) this cannot fail: the entry takes no ring
    /// slot. The caller guarantees at most one such entry per task.
    pub(super) fn schedule_task(&self, priority: CallbackPriority, entry: TaskEntry) {
        self.queues[priority.index()].schedule_task(entry);
    }

    /// Lifetime overflow count for a band — C `queueOverflows`
    /// (`callback.c:57`).
    pub fn overflow_count(&self, priority: CallbackPriority) -> u64 {
        self.queues[priority.index()].overflow_count()
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

#[cfg(test)]
impl CallbackPool {
    /// Every band of the pool is idle — see [`PriorityQueue::is_idle`].
    fn is_idle(&self) -> bool {
        self.queues.iter().all(|pq| pq.is_idle())
    }
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
        Self::build(queue_size, threads_per_priority, band_state::RUN)
    }

    /// [`Self::new`] for the IOC's own pool, whose bands wait at C `cbInit`
    /// for [`Self::init`]: constructing it is not `callbackInit`.
    pub(super) fn new_uninitialized() -> Self {
        Self::build(
            CONFIGURED_QUEUE_SIZE.load(Ordering::Relaxed),
            CallbackPriority::ALL.map(|p| CONFIGURED_THREADS[p.index()].load(Ordering::Relaxed)),
            band_state::INIT,
        )
    }

    fn build(
        queue_size: usize,
        threads_per_priority: [usize; NUM_CALLBACK_PRIORITIES],
        state: u64,
    ) -> Self {
        let capacity = queue_size.max(1);
        let threads_per_priority = threads_per_priority.map(|n| n.max(1));
        let queues: [Arc<PriorityQueue>; NUM_CALLBACK_PRIORITIES] =
            CallbackPriority::ALL.map(|p| {
                Arc::new(PriorityQueue::new(
                    capacity,
                    threads_per_priority[p.index()],
                    state,
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
                let (started, wait) = crate::runtime::sync::start_latch();
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
                    WORKER_OF.set(Arc::as_ptr(&pq) as usize);
                    drop(started);
                    run_facility_loop(FACILITY, || worker_loop(&pq, j), || pq.request_shutdown());
                });
                // callback.c:216, :333 — `callbackInit` waits on
                // `startStopEvent` until the thread has registered.
                wait.wait();
                workers.push(handle);
            }
        }

        CallbackPool { queues, workers }
    }

    /// C `callbackInit`'s state half (`callback.c:292-295`): `cbInit` to
    /// `cbRun`, or C's warning when the bands are already armed.
    pub fn init(&self) {
        if !self.arm() {
            eprintln!("Warning: callbackInit called again before callbackCleanup");
        }
    }

    /// `cbInit` to `cbRun`, or `false` when the bands were not in `cbInit`.
    pub fn arm(&self) -> bool {
        self.transition_all(band_state::INIT, band_state::RUN)
    }

    /// Every band from `from` to `to`, or none when the first is not in
    /// `from`: only these three pool methods move the bands, and always
    /// together.
    fn transition_all(&self, from: u64, to: u64) -> bool {
        let (first, rest) = self.queues.split_first().expect("three bands");
        if !first.transition(from, to) {
            return false;
        }
        for pq in rest {
            pq.transition(from, to);
        }
        true
    }

    /// C `callbackStop` (`callback.c:237-260`): from `cbRun` only, wait
    /// until every band has been found with all it was given run, a request
    /// made before then included, and refuse to run anything more. The
    /// workers stay up for the spawned tasks they also carry. A band this
    /// thread works for is not waited for: C's `callbackStop` would be
    /// joining itself.
    pub fn stop(&self) {
        if !self.transition_all(band_state::RUN, band_state::STOPPING) {
            return;
        }
        let own = WORKER_OF.get();
        for pq in &self.queues {
            if Arc::as_ptr(pq) as usize == own {
                pq.transition(band_state::STOPPING, band_state::STOPPED);
                continue;
            }
            while !pq.seal() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// C `callbackCleanup`'s state half (`callback.c:262-268`): `cbStop` back
    /// to `cbInit`, the only way the bands are armed again.
    pub fn cleanup(&self) {
        if !self.transition_all(band_state::STOPPED, band_state::INIT) {
            eprintln!("callbackCleanup() but not stopped");
        }
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

    /// See [`CallbackHandle::request_cell`].
    pub fn request_cell<B: CallbackBody>(
        &self,
        priority: CallbackPriority,
        cell: &Arc<CallbackCell<B>>,
    ) -> Result<(), CallbackError> {
        self.queues[priority.index()].request_cell(priority.name_prefix(), cell)
    }

    /// Lifetime overflow count for a band — C `queueOverflows`
    /// (`callback.c:57`).
    pub fn overflow_count(&self, priority: CallbackPriority) -> u64 {
        self.queues[priority.index()].overflow_count()
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
            pq.request_shutdown();
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
        // A server's executor is no `callback.c` band: always armed.
        let queue = Arc::new(PriorityQueue::new(
            CONFIGURED_QUEUE_SIZE.load(Ordering::Relaxed).max(1),
            threads,
            band_state::RUN,
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
                    run_facility_loop(FACILITY, || worker_loop(&pq, j), || pq.request_shutdown());
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
        self.queue.request_shutdown();
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

    /// A band state at C `cbRun`.
    fn armed() -> u64 {
        band_state::RUN
    }

    /// A closure as a task entry, for the band tests that only need one run.
    struct OnceRun(std::sync::Mutex<Option<Callback>>);

    impl OnceRun {
        fn entry(f: impl FnOnce() + Send + 'static) -> TaskEntry {
            TaskEntry::new(Arc::new(OnceRun(std::sync::Mutex::new(Some(Box::new(f))))))
        }
    }

    impl Run for OnceRun {
        fn run(self: Arc<Self>) {
            if let Some(f) = self.0.lock().unwrap().take() {
                f();
            }
        }

        fn cancel_unrun(&self) {}
    }
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// C `callbackInit` waits for each band thread's `taskwdInsert`
    /// (`callback.c:216`, `:333`), so a pool that exists has every band
    /// registered and none still starting up.
    #[test]
    fn every_band_is_registered_when_the_pool_is_built() {
        let _pool = CallbackPool::with_config(4, 1);
        let out = std::cell::RefCell::new(String::new());
        crate::runtime::taskwd::taskwd_show(1, &|line| {
            out.borrow_mut().push_str(line);
            out.borrow_mut().push('\n');
        });
        let out = out.into_inner();
        for prio in CallbackPriority::ALL {
            assert!(
                out.contains(prio.name_prefix()),
                "{} was not registered when the pool was built:\n{out}",
                prio.name_prefix()
            );
        }
    }

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
    fn a_full_ring_refuses_and_counts_every_request() {
        // Boundary: capacity-1 ring, worker pinned busy → the second live
        // entry fills the ring and every request after it is refused.
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
        // Every push now finds the ring full, and each one is its own loss.
        for _ in 0..2 {
            assert_eq!(
                pool.request(CallbackPriority::Low, Box::new(|| {})),
                Err(CallbackError::QueueFull)
            );
        }
        assert_eq!(pool.overflow_count(CallbackPriority::Low), 2);

        gate_tx.send(()).unwrap(); // release the worker so it drains.
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

    /// Waiter boundary: a worker is parked in `wait`, so the push has to
    /// signal. Four rounds, each starting from a band that has gone quiet.
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

    /// Waiter boundary: no worker is in `wait` — the only one is busy inside
    /// a callback — so the push signals nobody and the entry has to be picked
    /// up by the worker's own next look at the queue.
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

    /// One entry out of the ring is one slot back, and the next request takes
    /// it — the recovery half of
    /// [`a_full_ring_refuses_and_counts_every_request`].
    #[test]
    fn one_drained_entry_frees_one_slot() {
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
        // The worker returns the slot when it takes the entry out of the
        // ring, before it runs it, so this signal is proof the slot is back
        // and the next push has to be accepted.
        assert_eq!(ran_rx.recv_timeout(T).unwrap(), 1);
        let (tx, rx) = mpsc::channel();
        assert_eq!(
            pool.request(
                CallbackPriority::Low,
                Box::new(move || tx.send(2u32).unwrap())
            ),
            Ok(()),
            "a slot came back and the band still refused the request"
        );
        assert_eq!(rx.recv_timeout(T).unwrap(), 2);
        assert_eq!(pool.overflow_count(CallbackPriority::Low), 1);
        pool.shutdown();
    }

    /// Every refused request is counted, however many threads are refused at
    /// once — `queueOverflows` is the requests the band lost.
    #[test]
    fn every_refused_push_counts_its_own_overflow() {
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
            512,
            "every one of the 512 pushes was refused by a full ring"
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
            "callbackQueueStatus(reset=1) must put the mark back to the \
             entries still queued, and the ring is drained here"
        );
        pool.shutdown();
    }

    /// FIFO across 300 entries, pushed both while the band's worker is held
    /// and after it is released.
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

    /// A band widened by `callbackParallelThreads`: with four workers and
    /// four pushers, no entry may be left queued with nobody woken for it.
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
        // Four workers chain their nodes (`return_batch`), so the last entry
        // having run does not mean every node is back yet: each worker returns
        // its chain when it next finds the queue empty, just before parking.
        let gave_back = Instant::now();
        while pool.stats(CallbackPriority::Low, false).num_used != 0 {
            assert!(
                gave_back.elapsed() < T,
                "a parked worker is still holding nodes it ran"
            );
            std::thread::yield_now();
        }
        pool.shutdown();
    }

    /// The boundary the ring's accounting has to hold at: zero, with a worker
    /// consuming one entry while a requester takes the slot for the next.
    ///
    /// Counted from outside the band — raised after the push, lowered after
    /// the pop — the two are not ordered against each other, because the entry
    /// is a worker's to run from the moment it is linked: the decrement lands
    /// before its own increment and the count reads `usize::MAX`. That showed
    /// up as 5 failures in 25 runs of
    /// `a_parallel_band_loses_no_entry_however_its_workers_are_parked`, and
    /// `stats` would have spun forever on it rather than reported it. The
    /// count is the slot supply's own, so this holds by construction.
    /// Refusal follows the slots and nothing else: a band that has just
    /// refused a request takes the next one as soon as the ring drains, with
    /// no state left over from the refusal. Pre-#996 base keeps that state
    /// (`callback.c:365`) and a requester can raise it after the pop that
    /// would have cleared it, which shuts the band for the life of the IOC.
    #[test]
    fn a_drained_ring_takes_requests_again() {
        let pq = PriorityQueue::new(2, 1, armed());
        for _ in 0..2 {
            assert!(pq.request("cbLow", Box::new(|| {})).is_ok());
        }
        assert_eq!(
            pq.request("cbLow", Box::new(|| {})),
            Err(CallbackError::QueueFull),
            "a ring of two holds two"
        );

        while pq
            .queue
            .pop_into::<true>(&mut pq.queue.returns(1), &mut ReadyCursor::new())
            .is_some()
        {}
        assert_eq!(pq.queue.ring_used(), 0, "the ring drained");

        for _ in 0..2 {
            assert!(
                pq.request("cbLow", Box::new(|| {})).is_ok(),
                "every slot is free, so the band is not full"
            );
        }
        assert_eq!(pq.overflow_count(), 1, "one request was lost, and one only");
    }

    #[test]
    fn the_ring_count_never_reads_below_the_entries_queued() {
        const CAPACITY: usize = 4;
        const TOTAL: usize = 20_000;
        let pq = Arc::new(PriorityQueue::new(CAPACITY, 1, armed()));
        let claimed = Arc::new(AtomicUsize::new(0));
        let popped = Arc::new(AtomicUsize::new(0));
        // A broken count panics the thread that reads it, and the remaining
        // threads would then wait out the harness timeout for entries nobody
        // is pushing any more. Every loop watches for that, so a regression
        // reports in milliseconds.
        let broken = Arc::new(AtomicBool::new(false));
        let deep = |pq: &PriorityQueue, broken: &AtomicBool| {
            let used = pq.queue.ring_used();
            if used > CAPACITY {
                broken.store(true, Ordering::SeqCst);
                panic!("the ring of {CAPACITY} reported {used} entries queued");
            }
        };

        std::thread::scope(|s| {
            for _ in 0..2 {
                let (pq, claimed, broken) =
                    (Arc::clone(&pq), Arc::clone(&claimed), Arc::clone(&broken));
                s.spawn(move || {
                    // An entry is claimed before it is pushed, so exactly
                    // `TOTAL` reach the ring however the two pushers
                    // interleave. Counting pushes afterwards instead lets both
                    // read one short of `TOTAL`, push, and leave an entry
                    // behind that the poppers have already stopped counting.
                    while claimed.fetch_add(1, Ordering::Relaxed) < TOTAL {
                        while pq.request("cbLow", Box::new(|| {})).is_err() {
                            deep(&pq, &broken);
                            // A full ring is the poppers' turn: spinning on it
                            // instead starves them on an oversubscribed box.
                            std::thread::yield_now();
                        }
                        deep(&pq, &broken);
                        if broken.load(Ordering::SeqCst) {
                            return;
                        }
                    }
                });
            }
            for _ in 0..2 {
                let (pq, popped, broken) =
                    (Arc::clone(&pq), Arc::clone(&popped), Arc::clone(&broken));
                s.spawn(move || {
                    loop {
                        match pq
                            .queue
                            .pop_into::<true>(&mut pq.queue.returns(1), &mut ReadyCursor::new())
                        {
                            Some(took) => {
                                drop(took.value);
                                popped.fetch_add(1, Ordering::Relaxed);
                            }
                            // `TOTAL` pops can only have happened after
                            // `TOTAL` pushes, so nothing can still arrive.
                            None if popped.load(Ordering::Relaxed) >= TOTAL => return,
                            None if broken.load(Ordering::SeqCst) => return,
                            None => std::thread::yield_now(),
                        }
                        deep(&pq, &broken);
                    }
                });
            }
        });

        assert!(!broken.load(Ordering::SeqCst), "see the panic above");
        assert_eq!(
            pq.queue.ring_used(),
            0,
            "every entry ran and the ring still holds slots"
        );
    }

    /// Work queued behind a callback that blocks must run on another worker
    /// as soon as one is free — epics-base `callbackBlockedTest.c` (PR #996,
    /// `21a7f980e`). A worker that took a whole batch instead of one entry
    /// would hold the short callbacks behind the blocking one, and freeing a
    /// different worker would not release them.
    #[test]
    fn work_behind_a_blocked_callback_runs_on_a_freed_worker() {
        const NWORKERS: usize = 3;
        const NSHORT: usize = 20;
        let mut pool = CallbackPool::with_per_priority_config(64, [NWORKERS, 1, 1]);

        // C's `gate`: a callback that reports it started, then waits.
        let gate = |pool: &CallbackPool| {
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            pool.request(
                CallbackPriority::Low,
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }),
            )
            .unwrap();
            (started_rx, release_tx)
        };

        // Hold every worker inside a callback.
        let held: Vec<_> = (0..NWORKERS - 1)
            .map(|_| {
                let g = gate(&pool);
                g.0.recv_timeout(T).unwrap();
                g
            })
            .collect();
        let hold = gate(&pool);
        hold.0.recv_timeout(T).unwrap();

        // Nobody is free: a blocking callback and the short ones queue up, so
        // they land in one batch.
        let blocked = gate(&pool);
        let ran = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = mpsc::channel();
        for _ in 0..NSHORT {
            let (ran, done_tx) = (Arc::clone(&ran), done_tx.clone());
            pool.request(
                CallbackPriority::Low,
                Box::new(move || {
                    if ran.fetch_add(1, Ordering::SeqCst) + 1 == NSHORT {
                        done_tx.send(()).unwrap();
                    }
                }),
            )
            .unwrap();
        }

        // The released worker takes the blocking callback — it is the oldest
        // of the batch — and blocks in it.
        hold.1.send(()).unwrap();
        blocked.0.recv_timeout(T).unwrap();

        // Free one worker: it has to run the short ones while the worker that
        // took them out of the inbox stays blocked.
        held[0].1.send(()).unwrap();
        done_rx
            .recv_timeout(T)
            .expect("the short callbacks behind the blocked one never ran");
        assert_eq!(ran.load(Ordering::SeqCst), NSHORT);

        for h in &held[1..] {
            h.1.send(()).unwrap();
        }
        blocked.1.send(()).unwrap();
        pool.shutdown();
    }

    /// A task entry takes no ring slot, so a full ring must not refuse it:
    /// a refused wake would strand the task forever.
    #[test]
    fn a_full_ring_still_takes_a_task_entry() {
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
            OnceRun::entry(move || tx.send(99u32).unwrap()),
        );
        gate_tx.send(()).unwrap();
        assert_eq!(
            rx.recv_timeout(T).unwrap(),
            99,
            "a task entry was lost on a band whose ring was full"
        );
        pool.shutdown();
    }

    /// A band with two workers must let the second entry through while the
    /// first callback is still blocked inside its own. C keeps queued entries
    /// where every worker of the band can see them; a worker that claimed a
    /// whole batch for itself would strand the rest behind its own callback,
    /// and two callbacks that have to meet would deadlock.
    #[test]
    fn a_blocked_callback_does_not_strand_its_neighbours() {
        let mut pool = CallbackPool::with_per_priority_config(16, [2, 1, 1]);
        let (a_started_tx, a_started_rx) = mpsc::channel();
        let (a_release_tx, a_release_rx) = mpsc::channel::<()>();
        let (b_started_tx, b_started_rx) = mpsc::channel();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                a_started_tx.send(()).unwrap();
                // Bounded, so even a failing run joins its workers.
                let _ = a_release_rx.recv_timeout(T * 2);
            }),
        )
        .unwrap();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || b_started_tx.send(()).unwrap()),
        )
        .unwrap();

        a_started_rx
            .recv_timeout(T)
            .expect("the first callback never ran");
        let second = b_started_rx.recv_timeout(T);
        let _ = a_release_tx.send(());
        pool.shutdown();
        second.expect(
            "the second entry never ran while the first callback was blocked \
             and a worker of the band was idle",
        );
    }

    /// `used` and the high-water mark share one word, so the two boundaries of
    /// that word are worth separating: a reset must not disturb the count, and
    /// it must leave the mark at that count rather than at zero —
    /// `epicsRingPointerResetHighWaterMark` is `highWaterMark = used`
    /// (`epicsRingPointer.h:339-343`), and `callbackQueueStatus` is called on
    /// a live band, not a drained one.
    ///
    /// Tested on the band directly: a band with a worker has no state a test
    /// can hold still.
    #[test]
    fn a_high_water_reset_leaves_the_queued_entries_counted() {
        let pq = PriorityQueue::new(4, 1, armed());
        for _ in 0..3 {
            pq.request("cbLow", Box::new(|| {})).unwrap();
        }
        let st = pq.stats(false);
        assert_eq!((st.num_used, st.max_used), (3, 3));

        let st = pq.stats(true);
        assert_eq!(
            (st.num_used, st.max_used),
            (3, 3),
            "the reset sampled first"
        );
        let st = pq.stats(false);
        assert_eq!(st.num_used, 3, "the reset dropped the entries' count");
        assert_eq!(
            st.max_used, 3,
            "the reset put the mark below the entries already queued"
        );

        pq.request("cbLow", Box::new(|| {})).unwrap();
        let st = pq.stats(false);
        assert_eq!((st.num_used, st.max_used), (4, 4));
    }

    /// The ring's capacity boundary, on the band itself: `capacity` pushes go
    /// in and every one after that is refused by the slot supply and counted
    /// on its own.
    #[test]
    fn the_capacity_boundary_refuses_every_push_past_it() {
        let pq = PriorityQueue::new(2, 1, armed());
        assert!(pq.request("cbLow", Box::new(|| {})).is_ok());
        assert!(pq.request("cbLow", Box::new(|| {})).is_ok());
        assert_eq!(
            pq.request("cbLow", Box::new(|| {})),
            Err(CallbackError::QueueFull)
        );
        assert_eq!(
            pq.request("cbLow", Box::new(|| {})),
            Err(CallbackError::QueueFull)
        );
        assert_eq!(pq.overflow_count(), 2);
        assert_eq!(pq.stats(false).num_used, 2, "a refused push took no slot");
    }

    /// A task entry takes no ring slot, so a full band still accepts one —
    /// the property `Queued::Task` exists for. The band-level twin of
    /// `a_full_ring_still_takes_a_task_entry`, at the boundary where the ring
    /// is exactly full.
    #[test]
    fn a_full_band_still_takes_a_task_entry() {
        let pq = PriorityQueue::new(1, 1, armed());
        pq.request("cbLow", Box::new(|| {})).unwrap();
        assert_eq!(
            pq.request("cbLow", Box::new(|| {})),
            Err(CallbackError::QueueFull)
        );
        pq.schedule_task(OnceRun::entry(|| {}));
        assert_eq!(
            pq.stats(false).num_used,
            1,
            "the task entry was charged to the ring"
        );
    }

    /// An entry left queued when its band is torn down never ran, so it
    /// cancels its task.
    #[test]
    fn a_task_entry_dropped_with_its_band_cancels_the_task() {
        struct Flag(AtomicBool);
        impl Run for Flag {
            fn run(self: Arc<Self>) {}
            fn cancel_unrun(&self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let pq = PriorityQueue::new(1, 1, armed());
        pq.schedule_task(TaskEntry::new(Arc::clone(&flag) as Arc<dyn Run>));
        assert!(!flag.0.load(Ordering::SeqCst), "cancelled while queued");
        drop(pq);
        assert!(
            flag.0.load(Ordering::SeqCst),
            "the un-run entry was dropped silently"
        );
    }

    /// A cell body that counts its runs and reports each one.
    struct Count {
        runs: AtomicUsize,
        tx: std::sync::Mutex<mpsc::Sender<usize>>,
        /// Requests the body makes of its own cell, once each.
        requeue: std::sync::Mutex<Option<(CallbackHandle, Arc<CallbackCell<Count>>)>>,
    }

    impl CallbackBody for Count {
        fn run(&self) {
            let n = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((h, cell)) = self.requeue.lock().unwrap().take() {
                assert!(!cell.is_queued(), "the cell runs while still queued");
                h.request_cell(CallbackPriority::Low, &cell).unwrap();
            }
            self.tx.lock().unwrap().send(n).unwrap();
        }
    }

    fn count_cell() -> (Arc<CallbackCell<Count>>, mpsc::Receiver<usize>) {
        let (tx, rx) = mpsc::channel();
        let cell = CallbackCell::new(Count {
            runs: AtomicUsize::new(0),
            tx: std::sync::Mutex::new(tx),
            requeue: std::sync::Mutex::new(None),
        });
        (cell, rx)
    }

    /// Pin the band's only worker inside a callback until the sender drops.
    fn hold_worker(pool: &CallbackPool) -> mpsc::Sender<()> {
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                started_tx.send(()).unwrap();
                let _ = gate_rx.recv();
            }),
        )
        .unwrap();
        started_rx.recv_timeout(T).unwrap();
        gate_tx
    }

    /// Requests of a queued cell are taken by its pending run and give their
    /// slot back: one run, one slot used.
    #[test]
    fn a_queued_cell_takes_a_second_request_into_its_pending_run() {
        let mut pool = CallbackPool::with_config(4, 1);
        let gate = hold_worker(&pool);
        let (cell, rx) = count_cell();
        for _ in 0..3 {
            assert_eq!(pool.request_cell(CallbackPriority::Low, &cell), Ok(()));
        }
        assert!(cell.is_queued());
        assert_eq!(pool.stats(CallbackPriority::Low, false).num_used, 1);
        drop(gate);
        assert_eq!(rx.recv_timeout(T).unwrap(), 1);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(!cell.is_queued());
        pool.shutdown();
    }

    /// The cell is idle when its body runs, so the body's own request queues
    /// it once more, and a request after a run queues it again.
    #[test]
    fn a_cell_requested_from_its_own_run_runs_again() {
        let mut pool = CallbackPool::with_config(4, 1);
        let (cell, rx) = count_cell();
        *cell.body().requeue.lock().unwrap() = Some((pool.handle(), Arc::clone(&cell)));
        pool.request_cell(CallbackPriority::Low, &cell).unwrap();
        assert_eq!(rx.recv_timeout(T).unwrap(), 1);
        assert_eq!(rx.recv_timeout(T).unwrap(), 2);
        pool.request_cell(CallbackPriority::Low, &cell).unwrap();
        assert_eq!(rx.recv_timeout(T).unwrap(), 3);
        pool.shutdown();
    }

    /// The slot is taken before the claim: a full band refuses an idle cell
    /// and leaves it idle, and refuses a queued one too, counting both.
    #[test]
    fn a_full_band_refuses_a_cell_queued_or_not() {
        let mut pool = CallbackPool::with_config(1, 1);
        let gate = hold_worker(&pool);
        let (queued, rx) = count_cell();
        pool.request_cell(CallbackPriority::Low, &queued).unwrap();
        let (idle, idle_rx) = count_cell();
        assert_eq!(
            pool.request_cell(CallbackPriority::Low, &idle),
            Err(CallbackError::QueueFull)
        );
        assert!(!idle.is_queued(), "a refused cell stayed claimed");
        assert_eq!(
            pool.request_cell(CallbackPriority::Low, &queued),
            Err(CallbackError::QueueFull)
        );
        assert_eq!(pool.overflow_count(CallbackPriority::Low), 2);
        drop(gate);
        assert_eq!(rx.recv_timeout(T).unwrap(), 1);
        pool.request_cell(CallbackPriority::Low, &idle).unwrap();
        assert_eq!(idle_rx.recv_timeout(T).unwrap(), 1);
        pool.shutdown();
    }

    /// A cell left in a band that is torn down, or requested after shutdown,
    /// is idle afterwards and never runs.
    #[test]
    fn a_cell_dropped_with_its_band_is_idle_again() {
        let (cell, rx) = count_cell();
        let pq = PriorityQueue::new(1, 1, armed());
        pq.request_cell("test", &cell).unwrap();
        assert!(cell.is_queued());
        drop(pq);
        assert!(!cell.is_queued(), "the un-run entry left the cell claimed");

        let pool = CallbackPool::new();
        let h = pool.handle();
        drop(pool);
        assert_eq!(h.request_cell(CallbackPriority::Low, &cell), Ok(()));
        assert!(!cell.is_queued());
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    /// A body that panics leaves the band running and the cell requestable.
    #[test]
    fn a_panicking_cell_body_leaves_the_cell_idle() {
        struct Boom(AtomicUsize, std::sync::Mutex<mpsc::Sender<()>>);
        impl CallbackBody for Boom {
            fn run(&self) {
                let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
                self.1.lock().unwrap().send(()).unwrap();
                if first {
                    panic!("first run");
                }
            }
        }
        let mut pool = CallbackPool::with_config(4, 1);
        let (tx, rx) = mpsc::channel();
        let cell = CallbackCell::new(Boom(AtomicUsize::new(0), std::sync::Mutex::new(tx)));
        for _ in 0..2 {
            pool.request_cell(CallbackPriority::Low, &cell).unwrap();
            rx.recv_timeout(T).unwrap();
            let deadline = Instant::now() + T;
            while cell.is_queued() {
                assert!(Instant::now() < deadline, "the cell stayed queued");
                std::thread::yield_now();
            }
        }
        assert_eq!(cell.body().0.load(Ordering::SeqCst), 2);
        pool.shutdown();
    }

    /// A request taken by the pending run is served by it: the run sees what
    /// the requester wrote first. Probabilistic — store buffering loses one
    /// request in ~600k trials under a plain-store clear — and time-bounded.
    #[test]
    fn a_coalesced_request_is_seen_by_the_pending_run() {
        struct Latest {
            latest: AtomicUsize,
            seen: AtomicUsize,
        }
        impl CallbackBody for Latest {
            fn run(&self) {
                let v = self.latest.load(Ordering::Acquire);
                self.seen.fetch_max(v, Ordering::AcqRel);
            }
        }
        const N: usize = 50;
        let mut pool = CallbackPool::with_config(1024, 1);
        let budget = Instant::now() + Duration::from_secs(2);
        let (mut trials, mut lost) = (0usize, 0usize);
        while Instant::now() < budget {
            let cell = CallbackCell::new(Latest {
                latest: AtomicUsize::new(0),
                seen: AtomicUsize::new(0),
            });
            for i in 1..=N {
                cell.body().latest.store(i, Ordering::Release);
                while pool.request_cell(CallbackPriority::Low, &cell).is_err() {
                    std::thread::yield_now();
                }
            }
            let deadline = Instant::now() + T;
            while cell.body().seen.load(Ordering::Acquire) != N && Instant::now() < deadline {
                std::thread::yield_now();
            }
            lost += usize::from(cell.body().seen.load(Ordering::Acquire) != N);
            trials += 1;
        }
        pool.shutdown();
        assert_eq!(
            lost, 0,
            "{lost} of {trials} trials left the last request unserved"
        );
    }

    /// One slot of the stress test below: a callback identity the band is
    /// asked to run thousands of times from several threads at once.
    struct StressSlot {
        priority: CallbackPriority,
        /// Pushes the band ACCEPTED. Raised by whoever pushed, never by a
        /// worker, so a run can never inflate it.
        accepted: AtomicUsize,
        ran: AtomicUsize,
        /// Requeue myself from inside the callback while this is positive.
        chain: AtomicI64,
    }

    /// One push, counted only if the band took it; `Err` is a full ring, which
    /// the caller decides what to do about. The callback it queues is the
    /// stress load: it holds its worker now and then, and while `chain` lasts
    /// it queues itself again — so a worker is a requester too.
    fn stress_request(
        h: CallbackHandle,
        slots: Arc<Vec<StressSlot>>,
        j: usize,
    ) -> Result<(), CallbackError> {
        let inner = h.clone();
        let mine = Arc::clone(&slots);
        let priority = slots[j].priority;
        let cb = Box::new(move || {
            let again = {
                let s = &mine[j];
                // Hold this worker, so the rest of the batch has to find the
                // others — C's `slotCallback` sleeps on the same cadence.
                if s.ran.fetch_add(1, Ordering::AcqRel).is_multiple_of(128) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                s.chain.fetch_sub(1, Ordering::AcqRel) > 0
            };
            if again {
                // A refusal here loses a chain link, not an entry: nothing was
                // accepted, so nothing is owed a run.
                let _ = stress_request(inner, mine, j);
            }
        });
        h.request(priority, cb)?;
        slots[j].accepted.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// The whole band under the load it was built for, as C's
    /// `callbackStressTest` runs it (PR #996): four requester threads each
    /// pushing to six slots spread over the three priorities, four workers per
    /// band, callbacks that sleep inside so their worker is held, and
    /// callbacks that queue themselves again.
    ///
    /// Two properties, and they are the two a lost wake-up or a leaked slot
    /// breaks: **every accepted request runs exactly once**, and once the
    /// requesters are done **every band returns to idle and stays there**.
    /// Counted per slot, because a lost entry and a double run cancel in a
    /// sum.
    ///
    /// Three rounds, each starting from that idle state, so a round also
    /// covers the first request into a fully-asleep band — the one case where
    /// the requester's own wake is all that can start the work.
    ///
    /// A full ring is not what this measures: a refused push backs off and
    /// retries, and only an accepted one is owed a run.
    #[test]
    fn the_band_runs_every_accepted_request_once_and_returns_to_idle() {
        const WORKERS: usize = 4;
        const REQUESTERS: usize = 4;
        const SLOTS: usize = 6;
        const REQUESTS: usize = 4000;
        const CHAIN: i64 = 50;
        const ROUNDS: usize = 3;

        let pool = CallbackPool::with_per_priority_config(2048, [WORKERS; NUM_CALLBACK_PRIORITIES]);
        let h = pool.handle();
        let slots: Arc<Vec<StressSlot>> = Arc::new(
            (0..SLOTS)
                .map(|j| StressSlot {
                    priority: CallbackPriority::ALL[j % NUM_CALLBACK_PRIORITIES],
                    accepted: AtomicUsize::new(0),
                    ran: AtomicUsize::new(0),
                    chain: AtomicI64::new(0),
                })
                .collect(),
        );

        for round in 1..=ROUNDS {
            for s in slots.iter() {
                s.accepted.store(0, Ordering::Release);
                s.ran.store(0, Ordering::Release);
                s.chain.store(CHAIN, Ordering::Release);
            }

            let threads: Vec<_> = (0..REQUESTERS)
                .map(|t| {
                    let h = h.clone();
                    let slots = Arc::clone(&slots);
                    std::thread::spawn(move || {
                        let mut seed = t as u32 + 1;
                        for _ in 0..REQUESTS {
                            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                            let j = (seed >> 16) as usize % SLOTS;
                            while stress_request(h.clone(), Arc::clone(&slots), j).is_err() {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().expect("a requester thread");
            }

            let deadline = Instant::now() + T;
            while !pool.is_idle() {
                assert!(
                    Instant::now() < deadline,
                    "round {round}: the band never returned to idle"
                );
                std::thread::yield_now();
            }
            // Idle once is not idle: a worker could be between its park and
            // its next look at the queue. Nothing is pushing any more, so
            // this has to still hold.
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                pool.is_idle(),
                "round {round}: the band left idle with nothing to run"
            );

            for (j, s) in slots.iter().enumerate() {
                assert_eq!(
                    s.ran.load(Ordering::Acquire),
                    s.accepted.load(Ordering::Acquire),
                    "round {round} slot {j}: an accepted request did not run exactly once"
                );
            }
            for p in CallbackPriority::ALL {
                assert_eq!(
                    pool.stats(p, false).num_used,
                    0,
                    "round {round}: {p:?} still holds entries"
                );
            }
        }
    }

    /// A flag a callback sets when it runs.
    fn runs(flag: &Arc<AtomicBool>) -> Callback {
        let flag = flag.clone();
        Box::new(move || flag.store(true, Ordering::SeqCst))
    }

    /// C `callbackRequest` refuses with `S_db_notInit` before `callbackInit`
    /// (`callback.c:360-363`).
    #[test]
    fn a_request_before_init_is_refused() {
        let pool = CallbackPool::new_uninitialized();
        let ran = Arc::new(AtomicBool::new(false));
        assert_eq!(
            pool.request(CallbackPriority::Low, runs(&ran)),
            Err(CallbackError::NotInit)
        );
        pool.init();
        pool.request(CallbackPriority::Low, runs(&ran)).unwrap();
        pool.stop();
        assert!(ran.load(Ordering::SeqCst));
    }

    /// A second `callbackInit` warns and changes nothing (`callback.c:292-295`).
    #[test]
    fn a_second_init_leaves_the_bands_armed() {
        let pool = CallbackPool::new_uninitialized();
        pool.init();
        pool.init();
        assert!(pool.queues.iter().all(|pq| pq.state() == band_state::RUN));
        assert!(!pool.arm());
    }

    /// `callbackStop` returns once work queued before it has run: each
    /// `callbackTask` empties its ring before it sees the stop
    /// (`callback.c:210-235`).
    #[test]
    fn a_request_before_the_stop_runs_before_it_returns() {
        let pool = CallbackPool::with_config(8, 1);
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                std::thread::sleep(Duration::from_millis(50));
                flag.store(true, Ordering::SeqCst);
            }),
        )
        .unwrap();
        pool.stop();
        assert!(ran.load(Ordering::SeqCst));
    }

    /// A request a draining callback makes lands in the ring its worker is
    /// still emptying, so it runs before `callbackStop` returns.
    #[test]
    fn a_request_during_the_drain_runs_before_the_stop_returns() {
        let pool = CallbackPool::with_config(8, 1);
        let ran = Arc::new(AtomicBool::new(false));
        let (band, handle, inner) = (
            Arc::clone(&pool.queues[CallbackPriority::Medium.index()]),
            pool.handle(),
            runs(&ran),
        );
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || {
                while band.state() != band_state::STOPPING {
                    std::thread::yield_now();
                }
                handle.request(CallbackPriority::High, inner).unwrap();
            }),
        )
        .unwrap();
        pool.stop();
        assert!(ran.load(Ordering::SeqCst));
    }

    /// C `callbackTask` leaves its drain the first time it finds its band
    /// empty (`callback.c:217-226`), whatever another band is doing: a
    /// band is not held open to requests while `callbackStop` waits on
    /// another one.
    #[test]
    fn a_band_found_empty_is_sealed_while_the_stop_waits_on_another() {
        let pool = Arc::new(CallbackPool::with_config(8, 1));
        let (low_started_tx, low_started) = std::sync::mpsc::channel();
        let (release_low, low_held) = std::sync::mpsc::channel::<()>();
        pool.request(
            CallbackPriority::Low,
            Box::new(move || {
                low_started_tx.send(()).unwrap();
                let _ = low_held.recv();
            }),
        )
        .unwrap();
        low_started.recv_timeout(T).unwrap();
        let medium = Arc::clone(&pool.queues[CallbackPriority::Medium.index()]);
        let band = Arc::clone(&medium);
        pool.request(
            CallbackPriority::Medium,
            Box::new(move || {
                while band.state() != band_state::STOPPING {
                    std::thread::yield_now();
                }
            }),
        )
        .unwrap();
        let stopper = {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || pool.stop())
        };
        let deadline = std::time::Instant::now() + T;
        while medium.state() != band_state::STOPPED {
            assert!(
                std::time::Instant::now() < deadline,
                "Medium left open while the stop waits on Low"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        release_low.send(()).unwrap();
        stopper.join().unwrap();
    }

    /// `callbackStop` on a band whose workers were told to exit (the
    /// `run_facility_loop` exit path's `request_shutdown`) seals it on the
    /// count alone: the shutdown bit is not part of the drain test.
    #[test]
    fn a_stop_seals_a_shut_down_band() {
        let pool = CallbackPool::with_config(8, 1);
        let ran = Arc::new(AtomicBool::new(false));
        pool.request(CallbackPriority::Medium, runs(&ran)).unwrap();
        let band = Arc::clone(&pool.queues[CallbackPriority::Medium.index()]);
        let finished = || -> u64 {
            band.finished
                .iter()
                .map(|f| f.0.load(Ordering::Acquire))
                .sum()
        };
        while finished() != 1 {
            std::thread::yield_now();
        }
        band.request_shutdown();
        let (done, stopped) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            pool.stop();
            let _ = done.send(());
        });
        assert!(
            stopped.recv_timeout(Duration::from_secs(5)).is_ok(),
            "callbackStop spun on the shut-down band"
        );
        assert_eq!(band.state(), band_state::STOPPED);
    }

    /// After `callbackStop` a request is taken (`Ok`) and never run: C's
    /// ring outlives its workers until `callbackCleanup`.
    #[test]
    fn a_request_after_the_stop_is_taken_and_never_runs() {
        let pool = CallbackPool::with_config(8, 1);
        pool.stop();
        let ran = Arc::new(AtomicBool::new(false));
        assert_eq!(pool.request(CallbackPriority::Low, runs(&ran)), Ok(()));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!ran.load(Ordering::SeqCst));
    }

    /// `callbackCleanup` is the way back to `cbInit`, and the next
    /// `callbackInit` arms the bands for a second IOC.
    #[test]
    fn cleanup_rearms_the_bands_for_a_second_ioc() {
        let pool = CallbackPool::new_uninitialized();
        pool.init();
        pool.stop();
        pool.cleanup();
        let ran = Arc::new(AtomicBool::new(false));
        assert_eq!(
            pool.request(CallbackPriority::Low, runs(&ran)),
            Err(CallbackError::NotInit)
        );
        pool.init();
        pool.request(CallbackPriority::Low, runs(&ran)).unwrap();
        pool.stop();
        assert!(ran.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod rv_stop_race {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// RV: C callbackStop joins its workers, so no callback runs after it
    /// returns (callback.c:237-260).
    #[test]
    fn rv_nothing_runs_after_stop_returns() {
        let mut late_total = 0;
        for _ in 0..300 {
            let pool = Arc::new(CallbackPool::with_config(1024, 1));
            let stopped = Arc::new(AtomicBool::new(false));
            let late = Arc::new(AtomicUsize::new(0));
            let quit = Arc::new(AtomicBool::new(false));
            let hammer = {
                let (pool, stopped, late, quit) =
                    (pool.clone(), stopped.clone(), late.clone(), quit.clone());
                std::thread::spawn(move || {
                    while !quit.load(Ordering::SeqCst) {
                        let (s, l) = (stopped.clone(), late.clone());
                        let _ = pool.request(
                            CallbackPriority::Low,
                            Box::new(move || {
                                if s.load(Ordering::SeqCst) {
                                    l.fetch_add(1, Ordering::SeqCst);
                                }
                            }),
                        );
                    }
                })
            };
            std::thread::sleep(Duration::from_micros(200));
            pool.stop();
            stopped.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(2));
            quit.store(true, Ordering::SeqCst);
            hammer.join().unwrap();
            late_total += late.load(Ordering::SeqCst);
        }
        assert_eq!(late_total, 0, "callbacks ran after callbackStop returned");
    }
}
