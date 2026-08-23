//! **A `DBF_CHAR` array widens through `epicsInt8` for every target.**
//!
//! C's get table gives `DBF_CHAR` one row per destination and they are all the
//! same macro: `getCharEnum` is `GET(char, epicsEnum16)`
//! (`dbConvert.c:491`), installed in `dbGetConvertRoutine`'s `DBF_CHAR` row at
//! `:1716`, and the `GET` body (`:63-80`) is `typea *psrc = (typea *)
//! paddr->pfield; ... *pdst++ = (typeb) *psrc++;` with `psrc` a SIGNED `char`.
//! So `(epicsEnum16)(char)0xC8` is `(epicsEnum16)(-56)` = 65480. The put table
//! agrees (`putCharEnum PUT(char, epicsEnum16)`, `:1304`).
//!
//! The port's `convert_to` promoted `CharArray` through `as i8` for eight of
//! its nine numeric targets and forgot the ninth, so `DBF_CHAR -> DBF_ENUM`
//! landed 200 where C lands 65480 — and where the port's own `DBF_CHAR ->
//! DBF_USHORT` already landed 65480, so two neighbouring `FTVL`s disagreed
//! with each other only in the port.
//!
//! The scalar row for the same conversion never had the bug (`Char` is absent
//! from `as_int_i64`, so it falls through `to_f64`, which reads the byte
//! signed), which is why the array/scalar disagreement below is the sharpest
//! statement of the defect: one byte, one type, two answers.
//!
//! This is NOT closed by the CA write-path fix (wire `DBR_CHAR` now classifies
//! as unsigned, so CA stops reaching this arm); the `DBF_CHAR -> DBF_ENUM`
//! LINK path still does.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// The byte the reviewer's `caput -S SRC '\310'` puts in a `DBF_CHAR` field.
const BYTE: u8 = 0xC8;
/// `(epicsEnum16)(char)0xC8`.
const AS_ENUM: u16 = 65480;

const DB: &str = r#"
record(waveform,"SRC"){ field(FTVL,"CHAR")   field(NELM,"4") }
record(waveform,"DST"){ field(FTVL,"ENUM")   field(NELM,"4") field(INP,"SRC") }
record(waveform,"USH"){ field(FTVL,"USHORT") field(NELM,"4") field(INP,"SRC") }
"#;

fn one_byte() -> EpicsValue {
    EpicsValue::CharArray(vec![BYTE])
}

#[test]
fn every_numeric_target_promotes_the_byte_through_epicsint8() {
    // One case per row of C's `DBF_CHAR` column, so a row that loses its sign
    // extension is named individually rather than hidden behind a sibling.
    assert_eq!(
        one_byte().convert_to(DbFieldType::Enum),
        EpicsValue::EnumArray(vec![AS_ENUM])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::UShort),
        EpicsValue::UShortArray(vec![AS_ENUM])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::Short),
        EpicsValue::ShortArray(vec![-56])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::Long),
        EpicsValue::LongArray(vec![-56])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::ULong),
        EpicsValue::ULongArray(vec![0xFFFF_FFC8])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::Double),
        EpicsValue::DoubleArray(vec![-56.0])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::Float),
        EpicsValue::FloatArray(vec![-56.0])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::Int64),
        EpicsValue::Int64Array(vec![-56])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::UInt64),
        EpicsValue::UInt64Array(vec![0xFFFF_FFFF_FFFF_FFC8])
    );
}

#[test]
fn the_byte_carriers_stay_byte_identical() {
    // The two non-widening rows: C `charToUchar` is a cast, not a promotion,
    // and the text row keeps the buffer verbatim. Pinned so the `as i8` added
    // to the Enum row is not "helpfully" spread onto them.
    assert_eq!(
        one_byte().convert_to(DbFieldType::UChar),
        EpicsValue::UCharArray(vec![BYTE])
    );
    assert_eq!(
        one_byte().convert_to(DbFieldType::Char),
        EpicsValue::CharArray(vec![BYTE])
    );
}

#[test]
fn a_one_element_array_agrees_with_the_scalar() {
    // The disagreement the missing cast created: the scalar row reads the byte
    // signed via `to_f64`, so an array of one must land the same number.
    let scalar = EpicsValue::Char(BYTE).convert_to(DbFieldType::Enum);
    assert_eq!(scalar, EpicsValue::Enum(AS_ENUM));
    let array = one_byte().convert_to(DbFieldType::Enum);
    assert_eq!(array, EpicsValue::EnumArray(vec![AS_ENUM]));
}

#[epics_macros_rs::epics_test]
async fn a_char_waveform_linked_into_an_enum_waveform_lands_signed() {
    // The path that survives the CA write-path fix: the array-landing owner
    // `waveform.rs` `value.convert_to(self.ftvl_element_type())`, reached
    // through a DB link rather than the wire.
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .expect("parse db")
        .build()
        .await
        .expect("build ioc");
    db.put_pv("SRC", EpicsValue::CharArray(vec![BYTE]))
        .await
        .expect("seed SRC");

    process(&db, "DST").await;
    process(&db, "USH").await;

    assert_eq!(
        db.get_pv("DST").expect("DST"),
        EpicsValue::EnumArray(vec![AS_ENUM]),
        "FTVL=ENUM must land what C's getCharEnum lands"
    );
    assert_eq!(
        db.get_pv("USH").expect("USH"),
        EpicsValue::UShortArray(vec![AS_ENUM]),
        "the neighbouring FTVL must not disagree with it"
    );
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .expect("process");
}
