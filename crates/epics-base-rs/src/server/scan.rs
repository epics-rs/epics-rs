// RTEMS-EXEC-MODEL-ALLOW(6): the teardown test drives the scheduler from a
// tokio task (spawn/abort are its cancellation instrument) and the five
// ScanOwner tests (drop-teardown, redundant-owner, PINI-skip, PINI-run,
// tick-runs-on-its-own-thread) use the tokio test runtime only as the
// start-context `ScanOwner::start` requires; the scan/owner threads under
// test go through the exec seam (`block_on_sync` → `park_on`) when the
// feature is on — all six verified passing under --features
// rtems-exec-model.
use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::background::facility::{recover, run_isolated};
use crate::runtime::task::{StackSizeClass, ThreadPriority, enter_ioc_thread};
use crate::server::database::PvDatabase;
use crate::server::record::ScanType;

/// Scan scheduler that processes records at their configured scan rates.
///
/// `pub(crate)` by design: scanning is owned by the IOC core, and the
/// only public way to start it is [`ScanOwner::start`] — a protocol
/// server (CA, PVA) cannot construct or drive a scheduler of its own,
/// which is what used to leave server-less targets with every periodic
/// `SCAN` field dead.
pub(crate) struct ScanScheduler {
    db: Arc<PvDatabase>,
}

/// All periodic scan rates, **in C's menuScan order — slowest first**
/// ("10 second" through ".1 second" after the three non-periodic
/// choices). The index is load-bearing: C spawns `scan-%g` at
/// `epicsThreadPriorityScanLow + ind` (`dbScan.c:949`), so faster
/// rates preempt slower ones. Keep slowest-first or the priority
/// ladder inverts.
///
/// The scanOnce worker's priority is defined by C as
/// `epicsThreadPriorityScanLow + nPeriodic` (`dbScan.c:776`), so
/// `runtime::background::scan_once` needs this slice's length too. It cannot
/// read it: the runtime layer is a crate *below* this one, and the edge back
/// up would be a cycle. So it states the count itself and the two are pinned
/// by the assertion below — one source of truth for "how many periodic rates
/// exist", enforced at compile time rather than by two comments agreeing.
pub(crate) const PERIODIC_SCANS: &[ScanType] = &[
    ScanType::Sec10,
    ScanType::Sec5,
    ScanType::Sec2,
    ScanType::Sec1,
    ScanType::Sec05,
    ScanType::Sec02,
    ScanType::Sec01,
];

const _: () = assert!(
    PERIODIC_SCANS.len() == crate::runtime::background::scan_once::PERIODIC_SCAN_BAND_COUNT,
    "adding or removing a periodic scan rate moves the scanOnce worker's band \
     (dbScan.c:776) — update PERIODIC_SCAN_BAND_COUNT in epics-libcom-rs to match"
);

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
/// by the `ScanScheduler::run` future: dropping that future (the
/// [`ScanOwner`] thread unblocking, tokio cancellation, runtime
/// teardown) trips the flag and wakes every sleeper, preserving the
/// teardown contract the previous `JoinSet`-abort implementation
/// provided. No other path may set the flag.
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
/// from the async context that called `run` — record
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
    /// `run` inside a tokio runtime (the previous `JoinSet::spawn`
    /// implementation already required exactly that).
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
    pub(crate) fn new(db: Arc<PvDatabase>) -> Self {
        Self { db }
    }

    /// Run the PINI=YES pass (unless the IOC init path already ran it —
    /// see the exactly-once gate below) and all periodic scan tasks.
    /// Never returns; dropping the future stops every scan thread (see
    /// [`ScanStopGuard`]).
    ///
    /// If another `ScanScheduler` has already started for the same DB
    /// (e.g. an IOC entry point and an embedded harness both starting a
    /// [`ScanOwner`]), this call parks as a non-owner and spawns no
    /// duplicate scan tasks.
    pub(crate) async fn run(&self) {
        let is_first = self.db.try_claim_scan_start();

        if !is_first {
            // Another ScanScheduler already owns the periodic tasks for this DB.
            // Avoid spawning duplicates; just park this future.
            std::future::pending::<()>().await;
            return;
        }

        // C `initialProcess()` (iocInit.c:653-657) — the PINI=YES pass.
        // Exactly once per database, as in C (initialProcess runs once,
        // inside iocBuild): when the IOC init path (`IocApplication::run`
        // Phase 2b.6) already ran it and published completion, skip the
        // re-run instead of re-processing every PINI record.
        if !self.db.pini_done() {
            self.db
                .pini_process(crate::server::record::PiniMode::Yes)
                .await;
        }
        // Publish completion — `PvDatabase::wait_for_pini` subscribers
        // (anything ordering itself "after PINI") unblock here.
        self.db.mark_pini_done();

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

/// Single owner of "this IOC scans": starts the periodic scan machinery
/// (and, when not already done by the IOC init path, the PINI=YES pass)
/// on a dedicated thread, independent of every network server.
///
/// C parity: `scanInit`/`scanRun` are owned by `iocInit`/`iocRun`
/// (`dbScan.c`, `iocInit.c`) — RSRV has no hand in scanning. The Rust
/// analog of that owner is here:
///
/// * [`crate::server::ioc_app::IocApplication::run`] starts one at the C
///   `scanRun` point (after the PINI=RUN pass, before
///   `initHookAfterDatabaseRunning`), so every `IocApplication`-built IOC
///   scans no matter which protocol runner it hands off to.
/// * Entry-point binaries that assemble an IOC without `IocApplication`
///   (`softioc-rs`, `oracle-ioc`, `dual-ioc-rs`, `qsrv-rs`,
///   `rtems-ca-ioc`, `rtems-pva-ioc`) start one themselves, right where
///   their hand-rolled iocInit sequence ends.
///
/// Protocol servers must NOT start scanning — that was the defect this
/// type closes: the `ScanScheduler` used to be constructed and driven
/// only inside the CA/PVA server run loops, so a PVA-only RTEMS target
/// had every periodic `SCAN` field silently dead. Redundant starts stay
/// harmless by construction: `PvDatabase::try_claim_scan_start` makes any
/// second owner a parked non-owner, so an IOC plus an embedded harness
/// (or two servers on one database) never double-scan.
///
/// # Why a dedicated thread, not a spawned task
///
/// The owner future parks forever holding the [`ScanStopGuard`]. On the
/// exec backend (`rtems-exec-model` / RTEMS) a spawned task that returns
/// `Pending` with its waker registered nowhere has no strong holder — the
/// executor drops it (tokio keeps detached tasks alive), the guard drops,
/// and every scan thread exits within one tick. Measured on target:
/// probes reached the spawn point while the thread census showed zero
/// `scan-*` threads, with the handle both dropped and `mem::forget`-ed.
/// A thread keeps the future (and guard) alive on its own stack on both
/// backends. On the tokio backend the thread drives the future via the
/// handle captured at [`ScanOwner::start`] (so `start` must be called
/// inside a tokio runtime there); on the exec backend it drives it via
/// `block_on_sync` → `park_on`, the same seam every blocking CA/PVA
/// connection thread uses.
///
/// # Teardown
///
/// Dropping the handle wakes the owner thread, which drops the scheduler
/// future — tripping the stop flag through the [`ScanStopGuard`] — and
/// joins the owner thread (the `scan-%g` threads themselves exit within
/// one tick, unjoined, exactly as under the previous server-driven
/// cancellation).
pub struct ScanOwner {
    stop: Option<crate::runtime::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ScanOwner {
    /// Start the scan owner thread for `db`. See the type docs for who
    /// calls this and why redundant calls are harmless.
    pub fn start(db: Arc<PvDatabase>) -> Self {
        let (stop_tx, stop_rx) = crate::runtime::sync::oneshot::channel::<()>();
        // Captured on the caller's thread: the owner thread itself has no
        // runtime, and `TickDriver::capture` inside `run` needs one on
        // the tokio backend. Same contract `ScanScheduler::run` always
        // had ("must be called inside a tokio runtime"), surfaced at the
        // start call instead of inside the thread.
        #[cfg(tokio_backend)]
        let handle = tokio::runtime::Handle::try_current()
            .expect("ScanOwner::start on the tokio backend must be called inside a tokio runtime");
        let join = std::thread::Builder::new()
            .name("scan-owner".to_string())
            // The owner thread runs the PINI pass's record processing on
            // its own stack (the `scan-%g` threads it spawns carry Big
            // stacks of their own, dbScan.c:950). Medium is the proven
            // shape from the interim per-binary owner thread, measured on
            // the RTEMS target.
            .stack_size(StackSizeClass::Medium.bytes())
            .spawn(move || {
                // Below every scan band: the owner only parks after the
                // PINI pass; the ladder the `scan-%g` threads hold is the
                // measured one (`periodic_priority`).
                let _ = enter_ioc_thread(ThreadPriority::Low);
                let scheduler = ScanScheduler::new(db);
                let owner = async move {
                    tokio::select! {
                        _ = scheduler.run() => {}
                        _ = stop_rx => {}
                    }
                };
                #[cfg(tokio_backend)]
                handle.block_on(owner);
                #[cfg(exec_backend)]
                match crate::runtime::task::block_on_sync(owner) {
                    Ok(()) => {}
                    // Freshly spawned plain std thread: not a facility
                    // worker, no runtime entered — always blockable.
                    Err(e) => unreachable!("the scan-owner thread is a plain std thread: {e}"),
                }
            })
            .expect("failed to spawn the scan-owner thread");
        Self {
            stop: Some(stop_tx),
            join: Some(join),
        }
    }
}

impl Drop for ScanOwner {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            // Bounded: the send above wakes the parked owner future, the
            // thread drops the scheduler (tripping the stop flag) and
            // returns without waiting on the scan threads.
            let _ = join.join();
        }
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

    /// Cancelling `run` must tear the scan threads down —
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

    /// `doc/pi-lock-evaluation.md` §6 step 2 — the periodic scan's *record
    /// processing* must run on the rate's own banded `scan-%g` thread, not
    /// on a shared tokio worker. Under the previous `JoinSet::spawn` shape
    /// each tick body ran on whatever pool worker picked the task up, so
    /// the `ScanLow + ind` ladder applied to nothing that did work: the
    /// scan inherited the pool's scheduling class (measured tail in
    /// `doc/rtlinux-rt-measurement.md` §2). Pinning the *executing* thread
    /// is what makes the band load-bearing; asserting the thread merely
    /// exists (`periodic_ladder_matches_dbscan`) does not.
    ///
    /// Observed from inside `Record::process`, which the framework calls
    /// synchronously on whichever thread drives the tick's future.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_periodic_tick_processes_on_its_own_banded_scan_thread() {
        use crate::error::CaResult;
        use crate::server::record::{FieldDesc, ProcessOutcome, Record};
        use crate::types::EpicsValue;

        /// Records the name of the thread its `process()` ran on.
        struct ThreadProbe(Arc<Mutex<Option<String>>>);

        impl Record for ThreadProbe {
            fn record_type(&self) -> &'static str {
                "scan_thread_probe"
            }
            fn process(&mut self) -> CaResult<ProcessOutcome> {
                let name = std::thread::current().name().map(str::to_string);
                *self.0.lock().expect("probe mutex") = name;
                Ok(ProcessOutcome::complete())
            }
            fn get_field(&self, name: &str) -> Option<EpicsValue> {
                match name {
                    "VAL" => Some(EpicsValue::Double(0.0)),
                    _ => None,
                }
            }
            fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
                Ok(())
            }
            fn declared_fields(&self) -> &'static [FieldDesc] {
                &[]
            }
        }

        let seen = Arc::new(Mutex::new(None::<String>));
        let db = Arc::new(PvDatabase::new());
        db.add_record("SCAN:THREAD", Box::new(ThreadProbe(Arc::clone(&seen))))
            .await
            .unwrap();
        // The fastest rate, so one tick lands in ~100 ms.
        {
            let rec = db.get_record("SCAN:THREAD").unwrap();
            rec.write().common.scan = ScanType::Sec01;
        }
        db.update_scan_index("SCAN:THREAD", ScanType::Passive, ScanType::Sec01, 0, 0);

        let owner = ScanOwner::start(Arc::clone(&db));

        let deadline = Instant::now() + Duration::from_secs(10);
        let name = loop {
            if let Some(n) = seen.lock().expect("probe mutex").clone() {
                break n;
            }
            assert!(Instant::now() < deadline, "the record was never scanned");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        drop(owner);

        let expected = periodic_thread_name(ScanType::Sec01.interval().unwrap());
        assert_eq!(
            name, expected,
            "the .1 second tick processed on `{name}`, not on its own \
             banded `{expected}` thread — periodic scan is back on a \
             shared pool"
        );
    }

    /// Wait until `db`'s strong count satisfies `pred`, or panic after 10s.
    async fn wait_for_count(db: &Arc<PvDatabase>, what: &str, pred: impl Fn(usize) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pred(Arc::strong_count(db)) {
            assert!(
                Instant::now() < deadline,
                "{what}: {} Arc holders",
                Arc::strong_count(db)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The core-owned start: `ScanOwner::start` brings every rate's
    /// thread up, and dropping the handle tears them all down — the same
    /// teardown contract the server-driven `tokio::select!` cancellation
    /// used to provide.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_scan_owner_stops_the_scan_threads() {
        let db = Arc::new(PvDatabase::new());
        let owner = ScanOwner::start(Arc::clone(&db));

        // Test handle + scheduler (owner thread) + one clone per rate.
        wait_for_count(&db, "scan threads never started", |n| {
            n >= 2 + PERIODIC_SCANS.len()
        })
        .await;

        drop(owner);
        wait_for_count(&db, "scan threads still alive after ScanOwner drop", |n| {
            n == 1
        })
        .await;
    }

    /// PINI exactly-once boundary: when the IOC init path already ran the
    /// PINI=YES pass and published completion (`mark_pini_done`, as
    /// `IocApplication::run` Phase 2b.6 does), the scan owner must NOT
    /// re-run it — C's `initialProcess` (iocInit.c:653) runs once, inside
    /// iocBuild. Sync point: once every scan thread is up the owner is
    /// past its PINI stage, so a re-run would already have advanced the
    /// record's TIME.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_owner_skips_pini_when_the_init_path_already_ran_it() {
        use crate::server::record::PiniMode;
        use crate::server::records::ai::AiRecord;
        use crate::types::EpicsValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record("PINI:ONCE", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();
        {
            let rec = db.get_record("PINI:ONCE").unwrap();
            let mut inst = rec.write();
            inst.put_common_field("PINI", EpicsValue::String("YES".into()))
                .unwrap();
            inst.common.udf = 0;
        }

        // The IOC init path's own pass + publication (Phase 2b.6 shape).
        db.pini_process(PiniMode::Yes).await;
        db.mark_pini_done();
        let t_init = db.get_record("PINI:ONCE").unwrap().read().common.time;

        let owner = ScanOwner::start(Arc::clone(&db));
        wait_for_count(&db, "scan threads never started", |n| {
            n >= 2 + PERIODIC_SCANS.len()
        })
        .await;
        let t_owner = db.get_record("PINI:ONCE").unwrap().read().common.time;
        assert_eq!(
            t_owner, t_init,
            "the owner re-ran the PINI=YES pass the init path already ran"
        );
        drop(owner);
    }

    /// The other side of the boundary: with NO init-path pass, the owner
    /// runs PINI itself — the direct-entry-point contract (`softioc-rs`,
    /// the rtems binaries, oracle) where nothing pre-runs PINI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_owner_runs_pini_when_nothing_pre_ran_it() {
        use crate::server::records::ai::AiRecord;
        use crate::types::EpicsValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record("PINI:OWNED", Box::new(AiRecord::new(2.5)))
            .await
            .unwrap();
        let t_unprocessed = {
            let rec = db.get_record("PINI:OWNED").unwrap();
            let mut inst = rec.write();
            inst.put_common_field("PINI", EpicsValue::String("YES".into()))
                .unwrap();
            inst.common.udf = 0;
            inst.common.time
        };

        let owner = ScanOwner::start(Arc::clone(&db));
        wait_for_count(&db, "scan threads never started", |n| {
            n >= 2 + PERIODIC_SCANS.len()
        })
        .await;
        let t_owner = db.get_record("PINI:OWNED").unwrap().read().common.time;
        assert!(
            t_owner > t_unprocessed,
            "the owner must run the PINI=YES pass when the init path did not"
        );
        drop(owner);
    }

    /// Redundant-start boundary: a second `ScanOwner` on the same DB is a
    /// parked non-owner (`try_claim_scan_start` dedup), and dropping it
    /// must not disturb the first owner's scan threads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_redundant_scan_owner_parks_and_its_drop_is_harmless() {
        let db = Arc::new(PvDatabase::new());
        let first = ScanOwner::start(Arc::clone(&db));
        wait_for_count(&db, "scan threads never started", |n| {
            n >= 2 + PERIODIC_SCANS.len()
        })
        .await;
        let with_first = Arc::strong_count(&db) - 1;

        let second = ScanOwner::start(Arc::clone(&db));
        drop(second);
        // The second owner's scheduler clone is gone; every scan thread
        // (and the first owner) is still holding.
        wait_for_count(&db, "second owner's drop leaked or killed holders", |n| {
            n == with_first + 1
        })
        .await;

        drop(first);
        wait_for_count(
            &db,
            "scan threads still alive after first owner drop",
            |n| n == 1,
        )
        .await;
    }
}
