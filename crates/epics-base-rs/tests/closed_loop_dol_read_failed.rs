//! R3-2 / R3-3: a FAILED `dbGetLink` is not "no value arrived".
//!
//! C's `dbGetLink` (`dbLink.c:324-340`) ends in
//!
//! ```c
//! if (status) setLinkAlarm(plink);
//! ```
//!
//! and `setLinkAlarm` (`:318-322`) is
//! `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "field %s", dbLinkFieldName(plink))`
//! — unconditional on failure, independent of the link's `MS` class, and
//! carrying the LINK FIELD's own name as the AMSG. It is an effect of the READ,
//! not of the caller, which is why every one of these boundaries is the same
//! defect: the port left the alarm to each caller and six callers dropped it.
//!
//! On top of that alarm, every OMSL record gates its own body on the status:
//! `if (!status) convert(prec, value)` (`aoRecord.c:188`, `longoutRecord.c:155`,
//! `int64outRecord.c:146`) or `goto CONTINUE` (`mbboRecord.c:206`,
//! `mbboDirectRecord.c:186`); ao additionally writes `prec->val = prec->pval`
//! BEFORE the read (`aoRecord.c:441-442`, "don't allow dbputs to val field").
//!
//! Boundaries, one case each:
//!   * DOL `Failed` vs `Value` vs constant (`NoData`), OIF Full vs Incremental
//!   * ao VAL reverts to PVAL / live DOL still drives VAL
//!   * mbbo convert suppressed (a client RVAL survives) / live DOL converts
//!   * SDIS, TSEL (non-`.TIME`), SELL — same read, same alarm, own AMSG
//!   * TSEL `.TIME` — the ONE form C reads with `dbGetTimeStampTag`, no alarm

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "GOOD:SRC") { field(VAL, "3") }

record(ao, "AO:DEAD") {
    field(OMSL, "closed_loop")
    field(DOL,  "NO:SUCH:RECORD")
    field(VAL,  "2")
}
record(ao, "AO:LIVE") {
    field(OMSL, "closed_loop")
    field(DOL,  "GOOD:SRC")
    field(VAL,  "2")
}
record(ao, "AO:CONST") {
    field(OMSL, "closed_loop")
    field(DOL,  "9")
    field(VAL,  "2")
}
record(ao, "AO:DEAD:INC") {
    field(OMSL, "closed_loop")
    field(OIF,  "Incremental")
    field(DOL,  "NO:SUCH:RECORD")
    field(VAL,  "2")
}
record(ao, "AO:ROC") {
    field(OMSL, "closed_loop")
    field(DOL,  "NO:SUCH:RECORD")
    field(OROC, "1")
    field(VAL,  "0")
}

record(mbbo, "MBBO:DEAD") {
    field(OMSL, "closed_loop")
    field(DOL,  "NO:SUCH:RECORD")
    field(ZRVL, "10") field(ONVL, "11")
}

record(calc, "CALC:SDIS") {
    field(CALC, "1")
    field(SDIS, "NO:SUCH:RECORD.VAL")
    field(DISV, "1")
}
record(calc, "CALC:TSEL") {
    field(CALC, "1")
    field(TSEL, "NO:SUCH:RECORD.VAL")
}
record(calc, "CALC:TSEL:TIME") {
    field(CALC, "1")
    field(TSEL, "NO:SUCH:RECORD.TIME")
}
record(dfanout, "DFO:SELL") {
    field(SELM, "Specified")
    field(SELL, "NO:SUCH:RECORD.VAL")
}
record(ao, "SEQ:TGT") { field(VAL, "0") }
record(seq, "SEQ:DEAD") {
    field(SELM, "All")
    field(DOL0, "NO:SUCH:RECORD.VAL")
    field(LNK0, "SEQ:TGT")
    field(DO0, "7")
}
record(seq, "SEQ:LIVE") {
    field(SELM, "All")
    field(DOL0, "GOOD:SRC")
    field(LNK0, "SEQ:TGT")
}
record(scalcout, "SC:DEAD") {
    field(CALC, "1")
    field(INAA, "NO:SUCH:RECORD.VAL")
}
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn field(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
    f: &str,
) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field(f)
}

fn alarm(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
) -> (AlarmSeverity, u16, String) {
    let r = db.get_record(rec).unwrap();
    let c = &r.read().common;
    (c.sevr, c.stat, c.amsg.to_string())
}

// ---------------------------------------------------------------- DOL

/// `Failed`: C reverts VAL to PVAL before the read and skips `convert`, so the
/// client's put is discarded and the last actual output is what stands.
#[epics_macros_rs::epics_test]
async fn dead_dol_reverts_ao_val_to_pval() {
    let db = build().await;
    db.put_pv("AO:DEAD", EpicsValue::Double(7.0)).await.unwrap();
    process(&db, "AO:DEAD").await;

    assert_eq!(
        field(&db, "AO:DEAD", "VAL"),
        Some(EpicsValue::Double(2.0)),
        "C `fetch_value`: `prec->val = prec->pval` runs BEFORE dbGetLink"
    );
    assert_eq!(
        field(&db, "AO:DEAD", "PVAL"),
        Some(EpicsValue::Double(2.0)),
        "`if(!status) convert(prec,value)` is skipped, so PVAL never took the put"
    );
}

/// The same read raises C's `setLinkAlarm` — LINK/INVALID, AMSG `field DOL`.
#[epics_macros_rs::epics_test]
async fn dead_dol_raises_link_invalid_named_dol() {
    let db = build().await;
    process(&db, "AO:DEAD").await;
    assert_eq!(
        alarm(&db, "AO:DEAD"),
        (
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM,
            "field DOL".into()
        ),
    );
}

/// `Value`: the live boundary — VAL takes the source and no LINK alarm appears.
#[epics_macros_rs::epics_test]
async fn live_dol_drives_val_and_raises_no_link_alarm() {
    let db = build().await;
    process(&db, "AO:LIVE").await;
    assert_eq!(field(&db, "AO:LIVE", "VAL"), Some(EpicsValue::Double(3.0)));
    let (sevr, stat, _) = alarm(&db, "AO:LIVE");
    assert_eq!(
        (sevr, stat),
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM)
    );
}

/// `NoData`: a CONSTANT DOL is `dbConstGetValue` — status 0 with the buffer
/// untouched. It is loaded once at init and never re-read, so a later put
/// stands and no LINK alarm is raised.
#[epics_macros_rs::epics_test]
async fn constant_dol_is_not_a_failed_read() {
    let db = build().await;
    assert_eq!(
        field(&db, "AO:CONST", "VAL"),
        Some(EpicsValue::Double(9.0)),
        "the constant seeds VAL once, at init"
    );
    db.put_pv("AO:CONST", EpicsValue::Double(7.0))
        .await
        .unwrap();
    process(&db, "AO:CONST").await;
    assert_eq!(field(&db, "AO:CONST", "VAL"), Some(EpicsValue::Double(7.0)));
    let (sevr, stat, _) = alarm(&db, "AO:CONST");
    assert_eq!(
        (sevr, stat),
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM)
    );
}

/// OIF=Incremental takes the same failure arm: the increment is never applied
/// and VAL falls back to PVAL, with the same LINK/INVALID.
#[epics_macros_rs::epics_test]
async fn dead_dol_incremental_takes_the_same_failure_arm() {
    let db = build().await;
    db.put_pv("AO:DEAD:INC", EpicsValue::Double(7.0))
        .await
        .unwrap();
    process(&db, "AO:DEAD:INC").await;
    assert_eq!(
        field(&db, "AO:DEAD:INC", "VAL"),
        Some(EpicsValue::Double(2.0))
    );
    assert_eq!(
        alarm(&db, "AO:DEAD:INC"),
        (
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM,
            "field DOL".into()
        ),
    );
}

/// Convert suppression is observable on its own: with OROC set, C's skipped
/// `convert` freezes the output ramp instead of taking another step toward a
/// setpoint the link can no longer confirm.
#[epics_macros_rs::epics_test]
async fn dead_dol_freezes_the_oroc_ramp() {
    let db = build().await;
    db.put_pv("AO:ROC", EpicsValue::Double(10.0)).await.unwrap();
    process(&db, "AO:ROC").await;
    assert_eq!(
        field(&db, "AO:ROC", "OVAL"),
        Some(EpicsValue::Double(0.0)),
        "C skips convert(), so the OROC ramp does not advance"
    );
}

/// mbbo's arm is `goto CONTINUE`, which jumps past `convert(prec)` — a client's
/// RVAL is not recomputed out of a VAL the dead link never refreshed.
#[epics_macros_rs::epics_test]
async fn dead_dol_holds_a_client_rval_on_mbbo() {
    let db = build().await;
    // Define VAL first: the OTHER way into C's `goto CONTINUE` is
    // `else if (prec->udf)` (mbboRecord.c:210), and this boundary is about the
    // DOL arm, so UDF must be out of the way.
    db.put_pv("MBBO:DEAD", EpicsValue::Enum(1)).await.unwrap();
    db.put_pv("MBBO:DEAD.RVAL", EpicsValue::ULong(99))
        .await
        .unwrap();
    process(&db, "MBBO:DEAD").await;
    assert_eq!(
        field(&db, "MBBO:DEAD", "RVAL"),
        Some(EpicsValue::ULong(99)),
        "C `mbboRecord.c:205` jumps past convert(prec)"
    );
}

// ------------------------------------------------- the rest of the family

/// `dbAccess.c:566` reads SDIS with a plain `dbGetLink`.
#[epics_macros_rs::epics_test]
async fn dead_sdis_raises_link_invalid_named_sdis() {
    let db = build().await;
    process(&db, "CALC:SDIS").await;
    assert_eq!(
        alarm(&db, "CALC:SDIS"),
        (
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM,
            "field SDIS".into()
        ),
    );
}

/// `recGbl.c:315` reads every non-`.TIME` TSEL with a plain `dbGetLink`.
#[epics_macros_rs::epics_test]
async fn dead_tsel_raises_link_invalid_named_tsel() {
    let db = build().await;
    process(&db, "CALC:TSEL").await;
    assert_eq!(
        alarm(&db, "CALC:TSEL"),
        (
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM,
            "field TSEL".into()
        ),
    );
}

/// The one TSEL form that is NOT a `dbGetLink`: `recGbl.c:316-320` takes
/// `dbGetTimeStampTag` and reports failure with an errlog only.
#[epics_macros_rs::epics_test]
async fn dead_tsel_dot_time_raises_no_link_alarm() {
    let db = build().await;
    process(&db, "CALC:TSEL:TIME").await;
    let (sevr, stat, _) = alarm(&db, "CALC:TSEL:TIME");
    assert_eq!(
        (sevr, stat),
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM)
    );
}

/// `dfanoutRecord.c:126` / `fanoutRecord.c:103` / `seqRecord.c:152` read SELL
/// with a plain `dbGetLink`.
#[epics_macros_rs::epics_test]
async fn dead_sell_raises_link_invalid_named_sell() {
    let db = build().await;
    // VAL defined first: C `dfanoutRecord.c:126-127` reads SELL BEFORE
    // `checkAlarms`, so on a UDF dfanout its equal-severity UDF_ALARM loses the
    // strict-greater tie to the LINK alarm already raised. This port raises UDF
    // first (see the module note), so the boundary under test here is the
    // defined record, where nothing competes for the tie.
    db.put_pv("DFO:SELL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    process(&db, "DFO:SELL").await;
    assert_eq!(
        alarm(&db, "DFO:SELL"),
        (
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM,
            "field SELL".into()
        ),
    );
}

// ------------------------------------------------- seq DOLn / sCalcout INAA

/// seq reads each group's DOLn with `dbGetLink` (`seqRecord.c:259`), so a dead
/// one raises the seq's own LINK/INVALID named for that group's link field —
/// and, C returning before nothing, leaves DOn holding its previous value.
#[epics_macros_rs::epics_test]
async fn dead_seq_dol_raises_link_invalid_named_dol0() {
    let db = build().await;
    // Clear the born UDF so the equal-severity UDF alarm cannot win the STAT
    // tie against the LINK alarm this case is about.
    db.put_pv("SEQ:DEAD", EpicsValue::Double(0.0))
        .await
        .unwrap();
    process(&db, "SEQ:DEAD").await;

    assert_eq!(
        alarm(&db, "SEQ:DEAD"),
        (
            AlarmSeverity::Invalid,
            epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
            "field DOL0".to_string()
        ),
        "seqRecord.c:259 is a dbGetLink, so dbLink.c:339 setLinkAlarm applies"
    );
    assert_eq!(
        field(&db, "SEQ:DEAD", "DO0"),
        Some(EpicsValue::Double(7.0)),
        "a failed read leaves the DOn value field alone"
    );
}

/// The control: the same seq with a live DOL0 raises no LINK alarm and drives
/// its LNK0 target.
#[epics_macros_rs::epics_test]
async fn live_seq_dol_raises_no_link_alarm() {
    let db = build().await;
    db.put_pv("SEQ:LIVE", EpicsValue::Double(0.0))
        .await
        .unwrap();
    process(&db, "SEQ:LIVE").await;

    let (sevr, stat, _) = alarm(&db, "SEQ:LIVE");
    assert_eq!(
        (sevr, stat),
        (AlarmSeverity::NoAlarm, 0),
        "a healthy dbGetLink raises nothing"
    );
    assert_eq!(field(&db, "SEQ:LIVE", "DO0"), Some(EpicsValue::Double(3.0)));
    assert_eq!(field(&db, "SEQ:TGT", "VAL"), Some(EpicsValue::Double(3.0)));
}

/// sCalcout's string inputs are `dbGetLink` too (`sCalcoutRecord.c:916`,
/// `:934`), so a dead INAA raises LINK/INVALID even though `fetch_values`
/// returns 0 (`:941`) and the cycle still runs `sCalcPerform`.
#[epics_macros_rs::epics_test]
async fn dead_scalcout_string_input_raises_link_invalid_named_inaa() {
    let db = build().await;
    db.put_pv("SC:DEAD", EpicsValue::Double(0.0)).await.unwrap();
    process(&db, "SC:DEAD").await;

    assert_eq!(
        alarm(&db, "SC:DEAD"),
        (
            AlarmSeverity::Invalid,
            epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
            "field INAA".to_string()
        ),
    );
    assert_eq!(
        field(&db, "SC:DEAD", "VAL"),
        Some(EpicsValue::Double(1.0)),
        "and the cycle still ran: fetch_values returns 0 for the string loop"
    );
}
