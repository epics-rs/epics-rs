//! aSub LFLG=READ with a SUBL naming no registered subroutine (or none at all)
//! takes `apply_asub_dynamic_sub`'s skip arm: SNAM follows the link, ONAM stays,
//! the subroutine is not run, and the cycle ends NO_ALARM with VAL untouched.
//! softIoc R7.0.10.1, `AB` SUBL -> stringin "nosuch", `AE` SUBL -> stringin "",
//! `caput PROC 1` on each:
//!
//! ```text
//!          before                       after
//! AB       SNAM="" ONAM="" VAL=0        SNAM=nosuch ONAM="" VAL=0 SEVR=NO_ALARM STAT=NO_ALARM
//! AE       SNAM="" ONAM="" VAL=0        SNAM=""     ONAM="" VAL=0 SEVR=NO_ALARM STAT=NO_ALARM
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringin, "SB") { field(VAL, "nosuch") }
record(stringin, "SE") { field(VAL, "") }
record(ao, "SRC") { field(VAL, "3") }
record(aSub, "AB") { field(LFLG, "READ") field(SUBL, "SB") field(INPA, "SRC") field(FTA, "DOUBLE") field(FTVA, "DOUBLE") }
record(aSub, "AE") { field(LFLG, "READ") field(SUBL, "SE") field(INPA, "SRC") field(FTA, "DOUBLE") field(FTVA, "DOUBLE") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn get(db: &PvDatabase, pv: &str) -> EpicsValue {
    db.get_pv(pv).unwrap()
}

async fn check(db: &PvDatabase, rec: &str, snam: &str) {
    process(db, rec).await;
    assert_eq!(get(db, &format!("{rec}.SNAM")).text_g(), snam, "{rec}.SNAM");
    assert_eq!(get(db, &format!("{rec}.ONAM")).text_g(), "", "{rec}.ONAM");
    assert_eq!(
        get(db, &format!("{rec}.VAL")).to_f64(),
        Some(0.0),
        "{rec}.VAL"
    );
    assert_eq!(
        get(db, &format!("{rec}.SEVR")).to_f64(),
        Some(0.0),
        "{rec}.SEVR"
    );
    assert_eq!(
        get(db, &format!("{rec}.STAT")).to_f64(),
        Some(0.0),
        "{rec}.STAT"
    );
}

#[epics_macros_rs::epics_test]
async fn a_bad_or_empty_subl_skips_the_run_and_ends_no_alarm() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    check(&db, "AB", "nosuch").await;
    check(&db, "AE", "").await;
    // The skip is one-shot: a second cycle on the same names ends the same way.
    check(&db, "AB", "nosuch").await;
}
