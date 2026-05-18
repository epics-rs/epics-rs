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
