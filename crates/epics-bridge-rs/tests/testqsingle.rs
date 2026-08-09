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
//!
//! `qsrv-core` and not `qsrv`: this file reaches only `epics_bridge_rs::qsrv`,
//! which is what `qsrv-core` selects, and never the `PvaClient` that `qsrv`
//! additionally restores. Naming the wider feature would gate the file out of
//! the target's own selection for no reason.
#![cfg(feature = "qsrv-core")]

// RTEMS-EXEC-MODEL-ALLOW(24): checked - these run and pass in the feature-ON suite.

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

/// pvxs `IOCSource::doPreProcessing` parity (iocsource.cpp:363-375,
/// invoked from singlesource.cpp:354-356): a QSRV put to a `DISP=1`
/// record is rejected in *every* process mode, before any write. The
/// `Force` (process=true) and `Inhibit` (process=false) routes go
/// through `put_pv`, which does not itself gate DISP, so without the
/// boundary gate they would write/process a record an operator has
/// frozen — a safety-interlock bypass reachable via a standard
/// `record._options.process` pvRequest option.
#[tokio::test]
async fn disp_disabled_record_rejects_put_in_every_process_mode() {
    use epics_bridge_rs::qsrv::{ProcessMode, PutOptions};

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:ai_disp", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    // Operator freezes the record: DISP=1 (set through the internal
    // `put_pv`, which by design does not gate DISP).
    db.put_pv("TEST:ai_disp.DISP", EpicsValue::Char(1))
        .await
        .unwrap();

    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:ai_disp".into(),
        "TEST:ai_disp".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Double,
    );

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(9.0))));

    for mode in [
        ProcessMode::Passive,
        ProcessMode::Force,
        ProcessMode::Inhibit,
    ] {
        let opts = PutOptions {
            process: mode,
            block: false,
        };
        let err = ch
            .put_with_options(&put, opts)
            .await
            .expect_err("DISP=1 must reject the put in every process mode");
        assert!(
            err.to_string().to_lowercase().contains("disabled")
                || err.to_string().to_lowercase().contains("disp"),
            "expected a DISP rejection, got: {err}"
        );
    }

    // No write leaked through any mode — the frozen VAL is unchanged.
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    assert!(matches!(value, PvField::Scalar(ScalarValue::Double(v)) if (*v - 1.0).abs() < 1e-9));
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

/// EPICS `$` long-string field modifier (C `dbChannel.c:486-505`): a
/// `DBF_STRING` field named with a trailing `$` is re-viewed as a
/// `DBR_CHAR` character array, which pvxs serves as the `form = "String"`
/// long-string `NTScalar` (`ioc/iocsource.cpp:133-136`). `REC.VAL$`
/// therefore resolves (it no longer becomes a bogus field `VAL$`) and is
/// served as a string scalar via [`NtType::LongString`].
#[tokio::test]
async fn dollar_modifier_serves_string_field_as_long_string() {
    use epics_bridge_rs::qsrv::provider::{BridgeProvider, ChannelProvider};

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:str", Box::new(StringinRecord::new("init")))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    // Search must answer for the `$`-modified channel name.
    assert!(
        provider.channel_find("TEST:str.VAL$").await,
        "`REC.VAL$` must resolve at search"
    );

    let any = provider
        .create_channel_for("TEST:str.VAL$", "u", "h")
        .await
        .expect("`REC.VAL$` must resolve through the `$` modifier");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    // Resolved field drops the `$`; the channel serves the string view.
    assert_eq!(ch.record_name(), "TEST:str");
    assert_eq!(ch.field(), "VAL");
    assert!(
        matches!(ch.nt_type(), NtType::LongString),
        "`$` string field must be served as the long-string NTScalar view"
    );

    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    match value {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "init"),
        other => panic!("expected string scalar from `$` char view, got {other:?}"),
    }
}

/// `$` on a link field: C views a link (`DBF_INLINK..DBF_FWDLINK`) as a
/// `PVLINK_STRINGSZ` `DBR_CHAR` array (`dbChannel.c:494-498`), and pvxs
/// gives link fields the same string-form view (`ioc/channel.cpp:62-74`).
/// `REC.FLNK$` must resolve the link's textual form as a string scalar
/// rather than rejecting the `$` suffix as an unknown field.
#[tokio::test]
async fn dollar_modifier_serves_link_field_as_string() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:lnk", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    // Seed the forward link so the `$` view has a non-empty char string.
    db.put_record_field_from_ca_no_notify(
        "TEST:lnk",
        "FLNK",
        EpicsValue::String("TEST:other".into()),
    )
    .await
    .expect("seed FLNK");
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let any = provider
        .create_channel_for("TEST:lnk.FLNK$", "u", "h")
        .await
        .expect("`REC.FLNK$` must resolve the link as a char-array string view");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };
    assert_eq!(ch.field(), "FLNK");
    assert!(
        matches!(ch.nt_type(), NtType::LongString),
        "`$` link field must be served as the long-string NTScalar view"
    );

    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    match value {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "TEST:other"),
        other => panic!("expected link string from `$` char view, got {other:?}"),
    }
}

/// `$` is innermost in the channel name (`REC.FIELD$[range]{json}`), so it
/// must coexist with a trailing channel-filter suffix (C parses `$` then
/// `[range]` then `{json}`, `dbChannel.c:486-516`). A `$` channel that
/// also carries a JSON filter still resolves to the long-string view, and
/// the filter chain is attached for the read path.
#[tokio::test]
async fn dollar_modifier_with_filter_suffix_resolves() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:fstr", Box::new(StringinRecord::new("seed")))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let any = provider
        .create_channel_for(r#"TEST:fstr.VAL${"dbnd":{"d":0.5}}"#, "u", "h")
        .await
        .expect("`$` combined with a filter suffix must resolve");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };
    assert_eq!(ch.field(), "VAL");
    assert!(
        matches!(ch.nt_type(), NtType::LongString),
        "`$`+filter string channel must still be the long-string view"
    );
    // The stream-only `dbnd` filter short-circuits in read context, so the
    // GET still returns the underlying string.
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    match value {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "seed"),
        other => panic!("expected string scalar, got {other:?}"),
    }
}

/// `$` on a non-string, non-link field is `S_dbLib_fieldNotFound` in C
/// (`dbChannel.c:500-503`), which aborts channel creation. A numeric
/// `ai.VAL$` must therefore fail to create, not silently fall back to a
/// numeric scalar channel.
#[tokio::test]
async fn dollar_modifier_on_non_string_field_rejects_channel() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:num", Box::new(AiRecord::new(2.5)))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let result = provider.create_channel_for("TEST:num.VAL$", "u", "h").await;
    assert!(
        result.is_err(),
        "`$` on a numeric DBF_DOUBLE field must reject the channel \
         (S_dbLib_fieldNotFound parity), got Ok"
    );
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
        let rec = db.get_record("TEST:filt_wf").expect("rec");
        rec.write().notify_field("VAL", EventMask::VALUE);
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
        let rec = db.get_record("TEST:fld_ai").expect("rec exists");
        let inst = rec.read();
        inst.snapshot_for_field("EGU").map(|s| s.value)
    };
    assert!(
        matches!(egu_after, Some(EpicsValue::String(ref s)) if s == "Amps"),
        "EGU should be 'Amps', got {egu_after:?}"
    );

    let val_after = {
        let rec = db.get_record("TEST:fld_ai").expect("rec exists");
        let inst = rec.read();
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
    //
    // `.PROC` is `DBF_UCHAR` (`dbCommon.dbd:110`), not `DBF_CHAR`, and pvxs
    // serves `DBR_UCHAR` as `TypeCode::UInt8` (`ioc/typeutils.cpp`). The
    // `Byte` this case used to expect was the port's signed storage variant
    // showing through — the descriptor was derived from the stored value
    // instead of from the declaration.
    let cases = [
        ("TEST:ai.DESC", ScalarType::String), // DBF_STRING
        ("TEST:ai.PROC", ScalarType::UByte),  // DBF_UCHAR
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

/// A client STOP on a single-record monitor disables its
/// backing value+PROPERTY `DbSubscription`s — the in-process equivalent of
/// pvxs `MonitorControlOp::onStart(false)` ⇒ `db_event_disable`
/// (`singlesource.cpp:151`). Posts made while stopped are not delivered;
/// RESUME (`onStart(true)` ⇒ `db_event_enable`) restores delivery on the
/// same handles.
#[tokio::test]
async fn monitor_stop_disables_backing_subscription() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::provider::{BridgeProvider, PvaMonitor};

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:gate_ai", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let any = provider
        .create_channel_for("TEST:gate_ai", "u", "h")
        .await
        .expect("channel resolves");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    let mut mon = ch.create_monitor().await.expect("monitor");
    mon.start().await.expect("start");
    let handles = mon.activation_handles();
    assert!(
        !handles.is_empty(),
        "a started single-record monitor must expose its subscription gate handles"
    );

    async fn post(db: &Arc<PvDatabase>, mask: EventMask) {
        let rec = db.get_record("TEST:gate_ai").expect("rec");
        rec.write().notify_field("VAL", mask);
    }

    // Active (post-START): a VALUE post is delivered.
    post(&db, EventMask::VALUE).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), mon.poll())
        .await
        .expect("event must be delivered while the monitor is active")
        .expect("snapshot");

    // STOP: disable both subscriptions. A subsequent post is NOT delivered.
    for h in &handles {
        h.set_active(false).await;
    }
    post(&db, EventMask::VALUE).await;
    let stopped = tokio::time::timeout(std::time::Duration::from_millis(300), mon.poll()).await;
    assert!(
        stopped.is_err(),
        "no event may be delivered while the monitor is stopped"
    );

    // RESUME: re-enable the same handles. A post is delivered again.
    for h in &handles {
        h.set_active(true).await;
    }
    post(&db, EventMask::VALUE).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), mon.poll())
        .await
        .expect("event must be delivered after the monitor resumes")
        .expect("snapshot");
}

// ---------------------------------------------------------------------------
// asTrapWrite put-logging parity
//
// A QSRV PUT must fire the EPICS access-security put-logging hook
// (`asTrapWrite`) exactly when the matched ACF/ASG rule carries
// `TRAPWRITE`, mirroring pvxs's per-put `SecurityLogger`
// (ioc/singlesource.cpp:354-360) and C `asTrapWriteWithData` gating on
// the rule `trapMask` (libcom/src/as/asLib.h:57-60). A non-trapped or
// access-denied PUT fires nothing. The `WriteGrant` from the access
// layer is the single source of the trap decision.
// ---------------------------------------------------------------------------

use std::sync::Mutex;

use epics_base_rs::server::access_security::{
    TrapWriteListenerHandle, TrapWriteMessage, TrapWriteOp, register_trap_write_listener,
};
use epics_bridge_rs::qsrv::{AccessContext, AccessControl, ClientCreds, WriteGrant};

#[derive(Clone, Debug)]
struct CapturedTrap {
    op: TrapWriteOp,
    pv_name: String,
    user: String,
    host: String,
    value_str: String,
    dbr_type: u16,
    no_elements: u32,
    event_id: u64,
    status: Option<String>,
}

/// Test access policy: always grants write, with a configurable
/// `TRAPWRITE` flag on the returned [`WriteGrant`].
struct TrapStub {
    rule_was_trap: bool,
}
#[async_trait::async_trait]
impl AccessControl for TrapStub {
    async fn write_grant(&self, _channel: &str, _creds: &ClientCreds) -> WriteGrant {
        WriteGrant {
            allowed: true,
            rule_was_trap: self.rule_was_trap,
        }
    }
}

/// Register a trap-write listener that captures events for one PV.
/// nextest isolates each test in its own process, but filtering by PV
/// keeps the assertion sound even under a single-process `cargo test`.
fn capture_listener_for(
    pv: &'static str,
    sink: Arc<Mutex<Vec<CapturedTrap>>>,
) -> TrapWriteListenerHandle {
    register_trap_write_listener(Arc::new(move |msg: &TrapWriteMessage<'_>| {
        if msg.pv_name != pv {
            return;
        }
        sink.lock().unwrap().push(CapturedTrap {
            op: msg.op,
            pv_name: msg.pv_name.to_string(),
            user: msg.user.to_string(),
            host: msg.host.to_string(),
            value_str: msg.value_str.to_string(),
            dbr_type: msg.dbr_type,
            no_elements: msg.no_elements,
            event_id: msg.event_id,
            status: msg.status.map(|s| s.to_string()),
        });
    }))
}

/// A trapped PUT emits exactly one BeforeWrite and one AfterWrite, both
/// carrying the writing identity, value, and field DBR type.
#[tokio::test]
async fn trapped_single_put_emits_before_after_astrapwrite() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:trap_li", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:trap_li.VAL".into(),
        "TEST:trap_li".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Long,
    )
    .with_access(AccessContext::with_identity(
        Arc::new(TrapStub {
            rule_was_trap: true,
        }),
        "operator".into(),
        "host.acme".into(),
    ));

    let sink = Arc::new(Mutex::new(Vec::new()));
    let _handle = capture_listener_for("TEST:trap_li.VAL", sink.clone());

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Long(42))));
    ch.put(&put).await.expect("trapped put");

    let events = sink.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "one Before + one After: {events:?}");
    let before = &events[0];
    let after = &events[1];
    assert_eq!(before.op, TrapWriteOp::BeforeWrite);
    assert_eq!(after.op, TrapWriteOp::AfterWrite);
    assert_eq!(
        before.event_id, after.event_id,
        "the Before/After pair shares one event id"
    );
    for e in [before, after] {
        assert_eq!(e.pv_name, "TEST:trap_li.VAL");
        assert_eq!(e.user, "operator");
        assert_eq!(e.host, "host.acme");
        assert_eq!(e.value_str, "42");
        assert_eq!(e.dbr_type, DbFieldType::Long as u16);
        assert_eq!(e.no_elements, 1);
    }
    assert_eq!(before.status, None, "BeforeWrite carries no status");
    assert_eq!(after.status, Some("ok".to_string()), "successful put → ok");
}

/// A non-trapped PUT (matched rule has no `TRAPWRITE`) dispatches no
/// asTrapWrite event, but still performs the write.
#[tokio::test]
async fn non_trap_single_put_emits_nothing() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:notrap_li", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:notrap_li.VAL".into(),
        "TEST:notrap_li".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Long,
    )
    .with_access(AccessContext::with_identity(
        Arc::new(TrapStub {
            rule_was_trap: false,
        }),
        "operator".into(),
        "host.acme".into(),
    ));

    let sink = Arc::new(Mutex::new(Vec::new()));
    let _handle = capture_listener_for("TEST:notrap_li.VAL", sink.clone());

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Long(7))));
    ch.put(&put).await.expect("non-trap put");

    assert!(
        sink.lock().unwrap().is_empty(),
        "a non-trapped PUT must dispatch no asTrapWrite event"
    );
    // The write itself still happened.
    let got = ch.get(&empty_request()).await.expect("get");
    let n = match extract_value(&got).expect("value") {
        PvField::Scalar(ScalarValue::Long(x)) => *x,
        PvField::Scalar(ScalarValue::Int(x)) => *x as i64,
        other => panic!("unexpected scalar variant: {other:?}"),
    };
    assert_eq!(n, 7);
}

/// An access-denied PUT emits nothing — not even a BeforeWrite. The
/// grant gates emission before any dispatch.
#[tokio::test]
async fn denied_single_put_emits_nothing() {
    struct DenyAll;
    #[async_trait::async_trait]
    impl AccessControl for DenyAll {
        async fn can_write(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
    }

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:deny_li", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    let ch = BridgeChannel::from_cached(
        db,
        "TEST:deny_li.VAL".into(),
        "TEST:deny_li".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Long,
    )
    .with_access(AccessContext::with_identity(
        Arc::new(DenyAll),
        "u".into(),
        "h".into(),
    ));

    let sink = Arc::new(Mutex::new(Vec::new()));
    let _handle = capture_listener_for("TEST:deny_li.VAL", sink.clone());

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Long(1))));
    ch.put(&put).await.expect_err("denied put must reject");

    assert!(
        sink.lock().unwrap().is_empty(),
        "a denied PUT must emit nothing (no BeforeWrite)"
    );
}

/// A trapped PUT whose backing write fails still emits exactly one
/// BeforeWrite and one AfterWrite, the latter with `status = "fail"`.
/// The channel points at a record that was never added, so the backing
/// `put` returns `ChannelNotFound` after the grant passes.
#[tokio::test]
async fn trapped_single_put_failure_emits_one_after_fail() {
    let db = Arc::new(PvDatabase::new());
    let ch = BridgeChannel::from_cached(
        db,
        "TEST:trap_missing.VAL".into(),
        "TEST:trap_missing".into(),
        "VAL".into(),
        NtType::Scalar,
        DbFieldType::Long,
    )
    .with_access(AccessContext::with_identity(
        Arc::new(TrapStub {
            rule_was_trap: true,
        }),
        "operator".into(),
        "host.acme".into(),
    ));

    let sink = Arc::new(Mutex::new(Vec::new()));
    let _handle = capture_listener_for("TEST:trap_missing.VAL", sink.clone());

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Long(5))));
    ch.put(&put)
        .await
        .expect_err("put into missing record fails");

    let events = sink.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        2,
        "exactly one Before + one After even on failure: {events:?}"
    );
    assert_eq!(events[0].op, TrapWriteOp::BeforeWrite);
    assert_eq!(events[0].status, None);
    assert_eq!(events[1].op, TrapWriteOp::AfterWrite);
    assert_eq!(
        events[1].status,
        Some("fail".to_string()),
        "failed put → fail"
    );
    assert_eq!(events[0].event_id, events[1].event_id);
}

/// R17-31: the QSRV long-string idiom — a `DBF_CHAR` array VAL whose
/// record carries `info(Q:form, "String")` — is served as an
/// `NTScalar<string>`, not an `NTScalarArray<byte>`.
///
/// pvxs `IOCSource::getChannelValueType` (ioc/iocsource.cpp:634-636):
/// `final_field_type == DBR_CHAR && isArray && format() == "String"` →
/// `TypeCode::String`; the GET collapses the buffer at the NUL
/// (`getArrayValue`, :133-137) and a string PUT goes through
/// `putLongString` (:513-519) — `dbPut(DBR_CHAR, str, strlen+1)`, so
/// `NORD` counts the terminator.
///
/// Pre-fix the port had no `Q:form` reader outside `display.form`, so this
/// channel was an `NTScalarArray<byte>` and a string PUT was rejected
/// (the string was retyped to the bound `DBF_CHAR` and failed to parse).
#[tokio::test]
async fn r17_31_qform_string_char_waveform_serves_long_string() {
    use epics_pva_rs::pvdata::{FieldDesc, ScalarType};

    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:lstr",
        Box::new(WaveformRecord::new(40, DbFieldType::Char)),
    )
    .await
    .unwrap();
    {
        let rec = db.get_record("TEST:lstr").expect("record");
        rec.write().set_info("Q:form", "String");
    }
    db.put_pv("TEST:lstr", EpicsValue::CharArray(b"abc\0".to_vec()))
        .await
        .expect("seed");

    let ch = BridgeChannel::new(db.clone(), "TEST:lstr")
        .await
        .expect("new");
    assert_eq!(
        ch.nt_type(),
        NtType::LongString,
        "info(Q:form,\"String\") on a DBF_CHAR array VAL is the long-string idiom"
    );

    // The descriptor advertises a string scalar (NTScalar<string>).
    match ch.get_field().await.expect("get_field") {
        FieldDesc::Structure { fields, .. } => {
            let v = fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d);
            assert!(
                matches!(v, Some(FieldDesc::Scalar(ScalarType::String))),
                "value descriptor must be pvString, got {v:?}"
            );
        }
        other => panic!("expected NTScalar descriptor, got {other:?}"),
    }

    // GET collapses the CHAR buffer at the NUL.
    match extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value") {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "abc"),
        other => panic!("expected scalar string value, got {other:?}"),
    }

    // A string PUT is accepted (putLongString), and it writes the C image:
    // the bytes plus the NUL, so NORD == strlen + 1.
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::String("hello world".into())),
    ));
    ch.put(&put)
        .await
        .expect("string PUT into a long-string channel");

    match extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value") {
        PvField::Scalar(ScalarValue::String(s)) => assert_eq!(s, "hello world"),
        other => panic!("expected updated string, got {other:?}"),
    }
    let nord = {
        let rec = db.get_record("TEST:lstr").expect("record");
        let inst = rec.read();
        inst.resolve_field("NORD").expect("NORD")
    };
    assert_eq!(
        nord,
        EpicsValue::ULong(12),
        "putLongString writes strlen+1 CHAR elements (the NUL counts)"
    );
}

/// Boundary for R17-31: the SAME record without the info tag stays an
/// `NTScalarArray<byte>` — `Q:form` is what selects the string view, and
/// pvxs applies it to VAL only (`dbIsValueField`, ioc/channel.cpp:43-47).
#[tokio::test]
async fn r17_31_char_waveform_without_qform_stays_a_byte_array() {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:bytes",
        Box::new(WaveformRecord::new(40, DbFieldType::Char)),
    )
    .await
    .unwrap();
    db.put_pv("TEST:bytes", EpicsValue::CharArray(b"abc".to_vec()))
        .await
        .expect("seed");

    let ch = BridgeChannel::new(db.clone(), "TEST:bytes")
        .await
        .expect("new");
    assert_eq!(ch.nt_type(), NtType::ScalarArray);
    match extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value") {
        PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => {}
        other => panic!("expected a byte array, got {other:?}"),
    }
}

/// pvxs sends a DBF link field down the dbPutField path no matter the
/// requested process mode: `doDbPut` splits per-field
/// (`iocsource.cpp:451-458`) and the single source skips post-processing
/// for links entirely (`singlesource.cpp:374-383`); a blocking put lands in
/// `dbProcessNotify`'s link special case (write + immediate done,
/// `dbNotify.c:337-353`). The port's Force/Inhibit routes used `put_pv`,
/// the `dbPut` analogue that now refuses link fields (S_db_badDbrtype,
/// `dbAccess.c:1340`) — a link-field put must re-route, not refuse.
#[tokio::test]
async fn link_field_put_succeeds_in_every_process_mode() {
    use epics_bridge_rs::qsrv::{ProcessMode, PutOptions};

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:lnkmode", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("TEST:lnktgt", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let ch = BridgeChannel::from_cached(
        db.clone(),
        "TEST:lnkmode.FLNK".into(),
        "TEST:lnkmode".into(),
        "FLNK".into(),
        NtType::Scalar,
        DbFieldType::String,
    );

    for (mode, block, target) in [
        (ProcessMode::Passive, false, "TEST:lnktgt"),
        (ProcessMode::Inhibit, false, ""),
        (ProcessMode::Force, false, "TEST:lnktgt"),
        (ProcessMode::Force, true, ""),
    ] {
        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields.push((
            "value".into(),
            PvField::Scalar(ScalarValue::String(target.into())),
        ));
        let opts = PutOptions {
            process: mode,
            block,
        };
        ch.put_with_options(&put, opts)
            .await
            .unwrap_or_else(|e| panic!("{mode:?} block={block}: link-field put must succeed: {e}"));
        let text = match db.get_pv("TEST:lnkmode.FLNK").unwrap() {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            other => panic!("FLNK read back non-string: {other:?}"),
        };
        assert_eq!(text, target, "{mode:?} block={block}");
    }
}
