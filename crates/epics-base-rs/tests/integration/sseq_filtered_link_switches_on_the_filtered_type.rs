//! An sseq picks its `LNKn` put and `DOLn` read branch, and reports
//! `LTn`/`DTn`, by the link's final type through its filters: C
//! `sseqRecord.c:643,714` switch on `dbGetLinkDBFtype`, which is
//! `dbChannelFinalFieldType` (`dbChannel.c:577-621`), not `dbNameToAddr`'s
//! `field_type`.
//!
//! C oracleIoc (calc R2-x, softIoc R7.0.10), sseqs `PREC 2`, `DO1 1.23456`
//! on the `LNK1` ones, after the first `special` refresh (C reports the
//! unfiltered type before it):
//!
//! ```text
//! LNK1 WFT.VAL{"ts":{"num":"ts"}}   (CHAR waveform)  LT1 6   WFT = 1 NORD 1
//! LNK1 SOT.VAL{"ts":{"num":"dbl"}}  (stringout)      LT1 10  SOT = "1.234560"
//! LNK1 AOT.VAL{"ts":{"str":"epics"}} (ao, PREC 6)    LT1 0   AOT = 1.23
//! LNK1 I6O.VAL{"ts":{"num":"dbl"}}  (int64out)       LT1 10  I6O = 1
//! DOL1 I64.VAL{"ts":{"num":"dbl"}}  (int64in)        DT1 10  DO1 = I64's TIME
//!                                                            STR1 = DO1 "%.2f"
//! DOL1 I64.VAL                                       DT1 7   DO1 = 0
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "WFT") { field(FTVL, "CHAR") field(NELM, "40") }
record(stringout, "SOT") { }
record(ao, "AOT") { field(PREC, "6") }
record(int64out, "I6O") { }
record(int64in, "I64") { field(VAL, "5") }
record(sseq, "SQW") { field(PREC, "2") field(DO1, "1.23456") field(LNK1, "WFT.VAL{\"ts\":{\"num\":\"ts\"}}") }
record(sseq, "SQS") { field(PREC, "2") field(DO1, "1.23456") field(LNK1, "SOT.VAL{\"ts\":{\"num\":\"dbl\"}}") }
record(sseq, "SQA") { field(PREC, "2") field(DO1, "1.23456") field(LNK1, "AOT.VAL{\"ts\":{\"str\":\"epics\"}}") }
record(sseq, "SQO") { field(PREC, "2") field(DO1, "1.23456") field(LNK1, "I6O.VAL{\"ts\":{\"num\":\"dbl\"}}") }
record(sseq, "SQD") { field(PREC, "2") field(DOL1, "I64.VAL{\"ts\":{\"num\":\"dbl\"}}") }
record(sseq, "SQU") { field(PREC, "2") field(DOL1, "I64.VAL") }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Process `name` and wait until it leaves PACT: the step fires from a
/// callback, after the process call returns.
async fn run(db: &PvDatabase, name: &str) {
    db.process_record_with_links(name, &mut ProcStack::new())
        .await
        .unwrap();
    let cell = db.get_record(name).unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        while cell.read().is_processing() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} never left PACT"));
}

fn num(db: &PvDatabase, pv: &str) -> Option<f64> {
    db.get_pv(pv).unwrap().to_f64()
}

/// Seconds past the EPICS epoch, the scale of `{"ts":{"num":"dbl"}}`.
fn epics_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - 631_152_000.0
}

/// `num:ts` makes the CHAR waveform a two-element ULONG, so the scalar
/// arm puts the double: one element, not `STR1`'s bytes.
#[epics_macros_rs::epics_test]
async fn a_lnk_filtered_to_ulong_puts_the_double() {
    let db = build().await;
    assert_eq!(num(&db, "SQW.LT1"), Some(6.0));
    run(&db, "SQW").await;
    assert_eq!(
        db.get_pv("WFT").unwrap(),
        EpicsValue::CharArray(vec![1].into())
    );
    assert_eq!(num(&db, "WFT.NORD"), Some(1.0));
}

/// `num:dbl` makes the stringout DOUBLE, so it receives `DO1`, not `STR1`.
#[epics_macros_rs::epics_test]
async fn a_lnk_filtered_to_double_puts_do() {
    let db = build().await;
    assert_eq!(num(&db, "SQS.LT1"), Some(10.0));
    run(&db, "SQS").await;
    assert_eq!(
        db.get_pv("SOT").unwrap(),
        EpicsValue::String("1.234560".into())
    );
}

/// `str:epics` makes the ao STRING, so it receives `STR1`, `DO1` at PREC 2.
#[epics_macros_rs::epics_test]
async fn a_lnk_filtered_to_string_puts_str() {
    let db = build().await;
    assert_eq!(num(&db, "SQA.LT1"), Some(0.0));
    run(&db, "SQA").await;
    assert_eq!(num(&db, "AOT"), Some(1.23));
}

/// An int64out has no branch in C's switch; filtered to DOUBLE it does.
#[epics_macros_rs::epics_test]
async fn a_lnk_filtered_from_int64_puts() {
    let db = build().await;
    assert_eq!(num(&db, "SQO.LT1"), Some(10.0));
    run(&db, "SQO").await;
    assert_eq!(num(&db, "I6O"), Some(1.0));
}

/// Filtered to DOUBLE, an int64in `DOL1` is read: the filter delivers the
/// source's timestamp, and `STR1` follows `DO1` at PREC 2.
#[epics_macros_rs::epics_test]
async fn a_dol_filtered_from_int64_reads_the_double() {
    let db = build().await;
    assert_eq!(num(&db, "SQD.DT1"), Some(10.0));
    let before = epics_now();
    run(&db, "I64").await;
    run(&db, "SQD").await;
    let after = epics_now();
    let do1 = num(&db, "SQD.DO1").unwrap();
    assert!(
        (before - 1.0..=after + 1.0).contains(&do1),
        "DO1 {do1} is not I64's timestamp in [{before}, {after}]"
    );
    assert_eq!(
        db.get_pv("SQD.STR1").unwrap(),
        EpicsValue::String(format!("{do1:.2}").into())
    );
}

/// Unfiltered, the int64in `DOL1` takes C's `default: break`: no read.
#[epics_macros_rs::epics_test]
async fn an_unfiltered_int64_dol_is_not_read() {
    let db = build().await;
    assert_eq!(num(&db, "SQU.DT1"), Some(7.0));
    run(&db, "I64").await;
    run(&db, "SQU").await;
    assert_eq!(num(&db, "SQU.DO1"), Some(0.0));
}
