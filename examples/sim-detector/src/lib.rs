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

// `tokio_backend` as well as the feature: this module registers into
// `ad_plugins_rs::ioc::AdIoc`, which the reactor-free backend does not
// compile.
#[cfg(all(feature = "ioc", tokio_backend))]
pub mod ioc_support;

pub use driver::{SimDetector, SimDetectorRuntime, create_sim_detector};
