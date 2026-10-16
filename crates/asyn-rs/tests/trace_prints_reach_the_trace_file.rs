//! `asynSetTraceMask PORT -1 0x1b` on a C IOC shows a request travel through
//! the layers: the port thread's `ASYN_TRACE_FLOW` callback line
//! (asynManager.c:904), `asynPortDriver::write*`'s `ASYN_TRACEIO_DRIVER` line
//! once the value is in the parameter library (asynPortDriver.cpp:2565), and
//! device support's `ASYN_TRACEIO_DEVICE` process line (devAsynFloat64.c:341,
//! :316), each interface's own. None of the three was printed here: the masks could be set, and the
//! trace file stayed empty.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::services::PortServices;
use asyn_rs::trace::{TraceFile, TraceMask};
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::longout::LongoutRecord;

/// A driver that leaves every write to the `asynPortDriver` defaults, and
/// whose device never answers an octet read.
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
    fn io_write_octet(
        &mut self,
        _user: &mut asyn_rs::user::AsynUser,
        data: &[u8],
    ) -> asyn_rs::error::AsynResult<usize> {
        Ok(data.len())
    }
    fn io_read_octet(
        &mut self,
        _user: &asyn_rs::user::AsynUser,
        _buf: &mut [u8],
    ) -> asyn_rs::error::AsynResult<usize> {
        Err(asyn_rs::error::AsynError::Status {
            status: asyn_rs::error::AsynStatus::Timeout,
            message: "no reply".into(),
        })
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
    fixture_with(port, PortFlags::default())
}

fn fixture_with(port: &str, flags: PortFlags) -> Fixture {
    fixture_of(port, flags, |base| TestPort { base })
}

fn fixture_of<D: PortDriver>(
    port: &str,
    flags: PortFlags,
    driver: impl FnOnce(PortDriverBase) -> D,
) -> Fixture {
    let mut base = PortDriverBase::new(port, 1, flags);
    base.create_param("VAL", ParamType::Float64).unwrap();
    base.create_param("MSG", ParamType::Octet).unwrap();
    base.create_param("ARR", ParamType::Float64Array).unwrap();
    base.create_param("I32", ParamType::Int32).unwrap();
    base.create_param("I64", ParamType::Int64).unwrap();
    base.create_param("U32", ParamType::UInt32Digital).unwrap();
    let services = PortServices::new();
    let trace = services.trace().clone();
    let (runtime, _actor) = create_port_runtime_unclaimed(
        driver(base),
        RuntimeConfig {
            services,
            ..RuntimeConfig::default()
        },
    )
    .unwrap();
    asyn_rs::registry::register_port(
        port,
        runtime.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
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
        self.device_on(record, iface, "VAL")
    }

    fn device_on(&self, record: &str, iface: &str, param: &str) -> AsynDeviceSupport {
        self.device_at(record, iface, param, 0)
    }

    fn device_at(&self, record: &str, iface: &str, param: &str, addr: i32) -> AsynDeviceSupport {
        let link = AsynLink {
            port_name: self.port.clone(),
            addr,
            timeout: Some(Duration::from_secs(1)),
            drv_info: param.into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(self.handle.clone(), link, iface);
        ads.set_record_info(record, ScanType::Passive);
        ads
    }

    /// `record` bound through the asyn factory, as an IOC binds its DTYP.
    fn bound(
        &self,
        record: &str,
        record_type: &str,
        dtyp: &str,
        param: &str,
        rec: &mut dyn epics_base_rs::server::record::Record,
    ) -> Box<dyn DeviceSupport> {
        let link = format!("@asyn({},0,1){param}", self.port);
        let output = !dtyp.contains("ArrayOut") && dtyp != "asynOctetWrite";
        let ctx = epics_base_rs::server::ioc_app::DeviceSupportContext {
            record_type,
            dtyp,
            inp: if output { "" } else { &link },
            out: if output { &link } else { "" },
        };
        let mut dev = asyn_rs::adapter::universal_asyn_factory(&ctx).expect("the factory binds");
        dev.set_record_info(record, ScanType::Passive);
        dev.init(rec).unwrap();
        dev
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
        "asynPortDriver:writeFloat64: function=0, name=VAL, value=2.500000",
        "TR:AO devAsynFloat64::processCallbackOutput process value 2.500000",
        "TR:AI devAsynFloat64::processCallbackInput process value=2.500000",
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
    // An asynFloat64 write into an Int32 parameter: the default
    // `writeFloat64` returns setDoubleParam's refusal every time.
    let mut ao = fx.device_on("TR:AO", "asynFloat64", "I32");
    let mut rec = AoRecord::new(0.0);
    ao.init(&mut rec).unwrap();
    rec.oval = 1.0;
    assert!(ao.write(&mut rec).is_err());
    rec.oval = 2.0;
    assert!(ao.write(&mut rec).is_err());

    let log = fx.log();
    let needle = "TR:AO devAsynFloat64::processCallbackOutput process write error";
    assert_eq!(
        log.matches(needle).count(),
        1,
        "one ERROR line for one status change in:\n{log}"
    );
    // writeFloat64's own ASYN_TRACE_ERROR line, each time (:2560-2562).
    let refusal =
        "asynPortDriver:writeFloat64: error, status=8 function=3, name=I32, value=1.000000\n";
    assert!(log.contains(refusal), "missing {refusal:?} in:\n{log}");
    assert!(
        !log.contains("asynPortDriver:writeFloat64: function="),
        "a refused write is not reported as I/O in:\n{log}"
    );
}

/// C `reportQueueRequestStatus` (devAsynInt32.c:792-809): a `queueRequest` a
/// disabled blocking port refuses prints at `ASYN_TRACE_ERROR` once per change
/// of status, on the read and on the write side, and no `processCallback*` line
/// follows, as that callback never ran.
#[test]
fn a_repeated_queue_refusal_prints_once() {
    use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
    let fx = fixture_with(
        "TRQUEUE",
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
    );
    let mut ai = fx.device("TR:QAI", "asynFloat64");
    let mut ai_rec = AiRecord::new(0.0);
    ai.init(&mut ai_rec).unwrap();
    let mut ao = fx.device("TR:QAO", "asynFloat64");
    let mut ao_rec = AoRecord::new(0.0);
    ao.init(&mut ao_rec).unwrap();
    fx.handle.set_enable(-1, false).unwrap();

    let db = PvDatabase::new();
    let (ai_done, ao_done) = (
        DeviceDone::new(
            &db,
            &epics_base_rs::server::record::RecordInstance::new(
                "TR:QAI",
                epics_base_rs::server::records::ao::AoRecord::new(0.0),
            ),
        ),
        DeviceDone::new(
            &db,
            &epics_base_rs::server::record::RecordInstance::new(
                "TR:QAO",
                epics_base_rs::server::records::ao::AoRecord::new(0.0),
            ),
        ),
    );
    for _ in 0..2 {
        let _ = ai.read_begin(&mut ai_rec, &DeviceDoneSource::new(&ai_done));
        let _ = ai.read(&mut ai_rec);
        assert!(
            ao.write_begin(&mut ao_rec, &DeviceDoneSource::new(&ao_done))
                .is_err()
        );
    }

    let log = fx.log();
    for rec in ["TR:QAI", "TR:QAO"] {
        let needle = format!(
            "{rec} devAsynFloat64::reportQueueRequestStatus queueRequest error port TRQUEUE disabled"
        );
        assert_eq!(log.matches(&needle).count(), 1, "{needle:?} in:\n{log}");
    }
    assert!(!log.contains("process read error"), "{log}");
}

/// On a synchronous port C reports every `queueRequest`, so the first one
/// accepted after a refusal says the status returned to normal.
#[test]
fn a_sync_port_reports_the_refusal_and_its_end() {
    let fx = fixture("TRSYNCQ");
    let mut ao = fx.device("TR:SAO", "asynFloat64");
    let mut ao_rec = AoRecord::new(0.0);
    ao.init(&mut ao_rec).unwrap();
    ao_rec.oval = 1.0;
    ao.write(&mut ao_rec).unwrap();
    let mut ai = fx.device("TR:SAI", "asynFloat64");
    let mut rec = AiRecord::new(0.0);
    ai.init(&mut rec).unwrap();
    fx.handle.set_enable(-1, false).unwrap();
    let _ = ai.read(&mut rec);
    let _ = ai.read(&mut rec);
    fx.handle.set_enable(-1, true).unwrap();
    ai.read(&mut rec).unwrap();
    ai.read(&mut rec).unwrap();

    let log = fx.log();
    for needle in [
        "TR:SAI devAsynFloat64::reportQueueRequestStatus queueRequest error port TRSYNCQ disabled",
        "TR:SAI devAsynFloat64::reportQueueRequestStatus queueRequest status returned to normal",
    ] {
        assert_eq!(log.matches(needle).count(), 1, "{needle:?} in:\n{log}");
    }
    // The accepted read's callback ran inside queueRequest, so its line comes
    // before the status report (devAsynInt32.c:838-841).
    let read = log
        .find("TR:SAI devAsynFloat64::processCallbackInput process value=")
        .unwrap();
    let normal = log.find("queueRequest status returned to normal").unwrap();
    assert!(read < normal, "{log}");
    assert!(!log.contains("process read error"), "{log}");
}

/// On an `ASYN_CANBLOCK` port C's `processCallbackOutput` runs on the port
/// thread and prints there (devAsynInt32.c:511-528): the queued write's line,
/// its refusal by the driver once per change of status.
#[test]
fn a_queued_write_prints_on_the_port_thread() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
    let fx = fixture_with(
        "TRQWRITE",
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
    );
    let db = PvDatabase::new();
    let done = |name: &str| {
        DeviceDone::new(
            &db,
            &epics_base_rs::server::record::RecordInstance::new(name, AoRecord::new(0.0)),
        )
    };
    // Back once the request queued ahead at the record's band has run.
    let port_ran = || {
        let _ = fx.handle.submit_blocking(
            RequestOp::Float64Read,
            AsynUser::new(0).with_priority(asyn_rs::port::QueuePriority::Low),
        );
    };

    let mut ao = fx.device("TR:QWAO", "asynFloat64");
    let mut ao_rec = AoRecord::new(0.0);
    ao.init(&mut ao_rec).unwrap();
    let ao_done = done("TR:QWAO");
    ao_rec.oval = 2.5;
    ao.write_begin(&mut ao_rec, &DeviceDoneSource::new(&ao_done))
        .unwrap();
    port_ran();

    // writeFloat64 refuses a value for an Int32 parameter.
    let mut bad = fx.device_on("TR:QWBAD", "asynFloat64", "I32");
    let mut bad_rec = AoRecord::new(0.0);
    bad.init(&mut bad_rec).unwrap();
    let bad_done = done("TR:QWBAD");
    for v in [1.0, 2.0] {
        bad_rec.oval = v;
        bad.write_begin(&mut bad_rec, &DeviceDoneSource::new(&bad_done))
            .unwrap();
        port_ran();
    }

    let log = fx.log();
    assert!(
        log.contains("TR:QWAO devAsynFloat64::processCallbackOutput process value 2.500000"),
        "{log}"
    );
    let needle = "TR:QWBAD devAsynFloat64::processCallbackOutput process write error";
    assert_eq!(log.matches(needle).count(), 1, "{log}");
}

/// Each devAsyn file prints its own line. devAsynInt64.c:409-411
/// `"%s %s::processCallbackOutput process error %s"` has no "write";
/// devAsynOctet.c:688-689 `asynPrintIO(…, "%s devAsynOctet::writeIt\n", …)`
/// and never a `processCallbackOutput`; devAsynXXXArray.cpp:366-368
/// `"%s devAsynXXXArray::queueRequestCallback %s error %s\n"`.
#[test]
fn each_interface_prints_its_own_c_line() {
    use epics_base_rs::server::records::int64out::Int64outRecord;
    use epics_base_rs::server::records::stringout::StringoutRecord;
    use epics_base_rs::server::records::waveform::WaveformRecord;
    use epics_base_rs::types::{DbFieldType, EpicsValue};
    let fx = fixture_with(
        "TRIFACE",
        PortFlags {
            multi_device: true,
            ..PortFlags::default()
        },
    );

    let fx64 = fixture_of("TRIFACE64", PortFlags::default(), |base| RefusingPort {
        base,
    });
    let mut i64o = fx64.device_on("TR:I64", "asynInt64", "I64");
    let mut rec = Int64outRecord::new(0);
    i64o.init(&mut rec).unwrap();
    assert!(i64o.write(&mut rec).is_err());
    assert!(
        fx64.log()
            .contains("TR:I64 devAsynInt64::processCallbackOutput process error "),
        "{}",
        fx64.log()
    );

    let mut rec = StringoutRecord::new("hi");
    let mut so = fx.bound("TR:SO", "stringout", "asynOctetWrite", "MSG", &mut rec);
    so.write(&mut rec).unwrap();

    let mut rec = WaveformRecord::new(4, DbFieldType::Double);
    let mut wf = fx.bound("TR:WF", "waveform", "asynFloat64ArrayOut", "ARR", &mut rec);
    rec.put_field("VAL", EpicsValue::DoubleArray(vec![1.5, 2.5].into()))
        .unwrap();
    wf.read(&mut rec).unwrap();

    let log = fx.log();
    for expected in [
        "TR:SO devAsynOctet::writeIt\n",
        // TestPort keeps C's writeArray default, which refuses the put.
        "TR:WF devAsynXXXArray::queueRequestCallback write error asynPortDriver:writeArray not implemented\n",
    ] {
        assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
    }
    for absent in [
        "process write error",
        "devAsynOctet::processCallbackOutput",
        "TR:WF devAsynXXXArray::processCallback",
    ] {
        assert!(!log.contains(absent), "{absent:?} in:\n{log}");
    }
}

/// `asynOctetWrite` on a waveform is an input record that writes: C
/// callbackWfWrite runs writeIt on the port thread (devAsynOctet.c:1080-1088)
/// and prints its `"%s devAsynOctet::writeIt\n"` there.
#[test]
fn a_queued_waveform_octet_write_prints_writeit_on_the_port_thread() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
    use epics_base_rs::server::records::waveform::WaveformRecord;
    use epics_base_rs::types::{DbFieldType, EpicsValue};
    let fx = fixture_with(
        "TRWFWRITE",
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
    );
    let db = PvDatabase::new();
    let done = DeviceDone::new(
        &db,
        &epics_base_rs::server::record::RecordInstance::new("TR:WFW", AoRecord::new(0.0)),
    );
    let mut rec = WaveformRecord::new(8, DbFieldType::Char);
    let mut wf = fx.bound("TR:WFW", "waveform", "asynOctetWrite", "MSG", &mut rec);
    rec.put_field("VAL", EpicsValue::CharArray(b"ab".to_vec().into()))
        .unwrap();
    wf.read_begin(&mut rec, &DeviceDoneSource::new(&done))
        .unwrap();
    let _ = fx.handle.submit_blocking(
        RequestOp::Float64Read,
        AsynUser::new(0).with_priority(asyn_rs::port::QueuePriority::Low),
    );
    let before = fx.log();
    assert_eq!(
        before.matches("TR:WFW devAsynOctet::writeIt\n").count(),
        1,
        "printed by the port thread:\n{before}"
    );
    wf.read(&mut rec).unwrap();
    assert_eq!(fx.log(), before, "the reprocess pass prints nothing more");
}

/// devAsynInt32.c:485-492 masks `result.value` with the `@asynMask` nbits and
/// sign-extends a bipolar field before `"%s %s::%s process value=%d\n"`, so
/// the line shows the value the record gets.
#[test]
fn a_masked_int32_read_prints_the_masked_value() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    let fx = fixture("TRMASK");
    let _ = fx
        .handle
        .submit_blocking(RequestOp::Int32Write { value: 0xFFF }, AsynUser::new(3))
        .unwrap();

    let mut ai = fx
        .device_on("TR:MAI", "asynInt32", "I32")
        .with_mask((-12i32) as u32);
    let mut rec = AiRecord::new(0.0);
    ai.init(&mut rec).unwrap();
    ai.read(&mut rec).unwrap();

    let log = fx.log();
    assert!(
        log.contains("TR:MAI devAsynInt32::processCallbackInput process value=-1\n"),
        "{log}"
    );
    assert_eq!(rec.rval, -1);
}

/// A command-response whose reply never comes: C's callbackSiCmdResponse runs
/// writeIt, which prints the command (`asynPrintIO(…, "%s %s::%s\n", …)`,
/// devAsynOctet.c:688-689), then readIt, which prints
/// `"%s %s::%s failed %s\n"` (:706-709).
#[test]
fn a_cmd_response_read_failure_prints_the_write_then_the_read_failure() {
    use epics_base_rs::server::records::stringin::StringinRecord;
    let fx = fixture("TRCMD");
    let mut rec = StringinRecord::new("");
    let ctx = epics_base_rs::server::ioc_app::DeviceSupportContext {
        record_type: "stringin",
        dtyp: "asynOctetCmdResponse",
        inp: "@asyn(TRCMD,0,1)PING",
        out: "",
    };
    let mut si = asyn_rs::adapter::universal_asyn_factory(&ctx).expect("the factory binds");
    si.set_record_info("TR:CMD", ScanType::Passive);
    si.init(&mut rec).unwrap();
    let _ = si.read(&mut rec);

    let log = fx.log();
    let write = log
        .find("TR:CMD devAsynOctet::writeIt\n")
        .unwrap_or_else(|| panic!("no writeIt line in:\n{log}"));
    let read = log
        .find("TR:CMD devAsynOctet::readIt failed no reply\n")
        .unwrap_or_else(|| panic!("no readIt failure in:\n{log}"));
    assert!(write < read, "{log}");
}

/// An asynOctetRead whose device never answers (pixirad `SystemInfo`):
/// callbackSiRead's readIt prints `"%s %s::%s failed %s\n"`
/// (devAsynOctet.c:707-711), never a `processCallbackInput` line.
#[test]
fn an_octet_read_failure_prints_readit_failed() {
    use epics_base_rs::server::records::stringin::StringinRecord;
    let fx = fixture("TROREAD");
    let mut rec = StringinRecord::new("");
    let mut si = fx.bound("TR:SIR", "stringin", "asynOctetRead", "MSG", &mut rec);
    let _ = si.read(&mut rec);

    let log = fx.log();
    assert_eq!(
        log.matches("TR:SIR devAsynOctet::readIt failed no reply\n")
            .count(),
        1,
        "{log}"
    );
    assert!(!log.contains("processCallbackInput"), "{log}");
}

/// C's `write{Int32,Int64,UInt32Digital,Octet}` return callParamCallbacks'
/// status, not set*Param's (asynPortDriver.cpp:2028-2031, :2167-2170,
/// :2308-2311, :2672-2675): a value the parameter cannot hold is dropped, the
/// write succeeds, and the trace shows set*Param's `reportSetParamErrors`
/// line (:1099-1115) and then the I/O line, not an errorMessage.
/// (`TestPort` overrides the octet write, so writeOctet is pinned in port.rs.)
#[test]
fn a_write_the_store_refuses_succeeds_and_prints_the_io_line() {
    use epics_base_rs::server::records::bo::BoRecord;
    use epics_base_rs::server::records::int64out::Int64outRecord;
    let fx = fixture("TRDROP");

    let mut lo = fx.device("TR:DLO", "asynInt32");
    let mut rec = LongoutRecord::new(0);
    lo.init(&mut rec).unwrap();
    rec.val = 1;
    lo.write(&mut rec).unwrap();

    let mut i64o = fx.device("TR:DI64", "asynInt64");
    let mut rec = Int64outRecord::new(0);
    i64o.init(&mut rec).unwrap();
    i64o.write(&mut rec).unwrap();

    let mut bo = fx.device("TR:DBO", "asynUInt32Digital");
    let mut rec = BoRecord::new(0);
    bo.init(&mut rec).unwrap();
    bo.write(&mut rec).unwrap();

    let log = fx.log();
    for expected in [
        "asynPortDriver:writeInt32: function=0, name=VAL, value=1",
        "asynPortDriver:writeInt64: function=0, name=VAL, value=0",
        "asynPortDriver:writeUInt32Digital: function=0, name=VAL, value=0, mask=",
        "asynPortDriver:setIntegerParam: port=TRDROP error setting parameter 0 in list 0, wrong type\n",
        "asynPortDriver:setInteger64Param: port=TRDROP error setting parameter 0 in list 0, wrong type\n",
        "asynPortDriver:setUIntDigitalParam: port=TRDROP error setting parameter 0 in list 0, wrong type\n",
    ] {
        assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
    }
    assert!(!log.contains("status="), "{log}");
    // trace_print writes C's own newline once.
    assert!(!log.contains("\n\n"), "{log}");
}

/// A port whose writeInt64 fails, as a driver's override can.
struct RefusingPort {
    base: PortDriverBase,
}

impl PortDriver for RefusingPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_int64(
        &mut self,
        _user: &mut asyn_rs::user::AsynUser,
        _value: i64,
    ) -> asyn_rs::error::AsynResult<()> {
        Err(asyn_rs::error::AsynError::Status {
            status: asyn_rs::error::AsynStatus::Error,
            message: "refused".into(),
        })
    }
}

/// A port that keeps every `asynPortDriver` default, the octet read too.
struct DefaultPort {
    base: PortDriverBase,
}

impl PortDriver for DefaultPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// C's `read{Int32,Int64,UInt32Digital,Float64,Octet}` print the value read
/// at `ASYN_TRACEIO_DRIVER` (asynPortDriver.cpp:1988, :2127, :2267, :2516,
/// :2626); readUInt32Digital's is the masked value.
#[test]
fn a_default_read_prints_the_io_line() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    let fx = fixture_of("TRREAD", PortFlags::default(), |base| DefaultPort { base });
    for (reason, op) in [
        (0, RequestOp::Float64Write { value: 2.5 }),
        (
            1,
            RequestOp::OctetWrite {
                data: b"hi".to_vec(),
            },
        ),
        (3, RequestOp::Int32Write { value: -7 }),
        (4, RequestOp::Int64Write { value: 1 << 40 }),
        (
            5,
            RequestOp::UInt32DigitalWrite {
                value: 0xFF,
                mask: 0xFF,
            },
        ),
    ] {
        fx.handle
            .submit_blocking(op, AsynUser::new(reason))
            .unwrap();
    }
    for (reason, op) in [
        (0, RequestOp::Float64Read),
        (1, RequestOp::OctetRead { buf_size: 40 }),
        (3, RequestOp::Int32Read),
        (4, RequestOp::Int64Read),
        (5, RequestOp::UInt32DigitalRead { mask: 0x0F }),
    ] {
        fx.handle
            .submit_blocking(op, AsynUser::new(reason))
            .unwrap();
    }

    let log = fx.log();
    for expected in [
        "asynPortDriver:readFloat64: function=0, name=VAL, value=2.500000\n",
        "asynPortDriver:readOctet: function=1, name=MSG, value=hi\n",
        "asynPortDriver:readInt32: function=3, name=I32, value=-7\n",
        "asynPortDriver:readInt64: function=4, name=I64, value=1099511627776\n",
        "asynPortDriver:readUInt32Digital: function=5, name=U32, value=15, mask=15\n",
    ] {
        assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
    }
}

/// C's `getAddress` prints a refused address before `parseAsynUser` fails
/// the call (asynPortDriver.cpp:1907-1911), for reads and writes alike.
#[test]
fn a_refused_address_prints_the_getaddress_line() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    let multi = PortFlags {
        multi_device: true,
        ..PortFlags::default()
    };
    let fx = fixture_of("TRADDR", multi, |base| DefaultPort { base });
    for op in [
        RequestOp::Int32Read,
        RequestOp::Int32Write { value: 1 },
        RequestOp::Connect,
        RequestOp::Disconnect,
        RequestOp::DrvUserCreate(asyn_rs::port::DrvUserRequest::new("I32", 2)),
    ] {
        assert!(
            fx.handle
                .submit_blocking(op, AsynUser::new(3).with_addr(2))
                .is_err()
        );
    }
    let log = fx.log();
    assert_eq!(
        log.matches(
            "asynPortDriver:getAddress: TRADDR invalid address=2, must be in range 0 to 0\n"
        )
        .count(),
        5,
        "{log}"
    );
}

#[test]
fn drv_user_create_connect_and_disconnect_print_their_flow_lines() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    let fx = fixture_of("TRFLOW", PortFlags::default(), |base| DefaultPort { base });
    fx.handle
        .submit_blocking(
            RequestOp::DrvUserCreate(asyn_rs::port::DrvUserRequest::new("I32", 0)),
            AsynUser::new(0),
        )
        .unwrap();
    fx.handle
        .submit_blocking(RequestOp::Disconnect, AsynUser::new(0))
        .unwrap();
    fx.handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let log = fx.log();
    assert!(
        log.contains("asynPortDriver:drvUserCreate: drvInfo=I32, index="),
        "{log}"
    );
    assert!(
        log.contains("asynPortDriver:disconnect:, pasynUser=0x"),
        "{log}"
    );
    assert!(
        log.contains("asynPortDriver:connect:, pasynUser=0x"),
        "{log}"
    );
}

#[test]
fn an_unknown_drvinfo_prints_cannot_find_parameter() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    let fx = fixture_of("TRNOPARAM", PortFlags::default(), |base| DefaultPort {
        base,
    });
    assert!(
        fx.handle
            .submit_blocking(
                RequestOp::DrvUserCreate(asyn_rs::port::DrvUserRequest::new("NOPE", 0)),
                AsynUser::new(0),
            )
            .is_err()
    );
    let log = fx.log();
    assert!(
        log.contains("asynPortDriver:drvUserCreate: addr=0, cannot find parameter NOPE\n"),
        "{log}"
    );
}

/// `getAddr` is -1 on a port without `ASYN_MULTIDEVICE`, so `getAddress`
/// reads list 0 for any address the user carries (asynPortDriver.cpp:1905-1906).
#[test]
fn a_single_device_port_serves_any_address_from_list_0() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    let fx = fixture_of("TRANYADDR", PortFlags::default(), |base| DefaultPort {
        base,
    });
    fx.handle
        .submit_blocking(
            RequestOp::Int32Write { value: 9 },
            AsynUser::new(3).with_addr(2),
        )
        .unwrap();
    fx.handle
        .submit_blocking(
            RequestOp::DrvUserCreate(asyn_rs::port::DrvUserRequest::new("I32", 5)),
            AsynUser::new(0).with_addr(5),
        )
        .unwrap();
    let read = fx
        .handle
        .submit_blocking(RequestOp::Int32Read, AsynUser::new(3).with_addr(0))
        .unwrap();
    assert_eq!(read.int_val, Some(9));
    assert!(!fx.log().contains("getAddress"), "{}", fx.log());
}

/// C's default reads fetch through get*Param, whose reportGetParamErrors
/// prints a wrong type at `ASYN_TRACE_ERROR` and a never-set value at
/// `ASYN_TRACE_FLOW` (asynPortDriver.cpp:1575-1597).
#[test]
fn a_refused_param_read_prints_report_get_param_errors() {
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    let fx = fixture_of("TRGET", PortFlags::default(), |base| DefaultPort { base });
    for (reason, op) in [
        (0, RequestOp::Int32Read),
        (3, RequestOp::Int32Read),
        (1, RequestOp::Float64Read),
    ] {
        assert!(
            fx.handle
                .submit_blocking(op, AsynUser::new(reason).with_addr(0))
                .is_err()
        );
    }
    let log = fx.log();
    for expected in [
        "asynPortDriver:getIntegerParam: port=TRGET error getting parameter 0 VAL, in list 0, wrong type\n",
        "asynPortDriver:getIntegerParam: port=TRGET error getting parameter 3 I32, in list 0, value undefined\n",
        "asynPortDriver:getDoubleParam: port=TRGET error getting parameter 1 MSG, in list 0, wrong type\n",
    ] {
        assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
    }
}
