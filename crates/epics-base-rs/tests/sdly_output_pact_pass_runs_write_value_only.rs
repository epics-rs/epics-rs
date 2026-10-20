//! An output's SDLY defer re-enters `process()` with PACT set
//! (`aoRecord.c:585`), where `if (!pact)` skips the DOL fetch and `convert`:
//! the delayed SIOL write carries the first pass's value, and OROC steps once.
//!
//! softIoc 7.0.10, `SW` a longin at 1, `SDLY` 0.05, a 0.3 s wait:
//!
//! ```text
//! dbpf AO:DOL.PROC 1; dbpf SRC 9     TGT=5   AO:DOL.VAL=5   STAT=SIMM SEVR=MINOR
//! AO:OROC VAL 10 -> 20 (OROC 1)      TGT2=11 AO:OROC.OVAL=11
//! ```

use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(longin, "SW") { field(VAL, "1") }
record(ao, "SRC") { field(VAL, "5") }
record(ao, "TGT") {}
record(ao, "AO:DOL") { field(OMSL, "closed_loop") field(DOL, "SRC NPP") field(SIML, "SW") field(SIOL, "TGT") field(SIMS, "MINOR") field(SDLY, "0.05") }
record(ao, "TGT2") {}
record(ao, "AO:OROC") { field(VAL, "10") field(OROC, "1") field(SIML, "SW") field(SIOL, "TGT2") field(SIMS, "MINOR") field(SDLY, "0.05") }
"#;

fn double(db: &epics_base_rs::server::database::PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn settle() {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
}

#[epics_macros_rs::epics_test]
async fn the_delayed_write_keeps_the_first_pass_value() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AO:DOL", &mut visited)
        .await
        .unwrap();
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(9.0))
        .await
        .unwrap();
    settle().await;
    let rec = db.get_record("AO:DOL").unwrap();
    assert!(!rec.read().is_processing());
    assert_eq!((double(&db, "TGT"), double(&db, "AO:DOL")), (5.0, 5.0));
    let (stat, sevr) = {
        let i = rec.read();
        (i.common.stat, i.common.sevr)
    };
    assert_eq!(
        (stat, sevr),
        (alarm_status::SIMM_ALARM, AlarmSeverity::Minor)
    );

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AO:OROC", &mut visited)
        .await
        .unwrap();
    settle().await;
    assert_eq!(double(&db, "TGT2"), 10.0);
    db.put_record_field_from_ca("AO:OROC", "VAL", EpicsValue::Double(20.0))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        (double(&db, "TGT2"), double(&db, "AO:OROC.OVAL")),
        (11.0, 11.0)
    );
}
