//! Regression R0604-BASEREC-BINARY-MONITOR-1.
//!
//! C binary-family records raise the VAL monitor `DBE_VALUE | DBE_LOG`
//! ONLY when the value actually changed (`if (mlst != val)`), then advance
//! `mlst`; an unchanged process cycle posts no VALUE/LOG event for VAL.
//! References (all `monitor()`):
//!   bi          biRecord.c:250-255
//!   bo          boRecord.c:394-399
//!   busy        busyRecord.c:365-369
//!   mbbi        mbbiRecord.c:355-358
//!   mbbo        mbboRecord.c:400-403
//!   mbbiDirect  mbbiDirectRecord.c:228-231
//!   mbboDirect  mbboDirectRecord.c:311-314
//!
//! Before the fix these records returned `uses_monitor_deadband()==false`
//! with no `monitor_value_changed()` override, so the processing gate took
//! the unconditional always-post `(true, true)` branch and re-posted
//! VALUE|LOG every process cycle even when VAL was unchanged. Each record
//! now captures `mlst != val` in `process()` and returns it from
//! `monitor_value_changed()`, so the framework posts VAL only on a real
//! change — matching the C gate.
//!
//! These mirror the lsi/lso `h11_*_monitor_gate_only_on_change` tests:
//! the per-record `monitor_value_changed()` IS the value the processing
//! framework consumes (processing.rs:1478 and :2310), so asserting it
//! directly validates the gate. Alarm-only posting is handled by the
//! framework gate (`include_val || val_on_alarm`), shared with lsi/lso and
//! unchanged by this fix.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::server::records::mbbi_direct::MbbiDirectRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::types::EpicsValue;

#[test]
fn bi_monitor_gate_only_on_change() {
    let mut rec = BiRecord::default();
    // RVAL drives VAL on an input record (rval != 0 -> VAL = 1).
    rec.put_field("RVAL", EpicsValue::Long(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 0 -> 1
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
    rec.put_field("RVAL", EpicsValue::Long(0)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 1 -> 0
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
}

#[test]
fn bo_monitor_gate_only_on_change() {
    let mut rec = BoRecord::default();
    rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 0 -> 1
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
    rec.put_field("VAL", EpicsValue::Enum(0)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 1 -> 0
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
}

#[test]
fn busy_monitor_gate_only_on_change() {
    let mut rec = BusyRecord::default();
    rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 0 -> 1
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
    rec.put_field("VAL", EpicsValue::Enum(0)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 1 -> 0
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
}

#[test]
fn mbbi_monitor_gate_only_on_change() {
    let mut rec = MbbiRecord::default();
    // No state table defined (sdef=false) -> raw_to_val is the identity,
    // so RVAL drives VAL directly.
    rec.put_field("RVAL", EpicsValue::Long(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 0 -> 1
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
    rec.put_field("RVAL", EpicsValue::Long(2)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 1 -> 2
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
}

#[test]
fn mbbo_monitor_gate_only_on_change() {
    let mut rec = MbboRecord::default();
    rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 0 -> 1
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
    rec.put_field("VAL", EpicsValue::Enum(2)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 1 -> 2
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
}

#[test]
fn mbbi_direct_monitor_gate_only_on_change() {
    let mut rec = MbbiDirectRecord::default();
    // MASK=0 (no mask) and SHFT=0 -> VAL = RVAL as u32.
    rec.put_field("RVAL", EpicsValue::Long(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 0 -> 1
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
    rec.put_field("RVAL", EpicsValue::Long(5)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 1 -> 5
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
}

#[test]
fn mbbo_direct_monitor_gate_only_on_change() {
    let mut rec = MbboDirectRecord::default();
    rec.put_field("VAL", EpicsValue::Long(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 0 -> 1
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
    rec.put_field("VAL", EpicsValue::Long(5)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(true)); // 1 -> 5
    rec.process().unwrap();
    assert_eq!(rec.monitor_value_changed(), Some(false)); // unchanged
}
