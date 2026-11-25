//! event's `monitor()` (`eventRecord.c:157-165`) against softIoc R7.0.10,
//! three `camonitor -m v|a|l` clients on `EV EV.SVAL EV.SIMM EV.UDF`, driven
//! with `caput EV.PROC 1; EV b; EV.PROC 1; EV.SVAL x; EV.SIMM YES; EV.PROC 1;
//! EV.SIMM NO; EV.PROC 1`. The streams below are softIoc's with the
//! timestamps and the connect values cut.
//!
//! The boundaries: VAL posts on every cycle, moved or not, with `DBE_VALUE`
//! and never `DBE_LOG`; a simulated cycle copies SVAL into VAL and posts VAL
//! alone. SVAL and SIMM post only from their own puts; UDF never posts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(event, "EV") {
  field(VAL, "a")
}
"#;

const STEPS: [(&str, &str); 8] = [
    ("PROC", "1"),
    ("VAL", "b"),
    ("PROC", "1"),
    ("SVAL", "x"),
    ("SIMM", "YES"),
    ("PROC", "1"),
    ("SIMM", "NO"),
    ("PROC", "1"),
];

const VALUE: &str = "\
EV a
EV b
EV b
EV.SVAL x
EV.SIMM YES
EV x
EV.SIMM NO
EV x
";

const ALARM: &str = "\
EV a
";

const LOG: &str = "\
EV b
EV.SVAL x
EV.SIMM YES
EV.SIMM NO
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
            Some(EpicsValue::String(v)) => v.as_str_lossy().into_owned(),
            Some(EpicsValue::Enum(v)) => ["NO", "YES"][usize::from(*v)].to_string(),
            Some(EpicsValue::Char(v)) => format!("{v}"),
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
async fn event_posts_val_alone_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("EV").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("EV", "VAL", DbFieldType::String),
                ("EV.SVAL", "SVAL", DbFieldType::String),
                ("EV.SIMM", "SIMM", DbFieldType::Enum),
                ("EV.UDF", "UDF", DbFieldType::Char),
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
    for (field, value) in STEPS {
        db.put_record_field_from_ca("EV", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // A step moves VAL or SVAL once, so the value read now is the one
        // each of its posts carried.
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            for pv in ["EV", "EV.SVAL"] {
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
