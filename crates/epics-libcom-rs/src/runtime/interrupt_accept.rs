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
//! A port reads the gates its records bound ([`InterruptGate`] is what
//! `asyn-rs` binds through the device-support hook) and accepts while any of
//! them is open. A port no record bound reads [`any_gate_open`]; that
//! fallback lives in one place so that giving every port an owner at
//! registration replaces one site.
//!
//! # The one-shot flush
//!
//! A gate's `false → true` edge runs its [`InterruptGate::on_open`]
//! callbacks and the process-wide [`on_any_gate_open`] ones, never a
//! `true`-while-`true`, so a pause→resume that never lowered the flag does
//! not re-flush. `asyn-rs` registers one process-wide callback that sweeps
//! every port's changed params once; it is this crate's stand-in for C's
//! per-port `callbackThread`.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type OnOpen = Arc<dyn Fn() + Send + Sync + 'static>;

/// How many live gates are open: maintained on each edge and in `Drop`, so
/// [`any_gate_open`] is one load and a port's read path takes no lock.
static OPEN_GATES: AtomicUsize = AtomicUsize::new(0);

/// Callbacks fired on any gate's `false → true` edge. `Arc` so the list can be
/// snapshotted and the lock dropped before a callback runs.
static ON_ANY_OPEN: Mutex<Vec<OnOpen>> = Mutex::new(Vec::new());

/// One database's `interruptAccept`: closed at creation, written only by that
/// database's scan facility.
pub struct InterruptGate {
    open: AtomicBool,
    on_open: Mutex<Vec<OnOpen>>,
}

impl InterruptGate {
    /// A closed gate.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            open: AtomicBool::new(false),
            on_open: Mutex::new(Vec::new()),
        })
    }

    /// A gate that is already open: for a test that has no scan facility to
    /// open one. The caller keeps it alive for as long as it must stay open.
    pub fn opened() -> Arc<Self> {
        let gate = Self::new();
        gate.set(true);
        gate
    }

    /// C `interruptAccept`, for the database that owns this gate.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// Set the gate; on a `false → true` edge run its own callbacks, then the
    /// process-wide ones. The scan facility of the owning database is the
    /// only caller.
    pub fn set(&self, open: bool) {
        let was = self.open.swap(open, Ordering::AcqRel);
        if !open && was {
            OPEN_GATES.fetch_sub(1, Ordering::AcqRel);
        }
        if open && !was {
            OPEN_GATES.fetch_add(1, Ordering::AcqRel);
            let own: Vec<OnOpen> = self
                .on_open
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let any: Vec<OnOpen> = ON_ANY_OPEN
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for cb in own.into_iter().chain(any) {
                cb();
            }
        }
    }

    /// Register a callback for this gate's next `false → true` edges.
    /// Registration does not fire it, even if the gate is already open.
    pub fn on_open<F: Fn() + Send + Sync + 'static>(&self, cb: F) {
        self.on_open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(cb));
    }
}

impl Drop for InterruptGate {
    /// A dropped database's open gate stops counting.
    fn drop(&mut self) {
        if *self.open.get_mut() {
            OPEN_GATES.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Whether any live database's gate is open: what a port that no record
/// bound to a database reads. The only place a port's gate is read as a
/// process-wide fact. One atomic load: it runs under a record's hold.
pub fn any_gate_open() -> bool {
    OPEN_GATES.load(Ordering::Acquire) > 0
}

/// Register a callback for any gate's `false → true` edge.
///
/// Registration does **not** fire the callback, even if a gate is already
/// open: a port that appears after the edge is the caller's concern.
pub fn on_any_gate_open<F: Fn() + Send + Sync + 'static>(cb: F) {
    ON_ANY_OPEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Arc::new(cb));
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
        });
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// Boundary — one gate's edge does not run another gate's callbacks, and
    /// the process-wide list sees every gate's edge.
    #[test]
    fn an_edge_runs_its_own_gates_callbacks_and_the_process_wide_ones() {
        let a = InterruptGate::new();
        let b = InterruptGate::new();
        let (ha, hb) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (xa, xb) = (ha.clone(), hb.clone());
        a.on_open(move || {
            xa.fetch_add(1, Ordering::SeqCst);
        });
        b.on_open(move || {
            xb.fetch_add(1, Ordering::SeqCst);
        });
        let any = Arc::new(AtomicUsize::new(0));
        let xany = any.clone();
        on_any_gate_open(move || {
            xany.fetch_add(1, Ordering::SeqCst);
        });
        a.set(true);
        assert_eq!(
            (ha.load(Ordering::SeqCst), hb.load(Ordering::SeqCst)),
            (1, 0)
        );
        assert_eq!(any.load(Ordering::SeqCst), 1);
    }

    /// Boundary — a dropped gate no longer holds the fallback open.
    #[test]
    fn a_dropped_open_gate_does_not_keep_the_fallback_open() {
        let g = InterruptGate::new();
        g.set(true);
        assert!(any_gate_open());
        drop(g);
        assert!(!any_gate_open());
    }
}
