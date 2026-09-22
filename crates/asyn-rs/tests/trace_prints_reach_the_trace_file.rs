//! `asynSetTraceMask PORT -1 0x1b` on a C IOC shows a request travel through
//! the layers: the port thread's `ASYN_TRACE_FLOW` callback line
//! (asynManager.c:904), `asynPortDriver::write*`'s `ASYN_TRACEIO_DRIVER` line
//! once the value is in the parameter library (asynPortDriver.cpp:2565), and
//! device support's `ASYN_TRACEIO_DEVICE` process line (devAsynFloat64.c:341,
//! :316). None of the three was printed here: the masks could be set, and the
//! trace file stayed empty.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};
use asyn_rs::services::PortServices;
use asyn_rs::trace::{TraceFile, TraceMask};
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::longout::LongoutRecord;

/// A driver that leaves every write to the `asynPortDriver` defaults.
struct TestPort {
    base: PortDriverBase,
}

impl PortDriver for TestPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    port: String,
    handle: asyn_rs::port_handle::PortHandle,
    _runtime: asyn_rs::runtime::PortRuntimeHandle,
}

fn fixture(port: &str) -> Fixture {
    let mut base = PortDriverBase::new(port, 1, PortFlags::default());
    base.create_param("VAL", ParamType::Float64).unwrap();
    let services = PortServices::new();
    let trace = services.trace().clone();
    let (runtime, _actor) = create_port_runtime(
        TestPort { base },
        RuntimeConfig {
            services,
            ..RuntimeConfig::default()
        },
    )
    .unwrap();
    let dir = tempfile::tempdir().expect("fixture root");
    let path = dir.path().join("asyn_trace.log");
    let file = std::fs::File::create(&path).unwrap();
    trace.set_trace_file(Some(port), TraceFile::File(Arc::new(Mutex::new(file))));
    trace.set_trace_mask(
        Some(port),
        TraceMask::ERROR | TraceMask::IO_DEVICE | TraceMask::IO_DRIVER | TraceMask::FLOW,
    );
    Fixture {
        _dir: dir,
        path,
        port: port.to_string(),
        handle: runtime.port_handle().clone(),
        _runtime: runtime,
    }
}

impl Fixture {
    fn device(&self, record: &str, iface: &str) -> AsynDeviceSupport {
        let link = AsynLink {
            port_name: self.port.clone(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(self.handle.clone(), link, iface);
        ads.set_record_info(record, ScanType::Passive);
        ads
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }
}

#[test]
fn a_record_write_and_read_print_at_every_layer() {
    let fx = fixture("TRPRINT");

    let mut ao = fx.device("TR:AO", "asynFloat64");
    let mut rec = AoRecord::new(0.0);
    ao.init(&mut rec).unwrap();
    rec.oval = 2.5;
    ao.write(&mut rec).unwrap();

    let mut ai = fx.device("TR:AI", "asynFloat64");
    let mut rec = AiRecord::new(0.0);
    ai.init(&mut rec).unwrap();
    ai.read(&mut rec).unwrap();

    let log = fx.log();
    for expected in [
        "asynManager::portThread port=TRPRINT callback",
        "asynPortDriver:writeFloat64: function=0, name=VAL, value=2.5",
        "TR:AO devAsynFloat64::processCallbackOutput process value 2.5",
        "TR:AI devAsynFloat64::processCallbackInput process value=2.5",
    ] {
        assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
    }
}

/// C prints a failing process at `ASYN_TRACE_ERROR` only when its status
/// differs from the last process's (`pPvt->lastStatus`, devAsynInt32.c:519-525):
/// a record that fails the same way every scan says so once.
#[test]
fn a_repeated_write_failure_prints_once() {
    let fx = fixture("TRERR");
    // An asynInt32 write into a Float64 parameter: the default `writeInt32`
    // refuses it every time.
    let mut lo = fx.device("TR:LO", "asynInt32");
    let mut rec = LongoutRecord::new(0);
    lo.init(&mut rec).unwrap();
    rec.val = 1;
    assert!(lo.write(&mut rec).is_err());
    rec.val = 2;
    assert!(lo.write(&mut rec).is_err());

    let log = fx.log();
    let needle = "TR:LO devAsynInt32::processCallbackOutput process write error";
    assert_eq!(
        log.matches(needle).count(),
        1,
        "one ERROR line for one status change in:\n{log}"
    );
    assert!(
        !log.contains("asynPortDriver:writeInt32"),
        "a refused write is not reported as I/O in:\n{log}"
    );
}
