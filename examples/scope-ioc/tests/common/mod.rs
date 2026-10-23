use std::collections::{BTreeMap, HashMap};

use epics_base_rs::server::db_loader::parse_db;

pub type Records = BTreeMap<String, (String, Vec<(String, String)>)>;

pub fn records(text: &str) -> Records {
    let macros: HashMap<String, String> = [
        ("P", "T:"),
        ("R", "R:"),
        ("PORT", "PORT"),
        ("ADDR", "0"),
        ("TIMEOUT", "1"),
        ("NPOINTS", "1000"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    parse_db(text, &macros)
        .unwrap()
        .into_iter()
        .map(|def| {
            let mut fields: Vec<(String, String)> = def
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.value.to_string()))
                .collect();
            fields.sort();
            (def.name.to_string(), (def.record_type, fields))
        })
        .collect()
}
