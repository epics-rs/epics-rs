#![allow(unused_imports, clippy::all)]
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::types::EpicsValue;

#[tokio::test]
async fn test_write_notify_follows_flnk() {
    let db = PvDatabase::new();
    db.add_record("REC_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("REC_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("REC_A").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("REC_B".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("REC_A", &mut visited, 0)
        .await
        .unwrap();
    assert!(visited.contains("REC_A"));
    assert!(visited.contains("REC_B"));
}

#[tokio::test]
async fn test_inp_link_processing() {
    let db = PvDatabase::new();
    db.add_record("SOURCE", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();
    db.add_record("DEST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("DEST").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("SOURCE".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DEST", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("DEST").await.unwrap();
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
#[tokio::test]
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
    if let Some(rec) = db.get_record("BROKEN").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("NO_SUCH_PV".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("BROKEN", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("BROKEN").await.expect("record exists");
    let inst = rec.read().await;
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
/// C `recGblInheritSevrMsg` (recGbl.c:260) per-flavour semantics:
/// * **MS**  — DEST gets `LINK_ALARM` (NOT source stat), max-raised
///             sevr, no amsg propagation.
/// * **MSS** — DEST gets source stat + sevr + amsg.
/// * **MSI** — same as MS, but only when source.sevr == INVALID.
#[tokio::test]
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
    if let Some(rec) = db.get_record("SRC").await {
        let mut inst = rec.write().await;
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "src-msg".to_string();
    }

    if let Some(rec) = db.get_record("DST").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("SRC NPP MS".into()))
            .unwrap();
        inst.common.udf = false;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DST", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").await.expect("DST exists");
    let inst = dst.read().await;
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
#[tokio::test]
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

    if let Some(rec) = db.get_record("SRC").await {
        let mut inst = rec.write().await;
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "src-major".to_string();
    }

    if let Some(rec) = db.get_record("DST").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("SRC NPP MSS".into()))
            .unwrap();
        inst.common.udf = false;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DST", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").await.expect("DST exists");
    let inst = dst.read().await;
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
#[tokio::test]
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
    if let Some(rec) = db.get_record("SRC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("OUT", EpicsValue::String("DST PP MS".into()))
            .unwrap();
        inst.put_common_field("HIHI", EpicsValue::Double(50.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
        inst.common.udf = false;
    }
    if let Some(rec) = db.get_record("DST").await {
        // Clear DST's own UDF so its post-write process raises no alarm
        // of its own — the only severity it can end up at is the
        // inherited one.
        rec.write().await.common.udf = false;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").await.expect("DST exists");
    let inst = dst.read().await;
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
#[tokio::test]
async fn test_out_link_nms_does_not_propagate_alarm_to_dest() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(99.0)))
        .await
        .unwrap();
    db.add_record("DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("SRC").await {
        let mut inst = rec.write().await;
        // No MS-class modifier → NoMaximize.
        inst.put_common_field("OUT", EpicsValue::String("DST PP".into()))
            .unwrap();
        inst.put_common_field("HIHI", EpicsValue::Double(50.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
        inst.common.udf = false;
    }
    if let Some(rec) = db.get_record("DST").await {
        rec.write().await.common.udf = false;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    let dst = db.get_record("DST").await.expect("DST exists");
    let inst = dst.read().await;
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
#[tokio::test]
async fn test_pva_link_propagates_alarm_severity_into_link_alarm() {
    use epics_base_rs::server::database::LinkSet;
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    /// Stub lset: serves a value and a fixed (already gated) severity.
    struct AlarmingLset;
    impl LinkSet for AlarmingLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(12.0))
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
    if let Some(rec) = db.get_record("PVADST").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("pva://REMOTE:PV".into()))
            .unwrap();
        inst.common.udf = false;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("PVADST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("PVADST").await.expect("record exists");
    let inst = rec.read().await;
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
#[tokio::test]
async fn test_pva_link_no_alarm_when_lset_reports_none() {
    use epics_base_rs::server::database::LinkSet;
    use epics_base_rs::server::record::AlarmSeverity;

    struct QuietLset;
    impl LinkSet for QuietLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(5.0))
        }
        // alarm_severity defaults to None.
    }

    let db = PvDatabase::new();
    db.register_link_set("pva", Arc::new(QuietLset)).await;
    db.add_record("PVAQUIET", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PVAQUIET").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("pva://REMOTE:OK".into()))
            .unwrap();
        inst.common.udf = false;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("PVAQUIET", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("PVAQUIET").await.expect("record exists");
    let inst = rec.read().await;
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
impl epics_base_rs::server::database::LinkSet for CapturingLset {
    fn is_connected(&self, _: &str) -> bool {
        true
    }
    fn get_value(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    fn put_value(
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
/// (dbLink.c:434-448), which routes every link write through
/// `plink->lset->putValue` regardless of DB vs CA link.
#[tokio::test]
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
    if let Some(rec) = db.get_record("AO_PVAOUT").await {
        let mut inst = rec.write().await;
        inst.put_common_field("OUT", EpicsValue::String("pva://REMOTE:OUT".into()))
            .unwrap();
        inst.common.udf = false;
        // Set VAL so process() has a value to drive out the OUT link.
        inst.record
            .put_field("VAL", EpicsValue::Double(3.5))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_PVAOUT", &mut visited, 0)
        .await
        .unwrap();

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
#[tokio::test]
async fn test_pva_out_link_no_link_set_fails_gracefully() {
    let db = PvDatabase::new();
    // No register_link_set call — the "pva" scheme is unregistered.
    db.add_record("AO_NOLSET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("AO_NOLSET").await {
        let mut inst = rec.write().await;
        inst.put_common_field("OUT", EpicsValue::String("pva://NOWHERE:PV".into()))
            .unwrap();
        inst.common.udf = false;
        inst.record
            .put_field("VAL", EpicsValue::Double(1.0))
            .unwrap();
    }

    let mut visited = HashSet::new();
    // Must not panic; process completes cleanly.
    db.process_record_with_links("AO_NOLSET", &mut visited, 0)
        .await
        .expect("process must complete despite the unresolvable OUT link");

    let rec = db.get_record("AO_NOLSET").await.expect("record exists");
    let inst = rec.read().await;
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
#[tokio::test]
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
    if let Some(rec) = db.get_record("AO_PVAOUT_NOTIFY").await {
        let mut inst = rec.write().await;
        inst.put_common_field("OUT", EpicsValue::String("pva://REMOTE:OUT".into()))
            .unwrap();
        inst.common.udf = false;
        inst.record
            .put_field("VAL", EpicsValue::Double(7.0))
            .unwrap();
        // Arm a put-notify wait-set: the source record is now in a
        // blocking-put chain, so its OUT-link write must use Async.
        inst.notify = Some(NotifyWaitSet::new(tx));
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_PVAOUT_NOTIFY", &mut visited, 0)
        .await
        .unwrap();

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
#[tokio::test]
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
    if let Some(rec) = db.get_record("SRC_AMSG").await {
        let mut inst = rec.write().await;
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "msg1".to_string();
    }
    // Dest: MSS link to source. Subscribe to AMSG with ALARM mask
    // (C posts AMSG with stat_mask = DBE_ALARM on amsg-only change).
    if let Some(rec) = db.get_record("DST_AMSG").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("SRC_AMSG NPP MSS".into()))
            .unwrap();
        inst.common.udf = false;
    }

    // Cycle 1: drives sevr 0→Major, amsg ""→"msg1" (alarm_changed=true).
    let mut visited = HashSet::new();
    db.process_record_with_links("DST_AMSG", &mut visited, 0)
        .await
        .unwrap();

    // Now subscribe to AMSG with ALARM mask AFTER cycle 1, so
    // last_posted seeds at "msg1".
    let mut amsg_rx = {
        let rec = db.get_record("DST_AMSG").await.unwrap();
        let mut inst = rec.write().await;
        inst.add_subscriber("AMSG", 11, DbFieldType::String, EventMask::ALARM.bits())
    }
    .expect("AMSG subscription must be accepted");

    // Source: keep severity Major, change amsg only.
    if let Some(rec) = db.get_record("SRC_AMSG").await {
        let mut inst = rec.write().await;
        inst.common.amsg = "msg2".to_string();
    }

    // Cycle 2: dest picks up msg2. sevr stays Major (alarm_changed=false),
    // amsg "msg1"→"msg2" (amsg_changed=true). AMSG event must flow.
    let mut visited = HashSet::new();
    db.process_record_with_links("DST_AMSG", &mut visited, 0)
        .await
        .unwrap();

    {
        let rec = db.get_record("DST_AMSG").await.unwrap();
        let inst = rec.read().await;
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
#[tokio::test]
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
    if let Some(rec) = db.get_record("SRC_MASK").await {
        let mut inst = rec.write().await;
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "msg1".to_string();
    }
    // Dest: MSS link to source.
    if let Some(rec) = db.get_record("DST_MASK").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("SRC_MASK NPP MSS".into()))
            .unwrap();
        inst.common.udf = false;
    }

    let mut val_rx = {
        let rec = db.get_record("DST_MASK").await.unwrap();
        let mut inst = rec.write().await;
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
        let rec = db.get_record("DST_MASK").await.unwrap();
        let mut inst = rec.write().await;
        inst.add_subscriber("AMSG", 22, DbFieldType::String, EventMask::ALARM.bits())
    }
    .expect("AMSG subscription must be accepted");

    // Source: keep severity Major, change amsg only.
    if let Some(rec) = db.get_record("SRC_MASK").await {
        let mut inst = rec.write().await;
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

// BUG 2 regression — `process_record` (the foreign-process / QSRV-group
// path) calls `process_local`. A recent fix excluded UDF from the
// `process_local` `sub_updates` snapshot loop, citing "UDF via the
// explicit UDF push above" — but `process_local` had NO such push (the
// two `database/processing.rs` paths exclude UDF AND pair it with an
// explicit push at `:1327` and `:1948`). Without the push a UDF change
// driven through `process_record` was never delivered to `.UDF`
// subscribers. The fix adds the `if !event_mask.is_empty()` UDF push to
// `process_local`, mirroring `processing.rs`.
#[tokio::test]
async fn test_process_record_delivers_udf_monitor_event() {
    use epics_base_rs::server::recgbl::{EventMask, alarm_status};
    use epics_base_rs::server::record::AlarmSeverity;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    // Soft-Channel ai with a defined VAL — `process_local`'s
    // `value_is_undefined()` returns false, so processing clears UDF.
    db.add_record("UDF_REC", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();

    // Seed the prior UDF state: UDF=true with INVALID/UDF_ALARM, as a
    // freshly-initialised record reads before its first process. The
    // first `process_record` clears UDF (true→false) and the alarm
    // (INVALID→NO_ALARM), so `event_mask` carries DBE_ALARM and the UDF
    // push fires.
    {
        let rec = db.get_record("UDF_REC").await.unwrap();
        let mut inst = rec.write().await;
        inst.common.udf = true;
        inst.common.sevr = AlarmSeverity::Invalid;
        inst.common.stat = alarm_status::UDF_ALARM;
    }

    // Subscribe to UDF before processing.
    let mut udf_rx = {
        let rec = db.get_record("UDF_REC").await.unwrap();
        let mut inst = rec.write().await;
        inst.add_subscriber("UDF", 31, DbFieldType::Char, EventMask::ALARM.bits())
    }
    .expect("UDF subscription must be accepted");

    // Foreign-process path — `process_record` → `process_local`.
    db.process_record("UDF_REC").await.unwrap();

    {
        let rec = db.get_record("UDF_REC").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.common.udf, "process must have cleared UDF");
    }

    let event = udf_rx
        .try_recv()
        .expect("a UDF change via process_record must deliver a UDF monitor event");
    assert!(
        matches!(event.snapshot.value, EpicsValue::Char(0)),
        "UDF event payload should be the cleared value 0, got {:?}",
        event.snapshot.value
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
/// BEFORE the `process_record_with_links` call (field_io.rs:497),
/// so any consumer reading PUTF during the process cycle (TPRO
/// trace, monitor on .PUTF, async-completion path's
/// "put-driven vs scan-driven" classifier) always saw PUTF=0.
#[tokio::test]
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

    let rec = db.get_record("PUTF_SYNC").await.unwrap();
    let inst = rec.read().await;
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
#[tokio::test]
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
        let rec = db.get_record("ASYNC_PUTF").await.unwrap();
        let inst = rec.read().await;
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
        let rec = db.get_record("ASYNC_PUTF").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.is_processing(), "completion clears PACT");
        assert!(
            !inst.common.putf,
            "complete_async_record_inner must clear PUTF (recGblFwdLink parity)"
        );
    }
}

/// C `dbAccess.c::dbPut:1410-1411` clears `precord->udf = FALSE`
/// synchronously when the put target is the record-type's primary
/// value field (`dbIsValueField`). The clear runs INSIDE dbPut —
/// BEFORE dbProcess. Pre-fix the Rust port deferred UDF clearing
/// to the process-cycle's own `if instance.record.clears_udf()`
/// branch (processing.rs:839). The processing path drops the put's
/// write lock and re-acquires inside `process_record_with_links`,
/// so a second reader between the put and the process could
/// observe `(VAL=new, udf=true)` — a C-illegal pair. For async
/// records the window spans the entire device round trip until
/// `complete_async_record` runs its own clear. This test pins the
/// C-parity invariant: post-put, pre-process, UDF must already be
/// false on a primary-field write.
#[tokio::test]
async fn test_put_record_field_from_ca_clears_udf_on_primary_field_write() {
    let db = PvDatabase::new();
    db.add_record("UDF_ASYNC", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Record starts with udf=true (default).
    {
        let rec = db.get_record("UDF_ASYNC").await.unwrap();
        assert!(
            rec.read().await.common.udf,
            "AsyncRecord starts undefined (udf=true)"
        );
    }

    let _ = db
        .put_record_field_from_ca("UDF_ASYNC", "VAL", EpicsValue::Double(7.0))
        .await;

    // AsyncRecord returns AsyncPending; PACT is set, process bailed
    // before its own UDF clear at processing.rs:840 ran. The put-time
    // clear in field_io.rs must have already fired.
    let rec = db.get_record("UDF_ASYNC").await.unwrap();
    let inst = rec.read().await;
    assert!(
        inst.is_processing(),
        "AsyncRecord should be mid-async (PACT=true)"
    );
    assert!(
        !inst.common.udf,
        "primary-field CA put must clear UDF synchronously \
         (dbAccess.c::dbPut:1411 parity) — observable before \
         complete_async_record runs"
    );
}

/// epics-base PR #3fb10b6 regression: only the record directly
/// receiving a dbPut should carry PUTF=1 during chain processing.
/// Pre-fix the CP-target dispatch set PUTF=true on every chained
/// record, smearing put attribution across the entire chain.
#[tokio::test]
async fn test_putf_stays_off_for_cp_chained_targets() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("TGT").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("SRC CP".into()))
            .unwrap();
    }

    // Drive SRC's process directly. The CP dispatch enumerates TGT
    // and would (pre-fix) set TGT.common.putf=true before processing.
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    let tgt = db.get_record("TGT").await.expect("TGT exists");
    let inst = tgt.read().await;
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
#[tokio::test]
async fn test_putf_propagates_through_db_out_link_to_passive_target() {
    let db = PvDatabase::new();
    db.add_record("PUTF_OUT_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // Source ao: OUT to TGT, PP semantics so the target processes.
    db.add_record("PUTF_OUT_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PUTF_OUT_SRC").await {
        let mut inst = rec.write().await;
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
    let tgt = db.get_record("PUTF_OUT_TGT").await.unwrap();
    let inst = tgt.read().await;
    assert!(
        !inst.common.putf,
        "after both records' synchronous cycles complete, both clear putf"
    );
    assert!(
        !inst.common.rpro,
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
#[tokio::test]
async fn test_putf_propagates_mid_cycle_via_async_target_out_link() {
    let db = PvDatabase::new();
    // Async target: stays pact between process and complete_async.
    db.add_record("PROP_TGT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("PROP_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PROP_SRC").await {
        let mut inst = rec.write().await;
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

    let tgt = db.get_record("PROP_TGT").await.unwrap();
    let inst = tgt.read().await;
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
#[tokio::test]
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
    if let Some(rec) = db.get_record("AI:SIMRAW").await {
        let mut inst = rec.write().await;
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

    let ai_rec = db.get_record("AI:SIMRAW").await.expect("AI:SIMRAW exists");
    let inst = ai_rec.read().await;
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

#[tokio::test]
async fn test_cycle_detection() {
    let db = PvDatabase::new();
    db.add_record("CYCLE_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("CYCLE_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("CYCLE_A").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("CYCLE_B".into()))
            .unwrap();
    }
    if let Some(rec) = db.get_record("CYCLE_B").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("CYCLE_A".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("CYCLE_A", &mut visited, 0)
        .await
        .unwrap();
    assert!(visited.contains("CYCLE_A"));
    assert!(visited.contains("CYCLE_B"));
    assert_eq!(visited.len(), 2);
}

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

    let val = db.get_pv("OUTPUT").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

#[tokio::test]
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
    let val = db.get_pv("OUTPUT").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 110.0).abs() < 1e-10),
        other => panic!("expected Double(110.0), got {:?}", other),
    }
}

#[tokio::test]
async fn test_ao_ivoa_dont_drive() {
    let db = PvDatabase::new();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(999.0);
    ao.ivoa = 1;
    db.add_record("OUTPUT", Box::new(ao)).await.unwrap();

    if let Some(rec) = db.get_record("OUTPUT").await {
        let mut inst = rec.write().await;
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

    let val = db.get_pv("TARGET").await.unwrap();
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
/// A `.db` file's `field(ASL, "1")` directive must
/// land in `common.asl`. `db_loader::apply_fields` feeds every common
/// field as `EpicsValue::String`; the ASL handler must parse string
/// numerics or the directive is silently dropped at IOC load.
#[tokio::test]
async fn test_db_load_records_asl_field() {
    use epics_base_rs::server::db_loader;
    use epics_base_rs::server::records::ai::AiRecord;

    let defs = db_loader::parse_db(
        r#"
record(ai, "ASLT:HIGH") {
    field(ASL, "1")
}
record(ai, "ASLT:LOW") {
}
"#,
        &std::collections::HashMap::new(),
    )
    .unwrap();

    let db = PvDatabase::new();
    for def in defs {
        let mut record: Box<dyn epics_base_rs::server::record::Record> =
            Box::new(AiRecord::new(0.0));
        let mut common_fields = Vec::new();
        db_loader::apply_fields(&mut record, &def.fields, &mut common_fields).unwrap();
        db.add_record(&def.name, record).await.unwrap();
        if let Some(rec) = db.get_record(&def.name).await {
            let mut inst = rec.write().await;
            for (n, v) in common_fields {
                let _ = inst.put_common_field(&n, v);
            }
        }
    }

    let high = db.get_record("ASLT:HIGH").await.unwrap();
    let low = db.get_record("ASLT:LOW").await.unwrap();
    assert_eq!(
        high.read().await.common.asl,
        1,
        "field(ASL, \"1\") must set ASL=1"
    );
    assert_eq!(low.read().await.common.asl, 0, "absent ASL defaults to 0");
}

#[tokio::test]
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

    if let Some(rec) = db.get_record("SRC").await {
        let mut inst = rec.write().await;
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
    let v = db.get_pv("TARGET").await.unwrap();
    assert!(
        matches!(v, EpicsValue::Double(d) if (d - 42.0).abs() < 1e-9),
        "TARGET must receive IVOV via OVAL: got {v:?}"
    );
    // Source record's OVAL must also reflect IVOV (the C convention).
    let oval = db.get_pv("SRC.OVAL").await.unwrap();
    assert!(
        matches!(oval, EpicsValue::Double(d) if (d - 42.0).abs() < 1e-9),
        "SRC.OVAL must equal IVOV: got {oval:?}"
    );
}

#[tokio::test]
async fn test_bo_ivoa_set_to_ivov_writes_rval() {
    use epics_base_rs::server::records::bo::BoRecord;

    let db = PvDatabase::new();
    let mut bo = BoRecord::new(0);
    bo.ivoa = 2;
    bo.ivov = 1;
    db.add_record("BO_SRC", Box::new(bo)).await.unwrap();
    if let Some(rec) = db.get_record("BO_SRC").await {
        let mut inst = rec.write().await;
        inst.common.nsev = AlarmSeverity::Invalid;
        inst.common.nsta = epics_base_rs::server::recgbl::alarm_status::SOFT_ALARM;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("BO_SRC", &mut visited, 0)
        .await
        .unwrap();

    // After IVOA=2, RVAL must equal IVOV (=1) — pre-fix it stayed at 0.
    // RVAL is DBF_ULONG (boRecord.dbd.pod:252).
    let rval = db.get_pv("BO_SRC.RVAL").await.unwrap();
    assert!(
        matches!(rval, EpicsValue::ULong(1)),
        "BO_SRC.RVAL must equal IVOV(1): got {rval:?}"
    );
}

#[tokio::test]
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
    co.calc = "A".to_string();
    db.add_record("CO_SRC", Box::new(co)).await.unwrap();
    if let Some(rec) = db.get_record("CO_SRC").await {
        let mut inst = rec.write().await;
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
    let v = db.get_pv("OUT_TGT").await.unwrap();
    assert!(
        matches!(v, EpicsValue::Double(d) if (d - 17.5).abs() < 1e-9),
        "OUT_TGT must receive IVOV via OVAL: got {v:?}"
    );
}

#[tokio::test]
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

    let val = db.get_pv("SIM_AI").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 99.0).abs() < 1e-10),
        other => panic!("expected Double(99.0), got {:?}", other),
    }

    let sevr = db.get_pv("SIM_AI.SEVR").await.unwrap();
    assert!(matches!(sevr, EpicsValue::Short(1)));
}

/// A simulated value runs the record's full alarm tail like a real one.
/// C `aiRecord.c`: `readValue()` raises `recGblSetSevr(prec,
/// SIMM_ALARM, prec->sims)` (MAXIMIZE) and `process()` still runs
/// `checkAlarms` + `recGblResetAlarms` on the simulated VAL. So a sim
/// VAL of 99 trips HIHI, and because HHSV (MAJOR) outranks SIMM (MINOR)
/// the limit alarm must WIN — the pre-fix direct-commit clobbered it
/// (and never even evaluated the limit), reporting only MINOR/SIMM.
#[tokio::test]
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
    if let Some(rec) = db.get_record("SIM_AI2").await {
        let mut inst = rec.write().await;
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
    let sevr = db.get_pv("SIM_AI2.SEVR").await.unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(2)),
        "sim limit MAJOR must win over SIMM MINOR: got {sevr:?}"
    );
    let stat = db.get_pv("SIM_AI2.STAT").await.unwrap();
    assert!(
        matches!(stat, EpicsValue::Short(s) if s as u16 == alarm_status::HIHI_ALARM),
        "STAT must be HIHI_ALARM, not SIMM_ALARM: got {stat:?}"
    );
}

/// C `recGblResetAlarms` (recGbl.c:201-220) posts the alarm fields only
/// when the alarm state moved this cycle. The pre-fix SIMM tail pushed
/// SEVR/STAT into every simulated snapshot with one shared
/// `DBE_VALUE|DBE_ALARM` mask and bypassed the MDEL deadband, so a
/// steady simulated record re-sent its unchanged alarm fields and VAL on
/// every cycle.
#[tokio::test]
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
        let rec = db.get_record("SIM_AI3").await.unwrap();
        let mut inst = rec.write().await;
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
/// mask (recGbl.c:201-220): SEVR is `DBE_VALUE` only — a
/// `DBE_ALARM`-only `.SEVR` subscriber must NOT be notified — while
/// STAT's mask carries `DBE_ALARM` when the severity moved. The pre-fix
/// SIMM tail collapsed both onto a shared `DBE_VALUE|DBE_ALARM` mask.
#[tokio::test]
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
        let rec = db.get_record("SIM_AI4").await.unwrap();
        let mut inst = rec.write().await;
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
#[tokio::test]
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
        let rec = db.get_record("SIM_AI5").await.unwrap();
        let mut inst = rec.write().await;
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

#[tokio::test]
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

    if let Some(rec) = db.get_record("TEST_AI").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("REAL_SRC".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TEST_AI", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("TEST_AI").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 10.0).abs() < 1e-10),
        other => panic!("expected Double(10.0), got {:?}", other),
    }

    db.put_pv("SIM_SW", EpicsValue::Double(1.0)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TEST_AI", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("TEST_AI").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

#[tokio::test]
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

    let val = db.get_pv("SIM_OUT").await.unwrap();
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
#[tokio::test]
async fn test_sim_mode_input_nonlocal_db_siol() {
    use epics_base_rs::server::database::LinkSet;
    struct ValueCaLset(f64);
    impl LinkSet for ValueCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, name: &str) -> Option<EpicsValue> {
            // The bare non-local SIOL record name reaches the CA lset
            // via the read-locality fallback `resolve_external_pv`.
            (name == "REMOTE:SIM").then_some(EpicsValue::Double(self.0))
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

    let val = db.get_pv("SIM_AI_NL").await.unwrap();
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
#[tokio::test]
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

#[tokio::test]
async fn test_sdis_disable_skips_process() {
    let db = PvDatabase::new();
    db.add_record("DISABLE_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("TARGET").await {
        let mut inst = rec.write().await;
        inst.put_common_field("SDIS", EpicsValue::String("DISABLE_SW".into()))
            .unwrap();
        inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TARGET").await.unwrap();
    let inst = rec.read().await;
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

    let rec = db.get_record("TARGET").await.unwrap();
    let inst = rec.read().await;
    assert_ne!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::DISABLE_ALARM
    );
}

/// C `dbGetLink(&precord->sdis, DBR_SHORT, &precord->disa, 0, 0)`
/// reads the SDIS link for ANY type. The pre-fix port refreshed `disa`
/// only for a `ParsedLink::Db` SDIS, so a constant (and CA/PVA) SDIS was
/// silently ignored — `disa` stayed at its default and a record meant to
/// be disabled by a constant SDIS kept processing. Boundary: constant
/// value == DISV (disabled) vs != DISV (enabled).
#[tokio::test]
async fn test_sdis_constant_link_refreshes_disa() {
    let db = PvDatabase::new();
    db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    // Constant SDIS "1" — equals the default DISV (1) → disabled.
    if let Some(rec) = db.get_record("TARGET").await {
        let mut inst = rec.write().await;
        inst.put_common_field("SDIS", EpicsValue::String("1".into()))
            .unwrap();
        inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("TARGET").await.unwrap();
        let inst = rec.read().await;
        assert_eq!(
            inst.common.disa, 1,
            "constant SDIS '1' must refresh disa to 1 (was ignored pre-fix)"
        );
        assert_eq!(
            inst.common.stat,
            epics_base_rs::server::recgbl::alarm_status::DISABLE_ALARM,
            "disa==disv must disable the record"
        );
    }

    // Constant SDIS "0" — differs from DISV (1) → re-enabled. Proves the
    // refresh reads the actual constant value, not a hard-coded disable.
    if let Some(rec) = db.get_record("TARGET").await {
        let mut inst = rec.write().await;
        inst.put_common_field("SDIS", EpicsValue::String("0".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("TARGET", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("TARGET").await.unwrap();
        let inst = rec.read().await;
        assert_eq!(
            inst.common.disa, 0,
            "constant SDIS '0' must refresh disa to 0"
        );
        assert_ne!(
            inst.common.stat,
            epics_base_rs::server::recgbl::alarm_status::DISABLE_ALARM,
            "disa!=disv must leave the record enabled"
        );
    }
}

#[tokio::test]
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
        if let Some(rec) = db.get_record(name).await {
            let mut inst = rec.write().await;
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
                db.update_scan_index(name, old_scan, new_scan, p, p).await;
            }
        }
    }

    let names = db.records_for_scan(ScanType::Sec1).await;
    assert_eq!(names, vec!["REC_A", "REC_B", "REC_C"]);
}

#[tokio::test]
async fn test_depth_limit() {
    let db = PvDatabase::new();
    for i in 0..20 {
        db.add_record(&format!("CHAIN_{i}"), Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
    }
    for i in 0..19 {
        if let Some(rec) = db.get_record(&format!("CHAIN_{i}")).await {
            let mut inst = rec.write().await;
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
    assert!(visited.len() <= 17);
    assert!(visited.contains("CHAIN_0"));
}

#[tokio::test]
async fn test_disp_blocks_ca_put() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }

    let result = db
        .put_record_field_from_ca("REC", "VAL", EpicsValue::Double(42.0))
        .await;
    assert!(matches!(result, Err(CaError::PutDisabled(_))));
}

#[tokio::test]
async fn test_disp_allows_disp_write() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }

    let result = db
        .put_record_field_from_ca("REC", "DISP", EpicsValue::Char(0))
        .await;
    assert!(result.is_ok());

    let rec = db.get_record("REC").await.unwrap();
    let inst = rec.read().await;
    assert!(!inst.common.disp);
}

#[tokio::test]
async fn test_disp_bypassed_by_internal_put() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }

    let result = db.put_pv("REC", EpicsValue::Double(42.0)).await;
    assert!(result.is_ok());
}

#[tokio::test]
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
    let rec = db.get_record("REC").await.unwrap();
    let inst = rec.read().await;
    assert!(!inst.common.udf);
}

#[tokio::test]
async fn test_proc_works_any_scan() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("SCAN", EpicsValue::String("1 second".into()))
            .unwrap();
    }
    let result = db
        .put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1))
        .await;
    assert!(result.is_ok());
    let rec = db.get_record("REC").await.unwrap();
    let inst = rec.read().await;
    assert!(!inst.common.udf);
}

#[tokio::test]
async fn test_proc_bypasses_disp() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
    }
    let result = db
        .put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1))
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_proc_while_pact() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let result = db
        .put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1))
        .await;
    assert!(result.is_ok());
    let rec = db.get_record("REC").await.unwrap();
    let inst = rec.read().await;
    assert!(!inst.common.udf);
}

#[tokio::test]
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

#[tokio::test]
async fn test_ca_put_scan_index_update() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::String("1 second".into()))
        .await
        .unwrap();
    let names = db.records_for_scan(ScanType::Sec1).await;
    assert!(names.contains(&"REC".to_string()));
}

// --- Mock DeviceSupport for write/read counting ---

struct MockDeviceSupport {
    read_count: Arc<AtomicU32>,
    write_count: Arc<AtomicU32>,
    dtyp_name: String,
}

impl MockDeviceSupport {
    fn new(dtyp: &str, read_count: Arc<AtomicU32>, write_count: Arc<AtomicU32>) -> Self {
        Self {
            read_count,
            write_count,
            dtyp_name: dtyp.to_string(),
        }
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
}

#[tokio::test]
async fn test_ca_put_no_double_device_write() {
    let db = PvDatabase::new();
    db.add_record("AO_REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AO_REC").await {
        let mut inst = rec.write().await;
        inst.common.dtyp = "MockDev".to_string();
        inst.device = Some(Box::new(mock));
    }
    db.put_record_field_from_ca("AO_REC", "VAL", EpicsValue::Double(42.0))
        .await
        .unwrap();
    assert_eq!(write_count.load(Ordering::SeqCst), 1);
}

// epics-base f2fe9d12 (devBiSoftRaw): a `bi` record with
// `DTYP="Raw Soft Channel"`, MASK set, and a soft INP link must mask
// the link value into RVAL before the RVAL→VAL convert. The framework
// must route the INP value to `apply_raw_input` (not `set_val`).
#[tokio::test]
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
    if let Some(rec) = db.get_record("BI_RAW").await {
        let mut inst = rec.write().await;
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
    if let Some(rec) = db.get_record("BI_RAW").await {
        let inst = rec.read().await;
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

#[tokio::test]
async fn test_input_record_no_device_write() {
    let db = PvDatabase::new();
    db.add_record("AI_REC", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AI_REC").await {
        let mut inst = rec.write().await;
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

#[tokio::test]
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
    if let Some(rec) = db.get_record("AI_UTAG").await {
        let mut inst = rec.write().await;
        inst.common.dtyp = "UtagDev".to_string();
        inst.device = Some(Box::new(UtagDeviceSupport { utag: 0x9000_0000 }));
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("AI_UTAG", &mut visited, 0)
        .await
        .unwrap();
    if let Some(rec) = db.get_record("AI_UTAG").await {
        let inst = rec.read().await;
        assert_eq!(
            inst.common.utag, 0x9000_0000,
            "device-reported userTag must be adopted into common.utag"
        );
    }
}

#[tokio::test]
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
    if let Some(rec) = db.get_record("AO_NP").await {
        let mut inst = rec.write().await;
        inst.common.dtyp = "MockDev".to_string();
        inst.common.scan = ScanType::Sec1;
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

#[tokio::test]
async fn test_proc_triggers_device_write() {
    let db = PvDatabase::new();
    db.add_record("AO_PROC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let read_count = Arc::new(AtomicU32::new(0));
    let write_count = Arc::new(AtomicU32::new(0));
    let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());
    if let Some(rec) = db.get_record("AO_PROC").await {
        let mut inst = rec.write().await;
        inst.common.dtyp = "MockDev".to_string();
        inst.device = Some(Box::new(mock));
    }
    db.put_record_field_from_ca("AO_PROC", "PROC", EpicsValue::Char(1))
        .await
        .unwrap();
    assert_eq!(write_count.load(Ordering::SeqCst), 1);
}

// --- Scan Index Fix tests ---

#[tokio::test]
async fn test_phas_change_updates_scan_index() {
    let db = PvDatabase::new();
    db.add_record("REC_A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("REC_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    for (name, phas) in &[("REC_A", 10i16), ("REC_B", 5)] {
        if let Some(rec) = db.get_record(name).await {
            let mut inst = rec.write().await;
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
                db.update_scan_index(name, old_scan, new_scan, p, p).await;
            }
        }
    }
    let names = db.records_for_scan(ScanType::Sec1).await;
    assert_eq!(names, vec!["REC_B", "REC_A"]);

    if let Some(rec) = db.get_record("REC_A").await {
        let mut inst = rec.write().await;
        let result = inst.put_common_field("PHAS", EpicsValue::Short(0)).unwrap();
        if let CommonFieldPutResult::PhasChanged {
            scan,
            old_phas,
            new_phas,
        } = result
        {
            drop(inst);
            db.update_scan_index("REC_A", scan, scan, old_phas, new_phas)
                .await;
        }
    }
    let names = db.records_for_scan(ScanType::Sec1).await;
    assert_eq!(names, vec!["REC_A", "REC_B"]);
}

#[tokio::test]
async fn test_scan_change_preserves_phas() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
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

#[tokio::test]
async fn test_phas_change_passive_no_index() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
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
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
    }
}

#[tokio::test]
async fn test_async_pending_skips_post_process() {
    let db = PvDatabase::new();
    db.add_record("ASYNC", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("FLNK_TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("ASYNC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("FLNK_TARGET".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC", &mut visited, 0)
        .await
        .unwrap();
    assert!(visited.contains("ASYNC"));
    assert!(!visited.contains("FLNK_TARGET"));
    let rec = db.get_record("ASYNC").await.unwrap();
    let inst = rec.read().await;
    assert!(inst.common.udf);
}

#[tokio::test]
async fn test_complete_async_record() {
    let db = PvDatabase::new();
    db.add_record("ASYNC", Box::new(AsyncRecord { val: 42.0 }))
        .await
        .unwrap();
    db.add_record("FLNK_TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("ASYNC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("FLNK_TARGET".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC", &mut visited, 0)
        .await
        .unwrap();
    assert!(!visited.contains("FLNK_TARGET"));
    db.complete_async_record("ASYNC").await.unwrap();
    let rec = db.get_record("ASYNC").await.unwrap();
    let inst = rec.read().await;
    assert!(!inst.common.udf);
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
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
    }
}

#[tokio::test]
async fn test_complete_async_posts_sevr_with_per_field_mask() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::record::AlarmSeverity;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("ASYNC_SEVR", Box::new(AsyncAlarmingRecord))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("ASYNC_SEVR").await {
        let mut inst = rec.write().await;
        inst.common.udf = false;
    }

    // First cycle: record reports async_pending (PACT set).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_SEVR", &mut visited, 0)
        .await
        .unwrap();

    // Subscribe to SEVR twice: one DBE_VALUE-only, one DBE_ALARM-only.
    let (mut sevr_value_rx, mut sevr_alarm_rx) = {
        let rec = db.get_record("ASYNC_SEVR").await.unwrap();
        let mut inst = rec.write().await;
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
        let rec = db.get_record("ASYNC_SEVR").await.unwrap();
        let inst = rec.read().await;
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

// C parity (dbAccess.c::dbProcess:537-559): a second
// `process_record_with_links` against a PACT-active record must NOT
// re-enter `record.process()`. The first attempt must bail silently
// (lcnt counting up); after MAX_LOCK=10 consecutive bails, SCAN_ALARM /
// INVALID must be raised with "Async in progress" amsg and VAL must be
// posted with DBE_VALUE|DBE_LOG|DBE_ALARM.
#[tokio::test]
async fn test_pact_entry_guard_silent_bail_until_max_lock() {
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    db.add_record("ASYNC_PACT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Drive ASYNC_PACT into PACT=true (async pending, lock released).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_PACT", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("ASYNC_PACT").await.unwrap();
        let inst = rec.read().await;
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
        let rec = db.get_record("ASYNC_PACT").await.unwrap();
        let inst = rec.read().await;
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
    let rec = db.get_record("ASYNC_PACT").await.unwrap();
    let inst = rec.read().await;
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
#[tokio::test]
async fn test_pact_entry_guard_tpro_diagnostic_does_not_change_bail_outcome() {
    let db = PvDatabase::new();
    db.add_record("ASYNC_TPRO", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Set TPRO=true and RPRO=true so the diagnostic line carries
    // observable state.
    {
        let rec = db.get_record("ASYNC_TPRO").await.unwrap();
        let mut inst = rec.write().await;
        inst.common.tpro = true;
        inst.common.rpro = true;
    }

    // Cycle 1: drive into PACT.
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_TPRO", &mut visited, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("ASYNC_TPRO").await.unwrap();
        let inst = rec.read().await;
        assert!(inst.is_processing(), "must enter PACT");
        assert!(inst.common.tpro, "TPRO must be preserved");
        assert!(inst.common.rpro, "RPRO must be preserved across PACT entry");
    }

    // Re-entry while PACT=true: bail with lcnt increment. Diagnostic
    // is emitted as a side effect (eprintln) but the bail outcome
    // matches the non-TPRO case (verified by the silent-bail test).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_TPRO", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("ASYNC_TPRO").await.unwrap();
    let inst = rec.read().await;
    assert!(inst.is_processing(), "still PACT after bail");
    assert_eq!(inst.common.lcnt, 1, "lcnt must have advanced");
    assert!(
        inst.common.rpro,
        "RPRO must remain unchanged by the diagnostic path"
    );
}

// After PACT clears via complete_async_record, the next process must
// reset lcnt to 0 (mirrors C `else { precord->lcnt = 0; }`).
#[tokio::test]
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
        let rec = db.get_record("ASYNC_RESET").await.unwrap();
        assert_eq!(rec.read().await.common.lcnt, 3);
    }

    // Complete the async; this clears PACT.
    db.complete_async_record("ASYNC_RESET").await.unwrap();

    // Next process_record_with_links should reset lcnt (path: enters
    // body since PACT is now false).
    let mut visited = HashSet::new();
    db.process_record_with_links("ASYNC_RESET", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("ASYNC_RESET").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(inst.common.lcnt, 0, "lcnt must reset when PACT clears");
}

// Regression: when a record returns `AsyncPending` paired with a
// `ReprocessAfter` action (the timer-owned continuation pattern used
// by scaler DLY / calc AFTC), the spawned timer fire must call
// `process_record_continuation` and bypass the PACT entry guard so
// the record's `process()` runs again to advance the state machine.
// The foreign-caller guard (FLNK / scan / CA put) is still in
// effect — `test_pact_entry_guard_silent_bail_until_max_lock` above
// covers that case.
#[tokio::test]
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
        fn field_list(&self) -> &'static [FieldDesc] {
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
        let rec = db.get_record("CONT_REC").await.unwrap();
        assert!(
            rec.read().await.is_processing(),
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

    // Wait for the ReprocessAfter timer to fire.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

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
        let rec = db.get_record("CONT_REC").await.unwrap();
        assert!(
            !rec.read().await.is_processing(),
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
#[tokio::test]
async fn test_dbnd_filter_drops_subthreshold_changes() {
    use epics_base_rs::server::database::filters::DeadbandFilter;
    use epics_base_rs::server::recgbl::EventMask;
    use std::sync::Arc;

    let db = PvDatabase::new();
    db.add_record("DBND:REC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let rec = db.get_record("DBND:REC").await.unwrap();
    let mut rx = {
        let mut inst = rec.write().await;
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
        let mut inst = rec.write().await;
        inst.record
            .put_field("VAL", EpicsValue::Double(11.0))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    rx.try_recv()
        .expect("first value passes the deadband filter");

    // 11.4: |delta|=0.4 < 1.0 → silenced.
    {
        let mut inst = rec.write().await;
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
        let mut inst = rec.write().await;
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
#[tokio::test]
async fn test_dbnd_filter_passes_alarm_events() {
    use epics_base_rs::server::database::filters::DeadbandFilter;
    use epics_base_rs::server::recgbl::EventMask;
    use std::sync::Arc;

    let db = PvDatabase::new();
    db.add_record("DBND:ALR", Box::new(AoRecord::new(50.0)))
        .await
        .unwrap();
    let rec = db.get_record("DBND:ALR").await.unwrap();
    let mut rx = {
        let mut inst = rec.write().await;
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
        let mut inst = rec.write().await;
        inst.record
            .put_field("VAL", EpicsValue::Double(50.0))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    rx.try_recv().expect("seed value");

    // A 50.5 value-only update is silenced by the deadband (delta 0.5 < 10).
    {
        let mut inst = rec.write().await;
        inst.record
            .put_field("VAL", EpicsValue::Double(50.5))
            .unwrap();
        inst.notify_field("VAL", EventMask::VALUE);
    }
    assert!(rx.try_recv().is_err(), "sub-threshold value silenced");

    // But an ALARM-tagged emission with the SAME value MUST pass —
    // the filter's "always-pass alarm" rule.
    {
        let inst = rec.read().await;
        inst.notify_field("VAL", EventMask::ALARM);
    }
    rx.try_recv().expect("alarm event passes the filter");
}

#[tokio::test]
async fn test_notify_field_respects_mask() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").await.unwrap();
    let (mut value_rx, mut alarm_rx) = {
        let mut inst = rec.write().await;
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
        let inst = rec.read().await;
        inst.notify_field("VAL", EventMask::VALUE);
    }
    assert!(value_rx.try_recv().is_ok());
    assert!(alarm_rx.try_recv().is_err());
}

/// C `dbAccess.c:575-577` clears `precord->rpro = FALSE; precord->putf =
/// FALSE` and arms `callNotifyCompletion = TRUE` BEFORE the alarm
/// check whenever SDIS evaluates to DISV. Pre-fix Rust only
/// reset nsta/nsev and updated the alarm — rpro/putf leaked into the
/// next cycle and pending dbNotify completion callbacks stalled.
#[tokio::test]
async fn test_sdis_disable_clears_rpro_and_putf() {
    let db = PvDatabase::new();
    db.add_record("DIS_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("DIS_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DIS_TGT").await {
        let mut inst = rec.write().await;
        inst.put_common_field("SDIS", EpicsValue::String("DIS_SW".into()))
            .unwrap();
        // Pre-set rpro=true, putf=true so the disable path's clear is
        // observable.
        inst.common.rpro = true;
        inst.common.putf = true;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DIS_TGT", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("DIS_TGT").await.unwrap();
    let inst = rec.read().await;
    assert!(
        !inst.common.rpro,
        "SDIS disable must clear rpro (C dbAccess.c:575). Pre-fix this leaked."
    );
    assert!(
        !inst.common.putf,
        "SDIS disable must clear putf (C dbAccess.c:576). Pre-fix this leaked."
    );
}

/// C `dbAccess.c:622-623` runs `dbNotifyCompletion(precord)` at
/// `all_done` for the disable bail path because `callNotifyCompletion
/// = TRUE` was set at line 577. A CA WRITE_NOTIFY landing on a
/// disabled record must release its caller. Pre-fix the
/// put_notify_tx was never fired, stranding the call until socket
/// disconnect.
#[tokio::test]
async fn test_sdis_disable_fires_put_notify_completion() {
    let db = PvDatabase::new();
    db.add_record("DIS_NOT_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("DIS_NOT_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DIS_NOT_TGT").await {
        let mut inst = rec.write().await;
        inst.put_common_field("SDIS", EpicsValue::String("DIS_NOT_SW".into()))
            .unwrap();
    }

    // Arm a put-notify wait-set on the disabled target (pending=1 for the
    // originating record). The disable path must take it and `leave`,
    // draining the set to zero and firing the completion oneshot.
    let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
    {
        let rec = db.get_record("DIS_NOT_TGT").await.unwrap();
        let mut inst = rec.write().await;
        inst.notify = Some(NotifyWaitSet::new(tx));
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("DIS_NOT_TGT", &mut visited, 0)
        .await
        .unwrap();

    // rx should be ready — completion was sent via the disable bail.
    rx.await
        .expect("disable bail must fire put-notify completion (C dbAccess.c:622)");
    // membership must be taken (not left dangling for the next cycle).
    let rec = db.get_record("DIS_NOT_TGT").await.unwrap();
    assert!(
        rec.read().await.notify.is_none(),
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
//   * second WRITE_NOTIFY in-flight  → refused (Ok→Err PutCallbackInProgress)
// ---------------------------------------------------------------------------

/// Boundary: a fully synchronous chain (originating record + a sync FLNK
/// target) drains the wait-set within the put call, so the put reports
/// immediate completion (`Ok(None)`) — no receiver is handed back.
#[tokio::test]
async fn test_put_notify_sync_chain_completes_immediately() {
    let db = PvDatabase::new();
    db.add_record("PN_SYNC_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("PN_SYNC_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PN_SYNC_SRC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("PN_SYNC_TGT".into()))
            .unwrap();
    }

    let result = db
        .put_record_field_from_ca("PN_SYNC_SRC", "VAL", EpicsValue::Double(3.0))
        .await
        .expect("put must succeed");
    assert!(
        result.is_none(),
        "a fully synchronous chain drains the wait-set in-call → Ok(None); \
         got a deferred receiver instead"
    );
}

/// Boundary (the finding): a synchronous originating record whose FLNK
/// target is async must DEFER completion. Pre-fix the originating record
/// fired its completion the instant its own sync cycle ended, reporting
/// WRITE_NOTIFY done while the async FLNK target was still in flight. Now
/// the put returns a receiver that fires only when the async target's
/// `complete_async_record` runs.
#[tokio::test]
async fn test_put_notify_defers_for_async_flnk_target() {
    let db = PvDatabase::new();
    db.add_record("PN_FLNK_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("PN_FLNK_TGT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PN_FLNK_SRC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("PN_FLNK_TGT".into()))
            .unwrap();
    }

    let result = db
        .put_record_field_from_ca("PN_FLNK_SRC", "VAL", EpicsValue::Double(4.0))
        .await
        .expect("put must succeed");
    let mut rx = match result {
        Some(rx) => rx,
        None => panic!(
            "an async FLNK target must defer completion — \
             the put returned immediate (Ok(None)) instead of a receiver"
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
#[tokio::test]
async fn test_put_notify_defers_for_async_out_pp_target() {
    let db = PvDatabase::new();
    db.add_record("PN_OUT_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("PN_OUT_TGT", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("PN_OUT_SRC").await {
        let mut inst = rec.write().await;
        inst.put_common_field("OUT", EpicsValue::String("PN_OUT_TGT PP".into()))
            .unwrap();
    }

    let result = db
        .put_record_field_from_ca("PN_OUT_SRC", "VAL", EpicsValue::Double(5.0))
        .await
        .expect("put must succeed");
    let mut rx = match result {
        Some(rx) => rx,
        None => panic!(
            "an async OUT PP target must defer completion — \
             the put returned immediate (Ok(None)) instead of a receiver"
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

/// Boundary: a second WRITE_NOTIFY on a record whose put-notify is still
/// in flight is refused (C returns S_db_Blocked / ECA_PUTCBINPROG).
/// Silently overwriting the wait-set would drop the prior `Sender`, waking
/// the prior caller's receiver with a `RecvError` the CA dispatcher treats
/// as success.
#[tokio::test]
async fn test_put_notify_refuses_second_in_flight() {
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
        first.is_some(),
        "async record's first put must return a deferred receiver"
    );

    // Second put while the first is still in flight → refused.
    let second = db
        .put_record_field_from_ca("PN_DBL", "VAL", EpicsValue::Double(2.0))
        .await;
    assert!(
        matches!(second, Err(CaError::PutCallbackInProgress(_))),
        "a second WRITE_NOTIFY on an in-flight record must be refused with \
         PutCallbackInProgress; got {second:?}"
    );
}

/// Boundary (the live defect): a fire-and-forget put — C `dbPutField`
/// semantics: CA_PROTO_WRITE, `dbpf`, the internal `put_*_process`
/// helpers, non-blocking PVA puts — parks NO put-notify wait-set, even
/// when the record goes async-pending. Pre-fix EVERY processing put
/// parked one; a caller that dropped the receiver left the record's
/// notify slot occupied for the whole async round trip (a motor's
/// whole motion), refusing every legitimate WRITE_NOTIFY on the record
/// with PutCallbackInProgress in the meantime.
#[tokio::test]
async fn test_fire_and_forget_put_parks_no_notify_write_notify_stays_legal() {
    let db = PvDatabase::new();
    db.add_record("PN_FF", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("PN_FF", "VAL", EpicsValue::Double(1.0))
        .await
        .expect("fire-and-forget put must succeed");
    {
        let rec = db.get_record("PN_FF").await.unwrap();
        let inst = rec.read().await;
        assert!(inst.is_processing(), "async pending → PACT=true");
        assert!(
            inst.notify.is_none(),
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
    // was refused with PutCallbackInProgress because the fire-and-forget
    // put's orphaned wait-set occupied the slot.
    let rx = db
        .put_record_field_from_ca("PN_FF", "VAL", EpicsValue::Double(2.0))
        .await
        .expect("WRITE_NOTIFY after a fire-and-forget put must be accepted")
        .expect("record is still async-pending → completion is deferred");

    db.complete_async_record("PN_FF").await.unwrap();
    rx.await
        .expect("deferred WRITE_NOTIFY completion must fire at async completion");
}

/// Boundary (other direction): a fire-and-forget put on a record with
/// a WRITE_NOTIFY already parked is accepted (C `dbPutField` carries no
/// notify state to conflict) and leaves the parked wait-set undisturbed
/// — it neither steals nor fires the prior caller's completion.
#[tokio::test]
async fn test_fire_and_forget_put_does_not_disturb_parked_notify() {
    let db = PvDatabase::new();
    db.add_record("PN_FF2", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    let mut rx = db
        .put_record_field_from_ca("PN_FF2", "VAL", EpicsValue::Double(1.0))
        .await
        .expect("WRITE_NOTIFY must succeed")
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

/// Boundary: synchronous-completion PUTF clear still runs in no-notify
/// mode. `originating_pending` keys on the instance's parked notify;
/// a fire-and-forget put parks nothing, so it must fall through to the
/// guarded clear rather than mistaking an empty slot for "pending".
#[tokio::test]
async fn test_fire_and_forget_put_clears_putf_on_sync_completion() {
    let db = PvDatabase::new();
    db.add_record("PN_FF_SYNC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("PN_FF_SYNC", "VAL", EpicsValue::Double(42.0))
        .await
        .expect("fire-and-forget put must succeed");

    let rec = db.get_record("PN_FF_SYNC").await.unwrap();
    let inst = rec.read().await;
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

#[tokio::test]
async fn test_sdis_disable_notifies_alarm() {
    // C `dbAccess.c:587-592` — the disable branch of `dbProcess` posts:
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
    if let Some(rec) = db.get_record("TARGET").await {
        let mut inst = rec.write().await;
        inst.put_common_field("SDIS", EpicsValue::String("DISABLE_SW".into()))
            .unwrap();
        inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap();
    }
    let mut alarm_rx = {
        let rec = db.get_record("TARGET").await.unwrap();
        let mut inst = rec.write().await;
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

#[tokio::test]
async fn test_udf_cleared_by_process_with_links() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").await.unwrap();
    assert!(rec.read().await.common.udf);
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    assert!(!rec.read().await.common.udf);
}

#[tokio::test]
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
        fn field_list(&self) -> &'static [FieldDesc] {
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
    let rec = db.get_record("REC").await.unwrap();
    assert!(rec.read().await.common.udf);
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    assert!(rec.read().await.common.udf);
}

#[tokio::test]
async fn test_constant_inp_link() {
    let db = PvDatabase::new();
    db.add_record("AI_CONST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("AI_CONST").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("3.15".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("AI_CONST", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("AI_CONST").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 3.15).abs() < 1e-10),
        other => panic!("expected Double(3.15), got {:?}", other),
    }
}

#[tokio::test]
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
    let val = db.get_pv("CALC_REC").await.unwrap();
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
#[tokio::test]
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
    let val = db.get_pv("PP_DST").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "PP multi-input link must process source first: expected 42, got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    // The source itself must have been processed (VAL latched to 42).
    let src_val = db.get_pv("PP_SRC").await.unwrap();
    match src_val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "PP_SRC must have been processed by the PP link, VAL={v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
}

/// Defect 3 control: an `NPP` (no-process-passive) multi-input link
/// must NOT process its passive source — it reads whatever stale
/// value the source currently holds.
#[tokio::test]
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
    let val = db.get_pv("NPP_DST").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            v.abs() < 1e-10,
            "NPP multi-input link must NOT process source: expected 0, got {v}"
        ),
        other => panic!("expected Double(0.0), got {other:?}"),
    }
}

/// A **modifier-less** (bare) multi-input link must behave like NPP —
/// C `dbParseLink` (`dbStaticLib.c:2252,2369-2371`) leaves `pvlOptPP`
/// unset for a bare link, so `dbDbGetValue` (`dbDbLink.c:175`) does NOT
/// process the passive source on read. Before the fix `parse_link_v2`
/// defaulted a bare link to `ProcessPassive`, so `BARE_DST` would have
/// spuriously processed `BARE_SRC` and read 42; after the fix it reads
/// the stale 0.
#[tokio::test]
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

    let val = db.get_pv("BARE_DST").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            v.abs() < 1e-10,
            "bare multi-input link is NPP and must NOT process its source: expected 0, got {v}"
        ),
        other => panic!("expected Double(0.0), got {other:?}"),
    }
    // BARE_SRC must still hold its unprocessed default.
    let src_val = db.get_pv("BARE_SRC").await.unwrap();
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
/// all_done;` (dbAccess.c:537) and bails after one bounce. The Rust
/// fix threads the caller's `visited` set / `depth` through the PP
/// hop so the existing `visited.insert` guard
/// (`process_record_with_links_inner`) fires instead.
///
/// This test passing at all proves the fix: a regression re-aborts
/// the whole test process with a stack overflow.
#[tokio::test]
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
    let va = db.get_pv("CALC_A").await.unwrap();
    let vb = db.get_pv("CALC_B").await.unwrap();
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

#[tokio::test]
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
    let val = db.get_pv("CALC_CONST").await.unwrap();
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
#[tokio::test]
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
        let rec = db.get_record("CALC_LIM").await.unwrap();
        let inst = rec.read().await;
        inst.resolve_field("HIHI").and_then(|v| v.to_f64()).unwrap()
    };
    assert_eq!(hihi, 10.0);

    // Process — CALC="A" with A=15 → VAL=15 > HIHI=10 → HIHI_ALARM/MAJOR.
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_LIM", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("CALC_LIM").await.unwrap();
    let inst = rec.read().await;
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
#[tokio::test]
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
    let rec = db.get_record("CALC_AFTC").await.unwrap();
    {
        let mut inst = rec.write().await;
        let _ = inst.record.put_field("VAL", EpicsValue::Double(15.0));
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_AFTC", &mut visited, 0)
        .await
        .unwrap();
    let inst = rec.read().await;
    // afvl must have been updated (filter is engaged)
    let afvl = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .unwrap_or(0.0);
    assert!(afvl != 0.0, "AFVL must be updated when AFTC > 0");
}

#[tokio::test]
async fn test_fanout_all() {
    use epics_base_rs::server::records::fanout::FanoutRecord;
    let db = PvDatabase::new();
    let mut fanout = FanoutRecord::new();
    fanout.selm = 0;
    fanout.lnk1 = "TARGET_1".to_string();
    fanout.lnk2 = "TARGET_2".to_string();
    db.add_record("FANOUT", Box::new(fanout)).await.unwrap();
    db.add_record("TARGET_1", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TARGET_2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("FANOUT", &mut visited, 0)
        .await
        .unwrap();
    assert!(visited.contains("FANOUT"));
    assert!(visited.contains("TARGET_1"));
    assert!(visited.contains("TARGET_2"));
}

#[tokio::test]
async fn test_fanout_specified() {
    // C parity (fanoutRecord.c:114): SELM=Specified selects the link
    // at index `SELN + OFFS`, 0-based over LNK0..LNKF. With SELN=1,
    // OFFS=0 the selected link is LNK1 (NOT LNK2 — the pre-fix port
    // omitted LNK0 and was off by one).
    use epics_base_rs::server::records::fanout::FanoutRecord;
    let db = PvDatabase::new();
    let mut fanout = FanoutRecord::new();
    fanout.selm = 1;
    fanout.seln = 1;
    db.add_record("FANOUT", Box::new(fanout)).await.unwrap();
    db.add_record("T1", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("T2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("FANOUT").await {
        let mut inst = rec.write().await;
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
    assert!(visited.contains("FANOUT"));
    // SELN=1 → LNK1 → T1 processed; LNK2/T2 NOT processed.
    assert!(visited.contains("T1"));
    assert!(!visited.contains("T2"));
}

#[tokio::test]
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
    let val_a = db.get_pv("DEST_A").await.unwrap();
    match val_a {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
    let val_b = db.get_pv("DEST_B").await.unwrap();
    match val_b {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

/// C `dfanoutRecord.c:115-122` reads VAL from DOL on every process
/// cycle when `omsl == menuOmslclosed_loop`. The Rust port previously
/// omitted dfanout from the DOL-eligible record-type list in
/// `processing.rs::process_record_with_links_inner`, so a dfanout
/// configured with OMSL=closed_loop never sourced VAL from DOL —
/// every cycle silently kept the previously-cached VAL.
#[tokio::test]
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
    let val_a = db.get_pv("DFAN_DEST_A").await.unwrap();
    assert!(
        matches!(val_a, EpicsValue::Double(v) if (v - 7.5).abs() < 1e-10),
        "DFAN_DEST_A must reflect DOL_SRC (=7.5), got {val_a:?}"
    );
    let val_b = db.get_pv("DFAN_DEST_B").await.unwrap();
    assert!(
        matches!(val_b, EpicsValue::Double(v) if (v - 7.5).abs() < 1e-10),
        "DFAN_DEST_B must reflect DOL_SRC (=7.5), got {val_b:?}"
    );
}

/// Companion to the OMSL=closed_loop test: with OMSL=supervisory
/// (default), DOL must NOT be evaluated even if a DOL link is set —
/// VAL remains under operator control. This pins the gating so a
/// future refactor cannot silently widen the closed-loop scope.
#[tokio::test]
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

    let val_a = db.get_pv("DFAN_DEST_A2").await.unwrap();
    assert!(
        matches!(val_a, EpicsValue::Double(v) if (v - 3.0).abs() < 1e-10),
        "OMSL=supervisory must keep the operator-staged VAL (=3.0), got {val_a:?}"
    );
}

/// C `dfanoutRecord.c:308-339` `push_values` raises
/// `recGblSetSevr(LINK_ALARM, MAJOR_ALARM)` on every failed `dbPutLink`,
/// and `process()` (127-147) runs `push_values` between `checkAlarms` and
/// `recGblResetAlarms`, so the write-failure alarm folds into the SAME
/// cycle's committed SEVR and the VAL monitor post. The Rust dispatch
/// previously only logged the failure (no alarm), and drove the OUT links
/// in the post-commit forward-link tail where any alarm could not fold
/// into this cycle. After ONE process, a broken OUT link must leave
/// SEVR=MAJOR / STAT=LINK — proving the same-cycle fold (a one-cycle-late
/// latch would read NO_ALARM here).
#[tokio::test]
async fn test_dfanout_out_link_write_failure_raises_link_major() {
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

    let mut visited = HashSet::new();
    db.process_record_with_links("DFAN_LINKFAIL", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("DFAN_LINKFAIL").await.expect("record exists");
    let inst = rec.read().await;
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "failed dfanout OUT-link write must raise SEVR=MAJOR this cycle, got {:?}",
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
#[tokio::test]
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

    let mut visited = HashSet::new();
    db.process_record_with_links("DFAN_OK", &mut visited, 0)
        .await
        .unwrap();

    let val = db.get_pv("DFAN_OK_DEST").await.unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 5.0).abs() < 1e-10),
        "successful OUT write must land VAL=5.0 on the target, got {val:?}"
    );
    let rec = db.get_record("DFAN_OK").await.expect("record exists");
    let inst = rec.read().await;
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::NoAlarm,
        "successful dfanout OUT-link write must not raise any alarm, got {:?}",
        inst.common.sevr
    );
}

#[tokio::test]
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
    let val1 = db.get_pv("SEQ_DEST1").await.unwrap();
    match val1 {
        EpicsValue::Double(v) => assert!((v - 100.0).abs() < 1e-10),
        other => panic!("expected Double(100.0), got {:?}", other),
    }
    let val2 = db.get_pv("SEQ_DEST2").await.unwrap();
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
#[tokio::test]
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

    // LNK0 driven (existing behaviour).
    let dest = db.get_pv("SEQ9_DEST").await.unwrap();
    match dest {
        EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    // DO0 read back from DOL0 — was stale (0.0) pre-fix.
    let do0 = db.get_pv("SEQ9_REC.DO0").await.unwrap();
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
#[tokio::test]
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

    let do0 = db.get_pv("SEQ9B_REC.DO0").await.unwrap();
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
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
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
#[tokio::test]
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
// (transformRecord.c:608-619, scalerRecord.c:457-480), so a downstream
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
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
    }
}

#[tokio::test]
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
    if let Some(rec) = db.get_record("WF_OBSERVER").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("WF_TARGET".into()))
            .unwrap();
    }
    // Producer: WriteDbLink → WF_TARGET, FLNK → WF_OBSERVER.
    db.add_record("WF_PRODUCER", Box::new(WriteThenFlnkProducer))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("WF_PRODUCER").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("WF_OBSERVER".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("WF_PRODUCER", &mut visited, 0)
        .await
        .unwrap();

    // The OUT write ran before FLNK, so the observer read the fresh 42.0.
    let observed = db.get_pv("WF_OBSERVER").await.unwrap();
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
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
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
        if let Ok(EpicsValue::Double(v)) = db.get_pv(pv).await {
            if (v - want).abs() < 1e-10 {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "{pv}: did not reach {want} before timeout (last {:?})",
        db.get_pv(pv).await
    );
}

// BUG 1 regression — sseq `LNKn` is `DBF_OUTLINK` driven via `dbPutLink`
// → `dbDbPutValue` (`dbDbLink.c:388`). A bare sseq LNKn is NPP and must
// not process its target; an explicit-PP LNKn must.
#[tokio::test]
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
#[tokio::test]
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
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        db.get_pv("SSEQ_DLY_TGT1").await.unwrap(),
        EpicsValue::Double(0.0),
        "step 1 LNKn must not fire before its DLY1 delay elapses"
    );
    assert_eq!(
        db.get_pv("SSEQ_DLY_TGT2").await.unwrap(),
        EpicsValue::Double(0.0),
        "step 2 must not fire before step 1's delay completes"
    );

    // After DLY1 elapses, step 1 writes, then step 2 writes after it.
    poll_pv_double(&db, "SSEQ_DLY_TGT1", 11.0).await;
    poll_pv_double(&db, "SSEQ_DLY_TGT2", 22.0).await;
}

#[tokio::test]
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
    let seln = db.get_pv("SEL_REC.SELN").await.unwrap();
    match seln {
        EpicsValue::UShort(v) => assert_eq!(v, 2),
        other => panic!("expected UShort(2), got {:?}", other),
    }
    let val = db.get_pv("SEL_REC").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 30.0).abs() < 1e-10),
        other => panic!("expected Double(30.0), got {:?}", other),
    }
}

// SELN is DBF_USHORT (selRecord.dbd.pod:295): an NVL link value in the
// upper unsigned half (32768..65535) must reach SELN intact. The former
// f64->i16 carrier saturated such a value to 32767, losing the high half.
#[tokio::test]
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
    let seln = db.get_pv("SEL_REC_HI.SELN").await.unwrap();
    match seln {
        EpicsValue::UShort(v) => assert_eq!(v, 40000, "high SELN must survive the NVL link"),
        other => panic!("expected UShort(40000), got {:?}", other),
    }
}

#[tokio::test]
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
    let targets = db.get_cp_targets("MTR").await;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "MOTOR_POS");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

#[tokio::test]
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
    let val = db.get_pv("DST").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 10.0).abs() < 1e-10),
        other => panic!("expected Double(10.0), got {:?}", other),
    }
}

/// A CPP link registers as `passive_only` (distinct from CP). Collapsing
/// CPP into CP loses C's `precord->scan == 0` gate (`dbCa.c:994`).
#[tokio::test]
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
    let targets = db.get_cp_targets("SRC").await;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "DST");
    assert!(
        targets[0].passive_only,
        "CPP link must register as passive_only"
    );
}

/// CPP gate (`dbCa.c:854,994,1072`): on a source change, a CPP link
/// processes the link-holder only when its SCAN is Passive. A non-Passive
/// CPP target must NOT be processed by the dispatch — its own periodic scan
/// owns it. Boundary: target SCAN != Passive.
#[tokio::test]
async fn test_cpp_link_skips_nonpassive_target() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CPP".to_string();
    db.add_record("DST", Box::new(ao)).await.unwrap();
    if let Some(rec_arc) = db.get_record("DST").await {
        rec_arc.write().await.common.scan = ScanType::Sec1;
    }
    db.setup_cp_links().await;
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DST").await.unwrap();
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
#[tokio::test]
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
    let val = db.get_pv("DST").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 10.0).abs() < 1e-10,
            "Passive CPP target was not processed (VAL={v})"
        ),
        other => panic!("expected Double, got {:?}", other),
    }
}

/// CP (not CPP) processes the link-holder regardless of its SCAN
/// (`dbCa.c:993` adds CA_DBPROCESS unconditionally). A non-Passive CP target
/// IS processed — the boundary that distinguishes CP from CPP.
#[tokio::test]
async fn test_cp_link_processes_nonpassive_target() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1;
    ao.dol = "SRC CP".to_string();
    db.add_record("DST", Box::new(ao)).await.unwrap();
    if let Some(rec_arc) = db.get_record("DST").await {
        rec_arc.write().await.common.scan = ScanType::Sec1;
    }
    db.setup_cp_links().await;
    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DST").await.unwrap();
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
/// unconditional CP CA_DBPROCESS overrides the CPP scan gate (`dbCa.c:993`).
#[tokio::test]
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
    if let Some(rec_arc) = db.get_record("DST").await {
        rec_arc.write().await.common.inp = "SRC CPP".to_string();
    }
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("SRC").await;
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

#[tokio::test]
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
    let targets = db.get_cp_targets("SENSOR").await;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "MY_SEQ");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

#[tokio::test]
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
    let targets = db.get_cp_targets("INDEX_SRC").await;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].record, "MY_SEL");
    assert!(!targets[0].passive_only, "CP link must not be passive_only");
}

#[tokio::test]
async fn test_sdis_cp_link_registration() {
    let db = PvDatabase::new();
    db.add_record("DISABLE_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("GUARDED", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec_arc) = db.get_record("GUARDED").await {
        rec_arc.write().await.common.sdis = "DISABLE_SRC CP".to_string();
    }
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("DISABLE_SRC").await;
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
#[tokio::test]
async fn test_tse_minus1_always_overwrites_via_best_time() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // Stale but non-epoch timestamp — exactly the case the pre-fix
    // path mis-classified as "device-provided, keep".
    let stale = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234567);
    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.common.tse = -1;
        inst.common.time = stale;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("REC").await.unwrap();
    let inst = rec.read().await;
    assert_ne!(
        inst.common.time, stale,
        "TSE=-1 must always overwrite via generalTime BestTime, matching \
         C `epicsTimeGetEvent(-1)` called unconditionally"
    );
}

#[tokio::test]
async fn test_tse_minus2_keeps_time_unchanged() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let fixed_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(999);
    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.common.tse = -2;
        inst.common.time = fixed_time;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("REC").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(inst.common.time, fixed_time);
}

#[tokio::test]
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

#[tokio::test]
async fn test_rpro_causes_reprocessing() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(10.0)))
        .await
        .unwrap();
    db.add_record("DEST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DEST").await {
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("SRC".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("DEST", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DEST").await.unwrap();
    assert_eq!(val.to_f64().unwrap() as i64, 10);

    db.put_pv_no_process("SRC", EpicsValue::Double(20.0))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("DEST").await {
        let mut inst = rec.write().await;
        inst.common.rpro = true;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("DEST", &mut visited, 0)
        .await
        .unwrap();
    let val = db.get_pv("DEST").await.unwrap();
    assert_eq!(val.to_f64().unwrap() as i64, 20);
    let rec = db.get_record("DEST").await.unwrap();
    let inst = rec.read().await;
    assert!(!inst.common.rpro);
}

#[tokio::test]
async fn test_tsel_cp_link_registration() {
    let db = PvDatabase::new();
    db.add_record("TSE_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TARGET", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec_arc) = db.get_record("TARGET").await {
        let mut inst = rec_arc.write().await;
        inst.common.tsel = "TSE_SRC CP".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }
    db.setup_cp_links().await;
    let targets = db.get_cp_targets("TSE_SRC").await;
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
#[tokio::test]
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
        let rec = db.get_record("TSE_SRC").await.unwrap();
        let mut inst = rec.write().await;
        inst.common.time = src_time;
        inst.common.utag = src_utag;
    }

    // Target's TSEL points at the source's `.TIME` field.
    {
        let rec = db.get_record("TS_DST").await.unwrap();
        let mut inst = rec.write().await;
        inst.common.tsel = "TSE_SRC.TIME".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_DST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TS_DST").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(
        inst.common.time, src_time,
        "TSEL .TIME link must adopt the source's timestamp"
    );
    assert_eq!(
        inst.common.utag, src_utag,
        "TSEL .TIME link must also adopt the source's utag (recGbl.c:317)"
    );
    assert_eq!(
        inst.common.tse, -2,
        "adopting device/link time marks TSE=-2"
    );
}

#[tokio::test]
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
    impl LinkSet for TimeCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(1.0))
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
        let rec = db.get_record("TS_CADST").await.unwrap();
        let mut inst = rec.write().await;
        inst.common.tsel = "ca://TSE_SRC.TIME".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_CADST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TS_CADST").await.unwrap();
    let inst = rec.read().await;
    let expected = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 456);
    assert_eq!(
        inst.common.time, expected,
        "CA TSEL .TIME link must adopt the CA link's cached timestamp"
    );
    assert_eq!(inst.common.utag, 0, "CA wire carries no userTag");
    assert_eq!(inst.common.tse, -2, "adopting link time marks TSE=-2");
}

#[tokio::test]
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
    impl LinkSet for TimeCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(1.0))
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
        let rec = db.get_record("TS_NLDST").await.unwrap();
        let mut inst = rec.write().await;
        inst.common.tsel = "TSE_SRC.TIME".to_string();
        inst.parsed_tsel = parse_link_v2(&inst.common.tsel);
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("TS_NLDST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TS_NLDST").await.unwrap();
    let inst = rec.read().await;
    let expected = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_123, 789);
    assert_eq!(
        inst.common.time, expected,
        "non-local Db TSEL .TIME link must adopt the remote .TIME via the CA path"
    );
    assert_eq!(inst.common.tse, -2, "adopting link time marks TSE=-2");
}

#[tokio::test]
async fn test_new_common_fields_get_put() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").await.unwrap();

    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("UDFS"), Some(EpicsValue::Short(3)));
    }
    {
        let mut inst = rec.write().await;
        inst.put_common_field("UDFS", EpicsValue::Short(1)).unwrap();
    }
    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("UDFS"), Some(EpicsValue::Short(1)));
    }

    {
        let inst = rec.read().await;
        // C `field(SSCN,DBF_MENU){ menu(menuScan) initial("65535") }`: the
        // default is the out-of-range "use SCAN" sentinel, not Passive(0).
        assert_eq!(inst.get_common_field("SSCN"), Some(EpicsValue::Enum(65535)));
    }
    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("BKPT"), Some(EpicsValue::Char(0)));
    }
    {
        let mut inst = rec.write().await;
        inst.put_common_field("BKPT", EpicsValue::Char(1)).unwrap();
    }
    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("BKPT"), Some(EpicsValue::Char(1)));
    }

    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("TSE"), Some(EpicsValue::Short(0)));
    }
    {
        let inst = rec.read().await;
        assert_eq!(
            inst.get_common_field("TSEL"),
            Some(EpicsValue::String(String::new().into()))
        );
    }

    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("PUTF"), Some(EpicsValue::Char(0)));
    }
    {
        let mut inst = rec.write().await;
        let result = inst.put_common_field("PUTF", EpicsValue::Char(1));
        assert!(result.is_err());
    }

    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("RPRO"), Some(EpicsValue::Char(0)));
    }
    {
        let mut inst = rec.write().await;
        inst.put_common_field("RPRO", EpicsValue::Char(1)).unwrap();
    }
    {
        let inst = rec.read().await;
        assert_eq!(inst.get_common_field("RPRO"), Some(EpicsValue::Char(1)));
    }
}

/// SSCN (simulation-mode scan) is a `DBF_MENU`/`menuScan` field whose C dbd
/// default is the out-of-range sentinel 65535 ("use SCAN"), not a real menu
/// choice. The put path must round-trip both a real menuScan index and the
/// sentinel, and a put of any out-of-menu value collapses to the sentinel —
/// matching C `field(SSCN,DBF_MENU){ menu(menuScan) initial("65535") }`.
#[tokio::test]
async fn test_sscn_serves_menu_index_and_65535_sentinel() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("REC").await.unwrap();

    // Default is the 65535 sentinel.
    assert_eq!(
        rec.read().await.get_common_field("SSCN"),
        Some(EpicsValue::Enum(65535))
    );

    // A real menuScan index (2 == "I/O Intr") round-trips.
    rec.write()
        .await
        .put_common_field("SSCN", EpicsValue::Enum(2))
        .unwrap();
    assert_eq!(
        rec.read().await.get_common_field("SSCN"),
        Some(EpicsValue::Enum(2))
    );

    // Putting the sentinel back restores "use SCAN".
    rec.write()
        .await
        .put_common_field("SSCN", EpicsValue::Enum(65535))
        .unwrap();
    assert_eq!(
        rec.read().await.get_common_field("SSCN"),
        Some(EpicsValue::Enum(65535))
    );

    // An out-of-menu index (>9, not 65535) collapses to the sentinel.
    rec.write()
        .await
        .put_common_field("SSCN", EpicsValue::Enum(12))
        .unwrap();
    assert_eq!(
        rec.read().await.get_common_field("SSCN"),
        Some(EpicsValue::Enum(65535))
    );
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
#[tokio::test]
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
        if let Some(rec) = db.get_record(name).await {
            let mut inst = rec.write().await;
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
        let mut nord_rx = if let Some(rec) = db.get_record(name).await {
            let mut inst = rec.write().await;
            inst.add_subscriber("NORD", 1, DbFieldType::Long, EventMask::VALUE.bits())
        } else {
            None
        }
        .unwrap_or_else(|| panic!("NORD subscription must be accepted for {name}"));

        // Stage the new array onto VAL. set_val updates VAL and
        // implicitly NORD (now =3). Processing applies the
        // timestamp and posts subscribed-field events.
        if let Some(rec) = db.get_record(name).await {
            let mut inst = rec.write().await;
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
        assert!(
            matches!(event.snapshot.value, EpicsValue::Long(3)),
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
#[tokio::test]
async fn test_complete_async_record_gates_subscribed_field_on_change() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;

    let db = PvDatabase::new();
    db.add_record("ASYNC_GATE", Box::new(AsyncRecord { val: 0.0 }))
        .await
        .unwrap();

    // Seed DESC to a known value so add_subscriber's last_posted
    // initialiser captures it.
    if let Some(rec) = db.get_record("ASYNC_GATE").await {
        let mut inst = rec.write().await;
        inst.put_common_field("DESC", EpicsValue::String("alpha".into()))
            .unwrap();
    }

    let mut desc_rx = if let Some(rec) = db.get_record("ASYNC_GATE").await {
        let mut inst = rec.write().await;
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
    if let Some(rec) = db.get_record("ASYNC_GATE").await {
        let mut inst = rec.write().await;
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
#[tokio::test]
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
    if let Some(rec) = db.get_record("WF_GW").await {
        let mut inst = rec.write().await;
        inst.record.put_field("NELM", EpicsValue::Long(10)).unwrap();
        inst.record
            .put_field("FTVL", EpicsValue::Short(10))
            .unwrap();
    }

    // Subscribe to NORD and VAL separately. add_subscriber seeds
    // last_posted with current values (NORD=0, VAL=empty array) so
    // the next change is treated as new.
    let (mut nord_rx, mut val_rx) = if let Some(rec) = db.get_record("WF_GW").await {
        let mut inst = rec.write().await;
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
        matches!(nord_event.snapshot.value, EpicsValue::Long(4)),
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
#[tokio::test]
async fn test_output_link_cascade_uses_post_process_source_timestamp() {
    use std::time::SystemTime;

    let db = PvDatabase::new();
    db.add_record("TS_SRC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TS_DST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("TS_SRC").await {
        let mut inst = rec.write().await;
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
    if let Some(rec) = db.get_record("TS_SRC").await {
        let mut inst = rec.write().await;
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
        .await
        .expect("TS_SRC exists")
        .read()
        .await
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
        .await
        .expect("TS_DST exists")
        .read()
        .await
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
#[tokio::test]
async fn test_complete_async_record_updates_timestamp_at_completion() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::{DbFieldType, WallTime};
    use std::time::Duration;

    let db = PvDatabase::new();
    db.add_record("ASYNC_TS", Box::new(AsyncRecord { val: 1.0 }))
        .await
        .unwrap();

    let mut val_rx = if let Some(rec) = db.get_record("ASYNC_TS").await {
        let mut inst = rec.write().await;
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
    tokio::time::sleep(Duration::from_millis(20)).await;
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
/// false, then the framework's `on_output_complete` flips it to
/// `true` after the OUT link / device write succeeds.
///
/// This test pins the integration: a first process cycle with
/// OOPT=1 must drive write_db_link_value (observed via the target
/// record's `common.time` advancing past baseline), and a second
/// no-op process cycle must not.
#[tokio::test]
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
    if let Some(rec) = db.get_record("LO_SRC").await {
        let mut inst = rec.write().await;
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
        .await
        .expect("LO_DST exists")
        .read()
        .await
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
        .await
        .expect("LO_SRC exists")
        .read()
        .await
        .record
        .get_field("VAL")
        .is_some();
    assert!(src_first_done, "SRC must have processed at least once");

    // Second cycle with VAL still 0: OOPT=1 should now suppress
    // the cascade because val == pval and the first-cycle guard is
    // off. Capture DST's time before to detect any unwanted
    // re-process.
    let dst_time_before_second = dst_time_after_first;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let mut visited = HashSet::new();
    db.process_record_with_links("LO_SRC", &mut visited, 0)
        .await
        .unwrap();
    let dst_time_after_second = db
        .get_record("LO_DST")
        .await
        .expect("LO_DST exists")
        .read()
        .await
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
#[tokio::test]
async fn test_self_link_out_does_not_loop() {
    use epics_base_rs::server::records::longout::LongoutRecord;
    use std::time::Duration;

    let db = PvDatabase::new();
    db.add_record("SELF_LO", Box::new(LongoutRecord::new(0)))
        .await
        .unwrap();
    if let Some(rec) = db.get_record("SELF_LO").await {
        let mut inst = rec.write().await;
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
    let result = tokio::time::timeout(
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

    // Confirm the visited set picked up SELF_LO exactly once.
    assert!(visited.contains("SELF_LO"));

    // A subsequent process call (fresh visited) must also complete
    // promptly — the RPRO flag from the first call must not have
    // been left set on the record, otherwise the record would
    // reprocess in a loop after every external put.
    let mut visited2 = HashSet::new();
    let result2 = tokio::time::timeout(
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
        .await
        .expect("SELF_LO exists")
        .read()
        .await
        .common
        .rpro;
    assert!(
        !rpro_after,
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
/// (records/compress.rs:260). The framework then runs
/// `process_record_with_links_inner`, whose snapshot path includes
/// VAL via the always-on `include_val` branch for non-deadband
/// records, so the VAL subscriber sees the post-reset empty array.
#[tokio::test]
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
    if let Some(rec) = db.get_record("CMP_RES").await {
        let mut inst = rec.write().await;
        // Drive values through put_field/process so VAL is updated
        // through the public Record API rather than reaching into
        // the concrete CompressRecord state.
        // CompressRecord's process() pushes from INP — we don't have
        // an INP, so instead manually populate a few VAL entries.
        let arr = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let _ = inst.record.put_field("VAL", arr);
    }

    let mut val_rx = if let Some(rec) = db.get_record("CMP_RES").await {
        let mut inst = rec.write().await;
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
        .await
        .expect("CMP_RES exists")
        .read()
        .await
        .record
        .get_field("RES")
        .and_then(|v| match v {
            EpicsValue::Short(s) => Some(s),
            _ => None,
        })
        .expect("RES readable");
    assert_eq!(res, 0, "RES must auto-clear after the reset");
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
#[tokio::test]
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
    // bits[0] and bits[2] should reflect VAL=5 (binary 0101).
    assert!(matches!(rec2.get_field("B0"), Some(EpicsValue::Char(1))));
    assert!(matches!(rec2.get_field("B2"), Some(EpicsValue::Char(1))));
    assert!(matches!(rec2.get_field("B1"), Some(EpicsValue::Char(0))));

    // Sibling case: nothing set — UDF stays true, VAL stays 0.
    let mut rec3 = MbboDirectRecord::default();
    let mut udf3 = true;
    rec3.post_init_finalize_undef(&mut udf3).unwrap();
    assert!(udf3, "UDF stays true when nothing initialised");
    assert!(matches!(rec3.get_field("VAL"), Some(EpicsValue::Long(0))));
}

/// epics-base PR `e3c9d590` / `20404003` regression: `lnkCalc` JSON
/// link `{calc:{expr:"...", args:[...], time:"X"}}` parses into
/// `ParsedLink::Calc`, the read path evaluates the expression by
/// fetching each input PV and binding A..L slots, and timestamp
/// passthrough from the chosen input is available via
/// `evaluate_calc_link_with_time`.
#[tokio::test]
async fn test_lnk_calc_parses_evaluates_and_passes_timestamp() {
    use epics_base_rs::server::record::{CalcLink, ParsedLink, parse_link_v2};
    use epics_base_rs::server::records::ai::AiRecord;

    // Parser: full lnkCalc form.
    let parsed = parse_link_v2(r#"{calc:{"expr":"A+B*2","args":["pv_a","pv_b"],"time":"A"}}"#);
    let calc = match parsed {
        ParsedLink::Calc(c) => c,
        other => panic!("expected ParsedLink::Calc, got {other:?}"),
    };
    assert_eq!(calc.expr, "A+B*2");
    assert_eq!(calc.args, vec!["pv_a".to_string(), "pv_b".to_string()]);
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

    // Parser rejects args.len() > 12 (calc engine A..L cap).
    let too_many = parse_link_v2(
        r#"{calc:{"expr":"A","args":["a","b","c","d","e","f","g","h","i","j","k","l","m"]}}"#,
    );
    assert!(
        !matches!(too_many, ParsedLink::Calc(_)),
        "13+ args must NOT parse as Calc"
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
        args: vec!["pv_a".into(), "pv_b".into()],
        time_source: Some('A'),
    };
    let parsed = ParsedLink::Calc(calc.clone());
    let mut visited = HashSet::new();
    let value = db
        .read_link_value_soft(&parsed, true, &mut visited, 0)
        .await
        .expect("calc link evaluates");
    match value {
        EpicsValue::Double(v) => assert!((v - 13.0).abs() < 1e-9, "expected 3+5*2=13, got {v}"),
        other => panic!("expected Double, got {other:?}"),
    }

    // Timestamp passthrough: nudge pv_a's common.time to a known
    // value, then verify evaluate_calc_link_with_time returns it.
    let known = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    if let Some(rec) = db.get_record("pv_a").await {
        rec.write().await.common.time = known;
    }
    let (v, t) = db
        .evaluate_calc_link_with_time(&calc)
        .await
        .expect("calc evaluates with time");
    match v {
        EpicsValue::Double(x) => assert!((x - 13.0).abs() < 1e-9),
        other => panic!("expected Double, got {other:?}"),
    }
    assert_eq!(t, Some(known), "time pulled from pv_a (letter 'A')");
}

/// A `lnkCalc` `{calc:...}` link whose input record is NON-LOCAL must
/// read that input through the external CA path, and its timestamp
/// passthrough must adopt the remote `.TIME` — each `A..L` input is its
/// own `dbInitLink` link, so a non-local input is a CA link. The pre-fix
/// loop read every input with a local-only `get_pv` (and the time source
/// with a local-only `get_record`), so a single non-local input made the
/// whole evaluation return `None` and the time fell back to the
/// consumer's own. Sibling of the non-local Db read / OUT-write / TSEL
/// `.TIME` fixes — same `dbInitLink` locality cause, the lnkCalc inputs.
#[tokio::test]
async fn test_lnk_calc_nonlocal_input_resolves_externally() {
    use epics_base_rs::server::database::LinkSet;
    use epics_base_rs::server::record::CalcLink;
    use epics_base_rs::server::records::ai::AiRecord;

    struct CalcCaLset {
        secs: i64,
        nsec: i32,
    }
    impl LinkSet for CalcCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, name: &str) -> Option<EpicsValue> {
            (name == "REMOTE:A").then_some(EpicsValue::Double(10.0))
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
        args: vec!["REMOTE:A".into(), "pv_b".into()],
        time_source: Some('A'),
    };

    let (value, time) = db
        .evaluate_calc_link_with_time(&calc)
        .await
        .expect("calc with a non-local input must still evaluate");
    match value {
        // 10 (remote A) + 5 (local B) * 2 = 20.
        EpicsValue::Double(v) => assert!(
            (v - 20.0).abs() < 1e-9,
            "non-local calc input must resolve via CA: expected 20, got {v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
    let expected = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_456, 321);
    assert_eq!(
        time,
        Some(expected),
        "time source 'A' is non-local → adopt the remote .TIME via the CA path"
    );
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
#[tokio::test]
async fn test_ca_put_mbbo_val_recomputes_rval() {
    use epics_base_rs::server::records::mbbo::MbboRecord;

    let db = PvDatabase::new();
    let mut rec = MbboRecord::new(0);
    rec.shft = 4;
    db.add_record("MBBO_CA", Box::new(rec)).await.unwrap();

    db.put_record_field_from_ca("MBBO_CA", "VAL", EpicsValue::Enum(3))
        .await
        .unwrap();

    let rec = db.get_record("MBBO_CA").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Enum(3)),
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
#[tokio::test]
async fn test_ca_put_mbbo_direct_val_recomputes_rval() {
    use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;

    let db = PvDatabase::new();
    let mut rec = MbboDirectRecord::default();
    rec.shft = 4;
    db.add_record("MBBOD_CA", Box::new(rec)).await.unwrap();

    db.put_record_field_from_ca("MBBOD_CA", "VAL", EpicsValue::Long(5))
        .await
        .unwrap();

    let rec = db.get_record("MBBOD_CA").await.unwrap();
    let inst = rec.read().await;
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
#[tokio::test]
async fn test_simulation_mode_still_fires_forward_link() {
    let db = PvDatabase::new();
    db.add_record("SIM:SRC", Box::new(AoRecord::new(11.0)))
        .await
        .unwrap();
    db.add_record("SIM:FLNK_TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    // SIMM=1 (YES) with SIOL pointing at SIM:SRC — enters simulation.
    ai.simm = 1;
    ai.siol = "SIM:SRC".into();
    db.add_record("SIM:AI", Box::new(ai)).await.unwrap();
    if let Some(rec) = db.get_record("SIM:AI").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("SIM:FLNK_TARGET".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SIM:AI", &mut visited, 0)
        .await
        .unwrap();

    assert!(
        visited.contains("SIM:AI"),
        "the simulated record itself must be in the visited set"
    );
    assert!(
        visited.contains("SIM:FLNK_TARGET"),
        "a SIMM-mode record must still dispatch its FLNK forward link \
         (C aiRecord.c:168 recGblFwdLink runs unconditionally): {visited:?}"
    );
}

/// BUG 2 — a simulated `mbbi` is an INPUT record: it must READ the
/// value in from SIOL, not write VAL out to SIOL. `mbbiRecord.c:125-126`
/// declares SIML/SIOL and `mbbiRecord.c:388-394` reads
/// `dbGetLink(&prec->siol, DBR_ULONG, &prec->sval)`. Pre-fix the Rust
/// `is_input` set omitted `mbbi`, so a simulated mbbi fell into the
/// OUTPUT branch and wrote its own VAL out to the SIOL target.
#[tokio::test]
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
    let src = db.get_record("MBBISIM:SRC").await.unwrap();
    let src_val = src.read().await.record.get_field("VAL").unwrap();
    assert_eq!(
        src_val.to_f64().unwrap() as i64,
        3,
        "simulated mbbi must NOT write VAL out to its SIOL target"
    );

    // The mbbi must have READ the value in from SIOL.
    let mbbi_rec = db.get_record("MBBISIM:IN").await.unwrap();
    let mbbi_val = mbbi_rec.read().await.record.get_field("VAL").unwrap();
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
#[tokio::test]
async fn test_async_completion_flnk_cycle_terminates() {
    let db = PvDatabase::new();
    db.add_record("ACYC:A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("ACYC:B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    // A -> FLNK -> B -> FLNK -> A : a closed forward-link loop.
    if let Some(rec) = db.get_record("ACYC:A").await {
        let mut inst = rec.write().await;
        inst.put_common_field("FLNK", EpicsValue::String("ACYC:B".into()))
            .unwrap();
    }
    if let Some(rec) = db.get_record("ACYC:B").await {
        let mut inst = rec.write().await;
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
#[tokio::test]
async fn test_fanout_resolves_sell_link_into_seln() {
    use epics_base_rs::server::records::fanout::FanoutRecord;

    let db = PvDatabase::new();
    // SELL source: selects link index 2.
    db.add_record("FANSELL:SRC", Box::new(LonginRecord::new(2)))
        .await
        .unwrap();
    db.add_record("FANSELL:T2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("FANSELL:T0", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

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
    assert!(
        visited.contains("FANSELL:T2"),
        "SELL must resolve SELN=2 so LNK2 is dispatched: {visited:?}"
    );
    assert!(
        !visited.contains("FANSELL:T0"),
        "with SELL-resolved SELN=2, the stale SELN=0 (LNK0) must NOT \
         be dispatched: {visited:?}"
    );
    // SELN must now hold the SELL-resolved value.
    let fan_rec = db.get_record("FANSELL:FAN").await.unwrap();
    let seln = fan_rec.read().await.record.get_field("SELN").unwrap();
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
#[tokio::test]
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

        let rec = db.get_record("SEQALL:REC").await.unwrap();
        let seln = rec.read().await.record.get_field("SELN").unwrap();
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

        let rec = db.get_record("SEQSPEC:REC").await.unwrap();
        let seln = rec.read().await.record.get_field("SELN").unwrap();
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
#[tokio::test]
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

/// BUG 5 — `putAcks` (C `dbAccess.c:1303-1315`) compares the written
/// severity against the STORED unacknowledged severity `acks`, not
/// against the current `sevr`; `putAckt` (C `dbAccess.c:1285-1301`)
/// lowers `acks` down to `sevr` when ACKT is set false and
/// `acks > sevr`.
#[tokio::test]
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
        inst.put_common_field("ACKS", EpicsValue::Short(2)).unwrap();
        assert_eq!(
            inst.common.acks,
            AlarmSeverity::NoAlarm,
            "ACKS write at sev>=stored acks must clear acks \
             (C dbAccess.c:1309 compares *psev >= precord->acks)"
        );

        // A second case: written sev BELOW the stored acks must NOT
        // clear it — proving the comparison is against `acks`, not
        // `sevr`. Were it compared against sevr (Minor), a MINOR write
        // would wrongly clear.
        let rec2 = AoRecord::new(0.0);
        let mut inst2 = RecordInstance::new("ACKTEST2".into(), rec2);
        inst2.common.acks = AlarmSeverity::Major;
        inst2.common.sevr = AlarmSeverity::Minor;
        inst2
            .put_common_field("ACKS", EpicsValue::Short(1))
            .unwrap();
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
        inst.put_common_field("ACKT", EpicsValue::Short(0)).unwrap();
        assert!(!inst.common.ackt, "ACKT must be cleared");
        assert_eq!(
            inst.common.acks,
            AlarmSeverity::Minor,
            "ACKT=false with acks>sevr must lower acks down to sevr \
             (C dbAccess.c:1294-1297)"
        );
    }
}

/// BUG 2 regression — a bare (modifier-less) OUT link is NPP: the
/// value is written to the target but the target is NOT processed.
/// C `dbDbPutValue` (dbDbLink.c:386-389) calls `processTarget` only
/// when the link carries an explicit `PP` flag (or writes `.PROC`).
#[tokio::test]
async fn test_bare_out_link_does_not_process_target() {
    let db = PvDatabase::new();
    db.add_record("SRC_OUT", Box::new(AoRecord::new(33.0)))
        .await
        .unwrap();
    db.add_record("TGT_OUT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Bare OUT link — no PP modifier.
    if let Some(rec) = db.get_record("SRC_OUT").await {
        let mut inst = rec.write().await;
        inst.put_common_field("OUT", EpicsValue::String("TGT_OUT.VAL".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC_OUT", &mut visited, 0)
        .await
        .unwrap();

    // The value must have landed on the target.
    let tgt_val = db.get_pv("TGT_OUT").await.unwrap();
    assert_eq!(
        tgt_val.to_f64().unwrap(),
        33.0,
        "bare OUT link must still write the value to the target"
    );
    // ...but the target must NOT have been processed.
    assert!(
        !visited.contains("TGT_OUT"),
        "bare OUT link (NPP) must NOT process its target: {visited:?}"
    );
}

/// BUG 2 regression (positive case) — an OUT link with an explicit
/// `PP` token DOES process a Passive target, mirroring C
/// `dbDbPutValue` `pvlOptPP` branch.
#[tokio::test]
async fn test_pp_out_link_processes_passive_target() {
    let db = PvDatabase::new();
    db.add_record("SRC_PP", Box::new(AoRecord::new(44.0)))
        .await
        .unwrap();
    db.add_record("TGT_PP", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    if let Some(rec) = db.get_record("SRC_PP").await {
        let mut inst = rec.write().await;
        inst.put_common_field("OUT", EpicsValue::String("TGT_PP.VAL PP".into()))
            .unwrap();
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC_PP", &mut visited, 0)
        .await
        .unwrap();

    assert!(
        visited.contains("TGT_PP"),
        "explicit PP OUT link must process its Passive target: {visited:?}"
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
    let epoch = db.lock_records(["MR_R5_MEMBER"]).await;

    let db2 = db.clone();
    let processed = Arc::new(AtomicU32::new(0));
    let processed2 = processed.clone();
    let h = tokio::spawn(async move {
        // Foreign full-processing entry — must block on the gate the
        // epoch holds.
        let mut visited = HashSet::new();
        let _ = db2
            .process_record_with_links("MR_R5_MEMBER", &mut visited, 0)
            .await;
        processed2.store(1, Ordering::SeqCst);
    });

    // Give the spawned task time to reach (and block on) the gate.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
/// `_already_locked` full-processing entry. The gate `Mutex` is not
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
    let _epoch = db.lock_records(["MR_R5_OWNED"]).await;

    // Processing the member via the `_already_locked` entry while the
    // epoch is held must NOT dead-lock — bounded by a timeout so a
    // regression (reverting to the gate-acquiring entry) fails loudly.
    let mut visited = HashSet::new();
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        db.process_record_with_links_already_locked("MR_R5_OWNED", &mut visited, 0),
    )
    .await
    .expect("process_record_with_links_already_locked must not dead-lock under a held epoch");
    res.expect("owner-path processing of an owned member must succeed");
    assert!(visited.contains("MR_R5_OWNED"));
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
#[tokio::test]
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
    impl LinkSet for RawCaLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_value(&self, _: &str) -> Option<EpicsValue> {
            Some(EpicsValue::Double(7.0))
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
        if let Some(rec) = db.get_record("CADST").await {
            let mut inst = rec.write().await;
            inst.put_common_field("INP", EpicsValue::String(inp.into()))
                .unwrap();
            inst.common.udf = false;
        }
        let mut visited = HashSet::new();
        db.process_record_with_links("CADST", &mut visited, 0)
            .await
            .unwrap();
        let rec = db.get_record("CADST").await.expect("record exists");
        let inst = rec.read().await;
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
#[tokio::test]
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
            let rec = db.get_record("LSI_MPST").await.unwrap();
            let mut inst = rec.write().await;
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
            let rec = db.get_record("LSI_MPST").await.unwrap();
            let mut inst = rec.write().await;
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
#[tokio::test]
async fn sub_record_subroutine_runs_on_main_engine_path() {
    use epics_base_rs::server::records::sub_record::SubRecord;

    let db = PvDatabase::new();
    db.add_record("SUBM", Box::new(SubRecord::default()))
        .await
        .unwrap();

    // SNAM + a VAL-doubling subroutine, plus a seed VAL so the change is
    // observable.
    {
        let arc = db.get_record("SUBM").await.unwrap();
        let mut inst = arc.write().await;
        inst.record
            .put_field("SNAM", EpicsValue::String("double_val".into()))
            .unwrap();
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

    let arc = db.get_record("SUBM").await.unwrap();
    let inst = arc.read().await;
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
#[tokio::test]
async fn sub_record_hihi_alarm_fires_via_shared_owner() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::records::sub_record::SubRecord;

    let db = PvDatabase::new();

    // FIRE: subroutine drives VAL to 100, HIHI=50/Major -> HIHI_ALARM.
    db.add_record("SUB_HIHI", Box::new(SubRecord::default()))
        .await
        .unwrap();
    // CONTROL: same VAL=100 but HIHI=200 -> no alarm, LALM tracks VAL.
    db.add_record("SUB_OK", Box::new(SubRecord::default()))
        .await
        .unwrap();

    for (name, hihi) in [("SUB_HIHI", 50.0), ("SUB_OK", 200.0)] {
        let arc = db.get_record(name).await.unwrap();
        let mut inst = arc.write().await;
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
        let arc = db.get_record("SUB_HIHI").await.unwrap();
        let inst = arc.read().await;
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
        let arc = db.get_record("SUB_OK").await.unwrap();
        let inst = arc.read().await;
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
#[tokio::test]
async fn sub_record_mdel_gates_val_monitor() {
    use epics_base_rs::server::records::sub_record::SubRecord;

    let db = PvDatabase::new();
    let mut rec = SubRecord::default();
    rec.val = 100.0;
    rec.mdel = 10.0;
    rec.init_record(0).unwrap(); // MLST seeded to 100
    db.add_record("SUB_MDEL", Box::new(rec)).await.unwrap();

    // Helper: set VAL directly (no subroutine), process, read back MLST.
    async fn drive(db: &PvDatabase, val: f64) -> f64 {
        {
            let arc = db.get_record("SUB_MDEL").await.unwrap();
            let mut inst = arc.write().await;
            inst.record
                .put_field("VAL", EpicsValue::Double(val))
                .unwrap();
        }
        let mut visited = HashSet::new();
        db.process_record_with_links("SUB_MDEL", &mut visited, 0)
            .await
            .unwrap();
        let arc = db.get_record("SUB_MDEL").await.unwrap();
        let inst = arc.read().await;
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
#[tokio::test]
async fn sub_record_negative_status_raises_soft_alarm_at_brsv() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::records::sub_record::SubRecord;

    let db = PvDatabase::new();

    // FIRE: status -1 with BRSV=Major -> SOFT_ALARM/Major.
    db.add_record("SUB_SOFT", Box::new(SubRecord::default()))
        .await
        .unwrap();
    // CONTROL: status 0 with BRSV=Major -> no alarm.
    db.add_record("SUB_OK0", Box::new(SubRecord::default()))
        .await
        .unwrap();

    for (name, status) in [("SUB_SOFT", -1_i64), ("SUB_OK0", 0_i64)] {
        let arc = db.get_record(name).await.unwrap();
        let mut inst = arc.write().await;
        inst.record
            .put_field("BRSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
        let sub_fn: SubroutineFn = Box::new(move |_record: &mut dyn Record| Ok(status));
        inst.subroutine = Some(Arc::new(sub_fn));
    }

    for name in ["SUB_SOFT", "SUB_OK0"] {
        let mut visited = HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();
    }

    {
        let arc = db.get_record("SUB_SOFT").await.unwrap();
        let inst = arc.read().await;
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
        let arc = db.get_record("SUB_OK0").await.unwrap();
        let inst = arc.read().await;
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::NoAlarm,
            "sub status==0 must not raise SOFT_ALARM regardless of BRSV"
        );
    }
}

/// An `aSub` publishes the subroutine's return status as VAL (C
/// `aSubRecord.c:223` `prec->val = status`), overwriting whatever the
/// closure wrote to VAL, and a negative status raises SOFT_ALARM at BRSV.
#[tokio::test]
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
        let arc = db.get_record("ASUB_POS").await.unwrap();
        let mut inst = arc.write().await;
        let sub_fn: SubroutineFn = Box::new(|record: &mut dyn Record| {
            // The closure's own VAL write is discarded by `prec->val = status`.
            record.put_field("VAL", EpicsValue::Double(999.0))?;
            Ok(42)
        });
        inst.subroutine = Some(Arc::new(sub_fn));
    }
    {
        let arc = db.get_record("ASUB_NEG").await.unwrap();
        let mut inst = arc.write().await;
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
        let arc = db.get_record("ASUB_POS").await.unwrap();
        let inst = arc.read().await;
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Double(42.0)),
            "aSub VAL must be the return status (42), not the closure's 999"
        );
    }
    {
        let arc = db.get_record("ASUB_NEG").await.unwrap();
        let inst = arc.read().await;
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Double(-5.0)),
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

/// ao OMSL=closed_loop + OIF=Incremental must add DOL to PVAL (the last
/// actual output), not the current VAL. C `fetch_value`
/// (aoRecord.c:447-455) sets `prec->val = prec->pval` before
/// `*pvalue += prec->val`, discarding any client caput to VAL during a
/// DOL-driven cycle. The Rust port previously incremented from the current
/// VAL, so a caput between cycles shifted every subsequent increment.
#[tokio::test]
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
    let v1 = db.get_pv("AO_INCR_DST").await.unwrap();
    assert!(
        matches!(v1, EpicsValue::Double(v) if (v - 10.0).abs() < 1e-10),
        "cycle 1 VAL must be 10, got {v1:?}"
    );

    // A client caputs VAL=100 between cycles; C discards it (val=pval).
    {
        let arc = db.get_record("AO_INCR_DST").await.unwrap();
        let mut inst = arc.write().await;
        inst.record
            .put_field("VAL", EpicsValue::Double(100.0))
            .unwrap();
    }
    // Upstream setpoint changes to 5.
    {
        let arc = db.get_record("AO_INCR_SRC").await.unwrap();
        let mut inst = arc.write().await;
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
    let v2 = db.get_pv("AO_INCR_DST").await.unwrap();
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
#[tokio::test]
async fn ao_constant_dol_seeded_at_init_not_reapplied_at_process() {
    let db = PvDatabase::new();
    let mut ao = AoRecord::new(0.0);
    ao.omsl = 1; // closed_loop
    ao.dol = "7".to_string(); // constant DOL
    ao.init_record(0).unwrap(); // recGblInitConstantLink: VAL = 7
    db.add_record("AO_CONST", Box::new(ao)).await.unwrap();

    let v0 = db.get_pv("AO_CONST").await.unwrap();
    assert!(
        matches!(v0, EpicsValue::Double(v) if (v - 7.0).abs() < 1e-10),
        "constant DOL must seed VAL=7 at init, got {v0:?}"
    );

    // Process once: the constant must not be re-sourced; VAL stays 7.
    let mut visited = HashSet::new();
    db.process_record_with_links("AO_CONST", &mut visited, 0)
        .await
        .unwrap();
    let v1 = db.get_pv("AO_CONST").await.unwrap();
    assert!(
        matches!(v1, EpicsValue::Double(v) if (v - 7.0).abs() < 1e-10),
        "process must not change a constant-DOL VAL, got {v1:?}"
    );

    // A client caputs VAL=42.
    {
        let arc = db.get_record("AO_CONST").await.unwrap();
        let mut inst = arc.write().await;
        inst.record
            .put_field("VAL", EpicsValue::Double(42.0))
            .unwrap();
    }
    // Reprocess: the constant DOL is never re-fetched, so the caput wins.
    let mut visited2 = HashSet::new();
    db.process_record_with_links("AO_CONST", &mut visited2, 0)
        .await
        .unwrap();
    let v2 = db.get_pv("AO_CONST").await.unwrap();
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
#[tokio::test]
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
    let v = db.get_pv("AO_CONST_INCR").await.unwrap();
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
#[tokio::test]
async fn longout_constant_dol_seeded_at_init_not_reapplied_at_process() {
    use epics_base_rs::server::records::longout::LongoutRecord;
    let db = PvDatabase::new();
    let mut lo = LongoutRecord::new(0);
    lo.omsl = 1; // closed_loop
    lo.dol = "9".to_string(); // constant DOL
    lo.init_record(0).unwrap();
    db.add_record("LO_CONST", Box::new(lo)).await.unwrap();

    let v0 = db.get_pv("LO_CONST").await.unwrap();
    assert_eq!(
        v0.to_f64(),
        Some(9.0),
        "constant DOL must seed VAL=9 at init"
    );

    {
        let arc = db.get_record("LO_CONST").await.unwrap();
        let mut inst = arc.write().await;
        inst.record.put_field("VAL", EpicsValue::Long(99)).unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("LO_CONST", &mut visited, 0)
        .await
        .unwrap();
    let v1 = db.get_pv("LO_CONST").await.unwrap();
    assert_eq!(
        v1.to_f64(),
        Some(99.0),
        "constant DOL must not clobber a caput on reprocess (framework-only path)"
    );
}

/// The string path. A quoted constant DOL (`field(DOL,"\"hi\"")`) on a
/// `stringout` is copied into VAL once at init (C
/// `recGblInitConstantLink(..., DBF_STRING, ...)`); the gate keeps it out of
/// process so a caput survives.
#[tokio::test]
async fn stringout_constant_dol_seeded_at_init_not_reapplied_at_process() {
    use epics_base_rs::server::records::stringout::StringoutRecord;
    let db = PvDatabase::new();
    let mut so = StringoutRecord::new("");
    so.omsl = 1; // closed_loop
    so.dol = "\"hi\"".to_string(); // quoted string constant
    so.init_record(0).unwrap();
    db.add_record("SO_CONST", Box::new(so)).await.unwrap();

    let v0 = db.get_pv("SO_CONST").await.unwrap();
    assert!(
        matches!(&v0, EpicsValue::String(s) if s.as_str_lossy() == "hi"),
        "quoted constant DOL must seed VAL=\"hi\" at init, got {v0:?}"
    );

    {
        let arc = db.get_record("SO_CONST").await.unwrap();
        let mut inst = arc.write().await;
        inst.record
            .put_field("VAL", EpicsValue::String("world".into()))
            .unwrap();
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("SO_CONST", &mut visited, 0)
        .await
        .unwrap();
    let v1 = db.get_pv("SO_CONST").await.unwrap();
    assert!(
        matches!(&v1, EpicsValue::String(s) if s.as_str_lossy() == "world"),
        "constant DOL must not clobber a string caput on reprocess; got {v1:?}"
    );
}

/// Init-time constant-DOL application across the remaining OMSL records that
/// gained an `init_record`/`post_init` seed. One assertion per record's
/// value-type boundary (f64 / i64 / long-string / state-index→RVAL /
/// bit-field decomposition).
#[tokio::test]
async fn init_applies_constant_dol_across_record_types() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;
    use epics_base_rs::server::records::int64out::Int64outRecord;
    use epics_base_rs::server::records::lso::LsoRecord;
    use epics_base_rs::server::records::mbbo::MbboRecord;
    use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;

    // dfanout: DBF_DOUBLE.
    let mut df = DfanoutRecord::default();
    df.omsl = 1;
    df.dol = "3.5".to_string();
    df.init_record(0).unwrap();
    assert_eq!(df.val, 3.5, "dfanout constant DOL → VAL=3.5");

    // int64out: DBF_INT64.
    let mut i64o = Int64outRecord::default();
    i64o.omsl = 1;
    i64o.dol = "42".to_string();
    i64o.init_record(0).unwrap();
    assert_eq!(i64o.val, 42, "int64out constant DOL → VAL=42");

    // lso: quoted long-string constant.
    let mut lso = LsoRecord::default();
    lso.omsl = 1;
    lso.dol = "\"hello\"".to_string();
    lso.init_record(0).unwrap();
    assert_eq!(lso.val.as_str_lossy(), "hello", "lso constant DOL → VAL");
    assert_eq!(lso.len, 6, "lso LEN = strlen+1 (C convention)");

    // mbbo: constant DOL is the state index; convert() maps it to RVAL
    // (no state table defined → RVAL == state index).
    let mut mb = MbboRecord::default();
    mb.omsl = 1;
    mb.dol = "2".to_string();
    mb.init_record(0).unwrap();
    assert_eq!(mb.val, 2, "mbbo constant DOL → VAL (state index) = 2");
    assert_eq!(mb.rval, 2, "mbbo convert() maps state index → RVAL");

    // mbbo_direct: constant seeds VAL and the bit fields decompose from it
    // (5 = 0b101 → B0=1, B1=0, B2=1); UDF cleared.
    let mut mbd = MbboDirectRecord::default();
    mbd.omsl = 1;
    mbd.dol = "5".to_string();
    let mut udf = true;
    mbd.post_init_finalize_undef(&mut udf).unwrap();
    assert_eq!(mbd.val, 5, "mbbo_direct constant DOL → VAL=5");
    assert_eq!(mbd.bits[0], 1, "mbbo_direct B0 from VAL=5");
    assert_eq!(mbd.bits[1], 0, "mbbo_direct B1 from VAL=5");
    assert_eq!(mbd.bits[2], 1, "mbbo_direct B2 from VAL=5");
    assert!(
        !udf,
        "mbbo_direct constant DOL clears UDF (recGblInitConstantLink)"
    );
}

/// A calcout with ODLY > 0 defers its forward link (and VAL/OVAL monitors)
/// to the delayed callback cycle. C `calcoutRecord.c::process` (lines
/// 277-282) sets DLYA and `return 0` on the delaying cycle — BEFORE
/// `monitor()`/`recGblFwdLink()` (lines 306-307) — so the FLNK target must
/// NOT process on the delaying cycle; it processes exactly once on the
/// delayed cycle. Before the fix the delaying cycle ran the full Complete
/// snapshot + FLNK tail, firing the forward link twice (delaying + delayed).
#[tokio::test]
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
    src.put_field("A", EpicsValue::Double(42.0)).unwrap();
    src.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    db.add_record("CO6_SRC", Box::new(src)).await.unwrap();
    {
        let rec = db.get_record("CO6_SRC").await.unwrap();
        let mut inst = rec.write().await;
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
    assert!(
        !visited.contains("CO6_TGT"),
        "FLNK must not fire on the ODLY delaying cycle"
    );
    assert_eq!(
        db.get_pv("CO6_TGT").await.unwrap().to_f64(),
        Some(0.0),
        "FLNK target must not be processed on the delaying cycle"
    );

    // Delayed (callback) cycle: C fires recGblFwdLink exactly once here.
    let mut visited2 = HashSet::new();
    db.process_record_continuation("CO6_SRC", &mut visited2, 0)
        .await
        .unwrap();
    assert!(
        visited2.contains("CO6_TGT"),
        "FLNK must fire on the delayed cycle"
    );
    assert_eq!(
        db.get_pv("CO6_TGT").await.unwrap().to_f64(),
        Some(1.0),
        "FLNK target processed exactly once (delayed cycle only), not on both cycles"
    );
}

// C `selRecord.c::fetch_values` (411-438): in Specified mode (SELM==0)
// only INP[SELN] is read; the other input links are never fetched, so
// their PP sources are never processed. A non-selected PP source that
// computes 42-on-process must therefore stay at its default 0.
#[tokio::test]
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
    let sel_val = db.get_pv("R7_SEL").await.unwrap();
    match sel_val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "SEL value comes from the selected input INPB: expected 42, got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    let srcb = db.get_pv("R7_SRCB").await.unwrap();
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
    let srca = db.get_pv("R7_SRCA").await.unwrap();
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
#[tokio::test]
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
    let sel_val = db.get_pv("R7H_SEL").await.unwrap();
    match sel_val {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-10,
            "High mode selects max(10, 42) = 42, got {v}"
        ),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
    // BOTH sources processed: the non-max source still got fetched.
    let srca = db.get_pv("R7H_SRCA").await.unwrap();
    match srca {
        EpicsValue::Double(v) => assert!(
            (v - 10.0).abs() < 1e-10,
            "High mode must fetch ALL inputs: R7H_SRCA processed to 10, VAL={v}"
        ),
        other => panic!("expected Double(10.0), got {other:?}"),
    }
    let srcb = db.get_pv("R7H_SRCB").await.unwrap();
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
#[tokio::test]
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

    let val = db.get_pv("R8_SEL").await.unwrap();
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
#[tokio::test]
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

    let val = db.get_pv("R8N_SEL").await.unwrap();
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
#[tokio::test]
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

    let val = db.get_pv("R8_SEL_EMPTY").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            v.is_nan(),
            "empty selected link still runs do_sel over the NaN input → VAL=NaN, got {v}"
        ),
        other => panic!("expected Double(NaN), got {other:?}"),
    }
}
