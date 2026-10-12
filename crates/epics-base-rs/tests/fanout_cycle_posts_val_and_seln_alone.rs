//! fanout's posts (`fanoutRecord.c:147-152`, from `process()`: fanout has no
//! `monitor()`) against softIoc R7.0.10, three `camonitor -m v|a|l` clients on
//! `FO FO.SELN FO.SELM FO.OFFS FO.SHFT FO.SELL FO.LNK0 FO.UDF`, driven with
//! `caput FO.PROC 1; caput FO.SELN 3; caput FO.PROC 1; caput FO.OFFS 1;
//! caput FO.PROC 1; caput FO.SELM Mask; caput FO.SHFT 1; caput FO.PROC 1;
//! caput FO.SELN 6; caput FO.PROC 1`. The streams below are softIoc's with the
//! timestamps and the connect values cut.
//!
//! A cycle posts VAL with the alarm bits alone, and SELN only when the SELL
//! read moved it off its process-entry value; every other field posts from its
//! own put only. SELL is empty here: a SELL-moved SELN is
//! `fanout_monitor_posts_a_moved_seln`'s, which needs the fanout dispatch in
//! the Output stage. SELN stays in range, so no SOFT alarm is raised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::{MENU_ALARM_SEVR, Record};
use epics_base_rs::server::records::fanout::FanoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(fanout, "FO") {
  field(SELM, "Specified")
}
"#;

const SELM: [&str; 3] = ["All", "Specified", "Mask"];

const STEPS: [(&str, &str, &str); 10] = [
    ("FO", "PROC", "1"),
    ("FO", "SELN", "3"),
    ("FO", "PROC", "1"),
    ("FO", "OFFS", "1"),
    ("FO", "PROC", "1"),
    ("FO", "SELM", "Mask"),
    ("FO", "SHFT", "1"),
    ("FO", "PROC", "1"),
    ("FO", "SELN", "6"),
    ("FO", "PROC", "1"),
];

const VALUE: &str = "\
FO.SELN 3
FO.OFFS 1
FO.SELM Mask
FO.SHFT 1
FO.SELN 6
";

const ALARM: &str = "\
FO 0
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
            Some(EpicsValue::Short(v)) => format!("{v}"),
            Some(EpicsValue::UShort(v)) => format!("{v}"),
            Some(EpicsValue::Enum(v)) => SELM[*v as usize].to_string(),
            Some(EpicsValue::String(v)) => v.to_string(),
            Some(EpicsValue::Char(v)) => format!("{v}"),
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
async fn fanout_cycle_posts_val_and_seln_alone_as_softioc_does() {
    let got = streams(
        DB,
        "FO",
        &[
            ("FO", "VAL", DbFieldType::Long),
            ("FO.SELN", "SELN", DbFieldType::Short),
            ("FO.SELM", "SELM", DbFieldType::Enum),
            ("FO.OFFS", "OFFS", DbFieldType::Short),
            ("FO.SHFT", "SHFT", DbFieldType::Short),
            ("FO.SELL", "SELL", DbFieldType::String),
            ("FO.LNK0", "LNK0", DbFieldType::String),
            ("FO.UDF", "UDF", DbFieldType::Char),
        ],
        &STEPS,
    )
    .await;
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}

/// No field outside VAL and SELN is written by a fanout cycle today, so the
/// stream above cannot see the closed set; this holds the declaration itself.
#[test]
fn fanout_closes_its_cycle_posts_to_val_and_seln() {
    assert_eq!(
        FanoutRecord::default().process_posted_fields(),
        Some(&["VAL", "SELN"][..])
    );
}
