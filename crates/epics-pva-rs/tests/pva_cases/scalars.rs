//! Scalar `encode_pv_field` byte shapes — the baseline pvxs
//! contract that R13 / R10 family rely on.
//!
//! pvxs reference: `src/dataencode.cpp::to_wire_field` (lines
//! ~110-340 for scalar arms). The wire output of a scalar is
//! exactly the type-natural in-memory layout in the negotiated
//! byte order — no per-value header, no presence byte, no
//! padding. These golden tests pin that contract.

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_pvxs_scalar_int_be() {
    // pvxs to_wire_field(Int32) — 4-byte big-endian.
    let v = PvField::Scalar(ScalarValue::Int(0x0123_4567));
    let desc = FieldDesc::Scalar(ScalarType::Int);
    let mut out = Vec::new();
    encode_pv_field(&v, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), "01234567", "Int BE");
}

#[test]
fn golden_pvxs_scalar_int_le() {
    let v = PvField::Scalar(ScalarValue::Int(0x0123_4567));
    let desc = FieldDesc::Scalar(ScalarType::Int);
    let mut out = Vec::new();
    encode_pv_field(&v, &desc, ByteOrder::Little, &mut out);
    assert_eq!(hex(&out), "67452301", "Int LE");
}

#[test]
fn golden_pvxs_scalar_double_be() {
    // 1.0 = 0x3FF0_0000_0000_0000.
    let v = PvField::Scalar(ScalarValue::Double(1.0));
    let desc = FieldDesc::Scalar(ScalarType::Double);
    let mut out = Vec::new();
    encode_pv_field(&v, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), "3ff0000000000000", "Double 1.0 BE");
}

#[test]
fn golden_pvxs_scalar_string_be() {
    // pvxs `to_wire(string)`: Size-prefix (1 byte for len < 254)
    // + raw bytes, no NUL.
    let v = PvField::Scalar(ScalarValue::String("hi".into()));
    let desc = FieldDesc::Scalar(ScalarType::String);
    let mut out = Vec::new();
    encode_pv_field(&v, &desc, ByteOrder::Big, &mut out);
    // 02 68 69 — Size(2) + 'h' + 'i'.
    assert_eq!(hex(&out), "026869", "String 'hi' BE");
}

// ── extended scalar coverage (every ScalarType, both byte orders
//    where applicable) ───────────────────────────────────────────────

fn encode(value: PvField, desc: FieldDesc, order: ByteOrder) -> String {
    let mut out = Vec::new();
    encode_pv_field(&value, &desc, order, &mut out);
    hex(&out)
}

#[test]
fn golden_pvxs_scalar_bool_true() {
    // pvxs Boolean = single byte (0 or 1); endian-invariant.
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Boolean(true)),
            FieldDesc::Scalar(ScalarType::Boolean),
            ByteOrder::Big,
        ),
        "01"
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
        "00"
    );
}

#[test]
fn golden_pvxs_scalar_byte_neg1() {
    // i8 -1 = 0xFF (endian-invariant).
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Byte(-1)),
            FieldDesc::Scalar(ScalarType::Byte),
            ByteOrder::Big,
        ),
        "ff"
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
        "ab"
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
        "1234"
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
        "3412"
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
        "abcd"
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
        "deadbeef"
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
        "efbeadde"
    );
}

#[test]
fn golden_pvxs_scalar_long_be() {
    // i64 0x0102030405060708 BE.
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Long(0x0102_0304_0506_0708)),
            FieldDesc::Scalar(ScalarType::Long),
            ByteOrder::Big,
        ),
        "0102030405060708"
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
        "0807060504030201"
    );
}

#[test]
fn golden_pvxs_scalar_ulong_be() {
    // MR-R25: DBF_UINT64 round-trip. pvxs encodes ULong as u64 BE.
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::ULong(0xFFEE_DDCC_BBAA_9988)),
            FieldDesc::Scalar(ScalarType::ULong),
            ByteOrder::Big,
        ),
        "ffeeddccbbaa9988"
    );
}

#[test]
fn golden_pvxs_scalar_float_be() {
    // IEEE 754 binary32, 1.0 = 0x3F800000.
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Float(1.0)),
            FieldDesc::Scalar(ScalarType::Float),
            ByteOrder::Big,
        ),
        "3f800000"
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
        "0000803f"
    );
}

#[test]
fn golden_pvxs_scalar_double_le() {
    // 1.0 LE = 00 00 00 00 00 00 F0 3F.
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::Double(1.0)),
            FieldDesc::Scalar(ScalarType::Double),
            ByteOrder::Little,
        ),
        "000000000000f03f"
    );
}

#[test]
fn golden_pvxs_scalar_string_empty() {
    // pvxs string(""): Size(0) = single 0x00; no data bytes.
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String(String::new())),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        "00"
    );
}

#[test]
fn golden_pvxs_scalar_string_253_last_single_byte_size() {
    // 253-byte string: Size still fits in 1 byte (0xFD).
    let s = "x".repeat(253);
    let mut expected = String::from("fd");
    expected.push_str(&"78".repeat(253));
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String(s)),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        expected
    );
}

#[test]
fn golden_pvxs_scalar_string_254_extended_size_be() {
    // 254-byte string: first length to use the 5-byte extended form.
    // Size = 0xFE + u32_be(254) = "fe000000fe".
    let s = "x".repeat(254);
    let mut expected = String::from("fe000000fe");
    expected.push_str(&"78".repeat(254));
    assert_eq!(
        encode(
            PvField::Scalar(ScalarValue::String(s)),
            FieldDesc::Scalar(ScalarType::String),
            ByteOrder::Big,
        ),
        expected
    );
}
