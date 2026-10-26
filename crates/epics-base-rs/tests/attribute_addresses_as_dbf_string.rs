//! A record attribute (`RTYP`, `VERS`) addresses as C `dbNameToAddr` gives
//! it: `dbFindFieldPart` misses, `dbGetAttributePart` finds it
//! (`dbAccess.c:673-675`), and `dbEntryToAddr` copies
//! `dbPutRecordAttribute`'s descriptor, `DBF_STRING` of `MAX_STRING_SIZE`
//! with one element (`dbAccess.c:636-639`, `dbStaticLib.c:1268-1271`). A
//! `$` makes it 40 `DBF_CHAR` (`dbAccess.c:686-690`). A declared field of
//! the same name hides the attribute, since `dbFindFieldPart` runs first.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

const DB: &str = r#"
record(ai, "AI") {}
record(stringin, "SI") {}
record(sseq, "SQ") {
    field(DOL1, "AI.RTYP")
    field(LNK1, "SI.VAL")
    field(DOL2, "AI.VERS")
    field(LNK2, "SI.DESC")
}
"#;

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

/// `dbGetLinkDBFtype` of a DB link to an attribute: sseq reads it as a
/// string (`sseqRecord.c:643-647`) and, typed at init by `dbNameToAddr`,
/// reports it local (`sseqRecord.c:208-211`).
#[epics_macros_rs::epics_test]
async fn a_link_to_an_attribute_is_a_string_link() {
    let db = ioc().await;
    let h = db.async_handle();
    assert_eq!(h.link_target_field_type("AI.RTYP"), Some(DbfCode::String));
    assert_eq!(h.link_target_field_type("AI.VERS"), Some(DbfCode::String));
    assert_eq!(db.get_pv("SQ.DOL1V").unwrap(), EpicsValue::Enum(2));
    db.process_record("SQ").await.unwrap();
    let text = |pv| db.get_pv(pv).unwrap();
    for _ in 0..500 {
        if text("SI.DESC") != EpicsValue::String("".into()) {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(text("SI.VAL"), EpicsValue::String("ai".into()));
    assert_eq!(text("SI.DESC"), EpicsValue::String("none specified".into()));
}

#[epics_macros_rs::epics_test]
async fn name_to_addr_resolves_an_attribute() {
    let db = ioc().await;
    let h = db.async_handle();
    assert_eq!(h.name_to_addr("AI.RTYP"), Some(1));
    assert_eq!(h.name_to_addr("AI.VERS"), Some(1));
    assert_eq!(h.name_to_addr("AI.NOPE"), None);
    let addr = h.name_to_dbaddr("AI.RTYP", None).unwrap();
    assert_eq!(
        (addr.field.as_str(), addr.field_type, addr.no_elements),
        ("RTYP", DbfCode::String, 1)
    );
    assert!(h.has_pv("AI.RTYP"));
    assert!(!h.has_pv("AI.NOPE"));
}

/// `dbGet` of an attribute is `getAttrValue`'s text (`dbAccess.c:950-953`).
#[epics_macros_rs::epics_test]
async fn db_get_reads_an_attribute() {
    let db = ioc().await;
    let h = db.async_handle();
    assert_eq!(
        h.get_dbr_string("AI.RTYP", None).map(|s| s.to_string()),
        Some("ai".to_string())
    );
    let snap = h.db_get("AI.VERS", DbFieldType::String, 1, None).unwrap();
    assert_eq!(snap.value, EpicsValue::String("none specified".into()));
}

/// Autosave's `SR_get_array_info` (`dbrestore.c:1319-1334`).
#[epics_macros_rs::epics_test]
async fn restore_target_sizes_an_attribute() {
    let db = ioc().await;
    let t = db.restore_target("AI.RTYP").unwrap();
    assert_eq!(
        (t.elements, t.addr_type, t.field_size),
        (1, DbfCode::String, 40)
    );
    let t = db.restore_target("AI.RTYP$").unwrap();
    assert_eq!((t.elements, t.addr_type), (40, DbfCode::Char));
    assert!(db.restore_target("AI.NOPE").is_none());
}

/// An attribute `dbPutAttribute` adds is found like the seeded two; one
/// named like a declared field is never reached.
#[epics_macros_rs::epics_test]
async fn a_declared_field_hides_the_attribute() {
    let db = ioc().await;
    db.put_record_type_attribute("ai", Some("PREC"), Some("x"))
        .unwrap();
    db.put_record_type_attribute("ai", Some("XYZ"), Some("x"))
        .unwrap();
    let h = db.async_handle();
    assert_eq!(h.link_target_field_type("AI.PREC"), Some(DbfCode::Short));
    assert_eq!(
        h.name_to_dbaddr("AI.PREC", None).unwrap().field_type,
        DbfCode::Short
    );
    assert_eq!(h.link_target_field_type("AI.XYZ"), Some(DbfCode::String));
    assert_eq!(h.name_to_addr("AI.XYZ"), Some(1));
}

/// A record type with no field table: every name it spells is taken as its
/// own field.
struct Bare;

impl epics_base_rs::server::record::Record for Bare {
    fn record_type(&self) -> &'static str {
        "bare"
    }
    fn get_field(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    fn put_field(&mut self, name: &str, _: EpicsValue) -> epics_base_rs::error::CaResult<()> {
        Err(epics_base_rs::error::CaError::FieldNotFound(name.into()))
    }
}

/// No attribute shows through a type whose field names are not known,
/// with `$` or without: `dbNameToAddr`, channel create and `dbGet` agree.
#[epics_macros_rs::epics_test]
async fn a_type_with_no_field_table_shows_no_attribute() {
    let (db, _) = IocBuilder::new()
        .register_record_type("bare", || Box::new(Bare))
        .db_string("record(bare, \"B\") {}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let h = db.async_handle();
    assert_eq!(h.name_to_addr("B.RTYP"), None);
    assert_eq!(h.name_to_addr("B.RTYP$"), None);
    assert!(h.get_dbr_string("B.RTYP", None).is_none());
    let create = |pv: &[u8]| epics_base_rs::server::database::filters::db_channel_create(&db, pv);
    assert!(create(b"B.RTYP$").is_err());
}
