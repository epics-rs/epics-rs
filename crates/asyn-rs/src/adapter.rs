use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;

use crate::error::AsynError;
use crate::interfaces::InterfaceType;
use crate::interrupt::{InterruptFilter, InterruptSubscription};
use crate::port_handle::{AsyncCompletionHandle, PortHandle};
use crate::request::{RequestOp, RequestResult};
use crate::user::AsynUser;

/// Parsed `@asyn(portName, addr, timeout) drvInfoString` link specification.
#[derive(Debug, Clone)]
pub struct AsynLink {
    pub port_name: String,
    pub addr: i32,
    pub timeout: Duration,
    pub drv_info: String,
}

/// Parse an asyn link string.
///
/// Accepted formats (comma or space delimited, matching C EPICS):
/// - `@asyn(portName) drvInfo`
/// - `@asyn(portName, addr) drvInfo`
/// - `@asyn(portName, addr, timeout) drvInfo`
/// - `@asyn(portName addr) drvInfo`
/// - `@asyn(portName addr timeout) drvInfo`
pub fn parse_asyn_link(s: &str) -> Result<AsynLink, AsynError> {
    let s = s.trim();
    let rest = s
        .strip_prefix("@asyn(")
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("must start with @asyn(: {s}")))?;

    let paren_end = rest
        .find(')')
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("missing closing paren: {s}")))?;

    let args_str = &rest[..paren_end];
    let drv_info = rest[paren_end + 1..].trim().to_string();

    // C EPICS pasynEpicsUtils->parseLink accepts both comma and space as delimiters.
    // Split by comma first; if only one part, try splitting by whitespace.
    let parts: Vec<&str> = if args_str.contains(',') {
        args_str.split(',').map(|p| p.trim()).collect()
    } else {
        args_str.split_whitespace().collect()
    };
    if parts.is_empty() || parts[0].is_empty() {
        return Err(AsynError::InvalidLinkSyntax("portName is required".into()));
    }

    let port_name = parts[0].to_string();
    let addr = if parts.len() > 1 {
        parts[1]
            .parse::<i32>()
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid addr: {}", parts[1])))?
    } else {
        0
    };
    let timeout = if parts.len() > 2 {
        let secs: f64 = parts[2]
            .parse()
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid timeout: {}", parts[2])))?;
        Duration::from_secs_f64(secs)
    } else {
        Duration::from_secs(1)
    };

    Ok(AsynLink {
        port_name,
        addr,
        timeout,
        drv_info,
    })
}

/// Parsed `@asynMask(portName, addr, mask, timeout) drvInfoString` link specification.
#[derive(Debug, Clone)]
pub struct AsynMaskLink {
    pub port_name: String,
    pub addr: i32,
    pub mask: u32,
    pub timeout: Duration,
    pub drv_info: String,
}

/// Parse an asynMask link string.
///
/// Format: `@asynMask(portName, addr, mask[, timeout]) drvInfo`
pub fn parse_asyn_mask_link(s: &str) -> Result<AsynMaskLink, AsynError> {
    let s = s.trim();
    let rest = s
        .strip_prefix("@asynMask(")
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("must start with @asynMask(: {s}")))?;

    let paren_end = rest
        .find(')')
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("missing closing paren: {s}")))?;

    let args_str = &rest[..paren_end];
    let drv_info = rest[paren_end + 1..].trim().to_string();

    let parts: Vec<&str> = args_str.split(',').map(|p| p.trim()).collect();
    if parts.len() < 3 {
        return Err(AsynError::InvalidLinkSyntax(
            "asynMask requires at least 3 arguments: portName, addr, mask".into(),
        ));
    }

    let port_name = parts[0].to_string();
    let addr = parts[1]
        .parse::<i32>()
        .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid addr: {}", parts[1])))?;

    // Parse mask: support hex (0x...) and decimal
    let mask_str = parts[2];
    let mask = if let Some(hex) = mask_str
        .strip_prefix("0x")
        .or_else(|| mask_str.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid mask: {mask_str}")))?
    } else {
        mask_str
            .parse::<u32>()
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid mask: {mask_str}")))?
    };

    let timeout = if parts.len() > 3 {
        let secs: f64 = parts[3]
            .parse()
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid timeout: {}", parts[3])))?;
        Duration::from_secs_f64(secs)
    } else {
        Duration::from_secs(1)
    };

    Ok(AsynMaskLink {
        port_name,
        addr,
        mask,
        timeout,
        drv_info,
    })
}

/// Adapter bridging an asyn-rs PortDriver to epics-base-rs DeviceSupport.
pub struct AsynDeviceSupport {
    handle: PortHandle,
    addr: i32,
    timeout: Duration,
    drv_info: String,
    reason: usize,
    reason_set: bool,
    iface_type: String,
    /// Typed interface (resolved from `iface_type` string at construction).
    iface: Option<InterfaceType>,
    /// Bit mask for UInt32Digital read/write. Default: 0xFFFFFFFF.
    mask: u32,
    last_alarm_status: u16,
    last_alarm_severity: u16,
    last_ts: Option<SystemTime>,
    record_name: String,
    scan: ScanType,
    /// Maximum number of array elements for array read operations.
    /// Default: 307200 (enough for 640x480 images).
    max_array_elements: usize,
    /// Buffer cap for `asynOctet` reads, including the trailing NUL.
    /// C asyn devAsynOctet.c:1103 passes `plsi->sizv` as the read
    /// limit for lsi/lso/printf — the byte the driver leaves at
    /// position `sizv` is overwritten with `\0` (line 1124). For
    /// stringin/stringout the C path is the fixed 40-byte record
    /// field; for waveform it uses NELM*FTVL. We pick the per-record
    /// SIZV at init time when available; otherwise fall back to the
    /// stringin-grade 256-byte default.
    octet_max_size: usize,
    /// If true, read back the current driver value during init (for output records).
    initial_readback: bool,
    /// `info(asyn:READBACK, "1")` flag, asyn upstream PRs #60 / #208.
    /// When set on an output record, the adapter activates the
    /// driver-callback path even when `SCAN != "I/O Intr"` so the
    /// record reprocesses on every external value change. Wired
    /// through `io_intr_receiver` together with the existing IoIntr
    /// scan path.
    asyn_readback: bool,
    /// If true, this is a write-only device support (e.g. asynOctetWrite).
    /// read() returns no-op to avoid overwriting the record's native value type.
    write_only: bool,
    /// RAII interrupt subscription — dropping unsubscribes.
    interrupt_sub: Option<InterruptSubscription>,
    /// Per-record ring buffer of interrupt values, FIFO-ordered. The
    /// I/O Intr forwarding task pushes; `read()` pops the oldest
    /// entry. C parity: `devAsynInt32.c::ringBuffer` (DEFAULT 10,
    /// configurable via `info("asyn:FIFO")`). Overflow policy:
    /// drop-oldest + `overflows++`; the record-process wakeup is
    /// **only** sent on a fresh-entry add (not on overflow
    /// overwrite) so the dbScan queue does not flood.
    interrupt_fifo: Arc<std::sync::Mutex<InterruptFifo>>,
}

/// Cached interrupt value with metadata for alarm/timestamp propagation.
struct CachedInterrupt {
    value: crate::param::ParamValue,
    timestamp: SystemTime,
}

/// Per-record ring buffer for I/O Intr callbacks. Mirrors C
/// `devAsynInt32.c::ringBuffer` + `ringSize` + `ringBufferOverflows`.
struct InterruptFifo {
    entries: std::collections::VecDeque<CachedInterrupt>,
    /// Maximum entries (`asyn:FIFO` info-tag, default
    /// `DEFAULT_RING_BUFFER_SIZE`).
    ring_size: usize,
    /// Running count of drop-oldest overwrites since the last
    /// successful pop. Logged + reset by `pop_callback_value`.
    overflows: u64,
}

/// C `DEFAULT_RING_BUFFER_SIZE` at `devAsynInt32.c:63`.
const DEFAULT_RING_BUFFER_SIZE: usize = 10;

impl InterruptFifo {
    fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::with_capacity(DEFAULT_RING_BUFFER_SIZE),
            ring_size: DEFAULT_RING_BUFFER_SIZE,
            overflows: 0,
        }
    }

    /// Producer-side: push a fresh interrupt entry, dropping the
    /// oldest if full. Returns `true` if this push corresponds to a
    /// *new* entry (and therefore the caller should schedule a record
    /// process); `false` on overflow (no scanIoRequest in C parity).
    fn push_with_overflow(&mut self, entry: CachedInterrupt) -> bool {
        if self.entries.len() >= self.ring_size {
            self.entries.pop_front();
            self.overflows += 1;
            self.entries.push_back(entry);
            false
        } else {
            self.entries.push_back(entry);
            true
        }
    }

    /// Consumer-side: pop the oldest entry. Returns `None` when
    /// empty. The `overflows` counter is reset by the caller via
    /// `take_overflows` so the trace warning fires once per drain
    /// (C `getCallbackValue` behaviour).
    fn pop(&mut self) -> Option<CachedInterrupt> {
        self.entries.pop_front()
    }

    /// Read and clear the overflow counter for a single warning emit.
    fn take_overflows(&mut self) -> u64 {
        std::mem::take(&mut self.overflows)
    }
}

impl AsynDeviceSupport {
    /// Create from a [`PortHandle`].
    pub fn from_handle(handle: PortHandle, link: AsynLink, iface_type: &str) -> Self {
        let iface = InterfaceType::from_asyn_name(iface_type);
        Self {
            handle,
            addr: link.addr,
            timeout: link.timeout,
            drv_info: link.drv_info,
            reason: 0,
            reason_set: false,
            iface_type: iface_type.to_string(),
            iface,
            mask: 0xFFFFFFFF,
            max_array_elements: 307200,
            octet_max_size: 256,
            last_alarm_status: 0,
            last_alarm_severity: 0,
            last_ts: None,
            record_name: String::new(),
            scan: ScanType::Passive,
            initial_readback: false,
            asyn_readback: false,
            write_only: false,
            interrupt_sub: None,
            interrupt_fifo: Arc::new(std::sync::Mutex::new(InterruptFifo::new())),
        }
    }

    /// Create with a typed interface from a [`PortHandle`].
    pub fn with_interface_handle(handle: PortHandle, link: AsynLink, iface: InterfaceType) -> Self {
        Self::from_handle(handle, link, iface.asyn_name())
    }

    /// Set the bit mask for UInt32Digital read/write operations.
    pub fn with_mask(mut self, mask: u32) -> Self {
        self.mask = mask;
        self
    }

    /// Enable initial readback: on init, read the current value from the driver
    /// and set it on the record (for output records).
    pub fn with_initial_readback(mut self) -> Self {
        self.initial_readback = true;
        self
    }

    /// Enable `asyn:READBACK` mode (asyn upstream PRs #60 / #208).
    ///
    /// Manual override. The framework auto-parses
    /// `info("asyn:READBACK", "...")` from the record's info map via
    /// [`Self::apply_record_info`] — `wire_device_support` and
    /// `IocBuilder` both call that hook automatically, so this manual
    /// setter is only needed for callers that construct an adapter
    /// outside the IocApplication wiring (e.g. unit tests, embedded
    /// scripts). C asyn parity: matches `asynDbGetInfo(pr,
    /// "asyn:READBACK")` calls in `devAsynInt32.c:329`,
    /// `devAsynFloat64.c:218`, `devAsynInt64.c:257`,
    /// `devAsynUInt32Digital.c:286`, `devAsynOctet.c:337`,
    /// `devAsynXXXArray.cpp:172`.
    pub fn set_asyn_readback(&mut self, on: bool) {
        self.asyn_readback = on;
    }

    /// Override initial-readback mode. The framework auto-parses
    /// `info("asyn:INITIAL_READBACK", "...")` via
    /// [`Self::apply_record_info`] (mirror of `asynDbGetInfo(precord,
    /// "asyn:INITIAL_READBACK")` at `devAsynOctet.c:357`).
    pub fn set_initial_readback(&mut self, on: bool) {
        self.initial_readback = on;
    }

    /// Override the I/O Intr ring buffer size. The framework
    /// auto-parses `info("asyn:FIFO", "<n>")` via
    /// [`Self::apply_record_info`]; this manual setter exists for
    /// callers outside the IocApplication wiring. C parity:
    /// `devAsynInt32.c::createRingBuffer` at line 354-365 — sets
    /// `pPvt->ringSize` to `atoi(info)` or `DEFAULT_RING_BUFFER_SIZE`
    /// when the info tag is absent.
    pub fn set_fifo_size(&mut self, n: usize) {
        let mut g = self.interrupt_fifo.lock().unwrap();
        g.ring_size = n.max(1);
        // Drop any over-capacity entries already buffered so the new
        // limit takes effect immediately. Counts the truncation as
        // overflows so the trace warning fires.
        while g.entries.len() > g.ring_size {
            g.entries.pop_front();
            g.overflows = g.overflows.saturating_add(1);
        }
    }

    /// Set the driver info string (used for `drv_user_create` during init).
    /// Allows record-name-based device support to configure the adapter
    /// in `set_record_info()` before `init()` runs.
    pub fn set_drv_info(&mut self, drv_info: &str) {
        self.drv_info = drv_info.to_string();
    }

    /// Set the interface type string (e.g. "asynInt32", "asynFloat64").
    /// Allows record-name-based device support to configure the adapter
    /// in `set_record_info()` before `init()` runs.
    pub fn set_iface_type(&mut self, iface_type: &str) {
        self.iface_type = iface_type.to_string();
        self.iface = InterfaceType::from_asyn_name(iface_type);
    }

    /// Set the param reason (index) directly, skipping `drv_user_create` during init.
    /// Use when the caller already knows the param index.
    pub fn set_reason(&mut self, reason: usize) {
        self.reason = reason;
        self.reason_set = true;
    }

    /// Get the param reason (index).
    pub fn reason(&self) -> usize {
        self.reason
    }

    /// Get the asyn address.
    pub fn addr(&self) -> i32 {
        self.addr
    }

    /// Get a reference to the underlying port handle.
    pub fn handle(&self) -> &PortHandle {
        &self.handle
    }

    /// Build a write request op from an EpicsValue (public wrapper for subclasses).
    pub fn write_op_pub(&self, val: &EpicsValue) -> Option<RequestOp> {
        self.write_op(val)
    }
}

/// Mirror of C asyn `computeShift(epicsUInt32 mask)` at
/// `devAsynUInt32Digital.c:627-636`: returns the position of the
/// lowest set bit (0..=32). A mask of 0 falls through the loop and
/// returns 32 — but mbbi/mbbo treat 0 as "use the full word", so
/// callers should guard before invoking.
fn compute_mask_shift(mask: u32) -> u32 {
    let mut bit: u32 = 1;
    for i in 0..32 {
        if (mask & bit) != 0 {
            return i;
        }
        bit <<= 1;
    }
    32
}

/// Parse an `info(...)` tag value as a boolean.
///
/// Truthy: non-empty and not `0` / `no` / `false` (case-insensitive).
/// Mirrors the broader EPICS convention; C asyn uses `atoi` directly
/// (`devAsynInt32.c:330` — `enableCallbacks = atoi(callbackString)`),
/// which only treats `"0"` / non-numeric as falsey. Our parse is a
/// strict superset for the documented `"1"` / `"0"` values and
/// additionally accepts the human-friendly `"Y"` / `"true"` forms.
fn parse_info_bool(raw: &str) -> bool {
    let v = raw.trim();
    !v.is_empty()
        && !v.eq_ignore_ascii_case("0")
        && !v.eq_ignore_ascii_case("no")
        && !v.eq_ignore_ascii_case("false")
}

fn asyn_to_ca_error(e: AsynError) -> CaError {
    CaError::Protocol(e.to_string())
}

/// Convert an asyn error to an EPICS (alarm status, alarm severity) pair
/// for the device-support READ path.
///
/// Mirrors C `asynStatusToEpicsAlarm` (asynEpicsUtils.c:234-266) with the
/// READ-path defaults the Int32/Float64/UInt32 device support pass:
/// `defaultStat = READ_ALARM`, `defaultSevr = INVALID_ALARM`
/// (devAsynInt32.c:844-846). Every non-success asyn status maps to
/// INVALID severity; only the condition code differs:
/// - Success      → (NO_ALARM, NO_ALARM)
/// - Timeout      → (TIMEOUT_ALARM, INVALID)   = (10, 3)
/// - Overflow     → (HW_LIMIT_ALARM, INVALID)  = (11, 3)
/// - Disconnected → (COMM_ALARM, INVALID)      = (9, 3)
/// - Disabled     → (DISABLE_ALARM, INVALID)   = (18, 3)
/// - Error/other  → (READ_ALARM, INVALID)      = (1, 3)
///
/// Pre-fix this mapping was wrong: Timeout / Overflow / Error all yielded
/// (7, 2) — condition 7 is STATE_ALARM, not READ_ALARM (which is 1), and
/// MAJOR(2) instead of INVALID(3) — and Disabled was lumped with
/// Disconnected as COMM_ALARM(9) instead of DISABLE_ALARM(18).
fn asyn_error_to_alarm(e: &AsynError) -> (u16, u16) {
    use crate::error::AsynStatus;
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;
    let invalid = AlarmSeverity::Invalid as u16;
    match e {
        AsynError::Status {
            status: AsynStatus::Success,
            ..
        } => (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm as u16),
        AsynError::Status {
            status: AsynStatus::Timeout,
            ..
        } => (alarm_status::TIMEOUT_ALARM, invalid),
        AsynError::Status {
            status: AsynStatus::Overflow,
            ..
        } => (alarm_status::HW_LIMIT_ALARM, invalid),
        AsynError::Status {
            status: AsynStatus::Disconnected,
            ..
        } => (alarm_status::COMM_ALARM, invalid),
        AsynError::Status {
            status: AsynStatus::Disabled,
            ..
        } => (alarm_status::DISABLE_ALARM, invalid),
        AsynError::Status {
            status: AsynStatus::Error,
            ..
        } => (alarm_status::READ_ALARM, invalid),
        // Non-status asyn errors take C's asynError/default branch.
        _ => (alarm_status::READ_ALARM, invalid),
    }
}

/// Convert an asyn ParamValue to an EpicsValue.
fn param_value_to_epics_value(pv: &crate::param::ParamValue) -> Option<EpicsValue> {
    use crate::param::ParamValue;
    match pv {
        ParamValue::Int32(v) => Some(EpicsValue::Long(*v)),
        ParamValue::Int64(v) => Some(EpicsValue::Double(*v as f64)),
        ParamValue::Float64(v) => Some(EpicsValue::Double(*v)),
        ParamValue::Octet(s) => Some(EpicsValue::String(s.clone().into())),
        ParamValue::UInt32Digital(v) => Some(EpicsValue::Long(*v as i32)),
        ParamValue::Enum { index, .. } => Some(EpicsValue::Enum(*index as u16)),
        ParamValue::Int8Array(a) => {
            Some(EpicsValue::CharArray(a.iter().map(|&x| x as u8).collect()))
        }
        ParamValue::Int16Array(a) => Some(EpicsValue::ShortArray(a.to_vec())),
        ParamValue::Int32Array(a) => Some(EpicsValue::LongArray(a.to_vec())),
        ParamValue::Int64Array(a) => {
            Some(EpicsValue::LongArray(a.iter().map(|&x| x as i32).collect()))
        }
        ParamValue::Float32Array(a) => Some(EpicsValue::FloatArray(a.to_vec())),
        ParamValue::Float64Array(a) => Some(EpicsValue::DoubleArray(a.to_vec())),
        _ => None,
    }
}

/// Bridges async `AsyncCompletionHandle` to epics-base-rs `WriteCompletion`.
struct AsynAsyncWriteCompletion {
    handle: parking_lot::Mutex<Option<AsyncCompletionHandle>>,
}

impl WriteCompletion for AsynAsyncWriteCompletion {
    fn wait(&self, timeout: Duration) -> CaResult<()> {
        if let Some(h) = self.handle.lock().take() {
            match h.wait_blocking(timeout) {
                Ok(_) => Ok(()),
                Err(e) => Err(CaError::Protocol(e.to_string())),
            }
        } else {
            Ok(())
        }
    }
}

impl AsynDeviceSupport {
    /// C parity for `devAsynInt32.c::initAi` (lines 821-828) +
    /// `convertAi` (lines 437-454): query the driver's int32 / int64
    /// bounds, then compute `ESLO = (EGUF - EGUL) / (high - low)` and
    /// `EOFF = (high*EGUL - low*EGUF) / (high - low)` and write
    /// them on the record. Caller has already verified the record
    /// exposes ESLO and the interface is `asynInt32` / `asynInt64`.
    fn apply_linear_eslo_eoff(&self, record: &mut dyn Record) {
        let op = if self.iface_type == "asynInt64" {
            RequestOp::GetBoundsInt64
        } else {
            RequestOp::GetBoundsInt32
        };
        let user = AsynUser::new(self.reason)
            .with_addr(self.addr)
            .with_timeout(self.timeout);
        let result = match self.handle.submit_blocking(op, user) {
            Ok(r) => r,
            Err(_) => return,
        };
        let (low, high) = match result.bounds {
            Some((l, h)) => (l as f64, h as f64),
            None => return,
        };
        // C parity: `if (deviceHigh != deviceLow)` (convertAi:444).
        // A degenerate driver that reports 0,0 (the "I don't know"
        // sentinel — matches the early-out at initAi:824) leaves the
        // existing ESLO/EOFF untouched.
        if (high - low).abs() < f64::EPSILON {
            return;
        }
        let eguf = match record.get_field("EGUF") {
            Some(EpicsValue::Double(v)) => v,
            _ => return,
        };
        let egul = match record.get_field("EGUL") {
            Some(EpicsValue::Double(v)) => v,
            _ => return,
        };
        let denom = high - low;
        let eslo = (eguf - egul) / denom;
        let eoff = (high * egul - low * eguf) / denom;
        let _ = record.put_field("ESLO", EpicsValue::Double(eslo));
        let _ = record.put_field("EOFF", EpicsValue::Double(eoff));
    }

    /// Build a `RequestOp` for reading the current interface type.
    fn read_op(&self) -> Option<RequestOp> {
        match self.iface_type.as_str() {
            "asynInt32" => Some(RequestOp::Int32Read),
            "asynInt64" => Some(RequestOp::Int64Read),
            "asynFloat64" => Some(RequestOp::Float64Read),
            "asynOctet" => Some(RequestOp::OctetRead {
                buf_size: self.octet_max_size,
            }),
            "asynUInt32Digital" => Some(RequestOp::UInt32DigitalRead { mask: self.mask }),
            "asynEnum" => Some(RequestOp::EnumRead),
            "asynInt8Array" => Some(RequestOp::Int8ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynInt16Array" => Some(RequestOp::Int16ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynInt32Array" => Some(RequestOp::Int32ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynInt64Array" => Some(RequestOp::Int64ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynFloat32Array" => Some(RequestOp::Float32ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynFloat64Array" => Some(RequestOp::Float64ArrayRead {
                max_elements: self.max_array_elements,
            }),
            _ => None,
        }
    }

    /// Extract an EpicsValue from a RequestResult based on interface type.
    fn result_to_value(&self, result: &RequestResult) -> Option<EpicsValue> {
        match self.iface_type.as_str() {
            "asynInt32" => result.int_val.map(EpicsValue::Long),
            "asynInt64" => result.int64_val.map(|v| EpicsValue::Double(v as f64)),
            "asynFloat64" => result.float_val.map(EpicsValue::Double),
            "asynOctet" => result.data.as_ref().map(|d| {
                let n = result.nbytes.min(d.len());
                EpicsValue::String(String::from_utf8_lossy(&d[..n]).into_owned().into())
            }),
            "asynUInt32Digital" => result.uint_val.map(|v| EpicsValue::Long(v as i32)),
            "asynEnum" => result.enum_index.map(|v| EpicsValue::Enum(v as u16)),
            "asynInt8Array" => result
                .int8_array
                .clone()
                .map(|v| EpicsValue::CharArray(v.iter().map(|&x| x as u8).collect())),
            "asynInt16Array" => result.int16_array.clone().map(EpicsValue::ShortArray),
            "asynInt32Array" => result.int32_array.clone().map(EpicsValue::LongArray),
            "asynInt64Array" => result
                .int64_array
                .clone()
                .map(|v| EpicsValue::LongArray(v.iter().map(|&x| x as i32).collect())),
            "asynFloat32Array" => result.float32_array.clone().map(EpicsValue::FloatArray),
            "asynFloat64Array" => result.float64_array.clone().map(EpicsValue::DoubleArray),
            _ => None,
        }
    }

    /// Build a `RequestOp` for writing an `EpicsValue` for the current interface type.
    fn write_op(&self, val: &EpicsValue) -> Option<RequestOp> {
        // First try exact match, then coerce numeric types to match the interface.
        // C EPICS always converts record VAL to the interface type (e.g. ao→double).
        match (self.iface_type.as_str(), val) {
            ("asynInt32", EpicsValue::Long(v)) => Some(RequestOp::Int32Write { value: *v }),
            ("asynInt32", EpicsValue::Enum(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt32", EpicsValue::Short(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt32", EpicsValue::Double(v)) => {
                Some(RequestOp::Int32Write { value: *v as i32 })
            }
            ("asynInt32", EpicsValue::Float(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt64", EpicsValue::Long(v)) => Some(RequestOp::Int64Write { value: *v as i64 }),
            ("asynInt64", EpicsValue::Double(v)) => {
                Some(RequestOp::Int64Write { value: *v as i64 })
            }
            ("asynFloat64", EpicsValue::Double(v)) => Some(RequestOp::Float64Write { value: *v }),
            ("asynFloat64", EpicsValue::Long(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynFloat64", EpicsValue::Float(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynFloat64", EpicsValue::Short(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynFloat64", EpicsValue::Enum(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynOctet", EpicsValue::String(s)) => Some(RequestOp::OctetWrite {
                data: s.as_bytes().to_vec(),
            }),
            ("asynOctet", EpicsValue::CharArray(data)) => {
                // Trim trailing nulls (waveform FTVL=CHAR pads to NELM)
                let len = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                Some(RequestOp::OctetWrite {
                    data: data[..len].to_vec(),
                })
            }
            // Coerce numeric types to octet (e.g. longout writing to NDArrayPort string param)
            ("asynOctet", v) => {
                let s = format!("{v}");
                Some(RequestOp::OctetWrite {
                    data: s.as_bytes().to_vec(),
                })
            }
            ("asynUInt32Digital", EpicsValue::Long(v)) => Some(RequestOp::UInt32DigitalWrite {
                value: *v as u32,
                mask: self.mask,
            }),
            ("asynUInt32Digital", EpicsValue::Enum(v)) => Some(RequestOp::UInt32DigitalWrite {
                value: *v as u32,
                mask: self.mask,
            }),
            ("asynEnum", EpicsValue::Long(v)) => Some(RequestOp::EnumWrite { index: *v as usize }),
            ("asynEnum", EpicsValue::Enum(v)) => Some(RequestOp::EnumWrite { index: *v as usize }),
            ("asynInt8Array", EpicsValue::CharArray(data)) => Some(RequestOp::Int8ArrayWrite {
                data: data.iter().map(|&x| x as i8).collect(),
            }),
            ("asynInt16Array", EpicsValue::ShortArray(data)) => {
                Some(RequestOp::Int16ArrayWrite { data: data.clone() })
            }
            ("asynInt32Array", EpicsValue::LongArray(data)) => {
                Some(RequestOp::Int32ArrayWrite { data: data.clone() })
            }
            ("asynInt64Array", EpicsValue::LongArray(data)) => Some(RequestOp::Int64ArrayWrite {
                data: data.iter().map(|&x| x as i64).collect(),
            }),
            ("asynFloat32Array", EpicsValue::FloatArray(data)) => {
                Some(RequestOp::Float32ArrayWrite { data: data.clone() })
            }
            ("asynFloat64Array", EpicsValue::DoubleArray(data)) => {
                Some(RequestOp::Float64ArrayWrite { data: data.clone() })
            }
            _ => None,
        }
    }
}

impl DeviceSupport for AsynDeviceSupport {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if !self.reason_set {
            match self.handle.drv_user_create_blocking(&self.drv_info) {
                Ok(reason) => {
                    self.reason = reason;
                }
                Err(e) => {
                    // Param not found — this record has no corresponding driver param.
                    eprintln!(
                        "[asyn] init FAILED: port='{}' drv_info='{}' err={e}",
                        self.handle.port_name(),
                        self.drv_info
                    );
                    self.reason_set = false;
                    return Ok(());
                }
            }
            self.reason_set = true;
        }

        // Read NELM from the record to set max_array_elements for array reads.
        if let Some(EpicsValue::Long(nelm)) = record.get_field("NELM") {
            if nelm > 0 {
                self.max_array_elements = nelm as usize;
            }
        }

        // Read SIZV from lsi/lso/printf records to size the asynOctet
        // read buffer. C parity: devAsynOctet.c:1103 passes
        // `plsi->sizv` to initCommon as the per-record buffer size;
        // the read path (line 1117-1124) then writes up to sizv-1
        // bytes and stuffs `\0` at the boundary. For stringin /
        // stringout (no SIZV) we keep the 256-byte default.
        if let Some(EpicsValue::Short(sizv)) = record.get_field("SIZV") {
            if sizv > 0 {
                self.octet_max_size = sizv as usize;
            }
        }

        // asynUInt32Digital MASK/SHFT propagation. C devAsynUInt32Digital.c
        // (init paths for mbbi/mbbo/mbbiDirect/mbboDirect at lines 881,
        // 925, 1010, 1054) sets:
        //
        //     pr->mask = pPvt->mask;
        //     pr->shft = computeShift(pPvt->mask);
        //
        // so the record's standard RVAL→VAL conversion shifts the
        // masked bits down to bit 0. Without this, a link like
        // `@asynMask(port,0,0xFF00) BITS` reads RVAL with bits 8-15
        // set but the record's shft stays 0 and VAL ends up
        // 65280-shaped instead of 0-255. Apply this regardless of
        // mask value — a fully-set 0xFFFFFFFF mask shifts by 0
        // (no-op) which is the correct contract for "every bit".
        if self.iface_type == "asynUInt32Digital" && self.mask != 0 {
            let shft = compute_mask_shift(self.mask);
            let _ = record.put_field("MASK", EpicsValue::Long(self.mask as i32));
            let _ = record.put_field("SHFT", EpicsValue::Short(shft as i16));
        }

        // ai/ao LINEAR ESLO/EOFF wiring.
        //
        // C devAsynInt32.c::initAi (line 822-828) / initAo / initAiAverage:
        //
        //     if (deviceLow == 0 && deviceHigh == 0) {
        //         pasynInt32SyncIO->getBounds(..., &deviceLow, &deviceHigh);
        //     }
        //     convertAi(pr, 1);   // line 437-454: ESLO/EOFF from EGUF/EGUL+bounds
        //
        // The bounds query is only meaningful for asynInt32/asynInt64
        // (asynFloat64 has no getBounds in C; mbbi/mbbo use the mask
        // path computed above). The record only applies the result
        // when LINR != NO_CONVERSION, but C writes ESLO/EOFF
        // unconditionally and lets record processing decide whether
        // to honour it.
        if (self.iface_type == "asynInt32" || self.iface_type == "asynInt64")
            && record.get_field("ESLO").is_some()
        {
            self.apply_linear_eslo_eoff(record);
        }

        if self.initial_readback {
            if let Some(op) = self.read_op() {
                let user = AsynUser::new(self.reason)
                    .with_addr(self.addr)
                    .with_timeout(self.timeout);
                if let Ok(result) = self.handle.submit_blocking(op, user) {
                    if let Some(val) = self.result_to_value(&result) {
                        let _ = record.set_val(val);
                    }
                }
            }
        }
        Ok(())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        if !self.reason_set {
            return Ok(DeviceReadOutcome::ok());
        }

        // Write-only (asynOctetWrite): waveform is an input record type so
        // process calls read(), not write(). Perform the write here instead.
        if self.write_only {
            if let Some(val) = record.val() {
                if let Some(op) = self.write_op(&val) {
                    let user = AsynUser::new(self.reason)
                        .with_addr(self.addr)
                        .with_timeout(self.timeout);
                    let _ = self.handle.submit_blocking(op, user);
                }
            }
            return Ok(DeviceReadOutcome::ok());
        }

        // For I/O Intr records, pop the oldest entry from the ring
        // buffer. C parity: `devAsynInt32.c::getCallbackValue` —
        // returns the next FIFO entry, logs+resets the overflow
        // counter on consume.
        if self.scan == ScanType::IoIntr {
            let (entry, overflows) = {
                let mut fifo = self.interrupt_fifo.lock().unwrap();
                (fifo.pop(), fifo.take_overflows())
            };
            if overflows > 0 {
                tracing::warn!(
                    target: "asyn_rs::adapter",
                    port = %self.handle.port_name(),
                    record = %self.record_name,
                    overflows = overflows,
                    "ring buffer overflows (C asyn ASYN_TRACE_WARNING)"
                );
            }
            if let Some(ci) = entry {
                if let Some(val) = param_value_to_epics_value(&ci.value) {
                    let _ = record.set_val(val);
                }
                self.last_ts = Some(ci.timestamp);
            }
            // Return computed() so the record skips its built-in
            // RVAL→VAL conversion and uses the value we just set.
            return Ok(DeviceReadOutcome::computed());
        }

        if let Some(op) = self.read_op() {
            let user = AsynUser::new(self.reason)
                .with_addr(self.addr)
                .with_timeout(self.timeout);
            match self.handle.submit_blocking(op, user) {
                Ok(result) => {
                    if let Some(val) = self.result_to_value(&result) {
                        let _ = record.set_val(val);
                    }
                    self.last_alarm_status = result.alarm_status;
                    self.last_alarm_severity = result.alarm_severity;
                    self.last_ts = result.timestamp;
                }
                Err(e) => {
                    // Convert asyn error to EPICS alarm (C parity: asynStatusToEpicsAlarm)
                    let (alarm_status, alarm_severity) = asyn_error_to_alarm(&e);
                    self.last_alarm_status = alarm_status;
                    self.last_alarm_severity = alarm_severity;
                }
            }
        }
        Ok(DeviceReadOutcome::computed())
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if !self.reason_set {
            return Ok(());
        }
        if let Some(val) = record.val() {
            if let Some(op) = self.write_op(&val) {
                let user = AsynUser::new(self.reason)
                    .with_addr(self.addr)
                    .with_timeout(self.timeout);
                self.handle
                    .submit_blocking(op, user)
                    .map_err(asyn_to_ca_error)?;
            }
        }
        Ok(())
    }

    fn dtyp(&self) -> &str {
        &self.iface_type
    }

    fn last_alarm(&self) -> Option<(u16, u16)> {
        if self.last_alarm_status == 0 && self.last_alarm_severity == 0 {
            None
        } else {
            Some((self.last_alarm_status, self.last_alarm_severity))
        }
    }

    fn last_timestamp(&self) -> Option<SystemTime> {
        self.last_ts
    }

    fn set_record_info(&mut self, name: &str, scan: ScanType) {
        self.record_name = name.to_string();
        self.scan = scan;
    }

    fn apply_record_info(&mut self, info: &std::collections::HashMap<String, String>) {
        // C parity: `asynDbGetInfo(pr, "asyn:READBACK")` +
        // `asynDbGetInfo(pr, "asyn:INITIAL_READBACK")` at
        // devAsynInt32.c:329, devAsynFloat64.c:218,
        // devAsynInt64.c:257, devAsynUInt32Digital.c:286,
        // devAsynOctet.c:337+357, devAsynXXXArray.cpp:172.
        // C semantics: a non-NULL info-string is fed through `atoi`,
        // any non-zero numeric result enables the flag — which means
        // "0", "0x0", "00" disable; "1", "Y" (atoi → 0!), garbage
        // strings (atoi → 0) disable. We use the broader EPICS-style
        // "truthy / falsey" parse here so values like "Y" or "true"
        // also work (they would NOT work under strict atoi parity,
        // but the broader parse is a strict superset for "1" / "0").
        if let Some(raw) = info.get("asyn:READBACK") {
            self.set_asyn_readback(parse_info_bool(raw));
        }
        if let Some(raw) = info.get("asyn:INITIAL_READBACK") {
            self.set_initial_readback(parse_info_bool(raw));
        }
        // C parity: `info("asyn:FIFO", "<n>")` at
        // devAsynInt32.c:361-362 — `atoi(sizeString)` overrides
        // DEFAULT_RING_BUFFER_SIZE (10). C uses raw `atoi`, which
        // returns 0 for unparseable input; we mirror that by
        // ignoring zero / negative values so the default isn't
        // accidentally clobbered by a typo.
        if let Some(raw) = info.get("asyn:FIFO") {
            if let Ok(n) = raw.trim().parse::<i64>() {
                if n > 0 {
                    self.set_fifo_size(n as usize);
                }
            }
        }
    }

    fn write_begin(
        &mut self,
        record: &mut dyn Record,
    ) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        let val = match record.val() {
            Some(v) => v,
            None => return Ok(None),
        };
        let op = match self.write_op(&val) {
            Some(op) => op,
            None => return Ok(None),
        };
        let user = AsynUser::new(self.reason)
            .with_addr(self.addr)
            .with_timeout(self.timeout);

        // For non-blocking ports, use synchronous submit to match C EPICS behavior:
        // the write completes within the same dbProcess call, so CP chain targets
        // see the updated value immediately. This prevents actor channel overflow
        // and stale reads during fast motor moves.
        if !self.handle.can_block() {
            let _ = self
                .handle
                .submit_blocking(op, user)
                .map_err(asyn_to_ca_error)?;
            return Ok(None); // completed synchronously, no async completion needed
        }

        let completion = self.handle.try_submit(op, user).map_err(asyn_to_ca_error)?;
        Ok(Some(Box::new(AsynAsyncWriteCompletion {
            handle: parking_lot::Mutex::new(Some(completion)),
        })))
    }

    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        // Activate the driver-callback path for either:
        //   1. records with `SCAN="I/O Intr"` (legacy behaviour), OR
        //   2. records flagged via `set_asyn_readback(true)` (asyn
        //      upstream PRs #60 / #208 — output records that follow
        //      driver-side changes regardless of SCAN).
        if !self.reason_set {
            return None;
        }
        if self.scan != ScanType::IoIntr && !self.asyn_readback {
            return None;
        }

        // C parity: a UInt32Digital record registers its @asynMask as the
        // interrupt mask (devAsynUInt32Digital.c:293/343); the driver fires
        // the callback only when `pInterrupt->mask & interruptMask`
        // (asynPortDriver.cpp:720) and delivers `pInterrupt->mask & value`
        // (asynPortDriver.cpp:729). Other interfaces carry no changed-bit
        // mask (uint32_changed_mask == 0), so a mask filter would gate every
        // interrupt out — restrict the gate (and the value masking below) to
        // UInt32Digital.
        let is_uint32 = self.iface_type == "asynUInt32Digital";
        let mask = self.mask;
        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.addr),
            uint32_mask: if is_uint32 { Some(mask) } else { None },
        };

        let (sub, mut intr_rx) = self.handle.interrupts().register_interrupt_user(filter);
        self.interrupt_sub = Some(sub);

        // Bridge mailbox-based InterruptReceiver to the mpsc<()> wakeup channel
        // consumed by setup_io_intr(). The mailbox already coalesces intermediate
        // updates, so no data is lost even if the record processes slowly.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let fifo = self.interrupt_fifo.clone();
        tokio::spawn(async move {
            while let Some(iv) = intr_rx.recv().await {
                // C parity (asynPortDriver.cpp:729 + devAsynUInt32Digital.c:464):
                // deliver `mask & value` for UInt32Digital so the I/O-Intr value
                // matches the polled read (RequestOp::UInt32DigitalRead { mask }).
                // interruptCallbackInput stores the already-masked value in the
                // ring buffer; mirror that here. Non-UInt32 values pass through.
                // C parity (asynPortDriver.cpp:729 + devAsynUInt32Digital.c:464):
                // deliver `mask & value` for UInt32Digital so the I/O-Intr value
                // matches the polled read (RequestOp::UInt32DigitalRead { mask }).
                // interruptCallbackInput stores the already-masked value in the
                // ring buffer; mirror that here. Non-UInt32 values pass through.
                let value = match iv.value {
                    crate::param::ParamValue::UInt32Digital(v) if is_uint32 => {
                        crate::param::ParamValue::UInt32Digital(v & mask)
                    }
                    other => other,
                };
                let entry = CachedInterrupt {
                    value,
                    timestamp: iv.timestamp,
                };
                // C parity (devAsynInt32.c:564-576):
                //   - On overflow (ring full), drop oldest +
                //     overflows++ and DO NOT call scanIoRequest.
                //     The already-pending process will pick up the
                //     newer tail; a duplicate request would just
                //     flood dbScan.
                //   - On normal add, request the record to process.
                let was_fresh_add = {
                    let mut g = fifo.lock().unwrap();
                    g.push_with_overflow(entry)
                };
                if was_fresh_add && tx.send(()).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
}

// ===== Universal asyn device support =====

/// Normalize array DTYP names by stripping "In"/"Out" direction suffixes.
///
/// C EPICS uses distinct DTYPs for input vs output array records
/// (e.g. `asynFloat64ArrayIn`, `asynFloat64ArrayOut`), but the underlying
/// asyn interface name is just `asynFloat64Array`. This function strips
/// the direction suffix so the adapter's `read_op()`/`write_op()` matchers
/// can find the correct interface.
/// Normalize asyn DTYP names to their base interface type.
///
/// C EPICS uses direction-specific DTYPs for some interfaces:
/// - `asynFloat64ArrayIn` / `asynFloat64ArrayOut` → `asynFloat64Array`
/// - `asynOctetRead` / `asynOctetWrite` → `asynOctet`
///
/// The underlying asyn interface is direction-agnostic.
fn normalize_asyn_dtyp(dtyp: &str) -> String {
    // Array direction suffixes: asynXxxArrayIn/Out → asynXxxArray
    if let Some(base) = dtyp.strip_suffix("In").or_else(|| dtyp.strip_suffix("Out")) {
        if base.ends_with("Array") {
            return base.to_string();
        }
    }
    // Octet direction suffixes: asynOctetRead/Write → asynOctet
    if dtyp == "asynOctetRead" || dtyp == "asynOctetWrite" {
        return "asynOctet".to_string();
    }
    dtyp.to_string()
}

/// Create a universal asyn device support factory.
///
/// Handles all standard asyn DTYPs (`asynInt32`, `asynFloat64`, `asynOctet`,
/// array types, etc.) by parsing `@asyn(PORT,ADDR,TIMEOUT)DRVINFO` links
/// and dispatching to the appropriate port driver.
///
/// During `init()`, `drv_user_create(drvInfo)` is called on the port, which
/// resolves the drvInfo string to a param index via `find_param()`. This
/// matches the C EPICS asyn device support behavior exactly.
///
/// Handles all records with `@asyn(PORT,...)` links. Register via
/// `register_asyn_device_support(app)` or `AdIoc` (which registers it
/// automatically).
///
/// ```ignore
/// app = asyn_rs::adapter::register_asyn_device_support(app);
/// ```
pub fn universal_asyn_factory(
    ctx: &epics_base_rs::server::ioc_app::DeviceSupportContext,
) -> Option<Box<dyn DeviceSupport>> {
    // Try @asyn() link in INP or OUT
    let (link_str, is_output) = if ctx.out.contains("@asyn") || ctx.out.contains("@asynMask") {
        (ctx.out, true)
    } else if ctx.inp.contains("@asyn") || ctx.inp.contains("@asynMask") {
        // asynOctetWrite uses INP field for output (C EPICS convention for waveform records)
        let is_write_dtyp = ctx.dtyp == "asynOctetWrite";
        (ctx.inp, is_write_dtyp)
    } else {
        return None;
    };

    // Parse the link
    let link = if link_str.contains("@asynMask") {
        let ml = parse_asyn_mask_link(link_str).ok()?;
        AsynLink {
            port_name: ml.port_name,
            addr: ml.addr,
            timeout: ml.timeout,
            drv_info: ml.drv_info,
        }
    } else {
        parse_asyn_link(link_str).ok()?
    };

    // Look up port in global registry
    let entry = crate::asyn_record::get_port(&link.port_name)?;

    // Normalize DTYP: strip "In"/"Out" suffixes from array types.
    // C EPICS uses DTYPs like "asynFloat64ArrayIn" / "asynFloat64ArrayOut"
    // but the underlying asyn interface is "asynFloat64Array".
    let dtyp = normalize_asyn_dtyp(ctx.dtyp);

    let mut adapter = AsynDeviceSupport::from_handle(entry.handle, link, &dtyp);

    if is_output {
        if ctx.dtyp == "asynOctetWrite" {
            // asynOctetWrite is write-only — no reads allowed.
            // Reading would replace waveform CharArray with String, breaking element_count.
            adapter.write_only = true;
        } else {
            // Output records: read back current driver value on init.
            // Mirrors C `initAo` / `initBo` / `initLongout` / `initMbbo`
            // which call `pasynManager->queueRequest(... ASYN_INIT ...)`
            // to pull the driver's current value into the record before
            // record processing starts.
            adapter = adapter.with_initial_readback();
        }
    }
    // Input records: do NOT auto-readback. C `initAi`/`initLongin`/
    // `initMbbi`/`initBi` (devAsynInt32.c:812+, similar for Float64 /
    // Int64 / UInt32Digital / Octet) only sets up the asynUser and
    // gets bounds — the first value comes from `processAi` (driven by
    // the scan task or an I/O Intr callback), not from a synchronous
    // read at init. The previous "matches C init_common() behavior"
    // comment was wrong: C devAsynOctet init_common() also only
    // installs callbacks; the initial value flows through the scan
    // path. Auto-readback on inputs caused two problems: a spurious
    // blocking read against an unconnected driver, and overwriting a
    // template's deliberate default value with a stale/zero readback.

    // UInt32Digital: apply mask
    if link_str.contains("@asynMask") {
        if let Ok(ml) = parse_asyn_mask_link(link_str) {
            adapter = adapter.with_mask(ml.mask);
        }
    }

    Some(Box::new(adapter))
}

/// Register universal asyn device support on an IocApplication.
///
/// This is the Rust equivalent of C EPICS's standard asyn device support
/// registration. Call this BEFORE registering plugin or driver-specific
/// factories so they take precedence (dynamic factories chain last-registered-first).
pub fn register_asyn_device_support(
    app: epics_base_rs::server::ioc_app::IocApplication,
) -> epics_base_rs::server::ioc_app::IocApplication {
    app.register_dynamic_device_support(universal_asyn_factory)
}

/// IocBuilder companion to [`register_asyn_device_support`] —
/// installs the universal asyn factory on the pure-Rust build path
/// (added `register_dynamic_device_support` to IocBuilder).
/// Without this helper, callers using `IocBuilder` instead of
/// `IocApplication` would have to wire `universal_asyn_factory`
/// manually; that asymmetry is exactly what `register_asyn_device_support`
/// hides for the IocApplication path.
pub fn register_asyn_device_support_for_builder(
    builder: epics_base_rs::server::ioc_builder::IocBuilder,
) -> epics_base_rs::server::ioc_builder::IocBuilder {
    builder.register_dynamic_device_support(universal_asyn_factory)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every asyn read error maps to the C `asynStatusToEpicsAlarm` pair
    /// under the device-support READ-path defaults (READ_ALARM / INVALID).
    /// Condition codes per epics-base alarm.h: READ=1, COMM=9, TIMEOUT=10,
    /// HW_LIMIT=11, DISABLE=18; INVALID severity = 3.
    #[test]
    fn asyn_error_to_alarm_matches_c_status_to_epics_alarm() {
        use crate::error::AsynStatus;
        let mk = |s: AsynStatus| {
            asyn_error_to_alarm(&AsynError::Status {
                status: s,
                message: String::new(),
            })
        };
        assert_eq!(mk(AsynStatus::Success), (0, 0));
        assert_eq!(
            mk(AsynStatus::Timeout),
            (10, 3),
            "Timeout -> TIMEOUT_ALARM(10), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Overflow),
            (11, 3),
            "Overflow -> HW_LIMIT_ALARM(11), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Disconnected),
            (9, 3),
            "Disconnected -> COMM_ALARM(9), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Disabled),
            (18, 3),
            "Disabled -> DISABLE_ALARM(18), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Error),
            (1, 3),
            "Error -> READ_ALARM(1), INVALID(3)"
        );
        // Non-status asyn errors take C's asynError/default branch.
        assert_eq!(
            asyn_error_to_alarm(&AsynError::PortNotFound("p".into())),
            (1, 3)
        );
    }

    /// Regression: the IocBuilder companion helper exists
    /// and accepts a pure-Rust builder. Pre-fix, `register_asyn_device_support`
    /// only accepted IocApplication, so an IocBuilder caller had to
    /// expose `universal_asyn_factory` themselves.
    #[tokio::test]
    async fn register_asyn_device_support_for_builder_compiles_and_attaches() {
        use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
        use epics_base_rs::server::ioc_builder::IocBuilder;
        use epics_base_rs::server::record::ScanType;
        use epics_base_rs::types::EpicsValue;
        let _ = (
            ScanType::Passive,
            EpicsValue::Double(0.0),
            std::any::type_name::<dyn DeviceSupport>(),
            std::any::type_name::<DeviceReadOutcome>(),
        );

        // The helper consumes and returns the builder — chain on .build().
        let _builder = register_asyn_device_support_for_builder(IocBuilder::new());
    }

    #[test]
    fn test_parse_full() {
        let link = parse_asyn_link("@asyn(myPort, 0, 1.0) TEMPERATURE").unwrap();
        assert_eq!(link.port_name, "myPort");
        assert_eq!(link.addr, 0);
        assert_eq!(link.timeout, Duration::from_secs_f64(1.0));
        assert_eq!(link.drv_info, "TEMPERATURE");
    }

    #[test]
    fn test_parse_port_only() {
        let link = parse_asyn_link("@asyn(port1) PARAM").unwrap();
        assert_eq!(link.port_name, "port1");
        assert_eq!(link.addr, 0);
        assert_eq!(link.timeout, Duration::from_secs(1));
        assert_eq!(link.drv_info, "PARAM");
    }

    #[test]
    fn test_parse_port_and_addr() {
        let link = parse_asyn_link("@asyn(port2, 3) VALUE").unwrap();
        assert_eq!(link.port_name, "port2");
        assert_eq!(link.addr, 3);
        assert_eq!(link.drv_info, "VALUE");
    }

    #[test]
    fn test_parse_fractional_timeout() {
        let link = parse_asyn_link("@asyn(dev, 1, 0.5) CMD").unwrap();
        assert_eq!(link.timeout, Duration::from_secs_f64(0.5));
    }

    #[test]
    fn test_parse_no_drv_info() {
        let link = parse_asyn_link("@asyn(port1)").unwrap();
        assert_eq!(link.drv_info, "");
    }

    #[test]
    fn test_parse_invalid_prefix() {
        assert!(parse_asyn_link("@wrong(port)").is_err());
    }

    #[test]
    fn test_parse_missing_paren() {
        assert!(parse_asyn_link("@asyn(port").is_err());
    }

    #[test]
    fn test_parse_invalid_addr() {
        assert!(parse_asyn_link("@asyn(port, abc) X").is_err());
    }

    #[test]
    fn test_parse_invalid_timeout() {
        assert!(parse_asyn_link("@asyn(port, 0, xyz) X").is_err());
    }

    #[test]
    fn test_parse_space_separated() {
        // NDCircularBuff.template uses space-separated format: @asyn(PORT 0)DRVINFO
        let link = parse_asyn_link("@asyn(CB1 0)CIRC_BUFF_CONTROL").unwrap();
        assert_eq!(link.port_name, "CB1");
        assert_eq!(link.addr, 0);
        assert_eq!(link.drv_info, "CIRC_BUFF_CONTROL");
    }

    #[test]
    fn test_parse_space_separated_with_timeout() {
        let link = parse_asyn_link("@asyn(PORT1 2 1.5) PARAM").unwrap();
        assert_eq!(link.port_name, "PORT1");
        assert_eq!(link.addr, 2);
        assert_eq!(link.timeout, Duration::from_secs_f64(1.5));
        assert_eq!(link.drv_info, "PARAM");
    }

    // --- asynMask link tests ---

    #[test]
    fn test_parse_mask_link_full() {
        let link = parse_asyn_mask_link("@asynMask(port1, 0, 0xFF, 2.0) BITS").unwrap();
        assert_eq!(link.port_name, "port1");
        assert_eq!(link.addr, 0);
        assert_eq!(link.mask, 0xFF);
        assert_eq!(link.timeout, Duration::from_secs_f64(2.0));
        assert_eq!(link.drv_info, "BITS");
    }

    #[test]
    fn test_parse_mask_link_no_timeout() {
        let link = parse_asyn_mask_link("@asynMask(port1, 0, 255) BITS").unwrap();
        assert_eq!(link.mask, 255);
        assert_eq!(link.timeout, Duration::from_secs(1));
    }

    #[test]
    fn test_parse_mask_link_hex_upper() {
        let link = parse_asyn_mask_link("@asynMask(p, 0, 0XFF00) X").unwrap();
        assert_eq!(link.mask, 0xFF00);
    }

    #[test]
    fn test_parse_mask_link_too_few_args() {
        assert!(parse_asyn_mask_link("@asynMask(port1, 0) BITS").is_err());
    }

    #[test]
    fn test_parse_mask_link_invalid_prefix() {
        assert!(parse_asyn_mask_link("@asyn(port1, 0, 0xFF) BITS").is_err());
    }

    use crate::error::AsynResult;
    use crate::interrupt::{InterruptManager, InterruptValue};
    use crate::param::ParamType;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::port_actor::PortActor;
    use std::sync::Arc;

    struct TestPort {
        base: PortDriverBase,
    }
    impl TestPort {
        fn new() -> Self {
            let mut base = PortDriverBase::new("test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            Self { base }
        }
    }
    impl PortDriver for TestPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    /// Port that reports configurable getBounds — used for the ai
    /// LINEAR ESLO/EOFF wiring tests.
    struct BoundedPort {
        base: PortDriverBase,
        low32: i32,
        high32: i32,
    }
    impl BoundedPort {
        fn new(low32: i32, high32: i32) -> Self {
            let mut base = PortDriverBase::new("test_bounds", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            Self {
                base,
                low32,
                high32,
            }
        }
    }
    impl PortDriver for BoundedPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn get_bounds_int32(&self, _user: &AsynUser) -> AsynResult<(i32, i32)> {
            Ok((self.low32, self.high32))
        }
    }

    fn make_bounded_adapter(low: i32, high: i32, iface: &str) -> AsynDeviceSupport {
        let driver = BoundedPort::new(low, high);
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-bounds-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test_bounds".into(), interrupts);
        let link = AsynLink {
            port_name: "test_bounds".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, iface);
        ads.set_record_info("TEST:AI", ScanType::Passive);
        ads
    }

    fn make_adapter(scan: ScanType) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-adapter-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test".into(), interrupts);

        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt32");
        ads.set_record_info("TEST:REC", scan);
        ads
    }

    #[test]
    fn test_io_intr_receiver_none_when_passive() {
        let mut ads = make_adapter(ScanType::Passive);
        assert!(ads.io_intr_receiver().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_io_intr_receiver_some_when_io_intr() {
        let mut ads = make_adapter(ScanType::IoIntr);
        // init() resolves drv_user_create → sets reason_set = true
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let rx = ads.io_intr_receiver();
        assert!(rx.is_some());
    }

    /// Build a `ScanType::IoIntr` adapter on the `asynUInt32Digital`
    /// interface with the given record `@asynMask`.
    fn make_uint32_io_intr_adapter(mask: u32) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-u32-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test".into(), interrupts);
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads =
            AsynDeviceSupport::from_handle(handle, link, "asynUInt32Digital").with_mask(mask);
        ads.set_record_info("TEST:U32", ScanType::IoIntr);
        ads
    }

    /// Poll the interrupt FIFO until an entry arrives (bridge runs on a
    /// spawned task), returning its value. Panics if none arrives.
    async fn await_fifo_value(
        fifo: &Arc<std::sync::Mutex<InterruptFifo>>,
    ) -> crate::param::ParamValue {
        for _ in 0..200 {
            if let Some(entry) = fifo.lock().unwrap().pop() {
                return entry.value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("interrupt value never reached FIFO");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_uint32_masks_delivered_value_to_record_mask() {
        // C uint32Callback delivers `pInterrupt->mask & value`
        // (asynPortDriver.cpp:729); the @asynMask=0x0F record must see
        // a 0xFF param value masked to 0x0F, matching the polled read.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_uint32_io_intr_adapter(0x0F);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = ads.io_intr_receiver().expect("io intr receiver for IoIntr");

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::UInt32Digital(0xFF),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0x01,
            ..Default::default()
        });

        match await_fifo_value(&fifo).await {
            crate::param::ParamValue::UInt32Digital(bits) => assert_eq!(
                bits, 0x0F,
                "I/O-Intr value must be masked to the record @asynMask"
            ),
            other => panic!("expected UInt32Digital, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_uint32_gates_out_non_overlapping_change() {
        // C uint32Callback fires only when `pInterrupt->mask &
        // interruptMask` (asynPortDriver.cpp:720): a change confined to
        // bits outside the @asynMask=0x0F record (changed=0xF0) must be
        // gated out, while an overlapping change still arrives.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_uint32_io_intr_adapter(0x0F);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = ads.io_intr_receiver().expect("io intr receiver for IoIntr");

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::UInt32Digital(0xFF),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0xF0,
            ..Default::default()
        });
        // matches() rejects synchronously at notify(); a bounded wait
        // then proves the value was dropped, not merely delayed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            fifo.lock().unwrap().pop().is_none(),
            "non-overlapping change must be gated out, not delivered"
        );

        // The bridge is still live: an overlapping change arrives, proving
        // the gate (not a dead bridge) dropped the previous notify.
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::UInt32Digital(0xFF),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0x01,
            ..Default::default()
        });
        assert!(matches!(
            await_fifo_value(&fifo).await,
            crate::param::ParamValue::UInt32Digital(0x0F)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_non_uint32_not_gated_by_default_mask() {
        // Guard: the default 0xFFFFFFFF mask must NOT gate a non-UInt32
        // interrupt (uint32_changed_mask == 0). An asynInt32 I/O-Intr
        // record must receive its interrupt unchanged.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = ads.io_intr_receiver().expect("io intr receiver for IoIntr");

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(42),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        assert!(
            matches!(
                await_fifo_value(&fifo).await,
                crate::param::ParamValue::Int32(42)
            ),
            "non-UInt32 interrupt must pass through ungated and unmasked"
        );
    }

    #[test]
    fn test_adapter_init_resolves_reason() {
        let mut ads = make_adapter(ScanType::Passive);

        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.reason, 0); // "VAL" is param index 0
    }

    // --- ai LINEAR ESLO/EOFF from getBounds (C devAsynInt32.c::convertAi) ---

    /// C `convertAi` formula: ESLO=(EGUF-EGUL)/(high-low),
    /// EOFF=(high*EGUL-low*EGUF)/(high-low). With low=0, high=4095,
    /// EGUL=0.0, EGUF=10.0 → ESLO ≈ 10/4095 ≈ 0.002442, EOFF=0.0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_linear_eslo_eoff_filled_from_get_bounds_int32() {
        let mut ads = make_bounded_adapter(0, 4095, "asynInt32");
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        // Configure EGUL/EGUF before init so convertAi has them.
        rec.eguf = 10.0;
        rec.egul = 0.0;
        rec.linr = 2; // LINEAR (LINR codes: 0=NO_CONVERSION, 1=SLOPE, 2=LINEAR)
        ads.init(&mut rec).unwrap();

        let eslo = match rec.get_field("ESLO").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        let eoff = match rec.get_field("EOFF").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (eslo - 10.0 / 4095.0).abs() < 1e-9,
            "ESLO must equal (EGUF-EGUL)/(high-low): got {eslo}"
        );
        assert!(
            eoff.abs() < 1e-9,
            "EOFF must equal 0 for symmetric range: got {eoff}"
        );
    }

    /// EGUF=10, EGUL=-10 with bounds [-2048, 2047] → ESLO ≈ 20/4095.
    /// EOFF = (2047*-10 - -2048*10)/4095 = (-20470 + 20480)/4095 = 10/4095.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_linear_eslo_eoff_signed_range() {
        let mut ads = make_bounded_adapter(-2048, 2047, "asynInt32");
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        rec.eguf = 10.0;
        rec.egul = -10.0;
        rec.linr = 2;
        ads.init(&mut rec).unwrap();
        let eslo = match rec.get_field("ESLO").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        let eoff = match rec.get_field("EOFF").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        let denom = (2047 - -2048) as f64;
        assert!((eslo - 20.0 / denom).abs() < 1e-9);
        let expected_eoff = (2047.0 * -10.0 - -2048.0 * 10.0) / denom;
        assert!(
            (eoff - expected_eoff).abs() < 1e-9,
            "EOFF expected {expected_eoff} got {eoff}"
        );
    }

    /// Driver returning low==high (no usable range) must leave the
    /// record's ESLO/EOFF unchanged — matches C `convertAi:444`
    /// (`if (deviceHigh != deviceLow)`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_linear_skip_when_bounds_equal() {
        let mut ads = make_bounded_adapter(0, 0, "asynInt32");
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        rec.eguf = 10.0;
        rec.egul = 0.0;
        rec.linr = 2;
        // Pre-set ESLO/EOFF to sentinel values to detect untouched.
        rec.eslo = 123.456;
        rec.eoff = 42.0;
        ads.init(&mut rec).unwrap();
        assert!((rec.eslo - 123.456).abs() < 1e-9);
        assert!((rec.eoff - 42.0).abs() < 1e-9);
    }

    /// Records without an ESLO field (e.g. longin) must skip the
    /// LINEAR wiring entirely — the `record.get_field("ESLO").is_some()`
    /// guard in `init()` prevents a wasted GetBoundsInt32 round-trip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn longin_skips_linear_wiring() {
        let mut ads = make_bounded_adapter(0, 4095, "asynInt32");
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        // Should not error or block — just no-ops because LonginRecord
        // doesn't expose ESLO.
        ads.init(&mut rec).unwrap();
    }

    // --- asyn:FIFO ring buffer ---

    fn intr_entry(v: i32, t_ms: u64) -> CachedInterrupt {
        CachedInterrupt {
            value: crate::param::ParamValue::Int32(v),
            timestamp: SystemTime::UNIX_EPOCH + Duration::from_millis(t_ms),
        }
    }

    #[test]
    fn fifo_default_size_matches_c_constant() {
        // C devAsynInt32.c:63 → DEFAULT_RING_BUFFER_SIZE = 10.
        let f = InterruptFifo::new();
        assert_eq!(f.ring_size, 10);
        assert!(f.entries.is_empty());
        assert_eq!(f.overflows, 0);
    }

    #[test]
    fn fifo_push_pop_fifo_order() {
        let mut f = InterruptFifo::new();
        assert!(f.push_with_overflow(intr_entry(1, 1)));
        assert!(f.push_with_overflow(intr_entry(2, 2)));
        assert!(f.push_with_overflow(intr_entry(3, 3)));
        let popped: Vec<_> = std::iter::from_fn(|| f.pop())
            .map(|c| match c.value {
                crate::param::ParamValue::Int32(v) => v,
                _ => panic!(),
            })
            .collect();
        assert_eq!(popped, vec![1, 2, 3], "FIFO order, not LIFO");
        assert_eq!(f.take_overflows(), 0);
    }

    #[test]
    fn fifo_overflow_drops_oldest_and_counts() {
        // C parity: devAsynInt32.c:566-571 — when ringHead wraps onto
        // ringTail, advance ringTail (drop oldest) + overflows++.
        let mut f = InterruptFifo::new();
        f.ring_size = 3;
        assert!(f.push_with_overflow(intr_entry(1, 1)));
        assert!(f.push_with_overflow(intr_entry(2, 2)));
        assert!(f.push_with_overflow(intr_entry(3, 3)));
        // Now full: every additional push must be reported as overflow.
        assert!(!f.push_with_overflow(intr_entry(4, 4)));
        assert!(!f.push_with_overflow(intr_entry(5, 5)));
        assert_eq!(f.overflows, 2);
        // Buffer now holds [3, 4, 5] — oldest two dropped.
        let popped: Vec<_> = std::iter::from_fn(|| f.pop())
            .map(|c| match c.value {
                crate::param::ParamValue::Int32(v) => v,
                _ => panic!(),
            })
            .collect();
        assert_eq!(popped, vec![3, 4, 5]);
    }

    #[test]
    fn fifo_take_overflows_resets() {
        let mut f = InterruptFifo::new();
        f.ring_size = 1;
        f.push_with_overflow(intr_entry(1, 1));
        f.push_with_overflow(intr_entry(2, 2)); // overflow
        f.push_with_overflow(intr_entry(3, 3)); // overflow
        assert_eq!(f.take_overflows(), 2);
        // Second call must return 0 — counter was reset.
        assert_eq!(f.take_overflows(), 0);
    }

    #[test]
    fn set_fifo_size_truncates_existing_entries() {
        let mut ads = make_adapter(ScanType::IoIntr);
        {
            let mut g = ads.interrupt_fifo.lock().unwrap();
            g.ring_size = 10;
            g.push_with_overflow(intr_entry(1, 1));
            g.push_with_overflow(intr_entry(2, 2));
            g.push_with_overflow(intr_entry(3, 3));
            g.push_with_overflow(intr_entry(4, 4));
        }
        // Shrink — must drop the 2 oldest and count them as overflows.
        ads.set_fifo_size(2);
        let g = ads.interrupt_fifo.lock().unwrap();
        assert_eq!(g.entries.len(), 2);
        assert_eq!(g.overflows, 2);
    }

    #[test]
    fn apply_record_info_parses_asyn_fifo() {
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut info = std::collections::HashMap::new();
        info.insert("asyn:FIFO".to_string(), "32".to_string());
        ads.apply_record_info(&info);
        assert_eq!(ads.interrupt_fifo.lock().unwrap().ring_size, 32);

        // C atoi("garbage") = 0 → keep default (we additionally
        // require n > 0).
        info.insert("asyn:FIFO".to_string(), "garbage".to_string());
        ads.apply_record_info(&info);
        assert_eq!(
            ads.interrupt_fifo.lock().unwrap().ring_size,
            32,
            "non-numeric must not clobber size"
        );

        // Negative / zero is rejected too.
        info.insert("asyn:FIFO".to_string(), "0".to_string());
        ads.apply_record_info(&info);
        assert_eq!(ads.interrupt_fifo.lock().unwrap().ring_size, 32);
    }

    #[test]
    fn compute_mask_shift_matches_c() {
        // C devAsynUInt32Digital.c:627-636 — position of lowest set bit.
        assert_eq!(compute_mask_shift(0x0001), 0);
        assert_eq!(compute_mask_shift(0x0002), 1);
        assert_eq!(compute_mask_shift(0x0080), 7);
        assert_eq!(compute_mask_shift(0x0F00), 8);
        assert_eq!(compute_mask_shift(0xFF00), 8);
        assert_eq!(compute_mask_shift(0x8000_0000), 31);
        // 0 mask is the "no bits" sentinel — C falls through to 32 too.
        assert_eq!(compute_mask_shift(0), 32);
    }

    /// C devAsynUInt32Digital.c:881 / 925 / 1010 / 1054 sets
    /// `pr->mask` and `pr->shft = computeShift(mask)` from the link
    /// mask so the record's RVAL→VAL conversion shifts the bits to
    /// bit-0. Verify the Rust adapter writes both fields at init.
    #[test]
    fn uint32_digital_init_propagates_mask_and_shft_to_record() {
        let mut ads = make_adapter(ScanType::Passive);
        ads.set_iface_type("asynUInt32Digital");
        ads = ads.with_mask(0xFF00);

        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let mut rec = MbbiRecord::default();
        ads.init(&mut rec).unwrap();

        assert_eq!(
            rec.get_field("MASK"),
            Some(EpicsValue::Long(0xFF00)),
            "MASK must propagate"
        );
        assert_eq!(
            rec.get_field("SHFT"),
            Some(EpicsValue::Short(8)),
            "SHFT must equal computeShift(0xFF00) = 8"
        );
    }

    /// C parity: devAsynOctet initCommon passes `plsi->sizv` as the
    /// asynOctet read-buffer size; an lsi record with a non-default
    /// SIZV must produce read ops sized accordingly, not the fixed
    /// 256-byte stringin default.
    #[test]
    fn octet_buffer_picks_up_sizv_from_record() {
        let mut ads = make_adapter(ScanType::Passive);
        // The adapter only routes asynOctet through octet_max_size,
        // so re-target it.
        ads.set_iface_type("asynOctet");
        // Default (no init yet) is the stringin fallback.
        assert_eq!(ads.octet_max_size, 256);

        use epics_base_rs::server::records::lsi::LsiRecord;
        let mut rec = LsiRecord::new("");
        // Default SIZV = 256 — still matches default.
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.octet_max_size, 256);

        // Bump SIZV and re-init: adapter must follow.
        rec.sizv = 1024;
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.octet_max_size, 1024);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_record_info_enables_readback_for_truthy_value() {
        // info("asyn:READBACK", "1") on a Passive output must allow
        // io_intr_receiver to activate (asyn upstream PRs #60 / #208).
        let mut ads = make_adapter(ScanType::Passive);
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        // Without the info tag, Passive scan keeps io_intr_receiver=None.
        assert!(ads.io_intr_receiver().is_none());
        // Apply the tag — adapter should now expose an Intr receiver.
        let mut info = std::collections::HashMap::new();
        info.insert("asyn:READBACK".to_string(), "1".to_string());
        ads.apply_record_info(&info);
        assert!(
            ads.io_intr_receiver().is_some(),
            "asyn:READBACK=1 must enable readback path"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_record_info_falsey_values_do_not_enable_readback() {
        let mut ads = make_adapter(ScanType::Passive);
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        for falsey in ["0", "no", "NO", "false", "False", ""] {
            let mut info = std::collections::HashMap::new();
            info.insert("asyn:READBACK".to_string(), falsey.to_string());
            ads.apply_record_info(&info);
            assert!(
                ads.io_intr_receiver().is_none(),
                "value {falsey:?} must not enable readback"
            );
        }
    }

    /// C parity for `asynDbGetInfo(precord, "asyn:INITIAL_READBACK")`
    /// at devAsynOctet.c:357 — info tag overrides the per-record
    /// default. Verifies the auto-parse path: the framework calls
    /// `apply_record_info`, which must flip `initial_readback`
    /// without the caller having to invoke `set_initial_readback`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_record_info_handles_initial_readback_tag() {
        let mut ads = make_adapter(ScanType::Passive);
        // Starts off (default for adapters built with from_handle).
        assert!(!ads.initial_readback);

        let mut info = std::collections::HashMap::new();
        info.insert("asyn:INITIAL_READBACK".to_string(), "1".to_string());
        ads.apply_record_info(&info);
        assert!(ads.initial_readback, "info tag must enable readback");

        // Falsey value resets it.
        info.insert("asyn:INITIAL_READBACK".to_string(), "0".to_string());
        ads.apply_record_info(&info);
        assert!(!ads.initial_readback, "value '0' must disable readback");
    }

    #[test]
    fn test_adapter_write_read() {
        let mut ads = make_adapter(ScanType::Passive);

        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();

        // Write a value
        rec.set_val(EpicsValue::Long(42)).unwrap();
        ads.write(&mut rec).unwrap();

        // Read it back
        let mut rec2 = LonginRecord::new(0);
        ads.read(&mut rec2).unwrap();
        assert_eq!(rec2.val(), Some(EpicsValue::Long(42)));
    }

    /// PR #162: `aai` (array analog input) and `aao` (array analog output)
    /// records use direction-specific DTYPs like `asynFloat64ArrayIn` and
    /// `asynFloat64ArrayOut` that must collapse to the underlying interface
    /// `asynFloat64Array` for the adapter dispatch. Without this the read_op
    /// and write_op matchers (around L383/L417/L489) miss the DTYP and fail
    /// to bind the record.
    #[test]
    fn dtyp_normalize_aai_aao_array_in_out() {
        // Float64 — most common aai/aao pattern.
        assert_eq!(
            normalize_asyn_dtyp("asynFloat64ArrayIn"),
            "asynFloat64Array"
        );
        assert_eq!(
            normalize_asyn_dtyp("asynFloat64ArrayOut"),
            "asynFloat64Array"
        );
        // Int32 family — covers waveform/aai/aao integer variants.
        assert_eq!(normalize_asyn_dtyp("asynInt32ArrayIn"), "asynInt32Array");
        assert_eq!(normalize_asyn_dtyp("asynInt32ArrayOut"), "asynInt32Array");
        // Other widths sanity-check the suffix rule, not exhaustive of C asyn.
        assert_eq!(normalize_asyn_dtyp("asynInt8ArrayIn"), "asynInt8Array");
        assert_eq!(normalize_asyn_dtyp("asynInt16ArrayOut"), "asynInt16Array");
        assert_eq!(normalize_asyn_dtyp("asynInt64ArrayIn"), "asynInt64Array");
        assert_eq!(
            normalize_asyn_dtyp("asynFloat32ArrayOut"),
            "asynFloat32Array"
        );
    }

    #[test]
    fn dtyp_normalize_preserves_non_array_dtyps() {
        // Scalar DTYPs must pass through unchanged — only Array/Octet
        // direction suffixes are stripped.
        assert_eq!(normalize_asyn_dtyp("asynInt32"), "asynInt32");
        assert_eq!(normalize_asyn_dtyp("asynFloat64"), "asynFloat64");
        // Octet read/write direction-specific DTYPs collapse to asynOctet
        // (C EPICS lsi/lso/printf adapter convention).
        assert_eq!(normalize_asyn_dtyp("asynOctetRead"), "asynOctet");
        assert_eq!(normalize_asyn_dtyp("asynOctetWrite"), "asynOctet");
    }
}
