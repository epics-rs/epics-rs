//! asyn's `monitor()` (`asynRecord.c:1006-1031`) against the asyn R4-46 oracle
//! IOC (oracleIoc), three `camonitor -m v|a|l` clients on `AS.VAL AS.AINP AS.TINP AS.NRRD
//! AS.NORD AS.NOWT AS.TMOD` of a record with no port, driven with `caput AS.PROC 1;
//! caput AS.PROC 1; caput AS.NRRD 5; caput AS.PROC 1; caput AS.TMOD Write;
//! caput AS.PROC 1; caput AS.TMOD Read; caput AS.PROC 1; caput AS.IFMT Hybrid;
//! caput AS.PROC 1`. The streams below are the oracle's with the timestamps
//! and the connect values cut.
//!
//! Portless, `process()` reports "Not connect to a port" with STATE/MINOR and
//! runs `monitor()`, which posts AINP (BINP once IFMT leaves ASCII) and TINP on
//! every cycle whose TMOD reads, with `recGblResetAlarms | DBE_VALUE |
//! DBE_LOG`. NRRD posts from the cycle as well as from its put: `stateNoDevice`
//! skips `REMEMBER_STATE`, so `POST_IF_NEW` compares against init's copy.
//! NOWT posts once, on the first cycle: its old copy starts at C's `calloc`
//! zero and the field at its dbd default 80.
//! VAL never posts, not even on the STATE/MINOR transition: `monitor()` does
//! not name it.
//!
//! AINP and TINP post by reference (the reader reads the record's buffer), so
//! a line carries no value for them; the oracle's buffers are empty here too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use asyn_rs::asyn_record::AsynRecord;
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(asyn, "AS") {
}
"#;

/// `menu(asynTMOD)`.
const TMOD: [&str; 5] = ["Write/Read", "Write", "Read", "Flush", "NoI/O"];

const STEPS: [(&str, &str); 10] = [
    ("PROC", "1"),
    ("PROC", "1"),
    ("NRRD", "5"),
    ("PROC", "1"),
    ("TMOD", "Write"),
    ("PROC", "1"),
    ("TMOD", "Read"),
    ("PROC", "1"),
    ("IFMT", "Hybrid"),
    ("PROC", "1"),
];

const VALUE: &str = "\
AS.AINP STATE MINOR
AS.TINP STATE MINOR
AS.NOWT 80 STATE MINOR
AS.AINP STATE MINOR
AS.TINP STATE MINOR
AS.NRRD 5 STATE MINOR
AS.AINP STATE MINOR
AS.TINP STATE MINOR
AS.NRRD 5 STATE MINOR
AS.TMOD Write STATE MINOR
AS.TMOD Read STATE MINOR
AS.AINP STATE MINOR
AS.TINP STATE MINOR
AS.TINP STATE MINOR
";

const ALARM: &str = "\
AS.AINP STATE MINOR
AS.TINP STATE MINOR
AS.NOWT 80 STATE MINOR
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
            Some(EpicsValue::String(v)) => v.to_string(),
            Some(EpicsValue::Enum(v)) => TMOD[*v as usize].to_string(),
            Some(other) => panic!("{}: {other:?}", self.pv),
            None => String::new(),
        };
        let mut line = format!("{} {value}", self.pv).trim_end().to_string();
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
async fn asyn_posts_its_read_fields_every_cycle_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("AS").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("AS.VAL", "VAL", DbFieldType::Long),
                ("AS.AINP", "AINP", DbFieldType::String),
                ("AS.TINP", "TINP", DbFieldType::String),
                ("AS.NRRD", "NRRD", DbFieldType::Long),
                ("AS.NORD", "NORD", DbFieldType::Long),
                ("AS.NOWT", "NOWT", DbFieldType::Long),
                ("AS.TMOD", "TMOD", DbFieldType::Enum),
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
        db.put_record_field_from_ca("AS", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
