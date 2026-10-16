//! C `dbNameToAddr`'s `$` arm makes a `DBR_CHAR` address of a `DBF_STRING`
//! or link field and refuses every other type with `S_dbLib_fieldNotFound`
//! (`dbAccess.c:681-701`); a put by a `$` name stores through that address.

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SRC") { field(OUT, "DST.VAL$ NPP") field(VAL, "2.5") }
record(ao, "DST") { field(VAL, "1") }
record(stringout, "SO") { field(VAL, "x") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

#[epics_macros_rs::epics_test]
async fn a_dollar_put_to_a_double_is_field_not_found() {
    let db = ioc().await;
    let put = db.put_pv("DST.VAL$", EpicsValue::Double(2.5)).await;
    assert!(matches!(put, Err(CaError::FieldNotFound(_))), "{put:?}");
    assert_eq!(db.get_pv("DST").unwrap().to_f64(), Some(1.0));
}

/// The link resolves to no local address, so the double is never written. C 8f5015b66 cannot run this db: `dbChannelTest`
/// accepts the name (`dbChannel.c:331-343`), so `dbInitLink` makes a CA link
/// that `iocInit` waits on (`dbLink.c:128-130`, `dbCa.c:376`), and the CA
/// server never connects it.
#[epics_macros_rs::epics_test]
async fn a_dollar_link_to_a_double_writes_nothing() {
    let db = ioc().await;
    let _ = db
        .process_record_with_links("SRC", &mut ProcStack::new())
        .await;
    assert_eq!(db.get_pv("DST").unwrap().to_f64(), Some(1.0));
    // `dbCaPutLinkCallback` refuses an unconnected link and `dbPutLink`
    // raises LINK/INVALID on the source (`dbCa.c:558-560`, `dbLink.c:443-446`).
    assert_eq!(db.get_pv("SRC.SEVR").unwrap().to_f64(), Some(3.0));
    assert_eq!(db.get_pv("SRC.STAT").unwrap().to_f64(), Some(14.0));
}

#[epics_macros_rs::epics_test]
async fn a_dollar_put_to_a_string_stores_the_chars() {
    let db = ioc().await;
    db.put_pv("SO.VAL$", EpicsValue::CharArray(b"abc\0".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(db.get_pv("SO").unwrap(), EpicsValue::String("abc".into()));
    db.put_pv("SO.val$", EpicsValue::CharArray(b"de\0".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(db.get_pv("SO").unwrap(), EpicsValue::String("de".into()));
}
