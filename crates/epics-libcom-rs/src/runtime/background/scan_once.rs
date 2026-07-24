//! `scanOnce` queue + worker — RTEMS-safe port of the `onceQ`/`scanOnce`/
//! `onceTask` machinery in `modules/database/src/ioc/db/dbScan.c`.
//!
//! # C parity
//!
//! C keeps a single bounded ring `onceQ` of `onceQueueSize == 1000` entries
//! (`dbScan.c:65-67`) drained by one dedicated `scanOnce` worker thread
//! (`dbScan.c:69`, `onceTaskId`). `scanOnce(prec)` / `scanOnceCallback`
//! (`dbScan.c:664-698`) pushes an entry and **returns immediately**
//! (`return !pushOK`, `dbScan.c:697`); the caller never blocks on record
//! processing. `onceTask` (`dbScan.c:700-730`) waits on `onceSem`, drains the
//! ring, and for each entry does `dbScanLock` / `dbProcess` / `dbScanUnlock`
//! (`dbScan.c:719-721`) plus an optional completion callback
//! (`dbScan.c:722-723`).
//!
//! The Rust port keeps that exact shape with **plain `std` threads +
//! `Mutex`/`Condvar`** and a boxed closure per entry (the closure carries the
//! "lock + process this record" tail the seam supplies later — this increment
//! does not touch `pv.rs`/`processing.rs`). No tokio-runtime dependency, so it
//! runs on RTEMS.
//!
//! ## Overflow hysteresis (`dbScan.c:676`, `:687-694`)
//!
//! A `static int newOverflow` latch makes C print the
//! `"scanOnce: Ring buffer overflow"` warning **once per overflow episode**:
//! the first full push logs and clears `newOverflow`; further full pushes only
//! bump `onceQOverruns` silently; the next *successful* push re-arms the latch.
//! We reproduce that latch exactly.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::facility::{recover, run_facility_loop, run_isolated};
use crate::runtime::task::{StackSizeClass, ThreadPriority, enter_ioc_thread};

/// A queued "process this record" tail. C stores `{prec, cb, usr}`
/// (`dbScan.c:668-672`); the Rust port boxes a closure that already captures
/// the record handle and completion callback.
pub type OnceCallback = Box<dyn FnOnce() + Send + 'static>;

/// Default ring capacity — C `onceQueueSize` (`dbScan.c:65`).
pub const DEFAULT_ONCE_QUEUE_SIZE: usize = 1000;

/// The `scanOnce` ring was full — C returns non-zero (`!pushOK`,
/// `dbScan.c:697`) and drops the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOnceOverflow;

struct OnceState {
    queue: VecDeque<OnceCallback>,
    /// C `onceQOverruns` — lifetime overflow count (`dbScan.c:68`).
    overflows: u64,
    /// C `static int newOverflow` latch (`dbScan.c:676`): `true` means the
    /// next overflow should log.
    new_overflow: bool,
    shutdown: bool,
}

struct Inner {
    capacity: usize,
    state: Mutex<OnceState>,
    /// C `onceSem` (`dbScan.c`), the worker's wake-up event.
    wake: Condvar,
}

impl Inner {
    /// Port of `scanOnceCallback` (`dbScan.c:674-698`).
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
            // dbScan.c:686-691 — ring full: log once per episode, then count.
            if st.new_overflow {
                tracing::warn!(
                    target: "epics_base_rs::runtime::scan_once",
                    "WARNING scanOnce: Ring buffer overflow"
                );
            }
            st.new_overflow = false; // dbScan.c:690
            st.overflows += 1; // dbScan.c:691
            Err(ScanOnceOverflow)
        } else {
            st.new_overflow = true; // dbScan.c:693 — re-arm on a good push.
            st.queue.push_back(cb);
            Ok(())
        };
        drop(st);
        // dbScan.c:695 — `epicsEventSignal(onceSem)` is issued unconditionally,
        // outside the push success/failure branch.
        self.wake.notify_one();
        result
    }
}

/// What this facility is called when it has to report something about itself.
const FACILITY: &str = "scanOnce worker";

/// How many periodic scan rates the record system defines — C's `nPeriodic`,
/// sized from menuScan at `scanInit` (`dbScan.c`).
///
/// Stated here rather than read from the record system because this crate is
/// *below* it: `epics-base-rs` owns `server::scan::PERIODIC_SCANS`, and a
/// dependency edge back up would be a cycle. The two are pinned together in
/// the direction that has one — `epics-base-rs`'s `server::scan` carries a
/// `const _: () = assert!(PERIODIC_SCANS.len() == PERIODIC_SCAN_BAND_COUNT)`,
/// so adding or removing a rate fails to compile rather than silently moving
/// one side of the band ladder.
pub const PERIODIC_SCAN_BAND_COUNT: usize = 7;

/// The scanOnce worker's EPICS band — `epicsThreadPriorityScanLow +
/// nPeriodic` (`dbScan.c:776`), where `nPeriodic` is the number of periodic
/// scan rates ([`PERIODIC_SCAN_BAND_COUNT`], pinned to
/// `server::scan::PERIODIC_SCANS.len()` so the two cannot drift apart).
/// 60 + 7 = 67: scanOnce preempts every periodic scan thread, as in C.
fn scan_once_priority() -> ThreadPriority {
    ThreadPriority::Custom(ThreadPriority::ScanLow.value() + PERIODIC_SCAN_BAND_COUNT as u8)
}

/// Port of `onceTask` (`dbScan.c:700-730`): wait on the wake event, drain the
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
        // dbScan.c:719-723 — the queued tail owns lock/dbProcess/unlock/cb.
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
    /// (`dbScan.c:664`).
    pub fn scan_once(&self, cb: OnceCallback) -> Result<(), ScanOnceOverflow> {
        self.inner.scan_once(cb)
    }

    /// Lifetime overflow count — C `onceQOverruns` (`dbScan.c:68`).
    pub fn overflow_count(&self) -> u64 {
        recover(FACILITY, self.inner.state.lock()).overflows
    }
}

/// The `scanOnce` facility: one bounded ring drained by one worker thread.
///
/// Dropping it stops and joins the worker.
pub struct ScanOnceQueue {
    inner: Arc<Inner>,
    worker: Option<JoinHandle<()>>,
}

impl ScanOnceQueue {
    /// Build with the C default capacity (`onceQueueSize`, `dbScan.c:65`).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_ONCE_QUEUE_SIZE)
    }

    /// Build with an explicit ring capacity (clamped to at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        let inner = Arc::new(Inner {
            capacity: capacity.max(1),
            state: Mutex::new(OnceState {
                queue: VecDeque::new(),
                overflows: 0,
                new_overflow: true,
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let worker_inner = Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            // dbScan.c:783 — thread name "scanOnce".
            .name("scanOnce".to_string())
            // dbScan.c:777 — `opts.stackSize = epicsThreadStackBig`.
            .stack_size(StackSizeClass::Big.bytes())
            .spawn(move || {
                // dbScan.c:776 — priority `epicsThreadPriorityScanLow + nPeriodic`.
                // `nPeriodic` is the number of periodic scan rates (menuScan's
                // seven periodic choices; `server::scan::PERIODIC_SCANS` here),
                // so scanOnce preempts every periodic scan thread: 60 + 7 = 67.
                // Measured on the C IOC on RTEMS 6: scanOnce OSIPRI 67
                // (doc/upstream-rtems-bugs/measurement-c-thread-priority-on-rtems-6.md).
                let _ = enter_ioc_thread(scan_once_priority());
                // Losing this thread stops every FLNK/scanOnce tail in the
                // IOC while records still accept writes.
                run_facility_loop(
                    FACILITY,
                    || once_loop(&worker_inner),
                    || recover(FACILITY, worker_inner.state.lock()).shutdown = true,
                );
            })
            .expect("failed to spawn scanOnce worker thread");
        ScanOnceQueue {
            inner,
            worker: Some(worker),
        }
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
        self.inner.scan_once(cb)
    }

    /// Lifetime overflow count — C `onceQOverruns` (`dbScan.c:68`).
    pub fn overflow_count(&self) -> u64 {
        recover(FACILITY, self.inner.state.lock()).overflows
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
        if let Some(w) = self.worker.take() {
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

    /// `dbScan.c:776` — scanOnce runs at `ScanLow + nPeriodic`, above every
    /// periodic scan thread. Measured on the C IOC on RTEMS 6 as OSIPRI 67;
    /// this pins ours to the same number and to the periodic-rate count, so
    /// adding or removing a rate moves both sides together.
    #[test]
    fn scan_once_band_is_scanlow_plus_n_periodic() {
        assert_eq!(
            scan_once_priority().value(),
            ThreadPriority::ScanLow.value() + PERIODIC_SCAN_BAND_COUNT as u8
        );
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
        // fills the ring, third overflows (dbScan.c:686).
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
