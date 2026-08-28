//! Which FIELD carries the enum-table re-propagation event, and with which
//! mask.
//!
//! C asyn posts it on VAL, alone, with a literal `DBE_PROPERTY`. All four
//! runtime enum callbacks in `devAsynInt32.c` (asyn e2a281e2) and all four in
//! `devAsynUInt32Digital.c` are the same three statements:
//!
//! ```c
//! dbScanLock((dbCommon*)pr);
//! setEnums((char*)&pr->zrst, (int*)&pr->zrvl, &pr->zrsv,
//!          strings, values, severities, nElements, MAX_ENUM_STATES);
//! db_post_events(pr, &pr->val, DBE_PROPERTY);
//! dbScanUnlock((dbCommon*)pr);
//! ```
//!
//! `interruptCallbackEnumMbbi` `devAsynInt32.c:712-724` (post at :722),
//! `EnumMbbo` :726-738 (:736), `EnumBi` :740-752 (:750), `EnumBo` :754-766
//! (:764); `devAsynUInt32Digital.c:547-559` (:557), :561-573 (:571),
//! :575-587 (:585), :589-601 (:599).
//!
//! `setEnums` rewrites ZRST/ZRVL/ZRSV… in place and posts NOTHING on them —
//! the one `db_post_events` names `&pr->val`. So a CA client monitoring the
//! PV itself (which is VAL) with DBE_PROPERTY is the one that learns the
//! table moved, and a DBE_VALUE monitor on VAL learns nothing, because no
//! value changed and the record is never processed.
//!
//! Asserting that the port's `property_post_receiver` channel carries the
//! right ZRST/ONST strings — which is all the existing unit test does —
//! cannot see either half of that.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::asyn_record::register_port;
use asyn_rs::interrupt::InterruptValue;
use asyn_rs::param::{EnumEntry, ParamType, ParamValue};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::trace::TraceManager;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Every mask bit, so an assertion reads the mask the IOC chose and not the
/// one the subscription filtered down to.
const ALL: u16 = 0x0F;
/// DBE_VALUE | DBE_LOG | DBE_ALARM — a classic monitor, no DBE_PROPERTY.
const VALUE_CLASS: u16 = 0x07;
const PROPERTY: u16 = 0x08;

const REC: &str = "ENUM:PROP";

struct EnumPort {
    base: PortDriverBase,
}

impl EnumPort {
    fn new(name: &str, choices: Vec<EnumEntry>) -> Self {
        let mut base = PortDriverBase::new(name, 1, PortFlags::default());
        base.create_param("MODE", ParamType::Enum).unwrap();
        base.set_enum_choices_param(0, 0, choices.into()).unwrap();
        Self { base }
    }
}

impl PortDriver for EnumPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

fn entry(s: &str, value: i32, severity: u16) -> EnumEntry {
    EnumEntry {
        string: s.to_string(),
        value,
        severity,
    }
}

/// Build a one-record IOC whose `record_type` binds `dtyp` to a driver enum
/// param, and hand back the port handle so the test can fire the driver-side
/// table change that C's `doCallbacksEnum` fires.
async fn enum_ioc(
    port: &str,
    record_type: &str,
    dtyp: &str,
    choices: Vec<EnumEntry>,
) -> (Arc<PvDatabase>, PortHandle) {
    let (runtime, _join) =
        create_port_runtime(EnumPort::new(port, choices), RuntimeConfig::default())
            .expect("port runtime starts");
    let handle = runtime.port_handle().clone();
    register_port(port, handle.clone(), Arc::new(TraceManager::new())).expect("port name is free");
    drop(runtime);

    let db_text = format!(
        r#"record({record_type}, "{REC}") {{
            field(DTYP, "{dtyp}")
            field(INP,  "@asyn({port},0)MODE")
            field(SCAN, "Passive")
        }}"#
    );
    let (database, _) =
        asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
            .db_string(&db_text, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
    (database, handle)
}

fn subscribe(db: &PvDatabase, field: &str, sid: u32, mask: u16) -> EventReader {
    let rec = db.get_record(REC).unwrap();
    let mut inst = rec.write();
    inst.add_subscriber(field, sid, DbFieldType::String, mask)
        .expect("field is subscribable")
}

/// The driver-side `doCallbacksEnum`: a new table on the same reason.
fn drive_table_change(handle: &PortHandle, choices: Vec<EnumEntry>) {
    handle.interrupts().notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::Enum {
            index: 0,
            choices: choices.into(),
        },
        ..Default::default()
    });
}

/// Drain whatever a reader collected, as (mask, value) pairs.
fn drain(reader: &mut EventReader) -> Vec<(EventMask, EpicsValue)> {
    let mut out = Vec::new();
    while let Ok(ev) = reader.try_recv() {
        out.push((ev.mask, ev.snapshot.value.clone()));
    }
    out
}

async fn settle() {
    for _ in 0..40 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// mbbi through `asynInt32`: C `interruptCallbackEnumMbbi`
/// (`devAsynInt32.c:712-724`) posts `&pr->val` with `DBE_PROPERTY` and posts
/// nothing on the sixteen state fields `setEnums` just rewrote. Both halves,
/// because the port used to do the exact opposite of each.
#[epics_base_rs::epics_test]
async fn an_enum_table_change_posts_dbe_property_on_val_and_on_no_state_field() {
    let (db, handle) = enum_ioc(
        "enumMbbiVal",
        "mbbi",
        "asynInt32",
        vec![entry("OFF", 0, 0), entry("ON", 1, 0)],
    )
    .await;
    let mut val = subscribe(&db, "VAL", 1, ALL);
    let mut zrst = subscribe(&db, "ZRST", 2, ALL);
    let mut onst = subscribe(&db, "ONST", 3, ALL);

    drive_table_change(
        &handle,
        vec![
            entry("CLOSED", 0, 0),
            entry("OPEN", 1, 2),
            entry("FAULT", 7, 2),
        ],
    );
    settle().await;

    let on_val = drain(&mut val);
    assert_eq!(
        on_val.len(),
        1,
        "exactly one event on VAL, C's single db_post_events: {on_val:?}"
    );
    assert_eq!(
        on_val[0].0,
        EventMask::PROPERTY,
        "the literal third argument is DBE_PROPERTY, nothing else"
    );
    assert!(
        drain(&mut zrst).is_empty(),
        "setEnums rewrites ZRST and posts on it nowhere"
    );
    assert!(drain(&mut onst).is_empty(), "nor on ONST");

    // The rewrite itself still happened — the event is worth nothing if the
    // strings a client then re-reads are stale.
    let rec = db.get_record(REC).unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.record.get_field("ZRST"),
        Some(EpicsValue::String("CLOSED".into()))
    );
    assert_eq!(
        inst.record.get_field("TWST"),
        Some(EpicsValue::String("FAULT".into()))
    );
}

/// The mask, not merely the field. A classic `DBE_VALUE|DBE_LOG|DBE_ALARM`
/// monitor on the PV must see nothing: no value changed and C never processes
/// the record on an enum callback (`dbScanLock`, `setEnums`, `db_post_events`,
/// `dbScanUnlock` — no `dbProcess`).
#[epics_base_rs::epics_test]
async fn a_value_class_monitor_on_val_sees_no_enum_table_change() {
    let (db, handle) = enum_ioc(
        "enumMbbiValueClass",
        "mbbi",
        "asynInt32",
        vec![entry("OFF", 0, 0), entry("ON", 1, 0)],
    )
    .await;
    let mut classic = subscribe(&db, "VAL", 1, VALUE_CLASS);
    let mut property = subscribe(&db, "VAL", 2, PROPERTY);

    drive_table_change(&handle, vec![entry("LOW", 0, 0), entry("HIGH", 1, 1)]);
    settle().await;

    assert!(
        drain(&mut classic).is_empty(),
        "a table re-key is not a new reading"
    );
    assert_eq!(drain(&mut property).len(), 1, "the property monitor is");
}

/// bi through `asynUInt32Digital`: C `interruptCallbackEnumBi`
/// (`devAsynUInt32Digital.c:575-587`) is the same three statements over the
/// two-state family (`&pr->znam`, `NULL` values, `&pr->zsv`, MAX 2) and posts
/// `&pr->val` just the same. The port must not special-case the shape.
#[epics_base_rs::epics_test]
async fn the_two_state_family_posts_on_val_as_well() {
    let (db, handle) = enum_ioc(
        "enumBiVal",
        "bi",
        "asynUInt32Digital",
        vec![entry("OFF", 0, 0), entry("ON", 1, 0)],
    )
    .await;
    let mut val = subscribe(&db, "VAL", 1, ALL);
    let mut znam = subscribe(&db, "ZNAM", 2, ALL);

    drive_table_change(&handle, vec![entry("SHUT", 0, 0), entry("OPEN", 1, 2)]);
    settle().await;

    let on_val = drain(&mut val);
    assert_eq!(on_val.len(), 1, "one event on VAL: {on_val:?}");
    assert_eq!(on_val[0].0, EventMask::PROPERTY);
    assert!(drain(&mut znam).is_empty(), "ZNAM is written, not posted");

    let rec = db.get_record(REC).unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.record.get_field("ZNAM"),
        Some(EpicsValue::String("SHUT".into()))
    );
    assert_eq!(
        inst.record.get_field("ONAM"),
        Some(EpicsValue::String("OPEN".into()))
    );
    assert_eq!(inst.record.get_field("OSV"), Some(EpicsValue::Short(2)));
}

/// An interrupt carrying the same table is the driver's value callback, not
/// `doCallbacksEnum`; C's asynEnum callback never runs for it, so no
/// DBE_PROPERTY reaches VAL either.
#[epics_base_rs::epics_test]
async fn an_unchanged_table_posts_nothing_on_val() {
    let choices = vec![entry("OFF", 0, 0), entry("ON", 1, 0)];
    let (db, handle) = enum_ioc("enumMbbiSameTable", "mbbi", "asynInt32", choices.clone()).await;
    let mut val = subscribe(&db, "VAL", 1, ALL);

    drive_table_change(&handle, choices);
    settle().await;

    assert!(
        drain(&mut val).is_empty(),
        "a value-only callback is not a property change"
    );
}
