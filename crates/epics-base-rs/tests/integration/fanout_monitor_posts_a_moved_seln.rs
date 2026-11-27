//! fanout's posts (`fanoutRecord.c:146-152`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `FO FO.SELN`, driven with `caput FO.PROC 1;
//! caput FO:S 2; caput FO.PROC 1; caput FO.PROC 1; caput FO.SELN 5;
//! caput FO.PROC 1; caput FO:S 20; caput FO.PROC 1; caput FO.PROC 1;
//! caput FO:S 1; caput FO.PROC 1; caput FO.VAL 3; caput FO.SELN 1;
//! caput FO.PROC 1`. The streams below are softIoc's with the timestamps and
//! the connect values cut.
//!
//! C posts VAL with the alarm events alone, and SELN when the SELL read moved
//! it off its process-entry value (`oldn`), with `events | DBE_VALUE | DBE_LOG`:
//! a SELN put the next SELL read restores posts again, and an unmoved SELN on
//! an alarm transition posts nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(longout, "FO:S") {
}
record(fanout, "FO") {
  field(SELM, "Specified")
  field(SELL, "FO:S")
}
"#;

const STEPS: [(&str, &str, &str); 14] = [
    ("FO", "PROC", "1"),
    ("FO:S", "VAL", "2"),
    ("FO", "PROC", "1"),
    ("FO", "PROC", "1"),
    ("FO", "SELN", "5"),
    ("FO", "PROC", "1"),
    ("FO:S", "VAL", "20"),
    ("FO", "PROC", "1"),
    ("FO", "PROC", "1"),
    ("FO:S", "VAL", "1"),
    ("FO", "PROC", "1"),
    ("FO", "VAL", "3"),
    ("FO", "SELN", "1"),
    ("FO", "PROC", "1"),
];

const VALUE: &str = "\
FO.SELN 0
FO.SELN 2
FO.SELN 5
FO.SELN 2
FO.SELN 20 SOFT INVALID
FO.SELN 1
FO.SELN 1
";

const ALARM: &str = "\
FO 0
FO.SELN 0
FO 0 SOFT INVALID
FO.SELN 20 SOFT INVALID
FO 0
FO.SELN 1
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
            Some(EpicsValue::Long(v)) => format!("{v}"),
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
async fn fanout_posts_val_on_alarms_and_a_moved_seln_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("FO").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("FO", "VAL", DbFieldType::Long),
                ("FO.SELN", "SELN", DbFieldType::UShort),
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
    for (rec, field, value) in STEPS {
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
