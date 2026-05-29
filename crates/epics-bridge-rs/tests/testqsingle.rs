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
use epics_base_rs::server::records::lsi::LsiRecord;
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
/// `record.FIELD` string when one is.
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

/// a pvxs-compatible channel-filter suffix `PV.VAL{...}`
/// strips off cleanly during record/field resolution, so the
/// returned channel name reflects the full filtered identity but
/// `record_name()` / `field()` resolve to the un-suffixed form.
/// The filter chain attaches to the monitor subscription (not
/// covered by this in-process test, but the parser tests in
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

/// pvxs wraps every QSRV GET in a `LocalFieldLog` and runs the
/// field-log chain before serialization (ioc/singlesource.cpp:278-292,
/// ioc/localfieldlog.cpp:15-24), so an `arr` channel filter reshapes
/// the GET read value exactly as it reshapes a monitor event. This
/// asserts the three-way parity: filtered GET == filtered monitor
/// event == the arr slice, and that an unfiltered channel on the same
/// record still returns the full array (proving the slice comes from
/// the filter chain, not the record state).
#[tokio::test]
async fn arr_channel_filter_applies_to_get_matching_monitor() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::provider::{BridgeProvider, PvaMonitor};

    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:filt_wf",
        Box::new(WaveformRecord::new(8, DbFieldType::Double)),
    )
    .await
    .unwrap();
    db.put_pv(
        "TEST:filt_wf",
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0, 50.0]),
    )
    .await
    .expect("seed");

    let provider = Arc::new(BridgeProvider::new(db.clone()));

    // `arr` slice [1..=3] over the 5-element seed selects 20,30,40.
    let any = provider
        .create_channel_for(r#"TEST:filt_wf.VAL{"arr":{"s":1,"e":3}}"#, "u", "h")
        .await
        .expect("filtered channel resolves");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    let doubles = |s: &PvStructure| -> Vec<f64> {
        match extract_value(s).expect("value") {
            PvField::ScalarArray(a) => a
                .iter()
                .map(|v| match v {
                    ScalarValue::Double(d) => *d,
                    other => panic!("expected double element, got {other:?}"),
                })
                .collect(),
            other => panic!("expected scalar array, got {other:?}"),
        }
    };

    // GET applies the chain in read context.
    let get_result = ch.get(&empty_request()).await.expect("get");
    let get_slice = doubles(&get_result);
    assert_eq!(
        get_slice,
        vec![20.0, 30.0, 40.0],
        "filtered GET must return the arr slice, not the full array"
    );

    // The monitor event on the same filtered channel carries the
    // identical slice.
    let mut mon = ch.create_monitor().await.expect("monitor");
    mon.start().await.expect("start");
    {
        let rec = db.get_record("TEST:filt_wf").await.expect("rec");
        rec.read().await.notify_field("VAL", EventMask::VALUE);
    }
    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), mon.poll())
        .await
        .expect("monitor event within 2s")
        .expect("snapshot");
    assert_eq!(
        doubles(&ev.value),
        get_slice,
        "monitor event slice must match the filtered GET slice"
    );

    // An unfiltered channel on the same record returns the full array.
    let plain = BridgeChannel::from_cached(
        db.clone(),
        "TEST:filt_wf".into(),
        "TEST:filt_wf".into(),
        "VAL".into(),
        NtType::ScalarArray,
        DbFieldType::Double,
    );
    let plain_result = plain.get(&empty_request()).await.expect("get");
    assert_eq!(
        doubles(&plain_result),
        vec![10.0, 20.0, 30.0, 40.0, 50.0],
        "an unfiltered channel must return all 5 seeded elements"
    );
}

/// Legacy EPICS array-range modifiers (`WF.VAL[start:incr:end]`) are
/// channel syntax, not field-name text. pvxs builds single-record
/// channels through `dbChannelCreate`, which parses the range and
/// inserts an `arr` filter (dbChannel.c:351-446, 507-510). Rust now
/// normalises the range into the same `arr` filter at the
/// `split_channel_name` resolution boundary, so `WF.VAL[1:3]` resolves
/// to record `WF`, field `VAL`, and serves the slice — identical to the
/// JSON `{"arr":{"s":1,"e":3}}` form. Before the fix the modifier was
/// preserved as a bogus field name `VAL[1:3]`, resolving the base record
/// at search but failing the first GET with `FieldNotFound`.
#[tokio::test]
async fn legacy_array_range_modifier_resolves_and_slices() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;

    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:rng_wf",
        Box::new(WaveformRecord::new(8, DbFieldType::Double)),
    )
    .await
    .unwrap();
    db.put_pv(
        "TEST:rng_wf",
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0, 50.0]),
    )
    .await
    .expect("seed");

    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let doubles = |s: &PvStructure| -> Vec<f64> {
        match extract_value(s).expect("value") {
            PvField::ScalarArray(a) => a
                .iter()
                .map(|v| match v {
                    ScalarValue::Double(d) => *d,
                    other => panic!("expected double element, got {other:?}"),
                })
                .collect(),
            other => panic!("expected scalar array, got {other:?}"),
        }
    };

    let single = |c: epics_bridge_rs::qsrv::AnyChannel| match c {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    // `[start:end]` → arr slice [1..=3] over the 5-element seed.
    let ch = single(
        provider
            .create_channel_for("TEST:rng_wf.VAL[1:3]", "u", "h")
            .await
            .expect("`[1:3]` range must resolve through split_channel_name"),
    );
    // Resolution uses the un-suffixed record/field; the full client
    // identity is preserved for ACF / error messages.
    assert_eq!(ch.channel_name(), "TEST:rng_wf.VAL[1:3]");
    assert_eq!(ch.record_name(), "TEST:rng_wf");
    assert_eq!(ch.field(), "VAL");
    let g = ch
        .get(&empty_request())
        .await
        .expect("get must not FieldNotFound");
    assert_eq!(
        doubles(&g),
        vec![20.0, 30.0, 40.0],
        "`[1:3]` must serve the slice, not the full array"
    );

    // `[N]` single element selects index N only.
    let ch1 = single(
        provider
            .create_channel_for("TEST:rng_wf.VAL[2]", "u", "h")
            .await
            .expect("`[2]` range resolves"),
    );
    assert_eq!(
        doubles(&ch1.get(&empty_request()).await.expect("get")),
        vec![30.0],
        "`[2]` must serve only element 2"
    );

    // `[start:incr:end]` strides the slice.
    let ch2 = single(
        provider
            .create_channel_for("TEST:rng_wf.VAL[0:2:4]", "u", "h")
            .await
            .expect("`[0:2:4]` range resolves"),
    );
    assert_eq!(
        doubles(&ch2.get(&empty_request()).await.expect("get")),
        vec![10.0, 30.0, 50.0],
        "`[0:2:4]` must serve every second element"
    );
}

/// a `record.FIELD` PV name binds to that field, not to VAL.
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

/// BR-56 parity: an `lsi` long-string record's VAL is a `DBF_CHAR` array
/// that semantically holds a string. pvxs serves it as a `pvString`
/// NTScalar (`form = "String"`); the Rust bridge must too, instead of
/// collapsing the byte array to a single `pvByte` (the first byte).
#[tokio::test]
async fn lsi_long_string_get_put_round_trips_as_string() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:lsi", Box::new(LsiRecord::new("abcdef")))
        .await
        .unwrap();

    // `BridgeChannel::new` classifies the channel from the record's
    // `long_string_fields` declaration.
    let ch = BridgeChannel::new(db.clone(), "TEST:lsi")
        .await
        .expect("new");
    assert_eq!(ch.nt_type(), NtType::LongString);

    // Descriptor advertises `value` as a string scalar.
    let desc = ch.get_field().await.expect("get_field");
    match desc {
        epics_pva_rs::pvdata::FieldDesc::Structure { fields, .. } => {
            let v = fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d);
            assert!(
                matches!(
                    v,
                    Some(epics_pva_rs::pvdata::FieldDesc::Scalar(
                        epics_pva_rs::pvdata::ScalarType::String
                    ))
                ),
                "value descriptor must be pvString, got {v:?}"
            );
        }
        other => panic!("expected NTScalar descriptor, got {other:?}"),
    }

    // GET returns the full string, not the first byte.
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    match value {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "abcdef"),
        other => panic!("expected scalar string value, got {other:?}"),
    }

    // PUT a scalar string; the record stores it (no DBF_CHAR retype that
    // would reject the multi-character string), and GET sees the update.
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::String("hello world".into())),
    ));
    ch.put(&put).await.expect("put string");

    let after = ch.get(&empty_request()).await.expect("get after put");
    match extract_value(&after).expect("value") {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "hello world"),
        other => panic!("expected updated string, got {other:?}"),
    }
}

/// BR-59 parity: a common-field channel (`.DESC`, `.PROC`, `.UTAG`, …)
/// must advertise its real DBF type in the `getField` descriptor, not
/// the `double` fallback. pvxs derives the served type from
/// `dbChannelFinalFieldType(chan)` (singlesource.cpp:189-205), which
/// covers `dbCommon` fields. Because the descriptor type and the GET
/// value both come from the field's resolved value, they must agree —
/// the prior `field_list` + `Double` fallback produced `double value`
/// descriptors over string/char/enum/ulong payloads.
#[tokio::test]
async fn common_field_descriptor_matches_value_type() {
    use epics_pva_rs::pvdata::{FieldDesc, ScalarType};

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:ai", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    // (field, expected descriptor scalar type)
    let cases = [
        ("TEST:ai.DESC", ScalarType::String), // DBF_STRING
        ("TEST:ai.PROC", ScalarType::Byte),   // DBF_CHAR
        ("TEST:ai.UTAG", ScalarType::ULong),  // DBF_UINT64
    ];

    for (name, expected) in cases {
        let ch = BridgeChannel::new(db.clone(), name).await.expect("new");

        let desc = ch.get_field().await.expect("get_field");
        let desc_ty = match &desc {
            FieldDesc::Structure { fields, .. } => fields
                .iter()
                .find(|(n, _)| n == "value")
                .and_then(|(_, d)| match d {
                    FieldDesc::Scalar(st) => Some(*st),
                    _ => None,
                }),
            _ => None,
        }
        .unwrap_or_else(|| panic!("{name}: no scalar value descriptor"));

        assert_eq!(
            desc_ty, expected,
            "{name}: descriptor value type must be the field's real DBF, not double"
        );
        assert_ne!(
            desc_ty,
            ScalarType::Double,
            "{name}: must not advertise the double fallback"
        );

        // The GET value's scalar type must agree with the descriptor —
        // the wire-schema contract the descriptor promises.
        let result = ch.get(&empty_request()).await.expect("get");
        let val_ty = match extract_value(&result).expect("value") {
            PvField::Scalar(sv) => sv.scalar_type(),
            other => panic!("{name}: expected scalar value, got {other:?}"),
        };
        assert_eq!(
            val_ty, desc_ty,
            "{name}: GET value type must match the advertised descriptor"
        );
    }
}

/// BR-60 parity: a non-VAL enum/menu field (`REC.SCAN`) must be served
/// as NTEnum (value.index + value.choices), not forced into NTScalar.
/// pvxs builds the single-record prototype from
/// `dbChannelFinalFieldType(chan)`, so DBF_ENUM/MENU/DEVICE fields select
/// nt::NTEnum regardless of the field name (singlesource.cpp:189-205,
/// dbAccess.c:88-90). A non-VAL scalar field stays NTScalar (no
/// over-promotion).
#[tokio::test]
async fn non_val_enum_field_is_ntenum() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:ai", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    // `.SCAN` is a DBF_MENU/ENUM common field → NTEnum.
    let scan = BridgeChannel::new(db.clone(), "TEST:ai.SCAN")
        .await
        .expect("new");
    assert_eq!(scan.nt_type(), NtType::Enum, ".SCAN must be NTEnum");

    // GET yields an `enum_t` value with an int32 index and the SCAN menu
    // choices — the path that the prior NTScalar classification dropped.
    let result = scan.get(&empty_request()).await.expect("get");
    match extract_value(&result).expect("NTEnum value") {
        PvField::Structure(s) => {
            let has_index = s
                .fields
                .iter()
                .any(|(n, f)| n == "index" && matches!(f, PvField::Scalar(ScalarValue::Int(_))));
            let choices_len = s
                .fields
                .iter()
                .find(|(n, _)| n == "choices")
                .and_then(|(_, f)| match f {
                    PvField::ScalarArray(a) => Some(a.len()),
                    _ => None,
                })
                .unwrap_or(0);
            assert!(has_index, ".SCAN value.index must be an int32");
            assert!(
                choices_len > 0,
                ".SCAN value.choices must enumerate the SCAN menu"
            );
        }
        other => panic!("expected enum_t value structure, got {other:?}"),
    }

    // A non-VAL scalar common field is NOT promoted — it stays NTScalar.
    let desc = BridgeChannel::new(db.clone(), "TEST:ai.DESC")
        .await
        .expect("new");
    assert_eq!(desc.nt_type(), NtType::Scalar, ".DESC must stay NTScalar");
}
