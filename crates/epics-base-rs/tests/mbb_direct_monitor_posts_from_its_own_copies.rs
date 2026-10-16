//! mbbiDirect's and mbboDirect's `monitor()` (`mbbiDirectRecord.c:208-239`,
//! `mbboDirectRecord.c:305-340`) against softIoc R7.0.10, three `camonitor -m
//! v|a|l` clients on `DI DI.RVAL DI.ORAW DI.MLST DI.B0 DI.B1 DR DR.RVAL DR.B0
//! DO DO.RVAL DO.RBV DO.ORAW DO.ORBV DO.OBIT DO.B0 DO.B1`, driven with `caput
//! SRC 1; DI.PROC 1; DR.PROC 1; DI.RVAL 5; DI.PROC 1; DR.RVAL 9; DR.PROC 1;
//! SRC 3; DI.PROC 1; DR.PROC 1; DO 1; DO 3; DO.PROC 1; DO 2`. The streams
//! below are softIoc's with the timestamps and the connect values cut.
//!
//! The boundary: RVAL posts when `oraw != rval`, against the record's own
//! `oraw`, whether or not VAL posts. The Soft Channel DI posts an RVAL put
//! again from its cycle; the Raw Soft Channel DR's cycle reads back the RVAL 1
//! `oraw` holds after a put of 9 and posts nothing. The trackers never post.
//!
//! Compared per PV: the order across fields (C posts mbbiDirect's bits
//! before VAL, mbboDirect's RVAL after the bits) is the framework's.

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
record(mbbiDirect, "DI") {
  field(INP, "SRC")
}
record(mbbiDirect, "DR") {
  field(DTYP, "Raw Soft Channel")
  field(INP, "SRC")
  field(NOBT, "4")
}
record(mbboDirect, "DO") {
  field(DTYP, "Raw Soft Channel")
  field(OUT, "SINK")
  field(NOBT, "4")
}
"#;

const STEPS: [(&str, &str, &str); 14] = [
    ("SRC", "VAL", "1"),
    ("DI", "PROC", "1"),
    ("DR", "PROC", "1"),
    ("DI", "RVAL", "5"),
    ("DI", "PROC", "1"),
    ("DR", "RVAL", "9"),
    ("DR", "PROC", "1"),
    ("SRC", "VAL", "3"),
    ("DI", "PROC", "1"),
    ("DR", "PROC", "1"),
    ("DO", "VAL", "1"),
    ("DO", "VAL", "3"),
    ("DO", "PROC", "1"),
    ("DO", "VAL", "2"),
];

const VALUE: &str = "\
DI.B0 1
DI 1
DR.B0 1
DR 1
DR.RVAL 1
DI.RVAL 5
DI.RVAL 5
DR.RVAL 9
DI.B1 1
DI 3
DR 3
DR.RVAL 3
DO 1
DO.B0 1
DO.B1 0
DO.RVAL 1
DO 3
DO.B1 1
DO.RVAL 3
DO 2
DO.B0 0
DO.RVAL 2
";

const ALARM: &str = "\
DI.B0 1
DI.B1 0
DI 1
DR.B0 1
DR 1
DR.RVAL 1
DO 1
DO.B0 1
DO.B1 0
DO.RVAL 1
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
            Some(EpicsValue::UChar(v)) => format!("{v}"),
            Some(EpicsValue::Short(v)) => format!("{v}"),
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
        // A step moves each field once, so the value read now is the one
        // each of its posts carried.
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            while let Some(at) = stream.find(" @") {
                let pv_start = stream[..at].rfind('\n').map_or(0, |n| n + 1);
                let pv = stream[pv_start..at].to_string();
                let now = db.get_pv(&pv).unwrap().to_f64().unwrap();
                stream.replace_range(at + 1..at + 2, &format!("{now}"));
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
        .collect::<String>()
}

/// `stream` with each PV's lines kept in order and the PVs apart.
fn by_pv(stream: &str) -> String {
    let mut lines: Vec<&str> = stream.lines().collect();
    lines.sort_by_key(|l| l.split(' ').next().unwrap_or_default());
    lines.iter().map(|l| format!("{l}\n")).collect()
}

async fn assert_streams(records: &[(&'static str, &[&str])]) {
    let got: Vec<String> = streams_of(records).await.iter().map(|s| by_pv(s)).collect();
    assert_eq!(got[0], by_pv(&lines_of(VALUE, records)), "DBE_VALUE");
    assert_eq!(got[1], by_pv(&lines_of(ALARM, records)), "DBE_ALARM");
    assert_eq!(got[2], by_pv(&lines_of(LOG, records)), "DBE_LOG");
}

#[epics_macros_rs::epics_test]
async fn mbbi_direct_posts_rval_from_its_own_oraw_as_softioc_does() {
    assert_streams(&[
        ("DI", &["VAL", "RVAL", "ORAW", "MLST", "B0", "B1"]),
        ("DR", &["VAL", "RVAL", "B0"]),
    ])
    .await;
}

#[epics_macros_rs::epics_test]
async fn mbbo_direct_posts_rval_from_its_own_oraw_as_softioc_does() {
    assert_streams(&[(
        "DO",
        &["VAL", "RVAL", "RBV", "ORAW", "ORBV", "OBIT", "B0", "B1"],
    )])
    .await;
}
