//! pvxs `Size` encoding boundary contract.
//!
//! pvxs reference: `src/pvaproto.h:266` `to_wire(Size)`:
//!   - `0..=253`        → single byte (raw value)
//!   - `254..=u32::MAX` → `0xFE` + `u32` in negotiated byte order
//!   - null sentinel    → `0xFF` (nullable strings / unselected variant)
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (captured from pvxs's own `to_wire(Size{n})` at run time).

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::proto::size::encode_size;

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_pvxs_size_0() {
    assert_eq!(hex(&encode_size(0, ByteOrder::Big)), golden("size_0_be"));
    assert_eq!(hex(&encode_size(0, ByteOrder::Little)), golden("size_0_le"));
}

#[test]
fn golden_pvxs_size_253_last_single_byte() {
    assert_eq!(
        hex(&encode_size(253, ByteOrder::Big)),
        golden("size_253_be")
    );
    assert_eq!(
        hex(&encode_size(253, ByteOrder::Little)),
        golden("size_253_le")
    );
}

#[test]
fn golden_pvxs_size_254_extended_be() {
    assert_eq!(
        hex(&encode_size(254, ByteOrder::Big)),
        golden("size_254_be")
    );
}

#[test]
fn golden_pvxs_size_254_extended_le() {
    assert_eq!(
        hex(&encode_size(254, ByteOrder::Little)),
        golden("size_254_le")
    );
}

#[test]
fn golden_pvxs_size_65535_be() {
    assert_eq!(
        hex(&encode_size(65535, ByteOrder::Big)),
        golden("size_65535_be")
    );
}

#[test]
fn golden_pvxs_size_65536_be() {
    assert_eq!(
        hex(&encode_size(65536, ByteOrder::Big)),
        golden("size_65536_be")
    );
}

#[test]
fn golden_pvxs_size_max_u31_be() {
    assert_eq!(
        hex(&encode_size(0x7FFF_FFFF, ByteOrder::Big)),
        golden("size_max_u31_be")
    );
}
