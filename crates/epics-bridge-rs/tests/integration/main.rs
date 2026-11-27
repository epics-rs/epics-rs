//! The `tests/*.rs` of `epics-bridge-rs` that need no process of their own,
//! built as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

#[cfg(all(feature = "pvalink", feature = "qsrv", tokio_backend, unix))]
mod common;
#[cfg(all(feature = "qsrv-bin", unix))]
mod qsrv_named_ports;

mod a_bare_ca_link_opens_no_pva_channel;
mod a_compress_pva_inp_of_an_empty_array_keeps_its_ring;
mod a_group_put_refuses_a_link_member;
mod a_group_put_to_an_active_record_prints_the_pvxs_line;
mod a_qsrv_put_reply_follows_its_update;
mod acf_access_control_contexts;
mod dbjlr_prints_the_pvxs_pva_report;
mod dbpvar_prints_the_pvxs_channel_report;
mod dual_ioc_rs_resolves_a_pva_link;
mod gw_upstream_unresponsive_window;
mod pva_gateway;
mod pva_link_empty_name_processes_invalid;
mod pva_link_parse_warnings;
mod pva_link_put_error_prints_the_pvxs_line;
mod pva_link_put_relinks_cp_holder;
mod pva_link_scan_needs_an_inlink;
mod pva_local_link_refuses_a_qsrv_group;
mod pva_local_link_unfulfilled;
mod pvalink_channel_closes_with_its_last_link;
mod pvalink_cp_scan_of_an_active_record_traces;
mod pvalink_get_failure_names_disconn;
mod pvalink_read_stamps_the_reader;
mod pvalink_reopened_cp_channel_scans;
mod pvalink_seam;
mod pvxsr_lists_the_builtin_source;
mod qsrv2_disabled_still_serves;
mod qsrv_app_declares_pva_link_type;
mod qsrv_app_group_file;
mod qsrv_dollar_string_ends_like_dbget;
mod qsrv_group_blocking_driver;
mod qsrv_put_fails_on_the_process_status;
mod qsrv_remote_log;
mod qsrv_rs_db_path_bytes;
mod qsrv_rs_group_file;
mod qsrv_rs_resolves_a_ca_link;
mod qsrv_rs_resolves_a_pva_link;
mod qsrv_rs_sources;
mod qsrv_ts_filter_probe;
mod realtime_pva_ioc_boots;
mod rtems_exec_model_gate;
mod testqgroup;
mod testqsingle;
mod tpro_names_the_pva_client;
