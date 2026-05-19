//! pvAccess client — re-exports the native [`crate::client_native`] impl.
//!
//! This file intentionally contains no `spvirit_client` references. It exists
//! only so existing callers continue to compile against `crate::client::*`.

pub use crate::client_native::{AssertedIdentity, PvGetResult, PvaClient, PvaClientBuilder};
