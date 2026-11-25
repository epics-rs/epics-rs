//! Scalar `encode_pv_field` byte shapes — the baseline pvxs
//! contract the compound-array and transport wire-shape tests rely on.
//!
//! pvxs reference: `src/dataencode.cpp::to_wire_field` (scalar arms,
//! ~lines 110-340). Wire output of a scalar is the type-natural
//! in-memory layout in the negotiated byte order — no per-value
//! header, no presence byte, no padding.
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (captured from pvxs's own `to_wire` at run time, not derived by
//! reading dataencode.cpp).

use epics_base_rs::types::PvString;
use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn encode(value: PvField, desc: FieldDesc, order: ByteOrder) -> String {
    let mut out = Vec::new();
    encode_pv_field(&value, &desc, order, &mut out);
    hex(&out)
}

// ── ints ──────────────────────────────────────────────────────────

#[test]
fn golden_pvxs_scalar_int_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Int(0x0123_4567)),
            FieldDesc::Scalar(ScalarType::Int),
            ByteOrder::Big,
        ),
        golden("scalar_int_be"),
    );
}

#[test]
fn golden_pvxs_scalar_int_le() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Int(0x0123_4567)),
            FieldDesc::Scalar(ScalarType::Int),
            ByteOrder::Little,
        ),
        golden("scalar_int_le"),
    );
}

#[test]
fn golden_pvxs_scalar_double_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Double(1.0)),
            FieldDesc::Scalar(ScalarType::Double),
            ByteOrder::Big,
        ),
        golden("scalar_double_be"),
    );
}

#[test]
fn golden_pvxs_scalar_string_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String("hi".into())),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        golden("scalar_string_hi_be"),
    );
}

// ── full scalar type coverage ─────────────────────────────────────

#[test]
fn golden_pvxs_scalar_bool_true() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Boolean(true)),
            FieldDesc::Scalar(ScalarType::Boolean),
            ByteOrder::Big,
        ),
        golden("scalar_bool_true"),
    );
}

#[test]
fn golden_pvxs_scalar_bool_false() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Boolean(false)),
            FieldDesc::Scalar(ScalarType::Boolean),
            ByteOrder::Big,
        ),
        golden("scalar_bool_false"),
    );
}

#[test]
fn golden_pvxs_scalar_byte_neg1() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Byte(-1)),
            FieldDesc::Scalar(ScalarType::Byte),
            ByteOrder::Big,
        ),
        golden("scalar_byte_neg1"),
    );
}

#[test]
fn golden_pvxs_scalar_ubyte() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::UByte(0xAB)),
            FieldDesc::Scalar(ScalarType::UByte),
            ByteOrder::Big,
        ),
        golden("scalar_ubyte_0xab"),
    );
}

#[test]
fn golden_pvxs_scalar_short_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Short(0x1234)),
            FieldDesc::Scalar(ScalarType::Short),
            ByteOrder::Big,
        ),
        golden("scalar_short_be"),
    );
}

#[test]
fn golden_pvxs_scalar_short_le() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Short(0x1234)),
            FieldDesc::Scalar(ScalarType::Short),
            ByteOrder::Little,
        ),
        golden("scalar_short_le"),
    );
}

#[test]
fn golden_pvxs_scalar_ushort_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::UShort(0xABCD)),
            FieldDesc::Scalar(ScalarType::UShort),
            ByteOrder::Big,
        ),
        golden("scalar_ushort_be"),
    );
}

#[test]
fn golden_pvxs_scalar_uint_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::UInt(0xDEADBEEF)),
            FieldDesc::Scalar(ScalarType::UInt),
            ByteOrder::Big,
        ),
        golden("scalar_uint_be"),
    );
}

#[test]
fn golden_pvxs_scalar_uint_le() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::UInt(0xDEADBEEF)),
            FieldDesc::Scalar(ScalarType::UInt),
            ByteOrder::Little,
        ),
        golden("scalar_uint_le"),
    );
}

#[test]
fn golden_pvxs_scalar_long_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Long(0x0102_0304_0506_0708)),
            FieldDesc::Scalar(ScalarType::Long),
            ByteOrder::Big,
        ),
        golden("scalar_long_be"),
    );
}

#[test]
fn golden_pvxs_scalar_long_le() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Long(0x0102_0304_0506_0708)),
            FieldDesc::Scalar(ScalarType::Long),
            ByteOrder::Little,
        ),
        golden("scalar_long_le"),
    );
}

#[test]
fn golden_pvxs_scalar_ulong_be() {
    // DBF_UINT64 round-trip locked here.
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::ULong(0xFFEE_DDCC_BBAA_9988)),
            FieldDesc::Scalar(ScalarType::ULong),
            ByteOrder::Big,
        ),
        golden("scalar_ulong_be"),
    );
}

#[test]
fn golden_pvxs_scalar_float_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Float(1.0)),
            FieldDesc::Scalar(ScalarType::Float),
            ByteOrder::Big,
        ),
        golden("scalar_float_be"),
    );
}

#[test]
fn golden_pvxs_scalar_float_le() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Float(1.0)),
            FieldDesc::Scalar(ScalarType::Float),
            ByteOrder::Little,
        ),
        golden("scalar_float_le"),
    );
}

#[test]
fn golden_pvxs_scalar_double_le() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Double(1.0)),
            FieldDesc::Scalar(ScalarType::Double),
            ByteOrder::Little,
        ),
        golden("scalar_double_le"),
    );
}

#[test]
fn golden_pvxs_scalar_string_empty() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String(PvString::new())),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        golden("scalar_string_empty"),
    );
}

#[test]
fn golden_pvxs_scalar_string_253_last_single_byte_size() {
    // 253-byte string: last length to fit in the 1-byte Size form.
    let s = "x".repeat(253);
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String(s.into())),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        golden("scalar_string_253"),
    );
}

#[test]
fn golden_pvxs_scalar_string_254_extended_size_be() {
    // 254-byte string: first length to use the 5-byte extended Size.
    let s = "x".repeat(254);
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String(s.into())),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        golden("scalar_string_254_be"),
    );
}

// ── floating-point special values ────────────────────────────────

#[test]
fn golden_pvxs_scalar_float_nan_be() {
    // f32::NAN may use a different bit pattern across compilers;
    // use the canonical quiet-NaN bits explicitly so the assertion
    // matches pvxs's 0x7FC00000.
    let v = f32::from_bits(0x7FC0_0000);
    assert!(v.is_nan());
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Float(v)),
            FieldDesc::Scalar(ScalarType::Float),
            ByteOrder::Big,
        ),
        golden("scalar_float_nan_be"),
    );
}

#[test]
fn golden_pvxs_scalar_float_inf_be() {
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Float(f32::INFINITY)),
            FieldDesc::Scalar(ScalarType::Float),
            ByteOrder::Big,
        ),
        golden("scalar_float_inf_be"),
    );
}

#[test]
fn golden_pvxs_scalar_double_neg_zero_be() {
    let v = f64::from_bits(0x8000_0000_0000_0000);
    assert!(v == 0.0 && v.is_sign_negative());
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Double(v)),
            FieldDesc::Scalar(ScalarType::Double),
            ByteOrder::Big,
        ),
        golden("scalar_double_neg_zero_be"),
    );
}

// ── UTF-8 multibyte strings ──────────────────────────────────────

#[test]
fn golden_pvxs_scalar_string_utf8_korean() {
    // "안녕" — 6 bytes UTF-8 (3 bytes per char). pvxs Size is
    // byte count, not char count; an ASCII-assuming encoder would
    // emit Size(2) and 4 stray bytes.
    let s = "안녕".to_string();
    assert_eq!(s.len(), 6, "test premise: 6 UTF-8 bytes");
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String(s.into())),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        golden("scalar_string_utf8_korean"),
    );
}
