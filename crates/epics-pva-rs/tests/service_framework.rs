//! Smoke test for `#[pva_service]` + `add_rpc_service`. Spins up
//! an in-process server with a service that exposes two RPC
//! methods, then drives them via `PvaClient::pvrpc`.
// The tests that drive a live server are `tokio_backend`-only, so on
// `exec_backend` the fixtures and imports they share go unreferenced while the
// rest of this file still runs. The default build lints it in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]
#![cfg(feature = "client")]

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the exec-backend
// suite.
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
#[cfg(tokio_backend)]
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{ChannelSource, OpError, SharedSource};
use epics_pva_rs::service::{Status, pva_service};

#[derive(Default)]
struct Counter {
    value: AtomicI64,
}

/// A project-local result alias. A proc-macro cannot resolve this back
/// to `Result`, so the `#[pva_service]` macro must route it to the
/// operation-error path via the type system (`IntoServiceOutcome`), not
/// by syntactically matching the literal token `Result`. Pre-fix, an
/// `RpcResult`-returning method failed to compile (the success branch
/// required `RpcResult<T>: IntoServiceResponse`).
type RpcResult<T> = Result<T, String>;

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

    /// Always fails — exercises the RPC operation-error path. The
    /// method's `Err` must surface to the client as an RPC error
    /// (wire `Status::error`), NOT a success NTRPCStatus payload.
    async fn boom(&self) -> Result<i64, String> {
        Err("intentional failure".into())
    }

    /// Returns an explicit app-level status payload. `Ok(Status::error)`
    /// is a successful RPC carrying a not-ok NTRPCStatus body — distinct
    /// from a method `Err`, which is an operation error.
    async fn app_status(&self) -> Result<Status, String> {
        Ok(Status::error("app-level not-ok"))
    }

    /// Returns through a `Result` type alias on the success path — pins
    /// that the macro compiles aliased-`Result` returns.
    async fn alias_add(&self, delta: i64) -> RpcResult<i64> {
        Ok(self.value.fetch_add(delta, Ordering::Relaxed) + delta)
    }

    /// Returns `Err` through a `Result` type alias — pins that an
    /// aliased `Result`'s `Err` is routed to the RPC operation-error
    /// path exactly like a literal `Result`.
    async fn alias_boom(&self) -> RpcResult<i64> {
        Err("aliased failure".into())
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
        PvField::Scalar(ScalarValue::String("".into())),
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

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_service_dispatch_round_trip() {
    let source = SharedSource::new();
    let registered = epics_pva_rs::service::add_rpc_service(&source, "counter", Counter::default())
        .expect("first registration must succeed");
    assert_eq!(registered.len(), 7);
    assert!(registered.contains(&"counter:add".to_string()));
    assert!(registered.contains(&"counter:reset".to_string()));
    assert!(registered.contains(&"counter:square".to_string()));
    assert!(registered.contains(&"counter:boom".to_string()));
    assert!(registered.contains(&"counter:app_status".to_string()));
    assert!(registered.contains(&"counter:alias_add".to_string()));
    assert!(registered.contains(&"counter:alias_boom".to_string()));

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
    .expect("rpc err")
    .into_value()
    .expect("value reply");
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
    .expect("add err")
    .into_value()
    .expect("value reply");
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
    .expect("add2 err")
    .into_value()
    .expect("value reply");
    let v2 = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            other => panic!("shape: {other:?}"),
        },
        other => panic!("wrapper: {other:?}"),
    };
    assert_eq!(v2, 8); // 5 + 3
}

#[cfg(tokio_backend)]
/// A service method that returns `Err(...)` must surface to the client
/// as an RPC **operation error** (wire `Status::error`), NOT a success
/// NTRPCStatus payload. Pre-fix the macro wrapped every return in
/// `Ok(...)` and the blanket `IntoServiceResponse for Result` turned
/// `Err` into a success NTRPCStatus{ok=false} body, so `pvrpc`
/// resolved to `Ok`. Now the macro routes `Err` to
/// `ServiceError::Method` → wire `Status::error`, so `pvrpc` resolves
/// to `Err` — pvxs `op->error`, client `RemoteError`
/// (`test/testrpc.cpp:193-209`). The companion case pins the other
/// boundary: an explicit `Ok(Status::error(...))` is still a
/// SUCCESSFUL RPC carrying a not-ok status body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_service_method_err_surfaces_as_rpc_error() {
    let source = SharedSource::new();
    epics_pva_rs::service::add_rpc_service(&source, "counter", Counter::default())
        .expect("registration must succeed");
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    // counter:boom always returns Err — the client RPC must error.
    let (desc, value) = nturi_request(&[]);
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("counter:boom", &desc, &value),
    )
    .await
    .expect("rpc timeout")
    .expect_err("a method Err must surface as an RPC operation error, not Ok");
    assert!(
        err.to_string().contains("intentional failure"),
        "the RPC error must carry the method's message, got: {err}"
    );

    // counter:app_status returns Ok(Status::error(..)) — an explicit
    // app-level status payload is still a SUCCESSFUL RPC carrying a
    // not-ok NTRPCStatus body (distinct from a method Err).
    let (desc, value) = nturi_request(&[]);
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("counter:app_status", &desc, &value),
    )
    .await
    .expect("rpc timeout")
    .expect("an explicit Ok(Status) is a successful RPC")
    .into_value()
    .expect("value reply");
    match resp {
        PvField::Structure(s) => {
            assert!(
                matches!(
                    s.get_field("ok"),
                    Some(PvField::Scalar(ScalarValue::Boolean(false)))
                ),
                "app_status body must carry ok=false"
            );
            assert!(
                matches!(
                    s.get_field("message"),
                    Some(PvField::Scalar(ScalarValue::String(m))) if m == "app-level not-ok"
                ),
                "app_status body must carry the message"
            );
        }
        other => panic!("unexpected app_status response: {other:?}"),
    }

    // counter:alias_add returns Ok through a `RpcResult` alias — a
    // successful RPC carrying the value.
    let (desc, value) = nturi_request(&[("delta", ScalarValue::Long(4))]);
    let (_, resp) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("counter:alias_add", &desc, &value),
    )
    .await
    .expect("rpc timeout")
    .expect("an aliased-Result Ok is a successful RPC")
    .into_value()
    .expect("value reply");
    match resp {
        PvField::Structure(s) => assert!(matches!(
            s.get_field("value"),
            Some(PvField::Scalar(ScalarValue::Long(4)))
        )),
        other => panic!("unexpected alias_add response: {other:?}"),
    }

    // counter:alias_boom returns Err through the same alias — it must
    // surface as an RPC operation error, identical to a literal Result.
    let (desc, value) = nturi_request(&[]);
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvrpc("counter:alias_boom", &desc, &value),
    )
    .await
    .expect("rpc timeout")
    .expect_err("an aliased-Result Err must surface as an RPC operation error");
    assert!(
        err.to_string().contains("aliased failure"),
        "the RPC error must carry the method's message, got: {err}"
    );
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

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_service_accepts_direct_struct_request() {
    let source = SharedSource::new();
    epics_pva_rs::service::add_rpc_service(&source, "calc", Counter::default())
        .expect("registration must succeed");
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
    .expect("rpc err")
    .into_value()
    .expect("value reply");
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
    .expect("add err")
    .into_value()
    .expect("value reply");
    let v = match resp {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            other => panic!("unexpected add shape: {other:?}"),
        },
        other => panic!("unexpected wrapper: {other:?}"),
    };
    assert_eq!(v, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_rpc_service_rejects_colliding_registration() {
    let source = SharedSource::new();
    let first = epics_pva_rs::service::add_rpc_service(&source, "counter", Counter::default())
        .expect("first registration must succeed");
    assert_eq!(first.len(), 7);

    // A second service under the same prefix collides on the very
    // first method (`counter:add`). pvxs `StaticSource::add()` rejects
    // duplicates instead of replacing, so this must error and leave the
    // already-served namespace untouched — never a silent half-swap.
    let err = epics_pva_rs::service::add_rpc_service(&source, "counter", Counter::default())
        .expect_err("colliding registration must be rejected");
    assert!(err.0.contains("counter:add"), "unexpected error: {err}");

    // All original PVs are still present and the count is unchanged:
    // the rejected call rolled itself back, adding nothing.
    for name in [
        "counter:add",
        "counter:reset",
        "counter:square",
        "counter:boom",
        "counter:app_status",
        "counter:alias_add",
        "counter:alias_boom",
    ] {
        assert!(source.has_pv(name).await, "{name} must remain registered");
    }
    assert_eq!(
        source.list_pvs().await.len(),
        7,
        "no extra PVs from the rejected call"
    );
}

/// Client end-to-end: a source that opens (descriptor
/// present at INIT) but fails the value read at the data phase makes
/// the server emit a data-phase error reply. Before the fix the
/// server hardcoded the INIT subcmd `0x08` on that error, so the
/// client decoded it as an INIT response and `pvget` failed with
/// "expected GET data, got Init …" — losing the server status. The
/// fix (server echoes the request subcmd `0x00`; client decodes the
/// status-only body and `op_get` maps it to an error) makes `pvget`
/// surface the server's data-phase status instead.
struct FailAtDataSource;
impl ChannelSource for FailAtDataSource {
    async fn list_pvs(&self) -> Vec<String> {
        vec!["dut".into()]
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "dut" }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        })
    }
    // The data-phase failure: INIT negotiated a descriptor, but the
    // value read returns None at exec time.
    async fn get_value(&self, _: &str) -> Option<PvField> {
        None
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Ok(())
    }
    async fn is_writable(&self, _: &str) -> bool {
        false
    }
    async fn subscribe(
        &self,
        _: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bfr13_client_get_surfaces_data_phase_error() {
    let server =
        PvaServer::isolated(Arc::new(FailAtDataSource)).expect("isolated test server must start");
    let client = server.client_config();

    let res = tokio::time::timeout(Duration::from_secs(5), client.pvget("dut"))
        .await
        .expect("pvget must not hang");
    let err = res.expect_err("a data-phase source failure must surface as a GET error");
    let msg = err.to_string();
    // The server's data-phase Status reaches the caller as itself (R18-27) —
    // `RemoteError(Status)`, message intact — not as a rendering of it and not
    // as a mis-framed INIT response.
    match &err {
        epics_pva_rs::error::PvaError::RemoteError(status) => assert_eq!(
            status.message(),
            Some("PV not found"),
            "client must surface the server's data-phase status message, got: {status:?}"
        ),
        other => panic!("data-phase failure must surface as RemoteError, got: {other:?}"),
    }
    assert!(
        !msg.contains("expected GET data, got Init"),
        "client must NOT mis-report the data-phase error as an unexpected INIT response, got: {msg}"
    );
}

/// a source handler that PANICS (not returns Err) must still
/// produce a client reply. Before the fix the panic unwound the spawned exec
/// task and skipped the reply-build; the op returned to Idle but the client
/// received nothing and waited out its full operation timeout. The fix wraps
/// the user handler so a panic is converted into the same error reply a
/// returned Err already produces. These tests assert the client gets an Err
/// quickly (the `tokio::time::timeout` wrapper would fail on the pre-fix hang).
///
/// The panic is deliberate and is caught inside the server's exec task; the
/// test process does not abort. A panic backtrace on stderr during this test
/// is expected.
struct PanicSource;
impl ChannelSource for PanicSource {
    async fn list_pvs(&self) -> Vec<String> {
        vec!["boom".into()]
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "boom" }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        })
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        panic!("get_value handler panicked on purpose");
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        panic!("put_value handler panicked on purpose");
    }
    async fn is_writable(&self, _: &str) -> bool {
        true
    }
    async fn subscribe(
        &self,
        _: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sr21_panicking_get_handler_replies_error_not_hang() {
    let server =
        PvaServer::isolated(Arc::new(PanicSource)).expect("isolated test server must start");
    let client = server.client_config();

    let res = tokio::time::timeout(Duration::from_secs(5), client.pvget("boom"))
        .await
        .expect("pvget must not hang when the GET handler panics");
    let err = res.expect_err("a panicking GET handler must surface as a GET error");
    let msg = err.to_string();
    assert!(
        msg.contains("panicked"),
        "client must surface the converted panic as the server's data-phase status, got: {msg}"
    );
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sr21_panicking_put_handler_replies_error_not_hang() {
    let server =
        PvaServer::isolated(Arc::new(PanicSource)).expect("isolated test server must start");
    let client = server.client_config();

    let res = tokio::time::timeout(Duration::from_secs(5), client.pvput("boom", "1.0"))
        .await
        .expect("pvput must not hang when the PUT handler panics");
    let err = res.expect_err("a panicking PUT handler must surface as a PUT error");
    let msg = err.to_string();
    assert!(
        msg.contains("panicked"),
        "client must surface the converted panic as the server's PUT status, got: {msg}"
    );
}
