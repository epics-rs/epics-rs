use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::{FieldDeclaration, Record, SimmCells};
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

/// The cells the by-name path would have produced.
fn by_name(rec: &dyn Record) -> SimmCells {
    let short = |n| match rec.get_field(n) {
        Some(EpicsValue::Short(v)) => v,
        other => panic!("{}: {n} is {other:?}", rec.record_type()),
    };
    SimmCells {
        simm: short("SIMM"),
        sims: short("SIMS"),
        sdly: match rec.get_field("SDLY") {
            Some(EpicsValue::Double(v)) => v,
            None => -1.0,
            other => panic!("{}: SDLY is {other:?}", rec.record_type()),
        },
    }
}

#[test]
fn every_base_record_that_declares_simm_lends_its_cells() {
    let mut records: Vec<Box<dyn Record>> = BASE_TYPES
        .iter()
        .map(|t| create_record(t).unwrap())
        .collect();
    // busy and swait are module-owned, not in the base registry.
    records.push(Box::new(BusyRecord::default()));
    records.push(Box::new(SwaitRecord::default()));

    let mut failures = Vec::new();
    let mut checked = 0;
    for rec in &records {
        // A type without SIMM may still lend cells (subArray shares the
        // waveform struct); the simulation block only asks a type that
        // declares SIMM.
        if !declares_simm(&**rec) {
            continue;
        }
        checked += 1;
        match rec.simm_cells() {
            None => failures.push(format!("{}: simm_cells is None", rec.record_type())),
            Some(cells) if cells != by_name(&**rec) => failures.push(format!(
                "{}: cells {cells:?} != get_field {:?}",
                rec.record_type(),
                by_name(&**rec)
            )),
            Some(_) => {}
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(checked, 23, "the SIMM-declaring population changed");
}

#[test]
fn cells_follow_a_put() {
    let mut ai = create_record("longin").unwrap();
    ai.put_field("SIMM", EpicsValue::Short(1)).unwrap();
    ai.put_field("SIMS", EpicsValue::Short(2)).unwrap();
    ai.put_field("SDLY", EpicsValue::Double(0.25)).unwrap();
    assert_eq!(
        ai.simm_cells(),
        Some(SimmCells {
            simm: 1,
            sims: 2,
            sdly: 0.25
        })
    );
}
