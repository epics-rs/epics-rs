//! `busyRecord.c:196-208` — busy is boRecord's closed-loop fetch verbatim:
//!
//! ```c
//! if (!prec->pact) {
//!     if ((prec->dol.type != CONSTANT) && (prec->omsl == menuOmslclosed_loop)){
//!         unsigned short val;
//!
//!         prec->pact = TRUE;
//!         status=dbGetLink(&prec->dol,DBR_USHORT, &val,0,0);
//!         prec->pact = FALSE;
//!         if(status==0){
//!             prec->val = val;
//!             prec->udf = FALSE;
//!         } else {
//!             recGblSetSevr(prec,LINK_ALARM,INVALID_ALARM);
//!         }
//! ```
//!
//! The port decided which records fetch DOL with a record-NAME match inside the
//! process cycle. `busy` was not in it, so a closed-loop busy never read its
//! input link at all: VAL stayed wherever it was, UDF stayed 1, and a dead DOL
//! raised no alarm. That list had already been wrong once for `dfanout`, which
//! is why the fact now lives on the record — [`Record::fetches_dol_closed_loop`].
//!
//! Boundaries asserted here: the fetch happens only under OMSL=closed_loop and
//! only for a non-constant DOL, it clears UDF on success, and it raises
//! LINK/INVALID on failure.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build(db_text: &str) -> Db {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn proc(db: &Db, rec: &str) {
    let mut visited = HashSet::new();
    let _ = db.process_record_with_links(rec, &mut visited, 0).await;
}

fn val(db: &Db, rec: &str) -> Option<f64> {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64())
}

fn udf(db: &Db, rec: &str) -> u8 {
    db.get_record(rec).unwrap().read().common.udf
}

fn sevr(db: &Db, rec: &str) -> AlarmSeverity {
    db.get_record(rec).unwrap().read().common.sevr
}

fn stat(db: &Db, rec: &str) -> u16 {
    db.get_record(rec).unwrap().read().common.stat
}

const LIVE: &str = r#"
record(bo, "SRC") { field(VAL, "1") }
record(busy, "B") { field(DTYP, "Soft Channel") field(OMSL, "closed_loop") field(DOL, "SRC") }
"#;

/// The finding's own trigger: SRC is 1, so a processed closed-loop busy reads 1
/// and reports itself defined.
#[epics_macros_rs::epics_test]
async fn a_closed_loop_busy_sources_val_from_dol() {
    let db = build(LIVE).await;
    assert_eq!(val(&db, "B"), Some(0.0), "baseline: Done");

    proc(&db, "B").await;

    assert_eq!(val(&db, "B"), Some(1.0), "busyRecord.c:203 prec->val = val");
    assert_eq!(udf(&db, "B"), 0, "busyRecord.c:204 prec->udf = FALSE");
}

/// Boundary: `prec->omsl == menuOmslclosed_loop`. The default OMSL is
/// supervisory, and C then never looks at DOL.
#[epics_macros_rs::epics_test]
async fn a_supervisory_busy_ignores_its_dol() {
    let db = build(
        r#"
record(bo, "SRC") { field(VAL, "1") }
record(busy, "B") { field(DTYP, "Soft Channel") field(DOL, "SRC") }
"#,
    )
    .await;
    proc(&db, "B").await;

    assert_eq!(val(&db, "B"), Some(0.0), "OMSL=supervisory reads nothing");
}

/// Boundary: `prec->dol.type != CONSTANT`. A constant DOL is applied once at
/// init and must not be re-sourced per cycle, so a client's put to VAL stands.
/// DOL is 0 and VAL is driven to 1, so a re-fetch would be visible.
#[epics_macros_rs::epics_test]
async fn a_constant_dol_is_not_re_fetched() {
    let db = build(
        r#"
record(busy, "B") { field(DTYP, "Soft Channel") field(OMSL, "closed_loop") field(DOL, "0") }
"#,
    )
    .await;
    db.put_record_field_from_ca("B", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    assert_eq!(val(&db, "B"), Some(1.0));

    proc(&db, "B").await;

    assert_eq!(val(&db, "B"), Some(1.0), "the cycle must not clobber it");
}

/// Boundary: the `else` arm. A DOL naming no record fails the read every cycle,
/// and C's failure arm is `recGblSetSevr(prec, LINK_ALARM, INVALID_ALARM)`.
#[epics_macros_rs::epics_test]
async fn a_dead_dol_drives_link_invalid() {
    let db = build(
        r#"
record(busy, "B") { field(DTYP, "Soft Channel") field(OMSL, "closed_loop") field(DOL, "NOSUCHREC") }
"#,
    )
    .await;
    proc(&db, "B").await;

    assert_eq!(sevr(&db, "B"), AlarmSeverity::Invalid, "INVALID_ALARM");
    assert_eq!(stat(&db, "B"), 14, "LINK_ALARM (alarm.h epicsAlarmLink)");
}

/// The hook is the record's own answer, and busy's is the one that was missing.
/// `aao` declares `menuOmsl` and `DOL` too and must still answer false — its C
/// fetch is an array copy, not this scalar one.
#[epics_macros_rs::epics_test]
async fn the_scalar_fetch_declaration_is_per_record() {
    let db = build(
        r#"
record(busy, "B")  { field(DTYP, "Soft Channel") }
record(aao,  "A")  { field(FTVL, "DOUBLE") field(NELM, "4") }
record(ai,   "AI") { }
"#,
    )
    .await;
    let get = |n: &str| {
        db.get_record(n)
            .unwrap()
            .read()
            .record
            .fetches_dol_closed_loop()
    };
    assert!(get("B"), "busyRecord.c:197");
    assert!(!get("A"), "aaoRecord.c::fetchValue copies an ARRAY");
    assert!(!get("AI"), "ai has no DOL");
}
