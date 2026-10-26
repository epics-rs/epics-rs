//! A one-element `dbGetLink` of an array source converts its element 0,
//! whatever the request type and whether or not the source renders it:
//! `dbCaGetLink`'s `aConvert` and `dbGet` both convert from offset 0, and
//! `lnkCalc` converts `pbuffer[0]` of what it is put (`lnkCalc.c:672`).
//!
//! softIoc 7.0.10, one `dbpf <rec>.PROC 1` each after the CA links connect:
//!
//! ```text
//! SI:CA:WC "97"   SI:CA:WL "7"   SI:CA:WD "2.500000"   SI:CA:WS "ab"
//! SI:DB:WS "ab"   SI:DB:WD "2.5"   SI:DB:WE ""  LINK/INVALID
//! SO:CA:WD "2.500000"   SO:DB:WS "ab"   AS:DB:WS.SNAM "ab"   AS:CA:WL.SNAM "7"
//! PF:CA:WD "2.500000"   LC:K 3   LCS "one" (LCO:WD NO_ALARM)
//! ```
//!
//! Every other record NO_ALARM.

mod calink_ioc;

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "WC") { field(FTVL, "CHAR") field(NELM, "4") field(INP, "[97, 98]") }
record(waveform, "WL") { field(FTVL, "LONG") field(NELM, "4") field(INP, "[7, 8]") }
record(waveform, "WD") { field(FTVL, "DOUBLE") field(NELM, "4") field(PREC, "1") field(INP, "[2.5, 3.5]") }
record(waveform, "WS") { field(FTVL, "STRING") field(NELM, "4") field(INP, "[\"ab\", \"cd\"]") }
record(waveform, "WE") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(stringin, "SI:CA:WC") { field(INP, "WC CA") }
record(stringin, "SI:CA:WL") { field(INP, "WL CA") }
record(stringin, "SI:CA:WD") { field(INP, "WD CA") }
record(stringin, "SI:CA:WS") { field(INP, "WS CA") }
record(stringin, "SI:DB:WS") { field(INP, "WS NPP") }
record(stringin, "SI:DB:WD") { field(INP, "WD NPP") }
record(stringin, "SI:DB:WE") { field(INP, "WE NPP") }
record(stringout, "SO:CA:WD") { field(OMSL, "closed_loop") field(DOL, "WD CA") }
record(stringout, "SO:DB:WS") { field(OMSL, "closed_loop") field(DOL, "WS NPP") }
record(aSub, "AS:DB:WS") { field(LFLG, "READ") field(SUBL, "WS NPP") }
record(aSub, "AS:CA:WL") { field(LFLG, "READ") field(SUBL, "WL CA") }
record(printf, "PF:CA:WD") { field(FMT, "%s") field(INP0, "WD CA") }
record(ai, "LC:K") { field(INP, {calc:{expr:"A*2", args:[{const:[1.5, 4]}]}}) }
record(bi, "LCS") { field(ZNAM, "zero") field(ONAM, "one") field(INP, {state:"lcs"}) }
record(aao, "LCO:WD") { field(FTVL, "DOUBLE") field(NELM, "2") field(OMSL, "closed_loop") field(DOL, "WD NPP") field(OUT, {calc:{expr:"VAL*2", out:{state:"lcs"}}}) }
"#;

async fn build() -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    calink_ioc::connect(&db).await;
    db
}

fn string(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        // printf's VAL is a char buffer.
        EpicsValue::CharArray(b) => {
            String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap()).into_owned()
        }
        other => format!("{other:?}"),
    }
}

fn alarm(db: &PvDatabase, rec: &str) -> (u16, AlarmSeverity) {
    let r = db.get_record(rec).unwrap();
    let i = r.read();
    (i.common.stat, i.common.sevr)
}

const NO_ALARM: (u16, AlarmSeverity) = (0, AlarmSeverity::NoAlarm);

#[epics_macros_rs::epics_test]
async fn a_one_element_read_of_an_array_converts_element_0() {
    let db = build().await;
    db.ca_sync().await;
    for rec in [
        "SI:CA:WC", "SI:CA:WL", "SI:CA:WD", "SI:CA:WS", "SI:DB:WS", "SI:DB:WD", "SI:DB:WE",
        "SO:CA:WD", "SO:DB:WS", "AS:DB:WS", "AS:CA:WL", "PF:CA:WD", "LC:K", "LCO:WD", "LCS",
    ] {
        // aSub returns `S_db_BadSub` for a name nothing registers.
        let _ = db.process_record(rec).await;
    }

    let mut wrong = Vec::new();
    for (pv, want) in [
        ("SI:CA:WC", "97"),
        ("SI:CA:WL", "7"),
        ("SI:CA:WD", "2.500000"),
        ("SI:CA:WS", "ab"),
        ("SI:DB:WS", "ab"),
        ("SI:DB:WD", "2.5"),
        ("SO:CA:WD", "2.500000"),
        ("SO:DB:WS", "ab"),
        ("AS:DB:WS.SNAM", "ab"),
        ("AS:CA:WL.SNAM", "7"),
        ("PF:CA:WD", "2.500000"),
    ] {
        let rec = pv.split('.').next().unwrap();
        let got = (string(&db, pv), alarm(&db, rec));
        if got != (want.to_string(), NO_ALARM) {
            wrong.push((pv, got));
        }
    }
    for (pv, want) in [("LC:K", 3.0), ("LCS", 1.0)] {
        let got = (db.get_pv(pv).unwrap().to_f64().unwrap(), alarm(&db, pv));
        if got != (want, NO_ALARM) {
            wrong.push((pv, (format!("{}", got.0), got.1)));
        }
    }
    if alarm(&db, "LCO:WD") != NO_ALARM {
        wrong.push(("LCO:WD", (String::new(), alarm(&db, "LCO:WD"))));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(
        alarm(&db, "SI:DB:WE"),
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "an empty array converts nothing"
    );
}
