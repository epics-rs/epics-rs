//! The `tests/*.rs` of `scaler-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod autocount_freq_post;
mod autocount_hold_ends_on_any_pass;
mod cnt_abort_cancels_delayed_start;
mod cnt_done_alarm;
mod cnt_link_put_inherits_after_coutp;
mod completion_cycle_log_post;
mod coutp_double_fire;
mod coutp_fires_inside_the_put;
mod dbstatic_dumps_list_scaler;
mod delayed_callbacks_are_c_callbacks;
mod display_refresh_is_not_a_process;
mod done_param_boot_replay;
mod done_pass_forwards_the_putf;
mod dset_waits_under_the_hold;
mod integration_tests;
mod nomod_fields_refused_by_the_gate;
mod nomod_out_put_keeps_the_link;
mod one_declaration_per_record_type;
mod out_put_is_refused_without_dsxt;
mod process_post_set;
mod rset_supplies_only_precision;
mod scaler_tests;
mod scan_order_places_scaler_by_name;
mod soft_port_connect;
mod soft_port_inputs;
mod special_posts_on_a_link_put;
mod special_scan_once;
mod special_side_effect_posts;
