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
    /// If true, read back the current driver value during init (for output records).
    initial_readback: bool,
    /// If true, this is a write-only device support (e.g. asynOctetWrite).
    /// read() returns no-op to avoid overwriting the record's native value type.
    write_only: bool,
    /// RAII interrupt subscription — dropping unsubscribes.
    interrupt_sub: Option<InterruptSubscription>,
    /// Shared cache for the latest interrupt value. Written by the I/O Intr
    /// forwarding task, read by read(). When an I/O Intr record is processed
    /// due to an interrupt, read() uses this cached value instead of sending
    /// a blocking read to the port driver. This matches C EPICS behavior
    /// where the interrupt callback directly provides the value.
    interrupt_cache: Arc<std::sync::Mutex<Option<crate::param::ParamValue>>>,
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
            last_alarm_status: 0,
            last_alarm_severity: 0,
            last_ts: None,
            record_name: String::new(),
            scan: ScanType::Passive,
            initial_readback: false,
            write_only: false,
            interrupt_sub: None,
            interrupt_cache: Arc::new(std::sync::Mutex::new(None)),
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

fn asyn_to_ca_error(e: AsynError) -> CaError {
    CaError::Protocol(e.to_string())
}

/// Convert an asyn ParamValue to an EpicsValue.
fn param_value_to_epics_value(pv: &crate::param::ParamValue) -> Option<EpicsValue> {
    use crate::param::ParamValue;
    match pv {
        ParamValue::Int32(v) => Some(EpicsValue::Long(*v)),
        ParamValue::Int64(v) => Some(EpicsValue::Double(*v as f64)),
        ParamValue::Float64(v) => Some(EpicsValue::Double(*v)),
        ParamValue::Octet(s) => Some(EpicsValue::String(s.clone())),
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
    /// Build a `RequestOp` for reading the current interface type.
    fn read_op(&self) -> Option<RequestOp> {
        match self.iface_type.as_str() {
            "asynInt32" => Some(RequestOp::Int32Read),
            "asynInt64" => Some(RequestOp::Int64Read),
            "asynFloat64" => Some(RequestOp::Float64Read),
            "asynOctet" => Some(RequestOp::OctetRead { buf_size: 256 }),
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
            "asynInt64" => result.int64_val.map(|v| EpicsValue::Long(v as i32)),
            "asynFloat64" => result.float_val.map(EpicsValue::Double),
            "asynOctet" => result.data.as_ref().map(|d| {
                let n = result.nbytes.min(d.len());
                EpicsValue::String(String::from_utf8_lossy(&d[..n]).into_owned())
            }),
            "asynUInt32Digital" => result.uint_val.map(|v| EpicsValue::Long(v as i32)),
            "asynEnum" => result.enum_index.map(|v| EpicsValue::Long(v as i32)),
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
            ("asynEnum", EpicsValue::Long(v)) => Some(RequestOp::EnumWrite { index: *v as usize }),
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
                Ok(reason) => self.reason = reason,
                Err(_) => {
                    // Param not found — this record has no corresponding driver param.
                    // Silently disable this device support (no-op reads/writes).
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

        // For I/O Intr records, use the cached interrupt value if available.
        // If the cache is still empty on a non-blocking port, fall through to
        // a direct read so clients observe the latest driver value.
        if self.scan == ScanType::IoIntr {
            let cached = self.interrupt_cache.lock().unwrap().take();
            if let Some(pv) = cached {
                if let Some(val) = param_value_to_epics_value(&pv) {
                    let _ = record.set_val(val);
                }
                return Ok(DeviceReadOutcome::ok());
            }
            if self.handle.can_block() {
                return Ok(DeviceReadOutcome::ok());
            }
        }

        if let Some(op) = self.read_op() {
            let user = AsynUser::new(self.reason)
                .with_addr(self.addr)
                .with_timeout(self.timeout);
            if let Ok(result) = self.handle.submit_blocking(op, user) {
                if let Some(val) = self.result_to_value(&result) {
                    let _ = record.set_val(val);
                }
                self.last_alarm_status = result.alarm_status;
                self.last_alarm_severity = result.alarm_severity;
                self.last_ts = result.timestamp;
            }
        }
        Ok(DeviceReadOutcome::ok())
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

        let completion = self.handle.try_submit(op, user).map_err(asyn_to_ca_error)?;
        Ok(Some(Box::new(AsynAsyncWriteCompletion {
            handle: parking_lot::Mutex::new(Some(completion)),
        })))
    }

    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        if self.scan != ScanType::IoIntr || !self.reason_set {
            return None;
        }

        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.addr),
        };

        let (sub, intr_rx) = self.handle.interrupts().register_interrupt_user(filter);
        self.interrupt_sub = Some(sub);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut intr_rx = intr_rx;
        let cache = self.interrupt_cache.clone();
        tokio::spawn(async move {
            while let Some(iv) = intr_rx.recv().await {
                // Cache the interrupt value so read() can use it without
                // a blocking port request. This matches C EPICS where the
                // interrupt callback directly provides the new value.
                *cache.lock().unwrap() = Some(iv.value);
                if tx.send(()).await.is_err() {
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

    // Output records: read back current driver value on init.
    if is_output {
        if ctx.dtyp == "asynOctetWrite" {
            // asynOctetWrite is write-only — no reads allowed.
            // Reading would replace waveform CharArray with String, breaking element_count.
            adapter.write_only = true;
        } else {
            adapter = adapter.with_initial_readback();
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

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

    use crate::interrupt::InterruptManager;
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

    #[test]
    fn test_adapter_init_resolves_reason() {
        let mut ads = make_adapter(ScanType::Passive);

        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.reason, 0); // "VAL" is param index 0
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
}
