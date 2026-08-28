//! The record types `epics-base-rs` implements but does **not** register by
//! default, in the shape an application registers them.
//!
//! `dbd/stdRecords.dbd` — vendored byte-identical from C's `dbd/stdRecords.dbd`
//! — is the manifest of the record types an EPICS Base IOC links. Seven of the
//! types this crate vendors a `.dbd` for and implements are not in it:
//! `aCalcout`, `sCalcout`, `sseq`, `swait` and `transform` belong to synApps
//! `calc`, `asyn` to asyn, `busy` to busy. Base's default registry no longer
//! claims them, so a test that loads one is an *application* that opted in, and
//! it says so with `register_record_type` exactly as a real one would.
//!
//! This module is the shared half of that opt-in: the three tests that walk the
//! whole vendored `.dbd` set (`one_declaration_per_record_type`,
//! `spc_nomod_declaration`, `seed_deadband_tracking_matches_c_init_record`) need
//! every type constructible, and hand-repeating seven factories in each of them
//! is how the set drifts. Per-record-type tests register only what they load.
#![allow(dead_code)]

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::RecordFactory;
use epics_base_rs::server::db_loader::create_record_with_factories;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::{
    acalcout::AcalcoutRecord, asyn_record::AsynRecord, busy::BusyRecord, scalcout::ScalcoutRecord,
    sseq::SseqRecord, swait::SwaitRecord, transform::TransformRecord,
};

/// Every module-owned record type, with the factory an application hands to
/// `register_record_type`.
pub fn factories() -> HashMap<String, RecordFactory> {
    fn f<R: Record + Default + 'static>() -> RecordFactory {
        Box::new(|| Box::new(R::default()))
    }
    HashMap::from([
        ("acalcout".to_string(), f::<AcalcoutRecord>()),
        ("asyn".to_string(), f::<AsynRecord>()),
        ("busy".to_string(), f::<BusyRecord>()),
        ("scalcout".to_string(), f::<ScalcoutRecord>()),
        ("sseq".to_string(), f::<SseqRecord>()),
        ("swait".to_string(), f::<SwaitRecord>()),
        ("transform".to_string(), f::<TransformRecord>()),
    ])
}

/// `create_record`, as seen by an application that registered all of the above.
/// Goes through `create_record_with_factories`, so the `.dbd` initials are
/// applied exactly as on the default path — and without touching the global
/// registry, which is shared by every test in a binary.
pub fn create_any(record_type: &str) -> CaResult<Box<dyn Record>> {
    create_record_with_factories(record_type, &factories())
}
