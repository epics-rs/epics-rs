//! This crate's half of the `rtems-exec-model` test-gating census.
//!
//! The rule, its message and its own boundary tests live in the `rtems-exec-gate`
//! dev-only crate; only the crate root and the file-count floor are local. See
//! that crate's module docs for what the four accounting options are and why the
//! census fails closed.
//!
//! Gated to the feature it describes, so the census is checked in exactly the
//! configuration whose reactor it is reasoning about — and so a site vouched for
//! by a bumped `RTEMS-EXEC-MODEL-ALLOW(N)` count still has to pass here.
#![cfg(feature = "rtems-exec-model")]

/// A floor, not the current count: it exists to turn a scan that silently finds
/// nothing — a moved directory, a renamed manifest — into a failure. 117 files
/// at the time of writing.
const MIN_FILES: usize = 100;

#[test]
fn every_reactor_dependent_test_is_accounted_for() {
    rtems_exec_gate::assert_crate_is_accounted_for(env!("CARGO_MANIFEST_DIR"), MIN_FILES);
}
