//! `longout` OOPT: PVAL advances on every cycle that reaches C's
//! `conditional_write`, not only on cycles that wrote.
//!
//! `longoutRecord.c:489-493` is `if (doDevSupWrite) status =
//! pdset->write_longout(prec); prec->pval = prec->val; prec->outpvt =
//! DONT_EXEC_OUTPUT;` — the latch is outside the `if`, and that is the entire
//! mechanism the transition modes rest on: `Transition_To_Zero` can only fire
//! on the cycle where VAL reaches 0 while PVAL does not, which requires the
//! earlier nonzero cycle — the one that wrote nothing — to have latched.
//!
//! Boundaries: the suppressed cycle that must latch, both transition modes,
//! `init_record`'s own seed (C `:126`), and the two cycles C reaches
//! `conditional_write` on but the port used to skip.

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(longin, "TGT") {
    field(VAL, "7")
}
record(longout, "TOZERO") {
    field(OOPT, "Transition To Zero")
    field(OUT, "TGT.VAL PP")
}
record(longin, "TGT2") {
    field(VAL, "7")
}
record(longout, "TONONZERO") {
    field(OOPT, "Transition To Non-zero")
    field(OUT, "TGT2.VAL PP")
}
record(longout, "SEEDED") {
    field(VAL, "5")
    field(OOPT, "Transition To Zero")
}
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// `caput LO 5` writes nothing (5 is not a transition to zero) but must still
/// latch PVAL=5; `caput LO 0` is then the transition and drives OUT.
#[epics_macros_rs::epics_test]
async fn a_suppressed_cycle_still_latches_pval_so_the_transition_fires() {
    let db = build().await;

    db.put_pv("TOZERO", EpicsValue::Long(5)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TOZERO", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(7.0),
        "5 is not a transition to zero — no write"
    );
    assert_eq!(
        db.get_pv("TOZERO.PVAL").unwrap().to_f64(),
        Some(5.0),
        "C `longoutRecord.c:492` latches outside `if (doDevSupWrite)`"
    );

    db.put_pv("TOZERO", EpicsValue::Long(0)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TOZERO", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(0.0),
        "5 → 0 is the transition: OUT is driven"
    );
}

/// The symmetric mode, whose suppressed cycle is the zero one.
#[epics_macros_rs::epics_test]
async fn transition_to_non_zero_fires_after_a_suppressed_zero_cycle() {
    let db = build().await;

    db.put_pv("TONONZERO", EpicsValue::Long(0)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TONONZERO", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("TGT2").unwrap().to_f64(),
        Some(7.0),
        "0 → 0 is not a transition"
    );

    db.put_pv("TONONZERO", EpicsValue::Long(3)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TONONZERO", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("TGT2").unwrap().to_f64(),
        Some(3.0),
        "0 → 3 is the transition: OUT is driven"
    );
}

/// C `longoutRecord.c:126` — `init_record` seeds PVAL from the loaded VAL, so
/// a `field(VAL,"5")` longout is already "at 5" before its first cycle.
#[epics_macros_rs::epics_test]
async fn init_record_seeds_pval_from_the_loaded_val() {
    let db = build().await;

    assert_eq!(
        db.get_pv("SEEDED.PVAL").unwrap().to_f64(),
        Some(5.0),
        "C seeds PVAL alongside MLST/ALST/LALM"
    );
}
