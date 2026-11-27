//! stringout's `monitor()` (`stringoutRecord.c:212-227`) against softIoc
//! R7.0.10, three `camonitor -m v|a|l` clients on `SO SO.OVAL`, driven with
//! `caput SO b; SO b; SO.MPST Always; SO b; SO.APST Always; SO.MPST "On
//! Change"; SO.PROC 1; SO c`. The streams below are softIoc's with the
//! timestamps and the connect values cut.
//!
//! The boundaries: an unchanged VAL posts nothing until MPST or APST is
//! Always, which opens its own class alone. OVAL, which `monitor()` copies
//! VAL into, never posts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(stringout, "SO") {
  field(VAL, "a")
}
"#;

const STEPS: [(&str, &str); 8] = [
    ("VAL", "b"),
    ("VAL", "b"),
    ("MPST", "Always"),
    ("VAL", "b"),
    ("APST", "Always"),
    ("MPST", "On Change"),
    ("PROC", "1"),
    ("VAL", "c"),
];

const VALUE: &str = "\
SO b
SO b
SO c
";

const ALARM: &str = "\
SO b
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
async fn stringout_posts_val_alone_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("SO").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [("SO", "VAL"), ("SO.OVAL", "OVAL")] {
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
    for (field, value) in STEPS {
        db.put_record_field_from_ca("SO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // A step moves VAL and OVAL once, so the value read now is the one
        // each of its posts carried.
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            for pv in ["SO", "SO.OVAL"] {
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
