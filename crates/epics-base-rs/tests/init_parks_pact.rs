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
//! PACT stays TRUE for as long as SNAM stays empty, so `dbProcess` takes its
//! PACT-active branch on every scan and record support does not run. It is how
//! C disables a `sub` with no subroutine — the record still serves its fields,
//! it just cannot process. The park is released by an SNAM put and by nothing
//! else (`subRecord.c:170-194`, covered in
//! `sub_snam_put_moves_the_pact_park.rs`). Measured on the compiled softIoc:
//!
//! ```text
//! $ caget -t P:SUB.PACT   -> 1        (record(sub,"P:SUB"){}, no SNAM)
//! $ caget -t P:SUB.VAL    -> 0
//! ```
//!
//! The boundary is SNAM: empty parks, non-empty does not. PACT is a `dbCommon`
//! field with one owner, so the transition happens in the init owner
//! (`RecordInstance::run_init_passes`) off the record's `Record::parks_pact`,
//! not by a record reaching into common state. The put owner re-asks the same
//! predicate either side of an SNAM put, which is C's two-pass `special()`.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::types::EpicsValue;

async fn pact_of(db: &PvDatabase, name: &str) -> EpicsValue {
    let arc = db.get_record(name).expect("record exists");
    let inst = arc.read();
    inst.client_field_value("PACT").expect("PACT resolves")
}

#[epics_macros_rs::epics_test]
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

#[epics_macros_rs::epics_test]
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

/// The park is not a one-shot flag a later process clears: no process cycle
/// releases it — only an SNAM put does — so every scan hits the PACT-active
/// branch and VAL never moves.
#[epics_macros_rs::epics_test]
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
