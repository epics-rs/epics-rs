//! asyn-rs's half of the record-processing runtime-seam census.
//!
//! The rule and its message live in `record-seam-gate`; this file supplies the
//! root. `asynRecord`'s `special()` and `process_cycle()` are framework
//! callbacks: they run on whichever thread is processing the record, including
//! a blocking CA/PVA connection thread and a callback-pool worker, neither of
//! which has a tokio runtime.
//!
//! Only the record layer is in scope, and that is expressed as the root rather
//! than as a list of files: the port actors, the axis runtime and the
//! adapter's receiver tasks live outside `src/asyn_record`, are started from
//! the IOC's own runtime, and are meant to stay on it. A new file added to the
//! record layer is censused the moment it exists; a hand-written list would
//! have waited for someone to remember it.
#[test]
fn a_record_support_tail_never_uses_the_ambient_seam() {
    record_seam_gate::assert_no_ambient_seam_in_tree(
        env!("CARGO_MANIFEST_DIR"),
        &["src/asyn_record"],
        &[],
    );
}
