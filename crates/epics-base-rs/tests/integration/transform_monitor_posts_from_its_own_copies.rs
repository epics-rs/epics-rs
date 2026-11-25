//! transform's `monitor()` (`transformRecord.c:786-810`) against oracleStdIoc,
//! three `camonitor -m v|a|l` clients on `T T.A T.B T.C T.LA`, driven with
//! `caput T.PROC 1; T.PROC 1; X 1; T.PROC 1; T.B 5; T.PROC 1; T.PROC 1; X 4;
//! T.PROC 1; T.PROC 1; X 0; T.PROC 1; T.A 7; T.PROC 1`. The streams below are
//! the oracle's with the timestamps and the connect values cut.
//!
//! The boundaries: the first cycle posts every A..P (`firstCalcPosted`), later
//! ones only those that moved off LA..LP; `monitor_mask` is overwritten with
//! `DBE_VALUE|DBE_LOG`, so nothing reaches `DBE_ALARM` and VAL never posts,
//! while each input post carries the record's alarm; a put to B posts from the
//! put and keeps B out of the next cycle's calculation; a put to A, which has
//! a link, is refused. The oracle reads LA 0, LB 0, LC 1 after the steps.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "X") {
  field(HIGH, "3")
  field(HSV, "MINOR")
}
record(transform, "T") {
  field(INPA, "X MS")
  field(CLCB, "A*2")
  field(CLCC, "B+1")
}
"#;

const STEPS: [(&str, &str, &str); 14] = [
    ("T", "PROC", "1"),
    ("T", "PROC", "1"),
    ("X", "VAL", "1"),
    ("T", "PROC", "1"),
    ("T", "B", "5"),
    ("T", "PROC", "1"),
    ("T", "PROC", "1"),
    ("X", "VAL", "4"),
    ("T", "PROC", "1"),
    ("T", "PROC", "1"),
    ("X", "VAL", "0"),
    ("T", "PROC", "1"),
    ("T", "A", "7"),
    ("T", "PROC", "1"),
];

const VALUE: &str = "\
T.A 0 LINK INVALID
T.B 0 LINK INVALID
T.C 1 LINK INVALID
T.A 1
T.B 2
T.C 3
T.B 5
T.B 5
T.C 6
T.B 2
T.C 3
T.A 4 LINK MINOR
T.B 8 LINK MINOR
T.C 9 LINK MINOR
T.A 0
T.B 0
T.C 1
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
            Some(EpicsValue::Double(v)) => format!("{v}"),
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
async fn transform_posts_inputs_from_its_own_la_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("T").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("T", "VAL"),
                ("T.A", "A"),
                ("T.B", "B"),
                ("T.C", "C"),
                ("T.LA", "LA"),
            ] {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::Double, class.bits())
                        .unwrap(),
                );
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
        let put = db
            .put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await;
        // The oracle's "Channel write request failed".
        assert_eq!(
            put.is_err(),
            field == "A",
            "{record}.{field} {value}: {put:?}"
        );
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(by_pv(&got[0]), by_pv(VALUE), "DBE_VALUE");
    assert_eq!(got[1], "", "DBE_ALARM");
    assert_eq!(by_pv(&got[2]), by_pv(LOG), "DBE_LOG");
    // The oracle's `caget` after the steps.
    for (pv, v) in [("T.LA", 0.0), ("T.LB", 0.0), ("T.LC", 1.0)] {
        assert_eq!(db.get_pv(pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
}

/// Each PV's lines in the order it received them.
fn by_pv(stream: &str) -> std::collections::BTreeMap<&str, Vec<&str>> {
    let mut m = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for line in stream.lines() {
        m.entry(line.split(' ').next().unwrap())
            .or_default()
            .push(line);
    }
    m
}
