//! Regression tests for parity-review findings 04 (database) and 05
//! (record infrastructure): fanout / dfanout / seq SELM link selection,
//! event-record routing, and UDF-on-NaN.
#![allow(clippy::all)]

use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::event::EventRecord;
use epics_base_rs::server::records::fanout::FanoutRecord;
use epics_base_rs::types::EpicsValue;

/// 04-H-1 — the fanout record has a `LNK0` field (C `NLINKS = 16`).
#[test]
fn fanout_has_lnk0_field() {
    let mut rec = FanoutRecord::new();
    rec.put_field("LNK0", EpicsValue::String("TARGET0".into()))
        .unwrap();
    assert_eq!(
        rec.get_field("LNK0"),
        Some(EpicsValue::String("TARGET0".into())),
        "fanout must expose LNK0 (C fanoutRecord LNK0..LNKF)"
    );
    // LNKF still present — 16 links total.
    rec.put_field("LNKF", EpicsValue::String("TARGETF".into()))
        .unwrap();
    assert_eq!(
        rec.get_field("LNKF"),
        Some(EpicsValue::String("TARGETF".into()))
    );
}

/// 04-H-1 — `SELM=All` fans out through `LNK0`; the primary first
/// slot is no longer silently dropped.
#[tokio::test]
async fn fanout_selm_all_processes_lnk0_target() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("DOWNSTREAM", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let mut fan = FanoutRecord::new();
    // LNK0 carries the primary fan-out target, PP so it processes.
    fan.put_field("LNK0", EpicsValue::String("DOWNSTREAM PP".into()))
        .unwrap();
    fan.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    db.add_record("FAN", Box::new(fan)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("FAN", &mut visited, 0)
        .await
        .unwrap();
    // The LNK0 target must have been processed (it appears in the
    // current process chain's visited set).
    assert!(
        visited.contains("DOWNSTREAM"),
        "fanout SELM=All must process its LNK0 target: {visited:?}"
    );
}

/// 04-H-2 / 07-C-2 — dfanout `SELM=Specified` is 1-based: `SELN=1`
/// drives OUTA, `SELN=0` drives nothing.
#[tokio::test]
async fn dfanout_specified_is_one_based() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("OUT_A", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("OUT_B", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut df = DfanoutRecord::new(7.0);
    df.put_field("OUTA", EpicsValue::String("OUT_A".into()))
        .unwrap();
    df.put_field("OUTB", EpicsValue::String("OUT_B".into()))
        .unwrap();
    df.put_field("SELM", EpicsValue::Short(1)).unwrap(); // Specified
    df.put_field("SELN", EpicsValue::Short(1)).unwrap(); // 1-based → OUTA
    db.add_record("DF", Box::new(df)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("DF", &mut visited, 0)
        .await
        .unwrap();

    // SELN=1 drives OUTA only.
    let a = db.get_record("OUT_A").await.unwrap();
    let b = db.get_record("OUT_B").await.unwrap();
    let a_val = a.read().await.record.val();
    let b_val = b.read().await.record.val();
    assert_eq!(
        a_val,
        Some(EpicsValue::Double(7.0)),
        "dfanout SELN=1 must drive OUTA (1-based)"
    );
    assert_eq!(
        b_val,
        Some(EpicsValue::Double(0.0)),
        "dfanout SELN=1 must NOT drive OUTB"
    );
}

/// 04-M-5 / 07-C-2 — an out-of-range `SELN` on a dfanout raises
/// SOFT_ALARM / INVALID_ALARM.
#[tokio::test]
async fn dfanout_specified_out_of_range_raises_invalid() {
    use epics_base_rs::server::record::AlarmSeverity;
    let db = Arc::new(PvDatabase::new());
    let mut df = DfanoutRecord::new(1.0);
    df.put_field("SELM", EpicsValue::Short(1)).unwrap(); // Specified
    df.put_field("SELN", EpicsValue::Short(99)).unwrap(); // > 16 → INVALID
    db.add_record("DF_BAD", Box::new(df)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("DF_BAD", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("DF_BAD").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "out-of-range SELN must raise INVALID alarm"
    );
    assert_eq!(
        inst.common.stat, 15, /* SOFT_ALARM */
        "out-of-range SELN must raise SOFT_ALARM status"
    );
}

/// 05-H-2 / 07-H-1 — the event record's `VAL` is a string event name.
#[test]
fn event_record_val_is_string() {
    let rec = EventRecord::new("myEvent");
    assert_eq!(
        rec.get_field("VAL"),
        Some(EpicsValue::String("myEvent".into())),
        "event record VAL must be a string event name (DBF_STRING)"
    );
}

/// 05-H-1 / 07-H-2 — `post_event_named` routes by event number:
/// a record with `EVNT=5` fires only on event 5, not event 7.
#[tokio::test]
async fn event_scan_routes_by_event_number() {
    use epics_base_rs::server::record::ScanType;

    let db = Arc::new(PvDatabase::new());
    // Two Event-scanned records on different event numbers.
    db.add_record("REC5", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("REC7", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    // Configure SCAN=Event and EVNT.
    {
        let r = db.get_record("REC5").await.unwrap();
        let mut inst = r.write().await;
        inst.common.scan = ScanType::Event;
        inst.common.evnt = "5".to_string();
    }
    {
        let r = db.get_record("REC7").await.unwrap();
        let mut inst = r.write().await;
        inst.common.scan = ScanType::Event;
        inst.common.evnt = "7".to_string();
    }
    // Register the Event scan-index entries.
    db.update_scan_index("REC5", ScanType::Passive, ScanType::Event, 0, 0)
        .await;
    db.update_scan_index("REC7", ScanType::Passive, ScanType::Event, 0, 0)
        .await;

    // Post event 5 — only REC5 should process. We detect processing
    // via the record's timestamp moving off the UNIX_EPOCH default.
    db.post_event_named("5").await;

    let r5 = db.get_record("REC5").await.unwrap();
    let r7 = db.get_record("REC7").await.unwrap();
    let t5 = r5.read().await.common.time;
    let t7 = r7.read().await.common.time;
    assert_ne!(
        t5,
        std::time::SystemTime::UNIX_EPOCH,
        "REC5 (EVNT=5) must process on event 5"
    );
    assert_eq!(
        t7,
        std::time::SystemTime::UNIX_EPOCH,
        "REC7 (EVNT=7) must NOT process on event 5"
    );
}

/// 06-H-2 — UDF stays true when the processed value is NaN, so the
/// record raises UDF_ALARM instead of reporting a garbage value.
#[tokio::test]
async fn udf_stays_true_on_nan_value() {
    use epics_base_rs::server::record::AlarmSeverity;

    let db = Arc::new(PvDatabase::new());
    // An ai record whose VAL is NaN — the framework must NOT clear
    // UDF, and must raise UDF_ALARM at the default UDFS=INVALID.
    db.add_record("NAN_REC", Box::new(AiRecord::new(f64::NAN)))
        .await
        .unwrap();

    db.process_record("NAN_REC").await.unwrap();

    let rec = db.get_record("NAN_REC").await.unwrap();
    let inst = rec.read().await;
    assert!(
        inst.common.udf,
        "UDF must stay true when VAL is NaN (C aiRecord.c:285)"
    );
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "NaN VAL must raise UDF_ALARM at UDFS=INVALID"
    );
}

/// 06-H-2 — UDF IS cleared when the processed value is a defined
/// (non-NaN) number — the fix must not over-suppress UDF clearing.
#[tokio::test]
async fn udf_cleared_on_defined_value() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("OK_REC", Box::new(AiRecord::new(3.14)))
        .await
        .unwrap();
    db.process_record("OK_REC").await.unwrap();
    let rec = db.get_record("OK_REC").await.unwrap();
    assert!(
        !rec.read().await.common.udf,
        "UDF must clear when VAL is a defined value"
    );
}
