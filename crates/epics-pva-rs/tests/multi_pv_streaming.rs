//! Reactor-dependent in full: its mock source delays `slow` with
//! `tokio::time::sleep` inside `get_introspection`/`get_value`, and under
//! `exec_backend` the `runtime::task` seam drives that future on a `cbMedium`
//! executor worker with no tokio reactor, so the fixture panics with "there is
//! no reactor running". Gated at file scope because every test here shares
//! that source.
#![cfg(tokio_backend)]
#![cfg(feature = "client")]

//! pvxs `pvxget` / `pvxinfo` install a per-operation `.result()` callback
//! that fires the instant *that* op completes (tools/get.cpp:107-119,
//! tools/info.cpp:86-94), so a fast PV is visible before a slow or missing
//! sibling's timeout expires. The Rust multi-PV helpers used to drain the
//! whole `JoinSet` and only then return an ordered `Vec`, so every
//! completed PV was buffered behind the slowest sibling.
//!
//! These tests exercise the streaming helpers end-to-end against an
//! isolated server whose `"slow"` PV delays its describe by ~1 s while
//! `"fast"` resolves immediately. They assert the fast PV's callback fires
//! in completion order — long before the slow sibling's — not after the
//! batch joins.
//!
//! FAIL-proof: reverting `pvget_many_full_streaming` /
//! `pvinfo_many_full_streaming` to collect-then-callback makes both
//! callbacks fire only after the slow sibling finishes (~1 s), so the
//! `fast_elapsed < 500 ms` and gap assertions fail.

use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer};

/// Delay applied to the `"slow"` PV's introspection. The describe is what
/// both CREATE_CHANNEL resolution and `pvinfo`'s GET_FIELD wait on, so this
/// makes the slow sibling genuinely slow on both the GET and INFO paths.
const SLOW: Duration = Duration::from_millis(1000);

/// A source with one instant PV (`"fast"`) and one that stalls its
/// introspection by [`SLOW`] (`"slow"`). The server spawns CREATE_CHANNEL
/// resolvers, so the slow describe does not head-of-line block the fast
/// sibling's channel from opening.
struct TwoSpeedSource;

impl ChannelSource for TwoSpeedSource {
    async fn list_pvs(&self) -> Vec<String> {
        vec!["fast".into(), "slow".into()]
    }
    async fn has_pv(&self, name: &str) -> bool {
        name == "fast" || name == "slow"
    }
    async fn get_introspection(&self, name: &str) -> Option<FieldDesc> {
        if name == "slow" {
            tokio::time::sleep(SLOW).await;
        }
        Some(FieldDesc::Scalar(ScalarType::Double))
    }
    async fn get_value(&self, name: &str) -> Option<PvField> {
        let v = if name == "slow" { 2.0 } else { 1.0 };
        Some(PvField::Scalar(ScalarValue::Double(v)))
    }
    async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
        Ok(())
    }
    async fn is_writable(&self, _name: &str) -> bool {
        false
    }
    async fn subscribe(
        &self,
        _name: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }
}

/// `pvget_many_full_streaming` must deliver the fast PV's result in
/// completion order — well before the slow sibling — not after the whole
/// batch joins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pvget_streaming_reports_fast_before_slow_sibling() {
    let server =
        PvaServer::isolated(Arc::new(TwoSpeedSource)).expect("isolated test server must start");
    let client = server.client_config();

    // idx 0 = slow, idx 1 = fast. Completion order must put the fast PV
    // first regardless of input order.
    let names = ["slow", "fast"];
    let mut events: Vec<(usize, Duration, bool)> = Vec::new();
    let t0 = Instant::now();
    client
        .pvget_many_full_streaming(&names, None, |idx, result| {
            events.push((idx, t0.elapsed(), result.is_ok()));
        })
        .await;

    assert_eq!(events.len(), 2, "both PVs must be reported: {events:?}");
    let (first_idx, first_elapsed, first_ok) = events[0];
    let (second_idx, second_elapsed, second_ok) = events[1];

    assert_eq!(
        first_idx, 1,
        "fast PV (idx 1) must be reported first, in completion order, got: {events:?}"
    );
    assert!(first_ok, "fast PV GET must succeed, got: {events:?}");
    assert_eq!(second_idx, 0, "slow PV (idx 0) must be reported second");
    assert!(second_ok, "slow PV GET must succeed, got: {events:?}");
    assert!(
        first_elapsed < Duration::from_millis(500),
        "fast PV must be reported at completion time (<500ms), not buffered \
         behind the slow sibling; fast fired at {first_elapsed:?}"
    );
    assert!(
        second_elapsed.saturating_sub(first_elapsed) >= Duration::from_millis(400),
        "fast PV must be reported well before the slow sibling; \
         fast at {first_elapsed:?}, slow at {second_elapsed:?}"
    );
}

/// `pvinfo_many_full_streaming` has the same completion-time contract: the
/// fast PV's describe is reported before the slow sibling's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pvinfo_streaming_reports_fast_before_slow_sibling() {
    let server =
        PvaServer::isolated(Arc::new(TwoSpeedSource)).expect("isolated test server must start");
    let client = server.client_config();

    let names = ["slow", "fast"];
    let mut events: Vec<(usize, Duration, bool)> = Vec::new();
    let t0 = Instant::now();
    client
        .pvinfo_many_full_streaming(&names, |idx, result| {
            events.push((idx, t0.elapsed(), result.is_ok()));
        })
        .await;

    assert_eq!(events.len(), 2, "both PVs must be reported: {events:?}");
    let (first_idx, first_elapsed, first_ok) = events[0];
    let (second_idx, second_elapsed, second_ok) = events[1];

    assert_eq!(
        first_idx, 1,
        "fast PV (idx 1) must be reported first, in completion order, got: {events:?}"
    );
    assert!(first_ok, "fast PV describe must succeed, got: {events:?}");
    assert_eq!(second_idx, 0, "slow PV (idx 0) must be reported second");
    assert!(second_ok, "slow PV describe must succeed, got: {events:?}");
    assert!(
        first_elapsed < Duration::from_millis(500),
        "fast PV must be reported at completion time (<500ms), not buffered \
         behind the slow sibling; fast fired at {first_elapsed:?}"
    );
    assert!(
        second_elapsed.saturating_sub(first_elapsed) >= Duration::from_millis(400),
        "fast PV must be reported well before the slow sibling; \
         fast at {first_elapsed:?}, slow at {second_elapsed:?}"
    );
}
