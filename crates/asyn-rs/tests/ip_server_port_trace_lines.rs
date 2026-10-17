//! `asynSetTraceMask SERVER -1 0x9` on a C drvAsynIPServerPort prints
//! connectIt, connectionListener, disconnect and closeConnection's lines
//! (drvAsynIPServerPort.c:157, :319-321, :347-349, :367-368, :488-489, :511-512).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::drivers::ip_server_port::{DrvAsynIPServerPort, IpServerConfig};
use asyn_rs::port::PortDriver;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::services::PortServices;
use asyn_rs::trace::{TraceFile, TraceMask};

#[test]
fn every_listener_line_is_c_s_text() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let info = format!("127.0.0.1:{port}");

    let services = PortServices::new();
    let trace = services.trace().clone();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.log");
    let file = Arc::new(Mutex::new(std::fs::File::create(&path).unwrap()));
    trace.set_trace_file(Some("SRVTR"), TraceFile::File(file));
    trace.set_trace_mask(
        Some("SRVTR"),
        TraceMask::ERROR | TraceMask::FLOW | TraceMask::IO_DRIVER,
    );
    let mut config = IpServerConfig::parse(&info).unwrap();
    config.max_clients = 1;
    let mut driver = DrvAsynIPServerPort::with_config("SRVTR", config).unwrap();
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
    handle.connect_blocking().unwrap();

    let log = || std::fs::read_to_string(&path).unwrap();
    let wait_for = |text: &str| {
        let until = Instant::now() + Duration::from_secs(5);
        while !log().contains(text) {
            assert!(Instant::now() < until, "{text:?} never printed:\n{}", log());
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let first = std::net::TcpStream::connect(&info).unwrap();
    wait_for("socket=");
    let second = std::net::TcpStream::connect(&info).unwrap();
    wait_for("too many clients");
    handle.disconnect_blocking().unwrap();
    drop((first, second));

    let log = log();
    let mut rest = log.as_str();
    for line in [
        "drvAsynIPServerPort: SRVTR connect\n".to_string(),
        format!("drvAsynIPServerPort: SRVTR started listening for connections on {info}\n"),
        "drvAsynIPServerPort: new connection, socket=".to_string(),
        format!(" on {info}\n"),
        "drvAsynIPServerPort: new connection, socket=".to_string(),
        "drvAsynIPServerPort: SRVTR: too many clients\n".to_string(),
        "drvAsynIPServerPort: SRVTR disconnect\n".to_string(),
        format!("drvAsynIPServerPort: close SRVTR connection on port {port}.\n"),
        format!("drvAsynIPServerPort: terminating connection thread for {info}\n"),
    ] {
        let at = rest
            .find(&line)
            .unwrap_or_else(|| panic!("{line:?} missing in order from:\n{log}"));
        rest = &rest[at + line.len()..];
    }
}
