//! lsi/lso LEN and OLEN and printf LEN are DBF_ULONG members in C: a `.db`
//! value is stored as the number, whatever it is, and `init_record` pass 0
//! zeroes them with the calloc of VAL/OVAL (`lsiRecord.c:58-60`,
//! `lsoRecord.c:62-64`, `printfRecord.c:348`). Storing LEN by sizing the
//! buffer to it made a `.db` LEN of 4294967295 allocate 4 GB.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::ioc_builder::IocBuilder;

const FIELDS: [(&str, &str); 5] = [
    ("lsi", "LEN"),
    ("lsi", "OLEN"),
    ("lso", "LEN"),
    ("lso", "OLEN"),
    ("printf", "LEN"),
];

fn db() -> String {
    FIELDS
        .iter()
        .map(|(rtype, field)| {
            format!(
                "record({rtype}, \"{rtype}:{field}\") {{\n  field({field}, \"4294967295\")\n}}\n"
            )
        })
        .collect()
}

#[epics_macros_rs::epics_test]
async fn a_loaded_len_is_the_number_until_init_zeroes_it() {
    let started = Instant::now();
    let loaded = IocBuilder::new()
        .db_string(&db(), &HashMap::new())
        .unwrap()
        .load()
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the load took {:?}",
        started.elapsed()
    );
    for (rtype, field) in FIELDS {
        let got = loaded.get_pv(format!("{rtype}:{field}.{field}")).unwrap();
        assert_eq!(
            got.to_f64(),
            Some(4294967295.0),
            "{rtype}.{field} read {got:?}"
        );
    }

    let (built, _) = IocBuilder::new()
        .db_string(&db(), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (rtype, field) in FIELDS {
        let got = built.get_pv(format!("{rtype}:{field}.{field}")).unwrap();
        assert_eq!(
            got.to_f64(),
            Some(0.0),
            "{rtype}.{field} after iocInit read {got:?}"
        );
    }
}
