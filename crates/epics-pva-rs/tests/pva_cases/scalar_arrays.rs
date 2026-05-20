//! pvxs scalar-array byte-exact reproduction.
//!
//! pvxs reference: `src/pvaproto.h:477` `to_wire(shared_array)` — the
//! length prefix followed by N elements in negotiated byte order.
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (captured from pvxs's own `to_wire(shared_array<E>)` at run time).
//! Touched by MR-R25 (DBF_UINT64 arr-filter slicing) — the ULong
//! array fixture in particular locks the contract for the
//! UInt64Array waveform path.

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn encode_array(items: Vec<ScalarValue>, st: ScalarType, order: ByteOrder) -> String {
    let mut out = Vec::new();
    encode_pv_field(
        &PvField::ScalarArray(items),
        &FieldDesc::ScalarArray(st),
        order,
        &mut out,
    );
    hex(&out)
}

#[test]
fn golden_pvxs_scalar_array_empty_int() {
    assert_eq!(
        encode_array(vec![], ScalarType::Int, ByteOrder::Big),
        golden("scalar_array_empty_int"),
    );
}

#[test]
fn golden_pvxs_scalar_array_bool() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Boolean(true), ScalarValue::Boolean(false)],
            ScalarType::Boolean,
            ByteOrder::Big,
        ),
        golden("scalar_array_bool"),
    );
}

#[test]
fn golden_pvxs_scalar_array_byte() {
    assert_eq!(
        encode_array(
            vec![
                ScalarValue::Byte(-1),
                ScalarValue::Byte(0),
                ScalarValue::Byte(1),
            ],
            ScalarType::Byte,
            ByteOrder::Big,
        ),
        golden("scalar_array_byte"),
    );
}

#[test]
fn golden_pvxs_scalar_array_ubyte() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::UByte(0xAA), ScalarValue::UByte(0xBB)],
            ScalarType::UByte,
            ByteOrder::Big,
        ),
        golden("scalar_array_ubyte"),
    );
}

#[test]
fn golden_pvxs_scalar_array_short_be() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Short(0x1234), ScalarValue::Short(0x5678)],
            ScalarType::Short,
            ByteOrder::Big,
        ),
        golden("scalar_array_short_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_short_le() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Short(0x1234), ScalarValue::Short(0x5678)],
            ScalarType::Short,
            ByteOrder::Little,
        ),
        golden("scalar_array_short_le"),
    );
}

#[test]
fn golden_pvxs_scalar_array_ushort_be() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::UShort(0xABCD)],
            ScalarType::UShort,
            ByteOrder::Big,
        ),
        golden("scalar_array_ushort_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_int_be() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Int(1), ScalarValue::Int(2)],
            ScalarType::Int,
            ByteOrder::Big,
        ),
        golden("scalar_array_int_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_int_le() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Int(1), ScalarValue::Int(2)],
            ScalarType::Int,
            ByteOrder::Little,
        ),
        golden("scalar_array_int_le"),
    );
}

#[test]
fn golden_pvxs_scalar_array_uint_be() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::UInt(0xDEADBEEF)],
            ScalarType::UInt,
            ByteOrder::Big,
        ),
        golden("scalar_array_uint_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_long_be() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Long(0x0102_0304_0506_0708)],
            ScalarType::Long,
            ByteOrder::Big,
        ),
        golden("scalar_array_long_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_ulong_be() {
    // MR-R25 contract — the UInt64Array waveform path round-trips
    // through this wire shape.
    assert_eq!(
        encode_array(
            vec![ScalarValue::ULong(0xFFEE_DDCC_BBAA_9988)],
            ScalarType::ULong,
            ByteOrder::Big,
        ),
        golden("scalar_array_ulong_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_float_be() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Float(1.0)],
            ScalarType::Float,
            ByteOrder::Big,
        ),
        golden("scalar_array_float_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_double_be() {
    assert_eq!(
        encode_array(
            vec![ScalarValue::Double(1.0)],
            ScalarType::Double,
            ByteOrder::Big,
        ),
        golden("scalar_array_double_be"),
    );
}

#[test]
fn golden_pvxs_scalar_array_string() {
    assert_eq!(
        encode_array(
            vec![
                ScalarValue::String("hi".into()),
                ScalarValue::String("world".into()),
            ],
            ScalarType::String,
            ByteOrder::Big,
        ),
        golden("scalar_array_string"),
    );
}

#[test]
fn golden_pvxs_scalar_array_string_utf8() {
    // ["a", "안"] — mixed ASCII + 3-byte UTF-8. The per-element
    // Size counts bytes, not characters.
    assert_eq!(
        encode_array(
            vec![
                ScalarValue::String("a".into()),
                ScalarValue::String("안".into()),
            ],
            ScalarType::String,
            ByteOrder::Big,
        ),
        golden("scalar_array_string_utf8"),
    );
}
