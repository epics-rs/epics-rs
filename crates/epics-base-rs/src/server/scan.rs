// RTEMS-EXEC-MODEL-ALLOW(8): the teardown test drives the scheduler from a
// tokio task (spawn/abort are its cancellation instrument) and the seven
// ScanOwner tests (drop-teardown, redundant-owner, PINI-skip, PINI-run,
// tick-runs-on-its-own-thread, watchdog-registration, scanOnce-creation) use
// the tokio test runtime only as the start-context `ScanOwner::start`
// requires; the scan/owner threads under test go through the exec seam
// (`block_on_sync` → `park_on`) when the exec backend is on — all eight
// verified passing under `EPICS_RS_BUILD_EXEC_BACKEND=thread`.
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::background::facility::{recover, run_isolated};
use crate::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use crate::runtime::taskwd::{CheckIn, TASKWD_DELAY, taskwd_insert};
use crate::server::database::PvDatabase;
use crate::server::record::ScanType;

/// Scan scheduler that processes records at their configured scan rates.
///
/// Module-private by design: scanning is owned by the IOC core, and the
/// only way to start it is [`ScanOwner::start`] — a protocol server (CA,
/// PVA) cannot construct or drive a scheduler of its own, which is what
/// used to leave server-less targets with every periodic `SCAN` field
/// dead. It was `pub(crate)` while `new` was the entry point; now that
/// `new` demands a [`TickDriver`] only `start` can produce, the narrower
/// visibility is what the code already meant.
struct ScanScheduler {
    db: Arc<PvDatabase>,
    /// Handed in by [`ScanOwner::start`], never captured here — see
    /// [`TickDriver::capture`]. Carrying it as a field is what makes
    /// `run`'s "must be inside a runtime" precondition a *type*
    /// obligation instead of an ambient one: there is no way to build a
    /// scheduler without having already answered the question.
    driver: TickDriver,
}

/// The periodic scan rates of the LOADED `menuScan`, slowest-first because
/// the menu is (`menuScan.dbd.pod:49-58`: "10 second" through ".1 second").
///
/// The order is load-bearing: C spawns `scan-%g` at
/// `epicsThreadPriorityScanLow + ind` (`dbScan.c:945`) where `ind` is the
/// offset into `papPeriodic`, so a menu that lists its rates fastest-first
/// inverts the priority ladder. That is the site's choice to make, exactly as
/// it is in C — nothing here reorders the menu.
///
/// This used to be a `const` list of seven `ScanType` variants. It is a
/// function over [`crate::server::record::menu_scan()`] because the rates are
/// site data: C reads them with `dbFindMenu(pdbbase, "menuScan")` in
/// `initPeriodic` and an IOC may ship its own menu with `60 Hz` or
/// `5 minutes`.
pub(crate) fn periodic_scans() -> Vec<ScanType> {
    let menu = crate::server::record::menu_scan();
    (0..menu.n_periodic())
        .map(|ind| ScanType::Menu(ind as u16 + crate::server::record::SCAN_1ST_PERIODIC))
        .collect()
}

/// What the periodic scan facility calls itself when reporting.
const FACILITY: &str = "periodic scan";

/// Band for the `ind`-th periodic rate — `dbScan.c:945`,
/// `opts.priority = epicsThreadPriorityScanLow + ind`. With
/// [`periodic_scans`] slowest-first this is scan-10 → 60 up to
/// scan-0.1 → 66, the ladder the C IOC measures on RTEMS 6.
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
/// The single owner of the stop transition is `ScanStopGuard`, held
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

/// The scan facility's run state — C `enum ctl` and the file-static
/// `scanCtl` it is held in (`dbScan.c:55`, `:60` @R7.0.10).
///
/// C gates every *asynchronous* scan source on this one cell and nothing
/// else: `periodicTask` scans its list only while `ctlRun`
/// (`dbScan.c:805`), `postEvent` returns immediately unless `ctlRun`
/// (`:538`), and `scanIoRequest` / `scanIoImmediate` queue nothing unless
/// `ctlRun` (`:617`, `:637`). `scanOnce` is deliberately NOT gated there,
/// so a paused IOC still runs a link's or a `dbpf`'s one-shot process —
/// that asymmetry is C's, and it is why `iocPause` freezes periodic and
/// event-driven processing without wedging the shell.
/// C's fourth state, `ctlInit`, has no analogue and is deliberately
/// absent: it is the window between the facility's creation (`scanInit`,
/// `dbScan.c:191-208`) and the first `scanRun`, and this port has no
/// separate creation step to open it. The build phase that occupies that
/// window in C occupies [`ScanCtl::Pause`] here, set where C's `scanInit`
/// sets it — inside the build, before anything can fire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanCtl {
    /// C `ctlRun`, and the port's starting value: a database with no IOC
    /// lifecycle over it (a test harness, an embedded user of
    /// `PvDatabase`) has nothing that could have paused it.
    Run,
    /// C `ctlPause`.
    Pause,
    /// C `ctlExit` — terminal for the threads that read it; a later
    /// `scan_run` re-arms the cell, which is what lets one process stand
    /// a second IOC up after the first shut down.
    Exit,
}

/// C's `static volatile enum ctl scanCtl` (`dbScan.c:60`). One cell for the
/// whole facility, exactly as C has it, so a new asynchronous scan source
/// cannot grow a private idea of whether the IOC is running.
static SCAN_CTL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(SCAN_CTL_RUN);

const SCAN_CTL_RUN: u8 = 1;
const SCAN_CTL_PAUSE: u8 = 2;
const SCAN_CTL_EXIT: u8 = 3;

/// The facility's current state — what every gate below reads.
pub fn scan_ctl() -> ScanCtl {
    match SCAN_CTL.load(std::sync::atomic::Ordering::Acquire) {
        SCAN_CTL_PAUSE => ScanCtl::Pause,
        SCAN_CTL_EXIT => ScanCtl::Exit,
        _ => ScanCtl::Run,
    }
}

/// True while asynchronous scan sources may fire — the single test C spells
/// `scanCtl == ctlRun` at each of its three gate sites.
pub fn scan_is_running() -> bool {
    scan_ctl() == ScanCtl::Run
}

/// C `scanRun` (`dbScan.c:210-223`). Called by the IOC lifecycle owner on
/// the `iocRun` transition, never by a protocol server.
pub fn scan_run() {
    SCAN_CTL.store(SCAN_CTL_RUN, std::sync::atomic::Ordering::Release);
}

/// C `scanPause` (`dbScan.c:225-237`). The periodic threads stay alive and
/// keep their deadlines; they simply stop calling `scanList`, which is what
/// lets `iocRun` resume without rebuilding the facility.
pub fn scan_pause() {
    SCAN_CTL.store(SCAN_CTL_PAUSE, std::sync::atomic::Ordering::Release);
}

/// C `scanStop` (`dbScan.c:154-178`), less the joins: the threads are
/// owned by [`ScanOwner`], whose `Drop` performs C's `epicsThreadMustJoin`
/// half. Setting the cell first is what makes a source that fires between
/// the two see a stopped facility rather than a half-torn one.
pub fn scan_stop() {
    SCAN_CTL.store(SCAN_CTL_EXIT, std::sync::atomic::Ordering::Release);
}

/// How this facility blocks a plain (banded) thread on a future — both a
/// periodic scan thread driving one tick's record processing and the owner
/// thread driving the scheduler itself.
///
/// Hosted: [`tokio::runtime::Handle::block_on`]. RTEMS: `block_on_sync`
/// → `park_on`, the same seam every blocking CA/PVA connection thread
/// already drives record processing through. Either way the *processing
/// itself* runs on this thread, so the thread's EPICS band applies to the
/// work — the point of having dedicated scan threads at all, as it is in C
/// (`periodicTask` calls `scanList` on its own `scan-%g` thread,
/// `db/dbScan.c:784` (`periodicTask`), `:806`; epics-base R7.0.10).
///
/// The handle is **not** here because record processing spawns tasks or
/// starts timers. It once was; that reason is now false. Every deferred
/// record tail under `server/` goes to `spawn_background` and every delay
/// to `sleep_background`, both of which land on the process-global
/// background executor on either backend — a property the
/// `record-seam-gate` census in `server/mod.rs` enforces by walking the
/// tree rather than by comment. What the handle still buys is that record
/// support *outside* this crate is pluggable: a site's own device support
/// may use tokio in the ordinary way, and it sees a normal runtime context
/// here instead of a panic. It also gives the facility one owner for "how
/// do I block on a future", so the scan-owner thread and the rate threads
/// cannot drift apart.
#[derive(Clone)]
struct TickDriver {
    #[cfg(tokio_backend)]
    handle: tokio::runtime::Handle,
}

impl TickDriver {
    /// Capture from the caller's async context — **once per IOC, in
    /// [`ScanOwner::start`]**, on the one thread that provably has an
    /// answer. Every thread downstream is handed the result.
    ///
    /// It used to be called a second time, inside `ScanScheduler::run`,
    /// where it could only ever re-derive what `start` had already
    /// established: `run` reaches that line under the very `block_on`
    /// the first capture set up. A second ambient capture is a second
    /// place the contract can be broken and a second `expect` to reason
    /// about, for no new information.
    fn capture() -> Self {
        Self {
            #[cfg(tokio_backend)]
            handle: tokio::runtime::Handle::try_current().expect(
                "ScanOwner::start on the tokio backend must be called inside a tokio runtime",
            ),
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
                // neither — this module just created it as a
                // `MandatoryThread` and it runs no facility loop.
                Err(e) => unreachable!("a periodic scan thread is blockable: {e}"),
            }
        }
    }
}

/// C `dbScan.c:89` — how long after the tenth consecutive over-run the first
/// warning may be printed.
const OVERRUN_REPORT_DELAY: f64 = 10.0;
/// C `dbScan.c:90` — the ceiling the report interval doubles up to.
const OVERRUN_REPORT_MAX: f64 = 3600.0;

/// What one post-scan bookkeeping step decided.
struct TickOutcome {
    /// The sweep ran past its deadline, so the rate's cumulative counter moves.
    overran: bool,
    /// The warning this over-run tripped, already formatted.
    warning: Option<String>,
}

/// C `periodicTask`'s over-run bookkeeping (`dbScan.c:788-852`) as one owner.
///
/// The rule it holds is that an over-running list retries after `penalty`, not
/// after a whole further period: a 10 s list whose sweep takes 11 s runs on a
/// ~12 s cycle in C, where waiting out the next period would make it ~21 s.
/// Keeping the arithmetic in one object rather than inline in the thread body
/// is what makes the three boundaries — the `period >= 2` penalty branch,
/// over-run vs on-time, and the ninth vs tenth consecutive over-run — testable
/// without a running scan thread.
struct OverrunTracker {
    /// Names the thread in the warning, exactly C's `ppsl->name`: the
    /// `menuScan` choice string.
    scan: ScanType,
    period: Duration,
    /// C `dbScan.c:798`.
    penalty: Duration,
    /// Over-runs **in a row** — C's local `overruns`, which the report counts
    /// and divides by. Distinct from the cumulative per-rate counter `scanppl`
    /// prints (C's `ppsl->overruns`), which never resets.
    consecutive: u32,
    /// Seconds of lateness accumulated across the current consecutive run, and
    /// its extremes. C reseeds all three when `overtime` is back to zero.
    overtime: f64,
    over_min: f64,
    over_max: f64,
    report_delay: f64,
    reported: Instant,
}

impl OverrunTracker {
    /// C `dbScan.c:798`: `(ppsl->period >= 2) ? 1 : (ppsl->period / 2)`.
    fn penalty_for(period: Duration) -> Duration {
        if period >= Duration::from_secs(2) {
            Duration::from_secs(1)
        } else {
            period / 2
        }
    }

    fn new(scan: ScanType, period: Duration, start: Instant) -> Self {
        Self {
            scan,
            period,
            penalty: Self::penalty_for(period),
            consecutive: 0,
            overtime: 0.0,
            over_min: 0.0,
            over_max: 0.0,
            report_delay: OVERRUN_REPORT_DELAY,
            reported: start,
        }
    }

    /// One iteration of C's post-scan block (`dbScan.c:809-851`). `next` is the
    /// deadline accumulator C advances by `period` and then, on an over-run,
    /// resets to `now + penalty` — the sleeper at the head of the loop waits
    /// until `next`, so C's separate `delay` needs no counterpart here.
    fn after_scan(&mut self, next: &mut Instant, now: Instant) -> TickOutcome {
        *next += self.period;
        if now < *next {
            // C `dbScan.c:846-850`. `over_min`/`over_max` are deliberately left
            // alone; the `overtime == 0.0` test below is what reseeds them.
            self.consecutive = 0;
            self.report_delay = OVERRUN_REPORT_DELAY;
            self.overtime = 0.0;
            return TickOutcome {
                overran: false,
                warning: None,
            };
        }

        let over = (now - *next).as_secs_f64();
        if self.overtime == 0.0 {
            self.overtime = over;
            self.over_min = over;
            self.over_max = over;
        } else {
            self.overtime += over;
            self.over_min = self.over_min.min(over);
            self.over_max = self.over_max.max(over);
        }
        *next = now + self.penalty;
        self.consecutive += 1;

        let warning =
            if self.consecutive >= 10 && (now - self.reported).as_secs_f64() > self.report_delay {
                let period = self.period.as_secs_f64();
                let scan = self.scan;
                let msg = format!(
                    "\ndbScan {} from '{scan}' scan thread:\n\tScan processing \
                 averages {:.3} seconds ({:.3} .. {:.3}).\n\tOver-runs have now \
                 happened {} times in a row.\n\tTo fix this, move some records \
                 to a slower scan rate.\n",
                    crate::runtime::log::erl_warning(),
                    period + self.overtime / f64::from(self.consecutive),
                    period + self.over_min,
                    period + self.over_max,
                    self.consecutive,
                );
                self.reported = now;
                if self.report_delay < OVERRUN_REPORT_MAX / 2.0 {
                    self.report_delay *= 2.0;
                } else {
                    self.report_delay = OVERRUN_REPORT_MAX;
                }
                Some(msg)
            } else {
                None
            };

        TickOutcome {
            overran: true,
            warning,
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
    // C `periodicTask` registers with the watchdog before it signals
    // `startStopEvent` (`dbScan.c:795-796`), and the registration lasts exactly
    // as long as the loop does. The interval it promises is two of its own
    // periods plus the watchdog's own granularity: a rate that overruns still
    // comes round its loop — that is what the penalty delay below is for — so
    // anything later than that is the loop itself stuck, not slow scanning.
    let watched = taskwd_insert(
        periodic_thread_name(period),
        CheckIn::Every(period * 2 + TASKWD_DELAY),
        None,
    );
    let mut next = Instant::now() + period;
    let mut overrun = OverrunTracker::new(scan_type, period, Instant::now());
    loop {
        watched.check_in();
        // Sleep until the deadline or the stop signal, whichever first.
        let mut stopped = recover(FACILITY, stop.stopped.lock());
        loop {
            if *stopped || scan_ctl() == ScanCtl::Exit {
                // C `periodicTask`'s loop condition (`dbScan.c:801`): the
                // facility's own stop ends the thread, not only the
                // owner handle's drop.
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

        // C `periodicTask` scans its list only while the facility is
        // running (`dbScan.c:805`); on `iocPause` the thread keeps its
        // deadlines and skips the sweep, which is what lets `iocRun`
        // resume the rate in phase instead of rebuilding it.
        //
        // A panicking record costs this tick, not the rate's thread —
        // the same isolation the scanOnce worker gives its tails.
        if scan_is_running() {
            run_isolated(FACILITY, || {
                driver.drive(async {
                    if let Some(list) = scan_type.scan_list() {
                        db.scan_list_once(list).await;
                    }
                });
            });
        }

        // Next deadline. Missed ticks are skipped rather than burst as
        // catch-up ticks, and an over-running list retries after `penalty`
        // rather than idling out a whole further period — see
        // [`OverrunTracker`].
        let outcome = overrun.after_scan(&mut next, Instant::now());
        if outcome.overran {
            db.record_scan_overrun(scan_type);
        }
        if let Some(warning) = outcome.warning {
            crate::runtime::log::errlog_printf(&warning);
        }
    }
}

impl ScanScheduler {
    fn new(db: Arc<PvDatabase>, driver: TickDriver) -> Self {
        Self { db, driver }
    }

    /// Run the PINI=YES pass (unless the IOC init path already ran it —
    /// see the exactly-once gate below) and all periodic scan tasks.
    /// Never returns; dropping the future stops every scan thread (see
    /// `ScanStopGuard`).
    ///
    /// If another `ScanScheduler` has already started for the same DB
    /// (e.g. an IOC entry point and an embedded harness both starting a
    /// [`ScanOwner`]), this call parks as a non-owner and spawns no
    /// duplicate scan tasks.
    async fn run(&self) {
        let is_first = self.db.try_claim_scan_start();

        if !is_first {
            // Another ScanScheduler already owns the periodic tasks for this DB.
            // Avoid spawning duplicates; just park this future.
            std::future::pending::<()>().await;
            return;
        }

        // C `scanInit` runs `initPeriodic(); initOnce();` before anything
        // else (`dbScan.c:201-202`), and all of `scanInit` precedes
        // `initialProcess` (`iocInit.c:186` then `:195`). Reading the menu
        // here is that `initPeriodic`: it is what freezes the periodic band
        // count (`menu_scan::menu_scan` pushes it down), and the `scanOnce`
        // worker takes its priority from that count — so the order is a data
        // dependency, not a position in this function.
        let scans = periodic_scans();
        crate::runtime::task::background_scan_once_start();

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

        // C `spawnPeriodic` (`dbScan.c:939-955`): one **dedicated,
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
        let driver = &self.driver;
        // Each rate is a [`MandatoryThread`]: C's `spawnPeriodic` waits on
        // `startStopEvent`, which only `periodicTask` posts, so a rate that
        // could not be created wedges `iocInit` and the C IOC never serves.
        // The Rust equivalent of "never serves" is that the process dies here —
        // a `.expect` would only have killed *this* thread on a `panic =
        // "unwind"` target, dropping the guard below and leaving an IOC that
        // answers CA with no periodic scanning at all.
        for (ind, scan_type) in scans.into_iter().enumerate() {
            if let Some(period) = scan_type.interval() {
                let db = Arc::clone(&self.db);
                let stop = Arc::clone(&stop);
                let driver = driver.clone();
                MandatoryThread::new(
                    periodic_thread_name(period),
                    periodic_priority(ind),
                    // dbScan.c:946 — `opts.stackSize = epicsThreadStackBig`.
                    StackSizeClass::Big,
                )
                .spawn(move || {
                    periodic_loop(db, scan_type, period, stop, driver);
                });
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
///   `realtime-ca-ioc`, `realtime-pva-ioc`) start one themselves, right where
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
/// The owner future parks forever holding the `ScanStopGuard`. On the
/// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread` / RTEMS) a spawned task
/// that returns
/// `Pending` with its waker registered nowhere has no strong holder — the
/// executor drops it (tokio keeps detached tasks alive), the guard drops,
/// and every scan thread exits within one tick. Measured on target:
/// probes reached the spawn point while the thread census showed zero
/// `scan-*` threads, with the handle both dropped and `mem::forget`-ed. A
/// thread keeps the future (and guard) alive on its own stack on both
/// backends. On the tokio backend the thread drives the future via the
/// handle captured at [`ScanOwner::start`] (so `start` must be called
/// inside a tokio runtime there); on the exec backend it drives it via
/// `block_on_sync` → `park_on`, the same seam every blocking CA/PVA
/// connection thread uses.
///
/// # Teardown
///
/// Dropping the handle wakes the owner thread, which drops the scheduler
/// future — tripping the stop flag through the `ScanStopGuard` — and
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
        // This is the port's `iocRun` point: `IocApplication` calls it
        // exactly where C's `iocRun` calls `scanRun`, and the six binaries
        // that build their database through a server builder call it as
        // the last step of their own init. The transition itself belongs
        // to the lifecycle owner, which is why it is not `scan_run()`
        // here — see `ioc_app::note_scan_owner_started`.
        crate::server::ioc_app::note_scan_owner_started();
        let (stop_tx, stop_rx) = crate::runtime::sync::oneshot::channel::<()>();
        // The scan facility's one and only ambient capture, taken on the
        // caller's thread because neither the owner thread nor the `scan-%g`
        // threads have a runtime of their own. Everything downstream — the
        // scheduler, the PINI pass, every rate thread — is handed this
        // driver rather than asking again.
        let driver = TickDriver::capture();
        // Mandatory: this thread *is* "this IOC scans". `start` has no error
        // path back to its callers (`IocApplication::run` and the entry-point
        // binaries all take a `Self`), so a thread that cannot be created takes
        // the process with it rather than leaving a scan-less IOC serving.
        let join = MandatoryThread::new(
            "scan-owner",
            // Below every scan band: the owner only parks after the PINI
            // pass; the ladder the `scan-%g` threads hold is the measured
            // one (`periodic_priority`).
            ThreadPriority::Low,
            // The owner thread runs the PINI pass's record processing on
            // its own stack (the `scan-%g` threads it spawns carry Big
            // stacks of their own, dbScan.c:946). Medium is the proven
            // shape from the interim per-binary owner thread, measured on
            // the RTEMS target.
            StackSizeClass::Medium,
        )
        .spawn(move || {
            let scheduler = ScanScheduler::new(db, driver.clone());
            let owner = async move {
                tokio::select! {
                    _ = scheduler.run() => {}
                    _ = stop_rx => {}
                }
            };
            // Through the same driver the rate threads use: one owner for
            // "how does a banded plain thread block on a future", so the
            // owner thread and the `scan-%g` threads cannot drift apart.
            driver.drive(owner);
        });
        Self {
            stop: Some(stop_tx),
            join: Some(join),
        }
    }
}

impl Drop for ScanOwner {
    fn drop(&mut self) {
        // C `scanStop` sets `ctlExit` before it signals and joins
        // (`dbScan.c:159-176`), so a source that fires between the two
        // sees a stopped facility rather than a half-torn one.
        scan_stop();
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
mod overrun_tests {
    use super::*;

    /// Drive `ticks` sweeps that each take `sweep` against a `period` list,
    /// starting from `base`. Returns every warning the tracker emitted.
    fn run(period: Duration, sweep: Duration, ticks: u32, base: Instant) -> Vec<String> {
        let mut next = base + period;
        let mut tracker = OverrunTracker::new(ScanType::SEC1, period, base);
        let mut warnings = Vec::new();
        for i in 1..=ticks {
            let now = base + sweep * i;
            if let Some(w) = tracker.after_scan(&mut next, now).warning {
                warnings.push(w);
            }
        }
        warnings
    }

    /// BOUNDARY: C `dbScan.c:798` — `period >= 2` retries after a flat second,
    /// anything faster after half its own period. Two seconds exactly is on
    /// the flat-second side.
    #[test]
    fn the_penalty_branches_at_a_two_second_period() {
        assert_eq!(
            OverrunTracker::penalty_for(Duration::from_secs(10)),
            Duration::from_secs(1)
        );
        assert_eq!(
            OverrunTracker::penalty_for(Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert_eq!(
            OverrunTracker::penalty_for(Duration::from_millis(1999)),
            Duration::from_micros(999_500)
        );
        assert_eq!(
            OverrunTracker::penalty_for(Duration::from_millis(100)),
            Duration::from_millis(50)
        );
    }

    /// BOUNDARY: on time. The deadline advances by exactly one period and
    /// nothing is counted.
    #[test]
    fn an_on_time_sweep_advances_the_deadline_by_one_period() {
        let base = Instant::now();
        let period = Duration::from_secs(10);
        let mut next = base + period;
        let mut tracker = OverrunTracker::new(ScanType::SEC10, period, base);

        let outcome = tracker.after_scan(&mut next, base + Duration::from_secs(3));

        assert!(!outcome.overran);
        assert!(outcome.warning.is_none());
        assert_eq!(next, base + Duration::from_secs(20));
    }

    /// BOUNDARY: over-run. The retry deadline is `now + penalty`, not
    /// `now + period`. This is the defect: a 10 s list whose sweep takes 11 s
    /// starts at its `base + 10 s` deadline and finishes at `base + 21 s`, so
    /// C's next sweep starts at 22 s — a ~12 s cycle — where waiting out a
    /// further whole period put it at 31 s, a ~21 s cycle.
    #[test]
    fn an_over_run_retries_after_the_penalty_not_a_whole_period() {
        let base = Instant::now();
        let period = Duration::from_secs(10);
        let mut next = base + period;
        let mut tracker = OverrunTracker::new(ScanType::SEC10, period, base);

        let now = base + Duration::from_secs(21);
        let outcome = tracker.after_scan(&mut next, now);

        assert!(outcome.overran);
        assert_eq!(
            next,
            base + Duration::from_secs(22),
            "C `dbScan.c:826-830`: delay = penalty, next = now + delay"
        );
    }

    /// BOUNDARY: the deadline is late by exactly zero. C tests `delay <= 0.0`,
    /// so an exactly-on-the-deadline sweep is an over-run.
    #[test]
    fn a_sweep_that_lands_exactly_on_the_deadline_is_an_over_run() {
        let base = Instant::now();
        let period = Duration::from_secs(1);
        let mut next = base + period;
        let mut tracker = OverrunTracker::new(ScanType::SEC1, period, base);

        let now = base + Duration::from_secs(2);
        assert!(tracker.after_scan(&mut next, now).overran);
        assert_eq!(next, now + Duration::from_millis(500));
    }

    /// BOUNDARY: nine consecutive over-runs are silent, the tenth reports —
    /// C `dbScan.c:830`, `++overruns >= 10`. Each sweep here takes two
    /// seconds on a one-second list, so by the ninth tick the report-delay
    /// half of the condition is long satisfied and only the count gates it.
    #[test]
    fn the_report_fires_on_the_tenth_consecutive_over_run() {
        let base = Instant::now();
        let period = Duration::from_secs(1);
        let sweep = Duration::from_secs(2);

        assert!(
            run(period, sweep, 9, base).is_empty(),
            "the ninth consecutive over-run is still silent"
        );

        let warnings = run(period, sweep, 10, base);
        assert_eq!(warnings.len(), 1, "the tenth reports");
        let w = &warnings[0];
        assert!(w.contains("from '1 second' scan thread"), "{w}");
        assert!(w.contains("10 times in a row"), "{w}");
        assert!(w.contains("move some records to a slower scan rate"), "{w}");
    }

    /// BOUNDARY: the report interval doubles after each report
    /// (`dbScan.c:840-843`). Sweeps take 2 s, so reports are gated by a
    /// 10 s then a 20 s interval: ticks 10 and 21. A fixed 10 s interval
    /// would have reported three times over the same span.
    #[test]
    fn the_report_interval_doubles_after_each_report() {
        let base = Instant::now();
        let warnings = run(Duration::from_secs(1), Duration::from_secs(2), 21, base);

        assert_eq!(warnings.len(), 2, "reports at tick 10 and tick 21");
        assert!(warnings[1].contains("21 times in a row"), "{}", warnings[1]);
    }

    /// BOUNDARY: one on-time sweep resets both the consecutive count and the
    /// report backoff (`dbScan.c:846-850`), so the next run of over-runs has
    /// to climb to ten again.
    #[test]
    fn an_on_time_sweep_resets_the_consecutive_run() {
        let base = Instant::now();
        let period = Duration::from_secs(1);
        let mut next = base + period;
        let mut tracker = OverrunTracker::new(ScanType::SEC1, period, base);

        // Nine over-runs, then one sweep that beats its deadline.
        for i in 1..=9u32 {
            tracker.after_scan(&mut next, base + Duration::from_secs(2) * i);
        }
        assert_eq!(tracker.consecutive, 9);
        next = base + Duration::from_secs(100);
        tracker.after_scan(&mut next, base + Duration::from_secs(100));
        assert_eq!(tracker.consecutive, 0);
        assert_eq!(tracker.report_delay, OVERRUN_REPORT_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dbScan.c:945` — the rate→priority ladder, pinned to the values
    /// the C IOC measures on RTEMS 6 (`scan-10` 60 … `scan-0.1` 66).
    #[test]
    fn periodic_ladder_matches_dbscan() {
        let expected: &[(ScanType, u8, &str)] = &[
            (ScanType::SEC10, 60, "scan-10"),
            (ScanType::SEC5, 61, "scan-5"),
            (ScanType::SEC2, 62, "scan-2"),
            (ScanType::SEC1, 63, "scan-1"),
            (ScanType::SEC05, 64, "scan-0.5"),
            (ScanType::SEC02, 65, "scan-0.2"),
            (ScanType::SEC01, 66, "scan-0.1"),
        ];
        let rates = periodic_scans();
        assert_eq!(rates.len(), expected.len());
        for (ind, &(scan_type, prio, name)) in expected.iter().enumerate() {
            assert_eq!(rates[ind], scan_type, "order is load-bearing");
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
        for ind in 0..periodic_scans().len() {
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
        let scheduler = ScanScheduler::new(Arc::clone(&db), TickDriver::capture());
        let task = tokio::spawn(async move { scheduler.run().await });

        // Wait until every rate's thread is up (7 clones + task's own).
        let deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&db) < 2 + periodic_scans().len() {
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

    /// The periodic scan's *record processing* must run on the rate's own
    /// banded `scan-%g` thread, not on a shared tokio worker. Under the
    /// previous `JoinSet::spawn` shape each tick body ran on whatever pool
    /// worker picked the task up, so the `ScanLow + ind` ladder applied to
    /// nothing that did work: the scan inherited the pool's scheduling
    /// class. Pinning the *executing* thread
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
            rec.write().common.scan = ScanType::SEC01;
        }
        db.update_scan_index("SCAN:THREAD", ScanType::Passive, ScanType::SEC01, 0, 0);

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

        let expected = periodic_thread_name(ScanType::SEC01.interval().unwrap());
        assert_eq!(
            name, expected,
            "the .1 second tick processed on `{name}`, not on its own \
             banded `{expected}` thread — periodic scan is back on a \
             shared pool"
        );
    }

    /// The hook, end to end: a real periodic scan thread is in the watchdog's
    /// table while it runs and out of it once it stops. `periodic_loop`'s
    /// registration is an RAII entry rather than C's paired `taskwdRemove`,
    /// so the removal half is only true if the entry is actually dropped on
    /// the thread's way out — which is what the second half asserts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_scan_thread_is_listed_by_the_task_watchdog() {
        fn table() -> String {
            let out = std::cell::RefCell::new(String::new());
            crate::runtime::taskwd::taskwd_show(1, &|line| {
                out.borrow_mut().push_str(line);
                out.borrow_mut().push('\n');
            });
            out.into_inner()
        }

        let wanted = periodic_thread_name(ScanType::SEC01.interval().unwrap());
        assert!(
            !table().contains(&wanted),
            "`{wanted}` was registered before any scan thread started"
        );

        let db = Arc::new(PvDatabase::new());
        let owner = ScanOwner::start(Arc::clone(&db));

        let deadline = Instant::now() + Duration::from_secs(10);
        while !table().contains(&wanted) {
            assert!(
                Instant::now() < deadline,
                "`{wanted}` never reached the watchdog table:\n{}",
                table()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        drop(owner);
        let deadline = Instant::now() + Duration::from_secs(10);
        while table().contains(&wanted) {
            assert!(
                Instant::now() < deadline,
                "`{wanted}` stayed registered after its thread exited:\n{}",
                table()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// C `scanInit` creates the `scanOnce` thread itself (`initOnce`,
    /// `dbScan.c:201`), so a C IOC that never runs a one-shot still lists
    /// `scanOnce` in `taskwdShow`. The port used to create it at the first
    /// submission, which is a thread an operator comparing the two tables
    /// would find missing — and a `MandatoryThread` failure discovered long
    /// after init rather than at it.
    ///
    /// The assertion is deliberately made with nothing ever submitted: a test
    /// that queued a one-shot first would pass on the lazy path too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn starting_the_scan_owner_creates_the_scan_once_worker() {
        fn table() -> String {
            let out = std::cell::RefCell::new(String::new());
            crate::runtime::taskwd::taskwd_show(1, &|line| {
                out.borrow_mut().push_str(line);
                out.borrow_mut().push('\n');
            });
            out.into_inner()
        }

        assert!(
            !table().contains("scanOnce"),
            "the one-shot worker existed before any scan owner started"
        );

        let db = Arc::new(PvDatabase::new());
        let _owner = ScanOwner::start(Arc::clone(&db));

        let deadline = Instant::now() + Duration::from_secs(10);
        while !table().contains("scanOnce") {
            assert!(
                Instant::now() < deadline,
                "`scanOnce` never reached the watchdog table:\n{}",
                table()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
            n >= 2 + periodic_scans().len()
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
            n >= 2 + periodic_scans().len()
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
            n >= 2 + periodic_scans().len()
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
            n >= 2 + periodic_scans().len()
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
