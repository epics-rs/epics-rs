//! aCalcout's and sCalcout's DLYA posts (`aCalcoutRecord.c:341`, `:424`;
//! `sCalcoutRecord.c:402`, `:426`) against the calc R3-7-5 oracle IOC,
//! `camonitor -m v` and `-m l` on `AC.DLYA SC.DLYA`, driven with
//! `caput AC.ODLY 0.3; AC.PROC 1; SC.ODLY 0.3; SC.PROC 1`, then after the
//! delays `AC.PROC 1; SC.PROC 1`. The streams below are oracleIoc's with the
//! timestamps and the connect values cut.
//!
//! The boundary: DLYA goes to DBE_VALUE alone, so a DBE_LOG monitor sees
//! neither edge. Both posts precede the cycle's `recGblResetAlarms`, so the
//! first delay's clear still carries UDF.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::{acalcout::AcalcoutRecord, scalcout::ScalcoutRecord};
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(acalcout, "AC") {
  field(CALC, "A")
  field(OOPT, "Every Time")
}
record(scalcout, "SC") {
  field(CALC, "A")
  field(OOPT, "Every Time")
}
"#;

const VALUE: &str = "\
AC.DLYA 1 UDF INVALID
AC.DLYA 0 UDF INVALID
AC.DLYA 1
AC.DLYA 0
SC.DLYA 1 UDF INVALID
SC.DLYA 0 UDF INVALID
SC.DLYA 1
SC.DLYA 0
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
async fn dlya_posts_to_dbe_value_alone_as_oracle_ioc_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let classes = [EventMask::VALUE, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    let mut sid = 0;
    for (record, pv, ty) in [
        ("AC", "AC.DLYA", DbFieldType::UShort),
        ("SC", "SC.DLYA", DbFieldType::Short),
    ] {
        let cell = db.get_record(record).unwrap();
        let mut inst = cell.write();
        for (class, stream) in classes.iter().zip(&streams) {
            readers.push(inst.add_subscriber("DLYA", sid, ty, class.bits()).unwrap());
            sid += 1;
            assert!(inst.attach_filter_to_last_subscriber(
                "DLYA",
                Arc::new(Line {
                    pv,
                    stream: stream.clone(),
                }),
            ));
        }
    }
    for record in ["AC", "SC"] {
        db.put_record_field_from_ca(record, "ODLY", EpicsValue::String("0.3".into()))
            .await
            .unwrap();
    }
    for _ in 0..2 {
        for record in ["AC", "SC"] {
            db.put_record_field_from_ca(record, "PROC", EpicsValue::String("1".into()))
                .await
                .unwrap();
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(600)).await;
    }
    let got: Vec<String> = streams.iter().map(|s| by_pv(&s.lock().unwrap())).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], "", "DBE_LOG");
}

/// `stream` with each PV's lines kept in order and the PVs apart.
fn by_pv(stream: &str) -> String {
    let mut lines: Vec<&str> = stream.lines().collect();
    lines.sort_by_key(|l| l.split(' ').next().unwrap_or_default());
    lines.iter().map(|l| format!("{l}\n")).collect()
}
