//! `mbbiDirectRecord.c:115-124` derives B0-B1F from VAL after the dset
//! `init_record`, and only when it returned 0; `mbboDirectRecord.c:139-140`
//! derives them after the constant DOL load. softIoc reads the bits below.
//! A runtime Bx put folds into VAL from mbboDirect's after-put special.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;

struct Probe(Option<DeviceInitOutcome>);

impl DeviceSupport for Probe {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        self.0
            .ok_or_else(|| CaError::InvalidValue("probe init fails".into()))
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "probe"
    }
}

async fn ioc(db: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_dynamic_device_support(|ctx: &DeviceSupportContext| {
            let outcome = match ctx.dtyp {
                "probeLive" => Some(DeviceInitOutcome::Live),
                "probeDead" => Some(DeviceInitOutcome::dead()),
                "probeErr" => None,
                _ => return None,
            };
            Some(Box::new(Probe(outcome)) as Box<dyn DeviceSupport>)
        })
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// B0..B3 as softIoc's `caget -t` prints them.
fn bits(db: &PvDatabase, name: &str) -> [i64; 4] {
    let rec = db.get_record(name).unwrap();
    let rec = rec.read();
    ["B0", "B1", "B2", "B3"].map(|b| {
        rec.record
            .get_field(b)
            .and_then(|v| v.to_f64())
            .unwrap_or_else(|| panic!("{name}.{b}")) as i64
    })
}

#[epics_macros_rs::epics_test]
async fn bits_are_derived_from_the_loaded_val() {
    let db = ioc(r#"record(mbbiDirect, "SOFT") { field(DTYP, "Soft Channel") field(INP, "5") }
record(mbbiDirect, "SOFTV") { field(DTYP, "Soft Channel") field(INP, "5") field(VAL, "2") }
record(mbbiDirect, "RAW") { field(DTYP, "Raw Soft Channel") field(INP, "5") field(VAL, "3") }
record(mbbiDirect, "PLAIN") { field(VAL, "6") }
record(mbboDirect, "OSOFT") { field(DTYP, "Soft Channel") field(DOL, "5") field(OMSL, "closed_loop") }
record(mbboDirect, "OVAL") { field(VAL, "6") }
record(mbboDirect, "ORAW") { field(DTYP, "Raw Soft Channel") field(VAL, "3") }"#)
    .await;
    for (name, want) in [
        ("SOFT", [1, 0, 1, 0]),
        ("SOFTV", [1, 0, 1, 0]),
        ("RAW", [1, 1, 0, 0]),
        ("PLAIN", [0, 1, 1, 0]),
        ("OSOFT", [1, 0, 1, 0]),
        ("OVAL", [0, 1, 1, 0]),
        ("ORAW", [1, 1, 0, 0]),
    ] {
        assert_eq!(bits(&db, name), want, "{name}");
    }
}

/// A `.db` load stores VAL and B0-B1F with no special: VAL keeps the file's
/// value whatever order the fields come in, and the init derives the bits
/// from it. softIoc reads VAL 6, B0-B3 0 1 1 0 for both orders.
#[epics_macros_rs::epics_test]
async fn mbbo_direct_db_bits_do_not_move_a_db_val() {
    let db = ioc(
        r#"record(mbboDirect, "VB") { field(VAL, "6") field(B0, "1") }
record(mbboDirect, "BV") { field(B0, "1") field(VAL, "6") }
record(mbboDirect, "BONLY") { field(B0, "1") field(B3, "1") }"#,
    )
    .await;
    for (name, val, want) in [
        ("VB", 6, [0, 1, 1, 0]),
        ("BV", 6, [0, 1, 1, 0]),
        ("BONLY", 9, [1, 0, 0, 1]),
    ] {
        let got = db.get_record(name).unwrap().read().record.get_field("VAL");
        assert_eq!(got.and_then(|v| v.to_f64()), Some(val as f64), "{name}.VAL");
        assert_eq!(bits(&db, name), want, "{name}");
    }
}

#[epics_macros_rs::epics_test]
async fn mbbi_direct_derives_no_bits_after_a_failed_dset_init() {
    let db = ioc(
        r#"record(mbbiDirect, "L") { field(DTYP, "probeLive") field(VAL, "5") }
record(mbbiDirect, "D") { field(DTYP, "probeDead") field(VAL, "5") }
record(mbbiDirect, "E") { field(DTYP, "probeErr") field(VAL, "5") }"#,
    )
    .await;
    assert_eq!(bits(&db, "L"), [1, 0, 1, 0]);
    assert_eq!(bits(&db, "D"), [0; 4]);
    assert_eq!(bits(&db, "E"), [0; 4]);
}
