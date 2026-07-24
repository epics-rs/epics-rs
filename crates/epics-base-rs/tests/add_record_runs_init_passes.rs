//! R17-67: `add_record` is the creation sink, so it owes every record C's
//! `iocInit` init passes — not just the constant seed.
//!
//! It seeded constants and deadbands but never ran `run_init_passes`, so a
//! record created programmatically or by iocsh `dbCreateRecord` skipped
//! `init_record(0)`/`init_record(1)`, the `doInitRecord0` prologue (`pact =
//! FALSE`; `if (udf && stat == UDF_ALARM) sevr = udfs`) and the post-init UDF
//! tail — the very passes the seed's own doc comment says have run.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64), right after `iocInit`, no processing:
//!
//! ```text
//! record(ai,"AI"){field(INP,"7")}                     UDF 0  SEVR INVALID  STAT UDF
//! record(ao,"AO"){field(DOL,"5") OMSL=closed_loop}    UDF 0  SEVR INVALID  STAT UDF
//! record(calc,"C1"){field(CALC,"1")}                         SEVR INVALID
//! record(mbboDirect,"MBD"){field(B0,"1") field(B2,"1")}  UDF 0  VAL 5
//! record(histogram,"HG"){field(NELM,"8") field(SVL,"0")} UDF 0
//! ```
//!
//! The INVALID severity is the prologue's: it runs BEFORE `init_record`, so the
//! constant seed that clears UDF cannot lower it — only the record's first
//! `recGblResetAlarms` does. That is what makes an `MS` consumer inherit
//! INVALID from a source that has not processed yet.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::types::EpicsValue;

/// The `doInitRecord0` prologue: a record born UDF=1/STAT=UDF advertises the
/// UDFS severity from creation, on the programmatic path too.
#[epics_macros_rs::epics_test]
async fn add_record_runs_the_init_udf_prologue() {
    let db = PvDatabase::new();
    db.add_record("AI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let inst = db.get_record("AI").unwrap();
    let inst = inst.read();
    assert!(inst.common.udf != 0, "a fresh ai is undefined");
    assert_eq!(
        (inst.common.stat, inst.common.sevr),
        (alarm_status::UDF_ALARM, AlarmSeverity::Invalid),
        "the prologue latches SEVR = UDFS on a never-processed record"
    );
}

/// `init_record(0)` — calcout compiles CALC/OCAL and reports the validity codes.
/// C's `postfix()` refuses an EMPTY expression (`postfix.c:235-240`:
/// CALC_ERR_NULL_ARG → -1), so a default `calcout` inits with CLCV = -1 and a
/// compiled one with 0. A record created through `add_record` never ran the
/// pass, so both came up 0 — "healthy" for an expression C calls invalid.
#[epics_macros_rs::epics_test]
async fn add_record_runs_init_record_pass_zero() {
    let db = PvDatabase::new();
    db.add_record("CO_EMPTY", Box::new(CalcoutRecord::default()))
        .await
        .unwrap();

    let mut co = CalcoutRecord::default();
    co.calc = "1".to_string();
    db.add_record("CO_OK", Box::new(co)).await.unwrap();

    let rec = db.get_record("CO_EMPTY").unwrap();
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(-1)),
        "an empty CALC is CALC_ERR_NULL_ARG"
    );
    let rec = db.get_record("CO_OK").unwrap();
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(0)),
        "CALC=\"1\" compiles"
    );
}

/// The post-init UDF tail (R17-66's owner) rides along: it is part of the same
/// passes, so `add_record` gets it too.
#[epics_macros_rs::epics_test]
async fn add_record_runs_the_post_init_udf_tail() {
    let db = PvDatabase::new();

    let mut mbd = MbboDirectRecord::default();
    mbd.put_field("B0", EpicsValue::Char(1)).unwrap();
    mbd.put_field("B2", EpicsValue::Char(1)).unwrap();
    db.add_record("MBD", Box::new(mbd)).await.unwrap();

    // histogram's UDF clear comes from the SOFT support's constant-SVL load
    // (`devHistogramSoft.c:44-45`: `if (recGblInitConstantLink(&prec->svl, …))
    // prec->udf = FALSE;`), which is what `post_init_finalize_undef` carries —
    // softIoc: `record(histogram,"HG"){field(NELM,"8") field(SVL,"0")}` is
    // UDF 0, while a bare `record(histogram,"H1"){}` stays UDF 1.
    let mut hg = HistogramRecord::new(8, 0.0, 10.0);
    hg.put_field("SVL", EpicsValue::String("0".into())).unwrap();
    db.add_record("HG", Box::new(hg)).await.unwrap();

    {
        let rec = db.get_record("MBD").unwrap();
        let inst = rec.read();
        assert!(inst.common.udf == 0, "the B0..B1F fold defines the record");
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Long(5)),
            "B0|B2 folds into VAL=5"
        );
    }
    {
        let rec = db.get_record("HG").unwrap();
        let inst = rec.read();
        assert!(
            inst.common.udf == 0,
            "the constant SVL load defines the histogram at init"
        );
    }
}
