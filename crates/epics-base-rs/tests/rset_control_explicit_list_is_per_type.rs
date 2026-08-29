//! `get_control_double`'s listed set is one list PER RECORD TYPE.
//!
//! `rset_explicit_control_cases.rs` pins what the listed arm ANSWERS. This file
//! pins WHICH FIELDS ARE IN IT — the prior question, and the one a single
//! shared predicate cannot answer: "VAL plus the seven alarm bands" is the list
//! of `ai`/`ao`/`calc`/`calcout`/`longin`/`longout`/`int64in`/`int64out`/`sub`
//! and of no other type.
//!
//! ```text
//! aSubRecord.c:372-376      lists NOTHING — a bare recGblGetControlDouble
//! seqRecord.c:342-353       DLYn only  (a literal; VAL is NOT listed)
//! boRecord.c:310-318        HIGH only  (a literal; VAL is NOT listed)
//! dfanoutRecord.c:197-213   VAL + LALM/ALST/MLST — but NOT the four bands
//! selRecord.c:203-235       the eight + A..L + LA..LL
//! waveformRecord.c:268-289  VAL + BUSY + NORD
//! aaiRecord.c:287-304       VAL + NORD
//! aaoRecord.c:292-309       VAL + NORD
//! ```
//!
//! Every case is written at the boundary where membership DECIDES the answer:
//! a listed field takes the record's own limits, an unlisted one takes
//! `recGblGetControlDouble` — the field's TYPE range. The two never coincide
//! here, so a passing test proves the list shape, not a lucky agreement.
//!
//! Measured on `softIocPVX` with empty records: transcribing these lists took
//! 35 cases from DEFECT to AGREED — SEL `A`..`L` and `LA`..`LL` (24) served
//! ±1e300 where C serves the record's own 0/0; DFANOUT's four bands (4), BO's
//! two latches (2) and ASUB.VAL (1) served 0/0 where C serves the field's type
//! range; WAVEFORM BUSY/NORD (2) and AAI/AAO NORD (2) served their type range
//! where C serves a computed span.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::sel::SelRecord;
use epics_base_rs::server::records::seq::SeqRecord;
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

/// The control limits as they reach the WIRE, which is what the oracle compares
/// and what these cases are about.
///
/// An unassigned `control` is not an absent leaf: `props.control_double` is what
/// decides whether the two leaves are served (`nt::qsrv_marks::property_leaves`),
/// and every type here supplies the slot. So `None` still puts a number on the
/// wire — the NT's structural 0/0.
///
/// `dfanout`/`sel` reach the wire that way for their LISTED fields: the port's
/// structs carry no `hopr`/`lopr` for `populate_control_info` to cache, though
/// their dbd declares both. The served 0/0 then matches C only because the
/// oracle's records are empty and C's own HOPR/LOPR are 0/0 there. That gap is
/// reported separately; it is orthogonal to the list shape these cases pin.
fn served_limits(inst: &RecordInstance, field: &str) -> (f64, f64) {
    let s = snap(inst, field);
    assert!(
        s.properties.control_double,
        "{field}: the type supplies get_control_double, so the leaves are served"
    );
    s.control_limits().unwrap_or((0.0, 0.0))
}

fn inst<R: Record + 'static>(rec: R) -> RecordInstance {
    RecordInstance::new("T:REC".to_string(), rec)
}

/// The DBF_USHORT range — what `recGblGetControlDouble` answers for an
/// unlisted `LALM`/`MLST` (`recGbl.c` `getMaxRangeValues`).
const USHORT_RANGE: (f64, f64) = (0.0, 65535.0);
/// The DBF_LONG range — what an unlisted `VAL` of `seq`/`aSub` answers
/// (`recGbl.c:393-396`: the lower is INT_MIN, one past the negated upper).
const LONG_RANGE: (f64, f64) = (-2147483648.0, 2147483647.0);

/// `boRecord.c:310-318` is `if (index == indexof(HIGH)) {...} else recGbl...`.
/// VAL is NOT in it, and neither are the latches — so `.LALM`/`.MLST` answer
/// the USHORT range, where a shared VAL-class list gave them the record's 0/0.
#[test]
fn bo_lists_high_alone_so_its_latches_take_the_ushort_range() {
    let inst = inst(BoRecord::new(0));

    for field in ["LALM", "MLST"] {
        assert_eq!(
            limits(&inst, field),
            USHORT_RANGE,
            "boRecord.c:310-318 lists HIGH alone: {field} takes \
             recGblGetControlDouble's DBF_USHORT range, not the record's limits"
        );
    }
}

/// `seqRecord.c:342-353` tests `dbGetFieldIndex(paddr) - indexof(DLY0)`, so
/// every field BELOW DLY0 — VAL included — takes the default arm. seq's VAL is
/// DBF_LONG (`seqRecord.dbd.pod`), so the boundary is visible: a shared
/// VAL-class list served 0/0 here.
#[test]
fn seq_does_not_list_val_so_it_takes_the_long_range() {
    let inst = inst(SeqRecord::default());

    assert_eq!(
        limits(&inst, "VAL"),
        LONG_RANGE,
        "seqRecord.c:342-353 lists only DLYn: VAL takes recGblGetControlDouble"
    );
}

/// `aSubRecord.c:372-376` is a bare `recGblGetControlDouble(paddr,pcd)` — the
/// whole body. Nothing is listed, so nothing keeps the cache.
#[test]
fn asub_lists_nothing_so_even_val_takes_the_default_arm() {
    let inst = inst(ASubRecord::default());

    assert_eq!(
        limits(&inst, "VAL"),
        LONG_RANGE,
        "aSubRecord.c:372-376 is a bare recGblGetControlDouble: VAL is not listed"
    );
}

/// `dfanoutRecord.c:197-213` lists VAL and the three LATCHES but not the four
/// BANDS — the one type whose control list splits the eight. So `.HIHI` takes
/// the DOUBLE range while `.LALM` takes the record's own limits.
///
/// Those own limits read 0/0 rather than HOPR/LOPR: the port's `DfanoutRecord`
/// carries no `hopr`/`lopr` field even though `dfanoutRecord.dbd.pod` declares
/// them, so `populate_control_info` has nothing to cache. That gap is invisible
/// to the oracle — its records are empty, where C's HOPR/LOPR are 0/0 too — and
/// is reported separately. It does not weaken the boundary below: the listed
/// and unlisted answers still differ, which is what the list decides.
#[test]
fn dfanout_lists_the_latches_but_not_the_bands() {
    let inst = inst(DfanoutRecord::new(0.0));

    for field in ["VAL", "LALM", "ALST", "MLST"] {
        assert_eq!(
            served_limits(&inst, field),
            (0.0, 0.0),
            "dfanoutRecord.c:202-204 lists {field}: it takes the record's own \
             limits, NOT recGblGetControlDouble's DBF_DOUBLE range"
        );
    }
    for field in ["HIHI", "HIGH", "LOW", "LOLO"] {
        assert_eq!(
            limits(&inst, field),
            (-1e300, 1e300),
            "dfanoutRecord.c:197-213 does NOT list {field}: it takes \
             recGblGetControlDouble's DBF_DOUBLE range"
        );
    }
}

/// `selRecord.c:214-215` lists `A ... L` and `LA ... LL` by GCC case range, so
/// sel's twelve inputs and twelve last-values answer the record's own limits —
/// where the shared VAL-class list left all 24 on the DBF_DOUBLE range. `SELN`
/// is the boundary: an unlisted field of the same record.
///
/// The same missing-`hopr` gap as `dfanout` above applies to the 0/0 here.
#[test]
fn sel_lists_its_twelve_args_and_their_last_values() {
    let inst = inst(SelRecord::default());

    for field in ["A", "F", "L", "LA", "LF", "LL"] {
        assert_eq!(
            served_limits(&inst, field),
            (0.0, 0.0),
            "selRecord.c:214-215 lists {field}: it takes the record's own limits, \
             NOT the DBF_DOUBLE range"
        );
    }
    assert_eq!(
        limits(&inst, "SELN"),
        USHORT_RANGE,
        "selRecord.c:203-235 does NOT list SELN: it takes recGblGetControlDouble's \
         DBF_USHORT range"
    );
    // M would be arg 13: sel has twelve (SEL_MAX), so it is past the range.
    assert!(
        inst.snapshot_for_field("M").is_none(),
        "sel has no field M — its args stop at L"
    );
}

/// `waveformRecord.c:277-285` lists BUSY (1/0) and NORD (NELM up) beyond VAL,
/// each answering a computed span rather than HOPR/LOPR. `NELM` is the
/// boundary: waveform does NOT list it (subArray does), so it takes the ULONG
/// range.
#[test]
fn waveform_lists_busy_and_nord_on_computed_spans_but_not_nelm() {
    let mut rec = WaveformRecord::with_kind(ArrayKind::Waveform);
    rec.put_field("NELM", EpicsValue::ULong(24)).expect("put");
    let inst = inst(rec);

    assert_eq!(
        limits(&inst, "BUSY"),
        (0.0, 1.0),
        "waveformRecord.c:278-280 answers BUSY 1 up / 0 down — a flag, not a range"
    );
    assert_eq!(
        limits(&inst, "NORD"),
        (0.0, 24.0),
        "waveformRecord.c:282-284 answers NORD prec->nelm up / 0 down"
    );
    assert_eq!(
        limits(&inst, "NELM"),
        (0.0, 4294967295.0),
        "waveformRecord.c:268-289 does NOT list NELM: it takes the DBF_ULONG range"
    );
}

/// `aaiRecord.c:302-304` / `aaoRecord.c:305-307` list NORD alone beyond VAL.
/// Same span as waveform's, reached through a different rset — so the three
/// kinds are pinned separately.
#[test]
fn aai_and_aao_list_nord_on_the_nelm_span() {
    for kind in [ArrayKind::Aai, ArrayKind::Aao] {
        let mut rec = WaveformRecord::with_kind(kind);
        rec.put_field("NELM", EpicsValue::ULong(12)).expect("put");
        let inst = inst(rec);

        assert_eq!(
            limits(&inst, "NORD"),
            (0.0, 12.0),
            "{:?} lists NORD: prec->nelm up / 0 down",
            kind
        );
    }
}
