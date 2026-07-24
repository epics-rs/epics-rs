//! End-to-end wiring for the std-module `devTimeOfDay.c` device support
//! ("Sec Past Epoch" ai, "Time of Day" stringin).
//!
//! These DTYPs were fully implemented in
//! `std_rs::device_support::time_of_day` but never registered, while
//! base-rs's `is_soft_dtyp` mis-classified "Sec Past Epoch" as a soft
//! channel — so the device never ran and VAL was silently wrong. With
//! "Sec Past Epoch" removed from `is_soft_dtyp` and `std_device_supports()`
//! registered, a processed record must have its VAL written by the device.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn read_val(db: &PvDatabase, name: &str) -> EpicsValue {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    inst.record.get_field("VAL").expect("VAL field present")
}

/// Build an IOC that registers the std device support via the
/// `std_device_supports()` entry point, then process the two `devTimeOfDay`
/// records and confirm the device wrote VAL.
#[tokio::test]
async fn std_time_of_day_devices_write_val_when_registered() {
    let db_content = r#"
record(ai, "TOD_SEC") {
    field(DTYP, "Sec Past Epoch")
    field(TSE, "0")
}
record(stringin, "TOD_STR") {
    field(DTYP, "Time of Day")
    field(TSE, "0")
}
"#;

    // Register the std-module device support through the new entry point —
    // the boxed factory satisfies `register_device_support`'s `Fn` bound.
    let mut builder = IocBuilder::new();
    for (dtyp, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(dtyp, factory);
    }
    let (db, _) = builder
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("TOD_SEC", &mut visited, 0)
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("TOD_STR", &mut visited, 0)
        .await
        .unwrap();

    // "Sec Past Epoch" (PHAS=0): VAL = current secPastEpoch, well over 1e9
    // in 2026. Pre-fix the soft-channel short-circuit left VAL at 0.0.
    match read_val(&db, "TOD_SEC").await {
        EpicsValue::Double(v) => assert!(
            v > 1.0e9,
            "Sec Past Epoch VAL must be the current secPastEpoch (>1e9), got {v}"
        ),
        other => panic!("expected Double VAL, got {other:?}"),
    }

    // "Time of Day" (PHAS=0): VAL = "%b %d, %Y %H:%M:%S", e.g.
    // "Jun 25, 2026 14:30:00" — must carry the current year.
    match read_val(&db, "TOD_STR").await {
        EpicsValue::String(s) => {
            let s = s.as_str_lossy();
            assert!(
                s.contains(", 20"),
                "Time of Day VAL must be a formatted date with a 20xx year, got {s:?}"
            );
        }
        other => panic!("expected String VAL, got {other:?}"),
    }
}
