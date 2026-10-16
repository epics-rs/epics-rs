//! A refused SUBL put removed the link, CP subscription included: `NM`
//! processes `AS` (and its FLNK counter) once at connect and once per
//! `NM.PROC` before the refusal, never after. `softIoc` R7.0.10-146
//! (8f5015b66) reads CNT 1, 2, then 2.

mod calink_ioc;

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(ai, "NM") { field(VAL, "1") }
record(calc, "CNT") { field(CALC, "VAL+1") }
record(aSub, "AS") { field(SNAM, "") field(SUBL, "NM CP") field(FLNK, "CNT") }
"#;

#[epics_macros_rs::epics_test]
async fn a_refused_subl_put_drops_its_cp_subscription() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.ioc_init().await;
    db.initialize_link_locality().await;
    calink_ioc::connect(&db).await;
    let cnt = || db.get_pv("CNT").unwrap().to_f64().unwrap();
    let proc_nm = async || {
        db.put_record_field_from_ca("NM", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
    };
    calink_ioc::until("the connect processes AS", || cnt() == 1.0).await;
    proc_nm().await;
    calink_ioc::until("NM's post processes AS", || cnt() == 2.0).await;

    let _ = db
        .put_record_field_from_ca("AS", "SUBL", EpicsValue::String(PvString::from("NM2")))
        .await;
    // The refusal removed the link, so AS holds no edge NM's posts could take.
    assert!(db.get_external_cp_targets(b"NM").is_empty());
    proc_nm().await;
    proc_nm().await;
    assert_eq!(cnt(), 2.0);
}
