//! `sel` runs `do_sel` only when `fetch_values` succeeded — in EVERY SELM.
//!
//! ```c
//! /* selRecord.c:112-115 */
//! if ( RTN_SUCCESS(fetch_values(prec)) ) {
//!     do_sel(prec);
//! }
//! /* selRecord.c:433-436 — the all-inputs loop */
//! for(i=0; i<SEL_MAX; i++, plink++, pvalue++) {
//!     status=dbGetLink(plink,DBR_DOUBLE, pvalue,0,0);
//! }
//! return(status);
//! ```
//!
//! `status` is assigned unguarded on every pass, so `fetch_values` returns the
//! LAST link's status — INPL's when every input is read. That is the whole
//! gate: a dead INPA is read, posted, and then ignored, while a dead INPL
//! freezes VAL/SELN even though INPA delivered fine. The asymmetry is C's, out
//! of the overwritten `status`; the port matches it rather than substituting an
//! "any input failed" rule of its own.
//!
//! `Specified` mode is the same rule over a one-link fetch list
//! (`selRecord.c:421-431`): `dbGetLink(&nvl, ...)` first, an early
//! `return(status)` on failure, then the selected `INP[SELN]`. NVL is read
//! ONLY there — the all-inputs loop never touches it — so a dead NVL is
//! invisible to High/Low/Median.
//!
//! Boundaries, one per gate input, not one per narrative:
//!   * last link fails / last link fine, in each of the four SELM values;
//!   * a non-last link fails (the quirk) — selection must still RUN;
//!   * NVL fails in `Specified` (gates) vs in `High Signal` (never read).

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SEL:SRC7") { field(INP, "7") field(VAL, "7") }
record(ai, "SEL:SRC3") { field(INP, "3") field(VAL, "3") }

record(sel, "SEL:HIGH:DEADL") {
    field(SELM, "High Signal")
    field(INPA, "SEL:SRC7") field(INPL, "NO:SUCH:RECORD.VAL")
}
record(sel, "SEL:LOW:DEADL") {
    field(SELM, "Low Signal")
    field(INPA, "SEL:SRC7") field(INPL, "NO:SUCH:RECORD.VAL")
}
record(sel, "SEL:MED:DEADL") {
    field(SELM, "Median Signal")
    field(INPA, "SEL:SRC7") field(INPL, "NO:SUCH:RECORD.VAL")
}
record(sel, "SEL:HIGH:DEADA") {
    field(SELM, "High Signal")
    field(INPA, "NO:SUCH:RECORD.VAL") field(INPB, "SEL:SRC7")
}
record(sel, "SEL:HIGH:OK") {
    field(SELM, "High Signal")
    field(INPA, "SEL:SRC7") field(INPB, "SEL:SRC3")
}
record(sel, "SEL:LOW:OK") {
    field(SELM, "Low Signal")
    field(INPA, "SEL:SRC7") field(INPB, "SEL:SRC3")
}
record(sel, "SEL:MED:OK") {
    field(SELM, "Median Signal")
    field(INPA, "SEL:SRC7")
}
record(sel, "SEL:SPEC:OK") {
    field(SELM, "Specified") field(SELN, "1")
    field(INPA, "SEL:SRC7") field(INPB, "SEL:SRC3")
}
record(sel, "SEL:SPEC:DEADSEL") {
    field(SELM, "Specified") field(SELN, "0")
    field(INPA, "NO:SUCH:RECORD.VAL")
}
record(sel, "SEL:SPEC:DEADNVL") {
    field(SELM, "Specified")
    field(NVL, "NO:SUCH:RECORD.VAL")
    field(INPA, "SEL:SRC7")
}
record(sel, "SEL:HIGH:DEADNVL") {
    field(SELM, "High Signal")
    field(NVL, "NO:SUCH:RECORD.VAL")
    field(INPA, "SEL:SRC7")
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

/// Seed VAL without processing, then process once. The seed is the sentinel
/// that says "`do_sel` did not run".
async fn seed_and_process(db: &PvDatabase, rec: &str, seed: f64) -> f64 {
    db.put_pv_no_process(rec, EpicsValue::Double(seed))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64())
        .unwrap()
}

fn alarm(db: &PvDatabase, rec: &str) -> (AlarmSeverity, u16) {
    let r = db.get_record(rec).unwrap();
    let c = &r.read().common;
    (c.sevr, c.stat)
}

/// Boundary: INPL — the LAST link — fails. `fetch_values` returns its status,
/// so `do_sel` is skipped and VAL keeps the sentinel, in all three
/// non-Specified modes.
#[epics_macros_rs::epics_test]
async fn a_failed_last_link_freezes_every_non_specified_mode() {
    for rec in ["SEL:HIGH:DEADL", "SEL:LOW:DEADL", "SEL:MED:DEADL"] {
        let db = build().await;
        assert_eq!(
            seed_and_process(&db, rec, 42.0).await,
            42.0,
            "{rec}: INPL failed, so `fetch_values` returned non-zero and \
             `do_sel` must not have run"
        );
    }
}

/// Boundary: a NON-last link fails. C's loop overwrites `status` with INPL's
/// success, so the selection RUNS on the inputs that did resolve — the dead
/// INPA contributes its init NaN and is skipped by `!isnan`.
#[epics_macros_rs::epics_test]
async fn a_failed_non_last_link_does_not_freeze_the_selection() {
    let db = build().await;
    assert_eq!(
        seed_and_process(&db, "SEL:HIGH:DEADA", 42.0).await,
        7.0,
        "INPA failed but INPL (unset) succeeded, so C's `status` is 0 and \
         High picks INPB"
    );
    // The failed read still raises its own `setLinkAlarm` (`dbLink.c:339`).
    assert_eq!(
        alarm(&db, "SEL:HIGH:DEADA"),
        (AlarmSeverity::Invalid, alarm_status::LINK_ALARM)
    );
}

/// Boundary: every link fine. The selection runs in each mode — the gate must
/// not fire on an UNSET link, which `dbGetLink` answers with success.
#[epics_macros_rs::epics_test]
async fn every_link_resolving_runs_the_selection_in_every_mode() {
    let db = build().await;
    assert_eq!(seed_and_process(&db, "SEL:HIGH:OK", 42.0).await, 7.0);
    assert_eq!(seed_and_process(&db, "SEL:LOW:OK", 42.0).await, 3.0);
    assert_eq!(seed_and_process(&db, "SEL:MED:OK", 42.0).await, 7.0);
    assert_eq!(seed_and_process(&db, "SEL:SPEC:OK", 42.0).await, 3.0);
}

/// Boundary: `Specified` mode's one-link fetch list. The selected input IS the
/// last link read, so its failure gates — unchanged by this fix, and the guard
/// that the uniform rule did not lose the mode that already had it.
#[epics_macros_rs::epics_test]
async fn specified_mode_still_freezes_on_its_selected_input() {
    let db = build().await;
    assert_eq!(seed_and_process(&db, "SEL:SPEC:DEADSEL", 42.0).await, 42.0);
}

/// Boundary: NVL. C reads it only inside `if (selm == Specified)`
/// (`selRecord.c:420-421`), so a dead NVL gates there and is invisible
/// everywhere else — no gate, and no `setLinkAlarm` from a link C never reads.
#[epics_macros_rs::epics_test]
async fn a_dead_nvl_gates_specified_and_is_never_read_by_high() {
    let db = build().await;
    assert_eq!(seed_and_process(&db, "SEL:SPEC:DEADNVL", 42.0).await, 42.0);
    assert_eq!(
        alarm(&db, "SEL:SPEC:DEADNVL"),
        (AlarmSeverity::Invalid, alarm_status::LINK_ALARM),
        "the NVL `dbGetLink` failed, so `setLinkAlarm` ran"
    );

    assert_eq!(
        seed_and_process(&db, "SEL:HIGH:DEADNVL", 42.0).await,
        7.0,
        "High never reads NVL, so its failure cannot gate the selection"
    );
    assert_eq!(
        alarm(&db, "SEL:HIGH:DEADNVL"),
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "a link C never reads cannot raise `setLinkAlarm`"
    );
}
