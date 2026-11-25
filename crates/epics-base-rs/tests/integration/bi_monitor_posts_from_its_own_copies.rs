//! bi's `monitor()` (`biRecord.c:283-305`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `BI BI.RVAL BI.ORAW BI.MLST BI.LALM BR
//! BR.RVAL BR.ORAW`, driven with `caput SRC 1; BI.PROC 1; BR.PROC 1; BI.RVAL 5;
//! BI.PROC 1; BR.RVAL 9; BR.PROC 1; SRC 0; BI.PROC 1; BR.PROC 1; BI.PROC 1`.
//! The streams below are softIoc's with the timestamps and the connect values
//! cut.
//!
//! The boundary: RVAL posts when `oraw != rval`, against the record's own
//! `oraw` and not against what a client last saw. On the Soft Channel BI,
//! which never converts, `caput BI.RVAL 5` posts 5 from the put and again
//! from its cycle. On the Raw Soft Channel BR, `caput BR.RVAL 9` posts 9 from
//! the put; the cycle reads RVAL 1 back, which `oraw` already holds, so
//! nothing more posts. The trackers ORAW, MLST and LALM never post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "SRC") { }
record(bi, "BI") {
  field(INP, "SRC")
  field(ZNAM, "Off")
  field(ONAM, "On")
  field(OSV, "MINOR")
}
record(bi, "BR") {
  field(DTYP, "Raw Soft Channel")
  field(INP, "SRC")
  field(ZNAM, "Off")
  field(ONAM, "On")
}
"#;

const STEPS: [(&str, &str, &str); 11] = [
    ("SRC", "VAL", "1"),
    ("BI", "PROC", "1"),
    ("BR", "PROC", "1"),
    ("BI", "RVAL", "5"),
    ("BI", "PROC", "1"),
    ("BR", "RVAL", "9"),
    ("BR", "PROC", "1"),
    ("SRC", "VAL", "0"),
    ("BI", "PROC", "1"),
    ("BR", "PROC", "1"),
    ("BI", "PROC", "1"),
];

const VALUE: &str = "\
BI On STATE MINOR
BR On
BR.RVAL 1
BI.RVAL 5 STATE MINOR
BI.RVAL 5 STATE MINOR
BR.RVAL 9
BI Off
BR Off
BR.RVAL 0
";

const ALARM: &str = "\
BI On STATE MINOR
BR On
BR.RVAL 1
BI Off
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
            Some(EpicsValue::Enum(v)) => ["Off", "On"][*v as usize].to_string(),
            Some(EpicsValue::Long(v)) => format!("{v}"),
            Some(EpicsValue::ULong(v)) => format!("{v}"),
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
async fn bi_posts_rval_from_its_own_oraw_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    let mut sid = 0;
    for (record, fields) in [
        ("BI", &["VAL", "RVAL", "ORAW", "MLST", "LALM"][..]),
        ("BR", &["VAL", "RVAL", "ORAW"][..]),
    ] {
        let cell = db.get_record(record).unwrap();
        let mut inst = cell.write();
        for (class, stream) in classes.iter().zip(&streams) {
            for &field in fields {
                let (pv, ty): (&'static str, _) = match field {
                    "VAL" => (record, DbFieldType::Enum),
                    _ => (format!("{record}.{field}").leak(), DbFieldType::Long),
                };
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
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
