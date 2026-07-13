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

/// The upstream EPICS Base release this crate ports, as evaluated by
/// `configure/CONFIG_BASE_VERSION`'s `EPICS_VERSION_NUMBER`
/// (`EPICS_VERSION.EPICS_REVISION.EPICS_MODIFICATION.EPICS_PATCH_LEVEL`
/// plus the `-DEV` development-snapshot suffix). This is the single
/// source of truth for the EPICS Base version the CA/PVA diagnostic
/// tools report — the Rust analogue of the C `EPICS_VERSION_STRING`
/// macro that pvxs prints from `version_information`
/// (`pvxs/src/describe.cpp`).
///
/// This is deliberately distinct from the `epics-base-rs` *crate*
/// version (`CARGO_PKG_VERSION`): the crate version tracks the Rust
/// port's own release cadence, while this names the EPICS Base release
/// being ported.
pub const EPICS_BASE_VERSION: &str = "7.0.10.1-DEV";

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
