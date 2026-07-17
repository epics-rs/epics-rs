//! The listed arm of `get_control_double` is not one shared field set.
//!
//! `rset_metadata_routes_per_field.rs` pins the two-arm SHAPE: a listed set
//! takes the record's own limits, everything else takes the `default:` arm.
//! This file pins the CONTENT of that listed set for the six types whose list
//! is not the shared VAL-class eight — each names extra fields, and each routes
//! them to a record field of its own choosing (not always `HOPR`/`LOPR`).
//!
//! Every case below is transcribed from the C rset, and every expectation is
//! chosen so it differs from what the `default:` arm would answer — so a test
//! that passes proves the explicit case fired, not that two paths coincide:
//!
//! ```text
//! aiRecord.c:267-288        SVAL              -> HOPR/LOPR   (SVAL is DBF_DOUBLE: default would be +-1e300)
//! longinRecord.c:206-224    SVAL              -> HOPR/LOPR   (DBF_LONG:   default would be +-2147483647)
//! aoRecord.c:341-363        OVAL, PVAL        -> DRVH/DRVL   (DBF_DOUBLE: default would be +-1e300)
//! compressRecord.c:487-502  IHIL, ILIL        -> HOPR/LOPR   (DBF_DOUBLE: default would be +-1e300)
//! histogramRecord.c:458-475 WDTH              -> ULIM - LLIM (DBF_DOUBLE: default would be +-1e300)
//! subArrayRecord.c:262-291  INDX, NELM,       -> MALM bounds (DBF_ULONG/LONG/SHORT ranges)
//!                           NORD, BUSY
//! ```
//!
//! Measured on `softIocPVX` with empty records, where every source field is 0:
//! before these cases existed the oracle reported AI.SVAL, AO.OVAL, AO.PVAL,
//! COMPRESS.IHIL, COMPRESS.ILIL, HISTOGRAM.WDTH, LONGIN.SVAL and SUBARRAY.INDX
//! as defects — C served the record's 0/0, the port served the type range.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::compress::CompressRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

fn snap(inst: &RecordInstance, field: &str) -> Snapshot {
    inst.snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
}

fn limits(inst: &RecordInstance, field: &str) -> (f64, f64) {
    snap(inst, field)
        .control_limits()
        .unwrap_or_else(|| panic!("{field} serves no control limits"))
}

/// Seed the record's own limit fields through its put path — the structs'
/// private members rule out struct-update syntax from an integration test.
fn with_fields<R: Record + 'static>(
    name: &str,
    mut rec: R,
    fields: &[(&str, EpicsValue)],
) -> RecordInstance {
    for (field, value) in fields {
        rec.put_field(field, value.clone())
            .unwrap_or_else(|e| panic!("put {field}: {e:?}"));
    }
    RecordInstance::new(name.to_string(), rec)
}

/// `aiRecord.c:280` lists `SVAL` — the simulation buffer — alongside `VAL`, so
/// it answers the record's `HOPR`/`LOPR` rather than the DBF_DOUBLE range.
#[test]
fn ai_sval_takes_the_records_hopr_lopr_not_the_double_range() {
    let inst = with_fields(
        "T:AI",
        AiRecord::new(0.0),
        &[
            ("HOPR", EpicsValue::Double(100.0)),
            ("LOPR", EpicsValue::Double(-50.0)),
        ],
    );

    assert_eq!(
        limits(&inst, "SVAL"),
        (-50.0, 100.0),
        "aiRecord.c:280 lists SVAL: C serves HOPR/LOPR, not the DBF_DOUBLE range"
    );

    // The negative boundary: SMOO is DBF_DOUBLE too (aiRecord.dbd.pod:324) but
    // the switch does not list it, so it still takes the `default:` arm.
    assert_eq!(
        limits(&inst, "SMOO"),
        (-1e300, 1e300),
        "aiRecord.c:271-283 does not list SMOO, so it takes the default: arm"
    );
}

/// `longinRecord.c:220` lists `SVAL`. Unlike ai's, longin's `SVAL` is
/// `DBF_LONG`, so the default arm it escapes is the LONG range — proof the
/// answer comes from the record field, not from a type coincidence.
#[test]
fn longin_sval_takes_the_records_hopr_lopr_not_the_long_range() {
    let inst = with_fields(
        "T:LONGIN",
        LonginRecord::new(0),
        &[
            ("HOPR", EpicsValue::Long(4000)),
            ("LOPR", EpicsValue::Long(-4000)),
        ],
    );

    assert_eq!(
        limits(&inst, "SVAL"),
        (-4000.0, 4000.0),
        "longinRecord.c:220 lists SVAL: C serves HOPR/LOPR, not the DBF_LONG \
         range of +-2147483647"
    );
}

/// The routing boundary that HOPR/LOPR-everywhere would hide: `ao`'s
/// `get_control_double` (`:356`) answers **DRVH/DRVL** — the drive limits —
/// while its `get_graphic_double` (`:332`) answers HOPR/LOPR for the same
/// field list. `OVAL` and `PVAL` are listed in both.
#[test]
fn ao_oval_and_pval_take_drvh_drvl_not_hopr_lopr() {
    let inst = with_fields(
        "T:AO",
        AoRecord::new(0.0),
        &[
            ("DRVH", EpicsValue::Double(10.0)),
            ("DRVL", EpicsValue::Double(-10.0)),
            ("HOPR", EpicsValue::Double(100.0)),
            ("LOPR", EpicsValue::Double(-100.0)),
        ],
    );

    for field in ["OVAL", "PVAL"] {
        assert_eq!(
            limits(&inst, field),
            (-10.0, 10.0),
            "aoRecord.c:347-348 lists {field}: control serves DRVH/DRVL, NOT \
             the HOPR/LOPR that get_graphic_double serves for the same field"
        );
    }
}

/// `compressRecord.c:493-494` lists `IHIL` and `ILIL` — the init limits — with
/// `VAL`, all on HOPR/LOPR. Note the list has no alarm bands: compress NULLs
/// `get_alarm_double` (`:60`), so it has none to list.
#[test]
fn compress_ihil_and_ilil_take_the_records_hopr_lopr() {
    let inst = with_fields(
        "T:COMPRESS",
        CompressRecord::new(10, 0),
        &[
            ("HOPR", EpicsValue::Double(75.0)),
            ("LOPR", EpicsValue::Double(-25.0)),
        ],
    );

    for field in ["IHIL", "ILIL"] {
        assert_eq!(
            limits(&inst, field),
            (-25.0, 75.0),
            "compressRecord.c:493-494 lists {field}: C serves HOPR/LOPR"
        );
    }
}

/// `histogramRecord.c:467-469` routes `WDTH` to neither HOPR nor LOPR but to a
/// COMPUTED span: `ULIM - LLIM` up, a literal `0.0` down.
#[test]
fn histogram_wdth_takes_the_ulim_minus_llim_span() {
    let inst = RecordInstance::new(
        "T:HISTOGRAM".to_string(),
        HistogramRecord::new(16, -20.0, 80.0),
    );

    assert_eq!(
        limits(&inst, "WDTH"),
        (0.0, 100.0),
        "histogramRecord.c:468 serves ULIM - LLIM = 80 - -20 = 100 up, 0.0 down"
    );
}

/// `subArrayRecord.c:271-286` lists four index fields, each bounded by `MALM`.
/// `NELM`'s lower is **1**, not 0 — C's control arm (`:277`) differs from its
/// own graphic arm (`:246`) on exactly this one value, so the two lists cannot
/// share a transcription.
#[test]
fn subarray_index_fields_take_malm_bounds() {
    let inst = with_fields(
        "T:SUBARRAY",
        WaveformRecord::with_kind(ArrayKind::SubArray),
        &[("MALM", EpicsValue::ULong(32))],
    );

    // INDX is a 0-based offset into MALM elements: MALM - 1.
    assert_eq!(
        limits(&inst, "INDX"),
        (0.0, 31.0),
        "subArrayRecord.c:272 serves MALM - 1 up"
    );
    // NELM is a length: MALM up, and 1 down.
    assert_eq!(
        limits(&inst, "NELM"),
        (1.0, 32.0),
        "subArrayRecord.c:276-277 serves MALM up and 1 down — the control arm's \
         lower differs from the graphic arm's 0"
    );
    // NORD is a count: MALM up, 0 down.
    assert_eq!(
        limits(&inst, "NORD"),
        (0.0, 32.0),
        "subArrayRecord.c:280-281 serves MALM up and 0 down"
    );
    // BUSY is a flag, bounded by neither MALM nor its DBF_SHORT range.
    assert_eq!(
        limits(&inst, "BUSY"),
        (0.0, 1.0),
        "subArrayRecord.c:284-285 serves the literal 1/0"
    );
}

/// The same struct backs `waveform`, whose own rset (`waveformRecord.c:268`)
/// lists VAL, BUSY and NORD but NOT `NELM` — so subArray's `NELM` case must
/// not leak across the kind, and `NELM` must reach the `default:` arm.
#[test]
fn waveform_does_not_borrow_subarrays_nelm_case() {
    let inst = RecordInstance::new(
        "T:WAVEFORM".to_string(),
        WaveformRecord::with_kind(ArrayKind::Waveform),
    );

    assert_eq!(
        limits(&inst, "NELM"),
        (0.0, 4294967295.0),
        "waveformRecord.c:272-284 does not list NELM, so it takes the default: \
         arm and serves the DBF_ULONG range — subArray's MALM bound must not \
         fire for a waveform"
    );
}
