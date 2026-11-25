//! The `tests/*.rs` of `ad-plugins-rs` that need no process of their own, built
//! as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod an_ndpvxs_pv_is_listed_under_builtin;
mod array_posts_at_the_edge;
mod circular_buff_configure_max_buffers;
mod circular_buff_control_gate;
mod color_convert_mode_default;
mod configure_duplicate_port;
mod integration;
mod ioc_asyn_commands;
mod ioc_env_paths;
mod plugin_configure_blocking_callbacks;
mod plugin_configure_max_threads;
mod plugin_configure_queue_size;
mod plugin_configure_source;
mod plugin_configure_stack_size;
mod plugin_configure_unknown_array_port;
mod plugin_interrupt_sources;
mod pool_throughput;
mod reference_plugin_asyn_flags;
mod reference_plugin_configure_arg_count;
mod std_arrays_configure_queue_size;
mod time_series_array_callbacks;
mod time_series_pool_buffer;
