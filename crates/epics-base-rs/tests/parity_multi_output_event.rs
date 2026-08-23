//! Regression tests for parity-review findings 04 (database) and 05
//! (record infrastructure): fanout / dfanout / seq SELM link selection,
//! event-record routing, and UDF-on-NaN.
#![allow(clippy::all)]

use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::event::EventRecord;
use epics_base_rs::server::records::fanout::FanoutRecord;
use epics_base_rs::types::EpicsValue;

/// The fanout record has a `LNK0` field (C `NLINKS = 16`).
#[test]
fn fanout_has_lnk0_field() {
    let mut rec = FanoutRecord::new();
    rec.put_field("LNK0", EpicsValue::String("TARGET0".into()))
        .unwrap();
    assert_eq!(
        rec.get_field("LNK0"),
        Some(EpicsValue::String("TARGET0".into())),
        "fanout must expose LNK0 (C fanoutRecord LNK0..LNKF)"
    );
    // LNKF still present — 16 links total.
    rec.put_field("LNKF", EpicsValue::String("TARGETF".into()))
        .unwrap();
    assert_eq!(
        rec.get_field("LNKF"),
        Some(EpicsValue::String("TARGETF".into()))
    );
}

/// `SELM=All` fans out through `LNK0`; the primary first
/// slot is no longer silently dropped.
#[epics_macros_rs::epics_test]
async fn fanout_selm_all_processes_lnk0_target() {
    let db = Arc::new(PvDatabase::new());
    // A counter: `VAL+1` reads the previous VAL, so VAL counts processes.
    // `visited` cannot be the oracle — it is scoped to the process frame and
    // is empty again once the chain unwinds.
    let mut tgt = CalcRecord::new("VAL+1");
    tgt.init_record(0).unwrap();
    db.add_record("DOWNSTREAM", Box::new(tgt)).await.unwrap();
    let mut fan = FanoutRecord::new();
    // LNK0 carries the primary fan-out target, PP so it processes.
    fan.put_field("LNK0", EpicsValue::String("DOWNSTREAM PP".into()))
        .unwrap();
    fan.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    db.add_record("FAN", Box::new(fan)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("FAN", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("DOWNSTREAM").unwrap().to_f64(),
        Some(1.0),
        "fanout SELM=All must process its LNK0 target"
    );
}

/// dfanout `SELM=Specified` is 1-based: `SELN=1`
/// drives OUTA, `SELN=0` drives nothing.
#[epics_macros_rs::epics_test]
async fn dfanout_specified_is_one_based() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("OUT_A", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("OUT_B", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut df = DfanoutRecord::new(7.0);
    df.put_field("OUTA", EpicsValue::String("OUT_A".into()))
        .unwrap();
    df.put_field("OUTB", EpicsValue::String("OUT_B".into()))
        .unwrap();
    df.put_field("SELM", EpicsValue::Short(1)).unwrap(); // Specified
    df.put_field("SELN", EpicsValue::Short(1)).unwrap(); // 1-based → OUTA
    db.add_record("DF", Box::new(df)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("DF", &mut visited, 0)
        .await
        .unwrap();

    // SELN=1 drives OUTA only.
    let a = db.get_record("OUT_A").unwrap();
    let b = db.get_record("OUT_B").unwrap();
    let a_val = a.read().record.val();
    let b_val = b.read().record.val();
    assert_eq!(
        a_val,
        Some(EpicsValue::Double(7.0)),
        "dfanout SELN=1 must drive OUTA (1-based)"
    );
    assert_eq!(
        b_val,
        Some(EpicsValue::Double(0.0)),
        "dfanout SELN=1 must NOT drive OUTB"
    );
}

/// An out-of-range `SELN` on a dfanout raises
/// SOFT_ALARM / INVALID_ALARM.
#[epics_macros_rs::epics_test]
async fn dfanout_specified_out_of_range_raises_invalid() {
    use epics_base_rs::server::record::AlarmSeverity;
    let db = Arc::new(PvDatabase::new());
    let mut df = DfanoutRecord::new(1.0);
    df.put_field("SELM", EpicsValue::Short(1)).unwrap(); // Specified
    df.put_field("SELN", EpicsValue::Short(99)).unwrap(); // > 16 → INVALID
    db.add_record("DF_BAD", Box::new(df)).await.unwrap();
    // VAL=1 is a `field(VAL,"1")` seed, which C loads with UDF=0
    // (`dbPutString`). dfanout never clears UDF in `process()`, and its
    // `checkAlarms` would otherwise raise INVALID/UDF first — softIoc gives
    // STAT=UDF for the undefined record and STAT=SOFT for this one.
    db.get_record("DF_BAD").unwrap().write().common.udf = 0;

    let mut visited = HashSet::new();
    db.process_record_with_links("DF_BAD", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("DF_BAD").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "out-of-range SELN must raise INVALID alarm"
    );
    assert_eq!(
        inst.common.stat, 15, /* SOFT_ALARM */
        "out-of-range SELN must raise SOFT_ALARM status"
    );
}

/// The event record's `VAL` is a string event name.
#[test]
fn event_record_val_is_string() {
    let rec = EventRecord::new("myEvent");
    assert_eq!(
        rec.get_field("VAL"),
        Some(EpicsValue::String("myEvent".into())),
        "event record VAL must be a string event name (DBF_STRING)"
    );
}

/// `post_event_named` routes by event number:
/// a record with `EVNT=5` fires only on event 5, not event 7.
#[epics_macros_rs::epics_test]
async fn event_scan_routes_by_event_number() {
    use epics_base_rs::server::record::ScanType;

    let db = Arc::new(PvDatabase::new());
    // Two Event-scanned records on different event numbers.
    db.add_record("REC5", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("REC7", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    // Configure SCAN=Event and EVNT.
    {
        let r = db.get_record("REC5").unwrap();
        let mut inst = r.write();
        inst.common.scan = ScanType::Event;
        inst.common.evnt = "5".to_string();
    }
    {
        let r = db.get_record("REC7").unwrap();
        let mut inst = r.write();
        inst.common.scan = ScanType::Event;
        inst.common.evnt = "7".to_string();
    }
    // Register the Event scan-index entries.
    db.update_scan_index("REC5", ScanType::Passive, ScanType::Event, 0, 0);
    db.update_scan_index("REC7", ScanType::Passive, ScanType::Event, 0, 0);

    // Post event 5 — only REC5 should process. We detect processing via
    // the record's timestamp moving off its never-processed value, which
    // is the EPICS epoch (an all-zero `epicsTimeStamp`), not the Unix
    // epoch — see `general_time::epics_epoch`.
    db.post_event_named("5").await;

    let unprocessed = epics_base_rs::runtime::general_time::epics_epoch();
    let r5 = db.get_record("REC5").unwrap();
    let r7 = db.get_record("REC7").unwrap();
    let t5 = r5.read().common.time;
    let t7 = r7.read().common.time;
    assert_ne!(t5, unprocessed, "REC5 (EVNT=5) must process on event 5");
    assert_eq!(t7, unprocessed, "REC7 (EVNT=7) must NOT process on event 5");
}

/// UDF stays true when the processed value is NaN, so the
/// record raises UDF_ALARM instead of reporting a garbage value.
#[epics_macros_rs::epics_test]
async fn udf_stays_true_on_nan_value() {
    use epics_base_rs::server::record::AlarmSeverity;

    let db = Arc::new(PvDatabase::new());
    // An ai record whose VAL is NaN — the framework must NOT clear
    // UDF, and must raise UDF_ALARM at the default UDFS=INVALID.
    db.add_record("NAN_REC", Box::new(AiRecord::new(f64::NAN)))
        .await
        .unwrap();

    db.process_record("NAN_REC").await.unwrap();

    let rec = db.get_record("NAN_REC").unwrap();
    let inst = rec.read();
    assert!(
        inst.common.udf != 0,
        "UDF must stay true when VAL is NaN (C aiRecord.c:285)"
    );
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "NaN VAL must raise UDF_ALARM at UDFS=INVALID"
    );
}

/// UDF IS cleared when the processed value is a defined
/// (non-NaN) number — the fix must not over-suppress UDF clearing.
#[epics_macros_rs::epics_test]
async fn udf_cleared_on_defined_value() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("OK_REC", Box::new(AiRecord::new(3.14)))
        .await
        .unwrap();
    db.process_record("OK_REC").await.unwrap();
    let rec = db.get_record("OK_REC").unwrap();
    assert!(
        rec.read().common.udf == 0,
        "UDF must clear when VAL is a defined value"
    );
}

/// BUG 3 regression — a fanout `LNKn` pointing at a non-Passive
/// (Periodic / Event / I/O-Intr) target must NOT be processed by the
/// fanout. C `fanoutRecord.c:110` dispatches each LNKn via
/// `dbScanFwdLink` → `dbScanPassive` (`dbDbLink.c:425-432`), which
/// returns early when `pto->scan != 0`. The Rust fanout path
/// previously called `process_record_with_links` unconditionally.
#[epics_macros_rs::epics_test]
async fn fanout_lnk_skips_non_passive_target() {
    use epics_base_rs::server::record::ScanType;

    let db = Arc::new(PvDatabase::new());
    // Counters (see `fanout_selm_all_processes_lnk0_target`).
    // PASS_TGT is Passive — must be processed.
    // PERIODIC_TGT is Periodic — its own scan owns it; the fanout
    // must NOT re-process it.
    for name in ["PASS_TGT", "PERIODIC_TGT"] {
        let mut tgt = CalcRecord::new("VAL+1");
        tgt.init_record(0).unwrap();
        db.add_record(name, Box::new(tgt)).await.unwrap();
    }
    {
        let r = db.get_record("PERIODIC_TGT").unwrap();
        let mut inst = r.write();
        inst.common.scan = ScanType::Sec1;
    }

    let mut fan = FanoutRecord::new();
    fan.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    fan.put_field("LNK0", EpicsValue::String("PASS_TGT".into()))
        .unwrap();
    fan.put_field("LNK1", EpicsValue::String("PERIODIC_TGT".into()))
        .unwrap();
    db.add_record("FAN_B3", Box::new(fan)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("FAN_B3", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("PASS_TGT").unwrap().to_f64(),
        Some(1.0),
        "fanout LNK0 Passive target must be processed"
    );
    assert_eq!(
        db.get_pv("PERIODIC_TGT").unwrap().to_f64(),
        Some(0.0),
        "fanout LNK1 Periodic target must NOT be processed by the fanout"
    );
}

// ---------------------------------------------------------------------------
// Single-owner invariant: sseq LNKn dispatched exactly once per cycle.
//
// A `sseq` record drives each selected step's `LNKn` exactly once, from
// its own async sequence machine (`SseqRecord::process()`, C
// `sseqRecord.c::processCallback` → `dbPutLink`). The retired all-at-once
// `dispatch_multi_output` `MultiOut::Sseq` arm no longer exists, so there
// is no second dispatcher; these tests guard that the machine writes each
// target exactly once (never zero, never twice).
// ---------------------------------------------------------------------------

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::FieldDesc;
use epics_base_rs::server::records::sseq::SseqRecord;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The sseq machine completes via spawned per-step re-entries
/// (`ReprocessAfter` / put-notify), so `process_record_with_links`
/// returns before the `LNKn` writes land — and the FINAL step's write
/// runs in the framework `Complete` tail, AFTER `BUSY` is already 0.
/// Poll the observable write effect itself (the counter), then settle so
/// any erroneous extra dispatch would also have landed before asserting.
async fn poll_until(label: &str, cond: impl Fn() -> bool) {
    for _ in 0..400 {
        if cond() {
            // settle window — a spurious second dispatch would land here.
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(30)).await;
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("{label}: sseq sequence did not complete within timeout");
}

/// Target record that counts every `put_field("VAL", ..)` — one count
/// per value-write that reaches it. A duplicate sseq dispatch shows up
/// as a count of 2 instead of 1.
struct CountingTarget {
    val: f64,
    writes: Arc<AtomicUsize>,
}

impl Record for CountingTarget {
    fn record_type(&self) -> &'static str {
        "counting_test"
    }
    fn process(&mut self) -> CaResult<epics_base_rs::server::record::ProcessOutcome> {
        Ok(epics_base_rs::server::record::ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.writes.fetch_add(1, Ordering::SeqCst);
                self.val = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// One `process` of an sseq record writes each selected `LNKn` value
/// exactly once — never twice. Regression for the two-owner
/// double-dispatch defect.
#[epics_macros_rs::epics_test]
async fn sseq_lnkn_dispatched_exactly_once_per_cycle() {
    let db = PvDatabase::new();

    let writes_a = Arc::new(AtomicUsize::new(0));
    let writes_b = Arc::new(AtomicUsize::new(0));
    db.add_record(
        "SSEQ_TGT_A",
        Box::new(CountingTarget {
            val: 0.0,
            writes: writes_a.clone(),
        }),
    )
    .await
    .unwrap();
    db.add_record(
        "SSEQ_TGT_B",
        Box::new(CountingTarget {
            val: 0.0,
            writes: writes_b.clone(),
        }),
    )
    .await
    .unwrap();

    // sseq: SELM=All, two steps with DO/LNK set (no DOL → DO is the
    // value source). LNK1 → SSEQ_TGT_A, LNK2 → SSEQ_TGT_B.
    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_TGT_A".into()))
        .unwrap();
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_TGT_B".into()))
        .unwrap();
    db.add_record("SSEQ_REC", Box::new(sseq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SSEQ_REC", &mut visited, 0)
        .await
        .unwrap();
    poll_until("both steps", || {
        writes_a.load(Ordering::SeqCst) >= 1 && writes_b.load(Ordering::SeqCst) >= 1
    })
    .await;

    assert_eq!(
        writes_a.load(Ordering::SeqCst),
        1,
        "sseq LNK1 must write its target exactly once per cycle (double-dispatch regression)"
    );
    assert_eq!(
        writes_b.load(Ordering::SeqCst),
        1,
        "sseq LNK2 must write its target exactly once per cycle (double-dispatch regression)"
    );

    // Value delivered correctly by the single owner.
    let tgt_a = db.get_record("SSEQ_TGT_A").unwrap();
    assert_eq!(
        tgt_a.read().record.get_field("VAL"),
        Some(EpicsValue::Double(11.0)),
        "sseq LNK1 must deliver DO1 to its target"
    );
    let tgt_b = db.get_record("SSEQ_TGT_B").unwrap();
    assert_eq!(
        tgt_b.read().record.get_field("VAL"),
        Some(EpicsValue::Double(22.0)),
        "sseq LNK2 must deliver DO2 to its target"
    );
}

/// `SELM=Specified` selects a single sseq step — only that step's
/// `LNKn` is dispatched (and only once), the others are not written.
#[epics_macros_rs::epics_test]
async fn sseq_selm_specified_writes_only_selected_step_once() {
    let db = PvDatabase::new();

    let writes_sel = Arc::new(AtomicUsize::new(0));
    let writes_other = Arc::new(AtomicUsize::new(0));
    db.add_record(
        "SSEQ_SEL_TGT",
        Box::new(CountingTarget {
            val: 0.0,
            writes: writes_sel.clone(),
        }),
    )
    .await
    .unwrap();
    db.add_record(
        "SSEQ_OTHER_TGT",
        Box::new(CountingTarget {
            val: 0.0,
            writes: writes_other.clone(),
        }),
    )
    .await
    .unwrap();

    // SELM=Specified, SELN=2 → only step index 1 (LNK2) selected.
    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(1)).unwrap(); // Specified
    sseq.put_field("SELN", EpicsValue::Short(2)).unwrap();
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_OTHER_TGT".into()))
        .unwrap();
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_SEL_TGT".into()))
        .unwrap();
    db.add_record("SSEQ_SEL_REC", Box::new(sseq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SSEQ_SEL_REC", &mut visited, 0)
        .await
        .unwrap();
    poll_until("selected step", || writes_sel.load(Ordering::SeqCst) >= 1).await;

    assert_eq!(
        writes_sel.load(Ordering::SeqCst),
        1,
        "sseq SELM=Specified must write the selected step's LNKn exactly once"
    );
    assert_eq!(
        writes_other.load(Ordering::SeqCst),
        0,
        "sseq SELM=Specified must NOT write an unselected step's LNKn"
    );
}
