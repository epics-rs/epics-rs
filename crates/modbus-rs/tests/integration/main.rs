//! The `tests/*.rs` of `epics-modbus-rs` that need no process of their own,
//! built as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod mbap_residue_dies_with_its_transaction;
mod reference_modbus_db;
mod tcp_loopback;
