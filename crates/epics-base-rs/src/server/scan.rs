// RTEMS-EXEC-MODEL-ALLOW(15): the teardown test drives the scheduler from a
// tokio task (spawn/abort are its cancellation instrument) and the seven
// ScanOwner tests (drop-teardown, redundant-owner, PINI-skip, PINI-run,
// tick-runs-on-its-own-thread, watchdog-registration, scanOnce-creation) use
// the tokio test runtime only as the start-context `ScanOwner::start`
// requires; the scan/owner threads under test go through the exec seam
// (`block_on_sync` → `park_on`) when the exec backend is on. The
// seven parallel-pass tests (PHAS order, no-helper walk, the two slow-rate
// cap boundaries, the two dedicated-helper boundaries, and the config gate)
// want a runtime only for the `.await` that loads their records — the leader
// and its helpers are `MandatoryThread`s either way. All fifteen verified
// passing under `EPICS_RS_BUILD_EXEC_BACKEND=thread`.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::background::facility::{recover, run_isolated};
use crate::runtime::sync::{Event, Signalled};
use crate::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use crate::runtime::taskwd::{CheckIn, TASKWD_DELAY, taskwd_insert};
use crate::server::database::PvDatabase;
use crate::server::database::scan_index::ScanPassSnapshot;
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
        // C `deletePeriodic` leaves `papPeriodic` NULL (`dbScan.c:1044-1045`),
        // which is what makes `scanParallelThreads` answerable again — see
        // [`PERIODIC_LISTS_BUILT`].
        PERIODIC_LISTS_BUILT.store(false, Ordering::Release);
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
    // C `scanRun` sets `interruptAccept = TRUE` here (`dbScan.c:218`). The
    // false→true edge is what fires asyn's one-shot boot flush, delivering
    // every seeded `_RBV` value to the records that registered during iocInit.
    crate::runtime::interrupt_accept::set_interrupts_accepted(true);
}

/// C `scanPause` (`dbScan.c:225-237`). The periodic threads stay alive and
/// keep their deadlines; they simply stop calling `scanList`, which is what
/// lets `iocRun` resume without rebuilding the facility.
pub fn scan_pause() {
    SCAN_CTL.store(SCAN_CTL_PAUSE, std::sync::atomic::Ordering::Release);
    // C `scanPause` clears `interruptAccept` (`dbScan.c:241`).
    crate::runtime::interrupt_accept::set_interrupts_accepted(false);
}

/// C `scanStop` (`dbScan.c:154-178`), less the joins: the threads are
/// owned by [`ScanOwner`], whose `Drop` performs C's `epicsThreadMustJoin`
/// half. Setting the cell first is what makes a source that fires between
/// the two see a stopped facility rather than a half-torn one.
pub fn scan_stop() {
    SCAN_CTL.store(SCAN_CTL_EXIT, std::sync::atomic::Ordering::Release);
    // C `scanStop` clears `interruptAccept` (`dbScan.c:165`).
    crate::runtime::interrupt_accept::set_interrupts_accepted(false);
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
    /// Which remedy the report names. Resolved once, when the tracker is
    /// built: both facts C reads at print time are already settled by then.
    remedy: Remedy,
}

/// What the over-run report tells the operator to do about it — C builds the
/// tail of the message from `nHelpers` and `epicsThreadGetCPUs()` at the point
/// it prints (`dbScan.c:988-993`).
///
/// One state rather than the two booleans C tests in sequence: the three tails
/// are mutually exclusive, both inputs are fixed once `spawnHelpers` has run,
/// and a tracker that stored them separately would invite a site to test the
/// CPU count on an IOC that already has helpers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Remedy {
    /// Helpers exist, so one more — or one dedicated to this rate — is the
    /// thing to add.
    MoreHelpers,
    /// No helper yet, and a CPU free to run one.
    FirstHelper,
    /// A single CPU: a helper there could only take turns with this thread,
    /// so the report names no helper at all.
    NoHelperWouldHelp,
}

impl Remedy {
    /// The state of an IOC whose helper pool `helpers` says exists.
    fn for_ioc(helpers: bool) -> Self {
        if helpers {
            Self::MoreHelpers
        } else if crate::runtime::background::callback_executor::cpu_count() > 1 {
            Self::FirstHelper
        } else {
            Self::NoHelperWouldHelp
        }
    }

    /// The tail C appends to "move some records to a slower scan rate".
    fn tail(self) -> &'static str {
        match self {
            Self::MoreHelpers => {
                ",\n\tor add helper threads with scanParallelThreads() or \
                 scanRateThreads() before iocInit."
            }
            Self::FirstHelper => {
                ",\n\tor add helper threads with scanParallelThreads() before \
                 iocInit."
            }
            Self::NoHelperWouldHelp => ".",
        }
    }
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

    fn new(scan: ScanType, period: Duration, start: Instant, remedy: Remedy) -> Self {
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
            remedy,
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
                let remedy = self.remedy.tail();
                let msg = format!(
                    "\ndbScan {} from '{scan}' scan thread:\n\tScan processing \
                 averages {:.3} seconds ({:.3} .. {:.3}).\n\tOver-runs have now \
                 happened {} times in a row.\n\tTo fix this, move some records \
                 to a slower scan rate{remedy}\n",
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

// ─── Parallel periodic passes ──────────────────────────────────────────────
//
// epics-base issue #998 ("Add parallel periodic scan threads") and the
// prototype it carries: the per-rate threads stay and keep timing their
// passes, and one pool of helper threads, shared by every rate, processes
// records of the pass alongside the rate's own thread.
//
// The ordering rule is what makes it more than a work queue, and it is the
// rule a sequential pass gives for free: **every record of a lower PHAS has
// finished before any record of a higher PHAS starts.** So a pass opens one
// PHAS group at a time and the next group opens only once the current one is
// done — which is why the lock-free *ready stack* of the callback band
// (epics-base #996) is not the mechanism here, and the prototype adds its own:
// a stack hands out whatever is on it, and a group barrier is exactly what it
// has no way to express.

/// Bits of a claim word that hold the slot index — C `SP_IDX_BITS`
/// (`dbScan.c:111`). The remaining bits hold the pass generation, so a cursor
/// and a limit left by different passes can never be paired.
const SP_IDX_BITS: u32 = if usize::BITS > 32 { 32 } else { 20 };
/// C `SP_IDX_MASK` (`dbScan.c:112`) — and so the longest list a pass can hand
/// out slots for; see [`periodic_pass`]'s walk-alone arm.
const SP_IDX_MASK: usize = (1usize << SP_IDX_BITS) - 1;
/// What a generation counts up to before it wraps — C `SP_GEN(SP_NONE)`
/// (`dbScan.c:1207`).
const SP_GEN_MASK: usize = usize::MAX >> SP_IDX_BITS;

/// C `SP_PACK` (`dbScan.c:113`).
const fn sp_pack(generation: usize, idx: usize) -> usize {
    (generation << SP_IDX_BITS) | idx
}

/// C `SP_IDX` (`dbScan.c:114`).
const fn sp_idx(word: usize) -> usize {
    word & SP_IDX_MASK
}

/// C `SP_GEN` (`dbScan.c:115`).
const fn sp_gen(word: usize) -> usize {
    word >> SP_IDX_BITS
}

/// How many helpers one pool can hold — C `SP_MAX_HELPERS` (`dbScan.c:164`): a
/// word's worth, which is also the width of the one-bit-per-rate `wanted`
/// mask. C spells the second use `SP_MAX_PERIODS` (`:165`) and defines it as
/// the first, so there is one number and two diagnostics that print it.
///
/// `pub(crate)` because the clamp C prints from inside
/// `scanParallelThreads` is printed by the iocsh command here, which has to
/// know the width it is clamping to.
pub(crate) const MAX_PARALLEL_THREADS: usize = usize::BITS as usize;

/// The rates faster than `ind` — C `runSlots`' `faster` mask
/// (`dbScan.c:1118`).
///
/// [`periodic_scans`] is slowest-first, so the faster rates are the higher
/// bits; that order is the site's `menuScan` and [`periodic_priority`] reads
/// it the same way. Shifted through `checked_shl` because the fastest rate's
/// own mask shifts the whole word out, which C's `2 << ind` reaches as
/// undefined behaviour at `ind == MAX_PARALLEL_THREADS - 1`.
fn faster_than(ind: usize) -> usize {
    usize::MAX.checked_shl(ind as u32 + 1).unwrap_or(0)
}

/// One rate's pass state, shared between the rate's own thread and the
/// helpers — C's additions to `periodic_scan_list` (`dbScan.c:122-130`).
///
/// # The claim
///
/// A pass works on a snapshot of the list ([`ScanPassSnapshot`]). The leader —
/// the rate's own thread — publishes one PHAS group at a time and takes slots
/// from it like any helper; a slot is claimed by a compare-and-swap on
/// [`Self::cursor`], and claims stop at [`Self::limit`]. Within a pass the
/// limit only grows, group by group, and only after [`Self::outstanding`] for
/// the previous group has reached zero. That is what keeps a higher PHAS from
/// starting before a lower one has finished.
///
/// # The invariant the accounting rests on
///
/// **Every claimed slot is subtracted from its group's `outstanding` exactly
/// once, by the thread that claimed it, before that thread looks for other
/// work.** [`run_slots`]' tail is the only writer of a decrement and the only
/// signaller of [`Self::done`]; the per-record [`run_isolated`] inside its
/// loop is what stops a panicking record from unwinding past that tail. A
/// group whose count never reaches zero parks its leader for the life of the
/// IOC, so this is not an accounting nicety.
struct PeriodicPass {
    /// Which rate this is — the SCAN value a slot's record must still carry
    /// to be processed. C `periodic_scan_list::scan` (`dbScan.c:122`).
    scan: ScanType,
    /// The leader's band, which a helper takes while it serves this rate — C
    /// `periodic_scan_list::prio` (`dbScan.c:123`).
    prio: ThreadPriority,
    /// The list this pass works, published by the leader before the first
    /// claim of the pass can succeed and read by a claimer *after* its claim.
    ///
    /// That order is the whole argument for reading it without a lock: a
    /// successful claim is a slot of a group the leader is still waiting on,
    /// so `outstanding` is not zero, so the leader cannot have reached the
    /// next pass and replaced this. C relies on the same argument to refill
    /// one `snap` buffer in place (`dbScan.c:1158-1181`).
    snapshot: arc_swap::ArcSwapOption<ScanPassSnapshot>,
    /// C `periodic_scan_list::cursor` (`dbScan.c:127`) — `SP_PACK(gen, next
    /// slot)`.
    cursor: AtomicUsize,
    /// C `periodic_scan_list::limit` (`dbScan.c:128`) — `SP_PACK(gen, end of
    /// the open group)`.
    limit: AtomicUsize,
    /// C `periodic_scan_list::outstanding` (`dbScan.c:129`) — slots of the
    /// open group not yet done.
    outstanding: AtomicI32,
    /// C `periodic_scan_list::doneEvent` (`dbScan.c:130`). One waiter for its
    /// whole life, the rate's own thread, which is what [`Event`] requires.
    done: Event,
}

impl PeriodicPass {
    fn new(scan: ScanType, prio: ThreadPriority) -> Self {
        Self {
            scan,
            prio,
            snapshot: arc_swap::ArcSwapOption::empty(),
            cursor: AtomicUsize::new(0),
            limit: AtomicUsize::new(0),
            outstanding: AtomicI32::new(0),
            done: Event::new(),
        }
    }

    /// Claim the next slot of the open group, or `None` — C `claimSlot`
    /// (`dbScan.c:1097-1111`).
    ///
    /// A cursor and a limit from different generations mean the leader is
    /// between the two stores of a publish; there is nothing to take from it
    /// yet.
    fn claim(&self) -> Option<usize> {
        let mut c = self.cursor.load(Ordering::Acquire);
        loop {
            let l = self.limit.load(Ordering::Acquire);
            if sp_gen(c) != sp_gen(l) || sp_idx(c) >= sp_idx(l) {
                return None;
            }
            match self
                .cursor
                .compare_exchange_weak(c, c + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(sp_idx(c)),
                Err(current) => c = current,
            }
        }
    }

    /// Whether the open group still has a slot to claim. A hint, taken without
    /// the generation check [`Self::claim`] makes — C `helperTask`'s look
    /// (`dbScan.c:1265-1267`).
    fn has_work(&self) -> bool {
        sp_idx(self.cursor.load(Ordering::Acquire)) < sp_idx(self.limit.load(Ordering::Acquire))
    }
}

/// Who is running slots — C `runSlots`' `helper` argument (`dbScan.c:1116`).
enum SlotRunner<'a> {
    /// The rate's own thread. It never leaves a group it opened: nothing else
    /// is waiting for that group, and it is.
    Leader,
    /// A pooled helper, which looks for a faster rate between records and
    /// leaves when one wants help.
    Helper {
        /// The pool's `helpWanted`.
        wanted: &'a AtomicUsize,
        /// The rates whose want ends this helper's stay — [`faster_than`].
        faster: usize,
    },
}

/// Run slots of the open group until none is left or, for a helper, until a
/// faster rate wants help — C `runSlots` (`dbScan.c:1116-1136`).
fn run_slots(db: &PvDatabase, pass: &PeriodicPass, who: &SlotRunner<'_>) {
    // One set for this stay, as the sequential sweep keeps one for its whole
    // walk: `run_process_frame` takes its own marker back out on every exit,
    // so a returned cascade leaves it empty again.
    let mut visited = crate::server::database::ProcStack::new();
    let mut ran = 0i32;
    while let Some(slot) = pass.claim() {
        if let Some(snapshot) = pass.snapshot.load_full() {
            // A panicking record costs that record. It must not unwind out of
            // here — see the invariant on [`PeriodicPass`].
            if !run_isolated(FACILITY, || {
                db.process_scan_slot(&snapshot, slot, pass.scan, &mut visited)
            }) {
                // An unwound frame may have left its cycle marker behind, and
                // a marker nothing takes out silences every later link onto
                // that record for the rest of this stay.
                visited = crate::server::database::ProcStack::new();
            }
        }
        ran += 1;
        if let SlotRunner::Helper { wanted, faster } = who {
            if wanted.load(Ordering::Acquire) & faster != 0 {
                break;
            }
        }
    }
    if ran > 0 && pass.outstanding.fetch_sub(ran, Ordering::AcqRel) == ran {
        pass.done.signal();
    }
}

/// What one periodic rate's thread is for — the rate, and the state it shares
/// with the helper pool.
///
/// A struct because the alternative is eight positional arguments to
/// [`periodic_loop`], three of which are only ever read together.
struct PeriodicDuty {
    scan_type: ScanType,
    period: Duration,
    /// This rate's offset into [`periodic_scans`]: the bit it owns in the
    /// pool's `wanted` mask, and the offset its band comes from.
    ind: usize,
    pass: Arc<PeriodicPass>,
    /// `None` when no helpers were configured, which is every IOC that has
    /// not called `scanParallelThreads` — then [`periodic_pass`] is the old
    /// sequential walk, over a snapshot.
    pool: Option<Arc<HelperPool>>,
}

/// One pass over a rate's records, PHAS group by PHAS group — C
/// `periodicPass` (`dbScan.c:1189-1240`).
///
/// The leader takes slots itself, so with no helpers this is a sequential walk
/// and the pass costs one group barrier per PHAS that nobody waits at. A
/// record added to the list during the pass waits for the next one, as it does
/// in C.
fn periodic_pass(db: &PvDatabase, duty: &PeriodicDuty) {
    let Some(list) = duty.scan_type.scan_list() else {
        return;
    };
    let pass = &duty.pass;
    let snapshot = db.scan_pass_snapshot(list);
    if snapshot.is_empty() {
        return;
    }
    if snapshot.len() > SP_IDX_MASK {
        // More records than a slot index holds — 32-bit targets only, where
        // the index is 20 bits. The leader walks the snapshot alone, so
        // nothing has to be published and no claim can be made.
        // C `dbScan.c:1198-1205`.
        let mut visited = crate::server::database::ProcStack::new();
        for slot in 0..snapshot.len() {
            if !run_isolated(FACILITY, || {
                db.process_scan_slot(&snapshot, slot, pass.scan, &mut visited)
            }) {
                visited = crate::server::database::ProcStack::new();
            }
        }
        return;
    }

    let generation = (sp_gen(pass.cursor.load(Ordering::Relaxed)) + 1) & SP_GEN_MASK;
    let snapshot = Arc::new(snapshot);
    pass.snapshot.store(Some(Arc::clone(&snapshot)));
    let mut start = 0;
    while start < snapshot.len() {
        let mut end = start + 1;
        while end < snapshot.len() && snapshot.phas(end) == snapshot.phas(start) {
            end += 1;
        }

        // `outstanding` and the snapshot before the words a claim needs, and
        // the cursor last on the first group: a claim that succeeds has seen
        // all of them, because it acquires what these two stores release.
        // C `dbScan.c:1220-1224`.
        pass.outstanding
            .store((end - start) as i32, Ordering::Relaxed);
        pass.limit
            .store(sp_pack(generation, end), Ordering::Release);
        if start == 0 {
            pass.cursor.store(sp_pack(generation, 0), Ordering::Release);
        }
        if let Some(pool) = &duty.pool {
            // One slot of the group is the leader's own, so the group wants
            // help for the rest of them and for no more than that — and, for
            // any rate but the one the pool keeps helpers free for, no more
            // than its own dedicated helpers plus the room the cap still has
            // (`dbScan.c:1271-1286`). The want bit goes up regardless, so a
            // helper already awake and walking the rates still finds this
            // group.
            if end - start > 1 {
                let mut want = end - start - 1;
                if duty.ind != pool.fast_rate {
                    want = want.min(pool.slow_room() + pool.dedicated[duty.ind]);
                }
                pool.want(duty.ind);
                pool.wake(want, duty.ind);
            }
        }

        run_slots(db, pass, &SlotRunner::Leader);
        pass.done
            .waiter()
            .wait_until(|| pass.outstanding.load(Ordering::Acquire) == 0);
        if let Some(pool) = &duty.pool {
            pool.unwant(duty.ind);
        }
        start = end;
    }
}

/// Which rates one helper may take work from — C `scan_helper::serves`
/// (`dbScan.c:161`), a mask whose all-ones value `SP_ALL` (`:166`) is what C
/// tests to tell the two kinds of helper apart.
///
/// Named variants rather than that sentinel: "serves every rate" and "is a
/// pool helper, and so subject to the slow-rate cap" are one fact, and a mask
/// that carries both invites a site to test the wrong one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Serves {
    /// Every rate, under [`HelperPool::slow_cap`] — a `scanParallelThreads`
    /// helper.
    Pool,
    /// This rate and no other, at its band — a `scanRateThreads` helper.
    Rate(usize),
}

impl Serves {
    /// C's bitmask form, which `helpWanted` is masked with and which
    /// `run_slots` narrows its faster-rate break to.
    fn mask(self) -> usize {
        match self {
            Self::Pool => usize::MAX,
            Self::Rate(ind) => 1usize << ind,
        }
    }

    /// C `me->serves == SP_ALL` (`dbScan.c:1302`).
    fn is_pool(self) -> bool {
        self == Self::Pool
    }
}

/// One helper — C `scan_helper` (`dbScan.c:157-162`).
struct Helper {
    /// C `scan_helper::wake` (`dbScan.c:158`).
    wake: Event,
    /// C `scan_helper::serves` (`dbScan.c:161`).
    serves: Serves,
}

/// The pool of helper threads, shared by every rate — C's `helpers[]`,
/// `helpWanted` and `helperShutdown` file statics (`dbScan.c:151-169`).
///
/// A leader that opens a group sets its bit in [`Self::wanted`] and wakes as
/// many sleeping helpers as the group has slots beyond its own. A helper
/// serves the fastest rate wanting help, at that rate's leader priority, one
/// record at a time, and looks for a faster rate after each. Leaders never
/// depend on a helper for progress: a leader runs the whole group itself if no
/// helper ever arrives.
///
/// # What the reserve keeps free
///
/// Every pool helper may enter every rate. What the reserve buys is a ceiling:
/// at most [`Self::slow_cap`] of them are inside a rate other than
/// [`Self::fast_rate`] at the same time, so a pass of that rate always finds
/// the rest of the pool idle — whichever helpers those turn out to be.
///
/// # The other kind
///
/// A `scanRateThreads` helper serves one rate and no other, at that rate's
/// band, so every pass of that rate gets the same workers however busy the
/// other rates are — the deterministic choice for an RT IOC, and the reason
/// the cap above does not apply to it. Both kinds may exist at once
/// (`dbScan.c:146-155`).
struct HelperPool {
    /// C `helpWanted` (`dbScan.c:179`): one bit per rate with a group open
    /// that a helper may still claim from.
    wanted: AtomicUsize,
    /// C `helperShutdown` (`dbScan.c:181`), owned by [`HelperStopGuard`].
    shutdown: AtomicBool,
    /// The rate helpers are kept free for: the fastest one with records when
    /// the pool was built, else the fastest there is — C `fastPeriod`
    /// (`dbScan.c:176`).
    fast_rate: usize,
    /// How many pool helpers may be inside a rate other than
    /// [`Self::fast_rate`] at one time — C `slowCap` (`dbScan.c:177`).
    slow_cap: usize,
    /// How many are, right now — C `slowBusy` (`dbScan.c:178`). [`SlowSlot`]
    /// is the only writer.
    slow_busy: AtomicUsize,
    /// Helpers serving only this rate, per rate — C `nDedicated`
    /// (`dbScan.c:175`). A leader adds its own to what the cap allows, since
    /// they are not under the cap.
    dedicated: Box<[usize]>,
    /// One per helper, dedicated ones first — C `helpers[]` (`dbScan.c:173`).
    ///
    /// That order is load-bearing: [`Self::wake`] walks it from the front, so
    /// a rate with its own helpers reaches them before it reaches the pool.
    helpers: Box<[Helper]>,
    /// Every rate's pass, indexed by the rate's offset — C `papPeriodic`
    /// (`dbScan.c:102`), which `helperTask` indexes the same way.
    passes: Box<[Arc<PeriodicPass>]>,
}

/// One pool helper's admission to a rate other than
/// [`HelperPool::fast_rate`] — C's `slowBusy` increment and the decrement
/// that matches it (`dbScan.c:1323-1337`).
///
/// A guard, and the only writer of either half, because the two have to stay
/// paired on every way out of the record: an increment left behind would take
/// a helper off the fast rate's ceiling for the life of the IOC.
struct SlowSlot<'a>(&'a HelperPool);

impl Drop for SlowSlot<'_> {
    fn drop(&mut self) {
        self.0.slow_busy.fetch_sub(1, Ordering::AcqRel);
    }
}

impl HelperPool {
    /// C `maskSet(&helpWanted, mybit)` (`dbScan.c:1285`).
    fn want(&self, ind: usize) {
        self.wanted.fetch_or(1usize << ind, Ordering::AcqRel);
    }

    /// C `maskClear(&helpWanted, mybit)` (`dbScan.c:1293`).
    fn unwant(&self, ind: usize) {
        self.wanted.fetch_and(!(1usize << ind), Ordering::AcqRel);
    }

    /// Wake up to `want` sleeping helpers that serve rate `ind` — C
    /// `wakeHelpers` (`dbScan.c:1191-1205`), whose `eligible` argument is
    /// `periodHelpers[ind]` (`:1286`).
    ///
    /// C picks sleeping helpers out of a `helperSleepers` bitmask and claims
    /// each with a compare-and-swap, so two leaders cannot count the same
    /// helper twice and a helper that is already working is not woken for
    /// nothing. [`Event::signal_if_parked`] *is* that claim — it answers
    /// whether this call took the helper out of its sleep — so the port needs
    /// neither the mask nor its CAS loop, and `periodHelpers` is read off
    /// each helper's own [`Helper::serves`] instead of cached per rate.
    fn wake(&self, mut want: usize, ind: usize) {
        let bit = 1usize << ind;
        for helper in &self.helpers {
            if want == 0 {
                return;
            }
            if helper.serves.mask() & bit == 0 {
                continue;
            }
            if helper.wake.signal_if_parked() == Signalled::Claimed {
                want -= 1;
            }
        }
    }

    /// How many more pool helpers may enter a rate other than
    /// [`Self::fast_rate`] — half of what a leader of such a rate caps its
    /// wake-up count by (`dbScan.c:1277-1283`), the other half being that
    /// rate's own [`Self::dedicated`] helpers.
    fn slow_room(&self) -> usize {
        self.slow_cap
            .saturating_sub(self.slow_busy.load(Ordering::Acquire))
    }

    /// The rates `serves` may take work from that still have a slot to claim,
    /// fastest first — C `helperTask`'s inner loop (`dbScan.c:1314-1322`).
    ///
    /// Fastest is last: `periodic_scans()` is slowest-first.
    fn offered(&self, serves: Serves) -> impl Iterator<Item = usize> + '_ {
        let wanted = self.wanted.load(Ordering::Acquire) & serves.mask();
        (0..self.passes.len())
            .rev()
            .filter(move |ind| wanted & (1usize << ind) != 0)
            .filter(|ind| self.passes[*ind].has_work())
    }

    /// The fastest offered rate this helper may enter, with the [`SlowSlot`]
    /// that let it in — C `helperTask`'s admission (`dbScan.c:1323-1328`).
    ///
    /// A dedicated helper needs no admission: it is not under the cap, and the
    /// only rate it is offered is its own. For a pool helper the answer is
    /// `None` when nothing is offered, and when every offer is a slower rate
    /// whose cap is taken. A refused admission moves on to the next rate
    /// rather than giving up, because an offer from a rate *faster* than
    /// [`Self::fast_rate`] — one that had no records when the pool was built —
    /// comes first in this walk.
    fn take_work(&self, serves: Serves) -> Option<(usize, Option<SlowSlot<'_>>)> {
        for ind in self.offered(serves) {
            if ind == self.fast_rate || !serves.is_pool() {
                return Some((ind, None));
            }
            if let Some(slot) = self.enter_slow() {
                return Some((ind, Some(slot)));
            }
        }
        None
    }

    /// Take one of the [`Self::slow_cap`] places, or give it straight back —
    /// C's increment and its back-out (`dbScan.c:1323-1327`).
    fn enter_slow(&self) -> Option<SlowSlot<'_>> {
        // Built before the test, so the back-out is the same `Drop` as an
        // ordinary exit rather than a second decrement site.
        let slot = SlowSlot(self);
        if self.slow_busy.fetch_add(1, Ordering::AcqRel) < self.slow_cap {
            Some(slot)
        } else {
            None
        }
    }

    /// What [`Self::take_work`] would answer, without taking an admission —
    /// the condition a parked helper re-tests.
    fn offers_work(&self, serves: Serves) -> bool {
        self.offered(serves)
            .any(|ind| ind == self.fast_rate || !serves.is_pool() || self.slow_room() > 0)
    }
}

/// One helper thread's body — C `helperTask` (`dbScan.c:1298-1348`).
///
/// C announces its sleep in `helperSleepers`, looks for work a second time,
/// and only then waits on its event, so a leader that publishes between the
/// two sees the announcement.
/// [`EventWaiter::wait_until`](crate::runtime::sync::EventWaiter::wait_until)
/// is that construction
/// — announce, poll, park — so the second look here is the condition itself.
///
/// `band` is what the thread was spawned at, not a constant: C seeds its
/// `prio` from `epicsThreadGetPrioritySelf` (`dbScan.c:1303`), which for a
/// dedicated helper is already its rate's band, so it never rebands at all.
fn helper_loop(
    db: Arc<PvDatabase>,
    pool: Arc<HelperPool>,
    me: usize,
    driver: TickDriver,
    mut band: ThreadPriority,
) {
    // C `taskwdInsert(0, NULL, NULL)` (`dbScan.c:1306`): a helper is monitored
    // but promises nothing, because an idle pool is the normal state and a
    // helper inside a long record owes no check-in either.
    let watched = taskwd_insert(format!("scanHelper{me}"), CheckIn::Unbounded, None);
    let helper = &pool.helpers[me];
    let serves = helper.serves;
    let waiter = helper.wake.waiter();
    while !pool.shutdown.load(Ordering::Acquire) {
        watched.check_in();
        match pool.take_work(serves) {
            Some((ind, slow)) => {
                let pass = &pool.passes[ind];
                if band != pass.prio {
                    band = pass.prio;
                    // C `epicsThreadSetPriority` (`dbScan.c:1331-1334`): a
                    // helper carries the band of the rate it serves, or a
                    // fast rate's records would be processed at a slow rate's
                    // priority. `reband_current_thread` is that call, and it
                    // moves the thread's row with it.
                    crate::runtime::task::reband_current_thread(band);
                }
                // The same runtime context the rate's own thread processes in,
                // for the same reason — see [`TickDriver`].
                driver.drive(async {
                    run_slots(
                        &db,
                        pass,
                        &SlotRunner::Helper {
                            wanted: &pool.wanted,
                            // C `runSlots(ppsl, me->serves)`, where the break
                            // mask is `serves & faster` (`dbScan.c:1171`): a
                            // dedicated helper has no faster rate to leave
                            // for, so it finishes the group it is in.
                            faster: serves.mask() & faster_than(ind),
                        },
                    );
                });
                // Given back here and not before — C decrements `slowBusy`
                // after `runSlots` returns (`dbScan.c:1336-1337`), so a helper
                // counts against the cap for as long as it is inside the rate.
                drop(slow);
            }
            None => waiter
                .wait_until(|| pool.shutdown.load(Ordering::Acquire) || pool.offers_work(serves)),
        }
    }
}

/// Build the pool and spawn it — C `spawnHelpers` (`dbScan.c:1352-1432`),
/// which `scanInit` calls before the first `spawnPeriodic` so a leader cannot
/// open a group before there is a pool to wake.
///
/// `rate_helpers` is `scanRateThreads`' per-rate count, indexed like `passes`;
/// `configured` is the shared pool's. `None` when neither asked for a helper,
/// and when there are more rates than a mask has bits for.
fn spawn_helpers(
    db: &Arc<PvDatabase>,
    passes: Box<[Arc<PeriodicPass>]>,
    lengths: &[usize],
    driver: &TickDriver,
    configured: usize,
    configured_reserve: usize,
    rate_helpers: &[usize],
) -> Option<Arc<HelperPool>> {
    let dedicated: Vec<usize> = (0..passes.len())
        .map(|ind| rate_helpers.get(ind).copied().unwrap_or(0))
        .collect();
    let n_rate: usize = dedicated.iter().sum();
    // The mask width is this module's invariant, not the caller's: one bit per
    // rate, in one word.
    let mut n_pool = configured.min(MAX_PARALLEL_THREADS);
    if n_pool + n_rate == 0 {
        return None;
    }
    if passes.len() > MAX_PARALLEL_THREADS {
        crate::runtime::log::errlog_printf(&format!(
            "scanParallelThreads: {} scan rates exceed the {} the helper pool can serve, \
             running without helpers\n",
            passes.len(),
            MAX_PARALLEL_THREADS
        ));
        return None;
    }
    if n_pool + n_rate > MAX_PARALLEL_THREADS {
        // C drops pool helpers rather than dedicated ones (`dbScan.c:1366-1375`):
        // a rate asked for its own by name, the pool is whatever is left over.
        crate::runtime::log::errlog_printf(&format!(
            "scanParallelThreads: {} helpers exceed {}, dropping pool helpers\n",
            n_pool + n_rate,
            MAX_PARALLEL_THREADS
        ));
        n_pool = MAX_PARALLEL_THREADS.saturating_sub(n_rate);
        if n_pool + n_rate > MAX_PARALLEL_THREADS {
            return None;
        }
    }

    // Helpers are kept free for the fastest rate that has records, or for the
    // fastest rate there is when no list has any yet — C `dbScan.c:1377-1384`.
    let fast_rate = (0..passes.len())
        .rev()
        .find(|ind| lengths.get(*ind).copied().unwrap_or(0) > 0)
        .unwrap_or(passes.len().saturating_sub(1));
    // C `slowCap = nPool - nReserveConfigured` (`dbScan.c:1386-1387`), where
    // `scanParallelThreads` has already clamped the reserve to the count.
    let slow_cap = n_pool.saturating_sub(configured_reserve);

    // Dedicated first, so a leader waking by lowest index reaches its own
    // before the pool — C `dbScan.c:1398-1412`.
    let kinds: Vec<Serves> = dedicated
        .iter()
        .enumerate()
        .flat_map(|(ind, n)| std::iter::repeat_n(Serves::Rate(ind), *n))
        .chain(std::iter::repeat_n(Serves::Pool, n_pool))
        .collect();
    let bands: Vec<ThreadPriority> = kinds
        .iter()
        .map(|serves| match serves {
            // C `opts.priority = papPeriodic[lowBit(serves)]->prio`
            // (`dbScan.c:1419-1421`): a dedicated helper is born at its rate's
            // band and never leaves it.
            Serves::Rate(ind) => passes[*ind].prio,
            // C `opts.priority = epicsThreadPriorityScanLow` (`:1420`); the
            // band a pool helper ends up at is the rate it is serving.
            Serves::Pool => ThreadPriority::ScanLow,
        })
        .collect();
    let pool = Arc::new(HelperPool {
        wanted: AtomicUsize::new(0),
        shutdown: AtomicBool::new(false),
        fast_rate,
        slow_cap,
        slow_busy: AtomicUsize::new(0),
        dedicated: dedicated.into_boxed_slice(),
        helpers: kinds
            .into_iter()
            .map(|serves| Helper {
                wake: Event::new(),
                serves,
            })
            .collect(),
        passes,
    });

    for (me, band) in bands.into_iter().enumerate() {
        let db = Arc::clone(db);
        let pool_for_thread = Arc::clone(&pool);
        let driver = driver.clone();
        // A helper that could not be created is C's `spawnHelpers` never
        // reaching the `startStopEvent` it waits for: `iocInit` wedges and the
        // IOC never serves. See the leaders' `MandatoryThread` for why that is
        // this process dying here.
        MandatoryThread::new(
            format!("scanHelper{me}"),
            band,
            // C `opts.stackSize = epicsThreadStackBig` (`dbScan.c:1423`).
            StackSizeClass::Big,
        )
        .spawn(move || {
            helper_loop(db, pool_for_thread, me, driver, band);
        });
    }
    Some(pool)
}

/// RAII owner of the pool's stop transition — C `stopHelpers`
/// (`dbScan.c:1327-1346`), which `scanStop` calls after the leaders.
///
/// C joins its helpers; the port's scan threads are not joined (see
/// [`ScanOwner`]'s teardown), so a helper exits at the flag on its next look.
/// What a helper cannot be left holding is a claim: `run_slots` has no
/// shutdown check inside its loop, so a helper that has claimed slots finishes
/// them and subtracts before it reads the flag again, and a parked helper
/// holds none. That is what lets a leader still waiting on a group reach zero
/// after the pool has been told to stop.
struct HelperStopGuard(Arc<HelperPool>);

impl Drop for HelperStopGuard {
    fn drop(&mut self) {
        self.0.shutdown.store(true, Ordering::Release);
        for helper in &self.0.helpers {
            // `wake`, not `signal`: the condition a helper re-tests here is
            // its own exit, and a signal dropped inside the announce window
            // would leave the thread parked for the life of the process.
            helper.wake.wake();
        }
    }
}

/// C `scanParallelThreadsDefault` (`dbScan.c:167`) — what
/// `scanParallelThreads(0, ...)` resolves to.
///
/// C declares it `8` and, unlike `callbackParallelThreadsDefault`, leaves it
/// there: no registration phase overwrites it with the processor count. It is
/// what the count resolves to, not a pool that exists — helpers are spawned
/// only once `scanParallelThreads` has been called at all.
static PARALLEL_THREADS_DEFAULT: AtomicI32 = AtomicI32::new(8);
/// C `nHelpersConfigured` (`dbScan.c:169`) — already resolved and clamped by
/// the `scanParallelThreads` command, as C resolves it inside the function.
static CONFIGURED_HELPERS: AtomicI32 = AtomicI32::new(0);
/// C `nReserveConfigured` (`dbScan.c:170`).
static CONFIGURED_RESERVE: AtomicI32 = AtomicI32::new(0);
/// C `nRateConfigured` (`dbScan.c:171`) — `scanRateThreads`' per-rate count,
/// keyed by the rate's offset below `SCAN_1ST_PERIODIC`.
///
/// A map where C has an array sized by the mask width: only the rates a
/// startup script named have an entry, and `scanRateThreads` runs before
/// `menuScan` freezes, so the ladder's length is not yet a number this can be
/// sized by.
static CONFIGURED_RATE_HELPERS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());
/// C's `papPeriodic` read as the already-initialised gate (`dbScan.c:288`):
/// non-NULL from `initPeriodic` until `deletePeriodic` frees it. The port has
/// no array to test for it, so the gate is its own cell, set where the passes
/// are built and cleared by [`ScanStopGuard`] — which is what lets one process
/// configure, run and tear down a second IOC, as the C test does three times
/// over.
static PERIODIC_LISTS_BUILT: AtomicBool = AtomicBool::new(false);

/// Read C `scanParallelThreadsDefault`.
pub fn parallel_threads_default() -> i32 {
    PARALLEL_THREADS_DEFAULT.load(Ordering::Relaxed)
}

/// Write C `scanParallelThreadsDefault`.
pub fn set_parallel_threads_default(value: i32) {
    PARALLEL_THREADS_DEFAULT.store(value, Ordering::Relaxed);
}

/// C's `if (papPeriodic)` refusal (`dbScan.c:288-292`) — whether the periodic
/// lists have been built, which is when a helper count can no longer be
/// changed.
pub fn periodic_lists_built() -> bool {
    PERIODIC_LISTS_BUILT.load(Ordering::Acquire)
}

/// C `scanParallelThreads`'s two stores (`dbScan.c:324-325`), minus the
/// arithmetic and the diagnostics around them: the caller owns those, because
/// C prints them from the same function only because C has nowhere else to put
/// them. `reserve` arrives resolved — zero already turned into the default of
/// one and negative into none — and clamped to `count`.
pub fn set_parallel_threads(count: i32, reserve: i32) {
    CONFIGURED_HELPERS.store(count, Ordering::Relaxed);
    CONFIGURED_RESERVE.store(reserve, Ordering::Relaxed);
}

/// C `scanRateThreads`' one store (`dbScan.c:352-353`), minus the rate-name
/// lookup and the diagnostics around it — the command owns those, as it owns
/// `scanParallelThreads`' count arithmetic. `rate` is the offset below
/// `SCAN_1ST_PERIODIC`, which is how `passes` and `wanted` are indexed too.
pub fn set_rate_threads(rate: usize, count: i32) {
    CONFIGURED_RATE_HELPERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(rate, count.max(0) as usize);
}

/// What [`set_rate_threads`] has stored, dense and indexed by rate offset —
/// what the pool builder reads. Outlives one IOC's lifetime, as C's array
/// does: the C test configures three IOCs in one process and has to set every
/// rate on each pass because of it.
pub fn rate_threads() -> Vec<usize> {
    let map = CONFIGURED_RATE_HELPERS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out = vec![0; map.keys().next_back().map_or(0, |last| last + 1)];
    for (rate, count) in map.iter() {
        out[*rate] = *count;
    }
    out
}

/// What [`set_parallel_threads`] last stored.
///
/// C has no accessor for its two statics and needs none — `spawnHelpers` is
/// in the same file. The port's pool builder is in this module too; this
/// exists for the `scanParallelThreads` command's test, which owns the count
/// arithmetic C keeps inside the function and sits in another file, so there
/// is otherwise nothing it can assert on but a side effect three threads
/// away.
pub fn parallel_threads() -> (i32, i32) {
    (
        CONFIGURED_HELPERS.load(Ordering::Relaxed),
        CONFIGURED_RESERVE.load(Ordering::Relaxed),
    )
}

/// One periodic rate's thread body — C `periodicTask`
/// (`dbScan.c:895-935`): sleep to the next deadline, scan the list,
/// repeat until told to stop.
fn periodic_loop(db: Arc<PvDatabase>, duty: PeriodicDuty, stop: Arc<ScanStop>, driver: TickDriver) {
    let scan_type = duty.scan_type;
    let period = duty.period;
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
    let mut overrun = OverrunTracker::new(
        scan_type,
        period,
        Instant::now(),
        Remedy::for_ioc(duty.pool.is_some()),
    );
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
                    periodic_pass(&db, &duty);
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
        // C `initPeriodic` gives every periodic rate its `periodic_scan_list`
        // — including one whose period the quantum check rejected, which then
        // simply never runs a pass (`dbScan.c:1009-1014`). The pass state is
        // built for all of them here for the same reason the pool indexes
        // `papPeriodic` directly: a rate's offset is the bit it owns, so the
        // array must have no holes.
        let passes: Box<[Arc<PeriodicPass>]> = scans
            .iter()
            .enumerate()
            .map(|(ind, scan_type)| Arc::new(PeriodicPass::new(*scan_type, periodic_priority(ind))))
            .collect();
        PERIODIC_LISTS_BUILT.store(true, Ordering::Release);
        // C `spawnHelpers()` picks the rate it keeps helpers free for from
        // `ellCount` of each list (`dbScan.c:1315-1323`), which at this point
        // is what `buildScanLists` just put there.
        let lengths: Vec<usize> = scans
            .iter()
            .map(|scan_type| self.db.scan_list_len(*scan_type))
            .collect();
        // C `scanInit`: `spawnHelpers()` and only then `spawnPeriodic(i)` for
        // each rate (`dbScan.c:279-281`) — a leader must not open a group
        // before there is a pool to wake.
        let pool = spawn_helpers(
            &self.db,
            passes.clone(),
            &lengths,
            driver,
            CONFIGURED_HELPERS.load(Ordering::Relaxed).max(0) as usize,
            CONFIGURED_RESERVE.load(Ordering::Relaxed).max(0) as usize,
            &rate_threads(),
        );
        let helper_guard = pool.as_ref().map(|p| HelperStopGuard(Arc::clone(p)));
        for (ind, scan_type) in scans.into_iter().enumerate() {
            if let Some(period) = scan_type.interval() {
                let db = Arc::clone(&self.db);
                let stop = Arc::clone(&stop);
                let driver = driver.clone();
                let duty = PeriodicDuty {
                    scan_type,
                    period,
                    ind,
                    pass: Arc::clone(&passes[ind]),
                    pool: pool.clone(),
                };
                MandatoryThread::new(
                    periodic_thread_name(period),
                    periodic_priority(ind),
                    // dbScan.c:946 — `opts.stackSize = epicsThreadStackBig`.
                    StackSizeClass::Big,
                )
                .spawn(move || {
                    periodic_loop(db, duty, stop, driver);
                });
            }
        }

        // The threads own the periodic work; this future only keeps the
        // stop guard alive. Cancelling it (tokio::select! or runtime
        // teardown) drops the guard, which trips the stop flag and wakes
        // every scan thread — a thread mid-tick finishes that tick, then
        // exits at the flag check.
        let _guard = guard;
        // C `scanStop` stops the helpers after the leaders (`dbScan.c:248`),
        // and drop order here is that order: the pool outlives the flag that
        // ends the rate threads.
        let _helpers = helper_guard;
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
    /// starting from `base`, on an IOC in the state `remedy` describes.
    /// Returns every warning the tracker emitted.
    fn run(
        period: Duration,
        sweep: Duration,
        ticks: u32,
        base: Instant,
        remedy: Remedy,
    ) -> Vec<String> {
        let mut next = base + period;
        let mut tracker = OverrunTracker::new(ScanType::SEC1, period, base, remedy);
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
        let mut tracker = OverrunTracker::new(ScanType::SEC10, period, base, Remedy::FirstHelper);

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
        let mut tracker = OverrunTracker::new(ScanType::SEC10, period, base, Remedy::FirstHelper);

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
        let mut tracker = OverrunTracker::new(ScanType::SEC1, period, base, Remedy::FirstHelper);

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
            run(period, sweep, 9, base, Remedy::FirstHelper).is_empty(),
            "the ninth consecutive over-run is still silent"
        );

        let warnings = run(period, sweep, 10, base, Remedy::FirstHelper);
        assert_eq!(warnings.len(), 1, "the tenth reports");
        let w = &warnings[0];
        assert!(w.contains("from '1 second' scan thread"), "{w}");
        assert!(w.contains("10 times in a row"), "{w}");
        assert!(w.contains("move some records to a slower scan rate"), "{w}");
    }

    /// BOUNDARY: each of the three remedies C can name (`dbScan.c:988-993`).
    /// An IOC with no helpers cannot be told to give a rate its own one,
    /// because `scanRateThreads` without a pool is not what adds the first
    /// helper; and an IOC with one CPU is told about no helper at all, because
    /// one there could only take turns with the thread that is over-running.
    #[test]
    fn each_remedy_names_only_what_would_help_this_ioc() {
        let base = Instant::now();
        let period = Duration::from_secs(1);
        let sweep = Duration::from_secs(2);
        let tail = |remedy| {
            let warnings = run(period, sweep, 10, base, remedy);
            assert_eq!(warnings.len(), 1, "{remedy:?} reported once");
            let head = "move some records to a slower scan rate";
            let at = warnings[0]
                .find(head)
                .unwrap_or_else(|| panic!("{remedy:?}: {}", warnings[0]));
            warnings[0][at + head.len()..].to_string()
        };

        assert_eq!(
            tail(Remedy::MoreHelpers),
            ",\n\tor add helper threads with scanParallelThreads() or \
             scanRateThreads() before iocInit.\n"
        );
        assert_eq!(
            tail(Remedy::FirstHelper),
            ",\n\tor add helper threads with scanParallelThreads() before iocInit.\n"
        );
        assert_eq!(tail(Remedy::NoHelperWouldHelp), ".\n");
    }

    /// BOUNDARY: which remedy an IOC is in. A helper pool wins over the CPU
    /// count — C tests `nHelpers` first — and the single-CPU state is reachable
    /// only with no pool.
    #[test]
    fn a_helper_pool_decides_the_remedy_before_the_cpu_count_does() {
        assert_eq!(Remedy::for_ioc(true), Remedy::MoreHelpers);
        let alone = crate::runtime::background::callback_executor::cpu_count() <= 1;
        assert_eq!(
            Remedy::for_ioc(false),
            if alone {
                Remedy::NoHelperWouldHelp
            } else {
                Remedy::FirstHelper
            }
        );
    }

    /// BOUNDARY: the report interval doubles after each report
    /// (`dbScan.c:840-843`). Sweeps take 2 s, so reports are gated by a
    /// 10 s then a 20 s interval: ticks 10 and 21. A fixed 10 s interval
    /// would have reported three times over the same span.
    #[test]
    fn the_report_interval_doubles_after_each_report() {
        let base = Instant::now();
        let warnings = run(
            Duration::from_secs(1),
            Duration::from_secs(2),
            21,
            base,
            Remedy::FirstHelper,
        );

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
        let mut tracker = OverrunTracker::new(ScanType::SEC1, period, base, Remedy::FirstHelper);

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

    /// The invariant a parallel pass exists to keep, and the one a sequential
    /// pass keeps for free: **every record of a lower PHAS has finished before
    /// any record of a higher PHAS starts, and no record of the next pass
    /// starts before this one is complete.**
    ///
    /// The predicate is the C prototype's (`dbScanParallelTest.c::fastProc`),
    /// deliberately: a record entering PHAS `k` of pass `p` asserts that PHAS
    /// `k-1` has `nrec*(p+1)` records done and PHAS `k+1` has `nrec*p`
    /// started. Checked from inside the record, where the two neighbours'
    /// counters say what the ordering actually was, rather than from a
    /// transcript afterwards — a pass that interleaved two PHAS for 200 us
    /// leaves no trace in the totals.
    mod parallel_pass {
        use super::*;
        use crate::error::CaResult;
        use crate::server::record::{FieldDesc, ProcessOutcome, Record};
        use crate::types::EpicsValue;
        use std::sync::atomic::AtomicI32;

        /// Records per PHAS of the fast list — the C test's `nrec[]`.
        const NREC: [i32; 3] = [24, 4, 2];
        /// Records on the slow list — the C test's `NSLOW`.
        const NSLOW: i32 = 6;
        /// What a fast record costs. The C test sleeps 1 ms.
        const FAST_WORK: Duration = Duration::from_millis(1);
        /// What a slow record costs, long enough that a pass of the fast list
        /// fits inside one of them several times over.
        const SLOW_WORK: Duration = Duration::from_millis(20);

        #[derive(Default)]
        struct Observed {
            started: [AtomicI32; 3],
            done: [AtomicI32; 3],
            violations: AtomicI32,
            slow_started: AtomicI32,
            /// Helpers inside a slow record right now, and the most there
            /// ever were at once — the C test's `slowHelpersNow` and
            /// `slowHelpersMax`. The second is what the reserve caps.
            slow_helpers_now: AtomicI32,
            slow_helpers_max: AtomicI32,
            /// Distinct threads that processed a PHAS-0 fast record — the C
            /// test's `tids[]`.
            threads: Mutex<Vec<String>>,
        }

        impl Observed {
            fn passes(&self) -> i32 {
                self.started[0].load(Ordering::Acquire) / NREC[0]
            }

            fn note_thread(&self) {
                let me = std::thread::current()
                    .name()
                    .unwrap_or("unnamed")
                    .to_string();
                let mut threads = self.threads.lock().expect("thread list");
                if !threads.contains(&me) {
                    threads.push(me);
                }
            }

            fn violation(&self, what: String) {
                self.violations.fetch_add(1, Ordering::AcqRel);
                eprintln!("PHAS order violation: {what}");
            }
        }

        /// A fast-list record: counts its PHAS in and out, and checks its two
        /// neighbours on the way in.
        struct FastRecord {
            observed: Arc<Observed>,
            phas: usize,
        }

        impl Record for FastRecord {
            fn record_type(&self) -> &'static str {
                "scan_parallel_fast"
            }
            fn process(&mut self) -> CaResult<ProcessOutcome> {
                let o = &self.observed;
                let k = self.phas;
                let started = o.started[k].fetch_add(1, Ordering::AcqRel) + 1;
                let pass = (started - 1) / NREC[k];
                if k > 0 {
                    let want = NREC[k - 1] * (pass + 1);
                    let got = o.done[k - 1].load(Ordering::Acquire);
                    if got != want {
                        o.violation(format!(
                            "PHAS {k} started with done[{}]={got}, expected {want}",
                            k - 1
                        ));
                    }
                }
                if k + 1 < NREC.len() {
                    let want = NREC[k + 1] * pass;
                    let got = o.started[k + 1].load(Ordering::Acquire);
                    if got != want {
                        o.violation(format!(
                            "PHAS {k} started with started[{}]={got}, expected {want}",
                            k + 1
                        ));
                    }
                }
                if k == 0 {
                    o.note_thread();
                }
                std::thread::sleep(FAST_WORK);
                o.done[k].fetch_add(1, Ordering::AcqRel);
                Ok(ProcessOutcome::complete())
            }
            fn get_field(&self, name: &str) -> Option<EpicsValue> {
                (name == "VAL").then_some(EpicsValue::Double(0.0))
            }
            fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
                Ok(())
            }
            fn declared_fields(&self) -> &'static [FieldDesc] {
                &[]
            }
        }

        /// A slow-list record — the C test's `slowProc`: it exists to hold a
        /// helper long enough that the fast rate's next pass finds the pool
        /// busy, and to say which helper held it.
        struct SlowRecord {
            observed: Arc<Observed>,
        }

        impl Record for SlowRecord {
            fn record_type(&self) -> &'static str {
                "scan_parallel_slow"
            }
            fn process(&mut self) -> CaResult<ProcessOutcome> {
                let o = &self.observed;
                o.slow_started.fetch_add(1, Ordering::AcqRel);
                // Only a helper counts against the cap — the rate's own
                // thread is never kept out of its own list, which is why C's
                // `slowProc` tests the thread name too.
                let helper = std::thread::current()
                    .name()
                    .is_some_and(|name| name.starts_with("scanHelper"));
                if helper {
                    let now = o.slow_helpers_now.fetch_add(1, Ordering::AcqRel) + 1;
                    // C walks a compare-and-swap up to the new maximum
                    // (`dbScanParallelTest.c:97-105`); `fetch_max` is that loop.
                    o.slow_helpers_max.fetch_max(now, Ordering::AcqRel);
                }
                std::thread::sleep(SLOW_WORK);
                if helper {
                    o.slow_helpers_now.fetch_sub(1, Ordering::AcqRel);
                }
                Ok(ProcessOutcome::complete())
            }
            fn get_field(&self, name: &str) -> Option<EpicsValue> {
                (name == "VAL").then_some(EpicsValue::Double(0.0))
            }
            fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
                Ok(())
            }
            fn declared_fields(&self) -> &'static [FieldDesc] {
                &[]
            }
        }

        /// The fast rate, and the slow one the cap tests run beside it.
        /// Real ladder entries, so `faster_than` and `HelperPool::pick` see
        /// the indices a running IOC gives them.
        fn rate(scan: ScanType) -> usize {
            periodic_scans()
                .iter()
                .position(|s| *s == scan)
                .expect("a periodic rate")
        }

        async fn load_fast(db: &Arc<PvDatabase>, observed: &Arc<Observed>, scan: ScanType) {
            for (k, n) in NREC.iter().enumerate() {
                for i in 0..*n {
                    let name = format!("fast{k}_{i}");
                    db.add_record(
                        &name,
                        Box::new(FastRecord {
                            observed: Arc::clone(observed),
                            phas: k,
                        }),
                    )
                    .await
                    .expect("load a fast record");
                    {
                        let rec = db.get_record(&name).expect("the record just loaded");
                        let mut inst = rec.write();
                        inst.common.scan = scan;
                        inst.common.phas = k as i16;
                    }
                    db.update_scan_index(&name, ScanType::Passive, scan, 0, k as i16);
                }
            }
        }

        async fn load_slow(db: &Arc<PvDatabase>, observed: &Arc<Observed>, scan: ScanType) {
            for i in 0..NSLOW {
                let name = format!("slow{i}");
                db.add_record(
                    &name,
                    Box::new(SlowRecord {
                        observed: Arc::clone(observed),
                    }),
                )
                .await
                .expect("load a slow record");
                {
                    let rec = db.get_record(&name).expect("the record just loaded");
                    rec.write().common.scan = scan;
                }
                db.update_scan_index(&name, ScanType::Passive, scan, 0, 0);
            }
        }

        /// One rate's thread, running passes back to back until told to stop —
        /// `periodic_loop` with the deadlines taken out, so a test measures
        /// ordering instead of the clock.
        fn leader(
            db: Arc<PvDatabase>,
            duty: PeriodicDuty,
            driver: TickDriver,
            stop: Arc<AtomicBool>,
        ) -> std::thread::JoinHandle<()> {
            let name = periodic_thread_name(duty.period);
            std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        driver.drive(async {
                            periodic_pass(&db, &duty);
                        });
                    }
                })
                .expect("spawn a leader")
        }

        /// Wait for `want` complete passes of the fast list, or fail.
        async fn wait_for_passes(observed: &Observed, want: i32) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while observed.passes() < want {
                assert!(
                    Instant::now() < deadline,
                    "only {} passes in 30 s",
                    observed.passes()
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        /// A pass is complete when every PHAS has started and finished the
        /// same number of passes as PHAS 0 — the C test's `waitPassComplete`.
        async fn wait_pass_complete(observed: &Observed) {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let passes = observed.passes();
                if (0..NREC.len()).all(|k| {
                    observed.started[k].load(Ordering::Acquire) == NREC[k] * passes
                        && observed.done[k].load(Ordering::Acquire) == NREC[k] * passes
                }) {
                    return;
                }
                assert!(Instant::now() < deadline, "a pass was still in flight");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        fn passes_for(rates: &[ScanType]) -> Box<[Arc<PeriodicPass>]> {
            rates
                .iter()
                .enumerate()
                .map(|(ind, scan)| Arc::new(PeriodicPass::new(*scan, periodic_priority(ind))))
                .collect()
        }

        /// Eight helpers on three PHAS groups: the order holds, every record
        /// of every pass ran exactly once, and more than one thread took part
        /// — without which this test would pass on a pool that never woke.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_parallel_pass_keeps_its_phas_order() {
            let fast = ScanType::SEC01;
            let observed = Arc::new(Observed::default());
            let db = Arc::new(PvDatabase::new());
            load_fast(&db, &observed, fast).await;

            let rates = periodic_scans();
            let passes = passes_for(&rates);
            let driver = TickDriver::capture();
            let lengths: Vec<usize> = rates.iter().map(|s| db.scan_list_len(*s)).collect();
            let pool = spawn_helpers(&db, passes.clone(), &lengths, &driver, 8, 0, &[])
                .expect("eight helpers");
            let guard = HelperStopGuard(Arc::clone(&pool));

            let stop = Arc::new(AtomicBool::new(false));
            let ind = rate(fast);
            let thread = leader(
                Arc::clone(&db),
                PeriodicDuty {
                    scan_type: fast,
                    period: fast.interval().expect("a rate"),
                    ind,
                    pass: Arc::clone(&passes[ind]),
                    pool: Some(Arc::clone(&pool)),
                },
                driver.clone(),
                Arc::clone(&stop),
            );

            wait_for_passes(&observed, 3).await;
            stop.store(true, Ordering::Release);
            thread.join().expect("the leader");
            drop(guard);
            wait_pass_complete(&observed).await;

            let passes_ran = observed.passes();
            assert_eq!(
                observed.violations.load(Ordering::Acquire),
                0,
                "a higher PHAS ran before a lower one had finished"
            );
            for k in 0..NREC.len() {
                assert_eq!(
                    observed.started[k].load(Ordering::Acquire),
                    NREC[k] * passes_ran,
                    "PHAS {k} started a different number of records than {passes_ran} passes of it"
                );
                assert_eq!(
                    observed.done[k].load(Ordering::Acquire),
                    observed.started[k].load(Ordering::Acquire),
                    "PHAS {k} left a record unfinished"
                );
            }
            let threads = observed.threads.lock().expect("thread list").clone();
            assert!(
                threads.len() > 1,
                "the helpers never took part: PHAS 0 ran on {threads:?}"
            );
        }

        /// The other boundary of the same mechanism: no pool at all, which is
        /// every IOC that never calls `scanParallelThreads`. The pass is then
        /// the leader's own walk — one thread, same ordering, no barrier
        /// anybody waits at.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_pass_with_no_helpers_is_the_leaders_own_walk() {
            let fast = ScanType::SEC01;
            let observed = Arc::new(Observed::default());
            let db = Arc::new(PvDatabase::new());
            load_fast(&db, &observed, fast).await;

            let rates = periodic_scans();
            let passes = passes_for(&rates);
            let ind = rate(fast);
            let stop = Arc::new(AtomicBool::new(false));
            let thread = leader(
                Arc::clone(&db),
                PeriodicDuty {
                    scan_type: fast,
                    period: fast.interval().expect("a rate"),
                    ind,
                    pass: Arc::clone(&passes[ind]),
                    pool: None,
                },
                TickDriver::capture(),
                Arc::clone(&stop),
            );

            wait_for_passes(&observed, 2).await;
            stop.store(true, Ordering::Release);
            thread.join().expect("the leader");

            assert_eq!(
                observed.violations.load(Ordering::Acquire),
                0,
                "the sequential walk broke its own PHAS order"
            );
            let threads = observed.threads.lock().expect("thread list").clone();
            assert_eq!(
                threads.len(),
                1,
                "a pass with no pool ran on more than the leader: {threads:?}"
            );
        }

        /// Drive the fast and the slow rate together over a pool of `count`
        /// helpers, `reserve` of which the pool keeps off the slower rates,
        /// plus `fast_threads` and `slow_threads` helpers dedicated to the two
        /// rates, and hand back what was observed — the C test's `runWith`.
        ///
        /// The two assertions every case shares: the slow list completed a
        /// pass, and the fast list kept its PHAS order while that happened.
        async fn run_with(
            count: usize,
            reserve: usize,
            fast_threads: usize,
            slow_threads: usize,
        ) -> Arc<Observed> {
            let fast = ScanType::SEC01;
            let slow = ScanType::SEC1;
            let observed = Arc::new(Observed::default());
            let db = Arc::new(PvDatabase::new());
            load_fast(&db, &observed, fast).await;
            load_slow(&db, &observed, slow).await;

            let rates = periodic_scans();
            let passes = passes_for(&rates);
            let driver = TickDriver::capture();
            let lengths: Vec<usize> = rates.iter().map(|s| db.scan_list_len(*s)).collect();
            let mut rate_helpers = vec![0; rates.len()];
            rate_helpers[rate(fast)] = fast_threads;
            rate_helpers[rate(slow)] = slow_threads;
            let pool = spawn_helpers(
                &db,
                passes.clone(),
                &lengths,
                &driver,
                count,
                reserve,
                &rate_helpers,
            )
            .expect("a pool");
            let guard = HelperStopGuard(Arc::clone(&pool));

            let stop = Arc::new(AtomicBool::new(false));
            let threads: Vec<_> = [fast, slow]
                .into_iter()
                .map(|scan| {
                    let ind = rate(scan);
                    leader(
                        Arc::clone(&db),
                        PeriodicDuty {
                            scan_type: scan,
                            period: scan.interval().expect("a rate"),
                            ind,
                            pass: Arc::clone(&passes[ind]),
                            pool: Some(Arc::clone(&pool)),
                        },
                        driver.clone(),
                        Arc::clone(&stop),
                    )
                })
                .collect();

            wait_for_passes(&observed, 3).await;
            stop.store(true, Ordering::Release);
            for thread in threads {
                thread.join().expect("a leader");
            }
            drop(guard);

            assert!(
                observed.slow_started.load(Ordering::Acquire) >= NSLOW,
                "the slow list never completed a pass: {} records",
                observed.slow_started.load(Ordering::Acquire)
            );
            assert_eq!(
                observed.violations.load(Ordering::Acquire),
                0,
                "the fast list broke its PHAS order while a slow list ran"
            );
            observed
        }

        /// What the reserve buys is the prototype's p99 row: with every helper
        /// free to enter a slow record, the fast rate's next pass can find the
        /// whole pool busy and fall back to its leader alone. The reserve is
        /// the ceiling that stops that — at most `count - reserve` helpers are
        /// inside a slower rate at once, which is the C test's
        /// `slowHelpersMax <= cap` check.
        ///
        /// BOUNDARY: `0 < slow_cap < count`. Both halves matter — the ceiling
        /// must hold, and helpers must still reach the slow list, or the
        /// reserve has simply stopped the pool from helping.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn the_reserve_caps_what_a_slower_rate_may_hold() {
            let observed = run_with(4, 2, 0, 0).await;
            let peak = observed.slow_helpers_max.load(Ordering::Acquire);
            assert!(
                peak <= 2,
                "{peak} helpers were inside the slow list at once"
            );
            assert!(peak > 0, "no helper ever reached the slow list");
        }

        /// BOUNDARY: `slow_cap == 0`, which `scanParallelThreads(n, n)` asks
        /// for. No helper may enter a slower rate at all, so that rate is its
        /// own leader's walk while the pool stays available to the fast one.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_full_reserve_keeps_every_helper_out_of_the_slower_rates() {
            let observed = run_with(4, 4, 0, 0).await;
            assert_eq!(
                observed.slow_helpers_max.load(Ordering::Acquire),
                0,
                "a helper entered a slower rate with every helper reserved"
            );
        }

        /// `scanRateThreads` with no pool at all: a rate's passes are shared
        /// by its own helpers and nothing else, which is the determinism an RT
        /// IOC buys with it. The C test's `nthreads == fastT + 1`.
        ///
        /// BOUNDARY: `n_pool == 0` with `n_rate > 0` — the pool exists only
        /// because a rate asked for helpers by name.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn dedicated_helpers_serve_their_own_rate_and_no_other() {
            const FAST_THREADS: usize = 2;
            let observed = run_with(0, 0, FAST_THREADS, 1).await;
            let threads = observed.threads.lock().expect("thread list").clone();
            assert_eq!(
                threads.len(),
                FAST_THREADS + 1,
                "the fast list ran on something other than its leader and its \
                 own {FAST_THREADS} helpers: {threads:?}"
            );
            assert_eq!(
                observed.slow_helpers_max.load(Ordering::Acquire),
                1,
                "the slow rate's one dedicated helper did not serve it alone"
            );
        }

        /// BOUNDARY: a dedicated helper is not under the reserve. With
        /// `reserve == count` no *pool* helper may enter the slow list, yet
        /// the rate's own helper must, or `scanRateThreads` would be undone by
        /// a `scanParallelThreads` on the same IOC.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_dedicated_helper_is_outside_the_reserve() {
            let observed = run_with(2, 2, 0, 1).await;
            assert_eq!(
                observed.slow_helpers_max.load(Ordering::Acquire),
                1,
                "the slow list held something other than its own one helper"
            );
        }
    }

    /// The config gate, both ways: `scanParallelThreads` is answerable before
    /// the periodic lists exist and refused once they do — C's `if
    /// (papPeriodic)` (`dbScan.c:288-292`) — and it is answerable again after
    /// the facility is torn down, because C's `deletePeriodic` leaves
    /// `papPeriodic` NULL and the C test configures three IOCs in one process.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(scan_parallel)]
    async fn a_helper_count_is_refused_only_while_the_lists_exist() {
        assert!(
            !periodic_lists_built(),
            "a process with no scan facility has no periodic lists"
        );
        let db = Arc::new(PvDatabase::new());
        let owner = ScanOwner::start(Arc::clone(&db));

        let deadline = Instant::now() + Duration::from_secs(10);
        while !periodic_lists_built() {
            assert!(
                Instant::now() < deadline,
                "the scan owner never built its periodic lists"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        drop(owner);
        let deadline = Instant::now() + Duration::from_secs(10);
        while periodic_lists_built() {
            assert!(
                Instant::now() < deadline,
                "teardown left the config gate shut"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

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
