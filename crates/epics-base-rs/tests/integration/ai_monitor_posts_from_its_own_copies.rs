//! ai's `monitor()` (`aiRecord.c:448-467`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `AI AI.RVAL AI.ORAW AI.MLST AI.ALST
//! AI.LALM`, a Raw Soft Channel ai reading `SRC`, driven with `caput SRC 1;
//! AI.PROC 1; SRC 2; AI.PROC 1; SRC 4; AI.PROC 1; SRC 200; AI.PROC 1;
//! AI.HIGH 500; AI.PROC 1; AI.PROC 1; SRC 202; AI.PROC 1`. The streams below
//! are softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: a VAL inside MDEL posts nothing to the value class and
//! RVAL waits with it (`oraw` is not advanced); an MDEL crossing posts both;
//! an alarm-only cycle posts VAL with `DBE_ALARM` and an unchanged RVAL not
//! at all; ORAW, MLST, ALST and LALM, which `monitor()` writes, never post.
//! Nor does AFVL, which `checkAlarms` moves under AFTC (softIoc: AFVL read
//! 3.09447 after `caput SRC 1; AF.PROC 1; SRC 20; AF.PROC 1; AF.PROC 1` and
//! posted to no class). Nor does INIT, which `process()` clears
//! (`aiRecord.c:170`) and a LINR put sets again (`:187`): softIoc read 1 at
//! connect and 0 after `caput SRC 1; AI.PROC 1; AI.LINR LINEAR; AI.PROC 1`,
//! with no event in any class.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "SRC") { field(VAL, "0") }
record(ai, "AI") {
  field(DTYP, "Raw Soft Channel")
  field(INP, "SRC")
  field(LINR, "LINEAR")
  field(ESLO, "0.5")
  field(MDEL, "1")
  field(ADEL, "3")
  field(HIGH, "50")
  field(HSV, "MINOR")
}
"#;

const STEPS: [(&str, &str, &str); 13] = [
    ("SRC", "VAL", "1"),
    ("AI", "PROC", "1"),
    ("SRC", "VAL", "2"),
    ("AI", "PROC", "1"),
    ("SRC", "VAL", "4"),
    ("AI", "PROC", "1"),
    ("SRC", "VAL", "200"),
    ("AI", "PROC", "1"),
    ("AI", "HIGH", "500"),
    ("AI", "PROC", "1"),
    ("AI", "PROC", "1"),
    ("SRC", "VAL", "202"),
    ("AI", "PROC", "1"),
];

const VALUE: &str = "\
AI 2
AI.RVAL 4
AI 100 HIGH MINOR
AI.RVAL 200 HIGH MINOR
";

const ALARM: &str = "\
AI 0.5
AI.RVAL 1
AI 100 HIGH MINOR
AI.RVAL 200 HIGH MINOR
AI 100
";

const LOG: &str = "\
AI 100 HIGH MINOR
AI.RVAL 200 HIGH MINOR
";

/// Appends each event it passes, as `camonitor` prints it without the time.
struct Line {
    pv: &'static str,
    stream: Arc<Mutex<String>>,
}

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let e = &event.event;
        let value = match e.value().as_value() {
            Some(EpicsValue::Double(v)) => format!("{v}"),
            Some(EpicsValue::Long(v)) => format!("{v}"),
            other => panic!("{}: {other:?}", self.pv),
        };
        let mut line = format!("{} {value}", self.pv);
        if e.sevr() != 0 {
            line += &format!(
                " {} {}",
                alarm_condition_string(e.stat()),
                MENU_ALARM_SEVR[e.sevr() as usize]
            );
        }
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(&line);
        stream.push('\n');
        Some(event)
    }
}

#[epics_macros_rs::epics_test]
async fn ai_posts_val_and_rval_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("AI").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("AI", "VAL", DbFieldType::Double),
                ("AI.RVAL", "RVAL", DbFieldType::Long),
                ("AI.ORAW", "ORAW", DbFieldType::Long),
                ("AI.MLST", "MLST", DbFieldType::Double),
                ("AI.ALST", "ALST", DbFieldType::Double),
                ("AI.LALM", "LALM", DbFieldType::Double),
            ] {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                sid += 1;
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv,
                        stream: stream.clone(),
                    }),
                ));
            }
        }
    }
    for (record, field, value) in STEPS {
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}

const AF_DB: &str = r#"
record(ao, "SRC") { field(VAL, "0") }
record(ai, "AF") {
  field(DTYP, "Soft Channel")
  field(INP, "SRC")
  field(AFTC, "10")
  field(HIGH, "5")
  field(HSV, "MINOR")
}
"#;

#[epics_macros_rs::epics_test]
async fn a_moved_afvl_posts_nothing() {
    let (db, _) = IocBuilder::new()
        .db_string(AF_DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let mut afvl = DbSubscription::subscribe(&db, "AF.AFVL").await.unwrap();
    while afvl.try_recv_event().is_ok() {}
    for (record, field, value) in [
        ("SRC", "VAL", "1"),
        ("AF", "PROC", "1"),
        ("SRC", "VAL", "20"),
        ("AF", "PROC", "1"),
        ("AF", "PROC", "1"),
    ] {
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let moved = db.get_pv("AF.AFVL").unwrap().to_f64().unwrap();
    assert_ne!(moved, 0.0, "AFVL moved");
    assert!(afvl.try_recv_event().is_err(), "AFVL posts nothing");
}

#[epics_macros_rs::epics_test]
async fn a_cleared_init_posts_nothing() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let mut init = DbSubscription::subscribe(&db, "AI.INIT").await.unwrap();
    while init.try_recv_event().is_ok() {}
    for (record, field, value) in [
        ("SRC", "VAL", "1"),
        ("AI", "PROC", "1"),
        ("AI", "LINR", "LINEAR"),
        ("AI", "PROC", "1"),
    ] {
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    assert_eq!(db.get_pv("AI.INIT").unwrap().to_f64(), Some(0.0));
    assert!(init.try_recv_event().is_err(), "INIT posts nothing");
}
