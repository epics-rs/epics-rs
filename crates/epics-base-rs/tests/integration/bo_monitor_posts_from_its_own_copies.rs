//! bo's `monitor()` (`boRecord.c:398-421`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `BO BO.RVAL BO.RBV BO.ORAW BO.ORBV BO.MLST
//! BO.LALM`, driven with `caput BO 1; BO.PROC 1; BO 0; BO.RVAL 7; BO.PROC 1;
//! BO 1; BO.RVAL 3; BO.PROC 1; BO.OSV NO_ALARM; BO.PROC 1`. The streams below
//! are softIoc's with the timestamps and the connect values cut.
//!
//! The boundary: RVAL posts when `oraw != rval`, against the record's own
//! `oraw` and not against what a client last saw. `caput BO.RVAL 7` posts 7
//! from the put; its cycle converts VAL 0 back to RVAL 0, which `oraw` already
//! holds, so softIoc posts nothing and the client is left holding 7. The
//! trackers ORAW, ORBV, MLST and LALM never post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "SINK") { }
record(bo, "BO") {
  field(DTYP, "Raw Soft Channel")
  field(OUT, "SINK")
  field(ZNAM, "Off")
  field(ONAM, "On")
  field(OSV, "MINOR")
  field(MASK, "3")
}
"#;

const STEPS: [(&str, &str); 10] = [
    ("VAL", "1"),
    ("PROC", "1"),
    ("VAL", "0"),
    ("RVAL", "7"),
    ("PROC", "1"),
    ("VAL", "1"),
    ("RVAL", "3"),
    ("PROC", "1"),
    ("OSV", "NO_ALARM"),
    ("PROC", "1"),
];

const VALUE: &str = "\
BO On STATE MINOR
BO.RVAL 3 STATE MINOR
BO Off
BO.RVAL 0
BO.RVAL 7
BO On STATE MINOR
BO.RVAL 3 STATE MINOR
BO.RVAL 3 STATE MINOR
";

const ALARM: &str = "\
BO On STATE MINOR
BO.RVAL 3 STATE MINOR
BO Off
BO.RVAL 0
BO On STATE MINOR
BO.RVAL 3 STATE MINOR
BO On
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
async fn bo_posts_rval_from_its_own_oraw_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("BO").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("BO", "VAL", DbFieldType::Enum),
                ("BO.RVAL", "RVAL", DbFieldType::Long),
                ("BO.RBV", "RBV", DbFieldType::Long),
                ("BO.ORAW", "ORAW", DbFieldType::Long),
                ("BO.ORBV", "ORBV", DbFieldType::Long),
                ("BO.MLST", "MLST", DbFieldType::Long),
                ("BO.LALM", "LALM", DbFieldType::Long),
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
        db.put_record_field_from_ca("BO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
