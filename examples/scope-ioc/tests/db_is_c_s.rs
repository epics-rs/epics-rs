//! `scopeSimulator.db` against asyn's `testAsynPortDriver.db`: every record
//! has C's type and fields, and every link names a driver parameter. The rs
//! db once dropped the waveforms' LOPR/HOPR and scanned `TimeBase_RBV` on
//! I/O Intr, which C never posts (testAsynPortDriver.cpp:71).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use asyn_rs::runtime::sync::Notify;
use epics_base_rs::reference::{ReferenceTree, reference_path};
use epics_base_rs::server::db_loader::parse_db;
use scope_ioc::driver::ScopeSimulator;

type Records = BTreeMap<String, (String, Vec<(String, String)>)>;

fn records(text: &str) -> Records {
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

#[test]
fn every_record_has_c_s_type_and_fields() {
    let c_path = reference_path(ReferenceTree::Modules, "asyn/db/testAsynPortDriver.db");
    let c = records(&std::fs::read_to_string(&c_path).unwrap());
    let rs = records(include_str!("../db/scopeSimulator.db"));
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

#[test]
fn every_link_names_a_driver_parameter() {
    let driver = ScopeSimulator::new("scopeDbParams", Arc::new(Notify::new()));
    let missing: Vec<_> = records(include_str!("../db/scopeSimulator.db"))
        .into_values()
        .flat_map(|(_, fields)| fields)
        .filter(|(name, _)| name == "INP" || name == "OUT")
        .map(|(_, link)| link.rsplit(')').next().unwrap().to_owned())
        .filter(|drv_info| driver.base.find_param(drv_info).is_none())
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
}
