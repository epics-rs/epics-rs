//! Database breakpoints — the port of C `dbBkpt.c` (`R7.0.10`,
//! `modules/database/src/ioc/db/dbBkpt.c`), the machinery behind the shell
//! commands `dbb`, `dbd`, `dbc`, `dbs`, `dbstat`, `dbp` and `dbap`.
//!
//! A breakpoint suspends record processing before record support runs. Once
//! suspended, the chain resumes on `dbc` (run to the next breakpoint) or `dbs`
//! (advance one record). The stopped record's fields print with `dbp`, or
//! automatically after every cycle with `dbap`.
//!
//! # Breakpoints are per lock set, and the lock sets are `record_lock.rs`'s
//!
//! Every structure in `dbBkpt.c` is keyed by lock set: `LS_LIST.l_num` is
//! `dbLockGetLockId(precord)` (`:341`), one continuation task is spawned per
//! lock set (`:373`), `dbstat` prints the lock set as its first column
//! (`:906`), and `dbc`/`dbs` with no argument default to the stopped lock set
//! on top of the stack (`FIND_CONT_NODE`, `:197-247`).
//!
//! There is one lock-set partition in this port and `record_lock.rs` owns it:
//! `PvDatabase::build_lock_sets` builds it at `iocInit` and
//! `PvDatabase::relink_lock_sets` is its only mutator afterwards. A lock set's
//! identity is therefore `PvDatabase::lock_set_of(record).id`, which is C's
//! `lockSet::id` — the very field `dbLockGetLockId` reads (`dbLock.c:175-182`)
//! and the one `dblsr` prints (`dbLock.c:886-887`). `dbstat`'s `LSet:` column
//! and `dblsr`'s set id are one number in C because they are one field; they
//! are one number here for the same reason. This module keeps no graph, no
//! walk over the link graph and no derived name of its own — a second model
//! could only
//! disagree with the first.
//!
//! `l_num` is frozen when the `LS_LIST` node is created (`:341`) and never
//! recomputed, while `FIND_LOCKSET` (`:176`) compares it against the record's
//! *current* id. [`LockSet::id`] is frozen at `dbb` for the same reason, so a
//! relink that moves a record to another set makes it stop matching its old
//! node exactly as it does in C.
//!
//! # The continuation thread is C's, not an executor worker
//!
//! C spawns one `bkptCont` task per lock set (`:373`) and suspends *it* with
//! `epicsThreadSuspendSelf()` (`:797`); `dbc`/`dbs` call `epicsThreadResume` on
//! its saved `taskid`. The isolation is the point: a lock set stopped at a
//! breakpoint must not stall the scan threads driving every other lock set.
//!
//! This port keeps that shape exactly, because it is also what makes the
//! suspension legal here. Record processing below
//! `PvDatabase::process_record_with_links` is a *synchronous* recursion —
//! `process_record_with_links_recursive` runs the FLNK/OUT chain as plain calls
//! inside the entry record's gate-held region and must not suspend — so a
//! mid-chain stop cannot be an `.await`; it can only be a parked thread.
//! Parking a runtime worker would be unacceptable, so the chain runs on the
//! lock set's own thread, which exists to be parked. Every other caller does
//! what C's does: queues the entry point, signals that thread, and returns
//! without running record support ([`Before::Skip`]).
//!
//! # What the hooks are
//!
//! C inserts two hooks in `dbProcess`, both guarded by `lset_stack_count != 0`
//! (`dbAccess.c:504-515`, `:614-616`): `dbBkpt()` before record support, whose
//! non-zero return means "skip support and fall out of `dbProcess`", and
//! `dbPrint()` after it. Here they are [`BreakpointTable::before_process`] and
//! [`BreakpointTable::after_process`], reached from
//! `processing.rs::run_process_frame` through `PvDatabase::breakpoints` — an
//! `ArcSwapOption` that holds `None` until the first `dbb`, so a database
//! nobody is debugging pays one relaxed atomic load where C pays one
//! comparison.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use super::PvDatabase;

/// `BKPT` bit 1 — a breakpoint is set in this record (C `dbBkpt.h:77`).
pub const BKPT_ON_MASK: u8 = 0x01;
/// C `dbBkpt.h:78`, the complement used to clear bit 1.
pub const BKPT_OFF_MASK: u8 = 0xFE;
/// `BKPT` bit 2 — print this record after every cycle (C `dbBkpt.h:79`).
pub const BKPT_PRINT_MASK: u8 = 0x02;
/// C `dbBkpt.h:80`, the complement used to clear bit 2.
pub const BKPT_PRINT_OFF_MASK: u8 = 0xFD;

/// C `dbBkpt.h:82` — the entry-point counter saturates rather than wrapping,
/// so a long-lived entry point keeps reporting a plausible count.
const MAX_EP_COUNT: u64 = 99_999;

/// One entry point detected for a lock set — C `struct EP_LIST`
/// (`dbBkpt.h:43-50`).
///
/// An entry point is a record whose processing was started from outside the
/// lock set's own continuation thread: the root of the recursive `dbProcess`
/// the debugger steps through. `dbstat` reports how often each has been seen
/// and how long it has been known.
#[derive(Debug, Clone)]
struct EntryPoint {
    name: String,
    /// C `pqe->count`.
    count: u64,
    /// C `pqe->time`, stamped when the entry point is first logged.
    first_seen: Instant,
    /// C `pqe->sched` — queued for the continuation thread's next pass.
    scheduled: bool,
}

/// What `run_process_frame` must do, decided by
/// [`BreakpointTable::before_process`].
///
/// C's `dbBkpt()` says the same thing as an `int`: zero to run record support,
/// non-zero to skip it and fall out of `dbProcess` (`dbBkpt.c:665-671`).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Before {
    /// Run record support. C's `return 0`.
    Run,
    /// Skip record support and return. C's `return 1` — either the entry point
    /// was handed to the continuation thread (`:751`), or `pact` is set and C
    /// deliberately skips the rest of `dbProcess` so no alarms fire (`:761`).
    Skip,
}

/// A lock set that holds breakpoints, or is stopped at one — C `struct LS_LIST`
/// (`dbBkpt.h:56-67`).
struct LockSet {
    /// C `pnode->l_num`: `dbLockGetLockId(precord)` frozen at `dbb` (`:341`).
    id: u64,
    /// C `pnode->precord` — where execution is currently stopped, if it is.
    /// Cleared by the continuation thread after a queue pass (`:615`), never
    /// by `dbc`/`dbs`, exactly as in C.
    stopped_at: Option<String>,
    /// C `pnode->current_ep` — the entry point the stopped record was reached
    /// through, named in the "within Entrypoint" half of the stop message.
    current_ep: Option<String>,
    /// C `pnode->bp_list`, in insertion order: `dbstat` prints it in that
    /// order and `dbd` removes by identity.
    breakpoints: Vec<String>,
    /// C `pnode->ep_queue`.
    ep_queue: Vec<EntryPoint>,
    /// C `pnode->step` — stop at the next record, not just at a breakpoint.
    step: bool,
    /// C `pnode->taskid`. `Some` from the moment `dbb` spawns this lock set's
    /// continuation thread — which is the whole of what the spawn guard and
    /// the "is this my own thread" test ask of it — and the cell holds that
    /// thread's `EPICS ID`, the same handle `epicsThreadShowAll` prints, once
    /// the thread's own prologue has published it. Zero until then, and only
    /// then, which is C's null taskid before `epicsThreadCreate` returns.
    ///
    /// One cell rather than an id beside a flag: C has one `taskid` and reads
    /// it for both questions, and a private ordinal here would have given the
    /// continuation thread a second identity the thread commands cannot name.
    cont: Option<Arc<AtomicU64>>,
    /// C `pnode->ex_sem`, the execution semaphore: signalled when an entry
    /// point is scheduled, and by `dbd` when the last breakpoint goes so the
    /// thread wakes to find its loop condition false.
    ex: Arc<BinarySemaphore>,
}

/// C `epicsEventCreate(epicsEventEmpty)` — a binary semaphore. Signalling one
/// that is already full is a no-op, and that idempotence is load-bearing: C
/// signals `ex_sem` once per scheduling and the continuation task answers by
/// walking the whole queue, so two signals before one pass must not produce
/// two passes.
#[derive(Default)]
struct BinarySemaphore {
    full: Mutex<bool>,
    cv: Condvar,
}

impl BinarySemaphore {
    fn signal(&self) {
        let mut full = self.full.lock().expect("breakpoint semaphore poisoned");
        *full = true;
        self.cv.notify_one();
    }

    /// C `epicsEventMustWait`.
    fn wait(&self) {
        let mut full = self.full.lock().expect("breakpoint semaphore poisoned");
        while !*full {
            full = self.cv.wait(full).expect("breakpoint semaphore poisoned");
        }
        *full = false;
    }
}

/// C `epicsThreadResume(pnode->taskid)` — what `dbc` (`dbBkpt.c:518`), `dbs`
/// (`:547`) and the last `dbd` do to a continuation thread parked at a
/// breakpoint.
///
/// The park itself is the thread registry's, not this module's: the
/// continuation thread suspends itself with `runtime::task::suspend_self`, so
/// the cell `epicsThreadShowAll`'s `STATE` column prints and the cell this
/// clears are the same one. A `SUSPEND` row and an `epicsThreadResume` that
/// refuses therefore cannot disagree, and `epicsThreadResume bkptCont` does
/// exactly what `dbc` does, as in C.
///
/// A zero cell is a thread that has not reached its registry prologue yet, so
/// there is nothing suspended to resume.
fn resume_continuation(cont: Option<&Arc<AtomicU64>>) {
    let Some(taskid) = cont else { return };
    let id = taskid.load(Ordering::Relaxed);
    if id != 0 {
        crate::runtime::task::resume_thread(id);
    }
}

/// The breakpoint stack — C's `lset_stack` plus the `bkpt_stack_sem` that
/// guards it (`dbBkpt.c:156-157`) and the `last_lset` default (`:165`).
///
/// One mutex over the whole stack, as C has one semaphore: every operation is
/// either a debugger command or a hook on a record already known to be in a
/// breakpointed lock set, so there is no contention to spread.
pub struct BreakpointTable {
    stack: Mutex<Stack>,
    /// What `dbPrint` prints with — C calls `dbpr(precord->name, 2)` directly
    /// (`dbBkpt.c:820`), which here needs the shell's sync→async bridge and
    /// its field walk. The mechanism holds the slot; `dbb` fills it, so this
    /// module owes nothing to an output channel and stays testable without
    /// one. `None` until then, and an auto-print is silently skipped, which is
    /// also what C does before `iocInit` wires `dbpr`.
    printer: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
}

#[derive(Default)]
struct Stack {
    /// Front is the top: C moves a lock set to the head when it stops
    /// (`dbBkpt.c:786-787`), and `FIND_CONT_NODE` takes the first stopped
    /// entry as the no-argument default.
    sets: VecDeque<LockSet>,
    /// C `last_lset` — `dbc`/`dbs` announce a change of lock set.
    last: Option<u64>,
}

// Names the lock set this thread is the continuation thread for. C
// discriminates with `epicsThreadGetIdSelf() != pnode->taskid`
// (`dbBkpt.c:701`); a thread-local is the same test without a task handle.
thread_local! {
    static CONTINUATION_FOR: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
}

/// Why an operation could not be performed, carrying C's exact message.
///
/// The caller prints it, so the machinery owes nothing to an output channel.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BkptError {
    /// C `dbBkpt.c:296-299`: `S_db_bkptSet`.
    AlreadySet,
    /// C `:421-424`: `S_db_bkptNotSet`.
    NotSet,
    /// C `:290`, `:416`: the name did not resolve.
    NotFound(String),
    /// C `:242`: `S_db_notStopped` for a named lock set.
    NotStopped,
    /// C `:224`: no record is stopped anywhere.
    NoneStopped,
    /// C `:436`, `:461`: `S_db_bkptLogic`. The record carries the BKPT bit but
    /// its lock set is not on the stack. Reachable in C too, and for the same
    /// reason: `FIND_LOCKSET` matches a node's frozen `l_num` against the
    /// record's current id, so a relink that moves the record to another set
    /// orphans its node. Handled the way C does — clear the bit and report.
    Logic,
}

impl BkptError {
    /// The exact line C prints before returning this status.
    pub fn message(&self) -> String {
        match self {
            Self::AlreadySet => "   BKPT> Breakpoint already set in this record".into(),
            Self::NotSet => "   BKPT> No breakpoint set in this record".into(),
            Self::NotFound(name) => format!("   BKPT> Record {name} not found"),
            Self::NotStopped => "   BKPT> Currently not stopped in this lockset".into(),
            Self::NoneStopped => "   BKPT> No records are currently stopped".into(),
            Self::Logic => "   BKPT> Logic Error in dbd()".into(),
        }
    }
}

/// One line of `dbstat` output. The machinery returns the report as data
/// rather than printing it, so `dbstat` renders to the shell's own sink.
#[derive(Debug, PartialEq, Clone)]
pub enum StatLine {
    /// C `dbBkpt.c:906-907` (stopped) and `:918-919` (not).
    LockSet {
        id: u64,
        stopped_at: Option<String>,
        breakpoint_count: usize,
        task: Option<u64>,
    },
    /// C `:915-916`. `elapsed_secs` is what C puts under the `C/S:` heading:
    /// `epicsTimeDiffInSeconds(&time, &pqe->time)`, the age of the entry, not
    /// a rate. The column heading and the value disagree in C; reproducing the
    /// value is what functional equivalence means here.
    Entrypoint {
        name: String,
        count: u64,
        elapsed_secs: f64,
    },
    /// C `:927-935`.
    Breakpoint { name: String, autoprint: bool },
}

impl StatLine {
    /// C's `printf` for this line, without the trailing newline.
    pub fn render(&self) -> String {
        match self {
            Self::LockSet {
                id,
                stopped_at: Some(rec),
                breakpoint_count,
                task,
            } => format!(
                "LSet: {id}  Stopped at: {:<28.28}  #B: {:05}  T: {}",
                rec,
                breakpoint_count,
                render_task(*task),
            ),
            Self::LockSet {
                id,
                stopped_at: None,
                breakpoint_count,
                task,
            } => format!(
                "LSet: {id}                                            #B: {:05}  T: {}",
                breakpoint_count,
                render_task(*task),
            ),
            Self::Entrypoint {
                name,
                count,
                elapsed_secs,
            } => format!(
                "             Entrypoint: {:<28.28}  #C: {:05}  C/S: {:7.1}",
                name, count, elapsed_secs
            ),
            Self::Breakpoint { name, autoprint } => format!(
                "             Breakpoint: {:<28.28}{}",
                name,
                if *autoprint { " (ap)" } else { "" }
            ),
        }
    }
}

/// C prints `pnode->taskid` with `%p`. This port has no thread pointer, so the
/// column carries the continuation thread's ordinal in the same fixed-width
/// shape; `(nil)` matches glibc's rendering of the null C prints before the
/// thread is spawned.
fn render_task(task: Option<u64>) -> String {
    match task {
        Some(id) => format!("0x{id:x}"),
        None => "(nil)".into(),
    }
}

/// C `dbLockGetLockId(precord)` (`dbLock.c:175-182`) — the id of the lock set
/// `record` is behind, or `None` when it is behind none.
///
/// `None` is C dereferencing a null `precord->lset`: no record has a
/// `lockRecord` until `dbLockInitRecords` runs, so in C every use of this
/// before `iocInit` is undefined. Here it is an absent identity, and the
/// callers refuse rather than invent one.
fn lockset_id(db: &PvDatabase, record: &str) -> Option<u64> {
    let name = canonical_record_name(db, record)?;
    db.lock_set_of(&name).map(|set| set.id)
}

/// The canonical name of `name`, or `None` when no such record exists.
/// C's `dbNameToAddr` half of every command's preamble.
fn canonical_record_name(db: &PvDatabase, name: &str) -> Option<String> {
    let name = db.resolve_alias(name).unwrap_or_else(|| name.to_string());
    db.get_record_no_resolve(&name).map(|_| name)
}

impl BreakpointTable {
    pub fn new() -> Self {
        Self {
            stack: Mutex::new(Stack::default()),
            printer: Mutex::new(None),
        }
    }

    /// Install what `dbPrint` prints with. Idempotent by last-writer-wins;
    /// `dbb` calls it on every invocation so a bridge captured by a later
    /// shell replaces one whose runtime has gone.
    pub fn set_printer(&self, printer: Arc<dyn Fn(&str) + Send + Sync>) {
        *self.printer.lock().expect("breakpoint printer poisoned") = Some(printer);
    }

    fn lock(&self) -> MutexGuard<'_, Stack> {
        self.stack.lock().expect("breakpoint stack poisoned")
    }

    /// True when no lock set is left — C's `lset_stack_count == 0`, the
    /// condition under which `dbProcess` stops calling the hooks.
    pub fn is_empty(&self) -> bool {
        self.lock().sets.is_empty()
    }

    /// C `dbBkpt()` (`dbBkpt.c:665-802`), called from `run_process_frame`
    /// before the record gate is taken and before record support runs.
    ///
    /// The order of the tests is C's and is load-bearing — C says so at
    /// `:672-675`. In particular `pact` is checked *after* entry-point
    /// queuing, so the entry-point statistics `dbstat` reports count an async
    /// record's cycles rather than only its completions (C `:754-758`).
    pub fn before_process(&self, db: &PvDatabase, record: &str) -> Before {
        {
            let mut stack = self.lock();
            let Some(idx) = stack.index_of_record(db, record) else {
                // No breakpoints in this record's lock set. C `:684-687`.
                return Before::Run;
            };

            // C `:690-702`: a disabled record does not stop, but does return 0
            // so `dbProcess` still raises its disable alarm.
            //
            // Deviation: C re-reads SDIS here (`dbGetLink(&precord->sdis, …)`)
            // and this reads the DISA left by the previous cycle, because the
            // SDIS fetch below in `process_record_with_links_body` is the one
            // owner of that link and re-fetching from a hook would double a
            // link read C performs once. The window is one cycle of a record
            // whose SDIS changed since it last processed.
            let (disa, disv, pact) = match db.get_record(record) {
                Some(rec) => {
                    let inst = rec.read();
                    (inst.common.disa, inst.common.disv, inst.is_processing())
                }
                None => return Before::Run,
            };
            if disa == disv {
                return Before::Run;
            }

            // C `:713-751`: processing that did not come from this lock set's
            // continuation thread queues the entry point and drops out of
            // dbProcess without running record support, so the continuation
            // thread — the one that may be parked — owns every cycle in a lock
            // set being debugged.
            let id = stack.sets[idx].id;
            let mine = stack.sets[idx].cont.is_none()
                || CONTINUATION_FOR.with(|c| *c.borrow() == Some(id));
            if !mine {
                stack.sets[idx].note_entrypoint(record, !pact);
                if !pact {
                    let ex = stack.sets[idx].ex.clone();
                    drop(stack);
                    ex.signal();
                }
                return Before::Skip;
            }

            // C `:756-761`: with pact set, skip the rest of dbProcess so no
            // alarms fire. Checked after queuing, never before.
            if pact {
                return Before::Skip;
            }

            // C `:764-771`: a breakpoint turns stepping on.
            if record_bkpt(db, record) & BKPT_ON_MASK != 0 {
                stack.sets[idx].step = true;
            }
            // C `:777`: stop only while stepping.
            if !stack.sets[idx].step {
                return Before::Run;
            }
            stack.sets[idx].stopped_at = Some(record.to_string());
            // C `:778-780` prints the stop banner with the entry point it was
            // reached through, then leaves the `-> ` prompt hanging.
            let ep = stack.sets[idx].current_ep.clone().unwrap_or_default();
            println!("\n   BKPT> Stopped at:  {record}  within Entrypoint:  {ep}\n-> ");
            // C `:786-787` — the stopped lock set becomes the default for a
            // `dbc`/`dbs` with no argument.
            let node = stack.sets.remove(idx).expect("index just used");
            stack.sets.push_front(node);
        };

        // Parked with the stack mutex dropped and no record gate taken, which
        // is C's state at `:794-796`: it releases both before suspending so
        // the debugger commands still work while a record is stopped. The
        // thread marks itself suspended here and nowhere else, so what
        // `epicsThreadShowAll` reports is this call, not a flag beside it.
        crate::runtime::task::suspend_self();
        Before::Run
    }

    /// C `dbPrint()` (`dbBkpt.c:806-822`), called after record support.
    ///
    /// Prints the record at interest level 2 through the installed printer,
    /// C's `dbpr(precord->name, 2)`.
    pub fn after_process(&self, db: &PvDatabase, record: &str) {
        if record_bkpt(db, record) & BKPT_PRINT_MASK == 0 {
            return;
        }
        // C `:814-817`: no print unless the lock set currently holds
        // breakpoints, so a stale BKPT bit on an undebugged record is silent.
        {
            let mut stack = self.lock();
            if stack.index_of_record(db, record).is_none() {
                return;
            }
        }
        // Printed with neither the stack nor any record lock held: the printer
        // walks the record's whole field table and would otherwise re-enter
        // the read lock the walk needs.
        let printer = self
            .printer
            .lock()
            .expect("breakpoint printer poisoned")
            .clone();
        if let Some(printer) = printer {
            printer(record);
        }
    }

    /// C `dbb()` (`dbBkpt.c:274-386`) — set a breakpoint.
    ///
    /// `spawn` builds the lock set's continuation thread, and is called only
    /// for the first breakpoint in a lock set (C `:361-380`). It is a
    /// parameter so the mechanism is testable without a thread; the shell
    /// command passes the real spawner.
    pub fn set(
        &self,
        db: &PvDatabase,
        record: &str,
        spawn: impl FnOnce(u64, Arc<BinarySemaphoreHandle>),
    ) -> Result<(), BkptError> {
        let Some(name) = canonical_record_name(db, record) else {
            return Err(BkptError::NotFound(record.to_string()));
        };
        if record_bkpt(db, &name) & BKPT_ON_MASK != 0 {
            return Err(BkptError::AlreadySet);
        }

        // C reaches `dbLockGetLockId` only through `dbNameToAddr`, and before
        // `iocInit` that pointer chain ends in a null `lset`; a record with no
        // lock set is as unusable to `dbb` as one the database does not have.
        let Some(id) = lockset_id(db, &name) else {
            return Err(BkptError::NotFound(record.to_string()));
        };
        let mut stack = self.lock();
        let idx = match stack.sets.iter().position(|s| s.id == id) {
            Some(i) => i,
            None => {
                stack.sets.push_back(LockSet {
                    id,
                    stopped_at: None,
                    current_ep: None,
                    breakpoints: Vec::new(),
                    ep_queue: Vec::new(),
                    step: false,
                    cont: None,
                    ex: Arc::new(BinarySemaphore::default()),
                });
                stack.sets.len() - 1
            }
        };
        stack.sets[idx].breakpoints.push(name.clone());
        set_record_bkpt(db, &name, |b| b | BKPT_ON_MASK);

        if stack.sets[idx].cont.is_none() {
            let taskid = Arc::new(AtomicU64::new(0));
            stack.sets[idx].cont = Some(taskid.clone());
            let handle = Arc::new(BinarySemaphoreHandle {
                ex: stack.sets[idx].ex.clone(),
                taskid,
            });
            drop(stack);
            spawn(id, handle);
        }
        Ok(())
    }

    /// C `dbd()` (`dbBkpt.c:399-479`) — delete a breakpoint.
    pub fn clear(&self, db: &PvDatabase, record: &str) -> Result<(), BkptError> {
        let Some(name) = canonical_record_name(db, record) else {
            return Err(BkptError::NotFound(record.to_string()));
        };
        if record_bkpt(db, &name) & BKPT_ON_MASK == 0 {
            return Err(BkptError::NotSet);
        }

        let mut stack = self.lock();
        let mut found = stack.index_of_record(db, &name);
        if let Some(i) = found {
            if !stack.sets[i].breakpoints.iter().any(|b| b == &name) {
                found = None;
            }
        }
        let Some(idx) = found else {
            // C `:431-439` / `:456-462`: clear the bit anyway and report.
            set_record_bkpt(db, &name, |b| b & BKPT_OFF_MASK);
            return Err(BkptError::Logic);
        };
        stack.sets[idx].breakpoints.retain(|b| b != &name);
        set_record_bkpt(db, &name, |b| b & BKPT_OFF_MASK);

        // C `:469-471`: the last breakpoint gone signals the execution
        // semaphore so the continuation task wakes, finds its loop condition
        // false, and frees the node. Both signals are needed here: a task
        // parked at a breakpoint is waiting on `park`, not on `ex`.
        if stack.sets[idx].breakpoints.is_empty() {
            let ex = stack.sets[idx].ex.clone();
            let cont = stack.sets[idx].cont.clone();
            stack.sets[idx].step = false;
            drop(stack);
            resume_continuation(cont.as_ref());
            ex.signal();
        }
        Ok(())
    }

    /// C `dbc()` (`dbBkpt.c:489-518`) — continue: stepping off, then resume.
    pub fn cont(&self, db: &PvDatabase, record: Option<&str>) -> Result<Option<String>, BkptError> {
        self.resume(db, record, false)
    }

    /// C `dbs()` (`dbBkpt.c:528-556`) — step. Unlike `dbc` it leaves stepping
    /// on, so the next record in the chain stops too.
    pub fn step(&self, db: &PvDatabase, record: Option<&str>) -> Result<Option<String>, BkptError> {
        self.resume(db, record, true)
    }

    fn resume(
        &self,
        db: &PvDatabase,
        record: Option<&str>,
        stepping: bool,
    ) -> Result<Option<String>, BkptError> {
        let mut stack = self.lock();
        let (idx, _) = stack.find_cont_node(db, record)?;
        let id = stack.sets[idx].id;

        // C `:506-507` / `:543-544`: announced only for the no-argument form,
        // and only when the default has moved to a different lock set. C names
        // `pnode->precord`, the stopped record, not the argument.
        let announce = if record.is_none() && stack.last != Some(id) {
            stack.sets[idx].stopped_at.as_ref().map(|s| {
                if stepping {
                    format!("   BKPT> Stepping:    {s}")
                } else {
                    format!("   BKPT> Continuing:  {s}")
                }
            })
        } else {
            None
        };
        stack.last = Some(id);

        // C `dbc` clears `step`; `dbs` leaves it, and it is already set
        // because only a stepping lock set can be stopped.
        if !stepping {
            stack.sets[idx].step = false;
        }
        let cont = stack.sets[idx].cont.clone();
        drop(stack);
        resume_continuation(cont.as_ref());
        Ok(announce)
    }

    /// C `dbstat()` (`dbBkpt.c:884-940`), as data.
    pub fn status(&self, db: &PvDatabase) -> Vec<StatLine> {
        let now = Instant::now();
        let stack = self.lock();
        let mut out = Vec::new();
        for set in &stack.sets {
            out.push(StatLine::LockSet {
                id: set.id,
                stopped_at: set.stopped_at.clone(),
                breakpoint_count: set.breakpoints.len(),
                // Zero is "spawned, not yet on the thread list"; C's `%p` of
                // a taskid it has not stored yet prints `(nil)` the same way.
                task: set
                    .cont
                    .as_ref()
                    .map(|t| t.load(Ordering::Relaxed))
                    .filter(|&id| id != 0),
            });
            // C prints the entry-point block only for a stopped lock set
            // (`:905-917`), and only for entries whose elapsed time is
            // non-zero — the one line C suppresses.
            if set.stopped_at.is_some() {
                for ep in &set.ep_queue {
                    let elapsed = now.saturating_duration_since(ep.first_seen).as_secs_f64();
                    if elapsed != 0.0 {
                        out.push(StatLine::Entrypoint {
                            name: ep.name.clone(),
                            count: ep.count,
                            elapsed_secs: elapsed,
                        });
                    }
                }
            }
            for bp in &set.breakpoints {
                out.push(StatLine::Breakpoint {
                    name: bp.clone(),
                    autoprint: record_bkpt(db, bp) & BKPT_PRINT_MASK != 0,
                });
            }
        }
        out
    }

    /// The record `dbp` prints — C `dbp()`'s `FIND_CONT_NODE` half
    /// (`dbBkpt.c:825-845`). With a name it is the *named* record; with none,
    /// the stopped one.
    pub fn print_target(&self, db: &PvDatabase, record: Option<&str>) -> Result<String, BkptError> {
        let stack = self.lock();
        let (_, target) = stack.find_cont_node(db, record)?;
        Ok(target)
    }
}

impl Default for BreakpointTable {
    fn default() -> Self {
        Self::new()
    }
}

impl LockSet {
    /// C `:707-739` — add the entry point if new, otherwise bump its count,
    /// and mark it scheduled when `pact` is clear.
    fn note_entrypoint(&mut self, record: &str, schedule: bool) {
        match self.ep_queue.iter_mut().find(|e| e.name == record) {
            Some(ep) => {
                if ep.count < MAX_EP_COUNT {
                    ep.count += 1;
                }
                if schedule {
                    ep.scheduled = true;
                }
            }
            None => self.ep_queue.push(EntryPoint {
                name: record.to_string(),
                count: 1,
                first_seen: Instant::now(),
                scheduled: schedule,
            }),
        }
    }
}

impl Stack {
    /// The lock set holding `record` — C's `FIND_LOCKSET` (`:176`), which
    /// compares each node's frozen `l_num` against the record's current id.
    fn index_of_record(&mut self, db: &PvDatabase, record: &str) -> Option<usize> {
        if self.sets.is_empty() {
            return None;
        }
        let id = lockset_id(db, record)?;
        self.sets.iter().position(|s| s.id == id)
    }

    /// C `FIND_CONT_NODE` (`dbBkpt.c:197-247`): with no name, the first
    /// stopped lock set on the stack and its stopped record; with a name, that
    /// record's lock set — which must be stopped — and the named record
    /// itself, which is what C hands back and `dbp` then prints.
    fn find_cont_node(
        &self,
        db: &PvDatabase,
        record: Option<&str>,
    ) -> Result<(usize, String), BkptError> {
        match record {
            None => self
                .sets
                .iter()
                .position(|s| s.stopped_at.is_some())
                .map(|i| {
                    (
                        i,
                        self.sets[i]
                            .stopped_at
                            .clone()
                            .expect("position matched Some"),
                    )
                })
                .ok_or(BkptError::NoneStopped),
            Some(name) => {
                let canonical = canonical_record_name(db, name)
                    .ok_or_else(|| BkptError::NotFound(name.to_string()))?;
                let id = lockset_id(db, &canonical).ok_or(BkptError::NotStopped)?;
                let idx = self
                    .sets
                    .iter()
                    .position(|s| s.id == id)
                    .ok_or(BkptError::NotStopped)?;
                if self.sets[idx].stopped_at.is_none() {
                    return Err(BkptError::NotStopped);
                }
                Ok((idx, canonical))
            }
        }
    }
}

fn record_bkpt(db: &PvDatabase, record: &str) -> u8 {
    db.get_record(record)
        .map(|r| r.read().common.bkpt)
        .unwrap_or(0)
}

fn set_record_bkpt(db: &PvDatabase, record: &str, f: impl FnOnce(u8) -> u8) {
    if let Some(rec) = db.get_record(record) {
        let mut inst = rec.write();
        inst.common.bkpt = f(inst.common.bkpt);
    }
}

/// C `dbap()` (`dbBkpt.c:848-881`) — toggle auto-print, returning C's line.
pub fn toggle_autoprint(db: &PvDatabase, record: &str) -> Result<String, BkptError> {
    let Some(name) = canonical_record_name(db, record) else {
        return Err(BkptError::NotFound(record.to_string()));
    };
    let on = record_bkpt(db, &name) & BKPT_PRINT_MASK == 0;
    set_record_bkpt(db, &name, |b| {
        if on {
            b | BKPT_PRINT_MASK
        } else {
            b & BKPT_PRINT_OFF_MASK
        }
    });
    Ok(if on {
        format!("   BKPT> Auto print on for record {name}")
    } else {
        format!("   BKPT> Auto print off for record {name}")
    })
}

/// What a continuation thread is handed at spawn: C `pnode->ex_sem`, and the
/// cell the thread publishes its own `epicsThreadId` into.
///
/// The cell rides here rather than in the spawn callback's signature because
/// this is already the one value that crosses from `dbb` to the thread, and
/// the publication has to happen on the thread — the `EPICS ID` is assigned by
/// the thread's own registry prologue, so `dbb` cannot know it at spawn.
pub struct BinarySemaphoreHandle {
    ex: Arc<BinarySemaphore>,
    taskid: Arc<AtomicU64>,
}

/// Claim the calling thread as lock set `id`'s continuation thread.
///
/// C stores `pnode->taskid` from `epicsThreadCreate`'s return (`dbBkpt.c:373`)
/// and compares against it to decide whether `dbProcess` came from the
/// breakpoint task (`:701`); those are one fact, so both halves are set here
/// and nowhere else. Setting only the marker would give a thread the hook
/// recognises but `dbc` cannot resume, and setting only the id would give the
/// reverse.
///
/// The registry assigns the `EPICS ID`, so only the thread itself can publish
/// it — which is also why `dbb` cannot fill this in at spawn time. The cell
/// holds 0 from `dbb` until this runs, and `dbstat` renders that window as
/// C's `(nil)`.
fn claim_continuation(id: u64, taskid: &AtomicU64) {
    CONTINUATION_FOR.with(|c| *c.borrow_mut() = Some(id));
    taskid.store(crate::runtime::task::current_thread_id(), Ordering::Relaxed);
}

/// The body of one continuation thread — C `dbBkptCont` (`dbBkpt.c:561-634`).
///
/// Waits on the execution semaphore, walks the entry-point queue processing
/// every scheduled entry, and repeats until the lock set has no breakpoints
/// left. `db` is a cheap `Arc` clone, and the thread marks itself so the
/// hook's "is this my own thread" test — C's `taskid` comparison at `:701` —
/// answers yes.
///
/// Returns C's closing line, for the caller to print (`:628`).
pub fn continuation_loop(db: PvDatabase, id: u64, handle: Arc<BinarySemaphoreHandle>) -> String {
    claim_continuation(id, &handle.taskid);
    loop {
        handle.ex.wait();

        let Some(table) = db.breakpoints() else { break };
        // The scheduled entries, snapshotted under the stack lock: processing
        // one takes the record gate and may park, and neither may happen while
        // this thread holds the stack C releases at `:585`.
        let scheduled: Vec<String> = {
            let stack = table.lock();
            let Some(set) = stack.sets.iter().find(|s| s.id == id) else {
                break;
            };
            set.ep_queue
                .iter()
                .filter(|e| e.scheduled)
                .map(|e| e.name.clone())
                .collect()
        };

        for entry in scheduled {
            {
                let mut stack = table.lock();
                let Some(set) = stack.sets.iter_mut().find(|s| s.id == id) else {
                    break;
                };
                // C `:601` saves the entry point before processing so the stop
                // banner can name it.
                set.current_ep = Some(entry.clone());
            }
            // C `:604-606`: lock the lock set, process, unlock. This is the
            // one call in the port that may park inside, which is why it runs
            // here and not on a runtime worker.
            let _ = db.process_record_for_breakpoint(&entry);
            let mut stack = table.lock();
            if let Some(set) = stack.sets.iter_mut().find(|s| s.id == id) {
                // C `:609-610`: reset schedule and stepping AFTER processing.
                if let Some(ep) = set.ep_queue.iter_mut().find(|e| e.name == entry) {
                    ep.scheduled = false;
                }
                set.step = false;
            }
        }

        // C `:615` — no record is at a breakpoint any more.
        let mut stack = table.lock();
        let Some(idx) = stack.sets.iter().position(|s| s.id == id) else {
            break;
        };
        stack.sets[idx].stopped_at = None;
        if stack.sets[idx].breakpoints.is_empty() {
            stack.sets.remove(idx);
            break;
        }
    }
    CONTINUATION_FOR.with(|c| *c.borrow_mut() = None);
    // C `:619` decrements `lset_stack_count`; here the whole observer goes
    // once the last lock set does, so the hot path is a `None` load again.
    db.retire_breakpoints_if_idle();
    format!("\n   BKPT> End debug of lockset {id}\n-> ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;
    use std::collections::HashSet;

    /// `dbb` is only usable after `iocInit`, so every fixture ends with
    /// `build_lock_sets` — C's `dbLockInitRecords` and the `dbLockSetMerge`
    /// each DB link performs as it is opened.
    async fn db_with(records: &[&str]) -> PvDatabase {
        let db = PvDatabase::new();
        for name in records {
            db.add_record(name, Box::new(AiRecord::new(0.0)))
                .await
                .expect("add_record");
        }
        db.build_lock_sets();
        db
    }

    /// Both halves of a forward link: the raw field `build_lock_sets` reads
    /// through `record_link_fields`, and the parsed link the processing chain
    /// dispatches on. A test that sets only one of them measures only one.
    ///
    /// Rebuilding afterwards is C's per-link `dbLockSetMerge`: the write is a
    /// direct field poke, not the put path that would hand out a
    /// `LockSetEdit`, so the merge has to be asked for. Merging is idempotent,
    /// which is why a second call costs nothing.
    fn set_flnk(db: &PvDatabase, from: &str, to: &str) {
        {
            let rec = db.get_record(from).expect("record");
            let mut inst = rec.write();
            inst.common.flnk = to.to_string();
            inst.parsed_flnk = crate::server::record::parse_link_field(
                to,
                crate::server::record::LinkFieldType::Fwd,
            );
        }
        db.build_lock_sets();
    }

    /// The identity `dbb` keys a lock set by must be the one `dblsr` prints,
    /// and must not depend on which member `dbb` names — two `dbb`s in one
    /// lock set would otherwise make two entries on the stack.
    // RTEMS-EXEC-MODEL-ALLOW(1): keys off lock_set_of, a sync registry read; the runtime only awaits the fixture's add_record; passes on the exec backend.
    #[tokio::test]
    async fn the_lockset_id_is_the_one_record_lock_maintains() {
        let db = db_with(&["BP:a", "BP:b", "BP:c", "BP:lone"]).await;
        set_flnk(&db, "BP:a", "BP:b");
        set_flnk(&db, "BP:b", "BP:c");

        let dblsr = |name: &str| db.lock_set_of(name).expect("a set after iocInit");
        let want = dblsr("BP:a").id;
        for seed in ["BP:a", "BP:b", "BP:c"] {
            assert_eq!(lockset_id(&db, seed), Some(want), "seed {seed}");
        }
        // The chain is one set: this is what makes the id shared, and it is
        // symmetric, so the tail names the same set as the head.
        let mut members = dblsr("BP:c").members;
        members.sort();
        assert_eq!(members, ["BP:a", "BP:b", "BP:c"]);

        // A record with no DB links is its own set, as in C.
        assert_ne!(lockset_id(&db, "BP:lone"), Some(want));
    }

    /// C merges lock sets only in `dbDbInitLink`/`dbDbAddLink`, so a link that
    /// resolved to another IOC merges nothing even when the name is local.
    // RTEMS-EXEC-MODEL-ALLOW(1): the same sync registry read across a ca:// link; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_ca_link_does_not_widen_the_lockset() {
        let db = db_with(&["BP:a", "BP:b"]).await;
        set_flnk(&db, "BP:a", "ca://BP:b");

        assert_ne!(
            lockset_id(&db, "BP:a"),
            lockset_id(&db, "BP:b"),
            "a ca:// forward link is a dbCa link, not a lock-set merge"
        );
    }

    /// A name the database does not have has no lock set, and neither does any
    /// record before `iocInit` — C reaches `dbLockGetLockId` through a null
    /// `precord->lset` there.
    // RTEMS-EXEC-MODEL-ALLOW(1): asserts an absent lock set before build_lock_sets; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_record_without_a_lockset_has_no_id() {
        let db = PvDatabase::new();
        db.add_record("BP:pre", Box::new(AiRecord::new(0.0)))
            .await
            .expect("add_record");
        assert_eq!(lockset_id(&db, "BP:pre"), None, "before build_lock_sets");
        assert_eq!(lockset_id(&db, "BP:absent"), None, "no such record");

        let table = BreakpointTable::new();
        assert_eq!(
            table.set(&db, "BP:pre", |_, _| unreachable!("no set, no thread")),
            Err(BkptError::NotFound("BP:pre".to_string()))
        );
    }

    /// `dbb`/`dbd`'s two refusals, C `:296-299` and `:421-424`.
    // RTEMS-EXEC-MODEL-ALLOW(1): dbb/dbd's two refusals are sync table calls; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_second_dbb_and_a_dbd_without_a_breakpoint_refuse() {
        let db = db_with(&["BP:a"]).await;
        let table = BreakpointTable::new();

        table.set(&db, "BP:a", |_, _| {}).expect("first dbb");
        assert_eq!(
            table.set(&db, "BP:a", |_, _| {}),
            Err(BkptError::AlreadySet)
        );

        table.clear(&db, "BP:a").expect("dbd");
        assert_eq!(table.clear(&db, "BP:a"), Err(BkptError::NotSet));
        // The lock set outlives its last breakpoint until its continuation
        // thread wakes and removes it — C frees the node in `dbBkptCont`
        // (`:617-619`), never in `dbd`, which only signals. This table was
        // built with a no-op spawner, so nothing wakes.
        assert_eq!(table.lock().sets.len(), 1);
        assert!(table.lock().sets[0].breakpoints.is_empty());
    }

    /// The other half of that: with a real continuation thread, `dbd` of the
    /// last breakpoint does retire the lock set, and the observer with it.
    // RTEMS-EXEC-MODEL-ALLOW(1): the continuation is a real std::thread, not a runtime worker; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_real_continuation_thread_retires_the_lockset_on_the_last_dbd() {
        let db = db_with(&["BP:a"]).await;
        let table = db.breakpoints_or_install();

        let joiner = {
            let owned = db.clone();
            let cell: Arc<Mutex<Option<std::thread::JoinHandle<String>>>> =
                Arc::new(Mutex::new(None));
            let out = cell.clone();
            table
                .set(&db, "BP:a", move |key, ex| {
                    *cell.lock().expect("cell") = Some(std::thread::spawn(move || {
                        continuation_loop(owned, key, ex)
                    }));
                })
                .expect("dbb");
            out.lock().expect("cell").take().expect("spawned")
        };

        table.clear(&db, "BP:a").expect("dbd");
        let closing = joiner.join().expect("join");
        assert!(
            closing.contains("End debug of lockset"),
            "C `:628` prints the closing line, got {closing:?}"
        );
        assert!(table.is_empty());
        assert!(
            db.breakpoints().is_none(),
            "the observer goes with the last lock set, so the hot path is a None load"
        );
    }

    /// C `:431-439`: a record carrying the BKPT bit whose lock set is not on
    /// the stack is a logic error — and C still clears the bit, so the record
    /// cannot be left marked by a table that has forgotten it.
    // RTEMS-EXEC-MODEL-ALLOW(1): clearing an orphaned BKPT bit is a sync table call; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_marked_record_whose_lockset_is_gone_is_cleared_and_reported() {
        let db = db_with(&["BP:a"]).await;
        set_record_bkpt(&db, "BP:a", |b| b | BKPT_ON_MASK);

        assert_eq!(
            BreakpointTable::new().clear(&db, "BP:a"),
            Err(BkptError::Logic),
            "the BKPT bit is set but this table never saw the record"
        );
        assert_eq!(
            record_bkpt(&db, "BP:a") & BKPT_ON_MASK,
            0,
            "the bit is cleared anyway, so the record is not left marked"
        );
    }

    /// A missing name is C's `S_db_notFound` on every command that takes one.
    // RTEMS-EXEC-MODEL-ALLOW(1): the five not-found refusals are sync table calls; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn every_named_command_refuses_a_missing_record() {
        let db = db_with(&["BP:a"]).await;
        let table = BreakpointTable::new();
        let missing = BkptError::NotFound("BP:nope".into());

        assert_eq!(table.set(&db, "BP:nope", |_, _| {}).unwrap_err(), missing);
        assert_eq!(table.clear(&db, "BP:nope").unwrap_err(), missing);
        assert_eq!(toggle_autoprint(&db, "BP:nope").unwrap_err(), missing);
        assert_eq!(table.cont(&db, Some("BP:nope")).unwrap_err(), missing);
    }

    /// Two breakpoints in one lock set share one stack node; two lock sets
    /// make two. This is what keying by `l_num` rather than by record buys.
    // RTEMS-EXEC-MODEL-ALLOW(1): grouping is decided under the stack mutex; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn breakpoints_group_by_lockset_not_by_record() {
        let db = db_with(&["BP:a", "BP:b", "BP:far"]).await;
        set_flnk(&db, "BP:a", "BP:b");
        let table = BreakpointTable::new();

        table.set(&db, "BP:a", |_, _| {}).expect("dbb a");
        table.set(&db, "BP:b", |_, _| {}).expect("dbb b");
        assert_eq!(table.lock().sets.len(), 1);
        assert_eq!(table.lock().sets[0].breakpoints, ["BP:a", "BP:b"]);

        table.set(&db, "BP:far", |_, _| {}).expect("dbb far");
        assert_eq!(table.lock().sets.len(), 2);
    }

    /// C `dbBkpt.c:684-687`: a record whose lock set holds no breakpoint runs
    /// record support and is not queued.
    // RTEMS-EXEC-MODEL-ALLOW(1): before_process is the synchronous hook itself; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_record_outside_every_breakpointed_lockset_just_runs() {
        let db = db_with(&["BP:a", "BP:far"]).await;
        let table = BreakpointTable::new();
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");

        assert_eq!(table.before_process(&db, "BP:far"), Before::Run);
        assert!(table.lock().sets[0].ep_queue.is_empty());
    }

    /// C `:713-751`: foreign processing queues the entry point and drops out
    /// of `dbProcess` without running record support. C `:754-758`: `pact` is
    /// tested AFTER queuing, so an async record's cycles still count — but it
    /// is not scheduled, because the continuation task must not re-enter one.
    // RTEMS-EXEC-MODEL-ALLOW(1): queueing a foreign entry point is sync; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_foreign_entry_is_queued_counted_and_skipped() {
        let db = db_with(&["BP:a"]).await;
        let table = BreakpointTable::new();
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");

        assert_eq!(table.before_process(&db, "BP:a"), Before::Skip);
        assert_eq!(table.before_process(&db, "BP:a"), Before::Skip);
        {
            let stack = table.lock();
            let ep = &stack.sets[0].ep_queue[0];
            assert_eq!(
                (ep.name.as_str(), ep.count, ep.scheduled),
                ("BP:a", 2, true)
            );
        }

        // Same record, now PACT: counted, not scheduled.
        db.get_record("BP:a").expect("record").read().enter_pact();
        {
            let mut stack = table.lock();
            stack.sets[0].ep_queue[0].scheduled = false;
        }
        assert_eq!(table.before_process(&db, "BP:a"), Before::Skip);
        let stack = table.lock();
        let ep = &stack.sets[0].ep_queue[0];
        assert_eq!(
            (ep.count, ep.scheduled),
            (3, false),
            "pact is checked after queuing, so the count moves and the schedule does not"
        );
    }

    /// C `:690-702`: a disabled record returns 0 so `dbProcess` still raises
    /// its disable alarm, and is not queued as an entry point.
    // RTEMS-EXEC-MODEL-ALLOW(1): the DISA/DISV branch is sync; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn a_disabled_record_runs_and_is_not_queued() {
        let db = db_with(&["BP:a"]).await;
        let table = BreakpointTable::new();
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");
        {
            let rec = db.get_record("BP:a").expect("record");
            let mut inst = rec.write();
            inst.common.disa = 1;
            inst.common.disv = 1;
        }

        assert_eq!(table.before_process(&db, "BP:a"), Before::Run);
        assert!(table.lock().sets[0].ep_queue.is_empty());
    }

    /// The boundary a naive park gets wrong, from the other side: the
    /// execution semaphore DOES bank, because C's `ex_sem` is an
    /// `epicsEventCreate(epicsEventEmpty)` and a scheduling signalled before
    /// the thread waits must not be lost. The park's opposite half — a resume
    /// with nobody suspended banking nothing — is
    /// `resume_thread_on_a_running_thread_banks_nothing` in `runtime::task`,
    /// where the park now lives.
    #[test]
    fn the_execution_semaphore_banks_a_signal() {
        let ex = Arc::new(BinarySemaphore::default());
        ex.signal();
        ex.signal();
        ex.wait(); // returns: the signal was banked
        assert!(
            !*ex.full.lock().expect("sem"),
            "epicsEventSignal is binary: two signals before one wait are one pass"
        );
    }

    /// The stop/resume round trip across two threads: the parked side must not
    /// come back before `dbc`, and must come back after it.
    // RTEMS-EXEC-MODEL-ALLOW(1): parks a real OS thread, the std-thread backend's own primitive; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_holds_until_dbc_releases_it() {
        let db = db_with(&["BP:a"]).await;
        let table = Arc::new(BreakpointTable::new());
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");

        // A registered thread, as every continuation thread is: `dbc` resumes
        // it by the `EPICS ID` it publishes, so a bare `std::thread` here
        // would be a park nothing could reach.
        let taskid = table.lock().sets[0]
            .cont
            .clone()
            .expect("dbb installs the taskid cell");
        let (tx, rx) = std::sync::mpsc::channel();
        let stopper = {
            let (table, db, tx) = (table.clone(), db.clone(), tx.clone());
            let set_id = lockset_id(&db, "BP:a").expect("lock set");
            crate::runtime::task::spawn_dedicated_thread(
                "bkptCont".to_string(),
                crate::runtime::task::ThreadPriority::ScanLow,
                crate::runtime::task::StackSizeClass::Small,
                move || {
                    claim_continuation(set_id, &taskid);
                    tx.send("entered").expect("send");
                    assert_eq!(table.before_process(&db, "BP:a"), Before::Run);
                    tx.send("resumed").expect("send");
                },
            )
            .expect("spawn")
        };

        assert_eq!(rx.recv().expect("entered"), "entered");
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_millis(250)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "the breakpoint holds the thread until dbc"
        );
        assert!(table.lock().sets[0].stopped_at.is_some());

        assert_eq!(
            table.cont(&db, None).expect("dbc"),
            Some("   BKPT> Continuing:  BP:a".to_string())
        );
        assert_eq!(rx.recv().expect("resumed"), "resumed");
        stopper.join().expect("join");
        assert!(
            !table.lock().sets[0].step,
            "dbc clears stepping; dbs would not"
        );
    }

    /// The shell's half of C's pair: a record genuinely stopped at a
    /// breakpoint puts its continuation thread in `SUSPEND`, and
    /// `epicsThreadResume` — the same `epicsThreadResume(pnode->taskid)`
    /// `dbc` calls (`dbBkpt.c:518`) — continues it.
    ///
    /// The stopped thread and the row `epicsThreadShowAll` prints are one
    /// cell, so this cannot pass while the listing still says `OK`.
    // RTEMS-EXEC-MODEL-ALLOW(1): parks a real OS thread, the std-thread backend's own primitive; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epics_thread_resume_continues_a_stopped_record() {
        let db = db_with(&["BP:a"]).await;
        let table = Arc::new(BreakpointTable::new());
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");
        let taskid = table.lock().sets[0]
            .cont
            .clone()
            .expect("dbb installs the taskid cell");

        let (tx, rx) = std::sync::mpsc::channel();
        let stopper = {
            let (table, db, tx) = (table.clone(), db.clone(), tx.clone());
            let set_id = lockset_id(&db, "BP:a").expect("lock set");
            let taskid = taskid.clone();
            crate::runtime::task::spawn_dedicated_thread(
                "bkptCont".to_string(),
                crate::runtime::task::ThreadPriority::ScanLow,
                crate::runtime::task::StackSizeClass::Small,
                move || {
                    claim_continuation(set_id, &taskid);
                    tx.send("entered").expect("send");
                    assert_eq!(table.before_process(&db, "BP:a"), Before::Run);
                    tx.send("resumed").expect("send");
                },
            )
            .expect("spawn")
        };
        assert_eq!(rx.recv().expect("entered"), "entered");

        let id = taskid.load(Ordering::Relaxed);
        assert_ne!(id, 0, "the thread publishes the handle dbstat prints");
        let row = (0..500)
            .find_map(|_| {
                let row = crate::runtime::task::thread_by_id(id).expect("row");
                row.is_suspended().then_some(row).or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    None
                })
            })
            .expect("a stopped record must show the thread suspended");
        assert!(
            row.show_line().ends_with(" SUSPEND"),
            "epicsThreadShowAll must read SUSPEND while stopped, got {:?}",
            row.show_line()
        );
        assert!(table.lock().sets[0].stopped_at.is_some());

        assert!(
            crate::runtime::task::resume_thread(id),
            "epicsThreadResume must find the thread suspended"
        );
        assert_eq!(rx.recv().expect("resumed"), "resumed");
        stopper.join().expect("join");
        assert!(
            !crate::runtime::task::thread_by_id(id).is_some_and(|t| t.is_suspended()),
            "the resumed thread is no longer suspended"
        );
    }

    /// C `:506-507` / `:543-544`: the announcement is for the no-argument form
    /// only, and only when the default lock set has moved.
    // RTEMS-EXEC-MODEL-ALLOW(1): the last_lset announcement is sync; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn resume_announces_only_a_change_of_default_lockset() {
        let db = db_with(&["BP:a"]).await;
        let table = BreakpointTable::new();
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");
        table.lock().sets[0].stopped_at = Some("BP:a".into());

        assert_eq!(
            table.step(&db, None).expect("dbs"),
            Some("   BKPT> Stepping:    BP:a".to_string())
        );
        table.lock().sets[0].stopped_at = Some("BP:a".into());
        assert_eq!(
            table.step(&db, None).expect("dbs"),
            None,
            "same lock set as last time: silent"
        );
        table.lock().sets[0].stopped_at = Some("BP:a".into());
        assert_eq!(
            table.cont(&db, Some("BP:a")).expect("dbc"),
            None,
            "the named form never announces"
        );
    }

    /// C `FIND_CONT_NODE` `:224` and `:242`, the two "not stopped" answers.
    // RTEMS-EXEC-MODEL-ALLOW(1): find_cont_node's refusal is sync; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn resume_refuses_when_nothing_is_stopped() {
        let db = db_with(&["BP:a", "BP:far"]).await;
        let table = BreakpointTable::new();
        assert_eq!(table.cont(&db, None), Err(BkptError::NoneStopped));

        table.set(&db, "BP:a", |_, _| {}).expect("dbb");
        assert_eq!(table.cont(&db, Some("BP:a")), Err(BkptError::NotStopped));
        assert_eq!(
            table.cont(&db, Some("BP:far")),
            Err(BkptError::NotStopped),
            "a record in no breakpointed lock set is 'not stopped in this lockset'"
        );
    }

    /// C `dbp` prints the NAMED record, and with no name the stopped one
    /// (`FIND_CONT_NODE` `:206-212` vs `:230`).
    // RTEMS-EXEC-MODEL-ALLOW(1): dbp renders from a sync record read; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn dbp_targets_the_named_record_and_otherwise_the_stopped_one() {
        let db = db_with(&["BP:a", "BP:b"]).await;
        set_flnk(&db, "BP:a", "BP:b");
        let table = BreakpointTable::new();
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");
        table.lock().sets[0].stopped_at = Some("BP:b".into());

        assert_eq!(table.print_target(&db, None).expect("dbp"), "BP:b");
        assert_eq!(table.print_target(&db, Some("BP:a")).expect("dbp"), "BP:a");
    }

    /// C `dbap` `:868-878`, both directions and both lines.
    // RTEMS-EXEC-MODEL-ALLOW(1): dbap toggles the BKPT field synchronously; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn dbap_toggles_and_reports_both_ways() {
        let db = db_with(&["BP:a"]).await;
        assert_eq!(
            toggle_autoprint(&db, "BP:a").expect("dbap"),
            "   BKPT> Auto print on for record BP:a"
        );
        assert_eq!(record_bkpt(&db, "BP:a") & BKPT_PRINT_MASK, BKPT_PRINT_MASK);
        assert_eq!(
            toggle_autoprint(&db, "BP:a").expect("dbap"),
            "   BKPT> Auto print off for record BP:a"
        );
        assert_eq!(record_bkpt(&db, "BP:a") & BKPT_PRINT_MASK, 0);
    }

    /// C `dbPrint` `:810-817`: the BKPT print bit alone is not enough — the
    /// lock set must currently hold breakpoints.
    // RTEMS-EXEC-MODEL-ALLOW(1): after_process is the synchronous hook itself; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn auto_print_is_silent_without_a_breakpoint_in_the_lockset() {
        let db = db_with(&["BP:a", "BP:far"]).await;
        let table = BreakpointTable::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            table.set_printer(Arc::new(move |n: &str| {
                seen.lock().expect("seen").push(n.to_string())
            }));
        }

        toggle_autoprint(&db, "BP:far").expect("dbap");
        table.after_process(&db, "BP:far");
        assert!(seen.lock().expect("seen").is_empty(), "no lock set at all");

        table.set(&db, "BP:a", |_, _| {}).expect("dbb");
        table.after_process(&db, "BP:far");
        assert!(
            seen.lock().expect("seen").is_empty(),
            "a different lock set holds the breakpoint"
        );

        toggle_autoprint(&db, "BP:a").expect("dbap");
        table.after_process(&db, "BP:a");
        assert_eq!(*seen.lock().expect("seen"), ["BP:a"]);

        // And without the print bit, nothing, breakpoint or not.
        toggle_autoprint(&db, "BP:a").expect("dbap");
        table.after_process(&db, "BP:a");
        assert_eq!(seen.lock().expect("seen").len(), 1);
    }

    /// `dbstat`'s three line shapes, against C's format strings at `:906`,
    /// `:918` and `:927-933`.
    ///
    /// `T:` is `(nil)` here because the spawn callback is a no-op, so no
    /// continuation thread ever publishes an id — the same column C prints
    /// from a `pnode->taskid` it has not stored yet. The live value is the
    /// thread's `EPICS ID`; `dbb_puts_its_continuation_thread_on_the_thread_list`
    /// covers that end.
    // RTEMS-EXEC-MODEL-ALLOW(1): dbstat renders from a sync snapshot; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn dbstat_renders_c_line_shapes() {
        let db = db_with(&["BP:a"]).await;
        let table = BreakpointTable::new();
        table.set(&db, "BP:a", |_, _| {}).expect("dbb");
        toggle_autoprint(&db, "BP:a").expect("dbap");

        let lines = table.status(&db);
        let id = db.lock_set_of("BP:a").expect("a set after iocInit").id;
        assert_eq!(
            lines[0].render(),
            format!("LSet: {id}                                            #B: 00001  T: (nil)")
        );
        assert_eq!(
            lines[1].render(),
            "             Breakpoint: BP:a                         (ap)"
        );
        assert_eq!(
            lines.len(),
            2,
            "entry points print only for a stopped lock set"
        );

        table.lock().sets[0].stopped_at = Some("BP:a".into());
        let stopped = table.status(&db);
        assert_eq!(
            stopped[0].render(),
            format!("LSet: {id}  Stopped at: BP:a                          #B: 00001  T: (nil)")
        );

        // Both LSet forms put `#B:` in the same column, as C's two format
        // strings do — the blank-stopped form pads by hand.
        let (a, b) = (lines[0].render(), stopped[0].render());
        assert_eq!(
            a.find("#B:").expect("a"),
            b.find("#B:").expect("b"),
            "the two LSet lines must agree on the #B column"
        );

        assert_eq!(
            StatLine::Entrypoint {
                name: "BP:a".into(),
                count: 7,
                elapsed_secs: 12.25,
            }
            .render(),
            "             Entrypoint: BP:a                          #C: 00007  C/S:    12.2"
        );
    }

    /// Wait for `pred` to hold of the stopped record, or give up. Polling is
    /// how a shell user waits too: the stop happens on the continuation
    /// thread, which the shell has no handle on.
    fn await_stop(table: &BreakpointTable, pred: impl Fn(Option<&str>) -> bool) -> Option<String> {
        for _ in 0..400 {
            {
                let stack = table.lock();
                let at = stack.sets.front().and_then(|s| s.stopped_at.clone());
                if pred(at.as_deref()) {
                    return at;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }

    /// The whole debugger, end to end: set a breakpoint, process the record
    /// from outside, watch it stop, step into the FLNK target, read `dbstat`
    /// at both stops, then continue and delete.
    ///
    /// This is the shape the brief calls functional equivalence — a stop that
    /// really holds a chain mid-flight, not a name that registers and returns.
    // RTEMS-EXEC-MODEL-ALLOW(1): steps a chain on a parked OS thread, the std-thread backend's own primitive; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_breakpoint_stops_a_chain_and_dbs_steps_into_the_flnk_target() {
        let db = db_with(&["BPX:a", "BPX:b"]).await;
        set_flnk(&db, "BPX:a", "BPX:b");
        let table = db.breakpoints_or_install();

        let joiner = {
            let owned = db.clone();
            let cell: Arc<Mutex<Option<std::thread::JoinHandle<String>>>> =
                Arc::new(Mutex::new(None));
            let out = cell.clone();
            table
                .set(&db, "BPX:a", move |key, ex| {
                    *cell.lock().expect("cell") = Some(std::thread::spawn(move || {
                        continuation_loop(owned, key, ex)
                    }));
                })
                .expect("dbb");
            out.lock().expect("cell").take().expect("spawned")
        };

        // A foreign entry: C queues it and drops out of dbProcess without
        // running record support, handing the cycle to the continuation
        // thread. The `Ok` here is that fall-out, not a processed record.
        db.process_record_with_links("BPX:a", &mut HashSet::new(), 0)
            .await
            .expect("foreign entry falls out of dbProcess");

        assert_eq!(
            await_stop(&table, |at| at == Some("BPX:a")).as_deref(),
            Some("BPX:a"),
            "the breakpoint stops the chain at its own record"
        );
        let stat = table.status(&db);
        assert!(
            stat[0].render().contains("Stopped at: BPX:a"),
            "dbstat names the stopped record, got {:?}",
            stat[0].render()
        );

        // `dbs` — one record further along the chain, which is the FLNK
        // target. C stops there because stepping stays on.
        table.step(&db, None).expect("dbs");
        assert_eq!(
            await_stop(&table, |at| at == Some("BPX:b")).as_deref(),
            Some("BPX:b"),
            "dbs steps INTO the FLNK target, mid-chain"
        );
        assert!(
            table.status(&db)[0].render().contains("Stopped at: BPX:b"),
            "dbstat follows the step"
        );

        // `dbc` — stepping off, so the chain runs out and the lock set
        // reports itself unstopped again.
        table.cont(&db, None).expect("dbc");
        assert_eq!(
            await_stop(&table, |at| at.is_none()),
            None,
            "the chain completes and nothing is stopped"
        );

        table.clear(&db, "BPX:a").expect("dbd");
        joiner.join().expect("join");
        assert!(db.breakpoints().is_none());
    }

    /// `dbstat`'s `LSet:` column and `dblsr`'s set id are one number, because
    /// they are one field. C reads both off `lockSet::id`
    /// (`dbLock.c:175-182` and `:886-887`); a `dbb` on either member of a set
    /// must therefore report what `dblsr` reports for that set, whichever
    /// member is named.
    // RTEMS-EXEC-MODEL-ALLOW(1): compares dbstat's column against lock_set_of, both sync; the runtime only awaits add_record; passes on the exec backend.
    #[tokio::test]
    async fn dbstat_and_dblsr_agree_on_the_set_id() {
        let db = db_with(&["BP:a", "BP:b"]).await;
        set_flnk(&db, "BP:b", "BP:a");
        let dblsr = db.lock_set_of("BP:a").expect("a set after iocInit").id;
        assert_eq!(db.lock_set_of("BP:b").expect("same set").id, dblsr);

        let first = BreakpointTable::new();
        first.set(&db, "BP:b", |_, _| {}).expect("dbb");
        let from_b = first.lock().sets[0].id;
        assert!(
            first.status(&db)[0]
                .render()
                .starts_with(&format!("LSet: {dblsr}"))
        );
        first.clear(&db, "BP:b").expect("dbd");

        let second = BreakpointTable::new();
        second.set(&db, "BP:a", |_, _| {}).expect("dbb");
        assert_eq!(second.lock().sets[0].id, from_b);
        assert_eq!(from_b, dblsr, "dbstat prints dblsr's id");
    }
}
