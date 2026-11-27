//! The `tests/*.rs` of `optics-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

mod hsc1_mock;
#[path = "hsc1_mock/rig.rs"]
mod hsc1_mock_rig;

mod a_removed_drive_link_writes_nothing;
mod dbaddr_fields_serve_their_arrays;
mod dbstatic_dumps_list_table;
mod declared_variant_float_unsigned;
mod filter_drive_runs_the_c_states;
mod flex_combined_motion_runs_the_c_states;
mod hr_reads_monitor_vars;
mod integration_tests;
mod io_runs_on_io_db;
mod kohzu_init_runs_the_theta_command_path;
mod kohzu_soft_init_runs_the_connect_time_passes;
mod ml_mono_reads_monitor_vars;
mod nomod_fields_refused_by_the_gate;
mod one_declaration_per_record_type;
mod orient_reads_monitor_vars;
mod pf4_runs_the_c_states;
mod sncqxbpm_runs_the_c_states;
mod table_init_counts_a_pva_limit_axis;
mod table_init_reads_its_motors;
mod table_init_reads_only_what_c_reads;
mod table_link_kind;
mod table_monitor_posts_from_its_saved_copy;
mod table_posts_only_what_monitor_posts;
mod table_pva_axis;
mod table_reads_its_links_as_double;
mod table_reads_speeds_only_to_move;
mod table_returns_a_failed_speed_restore;
mod table_set_posts_match_oracle;
mod table_stops_each_read_loop_at_a_failed_read;
mod xia_slit_runs_the_c_states;
mod xiahsc_runs_the_c_states;
