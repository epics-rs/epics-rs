//! A registered port must not die because whoever created it dropped its
//! [`PortRuntimeHandle`].
//!
//! `create_port_runtime` hands back a `PortRuntimeHandle`. An iocsh port-create
//! command — in this crate or in any downstream IOC crate — publishes the
//! port's `PortHandle` to the registry and then returns, dropping its
//! `PortRuntimeHandle` at the end of the closure. That drop used to close the
//! actor's shutdown channel, killing the port: every later request through the
//! registry failed with "actor channel closed for port …".
//!
//! Staying alive must be a property of the port being *reachable*, not of
//! someone remembering to park the runtime handle in a static.

use std::time::Duration;

use asyn_rs::asyn_record::{get_port, register_port};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::trace::TraceManager;
use std::sync::Arc;

/// Blocking I/O below must fail, never hang, if this regresses.
const WATCHDOG: Duration = Duration::from_secs(10);

/// Run `f` on a scratch thread, failing (never hanging) if it does not finish
/// within [`WATCHDOG`]. A panic inside `f` is re-raised as a panic here, so a
/// regression is reported as the assertion that actually failed rather than as
/// a timeout.
fn with_watchdog<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(format!("watchdog-{what}"))
        .spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
        })
        .expect("spawn");
    match rx.recv_timeout(WATCHDOG) {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("{what} hung (no result within {WATCHDOG:?})"),
    }
}

struct TestPort {
    base: PortDriverBase,
}

impl TestPort {
    fn new(name: &str) -> Self {
        let mut base = PortDriverBase::new(name, 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        Self { base }
    }
}

impl PortDriver for TestPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// Exactly what an out-of-tree `drvAsynXxxPortConfigure` does: create the port,
/// publish it, return. The `PortRuntimeHandle` dies with the closure.
fn configure_port_like_iocsh(name: &str) {
    let (runtime, _join) = create_port_runtime(TestPort::new(name), RuntimeConfig::default())
        .expect("the port runtime thread must start");
    register_port(
        name,
        runtime.port_handle().clone(),
        Arc::new(TraceManager::new()),
    )
    .expect("port name is free");
    // `runtime` (and the actor thread's JoinHandle) drop here — the only thing
    // still reaching this port is the registry's PortHandle.
}

/// The defect. On unfixed main every request after the closure returns fails
/// with `actor channel closed for port drop_survivor`.
#[test]
fn registered_port_survives_drop_of_its_runtime_handle() {
    configure_port_like_iocsh("drop_survivor");

    let entry = get_port("drop_survivor").expect("port registered");
    let port: PortHandle = entry.handle.clone();

    with_watchdog("registered-port-io", move || {
        port.write_int32_blocking(0, 0, 42)
            .expect("write must succeed: the registry still reaches this port");
        assert_eq!(
            port.read_int32_blocking(0, 0)
                .expect("read must succeed: the registry still reaches this port"),
            42
        );
    });
}

/// Explicit shutdown is still explicit: it stops the actor even while other
/// `PortHandle`s (here, the registry's) can still reach the port. Dropping a
/// runtime handle must not be a shutdown, but calling `shutdown_and_wait` must.
#[test]
fn explicit_shutdown_still_stops_a_registered_port() {
    let (runtime, _join) = create_port_runtime(
        TestPort::new("explicit_shutdown_reg"),
        RuntimeConfig::default(),
    )
    .expect("the port runtime thread must start");
    register_port(
        "explicit_shutdown_reg",
        runtime.port_handle().clone(),
        Arc::new(TraceManager::new()),
    )
    .expect("port name is free");
    let registry_handle: PortHandle = get_port("explicit_shutdown_reg").unwrap().handle.clone();

    // Alive before.
    registry_handle.write_int32_blocking(0, 0, 1).unwrap();

    with_watchdog("explicit-shutdown", move || runtime.shutdown_and_wait());

    // Dead after — an explicit shutdown outranks any outstanding handle.
    with_watchdog("post-shutdown-io", move || {
        assert!(
            registry_handle.write_int32_blocking(0, 0, 2).is_err(),
            "an explicitly shut-down port must refuse further requests"
        );
    });
}

/// A port nobody can reach any more must still stop its actor thread — the
/// fix must not leak a thread per created-and-forgotten port.
#[test]
fn unreachable_port_stops_its_actor_thread() {
    let (runtime, join) =
        create_port_runtime(TestPort::new("unreachable"), RuntimeConfig::default())
            .expect("the port runtime thread must start");
    // No registry entry, no surviving PortHandle clone.
    drop(runtime);
    with_watchdog("unreachable-join", move || {
        join.join()
            .expect("actor thread must exit once nothing can reach the port");
    });
}
