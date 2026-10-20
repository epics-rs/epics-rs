//! sCalcoutRecord.c includes `epicsStdio.h`, so the `sCalcPostfix returns:`
//! line its `special()` prints for a bad CALC goes to the thread's stdout:
//! `dbpf S.CALC "1+" > f` puts it in `f`. The expected bytes are the C
//! oracle IOC's (`oracleStdIoc`) for the same line.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;

#[epics_macros_rs::epics_test]
async fn a_bad_calc_s_postfix_number_goes_to_the_dbpf_s_file() {
    let db = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(
            r#"record(scalcout, "S") { field(CALC, "1") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("c.txt").display().to_string();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let line = format!("dbpf S.CALC \"1+\" > {file}");
    std::thread::spawn(move || IocShell::new(db, bridge).execute_line(line))
        .join()
        .unwrap()
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "sCalcPostfix returns: 8\nDBF_STRING:         \"1+\"      \n"
    );
}
