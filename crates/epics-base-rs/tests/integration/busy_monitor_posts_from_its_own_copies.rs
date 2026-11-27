//! busy's `monitor()` (`busyRecord.c:357-383`) against the oracle IOC's
//! busy R1-7 (`oracleBusyIoc st-busy.cmd`), three `camonitor -m v|a|l`
//! clients on `BY BY.RVAL BY.RBV BY.ORAW BY.ORBV BY.MLST`, driven with
//! `caput BY 1; BY 1; BY 0; BY.PROC 1; BY.RVAL 7; BY.PROC 1`. The streams
//! below are the IOC's with the timestamps and the connect values cut.
//!
//! The boundaries: VAL posts on `mlst != val`, so a repeated value or a
//! reprocess posts nothing; RVAL posts against `oraw`, so the cycle after a
//! RVAL put that converts back to the value `oraw` holds posts nothing. RBV,
//! ORAW, ORBV and MLST never post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(busy, "BY") {
  field(MASK, "5")
}
"#;

const STEPS: [(&str, &str); 6] = [
    ("VAL", "1"),
    ("VAL", "1"),
    ("VAL", "0"),
    ("PROC", "1"),
    ("RVAL", "7"),
    ("PROC", "1"),
];

const VALUE: &str = "\
BY Busy
BY.RVAL 5
BY Done
BY.RVAL 0
BY.RVAL 7
";

const ALARM: &str = "\
BY Busy
BY.RVAL 5
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
            Some(EpicsValue::Enum(v)) => ["Done", "Busy"][usize::from(*v)].to_string(),
            Some(EpicsValue::ULong(v)) => format!("{v}"),
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
async fn busy_posts_rval_from_its_own_oraw_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("BY").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("BY", "VAL", DbFieldType::Enum),
                ("BY.RVAL", "RVAL", DbFieldType::Long),
                ("BY.RBV", "RBV", DbFieldType::Long),
                ("BY.ORAW", "ORAW", DbFieldType::Long),
                ("BY.ORBV", "ORBV", DbFieldType::Long),
                ("BY.MLST", "MLST", DbFieldType::Short),
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
        db.put_record_field_from_ca("BY", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
