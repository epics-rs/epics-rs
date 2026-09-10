//! Two-phase PUT (`PvaClient::pvput_begin` / `PutOp::commit`) against an
//! in-process `SharedPV`: the readback rides the put's own op, the DATA
//! phase writes the delta, an abandoned op is destroyed, and a circuit
//! lost between the phases surfaces as `Disconnected` rather than being
//! re-queued (pvxs `clientget.cpp:380-404`, non-autoExec).
#![cfg(all(feature = "client", tokio_backend))]

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client_native::ops_v2::{MonitorConnEvent, PutLeaf};
use epics_pva_rs::error::PvaError;
use epics_pva_rs::nt::NTScalar;
use epics_pva_rs::pv_request::PvRequestExpr;
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

const T: Duration = Duration::from_secs(5);

fn mailbox(initial: f64) -> (SharedPV, FieldDesc) {
    let desc = NTScalar::new(ScalarType::Double).build();
    let pv = SharedPV::build_mailbox();
    let mut value = epics_pva_rs::pvdata::encode::default_value_for(&desc);
    set_value(&mut value, initial);
    pv.open(desc.clone(), value).unwrap();
    (pv, desc)
}

fn set_value(v: &mut PvField, x: f64) {
    match v {
        PvField::Structure(s) => {
            let slot = s.fields.iter_mut().find(|(n, _)| n == "value").unwrap();
            slot.1 = PvField::Scalar(ScalarValue::Double(x));
        }
        other => panic!("not a structure: {other:?}"),
    }
}

fn value_of(v: &PvField) -> f64 {
    match v {
        PvField::Structure(s) => match s.fields.iter().find(|(n, _)| n == "value") {
            Some((_, PvField::Scalar(ScalarValue::Double(x)))) => *x,
            other => panic!("no double value: {other:?}"),
        },
        other => panic!("not a structure: {other:?}"),
    }
}

async fn next_conn_event(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<MonitorConnEvent>,
) -> MonitorConnEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for a connection event")
        .expect("the connection-event channel closed")
}

fn serve(pv: SharedPV) -> PvaServer {
    let source = SharedSource::new();
    source.add("TP:VAL", pv);
    PvaServer::isolated(Arc::new(source)).expect("isolated test server must start")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_present_reads_back_on_the_put_op_and_commit_writes_the_delta() {
    let (pv, desc) = mailbox(1.5);
    let server = serve(pv.clone());
    let client = server.client_config();

    let mut op = tokio::time::timeout(T, client.pvput_begin("TP:VAL", None, true))
        .await
        .expect("timeout")
        .expect("begin");
    assert_eq!(**op.introspection(), desc);
    let (present, marks) = op.take_present().expect("readback present");
    assert_eq!(value_of(&present), 1.5);
    assert!(marks.get(0) || marks.get(desc.bit_for_path("value").unwrap()));
    assert!(op.take_present().is_none(), "the readback is taken once");

    let mut value = present;
    set_value(&mut value, 2.5);
    let mut changed = epics_pva_rs::proto::BitSet::new();
    changed.set(desc.bit_for_path("value").unwrap());
    tokio::time::timeout(T, op.commit(&value, &changed))
        .await
        .expect("timeout")
        .expect("commit");
    assert_eq!(value_of(&pv.current().unwrap()), 2.5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_fetch_present_there_is_no_readback() {
    let (pv, _) = mailbox(1.5);
    let server = serve(pv.clone());
    let client = server.client_config();

    let req = PvRequestExpr::parse("field(value)").unwrap();
    let mut op = tokio::time::timeout(T, client.pvput_begin("TP:VAL", Some(&req), false))
        .await
        .expect("timeout")
        .expect("begin");
    assert!(op.take_present().is_none());
    tokio::time::timeout(
        T,
        op.commit_fields_typed(&[(
            "value".into(),
            PutLeaf::Typed(PvField::Scalar(ScalarValue::Double(7.0))),
        )]),
    )
    .await
    .expect("timeout")
    .expect("commit");
    assert_eq!(value_of(&pv.current().unwrap()), 7.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_abandoned_op_is_destroyed_and_the_channel_stays_usable() {
    let (pv, _) = mailbox(1.5);
    let server = serve(pv.clone());
    let client = server.client_config();

    for round in 0..3 {
        let op = tokio::time::timeout(T, client.pvput_begin("TP:VAL", None, true))
            .await
            .expect("timeout")
            .expect("begin");
        drop(op);
        let op = tokio::time::timeout(T, client.pvput_begin("TP:VAL", None, true))
            .await
            .expect("timeout")
            .expect("begin after drop");
        let x = 10.0 + round as f64;
        tokio::time::timeout(
            T,
            op.commit_fields_typed(&[(
                "value".into(),
                PutLeaf::Typed(PvField::Scalar(ScalarValue::Double(x))),
            )]),
        )
        .await
        .expect("timeout")
        .expect("commit after drop");
        assert_eq!(value_of(&pv.current().unwrap()), x);
    }
}

/// The circuit is lost between the phases. `drop(server)` is not the
/// loss itself — the connection task closes the socket asynchronously —
/// so the loss is observed the way a consumer observes it, through a
/// monitor's connection-event stream on the same circuit, before the
/// DATA phase is attempted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_on_a_lost_circuit_is_disconnected_not_requeued() {
    let (pv, _) = mailbox(1.5);
    let server = serve(pv.clone());
    let client = server.client_config();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _watch = client
        .pvmonitor_handle(
            "TP:VAL",
            |_desc, _value| {},
            move |ev| {
                let _ = tx.send(ev);
            },
        )
        .await
        .expect("monitor must start");
    assert!(matches!(
        next_conn_event(&mut rx).await,
        MonitorConnEvent::Connected { .. }
    ));

    let op = tokio::time::timeout(T, client.pvput_begin("TP:VAL", None, true))
        .await
        .expect("timeout")
        .expect("begin");
    drop(server);
    assert_eq!(
        next_conn_event(&mut rx).await,
        MonitorConnEvent::Disconnected
    );

    let err = tokio::time::timeout(
        T,
        op.commit_fields_typed(&[(
            "value".into(),
            PutLeaf::Typed(PvField::Scalar(ScalarValue::Double(9.0))),
        )]),
    )
    .await
    .expect("commit must fail promptly on a lost circuit")
    .expect_err("commit on a lost circuit");
    assert!(
        matches!(err, PvaError::Disconnected),
        "expected Disconnected, got {err:?}"
    );
    assert_eq!(value_of(&pv.current().unwrap()), 1.5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pvput_build_sees_the_readback_on_the_put_op() {
    let (pv, _) = mailbox(1.5);
    let server = serve(pv.clone());
    let client = server.client_config();

    tokio::time::timeout(
        Duration::from_secs(5),
        client.pvput_build("TP:VAL", |v| {
            assert_eq!(value_of(v), 1.5, "builder must see the present value");
            set_value(v, 3.0);
            Ok(())
        }),
    )
    .await
    .expect("timeout")
    .expect("pvput_build");
    assert_eq!(value_of(&pv.current().unwrap()), 3.0);
}
