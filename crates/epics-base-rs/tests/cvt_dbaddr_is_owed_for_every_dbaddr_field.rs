//! C's `dbNameToAddr` runs the record's `cvt_dbaddr` for whichever
//! `SPC_DBADDR` field it resolves, however many the type declares (sscan
//! declares 152). A lock-free resolve that finds the record busy owes that
//! `cvt_dbaddr` to the record's next hold, and must owe it for the field it
//! resolved, past the 64th as before it.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, mpsc};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

const N: usize = 70;

static NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    (0..N)
        .map(|i| &*Box::leak(format!("F{i:02}").into_boxed_str()))
        .collect()
});

static FIELDS: LazyLock<Vec<FieldDesc>> = LazyLock::new(|| {
    NAMES
        .iter()
        .map(|n| FieldDesc::new(n, DbFieldType::Long, false))
        .collect()
});

/// The fields `cvt_dbaddr` ran for.
static CVT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A type with more `SPC_DBADDR` fields than a word has bits.
#[derive(Default)]
struct Wide;

impl Record for Wide {
    fn record_type(&self) -> &'static str {
        "wide"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        NAMES.contains(&name).then_some(EpicsValue::Long(0))
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        Err(CaError::FieldNotFound(name.into()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &FIELDS
    }
    fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
        Some(&NAMES)
    }
    fn cvt_dbaddr(&self, field: &str) {
        CVT.lock().unwrap().push(field.to_string());
    }
}

#[epics_macros_rs::epics_test]
async fn a_resolve_runs_or_owes_the_cvt_dbaddr_of_the_field_it_resolved() {
    let (db, _) = IocBuilder::new()
        .register_record_type("wide", || Box::new(Wide))
        .db_string("record(wide, \"W\") {}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let h = db.async_handle();
    CVT.lock().unwrap().clear();

    // Free: run now.
    assert_eq!(h.name_to_addr("W.F69"), Some(1));
    assert_eq!(*CVT.lock().unwrap(), ["F69"]);

    // Busy: owed to the next hold.
    CVT.lock().unwrap().clear();
    let (held_tx, held_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let holder = {
        let h = h.clone();
        std::thread::spawn(move || {
            h.with_record("W", |_| {
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            })
        })
    };
    held_rx.recv().unwrap();
    assert_eq!(h.name_to_addr("W.F66"), Some(1));
    assert_eq!(h.name_to_addr("W.F03"), Some(1));
    go_tx.send(()).unwrap();
    holder.join().unwrap();
    h.with_record("W", |_| {});
    let mut ran = CVT.lock().unwrap().clone();
    ran.sort();
    assert_eq!(ran, ["F03", "F66"]);
}
