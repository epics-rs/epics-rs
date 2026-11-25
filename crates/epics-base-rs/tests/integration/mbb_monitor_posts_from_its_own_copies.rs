//! mbbi's and mbbo's `monitor()` (`mbbiRecord.c:349-367`,
//! `mbboRecord.c:394-416`) against softIoc R7.0.10, three `camonitor -m
//! v|a|l` clients on `MI MI.RVAL MI.ORAW MI.MLST MR MR.RVAL MO MO.RVAL MO.RBV
//! MO.ORAW MO.ORBV`, driven with `caput SRC 1; MI.PROC 1; MR.PROC 1; MI.RVAL
//! 5; MI.PROC 1; MR.RVAL 9; MR.PROC 1; SRC 2; MI.PROC 1; MR.PROC 1; MO 1; MO
//! 2; MO.RVAL 7; MO.PROC 1; MO 1`. The streams below are softIoc's with the
//! timestamps and the connect values cut.
//!
//! The boundary: RVAL posts when `oraw != rval`, against the record's own
//! `oraw`, whether or not VAL posts. The Soft Channel MI posts an RVAL put
//! again from its cycle; the Raw Soft Channel MR posts RVAL 2 on a cycle that
//! leaves VAL at its illegal state; MO's cycle converts a put RVAL 7 back to
//! the 0 `oraw` holds and posts nothing. The trackers never post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "SRC") { }
record(ao, "SINK") { }
record(mbbi, "MI") {
  field(ZRST, "Z")
  field(ONST, "O")
  field(TWST, "T")
  field(INP, "SRC")
  field(ONSV, "MINOR")
}
record(mbbi, "MR") {
  field(ZRST, "Z")
  field(ONST, "O")
  field(TWST, "T")
  field(DTYP, "Raw Soft Channel")
  field(INP, "SRC")
}
record(mbbo, "MO") {
  field(ZRST, "Z")
  field(ONST, "O")
  field(TWST, "T")
  field(DTYP, "Raw Soft Channel")
  field(OUT, "SINK")
  field(ONSV, "MINOR")
}
"#;

const STEPS: [(&str, &str, &str); 15] = [
    ("SRC", "VAL", "1"),
    ("MI", "PROC", "1"),
    ("MR", "PROC", "1"),
    ("MI", "RVAL", "5"),
    ("MI", "PROC", "1"),
    ("MR", "RVAL", "9"),
    ("MR", "PROC", "1"),
    ("SRC", "VAL", "2"),
    ("MI", "PROC", "1"),
    ("MR", "PROC", "1"),
    ("MO", "VAL", "1"),
    ("MO", "VAL", "2"),
    ("MO", "RVAL", "7"),
    ("MO", "PROC", "1"),
    ("MO", "VAL", "1"),
];

const VALUE: &str = "\
MI O STATE MINOR
MR Illegal Value
MR.RVAL 1
MI.RVAL 5 STATE MINOR
MI.RVAL 5 STATE MINOR
MR.RVAL 9
MI T
MR.RVAL 2
MO O STATE MINOR
MO T
MO.RVAL 7
MO O STATE MINOR
";

const ALARM: &str = "\
MI O STATE MINOR
MR Illegal Value
MR.RVAL 1
MI T
MO O STATE MINOR
MO T
MO O STATE MINOR
";

const LOG: &str = VALUE;

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
            Some(EpicsValue::Enum(v)) => ["Z", "O", "T"]
                .get(*v as usize)
                .map_or("Illegal Value", |s| s)
                .to_string(),
            Some(EpicsValue::Long(v)) => format!("{v}"),
            Some(EpicsValue::ULong(v)) => format!("{v}"),
            Some(EpicsValue::UShort(v)) => format!("{v}"),
            // Read at delivery: the test fills it in after the step.
            None => "@".to_string(),
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

/// The three streams of `records`' posts over `STEPS`.
async fn streams_of(records: &[(&'static str, &[&str])]) -> Vec<String> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    let mut sid = 0;
    for &(record, fields) in records {
        let cell = db.get_record(record).unwrap();
        let mut inst = cell.write();
        for (class, stream) in classes.iter().zip(&streams) {
            for &field in fields {
                let (pv, ty): (&'static str, _) = match field {
                    "VAL" => (record, DbFieldType::Enum),
                    _ => (format!("{record}.{field}").leak(), DbFieldType::Long),
                };
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
        // A step moves VAL once, so the value read now is the one each of
        // its posts carried.
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            for pv in ["MI", "MR", "MO"] {
                let EpicsValue::Enum(now) = db.get_pv(pv).unwrap() else {
                    panic!("{pv} is an enum");
                };
                let label = ["Z", "O", "T"]
                    .get(now as usize)
                    .map_or("Illegal Value", |s| s);
                *stream = stream.replace(&format!("{pv} @"), &format!("{pv} {label}"));
            }
        }
    }
    streams.iter().map(|s| s.lock().unwrap().clone()).collect()
}

/// softIoc's `stream`, cut to the lines of `records`' PVs.
fn lines_of(stream: &str, records: &[(&'static str, &[&str])]) -> String {
    stream
        .lines()
        .filter(|l| {
            records
                .iter()
                .any(|(r, _)| l.split([' ', '.']).next() == Some(r))
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

async fn assert_streams(records: &[(&'static str, &[&str])]) {
    let got = streams_of(records).await;
    assert_eq!(got[0], lines_of(VALUE, records), "DBE_VALUE");
    assert_eq!(got[1], lines_of(ALARM, records), "DBE_ALARM");
    assert_eq!(got[2], lines_of(LOG, records), "DBE_LOG");
}

#[epics_macros_rs::epics_test]
async fn mbbi_posts_rval_from_its_own_oraw_as_softioc_does() {
    assert_streams(&[
        ("MI", &["VAL", "RVAL", "ORAW", "MLST"]),
        ("MR", &["VAL", "RVAL"]),
    ])
    .await;
}

#[epics_macros_rs::epics_test]
async fn mbbo_posts_rval_from_its_own_oraw_as_softioc_does() {
    assert_streams(&[("MO", &["VAL", "RVAL", "RBV", "ORAW", "ORBV"])]).await;
}
