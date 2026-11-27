//! swait's `monitor()` (`swaitRecord.c:614-656`) against the oracle IOC's
//! calc R3-7 swait (`oracleSwaitIoc st-swait.cmd`), three `camonitor -m
//! v|a|l` clients on `SW SW.A SW.LA SW.B SW.LB SW.MLST`, driven with `caput
//! SW.PROC 1; SW.A 3; SW.PROC 1; SW.B 2; SW.PROC 1; SW.PROC 1; SW.A 0; SW.B 0;
//! SW.PROC 1`. The streams below are the IOC's with the timestamps and the
//! connect values cut.
//!
//! The boundaries: A..L are `pp(TRUE)`, so a put posts the input and its
//! cycle posts it again against LA..LL, followed by the LA..LL it advanced;
//! a cycle that moves no input posts none. MLST never posts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(swait, "SW") {
  field(CALC, "A+B")
}
"#;

const STEPS: [(&str, &str); 9] = [
    ("PROC", "1"),
    ("A", "3"),
    ("PROC", "1"),
    ("B", "2"),
    ("PROC", "1"),
    ("PROC", "1"),
    ("A", "0"),
    ("B", "0"),
    ("PROC", "1"),
];

const VALUE: &str = "\
SW.A 3
SW 3
SW.A 3
SW.LA 3
SW.B 2
SW 5
SW.B 2
SW.LB 2
SW.A 0
SW 2
SW.A 0
SW.LA 0
SW.B 0
SW 0
SW.B 0
SW.LB 0
";

const ALARM: &str = "\
SW 0
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
async fn swait_posts_inputs_from_its_own_la_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("SW").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("SW", "VAL"),
                ("SW.A", "A"),
                ("SW.LA", "LA"),
                ("SW.B", "B"),
                ("SW.LB", "LB"),
                ("SW.MLST", "MLST"),
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
    for (field, value) in STEPS {
        db.put_record_field_from_ca("SW", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
