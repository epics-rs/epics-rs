//! W10-A8 — an `AA`..`LL` write is a SPLICE into the record's `calloc(nelm)`
//! buffer, not a replacement of it.
//!
//! C allocates each array input once and keeps it for the record's lifetime:
//!
//! ```c
//! if (*pavalue == NULL) {
//!     *pavalue = (double *)calloc(pcalc->nelm, sizeof(double));
//!     pcalc->amem += pcalc->nelm * sizeof(double);
//! }
//! ...
//! nRequest = acalcGetNumElements( pcalc );
//! status = dbGetLink(plink, DBR_DOUBLE, *pavalue, 0, &nRequest);
//! if (!RTN_SUCCESS(status)) return(status);
//! if (nRequest<numElements) {
//!     for (j=nRequest; j<numElements; j++) (*pavalue)[j] = 0;
//! }
//! ```
//! (`fetch_values`, `aCalcoutRecord.c:1078-1102`.)
//!
//! Three regions, three rules:
//!
//! * `[0, nRequest)` — what the link delivered.
//! * `[nRequest, numElements)` — zeroed, but ONLY on the link path. This is
//!   `fetch_values`' own step; a client `dbPut` has no counterpart.
//! * `[numElements, nelm)` — NEVER written. `numElements` is
//!   `acalcGetNumElements()`, i.e. NUSE when NUSE < NELM, so this is the part of the
//!   buffer NUSE currently hides. It keeps its contents and REAPPEARS when NUSE
//!   grows again.
//!
//! The port did `arr_vals[idx] = <whatever was delivered>`, throwing the tail away
//! on both paths. Reads clamp to `num_elements()` and pad with zeros, which is why
//! the defect is invisible until NUSE grows — that is what these tests do.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

fn record_with_full_aa() -> AcalcoutRecord {
    let mut rec = AcalcoutRecord::new();
    rec.put_field("NELM", EpicsValue::ULong(10)).unwrap();
    rec.put_field("NUSE", EpicsValue::ULong(10)).unwrap();
    rec.put_field(
        "AA",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]),
    )
    .unwrap();
    rec
}

fn aa(rec: &AcalcoutRecord) -> Vec<f64> {
    match rec.get_field("AA") {
        Some(EpicsValue::DoubleArray(v)) => v,
        other => panic!("AA is not a DoubleArray: {other:?}"),
    }
}

/// The link path. With NUSE = 5 the window is `[0,5)` and the buffer's `[5,10)` is
/// hidden. A link that delivers two elements fills `[0,2)`, zeroes `[2,5)` — and
/// must leave `[5,10)` alone, so growing NUSE back to 10 shows 6..10 again.
#[test]
fn a_short_link_fetch_zeroes_the_window_and_preserves_the_hidden_tail() {
    let mut rec = record_with_full_aa();
    rec.put_field("NUSE", EpicsValue::ULong(5)).unwrap();

    rec.put_field_internal("AA", EpicsValue::DoubleArray(vec![7.0, 8.0]))
        .unwrap();

    // Inside the window: delivered, then zero-filled to numElements.
    assert_eq!(aa(&rec), vec![7.0, 8.0, 0.0, 0.0, 0.0]);

    // The hidden tail survived: C never wrote past numElements.
    rec.put_field("NUSE", EpicsValue::ULong(10)).unwrap();
    assert_eq!(
        aa(&rec),
        vec![7.0, 8.0, 0.0, 0.0, 0.0, 6.0, 7.0, 8.0, 9.0, 10.0]
    );
}

/// The client-put path. `dbPut` copies the elements it brought into the same
/// `calloc(nelm)` buffer and does NOT zero anything after them — so a two-element
/// put over a full buffer leaves 3..10 exactly as they were.
#[test]
fn a_short_client_put_preserves_everything_it_did_not_write() {
    let mut rec = record_with_full_aa();

    rec.put_field("AA", EpicsValue::DoubleArray(vec![7.0, 8.0]))
        .unwrap();

    assert_eq!(
        aa(&rec),
        vec![7.0, 8.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
    );
}

/// A full-length write still replaces every element — the splice is not a merge.
#[test]
fn a_full_length_write_replaces_the_whole_buffer() {
    let mut rec = record_with_full_aa();
    let fresh: Vec<f64> = (0..10).map(|i| (100 + i) as f64).collect();

    rec.put_field("AA", EpicsValue::DoubleArray(fresh.clone()))
        .unwrap();

    assert_eq!(aa(&rec), fresh);
}

/// NEWM still tracks a link fetch that changed the field, which is the reason the
/// override exists (`fetch_values` is the only C site that sets NEWM).
#[test]
fn the_link_fetch_still_sets_newm_when_the_value_changed() {
    let mut rec = record_with_full_aa();

    rec.put_field_internal("AA", EpicsValue::DoubleArray(vec![7.0, 8.0]))
        .unwrap();

    assert_eq!(rec.get_field("NEWM"), Some(EpicsValue::ULong(1)));
}
