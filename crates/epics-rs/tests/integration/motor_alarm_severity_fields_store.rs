//! `motorRecord.dbd` declares HSV and LSV, but motor runs its own alarm
//! check, so its instance has no `analog_alarm`. That slot means the
//! framework runs the ladder, not that it owns the storage: without it the
//! HIHI..LLSV arm used to drop the value with `Ok`.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[tokio::test(flavor = "multi_thread")]
async fn a_loaded_hsv_and_lsv_read_back_on_a_motor() {
    let (name, factory) = motor_rs::motor_record_factory();
    let db = IocBuilder::new()
        .register_record_type(name, factory)
        .db_string(
            "record(motor, \"M\") {\n  field(HSV, \"MAJOR\")\n  field(LSV, \"MINOR\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .load()
        .await
        .unwrap();
    for (field, index) in [("HSV", 2.0), ("LSV", 1.0)] {
        let got = db.get_pv(format!("M.{field}")).unwrap();
        assert_eq!(
            EpicsValue::to_f64(&got),
            Some(index),
            "M.{field} read back {got:?}"
        );
    }
}
