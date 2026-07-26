//! The concurrency, timing, logging and OS-facing primitives an IOC is built
//! on — C `libCom`'s `epicsThread` / `epicsTime` / `errlog` / `envDefs` half.
//!
//! # The two backends
//!
//! [`task`]'s spawn/sleep/interval seam has two mutually exclusive backends,
//! selected by this crate's `build.rs` into one `cfg` rather than repeated as a
//! target/feature predicate at each of the ~two dozen seam sites:
//!
//! * `tokio_backend` — the tokio runtime. The hosted default.
//! * `exec_backend` — the std-thread [`background`] executor (callback pool +
//!   delayed timer + scanOnce worker). Unconditional on RTEMS, which has no
//!   tokio reactor, and host-selectable through the `rtems-exec-model` feature
//!   for a Linux blocking-front-end deployment that wants the same
//!   runtime-free backend.
//!
//! [`crate::EXEC_BACKEND`] states which one this build got, so a crate above
//! can pin its own copy of the predicate to it.

pub mod accept;
pub mod background;
pub mod blocking_io;
pub mod build_info;
pub mod env;
pub mod env_table;
pub mod epics_string;
pub mod fs;
pub mod general_time;
pub mod json_string;
pub mod log;
pub mod net;
pub mod socket;
pub mod stdlib;
pub mod supervise;
pub mod sync;
pub mod task;
pub mod time;
pub mod version;
pub mod worker_pool;

// Re-export tokio::select! macro through the runtime facade.
pub use tokio::select;
