// RTEMS-EXEC-MODEL-ALLOW(1): the teardown test drives the scheduler from a
// tokio task (spawn/abort are its cancellation instrument), but the scan
// threads under test go through the exec seam (`block_on_sync` → `park_on`)
// when the feature is on — verified passing under --features rtems-exec-model.
use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::background::facility::{recover, run_isolated};
use crate::runtime::task::{StackSizeClass, ThreadPriority, enter_ioc_thread};
use crate::server::database::PvDatabase;
use crate::server::record::ScanType;

/// Scan scheduler that processes records at their configured scan rates.
pub struct ScanScheduler {
    db: Arc<PvDatabase>,
}

/// All periodic scan rates, **in C's menuScan order — slowest first**
/// ("10 second" through ".1 second" after the three non-periodic
/// choices). The index is load-bearing: C spawns `scan-%g` at
/// `epicsThreadPriorityScanLow + ind` (`dbScan.c:949`), so faster
/// rates preempt slower ones. Keep slowest-first or the priority
/// ladder inverts.
///
/// `pub(crate)`: the scanOnce worker's priority is defined by C as
/// `epicsThreadPriorityScanLow + nPeriodic` (`dbScan.c:776`), so
/// `runtime::background::scan_once` needs this slice's length — one
/// source of truth for "how many periodic rates exist".
pub(crate) const PERIODIC_SCANS: &[ScanType] = &[
    ScanType::Sec10,
    ScanType::Sec5,
    ScanType::Sec2,
    ScanType::Sec1,
    ScanType::Sec05,
    ScanType::Sec02,
    ScanType::Sec01,
];

/// What the periodic scan facility calls itself when reporting.
const FACILITY: &str = "periodic scan";

/// Band for the `ind`-th periodic rate — `dbScan.c:949`,
/// `opts.priority = epicsThreadPriorityScanLow + ind`. With
/// [`PERIODIC_SCANS`] slowest-first this is scan-10 → 60 up to
/// scan-0.1 → 66, the ladder the C IOC measures on RTEMS 6
/// (`doc/upstream-rtems-bugs/measurement-c-thread-priority-on-rtems-6.md`).
fn periodic_priority(ind: usize) -> ThreadPriority {
    ThreadPriority::Custom(ThreadPriority::ScanLow.value() + ind as u8)
}

/// C names the thread `scan-%g` of the period in seconds
/// (`dbScan.c:954`): `scan-10`, `scan-5`, … `scan-0.5`, `scan-0.1`.
/// Rust's shortest-roundtrip `f64` Display reproduces `%g` for every
/// menuScan period.
fn periodic_thread_name(period: Duration) -> String {
    format!("scan-{}", period.as_secs_f64())
}

/// Shutdown signal shared by the periodic scan threads.
///
/// The single owner of the stop transition is [`ScanStopGuard`], held
/// by the `run_with_hooks` future: dropping that future (tokio
/// cancellation, runtime teardown) trips the flag and wakes every
/// sleeper, preserving the teardown contract the previous
/// `JoinSet`-abort implementation provided. No other path may set the
/// flag.
struct ScanStop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

/// RAII owner of the stop transition — see [`ScanStop`].
struct ScanStopGuard(Arc<ScanStop>);

impl Drop for ScanStopGuard {
    fn drop(&mut self) {
        *recover(FACILITY, self.0.stopped.lock()) = true;
        self.0.wake.notify_all();
    }
}

/// How a periodic scan thread drives one tick's async record
/// processing on its own (banded) thread.
///
/// Hosted: [`tokio::runtime::Handle::block_on`], the handle captured
/// from the async context that called `run_with_hooks` — record
/// processing may spawn tasks and start timers, which need a runtime
/// context a plain `std` thread does not otherwise have. RTEMS:
/// `block_on_sync` → `park_on`, the same seam every blocking CA/PVA
/// connection thread already drives record processing through;
/// `runtime::task::spawn`/`sleep` route to the background executor
/// there. Either way the *processing itself* runs on this thread, so
/// the thread's EPICS band applies to the work — the point of having
/// dedicated scan threads at all.
#[derive(Clone)]
struct TickDriver {
    #[cfg(tokio_backend)]
    handle: tokio::runtime::Handle,
}

impl TickDriver {
    /// Capture from the current async context. Hosted callers reach
    /// `run_with_hooks` inside a tokio runtime (the previous
    /// `JoinSet::spawn` implementation already required exactly that).
    fn capture() -> Self {
        Self {
            #[cfg(tokio_backend)]
            handle: tokio::runtime::Handle::try_current()
                .expect("ScanScheduler::run must be called inside a tokio runtime"),
        }
    }

    fn drive<F: Future>(&self, fut: F) -> F::Output {
        #[cfg(tokio_backend)]
        {
            self.handle.block_on(fut)
        }
        #[cfg(exec_backend)]
        {
            match crate::runtime::task::block_on_sync(fut) {
                Ok(out) => out,
                // Both `NotBlockable` variants name a thread this is not: a
                // current-thread tokio runtime's own thread, or a
                // background-facility worker. A periodic scan thread is
                // neither — this module just spawned it with
                // `std::thread::Builder` and it runs no facility loop.
                Err(e) => unreachable!("a periodic scan thread is blockable: {e}"),
            }
        }
    }
}

/// One periodic rate's thread body — C `periodicTask`
/// (`dbScan.c:895-935`): sleep to the next deadline, scan the list,
/// repeat until told to stop.
fn periodic_loop(
    db: Arc<PvDatabase>,
    scan_type: ScanType,
    period: Duration,
    stop: Arc<ScanStop>,
    driver: TickDriver,
) {
    let mut next = Instant::now() + period;
    loop {
        // Sleep until the deadline or the stop signal, whichever first.
        let mut stopped = recover(FACILITY, stop.stopped.lock());
        loop {
            if *stopped {
                return;
            }
            let now = Instant::now();
            if now >= next {
                break;
            }
            let (guard, _timeout) = recover(FACILITY, stop.wake.wait_timeout(stopped, next - now));
            stopped = guard;
        }
        drop(stopped);

        // A panicking record costs this tick, not the rate's thread —
        // the same isolation the scanOnce worker gives its tails.
        run_isolated(FACILITY, || {
            driver.drive(async {
                let names = db.records_for_scan(scan_type).await;
                for name in &names {
                    let mut visited = HashSet::new();
                    let _ = db.process_record_with_links(name, &mut visited, 0).await;
                }
            });
        });

        // Next deadline; on overrun skip missed ticks rather than
        // bursting catch-up ticks — C's `periodicTask` also computes
        // its next delay from "now" after an overlong scan.
        next += period;
        let now = Instant::now();
        if next <= now {
            next = now + period;
        }
    }
}

impl ScanScheduler {
    pub fn new(db: Arc<PvDatabase>) -> Self {
        Self { db }
    }

    /// Run all scan tasks. Also processes PINI records at startup.
    /// This function runs indefinitely.
    pub async fn run(&self) {
        self.run_with_hooks(Vec::new()).await;
    }

    /// Run all scan tasks with post-PINI hooks.
    ///
    /// After PINI records are processed, the hooks are invoked before
    /// periodic scan tasks begin. This ensures pollers start only after
    /// the initial record processing burst is complete.
    ///
    /// If another `ScanScheduler` has already started for the same DB (e.g.
    /// CA server already running when PVA server starts in a QSRV setup),
    /// this call still runs the provided hooks but does NOT spawn duplicate
    /// scan tasks. It then awaits forever so the caller's `tokio::select!`
    /// behaves as expected.
    pub async fn run_with_hooks(&self, hooks: Vec<Box<dyn FnOnce() + Send>>) {
        let is_first = self.db.try_claim_scan_start();

        if is_first {
            // C `initialProcess()` (iocInit.c:653-657) — the PINI=YES pass.
            self.db
                .pini_process(crate::server::record::PiniMode::Yes)
                .await;
            // Release non-owner schedulers so they can run their hooks now.
            self.db.mark_pini_done();
        } else {
            // Non-owner: wait for the owner to finish PINI before running hooks.
            // This preserves the "PINI before after-init hooks" contract.
            self.db.wait_for_pini().await;
        }

        // Run the caller's after-init hooks (protocol-specific, e.g. registering
        // PVA PVs after the DB is loaded). Always AFTER PINI is done.
        for hook in hooks {
            hook();
        }

        if !is_first {
            // Another ScanScheduler already owns the periodic tasks for this DB.
            // Avoid spawning duplicates; just park this future.
            std::future::pending::<()>().await;
            return;
        }

        // C `spawnPeriodic` (`dbScan.c:943-959`): one **dedicated,
        // banded thread per periodic rate**, `scan-%g` at
        // `ScanLow + ind` on an `epicsThreadStackBig` stack — not an
        // anonymous task on a shared pool. The band is the point: a
        // tokio task runs at whatever priority its worker happens to
        // have, so periodic scans were invisible to the scheduler (and
        // to the RTEMS task listing) while C's scan-10/scan-5/scan-1
        // each hold their own measured level. Dedicated threads also
        // make periodic scan *possible* on RTEMS, where there is no
        // tokio runtime for a `JoinSet` to spawn onto.
        let stop = Arc::new(ScanStop {
            stopped: Mutex::new(false),
            wake: Condvar::new(),
        });
        let guard = ScanStopGuard(Arc::clone(&stop));
        let driver = TickDriver::capture();
        for (ind, &scan_type) in PERIODIC_SCANS.iter().enumerate() {
            if let Some(period) = scan_type.interval() {
                let db = Arc::clone(&self.db);
                let stop = Arc::clone(&stop);
                let driver = driver.clone();
                std::thread::Builder::new()
                    .name(periodic_thread_name(period))
                    // dbScan.c:950 — `opts.stackSize = epicsThreadStackBig`.
                    .stack_size(StackSizeClass::Big.bytes())
                    .spawn(move || {
                        let _ = enter_ioc_thread(periodic_priority(ind));
                        periodic_loop(db, scan_type, period, stop, driver);
                    })
                    .expect("failed to spawn periodic scan thread");
            }
        }

        // The threads own the periodic work; this future only keeps the
        // stop guard alive. Cancelling it (tokio::select! or runtime
        // teardown) drops the guard, which trips the stop flag and wakes
        // every scan thread — a thread mid-tick finishes that tick, then
        // exits at the flag check.
        let _guard = guard;
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dbScan.c:949` — the rate→priority ladder, pinned to the values
    /// the C IOC measures on RTEMS 6 (`scan-10` 60 … `scan-0.1` 66).
    #[test]
    fn periodic_ladder_matches_dbscan() {
        let expected: &[(ScanType, u8, &str)] = &[
            (ScanType::Sec10, 60, "scan-10"),
            (ScanType::Sec5, 61, "scan-5"),
            (ScanType::Sec2, 62, "scan-2"),
            (ScanType::Sec1, 63, "scan-1"),
            (ScanType::Sec05, 64, "scan-0.5"),
            (ScanType::Sec02, 65, "scan-0.2"),
            (ScanType::Sec01, 66, "scan-0.1"),
        ];
        assert_eq!(PERIODIC_SCANS.len(), expected.len());
        for (ind, &(scan_type, prio, name)) in expected.iter().enumerate() {
            assert_eq!(PERIODIC_SCANS[ind], scan_type, "order is load-bearing");
            assert_eq!(periodic_priority(ind).value(), prio);
            let period = scan_type.interval().expect("periodic rate has a period");
            assert_eq!(periodic_thread_name(period), name);
        }
    }

    /// The whole ladder stays inside the scan band: above every CA
    /// server thread, below `ScanHigh` and the callback bands — the
    /// ordering `epicsThread.h:82-85` encodes.
    #[test]
    fn periodic_ladder_stays_inside_the_scan_band() {
        for ind in 0..PERIODIC_SCANS.len() {
            let v = periodic_priority(ind).value();
            assert!(v >= ThreadPriority::ScanLow.value());
            assert!(v < ThreadPriority::ScanHigh.value());
            assert!(v > ThreadPriority::CaServerHigh.value());
        }
    }

    /// Cancelling `run_with_hooks` must tear the scan threads down —
    /// the contract the previous `JoinSet` implementation provided via
    /// task abort. Observed through the `Arc<PvDatabase>` strong count:
    /// every scan thread holds a clone, so the count returns to the
    /// caller's own handles once the threads have exited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_the_scheduler_stops_the_scan_threads() {
        let db = Arc::new(PvDatabase::new());
        let scheduler = ScanScheduler::new(Arc::clone(&db));
        let task = tokio::spawn(async move { scheduler.run().await });

        // Wait until every rate's thread is up (7 clones + task's own).
        let deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&db) < 2 + PERIODIC_SCANS.len() {
            assert!(Instant::now() < deadline, "scan threads never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        task.abort();
        let _ = task.await;

        // Guard dropped → flag tripped → every thread wakes and exits.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&db) > 1 {
            assert!(
                Instant::now() < deadline,
                "scan threads still alive after cancellation: {} Arc holders",
                Arc::strong_count(&db)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
