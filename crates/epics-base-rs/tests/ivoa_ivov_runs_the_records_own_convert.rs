//! `IVOA = Set_output_to_IVOV` must run the record's OWN VAL->raw conversion.
//!
//! C line numbers resolve at epics-base `R7.0.10`; `busy` is module `busy` at
//! `R1-7-4-6-g2dfe92d`. Every C arm has the same two statements — store IVOV
//! into VAL, then convert — and never assigns the raw word from IVOV:
//!
//! ```c
//! /* boRecord.c:230-238, busyRecord.c:235-243 */
//!     prec->val=prec->ivov;
//!     if ( prec->mask != 0 ) {
//!         if(prec->val==0) prec->rval = 0;
//!         else prec->rval = prec->mask;
//!     } else prec->rval = (epicsUInt32)prec->val;
//!
//! /* mbboRecord.c:232-236, mbboDirectRecord.c:210-214 */
//!     prec->val = prec->ivov;
//!     convert(prec);
//! ```
//!
//! IVOV is a value in VAL's units, so the raw word it produces is IVOV run
//! through the same translation an ordinary cycle uses: the MASK rule for
//! `bo`/`busy`, the ZRVL..FFVL state-value table plus SHFT for `mbbo`
//! (`mbboRecord.c:418-435`), SHFT for `mbboDirect` (`:342-349`).
//!
//! The port assigned `RVAL = IVOV` directly on all four, which is only C's
//! unmasked/unshifted fallback — and on `busy` it was not even that: the
//! native `UShort` IVOV fell through to `put_field("RVAL", UShort)`, whose
//! `TypeMismatch` (RVAL is DBF_ULONG) aborted the arm before VAL was written,
//! so the record drove its stale value.
//!
//! One case per boundary of each record's conversion, not per scenario.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::types::EpicsValue;

/// The IVOV a record's own `get_field` serves, which is what the framework's
/// IVOA owner hands to `apply_invalid_output_value` — not a carrier the test
/// picked.
fn served_ivov(rec: &mut dyn Record, ivov: EpicsValue) -> EpicsValue {
    rec.put_field("IVOV", ivov)
        .expect("IVOV accepts its own type");
    rec.get_field("IVOV").expect("IVOV is a served field")
}

fn raw(rec: &dyn Record) -> u32 {
    match rec.get_field("RVAL") {
        Some(EpicsValue::ULong(v)) => v,
        other => panic!("RVAL must be served as ULong, got {other:?}"),
    }
}

/// `bo`, MASK == 0 — C's `else prec->rval = (epicsUInt32)prec->val;`.
#[test]
fn bo_with_no_mask_puts_the_state_index_in_rval() {
    let mut rec = BoRecord::new(0);
    let ivov = served_ivov(&mut rec, EpicsValue::UShort(1));
    rec.apply_invalid_output_value(ivov).unwrap();

    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(1)));
    assert_eq!(raw(&rec), 1);
}

/// `bo`, MASK != 0 and VAL != 0 — C's `else prec->rval = prec->mask;`. This is
/// the boundary a direct `RVAL = IVOV` gets wrong: the wire word is the mask,
/// not the state index.
#[test]
fn bo_with_a_mask_puts_the_mask_word_in_rval() {
    let mut rec = BoRecord::new(0);
    rec.put_field("MASK", EpicsValue::ULong(0x8000_0000))
        .unwrap();
    let ivov = served_ivov(&mut rec, EpicsValue::UShort(1));
    rec.apply_invalid_output_value(ivov).unwrap();

    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(1)));
    assert_eq!(
        raw(&rec),
        0x8000_0000,
        "C `boRecord.c:236` writes the MASK word, not the state index"
    );
}

/// `bo`, MASK != 0 and VAL == 0 — the other half of C's `:235`,
/// `if(prec->val==0) prec->rval = 0;`.
#[test]
fn bo_with_a_mask_and_a_zero_ivov_clears_rval() {
    let mut rec = BoRecord::new(1);
    rec.put_field("MASK", EpicsValue::ULong(0x8000_0000))
        .unwrap();
    rec.put_field("RVAL", EpicsValue::ULong(0x8000_0000))
        .unwrap();
    let ivov = served_ivov(&mut rec, EpicsValue::UShort(0));
    rec.apply_invalid_output_value(ivov).unwrap();

    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(0)));
    assert_eq!(raw(&rec), 0);
}

/// `busy` at all: the served `UShort` IVOV used to abort the arm on
/// `put_field("RVAL", UShort)`, leaving VAL and RVAL at their stale values.
#[test]
fn busy_applies_its_own_served_ivov_carrier() {
    let mut rec = BusyRecord::new();
    rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    rec.put_field("RVAL", EpicsValue::ULong(1)).unwrap();
    let ivov = served_ivov(&mut rec, EpicsValue::UShort(0));
    assert!(
        matches!(ivov, EpicsValue::UShort(0)),
        "busyRecord.dbd:154 is DBF_USHORT, so this is the carrier the \
         framework's IVOA owner hands over"
    );

    rec.apply_invalid_output_value(ivov)
        .expect("the arm must accept the carrier its own get_field serves");
    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(0)));
    assert_eq!(raw(&rec), 0);
}

/// `busy`, MASK != 0 — `busyRecord.c:239-242` is `boRecord.c:234-237`
/// transcribed, so the same MASK rule applies.
#[test]
fn busy_with_a_mask_puts_the_mask_word_in_rval() {
    let mut rec = BusyRecord::new();
    rec.put_field("MASK", EpicsValue::ULong(0xF0)).unwrap();
    let ivov = served_ivov(&mut rec, EpicsValue::UShort(1));
    rec.apply_invalid_output_value(ivov).unwrap();

    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(1)));
    assert_eq!(raw(&rec), 0xF0);
}

/// `mbbo` with a state table — C `convert()` `mbboRecord.c:428` `prec->rval =
/// pvalues[prec->val]`, so IVOV = 2 drives TWVL, not 2.
#[test]
fn mbbo_with_a_state_table_puts_the_state_value_in_rval() {
    let mut rec = MbboRecord::default();
    rec.put_field("ZRVL", EpicsValue::ULong(0x10)).unwrap();
    rec.put_field("ONVL", EpicsValue::ULong(0x20)).unwrap();
    rec.put_field("TWVL", EpicsValue::ULong(0x40)).unwrap();
    let ivov = served_ivov(&mut rec, EpicsValue::UShort(2));
    rec.apply_invalid_output_value(ivov).unwrap();

    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(2)));
    assert_eq!(
        raw(&rec),
        0x40,
        "C looks the raw word up in ZRVL..FFVL; the state index is not it"
    );
}

/// `mbbo` with no state table — C `convert()` `:431` `prec->rval = prec->val;`
/// then `:433-434` `prec->rval <<= prec->shft;`.
#[test]
fn mbbo_with_no_state_table_shifts_the_state_index() {
    let mut rec = MbboRecord::default();
    rec.put_field("SHFT", EpicsValue::UShort(4)).unwrap();
    let ivov = served_ivov(&mut rec, EpicsValue::UShort(3));
    rec.apply_invalid_output_value(ivov).unwrap();

    // A stateless `mbbo` degenerates to DBF_USHORT (C `cvt_dbaddr`), so VAL
    // comes back as `UShort` here and as `Enum` in the state-table case above.
    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::UShort(3)));
    assert_eq!(raw(&rec), 3 << 4);
}

/// `mbboDirect` — C `convert()` `:345-348` is the assignment plus the SHFT
/// shift, and nothing else.
#[test]
fn mbbo_direct_shifts_the_ivov_word() {
    let mut rec = MbboDirectRecord::default();
    rec.put_field("SHFT", EpicsValue::UShort(8)).unwrap();
    let ivov = served_ivov(&mut rec, EpicsValue::Long(0x5));
    rec.apply_invalid_output_value(ivov).unwrap();

    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Long(0x5)));
    assert_eq!(raw(&rec), 0x5 << 8);
}

/// `mbboDirect` with SHFT == 0 — C's `if (prec->shft > 0)` is false, so RVAL
/// is the bare word. The boundary that keeps the shift from being applied
/// unconditionally.
#[test]
fn mbbo_direct_with_no_shift_puts_the_bare_word_in_rval() {
    let mut rec = MbboDirectRecord::default();
    let ivov = served_ivov(&mut rec, EpicsValue::Long(0x5));
    rec.apply_invalid_output_value(ivov).unwrap();

    assert_eq!(raw(&rec), 0x5);
}
