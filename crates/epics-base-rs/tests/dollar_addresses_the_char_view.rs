//! C `dbNameToAddr`'s `$` modifier (`dbAccess.c:681-701`), on the address
//! `dbEntryToAddr` made: a `DBF_STRING` field, an attribute among them,
//! becomes `field_size` `DBF_CHAR`s (`:686-690`), a link field
//! `PVLINK_STRINGSZ` (`dbDefs.h:79`) characters of its own type
//! (`:692-696`), and any other field is `S_dbLib_fieldNotFound`
//! (`:698-700`). Only the one character after the field name is read.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{FieldPut, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ai, "AI") { field(DESC, "hello") }
record(ai, "H") {}
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
    let char = Some(DbFieldType::Char);
    assert_eq!(addr("AI.DESC$"), Some(("DESC".into(), char, 41)));
    assert_eq!(addr("AI.RTYP$"), Some(("RTYP".into(), char, 40)));
    let inp = h.name_to_dbaddr("AI.INP", None).unwrap().field_type;
    assert_eq!(addr("AI.INP$"), Some(("INP".into(), inp, 1024)));
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
        assert_eq!(h.has_pv(pv, None), found, "{pv}");
        let own = AiRecord::default();
        let held = Some((&b"AI"[..], &own as &dyn Record));
        assert_eq!(h.has_pv(pv, held), found, "{pv} held");
    }
}

/// `dbGetLinkDBFtype` of a DB link to a `$` name, and sseq's `DTn` and
/// `DOLnV` from the `dbNameToAddr` its `init_record` makes
/// (`sseqRecord.c:208-219`).
#[epics_macros_rs::epics_test]
async fn a_link_to_a_char_view_is_a_char_link() {
    let db = ioc().await;
    let h = db.async_handle();
    let char = Some(DbFieldType::Char);
    assert_eq!(h.link_target_field_type("AI.DESC$"), char);
    assert_eq!(h.link_target_field_type("AI.RTYP$"), char);
    assert_eq!(
        h.link_target_field_type("AI.INP$"),
        h.link_target_field_type("AI.INP")
    );
    assert_eq!(h.link_target_field_type("AI.VAL$"), None);
    let get = |pv| db.get_pv(pv).unwrap();
    assert_eq!(get("SQ.DT1"), EpicsValue::Short(1));
    assert_eq!(get("SQ.DT2"), EpicsValue::Short(1));
    assert_eq!(get("SQ.DT3"), get("SQ.DT5"));
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
