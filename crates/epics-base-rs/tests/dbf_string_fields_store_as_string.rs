//! A record-owned `DBF_STRING` field must STORE a string.
//!
//! `apply_fields` (`db_loader/mod.rs:1694-1700`) parses a `.db` field value as
//! the type the record *stores* — `get_field(name).db_field_type()` first, the
//! `.dbd` row only as the fallback — because `put_field`'s arms match on the
//! stored representation and a `menu()` field is `DBF_MENU` in the `.dbd` but a
//! `Short` choice index in the record. That makes the stored type the load
//! path's parse spec, so a record that stores a `DBF_STRING` field as anything
//! else rejects the very text C accepts: `EpicsValue::parse_bytes` is handed
//! `Char` and `field(IVOV,"keep")` fails to parse as a number.
//!
//! `lso.IVOV` was that field. C declares it `DBF_STRING size(40)`
//! (`lsoRecord.dbd.pod:155-160`, R7.0.10) exactly as `stringout` does, and
//! `stringout` stores it as a string (`stringout.rs:248-253`); only `lso` returned
//! a `CharArray`, borrowing the representation its genuinely-long VAL/OVAL need
//! (`lsoRecord.c:63` `callocMustSucceed(1, sizv, …)`) for a field C gives a
//! plain `char ivov[40]`.
//!
//! Boundaries, one case each: the load the wrong type rejected, the 40-byte
//! budget the declaration carries (C `putStringString`, `dbConvert.c:923-926`,
//! `strncpy(pdst, psrc, size); *(pdst+size-1) = 0;`), and the invariant itself
//! across every record type, so the next field to drift is caught by count.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields};
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};
use std::collections::HashMap;

mod module_records;

/// The load `apply_fields` rejected: `IVOV` is `DBF_STRING`, and `keep` is not
/// a number.
#[epics_macros_rs::epics_test]
async fn a_dbf_string_field_loads_the_text_a_db_file_carries() {
    let db = IocBuilder::new()
        .db_string(
            r#"record(lso, "L") { field(IVOV, "keep") }"#,
            &HashMap::new(),
        )
        .expect("field(IVOV,\"keep\") is a legal .db row — C's IVOV is DBF_STRING")
        .build()
        .await
        .unwrap()
        .0;

    assert_eq!(
        db.get_record("L").unwrap().read().record.get_field("IVOV"),
        Some(EpicsValue::String(PvString::from("keep"))),
        "a DBF_STRING field reads back the string it was loaded with"
    );
}

/// The budget the declaration carries. C copies at most `field_size` bytes and
/// NUL-terminates the last one, so a 40-byte field holds 39 bytes of text.
#[test]
fn a_dbf_string_field_keeps_its_forty_byte_budget() {
    let mut rec = epics_base_rs::server::db_loader::create_record("lso").unwrap();
    let long = "x".repeat(45);
    rec.put_field("IVOV", EpicsValue::String(PvString::from(long.as_str())))
        .unwrap();

    let Some(EpicsValue::String(got)) = rec.get_field("IVOV") else {
        panic!("IVOV must store a string");
    };
    assert_eq!(
        got.len(),
        39,
        "dbConvert.c:923-926 keeps size-1 bytes and NUL-terminates: 40 -> 39"
    );
}

/// The invariant, over every record type a vendored `.dbd` declares. The count
/// is asserted so that a field silently dropping out of the sweep — a `.dbd`
/// row that stops being `DBF_STRING`, a record that stops answering
/// `get_field` — shows up as a number change rather than as a quieter pass.
#[test]
fn every_dbf_string_field_reports_dbf_string() {
    let mut scanned = 0usize;
    let mut wrong: Vec<String> = Vec::new();

    for rt in RECORD_TYPES {
        // The seven module-owned types are not in `stdRecords.dbd`, so Base's
        // default registry does not claim them; the sweep is an application
        // that opted in, exactly as the other whole-set walkers are.
        let rec = module_records::create_any(rt)
            .unwrap_or_else(|e| panic!("{rt}: create_record failed: {e}"));
        let fields = record_fields(rt).unwrap_or_else(|| panic!("{rt}: no .dbd field table"));
        // A long-string field is `DBF_NOACCESS` + `SPC_DBADDR` in C and is
        // refused at load by name, above the parse (`db_loader/mod.rs:1675`),
        // so its stored `CharArray` never reaches `parse_bytes`.
        let long_strings = rec.long_string_fields();

        for f in fields {
            if f.dbf_type != DbFieldType::String
                || long_strings.iter().any(|l| l.eq_ignore_ascii_case(f.name))
            {
                continue;
            }
            // A field the record does not answer for is framework-owned (INP,
            // OUT, DESC, …); `apply_fields` falls back to the `.dbd` row for
            // it, which is `DBF_STRING` by construction.
            let Some(v) = rec.get_field(f.name) else {
                continue;
            };
            scanned += 1;
            if v.db_field_type() != DbFieldType::String {
                wrong.push(format!(
                    "{rt}.{} declares DBF_STRING size({}) but stores {:?}",
                    f.name,
                    f.size,
                    v.db_field_type()
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "a DBF_STRING field stored as another type rejects the .db text C accepts: {wrong:#?}"
    );
    assert_eq!(
        scanned, 519,
        "record-owned DBF_STRING fields on this tree; a change here means a \
         field left or joined the sweep and the new count needs reading. \
         507 before scalcout began answering for PAA..PLL, which this port \
         serves as read-only scalar DBF_STRING (calc#42)"
    );
}
