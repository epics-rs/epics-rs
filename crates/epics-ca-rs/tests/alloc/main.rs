//! The `tests/*.rs` of `epics-ca-rs` that count allocations, built as one
//! binary around the counting allocator they share: test `t` of the former
//! binary `X` is `X::t` here, and `binary(alloc) & test(/^X::/)` selects what
//! `X` ran.

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;

mod a_network_ca_inp_count_allocates_nothing;
mod scan_thread_monitor_post_allocates_nothing;
