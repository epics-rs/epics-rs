//! The links C reads for every record type alike are read at one fixed
//! `dbrType`, whatever the record answers for its own links: `dbProcess`
//! reads SDIS at `DBR_SHORT` (`dbAccess.c:566`), `recGblGetTimeStampSimm`
//! TSEL at `DBR_SHORT` (`recGbl.c:322`) and `recGblGetSimm` SIML at
//! `DBR_USHORT` (`recGbl.c:453`). The conversions are `cvt_st_s`/`cvt_st_us`
//! (`dbFastLinkConv.c:119-144`), which stop at the first non-digit, fail an
//! unparsable or out-of-range string, and read `""` as 0.
//!
//! Every expectation below is softIoc's for the same records.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;

const SOURCES: [(&str, &str); 7] = [
    ("abc", "abc"),
    ("1e3", "1e3"),
    ("5abc", "5abc"),
    ("empty", ""),
    ("neg", "-1"),
    ("big", "70000"),
    ("hex", "0x10"),
];

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    let mut db = String::new();
    for (name, val) in SOURCES {
        db += &format!(
            "record(stringin, \"S:{name}\") {{ field(VAL, \"{val}\") }}\n\
             record(ai, \"D:{name}\") {{ field(SDIS, \"S:{name}\") field(DISV, \"1\") }}\n\
             record(ai, \"T:{name}\") {{ field(TSEL, \"S:{name}\") }}\n\
             record(ai, \"M:{name}\") {{ field(SIML, \"S:{name}\") field(SIOL, \"0\") }}\n"
        );
    }
    IocBuilder::new()
        .db_string(&db, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links(rec, &mut visited).await;
}

fn alarm(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
) -> (AlarmSeverity, u16, String) {
    let r = db.get_record(rec).unwrap();
    let c = &r.read().common;
    (c.sevr, c.stat, c.amsg.to_string())
}

fn no_alarm() -> (AlarmSeverity, u16, String) {
    (
        AlarmSeverity::NoAlarm,
        alarm_status::NO_ALARM,
        String::new(),
    )
}

fn link_alarm(field: &str) -> (AlarmSeverity, u16, String) {
    (
        AlarmSeverity::Invalid,
        alarm_status::LINK_ALARM,
        format!("field {field}"),
    )
}

#[epics_macros_rs::epics_test]
async fn sdis_is_read_at_dbr_short() {
    let db = build().await;
    let mut wrong = Vec::new();
    for (name, disa, alarm_is) in [
        ("abc", 0, link_alarm("SDIS")),
        (
            "1e3",
            1,
            (
                AlarmSeverity::NoAlarm,
                alarm_status::DISABLE_ALARM,
                String::new(),
            ),
        ),
        ("5abc", 5, no_alarm()),
        ("empty", 0, no_alarm()),
        ("neg", -1, no_alarm()),
        ("big", 0, link_alarm("SDIS")),
        ("hex", 16, no_alarm()),
    ] {
        let rec = format!("D:{name}");
        process(&db, &rec).await;
        let got = (
            db.get_record(&rec).unwrap().read().common.disa,
            alarm(&db, &rec),
        );
        if got != (disa, alarm_is.clone()) {
            wrong.push(format!("{rec}: {got:?}, C {:?}", (disa, alarm_is)));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[epics_macros_rs::epics_test]
async fn tsel_is_read_at_dbr_short() {
    let db = build().await;
    let mut wrong = Vec::new();
    for (name, tse, alarm_is) in [
        ("abc", 0, link_alarm("TSEL")),
        ("1e3", 1, no_alarm()),
        ("5abc", 5, no_alarm()),
        ("empty", 0, no_alarm()),
        ("neg", -1, no_alarm()),
        ("big", 0, link_alarm("TSEL")),
        ("hex", 16, no_alarm()),
    ] {
        let rec = format!("T:{name}");
        process(&db, &rec).await;
        let got = (
            db.get_record(&rec).unwrap().read().common.tse(),
            alarm(&db, &rec),
        );
        if got != (tse, alarm_is.clone()) {
            wrong.push(format!("{rec}: {got:?}, C {:?}", (tse, alarm_is)));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// A failed `dbTryGetLink` writes `nsta` alone (`recGbl.c:454`); a SIMM no
/// menu choice names is ai's SOFT/INVALID (`aiRecord.c` `readValue`).
#[epics_macros_rs::epics_test]
async fn siml_is_read_at_dbr_ushort() {
    let db = build().await;
    let soft = (
        AlarmSeverity::Invalid,
        alarm_status::SOFT_ALARM,
        String::new(),
    );
    let link = (
        AlarmSeverity::NoAlarm,
        alarm_status::LINK_ALARM,
        String::new(),
    );
    let mut wrong = Vec::new();
    for (name, simm, alarm_is) in [
        ("abc", 0, link.clone()),
        ("1e3", 1, no_alarm()),
        ("5abc", 5, soft.clone()),
        ("empty", 0, no_alarm()),
        ("neg", 65535, soft.clone()),
        ("big", 0, link),
        ("hex", 16, soft),
    ] {
        let rec = format!("M:{name}");
        process(&db, &rec).await;
        let got = (
            db.get_record(&rec)
                .unwrap()
                .read()
                .record
                .simm_value()
                .map(|v| v as u16),
            alarm(&db, &rec),
        );
        if got != (Some(simm), alarm_is.clone()) {
            wrong.push(format!("{rec}: {got:?}, C {:?}", (simm, alarm_is)));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
