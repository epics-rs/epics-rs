//! A `.db` menu value C's `dbPutString` will not take fails the load.
//!
//! C `dbRecordField` (`dbLexRoutines.c:1406-1416` @`R7.0.10`) prints the
//! refusal and calls `yyerror(NULL)`, so `record(ai,"M1"){field(SCAN,"Passiv")}`
//! ends in `ERROR: Failed to load 'm.db'` and softIoc exits 2 (measured). The
//! port used to print a warning of its own and load a Passive record, which is
//! an IOC that scans differently from the database it was given.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

async fn load(db: &str) -> Result<(), String> {
    let builder = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .expect("the value refusal is not a parse error — C parses the field fine");
    match builder.build().await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[epics_macros_rs::epics_test]
async fn a_misspelled_scan_choice_fails_the_load() {
    load(r#"record(ai, "MENUREF:M1") { field(SCAN, "Passiv") }"#)
        .await
        .expect_err("C refuses the database (measured on softIoc @R7.0.10)");
}

#[epics_macros_rs::epics_test]
async fn the_same_field_still_loads_every_choice_c_takes() {
    // An exact choice, an in-menu index, one past the last choice, the
    // `USHRT_MAX` sentinel, and the empty value C reads as zero — all measured
    // as accepted.
    load(
        r#"
        record(ai, "MENUREF:OK1") { field(SCAN, "1 second") }
        record(ai, "MENUREF:OK2") { field(PINI, "3") }
        record(ai, "MENUREF:OK3") { field(PINI, "6") }
        record(ai, "MENUREF:OK4") { field(PINI, "65535") }
        record(ai, "MENUREF:OK5") { field(PINI, "") }
        "#,
    )
    .await
    .expect("C loads all five");
}

/// The record's OWN menu fields are applied by `apply_fields`, not by the
/// common-field sink, and that half already failed the load. Pinned here so
/// the two halves cannot drift apart again — what it does NOT yet share is the
/// wording: this one still reports the port's own diagnostic rather than C's
/// `Can't set 'Y1.SELM' to 'Bogus' using menu selSELM : Illegal choice`.
#[epics_macros_rs::epics_test]
async fn a_record_type_own_menu_field_already_failed_the_load() {
    load(r#"record(sel, "MENUREF:Y1") { field(SELM, "Bogus") }"#)
        .await
        .expect_err("C refuses selSELM the same way");
}

/// C's numeric arm: a value that parses but clears no bound is refused with
/// `Bad Field value`, not `Illegal choice` — and it still fails the load.
#[epics_macros_rs::epics_test]
async fn an_out_of_menu_index_fails_the_load() {
    load(r#"record(ai, "MENUREF:P2") { field(PINI, "7") }"#)
        .await
        .expect_err("menuPini has six choices, so 7 is past C's bound");
}
