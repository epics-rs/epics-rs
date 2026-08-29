//! The generated field tables, pinned against the compiled C IOC.
//!
//! `tools/dbd-codegen` derives every field's type from the vendored `.dbd`. The
//! `.dbd` is not the last word on what a client sees: `cvt_dbaddr` re-types a
//! field at name-resolution time, `menu()` fields are served as `DBR_ENUM`, and
//! CA has no unsigned or 64-bit type so `ca_wire_type` promotes. This test
//! asserts the whole chain lands where the real C IOC lands.
//!
//! `tests/fixtures/c_native_types.tsv` is *measured*: it is `cainfo` run against
//! every field of every base record type on
//! `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`. See its header for
//! the instantiation (NELM=10, FTVL=DOUBLE, ...) the array records were probed
//! under. If the generator and this fixture disagree, the generator is wrong —
//! the fixture is what C serves.

use epics_base_rs::server::record::FieldDesc;
use epics_base_rs::server::record::dbd_generated::{DB_COMMON_FIELDS, record_fields};
use epics_base_rs::types::DbFieldType;

const ORACLE: &str = include_str!("fixtures/c_native_types.tsv");

/// The CA type code C's `cainfo` names, as `DbFieldType` numbers it.
fn ca_code(dbf: &str) -> Option<u16> {
    Some(match dbf {
        "DBF_STRING" => DbFieldType::String,
        "DBF_SHORT" => DbFieldType::Short,
        "DBF_FLOAT" => DbFieldType::Float,
        "DBF_ENUM" => DbFieldType::Enum,
        "DBF_CHAR" => DbFieldType::Char,
        "DBF_LONG" => DbFieldType::Long,
        "DBF_DOUBLE" => DbFieldType::Double,
        _ => return None,
    } as u16)
}

fn oracle_rows() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    ORACLE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let c: Vec<&str> = l.split('\t').collect();
            assert!(c.len() >= 5, "malformed oracle row: {l}");
            (c[0], c[1], c[4])
        })
}

fn lookup(record: &str, field: &str) -> Option<&'static FieldDesc> {
    record_fields(record)
        .and_then(|fs: &'static [FieldDesc]| fs.iter().find(|f| f.name == field))
        .or_else(|| DB_COMMON_FIELDS.iter().find(|f| f.name == field))
}

/// Every field the C IOC serves is typed by the generator exactly as C types it.
#[test]
fn generated_types_match_the_c_oracle() {
    let mut checked = 0usize;
    let mut missing = Vec::new();
    let mut wrong = Vec::new();

    for (record, field, c_native) in oracle_rows() {
        // Only base record types were probed; the fixture carries no others.
        if record_fields(record).is_none() {
            continue;
        }
        let Some(desc) = lookup(record, field) else {
            missing.push(format!("{record}.{field}"));
            continue;
        };
        let want = ca_code(c_native).unwrap_or_else(|| panic!("{record}.{field}: {c_native}?"));
        let got = desc.dbf_type.ca_wire_type();
        if got != want {
            wrong.push(format!(
                "{record}.{field}: C serves {c_native} (code {want}), \
                 generator says {:?} (wire code {got})",
                desc.dbf_type
            ));
        }
        checked += 1;
    }

    assert!(
        missing.is_empty(),
        "{} field(s) the C IOC serves are absent from the generated tables:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
    assert!(
        wrong.is_empty(),
        "{} field(s) disagree with the C oracle:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
    assert!(checked > 2000, "oracle covered only {checked} fields");
}

/// Not every `DBF_ENUM` field draws its choices from a `.dbd` menu. Two kinds
/// build them from record-instance state instead, so a static table cannot and
/// must not carry them:
///
/// * `DTYP` is `DBF_DEVICE` — the choices are the device supports registered
///   for that record type.
/// * `bi`/`bo`/`mbbi`/`mbbo` `VAL` take their choices from the record's own
///   state-name fields (`ZNAM`/`ONAM`, `ZRST`..`FFST`), which is what those
///   records' `get_enum_strs` reads.
fn choices_are_runtime_state(record: &str, field: &str) -> bool {
    field == "DTYP" || (field == "VAL" && matches!(record, "bi" | "bo" | "mbbi" | "mbbo"))
}

/// A `menu()` field is served as `DBR_ENUM` *with its choices*: a client asking
/// for `ai.LINR` must get `"NO CONVERSION"`, not `0`. Being typed Enum without
/// choices to serve `get_enum_strs` is the half-migration this guards against.
///
/// "Carries" means *resolves to*, not "has a table in its own descriptor".
/// `menu_choices_of` asks the descriptor first and the shared by-name registry
/// second, and one menu can only be answered by the second: `menuScan` is
/// site data that C re-reads at run time (`initPeriodic`, `dbScan.c:858`), so
/// `SCAN`/`SSCN` resolve through the loaded menu and carry no static table.
/// Asserting on `desc.menu` alone would have made a field that serves the
/// RIGHT labels look naked, and a field pinned to the WRONG (stock) labels
/// look correct.
#[test]
fn every_generated_menu_field_carries_its_choices() {
    let mut naked = Vec::new();
    for (record, field, c_native) in oracle_rows() {
        if c_native != "DBF_ENUM" || record_fields(record).is_none() {
            continue;
        }
        let Some(desc) = lookup(record, field) else {
            continue;
        };
        if choices_are_runtime_state(record, field) {
            continue;
        }
        let choices = desc
            .menu
            .or_else(|| epics_base_rs::server::record::shared_menu_choices(field));
        if choices.is_none_or(|m| m.is_empty()) {
            naked.push(format!("{record}.{field}"));
        }
    }
    assert!(
        naked.is_empty(),
        "{} field(s) C serves as DBF_ENUM have no choices to serve get_enum_strs:\n  {}",
        naked.len(),
        naked.join("\n  ")
    );
}
