//! The `tests/*.rs` of `mca-rs` that need no process of their own, built as one
//! binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod a_background_write_moves_nord;
mod a_failed_siol_read_keeps_the_channels_before_it;
mod a_json_siml_does_not_seed_simm;
mod a_nomod_inp_put_detaches_the_dset;
mod a_refused_device_link_put_leaves_io_intr;
mod a_remote_siml_is_a_ca_link;
mod a_removed_siml_reads_nothing;
mod an_acquiring_pass_consumes_rpro;
mod buffer_width_is_the_capacity;
mod channel_capacity_is_nmax;
mod dbstatic_dumps_list_mca;
mod device_passes_on_both_port_kinds;
mod mca_posts_what_its_cycle_marked;
mod one_declaration_per_record_type;
mod rset_metadata_reaches_mca;
mod served_spectrum_length;
mod siml_is_read_at_dbr_enum;
mod siml_is_read_by_read_value_only;
mod simm_alarm_is_raised_after_the_siol_read;
mod sims_menu_carrier_is_honoured;
mod simulation_reads_siol;
mod sis38xx_snl_runs_the_c_states;
mod time_moves_only_at_its_stamp_points;
