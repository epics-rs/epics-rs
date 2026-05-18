//! F6 — built-in `server` PV / channel-list facility.
//!
//! Mirrors pvxs `ServerSource` (`serversource.cpp`): every `PvaServer`
//! auto-registers a low-priority `__server` source exposing the
//! `server` PV. A `pvlist`-style client RPCs that PV with an NTURI
//! request carrying `query.op` — `op=channels` enumerates hosted
//! channel names, `op=info` returns server identity (GUID, version,
//! peer counts).
//!
//! These tests drive the facility end-to-end over a real in-process
//! PVA server + client, hosting a couple of user PVs and asserting the
//! built-in source reports them.

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::nt::typed::TypedNT;
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray,
};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

/// Build an NTURI RPC request with a single `query.op` string arg —
/// exactly what `pvxlist` / a `pvlist`-style client sends.
fn nturi_op(op: &str) -> (FieldDesc, PvField) {
    let mut query = PvStructure::new("");
    query
        .fields
        .push(("op".into(), PvField::Scalar(ScalarValue::String(op.into()))));
    let mut root = PvStructure::new("epics:nt/NTURI:1.0");
    root.fields.push((
        "scheme".into(),
        PvField::Scalar(ScalarValue::String("pva".into())),
    ));
    root.fields.push((
        "path".into(),
        PvField::Scalar(ScalarValue::String("server".into())),
    ));
    root.fields
        .push(("query".into(), PvField::Structure(query)));
    let desc = FieldDesc::Structure {
        struct_id: "epics:nt/NTURI:1.0".into(),
        fields: vec![
            ("scheme".into(), FieldDesc::Scalar(ScalarType::String)),
            ("path".into(), FieldDesc::Scalar(ScalarType::String)),
            (
                "query".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![("op".into(), FieldDesc::Scalar(ScalarType::String))],
                },
            ),
        ],
    };
    (desc, PvField::Structure(root))
}

/// Spin up a server hosting two NTScalar PVs and return a connected
/// client plus the live server handle.
fn server_with_two_pvs() -> (PvaServer, epics_pva_rs::client_native::context::PvaClient) {
    let pv_a = SharedPV::new();
    pv_a.open(f64::descriptor(), f64::to_pv_field(&1.0));
    let pv_b = SharedPV::new();
    pv_b.open(f64::descriptor(), f64::to_pv_field(&2.0));

    let source = SharedSource::new();
    source.add("test:alpha", pv_a);
    source.add("test:beta", pv_b);

    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();
    (server, client)
}

/// `op=channels` against a server hosting two PVs returns both names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_channels_lists_hosted_pvs() {
    let (server, client) = server_with_two_pvs();

    let (desc, value) = nturi_op("channels");
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("server", &desc, &value),
    )
    .await
    .expect("op=channels rpc timed out")
    .expect("op=channels rpc failed");

    let names = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::ScalarArrayTyped(TypedScalarArray::String(a))) => a.to_vec(),
            Some(PvField::ScalarArray(arr)) => arr
                .iter()
                .map(|v| match v {
                    ScalarValue::String(s) => s.clone(),
                    other => panic!("non-string channel name: {other:?}"),
                })
                .collect(),
            other => panic!("unexpected channels value shape: {other:?}"),
        },
        other => panic!("unexpected channels wrapper: {other:?}"),
    };

    assert!(
        names.contains(&"test:alpha".to_string()),
        "channel list must include test:alpha — got {names:?}"
    );
    assert!(
        names.contains(&"test:beta".to_string()),
        "channel list must include test:beta — got {names:?}"
    );
    // The built-in `server` PV must NOT self-list.
    assert!(
        !names.contains(&"server".to_string()),
        "built-in 'server' PV must not appear in its own channel list"
    );

    drop(server);
}

/// `op=info` returns the server-info structure with a GUID, a version
/// string equal to the crate version, and `implLang = rust`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_info_returns_server_identity() {
    let (server, client) = server_with_two_pvs();

    let (desc, value) = nturi_op("info");
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("server", &desc, &value),
    )
    .await
    .expect("op=info rpc timed out")
    .expect("op=info rpc failed");

    let s = match resp {
        PvField::Structure(s) => s,
        other => panic!("unexpected info wrapper: {other:?}"),
    };
    match s.get_field("guid") {
        Some(PvField::Scalar(ScalarValue::String(g))) => {
            assert_eq!(g.len(), 24, "GUID hex must be 24 chars: {g}");
        }
        other => panic!("unexpected guid field: {other:?}"),
    }
    match s.get_field("version") {
        Some(PvField::Scalar(ScalarValue::String(v))) => {
            assert_eq!(v, epics_pva_rs::VERSION);
        }
        other => panic!("unexpected version field: {other:?}"),
    }
    match s.get_field("implLang") {
        Some(PvField::Scalar(ScalarValue::String(l))) => assert_eq!(l, "rust"),
        other => panic!("unexpected implLang field: {other:?}"),
    }
    match s.get_field("channelCount") {
        Some(PvField::Scalar(ScalarValue::UInt(n))) => {
            assert_eq!(*n, 2, "two user PVs hosted");
        }
        other => panic!("unexpected channelCount field: {other:?}"),
    }

    drop(server);
}

/// A plain GET against the `server` PV returns the server-info
/// structure — pvxs's `ServerSource` answers GET the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_server_pv_returns_info() {
    let (server, client) = server_with_two_pvs();

    let resp = tokio::time::timeout(Duration::from_secs(5), client.pvget("server"))
        .await
        .expect("GET server timed out")
        .expect("GET server failed");

    match resp {
        PvField::Structure(s) => {
            assert!(s.get_field("guid").is_some(), "info struct has guid");
            assert!(s.get_field("version").is_some(), "info struct has version");
        }
        other => panic!("unexpected GET server response: {other:?}"),
    }

    drop(server);
}

/// An unknown `op` value is rejected with an error that names the bad
/// op, rather than silently returning empty data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_unknown_op_is_rejected() {
    let (server, client) = server_with_two_pvs();

    let (desc, value) = nturi_op("not-a-real-op");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("server", &desc, &value),
    )
    .await
    .expect("rpc timed out");

    assert!(
        result.is_err(),
        "unknown op must produce an RPC error, got {result:?}"
    );

    drop(server);
}

/// A user source serving a PV literally named `server` takes
/// precedence over the built-in source — the built-in `__server`
/// source is registered at the lowest priority (`order = i32::MAX`),
/// mirroring pvxs registering `ServerSource` at `(order = -1)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_server_pv_overrides_builtin() {
    let user_server = SharedPV::new();
    let sentinel: f64 = 123.5;
    user_server.open(f64::descriptor(), f64::to_pv_field(&sentinel));
    let source = SharedSource::new();
    source.add("server", user_server);

    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    // The user's NTScalar<f64> `server` PV must answer GET — not the
    // built-in info structure.
    let got: f64 =
        tokio::time::timeout(Duration::from_secs(5), client.pvget_typed::<f64>("server"))
            .await
            .expect("GET user server timed out")
            .expect("GET user server failed");
    assert_eq!(
        got, sentinel,
        "user 'server' PV must win over the built-in source"
    );

    drop(server);
}
