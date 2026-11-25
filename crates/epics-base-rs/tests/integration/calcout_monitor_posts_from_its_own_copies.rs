//! calcout's `monitor()` (`calcoutRecord.c:660-691`) and its DLYA posts
//! (`:279`, `:296`) against softIoc R7.0.10, three `camonitor -m v|a|l`
//! clients on `CO CO.OVAL CO.DLYA CO.A CO.B CO.PVAL CO.LA`, driven with
//! `caput CO.PROC 1; CO.OVAL 5; CO.PROC 1; CO.DOPT "Use OCAL"; CO.OVAL 7;
//! CO.PROC 1; CO.OVAL 3; CO.PROC 1; CO.DOPT "Use CALC"; CO.ODLY 0.3; CO.PROC
//! 1; CO.A 2`. The streams below are softIoc's with the timestamps and the
//! connect values cut.
//!
//! The boundary: OVAL posts when `povl != oval`, against the record's own
//! `povl` and not against what a client last saw. A put OVAL 5 is converted
//! back to the 1 `povl` holds and posts nothing more; under OCAL a put 7
//! posts again from its cycle (`povl` 1) and a later put 3 does not (`povl`
//! 7). A delayed cycle posts DLYA 1 then 0, to DBE_VALUE alone.
//!
//! Compared per PV: the walk visits A..U in `last_posted`'s hash order, not
//! C's index order, so the order across fields is not this type's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calcout, "CO") {
  field(CALC, "A")
  field(OCAL, "B")
  field(A, "1")
  field(B, "7")
}
"#;

const STEPS: [(&str, &str); 12] = [
    ("PROC", "1"),
    ("OVAL", "5"),
    ("PROC", "1"),
    ("DOPT", "Use OCAL"),
    ("OVAL", "7"),
    ("PROC", "1"),
    ("OVAL", "3"),
    ("PROC", "1"),
    ("DOPT", "Use CALC"),
    ("ODLY", "0.3"),
    ("PROC", "1"),
    ("A", "2"),
];

const VALUE: &str = "\
CO 1
CO.A 1
CO.B 7
CO.OVAL 1
CO.OVAL 5
CO.OVAL 7
CO.OVAL 7
CO.OVAL 3
CO.DLYA 1
CO.DLYA 0
CO.OVAL 1
CO.A 2
CO.DLYA 1
CO.DLYA 0
CO 2
CO.A 2
CO.OVAL 2
";

const ALARM: &str = "\
CO 1
CO.A 1
CO.B 7
CO.OVAL 1
";

const LOG: &str = "\
CO 1
CO.A 1
CO.B 7
CO.OVAL 1
CO.OVAL 5
CO.OVAL 7
CO.OVAL 7
CO.OVAL 3
CO.OVAL 1
CO.A 2
CO 2
CO.A 2
CO.OVAL 2
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
            Some(EpicsValue::Short(v)) => format!("{v}"),
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
async fn calcout_posts_oval_from_its_own_povl_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("CO").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("CO", "VAL", DbFieldType::Double),
                ("CO.OVAL", "OVAL", DbFieldType::Double),
                ("CO.DLYA", "DLYA", DbFieldType::Short),
                ("CO.A", "A", DbFieldType::Double),
                ("CO.B", "B", DbFieldType::Double),
                ("CO.PVAL", "PVAL", DbFieldType::Double),
                ("CO.LA", "LA", DbFieldType::Double),
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
        let put = db
            .put_record_field_from_ca("CO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // An ODLY cycle completes when its callback has written OVAL.
        if let Some(rx) = put.into_handle() {
            let _ = epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, rx)
                .await
                .expect("the delayed cycle never completed");
        }
    }
    let got: Vec<String> = streams.iter().map(|s| by_pv(&s.lock().unwrap())).collect();
    assert_eq!(got[0], by_pv(VALUE), "DBE_VALUE");
    assert_eq!(got[1], by_pv(ALARM), "DBE_ALARM");
    assert_eq!(got[2], by_pv(LOG), "DBE_LOG");
}

/// `stream` with each PV's lines kept in order and the PVs apart.
fn by_pv(stream: &str) -> String {
    let mut lines: Vec<&str> = stream.lines().collect();
    lines.sort_by_key(|l| l.split(' ').next().unwrap_or_default());
    lines.iter().map(|l| format!("{l}\n")).collect()
}
