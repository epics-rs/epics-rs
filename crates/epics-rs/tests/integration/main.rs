//! The `tests/*.rs` of `epics-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod every_declared_field_loads;
mod motor_alarm_severity_fields_store;
