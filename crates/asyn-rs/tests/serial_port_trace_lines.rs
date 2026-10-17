//! `asynSetTraceMask PORT -1 0x9` on a C drvAsynSerialPort prints connectIt,
//! writeIt, readIt and closeConnection's lines, each naming the device
//! (drvAsynSerialPort.c:630, :699, :762, :784-788, :864-865, :945-947, :978-979).
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::drivers::serial_port::DrvAsynSerialPort;
use asyn_rs::port::PortDriver;
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::services::PortServices;
use asyn_rs::trace::{TraceFile, TraceMask};
use asyn_rs::user::AsynUser;

#[test]
fn every_driver_line_is_c_s_text() {
    let (mut master, mut slave) = (0, 0);
    let mut name = [0 as libc::c_char; 256];
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            name.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert!(ret >= 0, "openpty: {}", std::io::Error::last_os_error());
    let dev = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::close(slave) };

    let services = PortServices::new();
    let trace = services.trace().clone();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.log");
    let file = Arc::new(Mutex::new(std::fs::File::create(&path).unwrap()));
    trace.set_trace_file(Some("SERTR"), TraceFile::File(file));
    trace.set_trace_mask(Some("SERTR"), TraceMask::FLOW | TraceMask::IO_DRIVER);
    let mut driver = DrvAsynSerialPort::new("SERTR", &dev).unwrap();
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
    let mut got = [0u8; 2];
    let n = unsafe { libc::read(master, got.as_mut_ptr().cast(), 2) };
    assert_eq!(n, 2);
    let n = unsafe { libc::write(master, b"xyz".as_ptr().cast(), 3) };
    assert_eq!(n, 3);
    handle
        .submit_blocking(RequestOp::OctetRead { buf_size: 10 }, user())
        .unwrap();
    handle.disconnect_blocking().unwrap();
    unsafe { libc::close(master) };

    let log = std::fs::read_to_string(&path).unwrap();
    let mut rest = log.as_str();
    for line in [
        format!("Open connection to {dev}\n"),
        format!("Opened connection to {dev}\n"),
        format!("{dev} write.\n"),
        format!("{dev} write 2\n"),
        format!("wrote 2 to {dev}, return asynSuccess\n"),
        format!("{dev} read.\n"),
        format!("{dev} read 3\n"),
        format!("{dev} read 3, return 0\n"),
        format!("{dev} disconnect\n"),
        format!("Close {dev} connection.\n"),
    ] {
        let at = rest
            .find(&line)
            .unwrap_or_else(|| panic!("{line:?} missing in order from:\n{log}"));
        rest = &rest[at + line.len()..];
    }
}
