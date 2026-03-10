use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::device_support::{DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;

use crate::error::AsynError;
use crate::interfaces::InterfaceType;
use crate::interrupt::{InterruptFilter, InterruptSubscription};
use crate::port::PortDriver;
use crate::port_handle::{AsyncCompletionHandle, PortHandle};
use crate::request::{CompletionHandle, RequestOp, RequestResult};
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
/// Accepted formats:
/// - `@asyn(portName) drvInfo`
/// - `@asyn(portName, addr) drvInfo`
/// - `@asyn(portName, addr, timeout) drvInfo`
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

    let parts: Vec<&str> = args_str.split(',').map(|p| p.trim()).collect();
    if parts.is_empty() || parts[0].is_empty() {
        return Err(AsynError::InvalidLinkSyntax(
            "portName is required".into(),
        ));
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
        let secs: f64 = parts[2].parse().map_err(|_| {
            AsynError::InvalidLinkSyntax(format!("invalid timeout: {}", parts[2]))
        })?;
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
    let mask = if let Some(hex) = mask_str.strip_prefix("0x").or_else(|| mask_str.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16)
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid mask: {mask_str}")))?
    } else {
        mask_str
            .parse::<u32>()
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid mask: {mask_str}")))?
    };

    let timeout = if parts.len() > 3 {
        let secs: f64 = parts[3].parse().map_err(|_| {
            AsynError::InvalidLinkSyntax(format!("invalid timeout: {}", parts[3]))
        })?;
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

/// Port driver access backend — either legacy mutex or actor handle.
enum PortBackend {
    /// Legacy: direct mutex locking on the port driver.
    Legacy(Arc<Mutex<dyn PortDriver>>),
    /// Actor: channel-based submission via PortHandle.
    Handle(PortHandle),
}

/// Adapter bridging an asyn-rs PortDriver to epics-base-rs DeviceSupport.
pub struct AsynDeviceSupport {
    backend: PortBackend,
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
    /// If true, read back the current driver value during init (for output records).
    initial_readback: bool,
    /// RAII interrupt subscription — dropping unsubscribes.
    interrupt_sub: Option<InterruptSubscription>,
}

impl AsynDeviceSupport {
    fn new_inner(backend: PortBackend, link: AsynLink, iface_type: &str) -> Self {
        let iface = InterfaceType::from_asyn_name(iface_type);
        Self {
            backend,
            addr: link.addr,
            timeout: link.timeout,
            drv_info: link.drv_info,
            reason: 0,
            reason_set: false,
            iface_type: iface_type.to_string(),
            iface,
            mask: 0xFFFFFFFF,
            last_alarm_status: 0,
            last_alarm_severity: 0,
            last_ts: None,
            record_name: String::new(),
            scan: ScanType::Passive,
            initial_readback: false,
            interrupt_sub: None,
        }
    }

    /// Create from a legacy `Arc<Mutex<dyn PortDriver>>`.
    pub fn new(
        port: Arc<Mutex<dyn PortDriver>>,
        link: AsynLink,
        iface_type: &str,
    ) -> Self {
        Self::new_inner(PortBackend::Legacy(port), link, iface_type)
    }

    /// Create from a [`PortHandle`] (actor model).
    pub fn from_handle(
        handle: PortHandle,
        link: AsynLink,
        iface_type: &str,
    ) -> Self {
        Self::new_inner(PortBackend::Handle(handle), link, iface_type)
    }

    /// Create with a typed interface (no string dispatch needed).
    pub fn with_interface(
        port: Arc<Mutex<dyn PortDriver>>,
        link: AsynLink,
        iface: InterfaceType,
    ) -> Self {
        Self::new_inner(
            PortBackend::Legacy(port),
            link,
            iface.asyn_name(),
        )
    }

    /// Create with a typed interface from a [`PortHandle`].
    pub fn with_interface_handle(
        handle: PortHandle,
        link: AsynLink,
        iface: InterfaceType,
    ) -> Self {
        Self::new_inner(
            PortBackend::Handle(handle),
            link,
            iface.asyn_name(),
        )
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
}

/// Bridges asyn-rs CompletionHandle to epics-base-rs WriteCompletion.
struct AsynWriteCompletion {
    handle: CompletionHandle,
}

impl WriteCompletion for AsynWriteCompletion {
    fn wait(&self, timeout: Duration) -> CaResult<()> {
        match self.handle.wait(timeout) {
            Ok(_) => Ok(()),
            Err(e) => Err(CaError::Protocol(e.to_string())),
        }
    }
}

fn asyn_to_ca_error(e: AsynError) -> CaError {
    CaError::Protocol(e.to_string())
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
            "asynInt32Array" => Some(RequestOp::Int32ArrayRead { max_elements: 4096 }),
            "asynFloat64Array" => Some(RequestOp::Float64ArrayRead { max_elements: 4096 }),
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
            "asynInt32Array" => result.int32_array.clone().map(EpicsValue::LongArray),
            "asynFloat64Array" => result.float64_array.clone().map(EpicsValue::DoubleArray),
            _ => None,
        }
    }

    /// Build a `RequestOp` for writing an `EpicsValue` for the current interface type.
    fn write_op(&self, val: &EpicsValue) -> Option<RequestOp> {
        match (self.iface_type.as_str(), val) {
            ("asynInt32", EpicsValue::Long(v)) => Some(RequestOp::Int32Write { value: *v }),
            ("asynInt32", EpicsValue::Enum(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt32", EpicsValue::Short(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt64", EpicsValue::Long(v)) => Some(RequestOp::Int64Write { value: *v as i64 }),
            ("asynFloat64", EpicsValue::Double(v)) => Some(RequestOp::Float64Write { value: *v }),
            ("asynOctet", EpicsValue::String(s)) => Some(RequestOp::OctetWrite {
                data: s.as_bytes().to_vec(),
            }),
            ("asynUInt32Digital", EpicsValue::Long(v)) => Some(RequestOp::UInt32DigitalWrite {
                value: *v as u32,
                mask: self.mask,
            }),
            ("asynEnum", EpicsValue::Long(v)) => Some(RequestOp::EnumWrite { index: *v as usize }),
            ("asynInt32Array", EpicsValue::LongArray(data)) => {
                Some(RequestOp::Int32ArrayWrite { data: data.clone() })
            }
            ("asynFloat64Array", EpicsValue::DoubleArray(data)) => {
                Some(RequestOp::Float64ArrayWrite { data: data.clone() })
            }
            _ => None,
        }
    }

    /// PortHandle-based init: resolve reason + optional readback.
    fn init_handle(&mut self, handle: &PortHandle, record: &mut dyn Record) -> CaResult<()> {
        if !self.reason_set {
            self.reason = handle
                .drv_user_create_blocking(&self.drv_info)
                .map_err(asyn_to_ca_error)?;
        }

        if self.initial_readback {
            if let Some(op) = self.read_op() {
                let user = AsynUser::new(self.reason)
                    .with_addr(self.addr)
                    .with_timeout(self.timeout);
                if let Ok(result) = handle.submit_blocking(op, user) {
                    if let Some(val) = self.result_to_value(&result) {
                        let _ = record.set_val(val);
                    }
                }
            }
        }
        Ok(())
    }

    /// PortHandle-based read: submit blocking read, extract value + alarm.
    fn read_handle(&mut self, handle: &PortHandle, record: &mut dyn Record) -> CaResult<()> {
        let op = self.read_op().ok_or_else(|| {
            CaError::Protocol(format!("unsupported interface type for read: {}", self.iface_type))
        })?;
        let user = AsynUser::new(self.reason)
            .with_addr(self.addr)
            .with_timeout(self.timeout);
        let result = handle.submit_blocking(op, user).map_err(asyn_to_ca_error)?;
        if let Some(val) = self.result_to_value(&result) {
            record.set_val(val)?;
        }
        self.last_alarm_status = result.alarm_status;
        self.last_alarm_severity = result.alarm_severity;
        self.last_ts = result.timestamp;
        Ok(())
    }

    /// PortHandle-based write: submit blocking write.
    fn write_handle(&mut self, handle: &PortHandle, record: &mut dyn Record) -> CaResult<()> {
        if let Some(val) = record.val() {
            if let Some(op) = self.write_op(&val) {
                let user = AsynUser::new(self.reason)
                    .with_addr(self.addr)
                    .with_timeout(self.timeout);
                handle.submit_blocking(op, user).map_err(asyn_to_ca_error)?;
            }
        }
        Ok(())
    }
}

impl DeviceSupport for AsynDeviceSupport {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        match &self.backend {
            PortBackend::Handle(handle) => {
                let h = handle.clone();
                return self.init_handle(&h, record);
            }
            PortBackend::Legacy(port) => {
                let port = port.clone();
                {
                    let p = port.lock();
                    if !self.reason_set {
                        self.reason = p.drv_user_create(&self.drv_info).map_err(asyn_to_ca_error)?;
                    }
                }

                if self.initial_readback {
                    let mut p = port.lock();
                    let user = AsynUser::new(self.reason).with_addr(self.addr);
                    match self.iface_type.as_str() {
                        "asynInt32" => {
                            if let Ok(v) = p.read_int32(&user) {
                                let _ = record.set_val(EpicsValue::Long(v));
                            }
                        }
                        "asynInt64" => {
                            if let Ok(v) = p.read_int64(&user) {
                                let _ = record.set_val(EpicsValue::Long(v as i32));
                            }
                        }
                        "asynFloat64" => {
                            if let Ok(v) = p.read_float64(&user) {
                                let _ = record.set_val(EpicsValue::Double(v));
                            }
                        }
                        "asynOctet" => {
                            let mut buf = vec![0u8; 256];
                            if let Ok(n) = p.read_octet(&user, &mut buf) {
                                let s = String::from_utf8_lossy(&buf[..n]).into_owned();
                                let _ = record.set_val(EpicsValue::String(s));
                            }
                        }
                        "asynUInt32Digital" => {
                            if let Ok(v) = p.read_uint32_digital(&user, self.mask) {
                                let _ = record.set_val(EpicsValue::Long(v as i32));
                            }
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
        }
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<()> {
        match &self.backend {
            PortBackend::Handle(handle) => {
                let h = handle.clone();
                return self.read_handle(&h, record);
            }
            PortBackend::Legacy(port) => {
                let mut port = port.lock();
                let user = AsynUser::new(self.reason).with_addr(self.addr);
                match self.iface_type.as_str() {
                    "asynInt32" => {
                        let v = port.read_int32(&user).map_err(asyn_to_ca_error)?;
                        record.set_val(EpicsValue::Long(v))?;
                    }
                    "asynInt64" => {
                        let v = port.read_int64(&user).map_err(asyn_to_ca_error)?;
                        record.set_val(EpicsValue::Long(v as i32))?;
                    }
                    "asynFloat64" => {
                        let v = port.read_float64(&user).map_err(asyn_to_ca_error)?;
                        record.set_val(EpicsValue::Double(v))?;
                    }
                    "asynOctet" => {
                        let mut buf = vec![0u8; 256];
                        let n = port.read_octet(&user, &mut buf).map_err(asyn_to_ca_error)?;
                        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
                        record.set_val(EpicsValue::String(s))?;
                    }
                    "asynUInt32Digital" => {
                        let v = port.read_uint32_digital(&user, self.mask).map_err(asyn_to_ca_error)?;
                        record.set_val(EpicsValue::Long(v as i32))?;
                    }
                    "asynEnum" => {
                        let (idx, _) = port.read_enum(&user).map_err(asyn_to_ca_error)?;
                        record.set_val(EpicsValue::Long(idx as i32))?;
                    }
                    "asynInt32Array" => {
                        let mut buf = vec![0i32; 4096];
                        let n = port.read_int32_array(&user, &mut buf).map_err(asyn_to_ca_error)?;
                        buf.truncate(n);
                        record.set_val(EpicsValue::LongArray(buf))?;
                    }
                    "asynFloat64Array" => {
                        let mut buf = vec![0f64; 4096];
                        let n = port.read_float64_array(&user, &mut buf).map_err(asyn_to_ca_error)?;
                        buf.truncate(n);
                        record.set_val(EpicsValue::DoubleArray(buf))?;
                    }
                    _ => {}
                }
                let (_, alarm_status, alarm_severity) = port.base().params
                    .get_param_status(self.reason, self.addr)
                    .unwrap_or((crate::error::AsynStatus::Success, 0, 0));
                self.last_alarm_status = alarm_status;
                self.last_alarm_severity = alarm_severity;
                self.last_ts = port.base().params
                    .get_timestamp(self.reason, self.addr)
                    .unwrap_or(None);
                Ok(())
            }
        }
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        match &self.backend {
            PortBackend::Handle(handle) => {
                let h = handle.clone();
                return self.write_handle(&h, record);
            }
            PortBackend::Legacy(port) => {
                let mut port = port.lock();
                let mut user = AsynUser::new(self.reason).with_addr(self.addr);
                if let Some(val) = record.val() {
                    match (self.iface_type.as_str(), val) {
                        ("asynInt32", EpicsValue::Long(v)) => {
                            port.write_int32(&mut user, v).map_err(asyn_to_ca_error)?;
                        }
                        ("asynInt32", EpicsValue::Enum(v)) => {
                            port.write_int32(&mut user, v as i32).map_err(asyn_to_ca_error)?;
                        }
                        ("asynInt32", EpicsValue::Short(v)) => {
                            port.write_int32(&mut user, v as i32).map_err(asyn_to_ca_error)?;
                        }
                        ("asynInt64", EpicsValue::Long(v)) => {
                            port.write_int64(&mut user, v as i64).map_err(asyn_to_ca_error)?;
                        }
                        ("asynFloat64", EpicsValue::Double(v)) => {
                            port.write_float64(&mut user, v).map_err(asyn_to_ca_error)?;
                        }
                        ("asynOctet", EpicsValue::String(ref s)) => {
                            port.write_octet(&mut user, s.as_bytes())
                                .map_err(asyn_to_ca_error)?;
                        }
                        ("asynUInt32Digital", EpicsValue::Long(v)) => {
                            port.write_uint32_digital(&mut user, v as u32, self.mask)
                                .map_err(asyn_to_ca_error)?;
                        }
                        ("asynEnum", EpicsValue::Long(v)) => {
                            port.write_enum(&mut user, v as usize)
                                .map_err(asyn_to_ca_error)?;
                        }
                        ("asynInt32Array", EpicsValue::LongArray(ref data)) => {
                            port.write_int32_array(&user, data)
                                .map_err(asyn_to_ca_error)?;
                        }
                        ("asynFloat64Array", EpicsValue::DoubleArray(ref data)) => {
                            port.write_float64_array(&user, data)
                                .map_err(asyn_to_ca_error)?;
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
        }
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

    fn write_begin(&mut self, record: &mut dyn Record) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        match &self.backend {
            PortBackend::Handle(handle) => {
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
                let completion = handle.try_submit(op, user).map_err(asyn_to_ca_error)?;
                Ok(Some(Box::new(AsynAsyncWriteCompletion {
                    handle: parking_lot::Mutex::new(Some(completion)),
                })))
            }
            PortBackend::Legacy(port) => {
                let queue = {
                    let p = port.lock();
                    match &p.base().worker_queue {
                        Some(q) => q.clone(),
                        None => return Ok(None),
                    }
                };

                let val = match record.val() {
                    Some(v) => v,
                    None => return Ok(None),
                };

                let op = match (self.iface_type.as_str(), &val) {
                    ("asynOctet", EpicsValue::String(s)) => {
                        RequestOp::OctetWrite { data: s.as_bytes().to_vec() }
                    }
                    ("asynInt32", EpicsValue::Long(v)) => {
                        RequestOp::Int32Write { value: *v }
                    }
                    ("asynInt32", EpicsValue::Enum(v)) => {
                        RequestOp::Int32Write { value: *v as i32 }
                    }
                    ("asynInt32", EpicsValue::Short(v)) => {
                        RequestOp::Int32Write { value: *v as i32 }
                    }
                    ("asynInt64", EpicsValue::Long(v)) => {
                        RequestOp::Int64Write { value: *v as i64 }
                    }
                    ("asynFloat64", EpicsValue::Double(v)) => {
                        RequestOp::Float64Write { value: *v }
                    }
                    ("asynUInt32Digital", EpicsValue::Long(v)) => {
                        RequestOp::UInt32DigitalWrite { value: *v as u32, mask: self.mask }
                    }
                    _ => return Ok(None),
                };

                let user = AsynUser::new(self.reason)
                    .with_addr(self.addr)
                    .with_timeout(self.timeout);

                let handle = queue.enqueue(op, user);
                Ok(Some(Box::new(AsynWriteCompletion { handle })))
            }
        }
    }

    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        if self.scan != ScanType::IoIntr {
            return None;
        }

        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.addr),
        };

        let (sub, intr_rx) = match &self.backend {
            PortBackend::Handle(handle) => {
                handle.interrupts().register_interrupt_user(filter)
            }
            PortBackend::Legacy(port) => {
                let p = port.lock();
                p.base().interrupts.register_interrupt_user(filter)
            }
        };
        self.interrupt_sub = Some(sub);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut intr_rx = intr_rx;
        tokio::spawn(async move {
            while intr_rx.recv().await.is_some() {
                if tx.send(()).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
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

    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::param::ParamType;

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
        fn base(&self) -> &PortDriverBase { &self.base }
        fn base_mut(&mut self) -> &mut PortDriverBase { &mut self.base }
    }

    fn make_adapter(port: Arc<Mutex<dyn PortDriver>>, scan: ScanType) -> AsynDeviceSupport {
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::new(port, link, "asynInt32");
        ads.reason = 0;
        ads.set_record_info("TEST:REC", scan);
        ads
    }

    fn make_handle_adapter(scan: ScanType) -> AsynDeviceSupport {
        use crate::port_actor::PortActor;
        use crate::interrupt::InterruptManager;

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
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestPort::new()));
        let mut ads = make_adapter(port, ScanType::Passive);
        assert!(ads.io_intr_receiver().is_none());
    }

    #[tokio::test]
    async fn test_io_intr_receiver_some_when_io_intr() {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestPort::new()));
        let mut ads = make_adapter(port, ScanType::IoIntr);
        let rx = ads.io_intr_receiver();
        assert!(rx.is_some());
    }

    // --- PortHandle-backed adapter tests ---

    #[test]
    fn test_handle_adapter_init_resolves_reason() {
        let mut ads = make_handle_adapter(ScanType::Passive);

        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.reason, 0); // "VAL" is param index 0
    }

    #[test]
    fn test_handle_adapter_write_read() {
        let mut ads = make_handle_adapter(ScanType::Passive);

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
