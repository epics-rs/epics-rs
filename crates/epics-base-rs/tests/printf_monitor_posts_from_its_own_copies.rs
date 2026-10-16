//! printf's post (`printfRecord.c:398-401`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `PF PF.LEN PF.FMT PF.SIZV`, driven with
//! `caput PF.PROC 1; PF.PROC 1; PF.FMT ww=%d; PF.PROC 1`. The streams below
//! are softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: VAL and LEN post on every cycle, moved or not, with
//! `DBE_VALUE | DBE_LOG`; FMT posts only from its own put; SIZV never posts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(printf, "PF") {
  field(FMT, "v=%d")
  field(INP0, "5")
}
"#;

const STEPS: [(&str, &str); 4] = [
    ("PROC", "1"),
    ("PROC", "1"),
    ("FMT", "ww=%d"),
    ("PROC", "1"),
];

const VALUE: &str = "\
PF v=5
PF.LEN 4
PF v=5
PF.LEN 4
PF.FMT ww=%d
PF ww=5
PF.LEN 5
PF ww=5
PF.LEN 5
";

const ALARM: &str = "\
PF v=5
PF.LEN 4
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
            Some(EpicsValue::String(v)) => v.as_str_lossy().into_owned(),
            Some(EpicsValue::CharArray(v)) => {
                let end = v.iter().position(|b| *b == 0).unwrap_or(v.len());
                String::from_utf8_lossy(&v[..end]).into_owned()
            }
            Some(EpicsValue::ULong(v)) => format!("{v}"),
            Some(EpicsValue::Long(v)) => format!("{v}"),
            Some(EpicsValue::UShort(v)) => format!("{v}"),
            // Read at delivery: the test fills it in after the step.
            None => "@".to_string(),
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
async fn printf_posts_val_and_len_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("PF").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("PF", "VAL", DbFieldType::Char),
                ("PF.LEN", "LEN", DbFieldType::Long),
                ("PF.FMT", "FMT", DbFieldType::String),
                ("PF.SIZV", "SIZV", DbFieldType::UShort),
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
        db.put_record_field_from_ca("PF", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // A step moves VAL or FMT once, so the value read now is the one each
        // of its posts carried.
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            for pv in ["PF", "PF.FMT"] {
                let now = match db.get_pv(pv).unwrap() {
                    EpicsValue::String(s) => s.as_str_lossy().into_owned(),
                    EpicsValue::CharArray(v) => {
                        let end = v.iter().position(|b| *b == 0).unwrap_or(v.len());
                        String::from_utf8_lossy(&v[..end]).into_owned()
                    }
                    other => panic!("{pv}: {other:?}"),
                };
                *stream = stream.replace(&format!("{pv} @"), &format!("{pv} {now}"));
            }
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}
