//! A `.db` value for an analog severity menu (`HHSV`/`HSV`/`LSV`/`LLSV`) is
//! C `dbPutStringNum`'s `DBF_MENU` arm (`dbStaticRun.c:484-512`): the choice
//! label, else `epicsParseUInt16(pstring, &value, 0, NULL)`, refused with
//! `S_db_badChoice` when that fails and `S_dbLib_badField` past the menu.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn load(
    db: &str,
) -> Result<std::sync::Arc<epics_base_rs::server::database::PvDatabase>, String> {
    let builder = IocBuilder::new()
        .db_string(db, &std::collections::HashMap::new())
        .map_err(|e| e.to_string())?;
    Ok(builder.build().await.map_err(|e| e.to_string())?.0)
}

fn ordinal(v: EpicsValue) -> f64 {
    v.to_f64().unwrap()
}

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran and passed on
// the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
// -p epics-base-rs --all-features`).
#[tokio::test]
async fn a_severity_index_string_is_epics_parse_uint16_base_0() {
    let db = load(r#"record(ai, "A") { field(HHSV, "0x2") field(HSV, " 1 ") field(LSV, "3") }"#)
        .await
        .unwrap();
    assert_eq!(ordinal(db.get_pv("A.HHSV").unwrap()), 2.0);
    assert_eq!(ordinal(db.get_pv("A.HSV").unwrap()), 1.0);
    assert_eq!(ordinal(db.get_pv("A.LSV").unwrap()), 3.0);
}

#[tokio::test]
async fn a_severity_string_that_is_no_choice_fails_the_load() {
    for field in [r#"field(LLSV, "junk")"#, r#"field(HHSV, "7")"#] {
        let db = format!(r#"record(ai, "A") {{ {field} }}"#);
        assert!(load(&db).await.is_err(), "{field} must not load");
    }
}
