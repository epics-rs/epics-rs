//! A put-notify to a DBF link field is written and answered at once, even
//! while another put-notify owns the record: C `dbProcessNotify` handles link
//! fields above the whole notify machinery (`dbNotify.c:331-345`) and never
//! queues them. softIoc (R7.0.10-146), with `caput -c CO.PROC 1` waiting out
//! CO's 3 s `ODLY`: `caput -c CO.INPA "Y CP"` returns in 0.04 s and
//! `caget CO.INPA` reads `Y CP NMS`.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(ao, "Y") { field(VAL, "0") }
record(calcout, "CO") { field(ODLY, "100") field(CALC, "VAL+1") field(OOPT, "Every Time") }
"#;

#[epics_macros_rs::epics_test]
async fn caput_c_co_inpa_y_cp_completes_while_co_proc_waits_out_odly() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let proc = db
        .put_record_field_from_ca("CO", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    assert!(!proc.is_sync(), "CO.PROC owns the notify slot across ODLY");

    let link = db
        .put_record_field_from_ca("CO", "INPA", EpicsValue::String(PvString::from("Y CP")))
        .await
        .unwrap();
    assert!(link.is_sync(), "a link put-notify is answered at once");
    assert_eq!(
        db.get_pv("CO.INPA").unwrap(),
        EpicsValue::String(PvString::from("Y CP NMS"))
    );
}
