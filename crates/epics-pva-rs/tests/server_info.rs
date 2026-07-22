//! Built-in `server` PV / channel-list facility.
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

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the feature-ON suite.

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
    pv_a.open(f64::descriptor(), f64::to_pv_field(&1.0))
        .unwrap();
    let pv_b = SharedPV::new();
    pv_b.open(f64::descriptor(), f64::to_pv_field(&2.0))
        .unwrap();

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
    .expect("op=channels rpc failed")
    .into_value()
    .expect("value reply");

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
        names.contains(&"test:alpha".into()),
        "channel list must include test:alpha — got {names:?}"
    );
    assert!(
        names.contains(&"test:beta".into()),
        "channel list must include test:beta — got {names:?}"
    );
    // The built-in `server` PV must NOT self-list.
    assert!(
        !names.contains(&"server".into()),
        "built-in 'server' PV must not appear in its own channel list"
    );

    drop(server);
}

/// `op=info` returns a bare structure with exactly `version` and
/// `implLang = rust` — pvxs `ServerSource::info` (serversource.cpp:19-22)
/// carries only `implLang` + `version`, no guid/peer/channel counters.
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
    .expect("op=info rpc failed")
    .into_value()
    .expect("value reply");

    let s = match resp {
        PvField::Structure(s) => s,
        other => panic!("unexpected info wrapper: {other:?}"),
    };
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
    // Rust-only fields are gone (pvxs parity).
    assert!(s.get_field("guid").is_none(), "guid must not be present");
    assert!(
        s.get_field("channelCount").is_none(),
        "channelCount must not be present"
    );

    drop(server);
}

/// A GET against the `server` PV must FAIL — pvxs `ServerSource`
/// installs only `onRPC` and no `onOp`, so `server` has no GET surface
/// (serversource.cpp:30-94). `op=info` over RPC is the supported path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_server_pv_has_no_surface() {
    let (server, client) = server_with_two_pvs();

    let resp = tokio::time::timeout(Duration::from_secs(5), client.pvget("server"))
        .await
        .expect("GET server timed out");
    assert!(
        resp.is_err(),
        "GET server must fail (pvxs has no onOp for `server`), got {resp:?}"
    );

    drop(server);
}

/// An unknown `op` value is rejected with pvxs's contract text. pvxs falls off
/// the `channels`/`info` chain into `eop->error("Not implemented")`
/// (serversource.cpp:93) — a bare string that neither echoes the bad op nor
/// lists the ones it knows. A missing `op` is not a hand-written diagnostic
/// either: `args["op"].as<std::string>()` (:53) throws `NoField` ("No such
/// field", data.cpp:17-19,419-422) and the EXEC catch forwards `e.what()` to
/// the client (serverget.cpp:504-508). The port used to answer
/// "unknown op '…' (expected 'channels' or 'info')" and
/// "missing 'op' query argument".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_op_errors_carry_pvxs_contract_text() {
    let (server, client) = server_with_two_pvs();

    let (desc, value) = nturi_op("not-a-real-op");
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("server", &desc, &value),
    )
    .await
    .expect("rpc timed out")
    .expect_err("unknown op must produce an RPC error");
    assert!(
        format!("{err}").contains("Not implemented"),
        "unknown op must return pvxs's bare \"Not implemented\", got: {err}"
    );

    // Missing `op`: an NTURI query structure with no `op` field at all.
    let mut query = PvStructure::new("");
    query.fields.push((
        "unrelated".into(),
        PvField::Scalar(ScalarValue::String("x".into())),
    ));
    let mut uri = PvStructure::new("epics:nt/NTURI:1.0");
    uri.fields.push((
        "scheme".into(),
        PvField::Scalar(ScalarValue::String("pva".into())),
    ));
    uri.fields.push(("query".into(), PvField::Structure(query)));
    let desc = FieldDesc::Structure {
        struct_id: "epics:nt/NTURI:1.0".into(),
        fields: vec![
            ("scheme".into(), FieldDesc::Scalar(ScalarType::String)),
            (
                "query".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![("unrelated".into(), FieldDesc::Scalar(ScalarType::String))],
                },
            ),
        ],
    };
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("server", &desc, &PvField::Structure(uri)),
    )
    .await
    .expect("rpc timed out")
    .expect_err("a missing op must produce an RPC error");
    assert!(
        format!("{err}").contains("No such field"),
        "a missing op must surface pvxs's NoField text, got: {err}"
    );

    drop(server);
}

/// Regression: pvxs registers its built-in `ServerSource` at
/// `(order = -1, "__server")` (server.cpp:542-547), BEFORE default-order
/// (0) user sources, and the lowest order is consulted first
/// (server.h:108-118). So a user source serving a PV literally named
/// `server` at default order does NOT shadow the diagnostic source —
/// `CREATE_CHANNEL`/GET/RPC for `server` reach the built-in source. The
/// Rust server previously registered the built-in at `i32::MAX` (lowest
/// priority), letting a user `server` PV win and hiding diagnostics from
/// `pvlist`-style clients. A user that genuinely wants `server` must now
/// register at an explicit order `< -1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_server_source_shadows_default_order_user_server_pv() {
    let user_server = SharedPV::new();
    let sentinel: f64 = 123.5;
    user_server
        .open(f64::descriptor(), f64::to_pv_field(&sentinel))
        .unwrap();
    let source = SharedSource::new();
    source.add("server", user_server);

    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    // RPC `op=info` against `server` must reach the built-in __server
    // source (returning the implLang/version identity), not the user's
    // NTScalar<f64> PV which has no RPC handler. GET is no longer the
    // discriminator — the built-in source has no GET surface (pvxs
    // onRPC-only); shadowing is proven by RPC reaching the built-in.
    let (desc, value) = nturi_op("info");
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("server", &desc, &value),
    )
    .await
    .expect("op=info rpc timed out")
    .expect("built-in __server must answer op=info, shadowing the user PV")
    .into_value()
    .expect("value reply");
    match resp {
        PvField::Structure(s) => {
            assert!(
                s.get_field("implLang").is_some() && s.get_field("version").is_some(),
                "built-in __server source must answer op=info, not the user PV: {s:?}"
            );
        }
        other => panic!(
            "expected the built-in info structure to shadow the user 'server' PV, got {other:?}"
        ),
    }

    drop(server);
}
