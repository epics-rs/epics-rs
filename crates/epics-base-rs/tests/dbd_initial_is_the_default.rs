//! R19-122 — a record's default field values come from its `.dbd` `initial(...)`.
//!
//! C `dbReadDatabase` builds one zeroed PROTOTYPE per record type and writes
//! each field's `initial("…")` into it (`dbLexRoutines.c`, `dbPutStringNum`);
//! `dbCreateRecord` copies that prototype, and the `.db`'s own `field(...)` lines
//! overwrite it. So `record(calc,"X") {}` in C is a record whose `CALC` is the
//! string `"0"`.
//!
//! The port never read `initial(...)`: every record hand-wrote a Rust `Default`,
//! and calc, calcout and swait all forgot the CALC one (`calcRecord.dbd:26`,
//! `calcoutRecord.dbd:62,382`, `swaitRecord.dbd:424` — all `initial("0")`). A
//! default record therefore carried the EMPTY expression, which base's
//! `postfix()` refuses outright (`CALC_ERR_NULL_ARG`, `postfix.c:235-240`), so
//! every evaluation failed and the record sat in CALC/INVALID.
//!
//! Measured on the compiled C IOC (`softIoc -S -d`, `record(calc,"T:C") {}`):
//!
//! ```text
//! caget T:C.CALC  ->  0
//! caput -c T:C.A 1 ; caget T:C.STAT T:C.SEVR  ->  NO_ALARM  NO_ALARM
//! ```
//!
//! The differential oracle scored this at 536 put-sweep DEFECTs across calc and
//! calcout: every put to a `pp(TRUE)` field processed the record and every one
//! of them alarmed.
//!
//! Boundaries: a record whose `.dbd` HAS the initial (calc/calcout/swait); a
//! record whose `.dbd` does NOT (scalcout/acalcout — synApps genuinely leaves
//! CALC empty there, and an empty CALC really does alarm, which is what
//! `calc_empty_program.rs` pins); a `.db` line overriding the initial; and a
//! non-calc initial, to show the rule is the loader's and not a calc special
//! case.

mod module_records;

use std::collections::HashMap;
use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

async fn build(db_text: &str) -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(db_text, &HashMap::new())
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

async fn string_of(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{pv} reads as {other:?}"),
    }
}

/// STAT/SEVR as a client sees them — the numeric codes a `caget` reads back.
async fn alarm_of(db: &PvDatabase, rec: &str) -> (i64, i64) {
    let stat = db.get_pv(&format!("{rec}.STAT")).unwrap();
    let sevr = db.get_pv(&format!("{rec}.SEVR")).unwrap();
    (stat.to_f64().unwrap() as i64, sevr.to_f64().unwrap() as i64)
}

/// `alarm_status::CALC_ALARM` / `AlarmSeverity::Invalid` as they are served.
const CALC_ALARM: i64 = epics_base_rs::server::recgbl::alarm_status::CALC_ALARM as i64;
const NO_ALARM: i64 = 0;
const INVALID: i64 = 3;

/// The finding, at the record: a plain `record(calc)` must compute, not alarm.
#[epics_macros_rs::epics_test]
async fn a_default_calc_record_computes_instead_of_alarming() {
    let db = build(r#"record(calc, "D:CALC") {}"#).await;

    assert_eq!(
        string_of(&db, "D:CALC.CALC").await,
        "0",
        "calcRecord.dbd:26 — initial(\"0\")"
    );

    process(&db, "D:CALC").await;

    assert_eq!(db.get_pv("D:CALC").unwrap().to_f64().unwrap(), 0.0);
    assert_eq!(
        alarm_of(&db, "D:CALC").await,
        (NO_ALARM, NO_ALARM),
        "C answers NO_ALARM/NO_ALARM here (measured on softIoc)"
    );
}

/// calcout carries the initial on BOTH of its expressions
/// (`calcoutRecord.dbd:62` CALC, `:382` OCAL) — an empty OCAL alarms exactly
/// like an empty CALC, so missing either one is the same defect.
#[epics_macros_rs::epics_test]
async fn a_default_calcout_record_has_both_expressions() {
    let db = build(r#"record(calcout, "D:OUT") {}"#).await;

    assert_eq!(string_of(&db, "D:OUT.CALC").await, "0");
    assert_eq!(string_of(&db, "D:OUT.OCAL").await, "0");

    process(&db, "D:OUT").await;
    assert_eq!(alarm_of(&db, "D:OUT").await, (NO_ALARM, NO_ALARM));
}

/// swait is the synApps record that DOES declare the initial
/// (`swaitRecord.dbd:424`) — and it compiles with base's `postfix()`, so the
/// empty expression was fatal there too.
#[epics_macros_rs::epics_test]
async fn a_default_swait_record_computes_instead_of_alarming() {
    let db = build(r#"record(swait, "D:WAIT") {}"#).await;

    assert_eq!(string_of(&db, "D:WAIT.CALC").await, "0");

    process(&db, "D:WAIT").await;
    assert_eq!(alarm_of(&db, "D:WAIT").await, (NO_ALARM, NO_ALARM));
}

/// The other side of the boundary: `sCalcoutRecord.dbd:68` and
/// `aCalcoutRecord.dbd:85` declare NO initial, so their CALC really is empty —
/// `sCalcPostfix("")`/`aCalcPostfix("")` accept it (status 0) and the perform
/// then fails every cycle (measured: `perform=-1`). The fix must not invent an
/// initial C does not have.
#[epics_macros_rs::epics_test]
async fn scalcout_and_acalcout_keep_the_empty_calc_their_dbd_declares() {
    let db = build(
        r#"
record(scalcout, "D:SCALC") {}
record(acalcout, "D:ACALC") {}
"#,
    )
    .await;

    assert_eq!(string_of(&db, "D:SCALC.CALC").await, "");
    assert_eq!(string_of(&db, "D:ACALC.CALC").await, "");

    // And an empty program still alarms — C's behaviour, not a regression.
    process(&db, "D:SCALC").await;
    assert_eq!(
        alarm_of(&db, "D:SCALC").await,
        (CALC_ALARM, INVALID),
        "an empty sCalc program compiles but fails every perform (measured: -1)"
    );
}

/// The prototype is what the `.db` OVERWRITES — the initial must not win over an
/// explicit field line, and must not be re-applied after it.
#[epics_macros_rs::epics_test]
async fn a_db_field_line_overrides_the_dbd_initial() {
    let db = build(
        r#"
record(calc, "D:EXPL") {
    field(CALC, "A+1")
    field(A, "4")
}
"#,
    )
    .await;

    assert_eq!(string_of(&db, "D:EXPL.CALC").await, "A+1");
    process(&db, "D:EXPL").await;
    assert_eq!(db.get_pv("D:EXPL").unwrap().to_f64().unwrap(), 5.0);
}

/// The rule belongs to the LOADER, not to the calc family: any field with an
/// initial gets it. `ai` declares `ASLO initial("1")` and `SDLY initial("-1.0")`
/// (`aiRecord.dbd`), and a slope of 0 would zero every raw-to-engineering
/// conversion.
#[epics_macros_rs::epics_test]
async fn the_initial_is_applied_to_every_record_type_not_just_the_calc_family() {
    let db = build(r#"record(ai, "D:AI") {}"#).await;

    assert_eq!(db.get_pv("D:AI.ASLO").unwrap().to_f64().unwrap(), 1.0);
    assert_eq!(db.get_pv("D:AI.ESLO").unwrap().to_f64().unwrap(), 1.0);
    assert_eq!(db.get_pv("D:AI.SDLY").unwrap().to_f64().unwrap(), -1.0);
}

/// The single owner: the record FACTORY hands back the prototype already seeded,
/// so there is no window in which a record exists with a default its `.dbd`
/// contradicts — every load path goes through it.
#[test]
fn the_record_factory_is_the_owner_of_the_dbd_initials() {
    for (record_type, field, want) in [
        ("calc", "CALC", "0"),
        ("calcout", "CALC", "0"),
        ("calcout", "OCAL", "0"),
        ("swait", "CALC", "0"),
        ("scalcout", "CALC", ""), // no initial in the C dbd
        ("acalcout", "CALC", ""),
    ] {
        let rec = module_records::create_any(record_type).unwrap();
        let got = match rec.get_field(field) {
            Some(EpicsValue::String(s)) => s.to_string(),
            other => panic!("{record_type}.{field} reads as {other:?}"),
        };
        assert_eq!(got, want, "{record_type}.{field} straight from the factory");
    }
}
