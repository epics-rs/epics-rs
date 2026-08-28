//! TSEL is resolved where the record stamps itself, not at the head of the
//! cycle.
//!
//! `recGblGetTimeStamp` (`recGbl.c:305-308`) is one function: the TSEL
//! resolution (`recGbl.c:315-323`) and the TSE→TIME event lookup
//! (`recGbl.c:324-342`) happen together, at the point the record calls it.
//! For `calc` that point is `calcRecord.c:127`, AFTER `fetch_values(prec)`
//! (`:120`) has walked `INPA..INPL` — so an `INPn` marked `PP` has already
//! processed its target by the time TSEL is read.
//!
//! ```c
//!     prec->pact = TRUE;
//!     if (fetch_values(prec) == 0) { ... }        /* calcRecord.c:120 */
//!     timeLast = prec->time;
//!     recGblGetTimeStamp(prec);                   /* calcRecord.c:127 */
//! ```
//!
//! The cycle used to resolve TSEL once at its head, before any input link ran,
//! so a record whose TSEL source is the same record its `INPn PP` drives read
//! the source's PREVIOUS state. `seq_restamps_time_per_group.rs` already pins
//! the same rule for `seq`'s per-group restamp (`seqRecord.c:261`); this pins
//! it for the ordinary one-stamp cycle.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(calc, "TSP:SRC") { field(CALC, "1") }
record(calc, "TSP:DST") {
    field(CALC, "1")
    field(INPA, "TSP:SRC.VAL PP")
    field(TSEL, "TSP:SRC.TIME")
}

record(calc, "TSP:VSRC") { field(CALC, "-2") }
record(calc, "TSP:VDST") {
    field(CALC, "1")
    field(INPA, "TSP:VSRC.VAL PP")
    field(TSEL, "TSP:VSRC.VAL")
}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn time_of(db: &PvDatabase, rec: &str) -> SystemTime {
    db.get_record(rec).unwrap().read().common.time
}

fn tse_of(db: &PvDatabase, rec: &str) -> i16 {
    db.get_record(rec).unwrap().read().common.tse
}

/// `TSEL="TSP:SRC.TIME"` with `INPA="TSP:SRC.VAL PP"`: `fetch_values` restamps
/// `TSP:SRC` first, so the `DBLINK_FLAG_TSELisTIME` copy at `recGbl.c:317`
/// hands `TSP:DST` the source's NEW time. Resolving TSEL at the cycle head
/// instead copies the time the source carried before this cycle touched it.
#[epics_macros_rs::epics_test]
async fn a_time_tsel_adopts_the_source_stamp_its_own_pp_input_just_made() {
    let db = build().await;

    // Give the source a stamp that is measurably older than the one its
    // in-cycle reprocess will produce.
    process(&db, "TSP:SRC").await;
    let stale = time_of(&db, "TSP:SRC");
    epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;

    process(&db, "TSP:DST").await;

    let fresh = time_of(&db, "TSP:SRC");
    assert!(
        fresh > stale,
        "INPA's PP must have reprocessed TSP:SRC: stale={stale:?} fresh={fresh:?}"
    );
    assert_eq!(
        time_of(&db, "TSP:DST"),
        fresh,
        "recGblGetTimeStamp runs after fetch_values (calcRecord.c:120-127), so \
         the .TIME TSEL copies the stamp TSP:SRC got inside this cycle"
    );
}

/// The non-`.TIME` arm, same ordering: `dbGetLink(&prec->tsel, DBR_SHORT,
/// &prec->tse)` (`recGbl.c:322`) reads the source AFTER `fetch_values` drove
/// it. `TSP:VSRC.VAL` moves 0 → -2 on its first process, so `TSP:VDST.TSE`
/// ends the cycle at -2; a head-of-cycle resolve leaves it at 0.
#[epics_macros_rs::epics_test]
async fn a_value_tsel_reads_the_source_its_own_pp_input_just_processed() {
    let db = build().await;

    process(&db, "TSP:VDST").await;

    assert_eq!(
        db.get_record("TSP:VSRC")
            .unwrap()
            .read()
            .record
            .get_field("VAL"),
        Some(epics_base_rs::types::EpicsValue::Double(-2.0)),
        "INPA's PP must have processed TSP:VSRC"
    );
    assert_eq!(
        tse_of(&db, "TSP:VDST"),
        -2,
        "TSE is loaded from TSEL at the stamp point, after fetch_values"
    );
}
