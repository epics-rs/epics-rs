//! A string put to a `DBF_MENU` / `DBF_DEVICE` field that names no choice and
//! no in-range index is refused and reported: C `cvt_st_menu` /
//! `cvt_st_device` print `recGblDbaddrError(S_db_badChoice, paddr, ...)`
//! (`dbFastLinkConv.c:314`, `:347`). Lines and values are softIoc's
//! `dbpf`/`dbgf` (R7.0.10-146).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(sel, "SE") {}
record(bo, "BO") {}
record(ai, "X") {}
"#;

/// (record, field, `dbpf` text, softIoc's errlog line or none, index after).
const C_PINS: &[(&str, &str, &str, Option<&str>, u16)] = &[
    (
        "SE",
        "SELM",
        " Mask",
        Some("recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: SE.SELM"),
        0,
    ),
    (
        "BO",
        "SCAN",
        " 1 second",
        Some("recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: BO.SCAN"),
        0,
    ),
    (
        "X",
        "PRIO",
        "9",
        Some("recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: X.PRIO"),
        0,
    ),
    (
        "X",
        "PINI",
        "yes",
        Some("recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: X.PINI"),
        0,
    ),
    (
        "X",
        "DTYP",
        "nope",
        Some("recGblDbaddrError: dbFastLinkConv(cvt_st_device) Illegal choice PV: X.DTYP"),
        0,
    ),
    ("X", "SCAN", "Passive", None, 0),
    ("SE", "SELM", "2", None, 2),
];

#[epics_macros_rs::epics_test]
async fn dbpf_sel_selm_space_mask_prints_recgbldbaddrerror_cvt_st_menu() {
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
    for &(rec, field, text, line, want) in C_PINS {
        epics_base_rs::runtime::log::errlog_flush();
        heard.lock().unwrap().clear();
        let put = db
            .put_record_field_from_ca(rec, field, EpicsValue::String(PvString::from(text)))
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
            "dbpf {rec}.{field} {text:?}: {put:?}"
        );
        assert_eq!(
            said,
            line.map(str::to_string).into_iter().collect::<Vec<_>>(),
            "dbpf {rec}.{field} {text:?}"
        );
        let index = db
            .get_pv(format!("{rec}.{field}"))
            .unwrap()
            .to_f64()
            .unwrap();
        assert_eq!(index, f64::from(want), "dbpf {rec}.{field} {text:?}");
    }
}
