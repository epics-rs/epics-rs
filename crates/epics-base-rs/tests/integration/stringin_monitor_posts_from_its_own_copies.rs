//! stringin's `monitor()` (`stringinRecord.c:174-188`) against softIoc
//! R7.0.10, three `camonitor -m v|a|l` clients on `SI SI.OVAL SI.SVAL`,
//! driven with `caput SI.PROC 1; SI.PROC 1; SI.MPST Always; SI.PROC 1;
//! SI.MPST "On Change"; SI.APST Always; SI.PROC 1; SI.APST "On Change"; SRC
//! b; SI.PROC 1; SI.SIMM YES; SS x; SI.PROC 1; SS y; SI.PROC 1`. The streams
//! below are softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: an unchanged VAL posts nothing until MPST or APST is
//! Always, which opens its own class alone. OVAL, `monitor()`'s private
//! copy, never posts; nor does SVAL, which the simulated cycle reads SIOL
//! into (softIoc read SVAL `y` at the end).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(stringout, "SS") { field(VAL, "s0") }
record(stringout, "SRC") { field(VAL, "a") }
record(stringin, "SI") {
  field(INP, "SRC")
  field(SIOL, "SS")
}
"#;

const STEPS: [(&str, &str, &str); 15] = [
    ("SI", "PROC", "1"),
    ("SI", "PROC", "1"),
    ("SI", "MPST", "Always"),
    ("SI", "PROC", "1"),
    ("SI", "MPST", "On Change"),
    ("SI", "APST", "Always"),
    ("SI", "PROC", "1"),
    ("SI", "APST", "On Change"),
    ("SRC", "VAL", "b"),
    ("SI", "PROC", "1"),
    ("SI", "SIMM", "YES"),
    ("SS", "VAL", "x"),
    ("SI", "PROC", "1"),
    ("SS", "VAL", "y"),
    ("SI", "PROC", "1"),
];

const VALUE: &str = "\
SI a
SI a
SI b
SI x
SI y
";

const ALARM: &str = "\
SI a
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
            Some(EpicsValue::String(v)) => v.as_str_lossy().into_owned(),
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
async fn stringin_posts_val_alone_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("SI").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [("SI", "VAL"), ("SI.OVAL", "OVAL"), ("SI.SVAL", "SVAL")] {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::String, class.bits())
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
        // A step moves VAL and OVAL once, so the value read now is the one
        // each of its posts carried.
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            for pv in ["SI", "SI.OVAL", "SI.SVAL"] {
                let EpicsValue::String(now) = db.get_pv(pv).unwrap() else {
                    panic!("{pv} is a string");
                };
                *stream =
                    stream.replace(&format!("{pv} @"), &format!("{pv} {}", now.as_str_lossy()));
            }
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
