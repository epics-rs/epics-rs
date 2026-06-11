//! pvAccess client — re-exports the native [`crate::client_native`] impl.
//!
//! This shim exists only so existing callers continue to compile against
//! `crate::client::*`.

pub use crate::client_native::{AssertedIdentity, PvGetResult, PvaClient, PvaClientBuilder};
