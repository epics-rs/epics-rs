//! A `.db` file cannot assign a `DBF_NOACCESS` field, whichever record declares it.
//!
//! C `dbPutString` switches on the declared field type and has one arm for the
//! whole family (`dbStaticLib.c:2646-2650`, R7.0.10):
//!
//! ```c
//!     case DBF_NOACCESS:
//!         dbMsgPrint(pdbentry, "Can't set array field before iocInit()");
//!         /* fall through */
//!     default:
//!         return S_dbLib_badField;
//! ```
//!
//! The port refused five fields it had been told about by name
//! (`Record::long_string_fields`: lsi/lso/printf VAL, lsi/lso OVAL) and accepted
//! the other 172 rows the vendored `.dbd`s declare — `waveform.VAL`,
//! `aai.VAL`, `aao.VAL`, `subArray.VAL`, every `aSub.VALx`, every `SIMPVT`.
//! The gate now reads the declaration, so the list cannot fall behind it again.
//!
//! Boundaries of that one arm, one case each: a declared-NOACCESS field with a
//! descriptor, one without, the `SPC_DBADDR` field that is NOT NOACCESS and must
//! still load, and an ordinary field on the same record.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

mod module_records;

async fn load(db: &str) -> Result<(), String> {
    let builder = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .expect("parse is fine — the refusal happens at field application");
    builder.build().await.map(|_| ()).map_err(|e| e.to_string())
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
        let mut record = module_records::create_any(&def.record_type)
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

/// The four array records reported: C declares VAL `DBF_NOACCESS` +
/// `special(SPC_DBADDR)` on every one of them (`waveformRecord.dbd.pod:400-403`,
/// `aaiRecord.dbd.pod:308-311`, `aaoRecord.dbd.pod:330-333`,
/// `subArrayRecord.dbd.pod:323-326`), and the port accepted all four.
#[epics_macros_rs::epics_test]
async fn an_array_records_val_is_refused_from_a_db_file() {
    for (rt, extra) in [
        ("waveform", r#"field(FTVL,"DOUBLE") field(NELM,"4")"#),
        ("aai", r#"field(FTVL,"DOUBLE") field(NELM,"4")"#),
        ("aao", r#"field(FTVL,"DOUBLE") field(NELM,"4")"#),
        ("subArray", r#"field(FTVL,"DOUBLE") field(MALM,"4")"#),
    ] {
        let db = format!(r#"record({rt}, "R") {{ {extra} field(VAL, "1.5") }}"#);
        let status = load(&db)
            .await
            .expect_err("C answers S_dbLib_badField for a DBF_NOACCESS field");
        assert_eq!(status, failed_to_load(), "{rt}.VAL must fail the load");
        let msg = reason(&db);
        assert!(
            msg.contains("before iocInit"),
            "{rt}.VAL must name C's constraint, got: {msg}"
        );
    }
}

/// The other shape of the same declaration: a `DBF_NOACCESS` field with no
/// `SPC_DBADDR`, which the generator drops from the field table entirely. It has
/// no descriptor to carry the flag, so the refusal comes off the dropped-name
/// list — and without it the value fell through to `common_fields` and was
/// stored on a field C refuses.
#[epics_macros_rs::epics_test]
async fn a_dropped_internal_is_refused_too() {
    let db = r#"record(ai, "A") { field(SIMPVT, "1") }"#;
    let status = load(db)
        .await
        .expect_err("SIMPVT is DBF_NOACCESS in aiRecord.dbd");
    assert_eq!(status, failed_to_load());
    let msg = reason(db);
    assert!(msg.contains("before iocInit"), "got: {msg}");
}

/// The boundary that says the gate reads the DECLARATION and not
/// `special(SPC_DBADDR)`: `mbbo.VAL` is `SPC_DBADDR` and runtime-typed exactly
/// like `waveform.VAL`, but C declares it `DBF_ENUM`, so it loads.
#[epics_macros_rs::epics_test]
async fn an_spc_dbaddr_field_that_is_not_noaccess_still_loads() {
    load(r#"record(mbbo, "M") { field(VAL, "1") }"#)
        .await
        .expect("mbbo VAL is DBF_ENUM + SPC_DBADDR — settable in C");
}

/// Control: the gate is per-field, so ordinary fields on a refused record's own
/// type still load.
#[epics_macros_rs::epics_test]
async fn ordinary_fields_on_the_same_record_still_load() {
    load(r#"record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "8") }"#)
        .await
        .expect("only the DBF_NOACCESS field is refused");
}

/// The structural closure: every `field(NAME,DBF_NOACCESS)` the vendored `.dbd`s
/// declare, refused. The count is asserted because the population is what the
/// by-name list could not track — 172 of these 178 rows loaded before the gate
/// read the declaration.
#[epics_macros_rs::epics_test]
async fn every_declared_noaccess_row_is_refused() {
    let mut rows = 0usize;
    let mut accepted: Vec<String> = Vec::new();

    for entry in std::fs::read_dir("dbd").expect("the vendored .dbd directory") {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let Some(stem) = file.strip_suffix("Record.dbd") else {
            continue;
        };
        let Some(record_type) = epics_base_rs::server::record::dbd_generated::RECORD_TYPES
            .iter()
            .find(|r| r.eq_ignore_ascii_case(stem))
        else {
            continue;
        };
        let text = std::fs::read_to_string(&path).unwrap();

        for field in declared_noaccess_names(&text) {
            rows += 1;
            // The seven module-owned types are not in `stdRecords.dbd`, so Base's
            // default registry does not claim them; the sweep is an application
            // that opted in, exactly as the other whole-set walkers are.
            let mut record = module_records::create_any(record_type)
                .unwrap_or_else(|e| panic!("{record_type}: create_record failed: {e}"));
            let mut common = Vec::new();
            if epics_base_rs::server::db_loader::apply_fields(
                &mut record,
                &[epics_base_rs::server::db_loader::DbFieldDef::new(
                    field.clone(),
                    "1",
                )],
                &mut common,
            )
            .is_ok()
            {
                accepted.push(format!("{record_type}.{field}"));
            }
        }
    }

    assert!(
        accepted.is_empty(),
        "C answers S_dbLib_badField for these, the loader accepted them: {accepted:#?}"
    );
    assert_eq!(
        rows, 178,
        "DBF_NOACCESS rows declared across the vendored .dbd files; a change here \
         means the population moved and the new number needs reading"
    );
}

/// `field(NAME,DBF_NOACCESS)` names in one `.dbd`, in declaration order.
fn declared_noaccess_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("field(") {
        rest = &rest[i + "field(".len()..];
        let Some(close) = rest.find(')') else { break };
        let header = &rest[..close];
        let mut parts = header.split(',');
        let name = parts.next().unwrap_or("").trim();
        if parts.next().map(str::trim) == Some("DBF_NOACCESS") && !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}
