//! This crate's half of the RTEMS exec-model test-gating census.
//!
//! The rule, its message and its own boundary tests live in the `rtems-exec-gate`
//! dev-only crate; only the crate root and the file-count floor are local. See
//! that crate's module docs for what the four accounting options are and why the
//! census fails closed.
//!
//! Gated to the backend it describes, so the census is checked in exactly the
//! configuration whose reactor it is reasoning about — and so a site vouched for
//! by a bumped `RTEMS-EXEC-MODEL-ALLOW(N)` count still has to pass here.
#![cfg(exec_backend)]

#[test]
fn every_reactor_dependent_test_is_accounted_for() {
    rtems_exec_gate::assert_crate_is_accounted_for(env!("CARGO_MANIFEST_DIR"));
}
