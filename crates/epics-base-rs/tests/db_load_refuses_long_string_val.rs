//! UI-80 (epics-base#548): `field(VAL, "...")` on a long-string record
//! (`lso`/`lsi`/`printf`) is refused at `.db` load, as in C — the field is
//! `DBF_NOACCESS` + `SPC_DBADDR`, and the reference softIoc reports
//! `Can't set 'L.VAL' to 'hello' Can't set array field before iocInit() :
//! Bad Field value` (measured). The port refused too, but with an
//! accidental Char-parse diagnostic that blamed the value's syntax; the
//! loader names the real constraint. The gate is now the declared
//! `DBF_NOACCESS` property rather than the `long_string_fields` name list
//! this test was written against — see `db_load_refuses_declared_noaccess`,
//! which covers the other 173 rows the name list never reached.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

async fn load(db: &str) -> Result<(), String> {
    let builder = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .expect("parse is fine — the refusal happens at field application");
    match builder.build().await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// The load's status when the text is refused: C `dbLoadRecords` reports
/// each refusal at the field and returns only a status, which `softMain`
/// prints as `ERROR: Failed to load '%s'`.
fn failed_to_load() -> String {
    format!(
        "Failed to load '{}'",
        epics_base_rs::server::db_loader::DB_STRING_SOURCE
    )
}

/// Why the loader refused. The status above carries the source, not the
/// reason, so the reason is asked of `apply_fields` — the same step the
/// builder calls per record, and the one that refuses.
fn reason(db: &str) -> String {
    let defs = epics_base_rs::server::db_loader::parse_db(db, &HashMap::new())
        .expect("parse is fine — the refusal happens at field application");
    for def in defs {
        let mut record = epics_base_rs::server::db_loader::create_record_with_factories(
            &def.record_type,
            &HashMap::new(),
        )
        .unwrap_or_else(|e| panic!("{}: create_record failed: {e}", def.record_type));
        let mut common = Vec::new();
        if let Err(e) =
            epics_base_rs::server::db_loader::apply_fields(&mut record, &def.fields, &mut common)
        {
            return e.to_string();
        }
    }
    panic!("no field was refused in: {db}");
}

#[epics_macros_rs::epics_test]
async fn lso_val_in_db_is_refused_with_the_real_reason() {
    let db = r#"record(lso, "L") { field(VAL, "hello") }"#;
    let status = load(db)
        .await
        .expect_err("C refuses lso VAL from a .db (measured on softIoc)");
    assert_eq!(status, failed_to_load());
    let msg = reason(db);
    assert!(
        msg.contains("before iocInit"),
        "diagnostic must name the real constraint, got: {msg}"
    );
    assert!(
        !msg.contains("cannot parse"),
        "must not blame the value's syntax, got: {msg}"
    );
}

#[epics_macros_rs::epics_test]
async fn printf_val_in_db_is_refused_and_other_lso_fields_still_load() {
    // Family member: printf VAL is in `long_string_fields` too.
    let db = r#"record(printf, "P") { field(VAL, "x") }"#;
    let status = load(db)
        .await
        .expect_err("printf VAL is the same DBF_NOACCESS refusal family");
    assert_eq!(status, failed_to_load());
    let msg = reason(db);
    assert!(msg.contains("before iocInit"), "got: {msg}");

    // Control: the gate is field-specific — an ordinary lso field loads.
    load(r#"record(lso, "L2") { field(SIZV, "80") }"#)
        .await
        .expect("non-long-string lso fields must still load");
}
