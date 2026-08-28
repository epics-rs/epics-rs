//! Invariant: **a record type has exactly one field declaration.**
//!
//! C has exactly one: the `.dbd`, read at runtime. The port compiles the
//! vendored `.dbd`s into `dbd_generated`, and until now several record types
//! also carried a hand-written `FieldDesc` table for the same fields. Both were
//! live. The wire happened to come out right only because `field_desc_of`
//! consulted the generated one first — a runtime ordering, not a guarantee —
//! and every consumer that reached for `Record::field_list()` directly (`dbpr`,
//! the `dbpf` typo hint, `motor`'s field gate, `QSRV`'s VAL introspection) read
//! the hand-written one instead. That is how `waveform.FTVL` was declared
//! `DBF_SHORT` with no `menu()` while `waveformRecord.dbd:83-89` declares it
//! `DBF_MENU`/`menu(menuFtype)`, and how `sseq.ABORTING` was declared
//! `special(SPC_NOMOD)` while `sseqRecord.dbd:820-824` declares it
//! `special(SPC_MOD)` — writable.
//!
//! The second source is now unrepresentable rather than merely unread:
//! `field_list()` lives on `FieldDeclaration`, which is blanket-implemented for
//! every `Record` and so cannot be overridden, and it serves the generated
//! table for every record type the `.dbd` set covers, never falling through to
//! `Record::declared_fields`. A record type cannot *supply* a declaration; it
//! can only be *asked* for one.
//!
//! The boundary this file walks is the fall-through: covered types must resolve
//! to the generated table (and carry no hand table at all), uncovered types must
//! resolve to their hand table.
//!
//! A second invariant lives here because it is the same question asked of the
//! other end of the `.dbd` set: **base's default registry contains base record
//! types only.** Vendoring a `.dbd` says the port knows a record type's shape;
//! it does not make that type base's to serve. `stdRecords.dbd` is C's manifest
//! of what a base IOC links, and the seven types outside it belong to synApps
//! `calc`, to asyn and to busy.

mod module_records;

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields};
use epics_base_rs::server::record::{FieldDeclaration, Record};

/// C's own manifest of what an EPICS Base IOC links, vendored byte-identical
/// from `$EPICS_BASE/dbd/stdRecords.dbd`. Each `include "<name>Record.dbd"`
/// names one base record type; nothing else in the file does.
const STD_RECORDS_DBD: &str = include_str!("../dbd/stdRecords.dbd");

/// The base record types, read off that manifest rather than listed here.
fn base_record_types() -> Vec<&'static str> {
    STD_RECORDS_DBD
        .lines()
        .filter_map(|l| l.trim().strip_prefix("include \""))
        .filter_map(|l| l.strip_suffix("Record.dbd\""))
        .collect()
}

/// **Base's registry contains base record types only.**
///
/// `create_record` with nothing registered is what a stock `IocApplication`
/// gives a `.db` load. It must construct every type in `stdRecords.dbd` and
/// refuse every other type this crate vendors a `.dbd` for — `aCalcout`,
/// `sCalcout`, `sseq`, `swait` and `transform` are synApps `calc`'s, `asyn` is
/// asyn's, `busy` is busy's, and base claiming them made a stock base IOC serve
/// record types a real one does not have. They stay implemented here; what
/// changed is that the application must ask for them, with
/// `register_record_type` on `IocBuilder`, on `IocApplication`, or in
/// `db_loader` directly.
///
/// Both directions are the gate. Asserting only that base types load would pass
/// with the module types silently back in the registry; asserting only that the
/// seven refuse would pass with base's own set gutted.
#[test]
fn bases_default_registry_holds_base_record_types_and_no_others() {
    let base: Vec<&str> = base_record_types();
    assert_eq!(
        base.len(),
        34,
        "stdRecords.dbd parsed to {} record types — the parse or the manifest moved",
        base.len()
    );

    for &record_type in &base {
        create_record(record_type).unwrap_or_else(|e| {
            panic!("{record_type} is in stdRecords.dbd, so a stock base must construct it: {e:?}")
        });
    }

    let module_owned: Vec<&str> = RECORD_TYPES
        .iter()
        .copied()
        .filter(|t| !base.contains(t))
        .collect();
    assert!(
        !module_owned.is_empty(),
        "the vendored .dbd set no longer exceeds stdRecords.dbd — this gate has nothing to guard"
    );

    for &record_type in &module_owned {
        let Err(err) = create_record(record_type) else {
            panic!(
                "{record_type} is not in stdRecords.dbd, so base's default registry must not \
                 construct it — an application registers it"
            );
        };
        let message = format!("{err:?}");
        assert!(
            message.contains("register_record_type"),
            "{record_type}: the refusal must name the way out, got {message}"
        );
    }

    // The fixture the whole-set walkers construct these through covers exactly
    // this set: a newly vendored module `.dbd` fails here, not silently later.
    let mut have: Vec<String> = module_records::factories().into_keys().collect();
    let mut want: Vec<String> = module_owned.iter().map(|t| t.to_string()).collect();
    have.sort();
    want.sort();
    assert_eq!(
        have, want,
        "tests/module_records must supply a factory for every module-owned record type"
    );
}

/// Covered side of the boundary: every record type with a vendored `.dbd` is
/// served that `.dbd`'s table, and hands over no second table of its own.
/// Module-owned types come through the same opt-in an application uses, so the
/// invariant still covers all of them.
#[test]
fn a_dbd_covered_record_type_is_declared_only_by_its_dbd() {
    for &record_type in RECORD_TYPES {
        let record = module_records::create_any(record_type)
            .unwrap_or_else(|_| panic!("{record_type}: vendored .dbd but no record impl"));
        let generated = record_fields(record_type)
            .unwrap_or_else(|| panic!("{record_type} is in RECORD_TYPES but has no field table"));

        // The declaration served IS the generated one — same table, not merely
        // an equal one.
        assert!(
            std::ptr::eq(record.field_list(), generated),
            "{record_type}: field_list() must serve the table generated from its .dbd"
        );

        // And there is no second table to contradict it.
        assert!(
            Record::declared_fields(record.as_ref()).is_empty(),
            "{record_type} has a vendored .dbd, so it must not also hand-write a field table"
        );
    }
    assert!(
        RECORD_TYPES.len() >= 40,
        "only {} record types walked — the .dbd set emptied",
        RECORD_TYPES.len()
    );
}

/// Uncovered side of the boundary: a record type the `.dbd` set does NOT cover
/// falls through to its hand-written table, which is then its one declaration.
///
/// The downstream Tier-3 record types (`motor`, `table`, `scaler`, `epid`,
/// `throttle`, `timestamp`) live here, in their own crates. This synthetic one
/// stands in for them so the fall-through has a test inside `epics-base-rs`.
#[test]
fn a_record_type_with_no_dbd_falls_through_to_its_hand_table() {
    use epics_base_rs::error::CaResult;
    use epics_base_rs::server::record::FieldDesc;
    use epics_base_rs::types::{DbFieldType, EpicsValue};

    struct NoDbdRecord;

    static HAND: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

    impl Record for NoDbdRecord {
        fn record_type(&self) -> &'static str {
            "noSuchRecordTypeInAnyDbd"
        }
        fn get_field(&self, _name: &str) -> Option<EpicsValue> {
            None
        }
        fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
            Ok(())
        }
        fn declared_fields(&self) -> &'static [FieldDesc] {
            HAND
        }
    }

    assert!(
        record_fields("noSuchRecordTypeInAnyDbd").is_none(),
        "the premise of this test is that no .dbd covers this record type"
    );
    assert!(std::ptr::eq(NoDbdRecord.field_list(), HAND));
}

/// A record type that overrides nothing has no declaration at all — it does not
/// silently inherit `dbCommon` or an empty-but-plausible table. This is what
/// makes `#[derive(EpicsRecord)]` safe to strip of its field emission: a derived
/// record with no `.dbd` gets `&[]`, which is loud, rather than a table invented
/// from its Rust struct members, which was quiet and wrong.
#[test]
fn a_record_type_that_declares_nothing_has_no_fields() {
    use epics_base_rs::error::CaResult;
    use epics_base_rs::types::EpicsValue;

    struct UndeclaredRecord;

    impl Record for UndeclaredRecord {
        fn record_type(&self) -> &'static str {
            "alsoNotInAnyDbd"
        }
        fn get_field(&self, _name: &str) -> Option<EpicsValue> {
            None
        }
        fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
            Ok(())
        }
    }

    assert!(UndeclaredRecord.field_list().is_empty());
}
