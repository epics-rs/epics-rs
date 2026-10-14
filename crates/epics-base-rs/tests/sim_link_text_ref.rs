use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::server::records::{busy::BusyRecord, swait::SwaitRecord};
use epics_base_rs::types::EpicsValue;

/// Every arm of `create_record_raw`'s base registry.
const BASE_TYPES: &[&str] = &[
    "ai",
    "ao",
    "bi",
    "bo",
    "stringin",
    "stringout",
    "longin",
    "longout",
    "int64in",
    "int64out",
    "lsi",
    "lso",
    "mbbi",
    "mbbo",
    "mbbiDirect",
    "mbboDirect",
    "event",
    "printf",
    "waveform",
    "aai",
    "aao",
    "subArray",
    "calc",
    "fanout",
    "seq",
    "calcout",
    "dfanout",
    "compress",
    "histogram",
    "sel",
    "sub",
    "aSub",
    "permissive",
    "state",
];

fn declares_simm(rec: &dyn Record) -> bool {
    rec.field_list().iter().any(|f| f.name == "SIMM")
}

/// The records the simulation block reads SIML/SIOL from: every base type
/// that declares SIMM, the three waveform kinds included.
fn sim_records() -> Vec<Box<dyn Record>> {
    let mut records: Vec<Box<dyn Record>> = BASE_TYPES
        .iter()
        .map(|t| create_record(t).unwrap())
        .collect();
    // busy and swait are module-owned, not in the base registry.
    records.push(Box::new(BusyRecord::default()));
    records.push(Box::new(SwaitRecord::default()));
    records.retain(|rec| declares_simm(&**rec));
    records
}

#[test]
fn sim_links_are_lent_as_the_text_get_field_serves() {
    let records = sim_records();
    assert_eq!(records.len(), 23, "the SIMM-declaring population changed");
    let mut failures = Vec::new();
    for mut rec in records {
        let rt = rec.record_type();
        if rec.link_text_ref("SIML") != Some(b"") || rec.link_text_ref("SIOL") != Some(b"") {
            failures.push(format!(
                "{rt}: unset SIML/SIOL lent as {:?}/{:?}",
                rec.link_text_ref("SIML"),
                rec.link_text_ref("SIOL")
            ));
            continue;
        }
        if let Err(e) = rec.put_field("SIOL", EpicsValue::String("OTHER.VAL".into())) {
            failures.push(format!("{rt}: put SIOL refused: {e}"));
            continue;
        }
        let owned = match rec.get_field("SIOL") {
            Some(EpicsValue::String(s)) => s,
            other => {
                failures.push(format!("{rt}: get_field SIOL is {other:?}"));
                continue;
            }
        };
        let lent = rec.link_text_ref("SIOL");
        if lent != Some(b"OTHER.VAL") || lent != Some(owned.as_bytes()) {
            failures.push(format!("{rt}: SIOL lent {lent:?}, get_field {owned:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
