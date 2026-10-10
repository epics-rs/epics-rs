//! Errors raised by the PVA gateway internals.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GwError {
    #[error(
        "upstream PV {0:?} did not deliver an initial monitor event before the connect timeout"
    )]
    UpstreamTimeout(String),
    // No `#[from]`: a `PvaError` comes from either side, and only its call
    // site knows which.
    #[error("upstream PVA error: {0}")]
    Upstream(epics_pva_rs::error::PvaError),
    #[error("downstream PVA server error: {0}")]
    Downstream(epics_pva_rs::error::PvaError),
    #[error("gateway not running")]
    NotRunning,
    #[error("channel cache full (cap = {0})")]
    CacheFull(usize),
    #[error("{0}")]
    Other(String),
}

pub type GwResult<T> = Result<T, GwError>;
