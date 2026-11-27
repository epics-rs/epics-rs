//! int64out's `monitor()` (`int64outRecord.c:353-377`) against softIoc
//! R7.0.10, three `camonitor -m v|a|l` clients on `QO QO.MLST QO.ALST
//! QO.LALM QO.DRVH`, driven with `caput QO 1; QO 3; QO 4; QO 9; QO 100;
//! QO.PROC 1; QO.HIGH 60; QO.PROC 1; QO.MDEL -1; QO.PROC 1`. The streams below
//! are softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: MDEL and ADEL gate VAL apart; DRVH clamps 100 to 50 before
//! the post; an alarm-only cycle posts VAL with `DBE_ALARM`; MDEL -1 posts an
//! unmoved VAL. Only VAL posts: MLST, ALST and LALM, which the cycle writes,
//! never do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(int64out, "QO") {
  field(MDEL, "2")
  field(ADEL, "5")
  field(DRVH, "50")
  field(DRVL, "-50")
  field(HIGH, "20")
  field(HSV, "MINOR")
}
"#;

const STEPS: [(&str, &str); 10] = [
    ("VAL", "1"),
    ("VAL", "3"),
    ("VAL", "4"),
    ("VAL", "9"),
    ("VAL", "100"),
    ("PROC", "1"),
    ("HIGH", "60"),
    ("PROC", "1"),
    ("MDEL", "-1"),
    ("PROC", "1"),
];

const VALUE: &str = "\
QO 3
QO 9
QO 50 HIGH MINOR
QO 50
";

const ALARM: &str = "\
QO 1
QO 50 HIGH MINOR
QO 50
";

const LOG: &str = "\
QO 9
QO 50 HIGH MINOR
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
            Some(EpicsValue::Int64(v)) => format!("{v}"),
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
async fn int64out_posts_val_alone_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("QO").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("QO", "VAL"),
                ("QO.MLST", "MLST"),
                ("QO.ALST", "ALST"),
                ("QO.LALM", "LALM"),
                ("QO.DRVH", "DRVH"),
            ] {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::Int64, class.bits())
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
        db.put_record_field_from_ca("QO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
