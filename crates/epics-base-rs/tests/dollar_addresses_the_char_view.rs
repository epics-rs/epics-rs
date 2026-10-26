//! C `dbNameToAddr`'s `$` modifier (`dbAccess.c:681-701`), on the address
//! `dbEntryToAddr` made: a `DBF_STRING` field, an attribute among them,
//! becomes `field_size` `DBF_CHAR`s (`:686-690`), a link field
//! `PVLINK_STRINGSZ` (`dbDefs.h:79`) characters of its own type
//! (`:692-696`), and any other field is `S_dbLib_fieldNotFound`
//! (`:698-700`). Only the one character after the field name is read.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{AsyncDbHandle, FieldPut, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::OwnRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

const DB: &str = r#"
record(ai, "AI") { field(DESC, "hello") }
record(ai, "H") {}
record(ai, "P") { field(DESC, "mine") field(INP, "H.VAL CP MS") }
record(lso, "L") { field(SIZV, "80") }
record(sseq, "SQ") {
    field(DOL1, "AI.DESC$")
    field(DOL2, "AI.RTYP$")
    field(DOL3, "AI.INP$")
    field(DOL4, "AI.VAL$")
    field(DOL5, "AI.INP")
}
"#;

async fn ioc() -> Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

#[epics_macros_rs::epics_test]
async fn name_to_addr_counts_the_char_view() {
    let db = ioc().await;
    let h = db.async_handle();
    assert_eq!(h.name_to_addr("AI.RTYP$"), Some(40));
    assert_eq!(h.name_to_addr("AI.DESC$"), Some(41));
    assert_eq!(h.name_to_addr("AI.DESC$x"), Some(41));
    assert_eq!(h.name_to_addr("AI.INP$"), Some(1024));
    assert_eq!(h.name_to_addr("AI.VAL$"), None);
    assert_eq!(h.name_to_addr("AI.TIME$"), None);
}

#[epics_macros_rs::epics_test]
async fn name_to_dbaddr_types_the_char_view() {
    let db = ioc().await;
    let h = db.async_handle();
    let addr = |pv| {
        h.name_to_dbaddr(pv, None)
            .map(|a| (a.field, a.field_type, a.no_elements))
    };
    let char = DbfCode::Char;
    assert_eq!(addr("AI.DESC$"), Some(("DESC".into(), char, 41)));
    assert_eq!(addr("AI.RTYP$"), Some(("RTYP".into(), char, 40)));
    assert_eq!(addr("AI.INP"), Some(("INP".into(), DbfCode::Inlink, 1)));
    assert_eq!(addr("AI.INP$"), Some(("INP".into(), DbfCode::Inlink, 1024)));
    assert_eq!(addr("AI.VAL$"), None);
    assert_eq!(addr("AI.TIME$"), None);
}

#[epics_macros_rs::epics_test]
async fn has_pv_takes_the_char_view() {
    let db = ioc().await;
    let h = db.async_handle();
    for (pv, found) in [
        ("AI.DESC$", true),
        ("AI.RTYP$", true),
        ("AI.INP$", true),
        ("AI.VAL$", false),
    ] {
        assert_eq!(h.has_pv(pv), found, "{pv}");
    }
}

/// `dbGetLinkDBFtype` of a DB link to a `$` name, and sseq's `DTn` and
/// `DOLnV` from the `dbNameToAddr` its `init_record` makes
/// (`sseqRecord.c:208-219`).
#[epics_macros_rs::epics_test]
async fn a_link_to_a_char_view_is_a_char_link() {
    let db = ioc().await;
    let h = db.async_handle();
    let char = Some(DbfCode::Char);
    assert_eq!(h.link_target_field_type("AI.DESC$"), char);
    assert_eq!(h.link_target_field_type("AI.RTYP$"), char);
    let inlink = Some(DbfCode::Inlink);
    assert_eq!(h.link_target_field_type("AI.INP$"), inlink);
    assert_eq!(h.link_target_field_type("AI.INP"), inlink);
    assert_eq!(h.link_target_field_type("AI.VAL$"), None);
    let get = |pv| db.get_pv(pv).unwrap();
    assert_eq!(get("SQ.DT1"), EpicsValue::Short(1));
    assert_eq!(get("SQ.DT2"), EpicsValue::Short(1));
    assert_eq!(get("SQ.DT3"), EpicsValue::Short(14));
    assert_eq!(get("SQ.DT5"), EpicsValue::Short(14));
    assert_eq!(get("SQ.DOL4V"), EpicsValue::Enum(0));
    assert_eq!(get("SQ.DT4"), EpicsValue::Short(-1));
}

/// `dbGet` through the `$` address: `DBR_CHAR` reads the characters, and
/// `DBR_STRING` converts the first one (`cvt_c_st`), except an attribute's,
/// which `getAttrValue` reads as its text (`dbAccess.c:950-953`).
#[epics_macros_rs::epics_test]
async fn db_get_reads_the_char_view() {
    let db = ioc().await;
    let h = db.async_handle();
    let chars = |pv, n| match h.db_get(pv, DbFieldType::Char, n, None).map(|s| s.value) {
        Some(EpicsValue::CharArray(a)) => Some(a.to_vec()),
        other => panic!("{pv}: {other:?}"),
    };
    assert_eq!(chars("AI.DESC$", 41), Some(b"hello\0".to_vec()));
    assert_eq!(chars("AI.RTYP$", 40), Some(b"ai\0".to_vec()));
    assert!(h.db_get("AI.VAL$", DbFieldType::Char, 1, None).is_none());
    let string = |pv| h.get_dbr_string(pv, None).map(|s| s.to_string());
    assert_eq!(string("AI.DESC$"), Some("104".into()));
    assert_eq!(string("AI.RTYP$"), Some("ai".into()));
    assert_eq!(string("AI.VAL$"), None);
}

/// `dbPut` and `dbPutField` through the `$` address `dbNameToAddr` made
/// write the field as characters.
#[epics_macros_rs::epics_test]
async fn a_held_put_writes_the_char_view() {
    let db = ioc().await;
    let h = db.async_handle();
    let desc = h.name_to_dbaddr("AI.DESC$", None).unwrap();
    let chars = |s: &[u8]| EpicsValue::CharArray(s.to_vec().into());
    let put = h.with_held("H", |held| held.db_put(&desc, chars(b"bye\0")));
    assert_eq!(put, Some(Ok(())));
    assert_eq!(
        db.get_pv("AI.DESC").unwrap(),
        EpicsValue::String("bye".into())
    );
    let put = h.with_held("H", |held| held.db_put_field(&desc, chars(b"now\0")));
    assert!(matches!(put, Some(FieldPut::Done(Ok(())))), "{put:?}");
    assert_eq!(
        db.get_pv("AI.DESC").unwrap(),
        EpicsValue::String("now".into())
    );
}

type Reads = (Option<EpicsValue>, Option<EpicsValue>, Option<String>);

/// `pv` read as `DBR_CHAR` and `DBR_STRING` and rendered by `get_dbr_string`.
fn reads(h: &AsyncDbHandle, pv: &str, n: usize, own: Option<OwnRecord<'_>>) -> Reads {
    let get = |dbr, n| h.db_get(pv, dbr, n, own).map(|s| s.value);
    (
        get(DbFieldType::Char, n),
        get(DbFieldType::String, 1),
        h.get_dbr_string(pv, own).map(|s| s.to_string()),
    )
}

/// C's `dbGet` reads a `$` address of the record its caller is inside as it
/// reads any other record's: no lock is taken either way.
#[epics_macros_rs::epics_test]
async fn the_callers_own_record_reads_its_char_view() {
    let db = ioc().await;
    db.put_pv("L.VAL", EpicsValue::String("a long string".into()))
        .await
        .unwrap();
    let h = db.async_handle();
    for (rec, pv, n) in [
        ("P", "P.DESC$", 41),
        ("P", "P.RTYP$", 40),
        ("P", "P.INP$", 1024),
        ("P", "P.FLNK$", 1024),
        ("L", "L.VAL$", 80),
        ("P", "P.VAL$", 1),
    ] {
        let unheld = reads(&h, pv, n, None);
        assert_eq!(unheld.0.is_some(), pv != "P.VAL$", "{pv}: {unheld:?}");
        let own = h.with_own(rec, |r, own| reads(&h, pv, n, Some(own.of(&*r))));
        assert_eq!(own, Some(unheld), "{pv}");
    }
}

/// Without `$` too, the record the caller is inside reads as any reader
/// reads it: a dbCommon field, an attribute, and a link `dbGetString`
/// renders (`dbAccess.c:850-856`).
#[epics_macros_rs::epics_test]
async fn the_callers_own_record_reads_as_any_reader() {
    let db = ioc().await;
    let h = db.async_handle();
    let mut wrong = Vec::new();
    for pv in [
        "P.DESC", "P.RTYP", "P.SCAN", "P.VAL", "P.PREC", "P.INP", "P.FLNK", "SQ.DOL1", "SQ.LNK1",
    ] {
        let rec = &pv[..pv.find('.').unwrap()];
        let unheld = reads(&h, pv, 1, None);
        assert!(unheld.2.is_some(), "{pv}");
        let own = h.with_own(rec, |r, own| reads(&h, pv, 1, Some(own.of(&*r))));
        if own.as_ref() != Some(&unheld) {
            wrong.push(format!("{pv}: own {own:?}, unheld {unheld:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// C `dbNameToAddr` finds a field of the caller's own record as it finds any
/// other record's: by its declaration, not by what the record stores.
#[epics_macros_rs::epics_test]
async fn has_pv_finds_the_callers_own_fields() {
    let db = ioc().await;
    let h = db.async_handle();
    for pv in ["P.DESC", "P.SCAN", "P.NAME", "P.MLOK", "P.TIME", "P.INP"] {
        let found = h.with_own("P", |_, _| h.has_pv(pv));
        assert_eq!(found, Some(true), "{pv}");
    }
}
