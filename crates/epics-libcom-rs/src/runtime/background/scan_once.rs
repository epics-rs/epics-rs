//! `scanOnce` queue + worker — RTEMS-safe port of the `onceQ`/`scanOnce`/
//! `onceTask` machinery in `modules/database/src/ioc/db/dbScan.c`.
//!
//! # C parity
//!
//! C keeps a single bounded ring `onceQ` of `onceQueueSize == 1000` entries
//! (`dbScan.c:64-66`) drained by one dedicated `scanOnce` worker thread
//! (`dbScan.c:68`, `onceTaskId`). `scanOnce(prec)` / `scanOnceCallback`
//! (`dbScan.c:660-694`) pushes an entry and **returns immediately**
//! (`return !pushOK`, `dbScan.c:693`); the caller never blocks on record
//! processing. `onceTask` (`dbScan.c:696-726`) waits on `onceSem`, drains the
//! ring, and for each entry does `dbScanLock` / `dbProcess` / `dbScanUnlock`
//! (`dbScan.c:715-717`) plus an optional completion callback
//! (`dbScan.c:718-719`).
//!
//! The Rust port keeps that exact shape with **plain `std` threads +
//! `Mutex`/`Condvar`** and a boxed closure per entry (the closure carries the
//! "lock + process this record" tail the seam supplies later — this increment
//! does not touch `pv.rs`/`processing.rs`). No tokio-runtime dependency, so it
//! runs on RTEMS.
//!
//! ## Overflow hysteresis (`dbScan.c:672`, `:683-690`)
//!
//! A `static int newOverflow` latch makes C print the
//! `"scanOnce: Ring buffer overflow"` warning **once per overflow episode**:
//! the first full push logs and clears `newOverflow`; further full pushes only
//! bump `onceQOverruns` silently; the next *successful* push re-arms the latch.
//! We reproduce that latch exactly.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::facility::{recover, run_facility_loop, run_isolated};
use crate::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};

/// A queued "process this record" tail. C stores `{prec, cb, usr}`
/// (`dbScan.c:664-668`); the Rust port boxes a closure that already captures
/// the record handle and completion callback.
pub type OnceCallback = Box<dyn FnOnce() + Send + 'static>;

/// Default ring capacity — C `onceQueueSize` (`dbScan.c:64`).
pub const DEFAULT_ONCE_QUEUE_SIZE: usize = 1000;

/// The `scanOnce` ring was full — C returns non-zero (`!pushOK`,
/// `dbScan.c:693`) and drops the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOnceOverflow;

struct OnceState {
    queue: VecDeque<OnceCallback>,
    /// C `epicsRingBytesHighWaterMark(onceQ)` — the deepest the ring has
    /// been since the last reset. `scanOnceQueueShow` reports it and
    /// `scanOnceQueueStatus(reset=1)` clears it (`dbScan.c:734-751`), so
    /// it has to be latched on the push rather than derived later.
    high_water: usize,
    /// C `onceQOverruns` — lifetime overflow count (`dbScan.c:67`).
    overflows: u64,
    /// C `static int newOverflow` latch (`dbScan.c:672`): `true` means the
    /// next overflow should log.
    new_overflow: bool,
    shutdown: bool,
}

struct Inner {
    capacity: usize,
    state: Mutex<OnceState>,
    /// C `onceSem` (`dbScan.c`), the worker's wake-up event.
    wake: Condvar,
    /// The drain thread, started on the first request rather than at
    /// construction — see [`Inner::ensure_worker`].
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    /// Start the drain thread if it is not running yet.
    ///
    /// C creates `onceTask` in `scanInit` (`dbScan.c:771-780`), which
    /// `iocInit` calls AFTER `initPeriodic` has sized `nPeriodic` from the
    /// loaded `menuScan` — the thread's priority is
    /// `epicsThreadPriorityScanLow + nPeriodic` and cannot be chosen before
    /// the menu is known. The port's `BackgroundExecutor` is built long before
    /// any `.dbd` is loaded, so the thread is started at the first `scanOnce`
    /// request instead: still before any work can be lost, and by then the
    /// record system has frozen the menu and pushed the count down with
    /// [`set_periodic_scan_band_count`].
    fn ensure_worker(self: &Arc<Self>) {
        let mut worker = recover(FACILITY, self.worker.lock());
        if worker.is_some() {
            return;
        }
        let worker_inner = Arc::clone(self);
        // Losing this thread stops every FLNK/scanOnce tail in the IOC while
        // records still accept writes — the same silent half-IOC whether the
        // loss happens later (`run_facility_loop`) or at creation
        // (`MandatoryThread`).
        *worker = Some(
            MandatoryThread::new(
                // dbScan.c:779 — thread name "scanOnce".
                "scanOnce",
                // dbScan.c:772 — priority `epicsThreadPriorityScanLow +
                // nPeriodic`, so scanOnce preempts every periodic scan thread:
                // 60 + 7 = 67 with base's own menu. Measured on the C IOC on
                // RTEMS 6: scanOnce OSIPRI 67.
                scan_once_priority(),
                // dbScan.c:773 — `opts.stackSize = epicsThreadStackBig`.
                StackSizeClass::Big,
            )
            .spawn(move || {
                // C `onceTask` registers before it signals `startStopEvent`
                // and removes on the way out (`dbScan.c:698`, `:724`).
                // Unbounded: the loop's normal state is parked on `onceSem`
                // with nothing queued, which is not a fault and has no
                // deadline to miss.
                let _watched = crate::runtime::taskwd::taskwd_insert(
                    "scanOnce",
                    crate::runtime::taskwd::CheckIn::Unbounded,
                    None,
                );
                run_facility_loop(
                    FACILITY,
                    || once_loop(&worker_inner),
                    || recover(FACILITY, worker_inner.state.lock()).shutdown = true,
                );
            }),
        );
    }

    /// Port of `scanOnceCallback` (`dbScan.c:670-694`).
    fn scan_once(&self, cb: OnceCallback) -> Result<(), ScanOnceOverflow> {
        let mut st = recover(FACILITY, self.state.lock());
        if st.shutdown {
            // Worker stopped: C drops late scanOnce requests during shutdown
            // without surfacing an error (parity with `callbackStop` handling
            // of late requests). Drop `cb` (never processed) and report
            // success rather than a spurious overflow.
            drop(st);
            tracing::trace!(
                target: "epics_base_rs::runtime::scan_once",
                "scanOnce after shutdown dropped"
            );
            return Ok(());
        }
        let result = if st.queue.len() >= self.capacity {
            // dbScan.c:682-687 — ring full: log once per episode, then count.
            if st.new_overflow {
                tracing::warn!(
                    target: "epics_base_rs::runtime::scan_once",
                    "WARNING scanOnce: Ring buffer overflow"
                );
            }
            st.new_overflow = false; // dbScan.c:686
            st.overflows += 1; // dbScan.c:687
            Err(ScanOnceOverflow)
        } else {
            st.new_overflow = true; // dbScan.c:689 — re-arm on a good push.
            st.queue.push_back(cb);
            st.high_water = st.high_water.max(st.queue.len());
            Ok(())
        };
        drop(st);
        // dbScan.c:691 — `epicsEventSignal(onceSem)` is issued unconditionally,
        // outside the push success/failure branch.
        self.wake.notify_one();
        result
    }

    /// Port of `scanOnceQueueStatus` (`dbScan.c:734-751`).
    fn stats(&self, reset: bool) -> ScanOnceQueueStats {
        let mut st = recover(FACILITY, self.state.lock());
        let out = ScanOnceQueueStats {
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

/// What this facility is called when it has to report something about itself.
const FACILITY: &str = "scanOnce worker";

/// How many periodic scan rates the record system has when nobody has said
/// otherwise — the seven of base's own `menuScan.dbd`.
///
/// The real count is C's `nPeriodic`, and it is site data: `initPeriodic`
/// sizes it from the loaded `menuScan` (`dbScan.c:866`), so an IOC that ships
/// its own menu has as many rates as it declared. This crate is *below* the
/// record system and cannot read the menu itself, so the owner of the menu
/// pushes the count down with [`set_periodic_scan_band_count`] when it freezes
/// the table. Until then this is the answer, which is the right one for every
/// IOC that does not override the menu.
pub const DEFAULT_PERIODIC_SCAN_BAND_COUNT: usize = 7;

static PERIODIC_SCAN_BAND_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(DEFAULT_PERIODIC_SCAN_BAND_COUNT);

/// Tell the scanOnce facility how many periodic scan rates the loaded
/// `menuScan` has, so its worker lands one band above the fastest of them.
///
/// The single caller is the owner of the menu — `epics-base-rs`'s
/// `server::record::menu_scan`, at the moment it freezes the table, which is
/// C's `initPeriodic` moment. It has to be called before the worker thread
/// starts, and it is: the worker is not spawned until the first `scanOnce`
/// request, and C likewise creates its `onceTask` in `scanInit`, after
/// `initPeriodic` and before any record can call `scanOnce`.
pub fn set_periodic_scan_band_count(n: usize) {
    PERIODIC_SCAN_BAND_COUNT.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// The scanOnce worker's EPICS band — `epicsThreadPriorityScanLow +
/// nPeriodic` (`dbScan.c:772`). With base's own menu that is 60 + 7 = 67:
/// scanOnce preempts every periodic scan thread, as in C.
fn scan_once_priority() -> ThreadPriority {
    let n = PERIODIC_SCAN_BAND_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    ThreadPriority::Custom(ThreadPriority::ScanLow.value() + n.min(u8::MAX as usize) as u8)
}

/// Port of `onceTask` (`dbScan.c:696-726`): wait on the wake event, drain the
/// ring, run each queued tail.
fn once_loop(inner: &Inner) {
    loop {
        let mut st = recover(FACILITY, inner.state.lock());
        while st.queue.is_empty() && !st.shutdown {
            st = recover(FACILITY, inner.wake.wait(st));
        }
        if st.queue.is_empty() {
            return; // empty + shutdown
        }
        let cb = st.queue.pop_front().unwrap();
        drop(st);
        // dbScan.c:715-719 — the queued tail owns lock/dbProcess/unlock/cb.
        run_isolated(FACILITY, cb);
    }
}

/// Cheap, clonable submission side of a [`ScanOnceQueue`] — the seam route the
/// FLNK/scanOnce chain hands records into.
#[derive(Clone)]
pub struct ScanOnceHandle {
    inner: Arc<Inner>,
}

impl ScanOnceHandle {
    /// Enqueue `cb` for one-shot processing and return immediately. `Err` on a
    /// full ring (the request is dropped, as in C). Port of `scanOnce`
    /// (`dbScan.c:660`).
    pub fn scan_once(&self, cb: OnceCallback) -> Result<(), ScanOnceOverflow> {
        self.inner.ensure_worker();
        self.inner.scan_once(cb)
    }

    /// Lifetime overflow count — C `onceQOverruns` (`dbScan.c:67`).
    pub fn overflow_count(&self) -> u64 {
        recover(FACILITY, self.inner.state.lock()).overflows
    }

    /// C `scanOnceQueueStatus` (`dbScan.c:734-751`).
    pub fn stats(&self, reset: bool) -> ScanOnceQueueStats {
        self.inner.stats(reset)
    }
}

/// The `scanOnce` ring as `scanOnceQueueShow` prints it — C
/// `scanOnceQueueStats` (`dbScan.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOnceQueueStats {
    /// Ring capacity — C `stats.size`.
    pub size: usize,
    /// Entries queued right now — C `stats.numUsed`.
    pub num_used: usize,
    /// Deepest the ring has been since the last reset — C `stats.maxUsed`.
    pub max_used: usize,
    /// Lifetime overflow count — C `stats.numOverflow` (`onceQOverruns`).
    pub num_overflow: u64,
}

/// The capacity [`ScanOnceQueue::new`] will use — C's `onceQueueSize`
/// file-static (`dbScan.c:64`), which `scanOnceSetQueueSize` writes and
/// `initOnce` reads when it creates the ring (`dbScan.c:774`).
static CONFIGURED_ONCE_QUEUE_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_ONCE_QUEUE_SIZE);

/// C `scanOnceSetQueueSize` (`dbScan.c:728-732`), which is the bare
/// assignment `onceQueueSize = size` — it validates nothing and reports
/// nothing. Clamped to at least 1 here so the ring is always usable.
pub fn set_queue_size(size: usize) {
    CONFIGURED_ONCE_QUEUE_SIZE.store(size.max(1), Ordering::Relaxed);
}

/// The `scanOnce` facility: one bounded ring drained by one worker thread.
///
/// Dropping it stops and joins the worker.
pub struct ScanOnceQueue {
    inner: Arc<Inner>,
}

impl ScanOnceQueue {
    /// Build with the C default capacity (`onceQueueSize`, `dbScan.c:64`).
    pub fn new() -> Self {
        Self::with_capacity(CONFIGURED_ONCE_QUEUE_SIZE.load(Ordering::Relaxed))
    }

    /// Build with an explicit ring capacity (clamped to at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        let inner = Arc::new(Inner {
            capacity: capacity.max(1),
            state: Mutex::new(OnceState {
                queue: VecDeque::new(),
                high_water: 0,
                overflows: 0,
                new_overflow: true,
                shutdown: false,
            }),
            wake: Condvar::new(),
            worker: Mutex::new(None),
        });
        ScanOnceQueue { inner }
    }

    /// Create the drain thread now — C `initOnce` (`dbScan.c:768-780`), which
    /// `scanInit` calls once the `menuScan` count is known and before it
    /// spawns the periodic threads (`dbScan.c:201-205`).
    ///
    /// The worker otherwise appears at the first `scanOnce`, which is late
    /// enough that an IOC that has never run a one-shot has no `scanOnce`
    /// thread where C always does — visible in `taskwdShow`, and it defers
    /// C's fail-at-init contract for a thread that cannot be created. Calling
    /// this at the port's own `scanInit` is what removes the difference; it
    /// stays idempotent, so the lazy path is still the safety net for a
    /// `scanOnce` that arrives before any IOC init.
    pub fn start(&self) {
        self.inner.ensure_worker();
    }

    /// A cheap, clonable submission handle (see [`ScanOnceHandle`]).
    pub fn handle(&self) -> ScanOnceHandle {
        ScanOnceHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Enqueue `cb` for one-shot processing — convenience wrapper over
    /// [`ScanOnceHandle::scan_once`].
    pub fn scan_once(&self, cb: OnceCallback) -> Result<(), ScanOnceOverflow> {
        self.inner.ensure_worker();
        self.inner.scan_once(cb)
    }

    /// Lifetime overflow count — C `onceQOverruns` (`dbScan.c:67`).
    pub fn overflow_count(&self) -> u64 {
        recover(FACILITY, self.inner.state.lock()).overflows
    }

    /// C `scanOnceQueueStatus` (`dbScan.c:734-751`): sample the ring and,
    /// when `reset` is set, clear the high-water mark.
    pub fn stats(&self, reset: bool) -> ScanOnceQueueStats {
        self.inner.stats(reset)
    }
}

impl Default for ScanOnceQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ScanOnceQueue {
    fn drop(&mut self) {
        {
            let mut st = recover(FACILITY, self.inner.state.lock());
            st.shutdown = true;
        }
        self.inner.wake.notify_all();
        let worker = recover(FACILITY, self.inner.worker.lock()).take();
        if let Some(w) = worker {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const T: Duration = Duration::from_secs(5);

    /// `dbScan.c:772` — scanOnce runs at `ScanLow + nPeriodic`, above every
    /// periodic scan thread. Measured on the C IOC on RTEMS 6 as OSIPRI 67
    /// with base's own seven-rate menu, which is what an IOC that overrides
    /// nothing has.
    #[test]
    fn scan_once_band_is_scanlow_plus_n_periodic() {
        assert_eq!(
            scan_once_priority().value(),
            ThreadPriority::ScanLow.value() + DEFAULT_PERIODIC_SCAN_BAND_COUNT as u8
        );
        assert_eq!(scan_once_priority().value(), 67);
    }

    /// A site menu with a different number of rates moves the band with it —
    /// C computes `nPeriodic` from the loaded menu, so an IOC with ten rates
    /// runs scanOnce at 70, still one band above its fastest scan thread.
    #[test]
    fn a_site_menu_moves_the_band_with_its_rate_count() {
        set_periodic_scan_band_count(10);
        assert_eq!(scan_once_priority().value(), 70);
        set_periodic_scan_band_count(DEFAULT_PERIODIC_SCAN_BAND_COUNT);
        assert_eq!(scan_once_priority().value(), 67);
    }

    /// Boundary: a queued tail that panics. `dbProcess` runs inside it, so
    /// before this one bad record stopped every FLNK and every `scanOnce` in
    /// the IOC, with nothing said.
    #[test]
    fn a_panicking_tail_does_not_stop_the_worker() {
        let q = ScanOnceQueue::new();
        q.scan_once(Box::new(|| panic!("a scanOnce tail panicked")))
            .expect("enqueue the panicking tail");

        let (tx, rx) = mpsc::channel();
        q.scan_once(Box::new(move || tx.send(7u32).unwrap()))
            .expect("enqueue the next tail");
        assert_eq!(
            rx.recv_timeout(T).unwrap(),
            7,
            "the tail after a panicking one never ran: the worker died with it"
        );
    }

    #[test]
    fn enqueue_returns_immediately_and_worker_drains() {
        let q = ScanOnceQueue::new();
        let (tx, rx) = mpsc::channel();
        // Non-blocking by construction: this returns before the worker runs.
        q.scan_once(Box::new(move || tx.send(7u32).unwrap()))
            .unwrap();
        assert_eq!(rx.recv_timeout(T).unwrap(), 7);
    }

    #[test]
    fn overflow_latches_and_counts() {
        // Boundary: capacity-1 ring, worker pinned busy → second live entry
        // fills the ring, third overflows (dbScan.c:682).
        let q = ScanOnceQueue::with_capacity(1);
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();

        // Worker picks this up and blocks inside it; ring is empty again.
        q.scan_once(Box::new(move || {
            started_tx.send(()).unwrap();
            gate_rx.recv().unwrap();
        }))
        .unwrap();
        started_rx.recv_timeout(T).unwrap();

        // Fill the single ring slot (worker is busy).
        q.scan_once(Box::new(|| {})).unwrap();
        // Ring full → overflow, request dropped.
        assert_eq!(q.scan_once(Box::new(|| {})), Err(ScanOnceOverflow));
        assert_eq!(q.scan_once(Box::new(|| {})), Err(ScanOnceOverflow));
        assert_eq!(q.overflow_count(), 2);

        gate_tx.send(()).unwrap(); // release the worker so it can drain.
    }

    #[test]
    fn scan_once_after_shutdown_is_silent_noop() {
        // Boundary: a ScanOnceHandle that outlives the queue must get Ok(())
        // and the tail must never run — not a spurious overflow error.
        let q = ScanOnceQueue::new();
        let h = q.handle();
        drop(q); // sets shutdown, joins the worker.

        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        let res = h.scan_once(Box::new(move || r.store(true, Ordering::SeqCst)));
        assert_eq!(res, Ok(())); // silent no-op, not Err(ScanOnceOverflow).
        assert!(
            !ran.load(Ordering::SeqCst),
            "scanOnce tail ran after shutdown; it must be dropped, not processed"
        );
        assert_eq!(h.overflow_count(), 0); // a shutdown drop is not an overflow.
    }
}
