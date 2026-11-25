//! The `tests/*.rs` of `epics-base-rs` that count allocations, built as one
//! binary around the counting allocator they share: test `t` of the former
//! binary `X` is `X::t` here, and `binary(alloc) & test(/^X::/)` selects what
//! `X` ran.

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;

mod a_lend_allocates_nothing;
mod a_monitored_post_fills_a_fixed_log;
mod a_post_burst_allocates_nothing;
mod acalcout_unwatched_process_post_allocates_nothing;
mod an_external_link_shape_allocates_nothing;
mod dol_siol_reads_allocate_as_requested;
mod enum_label_link_read_allocates_one_label;
mod errlog_under_a_hold_allocates_nothing;
mod external_forward_dispatch_alloc;
mod link_cache_hold_alloc;
mod ls_length_change_allocates_nothing;
mod ls_link_put_allocates_nothing;
mod mss_link_alarm_cycle_allocates_nothing;
mod periodic_tick_allocates_nothing;
mod process_callback_reentry_allocates_nothing;
mod put_row_does_not_allocate;
mod retiring_swap_prime;
mod scan_cycle_link_reads_allocate_nothing;
mod scan_overrun_report_allocates_nothing;
mod soft_out_write_allocates_nothing;
mod unconnected_external_link_cycle_allocates_nothing;
