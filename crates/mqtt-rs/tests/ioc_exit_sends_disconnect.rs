//! MQ4's far half: does an IOC exit actually put a DISCONNECT **on the wire**?
//!
//! The near half is already pinned elsewhere — `IocApplication::run` calls
//! `epics_libcom_rs::runtime::exit::call_at_exits`, asyn registers every port
//! there, and stopping a port actor drops the driver, so `MqttDriver::drop`
//! runs (`crates/epics-base-rs/tests/ioc_run_is_the_shutdown_owner.rs`,
//! `crates/asyn-rs/tests/ioc_exit_stops_every_port_actor.rs`). None of that
//! says a byte left the process. `MqttDriver::drop` only calls
//! `Notify::notify_one`; the DISCONNECT is written much later, by a *different*
//! task on the tokio runtime, out of rumqttc's command channel. Between the two
//! sits process exit.
//!
//! C has no such gap: `~MqttClient` calls `disconnect()` and that call *waits*
//! for the packet (mqttClient.cpp:37-41,51-55), on the thread that is exiting.
//!
//! So this test asserts the wire, not the call graph, and it does it against a
//! real process exit — a child re-exec of this same test binary that boots an
//! IOC the way `examples/mqtt-ioc` does (`#[epics_main]`'s multi-threaded
//! runtime, `register_mqtt_commands`, an `st.cmd` with `mqttDriverConfigure`,
//! `IocApplication::run`) and then simply ends. The parent is the broker: it
//! CONNACKs, releases the child, and records every control packet until the
//! socket dies. An in-process `call_at_exits()` could not test this — the race
//! being tested *is* the process ending.

#![cfg(feature = "ioc")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The child is this same binary, re-exec'd; these three variables are both the
/// role switch and the whole handshake.
const BROKER_PORT: &str = "MQ4_BROKER_PORT";
const READY_PORT: &str = "MQ4_READY_PORT";
const SCRIPT: &str = "MQ4_SCRIPT";

/// Must match the `#[test]` function's name — it is what the child is told to
/// run with `--exact`.
const TEST_NAME: &str = "an_ioc_exit_puts_a_disconnect_on_the_wire";

/// MQTT control packet types (v5 §2.1.2), the two this test names.
const CONNECT: u8 = 1;
const DISCONNECT: u8 = 14;

/// Whole-test bound. Generous: it is a process spawn plus a boot, and a wedge
/// here must fail rather than hang a CI runner.
const DEADLINE: Duration = Duration::from_secs(60);

#[test]
fn an_ioc_exit_puts_a_disconnect_on_the_wire() {
    if std::env::var(BROKER_PORT).is_ok() {
        be_the_ioc();
    }
    be_the_broker();
}

// ---------------------------------------------------------------------------
// The broker half (parent)
// ---------------------------------------------------------------------------

fn be_the_broker() {
    let broker = TcpListener::bind("127.0.0.1:0").expect("bind a broker socket");
    // The child's protocol runner blocks reading one byte from here, so the
    // shutdown can never overtake the session it is supposed to close: the byte
    // is written only after the CONNACK is out. Without it a fast child could
    // reach exit while rumqttc is still connecting, and the event loop
    // deliberately sends nothing when the session is already down — a pass that
    // proved nothing.
    let ready = TcpListener::bind("127.0.0.1:0").expect("bind the release socket");
    let broker_port = broker.local_addr().unwrap().port();
    let ready_port = ready.local_addr().unwrap().port();

    let script = std::env::temp_dir().join(format!("mq4-st-{}.cmd", std::process::id()));
    std::fs::write(
        &script,
        format!(
            "mqttDriverConfigure(\"MQ4\", \"tcp://127.0.0.1:{broker_port}\", \"mq4-client\", 0)\n"
        ),
    )
    .expect("write the child's st.cmd");

    let mut child =
        std::process::Command::new(std::env::current_exe().expect("this test binary's own path"))
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env(BROKER_PORT, broker_port.to_string())
            .env(READY_PORT, ready_port.to_string())
            .env(SCRIPT, &script)
            .spawn()
            .expect("spawn the IOC child");

    let deadline = Instant::now() + DEADLINE;
    let outcome = broker_session(&broker, &ready, deadline);

    // Let the child end by itself. The question is what an *exiting* process
    // put on the wire, so killing it the moment its socket closed would answer
    // a different one — and would hide a DISCONNECT that only ever arrives
    // because the test held the process open.
    let status = wait_before(&mut child, deadline);
    let _ = std::fs::remove_file(&script);

    let session = outcome.unwrap_or_else(|e| panic!("broker session failed: {e} (child {status})"));

    // The whole subject of this test is which packets arrived, so say so even
    // on the pass: a green line with an empty list would be the tell that the
    // session ended before the assertion could mean anything.
    println!("MQ4: packet types after CONNACK: {session:?}; child exited {status}");

    assert!(
        session.contains(&DISCONNECT),
        "an IOC exit must put a DISCONNECT on the wire — C's `~MqttClient` \
         disconnects and waits for the packet (mqttClient.cpp:37-41,51-55). \
         Packets seen after CONNACK: {session:?}; child exited {status}"
    );
}

/// CONNACK the child, release it, then record every packet type until the
/// session ends. The CONNECT itself is asserted, not recorded.
fn broker_session(
    broker: &TcpListener,
    ready: &TcpListener,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let mut sock = accept_before(broker, deadline)?;
    sock.set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;

    match next_packet(&mut sock) {
        Ok(CONNECT) => {}
        Ok(other) => return Err(format!("first packet was type {other}, expected CONNECT")),
        Err(e) => return Err(format!("no CONNECT from the IOC child: {e}")),
    }

    // v5 CONNACK: session_present=0, reason=Success, no properties.
    sock.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
        .and_then(|()| sock.flush())
        .map_err(|e| format!("send CONNACK: {e}"))?;

    let mut go = accept_before(ready, deadline)?;
    go.write_all(b"g")
        .and_then(|()| go.flush())
        .map_err(|e| format!("release the child: {e}"))?;

    let mut seen = Vec::new();
    loop {
        match next_packet(&mut sock) {
            Ok(t) => seen.push(t),
            // Both endings are "the session is over": a clean FIN, or the
            // socket dying with the process. Neither carries a DISCONNECT.
            Err(_) => return Ok(seen),
        }
    }
}

/// Wait for the child to exit on its own, killing it only if it overruns the
/// whole-test bound.
fn wait_before(child: &mut std::process::Child, deadline: Instant) -> String {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.to_string(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return "did not exit within the test bound (killed)".to_string();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return format!("could not be reaped: {e}"),
        }
    }
}

fn accept_before(listener: &TcpListener, deadline: Instant) -> Result<TcpStream, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;
    let result = loop {
        match listener.accept() {
            Ok((sock, _)) => break Ok(sock),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break Err("timed out waiting for the IOC child to connect".to_string());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => break Err(format!("accept: {e}")),
        }
    };
    let _ = listener.set_nonblocking(false);
    let sock = result?;
    sock.set_nonblocking(false)
        .map_err(|e| format!("set_nonblocking(false): {e}"))?;
    Ok(sock)
}

/// Read one whole MQTT control packet and return its type nibble. The body is
/// consumed and discarded: this test is about which packets arrive, not what
/// they carry.
fn next_packet(sock: &mut TcpStream) -> Result<u8, String> {
    let mut header = [0u8; 1];
    fill(sock, &mut header)?;

    // Remaining Length: a 1..=4 byte varint (v5 §1.5.5).
    let mut remaining = 0usize;
    let mut shift = 0u32;
    loop {
        let mut b = [0u8; 1];
        fill(sock, &mut b)?;
        remaining |= ((b[0] & 0x7f) as usize) << shift;
        if b[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 21 {
            return Err("malformed Remaining Length".to_string());
        }
    }

    if remaining > 0 {
        let mut body = vec![0u8; remaining];
        fill(sock, &mut body)?;
    }
    Ok(header[0] >> 4)
}

fn fill(sock: &mut TcpStream, buf: &mut [u8]) -> Result<(), String> {
    let mut done = 0;
    while done < buf.len() {
        match sock.read(&mut buf[done..]) {
            Ok(0) => return Err("end of session".to_string()),
            Ok(n) => done += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The IOC half (child)
// ---------------------------------------------------------------------------

/// Boot an IOC the way `examples/mqtt-ioc/src/bin/mqtt_ioc.rs` does, then end
/// the process the way it does — by letting `main` fall off the end.
/// Deliberately no grace period and no `runtime::exit::exit`: the question is
/// whether the shipped shape reaches the wire.
fn be_the_ioc() -> ! {
    let ready_port: u16 = std::env::var(READY_PORT)
        .expect(READY_PORT)
        .parse()
        .expect("READY_PORT is a port number");
    let script = std::env::var(SCRIPT).expect(SCRIPT);

    // Exactly what `#[epics_main]` expands to.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let outcome = runtime.block_on(async move {
        let trace = Arc::new(asyn_rs::trace::TraceManager::new());
        let handle = epics_base_rs::runtime::task::runtime_handle();
        let app = epics_ca_rs::server::ioc_app::IocApplication::new();
        let app = mqtt_rs::ioc::register_mqtt_commands(app, handle, trace);
        app.startup_script(&script)
            .run(move |_config| async move {
                // The broker has CONNACKed by the time this byte arrives.
                use tokio::io::AsyncReadExt;
                let mut go = tokio::net::TcpStream::connect(("127.0.0.1", ready_port))
                    .await
                    .expect("connect to the release socket");
                let mut b = [0u8; 1];
                go.read_exact(&mut b)
                    .await
                    .expect("wait for the broker's release byte");
                Ok(())
            })
            .await
    });

    // `#[epics_main]`'s runtime is a temporary dropped when `main`'s tail
    // expression ends, i.e. here.
    drop(runtime);

    match outcome {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("MQ4 child: the IOC failed to run: {e}");
            std::process::exit(2)
        }
    }
}
