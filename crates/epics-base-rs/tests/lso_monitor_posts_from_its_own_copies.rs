//! lso's `special()` and `monitor()` LEN and VAL posts (`lsoRecord.c:239,
//! 246-265`) against softIoc R7.0.10, three `camonitor -m v|a|l` clients on
//! `L L.LEN L.OLEN`, driven with `caput L.PROC 1; I.PROC 1; L ab; L.PROC 1; I.PROC 1; I.PROC 1; L ab; L abc;
//! I.PROC 1; L.MPST 1; L.PROC 1; I.MPST 1; I.PROC 1; L.APST 1; L.PROC 1; L x;
//! I.PROC 1`, lso L feeding lsi I through INP. The streams below are softIoc's
//! with the timestamps and the connect values cut.
//!
//! The boundaries: a VAL put posts LEN from `special()` under the alarm it
//! found, and its cycle posts LEN again only when it differs from OLEN; an
//! unchanged put posts no VAL; MPST/APST Always add `DBE_VALUE`/`DBE_LOG`
//! alone; OLEN never posts.
//!
//! Compared per PV: C posts LEN before VAL (`monitor()`'s order), the port's
//! `monitor_cycle` posts VAL's deadband class before its walk's LEN mark.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(lso, "L") {
  field(SIZV, "20")
}
record(lsi, "I") {
  field(SIZV, "20")
  field(INP, "L")
}
"#;

const STEPS: [(&str, &str, &str); 17] = [
    ("L", "PROC", "1"),
    ("I", "PROC", "1"),
    ("L", "VAL", "ab"),
    ("L", "PROC", "1"),
    ("I", "PROC", "1"),
    ("I", "PROC", "1"),
    ("L", "VAL", "ab"),
    ("L", "VAL", "abc"),
    ("I", "PROC", "1"),
    ("L", "MPST", "1"),
    ("L", "PROC", "1"),
    ("I", "MPST", "1"),
    ("I", "PROC", "1"),
    ("L", "APST", "1"),
    ("L", "PROC", "1"),
    ("L", "VAL", "x"),
    ("I", "PROC", "1"),
];

const VALUE: &str = "\
L.LEN 3 UDF INVALID
L.LEN 3
L ab
L.LEN 3
L.LEN 4
L.LEN 4
L abc
L abc
L abc
L.LEN 2
L.LEN 2
L x
";

const ALARM: &str = "\
L ab
";

const LOG: &str = "\
L.LEN 3 UDF INVALID
L.LEN 3
L ab
L.LEN 3
L.LEN 4
L.LEN 4
L abc
L abc
L.LEN 2
L.LEN 2
L x
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
            Some(EpicsValue::String(v)) => v.to_string(),
            Some(EpicsValue::Long(v)) => format!("{v}"),
            Some(EpicsValue::ULong(v)) => format!("{v}"),
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
async fn lso_posts_len_and_val_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("L").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("L", "VAL", DbFieldType::String),
                ("L.LEN", "LEN", DbFieldType::Long),
                ("L.OLEN", "OLEN", DbFieldType::Long),
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
        // A step posts L's VAL at most once, so the string read now is the
        // one its post carried.
        let now: String = match cell.read().record.get_field("VAL") {
            Some(EpicsValue::CharArray(v)) => v
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| char::from(c))
                .collect(),
            other => panic!("L: {other:?}"),
        };
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            *stream = stream.replace("L @", &format!("L {now}"));
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(by_pv(&got[0]), by_pv(VALUE), "DBE_VALUE");
    assert_eq!(by_pv(&got[1]), by_pv(ALARM), "DBE_ALARM");
    assert_eq!(by_pv(&got[2]), by_pv(LOG), "DBE_LOG");
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
