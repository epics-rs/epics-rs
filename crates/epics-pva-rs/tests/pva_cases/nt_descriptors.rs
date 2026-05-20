//! NormativeType type-descriptor wire bytes (`epics:nt/*` schemas).
//!
//! pvxs reference: `src/nt.cpp` NT*::build builders. Each Rust
//! `nt::*::build()` mirrors one. Locking these byte sequences
//! catches schema drift between the two ports — a Rust refactor
//! that reorders fields or drops `alarm` / `timeStamp` sub-
//! structures would surface here, not at a customer's IOC.
//!
//! All builders use their defaults (no display / control /
//! valueAlarm sub-structures); pvxs builds the same shape.

use epics_pva_rs::nt::nd_array;
use epics_pva_rs::nt::{NTEnum, NTScalar, NTTable, NTURI};
use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::FieldDesc;
use epics_pva_rs::pvdata::ScalarType;
use epics_pva_rs::pvdata::encode::encode_type_desc;

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn encode(desc: &FieldDesc, order: ByteOrder) -> String {
    let mut out = Vec::new();
    encode_type_desc(desc, order, &mut out);
    hex(&out)
}

#[test]
fn golden_pvxs_nt_scalar_int32_desc() {
    let d = NTScalar::new(ScalarType::Int).build();
    assert_eq!(encode(&d, ByteOrder::Big), golden("nt_scalar_int32_desc"));
}

#[test]
fn golden_pvxs_nt_scalar_array_double_desc() {
    let d = NTScalar::array(ScalarType::Double).build();
    assert_eq!(
        encode(&d, ByteOrder::Big),
        golden("nt_scalar_array_double_desc")
    );
}

#[test]
fn golden_pvxs_nt_enum_desc() {
    let d = NTEnum::new().build();
    assert_eq!(encode(&d, ByteOrder::Big), golden("nt_enum_desc"));
}

#[test]
fn golden_pvxs_nt_ndarray_desc() {
    let d = nd_array::nt_nd_array_desc();
    assert_eq!(encode(&d, ByteOrder::Big), golden("nt_ndarray_desc"));
}

#[test]
fn golden_pvxs_nt_table_desc() {
    // pvxs capture: NTTable{}.add_column(Int32,"A").add_column(String,"B").
    // Locks labels(string[]) + value{Int32A A, StringA B} + descriptor +
    // alarm_t + time_t — the sub-structures are where a builder refactor
    // would silently drift.
    let d = NTTable::new()
        .add_column(ScalarType::Int, "A", None)
        .add_column(ScalarType::String, "B", None)
        .build();
    assert_eq!(encode(&d, ByteOrder::Big), golden("nt_table_desc"));
}

#[test]
fn golden_pvxs_nt_uri_desc() {
    // pvxs capture: NTURI{ UInt32("arg1"), String("arg2") } — scheme/
    // authority/path strings + query{uint32 arg1, string arg2}.
    let d = NTURI::new()
        .arg_scalar("arg1", ScalarType::UInt)
        .arg_scalar("arg2", ScalarType::String)
        .build();
    assert_eq!(encode(&d, ByteOrder::Big), golden("nt_uri_desc"));
}
