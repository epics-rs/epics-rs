//! R17-63: clearing UDF at the end of `process()` is a *per-record-type*
//! action in C, not a rule of the processing engine.
//!
//! Most record types clear UDF in their own `process()` (ai: `prec->udf = FALSE`
//! after a good read; ao/bo: `udf = isnan(val)` in the DOL branch). Three types
//! never do:
//!
//! * `dfanout` — UDF is written only inside the closed-loop DOL branch
//!   (`dfanoutRecord.c:118-122`). With no DOL the record stays undefined and its
//!   `checkAlarms` (`recGblCheckUdf`) publishes INVALID/UDF *every cycle*.
//! * `histogram` — UDF is written only by `clear_histogram`, and no code path
//!   tests it, so an undefined histogram publishes NO_ALARM. (It does have one
//!   alarm of its own — the invalid-limits alarm of CBUG-F12 — but that fires
//!   on `LLIM >= ULIM`, never on UDF; the histograms here have valid limits.)
//! * `event` — same shape: no UDF clear, no `checkAlarms` at all.
//!
//! The port cleared UDF unconditionally in the shared process epilogue, so a
//! DOL-less dfanout came up NO_ALARM where C alarms, and histogram/event lost
//! their UDF flag.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64), each record `dbpf X.PROC 1` then
//! `dbgf X.UDF/.SEVR/.STAT`:
//!
//! ```text
//! record(dfanout,"DFN"){field(OUTA,"DEST")}                 UDF 1  INVALID  UDF
//! record(dfanout,"DFV"){field(VAL,"5") field(OUTA,"DEST")}  UDF 0  NO_ALARM NO_ALARM
//! record(dfanout,"DF2"){field(DOL,"SRC") field(OMSL,"closed_loop")}
//!                                                            UDF 0  NO_ALARM NO_ALARM
//! record(histogram,"H1"){}                                   UDF 1  NO_ALARM NO_ALARM
//! record(event,"E2"){}                                       UDF 1  NO_ALARM NO_ALARM
//! record(event,"E1"){field(VAL,"1")}                         UDF 0  NO_ALARM NO_ALARM
//! record(ai,"A1"){}   (a type that DOES clear)               UDF 0  NO_ALARM NO_ALARM
//! ```

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::event::EventRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn state(db: &PvDatabase, name: &str) -> (bool, AlarmSeverity, u16) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.udf != 0, inst.common.sevr, inst.common.stat)
}

/// A dfanout with no DOL is undefined FOREVER: it publishes INVALID/UDF on
/// every process, and processing never clears the flag.
#[epics_macros_rs::epics_test]
async fn dfanout_without_dol_stays_undefined_and_invalid() {
    let db = PvDatabase::new();
    db.add_record("DFN", Box::new(DfanoutRecord::new(0.0)))
        .await
        .unwrap();

    for cycle in 1..=3 {
        process(&db, "DFN").await;
        assert_eq!(
            state(&db, "DFN").await,
            (true, AlarmSeverity::Invalid, alarm_status::UDF_ALARM),
            "cycle {cycle}: dfanout `process()` never clears UDF, and its \
             `checkAlarms` republishes INVALID/UDF (softIoc: DFN)"
        );
    }
}

/// The closed-loop DOL branch is the ONE place dfanout writes UDF
/// (`udf = isnan(val)`), so a dfanout fed by a DOL comes up defined.
#[epics_macros_rs::epics_test]
async fn dfanout_with_closed_loop_dol_is_defined() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    {
        // an ai that has been read is defined
        let rec = db.get_record("SRC").unwrap();
        rec.write().common.udf = 0;
    }
    db.add_record("DF2", Box::new(DfanoutRecord::new(0.0)))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("DF2", "DOL", EpicsValue::String("SRC".into()))
        .await
        .unwrap();
    db.put_pv("DF2.OMSL", EpicsValue::String("closed_loop".into()))
        .await
        .unwrap();

    process(&db, "DF2").await;

    assert_eq!(
        state(&db, "DF2").await,
        (false, AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "DOL closed_loop defines the dfanout (softIoc: DF2)"
    );
    assert_eq!(
        db.get_pv("DF2.VAL").unwrap().to_f64().unwrap(),
        7.0,
        "the DOL value lands in VAL"
    );
}

/// histogram and event never clear UDF either — and neither tests it, so UDF=1
/// publishes NO_ALARM. Both halves matter: a blanket UDF clear hides the flag, a
/// blanket UDF *alarm* invents an alarm C does not raise. (Histogram's limits are
/// valid here, so its CBUG-F12 invalid-limits alarm stays silent — that alarm is
/// about `LLIM >= ULIM`, not about UDF.)
#[epics_macros_rs::epics_test]
async fn histogram_and_event_keep_udf_without_alarming() {
    let db = PvDatabase::new();
    db.add_record("H1", Box::new(HistogramRecord::new(16, 0.0, 10.0)))
        .await
        .unwrap();
    db.add_record("E2", Box::new(EventRecord::new("")))
        .await
        .unwrap();

    process(&db, "H1").await;
    process(&db, "E2").await;

    assert_eq!(
        state(&db, "H1").await,
        (true, AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "histogram: UDF stays 1 and nothing tests it (softIoc: H1)"
    );
    assert_eq!(
        state(&db, "E2").await,
        (true, AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "event: UDF stays 1 and there is no checkAlarms (softIoc: E2)"
    );
}

/// A record type that DOES clear UDF in its own `process()` is untouched by
/// the opt-out.
#[epics_macros_rs::epics_test]
async fn ai_and_ao_still_clear_udf_on_process() {
    let db = PvDatabase::new();
    db.add_record("A1", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("AO1", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    process(&db, "A1").await;
    process(&db, "AO1").await;

    assert!(!state(&db, "A1").await.0, "ai clears UDF after a good read");
    assert!(!state(&db, "AO1").await.0, "ao clears UDF on process");
}
