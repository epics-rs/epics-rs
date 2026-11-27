//! scalcout's `monitor()` (`sCalcoutRecord.c:820-870`) against oracleStdIoc,
//! three `camonitor -m v|a|l` clients on `SC SC.A SC.B SC.AA SC.SVAL SC.PA`,
//! driven with `caput SC.PROC 1; X 1; SC.PROC 1; SC.PROC 1; SC.B 5; SC.PROC 1;
//! SI x; SC.PROC 1; SC.PROC 1; X 4; SC.PROC 1; SC.PROC 1; X 0; SC.PROC 1`. The
//! streams below are the oracle's with the timestamps and the connect values
//! cut.
//!
//! The boundaries: `process()` copies A..L into PA..PL and AA..LL into
//! PAA..PLL before the fetch, so an input posts only when this cycle's fetch
//! moved it, and every A..L posts on a cycle whose alarm moved; a put to B
//! posts from the put alone; SVAL posts against PSVL; PA..PL never post. The
//! oracle reads PA 4, PB 5 after the steps.
//!
//! Compared per PV: C posts VAL, SVAL, A..L, AA..LL in that order, the port's
//! `monitor_cycle` in its subscriber walk's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "X") {}
record(stringin, "SI") {}
record(scalcout, "SC") {
  field(INPA, "X")
  field(INAA, "SI")
  field(CALC, "A+B")
  field(HIGH, "8")
  field(HSV, "MINOR")
}
"#;

const STEPS: [(&str, &str, &str); 14] = [
    ("SC", "PROC", "1"),
    ("X", "VAL", "1"),
    ("SC", "PROC", "1"),
    ("SC", "PROC", "1"),
    ("SC", "B", "5"),
    ("SC", "PROC", "1"),
    ("SI", "VAL", "x"),
    ("SC", "PROC", "1"),
    ("SC", "PROC", "1"),
    ("X", "VAL", "4"),
    ("SC", "PROC", "1"),
    ("SC", "PROC", "1"),
    ("X", "VAL", "0"),
    ("SC", "PROC", "1"),
];

const VALUE: &str = "\
SC.SVAL 0
SC.A 0
SC.B 0
SC 1
SC.SVAL 1
SC.A 1
SC.B 5
SC 6
SC.SVAL 6
SC.AA x
SC 9 HIGH MINOR
SC.SVAL 9 HIGH MINOR
SC.A 4 HIGH MINOR
SC.B 5 HIGH MINOR
SC 5
SC.SVAL 5
SC.A 0
SC.B 5
";

const ALARM: &str = "\
SC 0
SC.SVAL 0
SC.A 0
SC.B 0
SC 9 HIGH MINOR
SC.SVAL 9 HIGH MINOR
SC.A 4 HIGH MINOR
SC.B 5 HIGH MINOR
SC 5
SC.SVAL 5
SC.A 0
SC.B 5
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
            Some(EpicsValue::String(v)) => v.to_string(),
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

#[epics_macros_rs::epics_test]
async fn scalcout_posts_inputs_from_its_own_pa_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("SC").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("SC", "VAL", DbFieldType::Double),
                ("SC.A", "A", DbFieldType::Double),
                ("SC.B", "B", DbFieldType::Double),
                ("SC.AA", "AA", DbFieldType::String),
                ("SC.SVAL", "SVAL", DbFieldType::String),
                ("SC.PA", "PA", DbFieldType::Double),
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
        // A step posts each string at most once, so the one read now is the
        // one its post carried.
        for (pv, field) in [("SC.SVAL", "SVAL"), ("SC.AA", "AA")] {
            let now = match cell.read().record.get_field(field) {
                Some(EpicsValue::String(v)) => v.to_string(),
                other => panic!("{pv}: {other:?}"),
            };
            for stream in &streams {
                let mut stream = stream.lock().unwrap();
                *stream = stream.replace(&format!("{pv} @"), &format!("{pv} {now}"));
            }
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(by_pv(&got[0]), by_pv(VALUE), "DBE_VALUE");
    assert_eq!(by_pv(&got[1]), by_pv(ALARM), "DBE_ALARM");
    assert_eq!(by_pv(&got[2]), by_pv(LOG), "DBE_LOG");
    // The oracle's `caget` after the steps.
    for (pv, v) in [("SC.PA", 4.0), ("SC.PB", 5.0)] {
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
