//! A `compress` channel is `NSAM` wide however few samples the ring holds.
//!
//! C keeps the capacity and the current length in two hooks:
//!
//! ```c
//! /* cvt_dbaddr, compressRecord.c:395-407 — the CHANNEL's capacity */
//! paddr->no_elements = prec->nsam;
//! ...
//! if (prec->balg == bufferingALG_LIFO)
//!     paddr->special = SPC_NOMOD;
//!
//! /* get_array_info — the CURRENT valid length, with the ring's offset */
//! *no_elements = prec->nuse;
//! ```
//!
//! The port served `NUSE` and advertised nothing, so the channel was sized
//! from the ring's fill level. `ca_element_count` is settled once at
//! create-channel time, so a client that connected to an empty or partly
//! filled compress fixed its buffer there and never saw the ring fill up.
//!
//! Boundaries: `NUSE` at 0, strictly inside `NSAM`, and at `NSAM`.
//! (The `SPC_NOMOD`-under-LIFO half of the same hook was already ported as
//! `Record::field_no_mod`; `spc_nomod_declaration` covers it.)

use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::server::records::compress::CompressRecord;
use epics_base_rs::types::EpicsValue;

/// `alg` 0 is `menuCompressALG` `N to 1 Low Value`; the algorithm plays no
/// part in either hook.
fn ring(nsam: i32, nuse: i32) -> CompressRecord {
    let mut r = CompressRecord::new(nsam, 0);
    r.nuse = nuse;
    r
}

fn served_len(r: &CompressRecord) -> usize {
    match r.get_field("VAL") {
        Some(EpicsValue::DoubleArray(v)) => v.len(),
        other => panic!("VAL reads as {other:?}"),
    }
}

/// The reported case: a ring three deep in a ten-sample buffer.
#[test]
fn a_partly_filled_ring_advertises_nsam_not_nuse() {
    let r = ring(10, 3);

    assert_eq!(r.field_native_count("VAL"), Some(10));
    assert_eq!(served_len(&r), 3);
}

/// A full ring — the two hooks coincide.
#[test]
fn a_full_ring_advertises_the_same_nsam() {
    let r = ring(10, 10);

    assert_eq!(r.field_native_count("VAL"), Some(10));
    assert_eq!(served_len(&r), 10);
}

/// The connect-before-any-sample case, which is the one that used to strand a
/// client: C serves nothing yet (`get_array_info` has no floor here, unlike
/// `mca`) but still sizes the channel at `NSAM`.
#[test]
fn an_empty_ring_still_advertises_nsam() {
    let r = ring(10, 0);

    assert_eq!(r.field_native_count("VAL"), Some(10));
    assert_eq!(served_len(&r), 0);
}

/// `VAL` is the record's only `special(SPC_DBADDR)` field, so it is the only
/// channel with a capacity of its own.
#[test]
fn only_val_carries_a_capacity() {
    let r = ring(10, 3);

    for field in ["NSAM", "NUSE", "OFF", "ALG", "BALG", "INP"] {
        assert_eq!(r.field_native_count(field), None, "{field} is not an array");
    }
}
