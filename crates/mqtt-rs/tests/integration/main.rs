//! The `tests/*.rs` of `mqtt-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod configure_prints_nothing;
mod ioc_exit_sends_disconnect;
