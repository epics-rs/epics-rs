//! A `DBF_STRING` field read as a numeric DBR carries C's parse STATUS, not a
//! substituted zero.
//!
//! `cvt_st_d` (`dbFastLinkConv.c:233-244`) and the array twin `getStringDouble`
//! (`dbConvert.c:392-414`) both end in `epicsParseFloat64`'s status, which
//! `dbChannel_get` turns into -1 (`db_access.c:816`); rsrv then zeroes the
//! payload, stamps `m_cid = ECA_GETFAIL` and commits (`camessage.c:534-550`).
//! So `stringin` VAL="hello" read as `DBR_DOUBLE` is a failed get — a CA-linked
//! calc goes LINK/INVALID — where a substituted `0.0` at NO_ALARM had the calc
//! quietly computing with A=0.
//!
//! The empty string keeps its carve-out: `if (*from == 0) { *to = 0; return 0; }`
//! runs first in every row, so an empty field reads as a successful zero.

use epics_base_rs::error::{CaError, CaOp};
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString, encode_dbr};

const DBR_DOUBLE: u16 = 6;

/// Every numeric row of C's `[DBF_STRING][*]` get table.
const NUMERIC: &[DbFieldType] = &[
    DbFieldType::Char,
    DbFieldType::UChar,
    DbFieldType::Short,
    DbFieldType::UShort,
    DbFieldType::Long,
    DbFieldType::ULong,
    DbFieldType::Int64,
    DbFieldType::UInt64,
    DbFieldType::Float,
    DbFieldType::Double,
];

fn s(v: &str) -> EpicsValue {
    EpicsValue::String(PvString::from(v.to_string()))
}

#[test]
fn a_numeric_string_still_reads_as_its_value() {
    assert_eq!(
        s("3.125").get_convert(DbFieldType::Double).unwrap(),
        EpicsValue::Double(3.125)
    );
    assert_eq!(
        s("-7").get_convert(DbFieldType::Long).unwrap(),
        EpicsValue::Long(-7)
    );
    // `dbConvertBase == 0` on the get side too.
    assert_eq!(
        s("0x10").get_convert(DbFieldType::Short).unwrap(),
        EpicsValue::Short(16)
    );
}

#[test]
fn unparseable_text_fails_the_get_for_every_numeric_row() {
    for &t in NUMERIC {
        assert!(
            matches!(s("hello").get_convert(t), Err(CaError::GetConvertFailed(_))),
            "{t:?} must report the parse status, not substitute a value"
        );
    }
}

#[test]
fn the_empty_string_reads_as_a_successful_zero_for_every_numeric_row() {
    for &t in NUMERIC {
        let got = s("")
            .get_convert(t)
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(
            got.to_f64(),
            Some(0.0),
            "{t:?}: C's `if (*from == 0)` carve-out is a successful zero"
        );
    }
}

#[test]
fn a_string_waveform_parses_element_by_element_and_aborts_on_the_first_failure() {
    let ok = EpicsValue::StringArray(vec![
        PvString::from("1.5".to_string()),
        PvString::from("".to_string()),
        PvString::from("-2".to_string()),
    ]);
    assert_eq!(
        ok.get_convert(DbFieldType::Double).unwrap(),
        EpicsValue::DoubleArray(vec![1.5, 0.0, -2.0])
    );
    let bad = EpicsValue::StringArray(vec![
        PvString::from("1.5".to_string()),
        PvString::from("nope".to_string()),
    ]);
    assert!(matches!(
        bad.get_convert(DbFieldType::Double),
        Err(CaError::GetConvertFailed(_))
    ));
}

#[test]
fn the_dbr_encoder_reports_the_failure_instead_of_encoding_a_zero() {
    let snap = epics_base_rs::server::snapshot::Snapshot::new(
        s("hello"),
        0,
        0,
        std::time::SystemTime::UNIX_EPOCH,
    );
    assert!(matches!(
        encode_dbr(DBR_DOUBLE, &snap),
        Err(CaError::GetConvertFailed(_))
    ));
    assert_eq!(
        CaError::GetConvertFailed(String::new()).to_eca_status(CaOp::Read),
        152,
        "ECA_GETFAIL = DEFMSG(CA_K_WARNING, 19)"
    );
}

#[test]
fn coercion_keeps_its_own_total_contract() {
    // `convert_to` is the put/projection direction and must stay total — the
    // link and record-storage callers depend on it.
    assert_eq!(
        s("hello").convert_to(DbFieldType::Double),
        EpicsValue::Double(0.0)
    );
}
