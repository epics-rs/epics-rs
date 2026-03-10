//! Transport layer for port communication.
//!
//! Defines the [`RuntimeClient`] trait and provides [`InProcessClient`]
//! as the primary fast-path implementation (no serialization).

pub mod client;
pub mod error;
pub mod in_process;
pub mod tracker;

pub use client::{ConnectionState, RuntimeClient};
pub use error::TransportError;
pub use in_process::InProcessClient;
pub use tracker::RequestTracker;
