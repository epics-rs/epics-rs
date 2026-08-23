//! The constant-DOL init seed on `mbboDirect` and `busy`.
//!
//! C runs both through `recGblInitConstantLink`, the one owner of "a CONSTANT
//! link's text becomes the target field's value":
//!
//! ```c
//! /* mbboDirectRecord.c:119-120 */
//! if (recGblInitConstantLink(&prec->dol, DBF_ULONG, &prec->val))
//!     prec->udf = FALSE;
//!
//! /* busyRecord.c:151-159 */
//! if (prec->dol.type == CONSTANT) {
//!     unsigned short ival = 0;
//!     if (recGblInitConstantLink(&prec->dol, DBF_USHORT, &ival)) {
//!         if (ival == 0) prec->val = 0; else prec->val = 1;
//!         prec->udf = FALSE;
//!     }
//! }
//! ```
//!
//! `busy` declared no seed at all, so a constant DOL never reached VAL and the
//! record stayed undefined. `mbboDirect` open-coded one inside
//! `post_init_finalize_undef` with a bare `str::parse::<f64>()` and an
//! `as u32` cast, which is not what `recGblInitConstantLink` does: it neither
//! accepts C's hex literals nor wraps a negative into the unsigned target
//! (Rust's float→unsigned cast saturates to 0 where C's integer cast wraps).
//!
//! Boundaries here are the ones that distinguish the shared owner from a local
//! `parse()`: the link's number SYNTAX, the target field's integer WIDTH and
//! SIGNEDNESS, the boolean normalisation `busy` applies on top, the UDF clear,
//! and the negative case — a non-constant DOL seeds nothing.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build(db_text: &str) -> Db {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn field(db: &Db, rec: &str, f: &str) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field(f)
}

fn udf(db: &Db, rec: &str) -> u8 {
    db.get_record(rec).unwrap().read().common.udf
}

/// Syntax boundary: C's constant parse takes a hex literal, a bare `parse::<f64>()`
/// does not.
#[epics_macros_rs::epics_test]
async fn a_hex_constant_dol_seeds_mbbo_direct() {
    let db =
        build(r#"record(mbboDirect, "M") { field(OMSL, "closed_loop") field(DOL, "0x1F") }"#).await;

    assert_eq!(field(&db, "M", "VAL"), Some(EpicsValue::Long(0x1F)));
    assert_eq!(field(&db, "M", "B4"), Some(EpicsValue::UChar(1)), "bit 4");
    assert_eq!(field(&db, "M", "B5"), Some(EpicsValue::UChar(0)), "bit 5");
    assert_eq!(udf(&db, "M"), 0, "a seeded record is defined");
}

/// Width/signedness boundary: C loads the constant as DBF_ULONG with an integer
/// cast, so `-1` fills all 32 bits. A float→unsigned cast saturates to 0.
#[epics_macros_rs::epics_test]
async fn a_negative_constant_dol_wraps_into_the_unsigned_target() {
    let db =
        build(r#"record(mbboDirect, "N") { field(OMSL, "closed_loop") field(DOL, "-1") }"#).await;

    assert_eq!(field(&db, "N", "VAL"), Some(EpicsValue::Long(-1)));
    for b in ["B0", "B1F"] {
        assert_eq!(
            field(&db, "N", b),
            Some(EpicsValue::UChar(1)),
            "{b} set by the all-ones seed"
        );
    }
}

/// Negative control: `recGblInitConstantLink` returns FALSE for a link that is
/// not CONSTANT and touches nothing, so a DB-link DOL leaves VAL and UDF alone.
#[epics_macros_rs::epics_test]
async fn a_db_link_dol_seeds_nothing() {
    let db = build(
        r#"
record(ao, "SRC") { field(VAL, "7") }
record(mbboDirect, "D") { field(OMSL, "closed_loop") field(DOL, "SRC") }
"#,
    )
    .await;

    assert_eq!(field(&db, "D", "VAL"), Some(EpicsValue::Long(0)));
    assert_eq!(udf(&db, "D"), 1, "nothing defined this record yet");
}

/// The B0..B1F fold (epics-base dabcf89) still wins when there is no DOL to
/// seed: bits set in the `.db` become VAL and define the record.
#[epics_macros_rs::epics_test]
async fn bits_still_fold_into_val_without_a_dol() {
    let db = build(r#"record(mbboDirect, "F") { field(B0, "1") field(B3, "1") }"#).await;

    assert_eq!(field(&db, "F", "VAL"), Some(EpicsValue::Long(0b1001)));
    assert_eq!(udf(&db, "F"), 0);
}

/// busy stores the BOOLEAN of the constant, not the constant — C's
/// `if (ival == 0) val = 0; else val = 1;`.
#[epics_macros_rs::epics_test]
async fn a_constant_dol_seeds_busy_as_a_boolean() {
    let db = build(
        r#"
record(busy, "B5") { field(DTYP, "Soft Channel") field(OMSL, "closed_loop") field(DOL, "5") }
record(busy, "B0") { field(DTYP, "Soft Channel") field(OMSL, "closed_loop") field(DOL, "0") }
"#,
    )
    .await;

    assert_eq!(
        field(&db, "B5", "VAL"),
        Some(EpicsValue::Enum(1)),
        "DOL=5 is Busy, not 5"
    );
    assert_eq!(udf(&db, "B5"), 0);

    // The zero case is the one that shows the seed RAN: VAL is 0 either way,
    // and only the seed clears UDF.
    assert_eq!(field(&db, "B0", "VAL"), Some(EpicsValue::Enum(0)));
    assert_eq!(udf(&db, "B0"), 0, "a loaded constant defines the record");
}

/// C's busy init tail converts the seeded VAL to RVAL through MASK
/// (`busyRecord.c:173-177`), so a seeded record already reads back its raw
/// word before any process.
#[epics_macros_rs::epics_test]
async fn the_busy_seed_reaches_rval_through_mask() {
    let db = build(
        r#"
record(busy, "R") { field(DTYP, "Soft Channel") field(OMSL, "closed_loop") field(DOL, "1")
                    field(MASK, "12") }
record(busy, "P") { field(DTYP, "Soft Channel") field(OMSL, "closed_loop") field(DOL, "1") }
"#,
    )
    .await;

    assert_eq!(
        field(&db, "R", "RVAL"),
        Some(EpicsValue::ULong(12)),
        "MASK != 0: a nonzero VAL takes the whole mask"
    );
    assert_eq!(
        field(&db, "P", "RVAL"),
        Some(EpicsValue::ULong(1)),
        "MASK == 0: RVAL is VAL"
    );
}

/// A busy with no DOL is untouched by the seed owner.
#[epics_macros_rs::epics_test]
async fn a_busy_without_a_dol_stays_undefined() {
    let db = build(r#"record(busy, "U") { field(DTYP, "Soft Channel") }"#).await;

    assert_eq!(field(&db, "U", "VAL"), Some(EpicsValue::Enum(0)));
    assert_eq!(udf(&db, "U"), 1);
}
