//! The `tests/*.rs` of `std-rs` that need no process of their own, built as one
//! binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

mod common;

mod dbstatic_dumps_match_oracle;
mod delay_do_settings_req_resolves;
mod dtyp_menu_follows_device_registration_order;
mod e2e_tests;
mod epid_dtyp_put_keeps_the_init_dset;
mod epid_empty_inp_is_constant;
mod epid_link_failure_raises_link_alarm;
mod epid_monitor_posts_against_its_own_copies;
mod epid_rset_metadata;
mod epid_secondary_monitor_mask;
mod epid_tests;
mod integration_tests;
mod nomod_fields_refused_by_the_gate;
mod one_declaration_per_record_type;
mod record_seam_gate;
mod snl_runners_drive_the_db;
mod throttle_ca_link_to_a_local_name_is_local;
mod throttle_checks_its_out_link_on_accepted_cycles;
mod throttle_link_put_leaves_the_other_status;
mod throttle_never_posts_its_old_copies;
mod throttle_out_refuses_a_hardware_link;
mod throttle_special_posts_like_the_oracle;
mod throttle_sync_checks_an_unconnected_sinp;
mod throttle_sync_posts_value_alone;
mod throttle_sync_right_after_a_sinp_put;
mod throttle_tests;
mod throttle_value_put_posts;
mod time_of_day_device;
mod time_of_day_dtyp_menu;
mod timestamp_cycle_posts_val_and_rval_alone;
mod timestamp_monitor_gate;
mod timestamp_tests;
mod tod_linconv_moves_eoff_to_egul;
