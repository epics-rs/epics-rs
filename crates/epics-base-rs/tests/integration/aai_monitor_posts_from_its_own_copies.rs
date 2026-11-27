//! aai's `process()` NORD post and `monitor()` (`aaiRecord.c:177-178,
//! 314-346`) against softIoc R7.0.10: a Soft Channel aai reading waveform W
//! through INP, three `camonitor -m v|a|l` clients on `A A.NORD A.HASH`,
//! driven with `caput A.PROC 1; caput -a W 2 1 2; A.PROC 1; A.PROC 1; caput -a
//! W 2 1 3; A.PROC 1; A.MPST 1; A.PROC 1; A.PROC 1; caput -a W 3 1 2 3;
//! A.PROC 1; caput -a A 1 7; A.PROC 1; A.PROC 1`. The streams below are
//! softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: a read that moves NORD posts it from the cycle, one that
//! leaves it posts nothing; a VAL put posts NORD from `put_array_info` and its
//! cycle, reading INP back, posts it again; On Change posts VAL and HASH only
//! when the content hash moves, so the put's own cycle, restoring the old
//! content, posts no VAL.
//!
//! Compared per PV: C posts HASH before VAL (`aaiRecord.c:340, 345`), the
//! port's `monitor_cycle` after its subscriber walk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(waveform, "W") {
  field(FTVL, "DOUBLE")
  field(NELM, "4")
}
record(aai, "A") {
  field(FTVL, "DOUBLE")
  field(NELM, "4")
  field(INP, "W")
}
"#;

enum Step {
    Proc,
    Mpst,
    Put(&'static str, &'static [f64]),
}
use Step::*;

const STEPS: [Step; 14] = [
    Proc,
    Put("W", &[1.0, 2.0]),
    Proc,
    Proc,
    Put("W", &[1.0, 3.0]),
    Proc,
    Mpst,
    Proc,
    Proc,
    Put("W", &[1.0, 2.0, 3.0]),
    Proc,
    Put("A", &[7.0]),
    Proc,
    Proc,
];

const VALUE: &str = "\
A 
A.NORD 2
A 2 1 2
A 2 1 2
A 2 1 3
A.HASH 2.72176e+09
A 2 1 3
A.NORD 3
A.HASH 1.48088e+09
A 3 1 2 3
A.NORD 1
A.NORD 3
";

const ALARM: &str = "\
A 
";

const LOG: &str = "\
A 
A.NORD 2
A 2 1 2
A 2 1 2
A 2 1 3
A 2 1 3
A 2 1 3
A.NORD 3
A 3 1 2 3
A.NORD 1
A.NORD 3
A 3 1 2 3
A 3 1 2 3
A 3 1 2 3
";

/// `%g`, as `camonitor` prints a ULONG.
fn g(v: f64) -> String {
    if v.abs() < 1e6 {
        return format!("{v}");
    }
    let s = format!("{v:.5e}");
    let (m, e) = s.split_once('e').unwrap();
    let m = m.trim_end_matches('0').trim_end_matches('.');
    let e: i32 = e.parse().unwrap();
    format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
}

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
            Some(v @ EpicsValue::DoubleArray(_)) => array(v),
            // Read at delivery: the test fills it in after the step.
            None => "@".to_string(),
            Some(EpicsValue::Double(v)) => format!("{v}"),
            Some(EpicsValue::ULong(v)) => g(f64::from(*v)),
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
async fn aai_posts_nord_and_hash_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("A").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("A", "VAL", DbFieldType::Double),
                ("A.NORD", "NORD", DbFieldType::Long),
                ("A.HASH", "HASH", DbFieldType::Long),
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
    for step in STEPS {
        let (record, field, value) = match step {
            Proc => ("A", "PROC", EpicsValue::String("1".into())),
            Mpst => ("A", "MPST", EpicsValue::String("1".into())),
            Put(record, a) => (record, "VAL", EpicsValue::DoubleArray(a.to_vec().into())),
        };
        db.put_record_field_from_ca(record, field, value)
            .await
            .unwrap();
        // A step posts A's VAL at most once, so the array read now is the
        // one its post carried.
        let now = array(&db.get_pv("A").unwrap());
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            *stream = stream.replace("A @", &format!("A {now}"));
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

/// The array as `camonitor` prints it: the length, then each element; one
/// element alone, as a scalar; nothing at all for an empty one.
fn array(v: &EpicsValue) -> String {
    let EpicsValue::DoubleArray(v) = v else {
        panic!("A: {v:?}");
    };
    match v.len() {
        0 => return String::new(),
        1 => return v[0].to_string(),
        _ => {}
    }
    std::iter::once(v.len().to_string())
        .chain(v.iter().map(f64::to_string))
        .collect::<Vec<_>>()
        .join(" ")
}
