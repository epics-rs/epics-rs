//! A synchronous port's boot flush runs as its owner gate's job, so the
//! gate's `jobs_settled` — the wait `dbCaRun`'s step makes — returns only
//! after every callback the flush fired has run and the port owes no sweep,
//! for a port registered before the open edge and for one registered while
//! the gate is already open. A CANBLOCK port's job only queues its sweep,
//! which the port owes — what a CP link's first event waits for — until its
//! callbacks have run. Only an asynPortDriver port has one.

// RTEMS-EXEC-MODEL-ALLOW(4): checked, not waived — all 4 ran and passed
// on the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest
// run -p asyn-rs --test boot_flush_is_its_owner_gates_job`, 4/4).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamType;
use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_libcom_rs::runtime::interrupt_accept::{InterruptGate, Outstanding};

struct SeededPort {
    base: PortDriverBase,
}

impl PortDriver for SeededPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// Register a port named `name`, its base built by `base` with `flags`,
/// seeded with one changed param on `gate`, whose flush callback takes
/// 100 ms; returns what the callbacks saw, the param and the port's edge
/// sweeps.
fn seeded_port(
    name: &str,
    gate: &Arc<InterruptGate>,
    base: fn(&str, usize, PortFlags, InterfaceMask, InterfaceMask) -> PortDriverBase,
    flags: PortFlags,
) -> (Arc<Mutex<Vec<usize>>>, usize, Arc<Outstanding>, impl Sized) {
    let mut base = base(
        name,
        1,
        flags,
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
    );
    let val = base.create_param("VAL", ParamType::Float64).unwrap();
    base.set_float64_param(val, 0, 4.0).unwrap();
    let (rt, jh) =
        create_port_runtime_unclaimed(SeededPort { base }, RuntimeConfig::default()).unwrap();
    let seen: Arc<Mutex<Vec<usize>>> = Arc::default();
    let sink = seen.clone();
    let sub = rt.port_handle().interrupts().register_sync_callback(
        InterruptFilter {
            reason: None,
            addr: Some(0),
            uint32_mask: None,
            iface: None,
            ..Default::default()
        },
        move |iv| {
            std::thread::sleep(Duration::from_millis(100));
            sink.lock().unwrap().push(iv.reason);
        },
    );
    asyn_rs::registry::register_port(name, rt.port_handle().clone(), gate).unwrap();
    let sweeps = rt.port_handle().interrupts().edge_sweeps().clone();
    (seen, val, sweeps, (rt, jh, sub))
}

/// Boundary — a port registered before the edge: the edge's job.
#[tokio::test]
async fn the_edges_flush_settles_after_its_callbacks() {
    let gate = InterruptGate::new();
    let (seen, val, sweeps, _port) = seeded_port(
        "FLUSHJOB_EDGE",
        &gate,
        PortDriverBase::new,
        PortFlags::default(),
    );
    gate.set(true);
    gate.jobs_settled().await;
    assert_eq!(*seen.lock().unwrap(), vec![val]);
    assert!(sweeps.is_clear());
}

/// Boundary — a port registered while its owner is open: the flush it
/// starts at once is the gate's job too.
#[tokio::test]
async fn an_open_owners_flush_settles_after_its_callbacks() {
    let gate = InterruptGate::new();
    gate.set(true);
    let (seen, val, sweeps, _port) = seeded_port(
        "FLUSHJOB_OPEN",
        &gate,
        PortDriverBase::new,
        PortFlags::default(),
    );
    gate.jobs_settled().await;
    assert_eq!(*seen.lock().unwrap(), vec![val]);
    assert!(sweeps.is_clear());
}

/// Boundary — a port that registers itself, as C's `drvAsyn*` ports do,
/// rather than deriving from asynPortDriver: C starts no `callbackThread` for
/// it, so the edge sweeps nothing, not even a param it holds.
#[tokio::test]
async fn a_self_registered_port_has_no_flush() {
    let gate = InterruptGate::new();
    let (seen, _, sweeps, _port) = seeded_port(
        "FLUSHJOB_BARE",
        &gate,
        PortDriverBase::new_exact_flags,
        PortFlags::default(),
    );
    gate.set(true);
    gate.jobs_settled().await;
    assert_eq!(*seen.lock().unwrap(), Vec::<usize>::new());
    assert!(sweeps.is_clear());
}

/// Boundary — a CANBLOCK port: its job settles once the sweep is queued, and
/// the port owes the sweep until its callbacks have run.
#[tokio::test]
async fn a_blocking_ports_sweep_is_owed_until_its_callbacks_run() {
    let gate = InterruptGate::new();
    let (seen, val, sweeps, _port) = seeded_port(
        "FLUSHJOB_BLOCK",
        &gate,
        PortDriverBase::new,
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
    );
    gate.set(true);
    gate.jobs_settled().await;
    sweeps.cleared().await;
    assert_eq!(*seen.lock().unwrap(), vec![val]);
}
