//! A `DBR_STRING` put that names no choice prints the line of the converter
//! that refused it, and the field keeps its value. softIoc R7.0.10 `dbpf` on
//! `bo B` (ZNAM Off, ONAM On), `mbbo M` (ZRST zero, ONST one), `ai A`:
//!
//! ```text
//! dbpf B Bogus      recGblRecordError: Bogus Illegal choice PV: B
//! dbpf B 5          recGblRecordError: 5 Illegal choice PV: B
//! dbpf M Bogus      recGblRecordError: Bogus Illegal choice PV: M
//! dbpf M 7          recGblRecordError: 7 Illegal choice PV: M
//! dbpf A.SCAN Bogus recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: A.SCAN
//! dbpf A.SCAN 99    recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: A.SCAN
//! dbpf A.PINI Bogus recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: A.PINI
//! dbpf A.PINI 9     recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: A.PINI
//! dbpf A.DTYP Bogus recGblDbaddrError: dbFastLinkConv(cvt_st_device) Illegal choice PV: A.DTYP
//! dbpf A.DTYP 99    recGblDbaddrError: dbFastLinkConv(cvt_st_device) Illegal choice PV: A.DTYP
//! ```
//!
//! each followed by the kept value: Off, zero, Passive, NO, Soft Channel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_refused_string_put_prints_its_converters_line() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(bo, "B") { field(ZNAM, "Off") field(ONAM, "On") }
record(mbbo, "M") { field(ZRST, "zero") field(ONST, "one") }
record(ai, "A") {}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_string()));

    let menu = |pv: &str| {
        format!("recGblDbaddrError: dbFastLinkConv(cvt_st_menu) Illegal choice PV: {pv}")
    };
    let device = "recGblDbaddrError: dbFastLinkConv(cvt_st_device) Illegal choice PV: A.DTYP";
    for (pv, put, line, kept) in [
        (
            "B",
            "Bogus",
            "recGblRecordError: Bogus Illegal choice PV: B".to_string(),
            "Off",
        ),
        (
            "B",
            "5",
            "recGblRecordError: 5 Illegal choice PV: B".to_string(),
            "Off",
        ),
        (
            "M",
            "Bogus",
            "recGblRecordError: Bogus Illegal choice PV: M".to_string(),
            "zero",
        ),
        (
            "M",
            "7",
            "recGblRecordError: 7 Illegal choice PV: M".to_string(),
            "zero",
        ),
        ("A.SCAN", "Bogus", menu("A.SCAN"), "Passive"),
        ("A.SCAN", "99", menu("A.SCAN"), "Passive"),
        ("A.PINI", "Bogus", menu("A.PINI"), "NO"),
        ("A.PINI", "9", menu("A.PINI"), "NO"),
        ("A.DTYP", "Bogus", device.to_string(), "Soft Channel"),
        ("A.DTYP", "99", device.to_string(), "Soft Channel"),
    ] {
        errlog_flush();
        heard.lock().unwrap().clear();
        let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
        let refused = db
            .put_record_field_from_ca(rec, field, EpicsValue::String(put.into()))
            .await;
        assert!(refused.is_err(), "{pv} {put}: {refused:?}");
        errlog_flush();
        let said: Vec<String> = std::mem::take(&mut *heard.lock().unwrap())
            .into_iter()
            .filter(|m| m.starts_with("recGbl"))
            .collect();
        assert_eq!(said, [line], "{pv} {put}");
        let got = db.get_pv(pv).unwrap();
        assert_eq!(got.to_f64(), Some(0.0), "{pv} {put} kept {kept}");
    }
}
