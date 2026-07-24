//! Defect test: the `link_type` argument of a `device()` declaration is half of
//! that declaration, and the port used to throw it away.
//!
//! `tools/dbd-codegen` parsed `device(ai, INST_IO, devAiSoftTimestamp, "Soft
//! Timestamp")` and kept only the record type and the DTYP name. With the link
//! type gone, nothing could apply C's `dbCanSetLink` (`dbStaticLib.c:2400-2419`),
//! so a `.db` that hands an `INST_IO` device support a constant `INP` loaded
//! clean here while the C IOC refuses the link.
//!
//! GROUND TRUTH — the built C `softIoc` (7.0.10.1-DEV,
//! `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`) on the `.db` below,
//! printing one line per link it refuses to initialize:
//!
//! ```text
//! record(ai,"T:R1"){ field(DTYP,"Soft Timestamp") field(INP,"5")           }
//! record(ai,"T:R2"){ field(DTYP,"Soft Timestamp") field(INP,"OTHER.VAL")   }
//! record(ai,"T:R3"){ field(DTYP,"Soft Timestamp") field(INP,"")            }
//! record(ai,"T:R4"){ field(DTYP,"Soft Channel")   field(INP,"@instio parm")}
//! record(ai,"T:R5"){ field(DTYP,"Soft Timestamp") field(INP,"#C0 S0 @x")   }
//! record(ai,"T:N1"){                              field(INP,"@instio parm")}
//! record(ai,"T:N2"){ field(DTYP,"Soft Channel")   field(SDIS,"@instio parm")}
//! record(ai,"T:N3"){ field(DTYP,"Soft Channel")   field(FLNK,"@instio parm")}
//!
//! ERROR: T:R1.INP: can't initialize link type INST_IO with "5" (type CONSTANT)
//! ERROR: T:R2.INP: can't initialize link type INST_IO with "OTHER.VAL" (type PV_LINK)
//! ERROR: T:R3.INP: can't initialize link type INST_IO with "" (type CONSTANT)
//! ERROR: T:R4.INP: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! ERROR: T:R5.INP: can't initialize link type INST_IO with "#C0 S0 @x" (type VME_IO)
//! ERROR: T:NODTYP.INP: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! ERROR: T:SDIS.SDIS: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! ERROR: T:FLNK.FLNK: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! ```
//!
//! and, on the accepting side, NO error for any of:
//!
//! ```text
//! record(ai,"T:A1"){ field(DTYP,"Soft Timestamp") field(INP,"@instio parm")}
//! record(ai,"T:A2"){ field(DTYP,"Soft Channel")   field(INP,"5")           }
//! record(ai,"T:A3"){ field(DTYP,"Soft Channel")   field(INP,"OTHER.VAL")   }
//! record(ai,"T:A4"){ field(DTYP,"Soft Timestamp")                          }
//! ```
//!
//! DEVIATION from C (Tier 2, documented in the commit): C prints the ERROR and
//! runs the IOC anyway, leaving the record with a link that never reads or
//! writes. The port refuses the load — the dead link is the illegal state.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

/// Load one record and report whether the db-load accepted it.
async fn loads(body: &str) -> Result<(), String> {
    let db = format!("record(ai, \"AI:L\") {{\n{body}\n}}\n");
    let built = IocBuilder::new()
        .db_string(&db, &HashMap::new())
        .map_err(|e| e.to_string())?
        .build()
        .await;
    match built {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// The boundary is (declared link type of the bound device) × (link type the
/// `.db` text parses to). One case per cell C rejects — the four ways to give an
/// `INST_IO` device something that is not `@…`, and the one way to give a
/// `CONSTANT` device an `@…`.
#[epics_macros_rs::epics_test]
async fn a_link_the_bound_device_cannot_take_is_refused_exactly_where_c_refuses_it() {
    // "Soft Timestamp" is `device(ai, INST_IO, ...)`; "Soft Channel" is
    // `device(ai, CONSTANT, ...)` — aiRecord's own `.dbd`, not a guess.
    let rejected = [
        ("Soft Timestamp", "5"),          // CONSTANT
        ("Soft Timestamp", "OTHER.VAL"),  // PV_LINK
        ("Soft Timestamp", ""),           // CONSTANT (the empty text IS a link)
        ("Soft Timestamp", "#C0 S0 @x"),  // VME_IO
        ("Soft Channel", "@instio parm"), // INST_IO
    ];
    for (dtyp, inp) in rejected {
        let body = format!("    field(DTYP, \"{dtyp}\")\n    field(INP, \"{inp}\")");
        let err = loads(&body).await.expect_err(&format!(
            "DTYP={dtyp:?} INP={inp:?}: the C IOC refuses this link"
        ));
        assert!(
            err.contains("can't initialize link type"),
            "DTYP={dtyp:?} INP={inp:?}: rejected, but not by the link-type gate: {err}"
        );
    }
}

/// The accepting side of the same boundary: an exact match, and the
/// `{CONSTANT, PV_LINK, JSON_LINK}` set C treats as interchangeable
/// (`dbStaticLib.c:2408-2416`) — a soft device takes any of the three.
#[epics_macros_rs::epics_test]
async fn the_links_c_accepts_still_load() {
    let accepted = [
        ("Soft Timestamp", "@instio parm"), // INST_IO on INST_IO
        ("Soft Channel", "5"),              // CONSTANT on CONSTANT
        ("Soft Channel", "OTHER.VAL"),      // PV_LINK on CONSTANT
        ("Soft Channel", "{const:1.5}"),    // JSON_LINK on CONSTANT
    ];
    for (dtyp, inp) in accepted {
        let body = format!("    field(DTYP, \"{dtyp}\")\n    field(INP, \"{inp}\")");
        loads(&body)
            .await
            .unwrap_or_else(|e| panic!("DTYP={dtyp:?} INP={inp:?}: the C IOC loads this: {e}"));
    }
}

/// The other boundary in the rule: WHICH field. Only `INP`/`OUT` are device
/// links (C `dbLexRoutines.c:571`); every other link field has no device support
/// and so is expected to be `CONSTANT`, whatever the record's DTYP is. C refuses
/// `field(SDIS,"@instio parm")` and `field(FLNK,"@instio parm")` for that reason
/// — measured, not reasoned.
#[epics_macros_rs::epics_test]
async fn a_non_device_link_field_is_held_to_constant_whatever_the_dtyp_is() {
    for field in ["SDIS", "FLNK"] {
        let body =
            format!("    field(DTYP, \"Soft Channel\")\n    field({field}, \"@instio parm\")");
        let err = loads(&body).await.expect_err(&format!(
            "{field}: a hardware link on a non-device link field is refused by C"
        ));
        assert!(
            err.contains("CONSTANT"),
            "{field}: must be held to CONSTANT, got: {err}"
        );
    }
}

/// DTYP is a `DBF_DEVICE` menu index and its default is 0, so a record that
/// never spells DTYP is still bound to the FIRST device its `.dbd` declares
/// (`ai` → "Soft Channel", CONSTANT). C rejects a hardware `INP` there, and so
/// must the port: "no DTYP" is not "no device".
#[epics_macros_rs::epics_test]
async fn an_unspelled_dtyp_still_binds_the_first_declared_device() {
    let err = loads("    field(INP, \"@instio parm\")")
        .await
        .expect_err("no DTYP means DTYP index 0 — CONSTANT on ai — and C refuses @instio there");
    assert!(err.contains("CONSTANT"), "{err}");
}

/// The near-miss C also lets through: a link the `.db` never assigns is never
/// checked (`if (!plink->text) continue;`, `dbStaticLib.c:2213`). An `INST_IO`
/// DTYP with no `INP` line at all loads, in C and here — the check is on the
/// text the `.db` supplied, not on the record's declaration.
#[epics_macros_rs::epics_test]
async fn a_link_the_db_never_assigns_is_not_checked() {
    loads("    field(DTYP, \"Soft Timestamp\")")
        .await
        .expect("DTYP alone assigns no link text, so there is nothing to check");
}

/// C applies the SAME rule to a link written at runtime — `dbPutFieldLink`
/// calls `dbCanSetLink` too (`dbAccess.c:1133`) — so the port's gate must live
/// where both paths reach it, not in the `.db` loader alone.
///
/// MEASURED on the C softIoc: with `record(ai,"T:RT"){ field(DTYP,"Soft
/// Timestamp") field(INP,"@instio parm") }` loaded,
///
/// ```text
/// dbpf T:RT.INP "5"
/// DBF_STRING:  "@instio parm"     <- the put is refused, the link is unchanged
/// ```
#[epics_macros_rs::epics_test]
async fn a_runtime_put_of_a_link_the_device_cannot_take_is_refused_too() {
    use epics_base_rs::types::EpicsValue;

    let db = r#"record(ai, "AI:RT") { field(DTYP, "Soft Timestamp") field(INP, "@instio parm") }"#;
    let (database, _) = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let rec = database.get_record("AI:RT").unwrap();
    let mut inst = rec.write();
    let err = inst
        .put_common_field("INP", EpicsValue::String("5".into()))
        .expect_err("an INST_IO device support cannot take a constant INP, at load OR at runtime");
    assert!(
        err.to_string().contains("can't initialize link type"),
        "{err}"
    );
    assert_eq!(
        inst.common.inp, "@instio parm",
        "a refused put must leave the link exactly as it was — C's does"
    );
}

/// C checks links once, at `iocInit`, over the record as loaded
/// (`dbStaticLib.c:2178-2231`) — not as each field is parsed. So a `.db` may
/// spell `INP` before `DTYP` and still bind, and this port must not make the
/// verdict depend on the order the `.db` happened to use. MEASURED: C loads
/// `record(ai,"T:ORD"){ field(INP,"@instio parm") field(DTYP,"Soft Timestamp") }`
/// with no error.
#[epics_macros_rs::epics_test]
async fn the_verdict_does_not_depend_on_the_order_the_db_spells_the_fields() {
    loads("    field(INP, \"@instio parm\")\n    field(DTYP, \"Soft Timestamp\")")
        .await
        .expect("C checks the link against the loaded record's DTYP, whatever order it was set in");

    loads("    field(INP, \"5\")\n    field(DTYP, \"Soft Timestamp\")")
        .await
        .expect_err("and it still catches the mismatch when the order is reversed");
}

/// The declaration is the `.dbd`'s, and where there is no `.dbd` there is no
/// declaration to enforce. A DTYP registered at runtime by a downstream crate
/// (`asynInt32` on an `ai`) appears in no vendored `device()` line, so the port
/// has no `link_type` for it and invents none — it must not reject the `.db` on
/// a rule it cannot state.
#[epics_macros_rs::epics_test]
async fn a_dtyp_no_vendored_dbd_declares_carries_no_rule() {
    loads("    field(DTYP, \"asynInt32\")\n    field(INP, \"@asyn(PORT,0)VALUE\")")
        .await
        .expect("no vendored device() declares asynInt32, so there is no link type to enforce");
}
