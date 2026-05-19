//! pvxs `Size` encoding boundary contract — the 1-byte / 5-byte
//! wire form transition is where wire-encoder regressions love to
//! hide. A writer that uses `0xFF` as a sentinel desyncs a pvxs
//! peer at exactly the boundary covered here.
//!
//! pvxs reference: `pvaproto.h` `to_wire(Size)` / `from_wire(Size)`:
//!
//! - `0..=253`        → single byte (raw value)
//! - `254..=u32::MAX` → `0xFE` + `u32` in negotiated byte order
//! - null sentinel    → `0xFF` (nullable strings / unselected variant)

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::proto::size::encode_size;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_pvxs_size_0() {
    // Single 0x00; endian-invariant in the 1-byte form.
    assert_eq!(hex(&encode_size(0, ByteOrder::Big)), "00");
    assert_eq!(hex(&encode_size(0, ByteOrder::Little)), "00");
}

#[test]
fn golden_pvxs_size_253_last_single_byte() {
    // Last value that still fits in the single-byte form.
    assert_eq!(hex(&encode_size(253, ByteOrder::Big)), "fd");
    assert_eq!(hex(&encode_size(253, ByteOrder::Little)), "fd");
}

#[test]
fn golden_pvxs_size_254_extended_be() {
    // First length to use the 5-byte extended form: 0xFE + u32_be.
    assert_eq!(hex(&encode_size(254, ByteOrder::Big)), "fe000000fe");
}

#[test]
fn golden_pvxs_size_254_extended_le() {
    // Same value, little-endian u32: FE FE 00 00 00.
    assert_eq!(hex(&encode_size(254, ByteOrder::Little)), "fefe000000");
}

#[test]
fn golden_pvxs_size_65535_be() {
    assert_eq!(hex(&encode_size(65535, ByteOrder::Big)), "fe0000ffff");
}

#[test]
fn golden_pvxs_size_65536_be() {
    assert_eq!(hex(&encode_size(65536, ByteOrder::Big)), "fe00010000");
}

#[test]
fn golden_pvxs_size_max_u31_be() {
    // 0x7FFF_FFFF — exercises the high bit clearance in the u32 path.
    assert_eq!(hex(&encode_size(0x7FFF_FFFF, ByteOrder::Big)), "fe7fffffff");
}
