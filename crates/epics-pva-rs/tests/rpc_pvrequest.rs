//! RPC INIT never masks its pvRequest (R6-32).
//!
//! pvxs `serverget.cpp:402` connects an RPC with a default-constructed
//! (falsy) prototype — `if(cmd==CMD_RPC) { ctrl->connect(Value()); }` — so
//! `ServerGPRConnect::connect`'s `if(prototype)` arm (`serverget.cpp:198-201`)
//! never runs and `request2mask()` is never invoked on an RPC pvRequest. The
//! reply is written whole (`to_wire(R, desc(value)) + to_wire_full(R, value)`,
//! `serverget.cpp:105-109`), never through a field mask.
//!
//! The Rust server used to run `request_to_mask()` on every INIT. An RPC's
//! prototype is `FieldDesc::Variant`, which matches no named selector, so
//! *any* `field(...)` selector in an RPC pvRequest produced `EmptyMask` and
//! the INIT was answered with `Status::Error "invalid pvRequest mask: …"` —
//! the RPC never ran.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.
#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::pv_request::PvRequestBuilder;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

/// An RPC echo PV: replies with `{ value: <the argument's `value` string> }`.
fn echo_server() -> (PvaServer, epics_pva_rs::client_native::context::PvaClient) {
    let pv = SharedPV::new();
    pv.on_rpc(|_pv, _desc, arg| {
        let echoed = match &arg {
            PvField::Structure(s) => match s.get_field("value") {
                Some(PvField::Scalar(ScalarValue::String(v))) => v.clone(),
                other => return Err(format!("bad rpc arg: {other:?}")),
            },
            other => return Err(format!("bad rpc arg: {other:?}")),
        };
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::String))],
        };
        let mut s = PvStructure::new("");
        s.set("value", PvField::Scalar(ScalarValue::String(echoed)));
        Ok((desc, PvField::Structure(s)))
    });

    let source = SharedSource::new();
    source.add("rpc:echo", pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();
    (server, client)
}

/// The RPC argument: `{ value: "ping" }`.
fn echo_arg() -> (FieldDesc, PvField) {
    let desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::String))],
    };
    let mut s = PvStructure::new("");
    s.set("value", PvField::Scalar(ScalarValue::String("ping".into())));
    (desc, PvField::Structure(s))
}

/// An RPC INIT pvRequest naming a field — what pvxs's
/// `RPCBuilder::pvRequest("field(value)")` sends, and what `pva_gateway`
/// forwards through `pvrpc_with_request` — must be accepted. pvxs never
/// masks an RPC, so the selector is simply carried to the source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_init_with_field_selector_is_not_masked() {
    let (server, client) = echo_server();

    let req = PvRequestBuilder::new().field("value").build();
    let (req_desc, req_value) = (req.to_field_desc(), req.to_pv_field());
    let (arg_desc, arg_value) = echo_arg();

    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc_with_request("rpc:echo", &req_desc, &req_value, &arg_desc, &arg_value),
    )
    .await
    .expect("rpc with field(value) pvRequest timed out")
    .expect("rpc with field(value) pvRequest must succeed — pvxs never masks an RPC")
    .into_value()
    .expect("value reply");

    match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::String(v))) => assert_eq!(v.as_str_lossy(), "ping"),
            other => panic!("unexpected rpc reply value: {other:?}"),
        },
        other => panic!("unexpected rpc reply: {other:?}"),
    }

    drop(server);
}

/// The boundary the old code could never reach: a selector naming a field
/// that exists in NO prototype at all. For a GET/PUT/MONITOR this is
/// `EmptyMask` (`pvrequest.cpp:61-62`, INIT error); for an RPC pvxs builds no
/// mask, so it cannot fail — the call runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_init_with_nonexistent_field_selector_still_runs() {
    let (server, client) = echo_server();

    let req = PvRequestBuilder::new().field("noSuchField").build();
    let (req_desc, req_value) = (req.to_field_desc(), req.to_pv_field());
    let (arg_desc, arg_value) = echo_arg();

    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc_with_request("rpc:echo", &req_desc, &req_value, &arg_desc, &arg_value),
    )
    .await
    .expect("rpc with field(noSuchField) pvRequest timed out")
    .expect("an RPC pvRequest is never masked, so no selector can empty-mask it")
    .into_value()
    .expect("value reply");

    match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::String(v))) => assert_eq!(v.as_str_lossy(), "ping"),
            other => panic!("unexpected rpc reply value: {other:?}"),
        },
        other => panic!("unexpected rpc reply: {other:?}"),
    }

    drop(server);
}
