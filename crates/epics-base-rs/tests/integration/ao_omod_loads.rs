//! ao OMOD is a DBF_UCHAR member in C: a `.db` value is stored, and only
//! `convert` and `monitor` write it after that (`aoRecord.c:482,537`), so
//! iocInit leaves it as loaded.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = "record(ao, \"R\") {\n  field(OMOD, \"1\")\n}\n";

#[epics_macros_rs::epics_test]
async fn a_loaded_omod_reads_back_through_iocinit() {
    let loaded = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .load()
        .await
        .unwrap();
    let got = loaded.get_pv("R.OMOD").unwrap();
    assert_eq!(got.to_f64(), Some(1.0), "after load read {got:?}");

    let (built, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let got = built.get_pv("R.OMOD").unwrap();
    assert_eq!(got.to_f64(), Some(1.0), "after iocInit read {got:?}");
}
