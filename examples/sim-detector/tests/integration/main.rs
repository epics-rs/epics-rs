//! The `tests/*.rs` of `sim-detector` that need no process of their own, built
//! as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod acquire_readback;
mod attributes_file_over_ca;
mod integration;
