//! A link put through the field's `$` name is `dbPutFieldLink` on the same
//! field (`dbNameToAddr` keeps the link's dbfType, `dbAccess.c:692-697`), so
//! a CP link it writes is opened and processes its holder as one put by the
//! plain name does. `softIoc` R7.0.10-146 (8f5015b66), `caput -S AI.INP$ "AO
//! CP"` then `caput AO 3`: AI reads 2 at the connect, then 3; `AI2.SDIS$` the
//! same, AI2 processed both times.

mod calink_ioc;

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "AO") { field(VAL, "2") }
record(ai, "AI") { }
record(calc, "CNT") { field(CALC, "VAL+1") }
record(ai, "AI2") { field(FLNK, "CNT") }
"#;

fn chars(text: &str) -> EpicsValue {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    EpicsValue::CharArray(bytes.into())
}

#[epics_macros_rs::epics_test]
async fn a_cp_link_put_by_its_dollar_name_processes_its_holder() {
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
    let get = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();

    db.put_record_field_from_ca_no_notify("AI", "INP$", chars("AO CP"))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("AI2", "SDIS$", chars("AO CP"))
        .await
        .unwrap();
    calink_ioc::until("the INP connect processes AI", || get("AI") == 2.0).await;
    calink_ioc::until("the SDIS connect processes AI2", || get("CNT") == 1.0).await;

    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    calink_ioc::until("AO's post processes AI", || get("AI") == 3.0).await;
    calink_ioc::until("AO's post processes AI2", || get("CNT") == 2.0).await;
}
