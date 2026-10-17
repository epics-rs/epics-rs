//! scalcout's `monitor()` keeps its own copies of OSV and OVAL, POSV and POVL
//! (`sCalcoutRecord.c:847-851`, `:866-869`), and posts OVAL only when it
//! differs from POVL, with `monitor_mask | DBE_VALUE | DBE_LOG`.
//!
//! Against oracleStdIoc, three `camonitor -m v|a|l` clients on `S S.OVAL`,
//! driven with `caput S.PROC 1; S.POVL 7; S.PROC 1; S.A 9; S.PROC 1;
//! S.PROC 1; S.OVAL 3; S.PROC 1`, then `E.PROC 1; T.PROC 1` and the `caget`s
//! at the end. The streams are the oracle's with the timestamps and the
//! connect values cut. The boundaries: a POVL put makes the next cycle post
//! the OVAL it did not change, an OVAL put is posted by the put and NOT again
//! by the cycle that sets back the value POVL holds, and the alarm cycle's
//! OVAL reaches a DBE_ALARM client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(scalcout, "S") {
  field(CALC, "A")
  field(HIGH, "8")
  field(HSV, "MINOR")
}
record(scalcout, "E") { field(CALC, "$S(AA,'%*4c%d')/10.") }
record(scalcout, "T") {
  field(CALC, "AA")
  field(AA, "Stable")
}
"#;

const STEPS: [(&str, &str, &str); 10] = [
    ("S", "PROC", "1"),
    ("S", "POVL", "7"),
    ("S", "PROC", "1"),
    ("S", "A", "9"),
    ("S", "PROC", "1"),
    ("S", "PROC", "1"),
    ("S", "OVAL", "3"),
    ("S", "PROC", "1"),
    ("E", "PROC", "1"),
    ("T", "PROC", "1"),
];

const VALUE: &str = "\
S.OVAL 0
S 9 HIGH MINOR
S.OVAL 9 HIGH MINOR
S.OVAL 3 HIGH MINOR
";

const ALARM: &str = "\
S 0
S 9 HIGH MINOR
S.OVAL 9 HIGH MINOR
";

const LOG: &str = VALUE;

/// The oracle's `caget` after the steps.
const READS: [(&str, &str); 12] = [
    ("S.OVAL", "9"),
    ("S.POVL", "9"),
    ("S.POSV", "9"),
    ("E.SVAL", "***ERROR***"),
    ("E.OSV", "***ERROR***"),
    ("E.POSV", "***ERROR***"),
    ("E.OVAL", "-1"),
    ("E.POVL", "-1"),
    ("T.SVAL", "Stable"),
    ("T.OSV", "Stable"),
    ("T.POSV", "Stable"),
    ("T.POVL", "0"),
];

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
async fn scalcout_keeps_povl_and_posv_and_posts_oval_from_povl() {
    let (db, _) = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("S").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [("S", "VAL"), ("S.OVAL", "OVAL")] {
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
    for (pv, want) in READS {
        let got = db
            .get_pv(pv)
            .unwrap_or_else(|e| panic!("{pv}: {e}"))
            .to_string();
        assert_eq!(got, want, "{pv}");
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
