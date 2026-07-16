#![allow(
    clippy::approx_constant,
    clippy::collapsible_match,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::implicit_saturating_sub,
    clippy::io_other_error,
    clippy::items_after_test_module,
    clippy::manual_is_multiple_of,
    clippy::manual_range_contains,
    clippy::manual_strip,
    clippy::map_entry,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::redundant_closure,
    clippy::should_implement_trait,
    clippy::single_match,
    clippy::type_complexity,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::useless_conversion
)]

/// The upstream EPICS Base release this crate ports — C's
/// `EPICS_VERSION_FULL`.
///
/// Not written here: [`runtime::version`] is generated from the vendored
/// `configure/CONFIG_BASE_VERSION` the same way C generates
/// `epicsVersion.h`, so the next upstream bump is a spec edit plus a
/// regeneration, not a literal somebody has to remember to change.
pub use runtime::version::EPICS_VERSION_FULL as EPICS_BASE_VERSION;

/// `LinkSet` is an `#[async_trait]` trait: re-exported so an out-of-tree lset
/// can annotate its impl without taking its own `async-trait` dependency.
pub use async_trait::async_trait;

pub mod calc;
pub mod error;
pub mod net;
pub mod runtime;
pub mod server;
pub mod types;

pub use epics_macros_rs::epics_main;
pub use epics_macros_rs::epics_test;

#[doc(hidden)]
pub use tokio as __tokio;
