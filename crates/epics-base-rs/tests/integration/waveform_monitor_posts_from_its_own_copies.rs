//! waveform's `process()` NORD post and `monitor()` (`waveformRecord.c:147-148,
//! 293-325`) against softIoc R7.0.10, three `camonitor -m v|a|l` clients on `W
//! W.NORD W.HASH W.MPST`, driven with `caput W.PROC 1; caput -a W 2 1 2;
//! W.PROC 1; caput -a W 3 1 2 3; W.PROC 1; W.PROC 1; W.MPST 1; W.PROC 1;
//! W.PROC 1; caput -a W 3 1 2 3; W.PROC 1; caput -a W 1 5; W.PROC 1; W.APST 1;
//! W.PROC 1; caput -a W 1 6`. The streams below are softIoc's with the
//! timestamps and the connect values cut.
//!
//! The boundaries: a VAL put that changes NORD posts it from `put_array_info`
//! and its cycle does not post it again; MPST/APST Always post VAL every
//! cycle; On Change posts VAL and HASH only when the content hash moves.
//!
//! Compared per PV: C posts HASH before VAL (`waveformRecord.c:319, 324`),
//! the port's `monitor_cycle` after its subscriber walk.

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
"#;

enum Step {
    Field(&'static str, &'static str),
    Array(&'static [f64]),
}
use Step::*;

const STEPS: [Step; 16] = [
    Field("PROC", "1"),
    Array(&[1.0, 2.0]),
    Field("PROC", "1"),
    Array(&[1.0, 2.0, 3.0]),
    Field("PROC", "1"),
    Field("PROC", "1"),
    Field("MPST", "1"),
    Field("PROC", "1"),
    Field("PROC", "1"),
    Array(&[1.0, 2.0, 3.0]),
    Field("PROC", "1"),
    Array(&[5.0]),
    Field("PROC", "1"),
    Field("APST", "1"),
    Field("PROC", "1"),
    Array(&[6.0]),
];

const VALUE: &str = "\
W 
W.NORD 2
W 2 1 2
W 2 1 2
W.NORD 3
W 3 1 2 3
W 3 1 2 3
W 3 1 2 3
W.MPST On Change
W.HASH 1.48088e+09
W 3 1 2 3
W.NORD 1
W.HASH 4.15933e+09
W 5
W.HASH 4.15933e+09
W 6
";

const ALARM: &str = "\
W 
";

const LOG: &str = "\
W 
W.NORD 2
W 2 1 2
W 2 1 2
W.NORD 3
W 3 1 2 3
W 3 1 2 3
W 3 1 2 3
W.MPST On Change
W 3 1 2 3
W 3 1 2 3
W 3 1 2 3
W 3 1 2 3
W.NORD 1
W 5
W 5
W 6
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
            Some(EpicsValue::Enum(v)) => ["Always", "On Change"][*v as usize].to_string(),
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
async fn waveform_posts_nord_and_hash_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("W").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("W", "VAL", DbFieldType::Double),
                ("W.NORD", "NORD", DbFieldType::Long),
                ("W.HASH", "HASH", DbFieldType::Long),
                ("W.MPST", "MPST", DbFieldType::Enum),
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
        let (field, value) = match step {
            Field(f, v) => (f, EpicsValue::String(v.into())),
            Array(a) => ("VAL", EpicsValue::DoubleArray(a.to_vec().into())),
        };
        db.put_record_field_from_ca("W", field, value)
            .await
            .unwrap();
        // A step posts VAL at most once, so the array read now is the one its
        // post carried.
        let now = array(&db.get_pv("W").unwrap());
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            *stream = stream.replace("W @", &format!("W {now}"));
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
        panic!("W: {v:?}");
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
