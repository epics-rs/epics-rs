//! throttle has no device support, so `dbInitRecordLinks` gives its OUT no
//! `devSup` and `dbCanSetLink` expects CONSTANT (`dbStaticLib.c:2187-2193`,
//! `:2400-2419`): a hardware address is refused at `iocInit` and OUT is left
//! an empty constant. `valuePut` then takes its CONSTANT branch
//! (`throttleRecord.c:585-590`): STS Error, no alarm, no forward link.
//!
//! The std oracle IOC (oracleStdIoc) on this `.db`, `dbpf T:HW 1`:
//!
//! ```text
//! ERROR: T:HW.OUT: can't initialize link type CONSTANT with "#C0 S0" (type VME_IO)
//! T:HW.STS   "Error"
//! T:HW.STAT  "NO_ALARM"
//! T:HW.SEVR  "NO_ALARM"
//! T:F        0
//! T:HW.OUT   ""
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r##"
record(throttle, "T:HW") {
  field(OUT, "#C0 S0")
  field(DRVLH, "10")
  field(FLNK, "T:F")
}
record(calc, "T:F") { field(CALC, "VAL+1") }
"##;

#[tokio::test]
async fn a_hardware_out_link_is_refused_at_ioc_init() {
    std_rs::register_std_dbd();
    let (db, _) = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("calc", || Box::new(CalcRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.put_record_field_from_ca("T:HW", "VAL", EpicsValue::String("1".into()))
        .await
        .unwrap();
    let get = |pv: &str| db.get_pv(pv).unwrap();
    assert_eq!(get("T:HW.OUT"), EpicsValue::String("".into()));
    // throttleSTS: Unknown, Error, Success.
    assert_eq!(get("T:HW.STS").to_f64(), Some(1.0));
    assert_eq!(get("T:HW.STAT").to_f64(), Some(0.0));
    assert_eq!(get("T:HW.SEVR").to_f64(), Some(0.0));
    assert_eq!(get("T:F").to_f64(), Some(0.0), "no forward link");
}
