//! The `tests/*.rs` of `asyn-rs` that count allocations, built as one
//! binary around the counting allocator they share: test `t` of the former
//! binary `X` is `X::t` here, and `binary(alloc) & test(/^X::/)` selects what
//! `X` ran.

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;

mod accepting_reads_allocate_and_free_nothing;
#[cfg(feature = "epics")]
mod asyn_reentry_allocates_nothing;
mod enqueue_only_requests_allocate_nothing;
#[cfg(feature = "epics")]
mod enum_notify_allocates_nothing;
#[cfg(feature = "epics")]
mod notify_allocates_nothing;
mod port_thread_first_park_allocates_nothing;
