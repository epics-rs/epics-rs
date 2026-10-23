//! acalcout's `monitor()` (`aCalcoutRecord.c:955-1047`) against oracleStdIoc,
//! three `camonitor -m v|a|l` clients on `AC AC.A AC.B AC.OVAL`, driven with
//! `caput AC.PROC 1; X 1; AC.PROC 1; AC.PROC 1; AC.B 5; AC.PROC 1; AC.PROC 1;
//! X 4; AC.PROC 1; AC.PROC 1; X 0; AC.PROC 1; AC.A 7; AC.PROC 1`. The streams
//! below are the oracle's with the timestamps and the connect values cut.
//!
//! The boundaries: `process()` copies A..L into PA..PL before `fetch_values`
//! (`:393-395`), so an input posts only when this cycle's fetch moved it — a
//! put to A between cycles posts from the put and again when the fetch
//! restores it — and every A..L posts on a cycle whose alarm moved. The
//! oracle reads PA 0, PB 5 after the steps.
//!
//! Compared per PV: C posts VAL, then A..L, the port's `monitor_cycle` in its
//! subscriber walk's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "X") {}
record(acalcout, "AC") {
  field(NELM, "4")
  field(INPA, "X")
  field(CALC, "A+B")
  field(HIGH, "8")
  field(HSV, "MINOR")
}
"#;

const STEPS: [(&str, &str, &str); 14] = [
    ("AC", "PROC", "1"),
    ("X", "VAL", "1"),
    ("AC", "PROC", "1"),
    ("AC", "PROC", "1"),
    ("AC", "B", "5"),
    ("AC", "PROC", "1"),
    ("AC", "PROC", "1"),
    ("X", "VAL", "4"),
    ("AC", "PROC", "1"),
    ("AC", "PROC", "1"),
    ("X", "VAL", "0"),
    ("AC", "PROC", "1"),
    ("AC", "A", "7"),
    ("AC", "PROC", "1"),
];

const VALUE: &str = "\
AC.A 0
AC.B 0
AC 1
AC.A 1
AC.B 5
AC 6
AC 9 HIGH MINOR
AC.A 4 HIGH MINOR
AC.B 5 HIGH MINOR
AC 5
AC.A 0
AC.B 5
AC.A 7
AC.A 0
";

const ALARM: &str = "\
AC 0
AC.A 0
AC.B 0
AC 9 HIGH MINOR
AC.A 4 HIGH MINOR
AC.B 5 HIGH MINOR
AC 5
AC.A 0
AC.B 5
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
async fn acalcout_posts_inputs_from_its_own_pa_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("AC").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("AC", "VAL"),
                ("AC.A", "A"),
                ("AC.B", "B"),
                ("AC.OVAL", "OVAL"),
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
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(by_pv(&got[0]), by_pv(VALUE), "DBE_VALUE");
    assert_eq!(by_pv(&got[1]), by_pv(ALARM), "DBE_ALARM");
    assert_eq!(by_pv(&got[2]), by_pv(LOG), "DBE_LOG");
    // The oracle's `caget` after the steps.
    for (pv, v) in [("AC.PA", 0.0), ("AC.PB", 5.0)] {
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
