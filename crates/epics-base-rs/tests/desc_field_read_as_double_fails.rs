//! The reviewer's simplest trigger for the get-conversion failure channel:
//!
//! ```text
//! record(ai,"A"){ field(DESC,"hello") }
//! $ caget -d DBR_TIME_DOUBLE A.DESC
//! ```
//!
//! `DESC` is `DBF_STRING`, so C's read goes through `getStringDouble`
//! (`dbConvert.c:392-414`) / `cvt_st_d` (`dbFastLinkConv.c:233-244`) and
//! `epicsParseFloat64` returns a status; `dbChannel_get` reports -1
//! (`db_access.c:816`) and rsrv answers ECA_GETFAIL with a zeroed payload
//! (`camessage.c:534-550`). Serving `A.DESC 0` at NO_ALARM instead is worse
//! than a wrong number: it is indistinguishable from a real zero.
//!
//! A compound DBR type is used deliberately. The TIME metadata is written into
//! the buffer BEFORE the value converts, so this also pins that a metadata-only
//! frame is never committed when the value cannot be produced.
//!
//! Both boundaries are covered, because C's carve-out (`if (*psrc == 0)
//! *pdst++ = 0;`) makes an EMPTY DESC a successful zero — the same rule
//! `getStringShort` (`dbConvert.c:211-233`) applies, which is what makes it a
//! general rule of the string-to-numeric get and not a per-function quirk.

use epics_base_rs::error::CaError;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString, encode_dbr};

const DBR_TIME_DOUBLE: u16 = 20;
const DBR_TIME_SHORT: u16 = 15;

fn ai_with_desc(desc: &str) -> RecordInstance {
    let mut inst = RecordInstance::new("A".to_string(), AiRecord::new(0.0));
    inst.put_common_field("DESC", EpicsValue::String(PvString::from(desc.to_string())))
        .expect("DESC accepts a string");
    inst
}

#[test]
fn a_non_numeric_desc_read_as_double_is_a_get_failure() {
    let inst = ai_with_desc("hello");
    let snap = inst
        .snapshot_for_field("DESC")
        .expect("DESC has a snapshot");
    assert_eq!(
        snap.value,
        EpicsValue::String(PvString::from("hello".to_string())),
        "DESC is served as DBF_STRING"
    );
    assert!(matches!(
        encode_dbr(DBR_TIME_DOUBLE, &snap),
        Err(CaError::GetConvertFailed(_))
    ));
    // The Short row of the same table, whose C routine the second reviewer
    // opened: `getStringShort` fails on the same input.
    assert!(matches!(
        encode_dbr(DBR_TIME_SHORT, &snap),
        Err(CaError::GetConvertFailed(_))
    ));
    assert!(matches!(
        snap.value.get_convert(DbFieldType::Short),
        Err(CaError::GetConvertFailed(_))
    ));
}

#[test]
fn a_numeric_desc_still_reads_as_its_value() {
    let inst = ai_with_desc("12.5");
    let snap = inst
        .snapshot_for_field("DESC")
        .expect("DESC has a snapshot");
    let data = encode_dbr(DBR_TIME_DOUBLE, &snap).expect("a numeric DESC converts");
    // DBR_TIME_DOUBLE: status(2) severity(2) secs(4) nsec(4) pad(4) value(8).
    assert_eq!(data.len(), 24);
    assert_eq!(f64::from_be_bytes(data[16..24].try_into().unwrap()), 12.5);
}

#[test]
fn an_empty_desc_reads_as_a_successful_zero() {
    let inst = ai_with_desc("");
    let snap = inst
        .snapshot_for_field("DESC")
        .expect("DESC has a snapshot");
    let data = encode_dbr(DBR_TIME_DOUBLE, &snap).expect("C's empty carve-out succeeds");
    assert_eq!(f64::from_be_bytes(data[16..24].try_into().unwrap()), 0.0);
    // DBR_TIME_SHORT: status(2) severity(2) secs(4) nsec(4) RISC_pad(2) value(2).
    let short = encode_dbr(DBR_TIME_SHORT, &snap).expect("the Short row carves out too");
    assert_eq!(short.len(), 16);
    assert_eq!(i16::from_be_bytes(short[14..16].try_into().unwrap()), 0);
}
