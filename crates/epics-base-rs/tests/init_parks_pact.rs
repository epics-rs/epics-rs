//! A record whose `init_record` cannot possibly process is PARKED, not idle.
//!
//! C `subRecord.c:119-123`:
//!
//! ```c
//!     if (prec->snam[0] == 0) {
//!         epicsPrintf("%s.SNAM is empty\n", prec->name);
//!         prec->pact = TRUE;
//!         return 0;
//!     }
//! ```
//!
//! PACT stays TRUE for the life of the IOC, so `dbProcess` takes its
//! PACT-active branch on every scan and record support never runs again. It is
//! how C disables a `sub` with no subroutine — the record still serves its
//! fields, it just cannot process. Measured on the compiled softIoc:
//!
//! ```text
//! $ caget -t P:SUB.PACT   -> 1        (record(sub,"P:SUB"){}, no SNAM)
//! $ caget -t P:SUB.VAL    -> 0
//! ```
//!
//! The boundary is SNAM: empty parks, non-empty does not. PACT is a `dbCommon`
//! field with one owner, so the transition happens in the init owner
//! (`RecordInstance::run_init_passes`) off the record's `init_record_parks_pact`,
//! not by a record reaching into common state.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::types::EpicsValue;

async fn pact_of(db: &PvDatabase, name: &str) -> EpicsValue {
    let arc = db.get_record(name).expect("record exists");
    let inst = arc.read();
    inst.client_field_value("PACT").expect("PACT resolves")
}

#[tokio::test]
async fn r21_a_sub_with_no_snam_is_parked_active() {
    let db = PvDatabase::new();
    db.add_record("SUB_BARE", Box::new(SubRecord::default()))
        .await
        .unwrap();
    assert_eq!(
        pact_of(&db, "SUB_BARE").await,
        EpicsValue::UChar(1),
        "an empty SNAM has no subroutine: C parks PACT at init"
    );
}

#[tokio::test]
async fn r21_a_sub_with_an_snam_is_not_parked() {
    let db = PvDatabase::new();
    let mut rec = SubRecord::default();
    rec.put_field("SNAM", EpicsValue::String("mySub".into()))
        .unwrap();
    db.add_record("SUB_NAMED", Box::new(rec)).await.unwrap();
    assert_eq!(
        pact_of(&db, "SUB_NAMED").await,
        EpicsValue::UChar(0),
        "a named subroutine is the non-parking half of the boundary"
    );
}

/// The park is not a one-shot flag a later process clears: C never releases it,
/// so every scan from then on hits the PACT-active branch and VAL never moves.
#[tokio::test]
async fn r21_a_parked_sub_never_runs_record_support() {
    let db = PvDatabase::new();
    db.add_record("SUB_DEAD", Box::new(SubRecord::default()))
        .await
        .unwrap();

    let mut visited = std::collections::HashSet::new();
    let _ = db
        .process_record_with_links("SUB_DEAD", &mut visited, 0)
        .await;

    assert_eq!(
        pact_of(&db, "SUB_DEAD").await,
        EpicsValue::UChar(1),
        "processing a parked record must not release the park"
    );
}
