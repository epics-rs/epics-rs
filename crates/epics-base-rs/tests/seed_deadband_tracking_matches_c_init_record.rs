//! `Record::seed_deadband_tracking` must seed MLST/ALST/LALM from VAL for
//! exactly the record types whose C `init_record` does, and for no others.
//!
//! Most C value records end `init_record` with
//! `prec->mlst = prec->alst = prec->lalm = prec->val`, but seven of the types
//! this port implements do not, and nothing in a record's shape predicts which
//! group it lands in: `calcout` seeds and `acalcout` does not; `bo` seeds and
//! `busy` does not. Seeding uniformly suppressed a first-cycle post that C
//! makes whenever such a record is loaded with a nonzero initial VAL.
//!
//! The table below is every record type this port serves MLST, ALST or LALM
//! for, with the C `init_record` line range behind each verdict. The verdict
//! is about THIS hook, not about C in the abstract: `sub` is a `false` whose C
//! counterpart does seed, because its seed sits below `init_record`'s early
//! returns and so belongs to a later owner.

mod module_records;

use epics_base_rs::server::device_support::DsetInit;
use epics_base_rs::types::EpicsValue;

/// What each type's C `init_record` does to its trackers after its dset
/// `init_record` was absent, returned 0 (or `ao`/`bo`/`mbbo`'s 2), or
/// returned anything else.
#[derive(Clone, Copy)]
struct Seeds {
    absent: bool,
    succeeded: bool,
    failed: bool,
}

const ALWAYS: Seeds = Seeds {
    absent: true,
    succeeded: true,
    failed: true,
};
const NEVER: Seeds = Seeds {
    absent: false,
    succeeded: false,
    failed: false,
};
/// `if (status) return status;` ahead of the seed.
const UNLESS_FAILED: Seeds = Seeds {
    absent: true,
    succeeded: true,
    failed: false,
};
/// `aiRecord.c:122-129`: any dset `init_record` returns before the seed.
const ONLY_ABSENT: Seeds = Seeds {
    absent: true,
    succeeded: false,
    failed: false,
};

/// `(record type, a nonzero VAL in the type's own representation, what
/// `seed_deadband_tracking` does per dset outcome)`.
const CASES: &[(&str, EpicsValue, Seeds)] = &[
    ("ai", EpicsValue::Double(3.0), ONLY_ABSENT), // aiRecord.c:122-129
    ("ao", EpicsValue::Double(3.0), UNLESS_FAILED), // aoRecord.c:127-157
    ("bi", EpicsValue::Enum(1), UNLESS_FAILED),   // biRecord.c:112-114
    ("bo", EpicsValue::Enum(1), ALWAYS),          // boRecord.c:158-172
    ("calcout", EpicsValue::Double(3.0), ALWAYS), // calcoutRecord.c:215-217
    ("int64in", EpicsValue::Int64(3), UNLESS_FAILED), // int64inRecord.c:114-116
    ("int64out", EpicsValue::Int64(3), UNLESS_FAILED), // int64outRecord.c:114-116
    ("longin", EpicsValue::Long(3), UNLESS_FAILED), // longinRecord.c:114-120
    ("longout", EpicsValue::Long(3), UNLESS_FAILED), // longoutRecord.c:117-123
    ("mbbi", EpicsValue::Enum(3), ALWAYS),        // mbbiRecord.c:133-137
    ("mbbiDirect", EpicsValue::Long(3), ALWAYS),  // mbbiDirectRecord.c:116-128
    ("mbbo", EpicsValue::UShort(3), ALWAYS),      // mbboRecord.c:141-179
    ("mbboDirect", EpicsValue::Long(3), ALWAYS),  // mbboDirectRecord.c:127-160
    // Does not seed
    //
    // `sub` is the one row whose C counterpart DOES seed: `subRecord.c:130-132`
    // sits below `init_record`'s empty-SNAM (`:122`) and unregistered-SNAM
    // (`:128`) early returns, so only a record whose SNAM resolved reaches it.
    // The generic tail runs before the resolution exists and cannot tell those
    // apart, so the seed is performed by the resolution owner
    // (`ioc_app::wire_subroutine`) and this hook is empty. Covered by
    // `tests/snam_is_resolved_only_where_c_resolves_it.rs`.
    ("sub", EpicsValue::Double(3.0), NEVER), // subRecord.c:130-132
    ("calc", EpicsValue::Double(3.0), NEVER), // calcRecord.c:90-112
    ("dfanout", EpicsValue::Double(3.0), NEVER), // dfanoutRecord.c:96-109
    ("sel", EpicsValue::Double(3.0), NEVER), // selRecord.c:88-108
    ("scalcout", EpicsValue::Double(3.0), NEVER), // sCalcoutRecord.c:203-320
    ("acalcout", EpicsValue::Double(3.0), NEVER), // aCalcoutRecord.c:171-277
    ("busy", EpicsValue::Enum(1), NEVER),    // busyRecord.c:127-181
    ("swait", EpicsValue::Double(3.0), NEVER), // swaitRecord.c:266-
];

#[test]
fn each_type_seeds_its_trackers_exactly_as_its_c_init_record_does() {
    // Collected rather than asserted per cell: the two ways to get this wrong
    // (seeding a type C leaves at 0, and failing to seed one C seeds) live in
    // different record types, and stopping at the first would hide the other.
    let mut wrong = Vec::new();

    for (rtype, val, seeds) in CASES {
        for (dset_init, seeds) in [
            (DsetInit::Absent, seeds.absent),
            (DsetInit::Ok, seeds.succeeded),
            (DsetInit::Failed, seeds.failed),
        ] {
            let mut rec =
                module_records::create_any(rtype).unwrap_or_else(|e| panic!("{rtype}: {e:?}"));
            rec.put_field("VAL", val.clone())
                .unwrap_or_else(|e| panic!("{rtype}: VAL put rejected: {e:?}"));
            let val = val.to_f64().expect("test values are numeric");
            assert_ne!(
                val, 0.0,
                "{rtype}: the case value must differ from the 0 a non-seeding \
                 record leaves behind, or the two groups are indistinguishable"
            );

            rec.seed_deadband_tracking(dset_init);

            let want = if seeds { val } else { 0.0 };
            for field in ["MLST", "ALST", "LALM"] {
                let Some(got) = rec.get_field(field) else {
                    continue; // this type carries no such cell
                };
                let got = got
                    .to_f64()
                    .unwrap_or_else(|| panic!("{rtype}.{field} is not numeric"));
                if got != want {
                    wrong.push(format!(
                        "{rtype}.{field}: VAL={val}, dset {dset_init:?}, got {got} want {want}"
                    ));
                }
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
        let Ok(rec) = module_records::create_any(rtype) else {
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
