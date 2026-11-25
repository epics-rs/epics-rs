//! longin's `monitor()` (`longinRecord.c:373-396`) against softIoc R7.0.10,
//! three `camonitor -m v|a|l` clients on `LI LI.MLST LI.ALST LI.LALM LI.SVAL`,
//! driven with `caput LI 1; caput LI 2; caput LI 4; caput LI 60;
//! caput LI.HIGH 70; caput LI.PROC 1; caput LI.PROC 1; caput LI.SVAL 9;
//! caput LI.SIMM 1; caput LI.PROC 1; caput LI.SVAL 12; caput LI.PROC 1`. The
//! streams below are softIoc's with the timestamps and the connect values cut.
//!
//! C posts VAL alone: MLST, ALST and LALM move silently with it, and SVAL posts
//! only from its own put, never from the simulation read that copies it to VAL.
//!
//! The second pin links SIOL and SIML, so the cycle itself writes SVAL and
//! SIMM: `caput LI.PROC 1; caput LI:S 5; caput LI:M 1; caput LI.PROC 1;
//! caput LI:S 7; caput LI.PROC 1; caput LI.PROC 1; caput LI:M 0;
//! caput LI.PROC 1; caput LI.VAL 3` on `LI LI.SVAL LI.SIMM`. softIoc posts
//! neither from the cycle (`recGblGetSimm` and `readValue` write them silently).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(longin, "LI") {
  field(MDEL, "1")
  field(ADEL, "3")
  field(HIGH, "50")
  field(HSV, "MINOR")
}
"#;

const STEPS: [(&str, &str); 12] = [
    ("VAL", "1"),
    ("VAL", "2"),
    ("VAL", "4"),
    ("VAL", "60"),
    ("HIGH", "70"),
    ("PROC", "1"),
    ("PROC", "1"),
    ("SVAL", "9"),
    ("SIMM", "1"),
    ("PROC", "1"),
    ("SVAL", "12"),
    ("PROC", "1"),
];

const VALUE: &str = "\
LI 2
LI 4
LI 60 HIGH MINOR
LI.SVAL 9
LI 9
LI.SVAL 12
LI 12
";

const ALARM: &str = "\
LI 1
LI 60 HIGH MINOR
LI 60
";

const LOG: &str = "\
LI 4
LI 60 HIGH MINOR
LI.SVAL 9
LI 9
LI.SVAL 12
";

const LINKED_DB: &str = r#"
record(longout, "LI:S") {
}
record(bo, "LI:M") {
  field(ZNAM, "NO")
  field(ONAM, "YES")
}
record(longin, "LI") {
  field(SIOL, "LI:S")
  field(SIML, "LI:M")
  field(MDEL, "-1")
}
"#;

const LINKED_STEPS: [(&str, &str, &str); 10] = [
    ("LI", "PROC", "1"),
    ("LI:S", "VAL", "5"),
    ("LI:M", "VAL", "1"),
    ("LI", "PROC", "1"),
    ("LI:S", "VAL", "7"),
    ("LI", "PROC", "1"),
    ("LI", "PROC", "1"),
    ("LI:M", "VAL", "0"),
    ("LI", "PROC", "1"),
    ("LI", "VAL", "3"),
];

const LINKED_VALUE: &str = "\
LI 0
LI 5
LI 7
LI 7
LI 7
LI 3
";

const LINKED_ALARM: &str = "\
LI 0
";

const LINKED_LOG: &str = "\
LI 5
LI 7
LI 3
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
            Some(EpicsValue::Enum(v)) => ["NO", "YES"][*v as usize].to_string(),
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

/// Subscribes `fields` of `record` in the three classes and runs `steps`.
async fn streams(
    db: &str,
    record: &str,
    fields: &[(&'static str, &str, DbFieldType)],
    steps: &[(&str, &str, &str)],
) -> Vec<String> {
    let (db, _) = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record(record).unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for &(pv, field, ty) in fields {
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
    for &(rec, field, value) in steps {
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    streams.iter().map(|s| s.lock().unwrap().clone()).collect()
}

#[epics_macros_rs::epics_test]
async fn longin_posts_val_alone_as_softioc_does() {
    let steps: Vec<_> = STEPS.iter().map(|&(f, v)| ("LI", f, v)).collect();
    let got = streams(
        DB,
        "LI",
        &[
            ("LI", "VAL", DbFieldType::Long),
            ("LI.MLST", "MLST", DbFieldType::Long),
            ("LI.ALST", "ALST", DbFieldType::Long),
            ("LI.LALM", "LALM", DbFieldType::Long),
            ("LI.SVAL", "SVAL", DbFieldType::Long),
        ],
        &steps,
    )
    .await;
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}

#[epics_macros_rs::epics_test]
async fn longin_cycle_reads_of_sval_and_simm_post_nothing() {
    let got = streams(
        LINKED_DB,
        "LI",
        &[
            ("LI", "VAL", DbFieldType::Long),
            ("LI.SVAL", "SVAL", DbFieldType::Long),
            ("LI.SIMM", "SIMM", DbFieldType::Enum),
        ],
        &LINKED_STEPS,
    )
    .await;
    assert_eq!(got[0], LINKED_VALUE, "DBE_VALUE");
    assert_eq!(got[1], LINKED_ALARM, "DBE_ALARM");
    assert_eq!(got[2], LINKED_LOG, "DBE_LOG");
}
