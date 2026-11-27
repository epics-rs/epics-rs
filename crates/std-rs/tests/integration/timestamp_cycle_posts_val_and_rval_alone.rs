//! timestamp's `monitor()` (`timestampRecord.c:152-164`) against the std oracle
//! IOC (oracleStdIoc), three `camonitor -m v|a|l` clients on `TS TS.RVAL TS.OVAL
//! TS.TST TS.UDF TS.SEVR` of `record(timestamp, "TS") { field(TST, "5") }`,
//! driven with `caput TS.PROC 1; caput TS.PROC 1; caput TS.TST 4;
//! caput TS.PROC 1; caput TS.TST 5; caput TS.PROC 1`. The streams below are the
//! oracle's with the timestamps and the connect values cut; VAL and RVAL carry
//! the wall clock, so a line names them without their value.
//!
//! A cycle posts VAL and RVAL alone, both with `recGblResetAlarms | DBE_VALUE |
//! DBE_LOG`, and only when the rendered string moved. OVAL takes the new string
//! silently, and UDF clears silently.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Local, Timelike};
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(timestamp, "TS") {
  field(TST, "5")
}
"#;

/// `menu(timestampTST)`, the two choices the steps use.
const TST: [(i16, &str); 2] = [(4, "HH:MM:SS"), (5, "HH:MM")];

const STEPS: [(&str, &str); 6] = [
    ("PROC", "1"),
    ("PROC", "1"),
    ("TST", "4"),
    ("PROC", "1"),
    ("TST", "5"),
    ("PROC", "1"),
];

const VALUE: &str = "\
TS.SEVR NO_ALARM
TS
TS.RVAL
TS.TST HH:MM:SS
TS
TS.RVAL
TS.TST HH:MM
TS
TS.RVAL
";

const ALARM: &str = "\
TS
TS.RVAL
";

const LOG: &str = "\
TS
TS.RVAL
TS.TST HH:MM:SS
TS
TS.RVAL
TS.TST HH:MM
TS
TS.RVAL
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
        let value = match (self.pv, e.value().as_value()) {
            ("TS" | "TS.RVAL" | "TS.OVAL", _) => String::new(),
            ("TS.SEVR", Some(EpicsValue::Enum(v))) => MENU_ALARM_SEVR[*v as usize].to_string(),
            (_, Some(EpicsValue::Enum(v))) => TST
                .iter()
                .find(|(i, _)| *i as u16 == *v)
                .map_or_else(|| format!("{v}"), |(_, s)| s.to_string()),
            (_, Some(EpicsValue::Char(v))) => format!("{v}"),
            (pv, other) => panic!("{pv}: {other:?}"),
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

#[tokio::test]
async fn timestamp_posts_val_and_rval_alone_as_the_oracle_does() {
    // The HH:MM renders must land in one minute; a rollover would post a third.
    let secs_left = 60 - Local::now().second();
    if secs_left < 5 {
        std::thread::sleep(Duration::from_secs(u64::from(secs_left) + 1));
    }
    let (db, _) = IocBuilder::new()
        .register_record_type("timestamp", || Box::new(std_rs::TimestampRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("TS").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("TS", "VAL", DbFieldType::String),
                ("TS.RVAL", "RVAL", DbFieldType::Long),
                ("TS.OVAL", "OVAL", DbFieldType::String),
                ("TS.TST", "TST", DbFieldType::Enum),
                ("TS.UDF", "UDF", DbFieldType::Char),
                ("TS.SEVR", "SEVR", DbFieldType::Enum),
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
        db.put_record_field_from_ca("TS", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}

/// The stream above already matches the oracle without the closed set, so it
/// cannot see it; this holds the declaration itself.
#[test]
fn timestamp_closes_its_cycle_posts_to_val_and_rval() {
    use epics_base_rs::server::record::Record;
    assert_eq!(
        std_rs::TimestampRecord::default().process_posted_fields(),
        Some(
            &epics_base_rs::server::record::declared_set(
                std_rs::records::dbd_generated::TIMESTAMP_FIELDS,
                ["VAL", "RVAL"],
            )[..]
        )
    );
}
