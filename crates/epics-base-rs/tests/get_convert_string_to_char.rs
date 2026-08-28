//! A `DBF_STRING` field read as `DBR_CHAR` is parsed, not handed over as text.
//!
//! C `cvt_st_c` (`dbFastLinkConv.c:91-101`) is the `[DBF_STRING][DBR_CHAR]`
//! row of `dbFastGetConvertRoutine` (`:1645`), and the scalar fast path
//! (`dbAccess.c:964-974`) always selects it for a string field, whose
//! `no_elements` is 1:
//!
//! ```c
//! if (*from == 0) { *to = 0; return 0; }
//! return epicsParseInt8(from, to, dbConvertBase, &end);
//! ```
//!
//! So `VAL="65"` reads as the single byte `0x41`, and `VAL="hello"` is a
//! non-zero status, not the byte `0x68`. The empty-string carve-out comes
//! first and is a SUCCESSFUL zero — the put row (`putStringChar`) has no such
//! test, which is the asymmetry that keeps the two directions apart.
//!
//! Handing back the text bytes is `convert_to`'s job: an `FTVL=CHAR` waveform
//! put of `"hello"` stores those five bytes, and that stays untouched here.

use epics_base_rs::error::CaError;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

fn s(v: &str) -> EpicsValue {
    EpicsValue::String(PvString::from(v.to_string()))
}

#[test]
fn string_get_converts_to_the_parsed_byte() {
    assert_eq!(
        s("65").get_convert(DbFieldType::Char).unwrap(),
        EpicsValue::Char(65)
    );
    assert_eq!(
        s("65").get_convert(DbFieldType::UChar).unwrap(),
        EpicsValue::UChar(65)
    );
    // `dbConvertBase == 0`, so the prefixes strtol honours are honoured here.
    assert_eq!(
        s("0x41").get_convert(DbFieldType::Char).unwrap(),
        EpicsValue::Char(65)
    );
}

#[test]
fn negative_and_band_edges_follow_epics_parse_int8() {
    assert_eq!(
        s("-1").get_convert(DbFieldType::Char).unwrap(),
        EpicsValue::Char(0xFF)
    );
    assert_eq!(
        s("127").get_convert(DbFieldType::Char).unwrap(),
        EpicsValue::Char(127)
    );
    // Outside epicsInt8's range: `epicsParseInt8` returns S_stdlib_overflow.
    assert!(matches!(
        s("128").get_convert(DbFieldType::Char),
        Err(CaError::GetConvertFailed(_))
    ));
    // The unsigned row admits a negative and truncates (epicsStdlib.c:238).
    assert_eq!(
        s("-1").get_convert(DbFieldType::UChar).unwrap(),
        EpicsValue::UChar(255)
    );
}

#[test]
fn the_empty_string_reads_as_a_successful_zero() {
    assert_eq!(
        s("").get_convert(DbFieldType::Char).unwrap(),
        EpicsValue::Char(0)
    );
    assert_eq!(
        s("").get_convert(DbFieldType::UChar).unwrap(),
        EpicsValue::UChar(0)
    );
    // Whitespace is not the empty string C tests for: `*from` is ' ', so the
    // parse runs and refuses.
    assert!(matches!(
        s("   ").get_convert(DbFieldType::Char),
        Err(CaError::GetConvertFailed(_))
    ));
}

#[test]
fn unparseable_text_is_a_get_failure_not_its_first_byte() {
    assert!(matches!(
        s("hello").get_convert(DbFieldType::Char),
        Err(CaError::GetConvertFailed(_))
    ));
    assert!(matches!(
        s("hello").get_convert(DbFieldType::UChar),
        Err(CaError::GetConvertFailed(_))
    ));
}

#[test]
fn a_string_waveform_parses_element_by_element() {
    let src = EpicsValue::StringArray(vec![
        PvString::from("65".to_string()),
        PvString::from("".to_string()),
        PvString::from("-2".to_string()),
    ]);
    assert_eq!(
        src.get_convert(DbFieldType::Char).unwrap(),
        EpicsValue::CharArray(vec![65, 0, 0xFE])
    );
    // The first failing element aborts the whole get, as C returns the status
    // from inside `getStringChar`'s loop.
    let bad = EpicsValue::StringArray(vec![
        PvString::from("65".to_string()),
        PvString::from("nope".to_string()),
    ]);
    assert!(matches!(
        bad.get_convert(DbFieldType::Char),
        Err(CaError::GetConvertFailed(_))
    ));
}

#[test]
fn the_text_byte_projection_stays_on_convert_to() {
    assert_eq!(
        s("hello").convert_to(DbFieldType::Char),
        EpicsValue::CharArray(b"hello".to_vec())
    );
}
