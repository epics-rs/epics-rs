//! Every record-own field starts at the value its `.dbd` declares.
//!
//! The `dbCommon` half of this rule is `tests/common_field_initials.rs`. This is
//! the other ~2,200 fields: the generated per-record tables carry each field's
//! `initial()`, so a default the port picked by hand instead of reading off the
//! declaration is checkable for all record types at once rather than one
//! oracle-reported field at a time.
//!
//! C applies the initial in `dbStaticLib` at load: the record is calloc'd, then
//! every field with an `initial()` is put through the same converter a `.db`
//! `field()` line takes. A field with no `initial()` therefore starts ZERO — for
//! a `DBF_MENU` that means index 0, its FIRST choice, which is why a C
//! `record(waveform,"X"){}` serves `FTVL = STRING`.
//!
//! The exception is a field the record's own `init_record` COMPUTES before any
//! client can read it. Those are not declaration defaults at all, and the port
//! must reproduce the computed value rather than the zero. Each is listed in
//! `INIT_COMPUTED` with the C line that writes it — and, for the record types the
//! base softIoc has, the measurement off the compiled IOC.

use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields};
use epics_base_rs::types::EpicsValue;

mod module_records;

/// `(record, field)` pairs whose construction value is written by the record's
/// `init_record`, not by the `.dbd`'s `initial()`. Anything NOT listed here must
/// equal its declaration.
const INIT_COMPUTED: &[(&str, &str)] = &[
    // Link-status menus, classified from the link type at init. Measured on the
    // compiled softIoc, `record(calcout,"P:CO"){}`:
    //   caget -t P:CO.OUTV -> Constant   (menu calcoutINAV index 3)
    //   caget -t P:CO.INAV -> Constant
    //
    // C `init_record` classifies EVERY input/array/output link and stores
    // `<rec>INAV_CON`(3) for a CONSTANT link (`aCalcoutRecord.c:209-243`,
    // `sCalcoutRecord.c`, `transformRecord.c:444-473`), OVERWRITING the `.dbd`
    // `initial("1")`. A default record's links are all constant, so every one of
    // these reads `Constant`(3) — not the declared 1. The acalcout/scalcout/
    // transform records serve the fields read-only, hold the init-derived value
    // as their `Default`, and decline loader-seed ownership (`implements_field`)
    // so the raw `.dbd` initial is not written over it.
    //
    // `calcout` shares the C semantics; its OUTV (no `.dbd` initial to seed)
    // already reads the modeled `Constant`, so it is listed. Its INPUT-valid
    // fields (`INAV..INUV`, each `initial("1")`) are still seeded raw by the port
    // — a separate defect, not modeled here — so they read the declared 1 and
    // stay off this list.
    ("calcout", "OUTV"),
    ("acalcout", "INAV"),
    ("acalcout", "INBV"),
    ("acalcout", "INCV"),
    ("acalcout", "INDV"),
    ("acalcout", "INEV"),
    ("acalcout", "INFV"),
    ("acalcout", "INGV"),
    ("acalcout", "INHV"),
    ("acalcout", "INIV"),
    ("acalcout", "INJV"),
    ("acalcout", "INKV"),
    ("acalcout", "INLV"),
    ("acalcout", "IAAV"),
    ("acalcout", "IBBV"),
    ("acalcout", "ICCV"),
    ("acalcout", "IDDV"),
    ("acalcout", "IEEV"),
    ("acalcout", "IFFV"),
    ("acalcout", "IGGV"),
    ("acalcout", "IHHV"),
    ("acalcout", "IIIV"),
    ("acalcout", "IJJV"),
    ("acalcout", "IKKV"),
    ("acalcout", "ILLV"),
    ("acalcout", "OUTV"),
    ("scalcout", "INAV"),
    ("scalcout", "INBV"),
    ("scalcout", "INCV"),
    ("scalcout", "INDV"),
    ("scalcout", "INEV"),
    ("scalcout", "INFV"),
    ("scalcout", "INGV"),
    ("scalcout", "INHV"),
    ("scalcout", "INIV"),
    ("scalcout", "INJV"),
    ("scalcout", "INKV"),
    ("scalcout", "INLV"),
    ("scalcout", "IAAV"),
    ("scalcout", "IBBV"),
    ("scalcout", "ICCV"),
    ("scalcout", "IDDV"),
    ("scalcout", "IEEV"),
    ("scalcout", "IFFV"),
    ("scalcout", "IGGV"),
    ("scalcout", "IHHV"),
    ("scalcout", "IIIV"),
    ("scalcout", "IJJV"),
    ("scalcout", "IKKV"),
    ("scalcout", "ILLV"),
    ("scalcout", "OUTV"),
    ("transform", "IAV"),
    ("transform", "IBV"),
    ("transform", "ICV"),
    ("transform", "IDV"),
    ("transform", "IEV"),
    ("transform", "IFV"),
    ("transform", "IGV"),
    ("transform", "IHV"),
    ("transform", "IIV"),
    ("transform", "IJV"),
    ("transform", "IKV"),
    ("transform", "ILV"),
    ("transform", "IMV"),
    ("transform", "INV"),
    ("transform", "IOV"),
    ("transform", "IPV"),
    ("transform", "OAV"),
    ("transform", "OBV"),
    ("transform", "OCV"),
    ("transform", "ODV"),
    ("transform", "OEV"),
    ("transform", "OFV"),
    ("transform", "OGV"),
    ("transform", "OHV"),
    ("transform", "OIV"),
    ("transform", "OJV"),
    ("transform", "OKV"),
    ("transform", "OLV"),
    ("transform", "OMV"),
    ("transform", "ONV"),
    ("transform", "OOV"),
    ("transform", "OPV"),
    // swait: an empty INAN is `NO_PV` (`swaitRecord.c:346`, `*pPvStat = NO_PV`),
    // menu swaitINAV index 2 ("No PV") — not the zero ("PV OK").
    ("swait", "INAV"),
    ("swait", "INBV"),
    ("swait", "INCV"),
    ("swait", "INDV"),
    ("swait", "INEV"),
    ("swait", "INFV"),
    ("swait", "INGV"),
    ("swait", "INHV"),
    ("swait", "INIV"),
    ("swait", "INJV"),
    ("swait", "INKV"),
    ("swait", "INLV"),
    ("swait", "DOLV"),
    ("swait", "OUTV"),
    // sseq: `init_record` classifies each step's DOL/LNK link
    // (`sseqRecord.c:203-239`), so DTn/LTn hold a DBF type code, not a zero.
    ("sseq", "DT1"),
    ("sseq", "DT2"),
    ("sseq", "DT3"),
    ("sseq", "DT4"),
    ("sseq", "DT5"),
    ("sseq", "DT6"),
    ("sseq", "DT7"),
    ("sseq", "DT8"),
    ("sseq", "DT9"),
    ("sseq", "DTA"),
    ("sseq", "LT1"),
    ("sseq", "LT2"),
    ("sseq", "LT3"),
    ("sseq", "LT4"),
    ("sseq", "LT5"),
    ("sseq", "LT6"),
    ("sseq", "LT7"),
    ("sseq", "LT8"),
    ("sseq", "LT9"),
    ("sseq", "LTA"),
    // `sel`: `init_record` leaves every unconnected input at NaN, not at 0.
    // Measured: `record(sel,"P:SEL"){}` -> `caget -t P:SEL.A` = `nan`.
    ("sel", "A"),
    ("sel", "B"),
    ("sel", "C"),
    ("sel", "D"),
    ("sel", "E"),
    ("sel", "F"),
    ("sel", "G"),
    ("sel", "H"),
    ("sel", "I"),
    ("sel", "J"),
    ("sel", "K"),
    ("sel", "L"),
    // `acalcout.VERS` is the code version — `pcalc->vers = VERSION`
    // (`aCalcoutRecord.c:186`, `#define VERSION 1.4`) OVERWRITES the `.dbd`'s
    // `initial("1")` at init.
    ("acalcout", "VERS"),
    // Same for the sibling calc records: `pcalc->vers = VERSION` overwrites the
    // `.dbd` `initial("1")` at init — scalcout `#define VERSION 4.1`
    // (`sCalcoutRecord.c:55`), transform `#define VERSION 5.8`
    // (`transformRecord.c:92`).
    ("scalcout", "VERS"),
    ("transform", "VERS"),
    // `asyn.CNCT` is the port connection state, driven by the record's connection
    // management rather than by a declaration default.
    ("asyn", "CNCT"),
    // `asyn.TFIL` (trace I/O file) is seeded by `init_record` pass 0
    // (`asynRecord.c`: `strcpy(prec->tfil, "Unknown")`); `asynRecord.dbd`
    // declares no `initial(...)` for it, so the value comes from record code.
    ("asyn", "TFIL"),
];

fn is_init_computed(record_type: &str, field: &str) -> bool {
    INIT_COMPUTED
        .iter()
        .any(|(r, f)| *r == record_type && *f == field)
}

fn instance(record_type: &str) -> Option<RecordInstance> {
    // Through the module-record fixture, so the seven types outside
    // `stdRecords.dbd` are constructed rather than skipped. A type that still
    // fails to construct is a broken factory, not an unmet prerequisite, so it
    // panics: the `.ok()?` this replaces dropped seven record types from the
    // sweep and still reported green.
    let rec = module_records::create_any(record_type)
        .unwrap_or_else(|e| panic!("{record_type}: create_record failed: {e}"));
    Some(RecordInstance::new_boxed(format!("T:{record_type}"), rec))
}

/// The field's value as a client reads it, textually.
fn value_text(inst: &RecordInstance, field: &str) -> Option<String> {
    let v = inst.record.get_field(field)?;
    Some(match v {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::Char(n) => n.to_string(),
        EpicsValue::UChar(n) => n.to_string(),
        EpicsValue::Short(n) => n.to_string(),
        EpicsValue::UShort(n) => n.to_string(),
        EpicsValue::Enum(n) => n.to_string(),
        EpicsValue::Long(n) => n.to_string(),
        EpicsValue::ULong(n) => n.to_string(),
        EpicsValue::Int64(n) => n.to_string(),
        EpicsValue::UInt64(n) => n.to_string(),
        EpicsValue::Float(n) => n.to_string(),
        EpicsValue::Double(n) => n.to_string(),
        _ => return None, // arrays: VAL and friends, no scalar initial to check
    })
}

/// Does the port's value equal the declared initial, read the way a `.db` load
/// would read it — numerically for a number, through the choice list for a menu?
fn matches(inst: &RecordInstance, field: &str, initial: &str, got: &str) -> bool {
    if got == initial {
        return true;
    }
    if let (Ok(a), Ok(b)) = (got.parse::<f64>(), initial.parse::<f64>()) {
        return a == b;
    }
    inst.snapshot_for_field(field)
        .and_then(|s| s.enums)
        .is_some_and(|e| {
            e.strings
                .iter()
                .position(|c| c.as_str_lossy() == initial)
                .is_some_and(|i| got == i.to_string())
        })
}

#[test]
fn r21_every_record_field_starts_at_its_declared_initial() {
    let mut bad = Vec::new();
    for rt in RECORD_TYPES {
        let Some(inst) = instance(rt) else { continue };
        for f in record_fields(rt).unwrap_or(&[]) {
            let Some(initial) = f.initial else { continue };
            if is_init_computed(rt, f.name) {
                continue;
            }
            let Some(got) = value_text(&inst, f.name) else {
                continue;
            };
            if !matches(&inst, f.name, initial, &got) {
                bad.push(format!(
                    "{rt}.{}: declared {initial:?}, port {got:?}",
                    f.name
                ));
            }
        }
    }
    assert!(bad.is_empty(), "record defaults off the .dbd:\n{bad:#?}");
}

/// A field with no `initial()` starts at zero — C callocs the record and the
/// `initial` pass skips it. `ao.ASLO` was the live case: the port started it at
/// 1.0 ("a unit slope"), C starts it at 0.0 and guards every use with
/// `if (prec->aslo != 0.0)`, so 0.0 means "no ASLO scaling". `dfanout.MLST`/`ALST`
/// were the other: NaN, the port's "nothing posted yet" sentinel, served to
/// clients as `nan` where C serves `0`.
#[test]
fn r21_a_record_field_with_no_declared_initial_starts_zero() {
    let mut bad = Vec::new();
    for rt in RECORD_TYPES {
        let Some(inst) = instance(rt) else { continue };
        for f in record_fields(rt).unwrap_or(&[]) {
            if f.initial.is_some() || is_init_computed(rt, f.name) {
                continue;
            }
            let Some(got) = value_text(&inst, f.name) else {
                continue;
            };
            if !(got.is_empty() || got.parse::<f64>() == Ok(0.0)) {
                bad.push(format!(
                    "{rt}.{}: no declared initial, port {got:?}",
                    f.name
                ));
            }
        }
    }
    assert!(bad.is_empty(), "record defaults off the .dbd:\n{bad:#?}");
}

/// The exclusion list is not a place to park a defect: every entry must name a
/// field its record type actually declares, so a renamed or dropped field cannot
/// leave a silent exemption behind.
#[test]
fn r21_every_init_computed_exclusion_names_a_real_field() {
    for (rt, field) in INIT_COMPUTED {
        let fields = record_fields(rt).unwrap_or_else(|| panic!("{rt}: not a record type"));
        assert!(
            fields.iter().any(|f| f.name == *field),
            "{rt}.{field}: exclusion names a field the .dbd does not declare"
        );
    }
}
