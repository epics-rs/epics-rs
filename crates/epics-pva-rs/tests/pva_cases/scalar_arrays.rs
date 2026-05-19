//! pvxs scalar-array byte-exact reproduction.
//!
//! pvxs reference: `src/dataencode.cpp` ScalarA arms — `to_wire(Size)`
//! length prefix followed by `to_wire(shared_array)` body
//! (`pvaproto.h:477`). Each element is the type-natural in-memory
//! layout in the negotiated byte order, with no per-element header.
//!
//! Touched by MR-R25 (DBF_UINT64 arr-filter slicing) — the
//! ULong-array golden in particular locks the contract for the
//! UInt64Array waveform path.

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};

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
    // Size(0) only — vacuous-type-match guard still selects the
    // ScalarArray arm; the encoder emits the length prefix.
    assert_eq!(encode_array(vec![], ScalarType::Int, ByteOrder::Big), "00");
}

#[test]
fn golden_pvxs_scalar_array_bool() {
    // Size(2) + 0x01 0x00 (true, false). Boolean wire = 1 byte each.
    assert_eq!(
        encode_array(
            vec![ScalarValue::Boolean(true), ScalarValue::Boolean(false)],
            ScalarType::Boolean,
            ByteOrder::Big,
        ),
        "020100"
    );
}

#[test]
fn golden_pvxs_scalar_array_byte() {
    // Size(3) + i8 bytes [-1, 0, 1] = FF 00 01.
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
        "03ff0001"
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
        "02aabb"
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
        "0212345678"
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
        "0234127856"
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
        "01abcd"
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
        "020000000100000002"
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
        "020100000002000000"
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
        "01deadbeef"
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
        "010102030405060708"
    );
}

#[test]
fn golden_pvxs_scalar_array_ulong_be() {
    // MR-R25 territory: the UInt64Array arr-filter slicing path
    // round-trips through this wire shape. A future encoder refactor
    // that drops the u64-BE element layout (or skips the Size prefix)
    // would surface here as a byte mismatch.
    assert_eq!(
        encode_array(
            vec![ScalarValue::ULong(0xFFEE_DDCC_BBAA_9988)],
            ScalarType::ULong,
            ByteOrder::Big,
        ),
        "01ffeeddccbbaa9988"
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
        "013f800000"
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
        "013ff0000000000000"
    );
}

#[test]
fn golden_pvxs_scalar_array_string() {
    // Per-element Size + raw bytes. ["hi", "world"]:
    // outer Size(2) + 02 'h' 'i' + 05 'w' 'o' 'r' 'l' 'd'.
    assert_eq!(
        encode_array(
            vec![
                ScalarValue::String("hi".into()),
                ScalarValue::String("world".into()),
            ],
            ScalarType::String,
            ByteOrder::Big,
        ),
        "0202686905776f726c64"
    );
}
