//! End-to-end integration tests for std-rs features that require
//! actual framework link resolution, PV connections, and async PID.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;

// ============================================================
// 1. Throttle OUT link actually writes to target PV
// ============================================================

#[tokio::test]
async fn test_throttle_out_link_writes_to_target() {
    let db_str = r#"
record(ao, "TARGET") {
    field(VAL, "0")
}
record(throttle, "THR") {
    field(DLY, "0")
    field(OUT, "TARGET PP")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Write to throttle and process
    server.put("THR", EpicsValue::Double(42.0)).await.unwrap();
    db.put_record_field_from_ca("THR", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // SENT should be 42.0
    let sent = server.get("THR.SENT").await.unwrap();
    assert_eq!(sent, EpicsValue::Double(42.0));

    // TARGET should have received the value via OUT link WriteDbLink
    let target_val = server.get("TARGET").await.unwrap();
    assert_eq!(
        target_val,
        EpicsValue::Double(42.0),
        "OUT link should write SENT to TARGET PV"
    );
}

#[tokio::test]
async fn test_throttle_out_link_with_delay() {
    let db_str = r#"
record(ao, "TARGET2") {
    field(VAL, "0")
}
record(throttle, "THR2") {
    field(DLY, "0.15")
    field(OUT, "TARGET2 PP")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // First value — sent immediately
    server.put("THR2", EpicsValue::Double(10.0)).await.unwrap();
    db.put_record_field_from_ca("THR2", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    let target = server.get("TARGET2").await.unwrap();
    assert_eq!(
        target,
        EpicsValue::Double(10.0),
        "First value sent immediately"
    );

    // Second value during delay — queued
    server.put("THR2", EpicsValue::Double(20.0)).await.unwrap();
    db.put_record_field_from_ca("THR2", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    let target = server.get("TARGET2").await.unwrap();
    assert_eq!(
        target,
        EpicsValue::Double(10.0),
        "Second value should NOT arrive during delay"
    );

    // Wait for delay + reprocess
    tokio::time::sleep(Duration::from_millis(250)).await;

    let target = server.get("TARGET2").await.unwrap();
    assert_eq!(
        target,
        EpicsValue::Double(20.0),
        "After delay, pending value should arrive at TARGET via OUT link"
    );
}

// ============================================================
// 2. Scaler COUT/COUTP links fire to target PVs
// ============================================================

#[tokio::test]
async fn test_scaler_cout_fires_on_count_start() {
    let db_str = r#"
record(ao, "COUT_TARGET") {
    field(VAL, "-1")
}
record(scaler, "SC") {
    field(FREQ, "1000000")
    field(TP, "1.0")
    field(COUT, "COUT_TARGET PP")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(scaler_rs::ScalerRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Start counting
    server.put("SC.CNT", EpicsValue::Short(1)).await.unwrap();
    db.put_record_field_from_ca("SC", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // COUT_TARGET should have received CNT value (1 = counting)
    let cout_val = server.get("COUT_TARGET").await.unwrap();
    assert_eq!(
        cout_val,
        EpicsValue::Double(1.0),
        "COUT should fire CNT=1 to target on count start"
    );
}

// ============================================================
// 3. EpidFast: tokio channel PID loop
// ============================================================

#[tokio::test]
async fn test_epid_fast_callback_loop() {
    let dev = std_rs::device_support::epid_fast::EpidFastDeviceSupport::new();

    // Configure PID parameters
    {
        let pvt_arc = dev.pvt();
        let mut pvt = pvt_arc.lock().unwrap();
        pvt.kp = 1.0;
        pvt.ki = 0.0;
        pvt.kd = 0.0;
        pvt.val = 100.0; // setpoint
        pvt.fbon = true;
        pvt.fbop = true;
        pvt.drvh = 200.0;
        pvt.drvl = -200.0;
    }

    // Create input channel and output collector
    let (tx, rx) = tokio::sync::mpsc::channel::<f64>(100);
    let output_values: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let output_clone = output_values.clone();
    let output_fn: Arc<Mutex<dyn FnMut(f64) + Send>> = Arc::new(Mutex::new(move |v: f64| {
        output_clone.lock().unwrap().push(v);
    }));

    // Start the PID callback loop
    dev.start_callback_loop(rx, output_fn);

    // Feed controlled values (simulating 1kHz ADC readings)
    for i in 0..10 {
        let cval = 90.0 + i as f64; // approaching setpoint
        tx.send(cval).await.unwrap();
    }

    // Small delay for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Check output values were produced
    let outputs = output_values.lock().unwrap();
    assert!(!outputs.is_empty(), "PID loop should produce output values");

    // Verify PID state
    let pvt_arc = dev.pvt();
    let pvt = pvt_arc.lock().unwrap();
    assert!(pvt.cval > 0.0, "CVAL should be updated from input");
    assert!(pvt.oval != 0.0, "OVAL should be computed");
    // P = KP * (setpoint - cval) = 1.0 * (100 - 99) = 1.0 (last input)
    assert!(pvt.p.abs() > 0.0, "P component should be non-zero");
}

#[tokio::test]
async fn test_epid_fast_output_clamping() {
    let dev = std_rs::device_support::epid_fast::EpidFastDeviceSupport::new();

    {
        let pvt_arc = dev.pvt();
        let mut pvt = pvt_arc.lock().unwrap();
        pvt.kp = 100.0; // Very high gain → output will saturate
        pvt.val = 100.0;
        pvt.fbon = true;
        pvt.fbop = true;
        pvt.drvh = 50.0;
        pvt.drvl = -50.0;
    }

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let outputs: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let out_clone = outputs.clone();
    dev.start_callback_loop(
        rx,
        Arc::new(Mutex::new(move |v| {
            out_clone.lock().unwrap().push(v);
        })),
    );

    tx.send(0.0).await.unwrap(); // Error = 100, P = 10000 → clamped to 50
    tokio::time::sleep(Duration::from_millis(20)).await;

    let outs = outputs.lock().unwrap();
    assert!(!outs.is_empty());
    assert!(
        outs[0] <= 50.0,
        "Output should be clamped to DRVH=50, got {}",
        outs[0]
    );
}

// ============================================================
// 4. Scaler soft driver counting simulation
// ============================================================

#[tokio::test]
async fn test_scaler_soft_counting_simulation() {
    use scaler_rs::device_support::scaler_asyn::ScalerDriver;
    use scaler_rs::device_support::scaler_soft::SoftScalerDriver;
    use scaler_rs::records::scaler::MAX_SCALER_CHANNELS;

    let mut driver = SoftScalerDriver::new(8);
    let shared = driver.shared_counts();

    // Configure preset on channel 0
    driver.write_preset(0, 1000).unwrap();
    driver.arm(true).unwrap();

    // Simulate counting: background task increments counters
    let shared_clone = shared.clone();
    let counter_task = tokio::spawn(async move {
        for tick in 0..100 {
            {
                let mut counts = shared_clone.lock().unwrap();
                counts[0] = (tick + 1) * 10; // 10 counts per tick
                counts[1] = (tick + 1) * 5;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    // Wait for counting to finish
    counter_task.await.unwrap();

    // Read counts — should detect done
    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();

    assert_eq!(counts[0], 1000, "Channel 0 should reach 1000");
    assert_eq!(counts[1], 500, "Channel 1 should be 500");
    assert!(driver.done(), "Should be done — channel 0 reached preset");
}

// ============================================================
// 5. Autosave with std .req files
// ============================================================

#[tokio::test]
async fn test_autosave_req_file_loading() {
    // Verify that .req files bundled with std-rs can be parsed
    let req_dir = std::path::Path::new(std_rs::STD_DB_DIR);

    // Check that at least one .req file exists
    let has_req = std::fs::read_dir(req_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|ext| ext == "req"));

    assert!(has_req, "std-rs/db/ should contain .req autosave files");
}

#[tokio::test]
async fn test_autosave_save_and_restore_epid() {
    let dir = tempfile::tempdir().unwrap();
    let req_path = dir.path().join("epid_test.req");

    // Write a minimal .req file
    tokio::fs::write(&req_path, "TEST:PID.VAL\nTEST:PID.KP\nTEST:PID.KI\n")
        .await
        .unwrap();

    let db_str = r#"
record(epid, "TEST:PID") {
    field(KP, "2.5")
    field(KI, "0.1")
    field(DRVH, "100")
    field(DRVL, "-100")
}
"#;
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(db_str, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Set a value
    server
        .put("TEST:PID.VAL", EpicsValue::Double(50.0))
        .await
        .unwrap();

    // Save using AutosaveBuilder
    use epics_base_rs::server::autosave::{
        AutosaveBuilder, BackupConfig, SaveSetConfig, SaveStrategy,
    };

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "test".into(),
            save_path: dir.path().join("epid.sav"),
            strategy: SaveStrategy::Manual,
            request_file: Some(req_path),
            request_pvs: vec![],
            backup: BackupConfig {
                enable_savb: false,
                num_seq_files: 0,
                seq_period: Duration::from_secs(60),
                enable_dated: false,
                dated_interval: Duration::from_secs(3600),
            },
            macros: HashMap::new(),
            search_paths: vec![],
        })
        .build()
        .await;

    // Save
    let saved = mgr.manual_save("test", &db).await.unwrap();
    assert!(saved > 0, "Should save at least one PV");

    // Verify save file exists
    assert!(
        dir.path().join("epid.sav").exists(),
        "Save file should exist"
    );

    // Change the value
    server
        .put("TEST:PID.VAL", EpicsValue::Double(0.0))
        .await
        .unwrap();

    // Restore
    let results = mgr.restore_all(&db).await;
    assert!(!results.is_empty());

    tokio::time::sleep(Duration::from_millis(20)).await;

    // Value should be restored to 50.0
    let val = server.get("TEST:PID.VAL").await.unwrap();
    match val {
        EpicsValue::Double(v) => assert!(
            (v - 50.0).abs() < 1e-6,
            "VAL should be restored to 50.0, got {v}"
        ),
        other => panic!("expected Double, got {:?}", other),
    }
}

// ============================================================
// epid bumpless transfer — OUTL output-link readback (TASK 2).
//
// C `devEpidSoft.c:153-158`: on the feedback OFF->ON edge the
// integral term is seeded from the OUTL output link's *actual
// current value* — but only on a cycle that actually reaches
// `do_pid`'s PID block: past the record's UDF gate
// (`epidRecord.c:195`), past the CONSTANT-INP abort
// (`devEpidSoft.c:110-112`) and past the MDT gate
// (`devEpidSoft.c:125`).
// ============================================================

/// A db that reaches C's OUTL seed: a real INP (else `do_pid` aborts on the
/// CONSTANT-INP check), a CONSTANT STPL (which clears UDF at init —
/// `epidRecord.c:160-164` — else `process` returns before `do_pid`), and a
/// non-zero KI (else `if (ki == 0) i = 0.` wipes the seed).
fn epid_bumpless_db(mdt: &str) -> String {
    format!(
        r#"
record(ai, "SENSOR") {{
    field(VAL, "2.0")
}}
record(ao, "DAC") {{
    field(VAL, "7.0")
}}
record(epid, "PID") {{
    field(KP, "1.0")
    field(KI, "0.5")
    field(DRVH, "100")
    field(DRVL, "-100")
    field(INP, "SENSOR")
    field(STPL, "1.0")
    field(OUTL, "DAC")
    field(FMOD, "0")
    field(MDT, "{mdt}")
}}
"#
    )
}

#[tokio::test]
async fn test_epid_outl_readback_seeds_integral_term() {
    // DAC holds a known current output value of 7.0; the epid's OUTL
    // link points at it. On the feedback OFF->ON edge the epid must
    // seed its integral term I from DAC's current value, NOT from its
    // own last-commanded OVAL (which is 0.0).
    let db_str = epid_bumpless_db("0");
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("ai", || Box::new(AiRecord::default()))
        .db_string(&db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Confirm DAC holds 7.0.
    assert_eq!(server.get("DAC").await.unwrap(), EpicsValue::Double(7.0));

    // Turn feedback ON — this is the OFF->ON edge (FBOP starts 0).
    db.put_record_field_from_ca("PID", "FBON", EpicsValue::Short(1))
        .await
        .unwrap();
    // Process the epid: the framework stages DAC's value (7.0) from OUTL, and
    // `do_pid` — past the MDT gate (MDT=0 here) — moves it into I.
    db.put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let i_term = server.get("PID.I").await.unwrap();
    match i_term {
        EpicsValue::Double(v) => assert!(
            (v - 7.0).abs() < 1e-9,
            "bumpless edge must seed I from OUTL's actual value 7.0, got {v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// R7-64: C gates on `if (dt < pepid->mdt) return(1);` (`devEpidSoft.c:125`)
/// BEFORE the OUTL seed (`:150-158`). A sub-MDT cycle therefore never reads
/// OUTL and never touches `.I` — and because `fbop` is only committed by a
/// cycle that computes, the OFF->ON edge survives to the next full cycle,
/// which seeds for real.
#[tokio::test]
async fn test_epid_sub_mdt_edge_cycle_does_not_seed_i() {
    let db_str = epid_bumpless_db("0.5");
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("ai", || Box::new(AiRecord::default()))
        .db_string(&db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // OFF->ON edge, then process immediately: dt since the record's CT was
    // stamped is a few ms — far below MDT=0.5 s.
    db.put_record_field_from_ca("PID", "FBON", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    match server.get("PID.I").await.unwrap() {
        EpicsValue::Double(v) => assert_eq!(
            v, 0.0,
            "a sub-MDT cycle must not read OUTL nor post .I (C returns at \
             devEpidSoft.c:125, before the seed); got {v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }

    // Once MDT has elapsed the very same edge (FBOP is still 0 — a gated cycle
    // commits nothing) seeds the integral term for real.
    tokio::time::sleep(Duration::from_millis(600)).await;
    db.put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    match server.get("PID.I").await.unwrap() {
        EpicsValue::Double(v) => assert!(
            (v - 7.0).abs() < 1e-9,
            "the post-MDT cycle must seed I from OUTL (7.0), got {v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
}

// ============================================================
// epid devEpidSoftCallback — DB-type TRIG link write-then-read
// ordering, within ONE process pass.
//
// C `devEpidSoftCallback.c::do_pid`, for `ptriglink->type != CA_LINK`:
//   1. `dbPutLink(ptriglink, DBR_DOUBLE, &pepid->tval, 1)`
//      (`devEpidSoftCallback.c:121-127`) — synchronous trigger write;
//   2. `dbGetLink(&pepid->inp, DBR_DOUBLE, &pepid->cval, ...)`
//      (`devEpidSoftCallback.c:151`) — read CVAL from INP;
//   3. run the PID.
//
// The trigger write must land BEFORE the INP read so the freshly-
// triggered source value is what reaches CVAL. The Rust port emits
// the TRIG write from `EpidRecord::pre_input_link_actions`, which the
// framework runs strictly before the `INP -> CVAL` input-link fetch.
// ============================================================

#[tokio::test]
async fn test_epid_db_trig_write_then_read_in_one_cycle() {
    // SRC starts at 1.0. The epid's TRIG link points at SRC and its
    // TVAL is 42.0; its INP link ALSO reads SRC. In one process pass
    // the epid must: (1) write TVAL(42.0) into SRC via TRIG, then
    // (2) read SRC into CVAL. So CVAL must be 42.0 after a SINGLE
    // process cycle.
    //
    // If the trigger write were a POST-process action (the bug this
    // test guards), CVAL would still read SRC's stale 1.0 this cycle —
    // the freshly-triggered value would only reach CVAL one cycle late.
    let db_str = r#"
record(ao, "SRC") {
    field(VAL, "1.0")
}
record(epid, "PID") {
    field(DTYP, "Async Soft Channel")
    field(KP, "1.0")
    field(KI, "0.0")
    field(KD, "0.0")
    field(VAL, "100.0")
    field(DRVH, "1000")
    field(DRVL, "-1000")
    field(MDT, "0.0")
    field(INP, "SRC")
    field(TRIG, "SRC PP")
    field(TVAL, "42.0")
    field(FMOD, "0")
    field(FBON, "1")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_device_support("Async Soft Channel", || {
            Box::new(
                std_rs::device_support::epid_soft_callback::EpidSoftCallbackDeviceSupport::new(),
            )
        })
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // SRC starts at 1.0, epid CVAL starts at 0.0.
    assert_eq!(server.get("SRC").await.unwrap(), EpicsValue::Double(1.0));
    assert_eq!(
        server.get("PID.CVAL").await.unwrap(),
        EpicsValue::Double(0.0)
    );

    // Process the epid ONCE.
    db.put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // TRIG fired this cycle: SRC now holds TVAL.
    assert_eq!(
        server.get("SRC").await.unwrap(),
        EpicsValue::Double(42.0),
        "TRIG link must have written TVAL into SRC"
    );

    // The decisive assertion: CVAL read SRC *after* the TRIG write, in
    // the SAME process pass — so CVAL is the freshly-triggered 42.0,
    // not SRC's stale pre-trigger 1.0.
    let cval = server.get("PID.CVAL").await.unwrap();
    match cval {
        EpicsValue::Double(v) => assert!(
            (v - 42.0).abs() < 1e-9,
            "DB-TRIG: INP must read the freshly-triggered SRC value 42.0 \
             in the same process pass, got {v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }

    // PID ran with the fresh CVAL: ERR = VAL - CVAL = 100 - 42 = 58,
    // P = KP*ERR = 58. Confirms do_pid consumed the post-trigger CVAL.
    let p = server.get("PID.P").await.unwrap();
    match p {
        EpicsValue::Double(v) => assert!(
            (v - 58.0).abs() < 1e-9,
            "PID must have computed from the post-trigger CVAL (P=58.0), got {v}"
        ),
        other => panic!("expected Double, got {other:?}"),
    }
}
