//! longout's `monitor()` (`longoutRecord.c:371-395`) against softIoc
//! R7.0.10, three `camonitor -m v|a|l` clients on `LO LO.PVAL LO.MLST LO.ALST
//! LO.LALM LO.DRVH`, driven with `caput LO 1; LO 3; LO 4; LO 9; LO 100;
//! LO.PROC 1; LO.HIGH 60; LO.PROC 1; LO.MDEL -1; LO.PROC 1`. The streams below
//! are softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: MDEL and ADEL gate VAL apart; DRVH clamps 100 to 50 before
//! the post; an alarm-only cycle posts VAL with `DBE_ALARM`; MDEL -1 posts an
//! unmoved VAL. Only VAL posts: PVAL, MLST, ALST and LALM, which the cycle
//! writes, never do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(longout, "LO") {
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
LO 3
LO 9
LO 50 HIGH MINOR
LO 50
";

const ALARM: &str = "\
LO 1
LO 50 HIGH MINOR
LO 50
";

const LOG: &str = "\
LO 9
LO 50 HIGH MINOR
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
async fn longout_posts_val_alone_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("LO").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("LO", "VAL"),
                ("LO.PVAL", "PVAL"),
                ("LO.MLST", "MLST"),
                ("LO.ALST", "ALST"),
                ("LO.LALM", "LALM"),
                ("LO.DRVH", "DRVH"),
            ] {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::Long, class.bits())
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
        db.put_record_field_from_ca("LO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
