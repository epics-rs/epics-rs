//! R15-78: a CONSTANT input link is loaded ONCE, at init — never re-applied at
//! process.
//!
//! Two halves of one C rule:
//!
//!  * init: `devAaiSoft.c::init_record` (lines 51-67) and
//!    `devWfSoft.c::init_record` (lines 39-51) call
//!    `dbLoadLinkArray(&prec->inp, prec->ftvl, prec->bptr, &nRequest)` on a
//!    constant INP and set `NORD = nRequest`, `UDF = FALSE`;
//!  * process: `read_aai` / `read_wf` open with
//!    `if (dbLinkIsConstant(pinp)) return 0;` — nothing is read, VAL is left
//!    exactly as it stands. (`dbGetLink` on a constant would be a no-op anyway:
//!    `dbConstGetValue` returns status 0 having written nothing,
//!    `dbConstLink.c:219-225`.)
//!
//! The port had NEITHER half: no init load, and `read_link_value_soft` returned
//! the constant on every cycle. The everyday consequence is not the exotic
//! `field(INP,"[1,2,3]")` case — it is that an aai/waveform with an UNSET INP is
//! a constant link too (`dbLink.c:218`), which is how EVERY client-fed array
//! record is configured. Whatever a client caput into VAL was overwritten by the
//! empty constant on the next scan.
//!
//! Boundaries: constant array INP vs unset INP vs real DB INP; before-first-
//! process vs after; NORD and UDF on each.

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "SRC") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(aai, "CONST:AAI") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(INP, "[1, 2, 3]")
}
record(waveform, "CONST:WF") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(INP, "[4, 5]")
}
record(aai, "UNSET:AAI") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(waveform, "UNSET:WF") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(waveform, "LINKED:WF") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(INP, "SRC")
}
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// The constant array is in VAL before any process, with NORD = element count
/// and UDF clear (C `dbLoadLinkArray` -> `nord = nRequest; udf = FALSE`).
#[epics_macros_rs::epics_test]
async fn constant_array_inp_is_loaded_at_init() {
    let db = build().await;

    assert_eq!(
        db.get_pv("CONST:AAI").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
        "aai: field(INP,\"[1,2,3]\") must be loaded at init"
    );
    assert_eq!(db.get_pv("CONST:AAI.NORD").unwrap().to_f64(), Some(3.0));
    assert!(
        db.get_record("CONST:AAI").unwrap().read().common.udf == 0,
        "a record loaded from a constant link is DEFINED"
    );

    assert_eq!(
        db.get_pv("CONST:WF").unwrap(),
        EpicsValue::DoubleArray(vec![4.0, 5.0]),
        "waveform: field(INP,\"[4,5]\") must be loaded at init"
    );
    assert_eq!(db.get_pv("CONST:WF.NORD").unwrap().to_f64(), Some(2.0));
}

/// Process must not re-apply the constant over data a client put into VAL.
#[epics_macros_rs::epics_test]
async fn constant_inp_is_not_re_applied_at_process() {
    let db = build().await;

    for rec in ["CONST:AAI", "CONST:WF"] {
        db.put_pv(rec, EpicsValue::DoubleArray(vec![9.0, 8.0, 7.0, 6.0]))
            .await
            .unwrap();
        process(&db, rec).await;
        assert_eq!(
            db.get_pv(rec).unwrap(),
            EpicsValue::DoubleArray(vec![9.0, 8.0, 7.0, 6.0]),
            "{rec}: a constant INP must deliver NOTHING at process (C read_aai/read_wf: \
             `if (dbLinkIsConstant(pinp)) return 0;`)"
        );
        assert_eq!(
            db.get_pv(&format!("{rec}.NORD")).unwrap().to_f64(),
            Some(4.0)
        );
    }
}

/// The common case: an UNSET INP is a constant link (`dbLink.c:218`), so a
/// device-fed / client-fed array record keeps the data it was given across
/// scans. This is the case the pre-fix port wiped on every process.
#[epics_macros_rs::epics_test]
async fn unset_inp_does_not_wipe_client_data() {
    let db = build().await;

    for rec in ["UNSET:AAI", "UNSET:WF"] {
        db.put_pv(rec, EpicsValue::DoubleArray(vec![1.5, 2.5]))
            .await
            .unwrap();
        process(&db, rec).await;
        assert_eq!(
            db.get_pv(rec).unwrap(),
            EpicsValue::DoubleArray(vec![1.5, 2.5]),
            "{rec}: an unset INP is a constant link — process must not touch VAL"
        );
    }
}

/// A REAL input link is unaffected: it still reads its source every cycle.
#[epics_macros_rs::epics_test]
async fn real_db_inp_still_reads_every_cycle() {
    let db = build().await;

    db.put_pv("SRC", EpicsValue::DoubleArray(vec![10.0, 20.0]))
        .await
        .unwrap();
    process(&db, "LINKED:WF").await;
    assert_eq!(
        db.get_pv("LINKED:WF").unwrap(),
        EpicsValue::DoubleArray(vec![10.0, 20.0])
    );

    // A client caput IS overwritten here — the link is the value's owner.
    db.put_pv("LINKED:WF", EpicsValue::DoubleArray(vec![0.0]))
        .await
        .unwrap();
    db.put_pv("SRC", EpicsValue::DoubleArray(vec![30.0, 40.0, 50.0]))
        .await
        .unwrap();
    process(&db, "LINKED:WF").await;
    assert_eq!(
        db.get_pv("LINKED:WF").unwrap(),
        EpicsValue::DoubleArray(vec![30.0, 40.0, 50.0])
    );
}

/// C `dbParseLink` (`dbStaticLib.c:2346-2357`) calls a link constant iff it is
/// empty, parses whole as a double, or is BRACKETED. The bracketed form is the
/// only array constant there is — its text goes to `dbPutConvertJSON`.
///
/// `"1 2 3"` is not an array constant in C: `epicsParseDouble` rejects the
/// trailing garbage, so C makes it a PV_LINK to a record named `1` — which the
/// port now does too (see `link_modifier_split_runs_after_the_constant_test`).
/// Only a bracketed literal loads as the constant array.
#[test]
fn only_a_bracketed_literal_is_a_constant_array() {
    use epics_base_rs::server::record::{ParsedLink, parse_link_v2};

    assert!(
        matches!(parse_link_v2("[1, 2, 3]"), ParsedLink::Constant(_)),
        "a bracketed literal IS a constant array"
    );
    assert_eq!(
        parse_link_v2("[1, 2, 3]").constant_value(),
        Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0])),
        "C `dbConstLoadArray` -> `dbPutConvertJSON`: three elements"
    );
    assert_eq!(
        parse_link_v2("[]").constant_value(),
        Some(EpicsValue::DoubleArray(vec![])),
        "an empty literal is zero elements (nRequest = 0)"
    );
    assert_ne!(
        parse_link_v2("1 2 3").constant_value(),
        Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0])),
        "a whitespace-separated list is NOT the constant array [1,2,3]"
    );
}
