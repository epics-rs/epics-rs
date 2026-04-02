#![allow(
    clippy::collapsible_if,
    clippy::field_reassign_with_default,
    clippy::if_same_then_else,
    clippy::manual_range_contains,
    clippy::new_without_default,
    clippy::single_match,
    clippy::too_many_arguments
)]

pub mod compute;
pub mod driver;
pub mod params;
pub mod task;
pub mod types;

#[cfg(feature = "ioc")]
pub mod ioc_support;

pub use driver::{SimDetector, SimDetectorRuntime, create_sim_detector};
