//! The `tests/*.rs` of `rtems-exec-gate` that need no process of their own,
//! built as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod every_backend_deriving_crate_is_accounted_for;
mod mod_gate_resolution;
mod pin_maintenance;
