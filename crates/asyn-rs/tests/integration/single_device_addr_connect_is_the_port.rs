//! A single-device port has no device dpCommon, so C's `asynCommon->disconnect`
//! closes the IP socket whatever addr the user carries (drvAsynIPPort.c:611),
//! and the next connect opens a new circuit.

use std::io::Read;
use std::sync::mpsc;
use std::time::Duration;

use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::port::PortDriver;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

#[test]
fn an_addr_zero_disconnect_closes_the_socket() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted, circuits) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            accepted.send(stream.unwrap()).unwrap();
        }
    });

    let mut driver = DrvAsynIPPort::new("ADDR0", &format!("127.0.0.1:{port}")).unwrap();
    driver.base_mut().init_auto_connect(false);
    let (rt, _jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
    let handle = rt.port_handle();
    let wait = Duration::from_secs(5);

    handle.connect_addr_blocking(0).unwrap();
    let mut first = circuits
        .recv_timeout(wait)
        .expect("connect opens a circuit");

    handle.disconnect_addr_blocking(0).unwrap();
    first.set_read_timeout(Some(wait)).unwrap();
    let mut sink = [0u8; 1];
    assert_eq!(
        first.read(&mut sink).unwrap(),
        0,
        "an addr-0 disconnect must close the socket"
    );

    handle.connect_addr_blocking(0).unwrap();
    circuits
        .recv_timeout(wait)
        .expect("the reconnect opens a new circuit");
}
