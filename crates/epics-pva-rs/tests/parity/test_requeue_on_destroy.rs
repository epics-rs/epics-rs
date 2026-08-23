//! Regression: a one-shot GET that is in flight when the server sends
//! `CMD_DESTROY_CHANNEL` must be re-queued and completed, not failed.
//!
//! pvxs `Channel::disconnect` (`client.cpp:198-204`) runs
//! `op->disconnected(op)` for every op on the channel — on a circuit drop
//! and on a server-initiated DESTROY alike — and `GPROp::disconnected`
//! (`clientget.cpp:380-404`) pushes a one-call (`autoExec`, default per
//! `clientget.cpp:126`) op back into `chan->pending` with `state =
//! Connecting`. The channel re-enters a search bucket
//! (`client.cpp:209-213`) and `Channel::createOperations`
//! (`client.cpp:120-146`) re-issues it once the channel is Active again.
//!
//! The bug: epics-rs turned the dropped router slot into a terminal
//! `PvaError::Protocol("connection closed")`, so `pvget-rs` printed and
//! exited 1 with its remaining timeout unused — while the TCP circuit was
//! still up and serving that connection's other channels.
//!
//! `SharedPV::close()` is the trigger: it publishes through the bound
//! `ChannelInvalidator`, which is the port's `sharedpv.cpp:407-414`
//! `chan->close()` loop, and the client sees a server-initiated DESTROY.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::error::PvaError;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig, SharedPV, SharedSource};

fn nt_int_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
    }
}

fn nt_int_value(v: i32) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Int(v))));
    PvField::Structure(s)
}

/// A registered-but-closed `SharedPV`. `has_pv()` is true, so
/// CREATE_CHANNEL succeeds and the GET parks server-side waiting for the
/// descriptor — which is what keeps the op reliably in flight when the
/// DESTROY lands.
fn closed_pv_source() -> (Arc<SharedSource>, SharedPV) {
    let pv = SharedPV::new();
    let src = SharedSource::new();
    src.add("dut", pv.clone());
    (Arc::new(src), pv)
}

fn client_to(port: u16, op_timeout: Duration) -> PvaClient {
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    PvaClient::builder()
        .timeout(op_timeout)
        .server_addr(server_addr)
        .build()
}

/// The in-flight GET survives the DESTROY: the client re-searches,
/// re-creates the channel, re-issues the GET, and returns the value the
/// later `open()` published.
#[tokio::test]
async fn a_get_in_flight_when_the_server_destroys_the_channel_is_re_issued() {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };
    let (src, pv) = closed_pv_source();
    let server = PvaServer::start(src, cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let driver = pv.clone();
    tokio::spawn(async move {
        // The GET is parked on the server by now; tear its channel down.
        tokio::time::sleep(Duration::from_millis(400)).await;
        driver.close();
        // Then make the PV serviceable so the re-issued GET can answer.
        tokio::time::sleep(Duration::from_millis(400)).await;
        driver
            .open(nt_int_desc(), nt_int_value(7))
            .expect("open must succeed");
    });

    let client = client_to(port, Duration::from_secs(8));
    let v = tokio::time::timeout(Duration::from_secs(10), client.pvget("dut"))
        .await
        .expect("pvget timed out")
        .expect("a GET whose channel the server destroyed must be re-queued and completed");
    match v {
        PvField::Structure(s) => assert_eq!(
            s.get_field("value"),
            Some(&PvField::Scalar(ScalarValue::Int(7))),
            "the re-issued GET returns the reopened value"
        ),
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// Boundary: the re-queue is bounded by the caller's op timeout, not by
/// the attempt's. A channel the server keeps destroying ends the op as
/// `Timeout` at the caller's deadline — it neither returns early on the
/// first DESTROY nor loops past the budget.
#[tokio::test]
async fn a_repeatedly_destroyed_channel_ends_the_op_at_the_caller_deadline() {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };
    let (src, pv) = closed_pv_source();
    let server = PvaServer::start(src, cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let driver = pv.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_driver = stop.clone();
    tokio::spawn(async move {
        while !stop_driver.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(150)).await;
            driver.close();
        }
    });

    let client = client_to(port, Duration::from_secs(2));
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(10), client.pvget("dut"))
        .await
        .expect("pvget must end on its own deadline, not hang")
        .expect_err("a PV that never opens cannot answer");
    let elapsed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    assert!(
        matches!(err, PvaError::Timeout),
        "the caller's deadline owns the outcome, not the first DESTROY: got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "the re-queue must stop at the caller's budget, took {elapsed:?}"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}
