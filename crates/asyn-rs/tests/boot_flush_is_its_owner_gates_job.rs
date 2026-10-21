//! A synchronous port's boot flush runs as its owner gate's job, so the
//! gate's `jobs_settled` — the wait `dbCaRun`'s step makes — returns only
//! after every callback the flush fired has run, for a port registered before
//! the open edge and for one registered while the gate is already open. Only
//! an asynPortDriver port has one.

// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — all 3 ran and passed
// on the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest
// run -p asyn-rs --test boot_flush_is_its_owner_gates_job`, 3/3).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamType;
use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

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

/// Register a port named `name`, its base built by `base`, seeded with one
/// changed param on `gate`, whose flush callback takes 100 ms; returns what
/// the callbacks saw and the param.
fn seeded_port(
    name: &str,
    gate: &Arc<InterruptGate>,
    base: fn(&str, usize, PortFlags, InterfaceMask, InterfaceMask) -> PortDriverBase,
) -> (Arc<Mutex<Vec<usize>>>, usize, impl Sized) {
    let mut base = base(
        name,
        1,
        PortFlags::default(),
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
    (seen, val, (rt, jh, sub))
}

/// Boundary — a port registered before the edge: the edge's job.
#[tokio::test]
async fn the_edges_flush_settles_after_its_callbacks() {
    let gate = InterruptGate::new();
    let (seen, val, _port) = seeded_port("FLUSHJOB_EDGE", &gate, PortDriverBase::new);
    gate.set(true);
    gate.jobs_settled().await;
    assert_eq!(*seen.lock().unwrap(), vec![val]);
}

/// Boundary — a port registered while its owner is open: the flush it
/// starts at once is the gate's job too.
#[tokio::test]
async fn an_open_owners_flush_settles_after_its_callbacks() {
    let gate = InterruptGate::new();
    gate.set(true);
    let (seen, val, _port) = seeded_port("FLUSHJOB_OPEN", &gate, PortDriverBase::new);
    gate.jobs_settled().await;
    assert_eq!(*seen.lock().unwrap(), vec![val]);
}

/// Boundary — a port that registers itself, as C's `drvAsyn*` ports do,
/// rather than deriving from asynPortDriver: C starts no `callbackThread` for
/// it, so the edge sweeps nothing, not even a param it holds.
#[tokio::test]
async fn a_self_registered_port_has_no_flush() {
    let gate = InterruptGate::new();
    let (seen, _, _port) = seeded_port("FLUSHJOB_BARE", &gate, PortDriverBase::new_exact_flags);
    gate.set(true);
    gate.jobs_settled().await;
    assert_eq!(*seen.lock().unwrap(), Vec::<usize>::new());
}
