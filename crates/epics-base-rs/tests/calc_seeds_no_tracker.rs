//! `calcRecord.c::init_record` never writes MLST, ALST or LALM, so they stay
//! 0 through iocInit and the first `monitor()` posts a VAL loaded as 5.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn the_first_monitor_posts_the_loaded_val() {
    let (db, _) = IocBuilder::new()
        .db_string(
            "record(calc, \"C\") {\n  field(CALC, \"5\")\n  field(VAL, \"5\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let rec = db.get_record("C").unwrap();
    let mut inst = rec.write();
    let trackers = ["MLST", "ALST", "LALM"].map(|f| inst.record.get_field(f));
    assert_eq!(
        inst.check_deadband_ext(),
        (true, true),
        "value and archive post"
    );
    assert_eq!(trackers, [const { Some(EpicsValue::Double(0.0)) }; 3]);
}
