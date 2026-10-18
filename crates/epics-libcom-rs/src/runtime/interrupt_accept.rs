//! `interruptAccept` — the gate that says record processing and I/O Intr
//! callbacks are live, one per database.
//!
//! # Why this exists
//!
//! C's asyn param library defers every `callParamCallbacks` until the IOC has
//! finished wiring records to their driver. `paramList::callCallbacks` opens
//! with `if (!interruptAccept) return asynSuccess;` and returns **before**
//! `flags.clear()` (`asynPortDriver.cpp:838,871`), so every callback fired
//! while the IOC is still building is a no-op that *preserves* the accumulated
//! changed-flags. A per-port thread then does the callbacks exactly once, the
//! moment `interruptAccept` goes true (`callbackThread::run`,
//! `asynPortDriver.cpp:923-937`), delivering every seeded read-only value
//! (`Manufacturer`, `MaxSizeX`, …) to the `_RBV` records that only just
//! registered their interrupts.
//!
//! Without this gate the port's `call_param_callbacks` clears the changed-flags
//! whenever it happens to run first — a driver's own acquisition/array task can
//! fire it before `iocInit` wires the records — and a read-only parameter set
//! once at construction is then lost for the life of the process: its `_RBV`
//! record sits at the `.db` default forever. This is the owner of the flag that
//! `asyn-rs`'s `PortDriverBase::call_param_callbacks` consults and that the
//! scan facility drives.
//!
//! # One gate per database
//!
//! C has one database per process, so one `interruptAccept` serves as both
//! the database's gate and the process's. Here each database's scan facility
//! owns an [`InterruptGate`] and is its single writer: `scan_run` opens it
//! (C `scanRun`, `dbScan.c:218`), `scan_pause`/`scan_stop` close it
//! (`dbScan.c:241,165`). A gate is closed at creation, as C's flag is
//! (`dbAccess.c:67`).
//!
//! A port reads one gate: its owner's, set at `register_port`. A record of
//! another database on that port adds no reading — C has one
//! `interruptAccept` per port's IOC — and its own database's running check
//! decides whether it processes. There is no process-wide reading: a port
//! with no owner reads closed.
//!
//! # The one-shot flush
//!
//! A gate's `false → true` edge runs its [`InterruptGate::on_open`]
//! callbacks, never a `true`-while-`true`, so a pause→resume that never
//! lowered the flag does not re-flush. `asyn-rs` registers one such callback
//! per registered port that flushes its changed params once; it is this
//! crate's stand-in for C's per-port `callbackThread`.
//!
//! # Edge jobs
//!
//! An edge callback returns what it left running ([`EdgeDone`]): work it
//! cannot run inline is handed back for the gate to run, so no callback can
//! start work [`InterruptGate::jobs_settled`] does not see. The database's
//! `dbCaRun` step runs behind those jobs,
//! as C's `callbackThread` flush lands well ahead of the CP links' first
//! events, which only start connecting at `dbCaRun`.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type OnOpen = Arc<dyn Fn() -> EdgeDone + Send + Sync + 'static>;

/// What an edge callback leaves behind when it returns.
pub enum EdgeDone {
    /// Its work is done.
    Inline,
    /// Work to run on a thread of its own, named `name`, which the gate
    /// counts until it returns.
    Job {
        name: &'static str,
        job: Box<dyn FnOnce() + Send + 'static>,
    },
}

/// One database's `interruptAccept`: closed at creation, written only by that
/// database's scan facility.
pub struct InterruptGate {
    open: AtomicBool,
    on_open: Mutex<Vec<OnOpen>>,
    jobs: Arc<Jobs>,
}

/// The gate's running jobs.
#[derive(Default)]
struct Jobs {
    running: AtomicUsize,
    done: crate::runtime::sync::Notify,
}

/// One running job's count, released when its thread ends, a panic included,
/// or when the thread never started.
struct JobClaim(Arc<Jobs>);

impl Drop for JobClaim {
    fn drop(&mut self) {
        if self.0.running.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.done.notify_waiters();
        }
    }
}

impl InterruptGate {
    /// A closed gate.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            open: AtomicBool::new(false),
            on_open: Mutex::new(Vec::new()),
            jobs: Arc::default(),
        })
    }

    /// A gate that is already open: for a program or test that has no scan
    /// facility to open one.
    pub fn opened() -> Arc<Self> {
        let gate = Self::new();
        gate.set(true);
        gate
    }

    /// C `interruptAccept`, for the database that owns this gate.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// Set the gate; on a `false → true` edge run its callbacks. The scan
    /// facility of the owning database is the only caller.
    pub fn set(&self, open: bool) {
        let was = self.open.swap(open, Ordering::AcqRel);
        if open && !was {
            let own: Vec<OnOpen> = self
                .on_open
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for cb in own {
                if let EdgeDone::Job { name, job } = cb() {
                    self.spawn_job(name, job);
                }
            }
        }
    }

    /// Register a callback for this gate's next `false → true` edges.
    /// Registration does not fire it, even if the gate is already open.
    pub fn on_open<F: Fn() -> EdgeDone + Send + Sync + 'static>(&self, cb: F) {
        self.on_open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(cb));
    }

    /// Run `job` on a thread of its own, counted until it returns: an edge's
    /// [`EdgeDone::Job`], or work a producer starts while the gate is open.
    /// The thread is C asynPortDriver's callback thread's class
    /// (`asynPortDriver.cpp:907`), the work these jobs are.
    pub fn spawn_job(&self, name: &str, job: impl FnOnce() + Send + 'static) {
        self.jobs.running.fetch_add(1, Ordering::AcqRel);
        let claim = JobClaim(self.jobs.clone());
        let _ = crate::runtime::task::spawn_dedicated_thread(
            name.into(),
            crate::runtime::task::ThreadPriority::Medium,
            crate::runtime::task::StackSizeClass::Medium,
            move || {
                let _claim = claim;
                job();
            },
        );
    }

    /// Return once no job started by [`Self::spawn_job`] is running.
    pub async fn jobs_settled(&self) {
        loop {
            let done = self.jobs.done.notified();
            let mut done = std::pin::pin!(done);
            done.as_mut().enable();
            if self.jobs.running.load(Ordering::Acquire) == 0 {
                return;
            }
            done.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn a_gate_starts_closed_and_its_edge_fires_once_per_false_to_true() {
        let gate = InterruptGate::new();
        assert!(!gate.is_open());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        gate.on_open(move || {
            h.fetch_add(1, Ordering::SeqCst);
            EdgeDone::Inline
        });

        gate.set(true);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(gate.is_open());

        // true -> true: no fire.
        gate.set(true);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // false -> true again: a resume delivers what changed.
        gate.set(false);
        assert!(!gate.is_open());
        gate.set(true);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn registration_alone_does_not_fire() {
        let gate = InterruptGate::new();
        gate.set(true);
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        gate.on_open(move || {
            h.fetch_add(1, Ordering::SeqCst);
            EdgeDone::Inline
        });
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// Boundary — no job: settled at once.
    #[test]
    fn a_gate_with_no_job_is_settled() {
        crate::runtime::task::test_block_on(InterruptGate::new().jobs_settled());
    }

    /// Boundary — the job an edge callback handed back is still running:
    /// settled only after it returns.
    #[test]
    fn jobs_settled_waits_for_the_edges_job() {
        let gate = InterruptGate::new();
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        gate.on_open(move || {
            let r = r.clone();
            EdgeDone::Job {
                name: "edge-job-test",
                job: Box::new(move || {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    r.store(true, Ordering::SeqCst);
                }),
            }
        });
        gate.set(true);
        crate::runtime::task::test_block_on(gate.jobs_settled());
        assert!(ran.load(Ordering::SeqCst));
    }

    /// Boundary — a job that panics still releases its count.
    #[test]
    fn a_panicking_job_settles() {
        let gate = InterruptGate::new();
        gate.spawn_job("edge-job-panic", || panic!("edge job panics"));
        crate::runtime::task::test_block_on(gate.jobs_settled());
    }

    /// Boundary — one gate's edge does not run another gate's callbacks.
    #[test]
    fn an_edge_runs_only_its_own_gates_callbacks() {
        let a = InterruptGate::new();
        let b = InterruptGate::new();
        let (ha, hb) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (xa, xb) = (ha.clone(), hb.clone());
        a.on_open(move || {
            xa.fetch_add(1, Ordering::SeqCst);
            EdgeDone::Inline
        });
        b.on_open(move || {
            xb.fetch_add(1, Ordering::SeqCst);
            EdgeDone::Inline
        });
        a.set(true);
        assert_eq!(
            (ha.load(Ordering::SeqCst), hb.load(Ordering::SeqCst)),
            (1, 0)
        );
    }
}
