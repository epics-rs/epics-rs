//! R16-83: a successful constant-DOL load DEFINES the record — C follows every
//! `recGblInitConstantLink(&prec->dol, ...)` with `prec->udf = FALSE`
//! (`longoutRecord.c:113-114`, `mbboRecord.c:133-134`, `int64outRecord.c:110-111`,
//! `stringoutRecord.c:113-114`, `boRecord.c:146-149`) or with
//! `prec->udf = isnan(prec->val)` (`aoRecord.c:112-113`,
//! `dfanoutRecord.c:105-106`). The port landed VAL but left UDF=1, so a record
//! with `field(DOL,"5")` came up INVALID/UDF and stayed there until something
//! processed it.
//!
//! The seed is also followed by C's `init_record` TAIL — the ao/bo/mbbo
//! `oval/pval/mlst/alst/lalm/oraw/orbv` block that re-derives the record's
//! init-time state from the value the seed just loaded.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64) — every value below is a dbgf of the
//! records in this test:
//!
//! ```text
//! record(ao,"AO"){field(DOL,"5")}        VAL=5   OVAL=5  UDF=0
//! record(ao,"AONAN"){field(DOL,"NaN")}                   UDF=1
//! record(longout,"LO"){field(DOL,"7")}   VAL=7           UDF=0
//! record(longout,"LOHEX"){field(DOL,"0x1f")} VAL=31      UDF=0
//! record(int64out,"I64"){field(DOL,"9")} VAL=9           UDF=0
//! record(mbbo,"MBBO"){field(DOL,"2") ...} VAL=2 RVAL=12  UDF=0
//! record(bo,"BO"){field(DOL,"5")}        VAL=1   RVAL=1  UDF=0
//! record(dfanout,"DF"){field(DOL,"3.5")} VAL=3.5         UDF=0
//! record(stringout,"SO3"){field(DOL,"1.50")} VAL="1.50"  UDF=0
//! ```

// RTEMS-EXEC-MODEL-ALLOW(6): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

async fn build(db_text: &str) -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    inst.record.get_field(f).unwrap()
}

async fn udf(db: &PvDatabase, rec: &str) -> bool {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    inst.common.udf != 0
}

async fn sevr(db: &PvDatabase, rec: &str) -> AlarmSeverity {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    inst.common.sevr
}

/// The whole `recGblInitConstantLink(&prec->dol, …) → udf = FALSE` family, one
/// record per value-type boundary.
#[tokio::test]
async fn a_constant_dol_clears_udf_for_every_out_record() {
    let db = build(
        r#"
        record(ao,        "AO")  { field(DOL, "5") }
        record(longout,   "LO")  { field(DOL, "7") }
        record(int64out,  "I64") { field(DOL, "9") }
        record(mbbo,      "MB")  { field(DOL, "2") field(ZRVL, "10")
                                   field(ONVL, "11") field(TWVL, "12")
                                   field(ZRST, "z") field(ONST, "o")
                                   field(TWST, "t") }
        record(bo,        "BO")  { field(DOL, "5") }
        record(dfanout,   "DF")  { field(DOL, "3.5") }
        record(stringout, "SO")  { field(DOL, "1.50") }
        "#,
    )
    .await;

    for rec in ["AO", "LO", "I64", "MB", "BO", "DF", "SO"] {
        assert!(
            !udf(&db, rec).await,
            "{rec}: a successful constant-DOL load DEFINES the record (C sets udf = FALSE)"
        );
        // The seed clears UDF, but C's `doInitRecord0` prologue ran BEFORE
        // `init_record` and already latched the initial UDF severity, and
        // nothing lowers it until the record's first `recGblResetAlarms` —
        // softIoc, right after `iocInit`:
        //   record(ao,"AO"){field(DOL,"5")}  UDF 0  SEVR INVALID  STAT UDF
        assert_eq!(
            sevr(&db, rec).await,
            AlarmSeverity::Invalid,
            "{rec}: the init UDF severity survives the seed until the first process"
        );
    }

    assert_eq!(field(&db, "AO", "VAL").await, EpicsValue::Double(5.0));
    assert_eq!(field(&db, "LO", "VAL").await, EpicsValue::Long(7));
    assert_eq!(field(&db, "I64", "VAL").await, EpicsValue::Int64(9));
    assert_eq!(field(&db, "DF", "VAL").await, EpicsValue::Double(3.5));
    // C `cvt_st_st` copies the link TEXT, so "1.50" stays "1.50".
    assert_eq!(
        field(&db, "SO", "VAL").await,
        EpicsValue::String("1.50".into())
    );
}

/// ao/dfanout use `udf = isnan(val)`, so a NaN constant loads VAL and leaves
/// the record UNDEFINED (`aoRecord.c:113`, `dfanoutRecord.c:106`).
#[tokio::test]
async fn a_nan_constant_dol_leaves_the_record_undefined() {
    let db = build(
        r#"
        record(ao,      "AONAN") { field(DOL, "NaN") }
        record(dfanout, "DFNAN") { field(DOL, "NaN") }
        "#,
    )
    .await;

    for rec in ["AONAN", "DFNAN"] {
        assert!(
            udf(&db, rec).await,
            "{rec}: C's `udf = isnan(val)` keeps a NaN-seeded record undefined"
        );
        assert_eq!(
            sevr(&db, rec).await,
            AlarmSeverity::Invalid,
            "{rec}: still UDF, so still at the initial UDF severity (UDFS)"
        );
    }
}

/// bo loads the constant into a temporary and stores its BOOLEAN
/// (`boRecord.c:147`: `prec->val = !!ival`), so DOL="5" is VAL=1, not 5 — and
/// DOL="0" still clears UDF (the load SUCCEEDED; the value is simply 0).
#[tokio::test]
async fn a_bo_constant_dol_stores_the_boolean_of_the_constant() {
    let db = build(
        r#"
        record(bo, "BO5") { field(DOL, "5") }
        record(bo, "BO0") { field(DOL, "0") }
        "#,
    )
    .await;

    assert_eq!(
        field(&db, "BO5", "VAL").await,
        EpicsValue::Enum(1),
        "C: prec->val = !!ival"
    );
    assert_eq!(field(&db, "BO5", "RVAL").await, EpicsValue::ULong(1));
    assert!(!udf(&db, "BO5").await);

    assert_eq!(field(&db, "BO0", "VAL").await, EpicsValue::Enum(0));
    assert!(
        !udf(&db, "BO0").await,
        "the load succeeded, so the record is defined even at VAL=0"
    );
}

/// C's `init_record` tail runs right AFTER the constant load, so the tracking
/// state is derived from the seeded value: ao's OVAL/PVAL/MLST, mbbo's
/// convert() → RVAL. Pre-fix the iocsh/`dbLoadRecords` path never ran the tail
/// at all, and the builder path ran it against a VAL of 0.
#[tokio::test]
async fn a_init_tail_runs_after_the_constant_load() {
    let db = build(
        r#"
        record(ao,   "AOT") { field(DOL, "5") }
        record(mbbo, "MBT") { field(DOL, "2") field(ZRVL, "10") field(ONVL, "11")
                              field(TWVL, "12") field(ZRST, "z") field(ONST, "o")
                              field(TWST, "t") }
        "#,
    )
    .await;

    // softIoc: AO.OVAL == 5 at init (aoRecord.c:156).
    assert_eq!(field(&db, "AOT", "OVAL").await, EpicsValue::Double(5.0));
    assert_eq!(field(&db, "AOT", "MLST").await, EpicsValue::Double(5.0));
    // softIoc: MBBO.VAL == 2 (state index) and RVAL == TWVL == 12
    // (mbboRecord.c:177 `convert(prec)`).
    assert_eq!(field(&db, "MBT", "VAL").await, EpicsValue::Enum(2));
    assert_eq!(field(&db, "MBT", "RVAL").await, EpicsValue::ULong(12));
}

/// `dbConstLink.c:34-35` — *"constants may contain hex numbers, whereas
/// database conversions can't"*. softIoc: `record(longout,"LOHEX")
/// {field(DOL,"0x1f")}` comes up at VAL=31.
#[tokio::test]
async fn a_hex_constant_dol_loads_into_an_integer_field() {
    let db = build(r#"record(longout, "LOHEX") { field(DOL, "0x1f") }"#).await;

    assert_eq!(field(&db, "LOHEX", "VAL").await, EpicsValue::Long(31));
    assert!(!udf(&db, "LOHEX").await);
}

/// A DOL that names a PV is NOT a constant — nothing is seeded and the record
/// stays undefined until it processes (C: `recGblInitConstantLink` returns
/// FALSE for a PV_LINK).
#[tokio::test]
async fn a_pv_dol_seeds_nothing_and_leaves_udf_set() {
    let db = build(
        r#"
        record(ai, "SRC")  { field(VAL, "3") }
        record(ao, "AOPV") { field(DOL, "SRC") field(OMSL, "closed_loop") }
        "#,
    )
    .await;

    assert_eq!(field(&db, "AOPV", "VAL").await, EpicsValue::Double(0.0));
    assert!(
        udf(&db, "AOPV").await,
        "a PV DOL delivers nothing at init — the record is still undefined"
    );
}
