//! procServ — PTY-based process supervisor.
//!
//! See crate-level docs ([`crate`]) for architectural rationale.
//! This module is gated on `cfg(unix)` because it depends on
//! `forkpty(3)` and POSIX signals.

pub mod child;
pub mod client;
pub mod config;
#[cfg(unix)]
pub mod console;
pub mod daemon;
pub mod endpoint;
pub mod error;
pub mod listener;
pub mod menu;
pub mod messages;
pub mod restart;
pub mod sidecar;
pub mod supervisor;
pub mod telnet;

pub use config::ProcServConfig;
pub use error::{ProcServError, ProcServResult};
pub use restart::{RestartMode, RestartPolicy};
pub use supervisor::ProcServ;
