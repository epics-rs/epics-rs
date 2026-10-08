//! devBiSoftCallback and devMbbiSoftCallback read INP as `DBR_ENUM`. A
//! one-element `DBF_STRING` source converts through `cvt_st_e`, which calls
//! the SOURCE record's `put_enum_str` with the source's `dbAddr`: a state name
//! stores the source's VAL and leaves the reader's buffer unwritten, a number
//! below `no_str` is delivered, anything else fails. A longer string array
//! takes `dbGet`'s `getStringUshort` on element 0. Every expectation is
//! softIoc's, each reader read through the held (`NPP`) and general (`PP`)
//! paths.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const SOURCES: &str = r#"
record(bi, "BSRC") {
    field(ZNAM, "Low")
    field(ONAM, "High")
    field(VAL, "0")
    field(DESC, "1")
}
record(bi, "BSRC5") {
    field(ZNAM, "Low")
    field(ONAM, "High")
    field(DESC, "5")
}
record(mbbi, "MSRC") {
    field(ZRST, "zero")
    field(ONST, "one")
    field(TWST, "two")
    field(VAL, "0")
    field(DESC, "2")
}
record(stringin, "SI") {
    field(VAL, "High")
}
record(waveform, "W1") {
    field(FTVL, "STRING")
    field(NELM, "4")
}
record(waveform, "W2") {
    field(FTVL, "STRING")
    field(NELM, "4")
}
record(waveform, "W0") {
    field(FTVL, "STRING")
    field(NELM, "4")
}
"#;

const NO_ALARM: u16 = 0;
const READ: u16 = 1;

async fn ioc(rtype: &str, source: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(
            &format!(
                r#"{SOURCES}
record({rtype}, "HELD") {{
    field(DTYP, "Async Soft Channel")
    field(INP, "{source} NPP")
}}
record({rtype}, "GENERAL") {{
    field(DTYP, "Async Soft Channel")
    field(INP, "{source} PP")
}}"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let strings = |items: &[&str]| {
        EpicsValue::StringArray(items.iter().map(|s| PvString::from(*s)).collect())
    };
    db.put_pv("W1", strings(&["x"])).await.unwrap();
    db.put_pv("W2", strings(&["0x10", "x"])).await.unwrap();
    db.put_pv("W0", strings(&["7", "x"])).await.unwrap();
    db.put_pv("W0", strings(&[])).await.unwrap();
    db
}

/// `(VAL, STAT, UDF)` of `name`.
fn state(db: &PvDatabase, name: &str) -> (EpicsValue, u16, u8) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (
        inst.record.get_field("VAL").unwrap(),
        inst.common.stat,
        inst.common.udf,
    )
}

/// Process both readers of `source` and check each against `(val, stat)`.
async fn read_both(db: &PvDatabase, source: &str, val: u16, stat: u16) {
    for reader in ["HELD", "GENERAL"] {
        db.process_record(reader).await.unwrap();
        let (v, s, _) = state(db, reader);
        assert_eq!(
            (v, s),
            (EpicsValue::Enum(val), stat),
            "{reader} from {source}"
        );
    }
}

/// softIoc: `R_BONAM` VAL=0 NO_ALARM, and `BSRC` VAL 0 -> 1, UDF 0.
#[epics_macros_rs::epics_test]
async fn a_bi_state_name_stores_the_sources_val() {
    let db = ioc("bi", "BSRC.ONAM").await;
    read_both(&db, "BSRC.ONAM", 0, NO_ALARM).await;
    assert_eq!(state(&db, "BSRC"), (EpicsValue::Enum(1), NO_ALARM, 0));
}

/// softIoc: `R_MONST` VAL=0 NO_ALARM, and `MSRC` VAL 0 -> 1.
#[epics_macros_rs::epics_test]
async fn an_mbbi_state_name_stores_the_sources_val() {
    let db = ioc("mbbi", "MSRC.ONST").await;
    read_both(&db, "MSRC.ONST", 0, NO_ALARM).await;
    assert_eq!(state(&db, "MSRC").0, EpicsValue::Enum(1));
}

/// softIoc: `"1"` on a bi and `"2"` on an mbbi are below `no_str` and are
/// delivered; the source is not written. `"5"` is past a bi's two states.
#[epics_macros_rs::epics_test]
async fn a_number_below_no_str_is_delivered() {
    let db = ioc("bi", "BSRC.DESC").await;
    read_both(&db, "BSRC.DESC", 1, NO_ALARM).await;
    assert_eq!(state(&db, "BSRC").0, EpicsValue::Enum(0));
    let db = ioc("mbbi", "MSRC.DESC").await;
    read_both(&db, "MSRC.DESC", 2, NO_ALARM).await;
    assert_eq!(state(&db, "MSRC").0, EpicsValue::Enum(0));
    let db = ioc("bi", "BSRC5.DESC").await;
    read_both(&db, "BSRC5.DESC", 0, READ).await;
}

/// softIoc: a source with no `put_enum_str` (stringin, a one-element
/// waveform) fails the read.
#[epics_macros_rs::epics_test]
async fn a_source_without_put_enum_str_fails() {
    for source in ["SI", "W1"] {
        let db = ioc("mbbi", source).await;
        read_both(&db, source, 0, READ).await;
    }
}

/// softIoc: `["0x10","x"]` (NORD=2) parses element 0, `getStringUshort`.
#[epics_macros_rs::epics_test]
async fn a_longer_string_array_parses_element_0() {
    for rtype in ["bi", "mbbi"] {
        let db = ioc(rtype, "W2").await;
        read_both(&db, "W2", 16, NO_ALARM).await;
    }
}

/// softIoc: `W0` written `["7","x"]` then `[]` (NORD=0, element 0 still
/// "7") delivers the reader's buffer, NO_ALARM — the stale slot is not read.
#[epics_macros_rs::epics_test]
async fn an_empty_string_array_delivers_the_buffer() {
    for rtype in ["bi", "mbbi"] {
        let db = ioc(rtype, "W0").await;
        assert_eq!(
            db.get_pv("W0.NORD").unwrap().to_f64(),
            Some(0.0),
            "W0 is empty"
        );
        read_both(&db, "W0", 0, NO_ALARM).await;
    }
}

/// softIoc: the reader takes `["1","x"]` (1), VAL is then put to 0, and the
/// source emptied: the read delivers 1 — the dset's own buffer, not VAL.
#[epics_macros_rs::epics_test]
async fn the_buffer_is_the_dsets_not_val() {
    for rtype in ["bi", "mbbi"] {
        let db = ioc(rtype, "W2").await;
        let strings = |items: &[&str]| {
            EpicsValue::StringArray(items.iter().map(|s| PvString::from(*s)).collect())
        };
        db.put_pv("W2", strings(&["1", "x"])).await.unwrap();
        read_both(&db, "W2", 1, NO_ALARM).await;
        for reader in ["HELD", "GENERAL"] {
            db.put_pv(&format!("{reader}.VAL"), EpicsValue::Enum(0))
                .await
                .unwrap();
        }
        db.put_pv("W2", strings(&[])).await.unwrap();
        read_both(&db, "W2", 1, NO_ALARM).await;
    }
}

/// Sources whose `put_enum_str` reaches past the `no_str` table: a gap slot,
/// an empty name against an empty slot, SDEF from a state value alone, and no
/// states at all.
const SLOTS: &str = r#"
record(mbbi, "MGAP") {
    field(ZRST, "zero")
    field(TWST, "two")
    field(DESC, "two")
}
record(mbbi, "M2") {
    field(ZRST, "zero")
    field(ONST, "one")
}
record(bi, "B1") {
    field(ZNAM, "Low")
}
record(mbbi, "MV") {
    field(ZRVL, "1")
}
record(mbbi, "MNONE") {
}
"#;

/// `(source, reader type, reader STAT, source VAL)`, each softIoc's.
const SLOT_READS: [(&str, &str, u16, u16); 5] = [
    ("MGAP", "mbbi", NO_ALARM, 2),
    ("M2", "mbbi", NO_ALARM, 2),
    ("B1", "bi", NO_ALARM, 1),
    ("MV", "mbbi", NO_ALARM, 0),
    ("MNONE", "mbbi", READ, 0),
];

async fn slots_ioc() -> Arc<PvDatabase> {
    let readers: String = SLOT_READS
        .iter()
        .map(|(src, rtype, _, _)| {
            format!(
                r#"record({rtype}, "R_{src}") {{
    field(DTYP, "Async Soft Channel")
    field(INP, "{src}.DESC")
}}
"#
            )
        })
        .collect();
    let (db, _) = IocBuilder::new()
        .db_string(&format!("{SLOTS}{readers}"), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// softIoc: a name in a gap slot and an empty name both select the slot C's
/// `put_enum_str` scans to, past the `no_str` table; with no state defined
/// (SDEF=0) nothing matches and the read fails.
#[epics_macros_rs::epics_test]
async fn put_enum_str_scans_the_slots_c_scans() {
    let db = slots_ioc().await;
    for (src, _, stat, val) in SLOT_READS {
        let reader = format!("R_{src}");
        db.process_record(&reader).await.unwrap();
        assert_eq!(state(&db, &reader).1, stat, "{reader}");
        assert_eq!(state(&db, src).0, EpicsValue::Enum(val), "{src}");
    }
}

/// softIoc `dbpf <rec> ""`: the CA put route is the same `put_enum_str`.
#[epics_macros_rs::epics_test]
async fn an_empty_name_put_selects_the_first_empty_slot() {
    let db = slots_ioc().await;
    let empty = || EpicsValue::String(PvString::from(""));
    for (rec, val) in [("M2", 2), ("B1", 1), ("MV", 0)] {
        db.put_pv(rec, empty()).await.unwrap();
        assert_eq!(state(&db, rec).0, EpicsValue::Enum(val), "{rec}");
    }
    assert!(db.put_pv("MNONE", empty()).await.is_err());
}

/// softIoc: a name read from each source's DESC stores VAL 1 in all four,
/// but only bi's and mbbi's `put_enum_str` clear UDF; bo and mbbo keep 1.
#[epics_macros_rs::epics_test]
async fn only_bi_and_mbbi_put_enum_str_clear_udf() {
    let sources = [
        (
            "bo",
            "BO",
            r#"field(ZNAM, "Low") field(ONAM, "High") field(DESC, "High")"#,
            1,
        ),
        (
            "mbbo",
            "MO",
            r#"field(ZRST, "zero") field(ONST, "one") field(DESC, "one")"#,
            1,
        ),
        (
            "bi",
            "BI",
            r#"field(ZNAM, "Low") field(ONAM, "High") field(DESC, "High")"#,
            0,
        ),
        (
            "mbbi",
            "MI",
            r#"field(ZRST, "zero") field(ONST, "one") field(DESC, "one")"#,
            0,
        ),
    ];
    let text: String = sources
        .iter()
        .map(|(rtype, name, fields, _)| {
            let reader = if rtype.starts_with('m') { "mbbi" } else { "bi" };
            format!(
                r#"record({rtype}, "{name}") {{ {fields} }}
record({reader}, "R_{name}") {{
    field(DTYP, "Async Soft Channel")
    field(INP, "{name}.DESC")
}}
"#
            )
        })
        .collect();
    let (db, _) = IocBuilder::new()
        .db_string(&text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (_, name, _, udf) in sources {
        assert_eq!(state(&db, name).2, 1, "{name} starts undefined");
        db.process_record(&format!("R_{name}")).await.unwrap();
        let (val, _, got) = state(&db, name);
        assert_eq!((val, got), (EpicsValue::Enum(1), udf), "{name}");
    }
}

/// softIoc: after a read of `["5","x"]`, an INP put (same text) restarts the
/// buffer at 0, a DTYP round trip does not, and an emptied source then
/// delivers each reader's buffer.
#[epics_macros_rs::epics_test]
async fn an_inp_put_restarts_the_buffer_and_a_dtyp_put_does_not() {
    for rtype in ["bi", "mbbi"] {
        let db = ioc(rtype, "W2").await;
        let strings = |items: &[&str]| {
            EpicsValue::StringArray(items.iter().map(|s| PvString::from(*s)).collect())
        };
        let text = |s: &str| EpicsValue::String(PvString::from(s));
        db.put_pv("W2", strings(&["1", "x"])).await.unwrap();
        read_both(&db, "W2", 1, NO_ALARM).await;
        db.put_record_field_from_ca_no_notify("HELD", "INP", text("W2 NPP"))
            .await
            .unwrap();
        for dtyp in ["Soft Channel", "Async Soft Channel"] {
            db.put_record_field_from_ca_no_notify("GENERAL", "DTYP", text(dtyp))
                .await
                .unwrap();
        }
        db.put_pv("W2", strings(&[])).await.unwrap();
        for (reader, val) in [("HELD", 0), ("GENERAL", 1)] {
            db.process_record(reader).await.unwrap();
            let (v, s, _) = state(&db, reader);
            assert_eq!(
                (v, s),
                (EpicsValue::Enum(val), NO_ALARM),
                "{rtype} {reader}"
            );
        }
    }
}
