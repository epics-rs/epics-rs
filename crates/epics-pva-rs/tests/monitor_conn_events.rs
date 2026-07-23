//! The monitor-handle API's connection-state stream.
//!
//! **Invariant.** A monitor consumer MUST learn connection transitions from
//! the monitor's own event/state stream and MUST NOT infer them from the
//! subscription handle/future terminating.
//!
//! The handle loops (`op_monitor_handle`, `spawn_raw_frames_handle`)
//! re-subscribe INTERNALLY on `MonitorEnd::ConnectionLost`: announce, sleep
//! 200 ms, loop. So a dead upstream never makes the handle's task return, and
//! a consumer watching only the task reports a dead upstream as connected and
//! keeps serving its last value — measured on the RTEMS stage-5 target and
//! recorded in doc/pvalink-rtems-design.md §12.8 / §12.10.
//!
//! These are the OWNER-path tests: with a real server dropped underneath a
//! real subscription, `MonitorConnEvent::Disconnected` must reach the
//! consumer **while the handle's task is still running** — that is exactly
//! the case termination-based inference cannot see.
//!
//! Reactor-dependent, and it is the RE-dial that needs the reactor: losing
//! the peer makes the client re-dial, and feature-ON that runs on a
//! background-executor thread with no tokio reactor while `dial_pva`'s hosted
//! arm is still `tokio::net`. Gated out feature-ON, same as
//! `upstream_death_disconnects_the_inp_monitor_link` in epics-bridge-rs.
#![cfg(not(feature = "rtems-exec-model"))]

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::client_native::ops_v2::MonitorConnEvent;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

fn nt_double(v: f64) -> PvField {
    PvField::Structure(PvStructure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), PvField::Scalar(ScalarValue::Double(v)))],
    })
}

fn nt_double_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
    }
}

/// A one-PV isolated server plus a client pinned to it.
fn fixture(pv_name: &str) -> (PvaServer, PvaClient) {
    let pv = SharedPV::build_mailbox();
    pv.open(nt_double_desc(), nt_double(1.0))
        .expect("SharedPV must open");
    let source = SharedSource::new();
    source.add(pv_name, pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("test PVA server must start");
    let addr = server.tcp_addr();
    let client = PvaClient::builder()
        .server_addr(addr)
        .timeout(Duration::from_secs(3))
        .build();
    (server, client)
}

async fn next_event(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<MonitorConnEvent>,
    what: &str,
) -> MonitorConnEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("the connection-event channel closed before {what}"))
}

/// RAW-FRAMES handle (the gateway's path — doc §12.10's "the raw-frames
/// monitor path has no connection-state hook"): a dead upstream must reach
/// the consumer as `Disconnected` while the handle's task is still alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_frames_handle_reports_upstream_loss_without_terminating() {
    let (server, client) = fixture("CONN:RAW:PV");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let handle = client
        .pvmonitor_raw_frames_handle(
            "CONN:RAW:PV",
            |_desc, _body, _order| {},
            move |ev| {
                let _ = tx.send(ev);
            },
        )
        .await
        .expect("raw monitor must start");

    let first = next_event(&mut rx, "the initial Connected").await;
    assert!(
        matches!(first, MonitorConnEvent::Connected { .. }),
        "a live subscription announces Connected first, got {first:?}"
    );

    drop(server);

    let ev = next_event(&mut rx, "Disconnected after the upstream died").await;
    assert_eq!(
        ev,
        MonitorConnEvent::Disconnected,
        "a dead upstream must reach the consumer as a connection-state \
         transition, not as the handle terminating"
    );
    // The point of the whole change: the transition arrived and the loop is
    // STILL RUNNING (it re-subscribes internally). A consumer that waited on
    // `wait_terminal()` would still be waiting here, reporting the dead
    // upstream as connected.
    assert!(
        !handle.is_done(),
        "the handle's task must still be running when the disconnect is \
         reported — termination is not the disconnect signal"
    );
}

/// TYPED handle (`op_monitor_handle`): same invariant, same owner. Both
/// handle constructors drive one `ConnEventOwner`, so neither can drift into
/// having no connection-state hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_handle_reports_upstream_loss_without_terminating() {
    let (server, client) = fixture("CONN:TYPED:PV");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let handle = client
        .pvmonitor_handle(
            "CONN:TYPED:PV",
            |_desc, _value| {},
            move |ev| {
                let _ = tx.send(ev);
            },
        )
        .await
        .expect("typed monitor must start");

    let first = next_event(&mut rx, "the initial Connected").await;
    assert!(
        matches!(first, MonitorConnEvent::Connected { .. }),
        "a live subscription announces Connected first, got {first:?}"
    );

    drop(server);

    let ev = next_event(&mut rx, "Disconnected after the upstream died").await;
    assert_eq!(
        ev,
        MonitorConnEvent::Disconnected,
        "a dead upstream must reach the consumer as a connection-state \
         transition, not as the handle terminating"
    );
    assert!(
        !handle.is_done(),
        "the handle's task must still be running when the disconnect is \
         reported — termination is not the disconnect signal"
    );
}

/// Boundary, not narrative: `ConnEventOwner` emits at most ONE departure per
/// `Connected`. With the upstream gone the loop retries the dial forever
/// (500 ms cadence), and each failed dial is NOT a new outage — a consumer
/// that revokes its cache on every `Disconnected` must not be handed a storm
/// of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_departure_per_connect_no_disconnect_storm() {
    let (server, client) = fixture("CONN:ONCE:PV");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let _handle = client
        .pvmonitor_raw_frames_handle(
            "CONN:ONCE:PV",
            |_desc, _body, _order| {},
            move |ev| {
                let _ = tx.send(ev);
            },
        )
        .await
        .expect("raw monitor must start");

    let first = next_event(&mut rx, "the initial Connected").await;
    assert!(matches!(first, MonitorConnEvent::Connected { .. }));

    drop(server);
    assert_eq!(
        next_event(&mut rx, "the single Disconnected").await,
        MonitorConnEvent::Disconnected
    );

    // Several dial-retry cycles' worth of quiet. No reconnect is possible
    // (the server is gone), so no further transition may be announced.
    let extra = tokio::time::timeout(Duration::from_millis(2500), rx.recv()).await;
    assert!(
        extra.is_err(),
        "one outage must announce exactly one departure; got a second \
         transition {extra:?} from failed dial retries"
    );
}
