//! seq's posts (`seqRecord.c:219-268`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `SQ SQ.SELN SQ.DO0 SQ.DO1 SQ.OLDN SQ.SELM`,
//! driven with `caput SQ.PROC 1; SRC 1; SQ.PROC 1; SQ.PROC 1; SRC 2; SQ.PROC
//! 1; SQ.SELM Specified; SQ.SELN 1; SQ.PROC 1; SQ.SELN 20; SQ.PROC 1;
//! SQ.SELN 0; SRC 3; SQ.PROC 1`. The streams below are softIoc's with the
//! timestamps and the connect values cut.
//!
//! The boundaries: VAL posts only with alarm events; a group posts DOn from
//! its callback when the DOLn read moved it, carrying the alarm not yet reset;
//! `asyncFinish` posts SELN when it differs from OLDN; an out-of-range SELN
//! raises SOFT INVALID. OLDN, which the cycle writes, never posts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "SRC") {
}
record(ao, "DST") {
}
record(seq, "SQ") {
  field(SELM, "All")
  field(DOL0, "SRC")
  field(LNK0, "DST PP")
  field(DOL1, "SRC")
  field(LNK1, "DST")
}
"#;

const STEPS: [(&str, &str, &str); 14] = [
    ("SQ", "PROC", "1"),
    ("SRC", "VAL", "1"),
    ("SQ", "PROC", "1"),
    ("SQ", "PROC", "1"),
    ("SRC", "VAL", "2"),
    ("SQ", "PROC", "1"),
    ("SQ", "SELM", "Specified"),
    ("SQ", "SELN", "1"),
    ("SQ", "PROC", "1"),
    ("SQ", "SELN", "20"),
    ("SQ", "PROC", "1"),
    ("SQ", "SELN", "0"),
    ("SRC", "VAL", "3"),
    ("SQ", "PROC", "1"),
];

const VALUE: &str = "\
SQ.DO0 1
SQ.DO1 1
SQ.DO0 2
SQ.DO1 2
SQ.SELM Specified
SQ.SELN 1
SQ.SELN 20
SQ.SELN 20 SOFT INVALID
SQ.SELN 0 SOFT INVALID
SQ.DO0 3 SOFT INVALID
SQ.SELN 0
";

const ALARM: &str = "\
SQ 0
SQ 0 SOFT INVALID
SQ.SELN 20 SOFT INVALID
SQ 0
SQ.SELN 0
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
            Some(EpicsValue::Double(v)) => format!("{v}"),
            // SELM's menuSelm choices, as camonitor prints them.
            Some(EpicsValue::Enum(v)) => ["All", "Specified", "Mask"][*v as usize].to_string(),
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
async fn seq_posts_from_its_own_copies_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("SQ").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("SQ", "VAL", DbFieldType::Long),
                ("SQ.SELN", "SELN", DbFieldType::Short),
                ("SQ.DO0", "DO0", DbFieldType::Double),
                ("SQ.DO1", "DO1", DbFieldType::Double),
                ("SQ.OLDN", "OLDN", DbFieldType::Short),
                ("SQ.SELM", "SELM", DbFieldType::String),
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
    for (record, field, value) in STEPS {
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // The groups run from the callback task; wait for `asyncFinish`.
        for _ in 0..400 {
            if !cell.read().is_processing() {
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
        assert!(!cell.read().is_processing(), "SQ never finished");
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
