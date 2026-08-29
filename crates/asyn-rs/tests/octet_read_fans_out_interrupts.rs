//! R18-59: an octet read on a stream port fans out to the octet interrupt users.
//!
//! C `asynOctetBase::readIt` (asynOctetBase.c:224-238) calls
//! `callInterruptUsers(pasynUser, pasynPvt, data, nbytesTransfered, eomReason)`
//! after every successful read, when the port enabled `interruptProcess` — which
//! both stream drivers do (`pasynOctetBase->initialize(..., 1)`,
//! drvAsynIPPort.c:1055 and drvAsynSerialPort.c:1125). It is what makes a
//! `stringin`/`waveform` with `SCAN="I/O Intr"` on a serial or IP port process:
//! the driver read is the interrupt.
//!
//! Before the fix no driver notified after a read, so such a record never ran.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamValue;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};
use asyn_rs::sync_io::SyncIOHandle;

/// A device that answers one line. The record on the other end is scanned
/// `I/O Intr`, so the read itself must deliver the value.
#[test]
fn a_stream_port_read_notifies_the_octet_interrupt_users() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::Write;
        if let Ok((mut s, _)) = listener.accept() {
            s.write_all(b"READBACK-1").unwrap();
            std::thread::sleep(Duration::from_secs(5));
        }
    });

    let driver = DrvAsynIPPort::new("R1859", &format!("127.0.0.1:{port}")).unwrap();
    let (rt, _jh) = create_port_runtime(driver, RuntimeConfig::default())
        .expect("the port runtime thread must start");

    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _sub = rt.port_handle().interrupts().register_sync_callback(
        InterruptFilter {
            reason: None,
            addr: Some(0),
            uint32_mask: None,
            iface: Some(InterfaceType::Octet),
        },
        move |iv| {
            if let ParamValue::Octet(s) = &iv.value {
                sink.lock().unwrap().push(s.clone());
            }
        },
    );

    let io = SyncIOHandle::from_handle(rt.port_handle().clone(), 0, Duration::from_secs(2));
    let data = io.read_octet(0, 32).expect("read failed");
    assert_eq!(data, b"READBACK-1");

    let got = seen.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![b"READBACK-1".to_vec()],
        "a successful octet read on a port with interruptProcess must fan out to the \
         octet interrupt users (C asynOctetBase.c:224-238) — this is what drives \
         SCAN=\"I/O Intr\""
    );
}
