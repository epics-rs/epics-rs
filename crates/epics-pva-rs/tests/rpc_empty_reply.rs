//! The pvxs no-value RPC reply — `ExecOp::reply()` (R6-34).
//!
//! pvxs has two `ExecOp::reply()` overloads (`srvcommon.h:108`): one taking a
//! `Value` and one taking nothing. The no-value form leaves `value` invalid, and
//! the RPC EXEC reply then carries a bare NULL type code with no body —
//! `serverget.cpp:104-112` writes `to_wire(R, desc(value))` (which emits `0xFF`
//! for a null desc, `dataencode.cpp:30-35`) and only appends `to_wire_full(R,
//! value)` `if(value)`.
//!
//! The client accepts it symmetrically: `clientget.cpp:415-421` does
//! `from_wire_type(M, rxRegistry, data); if(data) from_wire_full(...)` — a NULL
//! type code yields an invalid `Value` and the operation still completes
//! successfully.
//!
//! Rust used to be unable to express this on either side: `ChannelSource::rpc`
//! returned `(FieldDesc, PvField)` (a value was mandatory) and the client
//! unconditionally decoded a descriptor, so a pvxs server replying with the
//! no-value form failed the decode. `RpcReply` now models both overloads.

#![cfg(tokio_backend)]
#![cfg(test)]
#![cfg(feature = "client")]

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, RpcReply, ScalarType, ScalarValue};
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{SharedPV, SharedSource};

/// An RPC argument that carries nothing the handlers below care about.
fn empty_arg() -> (FieldDesc, PvField) {
    (
        FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        },
        PvField::Structure(PvStructure::new("")),
    )
}

/// End-to-end: a `SharedPV` RPC handler returning [`RpcReply::Empty`] makes the
/// server emit pvxs's no-value reply, and the client resolves the operation
/// successfully with `RpcReply::Empty` — not an error, and not an empty
/// structure masquerading as a value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_handler_can_reply_with_no_value() {
    let pv = SharedPV::new();
    pv.on_rpc(|_pv, _desc, _arg| Ok(RpcReply::Empty));

    let source = SharedSource::new();
    source.add("rpc:ack", pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    let (arg_desc, arg_value) = empty_arg();
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("rpc:ack", &arg_desc, &arg_value),
    )
    .await
    .expect("no-value rpc timed out")
    .expect("a no-value reply is a SUCCESS reply, not an error");

    assert_eq!(
        reply,
        RpcReply::Empty,
        "an `ExecOp::reply()` (NULL type code, no body) must decode to RpcReply::Empty"
    );

    drop(server);
}

/// The other boundary: the value-bearing overload (`ExecOp::reply(Value)`) still
/// round-trips as `RpcReply::Value`, so `Empty` is a distinct state rather than
/// a decode fallback that swallows real replies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_handler_returning_a_value_is_still_a_value_reply() {
    let pv = SharedPV::new();
    pv.on_rpc(|_pv, _desc, _arg| {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        };
        let mut s = PvStructure::new("");
        s.set("value", PvField::Scalar(ScalarValue::Int(7)));
        Ok((desc, PvField::Structure(s)))
    });

    let source = SharedSource::new();
    source.add("rpc:seven", pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    let (arg_desc, arg_value) = empty_arg();
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("rpc:seven", &arg_desc, &arg_value),
    )
    .await
    .expect("rpc timed out")
    .expect("rpc must succeed");

    let (_, value) = reply.into_value().expect("a value-bearing reply");
    match value {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Int(v))) => assert_eq!(*v, 7),
            other => panic!("unexpected rpc reply value: {other:?}"),
        },
        other => panic!("unexpected rpc reply: {other:?}"),
    }

    drop(server);
}
