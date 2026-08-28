//! `seq` restamps TIME once per link group, not once per cycle.
//!
//! C `seqRecord.c::processCallback` (`:243-274`) is three statements in this
//! order for EVERY selected group:
//!
//! ```c
//!     dbGetLink(&pgrp->dol, DBR_DOUBLE, &pgrp->dov, 0, 0);   /* :259 */
//!     recGblGetTimeStamp(prec);                              /* :261 */
//!     dbPutLink(&pgrp->lnk, DBR_DOUBLE, &pgrp->dov, 1);      /* :264 */
//! ```
//!
//! `process` (`seqRecord.c:133-148`) stamps nothing at all; the only other
//! `recGblGetTimeStamp` in the file is `asyncFinish`'s (`:224`). So a `seq`
//! whose groups wait out their `DLYn` drives each target with the time of
//! THAT hop — the record's TIME advances across the chain — and a downstream
//! record with `field(TSEL, "SEQ.TIME")` sees it move.
//!
//! `recGblGetTimeStamp` is one function in C: TSEL resolution
//! (`recGbl.c:315-323`) THEN the TSE→TIME event lookup (`:324-342`). The port
//! splits the halves across the cycle, so the boundaries below pin both — the
//! restamp must go through the TSE owner (TSE=-2 leaves TIME alone) and must
//! re-resolve TSEL per group (a `.TIME` TSEL re-copies its source).

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ai, "SEQ:T0") { field(INP, "0") field(TSEL, "SEQ:CHAIN.TIME") }
record(ai, "SEQ:T1") { field(INP, "0") field(TSEL, "SEQ:CHAIN.TIME") }
record(seq, "SEQ:CHAIN") {
    field(SELM, "All")
    field(DLY0, "0.05") field(LNK0, "SEQ:T0.VAL PP")
    field(DLY1, "0.05") field(LNK1, "SEQ:T1.VAL PP")
}

record(ai, "SEQ:D0") { field(INP, "0") field(TSEL, "SEQ:DEVT.TIME") }
record(ai, "SEQ:D1") { field(INP, "0") field(TSEL, "SEQ:DEVT.TIME") }
record(seq, "SEQ:DEVT") {
    field(SELM, "All")
    field(TSE,  "-2")
    field(DLY0, "0.05") field(LNK0, "SEQ:D0.VAL PP")
    field(DLY1, "0.05") field(LNK1, "SEQ:D1.VAL PP")
}

record(ai, "SEQ:SRC") { field(INP, "0") }
record(ai, "SEQ:U1")  { field(INP, "0") field(TSEL, "SEQ:TSELCHAIN.TIME") }
record(seq, "SEQ:TSELCHAIN") {
    field(SELM, "All")
    field(TSEL, "SEQ:SRC.TIME")
    field(DLY0, "0.05") field(LNK0, "SEQ:SRC.VAL PP")
    field(DLY1, "0.05") field(LNK1, "SEQ:U1.VAL PP")
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

/// Process the `seq` and wait out its whole `DLYn` chain — C `process` returns
/// with `pact = TRUE` (`seqRecord.c:143`) and `asyncFinish` (`:219-241`)
/// clears it from the last hop.
async fn run_chain(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
    for _ in 0..400 {
        if !db.get_record(rec).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{rec} never finished its DLYn chain");
}

fn time_of(db: &PvDatabase, rec: &str) -> SystemTime {
    db.get_record(rec).unwrap().read().common.time
}

/// The boundary the port loses without the restamp: two groups separated by a
/// real `DLYn` must NOT share one timestamp. Each target adopts the `seq`'s
/// TIME through `TSEL="SEQ:CHAIN.TIME"`, so the later group's target carries a
/// strictly later time.
#[epics_macros_rs::epics_test]
async fn a_delayed_group_drives_its_target_with_its_own_hop_time() {
    let db = build().await;
    run_chain(&db, "SEQ:CHAIN").await;

    let t0 = time_of(&db, "SEQ:T0");
    let t1 = time_of(&db, "SEQ:T1");
    assert!(
        t1 > t0,
        "group 1 ran a DLY1 later than group 0, so `recGblGetTimeStamp` \
         (seqRecord.c:261) must have moved TIME between the two puts: \
         t0={t0:?} t1={t1:?}"
    );
}

/// TSE=-2 (`epicsTimeEventDeviceTime`) is the one value `recGblGetTimeStampSimm`
/// leaves TIME alone for (`recGbl.c:324-342`). The per-group restamp goes
/// through that owner, so a TSE=-2 `seq` publishes ONE time across the whole
/// chain — a restamp that read the clock directly would fail here.
#[epics_macros_rs::epics_test]
async fn tse_device_time_makes_the_per_group_restamp_a_no_op() {
    let db = build().await;
    run_chain(&db, "SEQ:DEVT").await;

    assert_eq!(
        time_of(&db, "SEQ:D0"),
        time_of(&db, "SEQ:D1"),
        "TSE=-2 means `epicsTimeGetEvent` is never called, so both groups \
         publish the record's untouched TIME"
    );
}

/// `recGblGetTimeStamp` re-reads TSEL on every call (`recGbl.c:315-323`), so a
/// `seq` whose TSEL is a `.TIME` link re-copies its source per group. Group 0
/// drives `SEQ:SRC` with `PP`, moving that source's timestamp; group 1's
/// restamp must therefore hand `SEQ:U1` the NEW source time, not the one the
/// cycle head resolved.
#[epics_macros_rs::epics_test]
async fn the_restamp_re_resolves_a_time_tsel_per_group() {
    let db = build().await;
    run_chain(&db, "SEQ:TSELCHAIN").await;

    let src = time_of(&db, "SEQ:SRC");
    let u1 = time_of(&db, "SEQ:U1");
    assert_eq!(
        u1, src,
        "group 1's `recGblGetTimeStamp` re-resolves TSEL=\"SEQ:SRC.TIME\" \
         after group 0 processed SEQ:SRC, so SEQ:U1 adopts the source's \
         post-group-0 time"
    );
    assert!(
        src > SystemTime::UNIX_EPOCH,
        "SEQ:SRC must actually have been processed by group 0"
    );
}
