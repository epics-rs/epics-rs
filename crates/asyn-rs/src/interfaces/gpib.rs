//! GPIB (IEEE-488) interface definitions.

use crate::error::AsynResult;
use crate::user::AsynUser;

/// GPIB addressed commands (require device address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpibCommand {
    /// Group Execute Trigger
    GET,
    /// Go To Local
    GTL,
    /// Selected Device Clear
    SDC,
    /// Serial Poll Disable
    SPD,
    /// Serial Poll Enable
    SPE,
    /// Take Control Synchronous
    TCTSynch,
    /// Take Control Asynchronous
    TCTAsync,
    /// Unlisten
    UNL,
    /// Untalk
    UNT,
}

/// GPIB universal commands (no device address needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpibUniversalCommand {
    /// Device Clear (all devices)
    DCL,
    /// Local Lockout
    LLO,
    /// Serial Poll Disable
    SPD,
    /// Serial Poll Enable
    SPE,
    /// Interface Clear
    IFC,
    /// Remote Enable
    REN,
}

/// SRQ (Service Request) status.
#[derive(Debug, Clone)]
pub struct SrqStatus {
    /// Whether SRQ is currently asserted.
    pub srq_asserted: bool,
    /// Status byte from serial poll (if available).
    pub status_byte: Option<u8>,
}

/// GPIB interface trait.
///
/// Provides IEEE-488 bus control operations for GPIB-capable drivers.
pub trait AsynGpib: Send + Sync {
    /// Send an addressed GPIB command.
    fn addressed_cmd(&mut self, user: &AsynUser, cmd: GpibCommand, addr: i32) -> AsynResult<()>;

    /// Send a universal GPIB command.
    fn universal_cmd(&mut self, user: &AsynUser, cmd: GpibUniversalCommand) -> AsynResult<()>;

    /// Assert Interface Clear (IFC).
    fn ifc(&mut self, user: &AsynUser) -> AsynResult<()>;

    /// Set Remote Enable (REN) state.
    fn ren(&mut self, user: &AsynUser, enable: bool) -> AsynResult<()>;

    /// Query SRQ (Service Request) status.
    fn srq_status(&self, user: &AsynUser) -> AsynResult<SrqStatus>;

    /// Enable or disable SRQ notification.
    fn srq_enable(&mut self, user: &AsynUser, enable: bool) -> AsynResult<()>;

    /// Perform a serial poll, returning the status byte.
    fn serial_poll(&mut self, user: &AsynUser) -> AsynResult<u8>;
}
