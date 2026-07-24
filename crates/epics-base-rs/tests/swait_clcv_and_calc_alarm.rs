//! R10-67 — swait's CLCV field, and the CALC_ALARM plumbing behind it.
//!
//! Two halves of one defect:
//!
//! * **CLCV** ("CALC Valid", `swaitRecord.dbd:433`, `DBF_LONG`) did not exist.
//!   C stores `postfix()`'s return status there on every compile
//!   (`swaitRecord.c:304` at init, `:561` in `special(SPC_CALC)`) and posts it
//!   `DBE_VALUE` (`:309`, `:566`) — it is how a client learns its CALC was
//!   rejected, since `special()` returns 0 and the put itself SUCCEEDS.
//!
//! * **The alarm.** C `swaitRecord.c:409-410` raises `recGblSetSevr(pwait,
//!   CALC_ALARM, INVALID_ALARM)` when `calcPerform` fails. The port carried a
//!   `calc_alarm` bool that nothing consumed: CALC_ALARM was raised by the
//!   framework's `evaluate_alarms` off a hardcoded record-type list
//!   (`calc`/`calcout`/`scalcout`) plus a `CALC_ALARM` pseudo-field no DBD
//!   declares — and swait was not on the list. A swait with a broken CALC ran
//!   the empty program, failed it every cycle, and reported NO_ALARM.
//!
//! The fix routes the raise through [`Record::check_alarms`] — C's
//! `checkAlarms()`, and already the single owner of this record's alarm
//! transitions (it raises swait's READ_ALARM). It CONSUMES the flag, so the
//! per-cycle fact cannot outlive its cycle: a gated or simulated cycle, which
//! runs no `calcPerform`, raises nothing.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const CALC_ALARM: u16 = 12;
const READ_ALARM: u16 = 1;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn alarm(db: &PvDatabase, rec: &str) -> (AlarmSeverity, u16) {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    (g.common.sevr, g.common.stat)
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

/// CLCV carries `postfix()`'s status: 0 for a CALC that compiled, -1 for one
/// that did not. Base `postfix("")` is `CALC_ERR_NULL_ARG` → -1, so swait's
/// empty CALC is INVALID (unlike sCalcPostfix/aCalcPostfix, which accept it).
#[epics_macros_rs::epics_test]
async fn r10_67_clcv_carries_the_postfix_status() {
    for (calc, want) in [("A+1", 0), ("", -1), ("1+", -1), ("@#$", -1)] {
        let mut w = SwaitRecord::default();
        w.put_field("CALC", EpicsValue::String(calc.into()))
            .unwrap();
        w.init_record(0).unwrap();
        assert_eq!(w.clcv, want, "CALC={calc:?}");
    }
}

/// The put SUCCEEDS and CLCV reports the verdict — C's `special(SPC_CALC)`
/// returns 0 (`swaitRecord.c:560-567`), unlike calcRecord's `S_db_badField`.
#[epics_macros_rs::epics_test]
async fn r10_67_a_broken_calc_put_succeeds_and_lands_in_clcv() {
    let db = PvDatabase::new();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    assert_eq!(field(&db, "W", "CLCV").await, EpicsValue::Long(0));

    db.put_record_field_from_ca("W", "CALC", EpicsValue::String("1+".into()))
        .await
        .expect("swait accepts an uncompilable CALC — the verdict goes to CLCV");

    assert_eq!(field(&db, "W", "CLCV").await, EpicsValue::Long(-1));
    assert_eq!(
        field(&db, "W", "CALC").await,
        EpicsValue::String("1+".into()),
        "the uncompilable string stays stored"
    );
}

/// C posts CLCV `DBE_VALUE` from `special()` (`swaitRecord.c:566`) — CLCV is not
/// `pp(TRUE)`, so nothing else would post it.
#[epics_macros_rs::epics_test]
async fn r10_67_a_calc_put_posts_clcv() {
    let db = PvDatabase::new();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    let inst = db.get_record("W").unwrap();
    let mut ev = inst
        .write()
        .add_subscriber("CLCV", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .expect("a CLCV subscription must be accepted");

    db.put_record_field_from_ca("W", "CALC", EpicsValue::String("1+".into()))
        .await
        .unwrap();

    let e = ev.try_recv().expect("a CALC put posts CLCV");
    assert_eq!(e.snapshot.value, EpicsValue::Long(-1));
}

/// The alarm. A broken CALC fails `calcPerform` every cycle, and C raises
/// CALC_ALARM/INVALID every cycle. Pre-fix the port reported NO_ALARM.
#[epics_macros_rs::epics_test]
async fn r10_67_a_broken_calc_raises_calc_alarm_every_cycle() {
    let db = PvDatabase::new();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1+".into()))
        .unwrap();
    w.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    for _ in 0..2 {
        process(&db, "W").await;
        assert_eq!(
            alarm(&db, "W").await,
            (AlarmSeverity::Invalid, CALC_ALARM),
            "swaitRecord.c:409-410 — every process, not just the first"
        );
        assert_eq!(
            field(&db, "W", "VAL").await,
            EpicsValue::Double(42.0),
            "a failed calcPerform leaves VAL alone"
        );
    }

    // Negative control: a CALC that compiles alarms not at all, and CLCV goes
    // back to 0.
    db.put_record_field_from_ca("W", "CALC", EpicsValue::String("3".into()))
        .await
        .unwrap();
    process(&db, "W").await;
    assert_eq!(field(&db, "W", "CLCV").await, EpicsValue::Long(0));
    assert_eq!(alarm(&db, "W").await, (AlarmSeverity::NoAlarm, 0));
    assert_eq!(field(&db, "W", "VAL").await, EpicsValue::Double(3.0));
}

/// The invariant boundary the `mem::take` closes: CALC_ALARM is a PER-CYCLE
/// fact. A cycle that runs no `calcPerform` — here because the input fetch
/// failed (C `swaitRecord.c:407`) — must not re-raise the previous cycle's
/// failure. C raises READ_ALARM instead, and nothing else.
#[epics_macros_rs::epics_test]
async fn r10_67_a_gated_cycle_does_not_inherit_the_previous_calc_alarm() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1+".into()))
        .unwrap(); // always fails
    w.put_field("INAN", EpicsValue::String("SRC".into()))
        .unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    process(&db, "W").await;
    assert_eq!(alarm(&db, "W").await, (AlarmSeverity::Invalid, CALC_ALARM));

    // Break the input link: the fetch gate now fails, so calcPerform does not
    // run. The stale flag must not survive into this cycle.
    db.put_record_field_from_ca("W", "INAN", EpicsValue::String("NOSUCHREC".into()))
        .await
        .unwrap();
    process(&db, "W").await;
    assert_eq!(
        alarm(&db, "W").await,
        (AlarmSeverity::Invalid, READ_ALARM),
        "swaitRecord.c:412-414 — the gated cycle's only alarm is READ_ALARM"
    );
}

/// The same boundary in the widened family: `calc` (and calcout/scalcout) shared
/// the sticky flag. C `calcRecord.c:120` runs no `calcPerform` on a failed fetch
/// and — unlike swait — raises nothing at all, so the record must report
/// NO_ALARM. Pre-fix the stale flag re-raised CALC_ALARM on every gated cycle.
#[epics_macros_rs::epics_test]
async fn r10_67_calc_gated_cycle_clears_the_calc_alarm() {
    let db = PvDatabase::new();
    db.add_record("CSRC", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    let mut c = CalcRecord::default();
    c.calc = "1+".into(); // uncompilable -> the empty program fails every cycle
    c.inpa = "CSRC".into();
    db.add_record("C", Box::new(c)).await.unwrap();

    process(&db, "C").await;
    assert_eq!(alarm(&db, "C").await, (AlarmSeverity::Invalid, CALC_ALARM));

    db.put_record_field_from_ca("C", "INPA", EpicsValue::String("NOSUCHREC".into()))
        .await
        .unwrap();
    process(&db, "C").await;
    assert_eq!(
        alarm(&db, "C").await,
        (AlarmSeverity::NoAlarm, 0),
        "calcRecord.c:120 — the gate skips calcPerform and calc raises no \
         READ_ALARM either"
    );
}

/// CALC_ALARM outranks UDF_ALARM on the STAT when both are INVALID. C raises
/// CALC_ALARM inside `process()` (`calcRecord.c:121-123`) BEFORE `checkAlarms`'s
/// UDF guard (`:300-303`), and `recGblSetSevr` is MAXIMIZE (strict `>`), so the
/// first one wins the tie. The framework used to raise CALC_ALARM after
/// `rec_gbl_check_udf`, reporting UDF_ALARM for a broken CALC on an undefined
/// record.
#[epics_macros_rs::epics_test]
async fn r10_67_calc_alarm_wins_the_stat_over_udf_alarm() {
    let db = PvDatabase::new();

    // A record that is BOTH undefined and calc-failing: VAL is NaN (so
    // `checkAlarms`'s UDF guard fires at UDFS = INVALID) and the CALC never
    // compiles (so `calcPerform` fails at INVALID too). Both alarms are INVALID,
    // so the STAT is decided purely by which is raised first.
    let mut c = CalcRecord::default();
    c.calc = "1+".into();
    db.add_record("C", Box::new(c)).await.unwrap();
    db.put_record_field_from_ca("C", "VAL", EpicsValue::Double(f64::NAN))
        .await
        .unwrap();

    process(&db, "C").await;

    let inst = db.get_record("C").unwrap();
    assert!(inst.read().common.udf != 0, "a NaN VAL keeps UDF");
    assert_eq!(
        alarm(&db, "C").await,
        (AlarmSeverity::Invalid, CALC_ALARM),
        "calcRecord.c:121-123 raises CALC_ALARM in process(), BEFORE \
         checkAlarms's UDF guard (:300-303); recGblSetSevr is MAXIMIZE, so the \
         first INVALID wins the STAT. The framework used to raise CALC_ALARM \
         after rec_gbl_check_udf and reported UDF_ALARM here."
    );
}
