//! `scalerRSET` supplies ONE property slot, so scaler marks one leaf's worth.
//!
//! `scalerRecord.c:147-158` NULLs five of the six:
//!
//! ```text
//! #define get_units NULL            :151
//! static long get_precision(...)    :152   <- the only survivor
//! #define get_enum_strs NULL        :154
//! #define get_graphic_double NULL   :156
//! #define get_control_double NULL   :157
//! #define get_alarm_double NULL     :158
//! ```
//!
//! and `scalerRSET` (`:160-179`) wires exactly those into the table. A NULL
//! slot makes `dbAccess.c` clear the corresponding option bit, so QSRV2 never
//! assigns the leaf and pvxs leaves it absent — where the port, having claimed
//! the slot, put a fabricated zero on the wire.
//!
//! scaler is not in the differential oracle's record set, so these cases are
//! the transcription's only check. They are written against the C rset table
//! above rather than against a measured `pvxget`.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::types::EpicsValue;
use scaler_rs::records::scaler::ScalerRecord;

fn scaler() -> RecordInstance {
    RecordInstance::new("T:SCALER".to_string(), ScalerRecord::default())
}

/// The reported defect: `#define get_alarm_double NULL` (`:158`), yet the port
/// claimed the slot and marked all four `valueAlarm.*Limit` leaves.
#[test]
fn scaler_does_not_claim_alarm_double() {
    let props = scaler().record.property_support();

    assert!(
        !props.alarm_double,
        "scalerRecord.c:158 is `#define get_alarm_double NULL`: the four \
         valueAlarm.*Limit leaves are never served"
    );
}

/// The rest of the same row. The citation named `get_alarm_double`, but the
/// rset NULLs three more slots the port also claimed — one wrong row, not one
/// wrong bit.
#[test]
fn scaler_claims_no_slot_its_rset_nulls() {
    let props = scaler().record.property_support();

    assert!(
        !props.units,
        "scalerRecord.c:151 is `#define get_units NULL`"
    );
    assert!(
        !props.graphic_double,
        "scalerRecord.c:156 is `#define get_graphic_double NULL`"
    );
    assert!(
        !props.control_double,
        "scalerRecord.c:157 is `#define get_control_double NULL`"
    );
    assert!(
        !props.enum_strs,
        "scalerRecord.c:154 is `#define get_enum_strs NULL`"
    );
}

/// The positive side, so the row is pinned as a transcription and not merely
/// emptied: `get_precision` (`:152`) IS supplied, and the row must say so.
///
/// It marks nothing today — pvxs assigns `display.precision` only inside its
/// `DBR_GR_DOUBLE` branch (`iocsource.cpp:288-291`) and scaler's
/// `get_graphic_double` is NULL — which is exactly why the bit has to record
/// the rset rather than the observable leaf: the two are not the same question.
#[test]
fn scaler_still_claims_the_one_slot_its_rset_supplies() {
    let props = scaler().record.property_support();

    assert!(props.precision, "scalerRecord.c:152 supplies get_precision");
}

/// The leaf-level consequence, which is what reaches the wire: a VAL snapshot
/// must carry no alarm limits at all. `property_support` is upstream of the
/// marking, so this is the assertion a client would actually observe.
#[test]
fn a_scaler_val_snapshot_carries_no_alarm_limits() {
    let inst = scaler();
    let snap = inst.snapshot_for_field("VAL").expect("scaler serves VAL");

    assert!(
        !snap.properties.alarm_double,
        "the slot is NULL, so nothing decides the valueAlarm limits"
    );
    assert!(
        snap.alarm_limits().is_none(),
        "no alarm limits are assigned for a type whose rset NULLs the slot"
    );
}

/// What that one slot ANSWERS, which is the half a property bit cannot say.
/// `scalerRecord.c:728-742` seeds `pscal->prec` and departs from it for `VERS`
/// alone (a literal 2); `recGblGetPrec` reaches only dbCommon, which has no
/// DBF_DOUBLE field, so `TP` and its siblings keep `PREC`.
///
/// Precision is not only a `caget -d` leaf: it is what the DBF_DOUBLE to
/// DBR_STRING conversion renders with (`dbConvert.c:783-786`), so a plain
/// `caget T:SCALER.TP` printed `1` where C prints `1.000`.
#[test]
fn scaler_precision_is_prec_everywhere_but_vers() {
    let mut rec = ScalerRecord::default();
    rec.put_field("PREC", EpicsValue::Short(3))
        .expect("scaler models PREC");
    let inst = RecordInstance::new("T:SCALER".to_string(), rec);

    let prec_of = |field: &str| {
        inst.snapshot_for_field(field)
            .unwrap_or_else(|| panic!("{field} has no snapshot"))
            .precision()
            .unwrap_or_else(|| panic!("{field} serves no precision leaf"))
    };

    assert_eq!(prec_of("TP"), 3);
    assert_eq!(prec_of("FREQ"), 3);
    assert_eq!(prec_of("VERS"), 2);
}
