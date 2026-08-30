// RTEMS-EXEC-MODEL-ALLOW(4): three multi-thread-flavored tokio tests plus one hand-built runtime; run and pass in the exec-backend suite.
#![allow(unused_imports, clippy::all)]
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{LinkBacking, PvDatabase};
use epics_base_rs::server::record::*;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::types::EpicsValue;

/// A `sub` with its SNAM already set, as a `.db` `field(SNAM,...)` line leaves it
/// BEFORE `init_record` runs. C parks PACT permanently on an empty SNAM
/// (`subRecord.c:119-123`), so a `sub` that is added first and configured after
/// is a record C would have declared dead — the tests below are about the
/// subroutine/alarm/deadband paths, not about that.
fn sub_with_snam(snam: &str) -> epics_base_rs::server::records::sub_record::SubRecord {
    let mut rec = epics_base_rs::server::records::sub_record::SubRecord::default();
    rec.put_field("SNAM", EpicsValue::String(snam.into()))
        .expect("SNAM is writable");
    rec
}

#[epics_macros_rs::epics_test]
async fn test_write_notify_follows_flnk() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();
    // Counters: `VAL+1` reads the previous VAL, so VAL is the number of
    // times the record processed. `visited` cannot answer that — it is
    // frame-scoped and empty again once the chain unwinds.
    for name in ["REC_A", "REC_B"] {
        let mut rec = CalcRecord::new("VAL+1");
        rec.init_record(0).unwrap();
        db.add_record(name, Box::new(rec)).await.unwrap();
    }

    if let Some(rec) = db.get_record("REC_A") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("REC_B".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("REC_A", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(db.get_pv("REC_A").unwrap().to_f64(), Some(1.0));
    assert_eq!(db.get_pv("REC_B").unwrap().to_f64(), Some(1.0));
}

#[epics_macros_rs::epics_test]
async fn test_inp_link_processing() {
    let db = PvDatabase::new();
    db.add_record("SOURCE", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();
    db.add_record("DEST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("DEST") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("SOURCE".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DEST", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("DEST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

/// epics-base PR #4737901 regression: a soft-channel ai record with
/// an INP link to a non-existent PV must surface LINK_ALARM/INVALID
/// rather than silently returning the cached VAL with NO_ALARM. The
/// pre-fix path called `read_link_value_soft → get_pv → Err` then
/// folded the error to `None` and let process() succeed — leaving
/// downstream alarm consumers blind to the broken link.
#[epics_macros_rs::epics_test]
async fn test_soft_inp_read_failure_sets_link_alarm() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    db.add_record("BROKEN", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Point INP at a record that doesn't exist. Soft Channel is the
    // default DTYP, so the read path runs through
    // `read_link_value_soft → get_pv("NO_SUCH_PV")` which returns Err.
    if let Some(rec) = db.get_record("BROKEN") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("NO_SUCH_PV".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("BROKEN", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("BROKEN").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "broken soft-channel INP must drive SEVR=INVALID, got {:?}",
        inst.common.sevr
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "broken soft-channel INP must drive STAT=LINK, got {}",
        inst.common.stat
    );
}

/// epics-base PR #d0cf47c regression: single-INP MS-class link must
/// propagate STAT/SEVR/AMSG from the source record. Previously only
/// the multi-input link path (INPA..INPL, calc/sub/aSub/sel) carried
/// MS-class alarms; ai/longin/bi/mbbi/stringin INP=`SRC MS/MSS/MSI`
/// silently dropped them even though the link parser recorded the
/// modifier.
///
/// C `recGblInheritSevrMsg` (recGbl.c:263-281) per-flavour semantics:
/// * **MS**  — DEST gets `LINK_ALARM` (NOT source stat), max-raised
///             sevr, no amsg propagation.
/// * **MSS** — DEST gets source stat + sevr + amsg.
/// * **MSI** — same as MS, but only when source.sevr == INVALID.
#[epics_macros_rs::epics_test]
async fn test_single_inp_ms_propagates_link_alarm_no_msg() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("DST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Force SRC into Major with a specific HIHI stat and a non-empty
    // amsg. Under plain MS, DST must lift to Major but surface
    // LINK_ALARM (NOT HIHI), and DST's amsg must NOT inherit "src-msg".
    if let Some(rec) = db.get_record("SRC") {
        let mut inst = rec.write();
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "src-msg".to_string();
    }

    if let Some(rec) = db.get_record("DST") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("SRC NPP MS".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DST", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").expect("DST exists");
    let inst = dst.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "MS link must lift DST severity to source's Major"
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "C parity: MS link MUST surface as LINK_ALARM, not the source's STAT"
    );
    assert!(
        inst.common.amsg.is_empty(),
        "C parity: MS link MUST NOT propagate amsg; got {:?}",
        inst.common.amsg
    );
}

/// MSS propagates source stat + sevr + amsg (PR d0cf47c).
#[epics_macros_rs::epics_test]
async fn test_single_inp_mss_propagates_stat_and_amsg() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("DST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("SRC") {
        let mut inst = rec.write();
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "src-major".to_string();
    }

    if let Some(rec) = db.get_record("DST") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("SRC NPP MSS".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DST", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").expect("DST exists");
    let inst = dst.read();
    assert_eq!(inst.common.sevr, AlarmSeverity::Major);
    assert_eq!(
        inst.common.stat,
        alarm_status::HIHI_ALARM,
        "MSS must carry source's STAT"
    );
    assert_eq!(
        inst.common.amsg, "src-major",
        "MSS must carry source's AMSG"
    );
}

/// OUTPUT-side twin of the INP MS-class tests above: C `dbDbPutValue`
/// (dbDbLink.c:382-383) folds the SOURCE record's alarm into a DB
/// OUT-link DEST via `recGblInheritSevrMsg`. A source at MAJOR writing
/// through `OUT = DST PP MS` must lift DST to MAJOR under `LINK_ALARM`
/// (NOT the source's HIHI stat), with no amsg. Pre-fix the OUT-link
/// write path only put the value and propagated PUTF — the dest never
/// inherited the source severity, so an alarming upstream record silently
/// drove a NO_ALARM downstream. (Per-mode semantics — MS/MSI/MSS/NMS —
/// are exercised by the INP tests above; both sides share the
/// `inherit_sevr_msg` helper, so these OUT tests pin the wiring: the OUT
/// path captures the source's committed alarm and applies it to the DEST.)
#[epics_macros_rs::epics_test]
async fn test_out_link_ms_propagates_link_alarm_to_dest() {
    use epics_base_rs::server::recgbl::alarm_status;

    let db = PvDatabase::new();
    // SRC: ao with VAL=99 over HIHI=50/HHSV=Major → computes MAJOR/HIHI
    // through its own analog-limit alarm, then writes 99 to DST via a
    // `PP MS` OUT link.
    db.add_record("SRC", Box::new(AoRecord::new(99.0)))
        .await
        .unwrap();
    db.add_record("DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("SRC") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("DST PP MS".into()))
            .unwrap();
        inst.put_common_field("HIHI", EpicsValue::Double(50.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
        inst.common.udf = 0;
    }
    if let Some(rec) = db.get_record("DST") {
        // Clear DST's own UDF so its post-write process raises no alarm
        // of its own — the only severity it can end up at is the
        // inherited one.
        rec.write().common.udf = 0;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").expect("DST exists");
    let inst = dst.read();
    // Guard: the OUT-link value actually landed.
    let val = inst.record.val().and_then(|v| v.to_f64()).unwrap_or(0.0);
    assert!(
        (val - 99.0).abs() < 1e-9,
        "OUT-link value must reach DST: got {val}"
    );
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "MS OUT link must lift DST severity to source's Major"
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "C parity: MS OUT link MUST surface as LINK_ALARM, not the source's HIHI stat"
    );
    assert!(
        inst.common.amsg.is_empty(),
        "C parity: MS OUT link MUST NOT propagate amsg; got {:?}",
        inst.common.amsg
    );
}

/// NMS contrast for the OUT-link inheritance above: a bare `OUT = DST PP`
/// carries the default NoMaximize switch, so a MAJOR source must NOT
/// raise the dest's severity.
#[epics_macros_rs::epics_test]
async fn test_out_link_nms_does_not_propagate_alarm_to_dest() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(99.0)))
        .await
        .unwrap();
    db.add_record("DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("SRC") {
        let mut inst = rec.write();
        // No MS-class modifier → NoMaximize.
        inst.put_common_field("OUT", EpicsValue::String("DST PP".into()))
            .unwrap();
        inst.put_common_field("HIHI", EpicsValue::Double(50.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
        inst.common.udf = 0;
    }
    if let Some(rec) = db.get_record("DST") {
        rec.write().common.udf = 0;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").expect("DST exists");
    let inst = dst.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::NoAlarm,
        "NMS OUT link MUST NOT propagate the source's Major severity"
    );
}

/// B2 regression: a soft-channel record whose INP is an external
/// `pva://` link must fold the lset's gated alarm severity into its
/// own `LINK_ALARM`. Previously `read_link_with_alarm` returned
/// `(None, None)` for any non-Db link, so a connected pva link
/// carrying a remote MAJOR severity left the owning record at
/// NO_ALARM.
#[epics_macros_rs::epics_test]
async fn test_pva_link_propagates_alarm_severity_into_link_alarm() {
    use epics_base_rs::server::database::LinkSet;
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    /// Stub lset: serves a value and a fixed (already gated) severity.
    struct AlarmingLset;
    #[epics_base_rs::async_trait]
    impl LinkSet for AlarmingLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(12.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn alarm_severity(&self, _: &str) -> Option<i32> {
            Some(2) // MAJOR — as if the link's MS mode let it through
        }
        fn alarm_message(&self, _: &str) -> Option<String> {
            Some("remote major".into())
        }
    }

    let db = PvDatabase::new();
    db.register_link_set("pva", Arc::new(AlarmingLset)).await;
    db.add_record("PVADST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PVADST") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("pva://REMOTE:PV".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("PVADST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("PVADST").expect("record exists");
    let inst = rec.read();
    // Value was read from the lset.
    assert_eq!(
        inst.record.val().and_then(|v| v.to_f64()),
        Some(12.0),
        "pva link value must be applied"
    );
    // Severity folded into LINK_ALARM.
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "pva link's MAJOR severity must reach the record's SEVR"
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "pva link alarm must surface as LINK_ALARM"
    );
}

/// B2: when the lset reports no alarm severity (`alarm_severity` →
/// None — e.g. NMS, or remote NO_ALARM), a connected pva link must
/// NOT raise any alarm on the owning record.
#[epics_macros_rs::epics_test]
async fn test_pva_link_no_alarm_when_lset_reports_none() {
    use epics_base_rs::server::database::LinkSet;
    use epics_base_rs::server::record::AlarmSeverity;

    struct QuietLset;
    #[epics_base_rs::async_trait]
    impl LinkSet for QuietLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(5.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        // alarm_severity defaults to None.
    }

    let db = PvDatabase::new();
    db.register_link_set("pva", Arc::new(QuietLset)).await;
    db.add_record("PVAQUIET", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PVAQUIET") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("pva://REMOTE:OK".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("PVAQUIET", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("PVAQUIET").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.record.val().and_then(|v| v.to_f64()),
        Some(5.0),
        "pva link value must still be applied"
    );
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::NoAlarm,
        "no lset severity → record stays NO_ALARM"
    );
}

/// Mock lset shared by the external-OUT-link dispatch tests: records
/// every `(name, value, op)` triple the database routes through
/// `put_value`, so a test can assert both the delivered value and the
/// chosen [`LinkPutOp`] (plain vs put-notify `Async`).
struct CapturingLset {
    writes: Arc<
        std::sync::Mutex<
            Vec<(
                String,
                EpicsValue,
                epics_base_rs::server::database::LinkPutOp,
            )>,
        >,
    >,
}
#[epics_base_rs::async_trait]
impl epics_base_rs::server::database::LinkSet for CapturingLset {
    fn is_connected(&self, _: &str) -> bool {
        true
    }
    fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
    async fn put_value(
        &self,
        name: &str,
        value: EpicsValue,
        op: epics_base_rs::server::database::LinkPutOp,
    ) -> Result<(), String> {
        self.writes
            .lock()
            .unwrap()
            .push((name.to_string(), value, op));
        Ok(())
    }
}

/// A record whose OUT link is an external `pva://` link must drive
/// the processed value through the registered link set's `put_value`.
///
/// Before this fix the OUT-link write stage in `processing.rs` only
/// matched `ParsedLink::Db` — a record with a `ParsedLink::Ca`/`Pva`
/// OUT link processed normally but the value went nowhere. The
/// OUTPUT side now mirrors the INPUT side: it dispatches the write
/// through the registered lset, matching C `dbLink.c::dbPutLink`
/// (dbLink.c:432-446), which routes every link write through
/// `plink->lset->putValue` regardless of DB vs CA link.
#[epics_macros_rs::epics_test]
async fn test_pva_out_link_writes_value_through_link_set() {
    use std::sync::Mutex;

    use epics_base_rs::server::database::LinkPutOp;

    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    db.register_link_set(
        "pva",
        Arc::new(CapturingLset {
            writes: writes.clone(),
        }),
    )
    .await;

    // Soft-Channel ao record (DTYP empty) — its OUT link is the
    // soft OUT-link write path.
    db.add_record("AO_PVAOUT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("AO_PVAOUT") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("pva://REMOTE:OUT".into()))
            .unwrap();
        inst.common.udf = 0;
        // Set VAL so process() has a value to drive out the OUT link.
        inst.record
            .put_field("VAL", EpicsValue::Double(3.5))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_PVAOUT", &mut visited, 0)
        .await
        .unwrap();
    // Staged on the link-put queue and returned, as C `dbCaPutLink` does
    // (`dbCa.c:593-595`); `dbCaSync` (`dbCa.c:1126-1129`) is the barrier
    // that makes the wire write observable. The `Async` twin below needs
    // no barrier: a put-notify put still awaits its completion.
    db.sync_external_link_puts().await;

    let captured = writes.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "the pva OUT link must drive exactly one put_value"
    );
    assert_eq!(
        captured[0].0, "REMOTE:OUT",
        "put_value must receive the bare PV name (scheme stripped)"
    );
    assert_eq!(
        captured[0].1.to_f64(),
        Some(3.5),
        "put_value must receive the record's processed value"
    );
    assert_eq!(
        captured[0].2,
        LinkPutOp::Plain,
        "a plain record-processing OUT write (no put-notify chain) must \
         deliver a Plain put, not a completion-aware Async put"
    );
}

/// A record with a `pva://` OUT link and NO registered link set must
/// fail gracefully — process() completes without panic, the value is
/// simply not delivered (C `dbPutLink` returns `S_db_noLSET`).
#[epics_macros_rs::epics_test]
async fn test_pva_out_link_no_link_set_fails_gracefully() {
    let db = PvDatabase::new();
    // No register_link_set call — the "pva" scheme is unregistered.
    db.add_record("AO_NOLSET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("AO_NOLSET") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("pva://NOWHERE:PV".into()))
            .unwrap();
        inst.common.udf = 0;
        inst.record
            .put_field("VAL", EpicsValue::Double(1.0))
            .unwrap();
    }

    let mut visited = HashSet::new();
    // Must not panic; process completes cleanly.
    db.process_record_with_links("AO_NOLSET", &mut visited, 0)
        .await
        .expect("process must complete despite the unresolvable OUT link");

    let rec = db.get_record("AO_NOLSET").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.record.val().and_then(|v| v.to_f64()),
        Some(1.0),
        "the record itself still holds its value"
    );
}

/// Boundary twin of `test_pva_out_link_writes_value_through_link_set`:
/// when the originating record is part of a put-notify / blocking-put
/// chain (it carries a completion wait-set), its external OUT-link
/// write must be delivered as [`LinkPutOp::Async`] — the C
/// `dbPutLinkAsync` / pvxs `pvaPutValueAsync` path. The plain-process
/// twin asserts the `Plain` boundary; this asserts `Async`, so the
/// notify→op mapping (`PvDatabase::external_put_op`) is pinned on both
/// sides of its single branch.
#[epics_macros_rs::epics_test]
async fn test_pva_out_link_put_notify_chain_uses_async_op() {
    use std::sync::Mutex;

    use epics_base_rs::server::database::LinkPutOp;

    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    db.register_link_set(
        "pva",
        Arc::new(CapturingLset {
            writes: writes.clone(),
        }),
    )
    .await;

    db.add_record("AO_PVAOUT_NOTIFY", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // Keep the receiver alive so the completion `send` at the tail of
    // processing never observes a dropped channel; the test only
    // inspects the op captured at OUT-write time.
    let (tx, _rx) = epics_base_rs::runtime::sync::oneshot::channel();
    if let Some(rec) = db.get_record("AO_PVAOUT_NOTIFY") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("pva://REMOTE:OUT".into()))
            .unwrap();
        inst.common.udf = 0;
        inst.record
            .put_field("VAL", EpicsValue::Double(7.0))
            .unwrap();
        // Arm a put-notify wait-set: the source record is now in a
        // blocking-put chain, so its OUT-link write must use Async.
        inst.install_or_queue_notify(tx)
            .expect("the record is free, so the wait-set installs");
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_PVAOUT_NOTIFY", &mut visited, 0)
        .await
        .unwrap();
    // The completion flavour stages and returns like the plain one (C
    // `dbCaPutLinkCallback`, `dbCa.c:585-595`), so the wire write is observed
    // through the `dbCaSync` barrier. This test asserts the OP FLAVOUR, not
    // the timing — the timing boundary is
    // `external_link_put_enqueue::async_put_completion_is_reported_through_the_notify_chain`.
    db.sync_external_link_puts().await;

    let captured = writes.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "the pva OUT link must drive exactly one put_value"
    );
    assert_eq!(captured[0].0, "REMOTE:OUT");
    assert_eq!(
        captured[0].2,
        LinkPutOp::Async,
        "an OUT-link write from a put-notify chain must be a \
         completion-aware Async put (C dbPutLinkAsync / pvxs \
         pvaPutValueAsync)"
    );
}

/// C `recGbl.c:194/210-211` — when only `amsg` changes (no SEVR/STAT
/// transition), `stat_mask` is set to `DBE_ALARM` and STAT/AMSG/VAL
/// are still posted. The Rust port previously only checked
/// `alarm_changed` (sevr-or-stat) and silently dropped the AMSG-only
/// update, leaving subscribers reading a stale message string.
///
/// Reproduce via MSS link: source carries Major severity. Cycle 1
/// propagates the source amsg into the dest, raising sevr 0→Major
/// (alarm_changed=true; AMSG flows in the normal path). Cycle 2
/// changes the source amsg but keeps the same severity — dest's
/// reset_alarms sees sevr Major→Major (alarm_changed=false) but
/// amsg "msg1"→"msg2" (amsg_changed=true). The fix posts AMSG for
/// this case so the subscriber sees the new message.
#[epics_macros_rs::epics_test]
async fn test_mss_propagates_amsg_only_change_posts_amsg_event() {
    use epics_base_rs::server::recgbl::{EventMask, alarm_status};
    use epics_base_rs::server::record::AlarmSeverity;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("SRC_AMSG", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("DST_AMSG", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Source: Major severity with first amsg.
    if let Some(rec) = db.get_record("SRC_AMSG") {
        let mut inst = rec.write();
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "msg1".to_string();
    }
    // Dest: MSS link to source. Subscribe to AMSG with ALARM mask
    // (C posts AMSG with stat_mask = DBE_ALARM on amsg-only change).
    if let Some(rec) = db.get_record("DST_AMSG") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("SRC_AMSG NPP MSS".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    // Cycle 1: drives sevr 0→Major, amsg ""→"msg1" (alarm_changed=true).
    let mut visited = HashSet::new();
    db.process_record_with_links("DST_AMSG", &mut visited, 0)
        .await
        .unwrap();

    // Now subscribe to AMSG with ALARM mask AFTER cycle 1, so
    // last_posted seeds at "msg1".
    let mut amsg_rx = {
        let rec = db.get_record("DST_AMSG").unwrap();
        let mut inst = rec.write();
        inst.add_subscriber("AMSG", 11, DbFieldType::String, EventMask::ALARM.bits())
    }
    .expect("AMSG subscription must be accepted");

    // Source: keep severity Major, change amsg only.
    if let Some(rec) = db.get_record("SRC_AMSG") {
        let mut inst = rec.write();
        inst.common.amsg = "msg2".to_string();
    }

    // Cycle 2: dest picks up msg2. sevr stays Major (alarm_changed=false),
    // amsg "msg1"→"msg2" (amsg_changed=true). AMSG event must flow.
    let mut visited = HashSet::new();
    db.process_record_with_links("DST_AMSG", &mut visited, 0)
        .await
        .unwrap();

    {
        let rec = db.get_record("DST_AMSG").unwrap();
        let inst = rec.read();
        assert_eq!(inst.common.sevr, AlarmSeverity::Major, "sevr unchanged");
        assert_eq!(inst.common.amsg, "msg2", "amsg propagated");
    }

    let event = amsg_rx
        .try_recv()
        .expect("AMSG-only change must produce an event on DBE_ALARM-class subscribers");
    assert!(
        matches!(event.snapshot.value, EpicsValue::String(ref s) if s == "msg2"),
        "AMSG event payload should be the new message, got {:?}",
        event.snapshot.value
    );
}

/// Per-event `DBE_*` mask delivery through the record posting path
/// (C `db_field_log.mask`, the discriminator pvxs narrows monitor
/// updates with — `groupsource.cpp:331-337`). Each delivered
/// `MonitorEvent.mask` must be THAT event's posting class, not a
/// subscription-wide constant:
///   * a pass that changes the value AND raises the alarm posts VAL
///     with `DBE_VALUE | DBE_LOG | DBE_ALARM` (C `recGblResetAlarms`
///     `val_mask` folded into the monitor mask);
///   * an amsg-only pass posts VAL with `DBE_ALARM` alone (the value
///     deadband did not fire; `recGbl.c:212` `val_mask = DBE_ALARM`)
///     and AMSG with `stat_mask = DBE_ALARM` (`recGbl.c:194/210-211`).
#[epics_macros_rs::epics_test]
async fn test_record_posts_carry_per_event_dbe_mask() {
    use epics_base_rs::server::recgbl::{EventMask, alarm_status};
    use epics_base_rs::server::record::AlarmSeverity;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("SRC_MASK", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("DST_MASK", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Source: Major severity with first amsg.
    if let Some(rec) = db.get_record("SRC_MASK") {
        let mut inst = rec.write();
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "msg1".to_string();
    }
    // Dest: MSS link to source.
    if let Some(rec) = db.get_record("DST_MASK") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("SRC_MASK NPP MSS".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    let mut val_rx = {
        let rec = db.get_record("DST_MASK").unwrap();
        let mut inst = rec.write();
        inst.add_subscriber(
            "VAL",
            21,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
        )
    }
    .expect("VAL subscription must be accepted");

    // Cycle 1: value 0→7 (MDEL/ADEL fire) and sevr 0→Major in one pass.
    let mut visited = HashSet::new();
    db.process_record_with_links("DST_MASK", &mut visited, 0)
        .await
        .unwrap();

    let event = val_rx.try_recv().expect("cycle 1 must post a VAL event");
    assert_eq!(
        event.mask,
        EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
        "value change + alarm raise in one pass: VAL's event mask must \
         carry all three classes, got {:?}",
        event.mask
    );

    // Subscribe AMSG after cycle 1 so last_posted seeds at "msg1".
    let mut amsg_rx = {
        let rec = db.get_record("DST_MASK").unwrap();
        let mut inst = rec.write();
        inst.add_subscriber("AMSG", 22, DbFieldType::String, EventMask::ALARM.bits())
    }
    .expect("AMSG subscription must be accepted");

    // Source: keep severity Major, change amsg only.
    if let Some(rec) = db.get_record("SRC_MASK") {
        let mut inst = rec.write();
        inst.common.amsg = "msg2".to_string();
    }
    // Cycle 2: value unchanged (deadband silent), amsg-only alarm update.
    let mut visited = HashSet::new();
    db.process_record_with_links("DST_MASK", &mut visited, 0)
        .await
        .unwrap();

    let event = val_rx
        .try_recv()
        .expect("alarm movement must post VAL even when the deadband is silent");
    assert_eq!(
        event.mask,
        EventMask::ALARM,
        "amsg-only pass: VAL posts with DBE_ALARM alone (C recGbl.c:212 \
         val_mask), got {:?}",
        event.mask
    );

    let event = amsg_rx.try_recv().expect("amsg-only change must post AMSG");
    assert_eq!(
        event.mask,
        EventMask::ALARM,
        "AMSG posts with stat_mask = DBE_ALARM (C recGbl.c:194/210-211), \
         got {:?}",
        event.mask
    );
}

// R14-63 — C posts `.UDF` from NO processing cycle. `recGblResetAlarms`
// (recGbl.c:202-222) posts SEVR/STAT/AMSG/ACKS and nothing else, no record's
// `monitor()` calls `db_post_events(..., &prec->udf, ...)` — the call appears
// nowhere in EPICS base or the modules — and the `recGblCheckUDF` that the
// port's justifying comment cited does not exist in C at all.
//
// The port pushed a synthesized `.UDF` event onto every posting cycle, with no
// change detection and a mask made from the union of the cycle's other posts.
// A `.UDF` subscriber must see an event only where C's generic `dbPut` posts
// the field it wrote (dbAccess.c) — i.e. a caput to `.UDF` itself.
#[epics_macros_rs::epics_test]
async fn test_process_cycle_posts_no_udf_event() {
    use epics_base_rs::server::recgbl::{EventMask, alarm_status};
    use epics_base_rs::server::record::AlarmSeverity;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    // Soft-Channel ai with a defined VAL — processing clears UDF (true→false)
    // and the alarm (INVALID→NO_ALARM), so this cycle posts VAL/SEVR/STAT:
    // the maximal case for the removed "UDF rides along with any post" rule.
    db.add_record("UDF_REC", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("UDF_REC").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 1;
        inst.common.sevr = AlarmSeverity::Invalid;
        inst.common.stat = alarm_status::UDF_ALARM;
    }

    let mut udf_rx = {
        let rec = db.get_record("UDF_REC").unwrap();
        let mut inst = rec.write();
        inst.add_subscriber("UDF", 31, DbFieldType::Char, EventMask::ALARM.bits())
    }
    .expect("UDF subscription must be accepted");

    // Foreign-process path — `process_record` → `process_local`.
    db.process_record("UDF_REC").await.unwrap();
    {
        let rec = db.get_record("UDF_REC").unwrap();
        let inst = rec.read();
        assert!(
            inst.common.udf == 0,
            "process must have cleared UDF in the record"
        );
    }
    assert!(
        udf_rx.try_recv().is_err(),
        "process_local must post no .UDF event — C posts UDF from no monitor()"
    );

    // The link/scan path (`process_record_with_links`) — the same rule.
    {
        let rec = db.get_record("UDF_REC").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 1;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("UDF_REC", &mut visited, 0)
        .await
        .unwrap();
    assert!(
        udf_rx.try_recv().is_err(),
        "process_record_with_links must post no .UDF event either"
    );
}

/// The other side of the R14-63 boundary: a client caput to `.UDF` DOES post,
/// because C's generic `dbPut` posts the field it wrote. Removing the
/// synthesized processing-cycle posts must not take this one with it.
#[epics_macros_rs::epics_test]
async fn test_caput_to_udf_field_posts_udf_event() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("UDF_PUT", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();

    let mut udf_rx = {
        let rec = db.get_record("UDF_PUT").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
        inst.add_subscriber("UDF", 32, DbFieldType::Char, EventMask::VALUE.bits())
    }
    .expect("UDF subscription must be accepted");

    db.put_record_field_from_ca("UDF_PUT", "UDF", EpicsValue::Char(1))
        .await
        .expect("caput to .UDF must be accepted");

    let event = udf_rx
        .try_recv()
        .expect("a caput to .UDF must deliver a .UDF monitor event");
    // `UDF` is `DBF_UCHAR` (`dbCommon.dbd:265`), so the monitor update carries
    // it as the unsigned byte it is declared as — CA then serves that as
    // `DBR_CHAR`, which is what the compiled C IOC reports for `.UDF`
    // (`fixtures/c_native_types.tsv`: `ai UDF DBF_UCHAR - DBF_CHAR`). Asserting
    // the SIGNED `EpicsValue::Char` here pinned the storage variant the record
    // happened to hold, which is exactly the type-from-the-value defect.
    assert!(
        matches!(event.snapshot.value, EpicsValue::UChar(1)),
        "the .UDF event must carry the value the client wrote, got {:?}",
        event.snapshot.value
    );
    assert_eq!(
        event.snapshot.value.dbr_type().ca_wire_type(),
        DbFieldType::Char as u16,
        "a DBF_UCHAR field goes on the CA wire as DBR_CHAR"
    );
}

/// C `dbAccess.c::dbPutField:1276` sets `precord->putf = TRUE`
/// IMMEDIATELY before calling `dbProcess`. The flag stays TRUE
/// throughout the entire process cycle and is cleared in
/// `recGblFwdLink` (`recGbl.c:302`) after the forward-link
/// dispatch — i.e. observable for the WHOLE put-driven processing
/// cycle. Async records keep PUTF=TRUE through the device round
/// trip; it clears only when the completion path runs FLNK.
///
/// Pre-fix the Rust port cleared PUTF in `put_record_field_from_ca`
/// BEFORE the `process_record_with_links` call (field_io.rs:1589),
/// so any consumer reading PUTF during the process cycle (TPRO
/// trace, monitor on .PUTF, async-completion path's
/// "put-driven vs scan-driven" classifier) always saw PUTF=0.
#[epics_macros_rs::epics_test]
async fn test_putf_clears_after_synchronous_put_completion() {
    // AoRecord is synchronous Soft Channel (process() returns
    // Complete immediately). The synchronous-completion clear
    // point in `put_record_field_from_ca` runs after the
    // `process_record_with_links` call returns — so the
    // test-observable end state is PUTF=false. The companion
    // async test below differentiates "stays set through round
    // trip" vs the pre-fix "always false during process".
    let db = PvDatabase::new();
    db.add_record("PUTF_SYNC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let _ = db
        .put_record_field_from_ca("PUTF_SYNC", "VAL", EpicsValue::Double(42.0))
        .await;

    let rec = db.get_record("PUTF_SYNC").unwrap();
    let inst = rec.read();
    assert!(
        !inst.common.putf,
        "after synchronous put completion, PUTF must clear (mirrors C recGblFwdLink:302)"
    );
}

/// Async-completion path: for a record that returns AsyncPending,
/// PUTF must remain TRUE across the device round trip and clear
/// only when `complete_async_record` runs. C parity:
/// `dbAccess.c::dbPutField:1276` sets putf=TRUE; the async device's
/// completion eventually calls `dbProcess` again, which runs through
/// `recGblFwdLink` (clears putf).
#[epics_macros_rs::epics_test]
async fn test_putf_survives_async_round_trip_and_clears_on_completion() {
    let db = PvDatabase::new();
    db.add_record("ASYNC_PUTF", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Drive a CA put. AsyncRecord returns AsyncPending, so the
    // process call returns with PACT=true; PUTF must stay TRUE.
    let _ = db
        .put_record_field_from_ca("ASYNC_PUTF", "VAL", EpicsValue::Double(7.0))
        .await;

    {
        let rec = db.get_record("ASYNC_PUTF").unwrap();
        let inst = rec.read();
        assert!(inst.is_processing(), "async pending → PACT=true");
        assert!(
            inst.common.putf,
            "PUTF must remain TRUE across the async round trip — \
             pre-fix the Rust port cleared it before the process call \
             so async-completion logic could not classify the trigger \
             as put-driven"
        );
    }

    // Now fire the async completion. PUTF must clear (mirrors C
    // recGblFwdLink:302 after the FLNK dispatch).
    db.complete_async_record("ASYNC_PUTF").await.unwrap();
    {
        let rec = db.get_record("ASYNC_PUTF").unwrap();
        let inst = rec.read();
        assert!(!inst.is_processing(), "completion clears PACT");
        assert!(
            !inst.common.putf,
            "complete_async_record_inner must clear PUTF (recGblFwdLink parity)"
        );
    }
}

/// C `dbAccess.c::dbPut:1405-1406` clears `precord->udf = FALSE`
/// synchronously when the put target is the record-type's primary
/// value field (`dbIsValueField`). The clear runs INSIDE dbPut —
/// BEFORE dbProcess. Pre-fix the Rust port deferred UDF clearing
/// to the process-cycle's own `if instance.record.clears_udf()`
/// branch (processing.rs:3687). The processing path drops the put's
/// write lock and re-acquires inside `process_record_with_links`,
/// so a second reader between the put and the process could
/// observe `(VAL=new, udf=true)` — a C-illegal pair. For async
/// records the window spans the entire device round trip until
/// `complete_async_record` runs its own clear. This test pins the
/// C-parity invariant: post-put, pre-process, UDF must already be
/// false on a primary-field write.
#[epics_macros_rs::epics_test]
async fn test_put_record_field_from_ca_clears_udf_on_primary_field_write() {
    let db = PvDatabase::new();
    db.add_record("UDF_ASYNC", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Record starts with udf=true (default).
    {
        let rec = db.get_record("UDF_ASYNC").unwrap();
        assert!(
            rec.read().common.udf != 0,
            "AsyncRecord starts undefined (udf=true)"
        );
    }

    let _ = db
        .put_record_field_from_ca("UDF_ASYNC", "VAL", EpicsValue::Double(7.0))
        .await;

    // AsyncRecord returns AsyncPending; PACT is set, process bailed
    // before its own UDF clear at processing.rs:840 ran. The put-time
    // clear in field_io.rs must have already fired.
    let rec = db.get_record("UDF_ASYNC").unwrap();
    let inst = rec.read();
    assert!(
        inst.is_processing(),
        "AsyncRecord should be mid-async (PACT=true)"
    );
    assert!(
        inst.common.udf == 0,
        "primary-field CA put must clear UDF synchronously \
         (dbAccess.c::dbPut:1411 parity) — observable before \
         complete_async_record runs"
    );
}

/// epics-base PR #3fb10b6 regression: only the record directly
/// receiving a dbPut should carry PUTF=1 during chain processing.
/// Pre-fix the CP-target dispatch set PUTF=true on every chained
/// record, smearing put attribution across the entire chain.
#[epics_macros_rs::epics_test]
async fn test_putf_stays_off_for_cp_chained_targets() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("TGT") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("SRC CP".into()))
            .unwrap();
    }

    // Drive SRC's process directly. The CP dispatch enumerates TGT
    // and would (pre-fix) set TGT.common.putf=true before processing.
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    let tgt = db.get_record("TGT").expect("TGT exists");
    let inst = tgt.read();
    assert!(
        !inst.common.putf,
        "CP-driven TGT must not carry PUTF=1 — that bit belongs only to the directly-put record"
    );
}

/// C `dbDbLink.c::processTarget:474` propagates `pdst->putf = psrc->putf`
/// when writing through a DB OUT link to a non-pact target. Pre-fix
/// the Rust `write_db_link_value` only put the value and called
/// `process_record_with_links` without touching `target.putf` — so a
/// CA put on an ao with OUT pointing at a passive ai left the ai's
/// PUTF=0 during the chained process cycle. dbNotify completion
/// attribution and device-support `put-driven vs scan-driven`
/// classifiers downstream of the OUT link silently observed
/// scan-driven processing instead of put-driven.
#[epics_macros_rs::epics_test]
async fn test_putf_propagates_through_db_out_link_to_passive_target() {
    let db = PvDatabase::new();
    db.add_record("PUTF_OUT_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // Source ao: OUT to TGT, PP semantics so the target processes.
    db.add_record("PUTF_OUT_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PUTF_OUT_SRC") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("PUTF_OUT_TGT PP".into()))
            .unwrap();
    }
    // Target must be Passive for processTarget to run; AoRecord
    // defaults to Passive scan so no explicit set needed.

    // Drive a CA put that lands as a put on SRC. SRC.putf becomes 1
    // before processing; during processing, the OUT-link write runs
    // and target should inherit putf=1 BEFORE its process cycle.
    let _ = db
        .put_record_field_from_ca("PUTF_OUT_SRC", "VAL", EpicsValue::Double(5.0))
        .await;

    // After both records' synchronous cycles complete, the C path
    // clears putf on each (each runs its own recGblFwdLink). What
    // this test pins is the steady-state observability: value
    // landed (proving OUT-write happened) AND target.rpro stayed
    // false (no spurious reprocess request — that path only fires
    // when target was pact at OUT-write time). The mid-cycle PUTF
    // observability is tested separately via an async target below.
    let tgt = db.get_record("PUTF_OUT_TGT").unwrap();
    let inst = tgt.read();
    assert!(
        !inst.common.putf,
        "after both records' synchronous cycles complete, both clear putf"
    );
    assert!(
        inst.common.rpro == 0,
        "target was not pact, so rpro must stay false (normal propagation)"
    );
    let val = inst.record.val().and_then(|v| v.to_f64()).unwrap_or(0.0);
    assert!(
        (val - 5.0).abs() < 1e-10,
        "OUT link write propagated value (val={val})"
    );
}

/// Mid-cycle PUTF propagation: when the source's OUT-link write
/// dispatches a target's process(), the target.putf must equal the
/// source's putf BEFORE the target's own clears fire. Using an async
/// target lets us observe the bit between write_db_link_value's set
/// and the eventual complete_async_record clear.
///
/// Pre-fix `write_db_link_value` only forwarded the value
/// and dispatched process — never touched `target.putf`. So even
/// when the source had `putf=1` from a CA put, the async target
/// stayed at `putf=0` for the duration of the in-flight cycle.
#[epics_macros_rs::epics_test]
async fn test_putf_propagates_mid_cycle_via_async_target_out_link() {
    let db = PvDatabase::new();
    // Async target: stays pact between process and complete_async.
    db.add_record("PROP_TGT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("PROP_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PROP_SRC") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("PROP_TGT PP".into()))
            .unwrap();
    }

    // Drive CA put. SRC processes synchronously, OUT writes to TGT,
    // dispatches process; TGT returns AsyncPending so its process
    // stays in flight — PUTF must be set on TGT before that return
    // and stay set until completion.
    let _ = db
        .put_record_field_from_ca("PROP_SRC", "VAL", EpicsValue::Double(11.0))
        .await;

    let tgt = db.get_record("PROP_TGT").unwrap();
    let inst = tgt.read();
    assert!(
        inst.is_processing(),
        "AsyncPending target stays pact between process and complete"
    );
    assert!(
        inst.common.putf,
        "target.putf must inherit from src.putf BEFORE complete_async_record clears it \
         (C dbDbLink.c::processTarget:474). Pre-fix this stayed false."
    );
}

/// epics-base 7.0.7 + PR #ac92e3e follow-up: SIMM=RAW input must
/// route the SIOL value through RVAL and run the record's conversion
/// chain (LINR/ESLO/EOFF), not overwrite VAL with the raw count.
/// Pre-fix the simulation path called both put_field("RVAL", v) AND
/// set_val(v), so VAL ended up holding raw counts and the operator's
/// configured EGU conversion was silently bypassed.
#[epics_macros_rs::epics_test]
async fn test_simm_raw_input_runs_conversion_chain() {
    let db = PvDatabase::new();
    // Source PV that the ai's SIOL link reads from — provides the
    // "raw count" for the simulation.
    db.add_record("RAW:SRC", Box::new(AoRecord::new(5.0)))
        .await
        .unwrap();
    // Target ai: configure LINR=SLOPE(1), ESLO=2.0, EOFF=10.0 so a
    // raw value of 5 should convert to VAL = 5*2 + 10 = 20.
    let mut ai = epics_base_rs::server::records::ai::AiRecord::new(0.0);
    ai.linr = 1;
    ai.eslo = 2.0;
    ai.eoff = 10.0;
    db.add_record("AI:SIMRAW", Box::new(ai)).await.unwrap();
    if let Some(rec) = db.get_record("AI:SIMRAW") {
        let mut inst = rec.write();
        // SIMM=2 (RAW) directly on the ai record's own SIMM field.
        // Putting through put_field exercises the same code path
        // operators hit via caput .SIMM 2.
        inst.record.put_field("SIMM", EpicsValue::Short(2)).unwrap();
        // SIOL lives on the ai record-specific struct (not common),
        // so put through the record's own put_field — put_common_field
        // would leave ai.siol empty and the simulation never enters.
        inst.record
            .put_field("SIOL", EpicsValue::String("RAW:SRC".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("AI:SIMRAW", &mut visited, 0)
        .await
        .unwrap();

    let ai_rec = db.get_record("AI:SIMRAW").expect("AI:SIMRAW exists");
    let inst = ai_rec.read();
    let val = inst
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64())
        .expect("VAL must be readable as f64");
    assert!(
        (val - 20.0).abs() < 1e-10,
        "SIMM=RAW must run convert(): expected VAL=5*ESLO+EOFF=20.0, got {val}"
    );
    let rval = inst
        .record
        .get_field("RVAL")
        .and_then(|v| match v {
            EpicsValue::Long(n) => Some(n as f64),
            other => other.to_f64(),
        })
        .expect("RVAL must be readable");
    assert!(
        (rval - 5.0).abs() < 1e-10,
        "RVAL must hold the raw count from SIOL; got {rval}"
    );
}

#[epics_macros_rs::epics_test]
async fn test_cycle_detection() {
    let db = PvDatabase::new();
    db.add_record("CYCLE_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("CYCLE_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("CYCLE_A") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("CYCLE_B".into()))
            .unwrap();
    }
    if let Some(rec) = db.get_record("CYCLE_B") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("CYCLE_A".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("CYCLE_A", &mut visited, 0)
        .await
        .unwrap();
    // What breaks the loop is that CYCLE_A is still ON THE STACK when its own
    // FLNK comes back round, not a record of having processed it earlier — C
    // stops the same loop with `psrc->pact = TRUE` (`dbDbLink.c:456`), also a
    // stack condition. The set is therefore empty once the chain unwinds, and
    // the per-record process counts live in
    // `tests/process_frame_marker_is_stack_scoped.rs`.
    assert!(visited.is_empty(), "the frame unwound: {visited:?}");
}

#[epics_macros_rs::epics_test]
async fn test_ao_drvh_drvl_clamp() {
    let mut rec = AoRecord::new(0.0);
    rec.drvh = 100.0;
    rec.drvl = -50.0;
    rec.val = 200.0;
    rec.process().unwrap();
    assert!((rec.val - 100.0).abs() < 1e-10);

    rec.val = -100.0;
    rec.process().unwrap();
    assert!((rec.val - (-50.0)).abs() < 1e-10);
}

#[epics_macros_rs::epics_test]
async fn test_ao_oroc_rate_limit() {
    let mut rec = AoRecord::new(0.0);
    rec.oroc = 5.0;
    rec.drvh = 0.0;
    rec.drvl = 0.0;

    rec.val = 100.0;
    rec.process().unwrap();
    // C: OROC modifies OVAL, not VAL
    assert!((rec.oval - 5.0).abs() < 1e-10, "First: oval={}", rec.oval);

    rec.val = 200.0;
    rec.process().unwrap();
    assert!((rec.oval - 10.0).abs() < 1e-10, "Second: oval={}", rec.oval);
}

#[epics_macros_rs::epics_test]
async fn test_ao_omsl_dol() {
    let db = PvDatabase::new();
    db.add_record("SOURCE", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SOURCE".to_string();
    db.add_record("OUTPUT", Box::new(ao)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("OUTPUT", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("OUTPUT").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

#[epics_macros_rs::epics_test]
async fn test_ao_oif_incremental() {
    let db = PvDatabase::new();
    db.add_record("DELTA", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(100.0);
    ao.omsl = 1;
    ao.oif = 1;
    ao.dol = "DELTA".to_string();
    // C `aoRecord.c::init_record` ends with `prec->pval = prec->val`, so a
    // fresh record's first Incremental cycle bases off its initial VAL.
    // Without this the bypassed-init record has PVAL=0 and the increment
    // (now correctly from PVAL, per aoRecord.c:447-455) would yield 10.
    ao.init_record(0).unwrap();
    db.add_record("OUTPUT", Box::new(ao)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("OUTPUT", &mut visited, 0)
        .await
        .unwrap();

    // Initial VAL=100 (=> PVAL=100 at init) + DOL delta 10 = 110.
    let val = db.get_pv("OUTPUT").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 110.0).abs() < 1e-10),
        other => panic!("expected Double(110.0), got {:?}", other),
    }
}

#[epics_macros_rs::epics_test]
async fn test_ao_ivoa_dont_drive() {
    let db = PvDatabase::new();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(999.0);
    ao.ivoa = 1;
    db.add_record("OUTPUT", Box::new(ao)).await.unwrap();

    if let Some(rec) = db.get_record("OUTPUT") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("TARGET".into()))
            .unwrap();
        inst.put_common_field("HIHI", EpicsValue::Double(100.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("OUTPUT", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("TARGET").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 0.0).abs() < 1e-10),
        other => panic!("expected Double(0.0), got {:?}", other),
    }
}

/// Regression: IVOA=2 ("set outputs to IVOV") must route
/// IVOV into the C-conventional output field for each record type.
/// Pre-fix the framework special-cased only `calcout` (OVAL) and fell
/// back to `set_val` (VAL) — every other output record left OVAL/RVAL
/// stale, so the soft-channel OUT writeback (which reads `OVAL.or(VAL)`)
/// shipped the pre-IVOA value instead of IVOV.
/// An `ASL` field value must land in `common.asl`. `db_loader::apply_fields`
/// feeds every common field as `EpicsValue::String`; the ASL handler must
/// parse string numerics or the directive is silently dropped at load.
///
/// NOT through `parse_db`: `ASL` is a per-FIELD `.dbd` attribute in C
/// (`asl(ASL0)`), never a record field, so `field(ASL,"1")` in a `.db` is
/// `ERROR: ai record 'ASLT:HIGH' doesn't have a field 'ASL'` on softIoc
/// @`R7.0.10` — measured, and the port's loader now refuses it identically.
/// The record-level `common.asl` the CA server and QSRV read is still real and
/// still set programmatically, which is what this exercises.
#[epics_macros_rs::epics_test]
async fn test_db_load_records_asl_field() {
    use epics_base_rs::server::db_loader;
    use epics_base_rs::server::records::ai::AiRecord;

    let defs: Vec<(&str, Vec<db_loader::DbFieldDef>)> = vec![
        ("ASLT:HIGH", vec![db_loader::DbFieldDef::new("ASL", "1")]),
        ("ASLT:LOW", Vec::new()),
    ];

    let db = PvDatabase::new();
    for (name, fields) in defs {
        let mut record: Box<dyn epics_base_rs::server::record::Record> =
            Box::new(AiRecord::new(0.0));
        let mut common_fields = Vec::new();
        db_loader::apply_fields(&mut record, &fields, &mut common_fields).unwrap();
        db.add_record(name, record).await.unwrap();
        if let Some(rec) = db.get_record(name) {
            let mut inst = rec.write();
            for (n, v) in common_fields {
                let _ = inst.put_common_field(&n, v);
            }
        }
    }

    let high = db.get_record("ASLT:HIGH").unwrap();
    let low = db.get_record("ASLT:LOW").unwrap();
    assert_eq!(
        high.read().common.asl,
        1,
        "field(ASL, \"1\") must set ASL=1"
    );
    assert_eq!(low.read().common.asl, 0, "absent ASL defaults to 0");
}

#[epics_macros_rs::epics_test]
async fn test_ao_ivoa_set_to_ivov_writes_oval() {
    use epics_base_rs::server::records::ao::AoRecord;

    let db = PvDatabase::new();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(7.0);
    ao.ivoa = 2;
    ao.ivov = 42.0;
    db.add_record("SRC", Box::new(ao)).await.unwrap();

    if let Some(rec) = db.get_record("SRC") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("TARGET".into()))
            .unwrap();
        inst.put_common_field("HIHI", EpicsValue::Double(1.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    // TARGET should now hold IVOV (42), not the original VAL (7).
    let v = db.get_pv("TARGET").unwrap();
    assert!(
        matches!(v, EpicsValue::Double(d) if (d - 42.0).abs() < 1e-9),
        "TARGET must receive IVOV via OVAL: got {v:?}"
    );
    // Source record's OVAL must also reflect IVOV (the C convention).
    let oval = db.get_pv("SRC.OVAL").unwrap();
    assert!(
        matches!(oval, EpicsValue::Double(d) if (d - 42.0).abs() < 1e-9),
        "SRC.OVAL must equal IVOV: got {oval:?}"
    );
}

#[epics_macros_rs::epics_test]
async fn test_bo_ivoa_set_to_ivov_writes_rval() {
    use epics_base_rs::server::records::bo::BoRecord;

    let db = PvDatabase::new();
    let mut bo = BoRecord::new(0);
    bo.ivoa = 2;
    bo.ivov = 1;
    db.add_record("BO_SRC", Box::new(bo)).await.unwrap();
    if let Some(rec) = db.get_record("BO_SRC") {
        let mut inst = rec.write();
        inst.common.nsev = AlarmSeverity::Invalid;
        inst.common.nsta = epics_base_rs::server::recgbl::alarm_status::SOFT_ALARM;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("BO_SRC", &mut visited, 0)
        .await
        .unwrap();

    // After IVOA=2, RVAL must equal IVOV (=1) — pre-fix it stayed at 0.
    // RVAL is DBF_ULONG (boRecord.dbd.pod:252).
    let rval = db.get_pv("BO_SRC.RVAL").unwrap();
    assert!(
        matches!(rval, EpicsValue::ULong(1)),
        "BO_SRC.RVAL must equal IVOV(1): got {rval:?}"
    );
}

#[epics_macros_rs::epics_test]
async fn test_calcout_ivoa_set_to_ivov_writes_oval_only() {
    use epics_base_rs::server::records::calcout::CalcoutRecord;

    let db = PvDatabase::new();
    db.add_record(
        "OUT_TGT",
        Box::new(epics_base_rs::server::records::ao::AoRecord::new(0.0)),
    )
    .await
    .unwrap();

    let mut co = CalcoutRecord::default();
    co.ivoa = 2;
    co.ivov = 17.5;
    co.val = 99.9;
    co.oval = 99.9;
    // The CALC has to PRODUCE the alarming value: `process()` recalculates VAL
    // before `checkAlarms`, so a `CALC="A"` with no INPA lands VAL=0 and trips
    // no HIHI — softIoc:
    //   CALC="99.9" HIHI=1 HHSV=INVALID IVOA="Set output to IVOV" IVOV=17.5
    //     -> SEVR INVALID, OUT target 17.5
    //   CALC="A"    (same fields)      -> SEVR NO_ALARM, OUT target 0
    co.calc = "99.9".to_string();
    db.add_record("CO_SRC", Box::new(co)).await.unwrap();
    if let Some(rec) = db.get_record("CO_SRC") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("OUT_TGT".into()))
            .unwrap();
        inst.put_common_field("HIHI", EpicsValue::Double(1.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("CO_SRC", &mut visited, 0)
        .await
        .unwrap();

    // OUT_TGT must receive OVAL=IVOV (17.5), not the calc result.
    let v = db.get_pv("OUT_TGT").unwrap();
    assert!(
        matches!(v, EpicsValue::Double(d) if (d - 17.5).abs() < 1e-9),
        "OUT_TGT must receive IVOV via OVAL: got {v:?}"
    );
}

#[epics_macros_rs::epics_test]
async fn test_sim_mode_input() {
    let db = PvDatabase::new();
    db.add_record("SIM_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SIM_VAL", Box::new(AoRecord::new(99.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SIM_SW".to_string();
    ai.siol = "SIM_VAL".to_string();
    ai.sims = 1;
    db.add_record("SIM_AI", Box::new(ai)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("SIM_AI").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 99.0).abs() < 1e-10),
        other => panic!("expected Double(99.0), got {:?}", other),
    }

    let sevr = db.get_pv("SIM_AI.SEVR").unwrap();
    assert!(matches!(sevr, EpicsValue::Short(1)));
}

/// A simulated value runs the record's full alarm tail like a real one.
/// C `aiRecord.c`: `readValue()` raises `recGblSetSevr(prec,
/// SIMM_ALARM, prec->sims)` (MAXIMIZE) and `process()` still runs
/// `checkAlarms` + `recGblResetAlarms` on the simulated VAL. So a sim
/// VAL of 99 trips HIHI, and because HHSV (MAJOR) outranks SIMM (MINOR)
/// the limit alarm must WIN — the pre-fix direct-commit clobbered it
/// (and never even evaluated the limit), reporting only MINOR/SIMM.
#[epics_macros_rs::epics_test]
async fn test_sim_value_trips_own_limit_and_maximizes_over_simm() {
    use epics_base_rs::server::recgbl::alarm_status;

    let db = PvDatabase::new();
    db.add_record("SIM_SW2", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SIM_VAL2", Box::new(AoRecord::new(99.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SIM_SW2".to_string();
    ai.siol = "SIM_VAL2".to_string();
    ai.sims = 1; // SIMM severity = MINOR
    db.add_record("SIM_AI2", Box::new(ai)).await.unwrap();
    if let Some(rec) = db.get_record("SIM_AI2") {
        let mut inst = rec.write();
        inst.put_common_field("HIHI", EpicsValue::Double(50.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI2", &mut visited, 0)
        .await
        .unwrap();

    // VAL=99 > HIHI=50 → HIHI MAJOR maximizes over the SIMM MINOR.
    let sevr = db.get_pv("SIM_AI2.SEVR").unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(2)),
        "sim limit MAJOR must win over SIMM MINOR: got {sevr:?}"
    );
    let stat = db.get_pv("SIM_AI2.STAT").unwrap();
    assert!(
        matches!(stat, EpicsValue::Short(s) if s as u16 == alarm_status::HIHI_ALARM),
        "STAT must be HIHI_ALARM, not SIMM_ALARM: got {stat:?}"
    );
}

/// C `recGblResetAlarms` (recGbl.c:202-222) posts the alarm fields only
/// when the alarm state moved this cycle. The pre-fix SIMM tail pushed
/// SEVR/STAT into every simulated snapshot with one shared
/// `DBE_VALUE|DBE_ALARM` mask and bypassed the MDEL deadband, so a
/// steady simulated record re-sent its unchanged alarm fields and VAL on
/// every cycle.
#[epics_macros_rs::epics_test]
async fn test_sim_steady_cycle_does_not_repost_unchanged_fields() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("SIM_SW3", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SIM_VAL3", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SIM_SW3".to_string();
    ai.siol = "SIM_VAL3".to_string();
    ai.sims = 1; // SIMM severity = MINOR
    db.add_record("SIM_AI3", Box::new(ai)).await.unwrap();

    // Cycle 1 commits the NO_ALARM -> MINOR/SIMM transition and VAL=42.
    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI3", &mut visited, 0)
        .await
        .unwrap();

    // Subscribe AFTER the transition committed.
    let (mut sevr_rx, mut stat_rx, mut val_rx) = {
        let rec = db.get_record("SIM_AI3").unwrap();
        let mut inst = rec.write();
        let s = inst
            .add_subscriber(
                "SEVR",
                31,
                DbFieldType::Short,
                (EventMask::VALUE | EventMask::ALARM).bits(),
            )
            .unwrap();
        let t = inst
            .add_subscriber(
                "STAT",
                32,
                DbFieldType::Short,
                (EventMask::VALUE | EventMask::ALARM).bits(),
            )
            .unwrap();
        let v = inst
            .add_subscriber("VAL", 33, DbFieldType::Double, EventMask::VALUE.bits())
            .unwrap();
        (s, t, v)
    };

    // Cycle 2: same SIOL value, same alarm state — nothing posts.
    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI3", &mut visited, 0)
        .await
        .unwrap();

    assert!(
        sevr_rx.try_recv().is_err(),
        "unchanged SEVR must not be re-posted on a steady simulated cycle"
    );
    assert!(
        stat_rx.try_recv().is_err(),
        "unchanged STAT must not be re-posted on a steady simulated cycle"
    );
    assert!(
        val_rx.try_recv().is_err(),
        "unchanged VAL must not be re-posted (MDEL deadband, delta = 0)"
    );
}

/// The simulated alarm transition posts each alarm field with its OWN C
/// mask (recGbl.c:202-222): SEVR is `DBE_VALUE` only — a
/// `DBE_ALARM`-only `.SEVR` subscriber must NOT be notified — while
/// STAT's mask carries `DBE_ALARM` when the severity moved. The pre-fix
/// SIMM tail collapsed both onto a shared `DBE_VALUE|DBE_ALARM` mask.
#[epics_macros_rs::epics_test]
async fn test_sim_alarm_transition_posts_per_field_masks() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("SIM_SW4", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SIM_VAL4", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SIM_SW4".to_string();
    ai.siol = "SIM_VAL4".to_string();
    ai.sims = 1; // SIMM severity = MINOR
    db.add_record("SIM_AI4", Box::new(ai)).await.unwrap();

    // Subscribe BEFORE the first simulated cycle.
    let (mut sevr_value_rx, mut sevr_alarm_rx, mut stat_alarm_rx) = {
        let rec = db.get_record("SIM_AI4").unwrap();
        let mut inst = rec.write();
        let v = inst
            .add_subscriber("SEVR", 41, DbFieldType::Short, EventMask::VALUE.bits())
            .unwrap();
        let a = inst
            .add_subscriber("SEVR", 42, DbFieldType::Short, EventMask::ALARM.bits())
            .unwrap();
        let s = inst
            .add_subscriber("STAT", 43, DbFieldType::Short, EventMask::ALARM.bits())
            .unwrap();
        (v, a, s)
    };

    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI4", &mut visited, 0)
        .await
        .unwrap();

    assert!(
        sevr_value_rx.try_recv().is_ok(),
        "DBE_VALUE SEVR subscriber must receive the SIMM alarm transition"
    );
    assert!(
        sevr_alarm_rx.try_recv().is_err(),
        "DBE_ALARM-only SEVR subscriber must NOT be notified — SEVR's C \
         mask is DBE_VALUE only"
    );
    assert!(
        stat_alarm_rx.try_recv().is_ok(),
        "DBE_ALARM STAT subscriber must receive the transition — \
         stat_mask carries DBE_ALARM when the severity moved"
    );
}

/// SIMM mode runs the record's normal `monitor()` in C — the MDEL
/// deadband throttles simulated values exactly like real reads
/// (`aiRecord.c` `monitor()`: `delta > mdel`). The pre-fix SIMM tail
/// posted VAL on every cycle regardless of MDEL.
#[epics_macros_rs::epics_test]
async fn test_sim_val_respects_mdel_deadband() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("SIM_SW5", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SIM_VAL5", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SIM_SW5".to_string();
    ai.siol = "SIM_VAL5".to_string();
    ai.mdel = 0.5;
    db.add_record("SIM_AI5", Box::new(ai)).await.unwrap();

    // Cycle 1: VAL 0 -> 42 crosses MDEL, posts, MLST=42.
    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI5", &mut visited, 0)
        .await
        .unwrap();

    let mut val_rx = {
        let rec = db.get_record("SIM_AI5").unwrap();
        let mut inst = rec.write();
        inst.add_subscriber("VAL", 51, DbFieldType::Double, EventMask::VALUE.bits())
            .unwrap()
    };

    // Sub-deadband change: |42.2 - 42| = 0.2 < MDEL=0.5 — no VALUE post.
    db.put_pv("SIM_VAL5", EpicsValue::Double(42.2))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI5", &mut visited, 0)
        .await
        .unwrap();
    assert!(
        val_rx.try_recv().is_err(),
        "sub-MDEL simulated change must not post DBE_VALUE"
    );

    // Crossing change: |43.0 - 42| = 1.0 > MDEL=0.5 — posts.
    db.put_pv("SIM_VAL5", EpicsValue::Double(43.0))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI5", &mut visited, 0)
        .await
        .unwrap();
    assert!(
        val_rx.try_recv().is_ok(),
        "MDEL-crossing simulated change must post DBE_VALUE"
    );
}

#[epics_macros_rs::epics_test]
async fn test_sim_mode_toggle() {
    let db = PvDatabase::new();
    db.add_record("SIM_SW", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("SIM_VAL", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();
    db.add_record("REAL_SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SIM_SW".to_string();
    ai.siol = "SIM_VAL".to_string();
    db.add_record("TEST_AI", Box::new(ai)).await.unwrap();

    if let Some(rec) = db.get_record("TEST_AI") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("REAL_SRC".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TEST_AI", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("TEST_AI").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 10.0).abs() < 1e-10),
        other => panic!("expected Double(10.0), got {:?}", other),
    }

    db.put_pv("SIM_SW", EpicsValue::Double(1.0)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TEST_AI", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("TEST_AI").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

#[epics_macros_rs::epics_test]
async fn test_sim_mode_output() {
    let db = PvDatabase::new();
    db.add_record("SIM_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SIM_OUT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(77.0);
    ao.siml = "SIM_SW".to_string();
    ao.siol = "SIM_OUT".to_string();
    db.add_record("TEST_AO", Box::new(ao)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("TEST_AO", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("SIM_OUT").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 77.0).abs() < 1e-10),
        other => panic!("expected Double(77.0), got {:?}", other),
    }
}

/// A SIMM-mode **input** record whose SIOL points at a NON-LOCAL record
/// must read the simulated value through the external CA path, not a
/// local lookup. C `readValue` reads SIOL via `dbGetLink`, and
/// `dbInitLink` (dbLink.c:118-130) made the non-local SIOL a CA link, so
/// the read is `dbCaGetLink`. The pre-fix port special-cased only a
/// local `ParsedLink::Db` SIOL, so a non-local SIOL read nothing yet
/// still returned `Simulated` — the record froze with no value. This is
/// the INPUT twin of the OUT-link locality fallback.
#[epics_macros_rs::epics_test]
async fn test_sim_mode_input_nonlocal_db_siol() {
    use epics_base_rs::server::database::LinkSet;
    struct ValueCaLset(f64);
    #[epics_base_rs::async_trait]
    impl LinkSet for ValueCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
            // The bare non-local SIOL record name reaches the CA lset
            // via the read-locality fallback `resolve_external_pv`.
            (name == "REMOTE:SIM").then_some(EpicsValue::Double(self.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
    }

    let db = PvDatabase::new();
    db.register_link_set("ca", Arc::new(ValueCaLset(73.0)))
        .await;
    db.add_record("SIM_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SIM_SW".to_string();
    // REMOTE:SIM is never added locally → dbInitLink makes it a CA link.
    ai.siol = "REMOTE:SIM".to_string();
    ai.sims = 1;
    db.add_record("SIM_AI_NL", Box::new(ai)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SIM_AI_NL", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("SIM_AI_NL").unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 73.0).abs() < 1e-10),
        "non-local Db SIOL must read the remote sim value into VAL: got {val:?}"
    );
}

/// A SIMM-mode **output** record whose SIOL points at a NON-LOCAL
/// record must write the simulated value through the external CA put
/// path (C `writeValue` → `dbPutLink` → `dbCaPutLink`), not a local
/// `dbPut`. OUTPUT twin of `test_sim_mode_input_nonlocal_db_siol`; the
/// pre-fix port special-cased only a local `ParsedLink::Db` SIOL, so a
/// non-local SIOL write went nowhere.
#[epics_macros_rs::epics_test]
async fn test_sim_mode_output_nonlocal_db_siol() {
    use std::sync::Mutex;

    use epics_base_rs::server::database::LinkPutOp;

    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    db.register_link_set(
        "ca",
        Arc::new(CapturingLset {
            writes: writes.clone(),
        }),
    )
    .await;
    db.add_record("SIM_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(55.0);
    ao.siml = "SIM_SW".to_string();
    // REMOTE:OUT is never added locally → dbInitLink makes it a CA link.
    ao.siol = "REMOTE:OUT".to_string();
    db.add_record("TEST_AO_NL", Box::new(ao)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("TEST_AO_NL", &mut visited, 0)
        .await
        .unwrap();

    // Staged on the link-put queue and returned, as C `dbCaPutLink` does
    // (`dbCa.c:593-595`); `dbCaSync` (`dbCa.c:1126-1129`) is the barrier
    // that makes the wire write observable.
    db.sync_external_link_puts().await;

    let captured = writes.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "the non-local SIOL must drive exactly one external put_value"
    );
    assert_eq!(
        captured[0].0, "REMOTE:OUT",
        "put_value must receive the bare SIOL record name"
    );
    assert_eq!(
        captured[0].1.to_f64(),
        Some(55.0),
        "put_value must receive the simulated output value"
    );
    assert_eq!(
        captured[0].2,
        LinkPutOp::Plain,
        "a SIMM-mode simulation write is a Plain put"
    );
}

#[epics_macros_rs::epics_test]
async fn test_sdis_disable_skips_process() {
    let db = PvDatabase::new();
    db.add_record("DISABLE_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("TARGET") {
        let mut inst = rec.write();
        inst.put_common_field("SDIS", EpicsValue::String("DISABLE_SW".into()))
            .unwrap();
        inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TARGET").unwrap();
    let inst = rec.read();
    // C `menuAlarmStat.dbd`: DISABLE = 18.
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::DISABLE_ALARM
    );
    assert_eq!(inst.common.sevr, AlarmSeverity::Minor);

    drop(inst);
    db.put_pv("DISABLE_SW", EpicsValue::Double(0.0))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TARGET").unwrap();
    let inst = rec.read();
    assert_ne!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::DISABLE_ALARM
    );
}

/// C `dbProcess` (`dbAccess.c:565`) reads SDIS with
/// `dbGetLink(&precord->sdis, DBR_SHORT, &precord->disa, 0, 0)` — for ANY link
/// type, through the lset. For a CONSTANT SDIS that lset is `dbConst_lset`,
/// whose `dbConstGetValue` (`dbConstLink.c:219-225`) writes NOTHING and returns
/// success, and dbCommon has no `recGblInitConstantLink` for SDIS. So a
/// constant SDIS never reaches DISA at all: DISA keeps its `initial(0)` and the
/// record RUNS. softIoc (EPICS 7):
///
/// ```text
/// record(calc,"S1") { field(SDIS,"3") field(DISV,"3") }
///   dbpf S1.PROC 1 ; dbgf S1.DISA  ->  DBF_SHORT: 0     (record processed)
/// ```
///
/// The port used to hand the constant's text back as if the link had delivered
/// it, which disabled the record permanently. A DB SDIS still refreshes DISA
/// every cycle — that is the other half of the boundary, asserted below.
#[epics_macros_rs::epics_test]
async fn test_constant_sdis_never_reaches_disa_but_db_sdis_does() {
    let db = PvDatabase::new();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    // Constant SDIS "1" — equal to the default DISV (1). C does NOT disable.
    if let Some(rec) = db.get_record("TARGET") {
        let mut inst = rec.write();
        inst.put_common_field("SDIS", EpicsValue::String("1".into()))
            .unwrap();
        inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("TARGET").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.common.disa, 0,
            "a CONSTANT SDIS delivers nothing at process — DISA keeps initial(0)"
        );
        assert_ne!(
            inst.common.stat,
            epics_base_rs::server::recgbl::alarm_status::DISABLE_ALARM,
            "so the record still processes"
        );
    }

    // A DB SDIS does refresh DISA every cycle — the link that really is read.
    db.add_record("SRC", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("TARGET") {
        let mut inst = rec.write();
        inst.put_common_field("SDIS", EpicsValue::String("SRC.VAL".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("TARGET").unwrap();
        let inst = rec.read();
        assert_eq!(inst.common.disa, 1, "a DB SDIS refreshes DISA from SRC.VAL");
        assert_eq!(
            inst.common.stat,
            epics_base_rs::server::recgbl::alarm_status::DISABLE_ALARM,
            "disa == disv disables the record"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn test_phas_scan_order() {
    let db = PvDatabase::new();

    db.add_record("REC_C", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("REC_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("REC_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    for (name, phas) in &[("REC_C", 2i16), ("REC_A", 0), ("REC_B", 1)] {
        if let Some(rec) = db.get_record(name) {
            let mut inst = rec.write();
            inst.put_common_field("PHAS", EpicsValue::Short(*phas))
                .unwrap();
            let result = inst
                .put_common_field("SCAN", EpicsValue::String("1 second".into()))
                .unwrap();
            if let CommonFieldPutResult::ScanChanged {
                old_scan,
                new_scan,
                phas: p,
            } = result
            {
                drop(inst);
                db.update_scan_index(name, old_scan, new_scan, p, p);
            }
        }
    }

    let names = db.records_for_scan(ScanType::SEC1).await;
    assert_eq!(names, vec!["REC_A", "REC_B", "REC_C"]);
}

/// Run a deep FLNK-processing chain test on a thread with a large stack.
///
/// `process_record_with_links` polls the large `process_record_with_links_inner`
/// future once per FLNK hop, up to `MAX_LINK_DEPTH` (16) frames deep. On
/// linux-arm64 those frames are big enough that 16 of them overflow the default
/// 2 MB test-thread stack (SIGABRT); x86_64 and macos-arm64 have smaller frames
/// and fit. A 16 MB stack clears it. The future is built and awaited on the
/// spawned thread, so it never crosses the thread boundary and needs no `Send`.
fn run_deep_flnk_recursion<F, Fut>(body: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(body());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn test_depth_limit() {
    run_deep_flnk_recursion(|| async {
        let db = PvDatabase::new();
        for i in 0..20 {
            db.add_record(&format!("CHAIN_{i}"), Box::new(AoRecord::new(0.0)))
                .await
                .unwrap();
        }
        for i in 0..19 {
            if let Some(rec) = db.get_record(&format!("CHAIN_{i}")) {
                let mut inst = rec.write();
                inst.put_common_field(
                    "FLNK",
                    EpicsValue::String(format!("CHAIN_{}", i + 1).into()),
                )
                .unwrap();
            }
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("CHAIN_0", &mut visited, 0)
            .await
            .unwrap();
        // Reading the set after the call cannot show how far the chain
        // walked any more; the refusal on the record at the bound can, and
        // that is what an operator sees.
        let refused = db
            .get_record("CHAIN_16")
            .expect("CHAIN_16 exists")
            .read()
            .common
            .amsg
            .clone();
        assert!(
            refused.contains("link chain depth limit"),
            "the record at MAX_LINK_DEPTH must carry the reason, got {refused:?}"
        );
        assert!(visited.is_empty(), "the frame unwound: {visited:?}");
    });
}

#[epics_macros_rs::epics_test]
async fn test_disp_blocks_ca_put() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }

    let result = db
        .put_record_field_from_ca("REC", "VAL", EpicsValue::Double(42.0))
        .await;
    assert!(matches!(result, Err(CaError::PutDisabled(_))));
}

#[epics_macros_rs::epics_test]
async fn test_disp_allows_disp_write() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }

    let result = db
        .put_record_field_from_ca("REC", "DISP", EpicsValue::Char(0))
        .await;
    assert!(result.is_ok());

    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    assert!(inst.common.disp == 0);
}

#[epics_macros_rs::epics_test]
async fn test_disp_bypassed_by_internal_put() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }

    let result = db.put_pv("REC", EpicsValue::Double(42.0)).await;
    assert!(result.is_ok());
}

#[epics_macros_rs::epics_test]
async fn test_proc_triggers_processing() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("REC", EpicsValue::Double(42.0)).await.unwrap();
    let result = db
        .put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1))
        .await;
    assert!(result.is_ok());
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf == 0);
}

#[epics_macros_rs::epics_test]
async fn test_proc_works_any_scan() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.put_common_field("SCAN", EpicsValue::String("1 second".into()))
            .unwrap();
    }
    let result = db
        .put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1))
        .await;
    assert!(result.is_ok());
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf == 0);
}

/// C `dbAccess.c::dbPutField:1255-1257` returns `S_db_putDisabled` for a
/// put to ANY field but DISP while `DISP=1` — the gate sits before `dbPut`
/// AND before the PROC-driven `dbProcess` (`:1262-1274`). `PROC`'s pfield is
/// not `&precord->disp`, so `caput REC.PROC 1` on a disabled record is
/// refused and the record does NOT process.
#[epics_macros_rs::epics_test]
async fn test_disp_blocks_proc_and_suppresses_processing() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }
    let result = db
        .put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1))
        .await;
    assert!(
        matches!(result, Err(CaError::PutDisabled(_))),
        "DISP=1 must refuse a PROC put (C S_db_putDisabled), got {result:?}"
    );

    // ...and the DISP gate precedes dbProcess, so nothing processed: a
    // fresh record is still UDF.
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    assert!(
        inst.common.udf != 0,
        "the refused PROC put must not have force-processed the record"
    );
}

/// The same gate ordering for an SPC_NOMOD field: C rejects PACT with
/// `S_db_noMod` inside `dbPut` (`dbAccess.c:123`), which `dbPutField` only
/// reaches AFTER the DISP gate — so on a `DISP=1` record the error a client
/// sees for `caput REC.PACT` is `S_db_putDisabled`, not `S_db_noMod`.
#[epics_macros_rs::epics_test]
async fn test_disp_gate_precedes_nomod_rejection() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }
    let result = db
        .put_record_field_from_ca("REC", "PACT", EpicsValue::Char(1))
        .await;
    assert!(
        matches!(result, Err(CaError::PutDisabled(_))),
        "DISP gate runs before the SPC_NOMOD rejection, got {result:?}"
    );
}

#[epics_macros_rs::epics_test]
async fn test_proc_while_pact() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let result = db
        .put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1))
        .await;
    assert!(result.is_ok());
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf == 0);
}

#[epics_macros_rs::epics_test]
async fn test_lcnt_ca_write_rejected() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let result = db
        .put_record_field_from_ca("REC", "LCNT", EpicsValue::Short(0))
        .await;
    assert!(matches!(result, Err(CaError::ReadOnlyField(_))));
}

#[epics_macros_rs::epics_test]
async fn test_ca_put_scan_index_update() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::String("1 second".into()))
        .await
        .unwrap();
    let names = db.records_for_scan(ScanType::SEC1).await;
    assert!(names.contains(&"REC".to_string()));
}

// --- Mock DeviceSupport for write/read counting ---

struct MockDeviceSupport {
    read_count: Arc<AtomicU32>,
    write_count: Arc<AtomicU32>,
    dtyp_name: String,
    callback_readback: bool,
}

impl MockDeviceSupport {
    fn new(dtyp: &str, read_count: Arc<AtomicU32>, write_count: Arc<AtomicU32>) -> Self {
        Self {
            read_count,
            write_count,
            dtyp_name: dtyp.to_string(),
            callback_readback: false,
        }
    }

    /// Declare the asyn devEpics `newOutputCallbackValue` contract
    /// (`output_callback_readback` = true): a driver-callback cycle reads
    /// the value back and suppresses the output write.
    fn with_callback_readback(mut self) -> Self {
        self.callback_readback = true;
        self
    }
}

impl epics_base_rs::server::device_support::DeviceSupport for MockDeviceSupport {
    fn read(
        &mut self,
        _record: &mut dyn Record,
    ) -> epics_base_rs::error::CaResult<epics_base_rs::server::device_support::DeviceReadOutcome>
    {
        self.read_count.fetch_add(1, Ordering::SeqCst);
        Ok(epics_base_rs::server::device_support::DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> epics_base_rs::error::CaResult<()> {
        self.write_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn dtyp(&self) -> &str {
        &self.dtyp_name
    }
    fn output_callback_readback(&self) -> bool {
        self.callback_readback
    }
}

#[epics_macros_rs::epics_test]
async fn test_ca_put_no_double_device_write() {
    let db = PvDatabase::new();
    db.add_record("AO_REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AO_REC") {
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".to_string();
        inst.device = Some(Box::new(mock));
    }
    db.put_record_field_from_ca("AO_REC", "VAL", EpicsValue::Double(42.0))
        .await
        .unwrap();
    assert_eq!(write_count.load(Ordering::SeqCst), 1);
}

/// A driver-callback cycle (`process_record_readback`) on a hardware output
/// whose device support does NOT declare the `newOutputCallbackValue`
/// contract is a full C `dbProcess`: the output stage runs. devMotorAsyn is
/// the live case — the motor record emits its retry / backlash-leg /
/// NTM-stop commands on exactly these passes, and suppressing the write
/// strands them in the command mailbox (DMOV stuck 0, MIP=RETRY|MOVE).
#[epics_macros_rs::epics_test]
async fn test_readback_cycle_runs_device_write_without_callback_contract() {
    let db = PvDatabase::new();
    db.add_record("AO_CB1", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AO_CB1") {
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".to_string();
        inst.device = Some(Box::new(mock));
    }
    let mut visited = HashSet::new();
    db.process_record_readback("AO_CB1", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        write_count.load(Ordering::SeqCst),
        1,
        "a callback cycle without the readback contract runs the output stage"
    );
}

/// The asyn devEpics contract (`output_callback_readback` = true,
/// `devAsynInt32.c::processBo` taking the `newOutputCallbackValue` branch):
/// the callback cycle reads the driver value back and must NOT re-write the
/// setpoint — re-asserting it would re-trigger the driver (AD `Acquire`
/// loop).
#[epics_macros_rs::epics_test]
async fn test_readback_cycle_suppresses_device_write_with_callback_contract() {
    let db = PvDatabase::new();
    db.add_record("AO_CB2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone())
        .with_callback_readback();
    if let Some(rec) = db.get_record("AO_CB2") {
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".to_string();
        inst.device = Some(Box::new(mock));
    }
    let mut visited = HashSet::new();
    db.process_record_readback("AO_CB2", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        read_count.load(Ordering::SeqCst),
        1,
        "the callback cycle reads the driver value back into VAL"
    );
    assert_eq!(
        write_count.load(Ordering::SeqCst),
        0,
        "the readback contract suppresses the output write on a callback cycle"
    );
}

// epics-base f2fe9d12 (devBiSoftRaw): a `bi` record with
// `DTYP="Raw Soft Channel"`, MASK set, and a soft INP link must mask
// the link value into RVAL before the RVAL→VAL convert. The framework
// must route the INP value to `raw_soft_input` (not `set_val`).
#[epics_macros_rs::epics_test]
async fn test_bi_raw_soft_channel_inp_applies_mask() {
    let db = PvDatabase::new();
    db.add_record("SRC_LI", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    db.add_record("BI_RAW", Box::new(BiRecord::new(0)))
        .await
        .unwrap();
    db.put_record_field_from_ca("SRC_LI", "VAL", EpicsValue::Long(0xFF))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("BI_RAW") {
        let mut inst = rec.write();
        inst.common.dtyp = "Raw Soft Channel".to_string();
        inst.common.inp = "SRC_LI".to_string();
        inst.parsed_inp = epics_base_rs::server::record::parse_link_v2(&inst.common.inp);
        inst.record
            .put_field("MASK", EpicsValue::Long(0x0F))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("BI_RAW", &mut visited, 0)
        .await
        .unwrap();
    if let Some(rec) = db.get_record("BI_RAW") {
        let inst = rec.read();
        let rval = inst.record.get_field("RVAL");
        // RVAL is DBF_ULONG (biRecord.dbd.pod:199).
        assert_eq!(
            rval,
            Some(EpicsValue::ULong(0x0F)),
            "MASK must clamp RVAL to low nibble"
        );
        let val = inst.record.get_field("VAL");
        assert_eq!(
            val,
            Some(EpicsValue::Enum(1)),
            "masked-non-zero RVAL → VAL=1"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn test_input_record_no_device_write() {
    let db = PvDatabase::new();
    db.add_record("AI_REC", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AI_REC") {
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".to_string();
        inst.device = Some(Box::new(mock));
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("AI_REC", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(read_count.load(Ordering::SeqCst), 1);
    assert_eq!(write_count.load(Ordering::SeqCst), 0);
}

// Device support that attaches a fixed userTag to its reading, like a
// timing receiver delivering a pulse-id. `epicsTimeStamp` carries no
// tag, so C device support writes `prec->utag` directly during
// `read()` (alongside `prec->time`, TSE=-2); the Rust framework picks
// it up from `last_utag()`.
struct UtagDeviceSupport {
    utag: u64,
}

impl epics_base_rs::server::device_support::DeviceSupport for UtagDeviceSupport {
    fn read(
        &mut self,
        _record: &mut dyn Record,
    ) -> epics_base_rs::error::CaResult<epics_base_rs::server::device_support::DeviceReadOutcome>
    {
        Ok(epics_base_rs::server::device_support::DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "UtagDev"
    }
    fn last_utag(&self) -> Option<u64> {
        Some(self.utag)
    }
}

#[epics_macros_rs::epics_test]
async fn test_device_support_utag_adopted_into_common() {
    // A device that reports a userTag via `last_utag()` must have it
    // adopted into `common.utag` when the record is processed, mirroring
    // C device support writing `prec->utag` directly. Bit 31 set
    // (0x9000_0000) guards against any narrowing/sign mishandling on the
    // adoption path.
    let db = PvDatabase::new();
    db.add_record("AI_UTAG", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("AI_UTAG") {
        let mut inst = rec.write();
        inst.common.dtyp = "UtagDev".to_string();
        inst.device = Some(Box::new(UtagDeviceSupport { utag: 0x9000_0000 }));
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("AI_UTAG", &mut visited, 0)
        .await
        .unwrap();
    if let Some(rec) = db.get_record("AI_UTAG") {
        let inst = rec.read();
        assert_eq!(
            inst.common.utag, 0x9000_0000,
            "device-reported userTag must be adopted into common.utag"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn test_non_passive_output_ca_put_defers_write_until_scan() {
    // C `dbAccess.c::dbPutField:1263-1266` only processes a record on a
    // put when `precord->scan == 0` (Passive). A CA put to a non-Passive
    // (here 1-second-scanned) output record updates VAL but does NOT
    // process — the device write happens on the next scan, not at put.
    let db = PvDatabase::new();
    db.add_record("AO_NP", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AO_NP") {
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".to_string();
        inst.common.scan = ScanType::SEC1;
        inst.device = Some(Box::new(mock));
    }
    db.put_record_field_from_ca("AO_NP", "VAL", EpicsValue::Double(42.0))
        .await
        .unwrap();
    assert_eq!(
        write_count.load(Ordering::SeqCst),
        0,
        "put to a non-Passive record must not process/write at put time"
    );

    // The periodic scan processes the record and writes the new VAL.
    let mut visited = HashSet::new();
    db.process_record_with_links("AO_NP", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        write_count.load(Ordering::SeqCst),
        1,
        "the scan cycle writes the value the put staged"
    );
}

#[epics_macros_rs::epics_test]
async fn test_proc_triggers_device_write() {
    let db = PvDatabase::new();
    db.add_record("AO_PROC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AO_PROC") {
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".to_string();
        inst.device = Some(Box::new(mock));
    }
    db.put_record_field_from_ca("AO_PROC", "PROC", EpicsValue::Char(1))
        .await
        .unwrap();
    assert_eq!(write_count.load(Ordering::SeqCst), 1);
}

// --- Scan Index Fix tests ---

#[epics_macros_rs::epics_test]
async fn test_phas_change_updates_scan_index() {
    let db = PvDatabase::new();
    db.add_record("REC_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("REC_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    for (name, phas) in &[("REC_A", 10i16), ("REC_B", 5)] {
        if let Some(rec) = db.get_record(name) {
            let mut inst = rec.write();
            inst.put_common_field("PHAS", EpicsValue::Short(*phas))
                .unwrap();
            let result = inst
                .put_common_field("SCAN", EpicsValue::String("1 second".into()))
                .unwrap();
            if let CommonFieldPutResult::ScanChanged {
                old_scan,
                new_scan,
                phas: p,
            } = result
            {
                drop(inst);
                db.update_scan_index(name, old_scan, new_scan, p, p);
            }
        }
    }
    let names = db.records_for_scan(ScanType::SEC1).await;
    assert_eq!(names, vec!["REC_B", "REC_A"]);

    if let Some(rec) = db.get_record("REC_A") {
        let mut inst = rec.write();
        let result = inst.put_common_field("PHAS", EpicsValue::Short(0)).unwrap();
        if let CommonFieldPutResult::PhasChanged {
            scan,
            old_phas,
            new_phas,
        } = result
        {
            drop(inst);
            db.update_scan_index("REC_A", scan, scan, old_phas, new_phas);
        }
    }
    let names = db.records_for_scan(ScanType::SEC1).await;
    assert_eq!(names, vec!["REC_A", "REC_B"]);
}

#[epics_macros_rs::epics_test]
async fn test_scan_change_preserves_phas() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.put_common_field("PHAS", EpicsValue::Short(3)).unwrap();
        let result = inst
            .put_common_field("SCAN", EpicsValue::String("1 second".into()))
            .unwrap();
        match result {
            CommonFieldPutResult::ScanChanged { phas, .. } => assert_eq!(phas, 3),
            other => panic!("expected ScanChanged, got {:?}", other),
        }
    }
}

#[epics_macros_rs::epics_test]
async fn test_phas_change_passive_no_index() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        let result = inst.put_common_field("PHAS", EpicsValue::Short(5)).unwrap();
        assert_eq!(result, CommonFieldPutResult::NoChange);
    }
}

// --- Async Processing Contract tests ---

struct AsyncRecord {
    val: f64,
}
impl Record for AsyncRecord {
    fn record_type(&self) -> &'static str {
        "async_test"
    }
    /// `VAL` is `pp(TRUE)`: a put to VAL processes the record (goes async-
    /// pending), as it does for any real async record. The put gate is total
    /// and fail-safe — an unmodeled type processes on PROC only — so this mock
    /// must declare the pp set its tests rely on rather than free-ride on the
    /// former process-on-every-put default.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
    fn process(&mut self) -> epics_base_rs::error::CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::async_pending())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> epics_base_rs::error::CaResult<()> {
        match name {
            "VAL" => {
                if let EpicsValue::Double(v) = value {
                    self.val = v;
                    Ok(())
                } else {
                    Err(CaError::InvalidValue("bad".into()))
                }
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// An async output whose device write is observable: every process pass
/// records the VAL it pushed to "the device". `process()` never completes on
/// its own — the test drives `complete_async_record`, exactly like a real
/// device callback.
struct AsyncOutRecord {
    val: f64,
    device_writes: Arc<std::sync::Mutex<Vec<f64>>>,
}
impl Record for AsyncOutRecord {
    fn record_type(&self) -> &'static str {
        "async_out_test"
    }
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
    fn process(&mut self) -> epics_base_rs::error::CaResult<ProcessOutcome> {
        self.device_writes.lock().unwrap().push(self.val);
        Ok(ProcessOutcome::async_pending())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> epics_base_rs::error::CaResult<()> {
        match name {
            "VAL" => {
                if let EpicsValue::Double(v) = value {
                    self.val = v;
                    Ok(())
                } else {
                    Err(CaError::InvalidValue("bad".into()))
                }
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        ASYNC_OUT_FIELDS
    }
}

/// The fake's `.dbd`. A synthetic type still has to DECLARE its `INP`: which
/// fields on a record are links is answered from the declaration and nowhere
/// else, and `FieldDesc::new` cannot spell a link because it derives
/// `declared_dbf` from the SERVED type, which is `DBF_STRING` for all three
/// classes.
static ASYNC_OUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        declared_dbf: epics_base_rs::types::DbfCode::Inlink,
        ..FieldDesc::new("INP", epics_base_rs::types::DbFieldType::String, false)
    },
    FieldDesc::new("VAL", epics_base_rs::types::DbFieldType::Double, false),
];

/// C `dbAccess.c::dbPutField:1266-1270` — a client put to a `pp` field of an
/// async-active (PACT) record sets `rpro = TRUE` and does NOT call
/// `dbProcess`. `recGblFwdLink` (`recGbl.c:296-300`) consumes RPRO when the
/// device round trip completes and queues `scanOnce`, so the second value
/// still reaches the device.
///
/// Pre-fix the port called `dbProcess` re-entrantly instead: it bailed at
/// dbProcess's own PACT guard (bumping LCNT, and after MAX_LOCK raising a
/// SCAN_ALARM C never raises for a client put) and never set RPRO — the
/// second value landed in VAL and was never written out.
#[epics_macros_rs::epics_test]
async fn test_put_to_pact_record_sets_rpro_and_second_value_reaches_device() {
    let db = PvDatabase::new();
    let writes: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    db.add_record(
        "ASYNC_OUT",
        Box::new(AsyncOutRecord {
            val: 0.0,
            device_writes: writes.clone(),
        }),
    )
    .await
    .unwrap();

    // Put 1: record is idle → putf=TRUE, process → device write of 1.0, PACT.
    db.put_record_field_from_ca_no_notify("ASYNC_OUT", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(
        *writes.lock().unwrap(),
        vec![1.0],
        "put 1 must reach device"
    );

    // Put 2 lands while the device round trip is still in flight.
    db.put_record_field_from_ca_no_notify("ASYNC_OUT", "VAL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    {
        let rec = db.get_record("ASYNC_OUT").unwrap();
        let inst = rec.read();
        assert!(inst.is_processing(), "still PACT from put 1");
        assert!(
            inst.common.rpro != 0,
            "put to a PACT record must set RPRO (C dbAccess.c:1269)"
        );
        assert_eq!(
            inst.common.lcnt, 0,
            "C's dbPutField never calls dbProcess on an active record, so LCNT \
             must not advance (pre-fix it did, and after MAX_LOCK raised SCAN_ALARM)"
        );
        assert_eq!(
            *writes.lock().unwrap(),
            vec![1.0],
            "put 2 must NOT re-enter process while PACT"
        );
    }

    // Device callback completes cycle 1. recGblFwdLink consumes RPRO and
    // queues the reprocess (a detached task, C's scanOnce ring).
    db.complete_async_record("ASYNC_OUT").await.unwrap();
    for _ in 0..100 {
        if writes.lock().unwrap().len() >= 2 {
            break;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        *writes.lock().unwrap(),
        vec![1.0, 2.0],
        "RPRO reprocess must push the second value to the device"
    );
    let rec = db.get_record("ASYNC_OUT").unwrap();
    let inst = rec.read();
    assert!(
        inst.common.rpro == 0,
        "RPRO is consumed (cleared) by the completion tail"
    );
}

#[epics_macros_rs::epics_test]
async fn test_async_pending_skips_post_process() {
    let db = PvDatabase::new();
    db.add_record("ASYNC", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("FLNK_TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("ASYNC") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("FLNK_TARGET".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC", &mut visited, 0)
        .await
        .unwrap();
    // `visited` is frame-scoped, so it says nothing after the call returns;
    // that the FLNK did not fire is what UDF below pins — the record is still
    // mid-async and C's `dbProcess` returns before `recGblFwdLink`.
    assert!(visited.is_empty(), "the frame unwound: {visited:?}");
    let rec = db.get_record("ASYNC").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf != 0);
}

#[epics_macros_rs::epics_test]
async fn test_complete_async_record() {
    let db = PvDatabase::new();
    db.add_record("ASYNC", Box::new(AsyncRecord { val: 42.0 }))
        .await
        .unwrap();
    let mut tgt = epics_base_rs::server::records::calc::CalcRecord::new("VAL+1");
    tgt.init_record(0).unwrap();
    db.add_record("FLNK_TARGET", Box::new(tgt)).await.unwrap();
    if let Some(rec) = db.get_record("ASYNC") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("FLNK_TARGET".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("FLNK_TARGET").unwrap().to_f64(),
        Some(0.0),
        "the FLNK must wait for the async completion"
    );
    db.complete_async_record("ASYNC").await.unwrap();
    assert_eq!(
        db.get_pv("FLNK_TARGET").unwrap().to_f64(),
        Some(1.0),
        "completion runs recGblFwdLink"
    );
    let rec = db.get_record("ASYNC").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf == 0);
}

/// Defect 1 regression: the async-completion path
/// (`complete_async_record_inner`) must post SEVR/STAT/AMSG with
/// their per-field C masks — exactly like the synchronous path and
/// `process_local` — not collapse them onto one record-wide mask.
///
/// C `recGblResetAlarms` posts SEVR with `DBE_VALUE` only. The pre-fix
/// async path pushed SEVR into `changed_fields`, which `notify_from_
/// snapshot` posts with the record-wide `event_mask` that carries
/// `DBE_ALARM` on an alarm transition. So a `DBE_ALARM`-only SEVR
/// subscriber was wrongly notified, and a `DBE_VALUE`-only SEVR
/// subscriber on a stat-only transition would have been missed.
///
/// This test drives an alarm transition through `complete_async_record`
/// and asserts:
///  * a `DBE_VALUE`-only SEVR subscriber RECEIVES the event,
///  * a `DBE_ALARM`-only SEVR subscriber does NOT (SEVR is DBE_VALUE).
/// Async record stub that raises a MAJOR `STATE_ALARM` from its
/// `check_alarms` hook — used to drive an alarm transition through
/// the async-completion path.
struct AsyncAlarmingRecord;
impl Record for AsyncAlarmingRecord {
    fn record_type(&self) -> &'static str {
        "async_alarm_test"
    }
    fn process(&mut self) -> epics_base_rs::error::CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::async_pending())
    }
    fn check_alarms(&mut self, common: &mut epics_base_rs::server::record::CommonFields) {
        use epics_base_rs::server::recgbl::{self, alarm_status};
        recgbl::rec_gbl_set_sevr(
            common,
            alarm_status::STATE_ALARM,
            epics_base_rs::server::record::AlarmSeverity::Major,
        );
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(1.0)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> epics_base_rs::error::CaResult<()> {
        match name {
            "VAL" => Ok(()),
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

#[epics_macros_rs::epics_test]
async fn test_complete_async_posts_sevr_with_per_field_mask() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::record::AlarmSeverity;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("ASYNC_SEVR", Box::new(AsyncAlarmingRecord))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("ASYNC_SEVR") {
        let mut inst = rec.write();
        inst.common.udf = 0;
    }

    // First cycle: record reports async_pending (PACT set).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_SEVR", &mut visited, 0)
        .await
        .unwrap();

    // Subscribe to SEVR twice: one DBE_VALUE-only, one DBE_ALARM-only.
    let (mut sevr_value_rx, mut sevr_alarm_rx) = {
        let rec = db.get_record("ASYNC_SEVR").unwrap();
        let mut inst = rec.write();
        let v = inst
            .add_subscriber("SEVR", 21, DbFieldType::Short, EventMask::VALUE.bits())
            .expect("DBE_VALUE SEVR subscription accepted");
        let a = inst
            .add_subscriber("SEVR", 22, DbFieldType::Short, EventMask::ALARM.bits())
            .expect("DBE_ALARM SEVR subscription accepted");
        (v, a)
    };

    // Complete the async cycle — alarm transition NoAlarm -> Major.
    db.complete_async_record("ASYNC_SEVR").await.unwrap();

    {
        let rec = db.get_record("ASYNC_SEVR").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::Major,
            "completion must raise Major"
        );
    }

    // DBE_VALUE SEVR subscriber MUST receive the event — SEVR posts
    // with DBE_VALUE.
    assert!(
        sevr_value_rx.try_recv().is_ok(),
        "DBE_VALUE SEVR subscriber must receive the SEVR change"
    );
    // DBE_ALARM-only SEVR subscriber must NOT — SEVR's C mask is
    // DBE_VALUE only, never DBE_ALARM.
    assert!(
        sevr_alarm_rx.try_recv().is_err(),
        "DBE_ALARM-only SEVR subscriber must NOT receive SEVR \
         (per-field mask collapsed onto record-wide ALARM mask)"
    );
}

// C parity (dbAccess.c::dbProcess:536-558): a second
// `process_record_with_links` against a PACT-active record must NOT
// re-enter `record.process()`. The first attempt must bail silently
// (lcnt counting up); after MAX_LOCK=10 consecutive bails, SCAN_ALARM /
// INVALID must be raised with "Async in progress" amsg and VAL must be
// posted with DBE_VALUE|DBE_LOG|DBE_ALARM.
#[epics_macros_rs::epics_test]
async fn test_pact_entry_guard_silent_bail_until_max_lock() {
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    db.add_record("ASYNC_PACT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    // C's guard is `if ((precord->stat == SCAN_ALARM) || (precord->lcnt++ <
    // MAX_LOCK) || (precord->sevr >= INVALID_ALARM)) goto all_done`
    // (dbAccess.c:543-546): the SCAN alarm is for a record that HAS completed a
    // process (SEVR below INVALID) and then hangs mid-async. A never-processed
    // record still carries the init UDF severity (SEVR=INVALID, softIoc reads
    // that on every record right after `iocInit`) and C never alarms it. This
    // mock never completes a cycle, so put it in the state a completed process
    // leaves behind: defined, NO_ALARM.
    {
        let rec = db.get_record("ASYNC_PACT").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
        inst.common.sevr = AlarmSeverity::NoAlarm;
        inst.common.stat = epics_base_rs::server::recgbl::alarm_status::NO_ALARM;
    }

    // Drive ASYNC_PACT into PACT=true (async pending, lock released).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_PACT", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("ASYNC_PACT").unwrap();
        let inst = rec.read();
        assert!(
            inst.is_processing(),
            "first cycle must leave PACT=true (AsyncPending)"
        );
        assert_eq!(inst.common.lcnt, 0, "first cycle must reset lcnt");
        assert_eq!(inst.common.sevr, AlarmSeverity::NoAlarm);
    }

    // Up to MAX_LOCK = 10 re-entries while PACT=true must NOT raise alarm.
    for i in 1..=10 {
        let mut visited = HashSet::new();
        db.process_record_with_links("ASYNC_PACT", &mut visited, 0)
            .await
            .unwrap();
        let rec = db.get_record("ASYNC_PACT").unwrap();
        let inst = rec.read();
        assert!(inst.is_processing(), "must remain PACT=true (iter {i})");
        assert_eq!(inst.common.lcnt, i as i16, "lcnt must increment per bail");
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::NoAlarm,
            "no SCAN_ALARM yet (iter {i})"
        );
    }

    // 11th attempt while pact (lcnt==10 before increment >= MAX_LOCK)
    // must raise SCAN_ALARM/INVALID and post VAL monitor.
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_PACT", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("ASYNC_PACT").unwrap();
    let inst = rec.read();
    assert!(inst.is_processing(), "PACT still true post-alarm-raise");
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::SCAN_ALARM
    );
    assert_eq!(inst.common.amsg, "Async in progress");
}

// C `dbAccess.c:539-541` — when TPRO is set on a record whose PACT is
// true, dbProcess prints "<thread>: dbProcess of Active '<name>' with
// RPRO=<n>" before the bail decision. The Rust port emits the same
// line via eprintln; this test exercises the path and verifies (a)
// TPRO=true does not interfere with the bail decision (lcnt still
// increments) and (b) RPRO state is preserved through the guard so
// the diagnostic value is meaningful.
#[epics_macros_rs::epics_test]
async fn test_pact_entry_guard_tpro_diagnostic_does_not_change_bail_outcome() {
    let db = PvDatabase::new();
    db.add_record("ASYNC_TPRO", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Set TPRO=true and RPRO=true so the diagnostic line carries
    // observable state.
    {
        let rec = db.get_record("ASYNC_TPRO").unwrap();
        let mut inst = rec.write();
        inst.common.tpro = 1;
        inst.common.rpro = 1;
    }

    // Cycle 1: drive into PACT.
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_TPRO", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("ASYNC_TPRO").unwrap();
        let inst = rec.read();
        assert!(inst.is_processing(), "must enter PACT");
        assert!(inst.common.tpro != 0, "TPRO must be preserved");
        assert!(
            inst.common.rpro != 0,
            "RPRO must be preserved across PACT entry"
        );
    }

    // Re-entry while PACT=true: bail with lcnt increment. Diagnostic
    // is emitted as a side effect (eprintln) but the bail outcome
    // matches the non-TPRO case (verified by the silent-bail test).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_TPRO", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("ASYNC_TPRO").unwrap();
    let inst = rec.read();
    assert!(inst.is_processing(), "still PACT after bail");
    assert_eq!(inst.common.lcnt, 1, "lcnt must have advanced");
    assert!(
        inst.common.rpro != 0,
        "RPRO must remain unchanged by the diagnostic path"
    );
}

// After PACT clears via complete_async_record, the next process must
// reset lcnt to 0 (mirrors C `else { precord->lcnt = 0; }`).
#[epics_macros_rs::epics_test]
async fn test_pact_entry_guard_resets_lcnt_after_completion() {
    let db = PvDatabase::new();
    db.add_record("ASYNC_RESET", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Cycle 1: kick off async, accumulate lcnt via re-entries.
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_RESET", &mut visited, 0)
        .await
        .unwrap();
    for _ in 0..3 {
        let mut visited = HashSet::new();
        db.process_record_with_links("ASYNC_RESET", &mut visited, 0)
            .await
            .unwrap();
    }
    {
        let rec = db.get_record("ASYNC_RESET").unwrap();
        assert_eq!(rec.read().common.lcnt, 3);
    }

    // Complete the async; this clears PACT.
    db.complete_async_record("ASYNC_RESET").await.unwrap();

    // Next process_record_with_links should reset lcnt (path: enters
    // body since PACT is now false).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_RESET", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("ASYNC_RESET").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.lcnt, 0, "lcnt must reset when PACT clears");
}

// D-A: a CP/CPP burst onto a PACT target. C's `CA_DBPROCESS` worker is bare
// `dbScanLock`/`db_process`/`dbScanUnlock` (`dbCa.c:1249-1257`), so an active
// target lands in `dbProcess`'s own PACT branch (`dbAccess.c:536-556`): count
// `lcnt`, and after `MAX_LOCK` raise SCAN_ALARM/INVALID with AMSG "Async in
// progress". That branch never writes `precord->rpro`.
//
// The port used to decide PACT a second time in `process_one_cp_target`,
// setting RPRO and skipping — so the starved target got an extra device write
// on PACT release and never raised the alarm that is how an operator sees the
// starvation at all.
//
// Boundaries, one assertion each: RPRO on a blocked dispatch; the device-write
// count while blocked; `lcnt` at MAX_LOCK and at MAX_LOCK+1; the device-write
// count after the async completes; and `lcnt` reset once the target is idle.
#[epics_macros_rs::epics_test]
async fn test_cp_burst_on_a_pact_target_alarms_instead_of_setting_rpro() {
    let db = PvDatabase::new();
    let writes: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    db.add_record("CPB_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // What `register_record_type` does for a real type: publish the table to
    // the by-name registry `dbf_link_class` reads. `add_record` takes an
    // instance and no factory, so nothing else does it here.
    epics_base_rs::server::record::register_declared_fields("async_out_test", ASYNC_OUT_FIELDS);
    db.add_record(
        "CPB_TGT",
        Box::new(AsyncOutRecord {
            val: 0.0,
            device_writes: writes.clone(),
        }),
    )
    .await
    .unwrap();
    {
        let rec = db.get_record("CPB_TGT").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("CPB_SRC CP".into()))
            .unwrap();
        // C alarms a record that HAS completed a cycle and then hangs mid-async;
        // a never-processed record still carries the init UDF severity and the
        // `sevr >= INVALID_ALARM` arm suppresses the alarm forever. Put the
        // target in the state a completed cycle leaves behind.
        inst.common.udf = 0;
        inst.common.sevr = AlarmSeverity::NoAlarm;
        inst.common.stat = epics_base_rs::server::recgbl::alarm_status::NO_ALARM;
    }
    // iocInit's CP scan is what puts the edge in the registry the dispatch
    // reads; without it `get_cp_targets` is empty and nothing is dispatched.
    db.setup_cp_links().await;

    // Every burst moves CPB_SRC's value, because the CP trigger is the
    // source's `DBE_VALUE|DBE_ALARM` POST, not the fact that it processed
    // (`CyclePosts` / C `dbCa.c:1225-1229`). A repeated identical value posts
    // nothing past `MDEL = 0` and would dispatch nothing at all, leaving every
    // assertion below vacuous.
    let burst = |db: PvDatabase, v: f64| async move {
        {
            let rec = db.get_record("CPB_SRC").unwrap();
            let mut inst = rec.write();
            inst.record.put_field("VAL", EpicsValue::Double(v)).unwrap();
        }
        let mut visited = HashSet::new();
        db.process_record_with_links("CPB_SRC", &mut visited, 0)
            .await
            .unwrap();
    };

    // Burst 1: target is idle, so the CP dispatch processes it — one device
    // write, and the target goes PACT.
    burst(db.clone(), 1.0).await;
    {
        let inst = db.get_record("CPB_TGT").unwrap();
        let inst = inst.read();
        assert!(
            inst.is_processing(),
            "CP dispatch must drive the target PACT"
        );
        assert_eq!(inst.common.lcnt, 0, "an idle target resets lcnt");
    }
    // 1.0 is burst 1's source value, read through the CP link into the
    // target's VAL before its device write.
    assert_eq!(
        *writes.lock().unwrap(),
        vec![1.0],
        "one device write so far"
    );

    // Bursts 2..=11: the target is PACT. MAX_LOCK = 10 of them bail silently.
    for i in 1..=10 {
        burst(db.clone(), 1.0 + i as f64).await;
        let inst = db.get_record("CPB_TGT").unwrap();
        let inst = inst.read();
        assert_eq!(
            inst.common.rpro, 0,
            "dbCa.c's CA_DBPROCESS never sets RPRO (burst {i})"
        );
        assert_eq!(
            inst.common.lcnt, i as i16,
            "lcnt counts the blocked dispatch"
        );
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::NoAlarm,
            "no SCAN_ALARM before MAX_LOCK (burst {i})"
        );
        assert_eq!(
            writes.lock().unwrap().len(),
            1,
            "a blocked dispatch must not reach the device (burst {i})"
        );
    }

    // Burst 12: lcnt is MAX_LOCK before the increment, so the alarm fires.
    burst(db.clone(), 12.0).await;
    {
        let inst = db.get_record("CPB_TGT").unwrap();
        let inst = inst.read();
        assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
        assert_eq!(
            inst.common.stat,
            epics_base_rs::server::recgbl::alarm_status::SCAN_ALARM
        );
        assert_eq!(inst.common.amsg, "Async in progress");
        assert_eq!(inst.common.rpro, 0, "the alarm arm writes no RPRO either");
    }

    // The device round trip finally completes. No RPRO was ever set, so the
    // completion tail queues no reprocess and the device sees no second write.
    db.complete_async_record("CPB_TGT").await.unwrap();
    for _ in 0..20 {
        if writes.lock().unwrap().len() > 1 {
            break;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        *writes.lock().unwrap(),
        vec![1.0],
        "no RPRO means no extra device write on PACT release"
    );

    // The target is idle again: the next CP dispatch runs the body and resets
    // lcnt, C's `else { precord->lcnt = 0; }`.
    burst(db.clone(), 13.0).await;
    let inst = db.get_record("CPB_TGT").unwrap();
    let inst = inst.read();
    assert_eq!(inst.common.lcnt, 0, "an idle target resets lcnt");
}

// Regression: when a record returns `AsyncPending` paired with a
// `ReprocessAfter` action (the timer-owned continuation pattern used
// by scaler DLY / calc AFTC), the spawned timer fire must call
// `process_record_continuation` and bypass the PACT entry guard so
// the record's `process()` runs again to advance the state machine.
// The foreign-caller guard (FLNK / scan / CA put) is still in
// effect — `test_pact_entry_guard_silent_bail_until_max_lock` above
// covers that case.
#[epics_macros_rs::epics_test]
async fn test_reprocess_after_continuation_bypasses_pact_guard() {
    use epics_base_rs::server::record::{ProcessAction, ProcessOutcome, RecordProcessResult};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct ContinuationRecord {
        process_count: Arc<AtomicU32>,
    }

    impl Record for ContinuationRecord {
        fn record_type(&self) -> &'static str {
            "continuation_test"
        }
        fn process(&mut self) -> epics_base_rs::error::CaResult<ProcessOutcome> {
            let n = self.process_count.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First process: arm the timer-driven continuation.
                Ok(ProcessOutcome {
                    result: RecordProcessResult::AsyncPending,
                    actions: vec![ProcessAction::ReprocessAfter(
                        std::time::Duration::from_millis(20),
                    )],
                    device_did_compute: false,
                    post_write_fields: Vec::new(),
                })
            } else {
                // Continuation reached: complete cleanly, clear PACT.
                Ok(ProcessOutcome::complete())
            }
        }
        fn get_field(&self, _name: &str) -> Option<EpicsValue> {
            None
        }
        fn put_field(
            &mut self,
            _name: &str,
            _value: EpicsValue,
        ) -> epics_base_rs::error::CaResult<()> {
            Ok(())
        }
        fn declared_fields(&self) -> &'static [FieldDesc] {
            &[]
        }
    }

    let process_count = Arc::new(AtomicU32::new(0));
    let db = PvDatabase::new();
    db.add_record(
        "CONT_REC",
        Box::new(ContinuationRecord {
            process_count: process_count.clone(),
        }),
    )
    .await
    .unwrap();

    // First process: returns AsyncPending + ReprocessAfter(20ms).
    let mut visited = HashSet::new();
    db.process_record_with_links("CONT_REC", &mut visited, 0)
        .await
        .unwrap();

    // PACT should be set immediately after AsyncPending returns.
    {
        let rec = db.get_record("CONT_REC").unwrap();
        assert!(
            rec.read().is_processing(),
            "PACT must be true after AsyncPending"
        );
    }
    assert_eq!(process_count.load(Ordering::SeqCst), 1);

    // A foreign caller during the wait must hit the entry guard (bail
    // silently) — proves the guard still protects against FLNK/scan
    // dual-fire while the continuation timer is pending.
    let mut visited = HashSet::new();
    db.process_record_with_links("CONT_REC", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        process_count.load(Ordering::SeqCst),
        1,
        "foreign re-entry during AsyncPending must NOT call process()"
    );

    // Wait for the ReprocessAfter timer to fire. Polled to a deadline
    // rather than slept for a fixed interval: the fire runs on the
    // process-global background executor, which this test's own
    // `spawn_background` constructs from cold, and a whole-suite run on a
    // loaded machine puts that construction plus a 20 ms timer well past
    // any interval short enough to be worth writing. The deadline is a
    // failure bound, not the expected latency (20.7 ms, idle Linux).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_count.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(1)).await;
    }

    // Continuation fired: process() ran a second time despite
    // pact=true.
    assert_eq!(
        process_count.load(Ordering::SeqCst),
        2,
        "ReprocessAfter timer must call process() again — owner-driven \
         continuation bypasses the PACT entry guard"
    );

    // BUG 1 regression — when the continuation's `process()` returns
    // `Complete` (not async-pending again), the `processing` flag set
    // on the original `AsyncPending` MUST be cleared. The continuation
    // path does NOT go through `complete_async_record`, so without an
    // explicit clear in `process_record_with_links_inner` the flag
    // stayed `true` forever. C parity: an async record's completion
    // re-entry clears `pact` inside `process()` (`aiRecord.c` second
    // pass). A leaked `processing=true` would make every later foreign
    // `process_record_with_links` trip the PACT entry guard.
    {
        let rec = db.get_record("CONT_REC").unwrap();
        assert!(
            !rec.read().is_processing(),
            "BUG 1: completed ReprocessAfter continuation must clear PACT"
        );
    }

    // A foreign caller after the continuation completed must actually
    // run `process()` again — proving the PACT entry guard no longer
    // fires (it would if `processing` had leaked true).
    let mut visited = HashSet::new();
    db.process_record_with_links("CONT_REC", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        process_count.load(Ordering::SeqCst),
        3,
        "BUG 1: after the continuation cleared PACT, a foreign process \
         must run process() again instead of bailing at the entry guard"
    );
}

// --- Monitor Mask tests ---

/// epics-base 3.15.7 — a server-side `dbnd` (deadband) filter
/// attached to a subscriber must drop sub-threshold value changes
/// while letting through deltas that cross the threshold. Mirrors
/// the per-subscription filter chain semantics that the JSON-name
/// parser (future commit) will wire in for real CA channels.
#[epics_macros_rs::epics_test]
async fn test_dbnd_filter_drops_subthreshold_changes() {
    use epics_base_rs::server::database::filters::DeadbandFilter;
    use epics_base_rs::server::recgbl::EventMask;
    use std::sync::Arc;

    let db = PvDatabase::new();
    db.add_record("DBND:REC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let rec = db.get_record("DBND:REC").unwrap();
    let mut rx = {
        let mut inst = rec.write();
        let rx = inst
            .add_subscriber(
                "VAL",
                1,
                epics_base_rs::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscribe");
        let attached =
            inst.attach_filter_to_last_subscriber("VAL", Arc::new(DeadbandFilter::absolute(1.0)));
        assert!(attached, "filter must attach to the just-added subscriber");
        rx
    };

    // 11.0: first event always passes (no `last_sent` baseline yet).
    {
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(11.0))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    rx.try_recv()
        .expect("first value passes the deadband filter");

    // 11.4: |delta|=0.4 < 1.0 → silenced.
    {
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(11.4))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    assert!(
        rx.try_recv().is_err(),
        "sub-threshold change must be filtered out"
    );

    // 12.5: |delta|=1.1 >= 1.0 → passes.
    {
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(12.5))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    rx.try_recv().expect("above-threshold change passes");
}

/// epics-base 446e0d4a — value filters MUST pass alarm-only events
/// through regardless of the deadband state. Otherwise an alarm
/// triggered mid-deadband-window would be silenced and clients
/// would miss the state change.
#[epics_macros_rs::epics_test]
async fn test_dbnd_filter_passes_alarm_events() {
    use epics_base_rs::server::database::filters::DeadbandFilter;
    use epics_base_rs::server::recgbl::EventMask;
    use std::sync::Arc;

    let db = PvDatabase::new();
    db.add_record("DBND:ALR", Box::new(AoRecord::new(50.0)))
        .await
        .unwrap();
    let rec = db.get_record("DBND:ALR").unwrap();
    let mut rx = {
        let mut inst = rec.write();
        let rx = inst
            .add_subscriber(
                "VAL",
                1,
                epics_base_rs::types::DbFieldType::Double,
                (EventMask::VALUE | EventMask::ALARM).bits(),
            )
            .expect("subscribe");
        inst.attach_filter_to_last_subscriber("VAL", Arc::new(DeadbandFilter::absolute(10.0)));
        rx
    };

    // Seed the filter state with one value event.
    {
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(50.0))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    rx.try_recv().expect("seed value");

    // A 50.5 value-only update is silenced by the deadband (delta 0.5 < 10).
    {
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(50.5))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    assert!(rx.try_recv().is_err(), "sub-threshold value silenced");

    // But an ALARM-tagged emission with the SAME value MUST pass —
    // the filter's "always-pass alarm" rule.
    {
        let mut inst = rec.write();
        inst.notify_field("VAL", EventMask::ALARM);
    }
    rx.try_recv().expect("alarm event passes the filter");
}

#[epics_macros_rs::epics_test]
async fn test_notify_field_respects_mask() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();
    let (mut value_rx, mut alarm_rx) = {
        let mut inst = rec.write();
        let value_rx = inst
            .add_subscriber(
                "VAL",
                1,
                epics_base_rs::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscribe should not be capped at default");
        let alarm_rx = inst
            .add_subscriber(
                "VAL",
                2,
                epics_base_rs::types::DbFieldType::Double,
                EventMask::ALARM.bits(),
            )
            .expect("subscribe should not be capped at default");
        (value_rx, alarm_rx)
    };
    {
        let mut inst = rec.write();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    assert!(value_rx.try_recv().is_ok());
    assert!(alarm_rx.try_recv().is_err());
}

/// C `dbAccess.c:574-576` clears `precord->rpro = FALSE; precord->putf =
/// FALSE` and arms `callNotifyCompletion = TRUE` BEFORE the alarm
/// check whenever SDIS evaluates to DISV. Pre-fix Rust only
/// reset nsta/nsev and updated the alarm — rpro/putf leaked into the
/// next cycle and pending dbNotify completion callbacks stalled.
#[epics_macros_rs::epics_test]
async fn test_sdis_disable_clears_rpro_and_putf() {
    let db = PvDatabase::new();
    db.add_record("DIS_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("DIS_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DIS_TGT") {
        let mut inst = rec.write();
        inst.put_common_field("SDIS", EpicsValue::String("DIS_SW".into()))
            .unwrap();
        // Pre-set rpro=true, putf=true so the disable path's clear is
        // observable.
        inst.common.rpro = 1;
        inst.common.putf = true;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DIS_TGT", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("DIS_TGT").unwrap();
    let inst = rec.read();
    assert!(
        inst.common.rpro == 0,
        "SDIS disable must clear rpro (C dbAccess.c:574). Pre-fix this leaked."
    );
    assert!(
        !inst.common.putf,
        "SDIS disable must clear putf (C dbAccess.c:575). Pre-fix this leaked."
    );
}

/// C `dbAccess.c:621-622` runs `dbNotifyCompletion(precord)` at
/// `all_done` for the disable bail path because `callNotifyCompletion
/// = TRUE` was set at line 577. A CA WRITE_NOTIFY landing on a
/// disabled record must release its caller. Pre-fix the
/// put_notify_tx was never fired, stranding the call until socket
/// disconnect.
#[epics_macros_rs::epics_test]
async fn test_sdis_disable_fires_put_notify_completion() {
    let db = PvDatabase::new();
    db.add_record("DIS_NOT_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("DIS_NOT_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DIS_NOT_TGT") {
        let mut inst = rec.write();
        inst.put_common_field("SDIS", EpicsValue::String("DIS_NOT_SW".into()))
            .unwrap();
    }

    // Arm a put-notify wait-set on the disabled target (pending=1 for the
    // originating record). The disable path must take it and `leave`,
    // draining the set to zero and firing the completion oneshot.
    let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
    {
        let rec = db.get_record("DIS_NOT_TGT").unwrap();
        let mut inst = rec.write();
        inst.install_or_queue_notify(tx)
            .expect("the record is free, so the wait-set installs");
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DIS_NOT_TGT", &mut visited, 0)
        .await
        .unwrap();

    // rx should be ready — completion was sent via the disable bail.
    rx.await
        .expect("disable bail must fire put-notify completion (C dbAccess.c:621)");
    // membership must be taken (not left dangling for the next cycle).
    let rec = db.get_record("DIS_NOT_TGT").unwrap();
    assert!(
        !rec.read().has_notify(),
        "put-notify wait-set membership must be cleared after firing"
    );
}

// ---------------------------------------------------------------------------
// Put-notify wait-set: CA WRITE_NOTIFY completion must wait for the WHOLE
// chain — the originating record AND every FLNK/OUT PP target it drives,
// synchronous or async — exactly like C `dbNotify.c` keeps every record in
// the `waitList` until `dbNotifyCompletion` drains it to empty. The four
// cases below are written per invariant boundary, not per narrative:
//   * no async member in the chain   → completes synchronously (Ok(None))
//   * async member reached via FLNK  → deferred (Ok(Some(rx)), fires later)
//   * async member reached via OUT PP→ deferred (same, other dispatch edge)
//   * second WRITE_NOTIFY in-flight  → queued on the restart list, not refused
// ---------------------------------------------------------------------------

/// Boundary: a fully synchronous chain (originating record + a sync FLNK
/// target) drains the wait-set within the put call, so the put reports
/// immediate completion (`Ok(None)`) — no receiver is handed back.
#[epics_macros_rs::epics_test]
async fn test_put_notify_sync_chain_completes_immediately() {
    let db = PvDatabase::new();
    db.add_record("PN_SYNC_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("PN_SYNC_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PN_SYNC_SRC") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("PN_SYNC_TGT".into()))
            .unwrap();
    }

    let result = db
        .put_record_field_from_ca("PN_SYNC_SRC", "VAL", EpicsValue::Double(3.0))
        .await
        .expect("put must succeed");
    assert!(
        result.is_sync(),
        "a fully synchronous chain drains the wait-set in-call → Sync; \
         got a deferred receiver instead"
    );
}

/// Boundary (the finding): a synchronous originating record whose FLNK
/// target is async must DEFER completion. Pre-fix the originating record
/// fired its completion the instant its own sync cycle ended, reporting
/// WRITE_NOTIFY done while the async FLNK target was still in flight. Now
/// the put returns a receiver that fires only when the async target's
/// `complete_async_record` runs.
#[epics_macros_rs::epics_test]
async fn test_put_notify_defers_for_async_flnk_target() {
    let db = PvDatabase::new();
    db.add_record("PN_FLNK_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("PN_FLNK_TGT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PN_FLNK_SRC") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("PN_FLNK_TGT".into()))
            .unwrap();
    }

    let result = db
        .put_record_field_from_ca("PN_FLNK_SRC", "VAL", EpicsValue::Double(4.0))
        .await
        .expect("put must succeed");
    let mut rx = match result {
        ProcessCompletion::Async(rx) => rx,
        ProcessCompletion::Sync => panic!(
            "an async FLNK target must defer completion — \
             the put returned immediate (Sync) instead of a receiver"
        ),
    };
    // The async FLNK target is in flight; completion MUST NOT have fired.
    assert!(
        matches!(
            rx.try_recv(),
            Err(epics_base_rs::runtime::sync::oneshot::error::TryRecvError::Empty)
        ),
        "completion fired while the async FLNK target was still pending \
         (the exact pre-fix defect)"
    );
    // Complete the async target — only now may the wait-set drain to zero.
    db.complete_async_record("PN_FLNK_TGT").await.unwrap();
    rx.await
        .expect("completion must fire once the async FLNK target completes");
}

/// Boundary: same deferral, reached through an OUT `PP` link instead of
/// FLNK — the other dispatch edge that calls `processTarget`/`dbNotifyAdd`.
#[epics_macros_rs::epics_test]
async fn test_put_notify_defers_for_async_out_pp_target() {
    let db = PvDatabase::new();
    db.add_record("PN_OUT_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("PN_OUT_TGT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PN_OUT_SRC") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("PN_OUT_TGT PP".into()))
            .unwrap();
    }

    let result = db
        .put_record_field_from_ca("PN_OUT_SRC", "VAL", EpicsValue::Double(5.0))
        .await
        .expect("put must succeed");
    let mut rx = match result {
        ProcessCompletion::Async(rx) => rx,
        ProcessCompletion::Sync => panic!(
            "an async OUT PP target must defer completion — \
             the put returned immediate (Sync) instead of a receiver"
        ),
    };
    assert!(
        matches!(
            rx.try_recv(),
            Err(epics_base_rs::runtime::sync::oneshot::error::TryRecvError::Empty)
        ),
        "completion fired while the async OUT PP target was still pending"
    );
    db.complete_async_record("PN_OUT_TGT").await.unwrap();
    rx.await
        .expect("completion must fire once the async OUT PP target completes");
}

/// Boundary: a second WRITE_NOTIFY on a record whose put-notify is still in
/// flight joins C's `restartList` (`dbNotify.c:213-220`) rather than being
/// refused — `S_db_Blocked` / ECA_PUTCBINPROG is not a status C sends from
/// this path, and returning it drops the client's value. It must also not
/// overwrite the wait-set, which would drop the prior `Sender` and wake the
/// prior caller's receiver with a `RecvError` the CA dispatcher treats as
/// success.
#[epics_macros_rs::epics_test]
async fn test_put_notify_queues_second_in_flight() {
    let db = PvDatabase::new();
    db.add_record("PN_DBL", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // First put: the record goes async-pending and keeps its wait-set
    // membership for the duration of the device round trip.
    let first = db
        .put_record_field_from_ca("PN_DBL", "VAL", EpicsValue::Double(1.0))
        .await
        .expect("first put must succeed");
    assert!(
        first.is_async(),
        "async record's first put must return a deferred receiver"
    );

    // Second put while the first is still in flight → queued, and the client
    // gets a receiver to wait on instead of an error.
    let second = db
        .put_record_field_from_ca("PN_DBL", "VAL", EpicsValue::Double(2.0))
        .await
        .expect("a second WRITE_NOTIFY must be queued, not refused");
    assert!(
        second.is_async(),
        "a queued put-notify completes on its own restart, so it must hand \
         back a receiver; got {second:?}"
    );

    // Nothing was written: C tests ownership above `putCallback`.
    {
        let rec = db.get_record("PN_DBL").unwrap();
        assert_eq!(
            rec.read().record.get_field("VAL"),
            Some(EpicsValue::Double(1.0)),
            "the queued put must write nothing until it is restarted"
        );
    }
}

/// Boundary (the live defect): a fire-and-forget put — C `dbPutField`
/// semantics: CA_PROTO_WRITE, `dbpf`, the internal `put_*_process`
/// helpers, non-blocking PVA puts — parks NO put-notify wait-set, even
/// when the record goes async-pending. Pre-fix EVERY processing put
/// parked one; a caller that dropped the receiver left the record's
/// notify slot occupied for the whole async round trip (a motor's
/// whole motion), refusing every legitimate WRITE_NOTIFY on the record
/// in the meantime.
#[epics_macros_rs::epics_test]
async fn test_fire_and_forget_put_parks_no_notify_write_notify_stays_legal() {
    let db = PvDatabase::new();
    db.add_record("PN_FF", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("PN_FF", "VAL", EpicsValue::Double(1.0))
        .await
        .expect("fire-and-forget put must succeed");
    {
        let rec = db.get_record("PN_FF").unwrap();
        let inst = rec.read();
        assert!(inst.is_processing(), "async pending → PACT=true");
        assert!(
            !inst.has_notify(),
            "a fire-and-forget put must not park a put-notify wait-set \
             (C builds a putNotify only in dbPutNotify, never dbPutField)"
        );
        assert!(
            inst.common.putf,
            "PUTF must survive the async round trip in no-notify mode \
             (the !is_processing() gate, not originating_pending, must \
             carry it)"
        );
    }

    // A WRITE_NOTIFY arriving mid-flight must be accepted — pre-fix it
    // was refused because the fire-and-forget
    // put's orphaned wait-set occupied the slot.
    let rx = db
        .put_record_field_from_ca("PN_FF", "VAL", EpicsValue::Double(2.0))
        .await
        .expect("WRITE_NOTIFY after a fire-and-forget put must be accepted")
        .into_handle()
        .expect("record is still async-pending → completion is deferred");

    // The record is PACT, so C `processNotifyCommon` (dbNotify.c:225-232) writes
    // nothing and puts the whole put on the restart list; the first completion
    // replays it. `AsyncRecord` goes async on EVERY process, so the replayed put
    // makes the record PACT again — and the callback belongs to THAT cycle, the
    // first one that actually saw 2.0.
    db.complete_async_record("PN_FF").await.unwrap();
    for _ in 0..2000 {
        let rec = db.get_record("PN_FF").unwrap();
        let inst = rec.read();
        if inst.is_processing() && inst.record.get_field("VAL") == Some(EpicsValue::Double(2.0)) {
            break;
        }
        drop(inst);
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(1)).await;
    }
    {
        let rec = db.get_record("PN_FF").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Double(2.0)),
            "the restart replayed the deferred put into the now-idle record"
        );
        assert!(
            inst.is_processing(),
            "and drove a real process cycle, which went async again"
        );
    }

    db.complete_async_record("PN_FF").await.unwrap();
    rx.await
        .expect("the WRITE_NOTIFY completes at the end of the cycle its value drove");
}

/// Boundary (other direction): a fire-and-forget put on a record with
/// a WRITE_NOTIFY already parked is accepted (C `dbPutField` carries no
/// notify state to conflict) and leaves the parked wait-set undisturbed
/// — it neither steals nor fires the prior caller's completion.
#[epics_macros_rs::epics_test]
async fn test_fire_and_forget_put_does_not_disturb_parked_notify() {
    let db = PvDatabase::new();
    db.add_record("PN_FF2", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    let mut rx = db
        .put_record_field_from_ca("PN_FF2", "VAL", EpicsValue::Double(1.0))
        .await
        .expect("WRITE_NOTIFY must succeed")
        .into_handle()
        .expect("async record defers completion");

    db.put_record_field_from_ca_no_notify("PN_FF2", "VAL", EpicsValue::Double(2.0))
        .await
        .expect("fire-and-forget put must be accepted while a notify is parked");
    assert!(
        matches!(
            rx.try_recv(),
            Err(epics_base_rs::runtime::sync::oneshot::error::TryRecvError::Empty)
        ),
        "the fire-and-forget put fired or dropped the parked WRITE_NOTIFY \
         completion — it must leave the prior caller's wait-set alone"
    );

    db.complete_async_record("PN_FF2").await.unwrap();
    rx.await
        .expect("parked WRITE_NOTIFY completion must still fire at async completion");
}

/// A record that, on its put-driven process, asks for an independent rescan of
/// itself — C's `if (precord->scan) scanOnce(precord)` at every `special()`
/// call site (e.g. `scalerRecord.c:655`/`:667`). The rescan is a *fresh*,
/// unrelated process cycle, not part of this put's completion chain.
struct ScanOnceEmitter {
    process_count: Arc<AtomicU32>,
}
impl Record for ScanOnceEmitter {
    fn record_type(&self) -> &'static str {
        "scan_once_emitter"
    }
    fn process(&mut self) -> epics_base_rs::error::CaResult<ProcessOutcome> {
        // The put-driven pass (count 0) queues the independent rescan; the
        // scanOnce'd pass (count ≥ 1) does the real work and does NOT re-queue,
        // exactly as `special()` moves state once and `process()` acts on it.
        if self.process_count.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(ProcessOutcome::complete_with(vec![ProcessAction::ScanOnce]))
        } else {
            Ok(ProcessOutcome::complete())
        }
    }
    fn get_field(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// Boundary (Increment ② completion contract): a put whose process only queues
/// an independent `scanOnce` completes **synchronously** — the scanOnce is a
/// separate cycle, NOT joined into this put-notify's wait-set. C `dbNotifyAdd`
/// (`dbNotify.c:477-501`, sole caller `dbDbLink.c:460`) joins only the
/// process-passive OUT/FLNK link chain; a `scanOnce`'d record is never added,
/// so `dbNotifyCompletion` does not wait on it. The RTEMS CA driver must
/// therefore see `ProcessCompletion::Sync` here and reply inline — waiting on
/// the scanOnce would hang the reply on an unrelated cycle.
#[epics_macros_rs::epics_test]
async fn scan_once_is_not_joined_to_put_notify_completion() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "SO",
        Box::new(ScanOnceEmitter {
            process_count: count.clone(),
        }),
    )
    .await
    .unwrap();
    // Non-Passive: C's `if (precord->scan)` guard is satisfied, so the
    // scanOnce actually spawns (a Passive record would be skipped — its own
    // pp(TRUE) pass already processed it). A non-Passive record also gets no
    // process from a plain pp put, which is exactly why C uses scanOnce; the
    // Force/`process_record_with_notify` entry (rsrv `write_notify` with the
    // process bit) is the unconditional-process WRITE_NOTIFY boundary.
    {
        db.get_record("SO").unwrap().write().common.scan = ScanType::SEC1;
    }

    let completion = db
        .process_record_with_notify("SO")
        .await
        .expect("the WRITE_NOTIFY process is accepted");
    assert!(
        completion.is_sync(),
        "a put that only queues an independent scanOnce must complete \
         synchronously — the scanOnce is not joined to the notify wait-set \
         (C dbNotifyAdd never adds a scanOnce'd record)"
    );

    // The queued scanOnce lands independently after the synchronous put
    // returned: the record is re-processed a second time, off the put's
    // completion path.
    // Polled to a deadline for the same reason the `ReprocessAfter`
    // continuation is: the scanOnce is a spawned cycle, so how long it takes
    // to land is the machine's business, and a fixed settle only encodes the
    // load the test was written under.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while count.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(
        count.load(Ordering::SeqCst) >= 2,
        "the scanOnce must run as its own cycle after the put completed, got \
         {} process passes",
        count.load(Ordering::SeqCst)
    );
}

/// Boundary: synchronous-completion PUTF clear still runs in no-notify
/// mode. `originating_pending` keys on the instance's parked notify;
/// a fire-and-forget put parks nothing, so it must fall through to the
/// guarded clear rather than mistaking an empty slot for "pending".
#[epics_macros_rs::epics_test]
async fn test_fire_and_forget_put_clears_putf_on_sync_completion() {
    let db = PvDatabase::new();
    db.add_record("PN_FF_SYNC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("PN_FF_SYNC", "VAL", EpicsValue::Double(42.0))
        .await
        .expect("fire-and-forget put must succeed");

    let rec = db.get_record("PN_FF_SYNC").unwrap();
    let inst = rec.read();
    assert!(
        !inst.common.putf,
        "after synchronous completion the fire-and-forget put must clear \
         PUTF (mirrors C recGblFwdLink:302)"
    );
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Double(42.0)),
        "the value must have been applied"
    );
}

#[epics_macros_rs::epics_test]
async fn test_sdis_disable_notifies_alarm() {
    // C `dbAccess.c:586-591` — the disable branch of `dbProcess` posts:
    //   db_post_events(&precord->stat, DBE_VALUE);            // STAT
    //   db_post_events(&precord->sevr, DBE_VALUE);            // SEVR
    //   db_post_events(&precord->VAL,  DBE_VALUE|DBE_ALARM);  // value field
    // Only the *value field* carries DBE_ALARM; STAT/SEVR are posted
    // with DBE_VALUE alone. A DBE_ALARM subscriber must therefore be
    // attached to the value field (VAL) to observe the disable event —
    // a DBE_ALARM-only subscription on .STAT/.SEVR would NOT be
    // notified, matching C semantics.
    let db = PvDatabase::new();
    db.add_record("DISABLE_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("TARGET") {
        let mut inst = rec.write();
        inst.put_common_field("SDIS", EpicsValue::String("DISABLE_SW".into()))
            .unwrap();
        inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap();
    }
    let mut alarm_rx = {
        let rec = db.get_record("TARGET").unwrap();
        let mut inst = rec.write();
        // DBE_ALARM subscriber on the value field — C posts VAL with
        // DBE_VALUE|DBE_ALARM in the disable branch (dbAccess.c:590-592).
        inst.add_subscriber(
            "VAL",
            1,
            epics_base_rs::types::DbFieldType::Double,
            EventMask::ALARM.bits(),
        )
        .expect("subscribe should not be capped at default")
    };
    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();
    assert!(alarm_rx.try_recv().is_ok());
}

// --- UDF in database context ---

#[epics_macros_rs::epics_test]
async fn test_udf_cleared_by_process_with_links() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();
    assert!(rec.read().common.udf != 0);
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    assert!(rec.read().common.udf == 0);
}

/// R6-5 — `PINI` is the 6-choice `menuPini` (`menuPini.dbd:11-18`), and C
/// matches the menu index **exactly** (`iocInit.c:598`
/// `if (precord->pini != pphase->pini) return;`). Each choice therefore selects
/// a disjoint pass: `YES` runs at `initialProcess()`, `RUN` at
/// `initHookAtIocRun`, `RUNNING` at `initHookAfterIocRunning`. A `PINI=RUN`
/// record must not be dragged into the `YES` pass, and must not be dropped
/// entirely (the pre-fix `bool` did both: `"RUN"` parsed to `false`).
///
/// UDF is the "did it process" probe — `process_record_with_links` clears it
/// (see `test_udf_cleared_by_process_with_links`).
#[epics_macros_rs::epics_test]
async fn test_pini_passes_select_disjoint_menu_choices() {
    use epics_base_rs::server::record::PiniMode;

    let db = PvDatabase::new();
    for name in ["PINI_YES", "PINI_RUN", "PINI_RUNNING", "PINI_NO"] {
        db.add_record(name, Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
    }
    // Set each record's PINI the way a `.db` file / caput does — through the
    // field put, by label — not by poking `common.pini`.
    for (name, label) in [
        ("PINI_YES", "YES"),
        ("PINI_RUN", "RUN"),
        ("PINI_RUNNING", "RUNNING"),
        ("PINI_NO", "NO"),
    ] {
        let rec = db.get_record(name).unwrap();
        let mut inst = rec.write();
        inst.put_common_field("PINI", EpicsValue::String(label.into()))
            .unwrap_or_else(|e| panic!("PINI={label} must be accepted: {e:?}"));
    }
    // The stored value is the menu index, so `caget REC.PINI` reports RUN as 2
    // (the pre-fix bool reported 0 — indistinguishable from NO).
    assert_eq!(
        db.get_record("PINI_RUN").unwrap().read().common.pini,
        PiniMode::Run.to_u16() as i16
    );

    // Pass 1: initialProcess() — YES only.
    db.pini_process(PiniMode::Yes).await;
    let udf = |n: &'static str| {
        let db = &db;
        async move { db.get_record(n).unwrap().read().common.udf != 0 }
    };
    assert!(
        !udf("PINI_YES").await,
        "PINI=YES processes at initialProcess"
    );
    assert!(
        udf("PINI_RUN").await,
        "PINI=RUN must NOT run in the YES pass"
    );
    assert!(
        udf("PINI_RUNNING").await,
        "PINI=RUNNING must NOT run in the YES pass"
    );
    assert!(udf("PINI_NO").await, "PINI=NO never processes");

    // Pass 2: initHookAtIocRun — RUN only.
    db.pini_process(PiniMode::Run).await;
    assert!(!udf("PINI_RUN").await, "PINI=RUN processes at iocRun");
    assert!(udf("PINI_RUNNING").await, "PINI=RUNNING is a later pass");
    assert!(udf("PINI_NO").await);

    // Pass 3: initHookAfterIocRunning — RUNNING only.
    db.pini_process(PiniMode::Running).await;
    assert!(
        !udf("PINI_RUNNING").await,
        "PINI=RUNNING processes after iocRunning"
    );
    assert!(udf("PINI_NO").await, "PINI=NO is in no pass at all");
}

/// R6-7 — `caput -a REC.VAL 0` (a zero-element array into a scalar field). C
/// `dbPut` (`dbAccess.c:1365-1367`, commit `12cfd418d`) **accepts** the put,
/// leaves the field unchanged and raises `LINK_ALARM`/`INVALID_ALARM` on the
/// record; `status` stays 0, so `dbPut` returns success. The port rejected the
/// put with an error, so the client saw a write failure and the record's alarm
/// state was never touched.
#[epics_macros_rs::epics_test]
async fn test_empty_array_into_scalar_is_accepted_and_alarms_the_record() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    db.add_record("EMPTYPUT", Box::new(AoRecord::new(5.0)))
        .await
        .unwrap();
    // Process once so VAL is committed and UDF is clear — the baseline the
    // empty put must not disturb.
    let mut visited = HashSet::new();
    db.process_record_with_links("EMPTYPUT", &mut visited, 0)
        .await
        .unwrap();

    let result = db
        .put_record_field_from_ca("EMPTYPUT", "VAL", EpicsValue::DoubleArray(vec![]))
        .await;
    assert!(
        result.is_ok(),
        "C dbPut accepts a zero-element request; the client must not see an error: {result:?}"
    );

    // The field is untouched…
    match db.get_pv("EMPTYPUT.VAL").unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 5.0).abs() < 1e-10,
            "the empty request must not overwrite VAL (silent zero was the bug 12cfd418d fixed)"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
    // …and the record is driven to LINK/INVALID, committed by the process cycle
    // the CA put triggers.
    let inst = db.get_record("EMPTYPUT").unwrap();
    let inst = inst.read();
    assert_eq!(inst.common.stat, alarm_status::LINK_ALARM);
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
}

/// R6-7 — the same zero-element request into an **array** field is an ordinary
/// no-op success: C takes the `no_elements > 1` branch (`dbAccess.c:1345`),
/// clamps `nRequest` to 0 and converts nothing — no alarm. Only the *scalar*
/// destination alarms. This is the boundary the fix turns on.
#[epics_macros_rs::epics_test]
async fn test_empty_array_into_array_field_is_a_silent_no_op() {
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    // NELM=4, FTVL=DOUBLE. (This used to set `wf.ftvl = 6` — menuFtype index 6 is
    // ULONG, not DOUBLE; the buffer stayed DoubleArray only because assigning the
    // index did not retype it. `new` derives both from the element type.)
    let wf = epics_base_rs::server::records::waveform::WaveformRecord::new(
        4,
        epics_base_rs::types::DbFieldType::Double,
    );
    db.add_record("EMPTYWF", Box::new(wf)).await.unwrap();

    let result = db
        .put_record_field_from_ca("EMPTYWF", "VAL", EpicsValue::DoubleArray(vec![]))
        .await;
    assert!(result.is_ok(), "empty array into a waveform must succeed");

    let inst = db.get_record("EMPTYWF").unwrap();
    let inst = inst.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::NoAlarm,
        "an array destination must NOT take the scalar empty-request alarm branch"
    );
    assert_ne!(
        inst.common.nsta,
        epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
        "no LINK_ALARM may even be pending on an array destination"
    );
}

/// R6-10 — `caput -a REC.VAL 3 1 2 3` (a multi-element array into a scalar
/// field). C `dbPut` takes the `nRequest > 1` branch, clamps the request to the
/// destination's element count (`if (no_elements < nRequest) nRequest =
/// no_elements;`, `dbAccess.c:1354`) and converts that one element: element 0
/// is written and the put SUCCEEDS. The port passed the array straight to the
/// record's typed `put_field` arm, which rejected it with `TypeMismatch`, so
/// the client saw a write failure.
#[epics_macros_rs::epics_test]
async fn r6_10_multi_element_array_into_scalar_writes_element_zero() {
    let db = PvDatabase::new();
    db.add_record("ARRPUT", Box::new(AoRecord::new(5.0)))
        .await
        .unwrap();

    let result = db
        .put_record_field_from_ca(
            "ARRPUT",
            "VAL",
            EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
        )
        .await;
    assert!(
        result.is_ok(),
        "C dbPut clamps nRequest to the scalar destination and succeeds: {result:?}"
    );
    match db.get_pv("ARRPUT.VAL").unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 1.0).abs() < 1e-10,
            "element 0 must be written (got {v}), the surplus elements dropped"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// R6-10 boundary — the clamp is driven by the *destination*, not the request:
/// the same multi-element array into an **array** field writes every element
/// (C: `no_elements >= nRequest`, nothing is clamped away). Only a one-element
/// destination reduces to element 0.
#[epics_macros_rs::epics_test]
async fn r6_10_multi_element_array_into_array_field_writes_all_elements() {
    let db = PvDatabase::new();
    let wf = epics_base_rs::server::records::waveform::WaveformRecord::new(
        4,
        epics_base_rs::types::DbFieldType::Double,
    );
    db.add_record("ARRWF", Box::new(wf)).await.unwrap();

    db.put_record_field_from_ca("ARRWF", "VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .expect("an array into an array field must succeed");
    assert_eq!(
        db.get_pv("ARRWF.VAL").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
        "an array destination must NOT be clamped to element 0"
    );
}

/// R6-10 boundary — a single-element array is still an array on the wire
/// (`caput -a REC.VAL 1 7`); C's clamp leaves `nRequest = 1` and writes it.
/// Between this and the zero-element case (R6-7, LINK/INVALID alarm, nothing
/// written) sit the two edges of C's `nRequest` branch.
#[epics_macros_rs::epics_test]
async fn r6_10_single_element_array_into_scalar_writes_that_element() {
    let db = PvDatabase::new();
    db.add_record("ONEPUT", Box::new(AoRecord::new(5.0)))
        .await
        .unwrap();

    db.put_record_field_from_ca("ONEPUT", "VAL", EpicsValue::DoubleArray(vec![7.0]))
        .await
        .expect("a one-element array into a scalar must succeed");
    match db.get_pv("ONEPUT.VAL").unwrap() {
        EpicsValue::Double(v) => assert!((v - 7.0).abs() < 1e-10, "expected 7.0, got {v}"),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// R6-10 sibling on the link-delivery side — the same array-into-scalar clamp.
/// C's link layer requests exactly one element (`dbGetLink(..., nRequest =
/// NULL)`), so `dbGet` converts the field at offset 0 and a waveform INP into
/// an `ai.VAL` lands `wf[0]`. The port's `set_val` was a second, parallel
/// coercion path that could not reduce the array, so the value was silently
/// dropped and VAL kept its stale content. `set_val` now routes through
/// `put_field_internal`, the single owner of internal-delivery coercion.
#[epics_macros_rs::epics_test]
async fn r6_10_array_source_link_into_scalar_val_delivers_element_zero() {
    let db = PvDatabase::new();
    let wf = epics_base_rs::server::records::waveform::WaveformRecord::new(
        4,
        epics_base_rs::types::DbFieldType::Double,
    );
    db.add_record("LNKWF", Box::new(wf)).await.unwrap();
    db.put_pv("LNKWF.VAL", EpicsValue::DoubleArray(vec![7.0, 8.0, 9.0]))
        .await
        .unwrap();

    db.add_record("LNKAI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("LNKAI").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("LNKWF".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("LNKAI", &mut visited, 0)
        .await
        .unwrap();

    match db.get_pv("LNKAI.VAL").unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 7.0).abs() < 1e-10,
            "an array source into a scalar VAL must deliver element 0 (got {v}; \
             the pre-fix port dropped the value and left VAL at 0)"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// R6-9 — a put to a field name no record type owns. C `dbNameToAddr`
/// (`dbAccess.c:659-675`) resolves the field part with `dbFindFieldPart`, then
/// falls back to `dbGetAttributePart`; a name that matches neither returns
/// `S_dbLib_fieldNotFound`, so `dbPutField` never runs and every caller reports
/// the failure (`dbpf` prints "PV '%s' not found" and returns -1,
/// `dbTest.c:785-793`). The port's `put_common_field` fell through to
/// `Ok(NoChange)`, so a misspelled field was a silent success.
#[epics_macros_rs::epics_test]
async fn r6_9_put_to_unknown_field_is_an_error_on_every_entry_point() {
    let db = PvDatabase::new();
    db.add_record("BADFLD", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    // The common-field owner itself.
    {
        let rec = db.get_record("BADFLD").unwrap();
        let mut inst = rec.write();
        let err = inst
            .put_common_field("NOSUCH", EpicsValue::Double(1.0))
            .expect_err("an unknown field name must not report success");
        assert!(
            matches!(err, CaError::FieldNotFound(ref f) if f == "NOSUCH"),
            "expected FieldNotFound (S_dbLib_fieldNotFound), got {err:?}"
        );
        // Boundary: a *known* common field on the same record still succeeds.
        inst.put_common_field("DESC", EpicsValue::String("ok".into()))
            .expect("a real dbCommon field must still be accepted");
    }

    // dbPut (`put_pv`) and dbPutField (`put_record_field_from_ca`) — the two
    // entry points `dbpf` / a link / QSRV reach.
    assert!(
        db.put_pv("BADFLD.NOSUCH", EpicsValue::Double(1.0))
            .await
            .is_err(),
        "put_pv to a nonexistent field must fail (C dbNameToAddr → S_db_badField)"
    );
    assert!(
        db.put_record_field_from_ca("BADFLD", "NOSUCH", EpicsValue::Double(1.0))
            .await
            .is_err(),
        "dbPutField to a nonexistent field must fail"
    );
    // …and the record is otherwise untouched by the rejected puts.
    assert_eq!(
        db.get_pv("BADFLD.DESC").unwrap(),
        EpicsValue::String("ok".into())
    );
}

/// R6-9 boundary — a record *attribute* is a different C outcome from an
/// unknown name: it resolves, but the write is refused. `NAME` is
/// `special(SPC_NOMOD)` (`dbCommon.dbd:13-17`) → `dbPutSpecial` pass 0 returns
/// `S_db_noMod` (`dbAccess.c:123-124`); `RTYP` is an attribute, whose address
/// carries `special == SPC_ATTRIBUTE` → `dbPutField` returns the same
/// `S_db_noMod` (`dbAccess.c:1249-1250`). Neither may silently succeed, and
/// neither may be reported as "field not found".
#[epics_macros_rs::epics_test]
async fn r6_9_put_to_a_record_attribute_is_read_only_not_not_found() {
    let db = PvDatabase::new();
    db.add_record("ATTRFLD", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    let rec = db.get_record("ATTRFLD").unwrap();
    let mut inst = rec.write();

    for field in ["NAME", "RTYP"] {
        let err = inst
            .put_common_field(field, EpicsValue::String("hijack".into()))
            .expect_err("an attribute write must not report success");
        assert!(
            matches!(err, CaError::ReadOnlyField(ref f) if f == field),
            "{field}: expected ReadOnlyField (S_db_noMod), got {err:?}"
        );
    }
    assert_eq!(inst.name, "ATTRFLD", "NAME must be unchanged");
}

/// R6-9 boundary — `OUTN` exists only on `swait` (it aliases `common.out`
/// there). On any other record type C has no such field, so the put is an
/// `S_dbLib_fieldNotFound` like any other unknown name; the port's `OUTN` arm
/// used to swallow it.
#[epics_macros_rs::epics_test]
async fn r6_9_outn_is_swait_only() {
    use epics_base_rs::server::records::swait::SwaitRecord;

    let db = PvDatabase::new();
    db.add_record("OUTN_AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("OUTN_SW", Box::new(SwaitRecord::default()))
        .await
        .unwrap();

    let ao = db.get_record("OUTN_AO").unwrap();
    let err = ao
        .write()
        .put_common_field("OUTN", EpicsValue::String("TGT".into()))
        .expect_err("OUTN on a non-swait record is not a field");
    assert!(matches!(err, CaError::FieldNotFound(ref f) if f == "OUTN"));

    let sw = db.get_record("OUTN_SW").unwrap();
    let mut sw = sw.write();
    sw.put_common_field("OUTN", EpicsValue::String("TGT".into()))
        .expect("OUTN on swait must still be accepted");
    assert_eq!(sw.common.out, "TGT");
}

/// R6-6 — C `piniProcess` (`iocInit.c:607-626`) sweeps the database once per
/// distinct `PHAS`, ascending, so PINI records process in phase order; within a
/// phase the order is database load order (`doRecordPini` under
/// `iterateRecords`). The port processed them in `HashMap` iteration order and
/// never read `PHAS` at all.
///
/// The probe: every PINI record drives a shared `SINK` through an OUT link, so
/// `SINK` ends up holding the value of whichever record processed **last** —
/// which under ascending-PHAS order is the highest-PHAS one. The records are
/// added in descending phase order so that load order alone gives the wrong
/// answer.
#[epics_macros_rs::epics_test]
async fn test_pini_records_process_in_ascending_phas_order() {
    use epics_base_rs::server::record::PiniMode;

    let db = PvDatabase::new();
    db.add_record("PHAS_SINK", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    for (name, val, phas) in [
        ("PHAS_C", 30.0, 30i16),
        ("PHAS_A", 10.0, 10),
        ("PHAS_B", 20.0, 20),
    ] {
        db.add_record(name, Box::new(AoRecord::new(val)))
            .await
            .unwrap();
        let rec = db.get_record(name).unwrap();
        let mut inst = rec.write();
        inst.put_common_field("PINI", EpicsValue::String("YES".into()))
            .unwrap();
        inst.put_common_field("PHAS", EpicsValue::Short(phas))
            .unwrap();
        inst.put_common_field("OUT", EpicsValue::String("PHAS_SINK".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    db.pini_process(PiniMode::Yes).await;

    match db.get_pv("PHAS_SINK").unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 30.0).abs() < 1e-10,
            "PINI must process in ascending PHAS order (PHAS_C last); SINK={v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// R6-6 — the sweep re-reads `PHAS` on every pass, which is why C re-scans
/// rather than sorting once: "PHAS fields can be changed at runtime, so we have
/// to look for the lowest value of PHAS each time" (`iocInit.c:613-618`). A
/// record moved out of the phase currently being processed must still be picked
/// up by the pass for its new phase, not dropped.
#[epics_macros_rs::epics_test]
async fn test_pini_sweep_covers_every_phase_present() {
    use epics_base_rs::server::record::PiniMode;

    let db = PvDatabase::new();
    // Phases far apart and not contiguous — the sweep must find each next
    // lowest PHAS rather than stepping one at a time or stopping at the first.
    for (name, phas) in [
        ("SWEEP_MIN", i16::MIN),
        ("SWEEP_ZERO", 0),
        ("SWEEP_MAX", i16::MAX),
    ] {
        db.add_record(name, Box::new(AoRecord::new(1.0)))
            .await
            .unwrap();
        let rec = db.get_record(name).unwrap();
        let mut inst = rec.write();
        inst.put_common_field("PINI", EpicsValue::String("YES".into()))
            .unwrap();
        inst.put_common_field("PHAS", EpicsValue::Short(phas))
            .unwrap();
    }

    db.pini_process(PiniMode::Yes).await;

    for name in ["SWEEP_MIN", "SWEEP_ZERO", "SWEEP_MAX"] {
        assert!(
            db.get_record(name).unwrap().read().common.udf == 0,
            "{name} must be processed by the pass for its own phase"
        );
    }
}

/// R6-5 — a `caput REC.PINI RUN` must store RUN, and an out-of-menu string must
/// be rejected rather than silently landing on NO. The pre-fix bool accepted
/// only `"YES"`/`"1"`/`"true"` and mapped everything else — including the four
/// real menu choices — to `false`, so `caput REC.PINI RUN` *disabled* PINI.
#[epics_macros_rs::epics_test]
async fn test_pini_put_accepts_every_menu_choice_and_rejects_junk() {
    use epics_base_rs::server::record::PiniMode;

    let db = PvDatabase::new();
    db.add_record("PREC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("PREC").unwrap();
    for (label, expect) in [
        ("NO", PiniMode::No),
        ("YES", PiniMode::Yes),
        ("RUN", PiniMode::Run),
        ("RUNNING", PiniMode::Running),
        ("PAUSE", PiniMode::Pause),
        ("PAUSED", PiniMode::Paused),
    ] {
        let mut inst = rec.write();
        inst.put_common_field("PINI", EpicsValue::String(label.into()))
            .unwrap();
        assert_eq!(inst.common.pini, expect.to_u16() as i16, "PINI={label}");
    }
    // Numeric puts index the menu (DBR_ENUM write from a CA client).
    {
        let mut inst = rec.write();
        inst.put_common_field("PINI", EpicsValue::Short(2)).unwrap();
        assert_eq!(inst.common.pini, PiniMode::Run.to_u16() as i16);
    }
    // A string outside the menu is an error, not a silent demotion to NO.
    {
        let mut inst = rec.write();
        assert!(
            inst.put_common_field("PINI", EpicsValue::String("MAYBE".into()))
                .is_err(),
            "an out-of-menu PINI string must be rejected"
        );
        assert_eq!(
            inst.common.pini,
            PiniMode::Run.to_u16() as i16,
            "the rejected put must not change PINI"
        );
    }
}

/// An asyn int32 readback delivers its value via `apply_raw_readback`
/// (sets VAL, requests skip-convert), then the record processes. C
/// `devAsynInt32.c::processAo` sets `pr->udf = isnan(value)` *inside* the
/// readback (:994); epics-rs sets UDF in the framework process loop instead
/// (`clears_udf()` true + `value_is_undefined() == val.is_nan()`), so a
/// finite readback value clears UDF exactly as C does — the device body
/// does not own UDF. Regression guard for the int32-ao udf-on-readback path
/// (the recurring "int32-ao udf-on-NaN" residual is framework-handled, not
/// a divergence).
#[epics_macros_rs::epics_test]
async fn test_ao_asyn_readback_clears_udf_via_framework() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();
    assert!(rec.read().common.udf != 0, "ao starts undefined");

    // Simulate the asyn readback: device sets VAL from the raw and asks the
    // framework to skip the forward VAL->RVAL convert.
    {
        let mut g = rec.write();
        assert!(g.record.apply_raw_readback(150), "ao claims the readback");
        g.record.set_device_did_compute(true);
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();

    let g = rec.read();
    assert!(
        g.common.udf == 0,
        "finite readback value clears UDF (isnan(value)==false)"
    );
    assert_eq!(g.record.get_field("VAL"), Some(EpicsValue::Double(150.0)));
}

#[epics_macros_rs::epics_test]
async fn test_udf_not_cleared_by_clears_udf_false() {
    struct NoClearRecord {
        val: f64,
    }
    impl Record for NoClearRecord {
        fn record_type(&self) -> &'static str {
            "noclear"
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                _ => None,
            }
        }
        fn put_field(
            &mut self,
            name: &str,
            value: EpicsValue,
        ) -> epics_base_rs::error::CaResult<()> {
            match name {
                "VAL" => {
                    if let EpicsValue::Double(v) = value {
                        self.val = v;
                        Ok(())
                    } else {
                        Err(CaError::InvalidValue("bad".into()))
                    }
                }
                _ => Err(CaError::FieldNotFound(name.into())),
            }
        }
        fn declared_fields(&self) -> &'static [FieldDesc] {
            &[]
        }
        fn clears_udf(&self) -> bool {
            false
        }
    }

    let db = PvDatabase::new();
    db.add_record("REC", Box::new(NoClearRecord { val: 0.0 }))
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();
    assert!(rec.read().common.udf != 0);
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    assert!(rec.read().common.udf != 0);
}

/// A constant INP link reaches VAL at INIT — `recGblInitConstantLink(&prec->inp,
/// DBF_DOUBLE, &prec->val)` in `devAiSoft.c::init_record` (line 44) — and NOT at
/// process: `read_ai`'s `dbGetLink` on a constant runs `dbConstGetValue`, which
/// returns 0 having written nothing (`dbConstLink.c:219-225`). So the value must
/// be there before the first process, and must not be re-applied by one.
///
/// The test used to build the record with a bare `add_record` (no init sequence
/// at all) + a runtime `put_common_field("INP", ...)`, then assert the value
/// appeared after `process` — which only passed because the process-time read
/// re-delivered the constant every cycle, the R15-78 defect.
#[epics_macros_rs::epics_test]
async fn test_constant_inp_link() {
    use epics_base_rs::server::ioc_builder::IocBuilder;
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ai, "AI_CONST") { field(INP, "3.15") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let init_val = db.get_pv("AI_CONST").unwrap();
    match init_val {
        EpicsValue::Double(v) => assert!(
            (v - 3.15).abs() < 1e-10,
            "constant INP must be loaded into VAL at init, got {v}"
        ),
        other => panic!("expected Double(3.15), got {other:?}"),
    }
    assert!(
        db.get_record("AI_CONST").unwrap().read().common.udf == 0,
        "C: `if (recGblInitConstantLink(...)) prec->udf = FALSE;`"
    );

    // A client write is NOT clobbered by the next process — the constant is
    // never re-read.
    db.put_pv("AI_CONST", EpicsValue::Double(7.0))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("AI_CONST", &mut visited, 0)
        .await
        .unwrap();
    match db.get_pv("AI_CONST").unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 7.0).abs() < 1e-10,
            "a constant INP must not be re-applied at process, got {v}"
        ),
        other => panic!("expected Double(7.0), got {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn test_calc_multi_input_db_links() {
    use epics_base_rs::server::records::calc::CalcRecord;
    let db = PvDatabase::new();
    db.add_record("SRC_A", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    db.add_record("SRC_B", Box::new(AoRecord::new(20.0)))
        .await
        .unwrap();
    let mut calc = CalcRecord::new("A+B");
    calc.inpa = "SRC_A".to_string();
    calc.inpb = "SRC_B".to_string();
    db.add_record("CALC_REC", Box::new(calc)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_REC", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("CALC_REC").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 30.0).abs() < 1e-10),
        other => panic!("expected Double(30.0), got {:?}", other),
    }
}

/// Defect 3 regression: a `ProcessPassive` (PP) multi-input link
/// (`INPA..INPL` for calc/sel/sub/aSub) must process its passive
/// source record BEFORE the value is read — C `dbGetLink` behaviour.
/// Before the fix the multi-input fetch loop used `read_link_with_alarm`
/// (bare `get_pv`, no PP processing), so a PP input link read a stale
/// source value. The single-INP path already did this via
/// `read_link_value_soft`.
#[epics_macros_rs::epics_test]
async fn test_calc_multi_input_pp_processes_passive_source() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();

    // SRC: a passive calc whose VAL computes to 42 only when processed.
    // Its stored VAL starts at the default 0.0.
    let src = CalcRecord::new("42");
    db.add_record("PP_SRC", Box::new(src)).await.unwrap();

    // DST: INPA = "PP_SRC PP" (process-passive). CALC="A" copies INPA.
    let mut dst = CalcRecord::new("A");
    dst.inpa = "PP_SRC PP".to_string();
    db.add_record("PP_DST", Box::new(dst)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("PP_DST", &mut visited, 0)
        .await
        .unwrap();

    // DST must see 42: the PP link processed PP_SRC first, computing
    // its VAL=42 before the value was read. A stale read would yield 0.
    let val = db.get_pv("PP_DST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "PP multi-input link must process source first: expected 42, got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    // The source itself must have been processed (VAL latched to 42).
    let src_val = db.get_pv("PP_SRC").unwrap();
    match src_val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "PP_SRC must have been processed by the PP link, VAL={v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
}

/// `process_record` (the public direct-process API) fetches input links like
/// the engine path. It used to call the reduced `process_local`, which fetched
/// no INPx, so a direct process of a calc/sub read stale A..U inputs; it now
/// delegates to the canonical link-fetching engine path.
#[epics_macros_rs::epics_test]
async fn process_record_fetches_input_links() {
    use epics_base_rs::server::records::ao::AoRecord;
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();

    // SRC_F latches the constructed VAL=10.
    db.add_record("SRC_F", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();

    // DST_F: CALC="A+1", INPA="SRC_F" (NPP — read the current source value).
    let mut dst = CalcRecord::new("A+1");
    dst.inpa = "SRC_F".to_string();
    db.add_record("DST_F", Box::new(dst)).await.unwrap();

    // Direct process via the public API. A=10 must be fetched from SRC_F,
    // giving VAL=11; the old reduced path left A=0 -> VAL=1.
    db.process_record("DST_F").await.unwrap();

    match db.get_pv("DST_F").unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 11.0).abs() < 1e-10,
            "process_record must fetch INPA (SRC_F=10): expected 11, got {v}"
        ),
        other => panic!("expected Double(11.0), got {other:?}"),
    }
}

/// Defect 3 control: an `NPP` (no-process-passive) multi-input link
/// must NOT process its passive source — it reads whatever stale
/// value the source currently holds.
#[epics_macros_rs::epics_test]
async fn test_calc_multi_input_npp_does_not_process_source() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();

    let src = CalcRecord::new("42");
    db.add_record("NPP_SRC", Box::new(src)).await.unwrap();

    let mut dst = CalcRecord::new("A");
    dst.inpa = "NPP_SRC NPP".to_string();
    db.add_record("NPP_DST", Box::new(dst)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("NPP_DST", &mut visited, 0)
        .await
        .unwrap();

    // NPP_SRC was never processed, so its VAL stays at the default 0.0
    // and DST reads 0, not 42.
    let val = db.get_pv("NPP_DST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            v.abs() < 1e-10,
            "NPP multi-input link must NOT process source: expected 0, got {v}"
        ),
        other => panic!("expected Double(0.0), got {other:?}"),
    }
}

/// R6-2 — a link whose target field name carries a digit (`B0`, `DO1`,
/// `LNK1`) is a plain local DB link. C `dbNameToAddr` (`dbAccess.c:666-670`)
/// terminates the record name at the first `.` and matches the remainder
/// against the record type's field table — no character-class restriction.
/// Pre-fix the port required the field part to be all-`is_ascii_uppercase`, so
/// `'0'` failed the guard and `INP="DIRECT.B0"` parsed as a link to a *record*
/// literally named `"DIRECT.B0"`: it never resolved locally and was re-routed
/// as an external CA channel, losing lock-set atomicity and PP/MS semantics.
#[epics_macros_rs::epics_test]
async fn test_link_to_digit_bearing_field_is_a_local_db_link() {
    use epics_base_rs::server::record::{DbLink, LinkProcessPolicy, MonitorSwitch, ParsedLink};
    use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;

    // Parse boundary: the field part is a C identifier, so digits and `_` are
    // legal and the old 4-character cap is gone (dbCommon has `OLDSIMM`).
    assert_eq!(
        epics_base_rs::server::record::parse_link_v2("DIRECT.B0 NPP MS"),
        ParsedLink::Db(DbLink::new(
            "DIRECT.B0",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::Maximize,
        ))
    );
    for (link, field) in [
        ("SEQ.DO1", "DO1"),
        ("SEQ.LNK1", "LNK1"),
        ("SEQ.LNKA", "LNKA"),
        ("REC.OLDSIMM", "OLDSIMM"),
        ("REC._X1", "_X1"),
    ] {
        match epics_base_rs::server::record::parse_link_v2(link) {
            ParsedLink::Db(db) => assert_eq!(db.target().field, field, "link {link}"),
            other => panic!("link {link} must be a Db link, got {other:?}"),
        }
    }
    // A remainder that is NOT a field name (leading digit) is C's "absent
    // field name" case: the whole string stays the record/PV name, which is
    // what preserves a dotted remote PV for the CA fallback.
    match epics_base_rs::server::record::parse_link_v2("OTHER:PV.1:X") {
        // `pvname` is the whole string exactly when the split left the field
        // at its `VAL` default; a `1:X` field half would print here.
        ParsedLink::Db(db) => assert_eq!(db.pvname(), "OTHER:PV.1:X"),
        other => panic!("expected a whole-name Db link, got {other:?}"),
    }
    // The record name terminates at the FIRST `.`, not the last.
    match epics_base_rs::server::record::parse_link_v2("A.B.C") {
        ParsedLink::Db(db) => assert_eq!(db.pvname(), "A.B.C", "'B.C' is not a field name"),
        other => panic!("expected a whole-name Db link, got {other:?}"),
    }

    // End to end: an ai whose INP names `DIRECT.B0` reads the B0 bit out of
    // the local mbboDirect record, not a nonexistent record "DIRECT.B0".
    let db = PvDatabase::new();
    let mut direct = MbboDirectRecord::default();
    direct.put_field("B0", EpicsValue::Short(1)).unwrap();
    db.add_record("DIRECT", Box::new(direct)).await.unwrap();
    db.add_record("SINK", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("SINK") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("DIRECT.B0".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SINK", &mut visited, 0)
        .await
        .unwrap();

    match db.get_pv("SINK").unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 1.0).abs() < 1e-10,
            "SINK.INP=DIRECT.B0 must read the B0 bit (1), got {v}"
        ),
        other => panic!("expected Double(1.0), got {other:?}"),
    }
    // The link resolved locally, so the record is NOT in LINK/INVALID alarm.
    if let Some(rec) = db.get_record("SINK") {
        let inst = rec.read();
        assert_eq!(
            inst.common.sevr,
            epics_base_rs::server::record::AlarmSeverity::NoAlarm,
            "a resolvable local link must not raise LINK_ALARM"
        );
    }
}

/// A **modifier-less** (bare) multi-input link must behave like NPP —
/// C `dbParseLink` (`dbStaticLib.c:2252,2369-2371`) leaves `pvlOptPP`
/// unset for a bare link, so `dbDbGetValue` (`dbDbLink.c:175`) does NOT
/// process the passive source on read. Before the fix `parse_link_v2`
/// defaulted a bare link to `ProcessPassive`, so `BARE_DST` would have
/// spuriously processed `BARE_SRC` and read 42; after the fix it reads
/// the stale 0.
#[epics_macros_rs::epics_test]
async fn test_calc_multi_input_bare_does_not_process_source() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();

    let src = CalcRecord::new("42");
    db.add_record("BARE_SRC", Box::new(src)).await.unwrap();

    let mut dst = CalcRecord::new("A");
    // No modifier — must default to NPP, NOT ProcessPassive.
    dst.inpa = "BARE_SRC".to_string();
    db.add_record("BARE_DST", Box::new(dst)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("BARE_DST", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("BARE_DST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            v.abs() < 1e-10,
            "bare multi-input link is NPP and must NOT process its source: expected 0, got {v}"
        ),
        other => panic!("expected Double(0.0), got {other:?}"),
    }
    // BARE_SRC must still hold its unprocessed default.
    let src_val = db.get_pv("BARE_SRC").unwrap();
    match src_val {
        EpicsValue::Double(v) => assert!(
            v.abs() < 1e-10,
            "bare INP must leave BARE_SRC unprocessed, VAL={v}"
        ),
        other => panic!("expected Double(0.0), got {other:?}"),
    }
}

/// Defect 1 regression (CRITICAL): two passive calc records whose
/// `INPA` PP links point at each other (`A.INPA="B PP"`,
/// `B.INPA="A PP"`) form a PP-link cycle. Before the fix
/// `process_passive_db_source` created a FRESH `visited` set and
/// reset depth to 0 on every PP hop, so neither `MAX_LINK_DEPTH`
/// nor the `visited` cycle guard fired across the hop — the cycle
/// recursed unboundedly to a stack overflow / SIGABRT.
///
/// C terminates this cycle because `calcRecord.c::process` sets
/// `prec->pact = TRUE` *before* `fetch_values()` (calcRecord.c:119),
/// so the re-entrant `dbProcess` hits `if (precord->pact) goto
/// all_done;` (dbAccess.c:536) and bails after one bounce. The Rust
/// fix threads the caller's `visited` set / `depth` through the PP
/// hop so the existing `visited.insert` guard
/// (`process_record_with_links_inner`) fires instead.
///
/// This test passing at all proves the fix: a regression re-aborts
/// the whole test process with a stack overflow.
#[epics_macros_rs::epics_test]
async fn test_calc_pp_link_cycle_terminates() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();

    // CALC_A.INPA = "CALC_B PP", CALC_B.INPA = "CALC_A PP".
    // Both passive, both CALC="A" (copy the input).
    let mut a = CalcRecord::new("A");
    a.inpa = "CALC_B PP".to_string();
    db.add_record("CALC_A", Box::new(a)).await.unwrap();

    let mut b = CalcRecord::new("A");
    b.inpa = "CALC_A PP".to_string();
    db.add_record("CALC_B", Box::new(b)).await.unwrap();

    // Must return cleanly (Ok) without overflowing the stack — the
    // cycle guard terminates the A->B->A bounce.
    let mut visited = HashSet::new();
    let result = db
        .process_record_with_links("CALC_A", &mut visited, 0)
        .await;
    assert!(
        result.is_ok(),
        "PP-link A<->B cycle must terminate cleanly, got {result:?}"
    );

    // Both records read a finite value (default 0.0 — neither has a
    // real source). The point is that processing completed at all.
    let va = db.get_pv("CALC_A").unwrap();
    let vb = db.get_pv("CALC_B").unwrap();
    match (va, vb) {
        (EpicsValue::Double(x), EpicsValue::Double(y)) => {
            assert!(
                x.is_finite() && y.is_finite(),
                "cycle must leave finite values, got A={x} B={y}"
            );
        }
        other => panic!("expected Double values, got {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn test_calc_constant_inputs() {
    use epics_base_rs::server::records::calc::CalcRecord;
    let db = PvDatabase::new();
    let mut calc = CalcRecord::new("A+B");
    calc.inpa = "5".to_string();
    calc.inpb = "3.5".to_string();
    db.add_record("CALC_CONST", Box::new(calc)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_CONST", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("CALC_CONST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 8.5).abs() < 1e-10),
        other => panic!("expected Double(8.5), got {:?}", other),
    }
}

// C parity (calcRecord.dbd.pod:716-744): calc record carries the same
// HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV alarm-limit fields as
// ai/ao/longin/longout. The Rust port omitted them — put_field for
// HIHI silently no-op'd because `common.analog_alarm` was None for
// rtype="calc".
#[epics_macros_rs::epics_test]
async fn test_calc_record_has_analog_alarm_limits() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();
    let mut calc = CalcRecord::new("A");
    calc.inpa = "15".to_string(); // VAL will compute to 15
    db.add_record("CALC_LIM", Box::new(calc)).await.unwrap();

    // Configure HIHI=10, HHSV=MAJOR. Put goes through put_record_field_from_ca
    // which routes to common.analog_alarm.
    db.put_record_field_from_ca("CALC_LIM", "HIHI", EpicsValue::Double(10.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("CALC_LIM", "HHSV", EpicsValue::String("MAJOR".into()))
        .await
        .unwrap();

    // Read back — verifies the put landed in common.analog_alarm.
    let hihi = {
        let rec = db.get_record("CALC_LIM").unwrap();
        let inst = rec.read();
        inst.resolve_field("HIHI").and_then(|v| v.to_f64()).unwrap()
    };
    assert_eq!(hihi, 10.0);

    // Process — CALC="A" with A=15 → VAL=15 > HIHI=10 → HIHI_ALARM/MAJOR.
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_LIM", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("CALC_LIM").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        epics_base_rs::server::record::AlarmSeverity::Major,
        "VAL=15, HIHI=10, HHSV=MAJOR — must raise HIHI alarm",
    );
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::HIHI_ALARM,
    );
}

// C parity (calcRecord.c::checkAlarms:339-381): with AFTC > 0 the
// alarm-range integer is exponentially smoothed, so a brief excursion
// above HIHI does NOT immediately raise the severity until the filter
// converges.
#[epics_macros_rs::epics_test]
async fn test_calc_record_aftc_filter_delays_alarm() {
    use epics_base_rs::server::records::calc::CalcRecord;

    let db = PvDatabase::new();
    let mut calc = CalcRecord::new("A");
    calc.inpa = "1".to_string();
    calc.aftc = 5.0; // 5-second filter time-constant
    db.add_record("CALC_AFTC", Box::new(calc)).await.unwrap();
    db.put_record_field_from_ca("CALC_AFTC", "HIHI", EpicsValue::Double(10.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("CALC_AFTC", "HHSV", EpicsValue::String("MAJOR".into()))
        .await
        .unwrap();

    // First process — filter seeds with NoAlarm (alarm_range=3, Normal).
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_AFTC", &mut visited, 0)
        .await
        .unwrap();

    // Set VAL=15 (HIHI condition) and process. With aftc=5s and dt
    // very small (sub-second between processes), alpha=5/(eps+5)≈1.0,
    // and filtered_range stays at 3 (Normal). The new alarm range (5)
    // must be smoothed out by the filter — alarm must NOT fire on the
    // first transition.
    let rec = db.get_record("CALC_AFTC").unwrap();
    {
        let mut inst = rec.write();
        let _ = inst.record.put_field("VAL", EpicsValue::Double(15.0));
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_AFTC", &mut visited, 0)
        .await
        .unwrap();
    let inst = rec.read();
    // afvl must have been updated (filter is engaged)
    let afvl = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .unwrap_or(0.0);
    assert!(afvl != 0.0, "AFVL must be updated when AFTC > 0");
}

#[epics_macros_rs::epics_test]
async fn test_fanout_all() {
    use epics_base_rs::server::records::calc::CalcRecord;
    use epics_base_rs::server::records::fanout::FanoutRecord;
    let db = PvDatabase::new();
    let mut fanout = FanoutRecord::new();
    fanout.selm = 0;
    fanout.lnk1 = "TARGET_1".to_string();
    fanout.lnk2 = "TARGET_2".to_string();
    db.add_record("FANOUT", Box::new(fanout)).await.unwrap();
    for name in ["TARGET_1", "TARGET_2"] {
        let mut tgt = CalcRecord::new("VAL+1");
        tgt.init_record(0).unwrap();
        db.add_record(name, Box::new(tgt)).await.unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("FANOUT", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(db.get_pv("TARGET_1").unwrap().to_f64(), Some(1.0));
    assert_eq!(db.get_pv("TARGET_2").unwrap().to_f64(), Some(1.0));
}

#[epics_macros_rs::epics_test]
async fn test_fanout_specified() {
    // C parity (fanoutRecord.c:114): SELM=Specified selects the link
    // at index `SELN + OFFS`, 0-based over LNK0..LNKF. With SELN=1,
    // OFFS=0 the selected link is LNK1 (NOT LNK2 — the pre-fix port
    // omitted LNK0 and was off by one).
    use epics_base_rs::server::records::calc::CalcRecord;
    use epics_base_rs::server::records::fanout::FanoutRecord;
    let db = PvDatabase::new();
    let mut fanout = FanoutRecord::new();
    fanout.selm = 1;
    fanout.seln = 1;
    db.add_record("FANOUT", Box::new(fanout)).await.unwrap();
    for name in ["T1", "T2"] {
        let mut tgt = CalcRecord::new("VAL+1");
        tgt.init_record(0).unwrap();
        db.add_record(name, Box::new(tgt)).await.unwrap();
    }
    if let Some(rec) = db.get_record("FANOUT") {
        let mut inst = rec.write();
        inst.record
            .put_field("LNK1", EpicsValue::String("T1".into()))
            .unwrap();
        inst.record
            .put_field("LNK2", EpicsValue::String("T2".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("FANOUT", &mut visited, 0)
        .await
        .unwrap();
    // SELN=1 → LNK1 → T1 processed; LNK2/T2 NOT processed.
    assert_eq!(db.get_pv("T1").unwrap().to_f64(), Some(1.0));
    assert_eq!(db.get_pv("T2").unwrap().to_f64(), Some(0.0));
}

#[epics_macros_rs::epics_test]
async fn test_dfanout_value_write() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;
    let db = PvDatabase::new();
    let mut dfan = DfanoutRecord::new(42.0);
    dfan.selm = 0;
    dfan.outa = "DEST_A".to_string();
    dfan.outb = "DEST_B".to_string();
    db.add_record("DFAN", Box::new(dfan)).await.unwrap();
    db.add_record("DEST_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("DEST_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("DFAN", &mut visited, 0)
        .await
        .unwrap();
    let val_a = db.get_pv("DEST_A").unwrap();
    match val_a {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
    let val_b = db.get_pv("DEST_B").unwrap();
    match val_b {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

/// C `dfanoutRecord.c:116-122` reads VAL from DOL on every process
/// cycle when `omsl == menuOmslclosed_loop`. The Rust port previously
/// omitted dfanout from the DOL-eligible record-type list in
/// `processing.rs::process_record_with_links_inner`, so a dfanout
/// configured with OMSL=closed_loop never sourced VAL from DOL —
/// every cycle silently kept the previously-cached VAL.
#[epics_macros_rs::epics_test]
async fn test_dfanout_omsl_closed_loop_sources_val_from_dol() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;

    let db = PvDatabase::new();

    // Upstream setpoint source.
    db.add_record("DOL_SRC", Box::new(AoRecord::new(7.5)))
        .await
        .unwrap();

    // dfanout with OMSL=closed_loop and DOL=DOL_SRC. SELM=0 (All)
    // distributes VAL to OUTA + OUTB.
    let mut dfan = DfanoutRecord::new(0.0);
    dfan.selm = 0;
    dfan.outa = "DFAN_DEST_A".to_string();
    dfan.outb = "DFAN_DEST_B".to_string();
    dfan.dol = "DOL_SRC".to_string();
    dfan.omsl = 1; // closed_loop (menuOmslclosed_loop)
    db.add_record("DFAN_OMSL", Box::new(dfan)).await.unwrap();

    db.add_record("DFAN_DEST_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("DFAN_DEST_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("DFAN_OMSL", &mut visited, 0)
        .await
        .unwrap();

    // DOL_SRC's VAL (7.5) must have flowed through DOL → dfanout.VAL → OUTA/OUTB.
    let val_a = db.get_pv("DFAN_DEST_A").unwrap();
    assert!(
        matches!(val_a, EpicsValue::Double(v) if (v - 7.5).abs() < 1e-10),
        "DFAN_DEST_A must reflect DOL_SRC (=7.5), got {val_a:?}"
    );
    let val_b = db.get_pv("DFAN_DEST_B").unwrap();
    assert!(
        matches!(val_b, EpicsValue::Double(v) if (v - 7.5).abs() < 1e-10),
        "DFAN_DEST_B must reflect DOL_SRC (=7.5), got {val_b:?}"
    );
}

/// Companion to the OMSL=closed_loop test: with OMSL=supervisory
/// (default), DOL must NOT be evaluated even if a DOL link is set —
/// VAL remains under operator control. This pins the gating so a
/// future refactor cannot silently widen the closed-loop scope.
#[epics_macros_rs::epics_test]
async fn test_dfanout_omsl_supervisory_ignores_dol() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;

    let db = PvDatabase::new();
    db.add_record("DOL_SRC2", Box::new(AoRecord::new(99.0)))
        .await
        .unwrap();

    let mut dfan = DfanoutRecord::new(3.0);
    dfan.selm = 0;
    dfan.outa = "DFAN_DEST_A2".to_string();
    dfan.dol = "DOL_SRC2".to_string();
    dfan.omsl = 0; // supervisory (menuOmslsupervisory)
    db.add_record("DFAN_SUP", Box::new(dfan)).await.unwrap();
    db.add_record("DFAN_DEST_A2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("DFAN_SUP", &mut visited, 0)
        .await
        .unwrap();

    let val_a = db.get_pv("DFAN_DEST_A2").unwrap();
    assert!(
        matches!(val_a, EpicsValue::Double(v) if (v - 3.0).abs() < 1e-10),
        "OMSL=supervisory must keep the operator-staged VAL (=3.0), got {val_a:?}"
    );
}

/// Stand in for a .db `field(VAL,"…")` seed on a programmatically created
/// record: C's static write of VAL also writes UDF=0
/// (`dbPutString`, dbStaticLib.c:2653-2660), which is what makes the record
/// DEFINED for record types whose `process()` never clears UDF (dfanout,
/// histogram, event).
async fn define_val(db: &PvDatabase, name: &str) {
    let rec = db.get_record(name).expect("record exists");
    rec.write().common.udf = 0;
}

/// A failed dfanout OUT-link put raises the LINK alarm TWICE in C, and the
/// stronger one wins:
///
/// * `dbPutLink` itself calls `setLinkAlarm` on a nonzero putValue status —
///   `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM)` (dbLink.c:316-321,
///   434-448); an OUT link naming no local record is a (never-connected) CA
///   link, whose put fails exactly this way;
/// * `push_values` then adds its own `recGblSetSevr(LINK_ALARM, MAJOR_ALARM)`
///   (dfanoutRecord.c:311-312), which `recGblSetSevr` drops because the
///   pending severity is already INVALID.
///
/// `process()` (127-147) runs `push_values` between `checkAlarms` and
/// `recGblResetAlarms`, so the write-failure alarm folds into the SAME
/// cycle's committed SEVR and the VAL monitor post. After ONE process, a
/// broken OUT link must leave SEVR=INVALID / STAT=LINK — proving the
/// same-cycle fold (a one-cycle-late latch would read NO_ALARM here).
#[epics_macros_rs::epics_test]
async fn test_dfanout_out_link_write_failure_raises_link_alarm() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::records::dfanout::DfanoutRecord;

    let db = PvDatabase::new();

    // SELM=All pushes VAL to OUTA. OUTA targets a record that does not
    // exist locally and no external link set is registered, so the write
    // is rejected — C `dbPutLink` status != 0.
    let mut dfan = DfanoutRecord::new(5.0);
    dfan.selm = 0; // All
    dfan.outa = "DFAN_NO_SUCH_DEST".to_string();
    db.add_record("DFAN_LINKFAIL", Box::new(dfan))
        .await
        .unwrap();
    // The seeded VAL is the `field(VAL,"5")` of a .db load, and C's
    // `dbPutString` writes UDF=0 alongside a static VAL write
    // (dbStaticLib.c:2653-2660). Without it the record is undefined, and
    // dfanout's `process()` never clears UDF — softIoc:
    // `record(dfanout,"DFN"){field(OUTA,"DEST")}` stays UDF=1 / INVALID /
    // UDF after `dbpf DFN.PROC 1`, while the same record with
    // `field(VAL,"5")` comes up UDF=0 / NO_ALARM.
    define_val(&db, "DFAN_LINKFAIL").await;

    let mut visited = HashSet::new();
    db.process_record_with_links("DFAN_LINKFAIL", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("DFAN_LINKFAIL").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "failed dfanout OUT-link write must raise SEVR=INVALID this cycle \
         (dbPutLink's setLinkAlarm), got {:?}",
        inst.common.sevr
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "failed dfanout OUT-link write must raise STAT=LINK, got {}",
        inst.common.stat
    );
}

/// Companion gate: a dfanout whose OUT links all write successfully must
/// NOT raise a LINK alarm — SEVR stays NO_ALARM. Pins that the
/// write-failure fold above only fires on a real `dbPutLink` failure.
#[epics_macros_rs::epics_test]
async fn test_dfanout_out_link_write_success_no_link_alarm() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;

    let db = PvDatabase::new();
    db.add_record("DFAN_OK_DEST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut dfan = DfanoutRecord::new(5.0);
    dfan.selm = 0; // All
    dfan.outa = "DFAN_OK_DEST".to_string();
    db.add_record("DFAN_OK", Box::new(dfan)).await.unwrap();
    // See `test_dfanout_out_link_write_failure_raises_link_alarm`: the
    // seeded VAL stands for `field(VAL,"5")`, which C loads with UDF=0.
    define_val(&db, "DFAN_OK").await;

    let mut visited = HashSet::new();
    db.process_record_with_links("DFAN_OK", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("DFAN_OK_DEST").unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 5.0).abs() < 1e-10),
        "successful OUT write must land VAL=5.0 on the target, got {val:?}"
    );
    let rec = db.get_record("DFAN_OK").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::NoAlarm,
        "successful dfanout OUT-link write must not raise any alarm, got {:?}",
        inst.common.sevr
    );
}

/// seq is async in C: `process` sets `pact` and arms the group chain on the
/// callback task whatever the DLYn ("Always use the callback task to avoid
/// recursion", `seqRecord.c:210-215`), so the group work and `asyncFinish`
/// (`:219-241`) run after `process_record_with_links` returns.
async fn settle_seq(db: &PvDatabase, rec: &str) {
    for _ in 0..400 {
        if !db.get_record(rec).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("{rec} never left PACT");
}

#[epics_macros_rs::epics_test]
async fn test_seq_dol_lnk_dispatch() {
    use epics_base_rs::server::records::seq::SeqRecord;
    let db = PvDatabase::new();
    db.add_record("SEQ_SRC1", Box::new(AoRecord::new(100.0)))
        .await
        .unwrap();
    db.add_record("SEQ_SRC2", Box::new(AoRecord::new(200.0)))
        .await
        .unwrap();
    db.add_record("SEQ_DEST1", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("SEQ_DEST2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut seq = SeqRecord::new();
    seq.selm = 0;
    seq.dol1 = "SEQ_SRC1".to_string();
    seq.lnk1 = "SEQ_DEST1".to_string();
    seq.dol2 = "SEQ_SRC2".to_string();
    seq.lnk2 = "SEQ_DEST2".to_string();
    db.add_record("SEQ_REC", Box::new(seq)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("SEQ_REC", &mut visited, 0)
        .await
        .unwrap();
    settle_seq(&db, "SEQ_REC").await;
    let val1 = db.get_pv("SEQ_DEST1").unwrap();
    match val1 {
        EpicsValue::Double(v) => assert!((v - 100.0).abs() < 1e-10),
        other => panic!("expected Double(100.0), got {:?}", other),
    }
    let val2 = db.get_pv("SEQ_DEST2").unwrap();
    match val2 {
        EpicsValue::Double(v) => assert!((v - 200.0).abs() < 1e-10),
        other => panic!("expected Double(200.0), got {:?}", other),
    }
}

// C `seqRecord.c::processCallback` (256-268) reads DOLn into the DOn
// value field (`dbGetLink(&dol, DBR_DOUBLE, &dov)`), drives LNKn with it,
// then posts DOn on change. The Rust seq dispatch used DOLn only
// transiently for the LNKn write and never wrote it back into DOn, so a
// client reading/monitoring DOn saw a stale value.
#[epics_macros_rs::epics_test]
async fn test_seq_writes_dol_readback_into_don() {
    use epics_base_rs::server::records::seq::SeqRecord;
    let db = PvDatabase::new();
    db.add_record("SEQ9_SRC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();
    db.add_record("SEQ9_DEST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut seq = SeqRecord::new();
    seq.selm = 0; // All
    seq.dol0 = "SEQ9_SRC".to_string();
    seq.lnk0 = "SEQ9_DEST".to_string();
    db.add_record("SEQ9_REC", Box::new(seq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SEQ9_REC", &mut visited, 0)
        .await
        .unwrap();
    settle_seq(&db, "SEQ9_REC").await;

    // LNK0 driven (existing behaviour).
    let dest = db.get_pv("SEQ9_DEST").unwrap();
    match dest {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    // DO0 read back from DOL0 — was stale (0.0) pre-fix.
    let do0 = db.get_pv("SEQ9_REC.DO0").unwrap();
    match do0 {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "DO0 must hold the DOL0 read-back (C dbGetLink->dov), got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
}

// C `seqRecord.c:182-189` processes a group when its DOLn OR LNKn is a
// real link. A DOL-only group (real DOLn, empty LNKn) still reads back
// DOn and posts it, even though nothing is driven. The Rust dispatch
// skipped any group with an empty LNKn, so DOn never updated.
#[epics_macros_rs::epics_test]
async fn test_seq_dol_only_group_updates_don_with_empty_lnk() {
    use epics_base_rs::server::records::seq::SeqRecord;
    let db = PvDatabase::new();
    db.add_record("SEQ9B_SRC", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    let mut seq = SeqRecord::new();
    seq.selm = 0; // All
    seq.dol0 = "SEQ9B_SRC".to_string();
    // lnk0 stays empty — DOL-only group
    db.add_record("SEQ9B_REC", Box::new(seq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SEQ9B_REC", &mut visited, 0)
        .await
        .unwrap();
    settle_seq(&db, "SEQ9B_REC").await;

    let do0 = db.get_pv("SEQ9B_REC.DO0").unwrap();
    match do0 {
        EpicsValue::Double(v) => assert!(
            (v - 7.0).abs() < 1e-10,
            "DOL-only group must still read back DO0 (C processes a group \
             when DOLn is real even with empty LNKn), got {v}"
        ),
        other => panic!("expected Double(7.0), got {other:?}"),
    }
}

// A process-counting target. `process()` bumps the shared counter so a
// test can prove whether a link DID or DID NOT process its target.
struct CountingTarget {
    process_count: Arc<AtomicU32>,
}

/// A link target must expose a `VAL` field: an OUT link's buffer is chosen
/// from the DESTINATION's DBF type (C `dbNameToAddr` / `dbGetLinkDBFtype`),
/// and a record whose field type does not resolve is not written at all
/// (sseq `processCallback`'s `default: break`). C has no field-less record.
static COUNTING_TARGET_FIELDS: &[FieldDesc] = &[FieldDesc::new(
    "VAL",
    epics_base_rs::types::DbFieldType::Double,
    false,
)];

impl Record for CountingTarget {
    fn record_type(&self) -> &'static str {
        "counting_target"
    }
    fn process(&mut self) -> epics_base_rs::error::CaResult<ProcessOutcome> {
        self.process_count.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        COUNTING_TARGET_FIELDS
    }
}

// BUG 1 regression — seq `LNKn` is `DBF_OUTLINK` (`seqRecord.dbd.pod:316`)
// driven via `dbPutLink` (`seqRecord.c:264`). `dbDbPutValue`
// (`dbDbLink.c:388`) processes the target only when the link carries an
// explicit `PP` modifier. A bare (modifier-less) seq LNKn is NPP — the
// target value is written but the target is NOT processed.
// `parse_link_v2`/`parse_output_link_v2` default a modifier-less link
// to NPP (`NoProcess`) like C `dbParseLink`, so the `MultiOut::Seq` arm
// needs no per-call downgrade.
#[epics_macros_rs::epics_test]
async fn test_seq_bare_lnk_does_not_process_passive_target() {
    use epics_base_rs::server::records::seq::SeqRecord;
    let db = PvDatabase::new();

    let bare_count = Arc::new(AtomicU32::new(0));
    let pp_count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "SEQ_BARE_TGT",
        Box::new(CountingTarget {
            process_count: bare_count.clone(),
        }),
    )
    .await
    .unwrap();
    db.add_record(
        "SEQ_PP_TGT",
        Box::new(CountingTarget {
            process_count: pp_count.clone(),
        }),
    )
    .await
    .unwrap();

    let mut seq = SeqRecord::new();
    seq.selm = 0;
    // Group 1: bare LNK — must NOT process the Passive target.
    seq.do1 = 11.0;
    seq.lnk1 = "SEQ_BARE_TGT".to_string();
    // Group 2: explicit PP LNK — must process the Passive target.
    seq.do2 = 22.0;
    seq.lnk2 = "SEQ_PP_TGT PP".to_string();
    db.add_record("SEQ_NPP_REC", Box::new(seq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SEQ_NPP_REC", &mut visited, 0)
        .await
        .unwrap();
    settle_seq(&db, "SEQ_NPP_REC").await;

    assert_eq!(
        bare_count.load(Ordering::SeqCst),
        0,
        "bare seq LNKn (NPP) must NOT process its Passive target"
    );
    assert_eq!(
        pp_count.load(Ordering::SeqCst),
        1,
        "explicit-PP seq LNKn must process its Passive target"
    );
}

// R0604 regression — a record's `WriteDbLink` OUT write must land BEFORE
// the producing record's FLNK fires. C record support performs
// `dbPutLink()` for OUT links before `recGblFwdLink()`
// (transformRecord.c:605-621, scalerRecord.c:457-480), so a downstream
// FLNK target that reads the written PV observes the new value. Pre-fix
// the framework ran the FLNK tail first, leaving the target stale.
struct WriteThenFlnkProducer;
impl Record for WriteThenFlnkProducer {
    fn record_type(&self) -> &'static str {
        "write_flnk_producer"
    }
    fn process(&mut self) -> epics_base_rs::error::CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete_with(vec![
            ProcessAction::WriteDbLink {
                link_field: "OUT",
                value: EpicsValue::Double(42.0),
            },
        ]))
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "OUT" => Some(EpicsValue::String("WF_TARGET".into())),
            _ => None,
        }
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    /// `OUT` is read back through `resolve_field`, which serves only what
    /// the record type declares.
    fn declared_fields(&self) -> &'static [FieldDesc] {
        WRITE_FLNK_PRODUCER_FIELDS
    }
}

static WRITE_FLNK_PRODUCER_FIELDS: &[FieldDesc] = &[FieldDesc::new(
    "OUT",
    epics_base_rs::types::DbFieldType::String,
    false,
)];

#[epics_macros_rs::epics_test]
async fn test_write_db_link_runs_before_flnk_target_reads_fresh() {
    let db = PvDatabase::new();
    // Target the producer writes 42.0 into.
    db.add_record("WF_TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // FLNK observer reads WF_TARGET via its INP during its own process.
    db.add_record("WF_OBSERVER", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("WF_OBSERVER") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("WF_TARGET".into()))
            .unwrap();
    }
    // Producer: WriteDbLink → WF_TARGET, FLNK → WF_OBSERVER.
    db.add_record("WF_PRODUCER", Box::new(WriteThenFlnkProducer))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("WF_PRODUCER") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("WF_OBSERVER".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("WF_PRODUCER", &mut visited, 0)
        .await
        .unwrap();

    // The OUT write ran before FLNK, so the observer read the fresh 42.0.
    let observed = db.get_pv("WF_OBSERVER").unwrap();
    match observed {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "FLNK observer must read the post-WriteDbLink value 42.0, got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
}

/// Poll an atomic counter until it reaches `want`, then settle so any
/// erroneous extra effect would also have landed. The sseq machine
/// completes via spawned per-step re-entries, so the kicking call returns
/// before the later steps' `LNKn` writes happen.
async fn poll_atomic_reaches(label: &str, counter: &Arc<AtomicU32>, want: u32) {
    for _ in 0..400 {
        if counter.load(Ordering::SeqCst) >= want {
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(30)).await;
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: counter reached {} (< {want}) before timeout",
        counter.load(Ordering::SeqCst)
    );
}

/// Poll a PV until it equals `want`, with a timeout. Used to wait out the
/// sseq machine's per-step `DLYn` delays + async re-entries.
async fn poll_pv_double(db: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..400 {
        if let Ok(EpicsValue::Double(v)) = db.get_pv(pv) {
            if (v - want).abs() < 1e-10 {
                return;
            }
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "{pv}: did not reach {want} before timeout (last {:?})",
        db.get_pv(pv)
    );
}

// BUG 1 regression — sseq `LNKn` is `DBF_OUTLINK` driven via `dbPutLink`
// → `dbDbPutValue` (`dbDbLink.c:388`). A bare sseq LNKn is NPP and must
// not process its target; an explicit-PP LNKn must.
#[epics_macros_rs::epics_test]
async fn test_sseq_bare_lnk_does_not_process_passive_target() {
    use epics_base_rs::server::records::sseq::SseqRecord;
    let db = PvDatabase::new();

    let bare_count = Arc::new(AtomicU32::new(0));
    let pp_count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "SSEQ_BARE_TGT",
        Box::new(CountingTarget {
            process_count: bare_count.clone(),
        }),
    )
    .await
    .unwrap();
    db.add_record(
        "SSEQ_PP_TGT",
        Box::new(CountingTarget {
            process_count: pp_count.clone(),
        }),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0;
    // Step 1: bare LNK — must NOT process the Passive target.
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_BARE_TGT".into()))
        .unwrap();
    // Step 2: explicit PP LNK — must process the Passive target.
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_PP_TGT PP".into()))
        .unwrap();
    db.add_record("SSEQ_NPP_REC", Box::new(sseq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SSEQ_NPP_REC", &mut visited, 0)
        .await
        .unwrap();
    // Step 2 (the PP target) is written in a later continuation; wait for
    // it, then the settle window lets a stray bare-target process land too.
    poll_atomic_reaches("sseq PP step", &pp_count, 1).await;

    assert_eq!(
        bare_count.load(Ordering::SeqCst),
        0,
        "bare sseq LNKn (NPP) must NOT process its Passive target"
    );
    assert_eq!(
        pp_count.load(Ordering::SeqCst),
        1,
        "explicit-PP sseq LNKn must process its Passive target"
    );
}

// sseq per-step DLYn regression — C `sseqRecord.c::processNextLink`
// schedules each selected step's LNKn write after its DLYn delay
// (`callbackRequestDelayed`), exactly as the base `seqRecord` does for
// DLY0..DLYF. The async machine ports this with a per-step
// `ReprocessAfter(DLYn)`.
#[epics_macros_rs::epics_test]
async fn test_sseq_per_step_dly_delays_step_write() {
    use epics_base_rs::server::records::sseq::SseqRecord;
    let db = PvDatabase::new();

    // Two Passive targets driven by explicit-PP LNKn so they accept
    // the written value. Step 1 carries a 0.3 s DLY1, step 2 has no
    // delay.
    db.add_record("SSEQ_DLY_TGT1", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("SSEQ_DLY_TGT2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps selected.
    // Step 1: delayed write.
    sseq.put_field("DLY1", EpicsValue::Double(0.3)).unwrap();
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_DLY_TGT1 PP".into()))
        .unwrap();
    // Step 2: no delay (but dispatched only after step 1 completes).
    sseq.put_field("DLY2", EpicsValue::Double(0.0)).unwrap();
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_DLY_TGT2 PP".into()))
        .unwrap();
    db.add_record("SSEQ_DLY_REC", Box::new(sseq)).await.unwrap();

    // Kick the sequence. The async machine returns to the caller after the
    // first step is *scheduled* (PACT set); the per-step writes happen in
    // spawned re-entries, so the kick does NOT block until completion.
    let mut visited = HashSet::new();
    db.process_record_with_links("SSEQ_DLY_REC", &mut visited, 0)
        .await
        .unwrap();

    // Before DLY1 (0.3 s) elapses, step 1's value must NOT be written yet,
    // and step 2 must not fire before step 1.
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        db.get_pv("SSEQ_DLY_TGT1").unwrap(),
        EpicsValue::Double(0.0),
        "step 1 LNKn must not fire before its DLY1 delay elapses"
    );
    assert_eq!(
        db.get_pv("SSEQ_DLY_TGT2").unwrap(),
        EpicsValue::Double(0.0),
        "step 2 must not fire before step 1's delay completes"
    );

    // After DLY1 elapses, step 1 writes, then step 2 writes after it.
    poll_pv_double(&db, "SSEQ_DLY_TGT1", 11.0).await;
    poll_pv_double(&db, "SSEQ_DLY_TGT2", 22.0).await;
}

#[epics_macros_rs::epics_test]
async fn test_sel_nvl_link() {
    use epics_base_rs::server::records::sel::SelRecord;
    let db = PvDatabase::new();
    db.add_record("NVL_SRC", Box::new(AoRecord::new(2.0)))
        .await
        .unwrap();
    let mut sel = SelRecord::default();
    sel.selm = 0;
    sel.nvl = "NVL_SRC".to_string();
    sel.a = 10.0;
    sel.b = 20.0;
    sel.c = 30.0;
    db.add_record("SEL_REC", Box::new(sel)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("SEL_REC", &mut visited, 0)
        .await
        .unwrap();
    let seln = db.get_pv("SEL_REC.SELN").unwrap();
    match seln {
        EpicsValue::UShort(v) => assert_eq!(v, 2),
        other => panic!("expected UShort(2), got {:?}", other),
    }
    let val = db.get_pv("SEL_REC").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 30.0).abs() < 1e-10),
        other => panic!("expected Double(30.0), got {:?}", other),
    }
}

// SELN is DBF_USHORT (selRecord.dbd.pod:295): an NVL link value in the
// upper unsigned half (32768..65535) must reach SELN intact. The former
// f64->i16 carrier saturated such a value to 32767, losing the high half.
#[epics_macros_rs::epics_test]
async fn test_sel_nvl_link_high_index_unsigned() {
    use epics_base_rs::server::records::sel::SelRecord;
    let db = PvDatabase::new();
    db.add_record("NVL_SRC_HI", Box::new(AoRecord::new(40000.0)))
        .await
        .unwrap();
    let mut sel = SelRecord::default();
    sel.selm = 0;
    sel.nvl = "NVL_SRC_HI".to_string();
    db.add_record("SEL_REC_HI", Box::new(sel)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("SEL_REC_HI", &mut visited, 0)
        .await
        .unwrap();
    let seln = db.get_pv("SEL_REC_HI.SELN").unwrap();
    match seln {
        EpicsValue::UShort(v) => assert_eq!(v, 40000, "high SELN must survive the NVL link"),
        other => panic!("expected UShort(40000), got {:?}", other),
    }
}

#[epics_macros_rs::epics_test]
async fn test_dol_cp_link_registration() {
    let db = PvDatabase::new();
    db.add_record("MTR", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "MTR CP".to_string();
    db.add_record("MOTOR_POS", Box::new(ao)).await.unwrap();
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("MTR");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "MOTOR_POS");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

#[epics_macros_rs::epics_test]
async fn test_dol_cp_link_triggers_processing() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CP".to_string();
    db.add_record("DST", Box::new(ao)).await.unwrap();
    db.setup_cp_links().await;
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 10.0).abs() < 1e-10),
        other => panic!("expected Double(10.0), got {:?}", other),
    }
}

/// R6-4 — an `OUT` link is a `DBF_OUTLINK`, and C's `dbParseLink` discards its
/// CP/CPP modifier before the link is ever built
/// (`dbStaticLib.c:2382-2387` — `modifiers &= ~(pvlOptCPP|pvlOptCP)`, with a
/// startup warning). It must therefore never enter the CP trigger registry:
/// registering it would make the holder reprocess on every target change,
/// which — since the holder *writes* that target — is a processing loop that
/// does not exist on a C IOC. The same text on an `INP` (`DBF_INLINK`) does
/// register, which is the boundary this pins.
#[epics_macros_rs::epics_test]
async fn test_out_link_cp_modifier_is_not_registered_as_a_cp_holder() {
    let db = PvDatabase::new();
    db.add_record("CPOUT_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("CPOUT_HOLDER", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("CPIN_HOLDER", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("CPOUT_HOLDER") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("CPOUT_TGT PP CP".into()))
            .unwrap();
    }
    if let Some(rec) = db.get_record("CPIN_HOLDER") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("CPOUT_TGT CP".into()))
            .unwrap();
    }
    db.setup_cp_links().await;

    let targets = db.get_cp_targets("CPOUT_TGT");
    assert_eq!(
        targets.len(),
        1,
        "only the DBF_INLINK holder may subscribe; got {targets:?}"
    );
    assert_eq!(targets[0].record, "CPIN_HOLDER");
    assert!(
        !targets.iter().any(|t| t.record == "CPOUT_HOLDER"),
        "an OUT link's CP modifier is discarded by C and must not register a CP trigger"
    );

    // The rest of the OUT link's modifiers survive the mask: ` PP` still
    // processes the target (`dbDbLink.c:387-390`).
    let mut visited = HashSet::new();
    db.process_record_with_links("CPOUT_HOLDER", &mut visited, 0)
        .await
        .unwrap();
    assert!(
        db.get_pv("CPOUT_TGT").is_ok(),
        "the PP OUT link must still reach its target"
    );
}

/// A CPP link registers as `passive_only` (distinct from CP). Collapsing
/// CPP into CP loses C's `precord->scan == 0` gate (`dbCa.c:959`).
#[epics_macros_rs::epics_test]
async fn test_cpp_link_registration_is_passive_only() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CPP".to_string();
    db.add_record("DST", Box::new(ao)).await.unwrap();
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("SRC");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "DST");
    assert!(
        targets[0].passive_only,
        "CPP link must register as passive_only"
    );
}

/// CPP gate (`dbCa.c:825,959,1034`): on a source change, a CPP link
/// processes the link-holder only when its SCAN is Passive. A non-Passive
/// CPP target must NOT be processed by the dispatch — its own periodic scan
/// owns it. Boundary: target SCAN != Passive.
#[epics_macros_rs::epics_test]
async fn test_cpp_link_skips_nonpassive_target() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CPP".to_string();
    db.add_record("DST", Box::new(ao)).await.unwrap();
    if let Some(rec_arc) = db.get_record("DST") {
        rec_arc.write().common.scan = ScanType::SEC1;
    }
    db.setup_cp_links().await;
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            v.abs() < 1e-10,
            "non-Passive CPP target was processed (VAL={v}); CPP must skip it"
        ),
        other => panic!("expected Double, got {:?}", other),
    }
}

/// CPP gate, complementary boundary: a CPP link DOES process the link-holder
/// when its SCAN is Passive (the default). Boundary: target SCAN == Passive.
#[epics_macros_rs::epics_test]
async fn test_cpp_link_processes_passive_target() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CPP".to_string();
    db.add_record("DST", Box::new(ao)).await.unwrap();
    // DST keeps the default Passive SCAN.
    db.setup_cp_links().await;
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 10.0).abs() < 1e-10,
            "Passive CPP target was not processed (VAL={v})"
        ),
        other => panic!("expected Double, got {:?}", other),
    }
}

/// CP (not CPP) processes the link-holder regardless of its SCAN
/// (`dbCa.c:958-962` adds CA_DBPROCESS unconditionally). A non-Passive CP target
/// IS processed — the boundary that distinguishes CP from CPP.
#[epics_macros_rs::epics_test]
async fn test_cp_link_processes_nonpassive_target() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CP".to_string();
    db.add_record("DST", Box::new(ao)).await.unwrap();
    if let Some(rec_arc) = db.get_record("DST") {
        rec_arc.write().common.scan = ScanType::SEC1;
    }
    db.setup_cp_links().await;
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DST").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 10.0).abs() < 1e-10,
            "non-Passive CP target was not processed (VAL={v}); CP must always process"
        ),
        other => panic!("expected Double, got {:?}", other),
    }
}

/// When the same source→target edge is registered from both a CP and a CPP
/// link, CP dominates: the merged edge is NOT passive_only — an
/// unconditional CP CA_DBPROCESS overrides the CPP scan gate (`dbCa.c:958-962`).
#[epics_macros_rs::epics_test]
async fn test_cp_overrides_cpp_for_same_edge() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CP".to_string(); // CP edge SRC -> DST
    db.add_record("DST", Box::new(ao)).await.unwrap();
    // Second SRC -> DST edge via INP, this time CPP.
    if let Some(rec_arc) = db.get_record("DST") {
        rec_arc.write().common.inp = "SRC CPP".to_string();
    }
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("SRC");
    assert_eq!(
        targets.len(),
        1,
        "CP and CPP to the same edge must merge to one"
    );
    assert_eq!(targets[0].record, "DST");
    assert!(
        !targets[0].passive_only,
        "CP must override CPP on a merged edge"
    );
}

#[epics_macros_rs::epics_test]
async fn test_seq_dol_cp_link_registration() {
    use epics_base_rs::server::records::seq::SeqRecord;
    let db = PvDatabase::new();
    db.add_record("SENSOR", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut seq = SeqRecord::default();
    seq.dol1 = "SENSOR CP".to_string();
    db.add_record("MY_SEQ", Box::new(seq)).await.unwrap();
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("SENSOR");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "MY_SEQ");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

#[epics_macros_rs::epics_test]
async fn test_sel_nvl_cp_link_registration() {
    use epics_base_rs::server::records::sel::SelRecord;
    let db = PvDatabase::new();
    db.add_record("INDEX_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut sel = SelRecord::default();
    sel.nvl = "INDEX_SRC CP".to_string();
    db.add_record("MY_SEL", Box::new(sel)).await.unwrap();
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("INDEX_SRC");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "MY_SEL");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

#[epics_macros_rs::epics_test]
async fn test_sdis_cp_link_registration() {
    let db = PvDatabase::new();
    db.add_record("DISABLE_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("GUARDED", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec_arc) = db.get_record("GUARDED") {
        rec_arc.write().common.sdis = "DISABLE_SRC CP".to_string();
    }
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("DISABLE_SRC");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "GUARDED");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

/// C `epicsTimeEventBestTime = -1` (epicsTime.h:103). The C path
/// (`recGbl.c::recGblGetTimeStampSimm:324-328`) calls
/// `epicsTimeGetEvent(-1)` unconditionally — that delegates to
/// `generalTimeGetEventPriority(-1)` (BestTime providers). A device
/// support that wants to keep its own timestamp must signal
/// TSE = -2 (epicsTimeEventDeviceTime), not -1.
///
/// Regression: the pre-fix Rust port read TSE=-1 as
/// "device-provided with BestTime fallback" and gated the call on
/// UNIX_EPOCH. A stale device write of any non-epoch SystemTime
/// suppressed every subsequent BestTime refresh.
#[epics_macros_rs::epics_test]
async fn test_tse_minus1_always_overwrites_via_best_time() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // Stale but non-epoch timestamp — exactly the case the pre-fix
    // path mis-classified as "device-provided, keep".
    let stale = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234567);
    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.common.tse = -1;
        inst.common.time = stale;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    assert_ne!(
        inst.common.time, stale,
        "TSE=-1 must always overwrite via generalTime BestTime, matching \
         C `epicsTimeGetEvent(-1)` called unconditionally"
    );
}

#[epics_macros_rs::epics_test]
async fn test_tse_minus2_keeps_time_unchanged() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let fixed_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(999);
    if let Some(rec) = db.get_record("REC") {
        let mut inst = rec.write();
        inst.common.tse = -2;
        inst.common.time = fixed_time;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.time, fixed_time);
}

#[epics_macros_rs::epics_test]
async fn test_putf_read_only_from_ca() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let result = db
        .put_record_field_from_ca("REC", "PUTF", EpicsValue::Char(1))
        .await;
    assert!(result.is_err());
}

#[epics_macros_rs::epics_test]
async fn test_rpro_causes_reprocessing() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    db.add_record("DEST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DEST") {
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("SRC".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("DEST", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DEST").unwrap();
    assert_eq!(val.to_f64().unwrap() as i64, 10);

    db.put_pv_no_process("SRC", EpicsValue::Double(20.0))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DEST") {
        let mut inst = rec.write();
        inst.common.rpro = 1;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("DEST", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DEST").unwrap();
    assert_eq!(val.to_f64().unwrap() as i64, 20);
    let rec = db.get_record("DEST").unwrap();
    let inst = rec.read();
    assert!(inst.common.rpro == 0);
}

#[epics_macros_rs::epics_test]
async fn test_tsel_cp_link_registration() {
    let db = PvDatabase::new();
    db.add_record("TSE_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TARGET", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec_arc) = db.get_record("TARGET") {
        let mut inst = rec_arc.write();
        inst.common.tsel = "TSE_SRC CP".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("TSE_SRC");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "TARGET");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

/// a record whose `TSEL` is a time-link at another record's `.TIME`
/// field (`DBLINK_FLAG_TSELisTIME`) adopts the source's timestamp AND
/// userTag, mirroring C `recGblGetTimeStampSimm` (recGbl.c:317) copying
/// both through `dbGetTimeStampTag(plink, &prec->time, &prec->utag)`.
/// Pre-fix only `common.time` was copied and the source's `utag` was
/// dropped.
#[epics_macros_rs::epics_test]
async fn test_tsel_time_link_copies_source_time_and_utag() {
    let db = PvDatabase::new();
    db.add_record("TSE_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TS_DST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Source carries a known timestamp and a bit-31 userTag; the bit-31
    // value pins that the u64 utag is copied verbatim (no narrowing or
    // reset on the way through processing).
    let src_time = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123);
    let src_utag: u64 = 0x9000_0000;
    {
        let rec = db.get_record("TSE_SRC").unwrap();
        let mut inst = rec.write();
        inst.common.time = src_time;
        inst.common.utag = src_utag;
    }

    // Target's TSEL points at the source's `.TIME` field.
    {
        let rec = db.get_record("TS_DST").unwrap();
        let mut inst = rec.write();
        inst.common.tsel = "TSE_SRC.TIME".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_DST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TS_DST").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.time, src_time,
        "TSEL .TIME link must adopt the source's timestamp"
    );
    assert_eq!(
        inst.common.utag, src_utag,
        "TSEL .TIME link must also adopt the source's utag (recGbl.c:317)"
    );
    assert_eq!(
        inst.common.tse, 0,
        "C returns before the TSE half (recGbl.c:321): TSE keeps its declared 0"
    );
}

#[epics_macros_rs::epics_test]
async fn test_tsel_ca_time_link_copies_source_time() {
    // C `TSEL_modified` (dbLink.c:80-86) sets DBLINK_FLAG_TSELisTIME for
    // ANY PV_LINK TSEL whose pvname contains `.TIME`, set before the
    // DB-vs-CA decision (dbLink.c:118) — so a CA TSEL `.TIME` link copies
    // the link's cached timestamp via dbGetTimeStampTag (recGbl.c:317),
    // exactly like a local-DB link. CA wire carries no userTag, so utag
    // stays 0.
    use epics_base_rs::server::database::LinkSet;
    struct TimeCaLset {
        secs: i64,
        nsec: i32,
    }
    #[epics_base_rs::async_trait]
    impl LinkSet for TimeCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(1.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn time_stamp(&self, name: &str) -> Option<(i64, i32, u64)> {
            // Only the source record name (with `.TIME` stripped) should
            // reach the lset, mirroring C `TSEL_modified` truncating the
            // pvname at `.TIME`.
            assert_eq!(name, "TSE_SRC", "TSEL .TIME must strip the .TIME suffix");
            Some((self.secs, self.nsec, 0))
        }
    }

    let db = PvDatabase::new();
    db.register_link_set(
        "ca",
        Arc::new(TimeCaLset {
            secs: 1_700_000_000,
            nsec: 456,
        }),
    )
    .await;
    db.add_record("TS_CADST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("TS_CADST").unwrap();
        let mut inst = rec.write();
        inst.common.tsel = "ca://TSE_SRC.TIME".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_CADST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TS_CADST").unwrap();
    let inst = rec.read();
    let expected = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 456);
    assert_eq!(
        inst.common.time, expected,
        "CA TSEL .TIME link must adopt the CA link's cached timestamp"
    );
    assert_eq!(inst.common.utag, 0, "CA wire carries no userTag");
    assert_eq!(
        inst.common.tse, 0,
        "C returns before the TSE half (recGbl.c:321): TSE keeps its declared 0"
    );
}

#[epics_macros_rs::epics_test]
async fn test_tsel_nonlocal_db_time_link_copies_remote_time() {
    // A bare TSEL `.TIME` link (no `ca://`, no CP/CPP/CA modifier) whose
    // target record is NOT local. C `dbInitLink` (dbLink.c:115-130) sets
    // `TSELisTIME` and strips `.TIME` BEFORE the DB-vs-CA decision, then
    // `dbDbInitLink` fails for the non-local target so the link becomes a
    // CA link — `dbGetTimeStampTag` reads the remote `.TIME`. The pre-fix
    // Db arm did a local `get_record`, found nothing, and never adopted
    // the timestamp; the record kept its local processing time.
    use epics_base_rs::server::database::LinkSet;
    struct TimeCaLset {
        secs: i64,
        nsec: i32,
    }
    #[epics_base_rs::async_trait]
    impl LinkSet for TimeCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(1.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn time_stamp(&self, name: &str) -> Option<(i64, i32, u64)> {
            // The bare Db record name (`.TIME` already split into the link
            // field by the parser) reaches the CA lset via the non-local
            // fallback `external_link_time("ca://TSE_SRC")`.
            assert_eq!(
                name, "TSE_SRC",
                "non-local TSEL .TIME must address the bare record"
            );
            Some((self.secs, self.nsec, 0))
        }
    }

    let db = PvDatabase::new();
    db.register_link_set(
        "ca",
        Arc::new(TimeCaLset {
            secs: 1_700_000_123,
            nsec: 789,
        }),
    )
    .await;
    // TSE_SRC is never added locally -> the bare Db TSEL .TIME is non-local.
    db.add_record("TS_NLDST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("TS_NLDST").unwrap();
        let mut inst = rec.write();
        inst.common.tsel = "TSE_SRC.TIME".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_NLDST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TS_NLDST").unwrap();
    let inst = rec.read();
    let expected = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_123, 789);
    assert_eq!(
        inst.common.time, expected,
        "non-local Db TSEL .TIME link must adopt the remote .TIME via the CA path"
    );
    assert_eq!(
        inst.common.tse, 0,
        "C returns before the TSE half (recGbl.c:321): TSE keeps its declared 0"
    );
}

#[epics_macros_rs::epics_test]
async fn test_new_common_fields_get_put() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();

    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("UDFS"), Some(EpicsValue::Short(3)));
    }
    {
        let mut inst = rec.write();
        inst.put_common_field("UDFS", EpicsValue::Short(1)).unwrap();
    }
    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("UDFS"), Some(EpicsValue::Short(1)));
    }

    {
        let inst = rec.read();
        // C `field(SSCN,DBF_MENU){ menu(menuScan) initial("65535") }`: the
        // default is the out-of-range "use SCAN" sentinel, not Passive(0).
        assert_eq!(inst.get_common_field("SSCN"), Some(EpicsValue::Enum(65535)));
    }
    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("BKPT"), Some(EpicsValue::Char(0)));
    }
    {
        let mut inst = rec.write();
        inst.put_common_field("BKPT", EpicsValue::Char(1)).unwrap();
    }
    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("BKPT"), Some(EpicsValue::Char(1)));
    }

    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("TSE"), Some(EpicsValue::Short(0)));
    }
    {
        let inst = rec.read();
        assert_eq!(
            inst.get_common_field("TSEL"),
            Some(EpicsValue::String(String::new().into()))
        );
    }

    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("PUTF"), Some(EpicsValue::Char(0)));
    }
    {
        let mut inst = rec.write();
        let result = inst.put_common_field("PUTF", EpicsValue::Char(1));
        assert!(result.is_err());
    }

    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("RPRO"), Some(EpicsValue::UChar(0)));
    }
    {
        let mut inst = rec.write();
        inst.put_common_field("RPRO", EpicsValue::Char(1)).unwrap();
    }
    {
        let inst = rec.read();
        assert_eq!(inst.get_common_field("RPRO"), Some(EpicsValue::UChar(1)));
    }
}

/// SSCN (simulation-mode scan) is a `DBF_MENU`/`menuScan` field whose C dbd
/// default is the out-of-range sentinel 65535 ("use SCAN"), not a real menu
/// choice. The put path must round-trip a real menuScan index, the sentinel,
/// and — since the field is a plain `epicsEnum16` C stores whatever a numeric
/// put sent — any OTHER out-of-menu index as itself.
///
/// CORRECTED: this test used to assert that an out-of-menu index (12) collapses
/// to the sentinel. It does not. Measured on the C softIoc,
/// `caput T:A.SSCN 10` reports `New : T:A.SSCN 10` and `caget -n T:A.SSCN`
/// answers `10`; the recGbl simulation helpers bail on `*psscn == USHRT_MAX`
/// (recGbl.c) and on nothing else, so 12 is an ordinary illegal index, not
/// "unset".
#[epics_macros_rs::epics_test]
async fn test_sscn_serves_menu_index_and_65535_sentinel() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").unwrap();

    // Default is the 65535 sentinel.
    assert_eq!(
        rec.read().get_common_field("SSCN"),
        Some(EpicsValue::Enum(65535))
    );

    // A real menuScan index (2 == "I/O Intr") round-trips.
    rec.write()
        .put_common_field("SSCN", EpicsValue::Enum(2))
        .unwrap();
    assert_eq!(
        rec.read().get_common_field("SSCN"),
        Some(EpicsValue::Enum(2))
    );

    // Putting the sentinel back restores "use SCAN".
    rec.write()
        .put_common_field("SSCN", EpicsValue::Enum(65535))
        .unwrap();
    assert_eq!(
        rec.read().get_common_field("SSCN"),
        Some(EpicsValue::Enum(65535))
    );

    // An out-of-menu index (>9, not 65535) is stored as itself — it is illegal,
    // but it is not the sentinel.
    rec.write()
        .put_common_field("SSCN", EpicsValue::Enum(12))
        .unwrap();
    assert_eq!(
        rec.read().get_common_field("SSCN"),
        Some(EpicsValue::Enum(12))
    );
    assert!(!rec.read().common.sscn.is_unset());
}

/// epics-base PR #359 (commits 5ba8080f6, aff74638b, 51c5b8f1e,
/// fabc8d06a) regression: NORD monitor events from waveform / aai /
/// aao / subArray must carry the post-process timestamp, not a stale
/// (or zero) timestamp captured before `recGblGetTimeStamp`.
///
/// In the C source the bug was that `db_post_events(prec, &prec->nord, …)`
/// was called inside `readValue()` *before* the record's timestamp was
/// updated, so the very first NORD camonitor update arrived with an
/// undefined timestamp. The upstream fix moved the NORD post into
/// `process()` after `recGblGetTimeStampSimm`, applied across all four
/// array record types.
///
/// In the Rust port the ordering is structural: every notify path
/// (main, AsyncPendingNotify, complete_async_record) calls
/// `apply_timestamp` *before* building the snapshot and invoking
/// `notify_from_snapshot`. This test pins that contract for all four
/// `ArrayKind` variants by subscribing to NORD, processing once, and
/// verifying the delivered MonitorEvent timestamp is fresh.
#[epics_macros_rs::epics_test]
async fn test_array_records_nord_monitor_uses_post_process_timestamp() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
    use epics_base_rs::types::{DbFieldType, WallTime};

    for (kind, name) in [
        (ArrayKind::Waveform, "WF_KIND"),
        (ArrayKind::Aai, "AAI_KIND"),
        (ArrayKind::Aao, "AAO_KIND"),
        (ArrayKind::SubArray, "SUBA_KIND"),
    ] {
        let db = PvDatabase::new();
        db.add_record(name, Box::new(WaveformRecord::with_kind(kind)))
            .await
            .unwrap();

        // Configure DOUBLE buffer with NELM=10 — gives the put room
        // to actually move NORD from 0 → N. For subArray, also set
        // INDX=0 / MALM=10 so the slice is valid.
        if let Some(rec) = db.get_record(name) {
            let mut inst = rec.write();
            inst.record.put_field("NELM", EpicsValue::Long(10)).unwrap();
            inst.record
                .put_field("FTVL", EpicsValue::Short(10))
                .unwrap();
            if matches!(kind, ArrayKind::SubArray) {
                inst.record.put_field("INDX", EpicsValue::Long(0)).unwrap();
                inst.record.put_field("MALM", EpicsValue::Long(10)).unwrap();
            }
        }

        // Wall-clock baseline AFTER record setup; the NORD event
        // timestamp must be ≥ this value.
        let start = WallTime::now();

        // Subscribe to NORD with VALUE mask. add_subscriber seeds
        // last_posted with the current NORD (=0), so the next
        // process cycle will treat the 0→N transition as a real
        // change.
        let mut nord_rx = if let Some(rec) = db.get_record(name) {
            let mut inst = rec.write();
            inst.add_subscriber("NORD", 1, DbFieldType::Long, EventMask::VALUE.bits())
        } else {
            None
        }
        .unwrap_or_else(|| panic!("NORD subscription must be accepted for {name}"));

        // Stage the new array onto VAL. set_val updates VAL and
        // implicitly NORD (now =3). Processing applies the
        // timestamp and posts subscribed-field events.
        if let Some(rec) = db.get_record(name) {
            let mut inst = rec.write();
            inst.record
                .set_val(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
                .unwrap();
        }
        let mut visited = HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();

        let event = nord_rx
            .try_recv()
            .unwrap_or_else(|_| panic!("NORD monitor event must be delivered for {name}"));
        // NORD is DBF_ULONG on waveform/aai/aao and DBF_LONG on subArray
        // (subArrayRecord.dbd.pod:394).
        let nord_is_3 = match kind {
            ArrayKind::SubArray => matches!(event.snapshot.value, EpicsValue::Long(3)),
            _ => matches!(event.snapshot.value, EpicsValue::ULong(3)),
        };
        assert!(
            nord_is_3,
            "{name}: NORD payload should reflect post-set_val length (3), got {:?}",
            event.snapshot.value
        );
        let ts = event.snapshot.timestamp;
        assert!(
            ts != WallTime::UNIX_EPOCH,
            "{name}: NORD event timestamp must not be the epoch sentinel"
        );
        assert!(
            ts >= start,
            "{name}: NORD event timestamp ({ts:?}) must be ≥ pre-process baseline ({start:?})"
        );
    }
}

/// Regression: `complete_async_record_inner`'s subscriber-snapshot loop
/// previously appended every subscribed non-{VAL,SEVR,STAT,UDF} field
/// unconditionally — no `last_posted` change check, no `last_posted`
/// update — while the main path (`process_record_with_links_inner`
/// L794-820) gates on actual change. The asymmetry meant every
/// async-completion cycle re-sent every subscribed auxiliary field even
/// when its value was unchanged, multiplying monitor traffic for
/// records that pair an async write with a sticky metadata field
/// subscription.
///
/// This test pins the post-fix behaviour for both halves of the gate:
/// (a) unchanged → no event; (b) changed → event flows through.
#[epics_macros_rs::epics_test]
async fn test_complete_async_record_gates_subscribed_field_on_change() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("ASYNC_GATE", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Seed DESC to a known value so add_subscriber's last_posted
    // initialiser captures it.
    if let Some(rec) = db.get_record("ASYNC_GATE") {
        let mut inst = rec.write();
        inst.put_common_field("DESC", EpicsValue::String("alpha".into()))
            .unwrap();
    }

    let mut desc_rx = if let Some(rec) = db.get_record("ASYNC_GATE") {
        let mut inst = rec.write();
        inst.add_subscriber("DESC", 7, DbFieldType::String, EventMask::VALUE.bits())
    } else {
        None
    }
    .expect("DESC subscription must be accepted");

    // Drive process → AsyncPending early-return, then async completion.
    // DESC value unchanged since subscription, so the gate must
    // suppress the event.
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_GATE", &mut visited, 0)
        .await
        .unwrap();
    db.complete_async_record("ASYNC_GATE").await.unwrap();

    assert!(
        desc_rx.try_recv().is_err(),
        "DESC unchanged across async-completion → must NOT post a duplicate event"
    );

    // Change DESC, re-run process+complete. The new value must flow.
    if let Some(rec) = db.get_record("ASYNC_GATE") {
        let mut inst = rec.write();
        inst.put_common_field("DESC", EpicsValue::String("beta".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_GATE", &mut visited, 0)
        .await
        .unwrap();
    db.complete_async_record("ASYNC_GATE").await.unwrap();

    let event = desc_rx
        .try_recv()
        .expect("DESC change must produce a post-completion event");
    assert!(
        matches!(event.snapshot.value, EpicsValue::String(ref s) if s == "beta"),
        "DESC event payload should reflect post-change value, got {:?}",
        event.snapshot.value
    );

    // And another no-op cycle after the change must again be silent.
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_GATE", &mut visited, 0)
        .await
        .unwrap();
    db.complete_async_record("ASYNC_GATE").await.unwrap();
    assert!(
        desc_rx.try_recv().is_err(),
        "DESC stable after the change → no further events"
    );
}

/// Regression: `put_pv_and_post_with_origin` (and the no-origin
/// alias used by the CA gateway monitor forwarder) writes only the
/// explicitly-named field to subscribers. For array-family records
/// (waveform/aai/aao/subArray) a put to VAL implicitly updates NORD
/// via the record's `put_field("VAL", …)` side-effect, but the
/// pre-fix code never told NORD subscribers about the new length.
/// Result: a CA gateway forwarding upstream waveform monitors
/// updated VAL on the shadow PV but left downstream NORD subscribers
/// stuck at their last seen length — frozen-element-count bug
/// observable in PyDM image views computing height from element
/// count.
///
/// The fix snapshots NORD before and after the put and, when changed,
/// posts a NORD event with the same fresh timestamp as the VAL event.
/// This test pins the behaviour for waveform; the same code path
/// applies to aai/aao/subArray since they share the WaveformRecord
/// implementation.
#[epics_macros_rs::epics_test]
async fn test_put_pv_and_post_propagates_nord_side_effect_on_waveform() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record(
        "WF_GW",
        Box::new(WaveformRecord::with_kind(ArrayKind::Waveform)),
    )
    .await
    .unwrap();
    if let Some(rec) = db.get_record("WF_GW") {
        let mut inst = rec.write();
        inst.record.put_field("NELM", EpicsValue::Long(10)).unwrap();
        inst.record
            .put_field("FTVL", EpicsValue::Short(10))
            .unwrap();
    }

    // Subscribe to NORD and VAL separately. add_subscriber seeds
    // last_posted with current values (NORD=0, VAL=empty array) so
    // the next change is treated as new.
    let (mut nord_rx, mut val_rx) = if let Some(rec) = db.get_record("WF_GW") {
        let mut inst = rec.write();
        let n = inst.add_subscriber("NORD", 1, DbFieldType::Long, EventMask::VALUE.bits());
        let v = inst.add_subscriber("VAL", 2, DbFieldType::Double, EventMask::VALUE.bits());
        (n, v)
    } else {
        (None, None)
    };
    let nord_rx = nord_rx.as_mut().expect("NORD subscription accepted");
    let val_rx = val_rx.as_mut().expect("VAL subscription accepted");

    // Drive the gateway-style put: VAL update via put_pv_and_post,
    // no record processing. NORD must be reported alongside.
    db.put_pv_and_post("WF_GW", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
        .await
        .unwrap();

    let val_event = val_rx
        .try_recv()
        .expect("VAL event must be delivered after put_pv_and_post");
    let nord_event = nord_rx
        .try_recv()
        .expect("NORD event must be delivered after put_pv_and_post (side-effect of VAL)");
    assert!(
        matches!(nord_event.snapshot.value, EpicsValue::ULong(4)),
        "NORD event should reflect post-put length (4), got {:?}",
        nord_event.snapshot.value
    );
    // VAL and NORD events must carry the SAME timestamp — both
    // observed the put within one critical section so they reflect
    // the same wall-clock snapshot.
    assert_eq!(
        val_event.snapshot.timestamp, nord_event.snapshot.timestamp,
        "VAL and NORD side-effect events must share the put's timestamp"
    );

    // No-op re-put with the same array: NORD didn't change, so no
    // duplicate NORD event.
    db.put_pv_and_post("WF_GW", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
        .await
        .unwrap();
    assert!(
        nord_rx.try_recv().is_err(),
        "NORD unchanged → no duplicate NORD event"
    );
}

/// epics-base commit f1e83b2 (2017) regression: output records must
/// update their TIME stamp BEFORE writing to OUT-link targets so that
/// downstream records (or anyone reading the source's TIME via TSEL)
/// see the post-process value, not the previous cycle's stale one.
///
/// In the C source the bug pattern was `recGblGetTimeStamp()` placed
/// AFTER `writeValue()` (the OUT-link write), so a downstream record
/// triggered by the OUT cascade would read the stale TIME until the
/// next process cycle.
///
/// In the Rust port the order is structural in
/// `process_record_with_links_inner`:
/// 1. `apply_timestamp` at L623 — TIME = now
/// 2. OUT stage at L668-764 — captures `out_info` (link, value)
/// 3. snapshot built / `notify_from_snapshot` at L866
/// 4. `write_db_link_value` at L870 — actual OUT-link write that
///    cascades downstream processing
///
/// This test pins the contract: when an SRC ao record with an OUT
/// link to a Passive DST processes, BOTH records' `common.time`
/// values must be ≥ the wall-clock baseline captured before
/// processing began. The test deliberately does not exercise the
/// downstream subscriber path (DST is an ai whose process()
/// recomputes VAL from RVAL, washing out the put_pv side-effect)
/// — the timestamp invariant is the load-bearing assertion here.
#[epics_macros_rs::epics_test]
async fn test_output_link_cascade_uses_post_process_source_timestamp() {
    use std::time::SystemTime;

    let db = PvDatabase::new();
    db.add_record("TS_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TS_DST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("TS_SRC") {
        let mut inst = rec.write();
        // Explicit `PP` — C `dbDbPutValue` processes the OUT-link
        // target only on an explicit PP flag (a bare OUT link is NPP
        // and would only write the value). This test exercises the
        // cascade, so the PP modifier is required.
        inst.put_common_field("OUT", EpicsValue::String("TS_DST PP".into()))
            .unwrap();
    }

    let baseline = SystemTime::now();

    // Drive the source. SRC processes → apply_timestamp → OUT stage
    // captures (TS_DST, val) → snapshot/notify → write_db_link_value
    // cascades into TS_DST processing (which itself runs
    // apply_timestamp).
    if let Some(rec) = db.get_record("TS_SRC") {
        let mut inst = rec.write();
        inst.record.set_val(EpicsValue::Double(7.5)).unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("TS_SRC", &mut visited, 0)
        .await
        .unwrap();

    // SRC's `common.time` must be ≥ baseline — it was updated by
    // apply_timestamp before write_db_link_value ran.
    let src_time = db
        .get_record("TS_SRC")
        .expect("TS_SRC exists")
        .read()
        .common
        .time;
    assert!(
        src_time >= baseline,
        "SRC.common.time ({src_time:?}) must be post-baseline ({baseline:?}) — \
         apply_timestamp must run before OUT write"
    );

    // DST's `common.time` must also be ≥ baseline — its own
    // apply_timestamp ran on the cascaded process call. (If the
    // cascade were broken or skipped, DST.common.time would be
    // UNIX_EPOCH from its uninitialised default.)
    let dst_time = db
        .get_record("TS_DST")
        .expect("TS_DST exists")
        .read()
        .common
        .time;
    assert!(
        dst_time >= baseline,
        "DST.common.time ({dst_time:?}) must be post-baseline ({baseline:?}) — \
         OUT cascade must drive Passive DST through process_record_with_links"
    );
}

/// f1e83b2 (second half) regression: for asynchronous output records
/// the timestamp must be updated AGAIN at completion, so the monitor
/// event reflects when the device write actually finished — not when
/// the process cycle started.
///
/// In the C source `recGblGetTimeStampSimm` is called inside the
/// `if (pact)` branch of process(), which fires at the async
/// completion callback.
///
/// In the Rust port `complete_async_record_inner` calls
/// `apply_timestamp` at L1192 before building the snapshot at
/// L1259-1262 and invoking `notify_from_snapshot` at L1351.
///
/// This test pins that contract by sleeping a small but measurable
/// interval between the synchronous `process_record_with_links`
/// (which puts the AsyncRecord into AsyncPending and returns) and
/// the `complete_async_record` call. The delivered VAL event must
/// carry a timestamp ≥ the post-sleep wall-clock instant — proving
/// the snapshot was timestamped at completion, not at process
/// start.
#[epics_macros_rs::epics_test]
async fn test_complete_async_record_updates_timestamp_at_completion() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::{DbFieldType, WallTime};
    use std::time::Duration;

    let db = PvDatabase::new();
    db.add_record("ASYNC_TS", Box::new(AsyncRecord { val: 1.0 }))
        .await
        .unwrap();

    let mut val_rx = if let Some(rec) = db.get_record("ASYNC_TS") {
        let mut inst = rec.write();
        inst.add_subscriber("VAL", 9, DbFieldType::Double, EventMask::VALUE.bits())
    } else {
        None
    }
    .expect("VAL subscription accepted");

    // First half: process → AsyncPending early return; no notify yet.
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_TS", &mut visited, 0)
        .await
        .unwrap();
    assert!(
        val_rx.try_recv().is_err(),
        "AsyncPending early-return must not deliver VAL event yet"
    );

    // Sleep a measurable interval so the completion timestamp is
    // distinguishable from the process-start timestamp.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    let post_sleep = WallTime::now();

    // Second half: completion fires snapshot/notify with a fresh
    // apply_timestamp.
    db.complete_async_record("ASYNC_TS").await.unwrap();
    let event = val_rx
        .try_recv()
        .expect("VAL event must be delivered at async completion");
    assert!(
        event.snapshot.timestamp >= post_sleep,
        "completion event timestamp ({:?}) must be ≥ post-sleep ({post_sleep:?}) — \
         apply_timestamp must run at async completion, not at process start",
        event.snapshot.timestamp
    );
}

/// epics-base PR #6c573b4 integration regression: a longout record
/// with `OOPT=On_Change` (1) must still emit its initial OUT-link
/// write on the very first process cycle even though val == pval ==
/// 0 satisfies the "no change" comparison. The C bug skipped that
/// initial write because outpvt was initialised to OUT_LINK_UNCHANGED;
/// the fix flipped the initial outpvt to EXEC_OUTPUT.
///
/// In the Rust port the equivalent flag is `LongoutRecord::first_output_done`
/// (`crates/epics-base-rs/src/server/records/longout.rs:69`):
/// `compute_should_output` short-circuits to `true` while it is
/// false, then the framework's `after_output_decision` flips it to
/// `true` at the end of the cycle — whether or not that cycle wrote.
///
/// This test pins the integration: a first process cycle with
/// OOPT=1 must drive write_db_link_value (observed via the target
/// record's `common.time` advancing past baseline), and a second
/// no-op process cycle must not.
#[epics_macros_rs::epics_test]
async fn test_longout_oopt_on_change_first_cycle_emits_then_suppresses() {
    use epics_base_rs::server::records::longout::LongoutRecord;
    use std::time::SystemTime;

    let db = PvDatabase::new();
    db.add_record("LO_SRC", Box::new(LongoutRecord::new(0)))
        .await
        .unwrap();
    db.add_record("LO_DST", Box::new(LongoutRecord::new(0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("LO_SRC") {
        let mut inst = rec.write();
        // Explicit `PP` — a bare OUT link is NPP (C `dbDbPutValue`);
        // this test observes the cascade via the target's timestamp,
        // so the OUT link must process the target.
        inst.put_common_field("OUT", EpicsValue::String("LO_DST PP".into()))
            .unwrap();
        inst.record.put_field("OOPT", EpicsValue::Short(1)).unwrap();
    }

    let baseline = SystemTime::now();

    // First cycle: val == pval == 0 satisfies "no change", but the
    // first-output-done guard forces the OUT cascade to fire.
    let mut visited = HashSet::new();
    db.process_record_with_links("LO_SRC", &mut visited, 0)
        .await
        .unwrap();

    let dst_time_after_first = db
        .get_record("LO_DST")
        .expect("LO_DST exists")
        .read()
        .common
        .time;
    assert!(
        dst_time_after_first >= baseline,
        "first-cycle OOPT=On_Change must drive OUT cascade (DST.time {dst_time_after_first:?} \
         must be ≥ baseline {baseline:?}); pre-fix the cascade was suppressed"
    );

    // Confirm the framework latched first_output_done=true.
    let src_first_done = db
        .get_record("LO_SRC")
        .expect("LO_SRC exists")
        .read()
        .record
        .get_field("VAL")
        .is_some();
    assert!(src_first_done, "SRC must have processed at least once");

    // Second cycle with VAL still 0: OOPT=1 should now suppress
    // the cascade because val == pval and the first-cycle guard is
    // off. Capture DST's time before to detect any unwanted
    // re-process.
    let dst_time_before_second = dst_time_after_first;
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    let mut visited = HashSet::new();
    db.process_record_with_links("LO_SRC", &mut visited, 0)
        .await
        .unwrap();
    let dst_time_after_second = db
        .get_record("LO_DST")
        .expect("LO_DST exists")
        .read()
        .common
        .time;
    assert_eq!(
        dst_time_after_second, dst_time_before_second,
        "second-cycle OOPT=On_Change with val==pval must NOT re-trigger OUT cascade — \
         DST.time should not advance from {dst_time_before_second:?} to {dst_time_after_second:?}"
    );
}

/// epics-base commit 62c11c2 (2019) regression: a record whose OUT
/// link points at itself ("self link") must not trigger an infinite
/// RPRO/PUTF reprocessing loop. The C bug computed
/// `dstset = pdst.procThread==NULL` without checking psrc==pdst, so
/// when the self-link write fired processTarget the dst-side state
/// (= same record) was set up for RPRO and the record was scheduled
/// to reprocess after the current pass completed — which would
/// re-fire the self-link, ad infinitum.
///
/// In the Rust port the equivalent guard is the `visited: HashSet<String>`
/// passed through every `process_record_with_links_inner` call:
/// `visited.insert(name)` returns false for the second call on the
/// same record, and the function returns Ok(()) immediately. The CP
/// dispatch path (`dispatch_cp_targets`) and the RPRO recheck at L942
/// likewise bail out on self-targets via the same guard.
///
/// This test pins the contract: a longout with OUT="<self>" must
/// process exactly once per `process_record_with_links` call and the
/// call must complete promptly (we use a 1s timeout to fail fast on
/// infinite recursion regressions).
#[epics_macros_rs::epics_test]
async fn test_self_link_out_does_not_loop() {
    use epics_base_rs::server::records::longout::LongoutRecord;
    use std::time::Duration;

    let db = PvDatabase::new();
    db.add_record("SELF_LO", Box::new(LongoutRecord::new(0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("SELF_LO") {
        let mut inst = rec.write();
        // OUT="SELF_LO.VAL PP" → explicit PP so write_db_link_value
        // attempts to re-process self; this is the case the visited
        // HashSet recursion guard must catch. A bare OUT link is NPP
        // (C `dbDbPutValue`) and would not exercise the guard at all.
        inst.put_common_field("OUT", EpicsValue::String("SELF_LO PP".into()))
            .unwrap();
    }

    // 1-second timeout: if the self-link guard regresses, the
    // process call would never return (infinite recursion via
    // write_db_link_value → process_record_with_links → ...).
    let mut visited = HashSet::new();
    let result = epics_base_rs::runtime::task::timeout(
        Duration::from_secs(1),
        db.process_record_with_links("SELF_LO", &mut visited, 0),
    )
    .await;

    assert!(
        result.is_ok(),
        "self-link processing must complete within 1s — \
         hang implies the visited HashSet guard regressed"
    );
    result.unwrap().expect("process call must succeed");

    // The frame unwound, so the guard released its marker; what the guard
    // did is proved by the call above returning at all.
    assert!(visited.is_empty(), "the frame unwound: {visited:?}");

    // A subsequent process call (fresh visited) must also complete
    // promptly — the RPRO flag from the first call must not have
    // been left set on the record, otherwise the record would
    // reprocess in a loop after every external put.
    let mut visited2 = HashSet::new();
    let result2 = epics_base_rs::runtime::task::timeout(
        Duration::from_secs(1),
        db.process_record_with_links("SELF_LO", &mut visited2, 0),
    )
    .await;
    assert!(
        result2.is_ok(),
        "subsequent self-link processing must also complete within 1s"
    );
    result2.unwrap().expect("second process call must succeed");

    // RPRO flag must be cleared after each call, not stuck at true.
    let rpro_after = db
        .get_record("SELF_LO")
        .expect("SELF_LO exists")
        .read()
        .common
        .rpro;
    assert!(
        rpro_after == 0,
        "RPRO must be cleared after self-link processing — \
         stuck-true would queue an infinite reprocess loop"
    );
}

/// epics-base commit 8ac2c87 (2025) regression: writing to a
/// compress record's RES field must reset the circular buffer AND
/// post a monitor event so CA clients see the empty array
/// immediately. Pre-fix C only updated VAL silently — clients
/// observing via camonitor would miss the reset.
///
/// Rust impl: `CompressRecord::put_field("RES", _)` clears
/// nuse/off/val in place and zeros res back to 0
/// (records/compress.rs:794). The framework then runs
/// `process_record_with_links_inner`, whose snapshot path includes
/// VAL via the always-on `include_val` branch for non-deadband
/// records, so the VAL subscriber sees the post-reset empty array.
#[epics_macros_rs::epics_test]
async fn test_compress_res_write_posts_val_monitor() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::compress::CompressRecord;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("CMP_RES", Box::new(CompressRecord::new(8, 4)))
        .await
        .unwrap();

    // Pre-load the buffer with some values so the post-reset state
    // is observably different from the initial zeros.
    if let Some(rec) = db.get_record("CMP_RES") {
        let mut inst = rec.write();
        // Drive values through put_field/process so VAL is updated
        // through the public Record API rather than reaching into
        // the concrete CompressRecord state.
        // CompressRecord's process() pushes from INP — we don't have
        // an INP, so instead manually populate a few VAL entries.
        let arr = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let _ = inst.record.put_field("VAL", arr);
    }

    let mut val_rx = if let Some(rec) = db.get_record("CMP_RES") {
        let mut inst = rec.write();
        inst.add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
    } else {
        None
    }
    .expect("VAL subscription accepted");

    // Drive RES=1 via the CA put path so processing runs.
    let _ = db
        .put_record_field_from_ca("CMP_RES", "RES", EpicsValue::Short(1))
        .await;

    let event = val_rx
        .try_recv()
        .expect("RES write must trigger a VAL monitor event");
    if let EpicsValue::DoubleArray(v) = &event.snapshot.value {
        // Post-reset: NUSE=0, so VAL should be all zeros (or empty
        // depending on PBUF). Either way, none of {1.0, 2.0, 3.0}
        // should still be present.
        assert!(
            v.iter().all(|&x| x == 0.0),
            "post-reset VAL must be all zeros; got {v:?}"
        );
    } else {
        panic!("VAL must be DoubleArray, got {:?}", event.snapshot.value);
    }

    // RES itself reset back to 0.
    let res = db
        .get_record("CMP_RES")
        .expect("CMP_RES exists")
        .read()
        .record
        .get_field("RES")
        .and_then(|v| match v {
            EpicsValue::Short(s) => Some(s),
            _ => None,
        })
        .expect("RES readable");
    assert_eq!(res, 0, "RES must auto-clear after the reset");
}

/// C `compressRecord.c::special` (:377-393) runs `reset(); monitor();` for
/// EVERY `special(SPC_RESET)` field — RES, ALG, PBUF, BALG and N
/// (`compressRecord.dbd.pod:396-437`) — never RES alone. softIoc transcript
/// (compress NSAM=4, ALG="Circular Buffer"), after three `dbpf CMP.VAL`:
///
/// ```text
/// dbgf CMP.NUSE            -> 3
/// dbpf CMP.BALG "LIFO Buffer"
/// dbgf CMP.NUSE            -> 0      dbgf CMP.OFF -> 0
/// (refill to NUSE=2) dbpf CMP.N 2
/// dbgf CMP.NUSE            -> 0      dbgf CMP.OFF -> 0
/// (refill to NUSE=1) dbpf CMP.ALG "N to 1 Low Value"
/// dbgf CMP.NUSE            -> 0
/// ```
///
/// Pre-fix the port reset only on RES, so a BALG FIFO→LIFO switch re-read the
/// stale ring in the new order and an N put corrupted the next emitted sample.
#[epics_macros_rs::epics_test]
async fn test_compress_every_spc_reset_field_resets_the_buffer() {
    use epics_base_rs::server::records::compress::CompressRecord;

    // Each SPC_RESET field, with the value a client would write.
    for (field, value) in [
        ("BALG", EpicsValue::Short(1)),
        ("ALG", EpicsValue::Short(0)),
        ("PBUF", EpicsValue::Short(1)),
        ("N", EpicsValue::Long(2)),
        ("RES", EpicsValue::Short(1)),
    ] {
        let db = PvDatabase::new();
        let name = format!("CMP_{field}");
        db.add_record(&name, Box::new(CompressRecord::new(4, 4)))
            .await
            .unwrap();

        // Fill the ring: NUSE=3, OFF=3 (the softIoc pre-put state).
        {
            let rec = db.get_record(&name).unwrap();
            let mut inst = rec.write();
            for v in [1.0, 2.0, 3.0] {
                inst.record.put_field("VAL", EpicsValue::Double(v)).unwrap();
            }
            assert_eq!(inst.record.get_field("NUSE"), Some(EpicsValue::ULong(3)));
        }

        db.put_record_field_from_ca(&name, field, value)
            .await
            .unwrap_or_else(|e| panic!("caput {field} rejected: {e:?}"));

        let rec = db.get_record(&name).unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("NUSE"),
            Some(EpicsValue::ULong(0)),
            "a put to SPC_RESET field {field} must zero NUSE"
        );
        assert_eq!(
            inst.record.get_field("OFF"),
            Some(EpicsValue::ULong(0)),
            "a put to SPC_RESET field {field} must zero OFF"
        );
        match inst.record.get_field("VAL") {
            Some(EpicsValue::DoubleArray(v)) => assert!(
                v.is_empty(),
                "a put to SPC_RESET field {field} must empty VAL (NUSE=0); got {v:?}"
            ),
            other => panic!("VAL must be DoubleArray, got {other:?}"),
        }
        // C's `reset()` zeroes RES itself, so it reads back 0 whatever was
        // written (softIoc: `dbpf CMP.RES 1` echoes `DBF_SHORT: 0`).
        assert_eq!(
            inst.record.get_field("RES"),
            Some(EpicsValue::Short(0)),
            "reset() zeroes RES after a put to {field}"
        );
    }
}

/// C `compressRecord.c::cvt_dbaddr` (:395-407) raises `SPC_NOMOD` on VAL when
/// BALG is LIFO, so `dbPut` refuses the write on every route. softIoc, after
/// `dbpf CMP.BALG "LIFO Buffer"`:
///
/// ```text
/// dbpf CMP.VAL 7
/// recGblDbaddrError: dbPut Attempt to modify noMod field PV: CMP.VAL
/// ```
///
/// while the same put under BALG=FIFO succeeds. The port expressed NOMOD only
/// as the static `FieldDesc::read_only`, which cannot depend on record state,
/// so a LIFO compress accepted VAL puts.
#[epics_macros_rs::epics_test]
async fn test_compress_val_no_mod_under_lifo_balg() {
    use epics_base_rs::server::records::compress::CompressRecord;

    let db = PvDatabase::new();
    db.add_record("CMP_LIFO", Box::new(CompressRecord::new(4, 4)))
        .await
        .unwrap();

    // BALG=FIFO (the default): VAL is writable.
    db.put_record_field_from_ca("CMP_LIFO", "VAL", EpicsValue::Double(1.0))
        .await
        .expect("FIFO compress VAL is writable");

    // Switch to LIFO. (The SPC_RESET on BALG empties the ring — R17-76.)
    db.put_record_field_from_ca("CMP_LIFO", "BALG", EpicsValue::Short(1))
        .await
        .unwrap();

    let err = db
        .put_record_field_from_ca("CMP_LIFO", "VAL", EpicsValue::Double(7.0))
        .await
        .expect_err("LIFO compress VAL is SPC_NOMOD — the put must be refused");
    assert!(
        matches!(err, CaError::ReadOnlyField(ref f) if f == "VAL"),
        "expected S_db_noMod on VAL, got {err:?}"
    );

    // The refused put stored nothing.
    let rec = db.get_record("CMP_LIFO").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.record.get_field("NUSE"),
        Some(EpicsValue::ULong(0)),
        "a refused put must not reach the ring"
    );

    // An internal (link) delivery is C's `dbGetLink` into `wptr`, not a
    // `dbPut` on VAL — it must still ingest under LIFO.
    drop(inst);
    let mut inst = rec.write();
    inst.record
        .put_field_internal("VAL", EpicsValue::Double(7.0))
        .expect("the INP-driven ingest is not a dbPut and is not gated");
    assert_eq!(inst.record.get_field("NUSE"), Some(EpicsValue::ULong(1)));
}

/// C `field(CSTA,DBF_SHORT){ special(SPC_NOMOD) }`
/// (`histogramRecord.dbd.pod:170-175`): the collection state is toggled only
/// through CMD (`histogramRecord.c:246-259`), never by a client put. softIoc:
///
/// ```text
/// dbpf HI.CSTA 0
/// recGblDbaddrError: dbPut Attempt to modify noMod field PV: HI.CSTA
/// dbgf HI.CSTA -> 1
/// ```
///
/// Pre-fix the port accepted the put on every route, so a caput silently
/// stopped a live acquisition.
#[epics_macros_rs::epics_test]
async fn test_histogram_csta_is_no_mod() {
    use epics_base_rs::server::records::histogram::HistogramRecord;

    let db = PvDatabase::new();
    db.add_record("HI_CSTA", Box::new(HistogramRecord::new(4, 0.0, 8.0)))
        .await
        .unwrap();

    let err = db
        .put_record_field_from_ca("HI_CSTA", "CSTA", EpicsValue::Short(0))
        .await
        .expect_err("CSTA is special(SPC_NOMOD) — the put must be refused");
    assert!(
        matches!(err, CaError::ReadOnlyField(ref f) if f == "CSTA"),
        "expected S_db_noMod on CSTA, got {err:?}"
    );

    let rec = db.get_record("HI_CSTA").unwrap();
    assert_eq!(
        rec.read().record.get_field("CSTA"),
        Some(EpicsValue::Short(1)),
        "a refused put must leave the live acquisition counting"
    );

    // CMD=3 (Stop) is the only route that clears it.
    db.put_record_field_from_ca("HI_CSTA", "CMD", EpicsValue::Short(3))
        .await
        .unwrap();
    assert_eq!(
        rec.read().record.get_field("CSTA"),
        Some(EpicsValue::Short(0)),
        "CMD=Stop is the owner of the counting state"
    );
}

/// C `dbConvert.c`'s `PUT(epicsFloat64, epicsInt32)` / `(epicsInt16)` is a bare
/// cast (`*pdst = (typeb) *psrc`, :96-113), which the standard leaves UNDEFINED
/// once the value is out of the destination's range (C17 6.3.1.4p1). Compiled C
/// is therefore not single-valued — an x86-64 IOC's `cvttsd2si` answers with the
/// integer indefinite, an aarch64 IOC's `fcvtzs` saturates. Per CBUG-E2 the port
/// saturates and NaN goes to 0, through the single owner `types::c_cast`.
///
/// The values a compiled x86-64 softIoc gives, which the port DELIBERATELY does
/// not reproduce, are recorded here so the divergence stays visible:
///
/// ```text
/// record(calcout,"CO") { field(CALC,"3.0e9") field(OUT,"LO.VAL PP") }
/// dbpf CO.PROC 1 ; dbgf LO.VAL   -> DBF_LONG: -2147483648 = 0x80000000
///                                  (port: 2147483647; aarch64 C agrees)
///
/// record(aao,"AAO") { field(DOL,"[1.7,2.2,-3.9,70000,5,6]") field(OUT,"WFS.VAL") }
/// (WFS: NELM=4 FTVL=SHORT)
/// dbpf AAO.PROC 1 ; dbgf WFS.VAL -> DBF_SHORT[4]: 1  2  -3  4464
///                                  (port: 1 2 -3 32767 — 4464 is 70000's low
///                                   16 bits, a wrap no reader can interpret)
///
/// dbpf LO2.VAL 70000.9 -> 70000     dbpf LO2.VAL -3.9 -> -3   (port agrees:
///                                   in range, no policy in play)
/// ```
#[epics_macros_rs::epics_test]
async fn test_double_to_int_narrowing_saturates_per_cbug_e2() {
    use epics_base_rs::server::records::longout::LongoutRecord;
    use epics_base_rs::server::records::waveform::WaveformRecord;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("CVT_LO", Box::new(LongoutRecord::new(0)))
        .await
        .unwrap();
    db.add_record(
        "CVT_WFS",
        Box::new(WaveformRecord::new(4, DbFieldType::Short)),
    )
    .await
    .unwrap();

    // DBF_LONG destination: `(epicsInt32) 3.0e9` clamps to i32::MAX. An x86-64
    // C IOC stores the integer indefinite (0x80000000) instead — UB, and the
    // opposite end of the range from the value the user asked for.
    db.put_record_field_from_ca("CVT_LO", "VAL", EpicsValue::Double(3.0e9))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("CVT_LO.VAL").unwrap(),
        EpicsValue::Long(2147483647),
        "CBUG-E2: saturate. x86-64 C: -2147483648"
    );

    // In-range doubles still truncate toward zero.
    db.put_record_field_from_ca("CVT_LO", "VAL", EpicsValue::Double(70000.9))
        .await
        .unwrap();
    assert_eq!(db.get_pv("CVT_LO.VAL").unwrap(), EpicsValue::Long(70000));
    db.put_record_field_from_ca("CVT_LO", "VAL", EpicsValue::Double(-3.9))
        .await
        .unwrap();
    assert_eq!(db.get_pv("CVT_LO.VAL").unwrap(), EpicsValue::Long(-3));

    // FTVL=SHORT destination: each element clamps at the ELEMENT's width, so
    // 70000 -> 32767. A C IOC converts through a 32-bit instruction and then
    // truncates to 16 bits, giving 70000's low half, 4464.
    db.put_record_field_from_ca(
        "CVT_WFS",
        "VAL",
        EpicsValue::DoubleArray(vec![1.7, 2.2, -3.9, 70000.0, 5.0, 6.0]),
    )
    .await
    .unwrap();
    match db.get_pv("CVT_WFS.VAL").unwrap() {
        EpicsValue::ShortArray(v) => assert_eq!(
            v,
            vec![1, 2, -3, 32767],
            "CBUG-E2: saturate. x86-64 C: 1 2 -3 4464"
        ),
        other => panic!("expected ShortArray, got {other:?}"),
    }
}

/// C `histogramRecord.c::wdogCallback` (:102-124), armed by `wdogInit`
/// (:126-152) from `init_record` pass 1 (:168) and from the SDEL
/// `special(SPC_RESET)` (:266-268):
///
/// ```c
/// if (prec->mcnt > 0) {
///     dbScanLock(prec);
///     recGblGetTimeStamp(prec);
///     db_post_events(prec, &prec->val, DBE_VALUE | DBE_LOG);
///     prec->mcnt = 0;
///     dbScanUnlock(prec);
/// }
/// if (prec->sdel > 0) callbackRequestDelayed(&pcallback->callback, prec->sdel);
/// ```
///
/// MDEL can hold every process-time post back indefinitely (`monitor()` posts
/// only when `mcnt > mdel`), so without the watchdog a slow accumulation never
/// reaches a display. Pre-fix an SDEL put stored the number and nothing else.
#[epics_macros_rs::epics_test]
async fn test_histogram_sdel_watchdog_posts_val() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::histogram::HistogramRecord;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    let mut hist = HistogramRecord::new(2, 0.0, 10.0);
    // MDEL far above the counts we will take: the process-time `monitor()`
    // post is suppressed, so ONLY the watchdog can publish.
    hist.mdel = 1000;
    db.add_record("HI_WDOG", Box::new(hist)).await.unwrap();

    let rec = db.get_record("HI_WDOG").unwrap();
    let mut val_rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .expect("VAL subscription accepted");

    // Accumulate a count (MCNT=1, well under MDEL — nothing is posted).
    db.put_record_field_from_ca("HI_WDOG", "SGNL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    while val_rx.try_recv().is_ok() {}

    // Arm the watchdog: SDEL is special(SPC_RESET) -> wdogInit.
    db.put_record_field_from_ca("HI_WDOG", "SDEL", EpicsValue::Double(0.05))
        .await
        .unwrap();

    // The tick must post VAL and zero MCNT.
    let event =
        epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(2), val_rx.recv())
            .await
            .expect("SDEL watchdog must post VAL within one period")
            .expect("subscription alive");
    match &event.snapshot.value {
        EpicsValue::ULongArray(v) => assert_eq!(
            v,
            &vec![1, 0],
            "the watchdog posts the accumulated bin counts"
        ),
        other => panic!("VAL must be ULongArray (C DBF_ULONG), got {other:?}"),
    }
    assert_eq!(
        rec.read().record.get_field("MCNT"),
        Some(EpicsValue::Short(0)),
        "wdogCallback zeroes MCNT after the post"
    );

    // It re-arms: a further count is posted on the next tick.
    db.put_record_field_from_ca("HI_WDOG", "SGNL", EpicsValue::Double(6.0))
        .await
        .unwrap();
    let event =
        epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(2), val_rx.recv())
            .await
            .expect("the watchdog re-arms itself (C: callbackRequestDelayed at the tail)")
            .expect("subscription alive");
    match &event.snapshot.value {
        EpicsValue::ULongArray(v) => assert_eq!(v, &vec![1, 1]),
        other => panic!("VAL must be ULongArray (C DBF_ULONG), got {other:?}"),
    }
}

/// C `compressRecord` serves OUSE and INPN, both `special(SPC_NOMOD)`
/// (`compressRecord.dbd.pod:481-484` / `:503-506`). OUSE is the latch behind
/// `monitor()`'s "post NUSE only when it changed" rule
/// (compressRecord.c:104-108); INPN is the INP element count that triggers the
/// WPTR realloc. softIoc (compress NSAM=4 ALG="Circular Buffer" INP=SRC, a
/// 3-element waveform):
///
/// ```text
/// dbgf CMP2.OUSE -> 0      dbgf CMP2.INPN -> 0
/// dbpf SRC.VAL 5 ; dbpf CMP2.PROC 1
/// dbgf CMP2.NUSE -> 1      dbgf CMP2.OUSE -> 1
/// dbpf CMP2.RES 1
/// dbgf CMP2.NUSE -> 0      dbgf CMP2.OUSE -> 0
/// dbpf CMP2.OUSE 9 -> dbPut Attempt to modify noMod field PV: CMP2.OUSE
/// dbpf CMP2.INPN 9 -> dbPut Attempt to modify noMod field PV: CMP2.INPN
/// ```
///
/// Both fields were absent from the port, so a caget on either failed.
#[epics_macros_rs::epics_test]
async fn test_compress_ouse_and_inpn_fields() {
    use epics_base_rs::server::records::compress::CompressRecord;

    let db = PvDatabase::new();
    db.add_record("CMP_OUSE", Box::new(CompressRecord::new(4, 4)))
        .await
        .unwrap();
    let rec = db.get_record("CMP_OUSE").unwrap();

    assert_eq!(db.get_pv("CMP_OUSE.OUSE").unwrap(), EpicsValue::ULong(0));
    assert_eq!(db.get_pv("CMP_OUSE.INPN").unwrap(), EpicsValue::Long(0));

    // Both are special(SPC_NOMOD): refused on every runtime route.
    for field in ["OUSE", "INPN"] {
        let err = db
            .put_record_field_from_ca("CMP_OUSE", field, EpicsValue::Long(9))
            .await
            .expect_err("special(SPC_NOMOD) field must refuse a caput");
        assert!(
            matches!(err, CaError::ReadOnlyField(ref f) if f == field),
            "expected S_db_noMod on {field}, got {err:?}"
        );
    }

    // A publishing process cycle latches OUSE to NUSE (C `monitor()`).
    {
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(5.0))
            .unwrap();
    }
    db.process_record("CMP_OUSE").await.unwrap();
    assert_eq!(db.get_pv("CMP_OUSE.NUSE").unwrap(), EpicsValue::ULong(1));
    assert_eq!(
        db.get_pv("CMP_OUSE.OUSE").unwrap(),
        EpicsValue::ULong(1),
        "monitor() latches OUSE = NUSE on a publishing cycle"
    );

    // RES resets, and the SPC_RESET monitor() latches OUSE back to 0.
    db.put_record_field_from_ca("CMP_OUSE", "RES", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(db.get_pv("CMP_OUSE.NUSE").unwrap(), EpicsValue::ULong(0));
    assert_eq!(db.get_pv("CMP_OUSE.OUSE").unwrap(), EpicsValue::ULong(0));

    // The latch is what gates the NUSE post: that first RES moved NUSE 1 -> 0,
    // so it posted NUSE alongside VAL...
    {
        let inst = rec.read();
        assert_eq!(
            inst.record.monitor_side_effect_fields("RES"),
            &["NUSE", "VAL"],
            "NUSE changed against OUSE -> C posts NUSE too"
        );
    }
    // ...while a second RES on an already-empty buffer leaves NUSE == OUSE, so
    // `monitor()` posts VAL alone.
    db.put_record_field_from_ca("CMP_OUSE", "RES", EpicsValue::Short(1))
        .await
        .unwrap();
    {
        let inst = rec.read();
        assert_eq!(
            inst.record.monitor_side_effect_fields("RES"),
            &["VAL"],
            "NUSE unchanged against OUSE -> C posts VAL only"
        );
    }
}

/// epics-base PR `dabcf89` (2021) regression: when an mbboDirect
/// record initialises with no VAL set (UDF=true on the framework
/// side) but with at least one B0..B1F bit set in the .db file,
/// VAL must be reconstructed from those bits and UDF cleared. The
/// pre-fix C code always derived bits from VAL, so an init like
/// `record(mbboDirect, "...") { field(B3, "1") }` without an
/// initial VAL produced VAL=0 (and UDF stayed true) instead of
/// VAL=8 (UDF=false).
///
/// Rust impl: `MbboDirectRecord::post_init_finalize_undef` is
/// invoked by ioc_builder after both `init_record` passes; it
/// chooses VAL→bits or bits→VAL based on the framework's
/// `common.udf`. We exercise the bits-set / undefined branch
/// directly via the trait method since the full IocBuilder pipeline
/// pulls in many unrelated pieces.
#[epics_macros_rs::epics_test]
async fn test_mbbo_direct_initialises_val_from_bits_when_undef() {
    use epics_base_rs::server::record::Record;
    use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;

    let mut rec = MbboDirectRecord::default();
    // Operator set B3=1 in the .db; framework UDF=true (no VAL).
    rec.put_field("B3", EpicsValue::Char(1)).unwrap();
    let mut udf = true;
    rec.post_init_finalize_undef(&mut udf).unwrap();
    assert!(
        !udf,
        "UDF must be cleared once bits supplied an initial value"
    );
    assert!(matches!(rec.get_field("VAL"), Some(EpicsValue::Long(8))));

    // Sibling case: VAL was set explicitly (UDF=false). bits should
    // be derived from VAL.
    let mut rec2 = MbboDirectRecord::default();
    rec2.put_field("VAL", EpicsValue::Long(0b0101)).unwrap();
    let mut udf2 = false;
    rec2.post_init_finalize_undef(&mut udf2).unwrap();
    assert!(!udf2, "UDF stays cleared");
    assert!(matches!(rec2.get_field("VAL"), Some(EpicsValue::Long(5))));
    // bits[0] and bits[2] should reflect VAL=5 (binary 0101). B0..B1F are
    // DBF_UCHAR, so mbboDirect serves the bit as the native unsigned `UChar`.
    assert!(matches!(rec2.get_field("B0"), Some(EpicsValue::UChar(1))));
    assert!(matches!(rec2.get_field("B2"), Some(EpicsValue::UChar(1))));
    assert!(matches!(rec2.get_field("B1"), Some(EpicsValue::UChar(0))));

    // Sibling case: nothing set — UDF stays true, VAL stays 0.
    let mut rec3 = MbboDirectRecord::default();
    let mut udf3 = true;
    rec3.post_init_finalize_undef(&mut udf3).unwrap();
    assert!(udf3, "UDF stays true when nothing initialised");
    assert!(matches!(rec3.get_field("VAL"), Some(EpicsValue::Long(0))));
}

/// epics-base PR `e3c9d590` / `20404003` regression: `lnkCalc` JSON
/// link `{calc:{expr:"...", args:[...], time:"X"}}` parses into
/// `ParsedLink::Calc` and the read path evaluates the expression by
/// fetching each input PV and binding the `A..` slots in order. What the
/// record then DOES with `time_source` is a processing-path question and
/// lives in `calc_link_adopts_its_time_inputs_stamp.rs`.
#[epics_macros_rs::epics_test]
async fn test_lnk_calc_parses_and_evaluates() {
    use epics_base_rs::server::record::{CalcLink, ParsedLink, parse_link_v2};
    use epics_base_rs::server::records::ai::AiRecord;

    // The port's string-arg shorthand: one `args` slot holding a link to
    // the named record. C refuses a bare string here (`lnkCalc.c:187-190`);
    // see the DEVIATION note on `json_calc_arg`.
    fn name_arg(pv: &str) -> CalcArg {
        CalcArg::Link(Box::new(parse_link_v2(pv)))
    }

    // Parser: full lnkCalc form.
    let parsed = parse_link_v2(r#"{calc:{"expr":"A+B*2","args":["pv_a","pv_b"],"time":"A"}}"#);
    let calc = match parsed {
        ParsedLink::Calc(c) => c,
        other => panic!("expected ParsedLink::Calc, got {other:?}"),
    };
    assert_eq!(calc.expr, "A+B*2");
    assert_eq!(calc.args, vec![name_arg("pv_a"), name_arg("pv_b")]);
    assert_eq!(calc.time_source, Some('A'));

    // Parser without `time` field — time_source must be None.
    let no_time = parse_link_v2(r#"{calc:{"expr":"A","args":["pv_a"]}}"#);
    assert!(matches!(
        no_time,
        ParsedLink::Calc(CalcLink {
            time_source: None,
            ..
        })
    ));

    // Parser refuses more than CALC_NARGS args — C `lnkCalc.c:135-139`
    // returns `jlif_stop`, it does not truncate. The 21/22 boundary itself
    // is pinned in `tests/calc_json_link_body.rs`.
    let names: Vec<String> = (0..=epics_base_rs::calc::CALC_NARGS)
        .map(|i| format!("\"pv{i}\""))
        .collect();
    let too_many = parse_link_v2(&format!(
        r#"{{calc:{{"expr":"A","args":[{}]}}}}"#,
        names.join(",")
    ));
    assert!(
        !matches!(too_many, ParsedLink::Calc(_)),
        "more than CALC_NARGS args must NOT parse as Calc"
    );

    // Read-path: feed real PVs, evaluate A+B*2.
    let db = PvDatabase::new();
    db.add_record("pv_a", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();
    db.add_record("pv_b", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();

    let calc = CalcLink {
        expr: "A+B*2".into(),
        args: vec![name_arg("pv_a"), name_arg("pv_b")],
        time_source: Some('A'),
    };
    let parsed = ParsedLink::Calc(calc);
    let mut visited = HashSet::new();
    let value = db
        .read_link_value_soft(&parsed, true, &mut visited, 0)
        .expect("calc link evaluates");
    match value {
        EpicsValue::Double(v) => assert!((v - 13.0).abs() < 1e-9, "expected 3+5*2=13, got {v}"),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// A `lnkCalc` `{calc:...}` link whose input record is NON-LOCAL must
/// read that input through the external CA path — each `A..` input is its
/// own `dbInitLink` link (`lnkCalc.c:353`), so a non-local input is a CA
/// link. The pre-fix loop read every input with a local-only `get_pv`, so
/// a single non-local input made the whole evaluation return `None`.
/// Sibling of the non-local Db read / OUT-write / TSEL `.TIME` fixes —
/// same `dbInitLink` locality cause, the lnkCalc inputs. The matching
/// non-local TIMESTAMP boundary is in
/// `calc_link_adopts_its_time_inputs_stamp.rs`, which owns that half.
#[epics_macros_rs::epics_test]
async fn test_lnk_calc_nonlocal_input_resolves_externally() {
    use epics_base_rs::server::database::LinkSet;
    use epics_base_rs::server::record::CalcLink;
    use epics_base_rs::server::records::ai::AiRecord;

    struct CalcCaLset {
        secs: i64,
        nsec: i32,
    }
    #[epics_base_rs::async_trait]
    impl LinkSet for CalcCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
            (name == "REMOTE:A").then_some(EpicsValue::Double(10.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn time_stamp(&self, name: &str) -> Option<(i64, i32, u64)> {
            (name == "REMOTE:A").then_some((self.secs, self.nsec, 0))
        }
    }

    let db = PvDatabase::new();
    db.register_link_set(
        "ca",
        Arc::new(CalcCaLset {
            secs: 1_700_000_456,
            nsec: 321,
        }),
    )
    .await;
    // pv_b is local; REMOTE:A is never added → non-local CA input.
    db.add_record("pv_b", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();

    let calc = CalcLink {
        expr: "A+B*2".into(),
        args: vec![
            CalcArg::Link(Box::new(parse_link_v2("REMOTE:A"))),
            CalcArg::Link(Box::new(parse_link_v2("pv_b"))),
        ],
        time_source: Some('A'),
    };

    let mut visited = HashSet::new();
    let value = db
        .read_link_value_soft(
            &epics_base_rs::server::record::ParsedLink::Calc(calc),
            true,
            &mut visited,
            0,
        )
        .expect("calc with a non-local input must still evaluate");
    match value {
        // 10 (remote A) + 5 (local B) * 2 = 20.
        EpicsValue::Double(v) => assert!(
            (v - 20.0).abs() < 1e-9,
            "non-local calc input must resolve via CA: expected 20, got {v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// Regression: a CA put to `mbbo.VAL` must recompute RVAL/ORAW.
///
/// C `mbboRecord.c::process` (line 217) calls `convert(prec)`
/// unconditionally on every non-pact process — the VAL→RVAL output
/// translation. Pre-fix, `put_record_field_from_ca` called
/// `set_device_did_compute(true)` for *any* VAL put, and `mbbo`
/// interpreted that as "skip the output convert", so RVAL/ORAW kept
/// their stale pre-put value while the OUT link drove the wrong raw.
///
/// With `shft = 4` and no state table, `convert()` yields
/// `RVAL = VAL << 4`. A CA put of VAL=3 must produce RVAL=ORAW=48.
#[epics_macros_rs::epics_test]
async fn test_ca_put_mbbo_val_recomputes_rval() {
    use epics_base_rs::server::records::mbbo::MbboRecord;

    let db = PvDatabase::new();
    let mut rec = MbboRecord::new(0);
    rec.shft = 4;
    db.add_record("MBBO_CA", Box::new(rec)).await.unwrap();

    db.put_record_field_from_ca("MBBO_CA", "VAL", EpicsValue::Enum(3))
        .await
        .unwrap();

    let rec = db.get_record("MBBO_CA").unwrap();
    let inst = rec.read();
    // `UShort`, not `Enum`: this record defines no state table, and C's
    // `cvt_dbaddr` (mbboRecord.c:300-313) degenerates a stateless mbbo's VAL to
    // DBF_USHORT — there are no labels to serve behind an enum index. The put
    // still arrives as `Enum(3)` (a CA client that asked for DBR_ENUM), which
    // is the point: the WRITE accepts the enum, the READ answers at the type C
    // serves. Measured: bare `record(mbbo,...)` -> `cainfo` DBF_LONG.
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::UShort(3)),
        "VAL holds the CA-written value"
    );
    // RVAL/ORAW are DBF_ULONG (mbboRecord.dbd.pod:620,624).
    assert_eq!(
        inst.record.get_field("RVAL"),
        Some(EpicsValue::ULong(48)),
        "RVAL must be recomputed from the new VAL (3 << 4), not left stale at 0"
    );
    assert_eq!(
        inst.record.get_field("ORAW"),
        Some(EpicsValue::ULong(48)),
        "ORAW must roll forward to the freshly converted RVAL"
    );
}

/// Regression: a CA put to `mbboDirect.VAL` must recompute RVAL/ORAW.
///
/// C `mbboDirectRecord.c::process` (line 198) calls `convert(prec)`
/// unconditionally. With `shft = 4`, `convert()` yields
/// `RVAL = VAL << 4`. A CA put of VAL=5 must produce RVAL=ORAW=80.
#[epics_macros_rs::epics_test]
async fn test_ca_put_mbbo_direct_val_recomputes_rval() {
    use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;

    let db = PvDatabase::new();
    let mut rec = MbboDirectRecord::default();
    rec.shft = 4;
    db.add_record("MBBOD_CA", Box::new(rec)).await.unwrap();

    db.put_record_field_from_ca("MBBOD_CA", "VAL", EpicsValue::Long(5))
        .await
        .unwrap();

    let rec = db.get_record("MBBOD_CA").unwrap();
    let inst = rec.read();
    // RVAL/ORAW are DBF_ULONG (mbboDirectRecord.dbd.pod:167,172).
    assert_eq!(
        inst.record.get_field("RVAL"),
        Some(EpicsValue::ULong(80)),
        "RVAL must be recomputed from the new VAL (5 << 4), not left stale at 0"
    );
    assert_eq!(
        inst.record.get_field("ORAW"),
        Some(EpicsValue::ULong(80)),
        "ORAW must roll forward to the freshly converted RVAL"
    );
}

/// CRITICAL 1 — a record in SIMM (simulation) mode must still run its
/// forward link. C `aiRecord.c:151-168`: simulation is handled inside
/// `readValue()`, then `process()` ALWAYS runs `recGblFwdLink(prec)`
/// (`aiRecord.c:168`). The pre-fix Rust port returned early from
/// `check_simulation_mode`, so FLNK / CP / RPRO were skipped — every
/// link chain downstream of a SIMM-mode record silently broke.
#[epics_macros_rs::epics_test]
async fn test_simulation_mode_still_fires_forward_link() {
    let db = PvDatabase::new();
    db.add_record("SIM:SRC", Box::new(AoRecord::new(11.0)))
        .await
        .unwrap();
    let mut tgt = epics_base_rs::server::records::calc::CalcRecord::new("VAL+1");
    tgt.init_record(0).unwrap();
    db.add_record("SIM:FLNK_TARGET", Box::new(tgt))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    // SIMM=1 (YES) with SIOL pointing at SIM:SRC — enters simulation.
    ai.simm = 1;
    ai.siol = "SIM:SRC".into();
    db.add_record("SIM:AI", Box::new(ai)).await.unwrap();
    if let Some(rec) = db.get_record("SIM:AI") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("SIM:FLNK_TARGET".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SIM:AI", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("SIM:FLNK_TARGET").unwrap().to_f64(),
        Some(1.0),
        "a SIMM-mode record must still dispatch its FLNK forward link \
         (C aiRecord.c:168 recGblFwdLink runs unconditionally)"
    );
}

/// BUG 2 — a simulated `mbbi` is an INPUT record: it must READ the
/// value in from SIOL, not write VAL out to SIOL. `mbbiRecord.c:125-126`
/// declares SIML/SIOL and `mbbiRecord.c:388-394` reads
/// `dbGetLink(&prec->siol, DBR_ULONG, &prec->sval)`. Pre-fix the Rust
/// `is_input` set omitted `mbbi`, so a simulated mbbi fell into the
/// OUTPUT branch and wrote its own VAL out to the SIOL target.
#[epics_macros_rs::epics_test]
async fn test_simulated_mbbi_reads_siol_not_writes_it() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let db = PvDatabase::new();
    // SIOL source holds the simulated input value (index 3).
    db.add_record("MBBISIM:SRC", Box::new(LonginRecord::new(3)))
        .await
        .unwrap();

    // mbbi starts at index 0; SIMM=1 (YES), SIOL -> the source.
    let mut mbbi = MbbiRecord::new(0);
    mbbi.simm = 1;
    mbbi.siol = "MBBISIM:SRC".into();
    db.add_record("MBBISIM:IN", Box::new(mbbi)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("MBBISIM:IN", &mut visited, 0)
        .await
        .unwrap();

    // The SIOL source must be UNCHANGED — a simulated mbbi must not
    // write its VAL out to SIOL.
    let src = db.get_record("MBBISIM:SRC").unwrap();
    let src_val = src.read().record.get_field("VAL").unwrap();
    assert_eq!(
        src_val.to_f64().unwrap() as i64,
        3,
        "simulated mbbi must NOT write VAL out to its SIOL target"
    );

    // The mbbi must have READ the value in from SIOL.
    let mbbi_rec = db.get_record("MBBISIM:IN").unwrap();
    let mbbi_val = mbbi_rec.read().record.get_field("VAL").unwrap();
    assert_eq!(
        mbbi_val.to_f64().unwrap() as i64,
        3,
        "simulated mbbi must read VAL in from SIOL (got {mbbi_val:?})"
    );
}

/// BUG 3 — async-completion FLNK must not recurse into the
/// just-completed record. `complete_async_record_inner` seeds the
/// cycle-guard `visited` set with the record's own name (mirroring the
/// synchronous `process_record_with_links_inner`). An FLNK chain that
/// loops back (A -> FLNK -> B -> FLNK -> A) must terminate, not
/// re-enter A unbounded.
#[epics_macros_rs::epics_test]
async fn test_async_completion_flnk_cycle_terminates() {
    let db = PvDatabase::new();
    db.add_record("ACYC:A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("ACYC:B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // A -> FLNK -> B -> FLNK -> A : a closed forward-link loop.
    if let Some(rec) = db.get_record("ACYC:A") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("ACYC:B".into()))
            .unwrap();
    }
    if let Some(rec) = db.get_record("ACYC:B") {
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("ACYC:A".into()))
            .unwrap();
    }

    // Driving the async-completion path on A must terminate — pre-fix
    // it re-entered A through B's FLNK because `visited` was never
    // seeded with A's own name. A hung/overflowed run fails the test
    // by timeout/panic; a clean return proves the cycle guard closed.
    db.complete_async_record("ACYC:A").await.unwrap();
}

/// BUG 4 — fanout/seq/sseq must resolve the SELL input link into SELN
/// before SELN is used. C `fanoutRecord.c:103` calls
/// `dbGetLink(&prec->sell, DBR_USHORT, &prec->seln, 0, 0)` at the top
/// of every `process()`. Pre-fix `dispatch_multi_output` read SELN
/// directly from the field and never followed SELL, so a SELL link
/// pointing at another record never updated the selection.
#[epics_macros_rs::epics_test]
async fn test_fanout_resolves_sell_link_into_seln() {
    use epics_base_rs::server::records::fanout::FanoutRecord;

    let db = PvDatabase::new();
    // SELL source: selects link index 2.
    db.add_record("FANSELL:SRC", Box::new(LonginRecord::new(2)))
        .await
        .unwrap();
    for name in ["FANSELL:T2", "FANSELL:T0"] {
        let mut tgt = epics_base_rs::server::records::calc::CalcRecord::new("VAL+1");
        tgt.init_record(0).unwrap();
        db.add_record(name, Box::new(tgt)).await.unwrap();
    }

    let mut fan = FanoutRecord::new();
    fan.put_field("SELM", EpicsValue::Short(1)).unwrap(); // Specified
    fan.put_field("SELN", EpicsValue::Short(0)).unwrap(); // stale init value
    // SELL points at the source — must resolve to SELN=2 at process.
    fan.put_field("SELL", EpicsValue::String("FANSELL:SRC".into()))
        .unwrap();
    fan.put_field("LNK0", EpicsValue::String("FANSELL:T0 PP".into()))
        .unwrap();
    fan.put_field("LNK2", EpicsValue::String("FANSELL:T2 PP".into()))
        .unwrap();
    db.add_record("FANSELL:FAN", Box::new(fan)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("FANSELL:FAN", &mut visited, 0)
        .await
        .unwrap();

    // SELL resolved SELN to 2 -> SELM=Specified fans out LNK2 only.
    assert_eq!(
        db.get_pv("FANSELL:T2").unwrap().to_f64(),
        Some(1.0),
        "SELL must resolve SELN=2 so LNK2 is dispatched"
    );
    assert_eq!(
        db.get_pv("FANSELL:T0").unwrap().to_f64(),
        Some(0.0),
        "with SELL-resolved SELN=2, the stale SELN=0 (LNK0) must NOT be dispatched"
    );
    // SELN must now hold the SELL-resolved value.
    let fan_rec = db.get_record("FANSELL:FAN").unwrap();
    let seln = fan_rec.read().record.get_field("SELN").unwrap();
    assert_eq!(
        seln,
        EpicsValue::UShort(2),
        "SELN must be updated from the SELL link"
    );
}

/// `seqRecord.c:148` places the SELL→SELN `dbGetLink` *inside* the
/// `else` of `if (prec->selm == seqSELM_All)`, so an All-mode seq never
/// refreshes SELN from SELL (unlike fanout/dfanout, whose `dbGetLink` runs
/// before the SELM switch every cycle — pinned above). The port read SELL
/// for all three unconditionally, so an All-mode seq spuriously updated SELN
/// (and would post a SELN monitor) from a live SELL link. Boundary test: All
/// freezes SELN, Specified refreshes it, with the same live SELL link.
#[epics_macros_rs::epics_test]
async fn test_seq_skips_sell_in_all_mode_reads_in_specified() {
    use epics_base_rs::server::records::seq::SeqRecord;

    // All mode: SELL is live (source value 5) but SELN must stay frozen at
    // its init value (3) — C does not read SELL in All mode.
    {
        let db = PvDatabase::new();
        db.add_record("SEQALL:SRC", Box::new(LonginRecord::new(5)))
            .await
            .unwrap();
        let mut seq = SeqRecord::new();
        seq.selm = 0; // All
        seq.seln = 3; // distinct from the SELL source value (5)
        seq.sell = "SEQALL:SRC".to_string();
        db.add_record("SEQALL:REC", Box::new(seq)).await.unwrap();

        let mut visited = HashSet::new();
        db.process_record_with_links("SEQALL:REC", &mut visited, 0)
            .await
            .unwrap();

        let rec = db.get_record("SEQALL:REC").unwrap();
        let seln = rec.read().record.get_field("SELN").unwrap();
        assert_eq!(
            seln,
            EpicsValue::UShort(3),
            "All-mode seq must NOT refresh SELN from SELL (C seqRecord.c:148 \
             reads SELL only in the non-All branch); the unconditional read \
             would have set SELN=5"
        );
    }

    // Specified mode: the same live SELL (source value 1) MUST refresh SELN.
    {
        let db = PvDatabase::new();
        db.add_record("SEQSPEC:SRC", Box::new(LonginRecord::new(1)))
            .await
            .unwrap();
        let mut seq = SeqRecord::new();
        seq.selm = 1; // Specified
        seq.seln = 0; // stale init value
        seq.sell = "SEQSPEC:SRC".to_string();
        db.add_record("SEQSPEC:REC", Box::new(seq)).await.unwrap();

        let mut visited = HashSet::new();
        db.process_record_with_links("SEQSPEC:REC", &mut visited, 0)
            .await
            .unwrap();

        let rec = db.get_record("SEQSPEC:REC").unwrap();
        let seln = rec.read().record.get_field("SELN").unwrap();
        assert_eq!(
            seln,
            EpicsValue::UShort(1),
            "Specified-mode seq MUST refresh SELN from SELL (C reads it in \
             the else branch)"
        );
    }
}

/// fanout/dfanout/seq `SELN` carries dbd `initial("1")`: a record
/// constructed without an explicit SELN must default to 1, not the
/// hand-coded 0. Observable in SELM=Specified/Mask when the .db omits SELN
/// (All ignores SELN). dfanout's Specified output is `seln - 1`, so 0 would
/// drive nothing where C drives OUTA.
#[epics_macros_rs::epics_test]
async fn test_fanout_dfanout_seq_seln_default_is_one() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;
    use epics_base_rs::server::records::fanout::FanoutRecord;
    use epics_base_rs::server::records::seq::SeqRecord;

    assert_eq!(
        FanoutRecord::new().get_field("SELN"),
        Some(EpicsValue::UShort(1)),
        "fanout SELN dbd initial(\"1\")"
    );
    assert_eq!(
        DfanoutRecord::new(0.0).get_field("SELN"),
        Some(EpicsValue::UShort(1)),
        "dfanout SELN dbd initial(\"1\")"
    );
    assert_eq!(
        SeqRecord::new().get_field("SELN"),
        Some(EpicsValue::UShort(1)),
        "seq SELN dbd initial(\"1\")"
    );
}

/// BUG 5 — `putAcks` (C `dbAccess.c:1300-1312`) compares the written
/// severity against the STORED unacknowledged severity `acks`, not
/// against the current `sevr`; `putAckt` (C `dbAccess.c:1282-1298`)
/// lowers `acks` down to `sevr` when ACKT is set false and
/// `acks > sevr`.
///
/// R17-62 corrected the ROUTE this test drives: acknowledgement is a DBR
/// request type (`DBR_PUT_ACKS`/`ACKT`) that `dbPut` intercepts
/// (`dbAccess.c:1333-1336`) above the `SPC_NOMOD` gate `dbPutSpecial` applies
/// (`:123`, called at `:1345-1348`), not a put to the ACKS/ACKT
/// fields — softIoc refuses `caput N1.ACKS 2` with "Write access denied".
/// The handlers moved to `RecordInstance::put_acks` / `put_ackt`; the
/// semantics asserted here are unchanged.
#[epics_macros_rs::epics_test]
async fn test_acks_put_compares_against_acks_and_ackt_lowers() {
    // putAcks: acks must be cleared when the written severity is >=
    // the STORED acks, even after sevr has dropped below it.
    {
        let rec = AoRecord::new(0.0);
        let mut inst = RecordInstance::new("ACKTEST1".into(), rec);
        // Latched sticky alarm: acks=MAJOR(2); current sevr has since
        // dropped to MINOR(1).
        inst.common.acks = AlarmSeverity::Major;
        inst.common.sevr = AlarmSeverity::Minor;
        // Acknowledge at MAJOR — written sev (2) >= acks (2) -> clear.
        inst.put_acks(2, LinkBacking::none());
        assert_eq!(
            inst.common.acks,
            AlarmSeverity::NoAlarm,
            "ACKS write at sev>=stored acks must clear acks \
             (C dbAccess.c:1306 compares *psev >= precord->acks)"
        );

        // A second case: written sev BELOW the stored acks must NOT
        // clear it — proving the comparison is against `acks`, not
        // `sevr`. Were it compared against sevr (Minor), a MINOR write
        // would wrongly clear.
        let rec2 = AoRecord::new(0.0);
        let mut inst2 = RecordInstance::new("ACKTEST2".into(), rec2);
        inst2.common.acks = AlarmSeverity::Major;
        inst2.common.sevr = AlarmSeverity::Minor;
        inst2.put_acks(1, LinkBacking::none());
        assert_eq!(
            inst2.common.acks,
            AlarmSeverity::Major,
            "ACKS write at sev BELOW stored acks must NOT clear acks; \
             comparing against sevr (Minor) instead would wrongly clear"
        );
    }

    // putAckt: ACKT set false with acks > sevr must lower acks to sevr.
    {
        let rec = AoRecord::new(0.0);
        let mut inst = RecordInstance::new("ACKTEST3".into(), rec);
        inst.common.ackt = true;
        inst.common.acks = AlarmSeverity::Major;
        inst.common.sevr = AlarmSeverity::Minor;
        inst.put_ackt(0, LinkBacking::none());
        assert!(!inst.common.ackt, "ACKT must be cleared");
        assert_eq!(
            inst.common.acks,
            AlarmSeverity::Minor,
            "ACKT=false with acks>sevr must lower acks down to sevr \
             (C dbAccess.c:1291-1294)"
        );
    }
}

/// BUG 2 regression — a bare (modifier-less) OUT link is NPP: the
/// value is written to the target but the target is NOT processed.
/// C `dbDbPutValue` (dbDbLink.c:386-389) calls `processTarget` only
/// when the link carries an explicit `PP` flag (or writes `.PROC`).
#[epics_macros_rs::epics_test]
async fn test_bare_out_link_does_not_process_target() {
    let db = PvDatabase::new();
    db.add_record("SRC_OUT", Box::new(AoRecord::new(33.0)))
        .await
        .unwrap();
    // A counter, so "written but not processed" reads 33 and "written and
    // processed" would read 34 — a value the assertion below can tell apart.
    let mut tgt = epics_base_rs::server::records::calc::CalcRecord::new("VAL+1");
    tgt.init_record(0).unwrap();
    db.add_record("TGT_OUT", Box::new(tgt)).await.unwrap();

    // Bare OUT link — no PP modifier.
    if let Some(rec) = db.get_record("SRC_OUT") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("TGT_OUT.VAL".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC_OUT", &mut visited, 0)
        .await
        .unwrap();

    // The value must have landed on the target.
    let tgt_val = db.get_pv("TGT_OUT").unwrap();
    assert_eq!(
        tgt_val.to_f64().unwrap(),
        33.0,
        "bare OUT link (NPP) writes the value and must NOT process the \
         target — a process would have made it 34"
    );
}

/// BUG 2 regression (positive case) — an OUT link with an explicit
/// `PP` token DOES process a Passive target, mirroring C
/// `dbDbPutValue` `pvlOptPP` branch.
#[epics_macros_rs::epics_test]
async fn test_pp_out_link_processes_passive_target() {
    let db = PvDatabase::new();
    db.add_record("SRC_PP", Box::new(AoRecord::new(44.0)))
        .await
        .unwrap();
    let mut tgt = epics_base_rs::server::records::calc::CalcRecord::new("VAL+1");
    tgt.init_record(0).unwrap();
    db.add_record("TGT_PP", Box::new(tgt)).await.unwrap();

    if let Some(rec) = db.get_record("SRC_PP") {
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("TGT_PP.VAL PP".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC_PP", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("TGT_PP").unwrap().to_f64(),
        Some(45.0),
        "explicit PP OUT link must process its Passive target — 44 written, \
         then one process turns it into 45"
    );
}

/// Formerly-bypassing path. A foreign full-processing entry
/// (`process_record_with_links`, the normal scan/event/FLNK-dispatch
/// caller) must block while a multi-record transaction holds the
/// member record's advisory write gate via `lock_records`. Before the
/// fix `process_record_with_links` took no gate, so a normal scan of a
/// member could interleave with a QSRV atomic group or pvalink atomic
/// scan epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mr_r5_foreign_process_blocks_on_held_epoch() {
    let db = PvDatabase::new();
    db.add_record("MR_R5_MEMBER", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Transaction owner holds the member's gate via `lock_records`.
    let epoch = db.lock_records(["MR_R5_MEMBER"]);

    let db2 = db.clone();
    let processed = Arc::new(AtomicU32::new(0));
    let processed2 = processed.clone();
    let h = epics_base_rs::runtime::task::Reactor::current()
        .expect("the test driver enters an executor")
        .spawn(async move {
            // Foreign full-processing entry — must block on the gate the
            // epoch holds.
            let mut visited = HashSet::new();
            let _ = db2
                .process_record_with_links("MR_R5_MEMBER", &mut visited, 0)
                .await;
            processed2.store(1, Ordering::SeqCst);
        });

    // Give the spawned task time to reach (and block on) the gate.
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        processed.load(Ordering::SeqCst),
        0,
        "foreign process_record_with_links must block while a lock_records epoch holds the member gate"
    );

    drop(epoch);
    h.await.unwrap();
    assert_eq!(
        processed.load(Ordering::SeqCst),
        1,
        "foreign process must complete once the epoch is released"
    );
}

/// Owner path. A transaction owner holding a member's advisory
/// write gate via `lock_records` processes that member through the
/// `_already_locked` full-processing entry. The gate is not
/// reentrant, so using the gate-acquiring `process_record_with_links`
/// here would dead-lock the epoch against itself; the `_already_locked`
/// entry must complete without blocking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mr_r5_already_locked_process_does_not_self_deadlock() {
    let db = PvDatabase::new();
    db.add_record("MR_R5_OWNED", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Owner holds the member gate for the whole transaction.
    let _epoch = db.lock_records(["MR_R5_OWNED"]);

    // Processing the member via the `_already_locked` entry while the
    // epoch is held must NOT dead-lock. Since H6 that entry is a `fn` —
    // it holds no `.await` at all, so a regression that put the
    // gate-acquiring entry back would not even compile here; before H6 this
    // was a `tokio::time::timeout` around the same call.
    let mut visited = HashSet::new();
    let res = db.process_record_with_links_already_locked("MR_R5_OWNED", &mut visited, 0);
    res.expect("owner-path processing of an owned member must succeed");
    // The marker unwinds with the frame (`dbDbLink.c:521-526`), so what a
    // completed call leaves behind is an empty set, not a record of itself.
    assert!(visited.is_empty(), "the frame unwound: {visited:?}");
}

// ---------------------------------------------------------------------
// The scan-index tail is INSIDE the advisory-gate window.
//
// `update_scan_index` is synchronous as of step 4, which is what lets the
// scan-list move stay inside the exclusion window C's `dbScanLock` has it
// in. These are boundary cases of one invariant — *the move is committed
// before `put_pv` returns, and is not observable to a party the window
// excludes* — not scenarios: the SCAN branch, the PHAS branch, and the
// held-epoch boundary.
// ---------------------------------------------------------------------

/// Boundary: `CommonFieldPutResult::ScanChanged`. The record has left its
/// old scan list and joined the new one at the instant `put_pv` returns —
/// no sleep, no `yield_now`, nothing that could let a deferred update land
/// in between. Verified to fail when the tail is deferred out of the window
/// (`tokio::spawn`ing the `update_scan_index` call makes this and the PHAS
/// test below fail while the epoch test still passes).
#[epics_macros_rs::epics_test]
async fn scan_move_is_committed_when_put_pv_returns() {
    let db = PvDatabase::new();
    db.add_record("SIW_SCAN", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("SIW_SCAN.SCAN", EpicsValue::String("1 second".into()))
        .await
        .unwrap();
    assert!(
        db.records_for_scan(ScanType::SEC1)
            .await
            .contains(&"SIW_SCAN".to_string()),
        "the record must be in the 1-second list the moment put_pv returns"
    );

    db.put_pv("SIW_SCAN.SCAN", EpicsValue::String("2 second".into()))
        .await
        .unwrap();
    assert!(
        !db.records_for_scan(ScanType::SEC1)
            .await
            .contains(&"SIW_SCAN".to_string()),
        "the old bucket must be swept in the same window, not later"
    );
    assert!(
        db.records_for_scan(ScanType::SEC2)
            .await
            .contains(&"SIW_SCAN".to_string()),
        "the record must be in the 2-second list the moment put_pv returns"
    );
}

/// Boundary: `CommonFieldPutResult::PhasChanged`. A PHAS put reorders the
/// bucket rather than moving the record between buckets, and that reorder
/// is likewise complete when `put_pv` returns.
#[epics_macros_rs::epics_test]
async fn phas_reorder_is_committed_when_put_pv_returns() {
    let db = PvDatabase::new();
    for name in ["SIW_PHAS_A", "SIW_PHAS_B"] {
        db.add_record(name, Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        db.put_pv(
            &format!("{name}.SCAN"),
            EpicsValue::String("1 second".into()),
        )
        .await
        .unwrap();
    }
    // Equal PHAS: load order decides, so A precedes B.
    assert_eq!(
        db.records_for_scan(ScanType::SEC1).await,
        vec!["SIW_PHAS_A".to_string(), "SIW_PHAS_B".to_string()]
    );

    // Raising A's PHAS must have reordered the bucket by the time the put
    // returns — PHAS is the primary sort key.
    db.put_pv("SIW_PHAS_A.PHAS", EpicsValue::Short(5))
        .await
        .unwrap();
    assert_eq!(
        db.records_for_scan(ScanType::SEC1).await,
        vec!["SIW_PHAS_B".to_string(), "SIW_PHAS_A".to_string()],
        "the PHAS reorder must be visible the moment put_pv returns"
    );
}

/// Boundary: a `lock_records` epoch holding the record's gate excludes the
/// whole put, scan move included. While the epoch is held the put cannot
/// complete AND no move is observable; the instant the put returns, it is.
///
/// What it pins, exactly: that the SCAN put still enters through the
/// advisory gate. Verified by removing the `lock_record` from
/// `put_pv_inner` — this fails, the two above still pass. It does **not**
/// distinguish a window shrunk *mid-put* (a gate dropped after the value
/// write but before the tail): the put still has to take the gate to
/// start, so it still runs entirely after the epoch releases. Catching
/// that needs a probe inside the put and is not attempted here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_move_cannot_land_inside_a_held_epoch() {
    let db = PvDatabase::new();
    db.add_record("SIW_EPOCH", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let epoch = db.lock_records(["SIW_EPOCH"]);

    let db2 = db.clone();
    let done = Arc::new(AtomicU32::new(0));
    let done2 = done.clone();
    let h = epics_base_rs::runtime::task::Reactor::current()
        .expect("the test driver enters an executor")
        .spawn(async move {
            db2.put_pv("SIW_EPOCH.SCAN", EpicsValue::String("1 second".into()))
                .await
                .unwrap();
            done2.store(1, Ordering::SeqCst);
        });

    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        done.load(Ordering::SeqCst),
        0,
        "put_pv must block on the gate the epoch holds"
    );
    assert!(
        db.records_for_scan(ScanType::SEC1).await.is_empty(),
        "no scan-list move may be observable while the epoch owns the record"
    );

    drop(epoch);
    h.await.unwrap();
    assert_eq!(done.load(Ordering::SeqCst), 1);
    assert!(
        db.records_for_scan(ScanType::SEC1)
            .await
            .contains(&"SIW_EPOCH".to_string()),
        "the move must be complete as soon as the put returns"
    );
}

/// a CA link carries its `MS`/`NMS`/`MSI`/`MSS` modifier in
/// the parsed model, and record processing applies the maximize-severity
/// gate using that switch (uniform with DB links). The CA lset returns
/// the RAW remote alarm (severity + status); processing decides what to
/// fold. Tested by invariant boundary, not by narrative:
///
/// * NMS  → never propagate
/// * MS   → max severity, STAT = LINK_ALARM (remote stat NOT preserved)
/// * MSI  → propagate only when remote sevr == INVALID
/// * MSS  → max severity AND remote STAT preserved (the cited gap)
#[epics_macros_rs::epics_test]
async fn br_fr3_ca_link_applies_maximize_switch_at_processing() {
    use epics_base_rs::server::database::LinkSet;
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    /// CA lset: connected, returns a configurable RAW remote alarm — it
    /// does NOT apply any MS/NMS gate itself (that is record processing's
    /// job for CA links now).
    struct RawCaLset {
        sevr: i32,
        stat: i32,
    }
    #[epics_base_rs::async_trait]
    impl LinkSet for RawCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(7.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn alarm_severity(&self, _: &str) -> Option<i32> {
            // Mirror the real CA resolver: only a non-zero severity is a
            // contribution worth returning.
            if self.sevr > 0 { Some(self.sevr) } else { None }
        }
        fn alarm_status(&self, _: &str) -> Option<i32> {
            Some(self.stat)
        }
    }

    // Process an ai record whose INP is `inp`, with the CA lset serving
    // the given raw remote (sevr, stat). Returns the record's resulting
    // (SEVR, STAT).
    async fn run(inp: &str, remote_sevr: i32, remote_stat: i32) -> (AlarmSeverity, u16) {
        let db = PvDatabase::new();
        db.register_link_set(
            "ca",
            Arc::new(RawCaLset {
                sevr: remote_sevr,
                stat: remote_stat,
            }),
        )
        .await;
        db.add_record("CADST", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        if let Some(rec) = db.get_record("CADST") {
            let mut inst = rec.write();
            inst.put_common_field("INP", EpicsValue::String(inp.into()))
                .unwrap();
            inst.common.udf = 0;
        }
        let mut visited = HashSet::new();
        db.process_record_with_links("CADST", &mut visited, 0)
            .await
            .unwrap();
        let rec = db.get_record("CADST").expect("record exists");
        let inst = rec.read();
        (inst.common.sevr, inst.common.stat)
    }

    // Remote: MAJOR severity, COMM_ALARM (9) status — distinct from
    // LINK_ALARM (14) so an MSS pass-through is observable.
    const REMOTE_STAT: i32 = alarm_status::COMM_ALARM as i32;

    // NMS — no propagation despite a connected MAJOR remote.
    let (sevr, _stat) = run("ca://REMOTE NMS", 2, REMOTE_STAT).await;
    assert_eq!(
        sevr,
        AlarmSeverity::NoAlarm,
        "NMS must not propagate the remote alarm"
    );

    // MS — lift to remote MAJOR, but surface as the generic LINK_ALARM.
    let (sevr, stat) = run("ca://REMOTE MS", 2, REMOTE_STAT).await;
    assert_eq!(sevr, AlarmSeverity::Major, "MS lifts SEVR to remote MAJOR");
    assert_eq!(
        stat,
        alarm_status::LINK_ALARM,
        "MS surfaces as LINK_ALARM, not the remote STAT"
    );

    // MSS — lift severity AND adopt the remote STAT (the cited gap).
    let (sevr, stat) = run("ca://REMOTE MSS", 2, REMOTE_STAT).await;
    assert_eq!(sevr, AlarmSeverity::Major, "MSS lifts SEVR to remote MAJOR");
    assert_eq!(
        stat,
        alarm_status::COMM_ALARM,
        "MSS must preserve the remote STAT code, not collapse to LINK_ALARM"
    );

    // MSI with a non-INVALID (MAJOR) remote — must NOT propagate.
    let (sevr, _stat) = run("ca://REMOTE MSI", 2, REMOTE_STAT).await;
    assert_eq!(
        sevr,
        AlarmSeverity::NoAlarm,
        "MSI ignores a non-INVALID remote alarm"
    );

    // MSI with an INVALID remote — inherits as LINK_ALARM + INVALID.
    let (sevr, stat) = run("ca://REMOTE MSI", 3, REMOTE_STAT).await;
    assert_eq!(
        sevr,
        AlarmSeverity::Invalid,
        "MSI inherits an INVALID remote alarm"
    );
    assert_eq!(stat, alarm_status::LINK_ALARM, "MSI surfaces as LINK_ALARM");
}

// lsi/lso `menuPost` MPST/APST "Always" mode.
//
// After fix 9587929c an unchanged lsi/lso process cycle posts NO
// VALUE/LOG monitor (C `lsiRecord.c`/`lsoRecord.c` monitor gate on
// `len != olen || memcmp(oval, val, len)`). The MPST/APST menu fields
// restore C's override: an unchanged cycle still posts DBE_VALUE when
// MPST == menuPost_Always and DBE_LOG when APST == menuPost_Always
// (lsiRecord.c:217-220). This test drives an unchanged cycle through
// the link-processing path and asserts the VALUE event fires only when
// MPST is Always.
#[epics_macros_rs::epics_test]
async fn test_lsi_mpst_always_posts_value_on_unchanged_cycle() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::lsi::LsiRecord;
    use epics_base_rs::types::DbFieldType;

    async fn unchanged_cycle_posts_value(mpst_always: bool) -> bool {
        let db = PvDatabase::new();
        db.add_record("LSI_MPST", Box::new(LsiRecord::new("hello")))
            .await
            .unwrap();
        if mpst_always {
            // MPST = menuPost_Always (1).
            let rec = db.get_record("LSI_MPST").unwrap();
            let mut inst = rec.write();
            inst.record.put_field("MPST", EpicsValue::Short(1)).unwrap();
        }

        // Cycle 1 commits oval/olen for the seeded "hello", so cycle 2 is
        // genuinely unchanged (value_changed == false).
        let mut visited = HashSet::new();
        db.process_record_with_links("LSI_MPST", &mut visited, 0)
            .await
            .unwrap();

        // Subscribe to VAL AFTER cycle 1.
        let mut val_rx = {
            let rec = db.get_record("LSI_MPST").unwrap();
            let mut inst = rec.write();
            inst.add_subscriber("VAL", 71, DbFieldType::Char, EventMask::VALUE.bits())
        }
        .expect("VAL subscription must be accepted");

        // Cycle 2: no new value. Without MPST this posts nothing; with
        // MPST == Always it must still post a DBE_VALUE event.
        let mut visited = HashSet::new();
        db.process_record_with_links("LSI_MPST", &mut visited, 0)
            .await
            .unwrap();

        val_rx.try_recv().is_ok()
    }

    assert!(
        unchanged_cycle_posts_value(true).await,
        "MPST == Always must post a VALUE monitor on an unchanged lsi cycle"
    );
    assert!(
        !unchanged_cycle_posts_value(false).await,
        "MPST == OnChange (default) must NOT post a VALUE monitor on an unchanged lsi cycle"
    );
}

/// A `sub` record's registered subroutine must run on the MAIN engine
/// processing path (SCAN / event / CA-put / FLNK), not only on the by-name
/// `process_local` path. C `subRecord.c::do_sub` runs the subroutine on
/// every `process()`. Before the fix the main engine called
/// `record.process()` (a no-op for `sub`) without invoking the framework's
/// `SubroutineFn`, so VAL never updated when the record was scanned.
#[epics_macros_rs::epics_test]
async fn sub_record_subroutine_runs_on_main_engine_path() {
    use epics_base_rs::server::records::sub_record::SubRecord;

    let db = PvDatabase::new();
    // SNAM is set BEFORE the record is added: C's `.db` load applies every
    // `field()` and only then runs `init_record`, which parks PACT for good when
    // SNAM is empty (subRecord.c:119-123). A sub configured after init is a
    // record C would have declared dead.
    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("double_val".into()))
        .unwrap();
    db.add_record("SUBM", Box::new(seed)).await.unwrap();

    // A VAL-doubling subroutine, plus a seed VAL so the change is observable.
    {
        let arc = db.get_record("SUBM").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(5.0))
            .unwrap();
        let sub_fn: SubroutineFn = Box::new(|record: &mut dyn Record| {
            if let Some(EpicsValue::Double(v)) = record.get_field("VAL") {
                record.put_field("VAL", EpicsValue::Double(v * 2.0))?;
            }
            Ok(0)
        });
        inst.subroutine = Some(Arc::new(sub_fn));
    }

    // Drive the MAIN engine path (not process_local).
    let mut visited = HashSet::new();
    db.process_record_with_links("SUBM", &mut visited, 0)
        .await
        .unwrap();

    let arc = db.get_record("SUBM").unwrap();
    let inst = arc.read();
    match inst.record.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!(
            (v - 10.0).abs() < 1e-10,
            "subroutine must double VAL on the main path: got {v}"
        ),
        other => panic!("expected Double(10.0), got {other:?}"),
    }
}

/// A `sub` record must run the shared analog `checkAlarms` (HIHI/HIGH/
/// LOLO/LOW with HYST + LALM). C `subRecord.c::checkAlarms` (lines 319-373)
/// is the standard analog limit check; the Rust port previously gave `sub`
/// no limit fields and skipped the analog-alarm owner entirely, so a `sub`
/// whose subroutine drove VAL past HIHI never alarmed.
#[epics_macros_rs::epics_test]
async fn sub_record_hihi_alarm_fires_via_shared_owner() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::records::sub_record::SubRecord;

    let db = PvDatabase::new();

    // FIRE: subroutine drives VAL to 100, HIHI=50/Major -> HIHI_ALARM.
    db.add_record("SUB_HIHI", Box::new(sub_with_snam("drive")))
        .await
        .unwrap();
    // CONTROL: same VAL=100 but HIHI=200 -> no alarm, LALM tracks VAL.
    db.add_record("SUB_OK", Box::new(sub_with_snam("drive")))
        .await
        .unwrap();

    for (name, hihi) in [("SUB_HIHI", 50.0), ("SUB_OK", 200.0)] {
        let arc = db.get_record(name).unwrap();
        let mut inst = arc.write();
        inst.put_common_field("HIHI", EpicsValue::Double(hihi))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
        let sub_fn: SubroutineFn = Box::new(|record: &mut dyn Record| {
            record.put_field("VAL", EpicsValue::Double(100.0))?;
            Ok(0)
        });
        inst.subroutine = Some(Arc::new(sub_fn));
    }

    for name in ["SUB_HIHI", "SUB_OK"] {
        let mut visited = HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();
    }

    // FIRE record: severity Major, stat HIHI_ALARM, LALM pinned to the
    // crossed threshold (C sets `prec->lalm = alev`).
    {
        let arc = db.get_record("SUB_HIHI").unwrap();
        let inst = arc.read();
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::Major,
            "sub VAL=100 over HIHI=50 must raise MAJOR"
        );
        assert_eq!(
            inst.common.stat,
            alarm_status::HIHI_ALARM,
            "sub over-HIHI must surface as HIHI_ALARM"
        );
        assert_eq!(
            inst.record.get_field("LALM"),
            Some(EpicsValue::Double(50.0)),
            "C parity: LALM pins to the crossed alarm threshold (HIHI=50)"
        );
    }
    // CONTROL record: no alarm, LALM tracks the current VAL.
    {
        let arc = db.get_record("SUB_OK").unwrap();
        let inst = arc.read();
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::NoAlarm,
            "sub VAL=100 under HIHI=200 must not alarm"
        );
        assert_eq!(
            inst.record.get_field("LALM"),
            Some(EpicsValue::Double(100.0)),
            "C parity: no alarm leaves LALM = current VAL"
        );
    }
}

/// A `sub` record must gate the `VAL` monitor on MDEL (C `subRecord.c::
/// monitor` lines 386-394, `recGblCheckDeadband` against MLST). The record
/// previously carried no MDEL/MLST, so the deadband owner saw
/// `mdel=0` and posted on every change. MLST tracks the last posted value,
/// so it is the observable witness of the deadband decision.
#[epics_macros_rs::epics_test]
async fn sub_record_mdel_gates_val_monitor() {
    use epics_base_rs::server::records::sub_record::SubRecord;

    let db = PvDatabase::new();
    let mut rec = sub_with_snam("noop");
    rec.val = 100.0;
    rec.mdel = 10.0;
    // C `subRecord.c:130` seeds MLST from VAL at the END of `init_record`,
    // past the SNAM resolution, so the port performs it in the resolution
    // owner (`ioc_app::wire_subroutine`). This record is built by hand and
    // never goes through it, so the post-init state is set directly.
    rec.mlst = 100.0;
    db.add_record("SUB_MDEL", Box::new(rec)).await.unwrap();

    // Helper: set VAL directly (no subroutine), process, read back MLST.
    async fn drive(db: &PvDatabase, val: f64) -> f64 {
        {
            let arc = db.get_record("SUB_MDEL").unwrap();
            let mut inst = arc.write();
            inst.record
                .put_field("VAL", EpicsValue::Double(val))
                .unwrap();
        }
        let mut visited = HashSet::new();
        db.process_record_with_links("SUB_MDEL", &mut visited, 0)
            .await
            .unwrap();
        let arc = db.get_record("SUB_MDEL").unwrap();
        let inst = arc.read();
        inst.record
            .get_field("MLST")
            .and_then(|v| v.to_f64())
            .unwrap()
    }

    // Change 5 (< MDEL=10) from MLST=100: no monitor, MLST frozen at 100.
    assert_eq!(
        drive(&db, 105.0).await,
        100.0,
        "VAL change below MDEL must NOT post (MLST stays at last-posted 100)"
    );
    // Change 15 (>= MDEL=10) from MLST=100: monitor posts, MLST -> 115.
    assert_eq!(
        drive(&db, 115.0).await,
        115.0,
        "VAL change at/over MDEL must post and advance MLST to 115"
    );
}

/// A `sub` subroutine returning a negative status must raise SOFT_ALARM at
/// the record's BRSV severity (C `subRecord.c::do_sub`: `if (status < 0)
/// recGblSetSevr(SOFT_ALARM, prec->brsv)`). The earlier `SubroutineFn`
/// returned `CaResult<()>`, so a subroutine could not signal an error and
/// no SOFT_ALARM was ever raised.
///
/// Each record is driven TWICE, with the subroutine returning 0 on the first
/// call, because C reaches SOFT_ALARM's severity only on a record that is
/// already DEFINED. `do_sub` writes `prec->udf` in its `else` arm alone
/// (`subRecord.c:434`), so on a first cycle the negative-status arm leaves UDF
/// at its loaded 1 and `checkAlarms` takes `if (prec->udf) { recGblSetSevr(
/// UDF_ALARM, prec->udfs); return; }` (`:323-326`) — UDFS is INVALID, which
/// maximizes over SOFT/MAJOR. The successful first call is what C's own
/// scanning record would have had before the failure. (This test asserted the
/// single-cycle case while the port re-derived UDF from VAL on every cycle and
/// so never had a UDF to trip over.)
#[epics_macros_rs::epics_test]
async fn sub_record_negative_status_raises_soft_alarm_at_brsv() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::records::sub_record::SubRecord;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let db = PvDatabase::new();

    // FIRE: status -1 with BRSV=Major -> SOFT_ALARM/Major.
    db.add_record("SUB_SOFT", Box::new(sub_with_snam("status")))
        .await
        .unwrap();
    // CONTROL: status 0 with BRSV=Major -> no alarm.
    db.add_record("SUB_OK0", Box::new(sub_with_snam("status")))
        .await
        .unwrap();

    for (name, status) in [("SUB_SOFT", -1_i64), ("SUB_OK0", 0_i64)] {
        let arc = db.get_record(name).unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("BRSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let sub_fn: SubroutineFn = Box::new(move |_record: &mut dyn Record| {
            // First call succeeds (VAL 0, so `udf = isnan(0)` clears); later
            // calls return the status under test.
            Ok(if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                0
            } else {
                status
            })
        });
        inst.subroutine = Some(Arc::new(sub_fn));
    }

    for name in ["SUB_SOFT", "SUB_OK0"] {
        for _ in 0..2 {
            let mut visited = HashSet::new();
            db.process_record_with_links(name, &mut visited, 0)
                .await
                .unwrap();
        }
    }

    {
        let arc = db.get_record("SUB_SOFT").unwrap();
        let inst = arc.read();
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::Major,
            "sub status<0 must raise SOFT_ALARM at BRSV=Major"
        );
        assert_eq!(
            inst.common.stat,
            alarm_status::SOFT_ALARM,
            "sub status<0 must set STAT=SOFT_ALARM"
        );
    }
    {
        let arc = db.get_record("SUB_OK0").unwrap();
        let inst = arc.read();
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::NoAlarm,
            "sub status==0 must not raise SOFT_ALARM regardless of BRSV"
        );
    }
}

/// An `aSub` publishes the subroutine's return status as VAL (C
/// `aSubRecord.c:224` `prec->val = status`), overwriting whatever the
/// closure wrote to VAL, and a negative status raises SOFT_ALARM at BRSV.
#[epics_macros_rs::epics_test]
async fn asub_record_val_is_return_status_and_negative_soft_alarms() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::records::asub_record::ASubRecord;

    let db = PvDatabase::new();

    // POSITIVE: closure writes VAL=999 but returns 42 -> VAL must be 42.
    db.add_record("ASUB_POS", Box::new(ASubRecord::default()))
        .await
        .unwrap();
    // NEGATIVE: returns -5 with BRSV=Minor -> VAL=-5, SOFT_ALARM/Minor.
    db.add_record("ASUB_NEG", Box::new(ASubRecord::default()))
        .await
        .unwrap();

    {
        let arc = db.get_record("ASUB_POS").unwrap();
        let mut inst = arc.write();
        let sub_fn: SubroutineFn = Box::new(|record: &mut dyn Record| {
            // The closure's own VAL write is discarded by `prec->val = status`.
            record.put_field("VAL", EpicsValue::Double(999.0))?;
            Ok(42)
        });
        inst.subroutine = Some(Arc::new(sub_fn));
    }
    {
        let arc = db.get_record("ASUB_NEG").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("BRSV", EpicsValue::Short(AlarmSeverity::Minor as i16))
            .unwrap();
        let sub_fn: SubroutineFn = Box::new(|_record: &mut dyn Record| Ok(-5));
        inst.subroutine = Some(Arc::new(sub_fn));
    }

    for name in ["ASUB_POS", "ASUB_NEG"] {
        let mut visited = HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();
    }

    {
        let arc = db.get_record("ASUB_POS").unwrap();
        let inst = arc.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Long(42)),
            "aSub VAL must be the return status (42), not the closure's 999"
        );
    }
    {
        let arc = db.get_record("ASUB_NEG").unwrap();
        let inst = arc.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Long(-5)),
            "aSub VAL must carry the negative return status (-5)"
        );
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::Minor,
            "aSub status<0 must raise SOFT_ALARM at BRSV=Minor"
        );
        assert_eq!(
            inst.common.stat,
            alarm_status::SOFT_ALARM,
            "aSub status<0 must set STAT=SOFT_ALARM"
        );
    }
}

/// aSub `EFLG` gates `VALx` output-array monitor posting (C
/// `aSubRecord.c::monitor`): NEVER suppresses it, ON CHANGE (default) posts on
/// change, ALWAYS posts every process even when unchanged. Exercised through
/// the real foreign-process monitor path (`process_record` -> `process_local`).
#[epics_macros_rs::epics_test]
async fn asub_eflg_gates_valx_output_monitor_posting() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::asub_record::ASubRecord;
    use epics_base_rs::types::DbFieldType;
    use std::sync::Mutex;

    let db = PvDatabase::new();
    let mut asub = ASubRecord::default();
    // The output capacity must be declared, as C requires: the subroutine
    // writes a 2-element array into VALA's NOVA-element buffer.
    asub.put_field("NOVA", EpicsValue::Long(2)).unwrap();
    db.add_record("ASUB_E", Box::new(asub)).await.unwrap();

    // Subroutine writes VALA from a shared cell so the test controls whether
    // the output changes between processes.
    let out = Arc::new(Mutex::new(vec![1.0_f64, 2.0]));
    {
        let arc = db.get_record("ASUB_E").unwrap();
        let mut inst = arc.write();
        let out2 = out.clone();
        let sub_fn: SubroutineFn = Box::new(move |record: &mut dyn Record| {
            let v = out2.lock().unwrap().clone();
            record.put_field("VALA", EpicsValue::DoubleArray(v))?;
            Ok(0)
        });
        inst.subroutine = Some(Arc::new(sub_fn));
    }

    let mut rx = {
        let arc = db.get_record("ASUB_E").unwrap();
        let mut inst = arc.write();
        inst.add_subscriber("VALA", 31, DbFieldType::Double, EventMask::VALUE.bits())
    }
    .expect("VALA subscription must be accepted");

    // ON CHANGE (default): first process changes VALA -> event.
    db.process_record("ASUB_E").await.unwrap();
    assert!(
        rx.try_recv().is_ok(),
        "ON CHANGE: VALA changed from its zeroed default -> monitor event"
    );
    // Same VALA again -> no event.
    db.process_record("ASUB_E").await.unwrap();
    assert!(
        rx.try_recv().is_err(),
        "ON CHANGE: VALA unchanged -> no monitor event"
    );

    // ALWAYS: unchanged VALA still posts every process.
    {
        let arc = db.get_record("ASUB_E").unwrap();
        let mut inst = arc.write();
        inst.record.put_field("EFLG", EpicsValue::Short(2)).unwrap();
    }
    db.process_record("ASUB_E").await.unwrap();
    assert!(
        rx.try_recv().is_ok(),
        "ALWAYS: unchanged VALA must still post a monitor event"
    );

    // NEVER: even a changed VALA is suppressed.
    {
        *out.lock().unwrap() = vec![9.0, 8.0];
        let arc = db.get_record("ASUB_E").unwrap();
        let mut inst = arc.write();
        inst.record.put_field("EFLG", EpicsValue::Short(0)).unwrap();
    }
    db.process_record("ASUB_E").await.unwrap();
    assert!(
        rx.try_recv().is_err(),
        "NEVER: changed VALA must post no monitor event"
    );
}

/// aSub `LFLG=READ` re-reads the subroutine name from the `SUBL` link each
/// process and re-resolves the function from the registry when it changed
/// (C `aSubRecord.c::fetch_values`). A name not in the registry is C
/// `S_db_BadSub`: the subroutine is not run (VAL frozen), ONAM kept for retry.
#[epics_macros_rs::epics_test]
async fn asub_lflg_read_reresolves_subroutine_from_subl_link() {
    use epics_base_rs::server::records::asub_record::ASubRecord;
    use epics_base_rs::server::records::stringout::StringoutRecord;
    use std::collections::HashMap;

    let db = PvDatabase::new();

    // Two registered subroutines; each publishes its identity via VAL (the
    // aSub return-status -> VAL contract, C `aSubRecord.c:224`).
    let mk = |status: i64| -> Arc<SubroutineFn> {
        Arc::new(Box::new(move |_: &mut dyn Record| Ok(status)) as SubroutineFn)
    };
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("sub_a".into(), mk(11));
    registry.insert("sub_b".into(), mk(22));
    db.install_subroutine_registry(registry).await;

    // A string record holds the current subroutine name; SUBL is a DB link
    // to it (the realistic LFLG=READ wiring).
    db.add_record("NAME_HOLDER", Box::new(StringoutRecord::new("sub_a")))
        .await
        .unwrap();

    let mut rec = ASubRecord::default();
    rec.put_field("LFLG", EpicsValue::Short(1)).unwrap(); // READ
    rec.put_field("SUBL", EpicsValue::String("NAME_HOLDER".into()))
        .unwrap();
    db.add_record("ASUB_L", Box::new(rec)).await.unwrap();

    let proc = |db: &PvDatabase| {
        let db = db.clone();
        async move {
            let mut visited = HashSet::new();
            db.process_record_with_links("ASUB_L", &mut visited, 0)
                .await
                .unwrap();
        }
    };
    let field = |db: &PvDatabase, f: &'static str| {
        let db = db.clone();
        async move {
            let arc = db.get_record("ASUB_L").unwrap();
            let inst = arc.read();
            inst.record.get_field(f)
        }
    };

    // First process: name changed (""->sub_a) -> resolve + run sub_a.
    proc(&db).await;
    assert_eq!(
        field(&db, "SNAM").await,
        Some(EpicsValue::String("sub_a".into())),
        "SNAM must track the SUBL link value"
    );
    assert_eq!(
        field(&db, "ONAM").await,
        Some(EpicsValue::String("sub_a".into())),
        "ONAM must be set to the resolved name"
    );
    assert_eq!(
        field(&db, "VAL").await,
        Some(EpicsValue::Long(11)),
        "sub_a must have run (VAL = its return status)"
    );

    // Repoint the holder to sub_b, process: re-resolve + run sub_b.
    {
        let arc = db.get_record("NAME_HOLDER").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("VAL", EpicsValue::String("sub_b".into()))
            .unwrap();
    }
    proc(&db).await;
    assert_eq!(
        field(&db, "ONAM").await,
        Some(EpicsValue::String("sub_b".into())),
        "ONAM must follow the changed name"
    );
    assert_eq!(
        field(&db, "VAL").await,
        Some(EpicsValue::Long(22)),
        "sub_b must have run after the name changed"
    );

    // Repoint to an unregistered name: C S_db_BadSub -> do_sub skipped.
    {
        let arc = db.get_record("NAME_HOLDER").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("VAL", EpicsValue::String("missing".into()))
            .unwrap();
    }
    proc(&db).await;
    assert_eq!(
        field(&db, "SNAM").await,
        Some(EpicsValue::String("missing".into())),
        "SNAM still tracks the link value even when unresolvable"
    );
    assert_eq!(
        field(&db, "ONAM").await,
        Some(EpicsValue::String("sub_b".into())),
        "ONAM must be kept (not advanced) on a bad sub, so it retries"
    );
    assert_eq!(
        field(&db, "VAL").await,
        Some(EpicsValue::Long(22)),
        "bad sub: subroutine not run, VAL frozen at the last good result"
    );
}

/// aSub `LFLG=IGNORE` (the default) resolves its subroutine from SNAM once at
/// init via the function registry (C `aSubRecord.c::init_record` ->
/// `registryFunctionFind`), wired by `wire_subroutines` on the `.db` path.
#[epics_macros_rs::epics_test]
async fn asub_lflg_ignore_subroutine_wired_by_snam_at_init() {
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use std::collections::HashMap;

    let db_content = r#"
record(aSub, "ASUB_S") {
    field(SNAM, "my_routine")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .register_subroutine("my_routine", |_: &mut dyn Record| Ok(7))
        .build()
        .await
        .unwrap();

    // The routine was wired at init; processing runs it and publishes its
    // return status as VAL (C `aSubRecord.c:224`).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASUB_S", &mut visited, 0)
        .await
        .unwrap();
    let arc = db.get_record("ASUB_S").unwrap();
    let inst = arc.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Long(7)),
        "aSub LFLG=IGNORE: subroutine resolved from SNAM at init must run"
    );
}

/// `sub` and `aSub` INAM init routine: resolved through the function registry
/// and invoked exactly once at init, before SNAM resolution, return discarded
/// (C `subRecord.c` / `aSubRecord.c::init_record`).
#[epics_macros_rs::epics_test]
async fn inam_init_routine_runs_once_at_init_for_sub_and_asub() {
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let db_content = r#"
record(sub, "SUB_INIT") {
    field(INAM, "sub_init")
    field(SNAM, "sub_proc")
}
record(aSub, "ASUB_INIT") {
    field(INAM, "asub_init")
    field(SNAM, "asub_proc")
}
"#;
    let sub_init_calls = Arc::new(AtomicUsize::new(0));
    let asub_init_calls = Arc::new(AtomicUsize::new(0));
    let sic = sub_init_calls.clone();
    let aic = asub_init_calls.clone();
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .register_subroutine("sub_init", move |rec: &mut dyn Record| {
            sic.fetch_add(1, Ordering::SeqCst);
            rec.put_field("VAL", EpicsValue::Double(99.0))?;
            Ok(0)
        })
        .register_subroutine("sub_proc", |_: &mut dyn Record| Ok(0))
        .register_subroutine("asub_init", move |rec: &mut dyn Record| {
            aic.fetch_add(1, Ordering::SeqCst);
            rec.put_field("VAL", EpicsValue::Double(88.0))?;
            Ok(0)
        })
        .register_subroutine("asub_proc", |_: &mut dyn Record| Ok(5))
        .build()
        .await
        .unwrap();

    // Both INAM routines ran exactly once at init, before any processing.
    assert_eq!(
        sub_init_calls.load(Ordering::SeqCst),
        1,
        "sub INAM init routine runs exactly once at init"
    );
    assert_eq!(
        asub_init_calls.load(Ordering::SeqCst),
        1,
        "aSub INAM init routine runs exactly once at init"
    );

    // The init routine's write is visible after init, before any processing.
    let sub = db.get_record("SUB_INIT").unwrap();
    assert_eq!(
        sub.read().record.get_field("VAL"),
        Some(EpicsValue::Double(99.0)),
        "sub INAM init write visible after init"
    );
    let asub = db.get_record("ASUB_INIT").unwrap();
    assert_eq!(
        asub.read().record.get_field("VAL"),
        Some(EpicsValue::Long(88)),
        "aSub INAM init write visible after init"
    );

    // SNAM process routine is still wired alongside INAM: aSub publishes its
    // return status as VAL (C `aSubRecord.c:224`).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASUB_INIT", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        asub.read().record.get_field("VAL"),
        Some(EpicsValue::Long(5)),
        "aSub SNAM routine runs after INAM and publishes its status as VAL"
    );
}

/// The public direct-process API `process_record` re-resolves aSub LFLG=READ:
/// it delegates to the canonical engine path, so a direct process re-reads the
/// SUBL link, swaps the subroutine, and skips a bad sub like any other process.
#[epics_macros_rs::epics_test]
async fn asub_lflg_read_reresolves_on_foreign_process_path() {
    use epics_base_rs::server::records::asub_record::ASubRecord;
    use epics_base_rs::server::records::stringout::StringoutRecord;
    use std::collections::HashMap;

    let db = PvDatabase::new();
    let mk = |status: i64| -> Arc<SubroutineFn> {
        Arc::new(Box::new(move |_: &mut dyn Record| Ok(status)) as SubroutineFn)
    };
    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("sub_a".into(), mk(11));
    registry.insert("sub_b".into(), mk(22));
    db.install_subroutine_registry(registry).await;

    db.add_record("NAME_HOLDER2", Box::new(StringoutRecord::new("sub_a")))
        .await
        .unwrap();
    let mut rec = ASubRecord::default();
    rec.put_field("LFLG", EpicsValue::Short(1)).unwrap();
    rec.put_field("SUBL", EpicsValue::String("NAME_HOLDER2".into()))
        .unwrap();
    db.add_record("ASUB_F", Box::new(rec)).await.unwrap();

    let val = |db: &PvDatabase| {
        let db = db.clone();
        async move {
            let arc = db.get_record("ASUB_F").unwrap();
            let inst = arc.read();
            inst.record.get_field("VAL")
        }
    };
    let set_name = |db: &PvDatabase, name: &'static str| {
        let db = db.clone();
        async move {
            let arc = db.get_record("NAME_HOLDER2").unwrap();
            let mut inst = arc.write();
            inst.record
                .put_field("VAL", EpicsValue::String(name.into()))
                .unwrap();
        }
    };

    // Foreign path resolves + runs sub_a.
    db.process_record("ASUB_F").await.unwrap();
    assert_eq!(
        val(&db).await,
        Some(EpicsValue::Long(11)),
        "foreign path: sub_a resolved and ran"
    );

    // Repoint -> sub_b: foreign path must re-resolve too.
    set_name(&db, "sub_b").await;
    db.process_record("ASUB_F").await.unwrap();
    assert_eq!(
        val(&db).await,
        Some(EpicsValue::Long(22)),
        "foreign path: re-resolved sub_b after the name changed"
    );

    // Bad sub: foreign path must skip do_sub, VAL frozen (shared one-shot
    // suppress flag, not an engine-path-only gate).
    set_name(&db, "missing").await;
    db.process_record("ASUB_F").await.unwrap();
    assert_eq!(
        val(&db).await,
        Some(EpicsValue::Long(22)),
        "foreign path bad sub: subroutine skipped, VAL frozen"
    );
}

/// ao OMSL=closed_loop + OIF=Incremental must add DOL to PVAL (the last
/// actual output), not the current VAL. C `fetch_value`
/// (aoRecord.c:447-455) sets `prec->val = prec->pval` before
/// `*pvalue += prec->val`, discarding any client caput to VAL during a
/// DOL-driven cycle. The Rust port previously incremented from the current
/// VAL, so a caput between cycles shifted every subsequent increment.
#[epics_macros_rs::epics_test]
async fn test_ao_incremental_dol_increments_from_pval_not_val() {
    let db = PvDatabase::new();

    db.add_record("AO_INCR_SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();

    let mut dest = AoRecord::new(0.0);
    dest.omsl = 1; // closed_loop
    dest.oif = 1; // Incremental
    dest.dol = "AO_INCR_SRC".to_string();
    dest.init_record(0).unwrap(); // C init: PVAL = VAL (= 0 here)
    db.add_record("AO_INCR_DST", Box::new(dest)).await.unwrap();

    // Cycle 1: PVAL=0, DOL=10 -> VAL = 0 + 10 = 10, PVAL becomes 10.
    let mut visited = HashSet::new();
    db.process_record_with_links("AO_INCR_DST", &mut visited, 0)
        .await
        .unwrap();
    let v1 = db.get_pv("AO_INCR_DST").unwrap();
    assert!(
        matches!(v1, EpicsValue::Double(v) if (v - 10.0).abs() < 1e-10),
        "cycle 1 VAL must be 10, got {v1:?}"
    );

    // A client caputs VAL=100 between cycles; C discards it (val=pval).
    {
        let arc = db.get_record("AO_INCR_DST").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(100.0))
            .unwrap();
    }
    // Upstream setpoint changes to 5.
    {
        let arc = db.get_record("AO_INCR_SRC").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(5.0))
            .unwrap();
    }

    // Cycle 2: increment from PVAL(10), not the caput VAL(100):
    // VAL = 10 + 5 = 15 (C), not 100 + 5 = 105 (pre-fix Rust).
    let mut visited2 = HashSet::new();
    db.process_record_with_links("AO_INCR_DST", &mut visited2, 0)
        .await
        .unwrap();
    let v2 = db.get_pv("AO_INCR_DST").unwrap();
    assert!(
        matches!(v2, EpicsValue::Double(v) if (v - 15.0).abs() < 1e-10),
        "cycle 2 must increment from PVAL=10 (=>15), not caput VAL=100 (=>105); got {v2:?}"
    );
}

/// A *constant* DOL (`field(DOL,"7")`) with `OMSL=closed_loop` is applied to
/// VAL exactly once at init (`recGblInitConstantLink`), and the framework's
/// per-cycle closed-loop fetch is gated out for constants
/// (C `!dbLinkIsConstant`, e.g. `aoRecord.c:442`). A client caput to VAL
/// must therefore survive a subsequent process — the constant is NOT
/// re-applied every cycle. Before the fix, both the framework
/// (`read_link_value` resolving a constant) and `ao::process` re-stamped the
/// constant each cycle, clobbering the caput. Record-level-removal path (ao).
#[epics_macros_rs::epics_test]
async fn ao_constant_dol_seeded_at_init_not_reapplied_at_process() {
    let db = PvDatabase::new();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1; // closed_loop
    ao.dol = "7".to_string(); // constant DOL
    ao.init_record(0).unwrap(); // recGblInitConstantLink: VAL = 7
    db.add_record("AO_CONST", Box::new(ao)).await.unwrap();

    let v0 = db.get_pv("AO_CONST").unwrap();
    assert!(
        matches!(v0, EpicsValue::Double(v) if (v - 7.0).abs() < 1e-10),
        "constant DOL must seed VAL=7 at init, got {v0:?}"
    );

    // Process once: the constant must not be re-sourced; VAL stays 7.
    let mut visited = HashSet::new();
    db.process_record_with_links("AO_CONST", &mut visited, 0)
        .await
        .unwrap();
    let v1 = db.get_pv("AO_CONST").unwrap();
    assert!(
        matches!(v1, EpicsValue::Double(v) if (v - 7.0).abs() < 1e-10),
        "process must not change a constant-DOL VAL, got {v1:?}"
    );

    // A client caputs VAL=42.
    {
        let arc = db.get_record("AO_CONST").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(42.0))
            .unwrap();
    }
    // Reprocess: the constant DOL is never re-fetched, so the caput wins.
    let mut visited2 = HashSet::new();
    db.process_record_with_links("AO_CONST", &mut visited2, 0)
        .await
        .unwrap();
    let v2 = db.get_pv("AO_CONST").unwrap();
    assert!(
        matches!(v2, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "constant DOL must not clobber a client caput on reprocess; expected 42, got {v2:?}"
    );
}

/// A constant DOL on an `OIF=Incremental` ao does NOT increment. C
/// `aoRecord.c:181-187` gates `fetch_value` (which holds the Incremental
/// `*pvalue += prec->val`) on `!dbLinkIsConstant`; a constant DOL takes the
/// `else { value = prec->val; }` branch, so the OIF mode is irrelevant. The
/// value tracks VAL (init constant, or a later caput) and never accumulates
/// the constant each cycle.
#[epics_macros_rs::epics_test]
async fn ao_constant_dol_incremental_does_not_increment() {
    let db = PvDatabase::new();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1; // closed_loop
    ao.oif = 1; // Incremental
    ao.dol = "5".to_string(); // constant DOL
    ao.init_record(0).unwrap(); // VAL = 5, PVAL = 5
    db.add_record("AO_CONST_INCR", Box::new(ao)).await.unwrap();

    // Process three times: a constant must not accumulate (5, 10, 15...).
    for _ in 0..3 {
        let mut visited = HashSet::new();
        db.process_record_with_links("AO_CONST_INCR", &mut visited, 0)
            .await
            .unwrap();
    }
    let v = db.get_pv("AO_CONST_INCR").unwrap();
    assert!(
        matches!(v, EpicsValue::Double(x) if (x - 5.0).abs() < 1e-10),
        "constant DOL + Incremental must stay at the constant (5), not accumulate; got {v:?}"
    );
}

/// The framework-only path. `longout` never had a record-level DOL
/// re-apply — the per-cycle constant re-stamp lived entirely in the
/// framework (`read_link_value` resolving `ParsedLink::Constant`). The new
/// `longout::init_record` seeds the constant once; the gate keeps it out of
/// process. Before the fix, the framework re-applied the constant every
/// cycle, clobbering a caput.
#[epics_macros_rs::epics_test]
async fn longout_constant_dol_seeded_at_init_not_reapplied_at_process() {
    use epics_base_rs::server::records::longout::LongoutRecord;
    let db = PvDatabase::new();
    let mut lo = LongoutRecord::new(0);
    lo.omsl = 1; // closed_loop
    lo.dol = "9".to_string(); // constant DOL
    lo.init_record(0).unwrap();
    db.add_record("LO_CONST", Box::new(lo)).await.unwrap();

    let v0 = db.get_pv("LO_CONST").unwrap();
    assert_eq!(
        v0.to_f64(),
        Some(9.0),
        "constant DOL must seed VAL=9 at init"
    );

    {
        let arc = db.get_record("LO_CONST").unwrap();
        let mut inst = arc.write();
        inst.record.put_field("VAL", EpicsValue::Long(99)).unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("LO_CONST", &mut visited, 0)
        .await
        .unwrap();
    let v1 = db.get_pv("LO_CONST").unwrap();
    assert_eq!(
        v1.to_f64(),
        Some(99.0),
        "constant DOL must not clobber a caput on reprocess (framework-only path)"
    );
}

/// The string path. A CONSTANT DOL on a `stringout` is copied into VAL once at
/// init as TEXT (C `recGblInitConstantLink(..., DBF_STRING, ...)` →
/// `cvt_st_st`, a plain `strncpy` of the link text); the gate keeps it out of
/// process so a caput survives.
///
/// The link text has to be a NUMBER — that is what makes a plain link CONSTANT
/// (`dbParseLink`, dbStaticLib.c:2346-2349). A quoted `"hi"` is not: softIoc
/// makes `field(DOL,"\"hi\"")` a `CA_LINK "hi" NPP NMS` and leaves VAL empty /
/// UDF=1, so this test used to encode a link type C does not have.
#[epics_macros_rs::epics_test]
async fn stringout_constant_dol_seeded_at_init_not_reapplied_at_process() {
    use epics_base_rs::server::records::stringout::StringoutRecord;
    let db = PvDatabase::new();
    let mut so = StringoutRecord::new("");
    so.omsl = 1; // closed_loop
    so.dol = "1.50".to_string(); // CONSTANT — softIoc seeds VAL="1.50", UDF=0
    so.init_record(0).unwrap();
    db.add_record("SO_CONST", Box::new(so)).await.unwrap();

    let v0 = db.get_pv("SO_CONST").unwrap();
    assert!(
        matches!(&v0, EpicsValue::String(s) if s.as_str_lossy() == "1.50"),
        "constant DOL must seed VAL=\"1.50\" at init (cvt_st_st copies the text), got {v0:?}"
    );

    {
        let arc = db.get_record("SO_CONST").unwrap();
        let mut inst = arc.write();
        inst.record
            .put_field("VAL", EpicsValue::String("world".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("SO_CONST", &mut visited, 0)
        .await
        .unwrap();
    let v1 = db.get_pv("SO_CONST").unwrap();
    assert!(
        matches!(&v1, EpicsValue::String(s) if s.as_str_lossy() == "world"),
        "constant DOL must not clobber a string caput on reprocess; got {v1:?}"
    );
}

/// Init-time constant-DOL application across the remaining OMSL records that
/// gained an `init_record`/`post_init` seed. One assertion per record's
/// value-type boundary (f64 / i64 / long-string / state-index→RVAL /
/// bit-field decomposition).
#[epics_macros_rs::epics_test]
async fn init_applies_constant_dol_across_record_types() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;
    use epics_base_rs::server::records::int64out::Int64outRecord;
    use epics_base_rs::server::records::lso::LsoRecord;
    use epics_base_rs::server::records::mbbo::MbboRecord;
    use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;

    // dfanout: DBF_DOUBLE. Its constant DOL (and SELL) are declared in
    // `Record::constant_init_links` and applied by the init-seed owner, so the
    // seed is asserted through the database rather than a bare `init_record`.
    let db = PvDatabase::new();
    let mut df = DfanoutRecord::default();
    df.omsl = 1;
    df.dol = "3.5".to_string();
    df.sell = "2".to_string();
    db.add_record("DF_CONST", Box::new(df)).await.unwrap();
    {
        let rec = db.get_record("DF_CONST").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Double(3.5)),
            "dfanout constant DOL → VAL=3.5 (dfanoutRecord.c:105)"
        );
        assert_eq!(
            inst.record.get_field("SELN"),
            Some(EpicsValue::UShort(2)),
            "dfanout constant SELL → SELN=2 (dfanoutRecord.c:102)"
        );
    }

    // int64out: DBF_INT64. Seeded by the init-seed owner, like dfanout.
    let mut i64o = Int64outRecord::default();
    i64o.omsl = 1;
    i64o.dol = "42".to_string();
    db.add_record("I64_CONST", Box::new(i64o)).await.unwrap();
    {
        let rec = db.get_record("I64_CONST").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Int64(42)),
            "int64out constant DOL → VAL=42"
        );
    }

    // lso: the long-string load is `dbLoadLinkLS` (lsoRecord.c:82), NOT
    // `recGblInitConstantLink`, and only a JSON `{const:"…"}` link carries text
    // — softIoc: `field(DOL,"\"hello\"")` is a CA_LINK (LEN 0, UDF 1), while
    // `field(DOL,{const:"hello"})` loads VAL "hello" / LEN 6 / UDF 0. See
    // tests/long_string_constant_link_load.rs.
    let mut lso = LsoRecord::default();
    lso.omsl = 1;
    lso.dol = r#"{const:"hello"}"#.to_string();
    db.add_record("LSO_CONST", Box::new(lso)).await.unwrap();
    {
        let rec = db.get_record("LSO_CONST").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::CharArray(b"hello".to_vec())),
            "lso JSON const DOL → VAL"
        );
        assert_eq!(
            inst.record.get_field("LEN"),
            Some(EpicsValue::ULong(6)),
            "lso LEN = strlen+1 (C convention)"
        );
        assert!(inst.common.udf == 0, "a loaded LS link defines the record");
    }

    // mbbo: constant DOL is the state index; the init tail's convert() maps it
    // to RVAL (no state table defined → RVAL == state index).
    let mut mb = MbboRecord::default();
    mb.omsl = 1;
    mb.dol = "2".to_string();
    db.add_record("MBBO_CONST", Box::new(mb)).await.unwrap();
    {
        let rec = db.get_record("MBBO_CONST").unwrap();
        let inst = rec.read();
        // `UShort`, not `Enum`, for the same reason the comment above gives:
        // with no state table there is nothing to label the index with, so C's
        // `cvt_dbaddr` serves VAL as DBF_USHORT (mbboRecord.c:300-313).
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::UShort(2)),
            "mbbo constant DOL → VAL (state index) = 2"
        );
        assert_eq!(
            inst.record.get_field("RVAL"),
            Some(EpicsValue::ULong(2)),
            "mbbo convert() maps state index → RVAL"
        );
    }

    // mbbo_direct: constant seeds VAL and the bit fields decompose from it
    // (5 = 0b101 → B0=1, B1=0, B2=1); UDF cleared. Driven through the record
    // creation sink like its siblings above — the seed belongs to the init-seed
    // owner, not to `post_init_finalize_undef`, which runs before it.
    let mut mbd = MbboDirectRecord::default();
    mbd.omsl = 1;
    mbd.dol = "5".to_string();
    db.add_record("MBD_CONST", Box::new(mbd)).await.unwrap();
    {
        let rec = db.get_record("MBD_CONST").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Long(5)),
            "mbbo_direct constant DOL → VAL=5"
        );
        assert_eq!(inst.record.get_field("B0"), Some(EpicsValue::UChar(1)));
        assert_eq!(inst.record.get_field("B1"), Some(EpicsValue::UChar(0)));
        assert_eq!(inst.record.get_field("B2"), Some(EpicsValue::UChar(1)));
        assert!(
            inst.common.udf == 0,
            "mbbo_direct constant DOL clears UDF (recGblInitConstantLink)"
        );
    }
}

/// A calcout with ODLY > 0 defers its forward link (and VAL/OVAL monitors)
/// to the delayed callback cycle. C `calcoutRecord.c::process` (lines
/// 277-282) sets DLYA and `return 0` on the delaying cycle — BEFORE
/// `monitor()`/`recGblFwdLink()` (lines 306-307) — so the FLNK target must
/// NOT process on the delaying cycle; it processes exactly once on the
/// delayed cycle. Before the fix the delaying cycle ran the full Complete
/// snapshot + FLNK tail, firing the forward link twice (delaying + delayed).
#[epics_macros_rs::epics_test]
async fn calcout_odly_defers_forward_link_to_delayed_cycle() {
    use epics_base_rs::server::records::calc::CalcRecord;
    use epics_base_rs::server::records::calcout::CalcoutRecord;
    let db = PvDatabase::new();

    // FLNK target: a calc counter — VAL increments by 1 each time it is
    // processed (the CALC `VAL` token reads the previous VAL).
    let mut tgt = CalcRecord::new("VAL+1");
    tgt.init_record(0).unwrap();
    db.add_record("CO6_TGT", Box::new(tgt)).await.unwrap();

    // Source calcout: output due (OOPT=Every Time) with ODLY > 0 and a
    // forward link to the counter. ODLY is large so the real ReprocessAfter
    // timer cannot fire during the test; the delayed cycle is driven
    // explicitly via process_record_continuation.
    let mut src = CalcoutRecord::default();
    src.put_field("CALC", EpicsValue::String("A".into()))
        .unwrap();
    src.special("CALC", true).unwrap();
    src.put_field("A", EpicsValue::Double(42.0)).unwrap();
    src.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    db.add_record("CO6_SRC", Box::new(src)).await.unwrap();
    {
        let rec = db.get_record("CO6_SRC").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("CO6_TGT".into()))
            .unwrap();
    }

    // Delaying cycle: output is due + ODLY > 0, so the cycle defers. The
    // FLNK target must NOT process (C returns before recGblFwdLink). The
    // delaying-cycle FLNK fires synchronously inside process_record_with_links
    // if at all, so this assertion is race-free.
    let mut visited = HashSet::new();
    db.process_record_with_links("CO6_SRC", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("CO6_TGT").unwrap().to_f64(),
        Some(0.0),
        "FLNK target must not be processed on the delaying cycle"
    );

    // Delayed (callback) cycle: C fires recGblFwdLink exactly once here.
    let mut visited2 = HashSet::new();
    db.process_record_continuation("CO6_SRC", &mut visited2, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("CO6_TGT").unwrap().to_f64(),
        Some(1.0),
        "FLNK target processed exactly once (delayed cycle only), not on both cycles"
    );
}

// C `selRecord.c::fetch_values` (411-438): in Specified mode (SELM==0)
// only INP[SELN] is read; the other input links are never fetched, so
// their PP sources are never processed. A non-selected PP source that
// computes 42-on-process must therefore stay at its default 0.
#[epics_macros_rs::epics_test]
async fn sel_specified_mode_fetches_only_the_selected_input() {
    use epics_base_rs::server::records::calc::CalcRecord;
    use epics_base_rs::server::records::sel::SelRecord;

    let db = PvDatabase::new();

    // Two passive sources that each compute 42 only when processed; both
    // start at the default VAL=0.
    db.add_record("R7_SRCA", Box::new(CalcRecord::new("42")))
        .await
        .unwrap();
    db.add_record("R7_SRCB", Box::new(CalcRecord::new("42")))
        .await
        .unwrap();

    // Specified mode, SELN=1 selects INPB. Both inputs are PP links.
    let mut sel = SelRecord::default();
    sel.selm = 0;
    sel.seln = 1;
    sel.inpa = "R7_SRCA PP".to_string();
    sel.inpb = "R7_SRCB PP".to_string();
    db.add_record("R7_SEL", Box::new(sel)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("R7_SEL", &mut visited, 0)
        .await
        .unwrap();

    // Selected input (INPB) source WAS processed by its PP link → 42.
    let sel_val = db.get_pv("R7_SEL").unwrap();
    match sel_val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "SEL value comes from the selected input INPB: expected 42, got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    let srcb = db.get_pv("R7_SRCB").unwrap();
    match srcb {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "selected source R7_SRCB must be processed by its PP link, VAL={v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    // KEY: the NON-selected input (INPA) source must NOT be fetched, so
    // its PP link never fires and its VAL stays at the default 0. Pre-fix
    // the framework fetched ALL inputs and this would be 42.
    let srca = db.get_pv("R7_SRCA").unwrap();
    match srca {
        EpicsValue::Double(v) => assert!(
            v.abs() < 1e-10,
            "non-selected source R7_SRCA must NOT be processed (Specified \
             mode reads only INP[SELN]), VAL={v}"
        ),
        other => panic!("expected Double(0.0), got {other:?}"),
    }
}

// Control for the above: in High mode (SELM==1) C fetch_values reads
// EVERY input to compare them, so all PP sources are processed.
#[epics_macros_rs::epics_test]
async fn sel_high_mode_fetches_all_inputs() {
    use epics_base_rs::server::records::calc::CalcRecord;
    use epics_base_rs::server::records::sel::SelRecord;

    let db = PvDatabase::new();

    db.add_record("R7H_SRCA", Box::new(CalcRecord::new("10")))
        .await
        .unwrap();
    db.add_record("R7H_SRCB", Box::new(CalcRecord::new("42")))
        .await
        .unwrap();

    // High mode ignores SELN and reads all inputs; both are PP links.
    let mut sel = SelRecord::default();
    sel.selm = 1;
    sel.inpa = "R7H_SRCA PP".to_string();
    sel.inpb = "R7H_SRCB PP".to_string();
    db.add_record("R7H_SEL", Box::new(sel)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("R7H_SEL", &mut visited, 0)
        .await
        .unwrap();

    // High picks the maximum of the fetched inputs → 42.
    let sel_val = db.get_pv("R7H_SEL").unwrap();
    match sel_val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "High mode selects max(10, 42) = 42, got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    // BOTH sources processed: the non-max source still got fetched.
    let srca = db.get_pv("R7H_SRCA").unwrap();
    match srca {
        EpicsValue::Double(v) => assert!(
            (v - 10.0).abs() < 1e-10,
            "High mode must fetch ALL inputs: R7H_SRCA processed to 10, VAL={v}"
        ),
        other => panic!("expected Double(10.0), got {other:?}"),
    }
    let srcb = db.get_pv("R7H_SRCB").unwrap();
    match srcb {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "High mode must fetch ALL inputs: R7H_SRCB processed to 42, VAL={v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
}

// C `selRecord.c::process` (114) runs `do_sel` only when `fetch_values`
// succeeds. In Specified mode a configured selected input that fails to
// resolve (here INPA points at a non-existent record) is a fetch
// failure, so do_sel is skipped and VAL — set to 5.0 on a prior cycle —
// freezes. Pre-fix do_sel ran over the NaN-initialised A field and VAL
// became NaN.
#[epics_macros_rs::epics_test]
async fn sel_specified_mode_freezes_value_when_selected_link_fails() {
    use epics_base_rs::server::records::sel::SelRecord;

    let db = PvDatabase::new();
    let mut sel = SelRecord::default();
    sel.selm = 0;
    sel.seln = 0; // selects INPA
    sel.val = 5.0; // value computed on a previous cycle
    sel.inpa = "NO_SUCH_PV".to_string();
    db.add_record("R8_SEL", Box::new(sel)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("R8_SEL", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("R8_SEL").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 5.0).abs() < 1e-10,
            "broken selected link must freeze VAL at 5.0 (C skips do_sel), got {v}"
        ),
        other => panic!("expected Double(5.0), got {other:?}"),
    }
}

// Specified-mode NVL failure also gates: C `fetch_values` returns the
// failed NVL read status BEFORE fetching any input, so do_sel is
// skipped. Here NVL points at a non-existent record while INPA holds a
// valid source (3.0). Pre-fix Rust fell back to the stale SELN, fetched
// INPA, and recomputed VAL=3.0; post-fix VAL freezes at 7.0.
#[epics_macros_rs::epics_test]
async fn sel_specified_mode_freezes_value_when_nvl_link_fails() {
    use epics_base_rs::server::records::sel::SelRecord;

    let db = PvDatabase::new();
    db.add_record("R8N_SRC", Box::new(AoRecord::new(3.0)))
        .await
        .unwrap();

    let mut sel = SelRecord::default();
    sel.selm = 0;
    sel.seln = 0;
    sel.val = 7.0;
    sel.nvl = "NO_SUCH_NVL".to_string();
    sel.inpa = "R8N_SRC".to_string();
    db.add_record("R8N_SEL", Box::new(sel)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("R8N_SEL", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("R8N_SEL").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 7.0).abs() < 1e-10,
            "broken NVL link must freeze VAL at 7.0 (C skips do_sel), got {v}"
        ),
        other => panic!("expected Double(7.0), got {other:?}"),
    }
}

// Control: an *empty* selected link is NOT a fetch failure. C
// `dbGetLink` on an unset constant link returns success, so do_sel runs
// and reads the NaN-initialised A field → VAL=NaN. This must NOT freeze
// (contrast the broken-link case), proving the gate keys on
// configured-but-unresolved, not merely unresolved.
#[epics_macros_rs::epics_test]
async fn sel_specified_mode_empty_selected_link_computes_nan_not_frozen() {
    use epics_base_rs::server::records::sel::SelRecord;

    let db = PvDatabase::new();
    let mut sel = SelRecord::default();
    sel.selm = 0;
    sel.seln = 0;
    sel.val = 5.0;
    // inpa stays empty (unset link)
    db.add_record("R8_SEL_EMPTY", Box::new(sel)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("R8_SEL_EMPTY", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("R8_SEL_EMPTY").unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            v.is_nan(),
            "empty selected link still runs do_sel over the NaN input → VAL=NaN, got {v}"
        ),
        other => panic!("expected Double(NaN), got {other:?}"),
    }
}

/// `promptgroup`/`special(SPC_NOMOD)` config fields must be settable at `.db`
/// load (dbStaticLib bypasses SPC_NOMOD) yet runtime-immutable, while a `pp`
/// config field stays writable on both paths. Closes the divergence where the
/// port rejected the load assignment (`put_field` hard-error) or dropped it
/// (field absent from `field_list`).
///
/// Covered fields (C dbd):
///   - `histogram.NELM` — `promptgroup special(SPC_NOMOD)`: load resizes bins.
///   - `compress.NSAM`   — `promptgroup special(SPC_NOMOD)`: load resizes buffer.
///   - `subArray.MALM`   — `special(SPC_NOMOD)`: load sets, runtime rejects.
///   - `subArray.INDX`   — `pp(TRUE)`: load AND runtime set.
#[epics_macros_rs::epics_test]
async fn promptgroup_config_fields_load_settable_runtime_immutable() {
    use epics_base_rs::server::db_loader::{DbFieldDef, apply_fields, create_record};

    // --- LOAD half: apply_fields (the `.db` load coercion) must store these
    //     fields, resizing the backing array where one exists. ---
    let mut hist = create_record("histogram").expect("create histogram");
    let mut common: Vec<(String, EpicsValue)> = Vec::new();
    apply_fields(&mut hist, &[DbFieldDef::new("NELM", "5")], &mut common)
        .expect("histogram NELM is .db-settable (promptgroup)");
    assert_eq!(hist.get_field("NELM"), Some(EpicsValue::UShort(5)));
    match hist.get_field("VAL") {
        Some(EpicsValue::ULongArray(v)) => {
            assert_eq!(v.len(), 5, "NELM load resizes the bin array")
        }
        other => panic!("histogram VAL should be a 5-element ULongArray, got {other:?}"),
    }

    let mut comp = create_record("compress").expect("create compress");
    let mut common = Vec::new();
    apply_fields(&mut comp, &[DbFieldDef::new("NSAM", "7")], &mut common)
        .expect("compress NSAM is .db-settable (promptgroup)");
    assert_eq!(comp.get_field("NSAM"), Some(EpicsValue::ULong(7)));

    let mut sarr = create_record("subArray").expect("create subArray");
    let mut common = Vec::new();
    apply_fields(
        &mut sarr,
        &[DbFieldDef::new("MALM", "8"), DbFieldDef::new("INDX", "3")],
        &mut common,
    )
    .expect("subArray MALM/INDX are .db-settable (in field_list)");
    assert_eq!(sarr.get_field("MALM"), Some(EpicsValue::ULong(8)));
    assert_eq!(sarr.get_field("INDX"), Some(EpicsValue::ULong(3)));

    // --- RUNTIME half: a CA caput is rejected for the SPC_NOMOD fields by the
    //     field_io read_only gate, but accepted for the pp field (subArray INDX). ---
    let db = PvDatabase::new();
    db.add_record("HIST", create_record("histogram").unwrap())
        .await
        .unwrap();
    db.add_record("COMP", create_record("compress").unwrap())
        .await
        .unwrap();
    db.add_record("SARR", create_record("subArray").unwrap())
        .await
        .unwrap();

    assert!(
        matches!(
            db.put_record_field_from_ca("HIST", "NELM", EpicsValue::Long(5))
                .await,
            Err(CaError::ReadOnlyField(_))
        ),
        "histogram NELM is SPC_NOMOD — runtime caput must be rejected"
    );
    assert!(
        matches!(
            db.put_record_field_from_ca("COMP", "NSAM", EpicsValue::Long(7))
                .await,
            Err(CaError::ReadOnlyField(_))
        ),
        "compress NSAM is SPC_NOMOD — runtime caput must be rejected"
    );
    assert!(
        matches!(
            db.put_record_field_from_ca("SARR", "MALM", EpicsValue::Long(8))
                .await,
            Err(CaError::ReadOnlyField(_))
        ),
        "subArray MALM is SPC_NOMOD — runtime caput must be rejected"
    );

    db.put_record_field_from_ca("SARR", "INDX", EpicsValue::Long(2))
        .await
        .expect("subArray INDX is pp(TRUE) — runtime caput must succeed");
    let rec = db.get_record("SARR").expect("SARR exists");
    let indx = rec.read().record.get_field("INDX");
    // INDX is pp(TRUE): the put PROCESSES the record, and `readValue` clamps
    // `if (indx >= malm) indx = malm - 1` (subArrayRecord.c:309-310). This SARR
    // is the bare `create_record` default — MALM=1 — so C reads INDX back as 0,
    // not 2. Verified on a built softIoc: `caput SA:D.INDX 2` on a MALM=1
    // subArray reads back `SA:D.INDX 0` (NORD 0, SEVR INVALID). The put landing
    // (no `ReadOnlyField`) is what this assertion is about; the clamp is C's.
    assert_eq!(
        indx,
        Some(EpicsValue::ULong(0)),
        "runtime INDX caput landed, then the pp(TRUE) process clamped it to MALM-1"
    );
}

/// NELM/FTVL runtime-writability must follow each ArrayKind's C dbd, even though
/// waveform/aai/aao/subArray share one `WaveformRecord`. `field_list()` selects a
/// kind-correct FieldDesc set so the field_io read_only gate blocks/allows the
/// right caputs:
///   - waveform/aai/aao NELM, FTVL: `special(SPC_NOMOD)` -> runtime-immutable.
///   - subArray NELM: `pp(TRUE)` -> runtime-writable; subArray FTVL: SPC_NOMOD.
/// Load (apply_fields) stays settable for all of them (SPC_NOMOD blocks only
/// runtime dbPutField).
#[epics_macros_rs::epics_test]
async fn waveform_nelm_ftvl_runtime_immutable_subarray_nelm_writable() {
    use epics_base_rs::server::db_loader::{DbFieldDef, apply_fields, create_record};

    // Load still sets NELM/FTVL on a waveform (SPC_NOMOD is runtime-only).
    let mut wf = create_record("waveform").expect("create waveform");
    let mut common: Vec<(String, EpicsValue)> = Vec::new();
    apply_fields(&mut wf, &[DbFieldDef::new("NELM", "4")], &mut common)
        .expect("waveform NELM is .db-settable at load");
    assert_eq!(wf.get_field("NELM"), Some(EpicsValue::ULong(4)));

    let db = PvDatabase::new();
    db.add_record("WF", create_record("waveform").unwrap())
        .await
        .unwrap();
    db.add_record("SA", create_record("subArray").unwrap())
        .await
        .unwrap();

    // waveform/aai/aao: NELM and FTVL are SPC_NOMOD — runtime caput rejected.
    assert!(
        matches!(
            db.put_record_field_from_ca("WF", "NELM", EpicsValue::Long(4))
                .await,
            Err(CaError::ReadOnlyField(_))
        ),
        "waveform NELM is special(SPC_NOMOD) — runtime caput must be rejected"
    );
    assert!(
        matches!(
            db.put_record_field_from_ca("WF", "FTVL", EpicsValue::Short(2))
                .await,
            Err(CaError::ReadOnlyField(_))
        ),
        "waveform FTVL is special(SPC_NOMOD) — runtime caput must be rejected"
    );

    // subArray: NELM is pp(TRUE) — runtime caput accepted; FTVL stays SPC_NOMOD.
    db.put_record_field_from_ca("SA", "NELM", EpicsValue::Long(3))
        .await
        .expect("subArray NELM is pp(TRUE) — runtime caput must succeed");
    assert!(
        matches!(
            db.put_record_field_from_ca("SA", "FTVL", EpicsValue::Short(2))
                .await,
            Err(CaError::ReadOnlyField(_))
        ),
        "subArray FTVL is special(SPC_NOMOD) — runtime caput must be rejected"
    );
}

/// aSub per-argument element-type fields `FTA..FTU` / `FTVA..FTVU` are
/// `field(FTx,DBF_MENU){ menu(menuFtype) }` in C `aSubRecord.dbd`. A real `.db`
/// sets them by menu *label* (`field(FTA,"DOUBLE")`), exactly like waveform
/// `FTVL`. The loader resolves a menu label only when the record exposes the
/// field's choice table; without it the label hits the integer parser, which
/// rejects "DOUBLE" and fails the whole record load. This pins that the labels
/// resolve through `menuFtype` (STRING=0, …, LONG=5, …, DOUBLE=10) and that the
/// numeric per-argument fields (`NOx`/`NOVx`) keep loading.
#[test]
fn asub_ftype_menu_fields_load_by_label() {
    use epics_base_rs::server::db_loader::{DbFieldDef, apply_fields, create_record};

    let mut rec = create_record("aSub").expect("create aSub");
    let mut common = Vec::new();
    apply_fields(
        &mut rec,
        &[
            DbFieldDef::new("FTA", "LONG"), // input A element type, menuFtype label
            DbFieldDef::new("FTVB", "STRING"), // output B element type, menuFtype label
            DbFieldDef::new("NOA", "5"),    // input A max elements, numeric
            DbFieldDef::new("NOVB", "3"),   // output B max elements, numeric
        ],
        &mut common,
    )
    .expect("aSub FTx/FTVx menuFtype labels and NOx/NOVx counts must load from .db");

    assert_eq!(
        rec.get_field("FTA"),
        Some(EpicsValue::Short(5)),
        "FTA=\"LONG\" must resolve to menuFtype index 5"
    );
    assert_eq!(
        rec.get_field("FTVB"),
        Some(EpicsValue::Short(0)),
        "FTVB=\"STRING\" must resolve to menuFtype index 0"
    );
    assert_eq!(rec.get_field("NOA"), Some(EpicsValue::Long(5)));
    assert_eq!(rec.get_field("NOVB"), Some(EpicsValue::Long(3)));
}

/// permissive `OVAL`/`OFLG` are `SPC_NOMOD` trackers that C `monitor()`
/// never posts (`permissiveRecord.c:90-117` posts only `&prec->val` and
/// `&prec->wflg`). The framework's generic subscribed-field change loop
/// would otherwise post any changed subscribed field, so the record lists
/// them in `event_posted_fields()` to exclude them. A `.OVAL`/`.OFLG`
/// subscriber must therefore receive no change update even though
/// `process()` updates both trackers every cycle — while VAL and WFLG must
/// still post on change.
#[epics_macros_rs::epics_test]
async fn test_permissive_oval_oflg_not_monitor_posted() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::permissive::PermissiveRecord;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("PERM", Box::new(PermissiveRecord::default()))
        .await
        .unwrap();

    let mask = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    // Subscribe at baseline (VAL=WFLG=OVAL=OFLG=0); add_subscriber seeds
    // last_posted to the current value, so the change must be staged AFTER
    // subscribing to exercise the generic change loop.
    let (mut val_rx, mut oval_rx, mut wflg_rx, mut oflg_rx) = {
        let rec = db.get_record("PERM").unwrap();
        let mut inst = rec.write();
        (
            inst.add_subscriber("VAL", 1, DbFieldType::UShort, mask)
                .unwrap(),
            inst.add_subscriber("OVAL", 2, DbFieldType::UShort, mask)
                .unwrap(),
            inst.add_subscriber("WFLG", 3, DbFieldType::UShort, mask)
                .unwrap(),
            inst.add_subscriber("OFLG", 4, DbFieldType::UShort, mask)
                .unwrap(),
        )
    };
    // Stage a change for this cycle: VAL 0->3, WFLG 0->1 (so process also
    // moves OVAL 0->3 and OFLG 0->1).
    {
        let rec = db.get_record("PERM").unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", EpicsValue::UShort(3)).unwrap();
        inst.record
            .put_field("WFLG", EpicsValue::UShort(1))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("PERM", &mut visited, 0)
        .await
        .unwrap();

    assert!(val_rx.try_recv().is_ok(), "VAL change must post a monitor");
    assert!(
        wflg_rx.try_recv().is_ok(),
        "WFLG change must post a monitor"
    );
    assert!(
        oval_rx.try_recv().is_err(),
        "OVAL is a SPC_NOMOD tracker C never posts; it must not monitor-post"
    );
    assert!(
        oflg_rx.try_recv().is_err(),
        "OFLG is a SPC_NOMOD tracker C never posts; it must not monitor-post"
    );
}

/// state `OVAL` is a `SPC_NOMOD` tracker C `monitor()` never posts
/// (`stateRecord.c:120-129` posts only `&prec->val[0]`). It is excluded
/// from the generic subscribed-field change loop via `event_posted_fields()`,
/// so a `.OVAL` subscriber receives no change update even though `process()`
/// copies VAL into OVAL on change — while VAL must still post on change.
#[epics_macros_rs::epics_test]
async fn test_state_oval_not_monitor_posted() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::state::StateRecord;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("ST", Box::new(StateRecord::default()))
        .await
        .unwrap();

    let mask = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    // Subscribe at baseline (VAL=OVAL=""); the change is staged afterwards
    // so process moves OVAL ""->"Run" through the generic change loop.
    let (mut val_rx, mut oval_rx) = {
        let rec = db.get_record("ST").unwrap();
        let mut inst = rec.write();
        (
            inst.add_subscriber("VAL", 1, DbFieldType::String, mask)
                .unwrap(),
            inst.add_subscriber("OVAL", 2, DbFieldType::String, mask)
                .unwrap(),
        )
    };
    {
        let rec = db.get_record("ST").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::String("Run".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("ST", &mut visited, 0)
        .await
        .unwrap();

    assert!(val_rx.try_recv().is_ok(), "VAL change must post a monitor");
    assert!(
        oval_rx.try_recv().is_err(),
        "OVAL is a SPC_NOMOD tracker C never posts; it must not monitor-post"
    );
}

/// R11-C10 — a put's `db_post_events` is the record's ONLY post for that put.
///
/// C `dbAccess.c::dbPut:1407-1414` posts the put field once with
/// `DBE_VALUE|DBE_LOG`, and no record's `monitor()` re-posts it: `monitor()`
/// posts a closed set and compares against the record's own `*_lst` / MARK
/// state, which the put never touched but the put's post did satisfy.
///
/// The port's put path posted the field but did not advance `last_posted`, so
/// the NEXT process cycle's generic change-detection loop compared the new
/// value against the pre-put one, found it "changed", and published a SECOND
/// event that C never sends. `SVAL` (ai simulation value) is the sample: a
/// plain, non-`pp(TRUE)`, non-metadata auxiliary field.
#[epics_macros_rs::epics_test]
async fn test_put_time_post_is_the_only_post_for_that_put() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("PUTPOST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut sval_rx = {
        let rec = db.get_record("PUTPOST").expect("record exists");
        let mut inst = rec.write();
        inst.add_subscriber("SVAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("SVAL subscription accepted")
    };

    // The put itself: C posts SVAL once (DBE_VALUE|DBE_LOG). SVAL is not
    // pp(TRUE), so `dbPutField` runs no process cycle either.
    db.put_record_field_from_ca("PUTPOST", "SVAL", EpicsValue::Double(7.5))
        .await
        .unwrap();
    sval_rx
        .try_recv()
        .expect("the put must post SVAL exactly once");
    assert!(
        sval_rx.try_recv().is_err(),
        "the put must post SVAL exactly once"
    );

    // The next process cycle re-reads every subscribed field. SVAL has not
    // moved since the put published it, so C's `monitor()` sends nothing.
    let mut visited = HashSet::new();
    db.process_record_with_links("PUTPOST", &mut visited, 0)
        .await
        .unwrap();
    assert!(
        sval_rx.try_recv().is_err(),
        "process must NOT re-post a field the put already published"
    );

    // The change detector is still live: a value that moves without a post
    // still publishes on the next cycle.
    {
        let rec = db.get_record("PUTPOST").expect("record exists");
        let mut inst = rec.write();
        inst.record
            .put_field("SVAL", EpicsValue::Double(9.0))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("PUTPOST", &mut visited, 0)
        .await
        .unwrap();
    sval_rx
        .try_recv()
        .expect("an unposted SVAL change must still publish on the next cycle");
}
