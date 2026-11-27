//! `field(DTYP, "<n>")` stores menu index `n` (`dbStaticRun.c:505-510`), so
//! the record reads back the choice, not the digits. softIoc R7.0.10 on the
//! DB below: `dbgf A.DTYP` is "Async Soft Channel", `dbgf C.DTYP` is "".

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn an_index_reads_back_as_its_choice() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(ai, "A") { field(DTYP, "2") }
record(calc, "C") { field(DTYP, "5") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let dtyp = |name: &str| {
        db.get_record(name)
            .unwrap()
            .read()
            .common
            .dtyp
            .as_str()
            .to_string()
    };
    assert_eq!(dtyp("A"), "Async Soft Channel");
    assert_eq!(db.get_pv("A.DTYP").unwrap(), EpicsValue::Enum(2));
    assert_eq!(dtyp("C"), "");
}
