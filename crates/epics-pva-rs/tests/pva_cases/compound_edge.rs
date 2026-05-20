//! Compound-array per-element edges that the "two present" goldens
//! in `compound_arrays.rs` leave uncovered.
//!
//! pvxs reference: `src/dataencode.cpp` StructA/UnionA/AnyA encode
//! (`:354-393`). Per element the wire begins with a presence byte:
//! `0x00` = null (no body follows); `0x01` = present (body
//! follows).
//!
//! ## Known Rust encoder gap (not regressed — surfaced)
//!
//! pvxs distinguishes "null element" (presence `0x00`, no body)
//! from "present with null body" (presence `0x01` followed by a
//! `Size`-null `0xFF` selector or descriptor). The current Rust
//! encoder always emits `0x01` (present) for each array element,
//! routing null cases through the inner `0xFF` sentinel. So the
//! pvxs fixtures
//!
//! - `struct_array_all_null`
//! - `struct_array_present_null_present`
//! - `union_array_null_element`
//! - `variant_array_null_descriptor`
//!
//! are committed in `tools/pvxs-golden-capture/fixtures.txt` as
//! the wire-shape reference but **not yet asserted in Rust** —
//! the encoder would need `Option<Element>` modelling to produce
//! the matching bytes. When that lands, drop the `#[ignore]`s
//! below and the existing fixture bytes pin the contract.

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_pvxs_union_array_empty() {
    // The zero-element case the encoder CAN already express:
    // Size(0) only, no per-element bytes.
    let desc = FieldDesc::UnionArray {
        struct_id: String::new(),
        variants: vec![("i".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    let val = PvField::UnionArray(vec![]);
    let mut out = Vec::new();
    encode_pv_field(&val, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("union_array_empty"));
}

#[test]
#[ignore = "Rust encoder always emits presence 0x01; pvxs `0x00` null-element \
            shape needs Option<PvStructure> modelling — see module docs"]
fn golden_pvxs_struct_array_all_null() {
    // Pinned by tools/pvxs-golden-capture/fixtures.txt: "03000000"
    // (Size(3) + three 0x00 null presence bytes).
    let _expected = golden("struct_array_all_null");
}

#[test]
#[ignore = "Rust encoder always emits presence 0x01; pvxs null-element shape \
            needs Option<PvStructure> modelling — see module docs"]
fn golden_pvxs_struct_array_present_null_present() {
    let _expected = golden("struct_array_present_null_present");
}

#[test]
#[ignore = "Rust encoder always emits presence 0x01; pvxs `0x00` null-element \
            shape needs Option<UnionItem> modelling — see module docs"]
fn golden_pvxs_union_array_null_element() {
    let _expected = golden("union_array_null_element");
}

#[test]
#[ignore = "Rust encoder always emits presence 0x01; pvxs `0x00` null-element \
            shape needs Option<VariantValue> modelling — see module docs"]
fn golden_pvxs_variant_array_null_descriptor() {
    let _expected = golden("variant_array_null_descriptor");
}
