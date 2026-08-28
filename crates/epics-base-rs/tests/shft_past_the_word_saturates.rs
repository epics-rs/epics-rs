//! `SHFT` at or past the word width saturates to zero, in both directions.
//!
//! C shifts with the bare operator under a `shft > 0` guard and nothing else —
//! `mbboDirectRecord.c:347-348` `if (prec->shft > 0) prec->rval <<= prec->shft;`
//! and `mbbiDirectRecord.c:159-160` `if (prec->shft > 0) rval >>= prec->shft;`
//! (both R7.0.10). `SHFT` is `DBF_USHORT`, so a client can put 40 into it and C
//! has no answer: a shift count at or past the width of the promoted type is
//! undefined behaviour, and the result is whatever the target does. SHFT is a
//! runtime value, so the form that matters is a register-controlled shift.
//! Executed with gcc 13.3.0 -O2 on x86_64 that gives `15u << 40` == 3840 and
//! `240u >> 40` == 0 — both the count masked to five bits (`15 << 8` is 3840,
//! `240 >> 8` is 0), so the two directions agree rather than differ. Written
//! with a literal 40 the same gcc folds both to 0, and clang 18.1.3 targeting
//! `aarch64-linux-gnu` or `armv7-none-eabi` emits a function that returns its
//! argument unshifted (`shl_const: ret`). Three answers for one expression —
//! and no execution on AArch64 or armv7 here, only the instruction selection.
//!
//! So there is no C behaviour to port, and the port picks the one answer that
//! is the same on every target: saturate to zero. DELIBERATE DEVIATION — C is
//! undefined here, and reproducing one host's undefined result would make our
//! RVAL depend on the build target. Eleven of the twelve SHFT sites in
//! `server/records/` already did this with `checked_sh*().unwrap_or(0)`;
//! `mbbo_direct`'s `wrapping_shl` was the twelfth and produced 3840 — Rust's
//! wrapping shift is the mod-32 rule — where its own input record produced 0,
//! so a VAL that went out came back as something else.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The input records take `Raw Soft Channel`, the one soft dset that lands its
/// read in RVAL and lets `convert` run (`devMbbiDirectSoftRaw.c`); the plain
/// `Soft Channel` writes VAL directly and returns 2 to skip the convert, so it
/// never reaches the shift under test.
const DB: &str = r#"
record(ai, "SRC") { field(VAL, "240") }

record(mbboDirect, "OUT:BIG")  { field(SHFT, "40") }
record(mbboDirect, "OUT:SANE") { field(SHFT, "4") }

record(mbbiDirect, "IN:BIG") {
    field(DTYP, "Raw Soft Channel") field(INP, "SRC") field(SHFT, "40")
}
record(mbbiDirect, "IN:SANE") {
    field(DTYP, "Raw Soft Channel") field(INP, "SRC") field(SHFT, "4")
}
"#;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &Db, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn field(db: &Db, rec: &str, name: &str) -> i64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field(name)
        .and_then(|v| v.to_f64())
        .map(|v| v as i64)
        .unwrap_or_else(|| panic!("{rec}.{name}"))
}

/// The out direction, `VAL << SHFT`. This is the site that used to wrap.
#[epics_macros_rs::epics_test]
async fn an_out_shift_past_the_word_gives_zero() {
    let db = build().await;
    db.put_pv("OUT:BIG", EpicsValue::Long(15)).await.unwrap();
    process(&db, "OUT:BIG").await;
    assert_eq!(
        field(&db, "OUT:BIG", "RVAL"),
        0,
        "SHFT 40 saturates; a mod-32 wrap would give 15 << 8 = 3840"
    );
}

/// The in direction reaches zero too, but one step earlier: the raw dset
/// positions MASK with the same out-of-range count (`prec->mask <<= prec->shft`,
/// `devMbbiDirectSoftRaw.c:48`), that saturates to 0, and `rval &= 0` empties
/// RVAL before the `>>` is reached. So this asserts the outcome, not the
/// operator — the `>>` itself is covered by the in-range control below.
#[epics_macros_rs::epics_test]
async fn an_in_shift_past_the_word_gives_zero() {
    let db = build().await;
    process(&db, "IN:BIG").await;
    assert_eq!(field(&db, "IN:BIG", "VAL"), 0, "SHFT 40 saturates");
}

/// Control at an in-range SHFT: saturation must not have eaten the ordinary
/// case, where C is defined and both directions must match it.
#[epics_macros_rs::epics_test]
async fn an_in_range_shift_still_shifts() {
    let db = build().await;

    db.put_pv("OUT:SANE", EpicsValue::Long(15)).await.unwrap();
    process(&db, "OUT:SANE").await;
    assert_eq!(field(&db, "OUT:SANE", "RVAL"), 240, "15 << 4");

    process(&db, "IN:SANE").await;
    assert_eq!(field(&db, "IN:SANE", "VAL"), 15, "240 >> 4");
}
