//! The `tests/*.rs` of `epics-bridge-rs` that count allocations, built as one
//! binary around the counting allocator they share: test `t` of the former
//! binary `X` is `X::t` here, and `binary(alloc) & test(/^X::/)` selects what
//! `X` ran.

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;

mod connected_pva_link_lset_calls_allocate_nothing;
mod pva_forward_link_dispatch_alloc;
mod pvalink_unconnected_cycle_allocates_nothing;
mod unconnected_pva_link_lset_calls_allocate_nothing;
