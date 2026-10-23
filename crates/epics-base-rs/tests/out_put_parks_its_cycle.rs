//! A cycle whose lent OUT put's `processTarget` recurses, and one that parks
//! after the put while the chain's driver runs it (`drive_hops`), both hold
//! what C's `dbProcess` frame holds across `dbDbPutValue`: its PACT
//! (`dbDbLink.c:457`), its PUTF, its place on the process stack, and the
//! rest of its output stage, which takes the target's status (`dbPutLink`'s
//! `setLinkAlarm`). Each case runs at every switch from one to the other
//! (`hop_rule::sweep`), entering at a root so its first record is a target.

mod calink_ioc;
mod hop_rule;

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "R") { field(FLNK, "A") }
record(ao, "A") { field(VAL, "1") field(OUT, "B.A PP") }
record(calc, "B") { field(CALC, "VAL+1") field(SDIS, "A.PACT") }

record(calc, "P") { field(FLNK, "PA") }
record(ao, "PA") { field(VAL, "1") field(OUT, "PB.B PP") }
record(calc, "PB") { field(INPA, "PA.PUTF") field(CALC, "A") }

record(calc, "F") { field(FLNK, "FA") }
record(ao, "FA") { field(VAL, "1") field(OUT, "FB PP") }
record(ao, "FB") { field(VAL, "1") field(OUT, "FS.NAME") }
record(calc, "FS") {}

record(calc, "Q") { field(FLNK, "L1") }
record(ao, "L1") { field(VAL, "5") field(OUT, "L2 PP") }
record(ao, "L2") { field(OUT, "L1 PP") }

record(calc, "H") { field(FLNK, "X") }
record(ao, "X") { field(VAL, "3") field(OUT, "Y.A PP") }
record(calc, "Y") { field(CALC, "A*10") }
record(calc, "Z") { field(INPA, "X CP") field(INPB, "Y") field(CALC, "B") }
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

/// menuAlarmStat DISABLE and LINK; menuAlarmSevr INVALID.
const DISABLE: f64 = 18.0;
const LINK: f64 = 14.0;
const INVALID: f64 = 3.0;

#[epics_macros_rs::epics_test]
async fn an_out_parked_cycle_holds_its_pact_through_its_target() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        db.process_record("R").await.unwrap();
        assert_eq!(num(&db, "B"), 0.0, "{rule}: A.PACT read 1 while A waited");
        assert_eq!(num(&db, "B.STAT"), DISABLE, "{rule}");
        assert_eq!(num(&db, "A.PACT"), 0.0, "{rule}: put back as A resumed");
    })
    .await;
}

#[epics_macros_rs::epics_test]
async fn an_out_parked_cycle_keeps_its_putf_until_its_target_returns() {
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

/// FB's put to a `SPC_NOMOD` field fails its `write_ao`, and FB's status is
/// what `processTarget` hands back to FA's `dbPutLink`.
#[epics_macros_rs::epics_test]
async fn an_out_targets_failed_process_alarms_the_parked_writer() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        db.process_record("F").await.unwrap();
        assert_eq!(num(&db, "FA.STAT"), LINK, "{rule}");
        assert_eq!(num(&db, "FA.SEVR"), INVALID, "{rule}");
    })
    .await;
}

/// L2's `processTarget` finds L1 active and does nothing
/// (`dbDbLink.c:490-495`).
#[epics_macros_rs::epics_test]
async fn an_out_loop_through_a_parked_writer_terminates() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        epics_base_rs::runtime::task::timeout(Duration::from_secs(5), db.process_record("Q"))
            .await
            .expect("the loop through a waiting L1 terminates")
            .unwrap();
        assert_eq!(num(&db, "L2"), 5.0, "{rule}");
        assert_eq!(num(&db, "L1"), 5.0, "{rule}");
    })
    .await;
}

/// Y is processed inside X's `writeValue`, before X's `monitor()` posts the
/// event Z's CP link processes on (the dbCa worker), so Z reads Y's new
/// value.
#[epics_macros_rs::epics_test]
async fn a_pp_target_runs_before_the_cp_holder_of_its_writer() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        db.initialize_link_locality().await;
        calink_ioc::connect(&db).await;
        calink_ioc::until("the connect-time event", || num(&db, "Z.A") == 3.0).await;
        db.process_record("H").await.unwrap();
        assert_eq!(num(&db, "Y"), 30.0, "{rule}");
        calink_ioc::until("Z processed on X's event with Y's new value", || {
            num(&db, "Z") == 30.0
        })
        .await;
    })
    .await;
}
