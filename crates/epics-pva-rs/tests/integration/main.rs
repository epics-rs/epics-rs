//! The `tests/*.rs` of `epics-pva-rs` that need no process of their own, built
//! as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

#[path = "interop_helpers/pv_builders.rs"]
mod interop_helpers_pv_builders;

mod a_put_resolves_after_its_monitor_callback;
mod a_put_resolves_beside_its_own_monitor_future;
mod a_union_array_root_pv_keeps_its_variants_on_the_wire;
mod cli_missing_pv_name_exit;
mod cli_timeout_text;
mod cli_unknown_iface;
mod create_channel_backpressure;
mod get_exec_after_init_reply;
mod heartbeat_echo;
mod interop_pvxs;
mod json5_round_trip;
mod local_cp_link_on_pva_server_builder;
mod monitor_conn_events;
mod monitor_decode_fault_resets_circuit;
mod monitor_finish_body;
mod multi_pv_streaming;
mod put_bench;
mod put_two_phase;
mod pva_server_builder_resolves_a_pva_link;
mod pvxs_registrar_binds_before_the_script;
mod qsrv2_decision;
mod report_info;
mod rpc_empty_reply;
mod rpc_pvrequest;
mod rtems_exec_model_gate;
mod run_pva_ioc_env_ports;
mod server_info;
mod server_lifecycle;
mod service_framework;
mod set_byte_order_monitor_relatch;
mod set_byte_order_renegotiate;
mod stability;
mod subscription_callbacks_run_in_wire_order;
mod testudpfwd;
mod tls;
mod typed_nt;
mod ui25_same_process_discovery;
mod wire_golden_complex_types;
mod wire_golden_decode_roundtrip;
mod wire_golden_pvxs;
