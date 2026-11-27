//! The `tests/*.rs` of `target-cfg-gate` that need no process of their own,
//! built as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod target_cfg_gate;
