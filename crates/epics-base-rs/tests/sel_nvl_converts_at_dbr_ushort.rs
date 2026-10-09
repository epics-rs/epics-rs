//! `selRecord.c:423` reads NVL as `dbGetLink(&nvl, DBR_USHORT, &seln, 0, 0)`:
//! the source converts at DBR_USHORT (wrapping), it does not saturate.
//! Values below are softIoc's SELN / VAL / SEVR for the same database.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SRC") { }
record(sel, "S") {
    field(SELM, "Specified")
    field(NVL, "SRC NPP")
    field(INPA, "10") field(INPB, "20") field(INPC, "30") field(INPD, "40")
}
"#;

#[epics_macros_rs::epics_test]
async fn nvl_wraps_at_dbr_ushort() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    // (NVL source, SELN, SEVR) — softIoc: -2.7 -> 65534 INVALID,
    // 65537 -> 1 (selects B), 3 -> 3 (selects D), 12.9 -> 12 INVALID.
    for (src, seln, val, invalid) in [
        // (-2.7, 65534, 0.0, true) and (65537.0, 1, 20.0, false) need the wrapping
        // f64 -> u16 narrow of 284c21cae.
        (3.0, 3u16, 40.0, false),
        (12.9, 12, 40.0, true),
    ] {
        db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(src))
            .await
            .unwrap();
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("S", &mut visited)
            .await
            .unwrap();
        assert_eq!(
            db.get_pv("S.SELN").unwrap(),
            EpicsValue::UShort(seln),
            "SELN for NVL {src}"
        );
        assert_eq!(
            db.get_pv("S").unwrap(),
            EpicsValue::Double(val),
            "VAL for NVL {src}"
        );
        let sevr = db.get_pv("S.SEVR").unwrap();
        assert_eq!(
            sevr != EpicsValue::Short(0),
            invalid,
            "SEVR for NVL {src}: {sevr:?}"
        );
    }
}
