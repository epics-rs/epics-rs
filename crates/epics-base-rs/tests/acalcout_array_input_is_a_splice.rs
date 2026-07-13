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
//! Three regions, three rules — and R14-6 corrects what W10-A8 wrote here about
//! the middle one:
//!
//! * `[0, nNew)` — what the writer delivered.
//! * `[nNew, numElements)` — ZEROED, on BOTH paths. The link's zero-fill is the
//!   `fetch_values` code above; the client's is `put_array_info`
//!   (`aCalcoutRecord.c:726-731`), which `dbPut` calls for every SPC_DBADDR field
//!   it writes (`dbAccess.c:1366-1369`):
//!
//!   ```c
//!   if ( pd && (nNew < numElements) )
//!       for (i=nNew; i<numElements; i++) pd[i] = 0.;
//!   ```
//!
//!   W10-A8 asserted the client path had "no counterpart" and left the tail
//!   alone. It has one, and it does not.
//! * `[numElements, nelm)` — NEVER written, by either. `numElements` is
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

/// The client-put path (R14-6). `dbPut` copies the elements it brought into the
/// same `calloc(nelm)` buffer and then calls `put_array_info`, which ZEROES
/// `[nNew, numElements)` — so a two-element put over a full ten-element window
/// leaves eight zeros behind it, not the eight values that were there.
#[test]
fn a_short_client_put_zeroes_the_rest_of_the_window() {
    let mut rec = record_with_full_aa();

    rec.put_field("AA", EpicsValue::DoubleArray(vec![7.0, 8.0]))
        .unwrap();

    assert_eq!(
        aa(&rec),
        vec![7.0, 8.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    );
}

/// …and it stops at the WINDOW, not at NELM: the same put with NUSE = 5 zeroes
/// `[2,5)` and leaves the hidden `[5,10)` intact, which is the splice invariant
/// the client path shares with the link path (R14-6 + R14-7).
#[test]
fn a_short_client_put_preserves_the_hidden_tail() {
    let mut rec = record_with_full_aa();
    rec.put_field("NUSE", EpicsValue::ULong(5)).unwrap();

    rec.put_field("AA", EpicsValue::DoubleArray(vec![7.0, 8.0]))
        .unwrap();
    assert_eq!(aa(&rec), vec![7.0, 8.0, 0.0, 0.0, 0.0]);

    rec.put_field("NUSE", EpicsValue::ULong(10)).unwrap();
    assert_eq!(
        aa(&rec),
        vec![7.0, 8.0, 0.0, 0.0, 0.0, 6.0, 7.0, 8.0, 9.0, 10.0]
    );
}

/// AVAL and OAV are SPC_DBADDR array fields too (`aCalcoutRecord.c:702`, `:711`),
/// so `put_array_info` serves them as well — a client put into either splices and
/// zeroes the rest of the window, exactly as into AA.
#[test]
fn aval_and_oav_take_the_same_client_put_rule() {
    for field in ["AVAL", "OAV"] {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(6)).unwrap();
        rec.put_field(field, EpicsValue::DoubleArray(vec![1.0; 6]))
            .unwrap();

        rec.put_field(field, EpicsValue::DoubleArray(vec![9.0, 9.0]))
            .unwrap();

        assert_eq!(
            rec.get_field(field),
            Some(EpicsValue::DoubleArray(vec![9.0, 9.0, 0.0, 0.0, 0.0, 0.0])),
            "{field}"
        );
    }
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
