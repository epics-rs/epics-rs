//! A record type with no `seed_deadband_tracking` of its own keeps the
//! MLST/ALST its `.db` load left, as a C `init_record` that never assigns
//! them does (`calcRecord.c:90-112`).

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_macros_rs::EpicsRecord;

#[derive(Default, EpicsRecord)]
#[record(type = "calc", crate_path = "epics_base_rs")]
struct Untracked {
    #[field(type = "Double")]
    pub val: f64,
    #[field(type = "Double")]
    pub mlst: f64,
    #[field(type = "Double")]
    pub alst: f64,
}

#[epics_macros_rs::epics_test]
async fn a_record_on_the_default_hook_keeps_its_trackers_at_zero() {
    let (db, ..) = IocBuilder::new()
        .register_record_type("calc", || Box::new(Untracked::default()))
        .db_string(r#"record(calc, "U") { field(VAL, "3") }"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let rec = db.get_record("U").unwrap();
    let rec = rec.read();
    let f = |name| rec.record.get_field(name).and_then(|v| v.to_f64()).unwrap();
    assert_eq!(f("VAL"), 3.0);
    assert_eq!((f("MLST"), f("ALST")), (0.0, 0.0));
}
