//! End-to-end wiring for the `Soft Timestamp` built-in device support
//! (base `devTimestamp.c`): an `ai`/`stringin` record with
//! `DTYP="Soft Timestamp"` must route through the pre-registered builtin
//! dynamic factory, get the device wired, and have its VAL written from the
//! record's resolved time stamp — not silently left untouched (the bug
//! before `is_soft_dtyp` stopped classifying "Soft Timestamp" as a soft
//! channel).

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Read a record's `VAL` field after a process cycle.
async fn read_val(db: &epics_base_rs::server::database::PvDatabase, name: &str) -> EpicsValue {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    inst.record.get_field("VAL").expect("VAL field present")
}

/// `devTimestampAI`: a processed `ai` with `DTYP="Soft Timestamp"` and
/// `TSE=0` (wall clock) writes `VAL = secPastEpoch + frac`. In 2026 that is
/// well over 1e9 seconds past the 1990 EPICS epoch, so a `VAL > 1.0e9`
/// proves the device ran (the pre-fix silent no-op left `VAL == 0.0`).
#[epics_macros_rs::epics_test]
async fn soft_timestamp_ai_writes_current_seconds_past_epoch() {
    let db_content = r#"
record(ai, "TS_AI") {
    field(DTYP, "Soft Timestamp")
    field(TSE, "0")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_AI", &mut visited, 0)
        .await
        .unwrap();

    match read_val(&db, "TS_AI").await {
        EpicsValue::Double(v) => assert!(
            v > 1.0e9,
            "ai Soft Timestamp VAL must be the current secPastEpoch (>1e9), got {v}"
        ),
        other => panic!("expected Double VAL, got {other:?}"),
    }
}

/// `devTimestampSI`: a processed `stringin` with `DTYP="Soft Timestamp"`
/// and an INST_IO `INP` strftime format writes the formatted resolved time
/// into VAL. With `INP="@%Y"` (wall clock, TSE=0) that is the current
/// four-digit year — proving the instio `INP` format string reached the
/// device (a missing INP would format an empty string).
#[epics_macros_rs::epics_test]
async fn soft_timestamp_stringin_formats_current_year() {
    let db_content = r#"
record(stringin, "TS_SI") {
    field(DTYP, "Soft Timestamp")
    field(INP, "@%Y")
    field(TSE, "0")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_SI", &mut visited, 0)
        .await
        .unwrap();

    match read_val(&db, "TS_SI").await {
        EpicsValue::String(s) => {
            let s = s.as_str_lossy();
            let year: i32 = s
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("VAL must be a 4-digit year, got {s:?}"));
            assert!(
                year >= 2024,
                "stringin Soft Timestamp VAL must be the current year, got {year}"
            );
        }
        other => panic!("expected String VAL, got {other:?}"),
    }
}
