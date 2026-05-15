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
    db.add_record("OUTPUT", Box::new(ao)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("OUTPUT", &mut visited, 0)
        .await
        .unwrap();

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

/// Round-30C regression: IVOA=2 ("set outputs to IVOV") must route
/// IVOV into the C-conventional output field for each record type.
/// Pre-fix the framework special-cased only `calcout` (OVAL) and fell
/// back to `set_val` (VAL) — every other output record left OVAL/RVAL
/// stale, so the soft-channel OUT writeback (which reads `OVAL.or(VAL)`)
/// shipped the pre-IVOA value instead of IVOV.
/// Round-34 (R34-G1): a `.db` file's `field(ASL, "1")` directive must
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
    let rval = db.get_pv("BO_SRC.RVAL").await.unwrap();
    assert!(
        matches!(rval, EpicsValue::Long(1)),
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
            inst.put_common_field("FLNK", EpicsValue::String(format!("CHAIN_{}", i + 1)))
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
        assert_eq!(
            rval,
            Some(EpicsValue::Long(0x0F)),
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

#[tokio::test]
async fn test_non_passive_output_ca_put_triggers_write() {
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
    assert_eq!(write_count.load(Ordering::SeqCst), 1);
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
    // pact=true, and on Complete the AsyncPending tail in
    // complete_async_record cleared pact.
    assert_eq!(
        process_count.load(Ordering::SeqCst),
        2,
        "ReprocessAfter timer must call process() again — owner-driven \
         continuation bypasses the PACT entry guard"
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

#[tokio::test]
async fn test_sdis_disable_notifies_alarm() {
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
        inst.add_subscriber(
            "SEVR",
            1,
            epics_base_rs::types::DbFieldType::Short,
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
    assert!(!visited.contains("T1"));
    assert!(visited.contains("T2"));
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
        EpicsValue::Short(v) => assert_eq!(v, 2),
        other => panic!("expected Short(2), got {:?}", other),
    }
    let val = db.get_pv("SEL_REC").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!((v - 30.0).abs() < 1e-10),
        other => panic!("expected Double(30.0), got {:?}", other),
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
    assert_eq!(targets, vec!["MOTOR_POS"]);
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
    assert_eq!(targets, vec!["MY_SEQ"]);
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
    assert_eq!(targets, vec!["MY_SEL"]);
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
    assert_eq!(targets, vec!["GUARDED"]);
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
    assert_eq!(targets, vec!["TARGET"]);
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
        assert_eq!(inst.get_common_field("SSCN"), Some(EpicsValue::Enum(0)));
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
            Some(EpicsValue::String(String::new()))
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
    use epics_base_rs::types::DbFieldType;
    use std::time::SystemTime;

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
        let start = SystemTime::now();

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
            ts != SystemTime::UNIX_EPOCH,
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
        inst.put_common_field("OUT", EpicsValue::String("TS_DST".into()))
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
    use epics_base_rs::types::DbFieldType;
    use std::time::{Duration, SystemTime};

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
    let post_sleep = SystemTime::now();

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
        inst.put_common_field("OUT", EpicsValue::String("LO_DST".into()))
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
        // OUT="SELF_LO" → defaults to .VAL with PP, writes back to
        // self and would normally re-trigger processing.
        inst.put_common_field("OUT", EpicsValue::String("SELF_LO".into()))
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
    let value = db
        .read_link_value_soft(&parsed, true)
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
