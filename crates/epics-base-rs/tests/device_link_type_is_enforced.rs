//! The `link_type` argument of a `device()` declaration is half of that
//! declaration, and the port used to throw it away.
//!
//! `tools/dbd-codegen` parsed `device(ai, INST_IO, devAiSoftTimestamp, "Soft
//! Timestamp")` and kept only the record type and the DTYP name. With the link
//! type gone, nothing could apply C's `dbCanSetLink` (`dbStaticLib.c:2400-2419`),
//! so a `.db` that hands an `INST_IO` device support a constant `INP` was
//! accepted here in silence where the C IOC prints a refusal.
//!
//! GROUND TRUTH — the built C `softIoc` (7.0.10.1-DEV,
//! `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`) on the `.db` below.
//!
//! C does NOT run this rule at db-load. `dbPutString` on a link field parses
//! the text and stores it, and nothing more (`dbStaticLib.c:2626-2634`); the
//! `dbCanSetLink` branch beside it is the *after*-init path. The rule runs
//! once, at `iocInit`, from `dbInitRecordLinks` (`dbStaticLib.c:2222`), which
//! prints one line per refusal and then returns 0 unconditionally.
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
//! record(ai,"T:A1"){ field(DTYP,"Soft Timestamp") field(INP,"@instio parm")}
//! record(ai,"T:A2"){ field(DTYP,"Soft Channel")   field(INP,"5")           }
//!
//! dbLoadRecords    <- prints NOTHING
//! dbl              <- all ten names, before iocInit
//! iocInit
//! ERROR: T:R1.INP: can't initialize link type INST_IO with "5" (type CONSTANT)
//! ERROR: T:R2.INP: can't initialize link type INST_IO with "OTHER.VAL" (type PV_LINK)
//! ERROR: T:R3.INP: can't initialize link type INST_IO with "" (type CONSTANT)
//! ERROR: T:R4.INP: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! ERROR: T:R5.INP: can't initialize link type INST_IO with "#C0 S0 @x" (type VME_IO)
//! ERROR: T:N1.INP: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! ERROR: T:N2.SDIS: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! ERROR: T:N3.FLNK: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
//! iocRun: All initialization complete
//! dbl              <- the same ten names
//! dbpr T:R1 1      <- INP : INST_IO @
//! ```
//!
//! `T:A1` and `T:A2` draw no line: an exact match and the
//! `{CONSTANT, PV_LINK, JSON_LINK}` set C treats as interchangeable
//! (`dbStaticLib.c:2408-2416`). A refused link is not left untyped — it keeps
//! the type its device support declares, with an empty payload, because
//! `dbInitRecordLinks` types every link from that support before it looks at
//! the text (`:2185-2212`) and frees the text on every arm (`:2229`). Which is
//! also why `record(ai,"X"){field(DTYP,"Soft Timestamp")}`, with no `INP` line
//! at all, reads `INP : INST_IO @` rather than `CONSTANT`.
//!
//! The port ran the rule at db-load instead and failed the whole file — on
//! this fixture it kept 2 records where C keeps 10. It now runs it where C
//! does, in `PvDatabase::db_init_record_links`, so the tests below assert
//! agreement rather than a phase of their own.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::link_type_refusal;

const REC: &str = "AI:L";

/// Load one record and hand back the database C would be running.
async fn loads(body: &str) -> Result<Arc<PvDatabase>, String> {
    let db = format!("record(ai, \"{REC}\") {{\n{body}\n}}\n");
    IocBuilder::new()
        .db_string(&db, &HashMap::new())
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map(|(db, _)| db)
        .map_err(|e| e.to_string())
}

/// The link field's text after `iocInit` — C's `dbgf REC.FIELD`.
fn link_text(db: &PvDatabase, field: &str) -> String {
    let rec = db
        .get_record(REC)
        .unwrap_or_else(|| panic!("{REC} must survive a refused link, as it does in C"));
    let inst = rec.read();
    match field {
        "INP" => inst.common.text.inp.to_string(),
        "SDIS" => inst.common.text.sdis.to_string(),
        "FLNK" => inst.common.text.flnk.to_string(),
        other => panic!("no accessor for {other}"),
    }
}

/// C's refusal line for this record, or `None` if the link fits — the same
/// call `db_init_record_links` prints from, so the wording is asserted at its
/// owner rather than scraped off stderr.
fn refusal(dtyp: &str, field: &str, text: &str) -> Option<String> {
    link_type_refusal(REC, "ai", Some(dtyp), field, text)
}

/// The boundary is (declared link type of the bound device) × (link type the
/// `.db` text parses to). One case per cell C refuses — the four ways to give
/// an `INST_IO` device something that is not `@…`, and the one way to give a
/// `CONSTANT` device an `@…`.
///
/// Each is refused at `iocInit` with the record KEPT and its link left as the
/// empty link of the type its device support declares.
#[epics_macros_rs::epics_test]
async fn a_link_the_bound_device_cannot_take_is_refused() {
    // "Soft Timestamp" is `device(ai, INST_IO, ...)`; "Soft Channel" is
    // `device(ai, CONSTANT, ...)` — aiRecord's own `.dbd`, not a guess.
    let rejected = [
        ("Soft Timestamp", "5", "INST_IO", "CONSTANT", "@"),
        ("Soft Timestamp", "OTHER.VAL", "INST_IO", "PV_LINK", "@"),
        ("Soft Timestamp", "#C0 S0 @x", "INST_IO", "VME_IO", "@"),
        ("Soft Channel", "@instio parm", "CONSTANT", "INST_IO", ""),
    ];
    for (dtyp, inp, declared, parsed, left_as) in rejected {
        let body = format!("    field(DTYP, \"{dtyp}\")\n    field(INP, \"{inp}\")");
        let db = loads(&body)
            .await
            .unwrap_or_else(|e| panic!("DTYP={dtyp:?} INP={inp:?}: C loads this file: {e}"));
        assert_eq!(
            refusal(dtyp, "INP", inp).as_deref(),
            Some(
                format!(
                    "{REC}.INP: can't initialize link type {declared} with \"{inp}\" (type {parsed})"
                )
                .as_str()
            )
        );
        assert_eq!(
            link_text(&db, "INP"),
            left_as,
            "DTYP={dtyp:?} INP={inp:?}: a refused link keeps its declared type with no payload"
        );
    }
}

/// `field(INP,"")` and no `INP` line at all are the same empty string once the
/// record is loaded, and C tells them apart by `plink->text`: the explicit one
/// is checked and refused (`T:R3`), the absent one is skipped
/// (`if (!plink->text) continue;`, `dbStaticLib.c:2213`). Both end as the empty
/// `INST_IO` link, so the difference is visible only in the printed line —
/// which is exactly why the port has to record the empty assignment.
#[epics_macros_rs::epics_test]
async fn an_explicitly_empty_link_is_checked_where_an_absent_one_is_not() {
    let db = loads("    field(DTYP, \"Soft Timestamp\")\n    field(INP, \"\")")
        .await
        .expect("C loads it and prints the refusal at iocInit");
    assert_eq!(link_text(&db, "INP"), "@");
    assert_eq!(
        refusal("Soft Timestamp", "INP", "").as_deref(),
        Some(
            format!("{REC}.INP: can't initialize link type INST_IO with \"\" (type CONSTANT)")
                .as_str()
        )
    );

    let db = loads("    field(DTYP, \"Soft Timestamp\")")
        .await
        .expect("DTYP alone assigns no link text, so there is nothing to check");
    assert_eq!(
        link_text(&db, "INP"),
        "@",
        "C types an unassigned device link from its support anyway (dbStaticLib.c:2185-2212)"
    );
}

/// The accepting side of the same boundary: an exact match, and the
/// `{CONSTANT, PV_LINK, JSON_LINK}` set C treats as interchangeable
/// (`dbStaticLib.c:2408-2416`) — a soft device takes any of the three. The
/// text survives `iocInit` untouched.
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
        let db = loads(&body)
            .await
            .unwrap_or_else(|e| panic!("DTYP={dtyp:?} INP={inp:?}: the C IOC loads this: {e}"));
        assert_eq!(refusal(dtyp, "INP", inp), None);
        assert_eq!(link_text(&db, "INP"), inp, "an accepted link is left alone");
    }
}

/// The other boundary in the rule: WHICH field. Only `INP`/`OUT` are device
/// links (C `dbLexRoutines.c:571`); every other link field has no device support
/// and so is expected to be `CONSTANT`, whatever the record's DTYP is. C refuses
/// `field(SDIS,"@instio parm")` and `field(FLNK,"@instio parm")` for that reason
/// — measured, not reasoned — and the empty `CONSTANT` it leaves behind is the
/// empty string.
#[epics_macros_rs::epics_test]
async fn a_non_device_link_field_is_held_to_constant_whatever_the_dtyp_is() {
    for field in ["SDIS", "FLNK"] {
        let body =
            format!("    field(DTYP, \"Soft Channel\")\n    field({field}, \"@instio parm\")");
        let db = loads(&body)
            .await
            .unwrap_or_else(|e| panic!("{field}: C loads this file: {e}"));
        let line = refusal("Soft Channel", field, "@instio parm")
            .unwrap_or_else(|| panic!("{field}: a hardware link here is refused by C"));
        assert!(line.contains("CONSTANT"), "{field}: {line}");
        assert_eq!(link_text(&db, field), "");
    }
}

/// DTYP is a `DBF_DEVICE` menu index and its default is 0, so a record that
/// never spells DTYP is still bound to the FIRST device its `.dbd` declares
/// (`ai` → "Soft Channel", CONSTANT). C refuses a hardware `INP` there, and so
/// must the port: "no DTYP" is not "no device".
#[epics_macros_rs::epics_test]
async fn an_unspelled_dtyp_still_binds_the_first_declared_device() {
    let db = loads("    field(INP, \"@instio parm\")")
        .await
        .expect("C loads the record and refuses the link at iocInit");
    let line = refusal("", "INP", "@instio parm").expect("DTYP index 0 on ai is CONSTANT");
    assert!(line.contains("CONSTANT"), "{line}");
    assert_eq!(link_text(&db, "INP"), "");
}

/// C applies the SAME rule to a link written at runtime — `dbPutFieldLink`
/// calls `dbCanSetLink` (`dbAccess.c:1132`), and so does `dbPutString` once the
/// links are initialized (`dbStaticLib.c:2634-2642`). Both RETURN the status, so unlike the `iocInit`
/// pass a runtime assignment really is refused and the link is left as it was.
/// C prints nothing on that path; the message below is the port's own.
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
        .expect_err("an INST_IO device support cannot take a constant INP at runtime");
    assert!(
        err.to_string().contains("can't initialize link type"),
        "{err}"
    );
    assert_eq!(
        inst.common.text.inp, "@instio parm",
        "a refused put must leave the link exactly as it was — C's does"
    );
}

/// C checks links once, at `iocInit`, over the record as loaded
/// (`dbStaticLib.c:2178-2231`) — not as each field is parsed. So a `.db` may
/// spell `INP` before `DTYP` and still bind, and the verdict cannot depend on
/// the order the `.db` happened to use. MEASURED: C loads
/// `record(ai,"T:ORD"){ field(INP,"@instio parm") field(DTYP,"Soft Timestamp") }`
/// with no error.
#[epics_macros_rs::epics_test]
async fn the_verdict_does_not_depend_on_the_order_the_db_spells_the_fields() {
    let db = loads("    field(INP, \"@instio parm\")\n    field(DTYP, \"Soft Timestamp\")")
        .await
        .expect("C checks the link against the loaded record's DTYP, whatever order it was set in");
    assert_eq!(
        link_text(&db, "INP"),
        "@instio parm",
        "accepted, not emptied"
    );

    let db = loads("    field(INP, \"5\")\n    field(DTYP, \"Soft Timestamp\")")
        .await
        .expect("and the mismatch it does catch still costs the link, not the load");
    assert_eq!(link_text(&db, "INP"), "@");
}

/// Every DTYP a `.db` can name has a `device()` line and so a link type: one
/// no loaded line declares (`asynInt32` on an IOC without asyn) is refused
/// before any link rule could apply.
#[epics_macros_rs::epics_test]
async fn a_dtyp_no_device_line_declares_does_not_load() {
    let built =
        loads("    field(DTYP, \"asynInt32\")\n    field(INP, \"@asyn(PORT,0)VALUE\")").await;
    assert!(
        built.is_err(),
        "no loaded device() declares asynInt32, so `dbPutString` refuses the DTYP"
    );
    assert_eq!(refusal("asynInt32", "INP", "@asyn(PORT,0)VALUE"), None);
}

/// The pass rewrites link text, so it owns re-deriving whatever a record
/// caches from that text.
///
/// A `calcout` classifies every `INPA..INPL` and its `OUT` into
/// `INAV..INUV`/`OUTV` (`calcoutRecord.c:160-189`). This port does that at
/// load, before `db_init_record_links` has emptied the links C refuses — so
/// the classification stood against text that no longer existed. Measured on
/// softIoc R7.0.10 with `record(calcout,"X"){field(INPA,"@instio p")
/// field(OUT,"@instio q")}`: `dbgf X.INAV` and `dbgf X.OUTV` both print
/// `Constant` (calcout has no device support, so `@…` is refused and the link
/// is left CONSTANT and empty); this port printed `Ext PV NC` for both.
#[epics_macros_rs::epics_test]
async fn a_refused_link_leaves_no_stale_classification_behind() {
    let db = IocBuilder::new()
        .db_string(
            "record(calcout, \"CO:L\") {\n\
             \x20   field(CALC, \"A\")\n\
             \x20   field(INPA, \"@instio p\")\n\
             \x20   field(OUT,  \"@instio q\")\n\
             }\n",
            &HashMap::new(),
        )
        .expect("C loads this file")
        .build()
        .await
        .map(|(db, _)| db)
        .expect("a refused link costs the link, not the load");

    let rec = db.get_record("CO:L").expect("the record is kept, as in C");
    let inst = rec.read();
    // Both links were emptied by the refusal, exactly as `dbgf CO:L.INPA`
    // and `dbgf CO:L.OUT` read on C.
    assert_eq!(
        inst.record.get_field("INPA"),
        Some(epics_base_rs::types::EpicsValue::String("".into()))
    );
    assert_eq!(inst.common.text.out, "");
    // calcout's `INAV`/`OUTV` menu is `{Ext PV NC, Ext PV OK, Local PV,
    // Constant}` (`calcoutRecord.dbd`), so `Constant` is index 3.
    assert_eq!(
        inst.record.get_field("INAV"),
        Some(epics_base_rs::types::EpicsValue::Enum(3)),
        "INAV must be Constant — the classification the emptied INPA implies"
    );
    assert_eq!(
        inst.record.get_field("OUTV"),
        Some(epics_base_rs::types::EpicsValue::Enum(3)),
        "OUTV must be Constant — calcout captures the common OUT in init_links"
    );
}

/// A record type with no `device()` line has no `devSup`, so its INP/OUT
/// expects CONSTANT as every other link field does (`dbStaticLib.c:2187-2193`).
/// The C `softIoc` on `record(compress,"T:C1"){ field(INP,"@instio parm") }`:
///
/// ```text
/// ERROR: T:C1.INP: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)
/// ```
#[epics_macros_rs::epics_test]
async fn a_device_link_of_a_type_without_device_support_is_held_to_constant() {
    let line = link_type_refusal("T:C1", "compress", None, "INP", "@instio parm")
        .expect("compress has no device support: C refuses a hardware INP");
    assert_eq!(
        line,
        "T:C1.INP: can't initialize link type CONSTANT with \"@instio parm\" (type INST_IO)"
    );
    let (db, _) = IocBuilder::new()
        .db_string(
            "record(compress, \"T:C1\") {\n    field(INP, \"@instio parm\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(db.get_record("T:C1").unwrap().read().common.text.inp, "");
}

/// The ten-record transcript above, heard where C prints it: `iocInit`'s
/// refusal lines go through errlog, so an errlog listener receives each one.
#[epics_macros_rs::epics_test]
async fn the_refusals_print_through_errlog_at_ioc_init() {
    use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_strip_ansi};
    use std::sync::Mutex;

    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| {
        let m = errlog_strip_ansi(m);
        if m.contains(": T:") {
            sink.lock().unwrap().push(m);
        }
    });
    // `build` ends in `ioc_init`.
    IocBuilder::new()
        .db_string(
            r##"
record(ai,"T:R1"){ field(DTYP,"Soft Timestamp") field(INP,"5")           }
record(ai,"T:R2"){ field(DTYP,"Soft Timestamp") field(INP,"OTHER.VAL")   }
record(ai,"T:R3"){ field(DTYP,"Soft Timestamp") field(INP,"")            }
record(ai,"T:R4"){ field(DTYP,"Soft Channel")   field(INP,"@instio parm")}
record(ai,"T:R5"){ field(DTYP,"Soft Timestamp") field(INP,"#C0 S0 @x")   }
record(ai,"T:N1"){                              field(INP,"@instio parm")}
record(ai,"T:N2"){ field(DTYP,"Soft Channel")   field(SDIS,"@instio parm")}
record(ai,"T:N3"){ field(DTYP,"Soft Channel")   field(FLNK,"@instio parm")}
record(ai,"T:A1"){ field(DTYP,"Soft Timestamp") field(INP,"@instio parm")}
record(ai,"T:A2"){ field(DTYP,"Soft Channel")   field(INP,"5")           }
"##,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    errlog_flush();
    let want = [
        r#"T:R1.INP: can't initialize link type INST_IO with "5" (type CONSTANT)"#,
        r#"T:R2.INP: can't initialize link type INST_IO with "OTHER.VAL" (type PV_LINK)"#,
        r#"T:R3.INP: can't initialize link type INST_IO with "" (type CONSTANT)"#,
        r#"T:R4.INP: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)"#,
        r##"T:R5.INP: can't initialize link type INST_IO with "#C0 S0 @x" (type VME_IO)"##,
        r#"T:N1.INP: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)"#,
        r#"T:N2.SDIS: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)"#,
        r#"T:N3.FLNK: can't initialize link type CONSTANT with "@instio parm" (type INST_IO)"#,
    ]
    .map(|line| format!("ERROR: {line}\n"));
    assert_eq!(*heard.lock().unwrap(), want);
}

/// `iocInit` walks the records type-major (`iterateRecords`, `iocInit.c:484`)
/// over the name-sorted `recordTypeList`, load order within a type, so its
/// lines come out in that order and not the `.db`'s. softIoc R7.0.10 on the DB
/// below prints, after the load's own six CP lines, exactly `want`.
#[epics_macros_rs::epics_test]
async fn the_ioc_init_lines_follow_cs_record_type_order() {
    use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_strip_ansi};
    use std::sync::Mutex;

    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| {
        let m = errlog_strip_ansi(m);
        if m.contains("Discarding") || m.contains("can't initialize") {
            sink.lock().unwrap().push(m);
        }
    });
    // `build` is the load and then `ioc_init`.
    IocBuilder::new()
        .db_string(
            r#"
record(seq, "S1") { field(LNK1, "R:S CP") }
record(ao, "A1") { field(OUT, "X:Y CP") }
record(ai, "I1") { field(DTYP, "Soft Timestamp") field(INP, "5") }
record(calcout, "C1") { field(OUT, "E:F CP") }
record(ao, "A2") { field(OUT, "X:Z CP") }
record(waveform, "W1") { field(DTYP, "Soft Channel") field(INP, "@w") }
record(bo, "B1") { field(OUT, "B:B CP") }
record(aSub, "U1") { field(OUTA, "U:U CP") }
record(ai, "I2") { field(INP, "@x") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    errlog_flush();
    let cp = |from: &str, to: &str| {
        format!("WARNING: Discarding CP/CPP modifier in CA output link from {from} to {to}.\n")
    };
    let load = [
        cp("S1.LNK1", "R:S"),
        cp("A1.OUT", "X:Y"),
        cp("C1.OUT", "E:F"),
        cp("A2.OUT", "X:Z"),
        cp("B1.OUT", "B:B"),
        cp("U1.OUTA", "U:U"),
    ];
    let ioc_init = [
        cp("U1.OUTA", "U:U"),
        r#"ERROR: I1.INP: can't initialize link type INST_IO with "5" (type CONSTANT)"#.to_string()
            + "\n",
        r#"ERROR: I2.INP: can't initialize link type CONSTANT with "@x" (type INST_IO)"#
            .to_string()
            + "\n",
        cp("A1.OUT", "X:Y"),
        cp("A2.OUT", "X:Z"),
        cp("B1.OUT", "B:B"),
        cp("C1.OUT", "E:F"),
        cp("S1.LNK1", "R:S"),
        r#"ERROR: W1.INP: can't initialize link type CONSTANT with "@w" (type INST_IO)"#
            .to_string()
            + "\n",
    ];
    assert_eq!(
        *heard.lock().unwrap(),
        [load.to_vec(), ioc_init.to_vec()].concat()
    );
}
