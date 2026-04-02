use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use scaler_rs::ScalerRecord;
use std::collections::HashMap;

// ============================================================
// Scaler: CNT start/stop via framework
// ============================================================

#[tokio::test]
async fn test_scaler_count_start_stop() {
    let db_str = r#"
record(scaler, "TEST:SC") {
    field(FREQ, "1000000")
    field(TP, "1.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Initial state
    let ss = server.get("TEST:SC.SS").await.unwrap();
    assert_eq!(ss, EpicsValue::Short(0), "SS should be IDLE initially");

    // Start counting: put CNT=1 then process
    server
        .put("TEST:SC.CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:SC", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let ss = server.get("TEST:SC.SS").await.unwrap();
    assert_eq!(
        ss,
        EpicsValue::Short(2),
        "SS should be COUNTING after CNT=1 + process"
    );

    let us = server.get("TEST:SC.US").await.unwrap();
    assert_eq!(us, EpicsValue::Short(3), "US should be USER_COUNTING");

    // Stop: put CNT=0 then process
    server
        .put("TEST:SC.CNT", EpicsValue::Short(0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:SC", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let ss = server.get("TEST:SC.SS").await.unwrap();
    assert_eq!(ss, EpicsValue::Short(0), "SS should be IDLE after stop");
}

// ============================================================
// Scaler: TP <-> PR1 conversion
// ============================================================

#[tokio::test]
async fn test_scaler_tp_pr1_conversion() {
    let db_str = r#"
record(scaler, "TEST:SC2") {
    field(FREQ, "1000000")
    field(TP, "2.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    let pr1 = server.get("TEST:SC2.PR1").await.unwrap();
    assert_eq!(pr1, EpicsValue::Long(2_000_000), "PR1 = TP * FREQ");
}

// ============================================================
// Scaler: DLY delayed start via AsyncPendingReprocess
// ============================================================

#[tokio::test]
async fn test_scaler_dly_delayed_start() {
    let db_str = r#"
record(scaler, "TEST:SC3") {
    field(FREQ, "1000000")
    field(TP, "1.0")
    field(DLY, "0.2")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Start counting with DLY=0.2s
    // special("CNT") sets US=WAITING, then process returns AsyncPendingReprocess
    server
        .put("TEST:SC3.CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:SC3", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // During DLY wait
    let us = server.get("TEST:SC3.US").await.unwrap();
    assert_eq!(us, EpicsValue::Short(1), "US should be WAITING during DLY");

    // Wait for DLY to expire + framework re-process
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let ss = server.get("TEST:SC3.SS").await.unwrap();
    assert_eq!(
        ss,
        EpicsValue::Short(2),
        "SS should be COUNTING after DLY expires"
    );

    let us = server.get("TEST:SC3.US").await.unwrap();
    assert_eq!(
        us,
        EpicsValue::Short(3),
        "US should be USER_COUNTING after DLY"
    );
}

// ============================================================
// Scaler: preset auto-enables gate
// ============================================================

#[tokio::test]
async fn test_scaler_preset_auto_gate() {
    let db_str = r#"
record(scaler, "TEST:SC4") {
    field(FREQ, "1000000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    // Set PR5 via framework — triggers special
    server
        .put("TEST:SC4.PR5", EpicsValue::Long(5000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let g5 = server.get("TEST:SC4.G5").await.unwrap();
    assert_eq!(g5, EpicsValue::Short(1), "G5 should auto-enable");

    let d5 = server.get("TEST:SC4.D5").await.unwrap();
    assert_eq!(d5, EpicsValue::Short(1), "D5 should auto-enable");
}

// ============================================================
// Scaler: indexed field access via framework
// ============================================================

#[tokio::test]
async fn test_scaler_indexed_fields() {
    let db_str = r#"
record(scaler, "TEST:SC5") {
    field(FREQ, "1000000")
    field(NM1, "clock")
    field(NM2, "detector")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    let nm1 = server.get("TEST:SC5.NM1").await.unwrap();
    assert_eq!(nm1, EpicsValue::String("clock".to_string()));

    let nm2 = server.get("TEST:SC5.NM2").await.unwrap();
    assert_eq!(nm2, EpicsValue::String("detector".to_string()));

    let s1 = server.get("TEST:SC5.S1").await.unwrap();
    assert_eq!(s1, EpicsValue::Long(0));
}
