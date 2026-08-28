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

#![cfg(tokio_backend)]
#![cfg(feature = "client")]

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::nt::typed::TypedNT;
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray,
};
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{SharedPV, SharedSource};

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

/// Name collision: a database record named literally `server`.
///
/// `record(ai,"server"){}` is legal, so the reserved diagnostic channel and
/// a user PV can want the same name. pvxs settles it by priority band, not
/// by name: its internals sit at `order = -1` (`src/server.cpp:542-546`) and an
/// application source added through `Server::addSource` defaults to
/// `order = 0` (`pvxs/server.h:116-118`), which is where QSRV's own sources
/// go (`ioc/singlesourcehooks.cpp:158`, `ioc/groupsourcehooks.cpp:219`).
/// CREATE_CHANNEL walks the registry ascending (`serverchan.cpp:304`), so
/// `ServerSource::onCreate` claims the `server` channel
/// (`serversource.cpp:30-33`) before the user source is ever asked, and the
/// record is shadowed. That is what keeps `pvxlist` alive: it reaches a
/// server by RPC-ing that same channel name (`tools/list.cpp:159-161`), so
/// a user PV winning the name is exactly what takes `pvxlist` / `pvxinfo`
/// off the air. Only pvxs's `addPV`-backed `builtinsrc` outranks the
/// diagnostic (`src/server.cpp:174-181`), and a hand-in source is not that.
///
/// Asserted as observable behaviour, not as a priority number, so a future
/// re-numbering that preserves the outcome keeps the test green.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_pv_named_server_does_not_take_pvxlist_off_the_air() {
    let colliding = SharedPV::new();
    let sentinel: f64 = 123.5;
    colliding
        .open(f64::descriptor(), f64::to_pv_field(&sentinel))
        .unwrap();
    let ordinary = SharedPV::new();
    let ordinary_value: f64 = 7.25;
    ordinary
        .open(f64::descriptor(), f64::to_pv_field(&ordinary_value))
        .unwrap();

    let source = SharedSource::new();
    source.add("server", colliding);
    source.add("test:ordinary", ordinary);

    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    // 1. `pvxlist` still answers, and it answers from the diagnostic source.
    let (desc, value) = nturi_op("channels");
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("server", &desc, &value),
    )
    .await
    .expect("op=channels rpc timed out")
    .expect("a user PV named `server` must not take op=channels off the air")
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
        names.contains(&"test:ordinary".into()),
        "channel list must still enumerate the user source — got {names:?}"
    );
    assert!(
        names.contains(&"server".into()),
        "the colliding record is still hosted and must still be listed — \
         pvxs unions Source::onList() over every source and ServerSource \
         contributes none — got {names:?}"
    );

    // 2. The record is shadowed for GET, because the diagnostic source
    //    claimed the channel and installs only onRPC
    //    (`serversource.cpp:36-95`) — there is no GET surface behind it.
    let got = tokio::time::timeout(Duration::from_secs(5), client.pvget("server"))
        .await
        .expect("GET server timed out");
    match got {
        Err(e) => {
            let _ = e;
        }
        Ok(v) => {
            let read = f64::from_pv_field(&v).ok();
            assert_ne!(
                read,
                Some(sentinel),
                "the diagnostic source must claim the `server` channel; a GET \
                 that reaches the colliding record means the user source was \
                 consulted first and `pvxlist` is one step from dead: {v:?}"
            );
        }
    }

    // 3. Every other name still falls through to the user source, so the
    //    band change costs the application nothing else.
    let resp = tokio::time::timeout(Duration::from_secs(5), client.pvget("test:ordinary"))
        .await
        .expect("GET test:ordinary timed out")
        .expect("a non-colliding user PV must still answer GET");
    assert_eq!(
        f64::from_pv_field(&resp).expect("user PV value"),
        ordinary_value,
        "non-colliding user PVs must be unaffected: {resp:?}"
    );

    drop(server);
}
