//! `scopeSimulator.db` against asyn's `testAsynPortDriver.db`: every record
//! has C's type and fields. The rs db once dropped the waveforms' LOPR/HOPR
//! and scanned `TimeBase_RBV` on I/O Intr, which C never posts
//! (testAsynPortDriver.cpp:71).

mod common;

use common::records;
use epics_base_rs::reference::{ReferenceTree, reference_path};

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
