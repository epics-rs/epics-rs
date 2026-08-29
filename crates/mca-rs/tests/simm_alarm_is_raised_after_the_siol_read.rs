//! `mca` raises `SIMM_ALARM` AFTER its SIOL read; the base records raise it
//! before. With `SIMS = INVALID` that order is the whole result.
//!
//! `recGblSetSevr` is strict-greater (`recGbl.c`), and a failed `dbGetLink`
//! raises LINK_ALARM/INVALID inside itself:
//!
//! ```c
//! /* dbLink.c:316-321, reached from dbGetLink at :337 */
//! static void setLinkAlarm(struct link* plink)
//! {
//!     recGblSetSevrMsg(plink->precord, LINK_ALARM, INVALID_ALARM,
//!                      "field %s", dbLinkFieldName(plink));
//! }
//! ```
//!
//! So whichever of the two INVALID_ALARMs is raised FIRST wins the tie and
//! becomes the record's STAT. The two orders in C:
//!
//! ```c
//! /* longinRecord.c:413-416 — raise, THEN read: STAT stays SIMM_ALARM */
//! case menuYesNoYES: {
//!     recGblSetSevr(prec, SIMM_ALARM, prec->sims);
//!     ...
//!         status = dbGetLink(&prec->siol, DBR_LONG, &prec->sval, 0, 0);
//!
//! /* mcaRecord.c:1116-1129 — read, THEN raise: LINK_ALARM wins */
//! if (pmca->simm == menuYesNoYES) {
//!     nRequest = pmca->nmax;
//!     status = dbGetLink(&(pmca->siol), pmca->ftvl, pmca->bptr, NULL, &nRequest);
//!     if (pmca->siol.type == DB_LINK) pmca->nord = nRequest;
//!     if (status == 0) {
//!         pmca->udf = FALSE;
//!     }
//! } else {
//!     status=-1;
//!     recGblSetSevr(pmca,SOFT_ALARM,INVALID_ALARM);
//!     return(status);
//! }
//! recGblSetSevr(pmca,SIMM_ALARM,pmca->sims);
//! ```
//!
//! The port raised SIMM first for every record, so a broken SIOL on an mca
//! published `STAT = SIMM_ALARM` where C publishes `STAT = LINK_ALARM` with
//! `AMSG = "field SIOL"` — the operator lost the only indication that the
//! simulation source itself was gone. `Record::raises_simm_after_read` is the
//! per-record declaration of that order; `mca` overrides it to `true`.
//!
//! C read against `mca` at `687d563` and `epics-base` at `8f5015b66`.
//! Boundaries: {mca, longin} x {SIMS INVALID, SIMS NO_ALARM} x {broken SIOL,
//! working SIOL}.

// RTEMS-EXEC-MODEL-ALLOW(4): checked, not waived — all 4 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p mca-rs
// --all-features`, 62/62). mca-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

/// `SIMS = INVALID` (3) on both record types, with a SIOL naming a record no
/// IOC here carries — the read fails and raises LINK_ALARM/INVALID.
const DB_BROKEN_SIOL: &str = r#"
record(mca, "MCA:BROKEN") {
    field(NMAX, "8")
    field(NUSE, "8")
    field(FTVL, "LONG")
    field(SIOL, "NO:SUCH:SIM:SOURCE")
    field(SIMS, "INVALID")
}
record(longin, "LI:BROKEN") {
    field(SIOL, "NO:SUCH:SIM:SOURCE")
    field(SIMS, "INVALID")
}
"#;

/// The same pair with `SIMS = NO_ALARM` (the field default), so no SIMM_ALARM
/// is pending at all and both orders must agree on LINK_ALARM.
const DB_NO_SIMS: &str = r#"
record(mca, "MCA:NOSIMS") {
    field(NMAX, "8")
    field(NUSE, "8")
    field(FTVL, "LONG")
    field(SIOL, "NO:SUCH:SIM:SOURCE")
}
record(longin, "LI:NOSIMS") {
    field(SIOL, "NO:SUCH:SIM:SOURCE")
}
"#;

/// A SIOL that reads cleanly, so no LINK_ALARM is ever raised and the
/// SIMM_ALARM stands whichever order it is raised in.
const DB_WORKING_SIOL: &str = r#"
record(waveform, "SIM:OK:SPEC") {
    field(FTVL, "LONG")
    field(NELM, "8")
}
record(mca, "MCA:OK") {
    field(NMAX, "8")
    field(NUSE, "8")
    field(FTVL, "LONG")
    field(SIOL, "SIM:OK:SPEC")
    field(SIMS, "INVALID")
}
"#;

async fn load(db: &PvDatabase, text: &str) {
    mca_rs::register_mca_record_type();
    for def in parse_db(text, &HashMap::new()).unwrap() {
        let mut rec = create_record(&def.record_type).unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &def.fields, &mut common).unwrap();
        db.add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
            .await
            .unwrap();
    }
}

/// Put SIMM = YES and process once.
async fn simulate(db: &PvDatabase, name: &str) {
    db.put_pv(&format!("{name}.SIMM"), EpicsValue::Short(1))
        .await
        .unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity, String) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr, inst.common.amsg.clone())
}

/// The corrective half. mca reads SIOL first, so the LINK_ALARM the failed read
/// raises is already pending when `recGblSetSevr(SIMM_ALARM, sims)` runs, and
/// the equal INVALID severity loses the strict-greater tie.
#[tokio::test]
async fn a_broken_siol_on_an_mca_publishes_link_alarm_not_simm() {
    let db = PvDatabase::new();
    load(&db, DB_BROKEN_SIOL).await;

    simulate(&db, "MCA:BROKEN").await;

    let (stat, sevr, amsg) = alarm_of(&db, "MCA:BROKEN");
    assert_eq!(
        stat,
        alarm_status::LINK_ALARM,
        "mcaRecord.c:1118 reads before :1129 raises, so LINK_ALARM wins the tie"
    );
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(
        amsg, "field SIOL",
        "the operator must be told WHICH link failed"
    );
}

/// The control that pins the default: a base record raises SIMM first, so the
/// LINK_ALARM raised by the same failed read loses the same tie. If the hook
/// were applied to every record this assertion flips.
#[tokio::test]
async fn a_broken_siol_on_a_longin_still_publishes_simm_alarm() {
    let db = PvDatabase::new();
    load(&db, DB_BROKEN_SIOL).await;

    simulate(&db, "LI:BROKEN").await;

    let (stat, sevr, _) = alarm_of(&db, "LI:BROKEN");
    assert_eq!(
        stat,
        alarm_status::SIMM_ALARM,
        "longinRecord.c:414 raises before :416 reads, so SIMM_ALARM wins the tie"
    );
    assert_eq!(sevr, AlarmSeverity::Invalid);
}

/// The severity boundary: with `SIMS = NO_ALARM` nothing is pending from the
/// simulation at all, so the order cannot matter and BOTH record types must
/// report the broken link.
#[tokio::test]
async fn without_sims_both_orders_report_the_broken_link() {
    let db = PvDatabase::new();
    load(&db, DB_NO_SIMS).await;

    simulate(&db, "MCA:NOSIMS").await;
    simulate(&db, "LI:NOSIMS").await;

    for name in ["MCA:NOSIMS", "LI:NOSIMS"] {
        let (stat, sevr, amsg) = alarm_of(&db, name);
        assert_eq!(
            stat,
            alarm_status::LINK_ALARM,
            "{name}: a NO_ALARM SIMS raises nothing, so LINK_ALARM stands alone"
        );
        assert_eq!(sevr, AlarmSeverity::Invalid, "{name}");
        assert_eq!(amsg, "field SIOL", "{name}");
    }
}

/// The link boundary: a SIOL that reads cleanly raises no LINK_ALARM, so mca's
/// later SIMM_ALARM has no tie to lose and stands at SIMS. Moving the raise
/// must not have dropped it.
#[tokio::test]
async fn a_working_siol_on_an_mca_still_raises_simm_alarm() {
    let db = PvDatabase::new();
    load(&db, DB_WORKING_SIOL).await;
    db.put_pv(
        "SIM:OK:SPEC",
        EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7, 8]),
    )
    .await
    .unwrap();

    simulate(&db, "MCA:OK").await;

    let (stat, sevr, _) = alarm_of(&db, "MCA:OK");
    assert_eq!(
        stat,
        alarm_status::SIMM_ALARM,
        "mcaRecord.c:1129 runs unconditionally once the read succeeded"
    );
    assert_eq!(sevr, AlarmSeverity::Invalid, "SIMS = INVALID");
    assert_eq!(
        db.get_pv("MCA:OK").unwrap(),
        EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7, 8]),
        "the spectrum must still land"
    );
}
