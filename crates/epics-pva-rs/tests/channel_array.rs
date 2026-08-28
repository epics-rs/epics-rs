//! End-to-end ChannelArray (PVA cmd 14) interop: client `pvarray_*`
//! ↔ server `handle_channel_array` ↔ `ChannelSource::channel_array_*`.
//!
//! Two halves:
//! 1. A source with NO array support (trait defaults) must answer every
//!    sub-op with a protocol `Status` error — NEVER the pre-fix silent
//!    drop where CMD_ARRAY fell through the dispatch default and the
//!    client hung. Each `pvarray_*` is wrapped in a timeout so a
//!    regression to the hang fails loudly.
//! 2. A source that serves a real windowed double array exercises the
//!    full INIT / getArray (full + sliced + strided) / putArray /
//!    setLength / getLength round trips.

#![cfg(tokio_backend)]
#![cfg(feature = "client")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::source::AccessChecked;
use epics_pva_rs::server_native::{ChannelContext, ChannelSource, OpError};

const TIMEOUT: Duration = Duration::from_secs(5);

fn doubles(values: &[f64]) -> PvField {
    PvField::ScalarArray(values.iter().copied().map(ScalarValue::Double).collect())
}

fn extract_doubles(field: &PvField) -> Vec<f64> {
    // The wire-decode path may yield either the generic `ScalarArray`
    // (`Vec<ScalarValue>`) or the packed `ScalarArrayTyped` form; normalise
    // both to `ScalarValue` before extracting.
    let items = match field {
        PvField::ScalarArray(items) => items.clone(),
        PvField::ScalarArrayTyped(arr) => arr.to_scalar_values(),
        other => panic!("expected ScalarArray, got {other:?}"),
    };
    items
        .iter()
        .map(|v| match v {
            ScalarValue::Double(d) => *d,
            other => panic!("expected Double element, got {other:?}"),
        })
        .collect()
}

// ── 1. No array support: every sub-op must error, not hang ──────────────

/// Minimal source that resolves a channel but leaves every
/// `channel_array_*` at the trait default ("not supported").
struct NoArraySource;
impl ChannelSource for NoArraySource {
    async fn list_pvs(&self) -> Vec<String> {
        vec!["dut".into()]
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "dut" }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Double))],
        })
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_array_init_errors_not_hangs() {
    let server =
        PvaServer::isolated(Arc::new(NoArraySource)).expect("isolated test server must start");
    let client = server.client_config();

    // getLength
    let r = tokio::time::timeout(TIMEOUT, client.pvarray_get_length("dut"))
        .await
        .expect("pvarray_get_length must not hang on an unsupported source");
    let e = r.expect_err("unsupported ChannelArray must surface as an error");
    assert!(
        e.to_string().contains("not supported"),
        "client must surface the server's not-supported status, got: {e}"
    );

    // getArray
    let r = tokio::time::timeout(TIMEOUT, client.pvarray_get("dut", 0, 0, 1))
        .await
        .expect("pvarray_get must not hang on an unsupported source");
    assert!(r.is_err(), "getArray on an unsupported source must error");

    // putArray
    let r = tokio::time::timeout(TIMEOUT, client.pvarray_put("dut", &doubles(&[1.0]), 0, 1))
        .await
        .expect("pvarray_put must not hang on an unsupported source");
    assert!(r.is_err(), "putArray on an unsupported source must error");

    // setLength
    let r = tokio::time::timeout(TIMEOUT, client.pvarray_set_length("dut", 4))
        .await
        .expect("pvarray_set_length must not hang on an unsupported source");
    assert!(r.is_err(), "setLength on an unsupported source must error");
}

// ── 2. Real windowed double array ───────────────────────────────────────

/// A source backing one PV ("arr") with an in-memory `Vec<f64>` that
/// serves the full ChannelArray surface.
struct ArraySource {
    data: Arc<Mutex<Vec<f64>>>,
}

impl ChannelSource for ArraySource {
    async fn list_pvs(&self) -> Vec<String> {
        vec!["arr".into()]
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "arr" }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Double))],
        })
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        Some(doubles(&self.data.lock().unwrap()))
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Ok(())
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

    // ChannelArray: the bound field is the whole double array.
    async fn channel_array_init(&self, _: &str, _: ChannelContext) -> Result<FieldDesc, OpError> {
        Ok(FieldDesc::ScalarArray(ScalarType::Double))
    }
    async fn channel_array_get(
        &self,
        checked: AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        _: ChannelContext,
    ) -> Result<PvField, OpError> {
        if !checked.allows_read() {
            return Err(OpError::denied("read denied"));
        }
        let data = self.data.lock().unwrap();
        let stride = stride.max(1) as usize;
        let want = count as usize; // 0 => to the end
        let mut out = Vec::new();
        let mut i = offset as usize;
        while i < data.len() && (want == 0 || out.len() < want) {
            out.push(ScalarValue::Double(data[i]));
            i += stride;
        }
        Ok(PvField::ScalarArray(out))
    }
    async fn channel_array_put(
        &self,
        checked: AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        _: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            return Err(OpError::denied("write denied"));
        }
        let new = extract_doubles(&value);
        let stride = stride.max(1) as usize;
        let mut data = self.data.lock().unwrap();
        let mut idx = offset as usize;
        for d in new {
            if idx >= data.len() {
                data.resize(idx + 1, 0.0);
            }
            data[idx] = d;
            idx += stride;
        }
        Ok(())
    }
    async fn channel_array_set_length(
        &self,
        checked: AccessChecked,
        length: u32,
        _: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            return Err(OpError::denied("write denied"));
        }
        self.data.lock().unwrap().resize(length as usize, 0.0);
        Ok(())
    }
    async fn channel_array_get_length(
        &self,
        checked: AccessChecked,
        _: ChannelContext,
    ) -> Result<u32, OpError> {
        if !checked.allows_read() {
            return Err(OpError::denied("read denied"));
        }
        Ok(self.data.lock().unwrap().len() as u32)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn array_round_trip_get_put_length() {
    let data = Arc::new(Mutex::new(vec![10.0, 20.0, 30.0, 40.0, 50.0]));
    let server = PvaServer::isolated(Arc::new(ArraySource { data: data.clone() }))
        .expect("isolated test server must start");
    let client = server.client_config();

    // getLength
    let len = tokio::time::timeout(TIMEOUT, client.pvarray_get_length("arr"))
        .await
        .expect("getLength must not hang")
        .expect("getLength must succeed");
    assert_eq!(len, 5, "initial length");

    // getArray full (count == 0 → to end)
    let (_d, full) = tokio::time::timeout(TIMEOUT, client.pvarray_get("arr", 0, 0, 1))
        .await
        .expect("getArray must not hang")
        .expect("getArray must succeed");
    assert_eq!(extract_doubles(&full), vec![10.0, 20.0, 30.0, 40.0, 50.0]);

    // getArray slice: offset 1, count 2, stride 1 → [20, 30]
    let (_d, slice) = client
        .pvarray_get("arr", 1, 2, 1)
        .await
        .expect("sliced getArray must succeed");
    assert_eq!(extract_doubles(&slice), vec![20.0, 30.0]);

    // getArray strided: offset 0, count 0, stride 2 → [10, 30, 50]
    let (_d, strided) = client
        .pvarray_get("arr", 0, 0, 2)
        .await
        .expect("strided getArray must succeed");
    assert_eq!(extract_doubles(&strided), vec![10.0, 30.0, 50.0]);

    // putArray: write [99, 98] at offset 1, stride 1 → [10, 99, 98, 40, 50]
    tokio::time::timeout(
        TIMEOUT,
        client.pvarray_put("arr", &doubles(&[99.0, 98.0]), 1, 1),
    )
    .await
    .expect("putArray must not hang")
    .expect("putArray must succeed");
    assert_eq!(
        *data.lock().unwrap(),
        vec![10.0, 99.0, 98.0, 40.0, 50.0],
        "putArray must splice at offset"
    );

    // setLength: shrink to 3 → [10, 99, 98]
    client
        .pvarray_set_length("arr", 3)
        .await
        .expect("setLength must succeed");
    assert_eq!(*data.lock().unwrap(), vec![10.0, 99.0, 98.0]);

    let len = client
        .pvarray_get_length("arr")
        .await
        .expect("getLength after resize must succeed");
    assert_eq!(len, 3, "length after setLength");
}
