//! The flush when interrupts open is C `callbackThread::run`
//! (asynPortDriver.cpp:923-937), a bare `callParamCallbacks`: its switch
//! (:846-865) fires scalars only, and a `doCallbacks*Array` a constructor
//! issued before `iocInit` reached no record. NDPluginTimeSeries'
//! constructor `createAxisArray` (NDPluginTimeSeries.cpp:160) leaves
//! `TSTimeAxis` with NORD 0 in C; the rs flush posted the axis.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
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

#[test]
fn the_boot_flush_posts_a_seeded_scalar_and_no_seeded_array() {
    let mut base = PortDriverBase::new(
        "BOOTARR",
        1,
        PortFlags::default(),
        asyn_rs::port::InterfaceMask::NONE,
        asyn_rs::port::InterfaceMask::NONE,
    );
    let val = base.create_param("VAL", ParamType::Float64).unwrap();
    let arr = base.create_param("ARR", ParamType::Float64Array).unwrap();
    base.set_float64_param(val, 0, 4.0).unwrap();
    base.params
        .set_float64_array(arr, 0, vec![1.0, 2.0])
        .unwrap();
    let (rt, _jh) =
        create_port_runtime_unclaimed(SeededPort { base }, RuntimeConfig::default()).unwrap();

    let seen: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _sub = rt
        .port_handle()
        .interrupts()
        .register_sync_callback(
            InterruptFilter {
                reason: None,
                addr: Some(0),
                uint32_mask: None,
                iface: None,
                ..Default::default()
            },
            move |iv| sink.lock().unwrap().push(iv.reason),
        )
        .unwrap();

    let gate = InterruptGate::new();
    asyn_rs::registry::register_port("BOOTARR", rt.port_handle().clone(), &gate).unwrap();
    gate.set(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // The flush is one actor request; a second one queued behind it lands
    // after every post the flush made.
    rt.port_handle().call_param_callbacks_blocking(0).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![val]);
}
