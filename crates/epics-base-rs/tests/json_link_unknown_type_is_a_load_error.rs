//! Brace-delimited link text is a JSON link or it is a load error. It is never
//! a PV name.
//!
//! C reference. `dbParseLink` (`dbStaticLib.c:2280-2286`) tests
//! `*str == '{' && str[len-1] == '}'` and hands the whole text to
//! `dbJLinkParse`; a non-zero return is `goto fail` → `S_dbLib_badField`, and
//! the PV-name / `#`-hardware arms below it are never reached. Inside,
//! `dbjl_map_key` looks the first key up with `dbFindLinkSup` and on a miss
//! prints `dbJLinkInit: Link type '%s' not found` and returns `jlif_stop`
//! (`dbJLink.c:260-268`), which `dbJLinkParse` turns into `S_db_badField`
//! (`:426-438`).
//!
//! The port had one `Option` for "not JSON" and "bad JSON", so the second fell
//! through `parse_link_field` into the legacy PV-name arm and produced a CA
//! link whose channel name was the JSON text — a channel that can never
//! connect, so the put was silently dropped and no db state moved.
//!
//! Boundaries here: unknown key, registered-but-unported key, ported key,
//! empty map, and the brace test itself (`{` without a closing `}` is NOT
//! JSON in C and must still parse as a PV name).

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{LinkFieldType, ParsedLink, parse_link_field};
use epics_base_rs::types::EpicsValue;

/// Why the load refused `text`. C reports the refusal at the field
/// (`dbJLinkInit: Link type '%s' not found`) and `dbLoadRecords` returns
/// only a status, which is all [`load_out`] can hand back, so the reason
/// is asked of `apply_fields` — the step the builder calls per record.
fn reason(text: &str) -> String {
    let db = format!("record(bo, \"TRIG\") {{\n    field(OUT, \"{text}\")\n}}\n");
    let defs = epics_base_rs::server::db_loader::parse_db(&db, &HashMap::new())
        .expect("parse is fine — the refusal happens at field application");
    for def in defs {
        let mut record = epics_base_rs::server::db_loader::create_record_with_factories(
            &def.record_type,
            &HashMap::new(),
        )
        .unwrap_or_else(|e| panic!("{}: create_record failed: {e}", def.record_type));
        let mut common = Vec::new();
        if let Err(e) =
            epics_base_rs::server::db_loader::apply_fields(&mut record, &def.fields, &mut common)
        {
            return e.to_string();
        }
    }
    panic!("no link was refused in: {db}");
}

/// Load one record whose OUT carries `text`, and return the load's verdict.
async fn load_out(text: &str) -> Result<(), String> {
    let db = format!("record(bo, \"TRIG\") {{\n    field(OUT, \"{text}\")\n}}\n");
    match IocBuilder::new().db_string(&db, &HashMap::new()) {
        Err(e) => Err(e.to_string()),
        Ok(b) => match b.build().await {
            Ok(_) => Ok(()),
            Err(e) => Err(e.to_string()),
        },
    }
}

/// BOUNDARY: a key naming no registered link type. C refuses the load and
/// names the type; the port must too, and must build no link at all.
#[epics_macros_rs::epics_test]
async fn an_unknown_json_link_type_fails_the_load_naming_the_type() {
    load_out(r#"{'nosuch':1}"#)
        .await
        .expect_err("C `dbFindLinkSup` misses and `dbJLinkParse` returns S_db_badField");
    let err = reason(r#"{'nosuch':1}"#);

    assert!(
        err.contains("nosuch"),
        "C names the type it could not find: `dbJLinkInit: Link type '%s' not found` — got {err}"
    );
    assert!(
        !matches!(
            parse_link_field(r#"{'nosuch':1}"#, LinkFieldType::Out),
            ParsedLink::Ca(_) | ParsedLink::Db(_)
        ),
        "brace text must never become a PV-name link; C's `dbParseLink` never \
         reaches its PV-name arm for it"
    );
}

/// DEVIATION from C, deliberate and the same one the file's other tests
/// state: a key epics-base DOES register (`links.dbd.pod:208`, jlif at
/// `lnkDebug.c:1030`) but this port has not implemented is rejected like an
/// unknown type, because a missing implementation must be visible rather
/// than degrade into a dead link.
///
/// C loads it. Measured on softIoc R7.0.10 with
/// `record(ai,"K1"){field(INP,"{debug:{const:1}}")}` and the `trace`
/// equivalent: both records load, `dbl` lists them, and after `iocInit`
/// `dbgf K1` is 1 and `dbgf K2` is 2 with the trace lines printed. So this
/// assertion is the port refusing what C accepts, not a shared rule.
///
/// The example used to be `state`, which this port now implements. `debug`
/// and `trace` are the two of C's five that remain, and they remain for a
/// structural reason rather than for want of writing: both are lset
/// *wrappers* — `lnkDebug.c:337-490` forwards every lset method to a child
/// link's lset and prints around it — and this port dispatches a link by its
/// `ParsedLink` variant, with no per-link vtable for them to wrap.
#[epics_macros_rs::epics_test]
async fn a_registered_but_unported_json_link_type_fails_the_load() {
    let db = "record(ai, \"K1\") {\n    field(INP, \"{debug:{const:1}}\")\n}\n";
    let built = match IocBuilder::new().db_string(db, &HashMap::new()) {
        Err(e) => Err(e.to_string()),
        Ok(b) => b.build().await.map(|_| ()).map_err(|e| e.to_string()),
    };
    built.expect_err("the port has no debug link support to build");

    let ParsedLink::None = parse_link_field(r#"{debug:{const:1}}"#, LinkFieldType::In) else {
        panic!("an unported jlink must build no link at all");
    };
}

/// BOUNDARY: a key the port DOES serve still loads. The gate rejects by
/// unknown type, not by the presence of braces.
#[epics_macros_rs::epics_test]
async fn a_ported_json_link_type_still_loads() {
    let db = IocBuilder::new()
        .db_string(
            "record(ai, \"SRC\") {\n    field(INP, \"{'const':7.5}\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .expect("`const` is registered (`links.dbd.pod:39`) and ported")
        .0;

    assert_eq!(
        db.get_pv("SRC").unwrap(),
        EpicsValue::Double(7.5),
        "a const JSON link seeds VAL at init, so the link really was built"
    );
}

/// BOUNDARY: the empty map. yajl parses `{}` without error, so `dbJLinkParse`
/// returns 0 with a NULL product and `dbParseLink` stores `JSON_LINK` with no
/// jlink (`dbStaticLib.c:2281-2285`) — a link that does nothing, not a load
/// failure.
#[epics_macros_rs::epics_test]
async fn an_empty_json_map_loads_as_no_link() {
    load_out("{}")
        .await
        .expect("C's yajl accepts `{}`; the link is simply empty");

    assert!(
        matches!(parse_link_field("{}", LinkFieldType::Out), ParsedLink::None),
        "an empty map builds no link"
    );
}

/// BOUNDARY: the brace test is `first == '{' AND last == '}'`. Text that only
/// opens a brace is not JSON to C, so `dbParseLink` falls through to its
/// PV-name arm and the port must keep doing the same.
#[epics_macros_rs::epics_test]
async fn text_that_does_not_close_its_brace_is_still_a_pv_name() {
    assert!(
        matches!(
            parse_link_field("{nosuch", LinkFieldType::Out),
            ParsedLink::Db(_)
        ),
        "C only takes the JSON arm when str[len-1] == '}}' (`dbStaticLib.c:2281`)"
    );
}

/// BOUNDARY: the runtime put path. C routes a link field's text through the
/// same `dbParseLink` on `dbPutField` as on load (`dbAccess.c` →
/// `dbPutFieldLink`), so a runtime write of an unknown JSON link fails and the
/// field keeps its old value.
#[epics_macros_rs::epics_test]
async fn a_runtime_put_of_an_unknown_json_link_is_refused() {
    let db = IocBuilder::new()
        .db_string(
            "record(bo, \"TRIG\") {\n    field(SDIS, \"OTHER\")\n}\nrecord(bi, \"OTHER\") {}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let rec = db.get_record("TRIG").unwrap();
    let err = rec
        .write()
        .put_common_field("SDIS", EpicsValue::String(r#"{'nosuch':1}"#.into()))
        .expect_err("C's `dbPutField` on a link field parses before it stores");

    assert!(
        err.to_string().contains("nosuch"),
        "the runtime diagnostic names the type too — got {err}"
    );
}
