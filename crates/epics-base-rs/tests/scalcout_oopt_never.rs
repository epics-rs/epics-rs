//! R18-4: `OOPT="Never"` must drive NO output, on every record whose menu has it.
//!
//! C's OOPT switch DECIDES an output; it does not veto one. `doOutput` starts at
//! 0 (`sCalcoutRecord.c:326`, `aCalcoutRecord.c:283`) and only a case that fires
//! raises it, so `Never` is an explicit `doOutput = 0` (`sCalcoutRecord.c:393-395`)
//! — and so is any menu index the switch does not name.
//!
//! scalcout's port switch matched `0..=5` and fell into `_ => true`, so menu
//! index 6 ("Never", `sCalcoutRecord.dbd:17`) drove the OUT link on EVERY cycle
//! — an exact polarity inversion on a physical link. acalcout named `6 => false`
//! but kept the same `_ => true` catch-all, breaking the other half of C's rule.
//!
//! Boundaries: Never vs Every Time (the OUT target moves / does not move), and
//! the unnamed-index catch-all.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "T:NEVER")  { field(VAL, "0") }
record(ao, "T:EVERY")  { field(VAL, "0") }
record(ao, "T:ANEVER") { field(VAL, "0") }
record(scalcout, "S:NEVER") {
    field(CALC, "7")
    field(OOPT, "Never")
    field(OUT, "T:NEVER PP")
}
record(scalcout, "S:EVERY") {
    field(CALC, "7")
    field(OOPT, "Every Time")
    field(OUT, "T:EVERY PP")
}
record(acalcout, "A:NEVER") {
    field(CALC, "7")
    field(OOPT, "Never")
    field(OUT, "T:ANEVER PP")
}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// `OOPT="Never"`, `CALC="7"`: C writes nothing to OUT. The port wrote 7.0.
#[tokio::test]
async fn scalcout_never_writes_nothing_to_out() {
    let db = build().await;

    for _ in 0..3 {
        process(&db, "S:NEVER").await;
    }

    assert_eq!(
        db.get_pv("S:NEVER").unwrap().to_f64(),
        Some(7.0),
        "the calc still runs — only the OUT write is suppressed"
    );
    assert_eq!(
        db.get_pv("T:NEVER").unwrap(),
        EpicsValue::Double(0.0),
        "C `case scalcoutOOPT_Never: doOutput = 0` — the OUT target never moves"
    );
}

/// The owner path: `Every Time` still drives OUT, so the fix is a polarity
/// correction on index 6, not a disabled output stage.
#[tokio::test]
async fn scalcout_every_time_still_drives_out() {
    let db = build().await;

    process(&db, "S:EVERY").await;

    assert_eq!(
        db.get_pv("T:EVERY").unwrap(),
        EpicsValue::Double(7.0),
        "OOPT=Every Time drives the OUT link"
    );
}

/// acalcout's `Never` (same menu index, same C rule).
#[tokio::test]
async fn acalcout_never_writes_nothing_to_out() {
    let db = build().await;

    for _ in 0..3 {
        process(&db, "A:NEVER").await;
    }

    assert_eq!(
        db.get_pv("T:ANEVER").unwrap(),
        EpicsValue::Double(0.0),
        "C `aCalcoutRecord.c` Never: doOutput = 0"
    );
}

/// The catch-all: an OOPT index the switch does not name is C's untouched
/// `doOutput = 0` — no output. Driven through the record's own field put, which
/// is the only way an out-of-menu index can arise.
#[tokio::test]
async fn an_unnamed_oopt_index_drives_no_output() {
    let db = build().await;

    let rec = db.get_record("S:EVERY").unwrap();
    {
        let mut inst = rec.write();
        // Past the last menu choice (6 = Never) — C's switch has no case for it.
        inst.record
            .put_field_internal("OOPT", EpicsValue::Short(9))
            .unwrap();
    }
    process(&db, "S:EVERY").await;

    assert_eq!(
        db.get_pv("T:EVERY").unwrap(),
        EpicsValue::Double(0.0),
        "C initialises doOutput = 0 and only a matching case raises it"
    );
}
