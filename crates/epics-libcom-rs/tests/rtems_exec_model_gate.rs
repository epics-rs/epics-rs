//! This crate's half of the `rtems-exec-model` test-gating census.
//!
//! The rule, its message and its own boundary tests live in the `rtems-exec-gate`
//! dev-only crate; only the crate root and the file-count floor are local. See
//! that crate's module docs for what the four accounting options are and why the
//! census fails closed.
//!
//! This crate declares the feature, so it has the same hole `epics-base-rs`,
//! `epics-ca-rs` and `epics-pva-rs` have: the seam it gates *is* this crate's
//! `runtime::task`, and its `RTEMS-EXEC-MODEL-ALLOW(N)` markers came across
//! with the files. Without this file those markers would be unchecked on the
//! side that owns them.
//!
//! Gated to the feature it describes, so the census is checked in exactly the
//! configuration whose reactor it is reasoning about — and so a site vouched for
//! by a bumped `RTEMS-EXEC-MODEL-ALLOW(N)` count still has to pass here.
#![cfg(feature = "rtems-exec-model")]

/// A floor, not the current count: it exists to turn a scan that silently finds
/// nothing — a moved directory, a renamed manifest — into a failure. 33 files
/// at the time of writing.
const MIN_FILES: usize = 28;

#[test]
fn every_reactor_dependent_test_is_accounted_for() {
    rtems_exec_gate::assert_crate_is_accounted_for(env!("CARGO_MANIFEST_DIR"), MIN_FILES);
}
