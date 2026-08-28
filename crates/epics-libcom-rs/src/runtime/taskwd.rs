//! `taskwd` — the IOC's task watchdog, C `libCom/src/taskwd/taskwd.c`
//! (R7.0.10).
//!
//! One low-priority thread wakes every [`TASKWD_DELAY`] and asks each
//! registered task whether it is still running. When one stops running the
//! watchdog says so on the console, calls that task's own callback, and tells
//! every registered monitor — which is how a C IOC turns a wedged scan thread
//! into an operator-visible event instead of records that quietly stop
//! updating.
//!
//! This is **not** a thread registry. Nothing here enumerates threads, names
//! them or reports their stacks; a task is here because it asked to be
//! watched, and the list is the watchdog's, not the process's.
//!
//! # What "suspended" means here
//!
//! C's watchdog polls `epicsThreadIsSuspended(tid)` (`taskwd.c:99`). On
//! vxWorks that is a real OS state; on POSIX it is a flag `epicsThreadSuspendSelf`
//! sets, so a C IOC on Linux only ever reports a thread that suspended
//! *itself* — out of memory, `cantProceed`. Rust has neither: a thread cannot
//! be suspended by another and never suspends itself.
//!
//! So the port asks the question the other way round, which is the only way it
//! can be asked here: a task checks in ([`TaskwdEntry::check_in`]) as it goes
//! round its loop, and a task that stops checking in inside the interval it
//! declared is this port's *suspended*. That covers strictly more than C's
//! POSIX build does — a thread wedged on a lock or an unbounded read is
//! invisible to `epicsThreadIsSuspended` and visible here — and the operator's
//! side of it is unchanged: the same console line, the same callback, the same
//! `taskwdShow` state column.
//!
//! A task that cannot promise to come back — one parked in `accept()`, or in a
//! blocking read with no deadline — registers [`CheckIn::Unbounded`] and is
//! listed but never reported, which is exactly what C's POSIX build does with
//! every one of its tasks.
//!
//! # Identity is the registration, not a thread id
//!
//! C keys everything on `epicsThreadId`, because the state it polls belongs to
//! a thread. Half the port's equivalents of C's call sites are futures, which
//! have no thread of their own and may run on a different one after every
//! await, so a thread id would name the wrong thing for them and the right
//! thing for the others — one field, two meanings. Registration returns a
//! [`TaskwdEntry`] instead, and [`TaskwdId`] is what the monitor API carries.
//!
//! The handle is also what removes the task: C pairs every `taskwdInsert` with
//! a `taskwdRemove` on each exit path and errlogs when it is passed a thread
//! that was never inserted (`taskwd.c:241-243`). Dropping the handle removes
//! it, on every path including a panic, and there is no way to ask for the
//! removal of something that was never registered.
//!
//! # Not ported
//!
//! * `taskwdAnyInsert` / `taskwdAnyRemove` (`taskwd.c:306-354`) — the
//!   deprecated pre-3.15 monitor API, which C implements as a monitor whose
//!   `notify` fires only on suspension. Nothing in base or in the modules this
//!   workspace ports calls it; the [`TaskwdMonitor`] trait is what it wraps.
//! * The free-node pool (`taskwd.c:395-430`) and the `%d free nodes` it puts in
//!   the report. It is an allocator for three C structs that share a union;
//!   Rust drops the entry instead, so there is no pool to count.
//! * `twdctlDisable` (`taskwd.c:74`) — the enum has the state, nothing in
//!   R7.0.10 ever assigns it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Once};
use std::time::{Duration, Instant};

use crate::runtime::sync::PriorityInheritanceMutex;
use crate::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};

/// How often the watchdog looks at its list — C `TASKWD_DELAY` (`taskwd.c:80`).
pub const TASKWD_DELAY: Duration = Duration::from_secs(6);

/// What a task promises about coming back to check in.
///
/// The promise is the task's, not the watchdog's: it is the interval after
/// which *the task itself* considers a missed check-in a fault, so a scan
/// thread on a 10 Hz rate promises seconds, not milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckIn {
    /// Report the task once this long has passed with no check-in.
    ///
    /// Detection is not instant: the watchdog only looks every
    /// [`TASKWD_DELAY`], so a task is reported between `d` and `d +
    /// TASKWD_DELAY` after its last check-in. C has the same granularity for
    /// the same reason.
    Every(Duration),
    /// The task makes no promise — it is parked in something with no deadline
    /// of its own. Listed by [`taskwd_show`], never reported.
    Unbounded,
}

/// The watchdog's name for one registration — C's `epicsThreadId` in the
/// monitor API, without the claim that a task is a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskwdId(u64);

/// What a monitor is told about, and what [`taskwd_show`] lists.
#[derive(Clone, Debug)]
pub struct TaskInfo {
    /// Identity of this registration.
    pub id: TaskwdId,
    /// The name the task registered under. C reads the thread's name out of
    /// the OS at report time (`epicsThreadGetName`, `taskwd.c:113`); a task
    /// here names itself, because a future has no thread name to read.
    pub name: String,
}

/// A task's own answer to being found stuck — C's `TASKWDFUNC` and its `usr`
/// pointer collapsed into one closure (`taskwd.h:33`).
///
/// Called from the watchdog thread, with no watchdog lock held.
pub type TaskwdCallback = Arc<dyn Fn() + Send + Sync>;

/// Something watching every task, rather than one — C's `taskwdMonitor`
/// (`taskwd.h:41-45`). Every method is optional there (a `NULL` slot is
/// skipped), so every method here has a do-nothing default.
///
/// Called from the registering task's thread (`insert`, `remove`) or the
/// watchdog thread (`notify`), always with no watchdog lock held, so a monitor
/// may call back into this module.
pub trait TaskwdMonitor: Send + Sync {
    /// A task joined the watchdog's list.
    fn insert(&self, task: &TaskInfo) {
        let _ = task;
    }
    /// A task changed state. `suspended` is the new state, so a monitor sees
    /// both the fault and the recovery — C notifies on the transition either
    /// way (`taskwd.c:100-121`).
    fn notify(&self, task: &TaskInfo, suspended: bool) {
        let _ = (task, suspended);
    }
    /// A task left the list.
    fn remove(&self, task: &TaskInfo) {
        let _ = task;
    }
}

/// One watched task — C's `struct tNode` (`taskwd.c:34-40`).
struct TaskEntry {
    info: TaskInfo,
    check_in: CheckIn,
    /// Bumped by the task, sampled by the watchdog. A counter and not a
    /// timestamp so that checking in costs one relaxed increment in a hot
    /// loop, and so that every reading of the clock belongs to the watchdog.
    beat: Arc<AtomicU64>,
    /// The watchdog's copy of `beat` from its previous look.
    seen_beat: u64,
    /// When the watchdog last saw `beat` move. Seeded at insert so a task that
    /// never checks in at all is reported after its own interval.
    last_move: Instant,
    /// C's `pt->suspended` (`taskwd.c:39`): the remembered state, so what is
    /// reported is the transition and not the level.
    suspended: bool,
    callback: Option<TaskwdCallback>,
}

/// One registered monitor — C's `struct mNode` (`taskwd.c:42-46`). C keys
/// removal on the `(funcs, usr)` pair; a handle is this port's key.
struct MonitorEntry {
    id: u64,
    monitor: Arc<dyn TaskwdMonitor>,
}

/// C's `twdCtl` (`taskwd.c:73-75`), less the state nothing assigns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctl {
    Run,
    Exit,
}

struct Taskwd {
    /// C's `tList` under `tLock` (`taskwd.c:61-62`).
    tasks: PriorityInheritanceMutex<Vec<TaskEntry>>,
    /// C's `mList` under `mLock` (`taskwd.c:65-66`).
    monitors: PriorityInheritanceMutex<Vec<MonitorEntry>>,
    /// C's `loopEvent` (`taskwd.c:76`) — a condvar because the only waiter is
    /// the watchdog itself.
    ctl: Mutex<Ctl>,
    wake: Condvar,
    /// C waits on `exitEvent` for the loop to finish (`taskwd.c:137`); joining
    /// the thread is the same wait with the handle doing the bookkeeping.
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    started: Once,
    next_id: AtomicU64,
    /// C's `TASKWD_DELAY` is a compile-time constant (`taskwd.c:80`). Here the
    /// watchdog reads it every pass, so a test can watch a task fault without
    /// waiting six seconds for the pass that notices.
    period_ms: AtomicU64,
}

/// The process's watchdog — C's file-scope `tList` / `mList` / `twdCtl`
/// (`taskwd.c:60-77`), which is the only instance a C IOC can have.
///
/// The state machine above is not written against it: everything a watchdog
/// does is a method on an instance, so a test can hold its own, drive [`scan`]
/// from a clock it controls, and never race the thread this one runs.
///
/// [`scan`]: Taskwd::scan
static TASKWD: LazyLock<Arc<Taskwd>> = LazyLock::new(Taskwd::new);

impl Taskwd {
    fn new() -> Arc<Self> {
        Arc::new(Taskwd {
            tasks: PriorityInheritanceMutex::new(Vec::new()),
            monitors: PriorityInheritanceMutex::new(Vec::new()),
            ctl: Mutex::new(Ctl::Run),
            wake: Condvar::new(),
            thread: Mutex::new(None),
            started: Once::new(),
            next_id: AtomicU64::new(1),
            period_ms: AtomicU64::new(TASKWD_DELAY.as_millis() as u64),
        })
    }

    /// C `twdInitOnce` (`taskwd.c:145-166`): start the one watchdog thread, or
    /// take the process down trying — `MandatoryThread` is `cantProceed`
    /// (`taskwd.c:162-163`) with the console text in one place.
    fn start(self: &Arc<Self>) {
        let owner = self.clone();
        self.started.call_once(|| {
            *self.ctl.lock().unwrap_or_else(|e| e.into_inner()) = Ctl::Run;
            let runner = owner.clone();
            let handle = MandatoryThread::new("taskwd", ThreadPriority::Low, StackSizeClass::Small)
                .spawn(move || runner.run());
            *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            // C `epicsAtExit(twdShutdown, NULL)` (`taskwd.c:165`).
            crate::runtime::exit::at_exit("taskwd", move || owner.shutdown());
        });
    }

    /// C `twdTask` (`taskwd.c:89-129`).
    fn run(&self) {
        loop {
            let ctl = *self.ctl.lock().unwrap_or_else(|e| e.into_inner());
            if ctl == Ctl::Exit {
                return;
            }
            self.scan(Instant::now());
            let period = Duration::from_millis(self.period_ms.load(Ordering::Relaxed));
            let guard = self.ctl.lock().unwrap_or_else(|e| e.into_inner());
            let (guard, _) = self
                .wake
                .wait_timeout_while(guard, period, |c| *c != Ctl::Exit)
                .unwrap_or_else(|e| e.into_inner());
            if *guard == Ctl::Exit {
                return;
            }
        }
    }

    /// C `twdShutdown` (`taskwd.c:132-143`).
    fn shutdown(&self) {
        *self.ctl.lock().unwrap_or_else(|e| e.into_inner()) = Ctl::Exit;
        self.wake.notify_all();
        let handle = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// One pass over the list — C `twdTask`'s body (`taskwd.c:96-124`).
    ///
    /// The callbacks and monitors C runs while holding `tLock` are run here
    /// after it is released. C can hold it because `epicsMutex` is recursive,
    /// so a callback that calls `taskwdShow` merely re-enters; ours is not, and
    /// that call would deadlock. The observable difference is confined to a
    /// task removed in the window between the two, whose callback can still
    /// fire — and that task is by definition the one that stopped running.
    fn scan(&self, now: Instant) {
        let mut transitions: Vec<(TaskInfo, bool, Option<TaskwdCallback>)> = Vec::new();
        {
            let mut tasks = self.tasks.lock();
            for task in tasks.iter_mut() {
                let beat = task.beat.load(Ordering::Relaxed);
                if beat != task.seen_beat {
                    task.seen_beat = beat;
                    task.last_move = now;
                }
                let suspended = match task.check_in {
                    CheckIn::Unbounded => false,
                    CheckIn::Every(deadline) => {
                        now.saturating_duration_since(task.last_move) >= deadline
                    }
                };
                if suspended != task.suspended {
                    task.suspended = suspended;
                    transitions.push((task.info.clone(), suspended, task.callback.clone()));
                }
            }
        }

        for (info, suspended, callback) in transitions {
            for monitor in self.monitor_snapshot() {
                monitor.notify(&info, suspended);
            }
            if suspended {
                // C's wording, because it is what an operator greps for
                // (`taskwd.c:114-115`).
                crate::runtime::log::errlog_printf(&format!(
                    "Thread {} ({}) suspended\n",
                    info.name, info.id.0
                ));
                if let Some(callback) = callback {
                    callback();
                }
            }
        }
    }

    fn monitor_snapshot(&self) -> Vec<Arc<dyn TaskwdMonitor>> {
        self.monitors
            .lock()
            .iter()
            .map(|m| m.monitor.clone())
            .collect()
    }
}

/// Start the watchdog thread if it is not running — C `taskwdInit`
/// (`taskwd.c:168-172`), which `iocInit` calls early (`iocInit.c:151`) and
/// every registration calls for itself.
pub fn taskwd_init() {
    TASKWD.start();
}

/// Watch this task — C `taskwdInsert` (`taskwd.c:177-205`).
///
/// `name` is what the console line and [`taskwd_show`] call it; give it the
/// name the thread or task already has, so an operator can match the two.
/// `callback` is what the task wants done when it is found stuck (C's
/// `TASKWDFUNC`); most call sites have none.
///
/// The task is watched until the returned handle is dropped.
pub fn taskwd_insert(
    name: impl Into<String>,
    check_in: CheckIn,
    callback: Option<TaskwdCallback>,
) -> TaskwdEntry {
    taskwd_init();
    TASKWD.insert(name, check_in, callback)
}

/// A watched task's registration. Dropping it stops the watch — C
/// `taskwdRemove` (`taskwd.c:207-244`) on every exit path, including the ones
/// C's callers have to remember.
pub struct TaskwdEntry {
    owner: Arc<Taskwd>,
    info: TaskInfo,
    beat: Arc<AtomicU64>,
}

impl TaskwdEntry {
    /// "Still here" — call it once per pass round the task's loop.
    ///
    /// One relaxed increment: cheap enough for a loop that runs at scan rate,
    /// and it reads no clock, so the watchdog stays the only owner of the
    /// timing question.
    pub fn check_in(&self) {
        self.beat.fetch_add(1, Ordering::Relaxed);
    }

    /// What the monitors and [`taskwd_show`] know this task as.
    pub fn info(&self) -> &TaskInfo {
        &self.info
    }
}

impl Drop for TaskwdEntry {
    fn drop(&mut self) {
        let removed = {
            let mut tasks = self.owner.tasks.lock();
            match tasks.iter().position(|t| t.info.id == self.info.id) {
                Some(idx) => {
                    tasks.remove(idx);
                    true
                }
                None => false,
            }
        };
        // C errlogs when the tid it was handed is not in the list
        // (`taskwd.c:241-243`); with the handle owning the registration the
        // only way here is a second drop, which Rust does not do.
        if removed {
            for monitor in self.owner.monitor_snapshot() {
                monitor.remove(&self.info);
            }
        }
    }
}

/// Watch every task — C `taskwdMonitorAdd` (`taskwd.c:249-264`).
///
/// The monitor is registered until the returned handle is dropped, which is C's
/// `taskwdMonitorDel` (`taskwd.c:266-288`).
pub fn taskwd_monitor_add(monitor: Arc<dyn TaskwdMonitor>) -> TaskwdMonitorEntry {
    taskwd_init();
    TASKWD.monitor_add(monitor)
}

/// A registered monitor's handle. Dropping it unregisters the monitor.
pub struct TaskwdMonitorEntry {
    owner: Arc<Taskwd>,
    id: u64,
}

impl Drop for TaskwdMonitorEntry {
    fn drop(&mut self) {
        self.owner.monitors.lock().retain(|m| m.id != self.id);
    }
}

/// Report the watchdog's list — C `taskwdShow` (`taskwd.c:359-390`).
///
/// `out` takes one line at a time, the shape the port's other report functions
/// use (`epics_base_rs::server::db_server::dbsr`). Registering it as the iocsh
/// `taskwdShow` command is `libComRegister.c`'s job, not this module's.
///
/// Unlike C's, this answers before anything has been registered: C locks
/// mutexes that `taskwdInit` creates, so `taskwdShow` on a process that never
/// initialised the watchdog dereferences a null mutex.
pub fn taskwd_show(level: u32, out: &dyn Fn(&str)) {
    TASKWD.show(level, out)
}

impl Taskwd {
    /// C `taskwdInsert` (`taskwd.c:177-205`).
    fn insert(
        self: &Arc<Self>,
        name: impl Into<String>,
        check_in: CheckIn,
        callback: Option<TaskwdCallback>,
    ) -> TaskwdEntry {
        let info = TaskInfo {
            id: TaskwdId(self.next_id.fetch_add(1, Ordering::Relaxed)),
            name: name.into(),
        };
        let beat = Arc::new(AtomicU64::new(0));

        // C tells the monitors before the task joins the list
        // (`taskwd.c:192-204`), so a monitor that reports the list from its own
        // `insert` does not see the task it is being told about.
        for monitor in self.monitor_snapshot() {
            monitor.insert(&info);
        }

        self.tasks.lock().push(TaskEntry {
            info: info.clone(),
            check_in,
            beat: beat.clone(),
            seen_beat: 0,
            last_move: Instant::now(),
            suspended: false,
            callback,
        });

        TaskwdEntry {
            owner: self.clone(),
            info,
            beat,
        }
    }

    /// C `taskwdMonitorAdd` (`taskwd.c:249-264`).
    fn monitor_add(self: &Arc<Self>, monitor: Arc<dyn TaskwdMonitor>) -> TaskwdMonitorEntry {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.monitors.lock().push(MonitorEntry { id, monitor });
        TaskwdMonitorEntry {
            owner: self.clone(),
            id,
        }
    }

    /// C `taskwdShow` (`taskwd.c:359-390`).
    ///
    /// Two fields of C's summary line have no counterpart here and are left
    /// off rather than faked. C's `%d free nodes` counts `fList`, the pool of
    /// recycled `union twdNode`s that `freeNode` pushes instead of calling
    /// `free` (`taskwd.c:393-420`); an entry here is removed by
    /// [`TaskwdEntry`]'s `Drop` and its memory returned to the allocator, so
    /// there is no pool to count and the number would be a constant zero
    /// dressed as a measurement. C's noun is "threads" because its `tList`
    /// holds `epicsThreadId`s and it reads each name back with
    /// `epicsThreadGetName` at print time; several rows here are futures on a
    /// banded executor rather than threads of their own, which is why the
    /// name is carried in the entry and the noun is "tasks".
    fn show(&self, level: u32, out: &dyn Fn(&str)) {
        let monitors = self.monitors.lock().len();
        let tasks = self.tasks.lock();
        out(&format!(
            "{} monitors, {} tasks registered",
            monitors,
            tasks.len()
        ));
        if level == 0 {
            return;
        }
        // C can fix its name column at `%16.16s` because every name it prints
        // is an `epicsThreadGetName` of a short literal, and identity lives in
        // the `EPICS TID` column beside it. This table has no such column, so
        // a per-connection row carries its peer in the name — which a fixed 16
        // truncated at exactly the character that made it distinct
        // (`CAS-TCP 0.0.0.0:5064` printed as `CAS-TCP 0.0.0.0:`). Width is the
        // widest name present, floored at C's 16 so a table of C-shaped names
        // still lays out as C's does.
        let width = tasks
            .iter()
            .map(|t| t.info.name.chars().count())
            .max()
            .unwrap_or(0)
            .max(16);
        out(&format!(
            "{:width$} {:>9} {:>12} {:>12} {:>8}",
            "TASK NAME", "STATE", "CHECK-IN", "LAST BEAT", "CALLBACK"
        ));
        let now = Instant::now();
        for task in tasks.iter() {
            let check_in = match task.check_in {
                CheckIn::Unbounded => "unbounded".to_string(),
                CheckIn::Every(d) => format!("{:.1}s", d.as_secs_f64()),
            };
            out(&format!(
                "{:width$} {:>9} {:>12} {:>11.1}s {:>8}",
                task.info.name,
                if task.suspended { "Suspended" } else { "Ok" },
                check_in,
                now.saturating_duration_since(task.last_move).as_secs_f64(),
                if task.callback.is_some() { "yes" } else { "-" }
            ));
        }
    }
}

#[cfg(test)]
impl Taskwd {
    /// C's scan interval is a constant; the one test that waits for the real
    /// watchdog thread to notice something cannot wait six seconds for it.
    fn set_period_for_test(&self, period: Duration) {
        self.period_ms
            .store(period.as_millis() as u64, Ordering::Relaxed);
        self.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Records what a monitor was told, and in what order.
    #[derive(Default)]
    struct Recorder {
        log: StdMutex<Vec<String>>,
        /// The instance to report from inside `insert`, so the test can see
        /// what the list looked like at that moment.
        watched: StdMutex<Option<Arc<Taskwd>>>,
    }

    impl Recorder {
        fn taken(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap_or_else(|e| e.into_inner()))
        }
        fn push(&self, line: String) {
            self.log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(line);
        }
    }

    impl TaskwdMonitor for Recorder {
        fn insert(&self, task: &TaskInfo) {
            let watched = self
                .watched
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let counts = std::cell::RefCell::new(Vec::new());
            if let Some(watched) = watched {
                watched.show(0, &|line: &str| counts.borrow_mut().push(line.to_string()));
            }
            self.push(format!(
                "insert {} [{}]",
                task.name,
                counts.into_inner().join("")
            ));
        }
        fn notify(&self, task: &TaskInfo, suspended: bool) {
            self.push(format!("notify {} suspended={suspended}", task.name));
        }
        fn remove(&self, task: &TaskInfo) {
            self.push(format!("remove {}", task.name));
        }
    }

    /// A watchdog with no thread: every test below drives [`Taskwd::scan`] from
    /// a clock it owns, so nothing races it and nothing sleeps.
    fn watchdog() -> Arc<Taskwd> {
        Taskwd::new()
    }

    fn rows(twd: &Taskwd, level: u32) -> Vec<String> {
        let out = std::cell::RefCell::new(Vec::new());
        twd.show(level, &|line: &str| out.borrow_mut().push(line.to_string()));
        out.into_inner()
    }

    fn state_of(twd: &Taskwd, name: &str) -> String {
        rows(twd, 1)
            .into_iter()
            .find(|r| r.starts_with(name))
            .unwrap_or_else(|| panic!("{name} is not in the report"))
    }

    #[test]
    fn a_task_that_keeps_checking_in_is_not_reported() {
        let twd = watchdog();
        let task = twd.insert("keeps-up", CheckIn::Every(Duration::from_secs(1)), None);
        let start = Instant::now();

        task.check_in();
        twd.scan(start + Duration::from_secs(2));

        assert!(
            state_of(&twd, "keeps-up").contains("Ok"),
            "{}",
            state_of(&twd, "keeps-up")
        );
    }

    /// The boundary the whole module turns on: below the deadline is silence,
    /// at it comes the report, the callback and the notify.
    #[test]
    fn a_task_that_stops_checking_in_is_reported_at_its_own_deadline() {
        let twd = watchdog();
        let recorder = Arc::new(Recorder::default());
        let _monitor = twd.monitor_add(recorder.clone());

        let fired = Arc::new(AtomicU64::new(0));
        let counter = fired.clone();
        let _task = twd.insert(
            "wedged",
            CheckIn::Every(Duration::from_secs(1)),
            Some(Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            })),
        );
        // After the insert, so the deadline is measured from a moment at or
        // after the task's own seed rather than before it.
        let start = Instant::now();
        let _ = recorder.taken();

        twd.scan(start + Duration::from_millis(999));
        assert_eq!(fired.load(Ordering::Relaxed), 0, "below its deadline");
        assert!(recorder.taken().is_empty(), "nothing to notify below it");

        twd.scan(start + Duration::from_secs(1));
        assert_eq!(fired.load(Ordering::Relaxed), 1, "at its deadline");
        assert_eq!(recorder.taken(), vec!["notify wedged suspended=true"]);
        assert!(state_of(&twd, "wedged").contains("Suspended"));

        // C reports the transition, not the level: a second pass with the task
        // still stuck says nothing more (`taskwd.c:100`).
        twd.scan(start + Duration::from_secs(30));
        assert_eq!(fired.load(Ordering::Relaxed), 1, "the transition, once");
        assert!(recorder.taken().is_empty());
    }

    #[test]
    fn a_task_that_checks_in_again_is_reported_recovered() {
        let twd = watchdog();
        let recorder = Arc::new(Recorder::default());
        let _monitor = twd.monitor_add(recorder.clone());

        let task = twd.insert("recovers", CheckIn::Every(Duration::from_secs(1)), None);
        let start = Instant::now();
        twd.scan(start + Duration::from_secs(1));
        let _ = recorder.taken();

        task.check_in();
        twd.scan(start + Duration::from_secs(2));

        assert_eq!(recorder.taken(), vec!["notify recovers suspended=false"]);
        assert!(state_of(&twd, "recovers").contains("Ok"));
    }

    /// C's POSIX build never reports any task, because nothing suspends a
    /// thread. That is what a task with no deadline of its own asks for.
    #[test]
    fn an_unbounded_task_is_listed_and_never_reported() {
        let twd = watchdog();
        let recorder = Arc::new(Recorder::default());
        let _monitor = twd.monitor_add(recorder.clone());

        let _task = twd.insert("parked-in-accept", CheckIn::Unbounded, None);
        let start = Instant::now();
        let _ = recorder.taken();

        twd.scan(start + Duration::from_secs(86_400));

        assert!(
            recorder.taken().is_empty(),
            "an unbounded task is never late"
        );
        assert!(state_of(&twd, "parked-in-accept").contains("Ok"));
    }

    #[test]
    fn dropping_the_handle_removes_the_task_and_tells_the_monitors() {
        let twd = watchdog();
        let recorder = Arc::new(Recorder::default());
        let _monitor = twd.monitor_add(recorder.clone());

        let before = rows(&twd, 0);
        let task = twd.insert("short-lived", CheckIn::Unbounded, None);
        assert_ne!(rows(&twd, 0), before, "in the count while it lives");
        let _ = recorder.taken();

        drop(task);

        assert_eq!(rows(&twd, 0), before, "and out of it once dropped");
        assert_eq!(recorder.taken(), vec!["remove short-lived"]);
    }

    /// C calls the monitors' `insert` before the task joins the list
    /// (`taskwd.c:192-204`), so a monitor reporting from inside it sees the
    /// count without the new task.
    #[test]
    fn a_monitor_is_told_before_the_task_joins_the_list() {
        let twd = watchdog();
        let recorder = Arc::new(Recorder::default());
        *recorder.watched.lock().unwrap() = Some(twd.clone());
        let _monitor = twd.monitor_add(recorder.clone());

        let _task = twd.insert("newcomer", CheckIn::Unbounded, None);

        assert_eq!(
            recorder.taken(),
            vec!["insert newcomer [1 monitors, 0 tasks registered]"]
        );
    }

    /// C's `%16.16s` fits because every C name is a short literal read back
    /// with `epicsThreadGetName`, and identity is in the `EPICS TID` column.
    /// Rows here carry their peer or bind address in the name instead, and a
    /// fixed 16 cut `CAS-TCP 0.0.0.0:5064` down to `CAS-TCP 0.0.0.0:` — the
    /// truncation landed on exactly the part that made the row distinct.
    #[test]
    fn a_name_longer_than_cs_column_is_not_truncated() {
        let twd = watchdog();
        let long = "CAS-client 192.168.0.44:34122";
        let _short = twd.insert("cbLow", CheckIn::Unbounded, None);
        let _task = twd.insert(long, CheckIn::Unbounded, None);

        let detailed = rows(&twd, 1);
        assert!(
            detailed.iter().any(|r| r.starts_with(long)),
            "the whole name must survive: {detailed:?}"
        );
        // Header and every row share the one width, so the STATE field still
        // ends at one column once a long name has widened them.
        let head = detailed[1].find("STATE").unwrap() + "STATE".len();
        for row in &detailed[2..] {
            assert_eq!(
                row.find("Ok").unwrap() + "Ok".len(),
                head,
                "STATE must end at one column for header and rows: {detailed:?}"
            );
        }
    }

    /// The floor: a table holding only C-shaped names lays out at C's 16, so
    /// the widening rule costs nothing on an IOC that never registers one.
    #[test]
    fn short_names_keep_cs_sixteen_wide_layout() {
        let twd = watchdog();
        let _task = twd.insert("cbLow", CheckIn::Unbounded, None);

        let detailed = rows(&twd, 1);
        assert_eq!(
            detailed[1].find("STATE"),
            // C's `%16.16s %9s`: 16 for the name, the separating space, then
            // STATE right-aligned in 9.
            Some(16 + 1 + 4),
            "{:?}",
            detailed[1]
        );
    }

    #[test]
    fn the_report_is_counts_alone_until_a_level_is_asked_for() {
        let twd = watchdog();
        let _task = twd.insert("listed", CheckIn::Every(Duration::from_secs(2)), None);

        assert_eq!(rows(&twd, 0), vec!["0 monitors, 1 tasks registered"]);
        let detailed = rows(&twd, 1);
        assert_eq!(detailed.len(), 3, "counts, header, one task: {detailed:?}");
        assert!(detailed[1].contains("TASK NAME"));
        assert!(detailed[2].contains("2.0s"), "its declared interval");
    }

    /// The wiring, end to end and on the process's own watchdog: the real
    /// thread notices, says so on the console with C's wording, and calls the
    /// task's callback.
    #[test]
    fn the_watchdog_thread_reports_a_task_that_stops_checking_in() {
        let (console_tx, console_rx) = std::sync::mpsc::channel::<String>();
        let listener = crate::runtime::log::errlog_add_listener(move |m: &str| {
            if m.contains("stalls") {
                let _ = console_tx.send(m.to_string());
            }
        });

        let (fired_tx, fired_rx) = std::sync::mpsc::channel::<()>();
        let _task = taskwd_insert(
            "stalls",
            CheckIn::Every(Duration::from_millis(20)),
            Some(Arc::new(move || {
                let _ = fired_tx.send(());
            })),
        );
        TASKWD.set_period_for_test(Duration::from_millis(10));

        assert!(
            fired_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the watchdog thread must call a stalled task's callback"
        );
        let line = console_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("and say so on the console");
        assert!(
            line.contains("suspended"),
            "C's wording is what an operator greps for: {line}"
        );

        crate::runtime::log::errlog_remove_listener(listener);
    }
}
