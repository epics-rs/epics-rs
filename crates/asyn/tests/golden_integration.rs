//! Golden integration tests for asyn-rs.
//!
//! These tests validate end-to-end scenarios that must not break
//! during the modernization refactoring.

use std::sync::Arc;
use std::time::Duration;

use asyn_rs::manager::PortManager;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::RequestOp;
use asyn_rs::sync_io::SyncIO;
use asyn_rs::user::AsynUser;

// -- Test driver --

struct GoldenDriver {
    base: PortDriverBase,
}

impl GoldenDriver {
    fn new(name: &str, can_block: bool) -> Self {
        let mut base = PortDriverBase::new(
            name,
            1,
            PortFlags {
                multi_device: false,
                can_block,
                destructible: true,
            },
        );
        base.create_param("INT_VAL", ParamType::Int32).unwrap();
        base.create_param("FLOAT_VAL", ParamType::Float64).unwrap();
        base.create_param("MSG", ParamType::Octet).unwrap();
        base.create_param("BITS", ParamType::UInt32Digital).unwrap();
        base.create_param("BIG", ParamType::Int64).unwrap();
        Self { base }
    }
}

impl PortDriver for GoldenDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

// -- Golden: register → queue → complete → SyncIO readback (can_block=false) --

#[test]
fn golden_register_queue_complete_sync_io() {
    let mgr = PortManager::new();
    mgr.register_port(GoldenDriver::new("golden_sync", false));

    // Write via queue_request
    let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
    let handle = mgr
        .queue_request("golden_sync", RequestOp::Int32Write { value: 42 }, user)
        .unwrap();
    handle.wait(Duration::from_secs(1)).unwrap();

    // Read back via SyncIO
    let sio = SyncIO::connect(&mgr, "golden_sync", 0, Duration::from_secs(1)).unwrap();
    assert_eq!(sio.read_int32(0).unwrap(), 42);
}

// -- Golden: register → queue → complete → SyncIO readback (can_block=true) --

#[test]
fn golden_register_queue_complete_sync_io_blocking() {
    let mgr = PortManager::new();
    mgr.register_port(GoldenDriver::new("golden_block", true));

    let user = AsynUser::new(1).with_timeout(Duration::from_secs(1));
    let handle = mgr
        .queue_request(
            "golden_block",
            RequestOp::Float64Write { value: 3.14 },
            user,
        )
        .unwrap();
    handle.wait(Duration::from_secs(1)).unwrap();

    let sio = SyncIO::connect(&mgr, "golden_block", 0, Duration::from_secs(1)).unwrap();
    assert!((sio.read_float64(1).unwrap() - 3.14).abs() < 1e-10);
}

// -- Golden: multi-type write/read roundtrip --

#[test]
fn golden_multi_type_roundtrip() {
    let mgr = PortManager::new();
    mgr.register_port(GoldenDriver::new("golden_multi", false));

    let sio = SyncIO::connect(&mgr, "golden_multi", 0, Duration::from_secs(1)).unwrap();

    // Int32
    sio.write_int32(0, -100).unwrap();
    assert_eq!(sio.read_int32(0).unwrap(), -100);

    // Float64
    sio.write_float64(1, 2.718).unwrap();
    assert!((sio.read_float64(1).unwrap() - 2.718).abs() < 1e-10);

    // Octet
    sio.write_octet(2, b"hello world").unwrap();
    let mut buf = [0u8; 64];
    let n = sio.read_octet(2, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello world");

    // UInt32Digital
    sio.write_uint32_digital(3, 0xAB, 0xFF).unwrap();
    assert_eq!(sio.read_uint32_digital(3, 0xFF).unwrap(), 0xAB);

    // Int64
    sio.write_int64(4, i64::MAX).unwrap();
    assert_eq!(sio.read_int64(4).unwrap(), i64::MAX);
}

// -- Golden: actor model write/read roundtrip --

#[test]
fn golden_actor_model_roundtrip() {
    let mgr = PortManager::new();
    let handle = mgr.register_port_actor(GoldenDriver::new("golden_actor", false));

    handle.write_int32_blocking(0, 0, 99).unwrap();
    assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 99);

    handle.write_float64_blocking(1, 0, 1.618).unwrap();
    assert!((handle.read_float64_blocking(1, 0).unwrap() - 1.618).abs() < 1e-10);
}

// -- Golden: actor model via SyncIOHandle --

#[test]
fn golden_sync_io_handle() {
    use asyn_rs::sync_io::SyncIOHandle;

    let mgr = PortManager::new();
    mgr.register_port_actor(GoldenDriver::new("golden_sio_handle", false));

    let sio = SyncIOHandle::connect(&mgr, "golden_sio_handle", 0, Duration::from_secs(1)).unwrap();
    sio.write_int32(0, 77).unwrap();
    assert_eq!(sio.read_int32(0).unwrap(), 77);

    sio.write_float64(1, 2.0).unwrap();
    assert!((sio.read_float64(1).unwrap() - 2.0).abs() < 1e-10);
}

// -- Golden: concurrent multi-client access --

#[test]
fn golden_concurrent_access() {
    let mgr = PortManager::new();
    mgr.register_port(GoldenDriver::new("golden_conc", true));

    let handles: Vec<_> = (0..10)
        .map(|i| {
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(5));
            mgr.queue_request(
                "golden_conc",
                RequestOp::Int32Write { value: i },
                user,
            )
            .unwrap()
        })
        .collect();

    for h in handles {
        h.wait(Duration::from_secs(5)).unwrap();
    }

    // Last writer wins
    let sio = SyncIO::connect(&mgr, "golden_conc", 0, Duration::from_secs(1)).unwrap();
    let val = sio.read_int32(0).unwrap();
    assert!((0..10).contains(&val));
}

// -- Golden: interrupt callback delivery --

#[tokio::test]
async fn golden_interrupt_delivery() {
    let mgr = PortManager::new();
    let port = mgr.register_port(GoldenDriver::new("golden_intr", false));

    let filter = asyn_rs::interrupt::InterruptFilter {
        reason: Some(0),
        addr: Some(0),
    };
    let (_sub, mut rx) = port
        .lock()
        .base()
        .interrupts
        .register_interrupt_user(filter);

    // Write + call_param_callbacks to trigger interrupt
    {
        let mut p = port.lock();
        let mut user = AsynUser::new(0);
        p.write_int32(&mut user, 42).unwrap();
        p.base_mut().call_param_callbacks(0).unwrap();
    }

    let v = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v.reason, 0);
}

// -- Golden: exception manager notification --

#[test]
fn golden_exception_notification() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mgr = PortManager::new();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    mgr.exception_manager().add_callback(move |event| {
        if event.exception == asyn_rs::exception::AsynException::Connect {
            c.fetch_add(1, Ordering::Relaxed);
        }
    });

    let port = mgr.register_port(GoldenDriver::new("golden_exc", false));
    {
        let mut p = port.lock();
        p.disconnect(&AsynUser::default()).unwrap();
    }
    assert_eq!(count.load(Ordering::Relaxed), 1);
}

// -- Golden: port shutdown lifecycle --

#[test]
fn golden_port_shutdown() {
    let mgr = PortManager::new();
    mgr.register_port(GoldenDriver::new("golden_shut", false));
    assert!(mgr.find_port("golden_shut").is_ok());

    mgr.shutdown_port("golden_shut").unwrap();
    assert!(mgr.find_port("golden_shut").is_err());
}

// -- Golden: adapter over PortHandle (actor model) --

#[cfg(feature = "epics")]
#[test]
fn golden_adapter_over_port_handle() {
    use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
    use epics_base_rs::server::device_support::DeviceSupport;
    use epics_base_rs::server::record::{Record, ScanType};
    use epics_base_rs::server::records::longin::LonginRecord;
    use epics_base_rs::types::EpicsValue;

    let mgr = PortManager::new();
    let handle = mgr.register_port_actor(GoldenDriver::new("golden_adapter", false));

    let link = AsynLink {
        port_name: "golden_adapter".into(),
        addr: 0,
        timeout: Duration::from_secs(1),
        drv_info: "INT_VAL".into(),
    };
    let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt32");
    ads.set_record_info("TEST:INT", ScanType::Passive);

    // Init resolves drv_info → reason
    let mut rec = LonginRecord::new(0);
    ads.init(&mut rec).unwrap();

    // Write
    rec.set_val(EpicsValue::Long(123)).unwrap();
    ads.write(&mut rec).unwrap();

    // Read back
    let mut rec2 = LonginRecord::new(0);
    ads.read(&mut rec2).unwrap();
    assert_eq!(rec2.val(), Some(EpicsValue::Long(123)));
}

// -- Golden: DrvUserCreate + multi-type via actor --

#[test]
fn golden_actor_drv_user_create() {
    let mgr = PortManager::new();
    let handle = mgr.register_port_actor(GoldenDriver::new("golden_drvuser", false));

    let reason = handle.drv_user_create_blocking("INT_VAL").unwrap();
    assert_eq!(reason, 0);

    let reason = handle.drv_user_create_blocking("FLOAT_VAL").unwrap();
    assert_eq!(reason, 1);

    let reason = handle.drv_user_create_blocking("MSG").unwrap();
    assert_eq!(reason, 2);
}
