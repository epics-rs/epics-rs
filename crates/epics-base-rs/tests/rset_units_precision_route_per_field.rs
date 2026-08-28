//! `get_units` / `get_precision` are per-field rset slots, and every type that
//! supplies them serves its own EGU/PREC — not only the handful the metadata
//! cache used to name.
//!
//! The two leaves were built by `populate_display_info`'s `match rtype`, so a
//! type the match missed served `""` and `0` on every field, VAL included,
//! while a type it named served VAL's EGU/PREC on every field. C asks per
//! field, in a switch whose default writes nothing:
//!
//! ```text
//! subArrayRecord.c:202-215 VAL/HOPR/LOPR -> EGU   :217-225 -> PREC
//! selRecord.c:136-144      DBF_DOUBLE    -> EGU   :146-165 -> PREC
//! subRecord.c:206-219      DBF_DOUBLE    -> EGU   :221-240 -> PREC
//! dfanoutRecord.c:170-179  DBF_DOUBLE    -> EGU   :181-188 -> PREC
//! sCalcoutRecord.c:607     (no gate)     -> EGU   :616     -> PREC
//! aCalcoutRecord.c:747     (no gate)     -> EGU   :756     -> PREC
//! ```
//!
//! Boundaries: a listed field of a type the cache missed, a field the switch
//! excludes on a type the cache named, the DBF gate each switch opens with,
//! `subArray`'s FTVL escape, and a type whose precision switch never seeds
//! from PREC at all.

mod module_records;

use epics_base_rs::server::database::LinkBacking;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::EpicsValue;

/// `(units, precision)` for one field of a record carrying EGU="mm", PREC=3.
fn served(rtype: &str, field: &str) -> (String, i16) {
    served_with(rtype, field, |_| {})
}

/// The dispatch order the runtime uses: the record's own store first, the
/// declared-override tail second. `sel`/`sub`/`dfanout` DECLARE EGU/PREC but
/// model no storage for them, so only the second lands — and `resolve_field`,
/// which the routing reads back through, spans both.
fn put(inst: &mut RecordInstance, name: &str, value: EpicsValue) {
    if inst.record.put_field(name, value.clone()).is_ok() {
        return;
    }
    match inst.put_common_field(name, value) {
        // A type that does not declare the field at all — `busy` has no EGU,
        // `sel` no FTVL. Seeding is best-effort; the assertions below say what
        // each type must then serve.
        Ok(_) | Err(epics_base_rs::error::CaError::FieldNotFound(_)) => {}
        Err(e) => panic!("{name}: {e:?}"),
    }
}

fn served_with(rtype: &str, field: &str, setup: impl FnOnce(&mut RecordInstance)) -> (String, i16) {
    let rec = module_records::create_any(rtype).unwrap_or_else(|e| panic!("{rtype}: {e:?}"));
    let mut inst = RecordInstance::new_boxed(format!("T:{rtype}"), rec);
    put(&mut inst, "EGU", EpicsValue::String("mm".into()));
    put(&mut inst, "PREC", EpicsValue::Short(3));
    setup(&mut inst);
    let d = inst
        .snapshot_for_field_with(field, LinkBacking::none())
        .unwrap_or_else(|| panic!("{rtype}.{field}: no snapshot"))
        .display
        .unwrap_or_else(|| panic!("{rtype}.{field}: no display block"));
    (d.units.to_string(), d.precision)
}

/// The lead's trigger and its siblings: four types whose rsets supply both
/// slots and which the record-level cache never named.
#[test]
fn the_types_the_cache_missed_serve_their_own_egu_and_prec() {
    for rtype in ["subArray", "sel", "sub", "dfanout", "scalcout", "acalcout"] {
        let got = served_with(rtype, "VAL", |inst| {
            // subArray's VAL is only DBF_DOUBLE once FTVL says so; the others
            // ignore the put.
            put(inst, "FTVL", EpicsValue::Short(10));
        });
        assert_eq!(
            got,
            ("mm".to_string(), 3),
            "{rtype}.VAL must serve the record's own EGU/PREC"
        );
    }
}

/// The DBF gate each C switch opens with: `sel`/`dfanout` write EGU only for a
/// DBF_DOUBLE field, so an integer or menu field of the same record gets none.
#[test]
fn a_field_outside_the_types_dbf_gate_gets_no_units() {
    // sel.SELN is DBF_USHORT, dfanout.SELN likewise — outside `DBF_DOUBLE`.
    for rtype in ["sel", "dfanout"] {
        let (units, _) = served(rtype, "SELN");
        assert_eq!(
            units, "",
            "{rtype}.SELN is not DBF_DOUBLE, so C writes nothing"
        );
    }
    // sel.A IS DBF_DOUBLE and sel has no link arm — selRecord.c:136-144 is a
    // bare type test, so the argument fields DO take EGU.
    assert_eq!(
        served("sel", "A").0,
        "mm",
        "sel.A is DBF_DOUBLE with no link arm"
    );
}

/// `ai` was always in the cache, and therefore served VAL's EGU on every field.
/// C excludes the raw-conversion coefficients by name (`aiRecord.c:217-232`).
#[test]
fn ais_conversion_coefficients_carry_no_units() {
    for field in ["ASLO", "AOFF", "SMOO"] {
        assert_eq!(
            served("ai", field).0,
            "",
            "ai.{field} is a conversion coefficient — C names it to skip EGU"
        );
    }
    assert_eq!(served("ai", "VAL").0, "mm");
    assert_eq!(served("ai", "HOPR").0, "mm");
}

/// `calc`'s `A`..`U` route through `dbGetUnits` on the backing link, and an
/// unset link is a constant that supplies nothing — so units stay empty while
/// precision still comes from the PREC seed (C's link arm skips
/// `recGblGetPrec` but leaves `*pprecision = prec->prec` standing).
#[test]
fn calcs_link_backed_arguments_take_the_links_units_not_egu() {
    assert_eq!(
        served("calc", "A"),
        (String::new(), 3),
        "calc.A: empty units from the constant link, PREC from the seed"
    );
    assert_eq!(served("calc", "VAL"), ("mm".to_string(), 3));
}

/// `subArray`'s VAL case falls through to HOPR/LOPR only when FTVL is neither
/// STRING nor ENUM (`subArrayRecord.c:208-212`).
#[test]
fn subarrays_val_units_follow_ftvl() {
    let string_ftvl = served_with("subArray", "VAL", |inst| {
        put(inst, "FTVL", EpicsValue::Short(0));
    });
    assert_eq!(
        string_ftvl.0, "",
        "FTVL=STRING breaks out of C's VAL case before the strncpy"
    );
    let double_ftvl = served_with("subArray", "VAL", |inst| {
        put(inst, "FTVL", EpicsValue::Short(10));
    });
    assert_eq!(
        double_ftvl.0, "mm",
        "FTVL=DOUBLE falls through to the strncpy"
    );
    // HOPR/LOPR take EGU whatever FTVL says — they are their own cases.
    assert_eq!(
        served_with("subArray", "HOPR", |inst| {
            put(inst, "FTVL", EpicsValue::Short(0));
        })
        .0,
        "mm"
    );
}

/// The precision seed is per type: `busy`'s switch never writes PREC, so its
/// only nonzero answer is the HIGH literal — a PREC put must not leak onto
/// another field.
#[test]
fn a_type_whose_switch_never_seeds_from_prec_keeps_zero() {
    assert_eq!(
        served("busy", "HIGH").1,
        2,
        "busyRecord.c:281 answers a literal, not PREC"
    );
    // `bo` shares that shape and additionally supplies get_units.
    assert_eq!(served("bo", "HIGH"), ("s".to_string(), 2));
}

/// `swait` supplies `get_precision` but not `get_units`
/// (`swaitRecord.c` rset), and its switch seeds every field from PREC — every
/// field but the one it names.
#[test]
fn swait_serves_prec_on_every_field_but_odly() {
    // PREC = 5, so the ODLY literal cannot be mistaken for the seed.
    let with_prec5 = |field: &str| {
        served_with("swait", field, |inst| {
            put(inst, "PREC", EpicsValue::Short(5));
        })
    };
    assert_eq!(with_prec5("VAL").1, 5, "swaitRecord.c seeds from PREC");
    assert_eq!(with_prec5("A").1, 5, "an input value takes the same seed");
    assert_eq!(
        with_prec5("ODLY").1,
        3,
        "the one named field answers a literal, not PREC"
    );
    assert_eq!(
        with_prec5("VAL").0,
        "",
        "swait's rset NULLs get_units, so no field carries EGU"
    );
}
