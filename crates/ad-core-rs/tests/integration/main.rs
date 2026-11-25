//! The `tests/*.rs` of `ad-core-rs` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod capture_flush_frees_the_buffer;
mod codec_compressor_states_front_every_codec;
mod common_plugins_c_plugin_set;
mod common_plugins_config_menu;
mod common_plugins_reads_only_c_macros;
mod common_plugins_request_paths;
mod epics_pv_attribute_native_types;
mod epics_pv_attributes_are_fed;
mod nd_codec_settings_saves_c_set;
mod nd_stats_settings_saves_reset_sseqs;
mod ndarray_tests;
mod reference_nd_array_base_template;
