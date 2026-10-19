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

// RTEMS-EXEC-MODEL-ALLOW(40): checked - these run and pass in the exec-backend
// suite.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::char_view::PvaDollar;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
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

/// pvxs `IOCSource::doPreProcessing` parity (iocsource.cpp:362-375,
/// invoked from singlesource.cpp:356-359): a QSRV put to a `DISP=1`
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
/// `DBR_CHAR` character array, which pvxs serves as `int8_t[]`, the text and
/// its NUL: `softIocPVX` 1.5.3 serves a stringin `REC.VAL$` holding `init`
/// as `{5}[105,110,105,116,0]`.
#[tokio::test]
async fn dollar_modifier_serves_string_field_as_its_char_image() {
    use epics_bridge_rs::qsrv::provider::{BridgeProvider, ChannelProvider};

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:str", Box::new(StringinRecord::new("init")))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    // Search must answer for the `$`-modified channel name.
    assert!(
        provider.channel_find(b"TEST:str.VAL$").await,
        "`REC.VAL$` must resolve at search"
    );

    let any = provider
        .create_channel_for(b"TEST:str.VAL$", "u", "h")
        .await
        .expect("`REC.VAL$` must resolve through the `$` modifier");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    // Resolved field drops the `$`; the channel serves the string view.
    assert_eq!(ch.record_name(), b"TEST:str");
    assert_eq!(ch.field(), "VAL");
    assert_eq!(ch.nt_type(), NtType::Dollar(PvaDollar::Chars));

    let result = ch.get(&empty_request()).await.expect("get");
    assert_eq!(extract_value(&result), Some(&chars(b"init\0")));
}

fn chars(b: &[u8]) -> PvField {
    epics_bridge_rs::convert::epics_to_pv_field(&EpicsValue::CharArray(b.to_vec().into()))
}

/// The SECOND channel for a `$` name — the one `BridgeProvider` builds from
/// its metadata cache through `BridgeChannel::from_cached` — carries the same
/// view as the first.
///
/// The cache is keyed by the full requested name, `$` included, and a name
/// with no `{json}` suffix takes the cached path, so `REC.VAL$` reaches
/// `from_cached` on every request after the first. `from_cached` is handed
/// only `(record, FIELD, nt_type, dbf)`, so it derives the view from
/// `pv_name` with the same `parse_channel_name` that `new` used rather than
/// taking it as an argument: a constructor that could be given a field
/// without its view is a constructor that can build the channel C's
/// `dbChannelCreate` never would.
///
/// Regression guard, not a fails-first witness: for the two eligible C
/// branches the viewed and unviewed reads currently agree on the value, so
/// this locks the door choice rather than catching a live divergence.
#[tokio::test]
async fn a_cached_dollar_channel_keeps_the_view_of_its_name() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:cached", Box::new(StringinRecord::new("seed")))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let build = |name: &'static str| {
        let provider = provider.clone();
        async move {
            match provider
                .create_channel_for(name.as_bytes(), "u", "h")
                .await
                .unwrap()
            {
                epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
                _ => panic!("expected single-record channel"),
            }
        }
    };

    let first = build("TEST:cached.VAL$").await;
    let cached = build("TEST:cached.VAL$").await;
    assert_eq!(
        cached.nt_type(),
        NtType::Dollar(PvaDollar::Chars),
        "the cached channel advertises the same char-image NT"
    );

    let of = |r: &PvStructure| extract_value(r).expect("value").clone();
    let a = of(&first.get(&empty_request()).await.expect("first get"));
    let b = of(&cached.get(&empty_request()).await.expect("cached get"));
    assert_eq!(a, chars(b"seed\0"));
    assert_eq!(a, b, "the cached channel serves what the fresh one serves");

    // The unviewed channel for the SAME field is a different channel, and
    // stays one: `TEST:cached.VAL` has no `$`, so nothing about the cached
    // `$` entry may leak into it.
    let plain = build("TEST:cached.VAL").await;
    assert_eq!(
        of(&plain.get(&empty_request()).await.expect("plain get")),
        PvField::Scalar(ScalarValue::String("seed".into()))
    );
}

/// `$` on a link field: C views a link (`DBF_INLINK..DBF_FWDLINK`) as a
/// `PVLINK_STRINGSZ` `DBR_CHAR` array (`dbChannel.c:494-499`), and pvxs
/// gives link fields the same string-form view (`ioc/channel.cpp:69-74`).
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
        .create_channel_for(b"TEST:lnk.FLNK$", "u", "h")
        .await
        .expect("`REC.FLNK$` must resolve the link as a char-array string view");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };
    assert_eq!(ch.field(), "FLNK");
    assert!(
        matches!(ch.nt_type(), NtType::Dollar(PvaDollar::Text)),
        "`$` link field must be served as its text"
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
/// also carries a JSON filter still resolves to the char image, and the
/// filter chain is attached for the read path: `softIocPVX` 1.5.3 serves
/// `REC.VAL${"dbnd":{"d":0.5}}` as `REC.VAL$`.
#[tokio::test]
async fn dollar_modifier_with_filter_suffix_resolves() {
    use epics_bridge_rs::qsrv::provider::BridgeProvider;

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:fstr", Box::new(StringinRecord::new("seed")))
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let any = provider
        .create_channel_for(br#"TEST:fstr.VAL${"dbnd":{"d":0.5}}"#, "u", "h")
        .await
        .expect("`$` combined with a filter suffix must resolve");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };
    assert_eq!(ch.field(), "VAL");
    assert_eq!(ch.nt_type(), NtType::Dollar(PvaDollar::Chars));
    // The stream-only `dbnd` filter short-circuits in read context, so the
    // GET still returns the underlying image.
    let result = ch.get(&empty_request()).await.expect("get");
    assert_eq!(extract_value(&result), Some(&chars(b"seed\0")));
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

    let result = provider
        .create_channel_for(b"TEST:num.VAL$", "u", "h")
        .await;
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
    db.put_pv(
        "TEST:wf",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()),
    )
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
        PvField::ScalarArrayTyped(arr) => arr.len(),
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
    assert_eq!(ch.channel_name(), b"TEST:abc");

    let ch_field = BridgeChannel::from_cached(
        db,
        "TEST:abc.DESC".into(),
        "TEST:abc".into(),
        "DESC".into(),
        NtType::Scalar,
        DbFieldType::String,
    );
    assert_eq!(ch_field.channel_name(), b"TEST:abc.DESC");
    assert_eq!(ch_field.record_name(), b"TEST:abc");
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
        .create_channel_for(br#"TEST:filt_ai.VAL{"dbnd":{"d":2.0}}"#, "u", "h")
        .await
        .expect("filtered channel must resolve through split_channel_name");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    // The full client identity is preserved (used by ACF and error msgs).
    assert_eq!(ch.channel_name(), br#"TEST:filt_ai.VAL{"dbnd":{"d":2.0}}"#);
    // Record / field resolution uses the un-suffixed form.
    assert_eq!(ch.record_name(), b"TEST:filt_ai");
    assert_eq!(ch.field(), "VAL");

    // GET works against the resolved record despite the filter suffix.
    let result = ch.get(&empty_request()).await.expect("get");
    let value = extract_value(&result).expect("NTScalar.value");
    assert!(matches!(value, PvField::Scalar(ScalarValue::Double(v)) if (*v - 1.5).abs() < 1e-9));
}

/// pvxs wraps every QSRV GET in a `LocalFieldLog` and runs the
/// field-log chain before serialization (ioc/singlesource.cpp:278-292,
/// ioc/localfieldlog.cpp:15-27), so an `arr` channel filter reshapes
/// the GET read value exactly as it reshapes a monitor event. This
/// asserts the three-way parity: filtered GET == filtered monitor
/// event == the arr slice, and that an unfiltered channel on the same
/// record still returns the full array (proving the slice comes from
/// the filter chain, not the record state).
#[tokio::test]
async fn arr_channel_filter_applies_to_get_matching_monitor() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::provider::BridgeProvider;
    use epics_pva_rs::server_native::source::ChannelSource;

    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:filt_wf",
        Box::new(WaveformRecord::new(8, DbFieldType::Double)),
    )
    .await
    .unwrap();
    db.put_pv(
        "TEST:filt_wf",
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0, 50.0].into()),
    )
    .await
    .expect("seed");

    let provider = Arc::new(BridgeProvider::new(db.clone()));

    // `arr` slice [1..=3] over the 5-element seed selects 20,30,40.
    let any = provider
        .create_channel_for(br#"TEST:filt_wf.VAL{"arr":{"s":1,"e":3}}"#, "u", "h")
        .await
        .expect("filtered channel resolves");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    let doubles = |s: &PvStructure| -> Vec<f64> {
        match extract_value(s).expect("value") {
            PvField::ScalarArrayTyped(a) => a.as_doubles().expect("double array").to_vec(),
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

    // The monitor event on the same filtered channel, from `qsrvSingle`,
    // carries the identical slice.
    let mut mon = epics_pva_rs::server::PvDatabaseSource::new(db.clone())
        .subscribe(br#"TEST:filt_wf.VAL{"arr":{"s":1,"e":3}}"#)
        .await
        .expect("monitor");
    {
        let rec = db.get_record("TEST:filt_wf").expect("rec");
        rec.write().notify_field("VAL", EventMask::VALUE);
    }
    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), mon.recv())
        .await
        .expect("monitor event within 2s")
        .expect("snapshot");
    let PvField::Structure(ev) = ev else {
        panic!("expected an NT structure, got {ev:?}");
    };
    assert_eq!(
        doubles(&ev),
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
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0, 50.0].into()),
    )
    .await
    .expect("seed");

    let provider = Arc::new(BridgeProvider::new(db.clone()));

    let doubles = |s: &PvStructure| -> Vec<f64> {
        match extract_value(s).expect("value") {
            PvField::ScalarArrayTyped(a) => a.as_doubles().expect("double array").to_vec(),
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
            .create_channel_for(b"TEST:rng_wf.VAL[1:3]", "u", "h")
            .await
            .expect("`[1:3]` range must resolve through split_channel_name"),
    );
    // Resolution uses the un-suffixed record/field; the full client
    // identity is preserved for ACF / error messages.
    assert_eq!(ch.channel_name(), b"TEST:rng_wf.VAL[1:3]");
    assert_eq!(ch.record_name(), b"TEST:rng_wf");
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

    // `[N]` selects index N only, and one element is a scalar channel:
    // softIocPVX 1.5.3 serves `W.VAL[2]` over `[1,2,3]` as `int32_t = 3`.
    let ch1 = single(
        provider
            .create_channel_for(b"TEST:rng_wf.VAL[2]", "u", "h")
            .await
            .expect("`[2]` range resolves"),
    );
    assert_eq!(
        extract_value(&ch1.get(&empty_request()).await.expect("get")),
        Some(&PvField::Scalar(ScalarValue::Double(30.0))),
        "`[2]` must serve only element 2"
    );

    // `[start:incr:end]` strides the slice.
    let ch2 = single(
        provider
            .create_channel_for(b"TEST:rng_wf.VAL[0:2:4]", "u", "h")
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
        .create_channel_for(b"TEST:fld_ai.EGU", "u", "h")
        .await
        .expect("create_channel");
    let ch = match any {
        epics_bridge_rs::qsrv::AnyChannel::Single(c) => c,
        _ => panic!("expected single-record channel"),
    };

    assert_eq!(ch.channel_name(), b"TEST:fld_ai.EGU");

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

/// Put 45 characters to `LS:O` of an lso with `sizv` through the bridge,
/// return `LEN`.
async fn lso_long_string_put_len(sizv: u16) -> f64 {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(
            &format!(r#"record(lso, "LS:O") {{ field(SIZV, "{sizv}") }}"#),
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let ch = BridgeChannel::new(db.clone(), b"LS:O").await.expect("new");
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::String("x".repeat(45).into())),
    ));
    ch.put(&put).await.expect("put string");
    let len = db.get_record("LS:O").unwrap().read().resolve_field("LEN");
    len.and_then(|v| v.to_f64()).expect("LEN")
}

/// pvxs re-views an lso VAL whose `SIZV` exceeds 41 as a char array
/// (`ioc/channel.cpp:56-72`), so the PUT is `putLongString`: 45 characters
/// store with `LEN` 46; at `SIZV` 41 it is `putScalar`'s `DBR_STRING`, 39
/// characters and `LEN` 40 — both `softIocPVX` 1.5.3 live.
#[tokio::test]
async fn lso_long_string_put_fills_sizv_past_41_only() {
    assert_eq!(lso_long_string_put_len(64).await, 46.0);
    assert_eq!(lso_long_string_put_len(41).await, 40.0);
}

/// BR-56 parity: an `lsi` long-string record's VAL is a `DBF_CHAR` array
/// that semantically holds a string. pvxs serves it as a `pvString`
/// NTScalar (`form = "String"`); the Rust bridge must too, instead of
/// collapsing the byte array to a single `pvByte` (the first byte).
#[tokio::test]
async fn lsi_long_string_get_put_round_trips_as_string() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:lsi", Box::new(LsiRecord::new("")))
        .await
        .unwrap();
    // After init_record, which empties VAL as C's calloc does.
    db.put_pv("TEST:lsi", EpicsValue::String("abcdef".into()))
        .await
        .unwrap();

    // `BridgeChannel::new` classifies the channel from the field.s
    // `FieldDesc::long_string` declaration.
    let ch = BridgeChannel::new(db.clone(), b"TEST:lsi")
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
/// `dbChannelFinalFieldType(chan)` (singlesource.cpp:189-206), which
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
    // `.PROC` is `DBF_UCHAR` (`dbCommon.dbd.pod:241`), not `DBF_CHAR`, and pvxs
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
        let ch = BridgeChannel::new(db.clone(), name.as_bytes())
            .await
            .expect("new");

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
/// nt::NTEnum regardless of the field name (singlesource.cpp:189-206,
/// dbAccess.c:88-90). A non-VAL scalar field stays NTScalar (no
/// over-promotion).
#[tokio::test]
async fn non_val_enum_field_is_ntenum() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:ai", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    // `.SCAN` is a DBF_MENU/ENUM common field → NTEnum.
    let scan = BridgeChannel::new(db.clone(), b"TEST:ai.SCAN")
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
    let desc = BridgeChannel::new(db.clone(), b"TEST:ai.DESC")
        .await
        .expect("new");
    assert_eq!(desc.nt_type(), NtType::Scalar, ".DESC must stay NTScalar");
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
    pv_name: Vec<u8>,
    user: Vec<u8>,
    host: Vec<u8>,
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
    async fn write_grant(&self, _channel: &[u8], _creds: &ClientCreds) -> WriteGrant {
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
        if msg.pv_name != pv.as_bytes() {
            return;
        }
        sink.lock().unwrap().push(CapturedTrap {
            op: msg.op,
            pv_name: msg.pv_name.to_vec(),
            user: msg.user.to_vec(),
            host: msg.host.to_vec(),
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
        assert_eq!(e.pv_name, b"TEST:trap_li.VAL");
        assert_eq!(e.user, b"operator");
        assert_eq!(e.host, b"host.acme");
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
        async fn can_write(&self, _: &[u8], _: &[u8], _: &str) -> bool {
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
/// pvxs `IOCSource::getChannelValueType` (ioc/iocsource.cpp:635-636):
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
    db.put_pv("TEST:lstr", EpicsValue::CharArray(b"abc\0".to_vec().into()))
        .await
        .expect("seed");

    let ch = BridgeChannel::new(db.clone(), b"TEST:lstr")
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
    db.put_pv("TEST:bytes", EpicsValue::CharArray(b"abc".to_vec().into()))
        .await
        .expect("seed");

    let ch = BridgeChannel::new(db.clone(), b"TEST:bytes")
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
/// `dbNotify.c:337-354`). The port's Force/Inhibit routes used `put_pv`,
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

/// The array-ness boundary is `dbChannelFinalElements(chan) != 1`
/// (`iocsource.cpp:631`), not the FTVL storage variant.
///
/// C QSRV serves an `NELM=1` array field as an `NTScalar` with a scalar
/// value leaf, and routes its PUT through `putScalar` (`iocsource.cpp:601`).
/// The port keyed the shape on the stored `*Array` variant instead, which
/// advertised `NTScalarArray` for every one-element array field the PVA
/// oracle measures — `aai`/`aao` VAL, `acalcout` AVAL/AA..LL/OAV,
/// `compress` VAL — and made `pvxput` refuse the drive with "Unable to
/// assign string[] with String".
///
/// Both sides of the boundary are here, because a rule that holds at only
/// one of them is the shape that produced the defect.
#[tokio::test]
async fn a_one_element_array_field_is_a_scalar_channel() {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:one",
        Box::new(WaveformRecord::new(1, DbFieldType::Double)),
    )
    .await
    .unwrap();
    db.add_record(
        "TEST:two",
        Box::new(WaveformRecord::new(2, DbFieldType::Double)),
    )
    .await
    .unwrap();

    let one = BridgeChannel::new(db.clone(), b"TEST:one")
        .await
        .expect("new");
    assert_eq!(
        one.nt_type(),
        NtType::Scalar,
        "NELM=1 is one element: `getChannelValueType` never runs arrayOf()"
    );
    match extract_value(&one.get(&empty_request()).await.expect("get")).expect("value") {
        PvField::Scalar(ScalarValue::Double(_)) => {}
        other => panic!("the value must be the scalar the descriptor advertises, got {other:?}"),
    }

    // `putScalar` into the one-element buffer, then read it back through the
    // same scalar leaf.
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(6.25))));
    one.put(&put)
        .await
        .expect("scalar put into an NELM=1 field");
    assert_eq!(
        extract_value(&one.get(&empty_request()).await.expect("get")).expect("value"),
        &PvField::Scalar(ScalarValue::Double(6.25))
    );

    let two = BridgeChannel::new(db.clone(), b"TEST:two")
        .await
        .expect("new");
    assert_eq!(two.nt_type(), NtType::ScalarArray, "NELM=2 keeps arrayOf()");
}

/// The same boundary on an `FTVL=STRING` buffer — the leaf whose mismatch
/// `pvxput` reports as "Unable to assign string[] with String", and the one
/// whose scalar-string PUT has to land in the array destination
/// (`dbAccess.c:1350` takes the array arm for any `special(SPC_DBADDR)`
/// field, whatever the count).
#[tokio::test]
async fn a_one_element_string_array_field_serves_and_takes_a_string_scalar() {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:s1",
        Box::new(WaveformRecord::new(1, DbFieldType::String)),
    )
    .await
    .unwrap();

    let ch = BridgeChannel::new(db.clone(), b"TEST:s1")
        .await
        .expect("new");
    assert_eq!(ch.nt_type(), NtType::Scalar);

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::String("hello".into())),
    ));
    ch.put(&put)
        .await
        .expect("string put into an NELM=1 buffer");

    assert_eq!(
        extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value"),
        &PvField::Scalar(ScalarValue::String("hello".into()))
    );
}

/// `Q:form,"String"` carries pvxs's own `isArray` conjunct
/// (`iocsource.cpp:635`): a one-element `DBF_CHAR` VAL is a `DBR_CHAR`
/// scalar, not a long string.
#[tokio::test]
async fn q_form_string_needs_the_array_half_too() {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "TEST:c1",
        Box::new(WaveformRecord::new(1, DbFieldType::Char)),
    )
    .await
    .unwrap();
    {
        let rec = db.get_record("TEST:c1").expect("record");
        rec.write().set_info("Q:form", "String");
    }

    let ch = BridgeChannel::new(db.clone(), b"TEST:c1")
        .await
        .expect("new");
    assert_eq!(
        ch.nt_type(),
        NtType::Scalar,
        "one element: the long-string branch does not fire"
    );
    match extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value") {
        PvField::Scalar(ScalarValue::Byte(_)) => {}
        other => panic!("expected a `DBR_CHAR` scalar, got {other:?}"),
    }
}

/// The PUT half of the same predicate, against a destination that takes
/// NOTHING but a buffer: `histogram` VAL answers `TypeMismatch` to a scalar,
/// and C never sends it one — `putScalar` is `doDbPut(chan, dbr, &value, 1)`
/// and `dbPut` takes its array arm for every `special(SPC_DBADDR)` field
/// whatever the count (`dbAccess.c:1350`). The channel is a scalar on the
/// wire and a one-element buffer in the record, and the two are one decision.
#[tokio::test]
async fn a_scalar_put_reaches_an_array_only_destination() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:h1", Box::new(HistogramRecord::new(1, 0.0, 10.0)))
        .await
        .unwrap();

    let ch = BridgeChannel::new(db.clone(), b"TEST:h1")
        .await
        .expect("new");
    assert_eq!(ch.nt_type(), NtType::Scalar, "NELM=1 histogram VAL");

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::UInt(7))));
    ch.put(&put)
        .await
        .expect("a scalar PUT must reach the array-only VAL writer");
    assert_eq!(
        extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value"),
        &PvField::Scalar(ScalarValue::UInt(7))
    );
}

/// The boundary the count cannot answer: an `lsi` holding ONE character.
/// C's `cvt_dbaddr` reports `SIZV`, so the channel is never scalar-shaped and
/// the long-string arms stay reachable; this port stores a `String`, whose
/// count is 1, so the declaration has to answer instead. Getting this wrong
/// serves a one-character `lsi` as a `DBR_CHAR` scalar.
#[tokio::test]
async fn a_one_character_long_string_stays_a_long_string() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:ls1", Box::new(LsiRecord::new("")))
        .await
        .unwrap();
    db.put_pv("TEST:ls1", EpicsValue::String("x".into()))
        .await
        .unwrap();

    let ch = BridgeChannel::new(db.clone(), b"TEST:ls1")
        .await
        .expect("new");
    assert_eq!(ch.nt_type(), NtType::LongString);
    assert_eq!(
        extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value"),
        &PvField::Scalar(ScalarValue::String("x".into()))
    );

    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::String("yz".into())),
    ));
    ch.put(&put).await.expect("long-string put");
    assert_eq!(
        extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value"),
        &PvField::Scalar(ScalarValue::String("yz".into()))
    );
}

/// A PVA put to `$` is `dbPut(DBR_CHAR)` of its `int8_t[]`: SIZV bounds it,
/// not `DBR_STRING`'s 40. `softIocPVX` 1.5.3, an lso SIZV 64 given 45
/// characters and the NUL, reads back `{46}[…]` on `$` and the text plain.
#[tokio::test]
async fn a_dollar_put_is_bounded_by_sizv_not_40() {
    let db = Arc::new(PvDatabase::new());
    let mut li = LsiRecord::default();
    li.sizv = 50;
    db.add_record("TEST:li", Box::new(li)).await.unwrap();
    let ch = BridgeChannel::new(db.clone(), b"TEST:li.VAL$")
        .await
        .expect("new");
    let mut image: Vec<u8> = (b'a'..=b'z').chain(b'A'..=b'S').collect();
    image.push(0);
    let mut put = PvStructure::new("epics:nt/NTScalarArray:1.0");
    put.fields.push(("value".into(), chars(&image)));
    ch.put(&put).await.expect("`$` put");
    assert_eq!(
        db.get_pv("TEST:li.LEN").expect("LEN"),
        EpicsValue::ULong(46)
    );
    assert_eq!(
        extract_value(&ch.get(&empty_request()).await.expect("get")).expect("value"),
        &chars(&image)
    );
}

/// Every value a `qsrvSingle` monitor on `name` posts: `put` runs at the
/// first 300 ms without an event, and the next quiet 300 ms ends the watch.
async fn monitor_values(
    db: &Arc<PvDatabase>,
    name: &str,
    put: impl std::future::Future<Output = ()>,
) -> Vec<PvField> {
    use epics_pva_rs::server_native::source::ChannelSource;
    let mut mon = epics_pva_rs::server::PvDatabaseSource::new(db.clone())
        .subscribe(name.as_bytes())
        .await
        .expect("monitor");
    let mut seen = Vec::new();
    let mut put = Some(Box::pin(put));
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(300), mon.recv()).await {
            Ok(Some(PvField::Structure(ev))) => {
                seen.push(extract_value(&ev).expect("value").clone())
            }
            Ok(Some(ev)) => panic!("expected an NT structure, got {ev:?}"),
            Ok(None) => break,
            Err(_) => match put.take() {
                Some(p) => p.await,
                None => break,
            },
        }
    }
    seen
}

/// A plain lso VAL monitor posts the text. `softIocPVX` 1.5.3,
/// `record(lso,"LS64"){field(SIZV,"64")}`, `pvxmonitor LS64.VAL` while
/// `pvxput LS64.VAL abc`: `""` then `"abc"`.
#[tokio::test]
async fn lso_long_string_monitor_posts_the_text() {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(
            r#"record(lso, "LS64") { field(SIZV, "64") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let val = BridgeChannel::new(db.clone(), b"LS64").await.expect("VAL");
    let writer = BridgeChannel::new(db.clone(), b"LS64")
        .await
        .expect("writer");
    let put = async move {
        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields.push((
            "value".into(),
            PvField::Scalar(ScalarValue::String("abc".into())),
        ));
        writer.put(&put).await.expect("put");
    };
    let string = |t: &str| PvField::Scalar(ScalarValue::String(t.into()));
    // The initial event is the server's read; the stream carries none.
    let initial = val.get(&empty_request()).await.expect("get");
    assert_eq!(extract_value(&initial), Some(&string("")));
    assert_eq!(monitor_values(&db, "LS64", put).await, vec![string("abc")]);
}

/// The records the `$` pins below read, as `softIocPVX` 1.5.3 loaded them.
async fn dollar_db() -> Arc<PvDatabase> {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(
            r#"
            record(lso, "LZ:O") { field(SIZV, "64") }
            record(lso, "LZ:F") { field(SIZV, "64") info(Q:form, "String") }
            record(lso, "LZ:S") { field(SIZV, "20") }
            record(lsi, "LZ:J") { field(SIZV, "64") }
            record(waveform, "LZ:W") { field(FTVL, "STRING") field(NELM, "4") }
            record(calcout, "LZ:C") { field(INPA, "LZ:J") }
            "#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn dollar_put(db: &Arc<PvDatabase>, name: &str, value: PvField) {
    let ch = BridgeChannel::new(db.clone(), name.as_bytes())
        .await
        .expect(name);
    let mut put = PvStructure::new("structure");
    put.fields.push(("value".into(), value));
    ch.put(&put).await.expect("put");
}

fn text(t: &str) -> PvField {
    PvField::Scalar(ScalarValue::String(t.into()))
}

fn strings(t: &[&str]) -> PvField {
    epics_bridge_rs::convert::epics_to_pv_field(&EpicsValue::StringArray(
        t.iter().map(|s| (*s).into()).collect::<Vec<_>>().into(),
    ))
}

/// A `$` GET per shape, each the `pvxget -r value` answer of `softIocPVX`
/// 1.5.3 over the same records after `pvxput LZ:O world`, `LZ:F world`,
/// `LZ:S abc` and `LZ:W '["abc","de"]'`.
#[tokio::test]
async fn a_dollar_get_serves_what_softiocpvx_serves() {
    let db = dollar_db().await;
    for (name, t) in [("LZ:O", "world"), ("LZ:F", "world"), ("LZ:S", "abc")] {
        dollar_put(&db, name, text(t)).await;
    }
    dollar_put(&db, "LZ:W", strings(&["abc", "de"])).await;
    let cases = [
        ("LZ:O.VAL$", chars(b"world\0")),
        ("LZ:O.DESC$", chars(b"\0")),
        ("LZ:O.RTYP$", chars(b"lso\0")),
        ("LZ:S.VAL$", chars(b"abc\0")),
        ("LZ:J.VAL$", chars(b"")),
        ("LZ:W.VAL$", chars(b"ab")),
        ("LZ:F.VAL$", text("world")),
        ("LZ:C.INPA$", text("LZ:J NPP NMS")),
    ];
    for (name, want) in cases {
        let ch = BridgeChannel::new(db.clone(), name.as_bytes())
            .await
            .expect(name);
        let got = ch.get(&empty_request()).await.expect("get");
        assert_eq!(extract_value(&got), Some(&want), "{name}");
    }
}

/// A `$` monitor per shape: `softIocPVX` 1.5.3's `pvxmonitor -r value`
/// over `pvxput LZ:O again`, `LZ:W '["xyz"]'`, `LZ:F again`, and
/// `LZ:C.INPA$` over `pvxput LZ:C.INPA LZ:O`.
#[tokio::test]
async fn a_dollar_monitor_serves_what_softiocpvx_serves() {
    let db = dollar_db().await;
    let cases = [
        ("LZ:O", text("again"), chars(b"again\0")),
        ("LZ:W", strings(&["xyz"]), chars(b"x")),
        ("LZ:F", text("again"), text("again")),
    ];
    for (name, second, want) in cases {
        let put = dollar_put(&db, name, second);
        assert_eq!(
            monitor_values(&db, &format!("{name}.VAL$"), put).await,
            vec![want],
            "{name}"
        );
    }
    let put = dollar_put(&db, "LZ:C.INPA", text("LZ:O"));
    assert_eq!(
        monitor_values(&db, "LZ:C.INPA$", put).await,
        vec![text("LZ:O NPP NMS")],
        "LZ:C.INPA$"
    );
}

/// An `int8_t[]` put to a `$` channel is `dbPut(DBR_CHAR)`, its NUL
/// supplied when absent: `softIocPVX` 1.5.3, `pvxput 'LZ:O.VAL$'
/// '[104,105,0]'` reads back `"hi"` and `{3}[104,105,0]`, and `'[106,107]'`
/// reads back `"jk"` and `{3}[106,107,0]`.
#[tokio::test]
async fn a_dollar_char_put_is_a_dbr_char_put() {
    let db = dollar_db().await;
    for (put, t, image) in [(&b"hi\0"[..], "hi", &b"hi\0"[..]), (b"jk", "jk", b"jk\0")] {
        dollar_put(&db, "LZ:O.VAL$", chars(put)).await;
        for (name, want) in [("LZ:O", text(t)), ("LZ:O.VAL$", chars(image))] {
            let ch = BridgeChannel::new(db.clone(), name.as_bytes())
                .await
                .expect(name);
            let got = ch.get(&empty_request()).await.expect("get");
            assert_eq!(extract_value(&got), Some(&want), "{name}");
        }
    }
}

/// An `arr` slice of a `$` channel: C's filter cuts the `field_size` image
/// and `dbGet` then forces a NUL into the last element and cuts at the first
/// (`dbAccess.c:1037-1045`). `softIocPVX` 1.5.3 over
/// `record(stringin,"FS"){field(VAL,"hello")}`, `pvxget -r value`.
#[tokio::test]
async fn a_dollar_arr_slice_serves_what_softiocpvx_serves() {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(
            r#"
            record(stringin, "FS") { field(VAL, "hello") }
            "#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let cases = [
        (r#"FS.VAL$"#, chars(b"hello\0")),
        (r#"FS.VAL${"arr":{"s":1,"e":3}}"#, chars(b"el\0")),
        (r#"FS.VAL${"arr":{"s":-3}}"#, chars(b"\0")),
        (r#"FS.VAL${"arr":{"s":1,"e":-1}}"#, chars(b"ello\0")),
        (r#"FS.VAL${"arr":{"s":3,"e":8}}"#, chars(b"lo\0")),
        (r#"FS.VAL${"arr":{"s":1,"e":3,"i":2}}"#, chars(b"e\0")),
        (r#"FS.VAL${"arr":{"s":2,"e":5}}"#, chars(b"llo\0")),
    ];
    for (name, want) in cases {
        let ch = BridgeChannel::new(db.clone(), name.as_bytes())
            .await
            .expect(name);
        let got = ch.get(&empty_request()).await.expect("get");
        assert_eq!(extract_value(&got), Some(&want), "{name}");
    }
}

/// A one-element `arr` slice is a scalar channel: `softIocPVX` 1.5.3's
/// `pvxget` over `FS` = "hello", `W` = `[1,2,3]` and `LS` = "hey". Its
/// monitor is pinned on `qsrvSingle` (`PvDatabaseSource`).
#[tokio::test]
async fn a_one_element_arr_slice_serves_what_softiocpvx_serves() {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(
            r#"record(stringin, "FS") { field(VAL, "hello") }
            record(waveform, "W") { field(FTVL, "LONG") field(NELM, "5") }
            record(lsi, "LS") { field(SIZV, "20") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    dollar_put(
        &db,
        "W",
        PvField::ScalarArrayTyped(epics_pva_rs::pvdata::TypedScalarArray::Int(
            vec![1, 2, 3].into(),
        )),
    )
    .await;
    dollar_put(&db, "LS", text("hey")).await;
    let cases = [
        (r#"FS.VAL${"arr":{"s":1,"e":1}}"#, ScalarValue::Byte(101)),
        (r#"LS.VAL${"arr":{"s":1,"e":1}}"#, ScalarValue::Byte(101)),
        (r#"W.VAL{"arr":{"s":1,"e":1}}"#, ScalarValue::Int(2)),
    ];
    for (name, want) in cases {
        let ch = BridgeChannel::new(db.clone(), name.as_bytes())
            .await
            .expect(name);
        let epics_pva_rs::pvdata::FieldDesc::Structure { fields, .. } =
            ch.get_field().await.expect("get_field")
        else {
            panic!("{name}: no NT descriptor");
        };
        let desc = fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d);
        assert_eq!(
            desc,
            Some(&epics_pva_rs::pvdata::FieldDesc::Scalar(want.scalar_type())),
            "{name}"
        );
        let got = ch.get(&empty_request()).await.expect("get");
        assert_eq!(extract_value(&got), Some(&PvField::Scalar(want)), "{name}");
    }
}

/// A link's or an attribute's `$` through `arr` is its text re-read at the
/// slice's count, from the start: `softIocPVX` 1.5.3's `pvxget`, `LK.INP`
/// being `FS NPP NMS`.
#[tokio::test]
async fn a_dollar_link_arr_slice_serves_what_softiocpvx_serves() {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(
            r#"record(stringin, "FS") { field(VAL, "hello") }
            record(stringin, "LK") { field(INP, "FS NPP") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let byte = |b| PvField::Scalar(ScalarValue::Byte(b));
    let cases = [
        ("LK.INP$", text("FS NPP NMS")),
        (r#"LK.INP${"arr":{"s":0,"e":1}}"#, text("F")),
        (r#"LK.INP${"arr":{"s":0,"e":2}}"#, text("FS")),
        (r#"LK.INP${"arr":{"s":1,"e":3}}"#, text("FS")),
        (r#"LK.INP${"arr":{"s":1,"e":1}}"#, byte(0)),
        (r#"LK.INP${"arr":{"s":0,"e":5}}"#, text("FS NP")),
        (r#"LK.INP${"arr":{"s":2}}"#, text("FS NPP NMS")),
        ("FS.RTYP$", chars(b"stringin\0")),
        (r#"FS.RTYP${"arr":{"s":1,"e":3}}"#, chars(b"st\0")),
        (r#"FS.RTYP${"arr":{"s":1,"e":1}}"#, byte(0)),
        (r#"FS.RTYP${"arr":{"s":0,"e":0}}"#, byte(0)),
    ];
    for (name, want) in cases {
        let ch = BridgeChannel::new(db.clone(), name.as_bytes())
            .await
            .expect(name);
        let got = ch.get(&empty_request()).await.expect("get");
        assert_eq!(extract_value(&got), Some(&want), "{name}");
    }
}

/// A long string whose failed device read left its NUL past `LEN`
/// (devlsiStream.c): pvxs keeps a SIZV 41 VAL the `DBF_STRING`
/// `cvt_dbaddr` made it, read to the buffer's NUL, and re-views a SIZV 100
/// VAL as its `LEN` chars (`ioc/channel.cpp:58-67`).
#[tokio::test]
async fn a_long_string_get_reads_past_len_only_without_the_re_view() {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(
            r#"record(lsi, "A") { field(SIZV, "41") }
            record(lsi, "B") { field(SIZV, "100") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    for (name, read) in [("A", b"abcdef".to_vec()), ("B", vec![b'x'; 50])] {
        let cell = db.get_record(name).unwrap();
        let mut inst = cell.write();
        let lsi = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<LsiRecord>())
            .unwrap();
        lsi.val.set_text(b"o");
        let sizv = lsi.sizv as usize;
        lsi.val.scan_in_place(sizv, |buf| {
            buf[..read.len()].copy_from_slice(&read);
            buf[read.len()] = 0;
            None
        });
    }
    for (name, want) in [("A", "abcdef"), ("B", "xx")] {
        let ch = BridgeChannel::new(db.clone(), name.as_bytes())
            .await
            .expect(name);
        let got = ch.get(&empty_request()).await.expect("get");
        assert_eq!(extract_value(&got), Some(&text(want)), "pvget {name}");
    }
}
