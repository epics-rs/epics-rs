//! acalcout's ARRAY input half is gated on the link's own status field; the
//! scalar half is not.
//!
//! ```c
//! /* aCalcoutRecord.c::fetch_values (:1051) */
//! for (i=0, plink=&pcalc->inpa, pvalue=&pcalc->a; i<MAX_FIELDS; ...) {
//!     status = dbGetLink(plink, DBR_DOUBLE, pvalue, 0, 0);   /* :1066 */
//!     if (!RTN_SUCCESS(status)) return(status);              /* :1067 */
//! }
//!
//! plinkValid = &pcalc->iaav;
//! for (i=0, plink=&pcalc->inaa, ...; i<ARRAY_MAX_FIELDS; ...) {
//!     if ((*plinkValid==acalcoutINAV_EXT) ||
//!         (*plinkValid==acalcoutINAV_LOC)) {                 /* :1074 */
//!         ...
//!         status = dbGetLink(plink, DBR_DOUBLE, *pavalue, 0, &nRequest);
//!         if (!RTN_SUCCESS(status)) return(status);          /* :1095 */
//!     }
//! }
//! ```
//!
//! `menu(acalcoutINAV)` is `EXT_NC, EXT, LOC, CON` (aCalcoutRecord.dbd:22-27),
//! so the gate at `:1078` admits exactly the two statuses that name a link C
//! believes can deliver, and an array link that is a constant or an
//! unconnected external PV is never read at all. That is not a silent success
//! — it is a link C never asks, so `fetch_values` returns 0 and `process`
//! (`:399`) runs `doCalc`.
//!
//! The port read all 24 links under `AbortOnFirstFailure`, so a single
//! unresolvable INAA killed the whole cycle: the record stopped computing, and
//! the scalar inputs that had already been read were left applied without a
//! calc behind them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;

const DB: &str = r#"
record(ai, "S") { field(VAL, "3") }
record(waveform, "WF") { field(FTVL, "DOUBLE") field(NELM, "4") }

# INAA names nothing this IOC knows: IAAV = "Ext PV NC", which C skips.
record(acalcout, "DEADARR") {
    field(NELM, "4")
    field(CALC, "A+1")
    field(INPA, "S")
    field(INAA, "NOSUCH")
}

# The scalar half has no such gate: an unresolvable INPA still aborts.
record(acalcout, "DEADSCALAR") {
    field(NELM, "4")
    field(CALC, "A+1")
    field(INPA, "NOSUCH")
}

# The control: a local array link (IAAV = "Local PV") is read as before.
record(acalcout, "LIVEARR") {
    field(NELM, "4")
    field(CALC, "A+1")
    field(INPA, "S")
    field(INAA, "WF")
}
"#;

type Db = Arc<PvDatabase>;

async fn build() -> Db {
    // `acalcout` is synApps `calc`, not Base: an application that loads it says
    // so, the way a real one loads `calcSupport.dbd`.
    IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &Db, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

fn field(db: &Db, rec: &str, f: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field(f)
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{rec}.{f}"))
}

/// The defect: an array link C would never have read must not gate the calc.
#[epics_macros_rs::epics_test]
async fn an_unreadable_array_link_does_not_stop_the_calc() {
    let db = build().await;
    process(&db, "DEADARR").await;

    assert_eq!(
        field(&db, "DEADARR", "A"),
        3.0,
        "the scalar half read INPA as usual"
    );
    assert_eq!(
        field(&db, "DEADARR", "VAL"),
        4.0,
        "aCalcoutRecord.c:1078 skips a non-EXT/LOC array link, so fetch_values returns 0"
    );
}

/// The boundary the gate must not cross: the SCALAR loop (`:1064-1067`) has no
/// status test, so an unresolvable INPA still returns non-zero and gates the
/// calc.
#[epics_macros_rs::epics_test]
async fn an_unreadable_scalar_link_still_stops_the_calc() {
    let db = build().await;
    process(&db, "DEADSCALAR").await;

    assert_eq!(
        field(&db, "DEADSCALAR", "VAL"),
        0.0,
        "the scalar loop returns at the first failing dbGetLink"
    );
}

/// The other boundary: a readable array link is still read.
#[epics_macros_rs::epics_test]
async fn a_local_array_link_is_still_read() {
    let db = build().await;
    db.put_pv(
        "WF",
        epics_base_rs::types::EpicsValue::DoubleArray(vec![7.0, 8.0, 9.0, 10.0]),
    )
    .await
    .unwrap();
    process(&db, "LIVEARR").await;

    let aa = db
        .get_record("LIVEARR")
        .unwrap()
        .read()
        .record
        .get_field("AA");
    let first = match aa {
        Some(epics_base_rs::types::EpicsValue::DoubleArray(a)) => a.first().copied(),
        other => panic!("AA is {other:?}"),
    };
    assert_eq!(first, Some(7.0), "IAAV = Local PV, so the link is read");
    assert_eq!(field(&db, "LIVEARR", "VAL"), 4.0);
}
