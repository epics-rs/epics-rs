//! `get_graphic_double` does not list VAL for every record type.
//!
//! Every other type's graphic switch opens with `case indexof(VAL):` — which is
//! why "VAL keeps its metadata cache" held as a type-independent rule for so
//! long. Two types never key on VAL at all:
//!
//! ```text
//! seqRecord.c:282-297   keys on `dbGetFieldIndex(paddr) - indexof(DLY0)`, so
//!                       every field BELOW DLY0 — VAL included — falls past the
//!                       switch to recGblGetGraphicDouble.
//! aSubRecord.c:350-368  keys on the link number (get_inlinkNumber, then
//!                       get_outlinkNumber). VAL is neither, so it falls out of
//!                       the function having written nothing — the dbAccess.c:216
//!                       seed stands (`graphic_default_arm` == Seed).
//! ```
//!
//! Measured on `softIocPVX` with `record(seq,"ORACLE:SEQ") {}`:
//!
//! ```text
//! $ pvxget ORACLE:SEQ.VAL  ->  display.limitLow -2147483648  limitHigh 2147483647
//! ```
//!
//! The port served 0/0 there — seq's empty VAL cache — because the routing owner
//! asked `is_value_field(field)` before it asked which type it was holding.

use epics_base_rs::server::database::LinkBacking;
use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::server::snapshot::Snapshot;

fn snap(inst: &RecordInstance, field: &str) -> Snapshot {
    inst.snapshot_for_field_with(field, LinkBacking::none())
        .unwrap_or_else(|| panic!("{field} has no snapshot"))
}

fn disp(inst: &RecordInstance, field: &str) -> (f64, f64) {
    let d = snap(inst, field)
        .display
        .unwrap_or_else(|| panic!("{field} carries no display block"));
    (d.lower_disp_limit, d.upper_disp_limit)
}

fn inst<R: Record + 'static>(rec: R) -> RecordInstance {
    RecordInstance::new("T:REC".to_string(), rec)
}

/// The measured regression: seq's VAL is DBF_LONG and unlisted, so it takes
/// `recGblGetGraphicDouble`'s range where the port served its empty cache's 0/0.
#[test]
fn seq_does_not_list_val_so_it_takes_the_long_range() {
    let inst = inst(SeqRecord::default());

    assert_eq!(
        disp(&inst, "VAL"),
        (-2147483648.0, 2147483647.0),
        "seqRecord.c:282-297 lists only DLYn and DOn: VAL reaches \
         recGblGetGraphicDouble, which answers the DBF_LONG range"
    );
}

/// The boundary the fix must not cross: seq's OTHER graphic answers are
/// unchanged. DLYn is listed on a literal 0..10 (`:328-331`) and DOn is
/// link-backed (`:332-336`) — both reached through the same predicate that now
/// answers `false` for every seq field.
#[test]
fn seqs_dlyn_literal_and_don_link_arms_still_stand() {
    let inst = inst(SeqRecord::default());

    assert_eq!(
        disp(&inst, "DLY0"),
        (0.0, 10.0),
        "seqRecord.c:286-288 answers DLYn a literal 0..10, NOT the DBF_DOUBLE range"
    );
    // DO0's DOL0 link is empty, so the link arm answers 0/0 — where the
    // DBF_DOUBLE default arm would answer +-1e300.
    assert_eq!(
        disp(&inst, "DO0"),
        (0.0, 0.0),
        "seqRecord.c:289-293 routes DOn through dbGetGraphicLimits on its DOLn link"
    );
}

/// `aSub`'s VAL answers 0/0 on BOTH paths — its Seed default arm and its empty
/// cache agree — so unlike the seq case above, this one changes no number and
/// would pass without the fix. It is here for what the fix newly makes it
/// depend on: VAL now reaches `graphic_default_arm`, so aSub's Seed arm is what
/// answers it. Were that arm ever "corrected" to `recGblGetGraphicDouble` — the
/// arm every OTHER type ends with — VAL would start serving the DBF_LONG range
/// and this case would catch it.
#[test]
fn asub_does_not_list_val_either() {
    let inst = inst(ASubRecord::default());

    assert!(
        snap(&inst, "VAL").properties.graphic_double,
        "aSub supplies get_graphic_double, so display's limit leaves are served"
    );
    assert_eq!(
        disp(&inst, "VAL"),
        (0.0, 0.0),
        "aSubRecord.c:350-368 writes nothing for VAL — neither link arm matches \
         and there is no recGbl call — so the dbAccess.c:216 seed stands"
    );
}

/// The negative case: every OTHER type does open on `case indexof(VAL)`, so the
/// cache still wins there. `calcRecord.c:187-212` lists VAL on HOPR/LOPR — if
/// the new per-type gate leaked, this would answer the DBF_DOUBLE range.
#[test]
fn a_listed_types_val_still_keeps_its_cache() {
    let mut rec = CalcRecord::default();
    rec.put_field("HOPR", epics_base_rs::types::EpicsValue::Double(60.0))
        .expect("put HOPR");
    rec.put_field("LOPR", epics_base_rs::types::EpicsValue::Double(-30.0))
        .expect("put LOPR");
    let inst = inst(rec);

    assert_eq!(
        disp(&inst, "VAL"),
        (-30.0, 60.0),
        "calcRecord.c:187-212 lists VAL: it keeps the record's HOPR/LOPR"
    );
}
