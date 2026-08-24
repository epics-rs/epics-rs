//! asyn-rs's half of the record-processing runtime-seam census.
//!
//! The rule and its message live in `record-seam-gate`; this file supplies the
//! list. `asynRecord`'s `special()` and `process_cycle()` are framework
//! callbacks: they run on whichever thread is processing the record, including
//! a blocking CA/PVA connection thread and a callback-pool worker, neither of
//! which has a tokio runtime.
//!
//! Only the record layer is listed. The port actors, the axis runtime and the
//! adapter's receiver tasks are started from the IOC's own runtime and are
//! meant to stay on it.
const SOURCES: &[(&str, &str)] = &[
    (
        "asyn_record/mod.rs",
        include_str!("../src/asyn_record/mod.rs"),
    ),
    (
        "asyn_record/device.rs",
        include_str!("../src/asyn_record/device.rs"),
    ),
    (
        "asyn_record/io_intr.rs",
        include_str!("../src/asyn_record/io_intr.rs"),
    ),
];

#[test]
fn a_record_support_tail_never_uses_the_ambient_seam() {
    record_seam_gate::assert_no_ambient_seam(SOURCES);
}
