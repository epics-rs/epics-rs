//! A link field holds the bytes the `.db` gave it: C `dbParseLink` keeps the
//! target in `pv_link.pvname` as spelled and `dbGetString` prints it back, so
//! a Latin-1 target reads back under its own bytes. softIoc 7.0.10, with
//! `record(ao, "o") { field(OUT, "caf\xe9 PP") }`:
//!
//! ```text
//! dbgf o.OUT   -> DBF_OUTLINK: "caf\xe9 PP NMS"
//! ```
//!
//! and with [`RESOLVE_DB`], after `dbpf o.PROC 1` and `dbpf i.PROC 1`,
//! `caf\xe9` and `i` both read 7 and `dblsr o 1` lists `f`, `o`, `i` and
//! `caf\xe9` in one lock set.

mod module_records;

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields};
use epics_base_rs::types::{EpicsValue, link_class_of};

/// The `.db` the C capture above ran.
const RESOLVE_DB: &[u8] = b"record(ao, \"caf\xe9\") { field(VAL, \"5\") }
alias(\"caf\xe9\", \"al\xe9\")
record(ao, \"o\") { field(VAL, \"7\") field(OUT, \"caf\xe9 PP\") }
record(ai, \"i\") { field(INP, \"al\xe9\") }
record(calcout, \"f\") { field(CALC, \"1\") field(FLNK, \"caf\xe9\") }
";

/// A Latin-1 OUT, an INP through a Latin-1 alias and a Latin-1 FLNK resolve
/// to the record they name.
#[epics_macros_rs::epics_test]
async fn a_latin1_link_target_resolves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("resolve.db");
    std::fs::write(&path, RESOLVE_DB).expect("write the .db");
    let (db, _) = IocBuilder::new()
        .db_file(path.to_str().unwrap(), &HashMap::new())
        .expect("the .db loads")
        .build()
        .await
        .unwrap();

    match db.get_pv("o.OUT") {
        Ok(EpicsValue::String(s)) => assert_eq!(s.as_bytes(), b"caf\xe9 PP NMS"),
        other => panic!("o.OUT: {other:?}"),
    }
    for pv in ["o.PROC", "i.PROC"] {
        let (record, field) = pv.split_once('.').unwrap();
        db.put_record_field_from_ca_no_notify(record, field, EpicsValue::Long(1))
            .await
            .unwrap();
    }
    let val = |name: &[u8]| {
        let rec = db.get_record(name).expect("record exists");
        let inst = rec.read();
        inst.record.val().and_then(|v| v.to_f64())
    };
    assert_eq!(val(b"caf\xe9"), Some(7.0), "o.OUT wrote its target");
    assert_eq!(val(b"i"), Some(7.0), "i.INP read through the alias");

    let mut members: Vec<Vec<u8>> = db
        .lock_set_of("o")
        .expect("o has a lock set")
        .members
        .iter()
        .map(|m| m.to_vec())
        .collect();
    members.sort();
    assert_eq!(
        members,
        [&b"caf\xe9"[..], b"f", b"i", b"o"].map(<[u8]>::to_vec),
        "OUT, INP and FLNK each joined their target's lock set"
    );
}

/// Every link field every record type declares, set to a Latin-1 target that
/// exists, must read back starting with that target's bytes.
#[epics_macros_rs::epics_test]
async fn every_declared_link_field_keeps_its_bytes() {
    let mut db_text: Vec<u8> = b"record(ai, \"caf\xe9\") {\n}\n".to_vec();
    let mut wired: Vec<(String, &'static str)> = Vec::new();
    for (i, record_type) in RECORD_TYPES.iter().enumerate() {
        if module_records::create_any(record_type).is_err() {
            continue;
        }
        let Some(fields) = record_fields(record_type) else {
            continue;
        };
        let name = format!("r{i}");
        db_text.extend_from_slice(format!("record({record_type}, \"{name}\") {{\n").as_bytes());
        for desc in fields.iter().filter(|d| link_class_of(d).is_some()) {
            db_text.extend_from_slice(format!("  field({}, \"", desc.name).as_bytes());
            db_text.extend_from_slice(b"caf\xe9 NPP\")\n");
            wired.push((name.clone(), desc.name));
        }
        db_text.extend_from_slice(b"}\n");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("links.db");
    std::fs::write(&path, &db_text).expect("write the .db");
    let mut builder = IocBuilder::new();
    for (record_type, factory) in module_records::factories() {
        builder = builder.register_record_type(&record_type, factory);
    }
    let (db, _) = builder
        .db_file(path.to_str().unwrap(), &HashMap::new())
        .expect("the .db loads")
        .build()
        .await
        .unwrap();

    let lost: Vec<String> = wired
        .iter()
        .filter_map(|(name, field)| {
            let pv = format!("{name}.{field}");
            let Some(rec) = db.get_record(name.as_str()) else {
                return Some(format!("{name} did not load"));
            };
            let record_type = rec.read().record.record_type();
            let read = match db.get_pv(&pv) {
                Ok(EpicsValue::String(s)) => s.as_bytes().to_vec(),
                Ok(other) => return Some(format!("{record_type} {pv}: {other:?}")),
                Err(e) => return Some(format!("{record_type} {pv}: {e}")),
            };
            (!read.starts_with(b"caf\xe9")).then(|| {
                format!(
                    "{record_type} {pv}: {:?}",
                    String::from_utf8_lossy(&read).escape_debug().to_string()
                )
            })
        })
        .collect();
    assert!(
        lost.is_empty(),
        "link text lost its bytes:\n{}",
        lost.join("\n")
    );
}
