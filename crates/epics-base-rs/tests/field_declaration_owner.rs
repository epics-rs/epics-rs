//! A field has ONE declaration, and every consumer reads it from the same
//! table.
//!
//! `aSub` is the boundary case: its `.dbd` declares `FTA`..`FTU` and
//! `FTVA`..`FTVU` as `DBF_MENU`/`menu(menuFtype)`, while the record's
//! hand-written `field_list()` types them `DBF_SHORT` with no menu. The type
//! consumers asked the generated table (so the wire announced `DBF_ENUM`) and
//! the *choice* consumers asked the hand table (so the renderer found no menu
//! and printed the index). Measured on the compiled C IOC:
//!
//! ```text
//! $ caget -t ORACLE:ASUB.FTA ORACLE:ASUB.FTVA
//! DOUBLE
//! DOUBLE
//! ```
//!
//! The port served `10` — the raw `menuFtype` index — for both.
//!
//! Boundaries covered: the default choice; a choice put by index; a choice put
//! by label; a `dbCommon` menu field, whose declaration lives in a third table
//! (`DB_COMMON_FIELDS`) and whose stored variant is not its declared type. An
//! index PAST the menu is not a boundary: C's `getMenuString` fails the whole
//! `dbGet` with `S_db_badChoice` (dbConvert.c:875-882), and the menu converter
//! that owns every write to the field cannot store one.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::EpicsValue;

fn instance(record_type: &str) -> RecordInstance {
    let rec = create_record(record_type).expect("record type is registered");
    RecordInstance::new_boxed(format!("T:{record_type}"), rec)
}

/// A DBR_STRING (`caget -t`) read of the field: the payload up to its NUL.
fn dbr_string_of(inst: &RecordInstance, field: &str) -> String {
    let snap = inst.snapshot_for_field(field).expect("field exists");
    let bytes = epics_base_rs::types::encode_dbr(0, &snap).expect("DBR_STRING encodes");
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The `.dbd`'s `menu(menuFtype)` reaches the renderer even though the record's
/// hand table declares the same field `DBF_SHORT` with no menu.
#[test]
fn r21_asub_ftype_field_renders_its_menu_choice_not_the_index() {
    let inst = instance("aSub");
    // menuFtype index 10, the aSub default (aSubRecord.dbd: FTA .. FTU and
    // FTVA .. FTVU all `initial("DOUBLE")`).
    assert_eq!(dbr_string_of(&inst, "FTA"), "DOUBLE");
    assert_eq!(dbr_string_of(&inst, "FTVA"), "DOUBLE");
}

/// The choice follows the stored index — the renderer indexes the menu, it does
/// not memoise the initial label.
#[test]
fn r21_asub_ftype_index_put_renders_the_new_choice() {
    let mut inst = instance("aSub");
    // menuFtype: STRING CHAR UCHAR SHORT USHORT LONG ULONG INT64 UINT64 FLOAT
    //            DOUBLE ENUM
    inst.record.put_field("FTA", EpicsValue::Enum(0)).unwrap();
    assert_eq!(dbr_string_of(&inst, "FTA"), "STRING");
    inst.record.put_field("FTA", EpicsValue::Enum(5)).unwrap();
    assert_eq!(dbr_string_of(&inst, "FTA"), "LONG");
}

/// The write side reads the same declaration: a LABEL put resolves against the
/// `.dbd` menu, so `caput ASUB.FTA LONG` selects index 5. Before the fix the
/// label found no menu on the hand table and was parsed as a number — index 0.
#[test]
fn r21_asub_ftype_label_put_resolves_against_the_dbd_menu() {
    let mut inst = instance("aSub");
    let coerced = epics_base_rs::server::record::coerce_put_value(
        inst.record.as_ref(),
        "FTA",
        epics_base_rs::types::DbFieldType::Enum,
        EpicsValue::String(epics_base_rs::types::PvString::from("LONG")),
    )
    .expect("LONG is a menuFtype choice");
    let epics_base_rs::types::c_parse::Converted::Stored(coerced) = coerced else {
        panic!("a menu label resolves to a stored index");
    };
    inst.record.put_field("FTA", coerced).unwrap();
    assert_eq!(dbr_string_of(&inst, "FTA"), "LONG");
}

/// A `dbCommon` menu field: its declaration is in neither the record's generated
/// table nor its hand table, and the variant it is STORED as (`Short`) is not
/// the type it is DECLARED as (`DBF_MENU` → served `DBR_ENUM`). Both halves must
/// still land: the label resolves, and the stored index renders as its choice.
#[test]
fn r21_common_menu_field_resolves_its_label_and_renders_its_choice() {
    let mut inst = instance("ai");
    inst.put_common_field(
        "PRIO",
        EpicsValue::String(epics_base_rs::types::PvString::from("HIGH")),
    )
    .expect("HIGH is a menuPriority choice");
    assert_eq!(inst.common.prio, 2);
    assert_eq!(dbr_string_of(&inst, "PRIO"), "HIGH");
}

/// A runtime-typed field has NO type in its declaration — C's `cvt_dbaddr`
/// derives it from record state (`FTVL`), and the `.dbd` entry is a placeholder
/// (`DBF_DOUBLE`). A consumer that reads the placeholder coerces a string write
/// into a `FTVL=CHAR` waveform to `0.0`; the stored variant is the answer.
#[test]
fn r21_runtime_typed_field_has_no_declared_type() {
    let mut inst = instance("waveform");
    inst.record.put_field("FTVL", EpicsValue::Short(1)).unwrap(); // CHAR
    inst.record
        .put_field("VAL", EpicsValue::CharArray(vec![0; 8]))
        .unwrap();
    assert_eq!(inst.declared_field_type("VAL"), None);
    // A field with a real declaration still answers.
    assert_eq!(
        inst.declared_field_type("FTVL"),
        Some(epics_base_rs::types::DbFieldType::Enum)
    );
}
