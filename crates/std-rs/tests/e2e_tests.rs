//! End-to-end integration tests for std-rs features that require
//! actual framework link resolution, PV connections, and async PID.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
        .await
        .unwrap();

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
