//! lsi, lso and printf `init_record` pass 0 clamp SIZV to [16, 0x7fff]
//! before sizing VAL (`lsiRecord.c:49-55`, `lsoRecord.c:53-58`,
//! `printfRecord.c:339-344`), so after iocInit a `.db` SIZV outside the range
//! reads back as the bound.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn sizv_reads_back_clamped_after_iocinit() {
    let mut db = String::new();
    for rtype in ["lsi", "lso", "printf"] {
        db += &format!("record({rtype}, \"{rtype}:LO\") {{\n  field(SIZV, \"5\")\n}}\n");
        db += &format!("record({rtype}, \"{rtype}:HI\") {{\n  field(SIZV, \"40000\")\n}}\n");
    }
    let (db, _) = IocBuilder::new()
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for rtype in ["lsi", "lso", "printf"] {
        for (name, want) in [("LO", 16.0), ("HI", 32767.0)] {
            let got = db.get_pv(format!("{rtype}:{name}.SIZV")).unwrap();
            assert_eq!(got.to_f64(), Some(want), "{rtype}:{name}.SIZV read {got:?}");
        }
    }
}
