//! ONVA..ONVU are the `neva` counts `monitor()` last posted: the init tail
//! seeds them (`aSubRecord.c:164-171`, `onva = neva`) and `monitor()` moves
//! them (`:422-438`). Served from a spill cell, they read the `.dbd` initial 1
//! whatever the record did.

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

fn two_elements(rec: &mut dyn Record) -> CaResult<i64> {
    rec.put_field("VALA", EpicsValue::DoubleArray(vec![1.0, 2.0].into()))?;
    Ok(0)
}

#[epics_macros_rs::epics_test]
async fn onva_follows_the_neva_the_record_posted() {
    let (db, _) = IocBuilder::new()
        .register_subroutine("two", two_elements)
        .db_string(
            "record(aSub, \"S\") {\n  field(SNAM, \"two\")\n  field(NOVA, \"4\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("S.ONVA").unwrap().to_f64(),
        Some(4.0),
        "the init tail copies NEVA (= NOVA) into ONVA"
    );

    db.process_record("S").await.unwrap();
    assert_eq!(db.get_pv("S.NEVA").unwrap().to_f64(), Some(2.0));
    assert_eq!(
        db.get_pv("S.ONVA").unwrap().to_f64(),
        Some(2.0),
        "monitor() copies the posted NEVA into ONVA"
    );
}
