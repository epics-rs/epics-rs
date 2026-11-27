//! A string put no state names and no in-range index reaches is refused and
//! reported: C `cvt_st_e` / `putStringEnum` print
//! `recGblRecordError(S_db_badChoice, precord, from)` (`dbFastLinkConv.c:279`,
//! `dbConvert.c:1189`). Lines and values are softIoc's `dbpf`/`dbgf`
//! (R7.0.10-146).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(mbbi, "MN") { field(VAL, "0") }
record(mbbi, "MS") { field(ZRST, "a") field(ONST, "b") field(TWST, "c") field(THST, "d") }
record(bi, "BS") { field(ZNAM, "zero") field(ONAM, "one") }
"#;

/// (record, `dbpf <rec>.VAL <text>`, softIoc's errlog line or none, VAL after).
const C_PINS: &[(&str, &str, Option<&str>, u16)] = &[
    (
        "MN",
        "1",
        Some("recGblRecordError: 1 Illegal choice PV: MN"),
        0,
    ),
    (
        "MS",
        "5",
        Some("recGblRecordError: 5 Illegal choice PV: MS"),
        0,
    ),
    (
        "MS",
        "zz",
        Some("recGblRecordError: zz Illegal choice PV: MS"),
        0,
    ),
    ("MS", "c", None, 2),
    ("MS", "1", None, 1),
    ("BS", "one", None, 1),
    (
        "BS",
        "2",
        Some("recGblRecordError: 2 Illegal choice PV: BS"),
        1,
    ),
    ("BS", "0", None, 0),
];

#[epics_macros_rs::epics_test]
async fn dbpf_mbbi_val_1_without_states_prints_recglbrecorderror_1_illegal_choice_pv_mn() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().unwrap().push(m.to_string());
    });
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for &(rec, text, line, want) in C_PINS {
        epics_base_rs::runtime::log::errlog_flush();
        heard.lock().unwrap().clear();
        let put = db
            .put_record_field_from_ca(rec, "VAL", EpicsValue::String(PvString::from(text)))
            .await;
        epics_base_rs::runtime::log::errlog_flush();
        let said: Vec<String> = heard
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.contains("Illegal choice"))
            .map(|m| m.trim_end().to_string())
            .collect();
        assert_eq!(
            put.is_err(),
            line.is_some(),
            "dbpf {rec}.VAL {text}: {put:?}"
        );
        assert_eq!(
            said,
            line.map(str::to_string).into_iter().collect::<Vec<_>>(),
            "dbpf {rec}.VAL {text}"
        );
        let val = db.get_pv(rec).unwrap().to_f64().unwrap();
        assert_eq!(val, f64::from(want), "dbpf {rec}.VAL {text}");
    }
}
