//! permissive's `monitor()` (`permissiveRecord.c:90-117`) against softIoc
//! R7.0.10, three `camonitor -m v|a|l` clients on `PM PM.WFLG PM.OVAL
//! PM.OFLG`, driven with `caput PM 1; PM.WFLG 1; PM.PROC 1; PM 1; PM.WFLG 0;
//! PM.PROC 1; PM 0`. The streams below are softIoc's with the timestamps and
//! the connect values cut.
//!
//! The boundaries: VAL posts on `oval != val` and WFLG on `oflg != wflg`, so
//! a WFLG put posts once from `dbPut` and again from the next cycle; the alarm
//! mask rides VAL's post only. OVAL and OFLG never post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(permissive, "PM") {
  field(LABL, "x")
}
"#;

const STEPS: [(&str, &str); 7] = [
    ("VAL", "1"),
    ("WFLG", "1"),
    ("PROC", "1"),
    ("VAL", "1"),
    ("WFLG", "0"),
    ("PROC", "1"),
    ("VAL", "0"),
];

const VALUE: &str = "\
PM 1
PM.WFLG 1
PM.WFLG 1
PM.WFLG 0
PM.WFLG 0
PM 0
";

const ALARM: &str = "\
PM 1
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
            Some(EpicsValue::UShort(v)) => format!("{v}"),
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
async fn permissive_posts_val_and_wflg_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("PM").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("PM", "VAL"),
                ("PM.WFLG", "WFLG"),
                ("PM.OVAL", "OVAL"),
                ("PM.OFLG", "OFLG"),
            ] {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::UShort, class.bits())
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
        db.put_record_field_from_ca("PM", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
