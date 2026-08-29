//! R10-66 — scalcout's previous-value pair, PVAL and PSVL.
//!
//! C keeps two "previous" cells and they are NOT the same thing:
//!
//! * `PVAL` (`sCalcoutRecord.dbd:60`, DBF_DOUBLE, writable) is the VAL from the
//!   end of the last process cycle. The OOPT switch compares against it
//!   (`sCalcoutRecord.c:379` On Change, `:382`/`:385` the transitions) and C
//!   advances it at `:393` — AFTER the switch has read it, on every cycle.
//! * `PSVL` (`sCalcoutRecord.dbd:63`, DBF_STRING, SPC_NOMOD) is the SVAL C
//!   last posted:
//!   `monitor()` posts SVAL when it differs from PSVL and then copies it
//!   (`:842-846`).
//!
//! The port had neither field. It had one private `prev_val`, captured at the
//! TOP of `process()` — the value VAL had at the *start of the cycle*, which is
//! the previous cycle's result only when nothing wrote VAL in between — and a
//! `prev_sval` that nothing read.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

async fn num(db: &PvDatabase, rec: &str, f: &str) -> f64 {
    field(db, rec, f).await.to_f64().unwrap()
}

fn scalcout(calc: &str) -> ScalcoutRecord {
    let mut c = ScalcoutRecord::new();
    c.put_field("CALC", EpicsValue::String(calc.into()))
        .unwrap();
    c.special("CALC", true).unwrap();
    c
}

/// A scalcout whose VAL follows SRC (an ai), with OOPT = On Change and an OUT
/// link into DEST. DEST's value is the record's "did the output fire" readout.
async fn on_change_db(mdel: f64) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("DEST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut c = scalcout("A");
    c.put_field("INPA", EpicsValue::String("SRC".into()))
        .unwrap();
    c.put_field("OUT", EpicsValue::String("DEST".into()))
        .unwrap();
    c.put_field("OOPT", EpicsValue::Short(1)).unwrap(); // On Change
    c.put_field("MDEL", EpicsValue::Double(mdel)).unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();
    db
}

/// PVAL is C's `pval`: the VAL from the END of the previous cycle. It is
/// readable, and it is what the OOPT comparison uses.
#[epics_macros_rs::epics_test]
async fn r10_66_pval_holds_the_previous_cycles_val() {
    let db = on_change_db(0.0).await;

    db.put_pv("SRC", EpicsValue::Double(5.0)).await.unwrap();
    process(&db, "SC").await;
    assert_eq!(num(&db, "SC", "VAL").await, 5.0);
    assert_eq!(
        num(&db, "SC", "PVAL").await,
        5.0,
        "C:397 advances pval = val at the end of the cycle"
    );

    db.put_pv("SRC", EpicsValue::Double(7.0)).await.unwrap();
    process(&db, "SC").await;
    assert_eq!(num(&db, "SC", "VAL").await, 7.0);
    assert_eq!(
        num(&db, "SC", "PVAL").await,
        7.0,
        "and again — PVAL never lags by more than the cycle it is advanced in"
    );
}

/// The divergence the missing field hid: a client write to VAL between cycles
/// moved the port's previous-value cell. C's `pval` is untouched by a `dbPut` to
/// VAL, so the next On-Change test still compares against the last COMPUTED
/// value — here 5, so a recompute back to 5 must NOT drive OUT.
#[epics_macros_rs::epics_test]
async fn r10_66_a_val_put_between_cycles_does_not_move_pval() {
    let db = on_change_db(0.0).await;

    // Cycle 1: VAL = 5, output fires (0 -> 5), DEST = 5. PVAL = 5.
    db.put_pv("SRC", EpicsValue::Double(5.0)).await.unwrap();
    process(&db, "SC").await;
    assert_eq!(
        num(&db, "DEST", "VAL").await,
        5.0,
        "the first change drives OUT"
    );
    assert_eq!(num(&db, "SC", "PVAL").await, 5.0);

    // An operator writes VAL directly. C's pval does not move.
    db.put_pv("SC.VAL", EpicsValue::Double(99.0)).await.unwrap();
    assert_eq!(
        num(&db, "SC", "PVAL").await,
        5.0,
        "a dbPut to VAL writes VAL and nothing else — pval is only advanced by \
         process() at :397"
    );

    // Cycle 2: the calc recomputes VAL = 5 (SRC unchanged). C compares 5 against
    // pval = 5: no change, no output. The old code compared against the value
    // captured at the top of this cycle (99) and drove OUT.
    db.put_pv("DEST", EpicsValue::Double(0.0)).await.unwrap();
    process(&db, "SC").await;
    assert_eq!(num(&db, "SC", "VAL").await, 5.0);
    assert_eq!(
        num(&db, "DEST", "VAL").await,
        0.0,
        "|pval - val| = 0 is not a change: C drives no output"
    );
}

/// PVAL is a plain DBF_DOUBLE in C — writable. An operator aims the next OOPT
/// decision with it: set PVAL away from VAL and the next cycle sees a change
/// even though the computed value did not move.
#[epics_macros_rs::epics_test]
async fn r10_66_pval_is_writable_and_aims_the_next_output() {
    let db = on_change_db(0.0).await;

    db.put_pv("SRC", EpicsValue::Double(5.0)).await.unwrap();
    process(&db, "SC").await;
    db.put_pv("DEST", EpicsValue::Double(0.0)).await.unwrap();

    // Without this put the next cycle is a no-change cycle (SRC is still 5).
    db.put_record_field_from_ca("SC", "PVAL", EpicsValue::Double(0.0))
        .await
        .unwrap();
    process(&db, "SC").await;

    assert_eq!(
        num(&db, "DEST", "VAL").await,
        5.0,
        "|pval(0) - val(5)| > mdel(0) — the write to PVAL forced the output"
    );
}

/// Negative control on the deadband: the On-Change test is `fabs(pval - val) >
/// mdel`, so a move inside MDEL drives nothing even though PVAL differs.
#[epics_macros_rs::epics_test]
async fn r10_66_pval_change_inside_mdel_drives_no_output() {
    let db = on_change_db(2.0).await;

    db.put_pv("SRC", EpicsValue::Double(5.0)).await.unwrap();
    process(&db, "SC").await;
    db.put_pv("DEST", EpicsValue::Double(0.0)).await.unwrap();

    db.put_pv("SRC", EpicsValue::Double(6.0)).await.unwrap();
    process(&db, "SC").await;

    assert_eq!(num(&db, "SC", "PVAL").await, 6.0, "pval still advances");
    assert_eq!(
        num(&db, "DEST", "VAL").await,
        0.0,
        "|5 - 6| = 1 is inside MDEL = 2 — C:379 drives no output"
    );
}

/// PSVL is the SVAL C last posted (`monitor()`, :842-846), so after a completed
/// cycle it equals SVAL.
#[epics_macros_rs::epics_test]
async fn r10_66_psvl_tracks_the_posted_sval() {
    let db = PvDatabase::new();
    let mut c = scalcout("'ab'+'cd'");
    c.put_field("OOPT", EpicsValue::Short(6)).unwrap(); // Never — no OUT link needed
    db.add_record("SC", Box::new(c)).await.unwrap();

    assert_eq!(
        field(&db, "SC", "PSVL").await,
        EpicsValue::String("".into()),
        "nothing has been posted yet"
    );

    process(&db, "SC").await;

    assert_eq!(
        field(&db, "SC", "SVAL").await,
        EpicsValue::String("abcd".into())
    );
    assert_eq!(
        field(&db, "SC", "PSVL").await,
        EpicsValue::String("abcd".into()),
        "C's monitor() copies the SVAL it just posted into PSVL (:845)"
    );
}

/// PSVL is `special(SPC_NOMOD)` — C refuses a client put outright.
#[epics_macros_rs::epics_test]
async fn r10_66_psvl_is_read_only() {
    let db = PvDatabase::new();
    db.add_record("SC", Box::new(scalcout("0"))).await.unwrap();

    let err = db
        .put_record_field_from_ca("SC", "PSVL", EpicsValue::String("nope".into()))
        .await;

    assert!(
        err.is_err(),
        "SPC_NOMOD: C's dbPut returns S_db_noMod for PSVL"
    );
}
