//! Request types for the port actor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::time::SystemTime;

use crate::error::AsynStatus;

/// A param value to set directly in the store (no writeInt32/on_param_change).
/// Mirrors C ADCore's setIntegerParam/setDoubleParam.
#[derive(Debug, Clone)]
pub enum ParamSetValue {
    Int32 {
        reason: usize,
        addr: i32,
        value: i32,
    },
    Float64 {
        reason: usize,
        addr: i32,
        value: f64,
    },
    Octet {
        reason: usize,
        addr: i32,
        value: String,
    },
    Float64Array {
        reason: usize,
        addr: i32,
        value: Vec<f64>,
    },
    Int32Array {
        reason: usize,
        addr: i32,
        value: Vec<i32>,
    },
    UInt32Digital {
        reason: usize,
        addr: i32,
        value: u32,
        mask: u32,
        /// Bits to force into the I/O Intr callback mask even when the
        /// stored value is unchanged (C `setUIntDigitalParam(..,
        /// interruptMask)`); `0` for a plain value set.
        interrupt_mask: u32,
    },
}

/// Operation the worker thread will dispatch to the port driver.
#[derive(Debug, Clone)]
pub enum RequestOp {
    OctetWrite {
        data: Vec<u8>,
    },
    OctetRead {
        buf_size: usize,
    },
    OctetWriteRead {
        data: Vec<u8>,
        buf_size: usize,
        /// Whether to drain the driver's input buffer before the write.
        /// `true` = `asynOctetSyncIO::writeRead` (flush → write → read), the
        /// StreamDevice/asynRecord pattern that discards stale warm-line bytes.
        /// `false` = `devAsynOctet` raw write-then-read (no flush) — the
        /// command-response dset (`callbackSiCmdResponse`) returns whatever the
        /// device sends, including bytes already in the buffer.
        flush: bool,
    },
    /// Binary octet write: writes `data` raw with the driver's output EOS
    /// temporarily suppressed. C parity: asynRecord binary output
    /// (`asynRecord.c:1528-1541`) saves the current output EOS, sets it to
    /// NULL for the write, and restores it. The actor performs the
    /// save/clear/restore atomically under its serial ownership so the EOS
    /// is restored on every exit path.
    OctetWriteBinary {
        data: Vec<u8>,
    },
    /// Binary octet read: reads with the driver's input EOS temporarily
    /// suppressed. C parity: asynRecord binary input
    /// (`asynRecord.c:1564-1577`) saves the current input EOS, sets it to
    /// NULL for the read, and restores it. The actor brackets the read so
    /// the EOS is restored on every exit path.
    OctetReadBinary {
        buf_size: usize,
    },
    Int32Write {
        value: i32,
    },
    Int32Read,
    Int64Write {
        value: i64,
    },
    Int64Read,
    Float64Write {
        value: f64,
    },
    Float64Read,
    UInt32DigitalWrite {
        value: u32,
        mask: u32,
    },
    UInt32DigitalRead {
        mask: u32,
    },
    Flush,
    /// Connect to the port (bypass enabled/connected checks).
    Connect,
    /// Disconnect from the port (bypass enabled/connected checks).
    Disconnect,
    /// Permanently shut down a `ASYN_DESTRUCTIBLE` port. C parity:
    /// `asynManager.c::shutdownPort` (lines 2251-2308). Marks the
    /// port defunct so every subsequent request short-circuits;
    /// idempotent; broadcasts `AsynException::Shutdown`.
    ShutdownPort,
    /// Connect a specific device address (multi-device ports).
    ConnectAddr,
    /// Disconnect a specific device address (multi-device ports).
    DisconnectAddr,
    /// Enable a specific device address (multi-device ports).
    EnableAddr,
    /// Disable a specific device address (multi-device ports).
    DisableAddr,
    /// Enable / disable the entire port. C parity:
    /// `pasynManager->enable(pasynUser, enable)`
    /// (`asynManager.c::enable`, fired by asynRecord `ENBL` writes
    /// at `asynRecord.c:484-486`).
    SetEnable {
        yes: bool,
    },
    /// Enable / disable auto-connect for the port. C parity:
    /// `pasynManager->autoConnect(pasynUser, autoConnect)`
    /// (`asynManager.c::autoConnect`, fired by asynRecord `AUCT`
    /// writes at `asynRecord.c:481-482`). `asynExceptionAutoConnect`
    /// is emitted unconditionally on every call.
    SetAutoConnect {
        yes: bool,
    },
    /// Query int32 bounds (low, high).
    GetBoundsInt32,
    /// Query int64 bounds (low, high).
    GetBoundsInt64,
    /// Query whether the port is currently enabled. C parity:
    /// `pasynManager->isEnabled` (`asynManager.c`).
    GetEnable,
    /// Query whether auto-connect is enabled for the port. C parity:
    /// `pasynManager->isAutoConnect` (`asynManager.c`).
    GetAutoConnect,
    /// Block the port: only this user's requests will be dequeued until unblocked.
    BlockProcess,
    /// Unblock the port.
    UnblockProcess,
    /// Resolve a driver info string to a parameter reason index.
    DrvUserCreate {
        drv_info: String,
        /// The record's asyn `addr`, so a multi-device driver can reject an
        /// out-of-range address at bind time (C `drvUserCreate` `checkOffset`).
        addr: i32,
    },
    /// Read an enum value (index + string choices).
    EnumRead,
    /// Write an enum index.
    EnumWrite {
        index: usize,
    },
    /// Read an i32 array.
    Int32ArrayRead {
        max_elements: usize,
    },
    /// Write an i32 array.
    Int32ArrayWrite {
        data: Vec<i32>,
    },
    /// Read an f64 array.
    Float64ArrayRead {
        max_elements: usize,
    },
    /// Write an f64 array.
    Float64ArrayWrite {
        data: Vec<f64>,
    },
    /// Read an i8 array.
    Int8ArrayRead {
        max_elements: usize,
    },
    /// Write an i8 array.
    Int8ArrayWrite {
        data: Vec<i8>,
    },
    /// Read an i16 array.
    Int16ArrayRead {
        max_elements: usize,
    },
    /// Write an i16 array.
    Int16ArrayWrite {
        data: Vec<i16>,
    },
    /// Read an i64 array.
    Int64ArrayRead {
        max_elements: usize,
    },
    /// Write an i64 array.
    Int64ArrayWrite {
        data: Vec<i64>,
    },
    /// Read an f32 array.
    Float32ArrayRead {
        max_elements: usize,
    },
    /// Write an f32 array.
    Float32ArrayWrite {
        data: Vec<f32>,
    },
    /// Set params directly in the store (like C setIntegerParam/setDoubleParam)
    /// and then fire interrupt notifications (callParamCallbacks).
    /// Does NOT trigger writeInt32/on_param_change — avoids re-entrancy.
    CallParamCallbacks {
        addr: i32,
        /// Param updates to apply before firing callbacks.
        /// Empty = just fire callbacks for previously changed params.
        updates: Vec<ParamSetValue>,
    },
    /// Get a port/driver option by key.
    GetOption {
        key: String,
    },
    /// Set a port/driver option by key.
    SetOption {
        key: String,
        value: String,
    },
    /// Print a driver report (matches C `asynManager->report` /
    /// iocsh `asynReport`). The actor calls
    /// [`crate::port::PortDriver::report`] which writes to stderr
    /// at the requested verbosity. Carried by the actor so the
    /// driver is observed from its own thread (consistent with C
    /// asyn's `pport->lock` invariant for `report`).
    Report {
        level: i32,
    },
    /// Set the port's input EOS bytes — C `pasynOctet->setInputEos`.
    /// Drives the same `PortDriver::set_input_eos(&[u8])` hook the EOS
    /// interpose layer reads, so asynRecord IEOS writes survive a
    /// round trip through the actor (previously routed through the
    /// generic option HashMap which no driver consumes).
    SetInputEos {
        eos: Vec<u8>,
    },
    /// Set the port's output EOS bytes — C `pasynOctet->setOutputEos`.
    SetOutputEos {
        eos: Vec<u8>,
    },
}

/// Result returned by the worker after executing a request.
#[derive(Debug)]
pub struct RequestResult {
    pub status: AsynStatus,
    pub message: String,
    pub nbytes: usize,
    pub data: Option<Vec<u8>>,
    pub int_val: Option<i32>,
    pub int64_val: Option<i64>,
    pub float_val: Option<f64>,
    pub uint_val: Option<u32>,
    /// Reason index (from DrvUserCreate).
    pub reason: Option<usize>,
    /// Enum index (from EnumRead).
    pub enum_index: Option<usize>,
    /// Driver enum string/value/severity table (from EnumRead). C asyn
    /// device support reads this via `asynEnum->read` and pushes it onto
    /// the record's state fields (ZRST/ZRVL/ZRSV…, ZNAM/ONAM…) at init —
    /// see `devAsynInt32.c::initCommon` (297-324) / `setEnums` (415-435).
    pub enum_entries: Option<Arc<[crate::param::EnumEntry]>>,
    /// i32 array data (from Int32ArrayRead).
    pub int32_array: Option<Vec<i32>>,
    /// f64 array data (from Float64ArrayRead).
    pub float64_array: Option<Vec<f64>>,
    /// i8 array data (from Int8ArrayRead).
    pub int8_array: Option<Vec<i8>>,
    /// i16 array data (from Int16ArrayRead).
    pub int16_array: Option<Vec<i16>>,
    /// i64 array data (from Int64ArrayRead).
    pub int64_array: Option<Vec<i64>>,
    /// f32 array data (from Float32ArrayRead).
    pub float32_array: Option<Vec<f32>>,
    /// Alarm status from the driver param store (populated on reads).
    pub alarm_status: u16,
    /// Alarm severity from the driver param store (populated on reads).
    pub alarm_severity: u16,
    /// Timestamp from the driver param store (populated on reads).
    pub timestamp: Option<SystemTime>,
    /// Device read auxiliary status (C `pasynUser->auxStatus`), populated on
    /// reads from the param store alongside the value. Distinct from
    /// [`Self::status`] (the request/op outcome that drives an `Err`/Error
    /// reply): a read OP can succeed and return a value while `aux_status`
    /// flags that value invalid. Device support gates the value store on this —
    /// C `processAi` stores the value only when `result.status == asynSuccess`
    /// and otherwise returns -1 keeping the prior value (devAsynInt32.c:848-855)
    /// — the same way the I/O Intr ring gates on `CachedInterrupt.aux_status`.
    pub aux_status: AsynStatus,
    /// Option value string (from GetOption).
    pub option_value: Option<String>,
    /// Int64 bounds (from GetBoundsInt32/Int64).
    pub bounds: Option<(i64, i64)>,
    /// End-of-message reason flags from an octet read.
    ///
    /// C parity: `asynOctet::read` returns `nbytes` together with
    /// `int *eomReason` (`interfaces/asynOctet.h:38-40`). The flags
    /// `ASYN_EOM_CNT | ASYN_EOM_EOS | ASYN_EOM_END` mirror
    /// [`crate::interpose::EomReason`]. Stored as `u32` so the
    /// request layer stays bitflag-crate-free; converters live on
    /// `EomReason::from_bits_truncate`.
    pub eom_reason: u32,
}

impl RequestResult {
    fn base() -> Self {
        Self {
            status: AsynStatus::Success,
            message: String::new(),
            nbytes: 0,
            data: None,
            int_val: None,
            int64_val: None,
            float_val: None,
            uint_val: None,
            reason: None,
            enum_index: None,
            enum_entries: None,
            int32_array: None,
            float64_array: None,
            int8_array: None,
            int16_array: None,
            int64_array: None,
            float32_array: None,
            alarm_status: 0,
            alarm_severity: 0,
            timestamp: None,
            aux_status: AsynStatus::Success,
            option_value: None,
            bounds: None,
            eom_reason: 0,
        }
    }

    pub fn write_ok() -> Self {
        Self::base()
    }

    /// Octet write result carrying the number of bytes transferred
    /// (C `asynOctet::write`'s `*nbytesTransfered`). Used by
    /// `PortHandle::write_octet` / `SyncIO::write_octet` to report how
    /// many bytes the driver actually wrote on success.
    pub fn write_n(nbytes: usize) -> Self {
        Self {
            nbytes,
            ..Self::base()
        }
    }

    pub fn octet_read(buf: Vec<u8>, nbytes: usize) -> Self {
        Self {
            nbytes,
            data: Some(buf),
            ..Self::base()
        }
    }

    /// Variant of [`Self::octet_read`] that carries the
    /// end-of-message reason flags returned by
    /// [`crate::port::PortDriver::io_read_octet_eom`]. The raw `u32`
    /// is decoded with `EomReason::from_bits_truncate` on the
    /// consumer side.
    pub fn octet_read_eom(buf: Vec<u8>, nbytes: usize, eom_reason: u32) -> Self {
        Self {
            nbytes,
            data: Some(buf),
            eom_reason,
            ..Self::base()
        }
    }

    pub fn int32_read(value: i32) -> Self {
        Self {
            int_val: Some(value),
            ..Self::base()
        }
    }

    pub fn int64_read(value: i64) -> Self {
        Self {
            int64_val: Some(value),
            ..Self::base()
        }
    }

    pub fn float64_read(value: f64) -> Self {
        Self {
            float_val: Some(value),
            ..Self::base()
        }
    }

    pub fn uint32_read(value: u32) -> Self {
        Self {
            uint_val: Some(value),
            ..Self::base()
        }
    }

    pub fn drv_user_create(reason: usize) -> Self {
        Self {
            reason: Some(reason),
            ..Self::base()
        }
    }

    pub fn enum_read(index: usize) -> Self {
        Self {
            enum_index: Some(index),
            ..Self::base()
        }
    }

    /// [`Self::enum_read`] carrying the driver's full enum table so the
    /// device-support init path can propagate it to the record's state
    /// fields (C `setEnums`). The index is the current selection.
    pub fn enum_read_with_entries(index: usize, entries: Arc<[crate::param::EnumEntry]>) -> Self {
        Self {
            enum_index: Some(index),
            enum_entries: Some(entries),
            ..Self::base()
        }
    }

    pub fn int32_array_read(data: Vec<i32>) -> Self {
        Self {
            int32_array: Some(data),
            ..Self::base()
        }
    }

    pub fn float64_array_read(data: Vec<f64>) -> Self {
        Self {
            float64_array: Some(data),
            ..Self::base()
        }
    }

    pub fn int8_array_read(data: Vec<i8>) -> Self {
        Self {
            int8_array: Some(data),
            ..Self::base()
        }
    }

    pub fn int16_array_read(data: Vec<i16>) -> Self {
        Self {
            int16_array: Some(data),
            ..Self::base()
        }
    }

    pub fn int64_array_read(data: Vec<i64>) -> Self {
        Self {
            int64_array: Some(data),
            ..Self::base()
        }
    }

    pub fn float32_array_read(data: Vec<f32>) -> Self {
        Self {
            float32_array: Some(data),
            ..Self::base()
        }
    }

    pub fn option_read(value: String) -> Self {
        Self {
            option_value: Some(value),
            ..Self::base()
        }
    }

    pub fn bounds_read(low: i64, high: i64) -> Self {
        Self {
            bounds: Some((low, high)),
            ..Self::base()
        }
    }

    /// Attach alarm/timestamp metadata to this result.
    pub fn with_alarm(
        mut self,
        alarm_status: u16,
        alarm_severity: u16,
        timestamp: Option<SystemTime>,
    ) -> Self {
        self.alarm_status = alarm_status;
        self.alarm_severity = alarm_severity;
        self.timestamp = timestamp;
        self
    }
}

/// Lifecycle of a queued request, mirroring C `asynManager` queue/callback
/// state so that `AQR` cancellation reproduces the `cancelRequest` `wasQueued`
/// split (asynManager.c:1630-1690) by construction rather than by a runtime
/// guard.
///
/// `cancelRequest` removes the request and reports `wasQueued==1` ONLY while it
/// is still on the queue (asynManager.c:1661-1668); once the port thread has
/// dequeued it and is running the callback (`callbackActive`) or it has already
/// finished, `wasQueued==0` and the I/O runs to completion and is reported
/// normally (asynManager.c:1645-1659). `Queued` is the only state a cancel can
/// win from; the executor's `Queued -> Running` transition closes that window.
const STATE_QUEUED: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_DONE: u8 = 2;
const STATE_CANCELLED: u8 = 3;

/// Token tracking the queue/execution lifecycle of an off-thread request.
///
/// The state machine makes the C `wasQueued` semantics hold by construction:
/// `cancel()` succeeds only from `Queued`, the executor claims the request with
/// `begin_running()` (refused once cancelled) and releases it with `finish()`,
/// so a cancel that arrives after execution started cannot transition the token
/// and is a no-op — the I/O completes and applies normally.
#[derive(Clone, Debug)]
pub struct CancelToken(pub Arc<AtomicU8>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(STATE_QUEUED)))
    }

    /// `AQR` / C `cancelRequest`: cancel the request iff it is still queued.
    ///
    /// Returns the C `wasQueued` flag — `true` when the request was removed
    /// from the queue (the caller must report "I/O request canceled",
    /// asynRecord.c:397-404); `false` when it had already been dequeued and was
    /// running or had completed, in which case the I/O runs to completion and
    /// reports normally (asynManager.c:1645-1659).
    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(
                STATE_QUEUED,
                STATE_CANCELLED,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_ok()
    }

    /// Executor at dequeue: claim the request for execution (C dequeue under
    /// `asynManagerLock`, asynManager.c:1661-1666 is the cancel counterpart).
    ///
    /// Returns `false` iff the request was cancelled while queued, in which
    /// case the executor must drop it and report cancellation. Otherwise the
    /// token enters `Running`. A multi-phase plan re-claims the same token for
    /// its next phase from `Done`, so this transitions from either `Queued` or
    /// `Done`; only `Cancelled` is terminal.
    pub fn begin_running(&self) -> bool {
        let mut cur = self.0.load(AtomicOrdering::Acquire);
        loop {
            if cur == STATE_CANCELLED {
                return false;
            }
            match self.0.compare_exchange_weak(
                cur,
                STATE_RUNNING,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Executor at completion: mark the running request finished so a later
    /// cancel is a no-op (the C `wasQueued==0` window). Idempotent and a no-op
    /// from any state other than `Running`.
    pub fn finish(&self) {
        let _ = self.0.compare_exchange(
            STATE_RUNNING,
            STATE_DONE,
            AtomicOrdering::AcqRel,
            AtomicOrdering::Acquire,
        );
    }

    /// True iff the request was cancelled while still queued — the C
    /// `wasQueued==true` outcome. A cancel that lost the race (the executor had
    /// already begun running) leaves the state `Running`/`Done`, so this stays
    /// `false` and the completed I/O applies normally.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire) == STATE_CANCELLED
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_succeeds_only_while_queued() {
        // C `wasQueued==1`: a still-queued request is cancelled and removed.
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        assert!(token.cancel(), "a queued request reports wasQueued==true");
        assert!(token.is_cancelled());
        // The executor then refuses to run it (it was removed from the queue).
        assert!(
            !token.begin_running(),
            "a cancelled request is not claimed for execution"
        );
    }

    #[test]
    fn cancel_after_begin_running_is_noop() {
        // C `wasQueued==0` while `callbackActive`: the I/O runs to completion.
        let token = CancelToken::new();
        assert!(
            token.begin_running(),
            "the executor claims a queued request"
        );
        assert!(
            !token.cancel(),
            "a cancel during execution reports wasQueued==false"
        );
        assert!(
            !token.is_cancelled(),
            "the running I/O is not treated as cancelled"
        );
        token.finish();
        assert!(!token.is_cancelled(), "the completed I/O applies normally");
    }

    #[test]
    fn cancel_after_finish_is_noop() {
        // C `wasQueued==0` after the callback finished: nothing to cancel.
        let token = CancelToken::new();
        assert!(token.begin_running());
        token.finish();
        assert!(
            !token.cancel(),
            "a cancel after completion reports wasQueued==false"
        );
        assert!(!token.is_cancelled());
    }

    #[test]
    fn begin_running_reclaims_token_for_next_phase() {
        // A WriteRead plan threads one token through two phases; the read phase
        // re-claims the token the write phase finished.
        let token = CancelToken::new();
        assert!(token.begin_running(), "write phase claims the queued token");
        token.finish();
        assert!(
            token.begin_running(),
            "read phase re-claims the finished token"
        );
        token.finish();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_is_terminal_across_phases() {
        // Once cancelled while queued, no later phase may run.
        let token = CancelToken::new();
        assert!(token.cancel());
        assert!(!token.begin_running(), "cancelled is terminal");
        assert!(token.is_cancelled());
    }
}
