//! `acalcout`'s `SIZE` menu picks the CHANNEL capacity; `NUSE` picks the served
//! length. They are two different answers and C computes them in two hooks.
//!
//! ```c
//! /* acalcGetNumElements, aCalcoutRecord.c:160-168 — the served length */
//! if ( (pcalc->nuse > 0) && (pcalc->nuse < pcalc->nelm) )
//!     numElements = pcalc->nuse;
//! else
//!     numElements = pcalc->nelm;
//!
//! /* cvt_dbaddr, :627-631 — the capacity CA is told */
//! if (pcalc->size == acalcoutSIZE_NUSE)
//!     paddr->no_elements = acalcGetNumElements( pcalc );
//! else
//!     paddr->no_elements = pcalc->nelm;
//! ```
//!
//! `SIZE` exists precisely so a client can be told the smaller number
//! (`:619-626`), and it defaults to `NELM` — the first menu choice. The port
//! modelled the branch in `dbaddr_no_elements` and used it to bound a client
//! write, but never advertised it, so the channel was sized from the SERVED
//! count. Under the default `SIZE=NELM` with `0 < NUSE < NELM` a client sized
//! its buffer at `NUSE` and, since `ca_element_count` is settled once at
//! create-channel time, never saw the array widen when `NUSE` grew.
//!
//! Boundaries: `NUSE` at 0, strictly inside `NELM`, and at `NELM`, each under
//! both `SIZE` settings; plus the field population that reaches `cvt_dbaddr`
//! at all.

use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// `acalcoutSIZE` menu (`aCalcoutRecord.dbd:32-35`): `NELM` first, `NUSE`
/// second.
const SIZE_NELM: i16 = 0;
const SIZE_NUSE: i16 = 1;

/// The 14 `special(SPC_DBADDR)` fields of `aCalcoutRecord.dbd` — the complete
/// set that reaches `cvt_dbaddr`.
const ARRAY_FIELDS: [&str; 14] = [
    "AVAL", "AA", "BB", "CC", "DD", "EE", "FF", "GG", "HH", "II", "JJ", "KK", "LL", "OAV",
];

fn acalcout(nelm: u32, nuse: u32, size: i16) -> AcalcoutRecord {
    let mut r = AcalcoutRecord::new();
    r.put_field("NELM", EpicsValue::ULong(nelm)).unwrap();
    r.put_field("NUSE", EpicsValue::ULong(nuse)).unwrap();
    r.put_field("SIZE", EpicsValue::Short(size)).unwrap();
    r
}

fn served_len(r: &AcalcoutRecord, field: &str) -> usize {
    match r.get_field(field) {
        Some(EpicsValue::DoubleArray(v)) => v.len(),
        other => panic!("{field} reads as {other:?}"),
    }
}

/// The default menu choice with a window open: the channel is the whole
/// buffer, the value served through it is the window.
#[test]
fn size_nelm_advertises_the_whole_buffer_while_nuse_hides_its_tail() {
    let r = acalcout(8, 3, SIZE_NELM);

    for field in ARRAY_FIELDS {
        assert_eq!(r.field_native_count(field), Some(8), "{field} capacity");
    }
    assert_eq!(served_len(&r, "AVAL"), 3);
    assert_eq!(served_len(&r, "OAV"), 3);
}

/// The other menu choice, and the reason the field exists: the client is told
/// the smaller number.
#[test]
fn size_nuse_narrows_the_channel_to_the_window() {
    let r = acalcout(8, 3, SIZE_NUSE);

    for field in ARRAY_FIELDS {
        assert_eq!(r.field_native_count(field), Some(3), "{field} capacity");
    }
    assert_eq!(served_len(&r, "AVAL"), 3);
}

/// `NUSE == 0` — no window. `acalcGetNumElements` answers `NELM`, so both menu
/// choices agree and the two hooks coincide.
#[test]
fn an_unset_nuse_leaves_both_menu_choices_at_nelm() {
    for size in [SIZE_NELM, SIZE_NUSE] {
        let r = acalcout(8, 0, size);
        assert_eq!(r.field_native_count("AVAL"), Some(8));
        assert_eq!(served_len(&r, "AVAL"), 8);
    }
}

/// `NUSE == NELM` — the edge of `nuse < nelm`, and not a window either.
#[test]
fn a_nuse_at_nelm_is_not_a_window() {
    for size in [SIZE_NELM, SIZE_NUSE] {
        let r = acalcout(8, 8, size);
        assert_eq!(r.field_native_count("AVAL"), Some(8));
        assert_eq!(served_len(&r, "AVAL"), 8);
    }
}

/// Only the `SPC_DBADDR` fields reach `cvt_dbaddr`; every other channel keeps
/// its own value's count.
#[test]
fn only_the_spc_dbaddr_fields_advertise_a_capacity() {
    let r = acalcout(8, 3, SIZE_NELM);

    for field in ["VAL", "OVAL", "PVAL", "NELM", "NUSE", "CALC", "SIZE"] {
        assert_eq!(r.field_native_count(field), None, "{field} is not an array");
    }
}
