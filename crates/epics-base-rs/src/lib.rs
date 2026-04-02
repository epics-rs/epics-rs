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

pub mod calc;
pub mod error;
pub mod runtime;
pub mod server;
pub mod types;

pub use epics_macros_rs::epics_main;
pub use epics_macros_rs::epics_test;

#[doc(hidden)]
pub use tokio as __tokio;
