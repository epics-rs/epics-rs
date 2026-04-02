use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use std::collections::HashMap;

// ============================================================
// Throttle: ReprocessAfter integration test
// ============================================================

#[tokio::test]
async fn test_throttle_delayed_reprocess() {
    let db_str = r#"
record(throttle, "TEST:THR") {
    field(DLY, "0.2")
    field(PREC, "2")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // First put + process: should send immediately
    server
        .put("TEST:THR", EpicsValue::Double(10.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(10.0),
        "First value should be sent immediately"
    );

    let wait = server.get("TEST:THR.WAIT").await.unwrap();
    assert_eq!(
        wait,
        EpicsValue::Short(1),
        "WAIT should be 1 during delay period"
    );

    // Second put during delay period — must process to queue the value
    server
        .put("TEST:THR", EpicsValue::Double(20.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(10.0),
        "Second value should NOT be sent yet"
    );

    // Wait for DLY to expire — framework's ReprocessAfter will drain the pending value
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let sent = server.get("TEST:THR.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(20.0),
        "After delay, pending value should be sent"
    );
}

#[tokio::test]
async fn test_throttle_no_delay_immediate() {
    let db_str = r#"
record(throttle, "TEST:THR2") {
    field(DLY, "0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    server
        .put("TEST:THR2", EpicsValue::Double(42.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR2", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR2.SENT").await.unwrap();
    assert_eq!(sent, EpicsValue::Double(42.0));

    let wait = server.get("TEST:THR2.WAIT").await.unwrap();
    assert_eq!(
        wait,
        EpicsValue::Short(0),
        "No delay means WAIT should be 0"
    );
}

#[tokio::test]
async fn test_throttle_limit_clipping_via_framework() {
    let db_str = r#"
record(throttle, "TEST:THR3") {
    field(DLY, "0")
    field(DRVLH, "100")
    field(DRVLL, "0")
    field(DRVLC, "1")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    server
        .put("TEST:THR3", EpicsValue::Double(150.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR3", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR3.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(100.0),
        "Should be clipped to DRVLH"
    );

    let drvls = server.get("TEST:THR3.DRVLS").await.unwrap();
    assert_eq!(
        drvls,
        EpicsValue::Short(2),
        "DRVLS should indicate high limit"
    );
}

// ============================================================
// Epid: PID runs in process via framework
// ============================================================

#[tokio::test]
async fn test_epid_pid_via_framework() {
    let db_str = r#"
record(epid, "TEST:PID") {
    field(KP, "2.0")
    field(KI, "0")
    field(KD, "0")
    field(FBON, "1")
    field(DRVH, "1000")
    field(DRVL, "-1000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Set setpoint
    server
        .put("TEST:PID.VAL", EpicsValue::Double(100.0))
        .await
        .unwrap();

    // Process twice with a small gap so dt > 0
    db.put_record_field_from_ca("TEST:PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    db.put_record_field_from_ca("TEST:PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // P = KP * (VAL - CVAL) = 2.0 * (100 - 0) = 200.0
    let p = server.get("TEST:PID.P").await.unwrap();
    match p {
        EpicsValue::Double(v) => {
            assert!((v - 200.0).abs() < 1.0, "P should be ~200.0, got {}", v);
        }
        other => panic!("expected Double, got {:?}", other),
    }

    // OVAL should be clamped but non-zero
    let oval = server.get("TEST:PID.OVAL").await.unwrap();
    match oval {
        EpicsValue::Double(v) => {
            assert!(v.abs() > 1.0, "OVAL should be non-zero, got {}", v);
        }
        other => panic!("expected Double, got {:?}", other),
    }
}

// ============================================================
// Timestamp: process produces output
// ============================================================

#[tokio::test]
async fn test_timestamp_via_framework() {
    let db_str = r#"
record(timestamp, "TEST:TS") {
    field(TST, "4")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("timestamp", || Box::new(std_rs::TimestampRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Trigger process
    db.put_record_field_from_ca("TEST:TS", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let val = server.get("TEST:TS").await.unwrap();
    match val {
        EpicsValue::String(s) => {
            assert!(!s.is_empty(), "Timestamp should be non-empty");
            assert!(s.contains(':'), "Format 4 (HH:MM:SS) should contain ':'");
        }
        other => panic!("expected String, got {:?}", other),
    }

    let rval = server.get("TEST:TS.RVAL").await.unwrap();
    match rval {
        EpicsValue::Long(v) => assert!(v > 0, "RVAL should be positive"),
        other => panic!("expected Long, got {:?}", other),
    }
}
