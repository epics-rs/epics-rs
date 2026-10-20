//! `dbGet` takes its scalar convert whenever the source holds one element,
//! counted request or not (`dbAccess.c:967`), so a one-element `DBF_STRING`
//! source read as `DBR_ENUM` is `cvt_st_e` against the source, not
//! `getStringUshort`. A waveform has no `put_enum_str`, so the read fails.
//!
//! oracleSwaitIoc (EPICS 7.0.10), `WES1` a STRING waveform holding ["1"]:
//!
//! ```text
//! AS2E  aSub FTA=ENUM NOA=2     LINK/INVALID  NEA 2
//! WFE   waveform FTVL=ENUM      LINK/INVALID  NORD 0
//! AS2D  aSub FTA=DOUBLE NOA=2   NO_ALARM      NEA 1  A 1
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "WES1") { field(FTVL, "STRING") field(NELM, "4") }
record(aSub, "AS2E") { field(FTA, "ENUM") field(NOA, "2") field(INPA, "WES1 NPP") }
record(waveform, "WFE") { field(FTVL, "ENUM") field(NELM, "2") field(INP, "WES1 NPP") }
record(aSub, "AS2D") { field(FTA, "DOUBLE") field(NOA, "2") field(INPA, "WES1 NPP") }
"#;

fn got(db: &PvDatabase, rec: &str, f: &str) -> (f64, u16, AlarmSeverity) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    let v = inst.record.get_field(f).unwrap();
    let v = v.first_element().unwrap_or(v).to_f64().unwrap();
    (v, inst.common.stat, inst.common.sevr)
}

#[epics_macros_rs::epics_test]
async fn a_one_element_string_source_read_as_enum_is_cvt_st_e() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify(
        "WES1",
        "VAL",
        EpicsValue::StringArray(vec!["1".into()].into()),
    )
    .await
    .unwrap();
    for rec in ["AS2E", "WFE", "AS2D"] {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        let _ = db.process_record_with_links(rec, &mut visited).await;
    }
    let link = (alarm_status::LINK_ALARM, AlarmSeverity::Invalid);
    let ok = (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm);
    let at = |(v, stat, sevr): (f64, u16, AlarmSeverity)| (v, (stat, sevr));
    assert_eq!(
        [
            at(got(&db, "AS2E", "NEA")),
            at(got(&db, "WFE", "NORD")),
            at(got(&db, "AS2D", "NEA")),
            at(got(&db, "AS2D", "A")),
        ],
        [(2.0, link), (0.0, link), (1.0, ok), (1.0, ok)],
        "AS2E.NEA, WFE.NORD, AS2D.NEA, AS2D.A"
    );
}
