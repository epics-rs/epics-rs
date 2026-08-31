//! Request types for the port actor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::time::SystemTime;

use crate::error::AsynStatus;
use crate::param::ParamValue;

/// A param value to set directly in the store (no writeInt32/on_param_change).
/// Mirrors C ADCore's setIntegerParam/setDoubleParam.
///
/// The value is a [`ParamValue`], not one variant per type: this carrier used to
/// enumerate its own subset of the parameter types (Int32/Float64/Octet/
/// Int32Array/Float64Array/UInt32Digital), so a driver thread pushing any other
/// supported type — `Int64`, `Int8Array`, `Int16Array`, `Int64Array`,
/// `Float32Array`, `Enum`, `GenericPointer` — simply had no variant to put it in.
/// Carrying the store's own value type means the two type sets cannot drift
/// apart again: the actor applies it through the single [`crate::param::ParamList::set_value`]
/// dispatch, whose exhaustive match makes a new `ParamValue` variant a compile
/// error rather than an update that never arrives.
#[derive(Debug, Clone)]
pub enum ParamSetValue {
    /// Set a parameter of any type (C `setIntegerParam` / `setDoubleParam` /
    /// `setStringParam` / `doCallbacksXxxArray` …).
    Value {
        reason: usize,
        addr: i32,
        value: ParamValue,
    },
    /// asynUInt32Digital masked set — C `setUIntDigitalParam(reason, value,
    /// mask, interruptMask)`, whose write mask and forced-callback mask a plain
    /// [`Self::Value`] has no room for.
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
    /// C `setParamStatus` / `setParamAlarmStatus` / `setParamAlarmSeverity`
    /// from a background thread — the alarm-push half of the C pattern
    /// `lock(); setParamStatus(..); callParamCallbacks(); unlock()`. The
    /// value is untouched; a status/alarm transition alone marks the param
    /// changed so the flush delivers it (paramVal.cpp:71-78,84-91,97-104),
    /// and the devEpics fill-in maps a non-Success `status` to the EPICS alarm
    /// (asynDisconnected → COMM/INVALID) when `alarm_status`/`alarm_severity`
    /// are left 0 (asynEpicsUtils.c:238-265).
    Status {
        reason: usize,
        addr: i32,
        status: AsynStatus,
        alarm_status: u16,
        alarm_severity: u16,
    },
}

impl ParamSetValue {
    /// Set `reason` at `addr` to `value` — any parameter type.
    pub fn new(reason: usize, addr: i32, value: ParamValue) -> Self {
        Self::Value {
            reason,
            addr,
            value,
        }
    }

    /// C `setUIntDigitalParam`: store `value & mask`, and force `interrupt_mask`
    /// bits into the callback mask even when the stored value is unchanged.
    pub fn uint32_digital(
        reason: usize,
        addr: i32,
        value: u32,
        mask: u32,
        interrupt_mask: u32,
    ) -> Self {
        Self::UInt32Digital {
            reason,
            addr,
            value,
            mask,
            interrupt_mask,
        }
    }

    /// C `setParamStatus(list, index, status)` plus the alarm pair: set the
    /// transport status and EPICS alarm without touching the value. Pass
    /// `alarm_status`/`alarm_severity` 0 to let the record-side fill-in map
    /// `status` itself (the C-normal path).
    pub fn status(
        reason: usize,
        addr: i32,
        status: AsynStatus,
        alarm_status: u16,
        alarm_severity: u16,
    ) -> Self {
        Self::Status {
            reason,
            addr,
            status,
            alarm_status,
            alarm_severity,
        }
    }

    /// The address list this update lands in — the list whose changed flags a
    /// later `callParamCallbacks` has to consume for it to reach a record.
    pub fn addr(&self) -> i32 {
        match self {
            Self::Value { addr, .. }
            | Self::UInt32Digital { addr, .. }
            | Self::Status { addr, .. } => *addr,
        }
    }
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
    /// A queued request that performs no transfer — C `asynCallbackProcess`
    /// (asynRecord.c:808-831) reached with `tmod == asynTMOD_NoIO` and no
    /// pending UCMD/ACMD: it resets ERRS, sets the user's timeout, and calls
    /// nothing.
    ///
    /// The point is the queue entry itself. `process()` queues without consulting
    /// TMOD (`:342-353`), so a NoI/O cycle still wakes the port thread — as
    /// the reconnect-nudge idiom pokes auto-connect — and still meets the queue
    /// gate, so a disconnected port answers it with the refusal the record turns
    /// into ERRS and STATE/MINOR.
    NoIo,
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
    /// Enable / disable — C `pasynManager->enable(pasynUser, enable)`
    /// (`asynManager.c::enable` :2224-2251, fired by asynRecord `ENBL` writes
    /// at `asynRecord.c:484-486`).
    ///
    /// One op for both scopes, as C has one call: `findDpCommon(puserPvt)`
    /// (:538-545) hands `enable` the DEVICE's `dpCommon` when the port is
    /// multi-device and the user names an address, the PORT's otherwise. The
    /// addr rides on the request's own [`crate::user::AsynUser`], so the
    /// resolution happens once, at the owner.
    SetEnable {
        yes: bool,
    },
    /// Enable / disable auto-connect — C
    /// `pasynManager->autoConnect(pasynUser, autoConnect)`
    /// (`asynManager.c::autoConnectAsyn` :2312-2329, fired by asynRecord `AUCT`
    /// writes at `asynRecord.c:481-482`). `asynExceptionAutoConnect` is emitted
    /// unconditionally on every call. Same one-op-two-scopes resolution as
    /// [`RequestOp::SetEnable`].
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
    /// C `pasynManager->blockProcessCallback(pasynUser, allDevices)`
    /// (asynManager.c:1692-1723). `all_devices` is C's own argument, and it
    /// picks which of the **two** holders this block takes: the port-wide
    /// `pport->pblockProcessHolder`, or the `pblockProcessHolder` of the one
    /// `dpCommon` the user's own `addr` resolves to (`findDpCommon`, :1765).
    /// devGpib takes the device form for an SRQ transaction
    /// (devSupportGpib.c:1216), which must not stall the port's other
    /// addresses.
    BlockProcess {
        all_devices: bool,
    },
    /// C `pasynManager->unblockProcessCallback(pasynUser, allDevices)`
    /// (asynManager.c:1725-1774) — releases the holder at the same scope.
    UnblockProcess {
        all_devices: bool,
    },
    /// Resolve a record's bind request to a parameter reason index. Carries the
    /// full [`DrvUserRequest`](crate::port::DrvUserRequest) — drvInfo, asyn `addr`, and the record's asyn
    /// interface — so an on-demand driver can create the parameter with the type
    /// the record will read it as.
    DrvUserCreate(crate::port::DrvUserRequest),
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
    /// Read back the port's input EOS bytes — C `pasynOctet->getInputEos`.
    /// asynRecord's `getEos` (asynRecord.c:1987-2025) calls it after every
    /// IEOS/OEOS put so the record shows what the driver actually holds, not
    /// what was requested. Returns the bytes in [`RequestResult::data`].
    GetInputEos,
    /// Read back the port's output EOS bytes — C `pasynOctet->getOutputEos`.
    GetOutputEos,
    /// Query whether the port's *transport* is connected. C parity:
    /// `pasynManager->isConnected` — the state the driver publishes through
    /// `exceptionConnect`/`exceptionDisconnect`, not "is a record bound to
    /// this port". `asynRecord` reads it in `monitorStatus` (asynRecord.c:
    /// 1089-1093) to refresh CNCT, and gates its `callbackConnect` on it
    /// (:858-888) so a CNCT put never re-connects an already-connected port.
    GetConnected,
    /// Install the echo interpose on top of the port's octet stack. C parity:
    /// `asynInterposeEcho(portName, addr)`
    /// (`asynInterposeEcho.c:165-186`), the iocsh command a startup script
    /// runs *after* the port is configured.
    ///
    /// It is a request rather than a direct `install_interpose` because the actor
    /// owns the driver once the port is registered — the same reason
    /// `SetOption` / `SetInputEos` are requests. Installing from the shell
    /// thread would race every in-flight transfer.
    PushEchoInterpose,
    /// Install the delay interpose on top of the port's octet stack. C parity:
    /// `asynInterposeDelay(portName, addr, delay)`
    /// (`asynInterposeDelay.c:176-215`), registered with iocsh at
    /// `asynInterposeDelay.c:215-237`. Same actor-ownership reason as
    /// [`RequestOp::PushEchoInterpose`].
    PushDelayInterpose {
        delay: std::time::Duration,
    },
    /// Install the EOS interpose on the addressed device's octet stack. C
    /// parity: `asynInterposeEosConfig(portName, addr, processEosIn,
    /// processEosOut)` (`asynInterposeEos.c:84-140`), registered with iocsh at
    /// :393-410. The two flags select which half of the layer is live.
    PushEosInterpose {
        process_in: bool,
        process_out: bool,
    },
    /// Set (or clear) the port's time-stamp source by NAME. C parity:
    /// `asynRegisterTimeStampSource(portName, functionName)` /
    /// `asynUnregisterTimeStampSource(portName)` (asynShellCommands.c:1181-1223)
    /// — C resolves the name through `registryFunctionFind` and hands the
    /// function to `pasynManager->registerTimeStampSource`. The NAME travels,
    /// not the function: that is what makes it resolvable on the far side of a
    /// remote port, exactly as C resolves it in the IOC's own registry.
    /// `None` = unregister (back to the driver's default clock).
    SetTimeStampSource {
        name: Option<String>,
    },
    /// Install the flush-timeout interpose on the addressed device's octet
    /// stack. C parity: `asynInterposeFlushConfig(portName, addr, timeout)`
    /// (`asynInterposeFlush.c:66-91`); C's shell argument is in milliseconds
    /// and `<= 0` means 1 ms (:78-79), so the conversion happens at the shell
    /// and the op carries a real duration.
    PushFlushInterpose {
        flush_timeout: std::time::Duration,
    },
    /// Send a GPIB universal command byte — C `asynGpib::universalCmd`
    /// (asynGpib.c:480-484). asynRecord's UCMD dispatch
    /// (`gpibUniversalCmd`, asynRecord.c:1638-1679).
    GpibUniversalCmd {
        cmd: u8,
    },
    /// Send a GPIB addressed-command frame — C `asynGpib::addressedCmd`
    /// (asynGpib.c:472-478). asynRecord's ACMD dispatch builds the frame
    /// (`gpibAddressedCmd`, asynRecord.c:1681-1756).
    GpibAddressedCmd {
        data: Vec<u8>,
    },
    /// Assert Interface Clear — C `asynGpib::ifc` (asynGpib.c:486-490).
    GpibIfc,
    /// Set the Remote Enable line — C `asynGpib::ren` (asynGpib.c:492-496).
    GpibRen {
        enable: bool,
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
    /// Per-record octet length cap (from DrvUserCreate; C `modbusDrvUser_t.len`).
    /// `None` when the drvInfo carried no cap.
    pub max_octet_len: Option<usize>,
    /// Enum index (from EnumRead).
    pub enum_index: Option<usize>,
    /// Driver enum string/value/severity table (from EnumRead). C asyn
    /// device support reads this via `asynEnum->read` and pushes it onto
    /// the record's state fields (ZRST/ZRVL/ZRSV…, ZNAM/ONAM…) at init —
    /// see `devAsynInt32.c::initCommon` (298-324) / `setEnums` (415-435).
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
            max_octet_len: None,
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

    pub fn drv_user_create(reason: usize, max_octet_len: Option<usize>) -> Self {
        Self {
            reason: Some(reason),
            max_octet_len,
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
///
/// The queue-wait timeout (C `queueTimeoutCallback`, asynManager.c:647-700) is
/// the second way a request can leave the queue without running, and it obeys
/// the same rule: the timer callback returns immediately when `!isQueued`
/// (:655-661), so a request the port thread has already dequeued always
/// completes. `TimedOut` is therefore a sibling of `Cancelled` — a terminal
/// state reachable only from `Queued` — and the two together are the complete
/// set of "this request never ran" outcomes. Which one won is what tells the
/// caller *which* C callback to report ("I/O request canceled" vs "process
/// queueRequest timeout"), so they are distinct states rather than one flag.
const STATE_QUEUED: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_DONE: u8 = 2;
const STATE_CANCELLED: u8 = 3;
const STATE_TIMED_OUT: u8 = 4;

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

    /// C `queueTimeoutCallback` (asynManager.c:647-700): the queue-wait deadline
    /// expired. Removes the request from the queue iff it is still queued —
    /// C's `if(!puserPvt->isQueued) { ...; return; }` guard (:655-661) — and
    /// returns whether it won.
    ///
    /// `false` means the port thread had already dequeued the request: the timer
    /// fired too late, the I/O runs to completion and reports normally, and the
    /// caller must keep waiting for it. This is the same `isQueued` gate
    /// [`Self::cancel`] answers for `AQR`, so a cancel and a timeout racing the
    /// same request cannot both win.
    pub fn time_out_if_queued(&self) -> bool {
        self.0
            .compare_exchange(
                STATE_QUEUED,
                STATE_TIMED_OUT,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_ok()
    }

    /// Executor at dequeue: claim the request for execution (C dequeue under
    /// `asynManagerLock`, asynManager.c:1661-1666 is the cancel counterpart).
    ///
    /// Returns `false` iff the request left the queue without running — it was
    /// cancelled (`AQR`) or its queue-wait deadline expired — in which case the
    /// executor must drop it and report that outcome. Otherwise the token enters
    /// `Running`. A multi-phase plan re-claims the same token for its next phase
    /// from `Done`, so this transitions from either `Queued` or `Done`;
    /// `Cancelled` and `TimedOut` are terminal.
    pub fn begin_running(&self) -> bool {
        let mut cur = self.0.load(AtomicOrdering::Acquire);
        loop {
            if cur == STATE_CANCELLED || cur == STATE_TIMED_OUT {
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

    /// True iff the request was removed from the queue by its queue-wait
    /// deadline — the C `queueTimeoutCallback` outcome. Mutually exclusive with
    /// [`Self::is_cancelled`]: both transition out of `Queued`, so exactly one
    /// can win.
    pub fn is_timed_out(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire) == STATE_TIMED_OUT
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
