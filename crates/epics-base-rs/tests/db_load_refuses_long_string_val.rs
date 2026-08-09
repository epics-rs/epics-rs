//! UI-80 (epics-base#548): `field(VAL, "...")` on a long-string record
//! (`lso`/`lsi`/`printf`) is refused at `.db` load, as in C — the field is
//! `DBF_NOACCESS` + `SPC_DBADDR`, and the reference softIoc reports
//! `Can't set 'L.VAL' to 'hello' Can't set array field before iocInit() :
//! Bad Field value` (measured). The port refused too, but with an
//! accidental Char-parse diagnostic that blamed the value's syntax; the
//! loader now consults `Record::long_string_fields` and names the real
//! constraint.

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

#[epics_macros_rs::epics_test]
async fn lso_val_in_db_is_refused_with_the_real_reason() {
    let msg = load(r#"record(lso, "L") { field(VAL, "hello") }"#)
        .await
        .expect_err("C refuses lso VAL from a .db (measured on softIoc)");
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
    let msg = load(r#"record(printf, "P") { field(VAL, "x") }"#)
        .await
        .expect_err("printf VAL is the same DBF_NOACCESS refusal family");
    assert!(msg.contains("before iocInit"), "got: {msg}");

    // Control: the gate is field-specific — an ordinary lso field loads.
    load(r#"record(lso, "L2") { field(SIZV, "80") }"#)
        .await
        .expect("non-long-string lso fields must still load");
}
