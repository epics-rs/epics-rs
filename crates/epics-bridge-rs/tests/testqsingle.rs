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
    let ch = BridgeChannel::from_cached(db, "TEST:ai".into(), NtType::Scalar, DbFieldType::Double);

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

/// `BridgeChannel::channel_name` reports the configured record name
/// — guards against accidental field-suffix leakage.
#[test]
fn channel_name_matches_record() {
    let db = Arc::new(PvDatabase::new());
    let ch = BridgeChannel::from_cached(db, "TEST:abc".into(), NtType::Scalar, DbFieldType::Double);
    assert_eq!(ch.channel_name(), "TEST:abc");
}
