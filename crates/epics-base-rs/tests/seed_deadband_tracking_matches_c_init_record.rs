// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that touches no runtime.
//! `Record::seed_deadband_tracking` must seed MLST/ALST/LALM from VAL for
//! exactly the record types whose C `init_record` does, and for no others.
//!
//! Most C value records end `init_record` with
//! `prec->mlst = prec->alst = prec->lalm = prec->val`, but seven of the types
//! this port implements do not, and nothing in a record's shape predicts which
//! group it lands in: `sub` seeds and `calc` does not; `calcout` seeds and
//! `acalcout` does not; `bo` seeds and `busy` does not. Seeding uniformly
//! suppressed a first-cycle post that C makes whenever such a record is loaded
//! with a nonzero initial VAL.
//!
//! The table below is every record type this port serves MLST, ALST or LALM
//! for, with the C `init_record` line range behind each verdict.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::types::EpicsValue;

/// `(record type, a nonzero VAL in the type's own representation, C seeds?)`
const CASES: &[(&str, EpicsValue, bool)] = &[
    // Seeds — std/rec
    ("ai", EpicsValue::Double(3.0), true), // aiRecord.c:129-131
    ("ao", EpicsValue::Double(3.0), true), // aoRecord.c:157-159
    ("bi", EpicsValue::Enum(1), true),     // biRecord.c:114-115
    ("bo", EpicsValue::Enum(1), true),     // boRecord.c:165-173
    ("calcout", EpicsValue::Double(3.0), true), // calcoutRecord.c:217-219
    ("int64in", EpicsValue::Int64(3), true), // int64inRecord.c:116-118
    ("int64out", EpicsValue::Int64(3), true), // int64outRecord.c:116-118
    ("longin", EpicsValue::Long(3), true), // longinRecord.c:120-122
    ("longout", EpicsValue::Long(3), true), // longoutRecord.c:123-125
    ("mbbi", EpicsValue::Enum(3), true),   // mbbiRecord.c:137-138
    ("mbbiDirect", EpicsValue::Long(3), true), // mbbiDirectRecord.c:128
    ("mbbo", EpicsValue::UShort(3), true), // mbboRecord.c:179-180
    ("mbboDirect", EpicsValue::Long(3), true), // mbboDirectRecord.c:160
    ("sub", EpicsValue::Double(3.0), true), // subRecord.c:130-132
    // Does not seed
    ("calc", EpicsValue::Double(3.0), false), // calcRecord.c:90-112
    ("dfanout", EpicsValue::Double(3.0), false), // dfanoutRecord.c:96-109
    ("sel", EpicsValue::Double(3.0), false),  // selRecord.c:88-108
    ("scalcout", EpicsValue::Double(3.0), false), // sCalcoutRecord.c:203-320
    ("acalcout", EpicsValue::Double(3.0), false), // aCalcoutRecord.c:171-277
    ("busy", EpicsValue::Enum(1), false),     // busyRecord.c:127-181
    ("swait", EpicsValue::Double(3.0), false), // swaitRecord.c:266-
];

#[test]
fn each_type_seeds_its_trackers_exactly_as_its_c_init_record_does() {
    // Collected rather than asserted per cell: the two ways to get this wrong
    // (seeding a type C leaves at 0, and failing to seed one C seeds) live in
    // different record types, and stopping at the first would hide the other.
    let mut wrong = Vec::new();

    for (rtype, val, seeds) in CASES {
        let mut rec = create_record(rtype).unwrap_or_else(|e| panic!("{rtype}: {e:?}"));
        rec.put_field("VAL", val.clone())
            .unwrap_or_else(|e| panic!("{rtype}: VAL put rejected: {e:?}"));
        let val = val.to_f64().expect("test values are numeric");
        assert_ne!(
            val, 0.0,
            "{rtype}: the case value must differ from the 0 a non-seeding \
             record leaves behind, or the two groups are indistinguishable"
        );

        rec.seed_deadband_tracking();

        let want = if *seeds { val } else { 0.0 };
        for field in ["MLST", "ALST", "LALM"] {
            let Some(got) = rec.get_field(field) else {
                continue; // this type carries no such cell
            };
            let got = got
                .to_f64()
                .unwrap_or_else(|| panic!("{rtype}.{field} is not numeric"));
            if got != want {
                wrong.push(format!(
                    "{rtype}.{field}: VAL={val}, C {} seed at init_record, got {got} want {want}",
                    if *seeds { "does" } else { "does not" }
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "{} cells disagree with C:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// The census the table rests on: no record type may serve one of these cells
/// without appearing above, or it silently inherits whichever default happens
/// to be in force.
#[test]
fn every_type_that_serves_a_tracker_is_in_the_table() {
    for rtype in epics_base_rs::server::record::dbd_generated::RECORD_TYPES {
        let Ok(rec) = create_record(rtype) else {
            continue;
        };
        let serves = ["MLST", "ALST", "LALM"]
            .iter()
            .any(|f| rec.get_field(f).is_some());
        assert_eq!(
            serves,
            CASES.iter().any(|(t, _, _)| t == rtype),
            "{rtype}: serves a deadband tracker but is not in the C init_record table \
             (or is in the table without serving one)"
        );
    }
}
