//! An sseq written by another record's DB link, against oracleStdIoc
//! (calc R2-x, x86-64 softIoc R7.0.10) under `camonitor -m va`.
//!
//! `record(ao,"SRC"){field(OUT,"SQ.DO1") field(PREC,"2")}` and
//! `record(sseq,"SQ"){field(PREC,"2")}`, then `caput SRC 5`, `caput SRC 6`:
//!
//! ```text
//! SQ.STR1  5.00 UDF INVALID
//! SQ.DO1   5    UDF INVALID
//! SQ.STR1  6.00 UDF INVALID
//! SQ.DO1   6    UDF INVALID
//! ```
//!
//! `dbPutLink` is a `dbPut`, so `special()` re-derives and posts `STRn` just
//! as on a client put; SQ never processes, so every post keeps its init alarm.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "SRC") { field(OUT, "SQ.DO1") field(PREC, "2") }
record(sseq, "SQ") { field(PREC, "2") }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn subscribe(db: &PvDatabase, field: &str, sid: u32, ftype: DbFieldType) -> EventReader {
    let inst = db.get_record("SQ").unwrap();
    let mut g = inst.write();
    g.add_subscriber(
        field,
        sid,
        ftype,
        (EventMask::VALUE | EventMask::ALARM).bits(),
    )
    .unwrap()
}

/// (value, status, severity) of every event queued so far.
fn drain(rx: &mut EventReader) -> Vec<(EpicsValue, u16, u16)> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        let s = &e.snapshot;
        out.push((s.value.clone(), s.alarm.status, s.alarm.severity));
    }
    out
}

const UDF: u16 = 17;
const INVALID: u16 = 3;

#[epics_macros_rs::epics_test]
async fn db_link_put_into_do_posts_the_derived_str() {
    let db = build().await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let mut do_rx = subscribe(&db, "DO1", 1, DbFieldType::Double);
    let mut str_rx = subscribe(&db, "STR1", 2, DbFieldType::String);
    drain(&mut do_rx);
    drain(&mut str_rx);
    for v in [5.0, 6.0] {
        db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(v))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            drain(&mut str_rx),
            [(EpicsValue::String(format!("{v:.2}").into()), UDF, INVALID)],
            "STR1 after SRC={v}"
        );
        assert_eq!(
            drain(&mut do_rx),
            [(EpicsValue::Double(v), UDF, INVALID)],
            "DO1 after SRC={v}"
        );
    }
}

/// oracleStdIoc, `DOL1` a DB link to an ao at 5 and `DOL2` the constant 7,
/// then `caput SQ.PROC 1` twice:
///
/// ```text
/// SQ.DO1   5    UDF INVALID
/// SQ.STR1  5.00 UDF INVALID
/// SQ.VAL   0              (alarm cleared)
/// ```
///
/// `processCallback` posts the link read as it lands (sseqRecord.c:668-681),
/// before `asyncFinish`'s `recGblResetAlarms`, so the first cycle's posts carry
/// the born UDF alarm; the second cycle moves nothing and posts no DO/STR.
#[epics_macros_rs::epics_test]
async fn first_cycle_link_read_posts_carry_the_born_udf_alarm() {
    const DB: &str = r#"
record(ao, "SRC") { field(VAL, "5") field(PREC, "2") }
record(sseq, "SQ") { field(PREC, "2") field(DOL1, "SRC") field(DOL2, "7") }
"#;
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let fields = [
        ("DO1", DbFieldType::Double),
        ("STR1", DbFieldType::String),
        ("DO2", DbFieldType::Double),
        ("STR2", DbFieldType::String),
        ("VAL", DbFieldType::Long),
    ];
    let mut rxs: Vec<EventReader> = fields
        .iter()
        .enumerate()
        .map(|(i, (f, t))| subscribe(&db, f, i as u32 + 1, *t))
        .collect();
    for rx in &mut rxs {
        drain(rx);
    }
    let want_first = [
        vec![(EpicsValue::Double(5.0), UDF, INVALID)],
        vec![(EpicsValue::String("5.00".into()), UDF, INVALID)],
        vec![],
        vec![],
        vec![(EpicsValue::Long(0), 0, 0)],
    ];
    let want_second = [
        vec![],
        vec![],
        vec![],
        vec![],
        vec![(EpicsValue::Long(0), 0, 0)],
    ];
    for (pass, want) in [want_first, want_second].iter().enumerate() {
        db.put_record_field_from_ca("SQ", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
        for (i, (f, _)) in fields.iter().enumerate() {
            assert_eq!(drain(&mut rxs[i]), want[i], "pass {pass}: SQ.{f}");
        }
    }
}

/// oracleStdIoc, step 1 reading `DOL1` from an ao at 5 and step 2 writing its
/// constant 7 back into `SQ2.DO1` through `LNK2`, `caput SQ2.PROC 1`:
///
/// ```text
/// SQ2.DO1  5  UDF INVALID
/// SQ2.DO1  7  UDF INVALID
/// ```
///
/// Each post carries the value at its `db_post_events` — the link read's, then
/// the `LNK2` put's — not the value the cycle ends with.
#[epics_macros_rs::epics_test]
async fn each_do_post_carries_the_value_it_was_made_with() {
    const DB: &str = r#"
record(ao, "SRC") { field(VAL, "5") }
record(sseq, "SQ2") { field(PREC, "2") field(DOL1, "SRC") field(DOL2, "7") field(LNK2, "SQ2.DO1") }
"#;
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let inst = db.get_record("SQ2").unwrap();
    let mut rx = inst
        .write()
        .add_subscriber(
            "DO1",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .unwrap();
    drain(&mut rx);
    db.put_record_field_from_ca("SQ2", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        drain(&mut rx),
        [
            (EpicsValue::Double(5.0), UDF, INVALID),
            (EpicsValue::Double(7.0), UDF, INVALID)
        ]
    );
}
