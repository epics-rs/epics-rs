//! Whole-database passes must visit records in **database load order**.
//!
//! C parity: `dbFirstRecord`/`dbNextRecord` walk each record type's list in the
//! order `dbReadDatabase` appended them, so `initDevSup`, `initDatabase` and
//! `initialProcess` (PINI) all follow the `.db` file order. Device support is
//! written against that contract — epics-modules/opcua's element records refuse
//! to init unless their `opcuaItem` record has already bound
//! (`linkParser.cpp:226-234`), which the shipped databases guarantee only by
//! declaring the item record first.
//!
//! The Rust database kept its records in a `HashMap` and `all_record_names()`
//! returned `keys()`, so every whole-database pass ran in hash order: not load
//! order, and not even stable across runs of the same binary (`RandomState`
//! reseeds per process). Booting the same database twice could wire records in
//! two different orders — one boot succeeding, the next failing with "opcuaItem
//! record ... is not loaded yet".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{PiniMode, Record};
use epics_base_rs::server::records::ai::AiRecord;

/// Enough records that hash order cannot coincide with load order.
const N: usize = 24;

struct NoopDevice;

impl DeviceSupport for NoopDevice {
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "seqDev"
    }
}

/// Names chosen so lexical order, hash order and load order are three different
/// sequences: the load order below is deliberately *not* sorted.
fn load_ordered_names() -> Vec<String> {
    // A fixed permutation of 0..N, so a passing run cannot be an accident of
    // the names happening to be sorted.
    (0..N)
        .map(|i| format!("LOAD:{:02}", (i * 7 + 3) % N))
        .collect()
}

/// The `IocBuilder` path binds device support while iterating the parsed record
/// defs, so it was already load-ordered; the nondeterministic path was
/// `IocApplication`'s `wire_device_support` (pinned by the unit test
/// `wire_device_support_binds_in_database_load_order`). Pinned here so the two
/// paths cannot silently diverge — an IOC must wire the same way whether its
/// database came from the builder or from iocsh `dbLoadRecords`.
#[epics_macros_rs::epics_test]
async fn builder_wires_device_support_in_database_load_order() {
    let names = load_ordered_names();

    let mut db = String::new();
    for name in &names {
        // The record's own name is echoed through INP because
        // `DeviceSupportContext` carries the links, not the record name — this
        // is how the test observes *which* record is being wired.
        db.push_str(&format!(
            "record(ai, \"{name}\") {{\n    field(DTYP, \"seqDev\")\n    field(INP, \"@{name}\")\n}}\n"
        ));
    }

    let wired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&wired);

    let (_database, _) = IocBuilder::new()
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| {
            captured
                .lock()
                .unwrap()
                .push(ctx.inp.trim_start_matches('@').to_string());
            Some(Box::new(NoopDevice) as Box<dyn DeviceSupport>)
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let wired = std::mem::take(&mut *wired.lock().unwrap());
    assert_eq!(
        wired, names,
        "device support must be wired in database load order (C initDevSup \
         walks dbFirstRecord/dbNextRecord); got hash order"
    );
}

/// The ordering owner itself: every whole-database walk goes through
/// `all_record_names`, so pinning it pins `wire_device_support`,
/// `setup_io_intr`, `setup_cp_links`, `dbl`, and the rest.
#[epics_macros_rs::epics_test]
async fn all_record_names_returns_load_order() {
    let db = Arc::new(PvDatabase::new());
    let names = load_ordered_names();
    for name in &names {
        db.add_record(name, Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
    }

    assert_eq!(
        db.all_record_names().await,
        names,
        "all_record_names must return database load order, not hash order"
    );
}

/// PINI processing is a whole-database walk too: C `initialProcess` iterates
/// with `dbFirstRecord`/`dbNextRecord`, so a PINI record that depends on an
/// earlier PINI record's output processes after it.
#[epics_macros_rs::epics_test]
async fn pini_records_returns_load_order() {
    let db = Arc::new(PvDatabase::new());
    let names = load_ordered_names();
    for name in &names {
        db.add_record(name, Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record(name).unwrap();
        // r6: `pini` is the `menuPini` index (i16), not a bool. YES = 1.
        rec.write().common.pini = PiniMode::Yes.to_u16() as i16;
    }

    assert_eq!(
        db.pini_records(PiniMode::Yes).await,
        names,
        "PINI records must process in database load order, not hash order"
    );
}
