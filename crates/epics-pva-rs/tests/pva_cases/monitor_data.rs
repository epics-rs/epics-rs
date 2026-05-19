//! Monitor-data field ordering — the bitset-gated payload that
//! follows the `changed` bitset in `MONITOR` messages.
//!
//! Established convention (kodex 1×): PVA monitor data field order
//! is `changed → value → overrun`. This golden locks the encoder
//! portion (the value bytes that follow the changed bitset);
//! `overrun` is appended by the monitor framer in
//! `service::monitor`.
//!
//! pvxs reference: the monitor send loop in `src/serverConn.cpp`
//! writes the changed `BitSet` via `to_wire(BitSet)` and then
//! invokes the equivalent of `encode_pv_field_with_bitset` so that
//! only marked fields are serialized — unmarked leaves are skipped.

use epics_pva_rs::proto::{BitSet, ByteOrder};
use epics_pva_rs::pvdata::encode::encode_pv_field_with_bitset;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn one_int_struct() -> (FieldDesc, PvField) {
    let desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("v".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    let val = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![("v".into(), PvField::Scalar(ScalarValue::Int(0x0102_0304)))],
    });
    (desc, val)
}

#[test]
fn golden_pvxs_monitor_whole_struct_bit0_emits_all_fields() {
    // Bit 0 in pvxs depth-first numbering = the structure itself.
    // Setting it means "emit all descendants" — the Int leaf bytes.
    let (desc, val) = one_int_struct();
    let mut bs = BitSet::new();
    bs.set(0);
    let mut out = Vec::new();
    encode_pv_field_with_bitset(&val, &desc, &bs, 0, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), "01020304", "whole-struct bit → all leaves");
}

#[test]
fn golden_pvxs_monitor_empty_bitset_emits_no_leaves() {
    // No marks → no leaf bytes. (The framer still wrote the empty
    // bitset's bytes before this call.) Re-running here would be
    // wasted wire traffic; the encoder must suppress.
    let (desc, val) = one_int_struct();
    let bs = BitSet::new();
    let mut out = Vec::new();
    encode_pv_field_with_bitset(&val, &desc, &bs, 0, ByteOrder::Big, &mut out);
    assert!(out.is_empty(), "expected empty body, got {}", hex(&out));
}
