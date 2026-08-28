//! C `getDoubleString` seeds `precision = 6` and keeps it for every record
//! type whose rset NULLs `get_precision`.
//!
//! ```c
//! /* dbConvert.c:772-790 */
//! long precision = 6;
//! if (paddr) prset = dbGetRset(paddr);
//! if (prset && prset->get_precision) status = prset->get_precision(paddr, &precision);
//! if (nRequest == 1 && offset == 0) { cvtDoubleToString(*psrc, pdst, precision); ... }
//! ```
//!
//! `getFloatString` is the twin. Seventeen types carry
//! `#define get_precision NULL` (`biRecord.c:54` and its siblings), and every
//! DBF_DOUBLE/DBF_FLOAT field on all of them renders at 6.
//!
//! The port could not reach the seed: `DisplayInfo` is minted for EVERY
//! snapshot (the DESC leaf pvxs fills for every record type), so
//! `display.is_some()` said nothing about the rset. The supply mask
//! (`PropertySupport`, via `Snapshot::precision`) is what carries it.
//!
//! Boundaries: a NULL slot on a double field and on a float field, a supplied
//! slot, a supplied slot answering a per-field literal, and a slot supplied by
//! the type but narrowed away by C's DBF gate.

mod module_records;

use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::EpicsValue;

fn instance(record_type: &str) -> RecordInstance {
    let rec = module_records::create_any(record_type).expect("record type is registered");
    RecordInstance::new_boxed(format!("T:{record_type}"), rec)
}

/// `caget -t` — the DBR_STRING form.
fn dbr_string_of(inst: &RecordInstance, field: &str) -> String {
    let snap = inst.snapshot_for_field(field).expect("field exists");
    let bytes = epics_base_rs::types::encode_dbr(0, &snap).expect("DBR_STRING encodes");
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn put(inst: &mut RecordInstance, name: &str, value: EpicsValue) {
    if inst.record.put_field(name, value.clone()).is_ok() {
        return;
    }
    inst.put_common_field(name, value)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
}

/// The lead's trigger: `record(bi,"B"){field(AFTC,"2.5")}`, `caget -s B.AFTC`.
#[test]
fn a_record_type_with_no_get_precision_renders_doubles_at_six() {
    let mut bi = instance("bi");
    put(&mut bi, "AFTC", EpicsValue::Double(2.5));
    assert_eq!(
        dbr_string_of(&bi, "AFTC"),
        "2.500000",
        "biRecord.c:54 is `#define get_precision NULL`, so C keeps its seed"
    );
}

/// The same seed on two sibling NULL-slot types.
#[test]
fn the_seed_holds_across_the_null_slot_types() {
    let mut mbbi = instance("mbbi");
    put(&mut mbbi, "AFTC", EpicsValue::Double(0.25));
    assert_eq!(dbr_string_of(&mbbi, "AFTC"), "0.250000");

    let mut li = instance("longin");
    put(&mut li, "AFTC", EpicsValue::Double(3.0));
    assert_eq!(
        dbr_string_of(&li, "AFTC"),
        "3.000000",
        "longinRecord.c NULLs get_precision too, and AFTC is DBF_DOUBLE"
    );
}

/// The other side of the gate: a type that DOES supply the slot answers PREC,
/// not 6.
#[test]
fn a_supplied_slot_still_answers_prec() {
    let mut ai = instance("ai");
    put(&mut ai, "PREC", EpicsValue::Short(3));
    put(&mut ai, "VAL", EpicsValue::Double(2.5));
    assert_eq!(dbr_string_of(&ai, "VAL"), "2.500");

    // PREC=0 is a real answer, not "unsupplied" — it must not fall to 6.
    let mut ai0 = instance("ai");
    put(&mut ai0, "PREC", EpicsValue::Short(0));
    put(&mut ai0, "VAL", EpicsValue::Double(2.5));
    assert_eq!(dbr_string_of(&ai0, "VAL"), "3", "cvtDoubleToString rounds");
}

/// A supplied slot whose switch answers a per-field LITERAL rather than PREC
/// (`swaitRecord.c`'s `ODLY`, DBF_FLOAT — the `getFloatString` path).
#[test]
fn a_per_field_literal_reaches_the_string_form() {
    let mut sw = instance("swait");
    put(&mut sw, "PREC", EpicsValue::Short(0));
    put(&mut sw, "ODLY", EpicsValue::Float(1.5));
    assert_eq!(
        dbr_string_of(&sw, "ODLY"),
        "1.500",
        "the literal 3 wins over both PREC and the 6 seed"
    );
}

/// C's DBF gate does not sit on this path — `getDoubleString` calls
/// `get_precision` directly — but it cannot be reached either, because only a
/// float/double value takes the precision branch at all. An integer field of a
/// slot-supplying type renders through the plain conversion.
#[test]
fn an_integer_field_renders_without_precision() {
    let mut ai = instance("ai");
    put(&mut ai, "PREC", EpicsValue::Short(3));
    put(&mut ai, "RVAL", EpicsValue::Long(7));
    assert_eq!(dbr_string_of(&ai, "RVAL"), "7");
}
