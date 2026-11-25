//! The `tests/*.rs` of `asyn-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod a_failed_array_write_raises_read_alarm;
mod a_plain_put_sees_a_failed_asyn_write_only_on_a_sync_port;
mod a_refused_octet_or_array_queue_keeps_the_last_alarm;
mod a_refused_output_queue_raises_its_status_alarm;
#[cfg(feature = "epics")]
mod a_special_applies_past_a_full_band;
mod an_unbounded_connect_retry_keeps_the_port;
#[cfg(feature = "epics")]
mod asyn_errs_posts_ahead_of_the_alarm;
mod asyn_linconv_follows_egu_puts;
#[cfg(feature = "epics")]
mod asyn_monitor_posts_its_read_fields;
#[cfg(feature = "epics")]
mod asyn_portless_errs_posts_once;
mod asyn_record_refused_io_intr_is_passive;
#[cfg(feature = "epics")]
mod asyn_special_posts_in_c_order;
#[cfg(feature = "epics")]
mod asyn_specials_under_the_hold;
mod blocking_read_completes_on_the_callback_band;
#[cfg(feature = "epics")]
mod blocking_write_completes_on_the_callback_band;
mod boot_flush_fires_no_array;
mod boot_flush_is_its_owner_gates_job;
mod boot_sweep_runs_behind_the_queue;
mod cp_first_event_waits_for_the_ports_sweep;
mod declared_variant_float_unsigned;
mod devasyn_scan_move_reregisters;
#[cfg(feature = "epics")]
mod device_support_brings_the_shell_commands;
#[cfg(feature = "epics")]
mod enum_property_post_mask;
#[cfg(feature = "epics")]
mod enum_table_lands_under_the_record_lock;
mod errlog_trace_console_keeps_device_bytes;
mod errlog_trace_is_one_message_per_c_print;
#[cfg(feature = "epics")]
mod errs_put_is_refused_by_the_gate;
#[cfg(feature = "epics")]
mod half_built_port_stays_registered;
mod initial_readback_tag_is_octet_output_only;
mod int32_store_rule;
mod interpose_is_manager_level;
#[cfg(feature = "epics")]
mod interrupt_gate_follows_the_database;
mod io_intr_needs_an_interrupt_source;
mod io_intr_reads_when_its_ring_is_empty;
mod ioc_exit_stops_every_port_actor;
#[cfg(feature = "epics")]
mod iocsh_port_is_traceable;
mod ip_port_trace_lines;
#[cfg(feature = "epics")]
mod ip_server_port_accepts;
mod ip_server_port_trace_lines;
mod ip_snc_server;
#[cfg(feature = "epics")]
mod non_blocking_write_reaches_the_driver_once;
mod octet_read_fans_out_interrupts;
#[cfg(feature = "epics")]
mod octet_short_write_raises_write_minor;
#[cfg(feature = "epics")]
mod octet_write_defines_the_record;
#[cfg(feature = "epics")]
mod octet_write_read_reads_its_command_record;
#[cfg(feature = "epics")]
mod octet_write_read_takes_no_lock_on_its_command_record;
mod octet_write_rule;
mod port_connects_at_registration;
mod port_driver_tests;
#[cfg(feature = "epics")]
mod port_runtime_lifetime;
mod port_thread_stack;
mod prints_follow_the_redirect;
#[cfg(feature = "epics")]
mod process_gate_tests;
mod readback_defines_the_record_before_the_pass;
mod readback_during_a_queued_write_is_deferred;
mod readback_error_raises_write_alarm;
mod record_seam_gate;
mod reuse_port_before_bind;
#[cfg(feature = "epics")]
mod ring_size_0_copies_under_the_record_lock;
mod serial_port_trace_lines;
mod single_device_addr_connect_is_the_port;
mod single_device_port_ignores_the_address;
#[cfg(feature = "epics")]
mod spc_nomod_fields_refuse_every_runtime_write;
#[cfg(feature = "epics")]
mod special_puts_queue_behind_a_blocked_read;
mod submit_blocking_contexts;
mod sync_io_connect_reruns_drv_user_create;
#[cfg(feature = "epics")]
mod time_series_busy_post_mask;
#[cfg(feature = "epics")]
mod trace_prints_reach_the_trace_file;
mod two_trip_read_on_both_port_kinds;
mod udp_server_fanout_refusal_is_logged;
