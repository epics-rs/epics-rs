//! `NDArrayBase.template` against ADCore's: every record has C's type and
//! fields. `ArraySize0..2_RBV` once read `ARRAY_SIZE_X/Y/Z` instead of the
//! `Dim0..2SA_RBV` subArrays (NDArrayBase.template:360-394), so an RGB1 frame
//! read 16x16x3 where C reads 3x16x16, and the broken FLNK chain left
//! `ArraySize3..9_RBV` unprocessed.
#![cfg(feature = "ioc")]

use std::collections::{BTreeMap, HashMap};

use epics_base_rs::reference::{ReferenceTree, reference_path};
use epics_base_rs::server::db_loader::parse_db;

type Records = BTreeMap<String, (String, Vec<(String, String)>)>;

fn records(text: &str) -> Records {
    let macros: HashMap<String, String> = [("P", "T:"), ("R", "R:"), ("PORT", "PORT")]
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

#[test]
fn every_record_has_c_s_type_and_fields() {
    let c_path = reference_path(
        ReferenceTree::Modules,
        "ADCore/ADApp/Db/NDArrayBase.template",
    );
    let c = records(&std::fs::read_to_string(&c_path).unwrap());
    let rs = records(include_str!("../db/NDArrayBase.template"));
    assert_eq!(
        c.keys().collect::<Vec<_>>(),
        rs.keys().collect::<Vec<_>>(),
        "record names"
    );
    let differ: Vec<_> = c
        .iter()
        .filter(|(name, def)| rs[*name] != **def)
        .map(|(name, def)| format!("{name}:\n  C  {def:?}\n  rs {:?}", rs[name]))
        .collect();
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}
