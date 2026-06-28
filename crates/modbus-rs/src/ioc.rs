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
//! Per the confirmed `drvUser` model, the C `pasynUser->drvUser` per-record
//! `{dataType, len}` struct has no asyn-rs equivalent: the data type is
//! encoded in the reason and the optional `=N` string length is dropped — a
//! string record's length comes from its own record buffer (`NELM`).
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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::sync_io::SyncIOHandle;
use asyn_rs::trace::TraceManager;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::iocsh::registry::*;

use crate::datatype::{self, ALL_DATA_TYPES, ModbusDataType};
use crate::driver::{ModbusConfig, ModbusEngine, ModbusFunctionCode, OctetTransport};
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
}

impl SyncIoTransport {
    /// Wrap a sync-I/O handle to the underlying octet port.
    pub fn new(handle: SyncIOHandle) -> Self {
        Self { handle }
    }
}

impl OctetTransport for SyncIoTransport {
    fn write_frame(&mut self, data: &[u8]) -> crate::error::ModbusResult<()> {
        self.handle
            .write_octet(0, data)
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
    /// `(reason, addr)` pairs touched by a record — discovered on first
    /// access, then refreshed every poll. Replaces the C `interruptStart`
    /// client enumeration.
    active: HashSet<(usize, i32)>,
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
        base.create_param(PARAM_POLL_DELAY, ParamType::Float64)?;
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

        Ok(Self {
            base,
            engine,
            transport,
            reason_to_datatype,
            active: HashSet::new(),
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

    /// Register bits of a record on first access so the poller refreshes it.
    fn touch(&mut self, reason: usize, addr: i32) {
        self.active.insert((reason, addr));
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
        self.engine.poll(self.transport.as_mut()).map_err(to_asyn)?;

        let active: Vec<(usize, i32)> = self.active.iter().copied().collect();
        let mut addrs: HashSet<i32> = HashSet::new();
        for (reason, addr) in active {
            let Ok(dt) = self.datatype_of(reason) else {
                continue;
            };
            // Defense-in-depth: an out-of-range `addr` must never index the
            // engine buffer. Accessors check the offset before `touch`, so
            // `self.active` should hold only valid addrs — but a bad addr
            // here is skipped, not allowed to panic.
            let data = self.engine.data();
            if addr < 0 || addr as usize >= data.len() {
                continue;
            }
            let regs = &data[addr as usize..];
            if dt.is_string() {
                let (bytes, _) =
                    datatype::read_string(dt, regs, regs.len() * 2).map_err(to_asyn)?;
                let s = String::from_utf8_lossy(&bytes).into_owned();
                self.base.set_string_param(reason, addr, s)?;
            } else {
                let (v, _) = datatype::read_float(dt, regs).map_err(to_asyn)?;
                self.base.set_float64_param(reason, addr, v)?;
            }
            self.base.mark_param_changed(reason, addr)?;
            addrs.insert(addr);
        }
        self.publish_stats()?;
        // The statistics/control params are all set at asyn addr 0 (their
        // `statistics.template` records bind `@asyn($(PORT) 0)`). Their
        // changed-param list lives in addr 0's bucket, so flush addr 0 every
        // cycle regardless of whether a data record happens to sit there —
        // otherwise the statistics monitors never post.
        self.base.call_param_callbacks(0)?;
        for addr in addrs {
            if addr != 0 {
                self.base.call_param_callbacks(addr)?;
            }
        }
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
    /// In relative mode the registers are also staged into the engine buffer
    /// at `offset` so a subsequent cached read reflects the write. In absolute
    /// mode the request targets `offset` as the wire address directly and
    /// nothing is staged — the buffer is only `config.length` words and the
    /// wire address can lie far outside it.
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
        let buf = self.engine.data_mut();
        for (i, &r) in regs.iter().enumerate() {
            if let Some(slot) = buf.get_mut(offset as usize + i) {
                *slot = r;
            }
        }
        Ok(())
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

impl PortDriver for ModbusPortDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn drv_user_create(&self, drv_info: &str) -> AsynResult<usize> {
        // Everything after a '=' is the optional string length — parsed and
        // dropped (see the module docs).
        let base_info = drv_info.split('=').next().unwrap_or(drv_info).trim();
        self.base
            .find_param(base_info)
            .ok_or_else(|| AsynError::ParamNotFound(drv_info.to_string()))
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
        self.touch(user.reason, user.addr);
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
        self.touch(user.reason, user.addr);
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
        self.touch(user.reason, user.addr);
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
        self.touch(user.reason, user.addr);
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
        self.touch(user.reason, user.addr);
        let regs = &self.engine.data()[user.addr as usize..];
        // String length comes from the record buffer (`NELM`).
        let (bytes, _) = datatype::read_string(dt, regs, buf.len()).map_err(to_asyn)?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
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
        self.flush_write(user.addr, &regs)
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
        self.flush_write(user.addr, &regs)
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
                    let regs = self
                        .engine
                        .do_modbus_io(
                            self.transport.as_mut(),
                            ModbusFunctionCode::ReadHoldingRegisters,
                            readback,
                            &[],
                            1,
                        )
                        .map_err(to_asyn)?;
                    let current = u32::from(regs.first().copied().unwrap_or(0));
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

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
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
        let (regs, _) = datatype::write_string(dt, bytes, budget).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        // Relative mode caches the value and fans out its monitor (see
        // `cache_write_numeric`); absolute mode has no parameter-table slot
        // for a wire address and no poller, so it skips the cache.
        if !self.is_absolute() {
            let s = String::from_utf8_lossy(data).into_owned();
            self.base.set_string_param(user.reason, user.addr, s)?;
            self.base.call_param_callbacks(user.addr)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// iocsh commands
// ---------------------------------------------------------------------------

/// Link types declared by `modbusInterposeConfig`, keyed by octet port name,
/// consumed by `drvModbusAsynConfigure`.
static PENDING_LINKS: Mutex<Option<HashMap<String, LinkType>>> = Mutex::new(None);

/// Port runtime handles — dropping one shuts the actor down, so they are kept.
static PORT_RUNTIMES: Mutex<Option<Vec<PortRuntimeHandle>>> = Mutex::new(None);

fn record_link(octet_port: &str, link: LinkType) {
    let mut g = PENDING_LINKS.lock().unwrap();
    g.get_or_insert_with(HashMap::new)
        .insert(octet_port.to_string(), link);
}

fn take_link(octet_port: &str) -> LinkType {
    PENDING_LINKS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(octet_port).copied())
        .unwrap_or(LinkType::Tcp)
}

fn keep_runtime(handle: PortRuntimeHandle) {
    let mut g = PORT_RUNTIMES.lock().unwrap();
    g.get_or_insert_with(Vec::new).push(handle);
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
            let port = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("portName required".into()),
            };
            let link = match &args[1] {
                ArgValue::Int(v) => {
                    LinkType::from_i32(*v as i32).ok_or_else(|| format!("invalid link type {v}"))?
                }
                _ => return Err("linkType required".into()),
            };
            record_link(&port, link);
            println!("modbusInterposeConfig: octet port '{port}' link={link:?}");
            Ok(CommandOutcome::Continue)
        },
    )
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
        let link = take_link(&octet_port);
        let poll_delay = config.poll_delay;
        // The read poller is started only for a relative-addressing read port.
        // An absolute-addressing port has no poller (drvModbusAsyn.cpp:1121,
        // `if (absoluteAddressing_) needReadThread = 0;`) — each record reads
        // its own wire address on access.
        let needs_poller = config.function.is_read() && !config.absolute_addressing();

        // Find the underlying octet port and build the framed transport.
        let entry = asyn_rs::asyn_record::get_port(&octet_port)
            .ok_or_else(|| format!("octet port '{octet_port}' not found"))?;
        let sync = SyncIOHandle::from_handle(entry.handle.clone(), 0, crate::driver::READ_TIMEOUT);
        let transport = Box::new(SyncIoTransport::new(sync));

        let driver = ModbusPortDriver::new(&port_name, config, link, transport)
            .map_err(|e| e.to_string())?;
        let read_reason = driver.read_reason;

        let (runtime, _jh) = create_port_runtime(driver, RuntimeConfig::default());
        let port_handle = runtime.port_handle().clone();
        asyn_rs::asyn_record::register_port(&port_name, port_handle.clone(), self.trace.clone());
        keep_runtime(runtime);

        println!("drvModbusAsynConfigure: port='{port_name}' octet='{octet_port}' link={link:?}");

        // Spawn the read poller — periodically triggers a poll cycle by
        // writing the MODBUS_READ parameter. Port of the `readPoller` thread.
        if needs_poller && !poll_delay.is_zero() {
            let poller_handle = port_handle.clone();
            self.handle.spawn(async move {
                loop {
                    tokio::time::sleep(poll_delay).await;
                    if poller_handle.write_int32(read_reason, 0, 1).await.is_err() {
                        break;
                    }
                }
            });
        }

        Ok(CommandOutcome::Continue)
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
        // No periodic poller in absolute mode, so the read must not register
        // the record in the `active` set.
        assert!(
            driver.active.is_empty(),
            "absolute reads must not touch the poller `active` set"
        );
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

        // A failed out-of-range access must never register the bad addr —
        // otherwise the next poll cycle would index the engine buffer out of
        // bounds and panic.
        assert!(
            !driver.active.iter().any(|&(_, a)| a == 100),
            "out-of-range addr must not be registered in `active`"
        );
    }

    /// After failed out-of-range reads and writes, `poll_cycle` must iterate
    /// `self.active` without panicking on a stale out-of-range addr. Guards
    /// against the touch-before-check regression where a bad addr was
    /// inserted into `active` and later indexed the engine buffer unchecked.
    #[test]
    fn poll_cycle_after_failed_out_of_range_access_does_not_panic() {
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

        // Out-of-range read on the 4-word buffer — must fail cleanly.
        let mut ruser = AsynUser::new(reason);
        ruser.addr = 100;
        assert!(driver.read_int32(&ruser).is_err());

        // Out-of-range write — must fail cleanly.
        let mut wuser = AsynUser::new(reason);
        wuser.addr = 100;
        assert!(driver.write_int32(&mut wuser, 1).is_err());

        // The bad addr must not have leaked into `active`.
        assert!(
            !driver.active.iter().any(|&(_, a)| a == 100),
            "out-of-range addr must not be registered in `active`"
        );

        // Defense-in-depth: even with a bad addr present in `active`, a full
        // `poll_cycle` (engine poll succeeds, then the active set is iterated)
        // must skip it instead of panicking on an out-of-bounds buffer index.
        driver.active.insert((reason, 100));
        driver
            .poll_cycle()
            .expect("poll_cycle must skip the stale out-of-range addr, not error");
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
        let reason = driver
            .base
            .find_param(ModbusDataType::UInt16.as_str())
            .expect("UINT16 parameter must exist");
        let read_ok_reason = driver.read_ok_reason;

        // The only data record is at addr 2 — nothing at addr 0.
        let mut ruser = AsynUser::new(reason);
        ruser.addr = 2;
        assert!(driver.read_int32(&ruser).is_ok());

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
}
