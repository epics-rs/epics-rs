//! The `tests/*.rs` of `sscan-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod rec_dyn_link;
mod save_data_init_prints_follow_the_redirect;
mod save_data_vars;
mod scan_progress;
mod scanparm;
mod scanparm_debug;
mod sscan_connect_wait;
mod sscan_link_event_past_a_full_band;
mod sscan_record;
