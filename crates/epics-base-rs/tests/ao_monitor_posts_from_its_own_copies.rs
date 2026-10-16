//! ao's `monitor()` (`aoRecord.c:520-551`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `AO AO.OVAL AO.RVAL AO.RBV`, driven with
//! `dbpf AO.VAL 0.5; dbpf AO.VAL 2; dbpf AO.VAL 20; dbpf AO.VAL 10;
//! dbpf AO.HIGH 5; dbpf AO.PROC 1; dbpf AO.PROC 1`. The streams below are
//! softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: a change inside MDEL posts VAL to the log class only while
//! `omod` still posts OVAL; an MDEL crossing posts all three; RVAL posts only
//! when `oraw != rval` and RBV never moves here; a cycle that moves nothing
//! posts nothing; an alarm-only cycle posts VAL and OVAL with `DBE_ALARM`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "AO") {
  field(MDEL, "1")
  field(DRVH, "10")
  field(DRVL, "-10")
  field(HIGH, "50")
  field(HSV, "MINOR")
  field(ESLO, "0.5")
  field(LINR, "LINEAR")
}
"#;

const STEPS: [(&str, &str); 7] = [
    ("VAL", "0.5"),
    ("VAL", "2"),
    ("VAL", "20"),
    ("VAL", "10"),
    ("HIGH", "5"),
    ("PROC", "1"),
    ("PROC", "1"),
];

const VALUE: &str = "\
AO.OVAL 0.5
AO.RVAL 1
AO 2
AO.OVAL 2
AO.RVAL 4
AO 10
AO.OVAL 10
AO.RVAL 20
";

const ALARM: &str = "\
AO 0.5
AO.OVAL 0.5
AO.RVAL 1
AO 10 HIGH MINOR
AO.OVAL 10 HIGH MINOR
";

const LOG: &str = "\
AO 0.5
AO.OVAL 0.5
AO.RVAL 1
AO 2
AO.OVAL 2
AO.RVAL 4
AO 10
AO.OVAL 10
AO.RVAL 20
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
            Some(EpicsValue::Double(v)) => format!("{v}"),
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
async fn ao_posts_val_oval_rval_and_rbv_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("AO").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("AO", "VAL", DbFieldType::Double),
                ("AO.OVAL", "OVAL", DbFieldType::Double),
                ("AO.RVAL", "RVAL", DbFieldType::Long),
                ("AO.RBV", "RBV", DbFieldType::Long),
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
        db.put_record_field_from_ca("AO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
