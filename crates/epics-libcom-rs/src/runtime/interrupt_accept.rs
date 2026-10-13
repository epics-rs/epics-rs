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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type OnOpen = Arc<dyn Fn() + Send + Sync + 'static>;

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

    /// Boundary — one gate's edge does not run another gate's callbacks.
    #[test]
    fn an_edge_runs_only_its_own_gates_callbacks() {
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
        a.set(true);
        assert_eq!(
            (ha.load(Ordering::SeqCst), hb.load(Ordering::SeqCst)),
            (1, 0)
        );
    }
}
