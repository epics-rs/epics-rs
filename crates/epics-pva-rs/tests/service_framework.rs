//! Smoke test for `#[pva_service]` + `add_rpc_service`. Spins up
//! an in-process server with a service that exposes two RPC
//! methods, then drives them via `PvaClient::pvrpc`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedSource};
use epics_pva_rs::service::pva_service;

#[derive(Default)]
struct Counter {
    value: AtomicI64,
}

#[pva_service]
impl Counter {
    /// Add `delta` to the counter; returns the new value.
    async fn add(&self, delta: i64) -> Result<i64, String> {
        let new = self.value.fetch_add(delta, Ordering::Relaxed) + delta;
        Ok(new)
    }

    /// Reset the counter to `value`; returns the previous value.
    async fn reset(&self, value: i64) -> Result<i64, String> {
        let prev = self.value.swap(value, Ordering::Relaxed);
        Ok(prev)
    }

    /// Square the input. Pure compute, no state.
    async fn square(&self, x: f64) -> Result<f64, String> {
        Ok(x * x)
    }
}

fn nturi_request(args: &[(&str, ScalarValue)]) -> (FieldDesc, PvField) {
    let mut query_fields = Vec::new();
    let mut query_desc = Vec::new();
    for (name, val) in args {
        let st = match val {
            ScalarValue::Long(_) => ScalarType::Long,
            ScalarValue::Int(_) => ScalarType::Int,
            ScalarValue::Double(_) => ScalarType::Double,
            ScalarValue::String(_) => ScalarType::String,
            _ => ScalarType::String,
        };
        query_fields.push((name.to_string(), PvField::Scalar(val.clone())));
        query_desc.push((name.to_string(), FieldDesc::Scalar(st)));
    }
    let mut query = PvStructure::new("");
    query.fields = query_fields;
    let mut root = PvStructure::new("epics:nt/NTURI:1.0");
    root.fields.push((
        "scheme".into(),
        PvField::Scalar(ScalarValue::String("pva".into())),
    ));
    root.fields.push((
        "path".into(),
        PvField::Scalar(ScalarValue::String(String::new())),
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
                    struct_id: "".into(),
                    fields: query_desc,
                },
            ),
        ],
    };
    (desc, PvField::Structure(root))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_service_dispatch_round_trip() {
    let source = SharedSource::new();
    let registered = epics_pva_rs::service::add_rpc_service(&source, "counter", Counter::default());
    assert_eq!(registered.len(), 3);
    assert!(registered.contains(&"counter:add".to_string()));
    assert!(registered.contains(&"counter:reset".to_string()));
    assert!(registered.contains(&"counter:square".to_string()));

    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    // counter:square — pure compute, easy parity check.
    let (desc, value) = nturi_request(&[("x", ScalarValue::Double(7.5))]);
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("counter:square", &desc, &value),
    )
    .await
    .expect("rpc timeout")
    .expect("rpc err");
    let result = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => *v,
            other => panic!("unexpected square response shape: {other:?}"),
        },
        other => panic!("unexpected response wrapper: {other:?}"),
    };
    assert_eq!(result, 56.25);

    // counter:add — stateful.
    let (desc, value) = nturi_request(&[("delta", ScalarValue::Long(5))]);
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("counter:add", &desc, &value),
    )
    .await
    .expect("add timeout")
    .expect("add err");
    let v1 = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            other => panic!("unexpected add response shape: {other:?}"),
        },
        other => panic!("unexpected response wrapper: {other:?}"),
    };
    assert_eq!(v1, 5);

    let (desc, value) = nturi_request(&[("delta", ScalarValue::Long(3))]);
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("counter:add", &desc, &value),
    )
    .await
    .expect("add2 timeout")
    .expect("add2 err");
    let v2 = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            other => panic!("shape: {other:?}"),
        },
        other => panic!("wrapper: {other:?}"),
    };
    assert_eq!(v2, 8); // 5 + 3
}

/// Direct-struct RPC request shape: arguments live at the top
/// level instead of inside `query.<name>`. pvxs `pvxcall` and
/// pvAccessJava both send NTURI by default, but custom services
/// (RPCBuilder.set("name", val).build() in pvxs) emit a flat
/// struct. The `Args::from_pv_field` fallback handles this.
fn direct_struct_request(args: &[(&str, ScalarValue)]) -> (FieldDesc, PvField) {
    let mut fields = Vec::new();
    let mut desc_fields = Vec::new();
    for (name, val) in args {
        let st = match val {
            ScalarValue::Long(_) => ScalarType::Long,
            ScalarValue::Int(_) => ScalarType::Int,
            ScalarValue::Double(_) => ScalarType::Double,
            ScalarValue::String(_) => ScalarType::String,
            _ => ScalarType::String,
        };
        fields.push((name.to_string(), PvField::Scalar(val.clone())));
        desc_fields.push((name.to_string(), FieldDesc::Scalar(st)));
    }
    let mut s = PvStructure::new("");
    s.fields = fields;
    let desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: desc_fields,
    };
    (desc, PvField::Structure(s))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_service_accepts_direct_struct_request() {
    let source = SharedSource::new();
    let _ = epics_pva_rs::service::add_rpc_service(&source, "calc", Counter::default());
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    // square(9.0) without NTURI wrapper.
    let (desc, value) = direct_struct_request(&[("x", ScalarValue::Double(9.0))]);
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("calc:square", &desc, &value),
    )
    .await
    .expect("rpc timeout")
    .expect("rpc err");
    let result = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => *v,
            other => panic!("unexpected square shape: {other:?}"),
        },
        other => panic!("unexpected wrapper: {other:?}"),
    };
    assert_eq!(result, 81.0);

    // add(7) with the direct-struct shape.
    let (desc, value) = direct_struct_request(&[("delta", ScalarValue::Long(7))]);
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("calc:add", &desc, &value),
    )
    .await
    .expect("add timeout")
    .expect("add err");
    let v = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            other => panic!("unexpected add shape: {other:?}"),
        },
        other => panic!("unexpected wrapper: {other:?}"),
    };
    assert_eq!(v, 7);
}
