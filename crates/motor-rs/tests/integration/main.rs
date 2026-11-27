//! The `tests/*.rs` of `motor-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

mod common;
mod passes;

mod a_refused_move_completes_past_a_full_band;
mod axis_from_out_link;
mod axis_write_status;
mod boot_moving;
mod callback_pass_after_put;
mod card_is_derived_from_the_out_link;
mod controller_lock;
mod controller_poller;
mod controller_port;
mod database_gate;
mod db_files;
mod dbstatic_dumps_list_motor;
mod declared_variant_float_unsigned;
mod dmov_monitor_after_unwatched_move;
mod dol_at_init;
mod done_pass_forwards_the_putf;
mod empty_dtyp_out_put_keeps_the_first_device;
mod empty_dtyp_out_put_rebinds_an_extending_first_device;
mod frac_reads_as_declared_float;
mod init_reads_after_the_startup_pass;
mod init_writes;
mod integration;
mod joined_axis_is_polled_before_init;
mod live_fields_answer_from_the_store;
mod model1_cdir;
mod model1_init_status;
mod model1_ntm;
mod model1_record_fields;
mod model1_routing;
mod model1_speed_writes;
mod model1_stop_sync;
mod model1_strings;
mod motor_metadata_window_is_the_soft_travel_limits;
mod motor_monitor_posts_match_oracle;
mod motor_util;
mod motor_util_init;
mod move_before_scan_run;
mod move_to_home;
mod nomod_fields_refused_by_the_gate;
mod non_finite_load_at_init;
mod null_move_completes_on_its_put_pass;
mod one_declaration_per_record_type;
mod poll_params;
mod poll_period;
mod port_sets;
mod rbv_deadband_at_move_start;
mod rlnk_status_is_the_process_status;
mod sim_motor_create;
mod stored_accel;
mod target_without_steps;
mod trace_lines;
mod write_posts;
