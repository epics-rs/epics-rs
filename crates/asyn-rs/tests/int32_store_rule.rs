//! A C driver overrides the virtual `setIntegerParam` to act on every store
//! of a reason — asynNDArrayDriver.cpp:636-663 sets `ADAcquireBusy` whenever
//! `ADAcquire` is stored. `PortDriverBase::set_int32_rule` is that override,
//! and every Int32 store path runs it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asyn_rs::error::{AsynError, AsynStatus};
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

struct Det {
    base: PortDriverBase,
}

impl PortDriver for Det {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

struct Params {
    acquire: usize,
    busy: usize,
    other: usize,
}

/// ADAcquire's rule without the plugin wait: busy follows acquire.
fn det(name: &str, runs: Arc<AtomicUsize>) -> (PortDriverBase, Params) {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    let acquire = base.create_param("ACQUIRE", ParamType::Int32).unwrap();
    let busy = base.create_param("ACQUIRE_BUSY", ParamType::Int32).unwrap();
    let other = base.create_param("OTHER", ParamType::Int32).unwrap();
    base.set_int32_rule(
        acquire,
        Arc::new(move |base: &mut PortDriverBase, list, value| {
            runs.fetch_add(1, Ordering::SeqCst);
            base.set_int32_param(busy, list, i32::from(value != 0))
        }),
    );
    (
        base,
        Params {
            acquire,
            busy,
            other,
        },
    )
}

#[test]
fn a_direct_store_runs_the_rule() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (mut base, p) = det("RULEDIRECT", runs.clone());

    base.set_int32_param(p.acquire, 0, 1).unwrap();
    assert_eq!(base.get_int32_param(p.busy, 0).unwrap(), 1);
    base.set_param_value(p.acquire, 0, ParamValue::Int32(0))
        .unwrap();
    assert_eq!(base.get_int32_param(p.busy, 0).unwrap(), 0);
    assert_eq!(base.get_int32_param(p.acquire, 0).unwrap(), 0);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[test]
fn the_actor_paths_run_the_rule() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (base, p) = det("RULEACTOR", runs.clone());
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Det { base }, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();

    handle
        .set_params_and_notify_blocking(
            0,
            vec![ParamSetValue::new(p.acquire, 0, ParamValue::Int32(1))],
        )
        .unwrap();
    let busy = p.busy;
    let read_busy = move |d: &mut Det| d.base.get_int32_param(busy, 0).unwrap();
    assert_eq!(handle.with_driver_blocking(read_busy).unwrap(), 1);

    // The default writeInt32 stores through setIntegerParam too.
    handle.write_int32_blocking(p.acquire, 0, 0).unwrap();
    assert_eq!(handle.with_driver_blocking(read_busy).unwrap(), 0);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[test]
fn a_reason_without_a_rule_stores_plainly() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (mut base, p) = det("RULENONE", runs.clone());

    base.set_int32_param(p.other, 0, 7).unwrap();
    base.set_param_value(p.other, 0, ParamValue::Int32(8))
        .unwrap();
    assert_eq!(base.get_int32_param(p.other, 0).unwrap(), 8);
    assert!(matches!(
        base.get_int32_param(p.busy, 0),
        Err(AsynError::ParamUndefined(_))
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failing_rule_does_not_stop_the_store() {
    let mut base = PortDriverBase::new(
        "RULEFAIL",
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    let acquire = base.create_param("ACQUIRE", ParamType::Int32).unwrap();
    base.set_int32_rule(
        acquire,
        Arc::new(|_: &mut PortDriverBase, _, _| {
            Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "rule".into(),
            })
        }),
    );

    assert!(base.set_int32_param(acquire, 0, 3).is_err());
    assert_eq!(base.get_int32_param(acquire, 0).unwrap(), 3);
}
