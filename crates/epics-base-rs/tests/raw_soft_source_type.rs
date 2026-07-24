//! A `DTYP="Raw Soft Channel"` input converts by the SOURCE type, not by `double`.
//!
//! The four raw-soft input dsets read the link straight into the raw word:
//! `dbGetLink(&prec->inp, DBR_LONG|DBR_ULONG, &prec->rval, ...)`. `dbGetLink`
//! chooses its conversion routine from BOTH ends — `dbFastGetConvertRoutine` is a
//! 2-D table indexed by source DBF and destination DBR (`dbConvert.c:1571-1638`) —
//! so an out-of-range value does NOT have one answer:
//!
//! * an **integer** source reaches `getLongUlong` / `getInt64Long`, a plain
//!   integer conversion: modulo 2^n, which C DEFINES for an unsigned destination
//!   (C17 6.3.1.3p2). `-1` read into `epicsUInt32` RVAL is `0xffffffff`.
//! * a **float** source reaches `getDoubleUlong`, the bare cast — UB out of range,
//!   which this port saturates (CBUG-E2). `3e9` into `epicsInt32` RVAL is
//!   `INT32_MAX`.
//!
//! All four dsets used to convert with `c_cast::f64_to_*(value.to_f64())`, which
//! forces the FLOAT rule onto every source: it saturated what C wraps, so a `-1`
//! from an integer PV landed in RVAL as `0` instead of `0xffffffff` — every bit
//! lost, and mbbiDirect's B0..BF all read low.
//!
//! The boundaries below are one case per axis of that table: {integer, float}
//! source x {signed, unsigned} RVAL, plus the in-range control that cannot tell
//! the two rules apart.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// `int64in` is a DBF_INT64 source, so it can hold values that leave both a
/// signed and an unsigned 32-bit RVAL. `ai` is the DBF_DOUBLE source.
const DB: &str = r#"
record(int64in, "SRC:INT")  { field(VAL, "-1") }
record(int64in, "SRC:BIG")  { field(VAL, "3735928559") }
record(ai,      "SRC:DBL")  { field(VAL, "3e9") }

record(ai,          "AI:INT")  { field(DTYP, "Raw Soft Channel") field(INP, "SRC:BIG") }
record(ai,          "AI:DBL")  { field(DTYP, "Raw Soft Channel") field(INP, "SRC:DBL") }
record(bi,          "BI:INT")  { field(DTYP, "Raw Soft Channel") field(INP, "SRC:INT") }
record(mbbi,        "MBI:INT") { field(DTYP, "Raw Soft Channel") field(INP, "SRC:INT") }
record(mbbiDirect,  "MBD:INT") { field(DTYP, "Raw Soft Channel") field(INP, "SRC:INT") }
record(ai,          "AI:OK")   { field(DTYP, "Raw Soft Channel") field(INP, "SRC:BIG") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn rval(db: &PvDatabase, rec: &str) -> EpicsValue {
    process(db, rec).await;
    db.get_pv(&format!("{rec}.RVAL")).unwrap()
}

/// An INTEGER source into a SIGNED RVAL. `3735928559` (0xdeadbeef) does not fit
/// `epicsInt32`; C's integer conversion keeps the bits, so RVAL is the negative
/// word with the same pattern. The float rule would have saturated it to
/// `INT32_MAX` and thrown the bits away.
#[tokio::test]
async fn an_integer_source_wraps_into_a_signed_rval() {
    let db = ioc().await;
    assert_eq!(rval(&db, "AI:INT").await, EpicsValue::Long(-559038737));
    assert_eq!(-559038737i32 as u32, 0xdead_beef, "the bits are preserved");
}

/// An INTEGER source into an UNSIGNED RVAL — C's defined modulo-2^32 conversion
/// (6.3.1.3p2). This is the case the old float rule destroyed outright: `-1.0`
/// through a saturating `f64 -> u32` is `0`, i.e. every bit of an all-ones source
/// lost, and mbbiDirect's whole bit field with it.
#[tokio::test]
async fn an_integer_source_wraps_into_an_unsigned_rval() {
    let db = ioc().await;
    for rec in ["BI:INT", "MBI:INT", "MBD:INT"] {
        assert_eq!(
            rval(&db, rec).await,
            EpicsValue::ULong(0xffff_ffff),
            "{rec}: -1 from a DBF_INT64 source is all ones in an epicsUInt32 RVAL"
        );
    }
}

/// A FLOAT source is the other row of the table: the bare cast, which is UB out of
/// range and which this port saturates (CBUG-E2) rather than reproducing x86-64's
/// `INT32_MIN`. The two rules must not be collapsed into one — this is why the
/// conversion is the coercion owner's to make and not a `to_f64()` away.
#[tokio::test]
async fn a_float_source_saturates_into_a_signed_rval() {
    let db = ioc().await;
    assert_eq!(rval(&db, "AI:DBL").await, EpicsValue::Long(i32::MAX));
}

/// The control: in range, both rules agree, so no test above proves anything
/// unless this one passes too.
#[tokio::test]
async fn an_in_range_source_is_unaffected_by_which_rule_runs() {
    let db = ioc().await;
    db.put_pv("SRC:BIG", EpicsValue::Long(37)).await.unwrap();
    assert_eq!(rval(&db, "AI:OK").await, EpicsValue::Long(37));
}
