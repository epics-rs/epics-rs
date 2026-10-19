//! `slots::<t>::<F>` is `F`'s position in `t`'s generated field table, the
//! position `RecordCell::new` caches `Record::field_slot` at.

mod module_records;

use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields, slots};
use epics_base_rs::server::record::{FieldDeclaration, FieldSlot};

/// Every `pub const F: FieldSlot = FieldSlot(i);` of every `pub mod t` in the
/// generated `slots` module, read off the checked-in source.
fn declared_slots() -> Vec<(String, String, u16)> {
    let src = include_str!("../src/server/record/dbd_generated.rs");
    let body = &src[src.find("pub mod slots {").expect("slots module")..];
    let mut out = Vec::new();
    let mut module = None;
    for line in body.lines().skip(1) {
        let line = line.trim();
        if let Some(m) = line.strip_prefix("pub mod ") {
            module = Some(m.trim_end_matches(" {").to_string());
        } else if let Some(count) = line.strip_prefix("pub const COUNT: usize = ") {
            let count: u16 = count.trim_end_matches(';').parse().expect(line);
            out.push((
                module.clone().expect("const outside a module"),
                "COUNT".into(),
                count,
            ));
        } else if let Some(rest) = line.strip_prefix("pub const ") {
            let (name, rest) = rest.split_once(": FieldSlot = FieldSlot(").expect(line);
            let index = rest.trim_end_matches(");").parse().expect(line);
            out.push((
                module.clone().expect("const outside a module"),
                name.into(),
                index,
            ));
        }
    }
    out
}

#[test]
fn slot_constants_index_the_field_table() {
    let declared = declared_slots();
    for rtype in RECORD_TYPES {
        // The table `RecordDesc::of` and `RecordCell::new` read.
        let record = module_records::create_any(rtype).expect(rtype);
        let fields = record.field_list();
        assert!(
            std::ptr::eq(fields, record_fields(rtype).unwrap()),
            "{rtype}"
        );
        let (count, consts): (Vec<_>, Vec<_>) = declared
            .iter()
            .filter(|(m, ..)| m == rtype)
            .partition(|(_, name, _)| name == "COUNT");
        assert_eq!(
            count.iter().map(|c| usize::from(c.2)).collect::<Vec<_>>(),
            [fields.len()],
            "{rtype}: COUNT"
        );
        assert_eq!(
            consts.len(),
            fields.len(),
            "{rtype}: one constant per field"
        );
        for (_, name, index) in consts {
            assert_eq!(fields[*index as usize].name, name, "slots::{rtype}::{name}");
        }
    }
    assert_eq!(
        declared
            .iter()
            .filter(|(_, name, _)| name != "COUNT")
            .count(),
        RECORD_TYPES
            .iter()
            .map(|t| record_fields(t).unwrap().len())
            .sum::<usize>(),
        "a slots module for a type RECORD_TYPES does not list"
    );

    // The constants the parse read are the ones that compile.
    assert_eq!(slots::ai::VAL, FieldSlot(0));
    assert_eq!(slots::ai::COUNT, record_fields("ai").unwrap().len());
    assert_eq!(
        record_fields("ao").unwrap()[slots::ao::OVAL.0 as usize].name,
        "OVAL"
    );
    assert_eq!(
        record_fields("aSub").unwrap()[slots::aSub::INPU.0 as usize].name,
        "INPU"
    );
    assert_eq!(
        record_fields("stringout").unwrap()[slots::stringout::VAL.0 as usize].name,
        "VAL"
    );
}
