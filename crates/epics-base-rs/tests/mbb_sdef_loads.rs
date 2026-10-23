//! mbbi/mbbo SDEF is a DBF_SHORT member in C: a `.db` value is stored, and
//! `init_common` then overwrites it with whether any state is defined
//! (`mbbiRecord.c:100-104`, `mbboRecord.c:102-106`).

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

fn db(rtype: &str) -> String {
    format!(
        "record({rtype}, \"{rtype}:none\") {{\n  field(SDEF, \"1\")\n}}\n\
         record({rtype}, \"{rtype}:zrst\") {{\n  field(SDEF, \"0\")\n  field(ZRST, \"a\")\n}}\n"
    )
}

#[epics_macros_rs::epics_test]
async fn a_loaded_sdef_reads_back_until_init_common_rewrites_it() {
    for rtype in ["mbbi", "mbbo"] {
        let loaded = IocBuilder::new()
            .db_string(&db(rtype), &HashMap::new())
            .unwrap()
            .load()
            .await
            .unwrap();
        let (built, _) = IocBuilder::new()
            .db_string(&db(rtype), &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        for (name, at_load, at_init) in [("none", 1.0, 0.0), ("zrst", 0.0, 1.0)] {
            let pv = format!("{rtype}:{name}.SDEF");
            let got = loaded.get_pv(&pv).unwrap();
            assert_eq!(got.to_f64(), Some(at_load), "{pv} after load read {got:?}");
            let got = built.get_pv(&pv).unwrap();
            assert_eq!(
                got.to_f64(),
                Some(at_init),
                "{pv} after iocInit read {got:?}"
            );
        }
    }
}
