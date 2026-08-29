//! Regression tests: channel filters must run on every value
//! delivery path, not only the record-field `EVENT_ADD` callback.
//!
//! epics-base attaches the parsed filter chain to the database channel
//! (`dbChannelRunPreChain`), so a `REC.{"arr":...}` suffix transforms
//! the value on `READ` / `READ_NOTIFY`, on `SimplePv` monitors, and on
//! record-field monitors alike. Pre-fix epics-ca-rs only consumed the
//! suffix on the record-field `EVENT_ADD` path; `READ_NOTIFY` and
//! `SimplePv` monitors returned the unfiltered value.
//!
//! These tests drive a real `CaClient` ↔ `CaServer` TCP round-trip and
//! assert the filtered slice arrives on each path. The `arr` filter is
//! used as the discriminator: with `{"arr":{"s":5,"e":7}}` over a
//! `[0,1,…,9]` array the first delivered element is the slice start
//! (`5.0`), whereas the unfiltered value's first element is `0.0`. The
//! first element is checked rather than the exact length so the
//! assertion is robust to any requested-count padding on the wire.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::time::Duration;

use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::DbFieldType;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaClient, MonitorHandle};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Point a soon-to-be-constructed `CaClient` at exactly this server so
/// it skips UDP search.
///
/// SAFETY: every test in this file is `#[serial]`, so no other test
/// mutates the environment concurrently, and the env is set before
/// `CaClient::new()` snapshots its resolver configuration.
fn point_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// `[start, start+1, …, start+len-1]` as `f64`s — a ramp whose every
/// element equals its own index offset, so a slice's first element
/// uniquely identifies the slice start.
fn ramp(start: f64, len: usize) -> Vec<f64> {
    (0..len).map(|i| start + i as f64).collect()
}

/// Extract the first element of a `DoubleArray`, panicking with the
/// actual variant on any other shape.
fn first_double(v: &EpicsValue) -> f64 {
    match v {
        EpicsValue::DoubleArray(a) => *a.first().expect("non-empty DoubleArray"),
        other => panic!("expected DoubleArray, got {other:?}"),
    }
}

fn len_double(v: &EpicsValue) -> usize {
    match v {
        EpicsValue::DoubleArray(a) => a.len(),
        other => panic!("expected DoubleArray, got {other:?}"),
    }
}

/// Borrow the `DoubleArray` slice, panicking on any other variant.
fn doubles(v: &EpicsValue) -> &[f64] {
    match v {
        EpicsValue::DoubleArray(a) => a.as_slice(),
        other => panic!("expected DoubleArray, got {other:?}"),
    }
}

/// Receive the next monitor value within 3s, unwrapping the
/// `Option<CaResult<Snapshot>>` to the `EpicsValue`.
async fn recv_value(mon: &mut MonitorHandle) -> EpicsValue {
    tokio::time::timeout(budget::FACT_BUDGET, mon.recv())
        .await
        .expect("monitor recv timed out")
        .expect("monitor stream closed before a value arrived")
        .expect("monitor yielded an error")
        .value
}

/// A filtered record-field `READ_NOTIFY` applies the
/// `arr` transform before DBR encoding. Pre-fix the `READ` /
/// `READ_NOTIFY` path called `get_full_snapshot()` and encoded it
/// directly, never consulting `entry.filter_suffix`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ca_fr_8_record_field_read_notify_applies_arr() {
    let server = CaServer::builder()
        .port(0)
        .record("CAFR8:WF:R1", WaveformRecord::new(10, DbFieldType::Double))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    // Seed VAL = [0,1,…,9] so element[0]=0.0 and element[5]=5.0 are
    // distinct — the arr-slice discriminator.
    // The server is spawned by value above; drive seeding through a
    // CA WRITE so we don't need a second handle to the server.
    let ch = client.create_channel("CAFR8:WF:R1");
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect for seed write");
    ch.put(&EpicsValue::DoubleArray(ramp(0.0, 10)))
        .await
        .expect("seed VAL with [0..9]");

    // Baseline: unfiltered read returns the full ramp, first element 0.0.
    let (_t, base) = tokio::time::timeout(budget::FACT_BUDGET, client.caget("CAFR8:WF:R1"))
        .await
        .expect("baseline caget did not complete")
        .expect("baseline caget should succeed");
    assert_eq!(
        first_double(&base),
        0.0,
        "unfiltered read must start at element 0 of the seeded ramp"
    );
    assert_eq!(
        len_double(&base),
        10,
        "unfiltered read must return the full 10-element waveform"
    );

    // Filtered read: arr {s:5,e:7} slices [5,6,7]; first element 5.0.
    let (_t, filtered) = tokio::time::timeout(
        budget::FACT_BUDGET,
        client.caget(r#"CAFR8:WF:R1.{"arr":{"s":5,"e":7}}"#),
    )
    .await
    .expect("filtered caget did not complete")
    .expect("filtered caget should succeed");
    // The arr slice is exactly `[5,6,7]`. The CREATE_CHAN reply now
    // advertises the filter-final element count (3, via
    // `dbChannelFinalElements`), so the client requests 3 and the server
    // returns the slice with no trailing zero-pad.
    assert_eq!(
        doubles(&filtered),
        &[5.0, 6.0, 7.0],
        "READ_NOTIFY must return exactly the arr slice [5,6,7], no zero-pad"
    );
}

/// A filtered record-field monitor still applies the
/// same chain on updates, not only on the first `EVENT_ADD` frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ca_fr_8_record_field_monitor_applies_arr_on_updates() {
    let server = CaServer::builder()
        .port(0)
        .record("CAFR8:WF:R2", WaveformRecord::new(10, DbFieldType::Double))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    // Seed [0..9] before subscribing so the initial frame is known.
    let seed = client.create_channel("CAFR8:WF:R2");
    seed.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect for seed");
    seed.put(&EpicsValue::DoubleArray(ramp(0.0, 10)))
        .await
        .expect("seed VAL");

    let channel = client.create_channel(r#"CAFR8:WF:R2.{"arr":{"s":5,"e":7}}"#);
    let mut monitor = channel.subscribe().await.expect("subscribe");

    // Initial EVENT_ADD frame: exactly the arr slice of the seed.
    let initial = recv_value(&mut monitor).await;
    assert_eq!(
        doubles(&initial),
        &[5.0, 6.0, 7.0],
        "initial monitor frame must be the arr slice [5,6,7]"
    );

    // Update VAL to [100..109]; the monitor must re-apply the chain on
    // the update → exactly [105,106,107] (unfiltered would start at 100).
    seed.put(&EpicsValue::DoubleArray(ramp(100.0, 10)))
        .await
        .expect("update VAL with [100..109]");
    let update = recv_value(&mut monitor).await;
    assert_eq!(
        doubles(&update),
        &[105.0, 106.0, 107.0],
        "record-field monitor must apply the arr chain on UPDATES, not only the first frame"
    );
}

/// A filtered `SimplePv` monitor applies `arr` instead
/// of the empty chain it received pre-fix. The `SimplePv` subscription
/// path created the subscriber via `ProcessVariable::add_subscriber()`
/// without passing the channel's filter suffix, so `pv.rs` gave every
/// `SimplePv` subscriber an empty `FilterChain`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ca_fr_8_simplepv_monitor_applies_arr() {
    let server = CaServer::builder()
        .port(0)
        .pv("CAFR8:SP:R3", EpicsValue::DoubleArray(ramp(0.0, 10)))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    let channel = client.create_channel(r#"CAFR8:SP:R3.{"arr":{"s":5,"e":7}}"#);
    let mut monitor = channel.subscribe().await.expect("subscribe");

    // Initial frame: the SimplePv subscriber must use the parsed chain,
    // not an empty one → exactly [5,6,7] (empty chain would give the
    // full [0..9]).
    let initial = recv_value(&mut monitor).await;
    assert_eq!(
        doubles(&initial),
        &[5.0, 6.0, 7.0],
        "SimplePv monitor initial frame must be the arr slice, not the full unfiltered array"
    );

    // Update via a CA write on an unfiltered channel; the SimplePv
    // event-delivery path must apply the same chain → first element
    // 105.0.
    let writer = client.create_channel("CAFR8:SP:R3");
    writer
        .wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect writer channel");
    writer
        .put(&EpicsValue::DoubleArray(ramp(100.0, 10)))
        .await
        .expect("update SimplePv value");
    let update = recv_value(&mut monitor).await;
    assert_eq!(
        doubles(&update),
        &[105.0, 106.0, 107.0],
        "SimplePv monitor must apply the arr chain on event-delivery updates"
    );
}

/// Finding #4: an `arr` filter on a SCALAR channel must be a no-op for
/// BOTH the value and the advertised element count — C `arr.c:148`
/// (`channelRegisterPost`: `if (no_elements <= 1) return; /* array data
/// only */`). Pre-fix the CREATE_CHAN reply advertised
/// `final_element_count` over the scalar's native count of 1, which a
/// slicing config like `{"s":5}` collapses to 0, while READ delivered the
/// untouched scalar — so the wire-advertised count and the value count
/// disagreed. The fix makes the count/slice path no-op at length <= 1
/// (matching `apply`'s scalar pass-through), so the channel resolves and
/// the read returns the scalar unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ca_fr_8_arr_on_scalar_channel_is_noop() {
    let server = CaServer::builder()
        .port(0)
        .pv("CAFR8:SP:SCALAR", EpicsValue::Double(42.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    // `s:5` past a scalar resolves to an empty slice under
    // `wrapArrayIndices`, so a (wrong) reshape would advertise 0 elements
    // and the read could not return the scalar. The filter must instead
    // be a no-op: the scalar passes through unchanged.
    let (_t, filtered) = tokio::time::timeout(
        budget::FACT_BUDGET,
        client.caget(r#"CAFR8:SP:SCALAR.{"arr":{"s":5,"e":7}}"#),
    )
    .await
    .expect("scalar+arr caget did not complete")
    .expect("scalar+arr caget should succeed — the channel must resolve, not advertise 0 elements");
    match filtered {
        EpicsValue::Double(v) => assert_eq!(
            v, 42.0,
            "arr on a scalar PV must pass the scalar through unchanged, not slice it away"
        ),
        other => panic!("expected the scalar Double unchanged, got {other:?}"),
    }
}

/// A malformed / unknown / bad-config filter suffix must REJECT the
/// channel at `CA_PROTO_CREATE_CHAN` (CREATE_CH_FAIL), matching EPICS
/// `dbChannelCreate()` → `chf_parse()` failure deleting the channel and
/// returning NULL. The earlier CA path failed OPEN: a bad suffix
/// downgraded to an unfiltered channel, so `caget("...{bad}")` read the
/// raw waveform. Each malformed case must now fail to connect rather
/// than return the unfiltered value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ca_fr_8_malformed_suffix_rejects_channel_create() {
    let server = CaServer::builder()
        .port(0)
        .record("CAFR8:WF:R4", WaveformRecord::new(10, DbFieldType::Double))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    let seed = client.create_channel("CAFR8:WF:R4");
    seed.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect for seed");
    seed.put(&EpicsValue::DoubleArray(ramp(0.0, 10)))
        .await
        .expect("seed VAL");

    // Sanity: the bare record (no suffix) still connects and reads.
    let (_t, ok) = tokio::time::timeout(budget::FACT_BUDGET, client.caget("CAFR8:WF:R4"))
        .await
        .expect("bare caget did not complete")
        .expect("bare record must still connect");
    assert_eq!(len_double(&ok), 10, "bare record reads the full waveform");

    // Each of these is rejected by `try_parse_filter_chain` at
    // channel creation: invalid JSON, an unknown filter name, and a
    // known filter whose config is rejected by its own parser. The
    // server replies CREATE_CH_FAIL and the channel never connects, so
    // `wait_connected` must error rather than the channel opening and
    // failing open to the raw stream.
    for bad in [
        r#"CAFR8:WF:R4.{bad}"#,          // not valid JSON
        r#"CAFR8:WF:R4.{"no_such":{}}"#, // unknown filter name
        r#"CAFR8:WF:R4.{"dec":{}}"#,     // dec requires `n` — config rejected
    ] {
        let ch = client.create_channel(bad);
        assert!(
            ch.wait_connected(Duration::from_secs(1)).await.is_err(),
            "{bad} must be rejected at CREATE_CHAN (CREATE_CH_FAIL) — the \
             channel must not connect (fail-open)"
        );
    }
}

/// A filter suffix whose JSON contains a `.` (e.g.
/// `dbnd` with `{"d":0.5}`) must still resolve the channel at UDP
/// search and CREATE_CHAN. This is the boundary that motivated the
/// structural fix in `Database::{has_name,find_entry}_no_resolve`:
/// the pre-fix code ran `parse_pv_name` (a last-dot split) on the raw
/// channel name, so the `.` inside `0.5` tore the suffix apart
/// (`base = "REC.{\"dbnd\":{\"d\":0"`) and the lookup missed — the
/// channel never connected. Stripping the channel-filter suffix first
/// (`split_channel_name`) removes the JSON before any dot split.
///
/// `dbnd` is a stream-only filter; on a one-shot read it passes the
/// value through unchanged (no prior value to deadband against), so
/// the assertion is simply that the channel connects and the read
/// returns the seeded value — proving search-time resolution, not a
/// value transform.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ca_fr_8_dotted_filter_suffix_resolves_at_search() {
    let server = CaServer::builder()
        .port(0)
        .record("CAFR8:DOT:R5", WaveformRecord::new(10, DbFieldType::Double))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");

    let seed = client.create_channel("CAFR8:DOT:R5");
    seed.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect for seed");
    seed.put(&EpicsValue::DoubleArray(ramp(0.0, 10)))
        .await
        .expect("seed VAL");

    // The `0.5` inside the suffix is the trap for a naive last-dot
    // split. Post-fix the channel resolves and the read succeeds.
    let (_t, val) = tokio::time::timeout(
        budget::FACT_BUDGET,
        client.caget(r#"CAFR8:DOT:R5.{"dbnd":{"d":0.5}}"#),
    )
    .await
    .expect("caget with a dot-containing filter suffix did not complete (search resolution failed)")
    .expect("caget with a dot-containing filter suffix should resolve and succeed");
    assert_eq!(
        first_double(&val),
        0.0,
        "dbnd passes the read value through unchanged; first element is the seeded ramp start"
    );
    assert_eq!(
        len_double(&val),
        10,
        "dbnd read context leaves the full waveform intact"
    );
}

#[path = "common/budget.rs"]
mod budget;
