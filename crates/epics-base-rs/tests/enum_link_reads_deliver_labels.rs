//! UI-63 (epics-base#183): a link read the record requests as `DBR_STRING`
//! must deliver an ENUM/MENU source's state LABEL, never its index digits.
//!
//! C reads:
//! - stringin INP  — `dbGetLink(..., DBR_STRING, ...)` (`devSiSoft.c:53`)
//! - stringin SIOL — same request (`stringinRecord.c:208`)
//! - lsi INP       — `dbGetLinkLS` (`devLsiSoft.c:32` → `dbLink.c:497-505`):
//!   CHAR/UCHAR source → the bytes it spells, else `DBR_STRING`
//! - stringout DOL — `dbGetLink(..., DBR_STRING, ...)` (`stringoutRecord.c:141`)
//! - lso DOL       — `dbGetLinkLS` (`lsoRecord.c:114`)
//! - printf `%s`   — `dbGetLink(..., DBR_STRING, ...)` (`printfRecord.c:291`)
//!
//! The pre-fix port fetched every one of these natively and let the target
//! field coerce blind, so a bi/mbbi/menu source stored `"1"` where fixed C
//! stores `"RUNNING"`. Boundary per read path, plus the numeric printf
//! conversion that must KEEP the index, and the external-link half (the
//! label table rides `LinkMetadata::enum_choices` — C `dbCa`'s second
//! `DBR_STRING` monitor).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{LinkMetadata, LinkPutOp, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn build(db_text: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(rec, &mut v, 0).await.unwrap();
}

fn field(db: &PvDatabase, rec: &str, field: &str) -> EpicsValue {
    let r = db.get_record(rec).unwrap();
    let v = r.read().record.get_field(field);
    v.unwrap_or_else(|| panic!("{rec}.{field} missing"))
}

fn field_str(db: &PvDatabase, rec: &str, name: &str) -> String {
    match field(db, rec, name) {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::CharArray(b) => String::from_utf8_lossy(&b)
            .trim_end_matches('\0')
            .to_string(),
        other => panic!("{rec}.{name} is not string-shaped: {other:?}"),
    }
}

/// stringin INP at a bi VAL: the DBR_STRING request renders ONAM.
#[epics_macros_rs::epics_test]
async fn stringin_inp_at_enum_source_delivers_the_label() {
    let db = build(
        r#"
        record(bi, "SRC") { field(ZNAM, "STOPPED") field(ONAM, "RUNNING") field(VAL, "1") }
        record(stringin, "SI") { field(INP, "SRC") }
        "#,
    )
    .await;
    process(&db, "SI").await;
    assert_eq!(
        field_str(&db, "SI", "VAL"),
        "RUNNING",
        "C devSiSoft reads INP with DBR_STRING — the pre-fix port stored \"1\""
    );
}

/// stringin INP at a MENU field: the same request renders the menu choice.
#[epics_macros_rs::epics_test]
async fn stringin_inp_at_menu_field_delivers_the_choice() {
    let db = build(
        r#"
        record(ai, "SRC2") { field(SCAN, "1 second") }
        record(stringin, "SI2") { field(INP, "SRC2.SCAN") }
        "#,
    )
    .await;
    process(&db, "SI2").await;
    assert_eq!(
        field_str(&db, "SI2", "VAL"),
        "1 second",
        "a DBF_MENU source read as DBR_STRING delivers its choice text"
    );
}

/// lsi INP, non-char source: `dbGetLinkLS`'s else-arm is DBR_STRING.
#[epics_macros_rs::epics_test]
async fn lsi_inp_at_enum_source_delivers_the_label() {
    let db = build(
        r#"
        record(bi, "SRC3") { field(ZNAM, "STOPPED") field(ONAM, "RUNNING") field(VAL, "1") }
        record(lsi, "LI") { field(INP, "SRC3") }
        "#,
    )
    .await;
    process(&db, "LI").await;
    assert_eq!(field_str(&db, "LI", "VAL"), "RUNNING");
}

/// lsi INP, CHAR-array source: `dbGetLinkLS`'s CHAR arm reads the bytes the
/// array spells — the other side of the source-class switch.
#[epics_macros_rs::epics_test]
async fn lsi_inp_at_char_array_source_reads_the_bytes() {
    let db = build(
        r#"
        record(waveform, "WFC") { field(FTVL, "CHAR") field(NELM, "16") }
        record(lsi, "LIC") { field(INP, "WFC") }
        "#,
    )
    .await;
    {
        let rec = db.get_record("WFC").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field_internal("VAL", EpicsValue::CharArray(b"hello".to_vec()))
            .unwrap();
    }
    process(&db, "LIC").await;
    assert_eq!(
        field_str(&db, "LIC", "VAL"),
        "hello",
        "a DBF_CHAR source goes through the char-array arm, not DBR_STRING"
    );
}

/// stringout closed-loop DOL at a bi VAL renders ONAM.
#[epics_macros_rs::epics_test]
async fn stringout_dol_at_enum_source_delivers_the_label() {
    let db = build(
        r#"
        record(bi, "SRC4") { field(ZNAM, "STOPPED") field(ONAM, "RUNNING") field(VAL, "1") }
        record(stringout, "SO") { field(DOL, "SRC4") field(OMSL, "closed_loop") }
        "#,
    )
    .await;
    process(&db, "SO").await;
    assert_eq!(field_str(&db, "SO", "VAL"), "RUNNING");
}

/// lso closed-loop DOL at a bi VAL renders ONAM (`dbGetLinkLS` else-arm).
#[epics_macros_rs::epics_test]
async fn lso_dol_at_enum_source_delivers_the_label() {
    let db = build(
        r#"
        record(bi, "SRC5") { field(ZNAM, "STOPPED") field(ONAM, "RUNNING") field(VAL, "1") }
        record(lso, "LO") { field(DOL, "SRC5") field(OMSL, "closed_loop") }
        "#,
    )
    .await;
    process(&db, "LO").await;
    assert_eq!(field_str(&db, "LO", "VAL"), "RUNNING");
}

/// printf `%s` renders the label; `%d` on the SAME source keeps the index —
/// the per-conversion request boundary (`printfRecord.c:291` vs `:129`).
#[epics_macros_rs::epics_test]
async fn printf_string_conversion_gets_the_label_numeric_keeps_the_index() {
    let db = build(
        r#"
        record(bi, "SRC6") { field(ZNAM, "STOPPED") field(ONAM, "RUNNING") field(VAL, "1") }
        record(printf, "PFS") { field(FMT, "%s") field(INP0, "SRC6") }
        record(printf, "PFD") { field(FMT, "%d") field(INP0, "SRC6") }
        "#,
    )
    .await;
    process(&db, "PFS").await;
    process(&db, "PFD").await;
    assert_eq!(
        field_str(&db, "PFS", "VAL"),
        "RUNNING",
        "a %s slot is read with DBR_STRING"
    );
    assert_eq!(
        field_str(&db, "PFD", "VAL"),
        "1",
        "a numeric slot keeps the native index"
    );
}

/// stringin SIMM=YES: the SIOL read carries the same DBR_STRING request.
#[epics_macros_rs::epics_test]
async fn stringin_siol_at_enum_source_delivers_the_label() {
    let db = build(
        r#"
        record(bi, "SRC7") { field(ZNAM, "STOPPED") field(ONAM, "RUNNING") field(VAL, "1") }
        record(stringin, "SIM") { field(SIMM, "YES") field(SIOL, "SRC7") }
        "#,
    )
    .await;
    process(&db, "SIM").await;
    assert_eq!(
        field_str(&db, "SIM", "VAL"),
        "RUNNING",
        "stringinRecord.c:208 reads SIOL with DBR_STRING into SVAL"
    );
}

/// An external enum link set: the label table rides
/// `LinkMetadata::enum_choices` (C `dbCa`'s second DBR_STRING monitor,
/// `pgetString`) — a bare wire index still renders as its label.
struct EnumLset {
    value: EpicsValue,
    metadata: LinkMetadata,
    last_put: Arc<Mutex<Option<EpicsValue>>>,
}

#[epics_base_rs::async_trait]
impl LinkSet for EnumLset {
    fn is_connected(&self, _name: &str) -> bool {
        true
    }
    fn get_cached_value(&self, _name: &str) -> Option<EpicsValue> {
        Some(self.value.clone())
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
    async fn put_value(
        &self,
        _name: &str,
        value: EpicsValue,
        _op: LinkPutOp,
    ) -> Result<(), String> {
        *self.last_put.lock().unwrap() = Some(value);
        Ok(())
    }
    fn link_metadata(&self, _name: &str) -> Option<LinkMetadata> {
        Some(self.metadata.clone())
    }
}

/// stringin INP at a `ca://` enum channel: the wire value is a bare index;
/// the cached CTRL label table renders it.
#[epics_macros_rs::epics_test]
async fn stringin_inp_at_external_enum_renders_via_cached_choices() {
    let db = build(r#"record(stringin, "SIX") { field(INP, "ca://EXT:ENUM") }"#).await;
    db.register_link_set(
        "ca",
        Arc::new(EnumLset {
            value: EpicsValue::Enum(1),
            metadata: LinkMetadata {
                enum_choices: Some(vec!["off".into(), "on".into()]),
                ..Default::default()
            },
            last_put: Arc::new(Mutex::new(None)),
        }),
    )
    .await;
    process(&db, "SIX").await;
    assert_eq!(
        field_str(&db, "SIX", "VAL"),
        "on",
        "C dbCaGetLink(DBR_STRING) serves the enum label, not the index"
    );
}

/// Same external read WITHOUT a cached label table: the index digits are
/// all that can be rendered (C before its string monitor delivers).
#[epics_macros_rs::epics_test]
async fn stringin_inp_at_external_enum_without_choices_falls_back_to_digits() {
    let db = build(r#"record(stringin, "SIY") { field(INP, "ca://EXT:ENUM2") }"#).await;
    db.register_link_set(
        "ca",
        Arc::new(EnumLset {
            value: EpicsValue::Enum(1),
            metadata: LinkMetadata::default(),
            last_put: Arc::new(Mutex::new(None)),
        }),
    )
    .await;
    process(&db, "SIY").await;
    assert_eq!(field_str(&db, "SIY", "VAL"), "1");
}
