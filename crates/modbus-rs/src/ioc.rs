//! IOC integration — the `asyn-rs` `PortDriver` and the `iocsh` commands.
//!
//! Available only with the `ioc` feature. Port of the `drvModbusAsyn` /
//! `modbusInterpose` IOC-facing surface:
//!
//! - [`ModbusPortDriver`] implements `asyn_rs::port::PortDriver`, wrapping a
//!   [`ModbusEngine`]. Records bind through `drv_user_create`: the `drvInfo`
//!   string selects a data-type parameter (the *reason*) and the asyn `addr`
//!   carries the register offset.
//! - [`SyncIoTransport`] bridges [`ModbusEngine`]'s [`OctetTransport`] onto an
//!   underlying `asyn-rs` octet port via blocking sync I/O.
//! - `modbusInterposeConfig` records a link type for an octet port;
//!   `drvModbusAsynConfigure` creates the driver and its poller.
//!
//! The C `pasynUser->drvUser` per-record `{dataType, len}` struct maps onto two
//! asyn-rs channels: the data type is encoded in the reason, and the optional
//! `=N` string length is returned from `drv_user_create` as the per-record
//! octet cap (`DrvUserInfo::max_octet_len`), which the binding applies to its
//! octet buffer length — the asyn-rs analogue of C `getStringLen` capping the
//! asyn octet `maxLen`. The `=N` suffix is validated as C does: it is legal
//! only for the eight string types and must be a non-negative integer, so an
//! invalid `drvInfo` fails record init rather than being silently accepted.
//!
//! # Absolute addressing
//!
//! When the port is configured with `modbusStartAddress == -1` the driver is
//! in *absolute* addressing mode. The C `drvModbusAsyn` then disables the
//! read poller and every record issues an individual Modbus request to its
//! own absolute wire address (the asyn `addr`) with a per-record length.
//! [`ModbusPortDriver`] mirrors this: each accessor branches on
//! [`ModbusConfig::absolute_addressing`] and, in absolute mode, calls
//! [`ModbusEngine::read_absolute`] / [`ModbusEngine::write_absolute`] instead
//! of indexing the polled buffer; `poll_cycle` is a no-op and the periodic
//! poller is not spawned.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interpose::EomReason;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{DrvUserInfo, DrvUserRequest, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::sync_io::SyncIOHandle;
use asyn_rs::trace::TraceManager;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::iocsh::registry::*;

use crate::datatype::{self, ALL_DATA_TYPES, ModbusDataType};
use crate::driver::{
    ModbusConfig, ModbusEngine, ModbusFunctionCode, ModbusIoResponse, OctetTransport,
};
use crate::error::ModbusError;
use crate::interpose::LinkType;
use crate::protocol::MAX_MODBUS_FRAME_SIZE;

/// `drvInfo` used by records that want the port's *default* data type — the C
/// `MODBUS_DATA_STRING`. Records needing a different type pass the data-type
/// string itself (e.g. `INT32_LE`) as `drvInfo`.
const PARAM_DATA: &str = "MODBUS_DATA";

/// Control/statistics parameter names (the `MODBUS_*_STRING` defines that are
/// registered with `asynPortDriver`, not data-type strings).
const PARAM_READ: &str = "MODBUS_READ";
const PARAM_READ_OK: &str = "READ_OK";
const PARAM_WRITE_OK: &str = "WRITE_OK";
const PARAM_IO_ERRORS: &str = "IO_ERRORS";
const PARAM_LAST_IO_TIME: &str = "LAST_IO_TIME";
const PARAM_MAX_IO_TIME: &str = "MAX_IO_TIME";
const PARAM_POLL_DELAY: &str = "POLL_DELAY";
const PARAM_ENABLE_HISTOGRAM: &str = "ENABLE_HISTOGRAM";
const PARAM_READ_HISTOGRAM: &str = "READ_HISTOGRAM";
const PARAM_HISTOGRAM_BIN_TIME: &str = "HISTOGRAM_BIN_TIME";
const PARAM_HISTOGRAM_TIME_AXIS: &str = "HISTOGRAM_TIME_AXIS";

/// `ModbusError` → `AsynError` bridge.
fn to_asyn(e: ModbusError) -> AsynError {
    let status = match e {
        ModbusError::Timeout => AsynStatus::Timeout,
        _ => AsynStatus::Error,
    };
    AsynError::Status {
        status,
        message: e.to_string(),
    }
}

/// Decode a poller register block (from a record's offset to the block end)
/// into an int32 array — the relative-mode decode of [`Ioc::read_int32_array`],
/// one element per `register_count` registers. Mirrors C `readPoller`'s
/// `for (i=0; offset<modbusLength_; i++) readPlcInt32(...)` array fan-out
/// (drvModbusAsyn.cpp:1840-1843). A malformed element decode aborts the poll.
fn decode_block_int32(dt: ModbusDataType, regs: &[u16]) -> AsynResult<Vec<i32>> {
    let rc = dt.register_count().max(1);
    let mut out = Vec::with_capacity(regs.len() / rc);
    let mut n = 0;
    while (n + 1) * rc <= regs.len() {
        out.push(
            datatype::read_int32(dt, &regs[n * rc..])
                .map_err(to_asyn)?
                .0,
        );
        n += 1;
    }
    Ok(out)
}

/// Float64 twin of [`decode_block_int32`] (C `readPlcFloat` fan-out,
/// drvModbusAsyn.cpp:1875-1878).
fn decode_block_float64(dt: ModbusDataType, regs: &[u16]) -> AsynResult<Vec<f64>> {
    let rc = dt.register_count().max(1);
    let mut out = Vec::with_capacity(regs.len() / rc);
    let mut n = 0;
    while (n + 1) * rc <= regs.len() {
        out.push(
            datatype::read_float(dt, &regs[n * rc..])
                .map_err(to_asyn)?
                .0,
        );
        n += 1;
    }
    Ok(out)
}

/// Whether a Modbus function may carry a record *array* write. C
/// `writeInt32Array`/`writeFloat64Array` (drvModbusAsyn.cpp:1398-1428 /
/// 1230-...) switch only on `MODBUS_WRITE_MULTIPLE_COILS` and
/// `MODBUS_WRITE_MULTIPLE_REGISTERS(_F23)`; every other function (the
/// single-value writes and all read functions) hits `default: asynError`.
fn is_array_write_function(function: ModbusFunctionCode) -> bool {
    matches!(
        function,
        ModbusFunctionCode::WriteMultipleCoils
            | ModbusFunctionCode::WriteMultipleRegisters
            | ModbusFunctionCode::WriteMultipleRegistersF23
    )
}

// ---------------------------------------------------------------------------
// Transport bridge
// ---------------------------------------------------------------------------

/// An [`OctetTransport`] backed by an `asyn-rs` octet port through blocking
/// sync I/O. Modbus framing is applied by [`ModbusEngine`] before the bytes
/// reach this layer, so the underlying port is a plain octet port (no asyn
/// interpose required).
pub struct SyncIoTransport {
    handle: SyncIOHandle,
    /// Slept before each frame write, mirroring C's pre-write
    /// `epicsThreadSleep(writeDelay)` (`modbusInterpose.c:246`); zero disables
    /// it. Needed by slow serial PLCs that require an inter-frame gap.
    write_delay: Duration,
}

impl SyncIoTransport {
    /// Wrap a sync-I/O handle to the underlying octet port, with no pre-write
    /// delay (the `modbusInterposeConfig writeDelayMsec` default).
    pub fn new(handle: SyncIOHandle) -> Self {
        Self::with_write_delay(handle, Duration::ZERO)
    }

    /// Wrap a sync-I/O handle with an explicit pre-write delay
    /// (C `modbusInterposeConfig writeDelayMsec`).
    pub fn with_write_delay(handle: SyncIOHandle, write_delay: Duration) -> Self {
        Self {
            handle,
            write_delay,
        }
    }
}

impl OctetTransport for SyncIoTransport {
    fn write_frame(&mut self, data: &[u8]) -> crate::error::ModbusResult<()> {
        // C sleeps before every write (modbusInterpose.c:246); the modbus
        // driver runs on a blocking sync-I/O thread (the same one that blocks
        // in `write_octet`/`read_octet`), so a blocking sleep here matches
        // `epicsThreadSleep` without stalling the async executor.
        if !self.write_delay.is_zero() {
            std::thread::sleep(self.write_delay);
        }
        self.handle
            .write_octet(0, data)
            .map(|_| ())
            .map_err(|e| ModbusError::Io(e.to_string()))
    }

    fn resend_frame(&mut self, data: &[u8]) -> crate::error::ModbusResult<()> {
        // UDP retransmit after a read failure: C resends via the raw octet
        // write (modbusInterpose.c:358), bypassing writeIt's pre-write
        // epicsThreadSleep(writeDelay) (:246). No pacing delay on a retransmit.
        self.handle
            .write_octet(0, data)
            .map(|_| ())
            .map_err(|e| ModbusError::Io(e.to_string()))
    }

    fn read_frame(&mut self, _timeout: Duration) -> crate::error::ModbusResult<Vec<u8>> {
        let buf = self
            .handle
            .read_octet(0, MAX_MODBUS_FRAME_SIZE)
            .map_err(|e| ModbusError::Io(e.to_string()))?;
        if buf.is_empty() {
            return Err(ModbusError::Timeout);
        }
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Port driver
// ---------------------------------------------------------------------------

/// The Modbus `asyn-rs` port driver — the `drvModbusAsyn` equivalent.
pub struct ModbusPortDriver {
    base: PortDriverBase,
    engine: ModbusEngine,
    transport: Box<dyn OctetTransport>,
    /// `reason` index → the data type it represents (`None` for non-data
    /// params such as the statistics counters).
    reason_to_datatype: Vec<Option<ModbusDataType>>,
    /// reason of the `MODBUS_READ` trigger parameter.
    read_reason: usize,
    /// reasons of the statistics parameters.
    read_ok_reason: usize,
    write_ok_reason: usize,
    io_errors_reason: usize,
    last_io_reason: usize,
    max_io_reason: usize,
    /// reasons of the histogram parameters.
    enable_histogram_reason: usize,
    read_histogram_reason: usize,
    histogram_bin_reason: usize,
    histogram_axis_reason: usize,
    /// reason of the POLL_DELAY control parameter.
    poll_delay_reason: usize,
    /// Live poll period in milliseconds, shared with the poller task so a
    /// runtime POLL_DELAY write retunes it (C `pollDelay_`).
    poll_delay: Arc<AtomicU64>,
    /// Wakes the poller when POLL_DELAY changes so the new period takes effect
    /// immediately, not after the current sleep (C `readPollerEventId_`).
    poll_wake: Arc<Notify>,
    /// Previous poll's register block — the baseline for the on-change gate
    /// (C `prevData`, drvModbusAsyn.cpp:1612/1934). Empty until the first poll.
    prev_data: Vec<u16>,
    /// Force the next successful poll's on-change callbacks regardless of
    /// whether the data changed (C `forceCallback_`, :331/1654). Set for the
    /// first cycle and after an I/O error; cleared at each cycle end (:1928).
    force_callback: bool,
    /// Status of the last poll's Modbus I/O (C `ioStatus_`, :1638). Every
    /// interrupt the poll cycle fires carries it as the callback's `auxStatus`
    /// (:1697/1738/1774/1810/1880/1915), which is what drives an I/O-Intr
    /// record to READ/INVALID while the PLC is unreachable and back to NO_ALARM
    /// when it returns.
    io_status: AsynStatus,
    /// Status of the *previous* poll (C `prevIOStatus`, :1602). The cycle
    /// compares against it to detect the two transitions C acts on: a status
    /// change forces the callbacks (:1654), and an unchanged *error* status is a
    /// persistent failure whose callbacks C skips entirely (:1648-1651).
    prev_io_status: AsynStatus,
    /// Set by a persistent-error cycle so the poller task applies C's 1.0 s
    /// error backoff before the next cycle (`epicsThreadSleep(1.0)`, :1649).
    /// The poller task is the sole reader and clears it by swapping.
    poll_backoff: Arc<AtomicBool>,
}

impl ModbusPortDriver {
    /// Build a Modbus port driver. `transport` carries framed bytes to the
    /// underlying octet port.
    pub fn new(
        port_name: &str,
        config: ModbusConfig,
        link_type: LinkType,
        transport: Box<dyn OctetTransport>,
    ) -> AsynResult<Self> {
        let engine = ModbusEngine::new(config, link_type).map_err(to_asyn)?;
        let flags = PortFlags {
            multi_device: true,
            can_block: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new(port_name, engine.config().length.max(1), flags);

        // One parameter per data type. The reason index recovers the type;
        // numeric types use a Float64 cache value (covers the int16/int32
        // ranges exactly), string types an Octet value.
        let mut reason_to_datatype: Vec<Option<ModbusDataType>> = Vec::new();
        for dt in ALL_DATA_TYPES {
            let ptype = if dt.is_string() {
                ParamType::Octet
            } else {
                ParamType::Float64
            };
            let idx = base.create_param(dt.as_str(), ptype)?;
            if reason_to_datatype.len() <= idx {
                reason_to_datatype.resize(idx + 1, None);
            }
            reason_to_datatype[idx] = Some(dt);
        }

        // MODBUS_DATA — an alias selecting the port's default data type.
        let default_dt = engine.config().data_type;
        let data_reason = base.create_param(
            PARAM_DATA,
            if default_dt.is_string() {
                ParamType::Octet
            } else {
                ParamType::Float64
            },
        )?;
        if reason_to_datatype.len() <= data_reason {
            reason_to_datatype.resize(data_reason + 1, None);
        }
        reason_to_datatype[data_reason] = Some(default_dt);

        // Control / statistics parameters.
        let read_reason = base.create_param(PARAM_READ, ParamType::Int32)?;
        let read_ok_reason = base.create_param(PARAM_READ_OK, ParamType::Int32)?;
        let write_ok_reason = base.create_param(PARAM_WRITE_OK, ParamType::Int32)?;
        let io_errors_reason = base.create_param(PARAM_IO_ERRORS, ParamType::Int32)?;
        let last_io_reason = base.create_param(PARAM_LAST_IO_TIME, ParamType::Int32)?;
        let max_io_reason = base.create_param(PARAM_MAX_IO_TIME, ParamType::Int32)?;
        let poll_delay_reason = base.create_param(PARAM_POLL_DELAY, ParamType::Float64)?;
        let enable_histogram_reason =
            base.create_param(PARAM_ENABLE_HISTOGRAM, ParamType::UInt32Digital)?;
        let read_histogram_reason =
            base.create_param(PARAM_READ_HISTOGRAM, ParamType::Int32Array)?;
        let histogram_bin_reason = base.create_param(PARAM_HISTOGRAM_BIN_TIME, ParamType::Int32)?;
        let histogram_axis_reason =
            base.create_param(PARAM_HISTOGRAM_TIME_AXIS, ParamType::Int32Array)?;

        // C `drvModbusAsyn` constructor (drvModbusAsyn.cpp:226-230) seeds the
        // statistics counters to 0 so a record reading them before the first
        // poll gets a defined value.
        for r in [
            read_ok_reason,
            write_ok_reason,
            io_errors_reason,
            last_io_reason,
            max_io_reason,
        ] {
            base.set_int32_param(r, 0, 0)?;
        }

        let poll_delay_ms = engine.config().poll_delay.as_millis() as u64;

        Ok(Self {
            base,
            engine,
            transport,
            reason_to_datatype,
            read_reason,
            read_ok_reason,
            write_ok_reason,
            io_errors_reason,
            last_io_reason,
            max_io_reason,
            enable_histogram_reason,
            read_histogram_reason,
            histogram_bin_reason,
            histogram_axis_reason,
            poll_delay_reason,
            poll_delay: Arc::new(AtomicU64::new(poll_delay_ms)),
            poll_wake: Arc::new(Notify::new()),
            prev_data: Vec::new(),
            // C sets forceCallback_ = true when the read-poller thread is
            // created (drvModbusAsyn.cpp:331), so the first cycle fires every
            // interface unconditionally.
            force_callback: true,
            // C `ioStatus_` is asynSuccess until the first poll, and
            // `prevIOStatus` is initialised to asynSuccess at readPoller entry
            // (drvModbusAsyn.cpp:1602) — so the first failing poll is an error
            // *transition* and fires the error callbacks.
            io_status: AsynStatus::Success,
            prev_io_status: AsynStatus::Success,
            poll_backoff: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The data type bound to a reason, or an error if the reason is not a
    /// data parameter.
    fn datatype_of(&self, reason: usize) -> AsynResult<ModbusDataType> {
        self.reason_to_datatype
            .get(reason)
            .copied()
            .flatten()
            .ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("reason {reason} is not a Modbus data parameter"),
            })
    }

    /// Modbus address for `offset`, relative to the configured start address.
    fn modbus_address(&self, offset: i32) -> u16 {
        (self.engine.config().start_address.max(0) + offset) as u16
    }

    /// Whether the port is configured for absolute addressing.
    fn is_absolute(&self) -> bool {
        self.engine.config().absolute_addressing()
    }

    /// The Modbus function an absolute-mode *read* must issue. Port of
    /// `checkModbusFunction` (drvModbusAsyn.cpp:2406-2412): a read port uses
    /// its own function; a write port uses its `readOnceFunction`. A write
    /// port with no poll delay has no defined readback function — C returns
    /// `asynError`, mirrored here.
    fn absolute_read_function(&self) -> AsynResult<ModbusFunctionCode> {
        let cfg = self.engine.config();
        if cfg.function.is_read() {
            return Ok(cfg.function);
        }
        if cfg.poll_delay.is_zero() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "absolute-mode readback needs a non-zero poll delay".into(),
            });
        }
        cfg.function
            .readonce_function()
            .ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "Modbus function {:?} has no readback function",
                    cfg.function
                ),
            })
    }

    /// Absolute-mode per-record read: issue one Modbus request at the wire
    /// address `addr` and return `count` words. Port of the
    /// `absoluteAddressing_` read branch shared by every read accessor.
    fn read_absolute_words(&mut self, addr: i32, count: usize) -> AsynResult<Vec<u16>> {
        let function = self.absolute_read_function()?;
        let result = self
            .engine
            .read_absolute(self.transport.as_mut(), function, addr, count);
        // C `doModbusIO` setIntegerParams the statistics counters inline on
        // every I/O — success OR failure — before it returns
        // (drvModbusAsyn.cpp:2206/2214/2255/2279/2301). The relative-mode
        // poller publishes them via `poll_cycle`, but an absolute-addressing
        // port has no poller (`poll_cycle` early-returns), so the per-record
        // read is the only place an I/O happens. Publish here too, or the
        // statistics records read 0 forever (R50).
        self.publish_stats()?;
        result.map_err(to_asyn)
    }

    /// Absolute-mode per-record write: issue one Modbus request carrying
    /// `regs` at the wire address `addr` with the configured write function.
    fn write_absolute_regs(&mut self, addr: i32, regs: &[u16]) -> AsynResult<()> {
        let function = self.engine.config().function;
        let result = self
            .engine
            .write_absolute(self.transport.as_mut(), function, addr, regs);
        // Publish the statistics counters after the I/O, same as the read
        // path — C `doModbusIO` setIntegerParams them inline on every write
        // too (drvModbusAsyn.cpp:2326/2334/2341), and the absolute-mode write
        // has no poller to publish them otherwise (R50).
        self.publish_stats()?;
        result.map_err(to_asyn)
    }

    /// One acquisition cycle: read all registers, refresh every touched
    /// record's parameter and fire its I/O Intr callbacks, then publish the
    /// statistics counters. Port of the data half of `readPoller`.
    fn poll_cycle(&mut self) -> AsynResult<()> {
        // Absolute addressing has no periodic poller (drvModbusAsyn.cpp:1121):
        // each record reads its own wire address on access. Nothing to do.
        if self.is_absolute() {
            return Ok(());
        }
        if !self.engine.config().function.is_read() {
            return Ok(());
        }

        // The acquisition's I/O status is *port state*, not the outcome of this
        // request: C `readPoller` stores it in `ioStatus_` (drvModbusAsyn.cpp:
        // 1638) and keeps polling forever — a failed read never ends the poller
        // and never suppresses the record callbacks. The failure reaches the
        // records as the `auxStatus` of the interrupt callbacks below, which is
        // what drives them to READ/INVALID; propagating it as an `Err` here
        // instead would abort the cycle before a single record learned about it
        // (and, in the old poller task, kill the poller for good).
        self.io_status = match self.engine.poll(self.transport.as_mut()) {
            Ok(_) => AsynStatus::Success,
            // `AsynError::status()` is the single owner of "which asynStatus is
            // this?" — it reads a queue refusal (a disabled/disconnected port
            // turning the request down) as the `asynDisabled`/`asynDisconnected`
            // it is, where matching `AsynError::Status` by hand flattened both to
            // `asynError` and told every record the wrong reason.
            Err(e) => to_asyn(e).status(),
        };

        // C `doModbusIO` moves the statistics counters on every I/O — success or
        // failure (`IOErrors_` at :2206, the timings at :2214-2217, `readOK_` at
        // :2255) — so they must be published on a failed cycle too; the old
        // early-`?` return left IO_ERRORS reading 0 through an outage.
        self.publish_stats()?;
        // The statistics/control params are set at asyn addr 0 (their
        // `statistics.template` records bind `@asyn($(PORT) 0)`) and still flow
        // through the param-cache callback path — they are single-interface
        // control values, not per-interface data points. Flush addr 0 every
        // cycle so the statistics monitors post.
        self.base.call_param_callbacks(0)?;

        // A *persistent* error — this poll failed and so did the previous one —
        // is the one case C skips the callbacks for: it sleeps 1.0 s and starts
        // the next cycle (drvModbusAsyn.cpp:1646-1651), leaving `forceCallback_`,
        // `prevIOStatus` and `prevData` exactly as the transition cycle left
        // them. The records already carry the error status from that transition;
        // re-firing it every second would be pure churn. Hand the backoff to the
        // poller task, the owner of this port's pacing.
        if self.io_status != AsynStatus::Success && self.io_status == self.prev_io_status {
            self.poll_backoff.store(true, Ordering::Relaxed);
            return Ok(());
        }

        // An I/O-status *transition* (good→bad or bad→good) forces the callbacks
        // regardless of whether the data changed (C :1654) — on the way down so
        // every record alarms, on the way up so every record recovers with the
        // fresh value.
        if self.io_status != self.prev_io_status {
            self.force_callback = true;
        }

        // Single finalizer for the abort => recover invariant. An error after
        // the poll's I/O — a mid-loop decode failure — aborts the cycle WITHOUT
        // advancing the on-change baseline, so the next cycle must be forced.
        // `run_poll_cycle` clears force_callback and advances prev_data /
        // prev_io_status only when the cycle fully completes (C :1928-1934);
        // every Err path re-arms the force here, in one place, so no mid-loop `?`
        // can leave the baseline frozen with the force cleared.
        match self.run_poll_cycle() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.force_callback = true;
                Err(e)
            }
        }
    }

    /// The fallible body of one poll cycle: fire every bound interface with the
    /// freshly-polled block (on-change gated per C `readPoller`, each callback
    /// carrying the poll's `io_status` as its `auxStatus`) and — only on full
    /// success — advance the on-change baseline (`prev_data` / `prev_io_status`)
    /// and clear the one-shot `force_callback`. Any `Err` leaves all three
    /// untouched so [`poll_cycle`](Self::poll_cycle) re-arms recovery.
    ///
    /// The Modbus read itself happened in [`poll_cycle`](Self::poll_cycle); on a
    /// failed read the engine's register block still holds the last good data and
    /// C fires it unchanged, with the failing status attached (:1638-1697).
    fn run_poll_cycle(&mut self) -> AsynResult<()> {
        let io_status = self.io_status;

        // On-change gate, mirroring C `readPoller`. int32/int64/float64 and
        // float64Array fire every cycle (ADC averaging, drvModbusAsyn.cpp:
        // 1714/1858); uInt32Digital (:1700, per-offset masked change),
        // int32Array (:1824) and octet (:1893) fire only on `forceCallback_ ||
        // anyChanged`. `anyChanged` is the port-wide block compare (memcmp
        // data_ vs prevData, :1658); `force` covers the first cycle and
        // post-I/O-error recovery (:331/1654).
        let force = self.force_callback;
        let any_changed = self.prev_data.as_slice() != self.engine.data();
        let port_gate = force || any_changed;

        // Fire every record on the interrupt list, exactly like C `readPoller`
        // (drvModbusAsyn.cpp:1600-1928), which walks the per-interface interrupt
        // lists populated at `registerInterruptUser` — independent of whether
        // the record was ever read. The interrupt registry is the single owner
        // of "which records want a fire"; the driver keeps no parallel set
        // seeded by reads. A read-seeded `active` set left every `SCAN="I/O
        // Intr"` record dead, since such a record never reads on its own — its
        // first (and every) value must come from the poller's fire.
        for (reason, addr) in self.base.interrupts.subscribed_bindings() {
            let Ok(dt) = self.datatype_of(reason) else {
                continue;
            };
            // Defense-in-depth: a subscriber may bind an out-of-range `addr`
            // (the registry does not range-check offsets); it must never index
            // the engine buffer. A bad addr here is skipped, not allowed to
            // panic.
            let data = self.engine.data();
            if addr < 0 || addr as usize >= data.len() {
                continue;
            }
            let regs = &data[addr as usize..];
            if dt.is_string() {
                // C gates the octet interrupt list on `forceCallback_ ||
                // anyChanged` (port-wide change, drvModbusAsyn.cpp:1893) and
                // skips even the `readPlcString` when nothing changed. asynOctet
                // records are the sole subscribers for a string offset
                // (:1894-1921), so a single typed fire suffices.
                if port_gate {
                    let (bytes, _) =
                        datatype::read_string(dt, regs, regs.len() * 2).map_err(to_asyn)?;
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    self.base.notify_interface_value(
                        reason,
                        addr,
                        InterfaceType::Octet,
                        ParamValue::Octet(s),
                        0,
                        io_status,
                    );
                }
            } else {
                // C `readPoller` decodes the SAME register block separately per
                // scalar interface and fires each interface's own interrupt list
                // (drvModbusAsyn.cpp:1736 int32 / 1772 int64 / 1808 float64 / 1706
                // uInt32Digital). One Modbus offset can be bound at once by an
                // asynInt32 ai (ESLO convert), an asynInt64 int64in, an
                // asynFloat64 ai (ASLO/SMOO), and an asynUInt32Digital bi (mask),
                // each needing the value in its own type — collapsing to one
                // Float64 (the old path) delivered a wrong-typed value to all but
                // the asynFloat64 record. Decode all up front so a mid-decode
                // error aborts the poll before any partial fire; the interrupt
                // iface filter routes each tagged value to only its interface's
                // records.
                let i32v = datatype::read_int32(dt, regs).map_err(to_asyn)?.0;
                let i64v = datatype::read_int64(dt, regs).map_err(to_asyn)?.0;
                let f64v = datatype::read_float(dt, regs).map_err(to_asyn)?.0;
                // Raw register word for UInt32Digital; the record applies its own
                // @asynMask via `apply_raw_readback`, matching the polled
                // `read_uint32_digital` which delivers the unmasked word.
                let word = regs.first().copied().unwrap_or(0) as u32;
                // Array interfaces: C `readPoller` decodes the SAME register
                // block from the record's offset to `modbusLength_` and fires the
                // array interrupt lists — int32Array (drvModbusAsyn.cpp:1840-1851)
                // and float64Array (:1875-1886) — so an asynInt32ArrayIn /
                // asynFloat64ArrayIn waveform on `SCAN="I/O Intr"` updates every
                // frame. Decode the whole block once per array interface a record
                // is bound to; the subscriber-presence gate skips the decode when
                // no array record exists, mirroring C iterating an empty interrupt
                // list (the per-element `readPlcInt32`/`readPlcFloat` loop never
                // runs). The waveform consumer caps the array at its NELM. Decode
                // up front with the scalars so a mid-decode error aborts the poll
                // before any partial fire.
                //
                // int32Array fires on `forceCallback_ || anyChanged` (port-wide
                // change, drvModbusAsyn.cpp:1824); float64Array fires every cycle
                // (ADC averaging, :1857). The `has_subscriber` gate skips the
                // whole-block decode when no array record is bound, mirroring C
                // iterating an empty interrupt list (the per-element
                // `readPlcInt32`/`readPlcFloat` loop never runs).
                let int32_array = if port_gate
                    && self
                        .base
                        .interrupts
                        .has_subscriber(reason, addr, InterfaceType::Int32Array)
                {
                    Some(decode_block_int32(dt, regs)?)
                } else {
                    None
                };
                let float64_array = if self.base.interrupts.has_subscriber(
                    reason,
                    addr,
                    InterfaceType::Float64Array,
                ) {
                    Some(decode_block_float64(dt, regs)?)
                } else {
                    None
                };
                self.base.notify_interface_value(
                    reason,
                    addr,
                    InterfaceType::Int32,
                    ParamValue::Int32(i32v),
                    0,
                    io_status,
                );
                self.base.notify_interface_value(
                    reason,
                    addr,
                    InterfaceType::Int64,
                    ParamValue::Int64(i64v),
                    0,
                    io_status,
                );
                self.base.notify_interface_value(
                    reason,
                    addr,
                    InterfaceType::Float64,
                    ParamValue::Float64(f64v),
                    0,
                    io_status,
                );
                // C fires uInt32Digital only on a per-offset masked change
                // (`forceCallback_ || (newValue & mask != prevValue & mask)`,
                // drvModbusAsyn.cpp:1695-1707). The asyn interrupt filter applies
                // the same gate: a subscriber with `@asynMask` M passes iff
                // `uint32_changed_mask & M != 0` (interrupt.rs `matches`;
                // asynPortDriver.cpp:720). So pass the actually-changed bits
                // `word ^ prev_word` as the changed mask — equivalent to C's
                // per-subscriber `(new ^ prev) & mask` test — and skip the fire
                // entirely when the offset's word is unchanged. `force` (first
                // cycle / post-I/O-error) passes `!0` so every subscriber fires
                // regardless, matching C `forceCallback_`.
                let prev_word = self.prev_data.get(addr as usize).copied().unwrap_or(0) as u32;
                let changed_bits = word ^ prev_word;
                if force || changed_bits != 0 {
                    let changed_mask = if force { !0 } else { changed_bits };
                    self.base.notify_interface_value(
                        reason,
                        addr,
                        InterfaceType::UInt32Digital,
                        ParamValue::UInt32Digital(word),
                        changed_mask,
                        io_status,
                    );
                }
                if let Some(arr) = int32_array {
                    self.base.notify_interface_value(
                        reason,
                        addr,
                        InterfaceType::Int32Array,
                        ParamValue::Int32Array(arr.into()),
                        0,
                        io_status,
                    );
                }
                if let Some(arr) = float64_array {
                    self.base.notify_interface_value(
                        reason,
                        addr,
                        InterfaceType::Float64Array,
                        ParamValue::Float64Array(arr.into()),
                        0,
                        io_status,
                    );
                }
            }
        }

        // Cycle fully completed: advance the on-change baseline, latch the I/O
        // status this cycle's callbacks carried, and clear the one-shot force
        // flag (C drvModbusAsyn.cpp:1928-1934). This is the LAST statement,
        // reached only on full success — any earlier `?` (a per-offset decode)
        // leaves `prev_data`, `prev_io_status` and `force_callback` untouched, so
        // `poll_cycle` re-arms the force and the next clean cycle recovers
        // instead of freezing the baseline.
        let snapshot: Vec<u16> = self.engine.data().to_vec();
        self.prev_data = snapshot;
        self.prev_io_status = io_status;
        self.force_callback = false;
        Ok(())
    }

    /// Copy the engine's I/O statistics into their parameters.
    fn publish_stats(&mut self) -> AsynResult<()> {
        let s = &self.engine.stats;
        let (read_ok, write_ok, io_errors, last, max) = (
            s.read_ok as i32,
            s.write_ok as i32,
            s.io_errors as i32,
            s.last_io_msec as i32,
            s.max_io_msec as i32,
        );
        self.base.set_int32_param(self.read_ok_reason, 0, read_ok)?;
        self.base
            .set_int32_param(self.write_ok_reason, 0, write_ok)?;
        self.base
            .set_int32_param(self.io_errors_reason, 0, io_errors)?;
        self.base.set_int32_param(self.last_io_reason, 0, last)?;
        self.base.set_int32_param(self.max_io_reason, 0, max)?;
        Ok(())
    }

    /// Flush a freshly-converted set of registers to the slave with the
    /// configured write function.
    ///
    /// The register cache (`engine.data_`) is NOT touched. C's scalar writes
    /// — `writeInt32` (`drvModbusAsyn.cpp:760-776`), `writeInt64` (`:920-936`),
    /// `writeFloat64`, `writeUInt32Digital` (`:596-617`) — convert the value
    /// into a *local* `epicsUInt16 buffer[4]` / `epicsUInt16 data` and send
    /// that; `data_` keeps whatever the poller (or the init read-once) last
    /// put there. Only the array/string writes transmit *from* `data_`, and
    /// they use [`Self::flush_write_staged`].
    fn flush_write(&mut self, offset: i32, regs: &[u16]) -> AsynResult<()> {
        let function = self.engine.config().function;
        // C parity (drvModbusAsyn.cpp:760-767 / 920-927 / writeFloat64): a
        // value that spans more than one register written through a
        // WRITE_SINGLE_REGISTER port is sent as one single-register request
        // per register at consecutive addresses — the FC06 PDU carries exactly
        // one register, so C loops `for (i=0; i<bufferLen; i++) doModbusIO(...,
        // modbusAddress+i, buffer+i, ...)`. Every other write function carries
        // the whole block in one request. Without this loop a 32/64-bit value
        // on an FC06 port would silently drop all registers past the first.
        let per_register = function == ModbusFunctionCode::WriteSingleRegister && regs.len() > 1;
        if self.is_absolute() {
            if per_register {
                for (i, &r) in regs.iter().enumerate() {
                    self.write_absolute_regs(offset + i as i32, &[r])?;
                }
            } else {
                self.write_absolute_regs(offset, regs)?;
            }
            return Ok(());
        }
        let addr = self.modbus_address(offset);
        if per_register {
            for (i, &r) in regs.iter().enumerate() {
                self.engine
                    .do_modbus_io(
                        self.transport.as_mut(),
                        function,
                        addr.wrapping_add(i as u16),
                        &[r],
                        1,
                    )
                    .map_err(to_asyn)?;
            }
        } else {
            self.engine
                .do_modbus_io(self.transport.as_mut(), function, addr, regs, regs.len())
                .map_err(to_asyn)?;
        }
        Ok(())
    }

    /// Flush registers that C transmits *from* the register cache.
    ///
    /// C's array and string writes — `writeInt32Array` (`drvModbusAsyn.cpp:
    /// 1402`), `writeFloat64Array` (`:1232`), `writeOctet` (`:1537-1554`) —
    /// set `dataAddress = data_ + offset` and convert the record's elements
    /// straight INTO `data_`, so the cache carries the written values and a
    /// cached read on the port sees them. The conversion happens before the
    /// I/O, so a failed request still leaves the staged values behind.
    ///
    /// Absolute mode stages nothing: C aliases `data_` as a scratch transmit
    /// buffer there (`dataAddress = data_; outIndex = 0;`, `:1219-1221`), and
    /// the port's reads go to the wire rather than to the cache, so the staged
    /// words are never observable.
    fn flush_write_staged(&mut self, offset: i32, regs: &[u16]) -> AsynResult<()> {
        if !self.is_absolute() {
            let buf = self.engine.data_mut();
            for (i, &r) in regs.iter().enumerate() {
                if let Some(slot) = buf.get_mut(offset as usize + i) {
                    *slot = r;
                }
            }
        }
        self.flush_write(offset, regs)
    }

    /// After a successful write, stage the value into the parameter cache and
    /// fan its monitor out.
    ///
    /// Relative mode only: the parameter library is sized to `config.length`
    /// addresses and feeds the poller's I/O-Intr callbacks. In absolute mode
    /// the asyn `addr` is a Modbus wire address (up to 65535) unrelated to the
    /// parameter table, and there is no poller fan-out — C `writeInt32` in
    /// absolute mode likewise does no parameter callback. So this is a no-op
    /// in absolute mode.
    fn cache_write_numeric(&mut self, user: &AsynUser, value: f64) -> AsynResult<()> {
        if self.is_absolute() {
            return Ok(());
        }
        self.base.set_float64_param(user.reason, user.addr, value)?;
        self.base.call_param_callbacks(user.addr)
    }
}

/// Parse a drvUser `=N` string-length suffix the way C does
/// (drvModbusAsyn.cpp:398-404): `strtol(suffix, &endptr, 0)` then reject if
/// `endptr[0] != '\0'` (trailing junk) or the value is negative. Base 0 means
/// `0x`/`0X` → hex, a leading `0` → octal, otherwise decimal. An empty suffix
/// (`TYPE=`) parses as 0, which C accepts. Returns the parsed non-negative
/// length, or `None` if C would reject the suffix. The caller stashes the
/// length as the per-record octet cap (C `drvUser->len`).
fn parse_drvuser_string_len(suffix: &str) -> Option<i64> {
    // C `strtol` skips leading whitespace and honours a leading sign; a real
    // drvInfo carries neither, but mirror the accept set so parity holds.
    let s = suffix.trim_start_matches([' ', '\t']);
    if s.is_empty() {
        // `strtol("")` returns 0 with `endptr` at the terminator → C accepts.
        return Some(0);
    }
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        i64::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse::<i64>().ok()?
    };
    let value = if negative { -value } else { value };
    (value >= 0).then_some(value)
}

impl PortDriver for ModbusPortDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn drv_user_create(&mut self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        // The modbus drvInfo names the data type (`INT16`, `FLOAT32LE`, …), so
        // the parameter type comes from the drvInfo, not from the bound record's
        // interface (`req.iface`): a record whose DTYP disagrees with the
        // configured data type is a configuration error, not a retype request.
        let (drv_info, addr) = (req.drv_info.as_str(), req.addr);
        // C `drvUserCreate` (drvModbusAsyn.cpp:368-433) strips everything after
        // the first '=' to resolve the data-type name, then validates the
        // optional `=N` string-length suffix: it is legal ONLY for the eight
        // string types (`strtol` base 0, non-negative); a non-string type with
        // a suffix, or a garbage/negative length, is rejected with `asynError`
        // (:399-412). A valid length is stashed as the per-record octet cap
        // (C `modbusDrvUser_t.len`); `getStringLen` later caps the asyn octet
        // `maxLen` to it (:2367-2377), which the binding applies to its octet
        // buffer length at init.
        let mut parts = drv_info.splitn(2, '=');
        let base_info = parts.next().unwrap_or(drv_info).trim();
        let suffix = parts.next();
        let reason = self
            .base
            .find_param(base_info)
            .ok_or_else(|| AsynError::ParamNotFound(drv_info.to_string()))?;
        let datatype = self.reason_to_datatype.get(reason).copied().flatten();
        if datatype.is_some() {
            // C runs `getAddr` + `checkOffset` ONLY inside the data-type-match
            // branch (:378-384), before the suffix switch; a non-data drvInfo
            // (statistics/control) falls through to the base class with no offset
            // check. Reject an out-of-range offset here at bind so the record
            // fails init instead of binding and alarming on every I/O.
            self.engine.check_offset(addr).map_err(to_asyn)?;
        }
        let mut max_octet_len = None;
        if let Some(suffix) = suffix {
            let is_string = datatype.is_some_and(|dt| dt.is_string());
            if !is_string {
                // C: the `=` length suffix is invalid for a non-string type.
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("invalid drvUser (length suffix on non-string): {drv_info}"),
                });
            }
            match parse_drvuser_string_len(suffix) {
                // C stores the parsed length in `drvUser->len`; it is `>= 0`
                // here (the parser rejects negatives), so the cap is exact.
                Some(len) => max_octet_len = Some(len as usize),
                None => {
                    // C: `strtol` base 0 with the `endptr[0] != '\0' || len < 0`
                    // guard rejects garbage or a negative length.
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("invalid string length: {suffix}"),
                    });
                }
            }
        }
        Ok(DrvUserInfo {
            reason,
            max_octet_len,
        })
    }

    fn connect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        // C `connect` (drvModbusAsyn.cpp:455-467) validates the register offset
        // (the asyn `addr`) against the addressing-mode bounds and returns
        // `asynError` for an out-of-range offset, so a misconfigured record
        // fails to connect instead of connecting and alarming on every I/O
        // (R52). modbus is a multi-device port, so `connect_addr` is the
        // per-`addr` connect the framework drives; reject before marking the
        // address connected. The bounds are the same `check_offset` enforces
        // per I/O (absolute `0..=0xFFFF`, relative `0..length`).
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        self.base.connect_addr(user.addr);
        Ok(())
    }

    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        // C `drvModbusAsyn::readInt32` (drvModbusAsyn.cpp:653-723): only the
        // `P_Data` reason runs the Modbus decode path; every other reason
        // (statistics/control params) delegates to `asynPortDriver::readInt32`,
        // returning the cached parameter-library value.
        let Some(dt) = self.reason_to_datatype.get(user.reason).copied().flatten() else {
            return self.base.get_int32_param(user.reason, user.addr);
        };
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        // Absolute mode: read this record's own wire address now (the C
        // `absoluteAddressing_` branch issues an individual `doModbusIO`).
        // C `readInt32` (drvModbusAsyn.cpp:675-676) uses a FIXED request
        // length of `min(2, modbusLength_)` registers, not the data-type
        // width. `read_absolute_words` clamps to `config.length`.
        if self.is_absolute() {
            let regs = self.read_absolute_words(user.addr, 2)?;
            return Ok(datatype::read_int32(dt, &regs).map_err(to_asyn)?.0);
        }
        let regs = &self.engine.data()[user.addr as usize..];
        Ok(datatype::read_int32(dt, regs).map_err(to_asyn)?.0)
    }

    fn read_int64(&mut self, user: &AsynUser) -> AsynResult<i64> {
        // Non-data reason → cached parameter value (see `read_int32`).
        let Some(dt) = self.reason_to_datatype.get(user.reason).copied().flatten() else {
            return self.base.get_int64_param(user.reason, user.addr);
        };
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        // C `readInt64` (drvModbusAsyn.cpp:836-837) uses a FIXED request
        // length of `min(4, modbusLength_)` registers.
        if self.is_absolute() {
            let regs = self.read_absolute_words(user.addr, 4)?;
            return Ok(datatype::read_int64(dt, &regs).map_err(to_asyn)?.0);
        }
        let regs = &self.engine.data()[user.addr as usize..];
        Ok(datatype::read_int64(dt, regs).map_err(to_asyn)?.0)
    }

    fn read_float64(&mut self, user: &AsynUser) -> AsynResult<f64> {
        // Non-data reason → cached parameter value (see `read_int32`).
        let Some(dt) = self.reason_to_datatype.get(user.reason).copied().flatten() else {
            return self.base.get_float64_param(user.reason, user.addr);
        };
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        // C `readFloat64` (drvModbusAsyn.cpp:982-983) uses a FIXED request
        // length of `min(4, modbusLength_)` registers.
        if self.is_absolute() {
            let regs = self.read_absolute_words(user.addr, 4)?;
            return Ok(datatype::read_float(dt, &regs).map_err(to_asyn)?.0);
        }
        let regs = &self.engine.data()[user.addr as usize..];
        Ok(datatype::read_float(dt, regs).map_err(to_asyn)?.0)
    }

    fn read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32> {
        // Non-data reason (e.g. ENABLE_HISTOGRAM) → cached parameter value;
        // see `read_int32` for the C parity reference.
        if self
            .reason_to_datatype
            .get(user.reason)
            .copied()
            .flatten()
            .is_none()
        {
            let raw = self.base.get_uint32_param(user.reason, user.addr)?;
            return Ok(if mask == 0 { raw } else { raw & mask });
        }
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        if self.is_absolute() {
            // C `readUInt32Digital` reads `min(1, modbusLength_)` words.
            let regs = self.read_absolute_words(user.addr, 1)?;
            let raw = regs.first().copied().unwrap_or(0) as u32;
            return Ok(if mask == 0 { raw } else { raw & mask });
        }
        let raw = self.engine.data()[user.addr as usize] as u32;
        Ok(if mask == 0 { raw } else { raw & mask })
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        if self.is_absolute() {
            // C `readOctet` (drvModbusAsyn.cpp:1464-1465) issues
            // `doModbusIO(..., min((int)maxChars, modbusLength_))`: the request
            // length in REGISTERS equals the string CHAR count, because
            // `readPlcString` (drvModbusAsyn.cpp:3001-3052) advances `offset`
            // by exactly one register per loop iteration regardless of
            // encoding. The single-byte encodings (`StringHigh`/`StringLow`/
            // `ZStringHigh`/`ZStringLow`) therefore need one register per char,
            // so the word count must be `buf.len()` (the char count) — not
            // `div_ceil(2)`, which would under-read them by half. The two-byte
            // encodings are over-read harmlessly, exactly as C over-reads them.
            // `read_absolute_words` clamps the count to `config.length`,
            // matching C's `min(maxChars, modbusLength_)`.
            let words = buf.len().max(1);
            let regs = self.read_absolute_words(user.addr, words)?;
            let (bytes, _) = datatype::read_string(dt, &regs, buf.len()).map_err(to_asyn)?;
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            return Ok(n);
        }
        let regs = &self.engine.data()[user.addr as usize..];
        // String length comes from the record buffer (`NELM`).
        let (bytes, _) = datatype::read_string(dt, regs, buf.len()).map_err(to_asyn)?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        // C `readOctet` sets `*eomReason = ASYN_EOM_CNT` on every successful
        // P_Data read (drvModbusAsyn.cpp:1480), and the poller octet callback
        // passes `ASYN_EOM_CNT` too (:1921): a register-snapshot read is always
        // a complete logical message, never a stream fragment. The generic
        // `PortDriver::io_read_octet_eom` only synthesises `CNT` when the buffer
        // fills, so a modbus string shorter than the record buffer would lose
        // the flag. Override to flag every successful read complete.
        let n = self.read_octet(user, buf)?;
        Ok((n, EomReason::CNT))
    }

    fn read_int32_array(&mut self, user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
        // Histogram parameters serve fixed-length diagnostic arrays.
        if user.reason == self.read_histogram_reason {
            let h = &self.engine.stats.histogram;
            let n = h.len().min(buf.len());
            for (slot, &v) in buf[..n].iter_mut().zip(h) {
                *slot = v as i32;
            }
            return Ok(n);
        }
        if user.reason == self.histogram_axis_reason {
            let bin = self.engine.stats.histogram_ms_per_bin.max(1) as i32;
            let n = crate::driver::HISTOGRAM_LENGTH.min(buf.len());
            for (i, slot) in buf[..n].iter_mut().enumerate() {
                *slot = i as i32 * bin;
            }
            return Ok(n);
        }
        // Otherwise a Modbus data array — one element per `register_count`.
        let dt = self.datatype_of(user.reason)?;
        let rc = dt.register_count().max(1);
        // Absolute mode: C `readInt32Array` (drvModbusAsyn.cpp:1294-1295)
        // issues `doModbusIO(..., std::min((int)maxChans, modbusLength_))` —
        // it requests the smaller of the record's array length (`maxChans`,
        // i.e. `buf.len()`) and `modbusLength_` (`config.length`). It then
        // decodes elements from offset 0. (`readFloat64Array`,
        // drvModbusAsyn.cpp:1125-1126, uses the bare `modbusLength_` instead,
        // so `read_float64_array` differs deliberately.)
        if self.is_absolute() {
            self.engine.check_offset(user.addr).map_err(to_asyn)?;
            let count = buf.len().min(self.engine.config().length);
            let words = self.read_absolute_words(user.addr, count)?;
            let mut n = 0;
            while n < buf.len() && (n + 1) * rc <= words.len() {
                buf[n] = datatype::read_int32(dt, &words[n * rc..])
                    .map_err(to_asyn)?
                    .0;
                n += 1;
            }
            return Ok(n);
        }
        let data = self.engine.data();
        let mut n = 0;
        while n < buf.len() && (user.addr as usize + (n + 1) * rc) <= data.len() {
            let regs = &data[user.addr as usize + n * rc..];
            buf[n] = datatype::read_int32(dt, regs).map_err(to_asyn)?.0;
            n += 1;
        }
        Ok(n)
    }

    fn read_float64_array(&mut self, user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        // C `readFloat64Array` serves the diagnostic histogram arrays too
        // (drvModbusAsyn.cpp:1181-1191), exactly like `readInt32Array`
        // (:1350-1360) — an aai/waveform with FTVL=DOUBLE binding to
        // READ_HISTOGRAM / HISTOGRAM_TIME_AXIS must work, not just the LONG one.
        if user.reason == self.read_histogram_reason {
            let h = &self.engine.stats.histogram;
            let n = h.len().min(buf.len());
            for (slot, &v) in buf[..n].iter_mut().zip(h) {
                *slot = v as f64;
            }
            return Ok(n);
        }
        if user.reason == self.histogram_axis_reason {
            let bin = self.engine.stats.histogram_ms_per_bin.max(1) as f64;
            let n = crate::driver::HISTOGRAM_LENGTH.min(buf.len());
            for (i, slot) in buf[..n].iter_mut().enumerate() {
                *slot = i as f64 * bin;
            }
            return Ok(n);
        }
        let dt = self.datatype_of(user.reason)?;
        let rc = dt.register_count().max(1);
        // Absolute mode: per-record read at the wire address (see
        // `read_int32_array`).
        if self.is_absolute() {
            self.engine.check_offset(user.addr).map_err(to_asyn)?;
            let words = self.read_absolute_words(user.addr, self.engine.config().length)?;
            let mut n = 0;
            while n < buf.len() && (n + 1) * rc <= words.len() {
                buf[n] = datatype::read_float(dt, &words[n * rc..])
                    .map_err(to_asyn)?
                    .0;
                n += 1;
            }
            return Ok(n);
        }
        let data = self.engine.data();
        let mut n = 0;
        while n < buf.len() && (user.addr as usize + (n + 1) * rc) <= data.len() {
            let regs = &data[user.addr as usize + n * rc..];
            buf[n] = datatype::read_float(dt, regs).map_err(to_asyn)?.0;
            n += 1;
        }
        Ok(n)
    }

    fn write_int32_array(&mut self, user: &AsynUser, data: &[i32]) -> AsynResult<()> {
        let dt = self.datatype_of(user.reason)?;
        // C `writeInt32Array` accepts only write-multiple functions; any other
        // function (single-value writes, read functions) returns asynError
        // (drvModbusAsyn.cpp:1422-1427). Reject it here rather than emit a
        // per-register fan-out the C driver never performs for arrays.
        let function = self.engine.config().function;
        if !is_array_write_function(function) {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "Modbus function {function:?} cannot write an array; \
                     configure a write-multiple function"
                ),
            });
        }
        // Reject a negative or out-of-range offset before it is cast to
        // `usize` in `flush_write` and wraps to a bogus wire address.
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let absolute = self.is_absolute();
        let limit = self.engine.config().length;
        let rc = dt.register_count().max(1);
        let mut regs = Vec::new();
        // C `writeInt32Array` (drvModbusAsyn.cpp:1373-1417) converts inside
        // `for (i=0; i<maxChans && outIndex<modbusLength_; i++)`. The loop
        // guard `outIndex < modbusLength_` is tested at element start, so an
        // element is emitted whole (full `register_count` registers) only
        // while `outIndex < modbusLength_`; once `outIndex` reaches
        // `modbusLength_` the conversion stops, truncating the record array
        // on the wire. `outIndex` is initialized to `0` in absolute mode and
        // to the record's register `offset` in relative mode
        // (drvModbusAsyn.cpp:1388-1396) — so the clamp applies in BOTH modes.
        let mut out_index = if absolute { 0usize } else { user.addr as usize };
        for &v in data {
            if out_index >= limit {
                break;
            }
            regs.extend(datatype::write_int32(dt, v).map_err(to_asyn)?);
            out_index += rc;
        }
        self.flush_write_staged(user.addr, &regs)
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
        let dt = self.datatype_of(user.reason)?;
        // C `writeFloat64Array` accepts only write-multiple functions; any
        // other function returns asynError (drvModbusAsyn.cpp:1252-... default).
        let function = self.engine.config().function;
        if !is_array_write_function(function) {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "Modbus function {function:?} cannot write an array; \
                     configure a write-multiple function"
                ),
            });
        }
        // Reject a negative or out-of-range offset before it is cast to
        // `usize` in `flush_write` and wraps to a bogus wire address.
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let absolute = self.is_absolute();
        let limit = self.engine.config().length;
        let rc = dt.register_count().max(1);
        let mut regs = Vec::new();
        // C `writeFloat64Array` (drvModbusAsyn.cpp:1203-1247) converts inside
        // `for (i=0; i<maxChans && outIndex<modbusLength_; i++)` — same
        // `outIndex < modbusLength_` cap as `writeInt32Array`; whole elements
        // only, truncated at `modbusLength_`. `outIndex` starts at `0` in
        // absolute mode and at the record's register `offset` in relative
        // mode (drvModbusAsyn.cpp:1218-1226), so the clamp applies in BOTH
        // modes.
        let mut out_index = if absolute { 0usize } else { user.addr as usize };
        for &v in data {
            if out_index >= limit {
                break;
            }
            regs.extend(datatype::write_float(dt, v).map_err(to_asyn)?);
            out_index += rc;
        }
        self.flush_write_staged(user.addr, &regs)
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        // Writing the MODBUS_READ parameter triggers a poll cycle.
        if user.reason == self.read_reason {
            return self.poll_cycle();
        }
        // HISTOGRAM_BIN_TIME sets the histogram bin width.
        if user.reason == self.histogram_bin_reason {
            // C `writeInt32` (drvModbusAsyn.cpp:794-803): set the bin width
            // (clamp <1 to 1), then erase the existing counts — the old counts
            // no longer map to the rebinned widths. (C also rebuilds the time
            // axis; here the axis is recomputed on demand in read_int32_array /
            // read_float64_array, so only the count erase is needed.)
            self.engine.stats.histogram_ms_per_bin = value.max(1) as u32;
            self.engine.stats.clear_histogram();
            return Ok(());
        }
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = datatype::write_int32(dt, value).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        self.cache_write_numeric(user, value as f64)
    }

    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = datatype::write_int64(dt, value).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        self.cache_write_numeric(user, value as f64)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        // POLL_DELAY retunes the read poller's period at runtime. C
        // `writeFloat64` (drvModbusAsyn.cpp:1094-1099) sets `pollDelay_` and
        // signals the poller event. The `ao` writes seconds; store the period
        // in milliseconds and wake the poller so the new period takes effect
        // immediately rather than after the current sleep. Checked before
        // `datatype_of`, which would otherwise reject this non-data reason.
        if user.reason == self.poll_delay_reason {
            let ms = (value * 1000.0).max(0.0) as u64;
            self.poll_delay.store(ms, Ordering::Relaxed);
            self.poll_wake.notify_one();
            return Ok(());
        }
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = datatype::write_float(dt, value).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        self.cache_write_numeric(user, value)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        // ENABLE_HISTOGRAM toggles read-time histogram accumulation.
        if user.reason == self.enable_histogram_reason {
            // C `writeUInt32Digital` (drvModbusAsyn.cpp:633-641): on an OFF→ON
            // transition, erase the existing counts before enabling so a
            // re-enable starts clean. A no-op when already enabled or on
            // disable.
            let enabling = value != 0;
            if enabling && !self.engine.stats.histogram_enabled {
                self.engine.stats.clear_histogram();
            }
            self.engine.stats.histogram_enabled = enabling;
            return Ok(());
        }
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        // C `writeUInt32Digital` (drvModbusAsyn.cpp:595-625) dispatches on the
        // configured function:
        //   - WRITE_SINGLE_COIL: write the value directly, ignoring the mask.
        //   - WRITE_SINGLE_REGISTER / WRITE_MULTIPLE_REGISTERS: read/modify/
        //     write the masked bits, but ONLY when the mask is partial. A mask
        //     of 0 or 0xFFFF writes the value directly with no readback. The
        //     readback is a fresh Modbus READ_HOLDING_REGISTERS at
        //     `modbusAddress + readbackOffset_` (the Wago readback offset), in
        //     both relative and absolute addressing.
        //   - any other function: asynError.
        let function = self.engine.config().function;
        let merged = match function {
            ModbusFunctionCode::WriteSingleCoil => value,
            ModbusFunctionCode::WriteSingleRegister
            | ModbusFunctionCode::WriteMultipleRegisters => {
                if mask == 0 || mask == 0xFFFF {
                    value
                } else {
                    // modbusAddress: the wire address (offset in absolute mode,
                    // start_address + offset in relative mode).
                    let modbus_address = if self.is_absolute() {
                        user.addr
                    } else {
                        i32::from(self.modbus_address(user.addr))
                    };
                    let readback =
                        (modbus_address + self.engine.config().readback_offset()).max(0) as u16;
                    let response = self
                        .engine
                        .do_modbus_io(
                            self.transport.as_mut(),
                            ModbusFunctionCode::ReadHoldingRegisters,
                            readback,
                            &[],
                            1,
                        )
                        .map_err(to_asyn)?;
                    let current = match response {
                        ModbusIoResponse::Data(regs) => {
                            u32::from(regs.first().copied().unwrap_or(0))
                        }
                        // Exception 05 copies nothing into C's readback
                        // destination, which is the local `epicsUInt16 data =
                        // value` (drvModbusAsyn.cpp:578, :2231-2237) — so the
                        // read/modify/write merges the record's own value with
                        // itself, instead of merging against a zero word that was
                        // never read.
                        ModbusIoResponse::Acknowledged => value & 0xFFFF,
                    };
                    // data |= (value & mask); data &= (value | ~mask)
                    (current & !mask) | (value & mask)
                }
            }
            other => {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!(
                        "Modbus function {other:?} cannot service a UInt32Digital write"
                    ),
                });
            }
        };
        self.flush_write(user.addr, &[merged as u16])?;
        Ok(())
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let dt = self.datatype_of(user.reason)?;
        // C `writeOctet` (drvModbusAsyn.cpp:1545-1562) accepts only the
        // write-multiple-registers functions; any other function returns
        // asynError.
        let function = self.engine.config().function;
        if !matches!(
            function,
            ModbusFunctionCode::WriteMultipleRegisters
                | ModbusFunctionCode::WriteMultipleRegistersF23
        ) {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "Modbus function {function:?} cannot write a string; \
                     configure a write-multiple-registers function"
                ),
            });
        }
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        // Word budget for the string. In relative mode the string shares the
        // polled buffer, so it is bounded by the registers above `addr`. In
        // absolute mode `addr` is the wire address (unrelated to the buffer);
        // the string is bounded by the whole `config.length` scratch budget,
        // matching C `writeOctet`'s use of the full `data_` buffer.
        let budget = if self.is_absolute() {
            self.engine.config().length
        } else {
            self.engine.config().length - user.addr as usize
        };
        // C `writeOctet` (drvModbusAsyn.cpp:1519-1529) writes a terminating
        // zero for the Z-string types: it builds a copy guaranteed to end in
        // '\0' and sizes the register write at `getStringLen(maxChars + 1)`, so
        // the NUL lands in the registers (and is then excluded from the
        // reported character count). Append it here so the PLC receives the
        // terminator; `write_string` caps the output at `budget` registers.
        let payload;
        let bytes: &[u8] = if dt.is_zero_terminated_string() {
            payload = {
                let mut v = Vec::with_capacity(data.len() + 1);
                v.extend_from_slice(data);
                v.push(0);
                v
            };
            &payload
        } else {
            data
        };
        let (regs, consumed) = datatype::write_string(dt, bytes, budget).map_err(to_asyn)?;
        self.flush_write_staged(user.addr, &regs)?;
        // Relative mode caches the value and fans out its monitor (see
        // `cache_write_numeric`); absolute mode has no parameter-table slot
        // for a wire address and no poller, so it skips the cache.
        if !self.is_absolute() {
            let s = String::from_utf8_lossy(data).into_owned();
            self.base.set_string_param(user.reason, user.addr, s)?;
            self.base.call_param_callbacks(user.addr)?;
        }
        // Bytes transferred = caller chars written, capped by the register
        // budget and excluding the appended NUL terminator for Z-strings
        // (C `writeOctet` reports the character count, not the NUL,
        // drvModbusAsyn.cpp:1519-1529).
        Ok(consumed.min(data.len()))
    }
}

// ---------------------------------------------------------------------------
// iocsh commands
// ---------------------------------------------------------------------------

/// The settings declared by `modbusInterposeConfig` for one octet port: the
/// Modbus link type, the I/O timeout, and the pre-write delay. C stores these
/// on the interpose `modbusPvt` (`modbusInterpose.c:83-85`).
#[derive(Clone, Copy)]
struct InterposeSettings {
    link: LinkType,
    /// Applied to each transport read/write (C `pasynUser->timeout`,
    /// `modbusInterpose.c:248/337`); `READ_TIMEOUT` when unset (C `DEFAULT_TIMEOUT`).
    timeout: Duration,
    /// Slept before each frame write (C `epicsThreadSleep(writeDelay)`,
    /// `modbusInterpose.c:246`); zero disables it.
    write_delay: Duration,
}

impl Default for InterposeSettings {
    fn default() -> Self {
        Self {
            link: LinkType::Tcp,
            timeout: crate::driver::READ_TIMEOUT,
            write_delay: Duration::ZERO,
        }
    }
}

/// Interpose settings declared by `modbusInterposeConfig`, keyed by octet port
/// name, consumed by `drvModbusAsynConfigure`.
static PENDING_LINKS: Mutex<Option<HashMap<String, InterposeSettings>>> = Mutex::new(None);

fn record_interpose(octet_port: &str, settings: InterposeSettings) {
    let mut g = PENDING_LINKS.lock().unwrap();
    g.get_or_insert_with(HashMap::new)
        .insert(octet_port.to_string(), settings);
}

fn take_interpose(octet_port: &str) -> InterposeSettings {
    PENDING_LINKS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(octet_port).copied())
        .unwrap_or_default()
}

/// `modbusInterposeConfig portName linkType timeoutMsec writeDelayMsec` — port
/// of the C `modbusInterposeConfig`. Here it only records the link type for
/// the octet port; framing itself is done by [`ModbusEngine`].
pub fn modbus_interpose_config_command() -> CommandDef {
    CommandDef::new(
        "modbusInterposeConfig",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "linkType",
                arg_type: ArgType::Int,
                optional: false,
            },
            ArgDesc {
                name: "timeoutMsec",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "writeDelayMsec",
                arg_type: ArgType::Int,
                optional: true,
            },
        ],
        "modbusInterposeConfig portName linkType timeoutMsec writeDelayMsec",
        |args: &[ArgValue], _ctx: &CommandContext| -> CommandResult {
            let (port, settings) = parse_interpose_args(args)?;
            record_interpose(&port, settings);
            println!(
                "modbusInterposeConfig: octet port '{port}' link={:?} \
                 timeout={:?} write_delay={:?}",
                settings.link, settings.timeout, settings.write_delay
            );
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Parse the `modbusInterposeConfig portName linkType timeoutMsec
/// writeDelayMsec` arguments into the octet port name and its
/// [`InterposeSettings`]. Mirrors C `modbusInterposeConfig`
/// (modbusInterpose.c:134-136): the timeout is `timeoutMsec/1000`, falling
/// back to `DEFAULT_TIMEOUT` (2 s = `READ_TIMEOUT`) when 0/unset; the write
/// delay is `writeDelayMsec/1000`, zero when 0/unset.
fn parse_interpose_args(args: &[ArgValue]) -> Result<(String, InterposeSettings), String> {
    let port = match args.first() {
        Some(ArgValue::String(s)) => s.clone(),
        _ => return Err("portName required".into()),
    };
    let link = match args.get(1) {
        Some(ArgValue::Int(v)) => {
            LinkType::from_i32(*v as i32).ok_or_else(|| format!("invalid link type {v}"))?
        }
        _ => return Err("linkType required".into()),
    };
    let timeout = match args.get(2) {
        Some(ArgValue::Int(v)) if *v > 0 => Duration::from_millis(*v as u64),
        _ => crate::driver::READ_TIMEOUT,
    };
    let write_delay = match args.get(3) {
        Some(ArgValue::Int(v)) if *v > 0 => Duration::from_millis(*v as u64),
        _ => Duration::ZERO,
    };
    Ok((
        port,
        InterposeSettings {
            link,
            timeout,
            write_delay,
        },
    ))
}

/// `drvModbusAsynConfigure portName octetPortName slave function startAddr
/// length dataType pollMsec plcType` — port of the C `drvModbusAsynConfigure`.
pub fn drv_modbus_asyn_configure_command(
    handle: epics_base_rs::runtime::task::RuntimeHandle,
    trace: Arc<TraceManager>,
) -> CommandDef {
    CommandDef::new(
        "drvModbusAsynConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "octetPortName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "modbusSlave",
                arg_type: ArgType::Int,
                optional: false,
            },
            ArgDesc {
                name: "modbusFunction",
                arg_type: ArgType::Int,
                optional: false,
            },
            ArgDesc {
                name: "modbusStartAddress",
                arg_type: ArgType::Int,
                optional: false,
            },
            ArgDesc {
                name: "modbusLength",
                arg_type: ArgType::Int,
                optional: false,
            },
            ArgDesc {
                name: "dataType",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "pollMsec",
                arg_type: ArgType::Int,
                optional: false,
            },
            ArgDesc {
                name: "plcType",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "drvModbusAsynConfigure portName octetPort slave function startAddr length dataType pollMsec plcType",
        ModbusConfigHandler { handle, trace },
    )
}

struct ModbusConfigHandler {
    handle: epics_base_rs::runtime::task::RuntimeHandle,
    trace: Arc<TraceManager>,
}

/// Parse the `drvModbusAsynConfigure` positional arguments into a
/// [`ModbusConfig`] plus the octet port name. The data type may be given as a
/// name (`INT32_LE`) or a numeric `modbusDataType_t` index.
fn parse_configure_args(args: &[ArgValue]) -> Result<(String, String, ModbusConfig), String> {
    let s = |i: usize| match &args[i] {
        ArgValue::String(v) => Ok(v.clone()),
        _ => Err(format!("argument {i} must be a string")),
    };
    let n = |i: usize| match &args[i] {
        ArgValue::Int(v) => Ok(*v as i32),
        _ => Err(format!("argument {i} must be an integer")),
    };
    let port_name = s(0)?;
    let octet_port = s(1)?;
    let slave = n(2)?;
    let function = ModbusFunctionCode::from_i32(n(3)?)
        .ok_or_else(|| format!("unsupported Modbus function {}", n(3).unwrap()))?;
    let start_address = n(4)?;
    let length = n(5)?;
    if length < 0 {
        return Err("modbusLength must be >= 0".into());
    }
    let dt_str = s(6)?;
    let data_type = if dt_str.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        dt_str
            .parse::<i32>()
            .ok()
            .and_then(ModbusDataType::from_i32)
            .ok_or_else(|| format!("invalid data type index '{dt_str}'"))?
    } else {
        ModbusDataType::from_type_string(&dt_str)
            .ok_or_else(|| format!("unknown data type '{dt_str}'"))?
    };
    let poll_msec = n(7)?;
    let plc_type = if args.len() > 8 {
        s(8).unwrap_or_default()
    } else {
        String::new()
    };
    let config = ModbusConfig {
        slave: slave as u8,
        function,
        start_address,
        length: length as usize,
        data_type,
        poll_delay: Duration::from_millis(poll_msec.max(0) as u64),
        plc_type,
    };
    config.validate().map_err(|e| e.to_string())?;
    Ok((port_name, octet_port, config))
}

impl CommandHandler for ModbusConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let (port_name, octet_port, config) =
            parse_configure_args(args).map_err(|e| e.to_string())?;
        let interpose = take_interpose(&octet_port);
        let link = interpose.link;
        let initial_poll_delay = config.poll_delay;
        // The read poller is started only for a relative-addressing read port.
        // An absolute-addressing port has no poller (drvModbusAsyn.cpp:1121,
        // `if (absoluteAddressing_) needReadThread = 0;`) — each record reads
        // its own wire address on access.
        let needs_poller = config.function.is_read() && !config.absolute_addressing();

        // Find the underlying octet port and build the framed transport.
        let entry = asyn_rs::asyn_record::get_port(&octet_port)
            .ok_or_else(|| format!("octet port '{octet_port}' not found"))?;
        // The configured timeout (C `modbusInterposeConfig timeoutMsec`) drives
        // both the underlying read and write; the write delay is applied by the
        // transport before each frame write (R53).
        let sync = SyncIOHandle::from_handle(entry.handle.clone(), 0, interpose.timeout);
        let transport = Box::new(SyncIoTransport::with_write_delay(
            sync,
            interpose.write_delay,
        ));

        let driver = ModbusPortDriver::new(&port_name, config, link, transport)
            .map_err(|e| e.to_string())?;
        let read_reason = driver.read_reason;
        // Cloned before the driver moves into the runtime: the poller reads the
        // live period from the shared atomic each cycle and the wake lets a
        // POLL_DELAY write interrupt the current sleep (R46). `poll_backoff` is
        // the poll cycle's request for C's persistent-error sleep.
        let poll_delay = driver.poll_delay.clone();
        let poll_wake = driver.poll_wake.clone();
        let poll_backoff = driver.poll_backoff.clone();

        // A port whose actor thread the OS refused is not a port: fail the
        // command before the registration below claims the name for it. Same
        // end state as C by a different route — `drvModbusAsyn` is an
        // `asynPortDriver` subclass (drvModbusAsyn.h:123) built with a bare
        // `new` (drvModbusAsyn.cpp:3134), so a failed `registerPort` throws
        // out of its constructor (asynPortDriver.cpp:4036-4040), iocsh catches
        // it (iocsh.cpp:1274-1284) and st.cmd continues with the port missing.
        // The `?` is the same error channel the duplicate-name failure below
        // already uses.
        let (runtime, _jh) =
            create_port_runtime(driver, RuntimeConfig::default()).map_err(|e| e.to_string())?;
        let port_handle = runtime.port_handle().clone();
        if let Err(e) =
            asyn_rs::asyn_record::register_port(&port_name, port_handle.clone(), self.trace.clone())
        {
            // Nothing published this port, so ask the actor to stop.
            runtime.shutdown();
            return Err(e.to_string());
        }
        // The registry above holds a live `PortHandle` for this port, which is
        // what keeps its actor alive; the `PortRuntimeHandle` may drop here.
        drop(runtime);

        println!("drvModbusAsynConfigure: port='{port_name}' octet='{octet_port}' link={link:?}");

        // Spawn the read poller — periodically triggers a poll cycle by
        // writing the MODBUS_READ parameter. Port of the `readPoller` thread.
        if needs_poller && !initial_poll_delay.is_zero() {
            self.handle.spawn(read_poller(
                port_handle.clone(),
                read_reason,
                poll_delay,
                poll_wake,
                poll_backoff,
            ));
        }

        Ok(CommandOutcome::Continue)
    }
}

/// C's persistent-I/O-error sleep (`epicsThreadSleep(1.0)`,
/// drvModbusAsyn.cpp:1649): once a poll has failed twice in a row the poller
/// stops hammering the unreachable slave at the poll period and retries once a
/// second until it answers.
const POLL_ERROR_BACKOFF: Duration = Duration::from_secs(1);

/// The read-poller task — port of the C `readPoller` thread's loop control
/// (drvModbusAsyn.cpp:1626-1937). It waits the live poll period, then triggers
/// one poll cycle by writing the `MODBUS_READ` parameter (C signals
/// `readPollerEventId_`; here the write runs the cycle inside the port actor).
///
/// The loop exits on exactly one condition: the port actor is gone — the Rust
/// equivalent of C's `if (modbusExiting_) break;` (:1637). A Modbus I/O error is
/// **not** an exit condition and never has been in C: the poll cycle delivers it
/// to the records as an alarm status and the poller keeps going, so an outage
/// alarms the records and a recovered link brings them back with no IOC restart.
/// The cycle asks for the 1.0 s backoff (`poll_backoff`) once the error is
/// persistent, exactly as C sleeps before its next `doModbusIO`.
async fn read_poller(
    handle: asyn_rs::port_handle::PortHandle,
    read_reason: usize,
    poll_delay: Arc<AtomicU64>,
    poll_wake: Arc<Notify>,
    poll_backoff: Arc<AtomicBool>,
) {
    loop {
        // Read the live period each cycle so a runtime POLL_DELAY write retunes
        // it. A wake (POLL_DELAY changed) ends the wait early — C signals
        // readPollerEventId_ and re-waits with the new pollDelay_ — then we
        // re-read the period.
        let ms = poll_delay.load(Ordering::Relaxed);
        let _ = tokio::time::timeout(Duration::from_millis(ms), poll_wake.notified()).await;
        if handle.is_closed() {
            break;
        }
        // The cycle's own I/O status reaches the records as their callback
        // status; an `Err` here is an internal fault (a data-type decode the
        // engine could not perform), which C likewise logs and polls through.
        let _ = handle.write_int32(read_reason, 0, 1).await;
        if poll_backoff.swap(false, Ordering::Relaxed) {
            tokio::time::sleep(POLL_ERROR_BACKOFF).await;
        }
    }
}

/// Register the Modbus `iocsh` commands on an `IocApplication`.
///
/// The underlying octet port is created with `asyn-rs`'s own
/// `drvAsynIPPortConfigure` (registered via
/// `asyn_rs::iocsh::register_asyn_commands`) — call that as well.
pub fn register_modbus_commands(
    app: epics_ca_rs::server::ioc_app::IocApplication,
    handle: epics_base_rs::runtime::task::RuntimeHandle,
    trace: Arc<TraceManager>,
) -> epics_ca_rs::server::ioc_app::IocApplication {
    app.register_startup_command(modbus_interpose_config_command())
        .register_startup_command(drv_modbus_asyn_configure_command(handle, trace))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Vec<ArgValue>) -> Vec<ArgValue> {
        v
    }

    /// A transport that never produces traffic — sufficient for tests that
    /// exercise the offset-checking paths, which fail before any I/O.
    struct NullTransport;

    impl OctetTransport for NullTransport {
        fn write_frame(&mut self, _data: &[u8]) -> crate::error::ModbusResult<()> {
            Ok(())
        }
        fn read_frame(&mut self, _timeout: Duration) -> crate::error::ModbusResult<Vec<u8>> {
            Err(ModbusError::Timeout)
        }
    }

    /// A transport that replays a queue of canned response frames — lets a
    /// test drive `poll_cycle` through a successful engine poll. Every written
    /// frame is appended to `written`, a shared buffer the test can inspect
    /// after the driver call to assert the on-wire request shape.
    type WriteLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

    struct ReplayTransport {
        responses: std::collections::VecDeque<crate::error::ModbusResult<Vec<u8>>>,
        written: WriteLog,
    }

    impl ReplayTransport {
        fn new(responses: Vec<crate::error::ModbusResult<Vec<u8>>>) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                written: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        /// A handle onto the shared write log, cloned before the transport is
        /// moved into the driver so the test can read it back afterwards.
        fn written_handle(&self) -> WriteLog {
            std::sync::Arc::clone(&self.written)
        }
    }

    impl OctetTransport for ReplayTransport {
        fn write_frame(&mut self, data: &[u8]) -> crate::error::ModbusResult<()> {
            self.written.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        fn read_frame(&mut self, _timeout: Duration) -> crate::error::ModbusResult<Vec<u8>> {
            self.responses
                .pop_front()
                .unwrap_or(Err(ModbusError::Timeout))
        }
    }

    /// Wrap a bare Modbus PDU in a Modbus/TCP MBAP frame for `txid`.
    fn tcp_response(txid: u16, pdu: &[u8]) -> Vec<u8> {
        let mut frame = crate::protocol::MbapHeader::new(txid, pdu.len() as u16)
            .to_bytes()
            .to_vec();
        frame.extend_from_slice(pdu);
        frame
    }

    fn test_config(start_address: i32, length: usize) -> ModbusConfig {
        ModbusConfig {
            slave: 1,
            function: ModbusFunctionCode::ReadHoldingRegisters,
            start_address,
            length,
            data_type: ModbusDataType::UInt16,
            poll_delay: Duration::from_millis(100),
            plc_type: String::new(),
        }
    }

    /// A read port configured with absolute addressing builds successfully —
    /// absolute mode is supported, not rejected.
    #[test]
    fn absolute_addressing_driver_builds() {
        let driver = ModbusPortDriver::new(
            "MB_ABS",
            test_config(-1, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("absolute addressing port must build");
        assert!(driver.is_absolute());
    }

    /// R51: the drvUser `=N` suffix is validated as C does — legal only for
    /// the eight string types, where it must be a non-negative integer
    /// (`strtol` base 0). A bare type, a valid string length, hex, and an empty
    /// suffix resolve; garbage, a negative length, and any suffix on a
    /// non-string type are rejected so record init fails (drvModbusAsyn.cpp
    /// :387-412).
    #[test]
    fn drv_user_create_validates_string_length_suffix() {
        let mut driver = ModbusPortDriver::new(
            "MB_DRVUSER",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("relative config must build");
        // Accepted.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH", 0))
                .is_ok()
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=5", 0))
                .is_ok()
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=0x10", 0))
                .is_ok()
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=010", 0))
                .is_ok()
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=", 0))
                .is_ok(),
            "empty length parses as 0, which C accepts"
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("INT16", 0))
                .is_ok()
        );
        // Rejected: garbage / negative length on a string type.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=abc", 0))
                .is_err()
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=5x", 0))
                .is_err()
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=-3", 0))
                .is_err()
        );
        // Rejected: a length suffix on a non-string type.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("INT16=5", 0))
                .is_err()
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("UINT16=0", 0))
                .is_err()
        );
        // Still rejected: an unknown type name.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("NOPE", 0))
                .is_err()
        );
    }

    /// R52: `connect_addr` rejects an out-of-range register offset (the asyn
    /// `addr`), mirroring C `connect` (drvModbusAsyn.cpp:455-467) which returns
    /// `asynError` so a misconfigured record fails to connect rather than
    /// connecting and alarming on every I/O.
    #[test]
    fn connect_addr_rejects_out_of_range_offset() {
        // Relative mode: valid offsets are 0..length (here 16).
        let mut relative = ModbusPortDriver::new(
            "MB_CONN_REL",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("relative config must build");
        assert!(
            relative
                .connect_addr(&AsynUser::new(0).with_addr(5))
                .is_ok()
        );
        assert!(
            relative
                .connect_addr(&AsynUser::new(0).with_addr(16))
                .is_err(),
            "offset == length is out of range in relative mode"
        );
        assert!(
            relative
                .connect_addr(&AsynUser::new(0).with_addr(99))
                .is_err()
        );

        // Absolute mode: the full 16-bit wire range 0..=0xFFFF is valid.
        let mut absolute = ModbusPortDriver::new(
            "MB_CONN_ABS",
            test_config(-1, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("absolute config must build");
        assert!(absolute.is_absolute());
        assert!(
            absolute
                .connect_addr(&AsynUser::new(0).with_addr(0xFFFF))
                .is_ok(),
            "0xFFFF is the top of the absolute wire range"
        );
        assert!(
            absolute
                .connect_addr(&AsynUser::new(0).with_addr(0x1_0000))
                .is_err(),
            "0x10000 is one past the 16-bit wire range"
        );
    }

    /// A data-type drvInfo with an out-of-range register offset (the asyn
    /// `addr`) fails at `drv_user_create` (record bind), mirroring C
    /// `drvUserCreate` (drvModbusAsyn.cpp:378-384) which runs `getAddr` +
    /// `checkOffset` inside the data-type branch and returns `asynError`, so a
    /// misconfigured record fails init rather than alarming on every I/O. A
    /// non-data (statistics) drvInfo skips the check, as C falls through to the
    /// base class with no offset validation (:433).
    #[test]
    fn drv_user_create_rejects_out_of_range_offset_for_data_reason() {
        // Relative mode: valid offsets are 0..length (here 16).
        let mut driver = ModbusPortDriver::new(
            "MB_DU_OFF",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("relative config must build");
        // In-range data offset binds.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("INT16", 5))
                .is_ok()
        );
        // Out-of-range data offset fails at bind, not per-I/O.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("INT16", 16))
                .is_err(),
            "offset == length is out of range in relative mode"
        );
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("INT16", 99))
                .is_err()
        );
        // A non-data (statistics) reason carries no offset → C skips
        // `checkOffset`, so an out-of-range addr must NOT reject it.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("READ_OK", 99))
                .is_ok(),
            "statistics params fall through to the base class with no offset check"
        );
    }

    /// A valid `TYPE=N` suffix on a string type yields the per-record octet cap
    /// (C `drvUser->len`, consumed by `getStringLen`); no suffix yields no cap;
    /// `TYPE=` yields 0 (C accepts `strtol("")` == 0). A non-string type never
    /// carries a cap.
    #[test]
    fn drv_user_create_returns_octet_len_cap() {
        let mut driver = ModbusPortDriver::new(
            "MB_DU_CAP",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("relative config must build");
        assert_eq!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=5", 0))
                .unwrap()
                .max_octet_len,
            Some(5)
        );
        assert_eq!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=0x10", 0))
                .unwrap()
                .max_octet_len,
            Some(16)
        );
        assert_eq!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH", 0))
                .unwrap()
                .max_octet_len,
            None,
            "no suffix → no cap (C leaves drvUser->len at -1)"
        );
        assert_eq!(
            driver
                .drv_user_create(&DrvUserRequest::new("STRING_HIGH=", 0))
                .unwrap()
                .max_octet_len,
            Some(0),
            "TYPE= caps to 0 (C strtol(\"\") == 0)"
        );
        assert_eq!(
            driver
                .drv_user_create(&DrvUserRequest::new("INT16", 0))
                .unwrap()
                .max_octet_len,
            None,
            "a non-string type never carries an octet cap"
        );
    }

    /// Unit-level coverage of the `strtol` base-0 accept/reject set the
    /// drvUser validator depends on (drvModbusAsyn.cpp:398-404).
    #[test]
    fn parse_drvuser_string_len_matches_strtol_base0() {
        assert_eq!(parse_drvuser_string_len("5"), Some(5));
        assert_eq!(parse_drvuser_string_len("0"), Some(0));
        assert_eq!(parse_drvuser_string_len(""), Some(0));
        assert_eq!(parse_drvuser_string_len("0x10"), Some(16));
        assert_eq!(parse_drvuser_string_len("0X1f"), Some(31));
        assert_eq!(parse_drvuser_string_len("010"), Some(8)); // octal
        assert_eq!(parse_drvuser_string_len("+7"), Some(7));
        assert_eq!(parse_drvuser_string_len("abc"), None);
        assert_eq!(parse_drvuser_string_len("5x"), None);
        assert_eq!(parse_drvuser_string_len("-3"), None);
        assert_eq!(parse_drvuser_string_len("-"), None);
    }

    /// Absolute-mode `read_int32`: the record's asyn `addr` is the absolute
    /// wire address; the accessor issues an individual Modbus request there
    /// and decodes the response — no shared polled buffer is consulted.
    #[test]
    fn absolute_read_int32_issues_request_at_wire_address() {
        // ReadHoldingRegisters response. `read_int32` now issues a fixed
        // 2-register request (C `readInt32`, drvModbusAsyn.cpp:675-676), so the
        // response carries two words; the UINT16 value decodes from the first.
        let pdu = [0x01u8, 0x03, 0x04, 0xBE, 0xEF, 0x00, 0x00];
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_RD",
            test_config(-1, 16),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        // Wire address far beyond the 16-word scratch buffer — legal in
        // absolute mode (0..=65535).
        user.addr = 0x2710;
        let v = driver
            .read_int32(&user)
            .expect("absolute read must succeed");
        assert_eq!(v, 0xBEEF);
    }

    /// R50: an absolute-addressing port has no poller, so the statistics
    /// counters were only ever published from `poll_cycle` and read 0 forever.
    /// C `doModbusIO` setIntegerParams them inline on every I/O
    /// (drvModbusAsyn.cpp:2255/2279/2301), so the per-record absolute read must
    /// publish them too. After one successful absolute read, READ_OK must be 1
    /// and IO_ERRORS 0.
    #[test]
    fn absolute_read_publishes_statistics() {
        let pdu = [0x01u8, 0x03, 0x04, 0xBE, 0xEF, 0x00, 0x00];
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_STATS",
            test_config(-1, 16),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("absolute config must build");
        let data_reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(data_reason);
        user.addr = 0x2710;
        driver
            .read_int32(&user)
            .expect("absolute read must succeed");

        // The statistics longins read their params (asyn addr 0) on scan.
        let read_ok_reason = driver.base.find_param(PARAM_READ_OK).unwrap();
        let io_errors_reason = driver.base.find_param(PARAM_IO_ERRORS).unwrap();
        assert_eq!(
            driver.read_int32(&AsynUser::new(read_ok_reason)).unwrap(),
            1,
            "READ_OK must be published after an absolute read (R50)"
        );
        assert_eq!(
            driver.read_int32(&AsynUser::new(io_errors_reason)).unwrap(),
            0,
        );
    }

    /// R50 error path: C setIntegerParams P_IOErrors inside `doModbusIO` on a
    /// transport failure *before* it returns the error (drvModbusAsyn.cpp:
    /// 2206), so a failed absolute read must still publish IO_ERRORS = 1.
    #[test]
    fn absolute_read_failure_publishes_io_errors() {
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_STATS_ERR",
            test_config(-1, 16),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Err(ModbusError::Timeout)])),
        )
        .expect("absolute config must build");
        let data_reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(data_reason);
        user.addr = 0x2710;
        driver
            .read_int32(&user)
            .expect_err("a timed-out absolute read must fail");

        let io_errors_reason = driver.base.find_param(PARAM_IO_ERRORS).unwrap();
        let read_ok_reason = driver.base.find_param(PARAM_READ_OK).unwrap();
        assert_eq!(
            driver.read_int32(&AsynUser::new(io_errors_reason)).unwrap(),
            1,
            "IO_ERRORS must be published even on a failed absolute read (R50)"
        );
        assert_eq!(
            driver.read_int32(&AsynUser::new(read_ok_reason)).unwrap(),
            0,
        );
    }

    /// R47: re-enabling the histogram (OFF→ON) must erase stale counts first
    /// (C drvModbusAsyn.cpp:633-641). Disabling, or re-asserting ENABLE while
    /// already on, must NOT clear.
    #[test]
    fn enable_histogram_rising_edge_clears_counts() {
        let mut driver = ModbusPortDriver::new(
            "MB_HIST_EN",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("config must build");
        let reason = driver.base.find_param(PARAM_ENABLE_HISTOGRAM).unwrap();
        let mut user = AsynUser::new(reason);

        // Enable, then stash a count as if an I/O had been timed.
        driver.write_uint32_digital(&mut user, 1, 0xFFFF).unwrap();
        driver.engine.stats.histogram[3] = 5;
        // Re-asserting ENABLE while already on must NOT clear (no rising edge).
        driver.write_uint32_digital(&mut user, 1, 0xFFFF).unwrap();
        assert_eq!(driver.engine.stats.histogram[3], 5);
        // Disabling must NOT clear (C clears only on the OFF→ON edge).
        driver.write_uint32_digital(&mut user, 0, 0xFFFF).unwrap();
        assert_eq!(driver.engine.stats.histogram[3], 5);
        // OFF→ON re-enable must erase the stale count.
        driver.write_uint32_digital(&mut user, 1, 0xFFFF).unwrap();
        assert_eq!(driver.engine.stats.histogram[3], 0);
        assert!(driver.engine.stats.histogram_enabled);
    }

    /// R48: changing HISTOGRAM_BIN_TIME must set the (clamped) bin width and
    /// erase the existing counts, which no longer map to the new bins
    /// (C drvModbusAsyn.cpp:794-803).
    #[test]
    fn histogram_bin_time_change_clears_counts() {
        let mut driver = ModbusPortDriver::new(
            "MB_HIST_BIN",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("config must build");
        let reason = driver.base.find_param(PARAM_HISTOGRAM_BIN_TIME).unwrap();
        let mut user = AsynUser::new(reason);
        driver.engine.stats.histogram[7] = 9;

        driver.write_int32(&mut user, 5).unwrap();
        assert_eq!(driver.engine.stats.histogram_ms_per_bin, 5);
        assert_eq!(driver.engine.stats.histogram[7], 0, "counts must be erased");

        // A value below 1 clamps to 1 (C `if (histogramMsPerBin_ < 1) = 1`).
        driver.engine.stats.histogram[7] = 4;
        driver.write_int32(&mut user, 0).unwrap();
        assert_eq!(driver.engine.stats.histogram_ms_per_bin, 1);
        assert_eq!(driver.engine.stats.histogram[7], 0);
    }

    /// R49: READ_HISTOGRAM and HISTOGRAM_TIME_AXIS must serve a Float64 array
    /// binding (aai/waveform FTVL=DOUBLE), not only the LONG path — C
    /// `readFloat64Array` serves both (drvModbusAsyn.cpp:1181-1191).
    #[test]
    fn read_float64_array_serves_histogram() {
        let mut driver = ModbusPortDriver::new(
            "MB_HIST_F64",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("config must build");
        driver.engine.stats.histogram_ms_per_bin = 4;
        driver.engine.stats.histogram[0] = 11;
        driver.engine.stats.histogram[2] = 22;

        let hist_reason = driver.base.find_param(PARAM_READ_HISTOGRAM).unwrap();
        let axis_reason = driver.base.find_param(PARAM_HISTOGRAM_TIME_AXIS).unwrap();

        let mut hbuf = [0.0f64; 5];
        let n = driver
            .read_float64_array(&AsynUser::new(hist_reason), &mut hbuf)
            .unwrap();
        assert_eq!(n, 5);
        assert_eq!(hbuf[0], 11.0);
        assert_eq!(hbuf[2], 22.0);

        let mut abuf = [0.0f64; 5];
        let n = driver
            .read_float64_array(&AsynUser::new(axis_reason), &mut abuf)
            .unwrap();
        assert_eq!(n, 5);
        // axis[i] = i * bin, bin = 4.
        assert_eq!(abuf, [0.0, 4.0, 8.0, 12.0, 16.0]);
    }

    /// R46: a POLL_DELAY write (the poll_delay.template `ao`, in seconds) must
    /// succeed and retune the live poll period, not error out through
    /// `datatype_of`. C `writeFloat64` sets `pollDelay_` and signals the poller
    /// (drvModbusAsyn.cpp:1094-1099).
    #[test]
    fn poll_delay_write_retunes_period() {
        let mut driver = ModbusPortDriver::new(
            "MB_POLL_DELAY",
            test_config(0, 16),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("config must build");
        // Initial period seeded from test_config = 100 ms.
        assert_eq!(driver.poll_delay.load(Ordering::Relaxed), 100);
        let reason = driver.base.find_param(PARAM_POLL_DELAY).unwrap();
        let mut user = AsynUser::new(reason);
        // 2.5 s -> 2500 ms; the write must succeed (not a WRITE/INVALID alarm).
        driver
            .write_float64(&mut user, 2.5)
            .expect("POLL_DELAY write must succeed");
        assert_eq!(driver.poll_delay.load(Ordering::Relaxed), 2500);
        // A negative value clamps to 0.
        driver.write_float64(&mut user, -1.0).unwrap();
        assert_eq!(driver.poll_delay.load(Ordering::Relaxed), 0);
    }

    /// Absolute-mode `read_octet` for a single-byte string encoding
    /// (`StringHigh`): C `readPlcString` (drvModbusAsyn.cpp:3001-3052) consumes
    /// exactly one register per character for `dataTypeStringHigh`, so
    /// `readOctet` (drvModbusAsyn.cpp:1464-1465) requests `min(maxChars,
    /// modbusLength_)` registers — one per char. A 10-char read must request
    /// 10 registers and return the full 10-char string, not a half-length one.
    #[test]
    fn absolute_read_octet_single_byte_string_reads_full_length() {
        // 10 ReadHoldingRegisters words, each high byte one ASCII char of
        // "ABCDEFGHIJ". Response PDU: fc 0x03, byte_count 20, then 10 words.
        let chars = b"ABCDEFGHIJ";
        let mut pdu = vec![0x01u8, 0x03, (chars.len() * 2) as u8];
        for &c in chars {
            pdu.push(c); // high byte = char (StringHigh)
            pdu.push(0x00); // low byte unused
        }
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        let mut cfg = test_config(-1, 64);
        cfg.data_type = ModbusDataType::StringHigh;
        let mut driver =
            ModbusPortDriver::new("MB_ABS_STR", cfg, LinkType::Tcp, Box::new(transport))
                .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::StringHigh.as_str())
            .expect("STRING_HIGH parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x3000;
        // The record buffer (`NELM`) holds 10 chars.
        let mut buf = [0u8; 10];
        let n = driver
            .read_octet(&user, &mut buf)
            .expect("absolute octet read must succeed");
        // Full 10-char string, not truncated to 5 by a `div_ceil(2)` request.
        assert_eq!(n, 10, "single-byte string must read all 10 chars");
        assert_eq!(&buf, b"ABCDEFGHIJ");
        // The on-wire request must ask for 10 registers (one per char),
        // matching C `min(maxChars, modbusLength_)` — not 5.
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one absolute read request");
        // TCP frame: 6-byte MBAP header, then PDU
        // [slave, fcode, addr_hi, addr_lo, count_hi, count_lo].
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x03, 0x30, 0x00, 0x00, 0x0A],
            "request must target wire addr 0x3000 with a 10-register count"
        );
    }

    /// `io_read_octet_eom` must report `ASYN_EOM_CNT` even when the decoded
    /// string is SHORTER than the record buffer — mirroring C `readOctet`
    /// (drvModbusAsyn.cpp:1480) which sets `*eomReason = ASYN_EOM_CNT`
    /// unconditionally. The generic `PortDriver` synthesis would return `empty`
    /// here because the buffer did not fill (R55).
    #[test]
    fn read_octet_eom_always_flags_cnt_for_short_string() {
        // High bytes spell "ABCD\0FGHIJ": the embedded NUL truncates the
        // decoded string to 4 chars, well under the 10-char buffer.
        let chars = b"ABCD\0FGHIJ";
        let mut pdu = vec![0x01u8, 0x03, (chars.len() * 2) as u8];
        for &c in chars {
            pdu.push(c); // high byte = char (StringHigh)
            pdu.push(0x00); // low byte unused
        }
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let mut cfg = test_config(-1, 64);
        cfg.data_type = ModbusDataType::StringHigh;
        let mut driver =
            ModbusPortDriver::new("MB_ABS_STR_EOM", cfg, LinkType::Tcp, Box::new(transport))
                .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::StringHigh.as_str())
            .expect("STRING_HIGH parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x3000;
        let mut buf = [0u8; 10];
        let (n, eom) = driver
            .io_read_octet_eom(&user, &mut buf)
            .expect("absolute octet read must succeed");
        assert_eq!(n, 4, "string truncates at the embedded NUL to 4 chars");
        assert!(n < buf.len(), "test must exercise the not-full-buffer case");
        assert_eq!(
            eom,
            EomReason::CNT,
            "modbus octet read must always flag ASYN_EOM_CNT (R55)"
        );
        assert_eq!(&buf[..4], b"ABCD");
    }

    /// Absolute-mode `read_int32` issues a fixed-length request of
    /// `min(2, modbusLength_)` = 2 registers, matching C `readInt32`
    /// (drvModbusAsyn.cpp:675-676) — independent of the record's data type
    /// width (here `UInt16`, whose `register_count()` is 1).
    #[test]
    fn absolute_read_int32_issues_fixed_two_register_request() {
        // ReadHoldingRegisters response for two words.
        let pdu = [0x01u8, 0x03, 0x04, 0xBE, 0xEF, 0x00, 0x00];
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_I32_LEN",
            test_config(-1, 16),
            LinkType::Tcp,
            Box::new(transport),
        )
        .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x2710;
        driver
            .read_int32(&user)
            .expect("absolute read must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one absolute read request");
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x03, 0x27, 0x10, 0x00, 0x02],
            "read_int32 must request a fixed 2-register count (C min(2,len))"
        );
    }

    /// Absolute-mode `read_float64` issues a fixed-length request of
    /// `min(4, modbusLength_)` = 4 registers, matching C `readFloat64`
    /// (drvModbusAsyn.cpp:982-983).
    #[test]
    fn absolute_read_float64_issues_fixed_four_register_request() {
        // ReadHoldingRegisters response for four words.
        let pdu = [0x01u8, 0x03, 0x08, 0, 0, 0, 0, 0, 0, 0, 0];
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        let mut cfg = test_config(-1, 16);
        cfg.data_type = ModbusDataType::Float64Le;
        let mut driver =
            ModbusPortDriver::new("MB_ABS_F64_LEN", cfg, LinkType::Tcp, Box::new(transport))
                .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::Float64Le.as_str())
            .expect("FLOAT64_LE parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x2710;
        driver
            .read_float64(&user)
            .expect("absolute read must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one absolute read request");
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x03, 0x27, 0x10, 0x00, 0x04],
            "read_float64 must request a fixed 4-register count (C min(4,len))"
        );
    }

    /// Absolute-mode `read_int32_array` with a record array length
    /// (`buf.len()`) smaller than `config.length`: C `readInt32Array`
    /// (drvModbusAsyn.cpp:1294-1295) issues
    /// `doModbusIO(..., std::min((int)maxChans, modbusLength_))`, so the
    /// on-wire request must ask for `min(buf.len(), config.length)` registers
    /// — not the bare `config.length`, which would over-read. With a 4-element
    /// record buffer and `config.length` 16, the request count must be 4.
    #[test]
    fn absolute_read_int32_array_request_clamps_to_record_array_length() {
        // ReadHoldingRegisters response for four words: 0x0001..=0x0004.
        let pdu = [
            0x01u8, 0x03, 0x08, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04,
        ];
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        // `config.length` 16 is larger than the 4-element record buffer.
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_I32ARR_LEN",
            test_config(-1, 16),
            LinkType::Tcp,
            Box::new(transport),
        )
        .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x2710;
        // Record array length (`NELM` / `maxChans`) is 4, below config.length.
        let mut buf = [0i32; 4];
        let n = driver
            .read_int32_array(&user, &mut buf)
            .expect("absolute int32-array read must succeed");
        assert_eq!(n, 4, "all 4 record-buffer elements must decode");
        assert_eq!(buf, [1, 2, 3, 4], "decoded values from response offset 0");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one absolute read request");
        // The on-wire request must ask for 4 registers — min(buf.len()=4,
        // config.length=16) — matching C `min(maxChans, modbusLength_)`.
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x03, 0x27, 0x10, 0x00, 0x04],
            "request count must be min(buf.len(), config.length) = 4, not 16"
        );
    }

    /// Absolute-mode `write_int32`: the record's asyn `addr` is the wire
    /// address; the accessor issues an individual write request there. A wire
    /// address beyond the parameter table must not fault on a cache update.
    #[test]
    fn absolute_write_int32_issues_request_at_wire_address() {
        // WriteSingleRegister echo response.
        let pdu = [0x01u8, 0x06, 0x27, 0x10, 0x12, 0x34];
        let mut cfg = test_config(-1, 4);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_WR",
            cfg,
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        // Wire address 0x2710 is well past the 4-word scratch buffer and past
        // the parameter table — the write must still succeed.
        user.addr = 0x2710;
        driver
            .write_int32(&mut user, 0x1234)
            .expect("absolute write must succeed");
    }

    /// In absolute mode `poll_cycle` is a no-op: there is no polled block.
    #[test]
    fn absolute_poll_cycle_is_noop() {
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_POLL",
            test_config(-1, 8),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("absolute config must build");
        // No transport traffic is consumed — a no-op cannot time out.
        driver
            .poll_cycle()
            .expect("absolute poll_cycle must be a no-op, not error");
    }

    /// Absolute-mode `write_int32_array` with a record array longer than
    /// `config.length`: C `writeInt32Array` (drvModbusAsyn.cpp:1412-1417)
    /// converts inside `for (i=0; i<maxChans && outIndex<modbusLength_; i++)`,
    /// so it emits whole elements only while `outIndex < modbusLength_` and
    /// truncates the on-wire write at `modbusLength_`. With INT32 (2 registers
    /// per element) and `config.length` 4, a 6-element record array must write
    /// exactly 2 whole elements = 4 registers, not all 12 registers.
    #[test]
    fn absolute_write_int32_array_request_clamps_to_modbus_length() {
        // WriteMultipleRegisters echo response: slave, fc 0x10, address,
        // quantity (4 registers).
        let pdu = [0x01u8, 0x10, 0x27, 0x10, 0x00, 0x04];
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        // `config.length` 4 registers — smaller than the 6-element INT32
        // record array (12 registers).
        let mut cfg = test_config(-1, 4);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_WI32ARR_LEN",
            cfg,
            LinkType::Tcp,
            Box::new(transport),
        )
        .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::Int32Be.as_str())
            .expect("INT32_BE parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x2710;
        // 6 INT32 elements = 12 registers; only 2 elements (4 registers) fit.
        let data = [1i32, 2, 3, 4, 5, 6];
        driver
            .write_int32_array(&user, &data)
            .expect("absolute int32-array write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one absolute write request");
        // TCP frame: 7-byte MBAP, then PDU [unit, fc=0x10, addr_hi, addr_lo,
        // qty_hi, qty_lo, byte_count, data...]. Quantity must be 4 registers.
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x10, 0x27, 0x10, 0x00, 0x04],
            "on-wire register count must be capped at config.length = 4"
        );
        // byte_count = 4 registers * 2 = 8; data begins at frame offset 13,
        // so the frame ends at 13 + 8 = 21 bytes — only 4 registers on wire.
        assert_eq!(frames[0][12], 0x08, "byte count must be 8 (4 registers)");
        assert_eq!(frames[0].len(), 21, "frame carries only 4 registers");
    }

    /// Absolute-mode `write_float64_array` with a record array longer than
    /// `config.length`: C `writeFloat64Array` (drvModbusAsyn.cpp:1242-1247)
    /// uses the same `for (i=0; i<maxChans && outIndex<modbusLength_; i++)`
    /// guard. FLOAT64 is 4 registers per element. With `config.length` 6, the
    /// first element starts at `outIndex` 0 (< 6) and is written whole
    /// (`outIndex` -> 4); the second starts at 4 (< 6) and is also written
    /// whole (`outIndex` -> 8); the third starts at 8 (>= 6) so the loop
    /// stops. The wire write is 2 whole elements = 8 registers — note this
    /// exceeds `config.length` because C emits a whole element once its start
    /// index passes the guard.
    #[test]
    fn absolute_write_float64_array_request_clamps_to_modbus_length() {
        // WriteMultipleRegisters echo response: 8 registers.
        let pdu = [0x01u8, 0x10, 0x27, 0x10, 0x00, 0x08];
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        let mut cfg = test_config(-1, 6);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver = ModbusPortDriver::new(
            "MB_ABS_WF64ARR_LEN",
            cfg,
            LinkType::Tcp,
            Box::new(transport),
        )
        .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::Float64Le.as_str())
            .expect("FLOAT64_LE parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x2710;
        // 4 FLOAT64 elements = 16 registers; only 2 whole elements fit (the
        // second starts at outIndex 4, still < config.length 6).
        let data = [1.0f64, 2.0, 3.0, 4.0];
        driver
            .write_float64_array(&user, &data)
            .expect("absolute float64-array write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one absolute write request");
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x10, 0x27, 0x10, 0x00, 0x08],
            "on-wire register count must be 2 whole FLOAT64 elements = 8"
        );
        // byte_count = 8 registers * 2 = 16; data begins at frame offset 13,
        // so the frame ends at 13 + 16 = 29 bytes.
        assert_eq!(frames[0][12], 0x10, "byte count must be 16 (8 registers)");
        assert_eq!(frames[0].len(), 29, "frame carries only 8 registers");
    }

    /// Relative-mode `write_int32_array` with `offset + total_registers >
    /// config.length`: C `writeInt32Array` (drvModbusAsyn.cpp:1392-1396)
    /// initializes `outIndex = offset` in relative mode, then the conversion
    /// loop `for (i=0; i<maxChans && outIndex<modbusLength_; i++)`
    /// (drvModbusAsyn.cpp:1412-1417) caps the on-wire write at
    /// `modbusLength_`. With INT32 (2 registers per element),
    /// `config.length` 6 and record `addr` 3, `outIndex` starts at 3:
    /// element 0 starts at 3 (< 6) -> whole, outIndex 5; element 1 starts at
    /// 5 (< 6) -> whole, outIndex 7; element 2 starts at 7 (>= 6) -> loop
    /// stops. A 5-element record array writes exactly 2 whole elements =
    /// 4 registers, not all 10.
    #[test]
    fn relative_write_int32_array_request_clamps_to_modbus_length() {
        // WriteMultipleRegisters echo response: slave, fc 0x10, address 3,
        // quantity (4 registers).
        let pdu = [0x01u8, 0x10, 0x00, 0x03, 0x00, 0x04];
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        // Relative mode (start_address >= 0), config.length 6 registers.
        let mut cfg = test_config(0, 6);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver = ModbusPortDriver::new(
            "MB_REL_WI32ARR_LEN",
            cfg,
            LinkType::Tcp,
            Box::new(transport),
        )
        .expect("relative config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::Int32Be.as_str())
            .expect("INT32_BE parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 3;
        // 5 INT32 elements = 10 registers; from outIndex 3 only 2 whole
        // elements (4 registers) fit before outIndex reaches config.length 6.
        let data = [1i32, 2, 3, 4, 5];
        driver
            .write_int32_array(&user, &data)
            .expect("relative int32-array write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one relative write request");
        // TCP frame: 7-byte MBAP, then PDU [unit, fc=0x10, addr_hi, addr_lo,
        // qty_hi, qty_lo, byte_count, data...]. Wire address = start_address
        // (0) + offset (3) = 3; quantity must be 4 registers.
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x10, 0x00, 0x03, 0x00, 0x04],
            "on-wire register count must be capped at config.length - offset"
        );
        assert_eq!(frames[0][12], 0x08, "byte count must be 8 (4 registers)");
        assert_eq!(frames[0].len(), 21, "frame carries only 4 registers");
    }

    /// Relative-mode `write_float64_array` with `offset + total_registers >
    /// config.length`: C `writeFloat64Array` (drvModbusAsyn.cpp:1222-1226)
    /// initializes `outIndex = offset` in relative mode; the loop
    /// `for (i=0; i<maxChans && outIndex<modbusLength_; i++)`
    /// (drvModbusAsyn.cpp:1242-1247) caps the wire write. FLOAT64 is 4
    /// registers per element. With `config.length` 6 and record `addr` 3,
    /// `outIndex` starts at 3: element 0 starts at 3 (< 6) -> whole, outIndex
    /// 7; element 1 starts at 7 (>= 6) -> loop stops. A 3-element record
    /// array writes exactly 1 whole element = 4 registers.
    #[test]
    fn relative_write_float64_array_request_clamps_to_modbus_length() {
        // WriteMultipleRegisters echo response: 4 registers at address 3.
        let pdu = [0x01u8, 0x10, 0x00, 0x03, 0x00, 0x04];
        let transport = ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))]);
        let written = transport.written_handle();
        let mut cfg = test_config(0, 6);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver = ModbusPortDriver::new(
            "MB_REL_WF64ARR_LEN",
            cfg,
            LinkType::Tcp,
            Box::new(transport),
        )
        .expect("relative config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::Float64Le.as_str())
            .expect("FLOAT64_LE parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 3;
        // 3 FLOAT64 elements = 12 registers; from outIndex 3 only 1 whole
        // element (4 registers) fits before outIndex reaches config.length 6.
        let data = [1.0f64, 2.0, 3.0];
        driver
            .write_float64_array(&user, &data)
            .expect("relative float64-array write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "exactly one relative write request");
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x10, 0x00, 0x03, 0x00, 0x04],
            "on-wire register count must be 1 whole FLOAT64 element = 4"
        );
        assert_eq!(frames[0][12], 0x08, "byte count must be 8 (4 registers)");
        assert_eq!(frames[0].len(), 21, "frame carries only 4 registers");
    }

    /// A multi-register data type written through a WRITE_SINGLE_REGISTER
    /// (FC06) port is sent as one single-register request per register at
    /// consecutive addresses. C `writeInt32` loops
    /// `for (i=0; i<bufferLen; i++) doModbusIO(..., modbusAddress+i, buffer+i,
    /// ...)` (drvModbusAsyn.cpp:763-766). Boundary: an INT32_BE value writes
    /// two FC06 requests — high word at `addr`, low word at `addr+1` — not a
    /// single request that drops the second register.
    #[test]
    fn relative_write_single_register_loops_per_register() {
        // Two FC06 echoes, one per single-register write (txid increments).
        let echo_hi = tcp_response(1, &[0x01, 0x06, 0x00, 0x01, 0x12, 0x34]);
        let echo_lo = tcp_response(2, &[0x01, 0x06, 0x00, 0x02, 0x56, 0x78]);
        let transport = ReplayTransport::new(vec![Ok(echo_hi), Ok(echo_lo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(0, 4);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver =
            ModbusPortDriver::new("MB_REL_WSR_I32", cfg, LinkType::Tcp, Box::new(transport))
                .expect("relative config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::Int32Be.as_str())
            .expect("INT32_BE parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 1;
        driver
            .write_int32(&mut user, 0x1234_5678)
            .expect("single-register int32 write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(
            frames.len(),
            2,
            "two single-register requests, one per word"
        );
        // FC06 PDU: [unit, 0x06, addr_hi, addr_lo, val_hi, val_lo].
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x06, 0x00, 0x01, 0x12, 0x34],
            "first request writes the high word at addr 1"
        );
        assert_eq!(
            &frames[1][6..12],
            &[0x01, 0x06, 0x00, 0x02, 0x56, 0x78],
            "second request writes the low word at addr 2"
        );
    }

    /// Absolute-mode counterpart: the per-register FC06 loop also applies when
    /// `modbusAddress = offset` (C writeInt32 absolute branch,
    /// drvModbusAsyn.cpp:747-766) — two requests at the wire address and the
    /// next register.
    #[test]
    fn absolute_write_single_register_loops_per_register() {
        let echo_hi = tcp_response(1, &[0x01, 0x06, 0x01, 0x00, 0x12, 0x34]);
        let echo_lo = tcp_response(2, &[0x01, 0x06, 0x01, 0x01, 0x56, 0x78]);
        let transport = ReplayTransport::new(vec![Ok(echo_hi), Ok(echo_lo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(-1, 4);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver =
            ModbusPortDriver::new("MB_ABS_WSR_I32", cfg, LinkType::Tcp, Box::new(transport))
                .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::Int32Be.as_str())
            .expect("INT32_BE parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x0100;
        driver
            .write_int32(&mut user, 0x1234_5678)
            .expect("absolute single-register int32 write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(
            frames.len(),
            2,
            "two single-register requests, one per word"
        );
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x06, 0x01, 0x00, 0x12, 0x34],
            "first request writes the high word at wire addr 0x0100"
        );
        assert_eq!(
            &frames[1][6..12],
            &[0x01, 0x06, 0x01, 0x01, 0x56, 0x78],
            "second request writes the low word at wire addr 0x0101"
        );
    }

    /// C `writeInt32Array`/`writeFloat64Array` accept only write-multiple
    /// functions and return asynError otherwise (drvModbusAsyn.cpp:1422-1427 /
    /// 1252-1257). An array write on a WRITE_SINGLE_REGISTER port must error,
    /// not silently fan out per-register or drop registers.
    #[test]
    fn array_write_rejects_non_multiple_write_function() {
        let mut cfg = test_config(0, 8);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver =
            ModbusPortDriver::new("MB_WSR_ARR", cfg, LinkType::Tcp, Box::new(NullTransport))
                .expect("config must build");
        let i32_reason = driver
            .base
            .find_param(ModbusDataType::Int32Be.as_str())
            .expect("INT32_BE parameter must exist");
        let f64_reason = driver
            .base
            .find_param(ModbusDataType::Float64Le.as_str())
            .expect("FLOAT64_LE parameter must exist");
        let i32_user = AsynUser::new(i32_reason);
        let f64_user = AsynUser::new(f64_reason);
        assert!(
            driver.write_int32_array(&i32_user, &[1, 2, 3]).is_err(),
            "int32-array write on FC06 port must be rejected"
        );
        assert!(
            driver.write_float64_array(&f64_user, &[1.0, 2.0]).is_err(),
            "float64-array write on FC06 port must be rejected"
        );
    }

    /// C `writeUInt32Digital` (drvModbusAsyn.cpp:604) writes the value
    /// directly when `mask == 0 || mask == 0xFFFF`; only a partial mask does a
    /// read/modify/write. A full mask in absolute mode must therefore issue a
    /// single write request and no readback read.
    #[test]
    fn uint32_digital_full_mask_writes_directly_without_readback() {
        // One FC06 write echo; if a readback read were issued there would be a
        // second written frame (the read request) before the write.
        let echo = tcp_response(1, &[0x01, 0x06, 0x01, 0x00, 0xAB, 0xCD]);
        let transport = ReplayTransport::new(vec![Ok(echo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(-1, 4);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver =
            ModbusPortDriver::new("MB_ABS_WSR_DIG", cfg, LinkType::Tcp, Box::new(transport))
                .expect("absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0x0100;
        driver
            .write_uint32_digital(&mut user, 0xABCD, 0xFFFF)
            .expect("full-mask digital write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(
            frames.len(),
            1,
            "full mask writes directly, no readback read"
        );
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x06, 0x01, 0x00, 0xAB, 0xCD],
            "single FC06 write of the full value at the wire address"
        );
    }

    /// A partial mask does a fresh READ_HOLDING_REGISTERS at
    /// `modbusAddress + readbackOffset_` (drvModbusAsyn.cpp:608-616), merges
    /// the masked bits, then writes — in relative mode too. The base value must
    /// come from a wire read, not the polled buffer.
    #[test]
    fn uint32_digital_partial_mask_reads_then_writes() {
        // Read echo (current = 0xAB12), then the merged write echo.
        let read_echo = tcp_response(1, &[0x01, 0x03, 0x02, 0xAB, 0x12]);
        // merged = (0xAB12 & 0xFF00) | (0x00F0 & 0x00FF) = 0xABF0.
        let write_echo = tcp_response(2, &[0x01, 0x06, 0x00, 0x02, 0xAB, 0xF0]);
        let transport = ReplayTransport::new(vec![Ok(read_echo), Ok(write_echo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(0, 4);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver =
            ModbusPortDriver::new("MB_REL_WSR_DIG", cfg, LinkType::Tcp, Box::new(transport))
                .expect("relative config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 2;
        driver
            .write_uint32_digital(&mut user, 0x00F0, 0x00FF)
            .expect("partial-mask digital write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(
            frames.len(),
            2,
            "a fresh wire read precedes the masked write"
        );
        // FC03 read of one register at the readback wire address (start 0 +
        // offset 2 + readbackOffset 0).
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x03, 0x00, 0x02, 0x00, 0x01],
            "readback reads one holding register at the wire address"
        );
        // FC06 write of the merged value.
        assert_eq!(
            &frames[1][6..12],
            &[0x01, 0x06, 0x00, 0x02, 0xAB, 0xF0],
            "write carries (current & ~mask) | (value & mask)"
        );
    }

    /// R8-70 sibling: the masked-write readback can itself answer exception 05.
    /// C's readback destination is the local `epicsUInt16 data = value`
    /// (drvModbusAsyn.cpp:578) and exception 05 copies nothing into it (:2231),
    /// so the merge runs against the record's own value and the write carries it
    /// unchanged. Treating the empty response as a zero word (the old
    /// `unwrap_or(0)`) instead cleared every bit outside the mask.
    #[test]
    fn uint32_digital_partial_mask_readback_acknowledge_merges_the_written_value() {
        // The readback answers exception 05 (fcode 0x83), then the write echoes.
        // The value carries bits OUTSIDE the mask (0xAB00), which is what
        // separates C's behaviour (they survive, because the merge base is the
        // record's own value) from a zero merge base (they are cleared).
        let ack = tcp_response(1, &[0x01, 0x83, 0x05]);
        let write_echo = tcp_response(2, &[0x01, 0x06, 0x00, 0x02, 0xAB, 0x12]);
        let transport = ReplayTransport::new(vec![Ok(ack), Ok(write_echo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(0, 4);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver =
            ModbusPortDriver::new("MB_REL_WSR_ACK", cfg, LinkType::Tcp, Box::new(transport))
                .expect("relative config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 2;
        driver
            .write_uint32_digital(&mut user, 0xAB12, 0x00FF)
            .expect("an Acknowledge readback is a success, not an error");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 2, "the readback still precedes the write");
        // merged = (value & ~mask) | (value & mask) = value = 0xAB12. A zero
        // merge base would have written 0x0012, dropping the high byte.
        assert_eq!(
            &frames[1][6..12],
            &[0x01, 0x06, 0x00, 0x02, 0xAB, 0x12],
            "with nothing read back, the merge runs against the value C left in \
             its readback local — the record's own value"
        );
    }

    /// A WRITE_SINGLE_COIL port writes the value directly and ignores the mask
    /// (drvModbusAsyn.cpp:596-599) — it must never do a holding-register
    /// readback even with a partial mask.
    #[test]
    fn uint32_digital_coil_writes_directly_without_readback() {
        let echo = tcp_response(1, &[0x01, 0x05, 0x00, 0x05, 0xFF, 0x00]);
        let transport = ReplayTransport::new(vec![Ok(echo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(-1, 16);
        cfg.function = ModbusFunctionCode::WriteSingleCoil;
        let mut driver =
            ModbusPortDriver::new("MB_ABS_WSC_DIG", cfg, LinkType::Tcp, Box::new(transport))
                .expect("absolute coil config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 5;
        // Partial mask (1) would have triggered a readback under the old flat
        // logic; the coil path must ignore it.
        driver
            .write_uint32_digital(&mut user, 1, 1)
            .expect("coil digital write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "coil write is direct, no readback read");
        assert_eq!(
            &frames[0][6..12],
            &[0x01, 0x05, 0x00, 0x05, 0xFF, 0x00],
            "single FC05 coil write set ON"
        );
    }

    /// A function that cannot service a digital write (a read function here)
    /// returns an error, matching C's `default: asynError`
    /// (drvModbusAsyn.cpp:620-625) — it must not fall through to a write.
    #[test]
    fn uint32_digital_rejects_invalid_function() {
        // test_config defaults to ReadHoldingRegisters.
        let mut driver = ModbusPortDriver::new(
            "MB_DIG_RDFN",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("read config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 1;
        assert!(
            driver.write_uint32_digital(&mut user, 1, 1).is_err(),
            "digital write on a read-function port must be rejected"
        );
    }

    /// A Z-string (zero-terminated) octet write must place a terminating NUL
    /// register on the wire. C `writeOctet` (drvModbusAsyn.cpp:1519-1529) sizes
    /// the write at `getStringLen(maxChars + 1)`, so "Hi" through ZSTRING_HIGH
    /// writes three registers — 'H', 'i', and a NUL — not two.
    #[test]
    fn zstring_octet_write_appends_terminating_nul() {
        // FC16 echo: three registers written at address 0.
        let echo = tcp_response(1, &[0x01, 0x10, 0x00, 0x00, 0x00, 0x03]);
        let transport = ReplayTransport::new(vec![Ok(echo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(0, 8);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver =
            ModbusPortDriver::new("MB_ZSTR_W", cfg, LinkType::Tcp, Box::new(transport))
                .expect("relative string config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::ZStringHigh.as_str())
            .expect("ZSTRING_HIGH parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0;
        driver
            .write_octet(&mut user, b"Hi")
            .expect("zstring write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "one FC16 write request");
        // FC16 header: [unit, 0x10, addr_hi, addr_lo, qty_hi, qty_lo, byte_ct].
        assert_eq!(
            &frames[0][6..13],
            &[0x01, 0x10, 0x00, 0x00, 0x00, 0x03, 0x06],
            "quantity 3 registers, byte count 6 — includes the NUL register"
        );
        // 'H'=0x4800, 'i'=0x6900, NUL=0x0000 (ZSTRING_HIGH = char in high byte).
        assert_eq!(
            &frames[0][13..19],
            &[0x48, 0x00, 0x69, 0x00, 0x00, 0x00],
            "third register is the terminating NUL"
        );
    }

    /// A non-Z string octet write must NOT append a NUL — the distinction is
    /// the whole point of the Z-string types. "Hi" through STRING_HIGH writes
    /// exactly two registers.
    #[test]
    fn plain_string_octet_write_has_no_terminating_nul() {
        let echo = tcp_response(1, &[0x01, 0x10, 0x00, 0x00, 0x00, 0x02]);
        let transport = ReplayTransport::new(vec![Ok(echo)]);
        let written = transport.written_handle();
        let mut cfg = test_config(0, 8);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver = ModbusPortDriver::new("MB_STR_W", cfg, LinkType::Tcp, Box::new(transport))
            .expect("relative string config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::StringHigh.as_str())
            .expect("STRING_HIGH parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0;
        driver
            .write_octet(&mut user, b"Hi")
            .expect("plain string write must succeed");
        let frames = written.lock().unwrap();
        assert_eq!(frames.len(), 1, "one FC16 write request");
        assert_eq!(
            &frames[0][6..13],
            &[0x01, 0x10, 0x00, 0x00, 0x00, 0x02, 0x04],
            "quantity 2 registers, byte count 4 — no NUL register"
        );
        assert_eq!(
            &frames[0][13..17],
            &[0x48, 0x00, 0x69, 0x00],
            "two registers, no terminator"
        );
    }

    /// C `writeOctet` (drvModbusAsyn.cpp:1557-1562) accepts only the
    /// write-multiple-registers functions; a string write on any other
    /// function returns asynError.
    #[test]
    fn octet_write_rejects_non_multiple_register_function() {
        let mut cfg = test_config(0, 8);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver =
            ModbusPortDriver::new("MB_STR_BADFN", cfg, LinkType::Tcp, Box::new(NullTransport))
                .expect("config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::StringHigh.as_str())
            .expect("STRING_HIGH parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0;
        assert!(
            driver.write_octet(&mut user, b"Hi").is_err(),
            "string write on a single-register port must be rejected"
        );
    }

    /// An `addr` beyond `config.length` must yield a clean error from the
    /// read/write accessors, not an out-of-bounds panic on the engine buffer.
    #[test]
    fn out_of_range_addr_returns_error_not_panic() {
        let mut driver = ModbusPortDriver::new(
            "MB_OOR",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("non-absolute config must build");
        // UINT16 is a numeric data parameter; recover its reason.
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        // addr 100 is well past the 4-word buffer.
        user.addr = 100;

        assert!(driver.read_int32(&user).is_err());
        assert!(driver.read_int64(&user).is_err());
        assert!(driver.read_float64(&user).is_err());
        assert!(driver.read_uint32_digital(&user, 0).is_err());
        let mut sbuf = [0u8; 8];
        assert!(driver.read_octet(&user, &mut sbuf).is_err());

        let mut wuser = AsynUser::new(reason);
        wuser.addr = 100;
        assert!(driver.write_int32(&mut wuser, 1).is_err());
        assert!(driver.write_int64(&mut wuser, 1).is_err());
        assert!(driver.write_float64(&mut wuser, 1.0).is_err());
        assert!(driver.write_uint32_digital(&mut wuser, 1, 0).is_err());
        assert!(driver.write_octet(&mut wuser, b"hi").is_err());
    }

    /// A subscriber may bind an out-of-range offset (the interrupt registry
    /// does not range-check). `poll_cycle` walks the subscriber bindings, so it
    /// must skip such a binding via its bounds guard instead of indexing the
    /// engine buffer out of bounds and panicking.
    #[test]
    fn poll_cycle_skips_out_of_range_subscriber_binding_without_panic() {
        // ReadHoldingRegisters response for the 4-word buffer: slave 1, fc 3,
        // byte_count 8, four zero registers. The engine's first poll expects
        // TCP transaction id 1.
        let pdu = [0x01u8, 0x03, 0x08, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut driver = ModbusPortDriver::new(
            "MB_POLL_OOR",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("non-absolute config must build");
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");

        // Out-of-range read/write still fail cleanly at the accessor.
        let mut ruser = AsynUser::new(reason);
        ruser.addr = 100;
        assert!(driver.read_int32(&ruser).is_err());
        let mut wuser = AsynUser::new(reason);
        wuser.addr = 100;
        assert!(driver.write_int32(&mut wuser, 1).is_err());

        // Register an interrupt subscriber at the out-of-range offset, then run
        // a full poll cycle (engine poll succeeds, bindings iterated). The
        // 4-word buffer has no index 100, so poll_cycle's bounds guard must skip
        // the binding rather than panic.
        let (_sub, _rx) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(100),
                    iface: Some(InterfaceType::Int32),
                    ..Default::default()
                });
        driver
            .poll_cycle()
            .expect("poll_cycle must skip the out-of-range subscriber binding, not error");
    }

    /// BUG 1 regression — a statistics/control reason has no
    /// `reason_to_datatype` entry. `read_int32` must return the cached
    /// parameter value (C `readInt32` delegates `reason != P_Data` to
    /// `asynPortDriver::readInt32`), not error.
    #[test]
    fn read_int32_returns_cached_value_for_statistics_reason() {
        let mut driver = ModbusPortDriver::new(
            "MB_STATS_READ",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(NullTransport),
        )
        .expect("non-absolute config must build");

        // READ_OK is a statistics param — not a Modbus data parameter.
        let read_ok = driver.read_ok_reason;
        // Constructor seeds it to 0; reading it must succeed, not error.
        let user = AsynUser::new(read_ok);
        assert_eq!(
            driver
                .read_int32(&user)
                .expect("statistics reason must be readable"),
            0
        );

        // After the driver stages a new statistics value, the read reflects it.
        driver.base.set_int32_param(read_ok, 0, 42).unwrap();
        assert_eq!(driver.read_int32(&user).unwrap(), 42);

        // The other statistics counters are equally readable.
        for reason in [
            driver.write_ok_reason,
            driver.io_errors_reason,
            driver.last_io_reason,
            driver.max_io_reason,
        ] {
            let u = AsynUser::new(reason);
            assert!(
                driver.read_int32(&u).is_ok(),
                "statistics reason {reason} must be readable via read_int32"
            );
        }
    }

    /// BUG 2 regression — when no data record sits at asyn addr 0, a poll
    /// cycle must still flush addr 0's changed-param list so the statistics
    /// monitors post. Bind a data record at addr 2, then verify a poll cycle
    /// emits interrupt notifications for the statistics params at addr 0.
    #[test]
    fn poll_cycle_flushes_statistics_monitors_without_addr0_data_record() {
        // ReadHoldingRegisters response for the 4-word buffer.
        let pdu = [0x01u8, 0x03, 0x08, 0, 1, 0, 2, 0, 3, 0, 4];
        let mut driver = ModbusPortDriver::new(
            "MB_STATS_MON",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("non-absolute config must build");
        let read_ok_reason = driver.read_ok_reason;

        // No data record is bound anywhere — the statistics params at addr 0
        // must still post their monitors after a poll, because `publish_stats`
        // is independent of the per-record interrupt fan-out.
        let mut rx = driver.base.interrupts.subscribe_async();
        driver.poll_cycle().expect("poll_cycle must succeed");

        // The statistics params live at addr 0; their monitors must have
        // posted even though no data record is bound there.
        let mut saw_read_ok = false;
        while let Ok(iv) = rx.try_recv() {
            if iv.reason == read_ok_reason && iv.addr == 0 {
                saw_read_ok = true;
            }
        }
        assert!(
            saw_read_ok,
            "READ_OK statistics monitor must post at addr 0 after a poll cycle"
        );
    }

    /// R54: one Modbus offset feeds several asyn interfaces, each needing the
    /// value in its own type. A poll cycle must fire a separately-decoded value
    /// per interface — int32 / int64 / float64 / the raw uInt32Digital word —
    /// not one collapsed Float64. Port of `readPoller`'s per-interface fan-out
    /// (drvModbusAsyn.cpp:1700-1815): an asynUInt32Digital `bi` sees the raw
    /// register word while an asynInt32 `ai` sees the multi-register integer.
    #[test]
    fn poll_cycle_fires_per_interface_typed_values() {
        // 4 holding registers: 0x0001, 0x0002, 0x0003, 0x0004.
        let pdu = [0x01u8, 0x03, 0x08, 0, 1, 0, 2, 0, 3, 0, 4];
        let mut driver = ModbusPortDriver::new(
            "MB_PER_IFACE",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("non-absolute config must build");

        // Activate the INT32_LE reason at addr 0. As a 32-bit little-endian
        // value the two-register decode (0x0002_0001 = 131073) differs from the
        // raw word (regs[0] = 1) the uInt32Digital interface delivers — so the
        // per-interface decode is observable, not the same number typed four
        // ways. `datatype_of` is per-reason, so the value is decoded as
        // Int32Le even though the port's config datatype is UInt16.
        let reason = driver
            .base
            .find_param(ModbusDataType::Int32Le.as_str())
            .expect("INT32_LE parameter must exist");
        // A record bound at addr 0 puts the offset on the interrupt list; the
        // poller then fires every scalar interface for it. One mailbox
        // subscription suffices to enumerate the binding — the per-interface
        // fan-out below is unconditional, so the broadcast observer sees all
        // four typed values regardless of this subscriber's own iface.
        let (_sub, _rx_sub) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::Int32),
                    ..Default::default()
                });

        let mut rx = driver.base.interrupts.subscribe_async();
        driver.poll_cycle().expect("poll_cycle must succeed");

        let mut int32 = None;
        let mut int64 = None;
        let mut float64 = None;
        let mut uint32 = None;
        while let Ok(iv) = rx.try_recv() {
            if iv.reason != reason || iv.addr != 0 {
                continue;
            }
            match (iv.iface, &iv.value) {
                (Some(InterfaceType::Int32), ParamValue::Int32(v)) => int32 = Some(*v),
                (Some(InterfaceType::Int64), ParamValue::Int64(v)) => int64 = Some(*v),
                (Some(InterfaceType::Float64), ParamValue::Float64(v)) => float64 = Some(*v),
                (Some(InterfaceType::UInt32Digital), ParamValue::UInt32Digital(v)) => {
                    // This is the first (forced) cycle, so every bit is marked
                    // changed (`!0`) and no `@asynMask` gates it out. The
                    // per-offset masked-change cadence on later cycles is covered
                    // by `poll_cycle_uint32_digital_fires_only_on_per_offset_masked_change`.
                    assert_eq!(
                        iv.uint32_changed_mask, !0,
                        "the forced first cycle marks all bits changed"
                    );
                    uint32 = Some(*v);
                }
                other => panic!("unexpected per-interface fire: {other:?}"),
            }
        }

        assert_eq!(
            int32,
            Some(0x0002_0001),
            "asynInt32 must see the 32-bit Int32Le decode"
        );
        assert_eq!(
            int64,
            Some(0x0002_0001),
            "asynInt64 must see the widened integer"
        );
        assert_eq!(
            float64,
            Some(131073.0),
            "asynFloat64 must see the float decode"
        );
        assert_eq!(
            uint32,
            Some(1),
            "asynUInt32Digital must see the raw register word, not the collapsed value"
        );
    }

    /// R56: a `SCAN="I/O Intr"` asynInt32ArrayIn / asynFloat64ArrayIn waveform
    /// must update every poll. C `readPoller` fires the int32Array / float64Array
    /// interrupt lists with the whole register block decoded from the record's
    /// offset (drvModbusAsyn.cpp:1840-1886); the Rust poll fires the array
    /// interfaces too — but only when a record is bound (the subscriber-presence
    /// gate mirrors C iterating an empty interrupt list). This pins both the fire
    /// (with correct whole-block contents) and the gate (silent when unbound).
    #[test]
    fn poll_cycle_fires_array_interfaces_only_when_a_record_is_bound() {
        // 4 holding registers 0x0001..0x0004, replayed for two polls.
        let pdu = [0x01u8, 0x03, 0x08, 0, 1, 0, 2, 0, 3, 0, 4];
        let mut driver = ModbusPortDriver::new(
            "MB_ARRAY_IOINTR",
            test_config(0, 4),
            LinkType::Tcp,
            // Two polls: the framer increments the transaction ID per request
            // (1 then 2), so the replies must carry the matching txid or the
            // poll skips them as stale and times out.
            Box::new(ReplayTransport::new(vec![
                Ok(tcp_response(1, &pdu)),
                Ok(tcp_response(2, &pdu)),
            ])),
        )
        .expect("non-absolute config must build");

        // INT32_LE reason at addr 0 (rc=2, two registers per element), so the
        // 4-register block decodes to two elements. No prior read: the poller
        // fires purely from the registered subscribers, exactly as a real
        // SCAN="I/O Intr" waveform — which never reads on its own — relies on.
        let reason = driver
            .base
            .find_param(ModbusDataType::Int32Le.as_str())
            .expect("INT32_LE parameter must exist");

        // --- Poll 1: array records ARE bound (mailbox subscribers present). ---
        let (_sub_i, _rx_i) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::Int32Array),
                    ..Default::default()
                });
        let (_sub_f, _rx_f) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::Float64Array),
                    ..Default::default()
                });
        let mut rx = driver.base.interrupts.subscribe_async();
        driver.poll_cycle().expect("poll_cycle must succeed");

        let mut int32_arr = None;
        let mut float64_arr = None;
        while let Ok(iv) = rx.try_recv() {
            match (iv.iface, iv.value) {
                (Some(InterfaceType::Int32Array), ParamValue::Int32Array(a)) => {
                    int32_arr = Some(a.to_vec());
                }
                (Some(InterfaceType::Float64Array), ParamValue::Float64Array(a)) => {
                    float64_arr = Some(a.to_vec());
                }
                _ => {}
            }
        }
        // INT32_LE: regs[0..2]=[1,2]=0x0002_0001=131073, regs[2..4]=[3,4]=
        // 0x0004_0003=262147 — the whole block, one element per two registers.
        assert_eq!(
            int32_arr.as_deref(),
            Some(&[131073i32, 262147][..]),
            "asynInt32Array must get the whole-block per-element Int32Le decode"
        );
        assert_eq!(
            float64_arr.as_deref(),
            Some(&[131073.0f64, 262147.0][..]),
            "asynFloat64Array must get the float decode of the same block"
        );

        // --- Poll 2: no array record bound (subscribers dropped) → gate skips. ---
        drop(_sub_i);
        drop(_sub_f);
        drop(_rx_i);
        drop(_rx_f);
        let mut rx2 = driver.base.interrupts.subscribe_async();
        driver.poll_cycle().expect("second poll_cycle must succeed");
        while let Ok(iv) = rx2.try_recv() {
            assert!(
                !matches!(
                    iv.iface,
                    Some(InterfaceType::Int32Array) | Some(InterfaceType::Float64Array)
                ),
                "no array interface may fire when no array record is bound \
                 (subscriber-presence gate)"
            );
        }
    }

    /// Drain every buffered interrupt fire for `(reason, addr 0)`, in order.
    /// The R57 on-change tests inspect which interfaces fired each poll.
    fn drain_addr0_fires(
        rx: &mut tokio::sync::broadcast::Receiver<asyn_rs::interrupt::InterruptValue>,
        reason: usize,
    ) -> Vec<asyn_rs::interrupt::InterruptValue> {
        let mut out = Vec::new();
        while let Ok(iv) = rx.try_recv() {
            if iv.reason == reason && iv.addr == 0 {
                out.push(iv);
            }
        }
        out
    }

    fn count_iface(fires: &[asyn_rs::interrupt::InterruptValue], iface: InterfaceType) -> usize {
        fires.iter().filter(|iv| iv.iface == Some(iface)).count()
    }

    /// A 4-register ReadHoldingRegisters response PDU for the given words.
    fn regs_pdu(words: [u16; 4]) -> Vec<u8> {
        let mut p = vec![0x01u8, 0x03, 0x08];
        for w in words {
            p.push((w >> 8) as u8);
            p.push((w & 0xff) as u8);
        }
        p
    }

    /// R57: a `uInt32Digital` I/O-Intr record fires only on a per-offset masked
    /// change, mirroring C `readPoller` (drvModbusAsyn.cpp:1695-1707) — not every
    /// poll. The first cycle forces (changed mask `!0`); an unchanged word is
    /// suppressed even when a different offset changed (per-offset, not
    /// port-wide); a changed word fires carrying only the changed bits, so a
    /// record `@asynMask` that does not overlap them is gated out by the
    /// interrupt filter.
    #[test]
    fn poll_cycle_uint32_digital_fires_only_on_per_offset_masked_change() {
        let mut driver = ModbusPortDriver::new(
            "MB_U32D_ONCHANGE",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![
                Ok(tcp_response(1, &regs_pdu([1, 2, 3, 4]))), // word@0 = 1
                Ok(tcp_response(2, &regs_pdu([1, 9, 3, 4]))), // word@0 unchanged (only @1 changed)
                Ok(tcp_response(3, &regs_pdu([5, 9, 3, 4]))), // word@0 1 -> 5 (changed bits 0x0004)
            ])),
        )
        .expect("non-absolute config must build");

        // UInt16 reason (rc=1) at addr 0 — the fired word is regs[0]. A mailbox
        // subscriber enumerates the binding; the broadcast observer sees fires.
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let (_sub, _rx_sub) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::UInt32Digital),
                    ..Default::default()
                });
        let mut rx = driver.base.interrupts.subscribe_async();

        let u32d = |fires: &[asyn_rs::interrupt::InterruptValue]| -> Vec<(u32, u32)> {
            fires
                .iter()
                .filter_map(|iv| match (iv.iface, &iv.value) {
                    (Some(InterfaceType::UInt32Digital), ParamValue::UInt32Digital(v)) => {
                        Some((*v, iv.uint32_changed_mask))
                    }
                    _ => None,
                })
                .collect()
        };

        // Poll 1: force -> fires word=1 with all bits marked changed.
        driver.poll_cycle().expect("poll 1 must succeed");
        assert_eq!(
            u32d(&drain_addr0_fires(&mut rx, reason)),
            vec![(1, !0)],
            "first poll forces the uInt32Digital fire with all bits marked changed"
        );

        // Poll 2: word@0 unchanged though the block changed (regs[1]) -> no fire.
        driver.poll_cycle().expect("poll 2 must succeed");
        assert!(
            u32d(&drain_addr0_fires(&mut rx, reason)).is_empty(),
            "an unchanged word must not fire uInt32Digital even when another \
             offset changed (per-offset gate, not port-wide)"
        );

        // Poll 3: word@0 1 -> 5, changed bits = 1 ^ 5 = 0x0004 -> fires that mask.
        driver.poll_cycle().expect("poll 3 must succeed");
        assert_eq!(
            u32d(&drain_addr0_fires(&mut rx, reason)),
            vec![(5, 0x0004)],
            "a changed word fires carrying only the changed bits as the mask"
        );
    }

    /// R57: `int32Array` fires only on a port-wide change (C `forceCallback_ ||
    /// anyChanged`, drvModbusAsyn.cpp:1824), while the int32/int64/float64
    /// scalars AND `float64Array` fire every poll (ADC-averaging, :1714/1858).
    /// An unchanged second poll must therefore drop int32Array but keep the
    /// unconditional fires; a changed third poll fires int32Array again.
    #[test]
    fn poll_cycle_int32_array_gated_on_change_scalars_unconditional() {
        let mut driver = ModbusPortDriver::new(
            "MB_ARR_ONCHANGE",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![
                Ok(tcp_response(1, &regs_pdu([1, 2, 3, 4]))),
                Ok(tcp_response(2, &regs_pdu([1, 2, 3, 4]))), // unchanged
                Ok(tcp_response(3, &regs_pdu([7, 2, 3, 4]))), // regs[0] changed
            ])),
        )
        .expect("non-absolute config must build");

        let reason = driver
            .base
            .find_param(ModbusDataType::Int32Le.as_str())
            .expect("INT32_LE parameter must exist");
        let (_si, _ri) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::Int32Array),
                    ..Default::default()
                });
        let (_sf, _rf) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::Float64Array),
                    ..Default::default()
                });
        let mut rx = driver.base.interrupts.subscribe_async();

        // Poll 1 (force): int32Array + float64Array + scalars all fire.
        driver.poll_cycle().expect("poll 1 must succeed");
        let f1 = drain_addr0_fires(&mut rx, reason);
        assert_eq!(
            count_iface(&f1, InterfaceType::Int32Array),
            1,
            "poll 1 int32Array"
        );
        assert_eq!(
            count_iface(&f1, InterfaceType::Float64Array),
            1,
            "poll 1 float64Array"
        );
        assert_eq!(
            count_iface(&f1, InterfaceType::Int32),
            1,
            "poll 1 int32 scalar"
        );

        // Poll 2 (unchanged): int32Array gated OFF; float64Array + scalars stay
        // on; uInt32Digital gated off (word unchanged).
        driver.poll_cycle().expect("poll 2 must succeed");
        let f2 = drain_addr0_fires(&mut rx, reason);
        assert_eq!(
            count_iface(&f2, InterfaceType::Int32Array),
            0,
            "int32Array must NOT fire on an unchanged poll (port-wide change gate)"
        );
        assert_eq!(
            count_iface(&f2, InterfaceType::Float64Array),
            1,
            "float64Array fires every poll (unconditional, ADC averaging)"
        );
        assert_eq!(
            count_iface(&f2, InterfaceType::Int32),
            1,
            "int32 scalar fires every poll (unconditional)"
        );
        assert_eq!(
            count_iface(&f2, InterfaceType::UInt32Digital),
            0,
            "uInt32Digital must NOT fire on an unchanged word"
        );

        // Poll 3 (regs[0] changed): int32Array fires again.
        driver.poll_cycle().expect("poll 3 must succeed");
        let f3 = drain_addr0_fires(&mut rx, reason);
        assert_eq!(
            count_iface(&f3, InterfaceType::Int32Array),
            1,
            "int32Array fires again once the block changes"
        );
    }

    /// R57: an `asynOctet` I/O-Intr record fires only when the port data changes
    /// (C `forceCallback_ || anyChanged`, drvModbusAsyn.cpp:1893) — not every
    /// poll. First poll forces; an identical second poll is silent; a changed
    /// third poll fires.
    #[test]
    fn poll_cycle_octet_fires_only_on_change() {
        // StringHigh: each register's high byte is one ASCII char.
        let str_pdu = |s: &[u8; 4]| -> Vec<u8> {
            let mut p = vec![0x01u8, 0x03, 0x08];
            for &c in s {
                p.push(c);
                p.push(0x00);
            }
            p
        };
        let mut cfg = test_config(0, 4);
        cfg.data_type = ModbusDataType::StringHigh;
        let mut driver = ModbusPortDriver::new(
            "MB_OCTET_ONCHANGE",
            cfg,
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![
                Ok(tcp_response(1, &str_pdu(b"ABCD"))),
                Ok(tcp_response(2, &str_pdu(b"ABCD"))), // unchanged
                Ok(tcp_response(3, &str_pdu(b"ABCE"))), // last char changed
            ])),
        )
        .expect("non-absolute string config must build");

        let reason = driver
            .base
            .find_param(ModbusDataType::StringHigh.as_str())
            .expect("STRING_HIGH parameter must exist");
        let (_sub, _rx_sub) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::Octet),
                    ..Default::default()
                });
        let mut rx = driver.base.interrupts.subscribe_async();

        driver.poll_cycle().expect("poll 1 must succeed");
        assert_eq!(
            count_iface(&drain_addr0_fires(&mut rx, reason), InterfaceType::Octet),
            1,
            "first poll forces the octet fire"
        );

        driver.poll_cycle().expect("poll 2 must succeed");
        assert_eq!(
            count_iface(&drain_addr0_fires(&mut rx, reason), InterfaceType::Octet),
            0,
            "an unchanged poll must not fire octet (port-wide change gate)"
        );

        driver.poll_cycle().expect("poll 3 must succeed");
        assert_eq!(
            count_iface(&drain_addr0_fires(&mut rx, reason), InterfaceType::Octet),
            1,
            "octet fires again once the string changes"
        );
    }

    /// R57: an I/O error forces the on-change callbacks even if the data is
    /// unchanged, mirroring C's `forceCallback_` on an I/O-status transition
    /// (drvModbusAsyn.cpp:1654) — once on the way down (the failed cycle fires
    /// the last good data with the failing status, R8-69) and once on the way
    /// back up, so the recovered cycle re-fires uInt32Digital with `!0` despite
    /// the word being identical.
    #[test]
    fn poll_cycle_io_error_forces_next_unchanged_cycle() {
        let mut driver = ModbusPortDriver::new(
            "MB_FORCE_RECOVER",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![
                Ok(tcp_response(1, &regs_pdu([1, 2, 3, 4]))), // poll 1 ok
                Err(ModbusError::Timeout),                    // poll 2 I/O error
                Ok(tcp_response(3, &regs_pdu([1, 2, 3, 4]))), // poll 3 ok, UNCHANGED
            ])),
        )
        .expect("non-absolute config must build");

        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let (_sub, _rx_sub) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::UInt32Digital),
                    ..Default::default()
                });
        let mut rx = driver.base.interrupts.subscribe_async();

        driver.poll_cycle().expect("poll 1 must succeed");
        let _ = drain_addr0_fires(&mut rx, reason); // discard the forced first fire

        // Poll 2 errors: the I/O-status transition forces a re-fire carrying the
        // last good word and the failing status (C :1654/1697), so the record
        // alarms instead of freezing.
        driver
            .poll_cycle()
            .expect("an I/O error reaches the records as a callback status, not as an Err");
        let f2 = drain_addr0_fires(&mut rx, reason);
        let u32d2: Vec<(u32, u32, AsynStatus)> = f2
            .iter()
            .filter_map(|iv| match (iv.iface, &iv.value) {
                (Some(InterfaceType::UInt32Digital), ParamValue::UInt32Digital(v)) => {
                    Some((*v, iv.uint32_changed_mask, iv.aux_status))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            u32d2,
            vec![(1, !0, AsynStatus::Timeout)],
            "the failed poll forces a re-fire of the last good word with the failing status"
        );

        // Poll 3: data identical to poll 1, but the error forced a re-fire.
        driver.poll_cycle().expect("poll 3 must succeed");
        let f3 = drain_addr0_fires(&mut rx, reason);
        let u32d: Vec<(u32, u32)> = f3
            .iter()
            .filter_map(|iv| match (iv.iface, &iv.value) {
                (Some(InterfaceType::UInt32Digital), ParamValue::UInt32Digital(v)) => {
                    Some((*v, iv.uint32_changed_mask))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            u32d,
            vec![(1, !0)],
            "post-I/O-error recovery forces a uInt32Digital re-fire (mask !0) even \
             though the word is unchanged"
        );
    }

    /// R57 finalizer: ANY aborted poll cycle must re-arm `force_callback` so the
    /// next clean cycle recovers — not only the engine-poll error, but a mid-loop
    /// decode error too. A subscriber bound at a tail offset whose datatype
    /// overruns the block (INT32_LE, rc=2, at the last register) aborts the
    /// per-offset decode `?`; the single finalizer in `poll_cycle` re-arms the
    /// force, so once the bad binding is gone the next cycle force-fires the
    /// on-change-gated interfaces even though the data never changed. Without the
    /// finalizer, `prev_data` would freeze and the gated fire would be lost.
    #[test]
    fn poll_cycle_mid_loop_decode_error_rearms_force_for_recovery() {
        let mut driver = ModbusPortDriver::new(
            "MB_MIDLOOP_ABORT",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![
                Ok(tcp_response(1, &regs_pdu([1, 2, 3, 4]))),
                Ok(tcp_response(2, &regs_pdu([1, 2, 3, 4]))), // unchanged
                Ok(tcp_response(3, &regs_pdu([1, 2, 3, 4]))), // unchanged
            ])),
        )
        .expect("non-absolute config must build");

        let u16_reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let i32_reason = driver
            .base
            .find_param(ModbusDataType::Int32Le.as_str())
            .expect("INT32_LE parameter must exist");

        // Good uInt32Digital binding at addr 0 (word = regs[0], always decodable).
        let (_good, _rg) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(u16_reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::UInt32Digital),
                    ..Default::default()
                });
        let mut rx = driver.base.interrupts.subscribe_async();

        let u32d = |fires: &[asyn_rs::interrupt::InterruptValue]| -> Vec<(u32, u32)> {
            fires
                .iter()
                .filter_map(|iv| match (iv.iface, &iv.value) {
                    (Some(InterfaceType::UInt32Digital), ParamValue::UInt32Digital(v)) => {
                        Some((*v, iv.uint32_changed_mask))
                    }
                    _ => None,
                })
                .collect()
        };

        // Poll 1: clean forced first cycle -> uInt32Digital@0 fires (1, !0);
        // force cleared, prev_data advanced.
        driver.poll_cycle().expect("poll 1 must succeed");
        assert_eq!(u32d(&drain_addr0_fires(&mut rx, u16_reason)), vec![(1, !0)]);

        // Bind an INT32_LE (rc=2) subscriber at addr 3: regs[3..] is one word, so
        // the per-offset read_int32 decode errors and aborts the cycle mid-loop.
        let (sub_bad, _rb) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(i32_reason),
                    addr: Some(3),
                    iface: Some(InterfaceType::Int32),
                    ..Default::default()
                });

        // Poll 2: the tail decode aborts the cycle (returns Err).
        driver
            .poll_cycle()
            .expect_err("a tail-offset decode overrun must abort poll 2");
        let _ = drain_addr0_fires(&mut rx, u16_reason);

        // Drop the bad binding so poll 3 is clean again.
        drop(sub_bad);

        // Poll 3: data identical to poll 1, but the aborted poll 2 re-armed the
        // force, so the on-change-gated uInt32Digital@0 fires anyway.
        driver.poll_cycle().expect("poll 3 must succeed");
        assert_eq!(
            u32d(&drain_addr0_fires(&mut rx, u16_reason)),
            vec![(1, !0)],
            "a mid-loop decode abort must re-arm force_callback so the next clean \
             cycle recovers (else prev_data freezes and the gated fire is lost)"
        );
    }

    #[test]
    fn parse_configure_args_named_datatype() {
        let (port, octet, cfg) = parse_configure_args(&args(vec![
            ArgValue::String("PLC1".into()),
            ArgValue::String("OCTET1".into()),
            ArgValue::Int(1),
            ArgValue::Int(3), // ReadHoldingRegisters
            ArgValue::Int(0),
            ArgValue::Int(10),
            ArgValue::String("INT32_LE".into()),
            ArgValue::Int(100),
            ArgValue::String("Koyo".into()),
        ]))
        .unwrap();
        assert_eq!(port, "PLC1");
        assert_eq!(octet, "OCTET1");
        assert_eq!(cfg.function, ModbusFunctionCode::ReadHoldingRegisters);
        assert_eq!(cfg.data_type, ModbusDataType::Int32Le);
        assert_eq!(cfg.length, 10);
        assert_eq!(cfg.poll_delay, Duration::from_millis(100));
        assert_eq!(cfg.readback_offset(), 0);
    }

    /// R53: `modbusInterposeConfig` reads the optional `timeoutMsec` and
    /// `writeDelayMsec` args (C modbusInterpose.c:134-136) instead of dropping
    /// them — a configured read timeout and inter-frame write delay must reach
    /// the transport.
    #[test]
    fn parse_interpose_args_reads_timeout_and_write_delay() {
        // All four args: timeout 500 ms, write delay 50 ms.
        let (port, s) = parse_interpose_args(&args(vec![
            ArgValue::String("OCTET1".into()),
            ArgValue::Int(0), // TCP
            ArgValue::Int(500),
            ArgValue::Int(50),
        ]))
        .unwrap();
        assert_eq!(port, "OCTET1");
        assert_eq!(s.link, LinkType::Tcp);
        assert_eq!(s.timeout, Duration::from_millis(500));
        assert_eq!(s.write_delay, Duration::from_millis(50));

        // Omitted/zero timeout falls back to DEFAULT_TIMEOUT (READ_TIMEOUT);
        // omitted/zero write delay is zero.
        let (_, s) = parse_interpose_args(&args(vec![
            ArgValue::String("OCTET2".into()),
            ArgValue::Int(0),
            ArgValue::Int(0),
        ]))
        .unwrap();
        assert_eq!(s.timeout, crate::driver::READ_TIMEOUT);
        assert_eq!(s.write_delay, Duration::ZERO);

        // Bare two-arg form: defaults for both.
        let (_, s) = parse_interpose_args(&args(vec![
            ArgValue::String("OCTET3".into()),
            ArgValue::Int(1), // RTU
        ]))
        .unwrap();
        assert_eq!(s.link, LinkType::Rtu);
        assert_eq!(s.timeout, crate::driver::READ_TIMEOUT);
        assert_eq!(s.write_delay, Duration::ZERO);

        // record/take round-trip preserves the settings.
        record_interpose(
            "OCTET_RT",
            InterposeSettings {
                link: LinkType::Rtu,
                timeout: Duration::from_millis(750),
                write_delay: Duration::from_millis(20),
            },
        );
        let got = take_interpose("OCTET_RT");
        assert_eq!(got.timeout, Duration::from_millis(750));
        assert_eq!(got.write_delay, Duration::from_millis(20));
        // An unconfigured port yields the defaults.
        let def = take_interpose("OCTET_NEVER_SET");
        assert_eq!(def.link, LinkType::Tcp);
        assert_eq!(def.timeout, crate::driver::READ_TIMEOUT);
        assert_eq!(def.write_delay, Duration::ZERO);
    }

    #[test]
    fn parse_configure_args_numeric_datatype() {
        // Data type index 4 = UInt16 (5th entry, 0-based).
        let (_, _, cfg) = parse_configure_args(&args(vec![
            ArgValue::String("P".into()),
            ArgValue::String("O".into()),
            ArgValue::Int(1),
            ArgValue::Int(4),
            ArgValue::Int(0),
            ArgValue::Int(5),
            ArgValue::String("4".into()),
            ArgValue::Int(0),
        ]))
        .unwrap();
        assert_eq!(cfg.data_type, ModbusDataType::UInt16);
    }

    #[test]
    fn parse_configure_args_rejects_bad_function() {
        let err = parse_configure_args(&args(vec![
            ArgValue::String("P".into()),
            ArgValue::String("O".into()),
            ArgValue::Int(1),
            ArgValue::Int(99),
            ArgValue::Int(0),
            ArgValue::Int(5),
            ArgValue::String("UINT16".into()),
            ArgValue::Int(0),
        ]))
        .unwrap_err();
        assert!(err.contains("function"));
    }

    #[test]
    fn parse_configure_args_rejects_unknown_datatype() {
        let err = parse_configure_args(&args(vec![
            ArgValue::String("P".into()),
            ArgValue::String("O".into()),
            ArgValue::Int(1),
            ArgValue::Int(3),
            ArgValue::Int(0),
            ArgValue::Int(5),
            ArgValue::String("NONSENSE".into()),
            ArgValue::Int(0),
        ]))
        .unwrap_err();
        assert!(err.contains("data type"));
    }
    // ---- R7-63: scalar writes must not stage into the register cache ----

    /// C `writeInt32` (drvModbusAsyn.cpp:748-776) converts the value into a
    /// LOCAL `epicsUInt16 buffer[4]` and sends that; `data_` — the register
    /// cache the poller fills and `readInt32` (`:541,:550`) serves — is never
    /// touched. So a read record on a write port keeps returning the
    /// last-polled / init-read value after a write, not the just-written one.
    #[test]
    fn relative_scalar_write_leaves_the_register_cache_untouched() {
        // WriteMultipleRegisters echo response.
        let pdu = [0x01u8, 0x10, 0x00, 0x00, 0x00, 0x01];
        let mut cfg = test_config(0, 4);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver = ModbusPortDriver::new(
            "MB_WR_NOSTAGE",
            cfg,
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("config must build");

        // Stand in for what the port's init read-once / poller left behind.
        driver.engine.data_mut()[0] = 0x1111;

        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0;
        driver
            .write_int32(&mut user, 0x2222)
            .expect("scalar write must succeed");

        assert_eq!(
            driver.engine.data()[0],
            0x1111,
            "C's scalar write converts into a local buffer; the cache keeps the polled value"
        );
        assert_eq!(
            driver.read_int32(&user).expect("cached read"),
            0x1111,
            "a read on the write port is served the polled value, not the written one"
        );
    }

    /// The uint32-digital scalar write is the same C shape (`epicsUInt16 data
    /// = value;` at drvModbusAsyn.cpp:578, sent from that local) — no staging.
    #[test]
    fn relative_uint32_digital_write_leaves_the_register_cache_untouched() {
        let pdu = [0x01u8, 0x06, 0x00, 0x00, 0x00, 0x0f];
        let mut cfg = test_config(0, 4);
        cfg.function = ModbusFunctionCode::WriteSingleRegister;
        let mut driver = ModbusPortDriver::new(
            "MB_WR_DIG_NOSTAGE",
            cfg,
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("config must build");
        driver.engine.data_mut()[0] = 0x1111;

        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0;
        // mask 0xFFFF -> straight write, no read/modify/write.
        driver
            .write_uint32_digital(&mut user, 0x000f, 0xffff)
            .expect("digital write must succeed");

        assert_eq!(driver.engine.data()[0], 0x1111, "cache untouched");
    }

    /// Control: the ARRAY write does stage. C `writeInt32Array`
    /// (drvModbusAsyn.cpp:1402) converts straight into `data_` and transmits
    /// from it (`dataAddress = data_ + offset`), so the cache carries the
    /// written registers.
    #[test]
    fn relative_array_write_stages_into_the_register_cache() {
        let pdu = [0x01u8, 0x10, 0x00, 0x00, 0x00, 0x02];
        let mut cfg = test_config(0, 4);
        cfg.function = ModbusFunctionCode::WriteMultipleRegisters;
        let mut driver = ModbusPortDriver::new(
            "MB_WR_ARR_STAGE",
            cfg,
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![Ok(tcp_response(1, &pdu))])),
        )
        .expect("config must build");
        driver.engine.data_mut()[0] = 0x1111;
        driver.engine.data_mut()[1] = 0x1111;

        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let mut user = AsynUser::new(reason);
        user.addr = 0;
        driver
            .write_int32_array(&user, &[0x2222, 0x3333])
            .expect("array write must succeed");

        assert_eq!(driver.engine.data()[0], 0x2222, "array write stages reg 0");
        assert_eq!(driver.engine.data()[1], 0x3333, "array write stages reg 1");
    }

    /// R8-69: the poll cycle's I/O status is port state that reaches the records
    /// as their callback status — it is never a reason to stop delivering.
    /// Walks C `readPoller`'s full status state machine (drvModbusAsyn.cpp:
    /// 1638-1655, :1928-1934) across four cycles:
    ///
    /// 1. clean poll → callbacks carry `Success` and the fresh data;
    /// 2. I/O failure (transition) → `forceCallback_` set, callbacks still fire,
    ///    carrying the failing status and the last good data (records alarm);
    /// 3. I/O failure again (persistent) → callbacks skipped, 1.0 s backoff asked
    ///    for, no `Err` out of the cycle;
    /// 4. link restored → status transition forces the callbacks again, now with
    ///    `Success` and the fresh data (records recover).
    #[test]
    fn poll_cycle_delivers_io_error_status_to_records_and_recovers() {
        let good1 = [0x01u8, 0x03, 0x08, 0, 1, 0, 2, 0, 3, 0, 4];
        let good2 = [0x01u8, 0x03, 0x08, 0, 9, 0, 8, 0, 7, 0, 6];
        // Cycle 1 succeeds, cycles 2 and 3 time out, cycle 4 succeeds. The
        // transaction id advances on every request, failed ones included.
        let mut driver = ModbusPortDriver::new(
            "MB_IO_ERR",
            test_config(0, 4),
            LinkType::Tcp,
            Box::new(ReplayTransport::new(vec![
                Ok(tcp_response(1, &good1)),
                Err(ModbusError::Timeout),
                Err(ModbusError::Timeout),
                Ok(tcp_response(4, &good2)),
            ])),
        )
        .expect("non-absolute config must build");

        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let (_sub, _rx_sub) =
            driver
                .base
                .interrupts
                .register_interrupt_user(asyn_rs::interrupt::InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    iface: Some(InterfaceType::Int32),
                    ..Default::default()
                });
        let mut rx = driver.base.interrupts.subscribe_async();

        // The Int32 fires this cycle produced, as (value, aux_status).
        let int32_fires =
            |rx: &mut tokio::sync::broadcast::Receiver<asyn_rs::interrupt::InterruptValue>| {
                let mut out = Vec::new();
                while let Ok(iv) = rx.try_recv() {
                    if iv.reason == reason
                        && iv.addr == 0
                        && iv.iface == Some(InterfaceType::Int32)
                        && let ParamValue::Int32(v) = iv.value
                    {
                        out.push((v, iv.aux_status));
                    }
                }
                out
            };

        // 1. Clean poll.
        driver.poll_cycle().expect("clean poll must succeed");
        assert_eq!(
            int32_fires(&mut rx),
            vec![(1, AsynStatus::Success)],
            "a clean cycle delivers the fresh value with Success status"
        );

        // 2. First failure — an I/O-status transition. C fires every interrupt
        // list with `auxStatus = ioStatus_` (:1697/1738), so the record alarms
        // (READ/INVALID) while keeping the last good value.
        driver
            .poll_cycle()
            .expect("an I/O error is delivered to the records, not returned as a request failure");
        assert_eq!(
            int32_fires(&mut rx),
            vec![(1, AsynStatus::Timeout)],
            "the error transition fires the last good data with the failing status"
        );
        assert!(
            !driver.poll_backoff.load(Ordering::Relaxed),
            "the transition cycle does not back off — C only sleeps once the error persists"
        );

        // 3. Second failure — persistent. C skips the callbacks entirely and
        // sleeps 1.0 s (:1646-1651); the records already carry the alarm.
        driver
            .poll_cycle()
            .expect("a persistent I/O error must not end the cycle in an error either");
        assert!(
            int32_fires(&mut rx).is_empty(),
            "a persistent error fires no callbacks"
        );
        assert!(
            driver.poll_backoff.load(Ordering::Relaxed),
            "a persistent error asks the poller for C's 1.0 s backoff"
        );
        assert_eq!(
            driver.engine.stats.io_errors, 2,
            "both failed polls counted as I/O errors"
        );

        // 4. Link restored — the status transition forces the callbacks.
        driver
            .poll_cycle()
            .expect("the recovered poll must succeed");
        assert_eq!(
            int32_fires(&mut rx),
            vec![(9, AsynStatus::Success)],
            "recovery re-fires every record with the fresh value and Success status"
        );
    }

    /// R8-69: the read-poller task survives Modbus I/O errors. On the unfixed
    /// tree the first failing poll broke the loop and the port never polled
    /// again (every I/O-Intr record frozen until an IOC restart). C `readPoller`
    /// exits only on `modbusExiting_` (drvModbusAsyn.cpp:1637) — here, only when
    /// the port actor is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_poller_keeps_polling_through_io_errors() {
        let good = [0x01u8, 0x03, 0x08, 0, 1, 0, 2, 0, 3, 0, 4];
        // Two failed polls (the second is persistent → 1.0 s backoff), then the
        // link comes back and every later poll succeeds.
        let mut responses: Vec<crate::error::ModbusResult<Vec<u8>>> =
            vec![Err(ModbusError::Timeout), Err(ModbusError::Timeout)];
        for txid in 3..=12u16 {
            responses.push(Ok(tcp_response(txid, &good)));
        }
        let mut config = test_config(0, 4);
        config.poll_delay = Duration::from_millis(20);
        let driver = ModbusPortDriver::new(
            "MB_POLLER_LIVE",
            config,
            LinkType::Tcp,
            Box::new(ReplayTransport::new(responses)),
        )
        .expect("non-absolute config must build");
        let read_reason = driver.read_reason;
        let read_ok_reason = driver.read_ok_reason;
        let poll_delay = driver.poll_delay.clone();
        let poll_wake = driver.poll_wake.clone();
        let poll_backoff = driver.poll_backoff.clone();

        let (runtime, _jh) = create_port_runtime(driver, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        let handle = runtime.port_handle().clone();
        let poller = tokio::spawn(read_poller(
            handle.clone(),
            read_reason,
            poll_delay,
            poll_wake,
            poll_backoff,
        ));

        // The two failures cost the 1.0 s persistent-error backoff; after it the
        // recovered link must produce successful reads. A dead poller reads 0.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let read_ok = handle
            .read_int32(read_ok_reason, 0)
            .await
            .expect("READ_OK must be readable");
        assert!(
            read_ok > 0,
            "the poller must keep polling after the I/O errors and recover — READ_OK={read_ok}"
        );
        assert!(!poller.is_finished(), "the poller task must still be alive");

        // Dropping the port runtime closes the actor — the poller's only exit.
        drop(runtime);
        tokio::time::timeout(Duration::from_secs(2), poller)
            .await
            .expect("the poller must exit once the port actor is gone")
            .expect("the poller task must not panic");
    }
}
