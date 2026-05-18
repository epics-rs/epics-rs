//! Single-record QSRV end-to-end parity tests, mirroring pvxs
//! `test/testqsingle.cpp::testGetScalar` / `testPut` /
//! `testGetPut64` / `testGetArray`.
//!
//! These exercise [`BridgeChannel`] directly against an in-memory
//! [`PvDatabase`] — no PVA wire involved. The wire path is covered
//! by `parity_interop` in epics-pva-rs; this suite locks down the
//! bridge layer's get/put → record translation independently.
//!
//! pvxs equivalent: tests run against a live IOC; we run against
//! `PvDatabase::add_record` since epics-base-rs gives us a
//! Rust-native record system without a separate `iocInit`.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

use epics_bridge_rs::qsrv::channel::BridgeChannel;
use epics_bridge_rs::qsrv::{Channel, NtType};
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

fn empty_request() -> PvStructure {
    PvStructure::new("epics:nt/NTRequest:1.0")
}

fn extract_value(s: &PvStructure) -> Option<&PvField> {
    s.fields
        .iter()
        .find(|(name, _)| name == "value")
        .map(|(_, v)| v)
}

/// pvxs `testGetScalar` parity: GET on an `ai` record returns an
/// NTScalar with the record's current `VAL`.
#[tokio::test]
async fn get_ai_scalar_returns_current_value() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:ai", Box::new(AiRecord::new(2.5)))
        .await
        .unwrap();
    let ch = BridgeChannel::from_cached(
        db,
        "TEST:ai".into(),
        "TEST:ai".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Double,
    );

    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    assert!(matches!(value, PvField::Scalar(ScalarValue::Double(v)) if (*v - 2.5).abs() < 1e-9));
}

/// pvxs `testPut` parity: PUT a new value, then GET sees it.
#[tokio::test]
async fn put_then_get_round_trips_double() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:ai_rt", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:ai_rt".into(),
        "TEST:ai_rt".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Double,
    );

    // PUT 7.5
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.5))));
    ch.put(&put).await.expect("put");

    // GET sees 7.5
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    assert!(matches!(value, PvField::Scalar(ScalarValue::Double(v)) if (*v - 7.5).abs() < 1e-9));
}

/// pvxs `testGetPut64` parity: 64-bit integer round-trip through a
/// long record (the record-side coercion drops to i32 internally,
/// but the Rust path encodes as Long → EpicsValue::Long, so we
/// verify the value survives to the GET side).
#[tokio::test]
async fn put_then_get_round_trips_long() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:longin", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:longin".into(),
        "TEST:longin".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Long,
    );
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Long(42))));
    ch.put(&put).await.expect("put");
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    let n = match value {
        PvField::Scalar(ScalarValue::Long(v)) => *v,
        PvField::Scalar(ScalarValue::Int(v)) => *v as i64,
        other => panic!("unexpected scalar variant: {other:?}"),
    };
    assert_eq!(n, 42);
}

/// pvxs `testGetScalar` parity for string records.
#[tokio::test]
async fn put_then_get_round_trips_string() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:str", Box::new(StringinRecord::new("init")))
        .await
        .unwrap();
    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:str".into(),
        "TEST:str".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::String,
    );
    // GET initial
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    match value {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "init"),
        other => panic!("expected string scalar, got {other:?}"),
    }
    // PUT new
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::String("hello".into())),
    ));
    ch.put(&put).await.expect("put");
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    match value {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "hello"),
        other => panic!("expected string scalar, got {other:?}"),
    }
}

/// pvxs `testGetArray` parity: NTScalarArray over a waveform.
#[tokio::test]
async fn waveform_array_round_trips() {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:wf",
        Box::new(WaveformRecord::new(8, DbFieldType::Double)),
    )
    .await
    .unwrap();
    // Seed an initial array via direct DB put.
    db.put_pv("TEST:wf", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .expect("seed");

    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:wf".into(),
        "TEST:wf".into(),
        "VAL".into(),
        NtType::ScalarArray,
        DbFieldType::Double,
    );
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalarArray.value");
    let len = match value {
        PvField::ScalarArray(arr) => arr.len(),
        other => panic!("expected scalar array, got {other:?}"),
    };
    assert!(
        len >= 3,
        "array should carry at least the seeded 3 elements"
    );
}

/// `BridgeChannel::channel_name` reports the full requested PV name —
/// the record name when no field suffix is given, and the full
/// `record.FIELD` string when one is (BR-R2).
#[test]
fn channel_name_matches_record() {
    let db = Arc::new(PvDatabase::new());
    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:abc".into(),
        "TEST:abc".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Double,
    );
    assert_eq!(ch.channel_name(), "TEST:abc");

    let ch_field = BridgeChannel::from_cached(
        db,
        "TEST:abc.DESC".into(),
        "TEST:abc".into(),
        "DESC".into(),
        NtType::Scalar,
        DbFieldType::String,
    );
    assert_eq!(ch_field.channel_name(), "TEST:abc.DESC");
    assert_eq!(ch_field.record_name(), "TEST:abc");
    assert_eq!(ch_field.field(), "DESC");
}

/// BR-R40: a pvxs-compatible channel-filter suffix `PV.VAL{...}`
/// strips off cleanly during record/field resolution, so the
/// returned channel name reflects the full filtered identity but
/// `record_name()` / `field()` resolve to the un-suffixed form.
/// The filter chain attaches to the monitor subscription (not
/// covered by this in-process test, but BR-R40's parser tests in
/// epics-base-rs exercise the chain construction).
#[tokio::test]
async fn channel_filter_suffix_strips_before_resolution() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:filt_ai", Box::new(AiRecord::new(1.5)))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let any = provider
        .create_channel_for(r#"TEST:filt_ai.VAL{"dbnd":{"d":2.0}}"#, "u", "h")
        .await
        .expect("filtered channel must resolve through split_channel_name");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    // The full client identity is preserved (used by ACF and error msgs).
    assert_eq!(ch.channel_name(), r#"TEST:filt_ai.VAL{"dbnd":{"d":2.0}}"#);
    // Record / field resolution uses the un-suffixed form.
    assert_eq!(ch.record_name(), "TEST:filt_ai");
    assert_eq!(ch.field(), "VAL");

    // GET works against the resolved record despite the filter suffix.
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    assert!(matches!(value, PvField::Scalar(ScalarValue::Double(v)) if (*v - 1.5).abs() < 1e-9));
}

/// BR-R2: a `record.FIELD` PV name binds to that field, not to VAL.
/// GET on `test:ai.EGU` returns the EGU string, not the AI VAL double.
/// PUT through the channel writes EGU, not VAL.
#[tokio::test]
async fn channel_with_field_suffix_binds_to_field() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:fld_ai", Box::new(AiRecord::new(3.125)))
        .await
        .unwrap();
    db.put_pv("TEST:fld_ai.EGU", EpicsValue::String("Volts".into()))
        .await
        .expect("seed EGU");

    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let any = provider
        .create_channel_for("TEST:fld_ai.EGU", "u", "h")
        .await
        .expect("create_channel");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    assert_eq!(ch.channel_name(), "TEST:fld_ai.EGU");

    // GET returns EGU string, not VAL double.
    let result = ch.get(&empty_request()).await.expect("get EGU");
    let value = extract_value(&result).expect("NTScalar.value");
    match value {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "Volts"),
        other => panic!("expected string EGU, got {other:?}"),
    }

    // PUT writes EGU, not VAL. After the put, EGU must change; VAL
    // must NOT change.
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::String("Amps".into())),
    ));
    ch.put(&put).await.expect("put EGU");

    let egu_after = {
        let rec = db.get_record("TEST:fld_ai").await.expect("rec exists");
        let inst = rec.read().await;
        inst.snapshot_for_field("EGU").map(|s| s.value)
    };
    assert!(
        matches!(egu_after, Some(EpicsValue::String(ref s)) if s == "Amps"),
        "EGU should be 'Amps', got {egu_after:?}"
    );

    let val_after = {
        let rec = db.get_record("TEST:fld_ai").await.expect("rec exists");
        let inst = rec.read().await;
        inst.snapshot_for_field("VAL").map(|s| s.value)
    };
    assert!(
        matches!(val_after, Some(EpicsValue::Double(v)) if (v - 3.125).abs() < 1e-9),
        "VAL must NOT have been overwritten, got {val_after:?}"
    );
}
