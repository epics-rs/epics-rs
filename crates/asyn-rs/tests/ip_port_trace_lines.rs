//! `asynSetTraceMask PORT -1 0x9` on a C drvAsynIPPort prints readIt, writeIt,
//! connectIt and closeConnection's lines, each naming the host-info spec
//! (drvAsynIPPort.c:208-209, :420-422, :591-592, :640-643, :751-753, :774-775,
//! :846-847).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::port::PortDriver;
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::services::PortServices;
use asyn_rs::trace::{TraceFile, TraceMask};
use asyn_rs::user::AsynUser;

#[test]
fn every_driver_line_is_c_s_text() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (wrote_tx, wrote) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut got = [0u8; 2];
        s.read_exact(&mut got).unwrap();
        s.write_all(b"xyz").unwrap();
        wrote_tx.send(()).unwrap();
        // Held open until the port closes it.
        let _ = s.read(&mut got);
    });

    let services = PortServices::new();
    let trace = services.trace().clone();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.log");
    let file = Arc::new(Mutex::new(std::fs::File::create(&path).unwrap()));
    trace.set_trace_file(Some("IPTR"), TraceFile::File(file));
    trace.set_trace_mask(Some("IPTR"), TraceMask::FLOW | TraceMask::IO_DRIVER);
    let mut driver = DrvAsynIPPort::new("IPTR", &host).unwrap();
    driver.base_mut().init_auto_connect(false);
    let (runtime, _actor) = create_port_runtime_unclaimed(
        driver,
        RuntimeConfig {
            services,
            ..RuntimeConfig::default()
        },
    )
    .unwrap();
    let handle = runtime.port_handle().clone();
    let user = || AsynUser::new(0).with_timeout(Duration::from_secs(2));

    handle.connect_blocking().unwrap();
    handle
        .submit_blocking(
            RequestOp::OctetWrite {
                data: b"ab".to_vec(),
            },
            user(),
        )
        .unwrap();
    wrote.recv_timeout(Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    handle
        .submit_blocking(RequestOp::OctetRead { buf_size: 10 }, user())
        .unwrap();
    handle.disconnect_blocking().unwrap();
    peer.join().unwrap();

    let log = std::fs::read_to_string(&path).unwrap();
    let mut rest = log.as_str();
    for line in [
        format!("Attempting to connect to {host}  reason:0  fd: -1\n"),
        format!("Opened connection OK to {host}\n"),
        format!("{host} write.\n"),
        format!("{host} write 2\n"),
        format!("wrote 2 to {host}, return asynSuccess.\n"),
        format!("{host} read.\n"),
        format!("{host} read 3\n"),
        format!("Closing {host} connection (fd "),
        "): Disconnect request\n".to_string(),
    ] {
        let at = rest
            .find(&line)
            .unwrap_or_else(|| panic!("{line:?} missing in order from:\n{log}"));
        rest = &rest[at + line.len()..];
    }
}
