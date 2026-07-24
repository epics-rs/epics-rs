//! Defect test: an `ai` whose `LINR` names a standard breakpoint table must
//! convert, and must land on the same value the C IOC lands on.
//!
//! The port shipped the whole breakpoint machinery — `cvt_bpt.rs`, the
//! `menuConvert` `LINR` names, the `breaktable(...)` grammar in the `.db`
//! loader — and none of the four tables EPICS actually ships data for. A
//! `field(LINR,"typeKdegC")` therefore resolved to no table, and the record
//! handed VAL back unconverted.
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
//! and, with the `dbLoadDatabase` line removed — the arm the port was stuck in:
//!
//! ```text
//! dbgf T:A.VAL   DBF_DOUBLE:  1702        <- RVAL, unconverted
//! dbgf T:A.STAT  DBF_STRING:  "SOFT"
//! ```

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// The C values above are `dbgf`'s 12-significant-digit rendering. A wrong (or
/// absent) table moves the result by whole engineering units, so this tolerance
/// is far tighter than any defect it must catch and far looser than the printed
/// precision.
const TOL: f64 = 1e-6;

async fn eng_value(linr: &str, rval: i32) -> f64 {
    let db_content = format!(
        r#"
record(ai, "AI:BPT") {{
    field(LINR, "{linr}")
}}
"#
    );
    let (db, _) = IocBuilder::new()
        .db_string(&db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let rec = db.get_record("AI:BPT").unwrap();
    let mut inst = rec.write();
    inst.record
        .put_field("RVAL", EpicsValue::Long(rval))
        .unwrap();
    inst.record.process().unwrap();
    match inst.record.get_field("VAL") {
        Some(EpicsValue::Double(v)) => v,
        other => panic!("AI:BPT.VAL must be a Double, got {other:?}"),
    }
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
        let got = eng_value(linr, rval).await;
        assert!(
            (got - want).abs() < TOL,
            "LINR={linr} RVAL={rval}: the C softIoc converts to {want}, the port gives {got}"
        );
    }
}

/// The other side of the boundary: a `menuConvert` name EPICS ships NO
/// `bpt*.data` for still reserves its `LINR` index (the menu is static), but has
/// no table — and the conversion fails rather than inventing one, exactly as C's
/// `dbFindBrkTable` returning NULL does. Only `typeJ*`/`typeK*` have data, in C
/// and here.
#[epics_macros_rs::epics_test]
async fn a_menu_convert_name_with_no_shipped_data_still_has_no_table() {
    use epics_base_rs::server::cvt_bpt::BreakTableRegistry;

    let registry = BreakTableRegistry::new();

    for shipped in ["typeKdegF", "typeKdegC", "typeJdegF", "typeJdegC"] {
        assert!(
            registry.get(shipped).is_some(),
            "{shipped}: EPICS ships dbd/bpt{shipped}.dbd, so the port must have its data"
        );
        assert!(
            registry.linr_index_of(shipped).is_some(),
            "{shipped} is a menuConvert name and must have a fixed LINR index"
        );
    }

    for unshipped in [
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
            registry.get(unshipped).is_none(),
            "{unshipped}: EPICS ships no bpt data for this curve, so neither may the port"
        );
        // The menu choice exists regardless — C's menuConvert is static.
        assert!(
            registry.linr_index_of(unshipped).is_some(),
            "{unshipped} is a menuConvert name: its LINR index is reserved even with no data"
        );
    }
}

/// The vendored tables are seeded by construction, so no load path can produce a
/// registry that has forgotten them. This is what makes the fix structural
/// rather than a call someone must remember to make.
#[epics_macros_rs::epics_test]
async fn every_registry_holds_the_vendored_tables_from_construction() {
    assert!(!BreakTableRegistryProbe::fresh().is_empty());
    assert!(!BreakTableRegistryProbe::defaulted().is_empty());

    struct BreakTableRegistryProbe;
    impl BreakTableRegistryProbe {
        fn fresh() -> epics_base_rs::server::cvt_bpt::BreakTableRegistry {
            epics_base_rs::server::cvt_bpt::BreakTableRegistry::new()
        }
        fn defaulted() -> epics_base_rs::server::cvt_bpt::BreakTableRegistry {
            Default::default()
        }
    }
}
