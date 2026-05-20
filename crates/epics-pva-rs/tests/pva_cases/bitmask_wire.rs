//! pvxs `BitMask` wire shape — `bitmask.h:257 to_wire(BitMask&)`.
//!
//! `Size(nbytes)` followed by `nbytes` of LSB-first bit packing,
//! with trailing zero bytes trimmed. Bit numbering is LSB-first
//! (bit 0 = LSB of word 0, bit 64 = LSB of word 1). Monitor
//! `changed` and `overrun` bitsets ride this shape on every event;
//! a 1-byte off-by-one in the trim corrupts every following
//! payload.
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (`to_wire(buf, BitMask{...})` at run time).

use epics_pva_rs::proto::{BitSet, ByteOrder};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn encode(bs: &BitSet, order: ByteOrder) -> String {
    let mut out = Vec::new();
    bs.write_into(order, &mut out);
    hex(&out)
}

#[test]
fn golden_pvxs_bitmask_empty_be() {
    let bs = BitSet::new();
    assert_eq!(encode(&bs, ByteOrder::Big), golden("bitmask_empty_be"));
}

#[test]
fn golden_pvxs_bitmask_bit0_be() {
    // First bit — single byte 0x01.
    let mut bs = BitSet::new();
    bs.set(0);
    assert_eq!(encode(&bs, ByteOrder::Big), golden("bitmask_bit0_be"));
}

#[test]
fn golden_pvxs_bitmask_bit7_be() {
    // Last bit in the first byte — single byte 0x80.
    let mut bs = BitSet::new();
    bs.set(7);
    assert_eq!(encode(&bs, ByteOrder::Big), golden("bitmask_bit7_be"));
}

#[test]
fn golden_pvxs_bitmask_bit8_be() {
    // First bit in the second byte — exercises the >8-bit boundary
    // that trims an off-by-one byte count if mishandled.
    let mut bs = BitSet::new();
    bs.set(8);
    assert_eq!(encode(&bs, ByteOrder::Big), golden("bitmask_bit8_be"));
}

#[test]
fn golden_pvxs_bitmask_bit64_be() {
    // First bit in the second u64 word — the boundary that
    // separates the byte-reversed-word path (BE) from the trailing
    // partial-byte path (storage order).
    let mut bs = BitSet::new();
    bs.set(64);
    assert_eq!(encode(&bs, ByteOrder::Big), golden("bitmask_bit64_be"));
}
