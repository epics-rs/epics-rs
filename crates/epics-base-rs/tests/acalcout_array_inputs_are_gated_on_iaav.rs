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

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status::{CALC_ALARM, LINK_ALARM};
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

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

record(stringin, "SR") { field(VAL, "abc") }
record(ai, "X") {}

# A Local PV array link whose read fails: "abc" does not convert.
record(acalcout, "BADSTR") {
    field(NELM, "4")
    field(CALC, "7")
    field(INAA, "SR")
}

# A Local PV array link whose read succeeds.
record(acalcout, "INPSTR") {
    field(NELM, "4")
    field(CALC, "7")
    field(INAA, "X.INP$")
}

# A Local PV array link to a `$` source whose read C fails.
record(acalcout, "RTYPARR") {
    field(NELM, "4")
    field(CALC, "7")
    field(INBB, "X.RTYP$")
}

# The same two `$` reads through the scalar half.
record(acalcout, "RTYPSCALAR") {
    field(NELM, "4")
    field(CALC, "A")
    field(INPA, "X.RTYP$")
}

record(acalcout, "INPSCALAR") {
    field(NELM, "4")
    field(CALC, "A")
    field(INPA, "X.INP$")
}

# A Constant array link: C neither loads nor reads it.
record(acalcout, "CONARR") {
    field(NELM, "4")
    field(CALC, "7")
    field(INAA, "3")
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
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .unwrap();
}

fn alarm(db: &Db, rec: &str) -> (AlarmSeverity, u16) {
    let cell = db.get_record(rec).unwrap();
    let inst = cell.read();
    (inst.common.sevr, inst.common.stat)
}

fn array(db: &Db, rec: &str, f: &str) -> Vec<f64> {
    match db.get_record(rec).unwrap().read().record.get_field(f) {
        Some(EpicsValue::DoubleArray(a)) => a.to_vec(),
        other => panic!("{rec}.{f} is {other:?}"),
    }
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
        EpicsValue::DoubleArray(vec![7.0, 8.0, 9.0, 10.0].into()),
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
        Some(EpicsValue::DoubleArray(a)) => a.first().copied(),
        other => panic!("AA is {other:?}"),
    };
    assert_eq!(first, Some(7.0), "IAAV = Local PV, so the link is read");
    assert_eq!(field(&db, "LIVEARR", "VAL"), 4.0);
}

/// A Local PV array link IS read (`:1078` admits LOC), so its failed
/// `dbGetLink` raises LINK/INVALID and returns from `fetch_values` (`:1099`).
/// C (oracleSwaitIoc, calc f207871): STAT LINK, SEVR INVALID, VAL 0,
/// AA 0 0 0 0.
#[epics_macros_rs::epics_test]
async fn a_failed_read_of_a_local_array_link_raises_link_invalid() {
    let db = build().await;
    process(&db, "BADSTR").await;
    assert_eq!(
        alarm(&db, "BADSTR"),
        (AlarmSeverity::Invalid, LINK_ALARM),
        "a LOC link's failed dbGetLink is setLinkAlarm"
    );
    assert_eq!(
        field(&db, "BADSTR", "VAL"),
        0.0,
        "fetch_values returned non-zero"
    );
    assert_eq!(array(&db, "BADSTR", "AA"), [0.0; 4]);
}

/// The same failure through a `$` read of a field that is not a link: C
/// (calc f207871): INBB `X.RTYP$` is STAT LINK, SEVR INVALID, VAL 0,
/// BB 0 0 0 0.
#[epics_macros_rs::epics_test]
async fn a_failed_long_string_read_of_an_array_link_raises_link_invalid() {
    let db = build().await;
    process(&db, "RTYPARR").await;
    assert_eq!(alarm(&db, "RTYPARR"), (AlarmSeverity::Invalid, LINK_ALARM));
    assert_eq!(field(&db, "RTYPARR", "VAL"), 0.0);
    assert_eq!(array(&db, "RTYPARR", "BB"), [0.0; 4]);
}

/// The scalar half on the same two `$` sources. C: INPA `X.RTYP$` is
/// LINK/INVALID with VAL 0 and A 0; INPA `X.INP$` reads NaN, so the calc
/// runs and `A` gives VAL NaN under CALC/INVALID.
#[epics_macros_rs::epics_test]
async fn the_scalar_half_reads_long_strings_as_c_does() {
    let db = build().await;
    process(&db, "RTYPSCALAR").await;
    assert_eq!(
        alarm(&db, "RTYPSCALAR"),
        (AlarmSeverity::Invalid, LINK_ALARM)
    );
    assert_eq!(field(&db, "RTYPSCALAR", "VAL"), 0.0);
    assert_eq!(field(&db, "RTYPSCALAR", "A"), 0.0);

    process(&db, "INPSCALAR").await;
    assert_eq!(
        alarm(&db, "INPSCALAR"),
        (AlarmSeverity::Invalid, CALC_ALARM)
    );
    assert!(field(&db, "INPSCALAR", "A").is_nan());
    assert!(field(&db, "INPSCALAR", "VAL").is_nan());
}

/// The control for the read above: a Local PV link that reads cleanly raises
/// nothing and is charged. C: NO_ALARM, VAL 7, AA nan 0 0 0, AMEM 192.
#[epics_macros_rs::epics_test]
async fn a_clean_read_of_a_local_array_link_raises_nothing() {
    let db = build().await;
    process(&db, "INPSTR").await;
    assert_eq!(alarm(&db, "INPSTR"), (AlarmSeverity::NoAlarm, 0));
    assert_eq!(field(&db, "INPSTR", "VAL"), 7.0);
    assert_eq!(field(&db, "INPSTR", "AMEM"), 192.0);
    let aa = array(&db, "INPSTR", "AA");
    assert!(aa[0].is_nan() && aa[1..] == [0.0; 3], "AA is {aa:?}");
}

/// A Constant array link is skipped by `:1078`: never read, so AA is neither
/// loaded nor charged. C: NO_ALARM, VAL 7, AA 0 0 0 0, AMEM 128.
#[epics_macros_rs::epics_test]
async fn a_constant_array_link_is_never_read() {
    let db = build().await;
    process(&db, "CONARR").await;
    assert_eq!(alarm(&db, "CONARR"), (AlarmSeverity::NoAlarm, 0));
    assert_eq!(field(&db, "CONARR", "VAL"), 7.0);
    assert_eq!(array(&db, "CONARR", "AA"), [0.0; 4]);
    assert_eq!(field(&db, "CONARR", "AMEM"), 128.0);
}
