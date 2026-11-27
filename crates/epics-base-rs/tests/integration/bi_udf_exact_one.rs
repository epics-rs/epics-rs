//! bi's `checkAlarms` returns early only on `udf == TRUE` (`biRecord.c:237`):
//! a UDF byte of 2 that a failed read leaves in place runs the state alarm
//! and the AFTC filter. oracleStdIoc: `2/LINK/INVALID`, AFVL 2.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_base_rs::epics_test]
async fn a_udf_byte_of_two_runs_the_state_alarm_filter() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(bi, "X") {
  field(UDF, "2")
  field(INP, "NOPE")
  field(AFTC, "1")
  field(ZSV, "MAJOR")
}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // The failed INP read is the cycle's status, as C's `process` returns it.
    let _ = db.process_record("X").await;
    let afvl = db.get_pv("X.AFVL").unwrap().to_f64().unwrap();
    let rec = db.get_record("X").unwrap();
    let g = rec.read();
    assert_eq!(
        (g.common.udf, g.common.stat, g.common.sevr as u16, afvl),
        (2, 14, 3, 2.0)
    );
}
