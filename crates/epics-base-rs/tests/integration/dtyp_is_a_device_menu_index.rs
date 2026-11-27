//! `DTYP` is `DBF_DEVICE`: an index into the record type's device menu.
//!
//! Measured on the compiled C `softIoc` (`softIoc.dbd`), one bare record per
//! type, no DTYP set:
//!
//! ```text
//! $ caget -t D:AI.DTYP    -> Soft Channel     $ caget -t -n D:AI.DTYP    -> 0
//! $ caget -t D:WF.DTYP    -> Soft Channel     $ caget -t -n D:WF.DTYP    -> 0
//! $ caget -t D:CALC.DTYP  ->                  $ caget -t -n D:CALC.DTYP  -> 0
//! $ caget -t D:ASUB.DTYP  ->                  $ caget -t -n D:ASUB.DTYP  -> 0
//! ```
//!
//! The value is the index; the string is the choice at that index
//! (`dbConvert.c::getDeviceString`). An unset DTYP is index 0 — the FIRST
//! `device()` line the loaded `.dbd` declares for the type — and a record type
//! that declares no device support has no `dbDeviceMenu`, so C's get fails and
//! the client sees the empty string. The port served the string `0` for both.
//!
//! The boundaries: a type with a menu / a type without one / a declared choice
//! selected by name / a name no `.dbd` declares (the port's registry accepts
//! one, C's would not) / a put by label.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{
    DbLinkType, DevSup, Dset, Record, RecordInstance, coerce_put_value,
};
use epics_base_rs::types::c_parse::Converted;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

fn instance(record_type: &str) -> RecordInstance {
    let rec = create_record(record_type).expect("record type is registered");
    RecordInstance::new_boxed(format!("T:{record_type}"), rec)
}

/// `caget -t` — the DBR_STRING form.
fn dbr_string_of(inst: &RecordInstance, field: &str) -> String {
    let snap = inst.snapshot_for_field(field).expect("field exists");
    let bytes = epics_base_rs::types::encode_dbr(0, &snap).expect("DBR_STRING encodes");
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// `caget -t -n` — the value itself.
fn value_of(inst: &RecordInstance, field: &str) -> EpicsValue {
    inst.client_field_value(field).expect("field exists")
}

#[test]
fn r21_unset_dtyp_is_index_zero_and_renders_the_first_declared_device() {
    for rt in ["ai", "waveform", "mbbi", "stringin", "subArray"] {
        let inst = instance(rt);
        assert_eq!(value_of(&inst, "DTYP"), EpicsValue::Enum(0), "{rt}");
        assert_eq!(dbr_string_of(&inst, "DTYP"), "Soft Channel", "{rt}");
    }
}

#[test]
fn r21_a_record_type_with_no_device_support_renders_dtyp_empty() {
    for rt in ["calc", "sub", "aSub", "fanout"] {
        let inst = instance(rt);
        assert_eq!(value_of(&inst, "DTYP"), EpicsValue::Enum(0), "{rt}");
        assert_eq!(dbr_string_of(&inst, "DTYP"), "", "{rt}");
    }
}

/// The record `.db` text loads, with whatever device rows `builder` carries.
async fn loaded(builder: IocBuilder, db: &str) -> Arc<PvDatabase> {
    builder
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// A DTYP the `.dbd` declares: the index is its position in the device menu and
/// the string is its own name — not the menu head.
#[epics_macros_rs::epics_test]
async fn r21_a_declared_dtyp_serves_its_own_index_and_name() {
    let db = loaded(
        IocBuilder::new(),
        r#"record(ai, "T") { field(DTYP, "Async Soft Channel") }"#,
    )
    .await;
    let rec = db.get_record("T").unwrap();
    let inst = rec.read();
    // device(ai, ...): Soft Channel, Raw Soft Channel, Async Soft Channel, ...
    assert_eq!(value_of(&inst, "DTYP"), EpicsValue::Enum(2));
    assert_eq!(dbr_string_of(&inst, "DTYP"), "Async Soft Channel");
}

/// A downstream crate's row, which no vendored `.dbd` declares.
static ASYN_INT32: DevSup = DevSup {
    name: "devAiAsynInt32Probe",
    choice: "asynInt32",
    link_type: DbLinkType::Constant,
    dset: Dset::new(5, &[], false),
};

struct Probe;

impl DeviceSupport for Probe {
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "asynInt32"
    }
}

/// A device support registered at runtime by a downstream crate has no
/// `device()` line in any vendored `.dbd`. Whatever index it lands on, the
/// string must name the device the record is actually BOUND to — never a
/// different one, and never the menu head.
#[epics_macros_rs::epics_test]
async fn r21_an_undeclared_dtyp_still_renders_its_own_name() {
    let db = loaded(
        IocBuilder::new().register_device_support("ai", &ASYN_INT32, || Box::new(Probe)),
        r#"record(ai, "T") { field(DTYP, "asynInt32") }"#,
    )
    .await;
    let rec = db.get_record("T").unwrap();
    assert_eq!(dbr_string_of(&rec.read(), "DTYP"), "asynInt32");
}

/// A `dbPut` of a label goes through C's `putStringMenu` against the DEVICE
/// menu: an exact choice selects its index, and a name the menu does not have
/// fails the put (`S_db_badChoice`) rather than landing as index 0.
#[test]
fn r21_a_dtyp_put_resolves_against_the_device_menu() {
    let inst = instance("ai");
    let ok = coerce_put_value(
        inst.record.as_ref(),
        "DTYP",
        DbFieldType::Enum,
        EpicsValue::String(PvString::from("Raw Soft Channel")),
    )
    .expect("a declared device choice");
    assert_eq!(ok, Converted::Stored(EpicsValue::Enum(1)));

    let bad = coerce_put_value(
        inst.record.as_ref(),
        "DTYP",
        DbFieldType::Enum,
        EpicsValue::String(PvString::from("No Such Device")),
    );
    assert!(bad.is_err(), "an undeclared choice must fail the put");
}
