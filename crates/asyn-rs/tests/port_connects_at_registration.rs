//! R18-61: an auto-connect port connects when it is registered, not when the
//! first request happens to arrive.
//!
//! C `registerPort` → `registerInterface(asynCommonType)` → `initPortConnect` +
//! `portConnectTimerCallback` + `waitConnect` (asynManager.c:2131-2136). The
//! timer callback queues a connect at `asynQueuePriorityConnect` the moment the
//! port exists (:3252-3266) and `waitConnect` blocks registration on the connect
//! exception for up to `DEFAULT_AUTOCONNECT_TIMEOUT` (0.5 s, :49/:507).
//!
//! Before the fix the port sat disconnected until some record's I/O forced a
//! lazy connect: `CNCT` read 0 straight after `drvAsynIPPortConfigure`, and a
//! port that no record ever talked to never came up at all.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::port::PortDriver;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};

/// The IP port connects with no request ever submitted: the TCP server sees the
/// connection land by itself.
#[test]
fn an_auto_connect_port_is_up_the_moment_it_is_created() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    std::thread::spawn(move || {
        // One accept is all this test needs; hold the socket open so the port
        // does not see an EOF and drop.
        if let Ok((mut stream, _)) = listener.accept() {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut sink = [0u8; 8];
            let _ = stream.read(&mut sink);
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    let driver = DrvAsynIPPort::new("R1861", &format!("127.0.0.1:{port}")).unwrap();
    assert!(
        driver.base().auto_connect,
        "drvAsynIPPortConfigure leaves autoConnect on unless noAutoConnect is given"
    );
    let (rt, _jh) = create_port_runtime(driver, RuntimeConfig::default())
        .expect("the port runtime thread must start");

    // No I/O request is submitted anywhere in this test: registration itself must
    // have brought the port up, and waited for it (C waitConnect). This is the
    // by-construction invariant — the connect exception `create_port_runtime`
    // waited on fires only AFTER `connect_tcp()` completes the handshake
    // (`ip_port.rs`: `connect_tcp()?` then `set_connected(true)`), so a live
    // `is_connected` proves the server's kernel has already accepted the circuit.
    assert!(
        rt.port_handle().is_connected_blocking(-1).unwrap(),
        "CNCT must read 1 straight after configure (asynRecord.c:1089-1093 reads \
         pasynManager->isConnected)"
    );
    // The server's `accept()` runs on its own thread, so it may not have returned
    // and bumped the counter the instant registration does — the connection is in
    // the kernel accept queue, but the accepting thread is a separate schedule.
    // Reading the counter with zero tolerance for that gap is a test-side race
    // (it flaked under load); wait a bounded moment for the accept to land.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while accepted.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "registering an auto-connect port must connect it — C queues the connect \
         at asynQueuePriorityConnect from portConnectTimerCallback and waits for \
         it (asynManager.c:2131-2136)"
    );
}

/// The `noAutoConnect` port is the negative control: C's `waitConnect` is gated
/// on `pport->dpc.autoConnect` (asynManager.c:2135) and its timer callback on
/// the same flag (:3258), so registration must leave this port down and must not
/// stall waiting for a connect that is never queued.
#[test]
fn a_no_auto_connect_port_stays_down_at_registration() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    std::thread::spawn(move || {
        if listener.accept().is_ok() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    let mut driver = DrvAsynIPPort::new("R1861N", &format!("127.0.0.1:{port}")).unwrap();
    driver.base_mut().auto_connect = false;
    let (rt, _jh) = create_port_runtime(driver, RuntimeConfig::default())
        .expect("the port runtime thread must start");

    assert!(
        !rt.port_handle().is_connected_blocking(-1).unwrap(),
        "a noAutoConnect port is not connected by registration"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "nothing may dial the device for a noAutoConnect port"
    );
}
