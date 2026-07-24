// RTEMS-EXEC-MODEL-ALLOW(1): CaServerBuilder::build binds a tokio::net listener (needs a reactor); runs and passes in the feature-ON suite.
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use std::collections::HashMap;

#[test]
fn test_register_record_type() {
    let builder = CaServerBuilder::new()
        .port(0)
        .register_record_type("busy", || Box::new(BusyRecord::default()));
    // Registration succeeds — builder is valid
    drop(builder);
}

// `#[tokio::test]`, not `#[epics_test]`: `CaServerBuilder::build` binds a
// `tokio::net` listener, which needs a real reactor on both backends. The
// census marker accounts for it.
#[tokio::test]
async fn test_db_file_load() {
    let db = r#"
record(busy, "TEST:BUSY") {
    field(ZNAM, "Idle")
    field(ONAM, "Running")
    field(HIGH, "1.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(db, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    // Verify record was loaded with correct field values
    let val: EpicsValue = server.get("TEST:BUSY").await.unwrap();
    assert_eq!(val, EpicsValue::Enum(0));
    let znam: EpicsValue = server.get("TEST:BUSY.ZNAM").await.unwrap();
    assert_eq!(znam, EpicsValue::String("Idle".into()));
}

#[test]
fn test_state_transition_full_cycle() {
    let mut rec = BusyRecord::default();

    // Initial state: idle
    assert_eq!(rec.val, 0);
    rec.process().unwrap();
    assert_eq!(rec.oval, 0);
    assert!(rec.should_fire_forward_link()); // val=0

    // External put: go busy
    rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    assert_eq!(rec.val, 1);
    rec.process().unwrap();
    assert_eq!(rec.oval, 1);
    // RVAL is DBF_ULONG (boRecord.dbd.pod:252) — served as the unsigned carrier.
    assert_eq!(rec.get_field("RVAL"), Some(EpicsValue::ULong(1)));

    // Stay busy — FLNK suppressed
    rec.process().unwrap();
    assert!(!rec.should_fire_forward_link());

    // External put: go done
    rec.put_field("VAL", EpicsValue::Enum(0)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.oval, 0);
    assert!(rec.should_fire_forward_link()); // val=0
    assert_eq!(rec.get_field("RVAL"), Some(EpicsValue::ULong(0)));
}
