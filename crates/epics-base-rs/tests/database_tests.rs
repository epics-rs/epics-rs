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
/// MS-class alarms; ai/longin/bi/mbbi/stringin INP=`SRC MSS` silently
/// dropped them even though the link parser recorded the MS modifier.
#[tokio::test]
async fn test_single_inp_ms_class_propagates_source_alarm() {
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;

    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    db.add_record("DST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Force the source into MAJOR severity with a specific STAT and
    // alarm message so we can verify both numeric fields and the
    // amsg string propagate through the MS modifier.
    if let Some(rec) = db.get_record("SRC").await {
        let mut inst = rec.write().await;
        inst.common.stat = alarm_status::HIHI_ALARM;
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.amsg = "src-major".to_string();
    }

    // INP="SRC NPP MS" — NPP prevents PP=ProcessPassive from
    // re-processing SRC and clearing the alarm state the test just
    // installed; MS (Maximize) makes the link propagate STAT/SEVR/AMSG.
    // Also clear DST's default UDF so its own UDF_ALARM/Invalid
    // doesn't mask the lower-severity Major from SRC under maximize.
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
        alarm_status::HIHI_ALARM,
        "MS link must carry source's STAT"
    );
    assert_eq!(
        inst.common.amsg, "src-major",
        "MS link must carry source's AMSG"
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
    assert_eq!(inst.common.stat, 14);
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
    assert_ne!(inst.common.stat, 14);
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

#[tokio::test]
async fn test_tse_minus1_preserves_device_timestamp() {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let device_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234567);
    if let Some(rec) = db.get_record("REC").await {
        let mut inst = rec.write().await;
        inst.common.tse = -1;
        inst.common.time = device_time;
    }
    let mut visited = HashSet::new();
    db.process_record_with_links("REC", &mut visited, 0)
        .await
        .unwrap();
    let rec = db.get_record("REC").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(inst.common.time, device_time);
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
