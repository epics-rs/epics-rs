#![cfg_attr(
    test,
    allow(clippy::disallowed_macros, reason = "tests may keep eprintln!")
)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_range_contains,
    clippy::new_without_default,
    clippy::too_many_arguments
)]

pub mod beamline_sim;
pub mod detector;
