//! The `tests/*.rs` of `qsrv-ioc` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod pva_port_env;
mod simulator_can_be_off;
mod st_cmd_goes_on_past_a_failed_load;
