//! fanout's SELM switch (`fanoutRecord.c:106-141`) against softIoc R7.0.10:
//! `LNKn` → `FM:Tn`, each of which stamps the order it ran in from a shared
//! counter, driven with `caput FM.SELM All; caput FM.PROC 1; caput FM.SELM
//! Mask; caput FM.SHFT 0; caput FM.SELN 33825; caput FM.PROC 1; caput FM.SHFT
//! 1; caput FM.SELN 32768; caput FM.PROC 1; caput FM.SHFT 16; caput FM.PROC 1;
//! caput FM.SHFT -1; caput FM.PROC 1; caput FM.SELM 7; caput FM.PROC 1` with
//! `camonitor -m a FM` on the side. The rows and the stream are softIoc's.
//!
//! Mask runs LNK0 first; SHFT 16 is out of range, SHFT -1 shifts SELN 32768
//! out of the `epicsUInt16`, and an out-of-menu SELM (caput sends it as
//! `DBR_ENUM`, which `dbPut` does not range-check) takes the `default:` arm.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const LINKS: [&str; 16] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "A", "B", "C", "D", "E", "F",
];

fn db() -> String {
    let mut db = String::from(r#"record(calc, "FM:SEQ") { field(CALC, "VAL+1") }"#);
    for n in LINKS {
        db += &format!(
            r#"record(calc, "FM:T{n}") {{ field(INPA, "FM:SEQ NPP") field(CALC, "A") field(FLNK, "FM:SEQ") }}"#
        );
    }
    db += r#"record(fanout, "FM") {"#;
    for n in LINKS {
        db += &format!(r#" field(LNK{n}, "FM:T{n}")"#);
    }
    db + "}"
}

/// `(label, puts, the links that ran in order, SEVR, STAT)`.
type Row = (
    &'static str,
    Vec<(&'static str, EpicsValue)>,
    &'static str,
    &'static str,
    &'static str,
);

fn rows() -> Vec<Row> {
    let s = |v: &str| EpicsValue::String(v.into());
    vec![
        (
            "all",
            vec![("SELM", s("All"))],
            "0 1 2 3 4 5 6 7 8 9 A B C D E F",
            "NO_ALARM",
            "NO_ALARM",
        ),
        (
            "mask8421",
            vec![("SELM", s("Mask")), ("SHFT", s("0")), ("SELN", s("33825"))],
            "0 5 A F",
            "NO_ALARM",
            "NO_ALARM",
        ),
        (
            "shft1",
            vec![("SHFT", s("1")), ("SELN", s("32768"))],
            "E",
            "NO_ALARM",
            "NO_ALARM",
        ),
        ("shft16", vec![("SHFT", s("16"))], "", "INVALID", "SOFT"),
        (
            "shftm1",
            vec![("SHFT", s("-1"))],
            "",
            "NO_ALARM",
            "NO_ALARM",
        ),
        (
            "selm7",
            vec![("SELM", EpicsValue::Enum(7))],
            "",
            "INVALID",
            "SOFT",
        ),
    ]
}

const ALARM: &str = "\
FM 0
FM 0 SOFT INVALID
FM 0
FM 0 SOFT INVALID
";

struct Line(Arc<Mutex<String>>);

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let e = &event.event;
        let value = e.value();
        let Some(EpicsValue::Long(v)) = value.as_value() else {
            panic!("FM: {:?}", value.as_value());
        };
        let mut line = format!("FM {v}");
        if e.sevr() != 0 {
            line += &format!(
                " {} {}",
                alarm_condition_string(e.stat()),
                MENU_ALARM_SEVR[e.sevr() as usize]
            );
        }
        let mut stream = self.0.lock().unwrap();
        stream.push_str(&line);
        stream.push('\n');
        Some(event)
    }
}

#[epics_macros_rs::epics_test]
async fn fanout_selm_rows_dispatch_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(&db(), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let stream: Arc<Mutex<String>> = Arc::default();
    let _reader = {
        let cell = db.get_record("FM").unwrap();
        let mut inst = cell.write();
        let reader = inst
            .add_subscriber("VAL", 0, DbFieldType::Long, EventMask::ALARM.bits())
            .unwrap();
        assert!(inst.attach_filter_to_last_subscriber("VAL", Arc::new(Line(stream.clone()))));
        reader
    };
    let f64_of = |rec: &str, field: &str| {
        db.get_record(rec)
            .unwrap()
            .read()
            .record
            .get_field(field)
            .and_then(|v| v.to_f64())
            .unwrap()
    };
    for (label, puts, ran, sevr, stat) in rows() {
        for (field, value) in puts {
            db.put_record_field_from_ca("FM", field, value)
                .await
                .unwrap_or_else(|e| panic!("{label}: put {field}: {e:?}"));
        }
        let before = f64_of("FM:SEQ", "VAL");
        db.put_record_field_from_ca("FM", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
        let mut order: Vec<(f64, &str)> = LINKS
            .iter()
            .map(|n| (f64_of(&format!("FM:T{n}"), "VAL"), *n))
            .filter(|(v, _)| *v >= before)
            .collect();
        order.sort_by(|a, b| a.0.total_cmp(&b.0));
        let order: Vec<&str> = order.into_iter().map(|(_, n)| n).collect();
        assert_eq!(order.join(" "), ran, "{label}: links run");
        let inst = db.get_record("FM").unwrap();
        let inst = inst.read();
        assert_eq!(
            MENU_ALARM_SEVR[inst.common.sevr as usize], sevr,
            "{label}: SEVR"
        );
        assert_eq!(
            alarm_condition_string(inst.common.stat),
            stat,
            "{label}: STAT"
        );
    }
    assert_eq!(*stream.lock().unwrap(), ALARM, "DBE_ALARM");
}
