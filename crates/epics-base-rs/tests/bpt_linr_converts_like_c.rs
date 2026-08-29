//! Defect test: an `ai` whose `LINR` names a breakpoint table must convert to
//! the value the C IOC lands on — and must NOT convert until that curve's
//! `.dbd` has been loaded, because C's `bptList` starts empty and every curve
//! is a separate opt-in.
//!
//! GROUND TRUTH — the built C `softIoc` (7.0.10.1-DEV,
//! `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`), with the vendored
//! table loaded the way a C IOC loads it:
//!
//! ```text
//! dbLoadDatabase("$(EPICS_BASE)/dbd/bptTypeKdegC.dbd")
//! record(ai, "T:A") { field(DTYP,"Raw Soft Channel") field(INP,"1702") field(LINR,"typeKdegC") }
//! record(ai, "T:B") { field(DTYP,"Raw Soft Channel") field(INP,"500")  field(LINR,"typeKdegC") }
//! record(ai, "T:C") { field(DTYP,"Raw Soft Channel") field(INP,"4098") field(LINR,"typeKdegC") }
//! record(ai, "T:D") { field(DTYP,"Raw Soft Channel") field(INP,"500")  field(LINR,"typeJdegC") }
//!
//! dbgf T:A.VAL   DBF_DOUBLE:  417.918353467
//! dbgf T:B.VAL   DBF_DOUBLE:  123.421505587
//! dbgf T:C.VAL   DBF_DOUBLE:  1000.77617954
//! dbgf T:D.VAL   DBF_DOUBLE:  90.5934958955
//! dbgf T:A.STAT  DBF_STRING:  "NO_ALARM"
//! ```
//!
//! and, with the `dbLoadDatabase` line removed — an arm this port could not
//! reach until `dbLoadDatabase` existed, because the registry seeded itself:
//!
//! ```text
//! dbgf T:A.VAL   DBF_DOUBLE:  1702        <- RVAL, unconverted
//! dbgf T:A.STAT  DBF_STRING:  "SOFT"
//! ```

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status::SOFT_ALARM;
use epics_base_rs::types::EpicsValue;

/// The C values above are `dbgf`'s 12-significant-digit rendering. A wrong (or
/// absent) table moves the result by whole engineering units, so this tolerance
/// is far tighter than any defect it must catch and far looser than the printed
/// precision.
const TOL: f64 = 1e-6;

/// One of the four curve files this crate ships, the file C names in
/// `dbLoadDatabase("$(EPICS_BASE)/dbd/bpt<curve>.dbd")`.
fn shipped_bpt(curve: &str) -> String {
    // EPICS names the file `bptTypeKdegC.dbd` for the curve `typeKdegC`.
    let mut chars = curve.chars();
    let capitalised = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => String::new(),
    };
    format!("{}/dbd/bpt{capitalised}.dbd", env!("CARGO_MANIFEST_DIR"))
}

/// Build a one-record IOC, load `curve`'s shipped `.dbd` first when `opt_in`,
/// then push `rval` through `convert`. Returns `VAL` and `STAT`.
async fn eng_value(linr: &str, rval: i32, opt_in: bool) -> (f64, u16) {
    let db_content = format!(
        r#"
record(ai, "AI:BPT") {{
    field(DTYP, "Raw Soft Channel")
    field(INP,  "0")
    field(LINR, "{linr}")
}}
"#
    );
    let mut builder = IocBuilder::new();
    if opt_in {
        builder = builder
            .db_file(&shipped_bpt(linr), &HashMap::new())
            .unwrap();
    }
    let (db, _) = builder
        .db_string(&db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    // C's oracle drove this with `dbpf("R:LIN.RVAL","150")`, i.e. `dbPutField`
    // — which processes. Going through the record handle instead leaves the
    // record UDF, and the UDF alarm outranks the SOFT one `convert` raises.
    db.put_record_field_from_ca_no_notify("AI:BPT", "RVAL", EpicsValue::Long(rval))
        .await
        .unwrap();

    let rec = db.get_record("AI:BPT").unwrap();
    let inst = rec.read();
    let val = match inst.record.get_field("VAL") {
        Some(EpicsValue::Double(v)) => v,
        other => panic!("AI:BPT.VAL must be a Double, got {other:?}"),
    };
    (val, inst.common.stat)
}

/// The boundary this walks is one point per segment class of the C curve: a
/// value inside the first fitted segment, one deep in the middle, one past the
/// last breakpoint (C extrapolates on the final slope rather than clamping), and
/// one on a second table so a single hard-coded table cannot pass.
#[epics_macros_rs::epics_test]
async fn a_standard_linr_converts_to_the_value_the_c_ioc_produces() {
    // (LINR, RVAL, VAL measured on the C softIoc)
    let cases: [(&str, i32, f64); 4] = [
        ("typeKdegC", 1702, 417.918353467),
        ("typeKdegC", 500, 123.421505587),
        ("typeKdegC", 4098, 1000.77617954),
        ("typeJdegC", 500, 90.5934958955),
    ];

    for (linr, rval, want) in cases {
        let (got, stat) = eng_value(linr, rval, true).await;
        assert!(
            (got - want).abs() < TOL,
            "LINR={linr} RVAL={rval}: the C softIoc converts to {want}, the port gives {got}"
        );
        assert_eq!(stat, 0, "LINR={linr} RVAL={rval}: C leaves STAT NO_ALARM");
    }
}

/// The other side of the same boundary, and the one the port could not reach
/// while `BreakTableRegistry::new` seeded itself: the SAME record with the
/// SAME `LINR`, built without the `dbLoadDatabase` line, keeps `VAL == RVAL`
/// and raises `STAT=SOFT`. C prints `BPT Error` in `AMSG` for it.
#[epics_macros_rs::epics_test]
async fn a_curve_whose_dbd_was_not_loaded_leaves_val_raw_and_raises_soft() {
    for (linr, rval) in [("typeKdegC", 1702), ("typeJdegC", 500)] {
        let (got, stat) = eng_value(linr, rval, false).await;
        assert!(
            (got - f64::from(rval)).abs() < TOL,
            "LINR={linr}: with no table loaded C leaves VAL at RVAL {rval}, the port gives {got}"
        );
        assert_eq!(
            stat, SOFT_ALARM,
            "LINR={linr}: a missing table is C's SOFT alarm, not a silent pass"
        );
    }
}

/// A `menuConvert` name reserves its `LINR` index whether or not any data is
/// loaded — C's menu is static — and holds no table until one is. That is now
/// true of all twelve names alike, `typeKdegC` included; the four EPICS ships
/// data for are not special until their file is read.
#[epics_macros_rs::epics_test]
async fn every_menu_convert_name_reserves_its_index_and_starts_with_no_table() {
    use epics_base_rs::server::cvt_bpt::BreakTableRegistry;

    let registry = BreakTableRegistry::new();
    assert!(
        registry.is_empty(),
        "C `bptList` starts empty, so a fresh registry must too"
    );

    for name in [
        "typeKdegF",
        "typeKdegC",
        "typeJdegF",
        "typeJdegC",
        "typeEdegF(ixe only)",
        "typeEdegC(ixe only)",
        "typeTdegF",
        "typeTdegC",
        "typeRdegF",
        "typeRdegC",
        "typeSdegF",
        "typeSdegC",
    ] {
        assert!(
            registry.get(name).is_none(),
            "{name}: no curve has data before its .dbd is loaded"
        );
        assert!(
            registry.linr_index_of(name).is_some(),
            "{name} is a menuConvert name: its LINR index is reserved even with no data"
        );
    }
}

/// Nothing may put tables back by construction — `Default` reaches the same
/// empty registry `new` does, so a caller cannot get a pre-seeded one by
/// picking the other constructor.
#[epics_macros_rs::epics_test]
async fn a_defaulted_registry_is_the_same_empty_registry() {
    let defaulted: epics_base_rs::server::cvt_bpt::BreakTableRegistry = Default::default();
    assert!(defaulted.is_empty());
}

/// `bpt_generated::BREAK_TABLES` is the compiled form of the four shipped
/// files; this is what keeps the generator's output and the files it was
/// generated from from drifting apart now that nothing else reads the
/// constant at runtime.
#[epics_macros_rs::epics_test]
async fn shipped_bpt_files_match_the_generated_tables() {
    use epics_base_rs::server::record::bpt_generated::BREAK_TABLES;

    assert_eq!(BREAK_TABLES.len(), 4, "EPICS ships four bpt*.dbd curves");
    for (name, points) in BREAK_TABLES {
        let text = std::fs::read_to_string(shipped_bpt(name))
            .unwrap_or_else(|e| panic!("dbd/bpt{name}.dbd must ship with the crate: {e}"));
        let parsed =
            epics_base_rs::server::db_loader::parse_db_with_breaktables(&text, &HashMap::new())
                .unwrap_or_else(|e| panic!("dbd/bpt{name}.dbd must parse: {e}"));
        let table = parsed
            .breaktables
            .iter()
            .find(|t| t.name == *name)
            .unwrap_or_else(|| panic!("dbd/bpt{name}.dbd must declare breaktable({name})"));
        let from_file: Vec<(f64, f64)> = table.points.iter().map(|p| (p.raw, p.eng)).collect();
        assert_eq!(from_file.as_slice(), *points, "bpt{name}.dbd drifted");
    }
}
