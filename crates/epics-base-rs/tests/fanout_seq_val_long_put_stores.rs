//! Family A: `fanout.VAL` and `seq.VAL` are `DBF_LONG`, and a `caput X.VAL v`
//! stores `v` verbatim while pp(TRUE) reprocesses (a no-op for a link-less
//! record).
//!
//! Both records declare `field(VAL,DBF_LONG){ pp(TRUE) }`
//! (fanoutRecord.dbd:21-25, seqRecord.dbd:21-25) — the "Used to trigger" field.
//! C `process` (fanoutRecord.c:92-158, seqRecord.c) never reads or writes VAL;
//! it only drives the forward links. So on a bare `record(fanout,"X"){}` a
//! `caput X.VAL 1` reads back 1 with STAT/SEVR NO_ALARM.
//!
//! The port previously declared VAL as `#[field(type = "Enum")] u16`, which
//! routed a `DBR_STRING` put through the enum choice-matcher — a bare "1" was
//! refused as `S_db_badChoice` ("Channel write request failed"), leaving VAL 0.
//! Declaring VAL `DBF_LONG` (i32) routes the string put through the numeric
//! `c_parse::put_string` Long row, which stores the value and range-checks it
//! exactly as C's `epicsParseInt32` does.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::fanout::FanoutRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::types::EpicsValue;

/// `caput REC.FIELD <text>` over the CA put path the oracle drives.
async fn caput(db: &PvDatabase, field: &str, text: &str) -> Result<(), String> {
    db.put_record_field_from_ca("REC", field, EpicsValue::String(text.into()))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The raw stored field value (not the CA-served projection).
async fn stored(db: &PvDatabase, field: &str) -> EpicsValue {
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    inst.record.get_field(field).unwrap()
}

/// (stat, sevr) after the last put/process cycle.
async fn alarm(db: &PvDatabase) -> (u16, AlarmSeverity) {
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

async fn db_with(record: Box<dyn Record>) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db
}

/// Every boundary class the oracle exercises for a `DBF_LONG` VAL put:
/// zero, one, negative-one, type-min (`i32::MIN`), type-max (`i32::MAX`).
/// Each stores verbatim and leaves the bare record at NO_ALARM.
async fn assert_val_put_stores(db: &PvDatabase) {
    for (text, expect) in [
        ("0", 0i32),
        ("1", 1),
        ("-1", -1),
        ("-2147483648", i32::MIN),
        ("2147483647", i32::MAX),
    ] {
        caput(db, "VAL", text)
            .await
            .unwrap_or_else(|e| panic!("caput VAL {text} must be accepted, got {e}"));
        assert_eq!(
            stored(db, "VAL").await,
            EpicsValue::Long(expect),
            "VAL put {text} must store {expect} as DBF_LONG"
        );
        let (stat, sevr) = alarm(db).await;
        assert_eq!(
            sevr,
            AlarmSeverity::NoAlarm,
            "bare record VAL={text}: SEVR NO_ALARM"
        );
        assert_eq!(stat, 0, "bare record VAL={text}: STAT NO_ALARM");
    }
}

#[epics_macros_rs::epics_test]
async fn fanout_val_long_put_stores() {
    let db = db_with(Box::new(FanoutRecord::new())).await;
    assert_val_put_stores(&db).await;
}

#[epics_macros_rs::epics_test]
async fn seq_val_long_put_stores() {
    let db = db_with(Box::new(SeqRecord::new())).await;
    assert_val_put_stores(&db).await;
}
