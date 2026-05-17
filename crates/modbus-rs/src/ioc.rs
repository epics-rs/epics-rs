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

    /// One acquisition cycle: read all registers, refresh every touched
    /// record's parameter and fire its I/O Intr callbacks, then publish the
    /// statistics counters. Port of the data half of `readPoller`.
    fn poll_cycle(&mut self) -> AsynResult<()> {
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
            let regs = &self.engine.data()[addr as usize..];
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
        for addr in addrs {
            self.base.call_param_callbacks(addr)?;
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
    /// configured write function, staging them in the engine buffer too.
    fn flush_write(&mut self, offset: i32, regs: &[u16]) -> AsynResult<()> {
        let function = self.engine.config().function;
        let addr = self.modbus_address(offset);
        self.engine
            .do_modbus_io(self.transport.as_mut(), function, addr, regs, regs.len())
            .map_err(to_asyn)?;
        let buf = self.engine.data_mut();
        for (i, &r) in regs.iter().enumerate() {
            if let Some(slot) = buf.get_mut(offset as usize + i) {
                *slot = r;
            }
        }
        Ok(())
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
        let dt = self.datatype_of(user.reason)?;
        self.touch(user.reason, user.addr);
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = &self.engine.data()[user.addr as usize..];
        Ok(datatype::read_int32(dt, regs).map_err(to_asyn)?.0)
    }

    fn read_int64(&mut self, user: &AsynUser) -> AsynResult<i64> {
        let dt = self.datatype_of(user.reason)?;
        self.touch(user.reason, user.addr);
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = &self.engine.data()[user.addr as usize..];
        Ok(datatype::read_int64(dt, regs).map_err(to_asyn)?.0)
    }

    fn read_float64(&mut self, user: &AsynUser) -> AsynResult<f64> {
        let dt = self.datatype_of(user.reason)?;
        self.touch(user.reason, user.addr);
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = &self.engine.data()[user.addr as usize..];
        Ok(datatype::read_float(dt, regs).map_err(to_asyn)?.0)
    }

    fn read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32> {
        self.touch(user.reason, user.addr);
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let raw = self.engine.data()[user.addr as usize] as u32;
        Ok(if mask == 0 { raw } else { raw & mask })
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let dt = self.datatype_of(user.reason)?;
        self.touch(user.reason, user.addr);
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
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
        let dt = self.datatype_of(user.reason)?;
        let rc = dt.register_count().max(1);
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
        let mut regs = Vec::new();
        for &v in data {
            regs.extend(datatype::write_int32(dt, v).map_err(to_asyn)?);
        }
        self.flush_write(user.addr, &regs)
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
        let dt = self.datatype_of(user.reason)?;
        let mut regs = Vec::new();
        for &v in data {
            regs.extend(datatype::write_float(dt, v).map_err(to_asyn)?);
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
            self.engine.stats.histogram_ms_per_bin = value.max(1) as u32;
            return Ok(());
        }
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = datatype::write_int32(dt, value).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        self.base
            .set_float64_param(user.reason, user.addr, value as f64)?;
        self.base.call_param_callbacks(user.addr)
    }

    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = datatype::write_int64(dt, value).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        self.base
            .set_float64_param(user.reason, user.addr, value as f64)?;
        self.base.call_param_callbacks(user.addr)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let regs = datatype::write_float(dt, value).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        self.base.set_float64_param(user.reason, user.addr, value)?;
        self.base.call_param_callbacks(user.addr)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        // ENABLE_HISTOGRAM toggles read-time histogram accumulation.
        if user.reason == self.enable_histogram_reason {
            self.engine.stats.histogram_enabled = value != 0;
            return Ok(());
        }
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        // Read-modify-write the masked bits of the register.
        let current = self.engine.data()[user.addr as usize] as u32;
        let merged = if mask == 0 {
            value
        } else {
            (current & !mask) | (value & mask)
        };
        self.flush_write(user.addr, &[merged as u16])?;
        Ok(())
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        let dt = self.datatype_of(user.reason)?;
        self.engine.check_offset(user.addr).map_err(to_asyn)?;
        let remaining = self.engine.config().length - user.addr as usize;
        let (regs, _) = datatype::write_string(dt, data, remaining).map_err(to_asyn)?;
        self.flush_write(user.addr, &regs)?;
        let s = String::from_utf8_lossy(data).into_owned();
        self.base.set_string_param(user.reason, user.addr, s)?;
        self.base.call_param_callbacks(user.addr)
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
        let is_read = config.function.is_read();

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
        if is_read && !poll_delay.is_zero() {
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
