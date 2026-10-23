//! A `field(UDF, "2")` from a .db keeps its byte through iocInit, as C's
//! `epicsUInt8` does; bo and stringout test `udf == TRUE` (`boRecord.c:376`,
//! `stringoutRecord.c:146`), so the byte 2 processes to NO_ALARM, while ao's
//! process re-derives it (`aoRecord.c:189`). Values from oracleStdIoc.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

async fn ioc(rtyp: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(
            &format!("record({rtyp}, \"X\") {{\n  field(UDF, \"2\")\n}}\n"),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// UDF/STAT/SEVR of X.
fn snap(db: &PvDatabase) -> (u8, u16, u16) {
    let rec = db.get_record("X").unwrap();
    let g = rec.read();
    (g.common.udf, g.common.stat, g.common.sevr as u16)
}

/// oracle: born `2/UDF/INVALID`; bo and stringout process to `2/NO_ALARM`,
/// ao to `0/NO_ALARM`.
#[epics_base_rs::epics_test]
async fn a_loaded_udf_byte_of_two_survives_init() {
    for (rtyp, processed) in [
        ("bo", (2, 0, 0)),
        ("stringout", (2, 0, 0)),
        ("ao", (0, 0, 0)),
    ] {
        let db = ioc(rtyp).await;
        assert_eq!(snap(&db), (2, 17, 3), "{rtyp} born");
        db.process_record("X").await.unwrap();
        assert_eq!(snap(&db), processed, "{rtyp} processed");
    }
}
