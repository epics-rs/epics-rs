//! ai, ao and swait INIT is a DBF_SHORT member in C: a `.db` value is
//! stored as the number, and `init_record` pass 1 overwrites it with TRUE
//! (`aiRecord.c:114`, `aoRecord.c:120`, `swaitRecord.c:330`).

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::swait::SwaitRecord;

const TYPES: [&str; 3] = ["ai", "ao", "swait"];

fn db() -> String {
    TYPES
        .iter()
        .map(|rtype| format!("record({rtype}, \"{rtype}\") {{\n  field(INIT, \"5\")\n}}\n"))
        .collect()
}

fn builder() -> IocBuilder {
    IocBuilder::new()
        .register_record_type("swait", || -> Box<dyn Record> {
            Box::new(SwaitRecord::default())
        })
        .db_string(&db(), &HashMap::new())
        .unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_loaded_init_reads_back_until_pass_1_sets_it() {
    let loaded = builder().load().await.unwrap();
    for rtype in TYPES {
        let got = loaded.get_pv(format!("{rtype}.INIT")).unwrap();
        assert_eq!(got.to_f64(), Some(5.0), "{rtype}.INIT read {got:?}");
    }

    let (built, _) = builder().build().await.unwrap();
    for rtype in TYPES {
        let got = built.get_pv(format!("{rtype}.INIT")).unwrap();
        assert_eq!(
            got.to_f64(),
            Some(1.0),
            "{rtype}.INIT after iocInit read {got:?}"
        );
    }
}
