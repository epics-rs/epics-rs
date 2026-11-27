//! The bundled `db/*.template` files are modbusApp/Db's, unchanged: a
//! template that drifts from C loads a record C does not (aoFloat64 once
//! had no `PINI`, mbbi/mbbo were missing, every record lacked `DESC`).
#![cfg(feature = "ioc")]

use std::path::Path;

use epics_base_rs::reference::{ReferenceTree, reference_path};

#[test]
fn every_c_template_is_bundled_unchanged() {
    let c_dir = reference_path(ReferenceTree::Modules, "modbus/modbusApp/Db");
    let rs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let mut names: Vec<_> = std::fs::read_dir(&c_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        // `._*` are macOS AppleDouble files in the checkout, not templates.
        .filter(|n| n.ends_with(".template") && !n.starts_with("._"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no templates under {}", c_dir.display());
    let differ: Vec<_> = names
        .iter()
        .filter(|n| {
            std::fs::read(rs_dir.join(n)).ok() != Some(std::fs::read(c_dir.join(n)).unwrap())
        })
        .collect();
    assert!(differ.is_empty(), "differ from C: {differ:?}");
}
