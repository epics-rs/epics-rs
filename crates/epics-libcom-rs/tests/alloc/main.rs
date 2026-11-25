//! The `tests/*.rs` of `epics-libcom-rs` that count allocations, built as one
//! binary around the counting allocator they share: test `t` of the former
//! binary `X` is `X::t` here, and `binary(alloc) & test(/^X::/)` selects what
//! `X` ran.

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;

mod callback_cell_request_allocates_nothing;
mod cfmt_formats_without_allocating;
mod errlog_flush_waits_on_a_thread_local_event;
mod errlog_post_allocates_nothing;
mod ioc_thread_first_drive_allocates_nothing;
mod ioc_thread_first_park_allocates_nothing;
mod ioc_thread_first_tokio_wait_allocates_nothing;
mod thread_loop_forced_yield_allocates_nothing;
