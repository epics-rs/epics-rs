//! The `tests/*.rs` of `ad-core-rs` that count allocations, built as one
//! binary around the counting allocator they share: test `t` of the former
//! binary `X` is `X::t` here, and `binary(alloc) & test(/^X::/)` selects what
//! `X` ran.

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;

mod driver_thread_burst_allocates_nothing;
mod file_plugin_path_allocates_nothing;
mod plugin_path_allocates_nothing;
