//! A cycle whose DB FLNK target recurses, and one that parks at it while the
//! chain's driver runs the target (`drive_hops`), both hold what C's
//! `dbProcess` frame holds across `dbScanFwdLink`: its PACT
//! (`dbDbLink.c:457`), its place on the process stack, and the rest of its
//! cycle (`recGblFwdLink`'s `putf = FALSE`, `recGbl.c:302`) until the target
//! has returned. Each case runs at every switch from one to the other
//! (`hop_rule::sweep`), entering at a root so its first record is a target.

mod hop_rule;

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "R") { field(FLNK, "A") }
record(calc, "A") { field(CALC, "VAL+1") field(FLNK, "B") }
record(calc, "B") { field(CALC, "VAL+1") field(SDIS, "A.PACT") }

record(calc, "Q") { field(FLNK, "L1") }
record(calc, "L1") { field(CALC, "VAL+1") field(FLNK, "L2") }
record(calc, "L2") { field(CALC, "VAL+1") field(FLNK, "L1") }

record(calc, "CNT") { field(CALC, "VAL+1") }
record(calc, "H") { field(FLNK, "X") }
record(ao, "X") { field(OUT, "Y PP") field(FLNK, "Z") }
record(calc, "Y") { field(INPA, "CNT PP") field(CALC, "A") field(FLNK, "W") }
record(calc, "W") { field(INPA, "CNT PP") field(CALC, "A") }
record(calc, "Z") { field(INPA, "CNT PP") field(CALC, "A") }

record(calc, "P") { field(FLNK, "PA") }
record(calc, "PA") { field(FLNK, "PB") }
record(calc, "PB") { field(INPA, "PA.PUTF") field(CALC, "A") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// menuAlarmStat DISABLE.
const DISABLE: f64 = 18.0;

#[epics_macros_rs::epics_test]
async fn a_parked_cycle_holds_its_pact_through_its_target() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        db.process_record("R").await.unwrap();
        assert_eq!(num(&db, "A"), 1.0, "{rule}");
        assert_eq!(num(&db, "B"), 0.0, "{rule}: A.PACT read 1 while A waited");
        assert_eq!(num(&db, "B.STAT"), DISABLE, "{rule}");
        assert_eq!(num(&db, "A.PACT"), 0.0, "{rule}: put back as A resumed");
    })
    .await;
}

/// C's `dbProcess` finds `L1->pact` set and returns (`dbAccess.c:536`).
#[epics_macros_rs::epics_test]
async fn a_parked_cycle_stays_on_the_process_stack() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, db.process_record("Q"))
            .await
            .expect("the loop through a waiting L1 terminates")
            .unwrap();
        assert_eq!(num(&db, "L1"), 1.0, "{rule}");
        assert_eq!(num(&db, "L2"), 1.0, "{rule}");
    })
    .await;
}

/// aoRecord's `writeValue` processes Y, Y's FLNK included, before X's
/// `recGblFwdLink`; softIoc numbers them Y=1, W=2, Z=3.
#[epics_macros_rs::epics_test]
async fn the_out_target_chain_runs_before_the_writers_forward_link() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        db.process_record("H").await.unwrap();
        assert_eq!(
            (num(&db, "Y"), num(&db, "W"), num(&db, "Z")),
            (1.0, 2.0, 3.0),
            "{rule}"
        );
    })
    .await;
}

/// `processTarget` copies the source's PUTF to the target, and PA clears its
/// own only after `dbScanFwdLink` returns.
#[epics_macros_rs::epics_test]
async fn a_parked_cycle_keeps_its_putf_until_its_target_returns() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        db.put_record_field_from_ca("P", "PROC", EpicsValue::Long(1))
            .await
            .unwrap();
        assert_eq!(
            num(&db, "PB"),
            1.0,
            "{rule}: PA.PUTF read 1 while PA waited"
        );
        assert_eq!(num(&db, "PA.PUTF"), 0.0, "{rule}");
    })
    .await;
}
