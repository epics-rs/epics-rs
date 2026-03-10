use crate::error::{CaError, CaResult};
use std::fmt;
use std::time::{Duration, SystemTime};

// DBR type ranges: native(0-6), STS(7-13), TIME(14-20), GR(21-27), CTRL(28-34)
pub const DBR_STS_STRING: u16 = 7;
pub const DBR_TIME_STRING: u16 = 14;
pub const DBR_TIME_SHORT: u16 = 15;
pub const DBR_TIME_FLOAT: u16 = 16;
pub const DBR_TIME_ENUM: u16 = 17;
pub const DBR_TIME_CHAR: u16 = 18;
pub const DBR_TIME_LONG: u16 = 19;
pub const DBR_TIME_DOUBLE: u16 = 20;

// db_access.h constants
const MAX_UNITS_SIZE: usize = 8;
const MAX_ENUM_STATES: usize = 16;
const MAX_ENUM_STRING_SIZE: usize = 26;

const EPICS_UNIX_EPOCH_OFFSET_SECS: u64 = 631_152_000;

/// Extract the native DBF type index (0-6) from any DBR type code.
fn dbr_native_index(dbr_type: u16) -> Option<u16> {
    match dbr_type {
        0..=6 => Some(dbr_type),
        7..=13 => Some(dbr_type - 7),
        14..=20 => Some(dbr_type - 14),
        21..=27 => Some(dbr_type - 21),
        28..=34 => Some(dbr_type - 28),
        _ => None,
    }
}

/// EPICS DBR field types (native types only)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DbFieldType {
    String = 0,
    Short = 1,  // aka Int16
    Float = 2,
    Enum = 3,
    Char = 4,   // aka UInt8
    Long = 5,   // aka Int32
    Double = 6,
}

impl DbFieldType {
    pub fn from_u16(v: u16) -> CaResult<Self> {
        match v {
            0 => Ok(Self::String),
            1 => Ok(Self::Short),
            2 => Ok(Self::Float),
            3 => Ok(Self::Enum),
            4 => Ok(Self::Char),
            5 => Ok(Self::Long),
            6 => Ok(Self::Double),
            _ => Err(CaError::UnsupportedType(v)),
        }
    }

    /// Size in bytes for a single element of this type
    pub fn element_size(&self) -> usize {
        match self {
            Self::String => 40, // MAX_STRING_SIZE
            Self::Short | Self::Enum => 2,
            Self::Float | Self::Long => 4,
            Self::Char => 1,
            Self::Double => 8,
        }
    }
}

pub fn native_type_for_dbr(dbr_type: u16) -> CaResult<DbFieldType> {
    match dbr_native_index(dbr_type) {
        Some(idx) => DbFieldType::from_u16(idx),
        None => Err(CaError::UnsupportedType(dbr_type)),
    }
}

pub fn serialize_dbr(
    dbr_type: u16,
    value: &EpicsValue,
    status: u16,
    severity: u16,
    timestamp: SystemTime,
) -> CaResult<Vec<u8>> {
    let native = native_type_for_dbr(dbr_type)?;
    let val_bytes = convert_and_serialize(native, value)?;
    match dbr_type {
        0..=6 => Ok(val_bytes),
        7..=13 => serialize_sts(native, &val_bytes, status, severity),
        14..=20 => serialize_time(native, &val_bytes, status, severity, timestamp),
        21..=27 => serialize_gr_ctrl(native, &val_bytes, status, severity, false),
        28..=34 => serialize_gr_ctrl(native, &val_bytes, status, severity, true),
        _ => Err(CaError::UnsupportedType(dbr_type)),
    }
}

fn epics_timestamp_parts(timestamp: SystemTime) -> (u32, u32) {
    let unix = timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let sec_past_epoch = unix
        .as_secs()
        .saturating_sub(EPICS_UNIX_EPOCH_OFFSET_SECS)
        .min(u32::MAX as u64) as u32;
    (sec_past_epoch, unix.subsec_nanos())
}

/// Convert value to the target native type and serialize to bytes.
fn convert_and_serialize(native: DbFieldType, value: &EpicsValue) -> CaResult<Vec<u8>> {
    if value.dbr_type() == native {
        return Ok(value.to_bytes());
    }
    Ok(value.convert_to(native).to_bytes())
}

/// RISC alignment padding bytes required between metadata header and value.
fn sts_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // status(2)+severity(2) = 4 → short/enum need 0 pad, char needs 1 pad byte
        DbFieldType::Char => &[0],
        // double needs 2 pad bytes to reach 8-byte alignment: sts(4)+pad(2) = 6? No.
        // C struct: sts_double has RISC_pad (dbr_short_t) between severity and value
        DbFieldType::Double => &[0, 0],
        _ => &[],
    }
}

/// RISC alignment padding bytes for TIME structs (after 12-byte metadata header).
fn time_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // 12 bytes header → short/enum need 2-pad, char needs 2+1=3 pad
        DbFieldType::Short | DbFieldType::Enum => &[0, 0],
        DbFieldType::Char => &[0, 0, 0],
        // double: 12 → pad 4 to reach 16-byte boundary for 8-byte double
        DbFieldType::Double => &[0, 0, 0, 0],
        _ => &[],
    }
}

fn serialize_sts(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
) -> CaResult<Vec<u8>> {
    let pad = sts_pad(native);
    let mut buf = Vec::with_capacity(4 + pad.len() + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());
    buf.extend_from_slice(pad);
    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

fn serialize_time(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
    timestamp: SystemTime,
) -> CaResult<Vec<u8>> {
    let (secs, nanos) = epics_timestamp_parts(timestamp);
    let pad = time_pad(native);
    let mut buf = Vec::with_capacity(12 + pad.len() + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());
    buf.extend_from_slice(&secs.to_be_bytes());
    buf.extend_from_slice(&nanos.to_be_bytes());
    buf.extend_from_slice(pad);
    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Serialize value with GR or CTRL metadata (zeroed) matching the C struct layout in db_access.h.
/// GR types include display/alarm limits; CTRL types add control limits.
fn serialize_gr_ctrl(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
    ctrl: bool,
) -> CaResult<Vec<u8>> {
    let mut buf = Vec::with_capacity(96 + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());

    match native {
        DbFieldType::String => {
            // GR/CTRL String: "not implemented; use struct_dbr_sts_string" per db_access.h
            buf.extend_from_slice(sts_pad(native));
        }
        DbFieldType::Enum => {
            // no_str: u16 + strs: char[16][26] — same for GR and CTRL
            buf.extend_from_slice(&0u16.to_be_bytes());
            buf.extend_from_slice(&[0u8; MAX_ENUM_STATES * MAX_ENUM_STRING_SIZE]);
        }
        DbFieldType::Float => {
            // precision(2) + RISC_pad(2) + units[8] + 6 or 8 limits (f32)
            buf.extend_from_slice(&[0u8; 4]); // precision + pad
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 4]);
        }
        DbFieldType::Double => {
            // precision(2) + RISC_pad(2) + units[8] + 6 or 8 limits (f64)
            buf.extend_from_slice(&[0u8; 4]); // precision + pad
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 8]);
        }
        DbFieldType::Short => {
            // units[8] + 6 or 8 limits (i16)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 2]);
        }
        DbFieldType::Long => {
            // units[8] + 6 or 8 limits (i32)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 4]);
        }
        DbFieldType::Char => {
            // units[8] + 6 or 8 limits (u8) + RISC_pad(1)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits]);
            buf.push(0); // RISC_pad
        }
    }

    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Encode a DBR response from a Snapshot. GR/CTRL types include real metadata.
/// Plain/Sts/Time are byte-identical to serialize_dbr output.
pub fn encode_dbr(
    dbr_type: u16,
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<Vec<u8>> {
    let native = native_type_for_dbr(dbr_type)?;
    let val_bytes = convert_and_serialize(native, &snapshot.value)?;
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;

    match dbr_type {
        0..=6 => Ok(val_bytes),
        7..=13 => serialize_sts(native, &val_bytes, status, severity),
        14..=20 => serialize_time(native, &val_bytes, status, severity, snapshot.timestamp),
        21..=27 => encode_gr(native, &val_bytes, snapshot),
        28..=34 => encode_ctrl(native, &val_bytes, snapshot),
        _ => Err(CaError::UnsupportedType(dbr_type)),
    }
}

/// Encode GR (graphic/display) metadata + value.
fn encode_gr(
    native: DbFieldType,
    val_bytes: &[u8],
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<Vec<u8>> {
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;
    let mut buf = Vec::with_capacity(96 + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());

    match native {
        DbFieldType::String => {
            buf.extend_from_slice(sts_pad(native));
        }
        DbFieldType::Enum => {
            encode_enum_metadata(&mut buf, snapshot);
        }
        DbFieldType::Float => {
            encode_prec_units_limits_f32(&mut buf, snapshot, 6);
        }
        DbFieldType::Double => {
            encode_prec_units_limits_f64(&mut buf, snapshot, 6);
        }
        DbFieldType::Short => {
            encode_units_limits_i16(&mut buf, snapshot, 6);
        }
        DbFieldType::Long => {
            encode_units_limits_i32(&mut buf, snapshot, 6);
        }
        DbFieldType::Char => {
            encode_units_limits_u8(&mut buf, snapshot, 6);
            buf.push(0); // RISC_pad
        }
    }

    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Encode CTRL (control) metadata + value. Same as GR but with 8 limits (adds upper/lower ctrl).
fn encode_ctrl(
    native: DbFieldType,
    val_bytes: &[u8],
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<Vec<u8>> {
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;
    let mut buf = Vec::with_capacity(96 + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());

    match native {
        DbFieldType::String => {
            buf.extend_from_slice(sts_pad(native));
        }
        DbFieldType::Enum => {
            encode_enum_metadata(&mut buf, snapshot);
        }
        DbFieldType::Float => {
            encode_prec_units_limits_f32(&mut buf, snapshot, 8);
        }
        DbFieldType::Double => {
            encode_prec_units_limits_f64(&mut buf, snapshot, 8);
        }
        DbFieldType::Short => {
            encode_units_limits_i16(&mut buf, snapshot, 8);
        }
        DbFieldType::Long => {
            encode_units_limits_i32(&mut buf, snapshot, 8);
        }
        DbFieldType::Char => {
            encode_units_limits_u8(&mut buf, snapshot, 8);
            buf.push(0); // RISC_pad
        }
    }

    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Write units field (8 bytes, null-padded).
fn encode_units(buf: &mut Vec<u8>, snapshot: &crate::server::snapshot::Snapshot) {
    let mut units_buf = [0u8; MAX_UNITS_SIZE];
    if let Some(ref disp) = snapshot.display {
        let bytes = disp.units.as_bytes();
        let len = bytes.len().min(MAX_UNITS_SIZE - 1);
        units_buf[..len].copy_from_slice(&bytes[..len]);
    }
    buf.extend_from_slice(&units_buf);
}

/// Get the 6 display limits + optional 2 control limits from snapshot.
fn get_limits(snapshot: &crate::server::snapshot::Snapshot, n_limits: usize) -> [f64; 8] {
    let mut limits = [0.0f64; 8];
    if let Some(ref disp) = snapshot.display {
        limits[0] = disp.upper_disp_limit;
        limits[1] = disp.lower_disp_limit;
        limits[2] = disp.upper_alarm_limit;
        limits[3] = disp.upper_warning_limit;
        limits[4] = disp.lower_warning_limit;
        limits[5] = disp.lower_alarm_limit;
    }
    if n_limits > 6 {
        if let Some(ref ctrl) = snapshot.control {
            limits[6] = ctrl.upper_ctrl_limit;
            limits[7] = ctrl.lower_ctrl_limit;
        }
    }
    limits
}

/// precision(2) + pad(2) + units(8) + n limits as f64
fn encode_prec_units_limits_f64(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    let prec = snapshot.display.as_ref().map(|d| d.precision).unwrap_or(0);
    buf.extend_from_slice(&prec.to_be_bytes());
    buf.extend_from_slice(&[0, 0]); // RISC_pad
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&l.to_be_bytes());
    }
}

/// precision(2) + pad(2) + units(8) + n limits as f32
fn encode_prec_units_limits_f32(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    let prec = snapshot.display.as_ref().map(|d| d.precision).unwrap_or(0);
    buf.extend_from_slice(&prec.to_be_bytes());
    buf.extend_from_slice(&[0, 0]); // RISC_pad
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&(*l as f32).to_be_bytes());
    }
}

/// units(8) + n limits as i16
fn encode_units_limits_i16(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&(*l as i16).to_be_bytes());
    }
}

/// units(8) + n limits as i32
fn encode_units_limits_i32(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&(*l as i32).to_be_bytes());
    }
}

/// units(8) + n limits as u8
fn encode_units_limits_u8(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.push(*l as u8);
    }
}

/// no_str(2) + strs(16x26)
fn encode_enum_metadata(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
) {
    if let Some(ref ei) = snapshot.enums {
        let no_str = ei.strings.len().min(MAX_ENUM_STATES) as u16;
        buf.extend_from_slice(&no_str.to_be_bytes());
        for i in 0..MAX_ENUM_STATES {
            let mut slot = [0u8; MAX_ENUM_STRING_SIZE];
            if let Some(s) = ei.strings.get(i) {
                let bytes = s.as_bytes();
                let len = bytes.len().min(MAX_ENUM_STRING_SIZE - 1);
                slot[..len].copy_from_slice(&bytes[..len]);
            }
            buf.extend_from_slice(&slot);
        }
    } else {
        // No enum info — zero everything (backward compatible)
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&[0u8; MAX_ENUM_STATES * MAX_ENUM_STRING_SIZE]);
    }
}

/// Runtime value from an EPICS PV
#[derive(Debug, Clone, PartialEq)]
pub enum EpicsValue {
    String(String),
    Short(i16),
    Float(f32),
    Enum(u16),
    Char(u8),
    Long(i32),
    Double(f64),
    // Array variants
    ShortArray(Vec<i16>),
    FloatArray(Vec<f32>),
    EnumArray(Vec<u16>),
    DoubleArray(Vec<f64>),
    LongArray(Vec<i32>),
    CharArray(Vec<u8>),
}

impl fmt::Display for EpicsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "{s}"),
            Self::Short(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Enum(v) => write!(f, "{v}"),
            Self::Char(v) => write!(f, "{v}"),
            Self::Long(v) => write!(f, "{v}"),
            Self::Double(v) => write!(f, "{v}"),
            Self::ShortArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::FloatArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::EnumArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::DoubleArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::LongArray(arr) => {
                let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Self::CharArray(arr) => {
                match std::str::from_utf8(arr) {
                    Ok(s) => write!(f, "{s}"),
                    Err(_) => write!(f, "{arr:?}"),
                }
            }
        }
    }
}

impl EpicsValue {
    /// Deserialize a value from raw bytes based on DBR type
    pub fn from_bytes(dbr_type: DbFieldType, data: &[u8]) -> CaResult<Self> {
        match dbr_type {
            DbFieldType::String => {
                let end = data.iter().position(|&b| b == 0).unwrap_or(data.len().min(40));
                let s = std::str::from_utf8(&data[..end])
                    .map_err(|e| CaError::Protocol(format!("invalid UTF-8: {e}")))?;
                Ok(Self::String(s.to_string()))
            }
            DbFieldType::Short => {
                if data.len() < 2 {
                    return Err(CaError::Protocol("short data too small".into()));
                }
                Ok(Self::Short(i16::from_be_bytes([data[0], data[1]])))
            }
            DbFieldType::Float => {
                if data.len() < 4 {
                    return Err(CaError::Protocol("float data too small".into()));
                }
                Ok(Self::Float(f32::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                ])))
            }
            DbFieldType::Enum => {
                if data.len() < 2 {
                    return Err(CaError::Protocol("enum data too small".into()));
                }
                Ok(Self::Enum(u16::from_be_bytes([data[0], data[1]])))
            }
            DbFieldType::Char => {
                if data.is_empty() {
                    return Err(CaError::Protocol("char data empty".into()));
                }
                Ok(Self::Char(data[0]))
            }
            DbFieldType::Long => {
                if data.len() < 4 {
                    return Err(CaError::Protocol("long data too small".into()));
                }
                Ok(Self::Long(i32::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                ])))
            }
            DbFieldType::Double => {
                if data.len() < 8 {
                    return Err(CaError::Protocol("double data too small".into()));
                }
                Ok(Self::Double(f64::from_be_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ])))
            }
        }
    }

    /// Serialize a value to bytes for writing
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::String(s) => {
                let mut buf = [0u8; 40];
                let bytes = s.as_bytes();
                let len = bytes.len().min(39);
                buf[..len].copy_from_slice(&bytes[..len]);
                buf.to_vec()
            }
            Self::Short(v) => v.to_be_bytes().to_vec(),
            Self::Float(v) => v.to_be_bytes().to_vec(),
            Self::Enum(v) => v.to_be_bytes().to_vec(),
            Self::Char(v) => vec![*v],
            Self::Long(v) => v.to_be_bytes().to_vec(),
            Self::Double(v) => v.to_be_bytes().to_vec(),
            Self::ShortArray(arr) => {
                let mut buf = Vec::with_capacity(arr.len() * 2);
                for v in arr {
                    buf.extend_from_slice(&v.to_be_bytes());
                }
                buf
            }
            Self::FloatArray(arr) => {
                let mut buf = Vec::with_capacity(arr.len() * 4);
                for v in arr {
                    buf.extend_from_slice(&v.to_be_bytes());
                }
                buf
            }
            Self::EnumArray(arr) => {
                let mut buf = Vec::with_capacity(arr.len() * 2);
                for v in arr {
                    buf.extend_from_slice(&v.to_be_bytes());
                }
                buf
            }
            Self::DoubleArray(arr) => {
                let mut buf = Vec::with_capacity(arr.len() * 8);
                for v in arr {
                    buf.extend_from_slice(&v.to_be_bytes());
                }
                buf
            }
            Self::LongArray(arr) => {
                let mut buf = Vec::with_capacity(arr.len() * 4);
                for v in arr {
                    buf.extend_from_slice(&v.to_be_bytes());
                }
                buf
            }
            Self::CharArray(arr) => arr.clone(),
        }
    }

    /// Deserialize an array value from raw bytes
    pub fn from_bytes_array(dbr_type: DbFieldType, data: &[u8], count: usize) -> CaResult<Self> {
        if count <= 1 {
            return Self::from_bytes(dbr_type, data);
        }
        match dbr_type {
            DbFieldType::Short => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 2;
                    if offset + 2 > data.len() { break; }
                    arr.push(i16::from_be_bytes([data[offset], data[offset+1]]));
                }
                Ok(Self::ShortArray(arr))
            }
            DbFieldType::Float => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 4;
                    if offset + 4 > data.len() { break; }
                    arr.push(f32::from_be_bytes([
                        data[offset], data[offset+1], data[offset+2], data[offset+3],
                    ]));
                }
                Ok(Self::FloatArray(arr))
            }
            DbFieldType::Enum => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 2;
                    if offset + 2 > data.len() { break; }
                    arr.push(u16::from_be_bytes([data[offset], data[offset+1]]));
                }
                Ok(Self::EnumArray(arr))
            }
            DbFieldType::Double => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 8;
                    if offset + 8 > data.len() { break; }
                    arr.push(f64::from_be_bytes([
                        data[offset], data[offset+1], data[offset+2], data[offset+3],
                        data[offset+4], data[offset+5], data[offset+6], data[offset+7],
                    ]));
                }
                Ok(Self::DoubleArray(arr))
            }
            DbFieldType::Long => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 4;
                    if offset + 4 > data.len() { break; }
                    arr.push(i32::from_be_bytes([
                        data[offset], data[offset+1], data[offset+2], data[offset+3],
                    ]));
                }
                Ok(Self::LongArray(arr))
            }
            DbFieldType::Char => {
                let len = count.min(data.len());
                Ok(Self::CharArray(data[..len].to_vec()))
            }
            _ => Self::from_bytes(dbr_type, data),
        }
    }

    /// Get the DBR type for this value
    pub fn dbr_type(&self) -> DbFieldType {
        match self {
            Self::String(_) => DbFieldType::String,
            Self::Short(_) | Self::ShortArray(_) => DbFieldType::Short,
            Self::Float(_) | Self::FloatArray(_) => DbFieldType::Float,
            Self::Enum(_) | Self::EnumArray(_) => DbFieldType::Enum,
            Self::Char(_) | Self::CharArray(_) => DbFieldType::Char,
            Self::Long(_) | Self::LongArray(_) => DbFieldType::Long,
            Self::Double(_) | Self::DoubleArray(_) => DbFieldType::Double,
        }
    }

    /// Get the element count for this value.
    pub fn count(&self) -> u32 {
        match self {
            Self::ShortArray(arr) => arr.len() as u32,
            Self::FloatArray(arr) => arr.len() as u32,
            Self::EnumArray(arr) => arr.len() as u32,
            Self::DoubleArray(arr) => arr.len() as u32,
            Self::LongArray(arr) => arr.len() as u32,
            Self::CharArray(arr) => arr.len() as u32,
            _ => 1,
        }
    }

    /// Convert to a different native type (scalar only; arrays use first element).
    pub fn convert_to(&self, target: DbFieldType) -> EpicsValue {
        if self.dbr_type() == target {
            return self.clone();
        }
        match target {
            DbFieldType::String => EpicsValue::String(format!("{self}")),
            DbFieldType::Short => EpicsValue::Short(self.to_f64().unwrap_or(0.0) as i16),
            DbFieldType::Float => EpicsValue::Float(self.to_f64().unwrap_or(0.0) as f32),
            DbFieldType::Enum => EpicsValue::Enum(self.to_f64().unwrap_or(0.0) as u16),
            DbFieldType::Char => EpicsValue::Char(self.to_f64().unwrap_or(0.0) as u8),
            DbFieldType::Long => EpicsValue::Long(self.to_f64().unwrap_or(0.0) as i32),
            DbFieldType::Double => EpicsValue::Double(self.to_f64().unwrap_or(0.0)),
        }
    }

    /// Convert to f64, if possible.
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Self::Double(v) => Some(*v),
            Self::Float(v) => Some(*v as f64),
            Self::Long(v) => Some(*v as f64),
            Self::Short(v) => Some(*v as f64),
            Self::Enum(v) => Some(*v as f64),
            Self::Char(v) => Some(*v as f64),
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Resolve EPICS menu string constants to their integer indices.
    ///
    /// C EPICS base uses a menu system to convert string constants (e.g. "NO_ALARM",
    /// "MINOR") to integer indices. This provides the same mapping for the most
    /// commonly used menus.
    fn resolve_menu_string(s: &str) -> Option<i16> {
        match s {
            // menuAlarmSevr
            "NO_ALARM" => Some(0),
            "MINOR" => Some(1),
            "MAJOR" => Some(2),
            "INVALID" => Some(3),
            // menuYesNo / menuSimm
            "NO" => Some(0),
            "YES" => Some(1),
            "RAW" => Some(2),
            // menuOmsl
            "supervisory" => Some(0),
            "closed_loop" => Some(1),
            // menuIvoa
            "Continue normally" => Some(0),
            "Don't drive outputs" => Some(1),
            "Set output to IVOV" => Some(2),
            // menuFtype (waveform FTVL)
            "STRING" => Some(0),
            "CHAR" => Some(1),
            "UCHAR" => Some(2),
            "SHORT" => Some(3),
            "USHORT" => Some(4),
            "LONG" => Some(5),
            "ULONG" => Some(6),
            "INT64" => Some(7),
            "UINT64" => Some(8),
            "FLOAT" => Some(9),
            "DOUBLE" => Some(10),
            "ENUM" => Some(11),
            // menuFanout / menuSelect
            "All" => Some(0),
            "Specified" => Some(1),
            "Mask" => Some(2),
            // calcoutOOPT (Output Option)
            "Every Time" => Some(0),
            "On Change" => Some(1),
            "When Zero" => Some(2),
            "When Non-zero" => Some(3),
            "Transition To Zero" => Some(4),
            "Transition To Non-zero" => Some(5),
            // calcoutDOPT (Data Option)
            "Use CALC" => Some(0),
            "Use OCAL" => Some(1),
            // menuScan
            "Passive" => Some(0),
            "Event" => Some(1),
            "I/O Intr" => Some(2),
            "10 second" => Some(3),
            "5 second" => Some(4),
            "2 second" => Some(5),
            "1 second" => Some(6),
            ".5 second" => Some(7),
            ".2 second" => Some(8),
            ".1 second" => Some(9),
            // menuPini (NO=0, YES=1 already handled via menuYesNo)
            "RUNNING" => Some(2),
            "RUNNING_NOT_CA" => Some(3),
            "PAUSED" => Some(4),
            "PAUSED_NOT_CA" => Some(5),
            _ => None,
        }
    }

    /// Parse a string value into an EpicsValue of the given type
    pub fn parse(dbr_type: DbFieldType, s: &str) -> CaResult<Self> {
        match dbr_type {
            DbFieldType::String => Ok(Self::String(s.to_string())),
            DbFieldType::Short => s
                .parse::<i16>()
                .map(Self::Short)
                .or_else(|_| {
                    Self::resolve_menu_string(s)
                        .map(Self::Short)
                        .ok_or_else(|| CaError::InvalidValue(format!("invalid short or menu string: {s}")))
                }),
            DbFieldType::Float => s
                .parse::<f32>()
                .map(Self::Float)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Enum => s
                .parse::<u16>()
                .map(Self::Enum)
                .or_else(|_| {
                    Self::resolve_menu_string(s)
                        .map(|v| Self::Enum(v as u16))
                        .ok_or_else(|| CaError::InvalidValue(format!("invalid enum or menu string: {s}")))
                }),
            DbFieldType::Char => s
                .parse::<u8>()
                .map(Self::Char)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Long => s
                .parse::<i32>()
                .map(Self::Long)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Double => s
                .parse::<f64>()
                .map(Self::Double)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_double_roundtrip() {
        let val = EpicsValue::Double(3.14);
        let bytes = val.to_bytes();
        let val2 = EpicsValue::from_bytes(DbFieldType::Double, &bytes).unwrap();
        match val2 {
            EpicsValue::Double(v) => assert!((v - 3.14).abs() < 1e-10),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_string_roundtrip() {
        let val = EpicsValue::String("hello".into());
        let bytes = val.to_bytes();
        assert_eq!(bytes.len(), 40);
        let val2 = EpicsValue::from_bytes(DbFieldType::String, &bytes).unwrap();
        match val2 {
            EpicsValue::String(s) => assert_eq!(s, "hello"),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_parse_values() {
        match EpicsValue::parse(DbFieldType::Long, "42").unwrap() {
            EpicsValue::Long(v) => assert_eq!(v, 42),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_serialize_ctrl_double_layout() {
        // DBR_CTRL_DOUBLE = 34: status(2)+severity(2)+precision(2)+pad(2)+units(8)+8*f64_limits+value(8)
        let val = EpicsValue::Double(42.0);
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 100);
        let data = serialize_dbr(34, &val, 0, 0, ts).unwrap();
        // Header: 4 + precision/pad: 4 + units: 8 + 8 limits * 8 = 80 + value: 8 = 88
        assert_eq!(data.len(), 88);
        // units field at offset 8..16 should be all zeros
        assert_eq!(&data[8..16], &[0u8; 8]);
        // value at offset 80..88
        assert_eq!(&data[80..88], &42.0f64.to_be_bytes());
    }

    #[test]
    fn test_serialize_ctrl_long_layout() {
        // DBR_CTRL_LONG = 33: status(2)+severity(2)+units(8)+8*i32_limits+value(4)
        let val = EpicsValue::Long(99);
        let ts = SystemTime::UNIX_EPOCH;
        let data = serialize_dbr(33, &val, 0, 0, ts).unwrap();
        // Header: 4 + units: 8 + 8 limits * 4 = 44 + value: 4 = 48
        assert_eq!(data.len(), 48);
        assert_eq!(&data[4..12], &[0u8; 8]); // units
        assert_eq!(&data[44..48], &99i32.to_be_bytes()); // value
    }

    #[test]
    fn test_serialize_gr_short_layout() {
        // DBR_GR_SHORT = 22: status(2)+severity(2)+units(8)+6*i16_limits+value(2)
        let val = EpicsValue::Short(7);
        let ts = SystemTime::UNIX_EPOCH;
        let data = serialize_dbr(22, &val, 0, 0, ts).unwrap();
        // Header: 4 + units: 8 + 6 limits * 2 = 24 + value: 2 = 26
        assert_eq!(data.len(), 26);
        assert_eq!(&data[24..26], &7i16.to_be_bytes());
    }

    #[test]
    fn test_serialize_ctrl_enum_layout() {
        // DBR_CTRL_ENUM = 31: status(2)+severity(2)+no_str(2)+strs(416)+value(2)
        let val = EpicsValue::Enum(3);
        let ts = SystemTime::UNIX_EPOCH;
        let data = serialize_dbr(31, &val, 0, 0, ts).unwrap();
        // 6 + 416 + 2 = 424
        assert_eq!(data.len(), 424);
        assert_eq!(&data[422..424], &3u16.to_be_bytes());
    }

    #[test]
    fn test_serialize_ctrl_char_layout() {
        // DBR_CTRL_CHAR = 32: status(2)+severity(2)+units(8)+8*u8_limits+RISC_pad(1)+value(1)
        let val = EpicsValue::Char(0xAB);
        let ts = SystemTime::UNIX_EPOCH;
        let data = serialize_dbr(32, &val, 0, 0, ts).unwrap();
        // 4 + 8 + 8 + 1(pad) + 1(val) = 22
        assert_eq!(data.len(), 22);
        assert_eq!(data[21], 0xAB); // value
    }

    #[test]
    fn test_serialize_ctrl_float_layout() {
        // DBR_CTRL_FLOAT = 30: status(2)+severity(2)+prec(2)+pad(2)+units(8)+8*f32_limits+value(4)
        let val = EpicsValue::Float(1.5);
        let ts = SystemTime::UNIX_EPOCH;
        let data = serialize_dbr(30, &val, 0, 0, ts).unwrap();
        // 8 + 8 + 32 + 4 = 52
        assert_eq!(data.len(), 52);
        assert_eq!(&data[48..52], &1.5f32.to_be_bytes());
    }

    #[test]
    fn test_serialize_gr_string_falls_back_to_sts() {
        // DBR_GR_STRING = 21: same as STS (db_access.h says "not implemented; use sts_string")
        let val = EpicsValue::String("test".into());
        let ts = SystemTime::UNIX_EPOCH;
        let data = serialize_dbr(21, &val, 0, 0, ts).unwrap();
        // STS string: status(2) + severity(2) + value(40) = 44
        assert_eq!(data.len(), 44);
    }

    // ---- PR1: Golden packet tests ----
    // Lock the existing wire format with byte-level regression tests.

    #[test]
    fn test_golden_plain_string() {
        let val = EpicsValue::String("hello".into());
        let data = serialize_dbr(0, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data.len(), 40);
        assert_eq!(&data[..5], b"hello");
        assert_eq!(&data[5..], &[0u8; 35]);
    }

    #[test]
    fn test_golden_plain_short() {
        let val = EpicsValue::Short(42);
        let data = serialize_dbr(1, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data, 42i16.to_be_bytes());
    }

    #[test]
    fn test_golden_plain_float() {
        let val = EpicsValue::Float(1.5);
        let data = serialize_dbr(2, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data, 1.5f32.to_be_bytes());
    }

    #[test]
    fn test_golden_plain_enum() {
        let val = EpicsValue::Enum(7);
        let data = serialize_dbr(3, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data, 7u16.to_be_bytes());
    }

    #[test]
    fn test_golden_plain_char() {
        let val = EpicsValue::Char(0xFF);
        let data = serialize_dbr(4, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data, [0xFF]);
    }

    #[test]
    fn test_golden_plain_long() {
        let val = EpicsValue::Long(-1000);
        let data = serialize_dbr(5, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data, (-1000i32).to_be_bytes());
    }

    #[test]
    fn test_golden_plain_double() {
        let val = EpicsValue::Double(std::f64::consts::PI);
        let data = serialize_dbr(6, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data, std::f64::consts::PI.to_be_bytes());
    }

    #[test]
    fn test_golden_sts_double() {
        // STS_DOUBLE = 13: status(2) + severity(2) + RISC_pad(2) + value(8)
        let val = EpicsValue::Double(99.9);
        let data = serialize_dbr(13, &val, 3, 2, SystemTime::UNIX_EPOCH).unwrap();
        // sts_pad for Double = [0, 0] (2 bytes)
        // total = 2(status) + 2(severity) + 2(pad) + 8(value) = 14
        assert_eq!(data.len(), 14);
        assert_eq!(&data[0..2], &3u16.to_be_bytes()); // status
        assert_eq!(&data[2..4], &2u16.to_be_bytes()); // severity
        assert_eq!(&data[4..6], &[0, 0]);             // RISC pad
        assert_eq!(&data[6..14], &99.9f64.to_be_bytes()); // value
    }

    #[test]
    fn test_golden_sts_char() {
        // STS_CHAR = 11: status(2) + severity(2) + RISC_pad(1) + value(1)
        let val = EpicsValue::Char(0x42);
        let data = serialize_dbr(11, &val, 1, 1, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(data.len(), 6); // 4 + 1 pad + 1 value
        assert_eq!(&data[0..2], &1u16.to_be_bytes());
        assert_eq!(&data[2..4], &1u16.to_be_bytes());
        assert_eq!(data[4], 0);    // RISC pad
        assert_eq!(data[5], 0x42); // value
    }

    #[test]
    fn test_golden_time_double() {
        // TIME_DOUBLE = 20: status(2)+severity(2)+secs(4)+nanos(4)+RISC_pad(4)+value(8) = 24
        let ts = SystemTime::UNIX_EPOCH
            + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 1000);
        let val = EpicsValue::Double(1.23);
        let data = serialize_dbr(20, &val, 0, 0, ts).unwrap();
        assert_eq!(data.len(), 24); // 12 + 4 pad + 8 value
        assert_eq!(&data[0..2], &0u16.to_be_bytes());    // status
        assert_eq!(&data[2..4], &0u16.to_be_bytes());    // severity
        assert_eq!(&data[4..8], &1000u32.to_be_bytes());  // EPICS secs
        assert_eq!(&data[8..12], &0u32.to_be_bytes());    // nanos
        assert_eq!(&data[12..16], &[0, 0, 0, 0]);         // RISC pad (4 bytes for double)
        assert_eq!(&data[16..24], &1.23f64.to_be_bytes()); // value
    }

    #[test]
    fn test_golden_time_short() {
        // TIME_SHORT = 15: status(2)+severity(2)+secs(4)+nanos(4)+pad(2)+value(2) = 16
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 500);
        let val = EpicsValue::Short(777);
        let data = serialize_dbr(15, &val, 0, 0, ts).unwrap();
        assert_eq!(data.len(), 16);
        assert_eq!(&data[12..14], &[0, 0]); // 2-byte pad
        assert_eq!(&data[14..16], &777i16.to_be_bytes());
    }

    #[test]
    fn test_golden_time_char() {
        // TIME_CHAR = 18: status(2)+severity(2)+secs(4)+nanos(4)+pad(3)+value(1) = 16
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 10);
        let val = EpicsValue::Char(0xBE);
        let data = serialize_dbr(18, &val, 0, 0, ts).unwrap();
        assert_eq!(data.len(), 16);
        assert_eq!(&data[12..15], &[0, 0, 0]); // 3-byte pad
        assert_eq!(data[15], 0xBE);
    }

    #[test]
    fn test_golden_time_float() {
        // TIME_FLOAT = 16: status(2)+severity(2)+secs(4)+nanos(4)+value(4) = 16 (no pad)
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS);
        let val = EpicsValue::Float(2.5);
        let data = serialize_dbr(16, &val, 0, 0, ts).unwrap();
        assert_eq!(data.len(), 16); // 12 + 0 pad + 4 value
        assert_eq!(&data[12..16], &2.5f32.to_be_bytes());
    }

    #[test]
    fn test_golden_time_enum() {
        // TIME_ENUM = 17: status(2)+severity(2)+secs(4)+nanos(4)+pad(2)+value(2) = 16
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 1);
        let val = EpicsValue::Enum(5);
        let data = serialize_dbr(17, &val, 0, 0, ts).unwrap();
        assert_eq!(data.len(), 16);
        assert_eq!(&data[12..14], &[0, 0]); // 2-byte pad
        assert_eq!(&data[14..16], &5u16.to_be_bytes());
    }

    #[test]
    fn test_golden_time_string() {
        // TIME_STRING = 14: status(2)+severity(2)+secs(4)+nanos(4)+value(40) = 52 (no pad)
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 99);
        let val = EpicsValue::String("abc".into());
        let data = serialize_dbr(14, &val, 0, 0, ts).unwrap();
        assert_eq!(data.len(), 52); // 12 + 0 pad + 40 value
        assert_eq!(&data[12..15], b"abc");
        assert_eq!(&data[15..52], &[0u8; 37]); // rest of 40-byte string
    }

    #[test]
    fn test_golden_gr_matches_time() {
        // Current behavior: GR serializes same as TIME (no metadata in GR/CTRL yet)
        // GR_DOUBLE = 27 vs TIME_DOUBLE = 20
        // Actually GR serializes differently (has zeroed metadata). Let's verify GR has its own layout.
        let val = EpicsValue::Double(42.0);
        let ts = SystemTime::UNIX_EPOCH;
        let gr = serialize_dbr(27, &val, 0, 0, ts).unwrap();
        let time = serialize_dbr(20, &val, 0, 0, ts).unwrap();
        // GR_DOUBLE has: status(2)+severity(2)+prec(2)+pad(2)+units(8)+6*f64_limits(48)+value(8) = 72
        assert_eq!(gr.len(), 72);
        // TIME_DOUBLE: status(2)+severity(2)+secs(4)+nanos(4)+pad(4)+value(8) = 24
        assert_eq!(time.len(), 24);
        // They are NOT the same — GR has zeroed metadata fields but different layout
        assert_ne!(gr, time);
        // Verify GR metadata is all zeros (current gap: no real data)
        assert_eq!(&gr[4..64], &[0u8; 60]); // prec+pad+units+limits all zero
    }

    #[test]
    fn test_golden_ctrl_matches_gr_pattern() {
        // CTRL_DOUBLE = 34 adds 2 more f64 limits beyond GR
        let val = EpicsValue::Double(42.0);
        let ts = SystemTime::UNIX_EPOCH;
        let ctrl = serialize_dbr(34, &val, 0, 0, ts).unwrap();
        let gr = serialize_dbr(27, &val, 0, 0, ts).unwrap();
        // CTRL adds 2 more f64 limits = 16 bytes more
        assert_eq!(ctrl.len(), gr.len() + 16);
        // Both have same status/severity prefix
        assert_eq!(&ctrl[0..4], &gr[0..4]);
        // Both have zeroed metadata currently
        assert_eq!(&ctrl[4..64], &[0u8; 60]); // same zeroed region as GR
    }

    #[test]
    fn test_golden_type_conversion() {
        // DBR_TIME_SHORT for a Double value → should convert to i16
        let val = EpicsValue::Double(42.7);
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS);
        let data = serialize_dbr(15, &val, 0, 0, ts).unwrap();
        // TIME_SHORT: 12 + 2 pad + 2 value = 16
        assert_eq!(data.len(), 16);
        // 42.7 as i16 = 42
        assert_eq!(&data[14..16], &42i16.to_be_bytes());
    }

    #[test]
    fn test_golden_header_read_notify() {
        // Verify CaHeader bytes for a READ_NOTIFY response
        use crate::protocol::*;
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.data_type = 20; // DBR_TIME_DOUBLE
        hdr.set_payload_size(24, 1);
        hdr.cid = ECA_NORMAL;
        hdr.available = 42; // ioid

        let bytes = hdr.to_bytes_extended();
        // Standard header (16 bytes): cmd(2)+postsize(2)+data_type(2)+count(2)+cid(4)+available(4)
        assert_eq!(&bytes[0..2], &CA_PROTO_READ_NOTIFY.to_be_bytes());
        assert_eq!(&bytes[4..6], &20u16.to_be_bytes()); // data_type
        assert_eq!(&bytes[12..16], &42u32.to_be_bytes()); // ioid
    }

    // ---- PR4: encode_dbr tests ----

    use crate::server::snapshot::*;

    /// Helper: create a bare snapshot (no metadata) for testing.
    fn bare_snapshot(value: EpicsValue) -> Snapshot {
        Snapshot::new(value, 0, 0, SystemTime::UNIX_EPOCH)
    }

    /// Helper: create a snapshot with display + control metadata.
    fn full_snapshot(value: EpicsValue) -> Snapshot {
        let mut snap = Snapshot::new(value, 3, 2, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "degC".to_string(),
            precision: 3,
            upper_disp_limit: 100.0,
            lower_disp_limit: -50.0,
            upper_alarm_limit: 90.0,
            upper_warning_limit: 80.0,
            lower_warning_limit: -20.0,
            lower_alarm_limit: -40.0,
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 95.0,
            lower_ctrl_limit: -45.0,
        });
        snap
    }

    #[test]
    fn test_encode_plain_matches_serialize() {
        let val = EpicsValue::Double(42.0);
        let ts = SystemTime::UNIX_EPOCH;
        let snap = bare_snapshot(val.clone());
        assert_eq!(
            encode_dbr(6, &snap).unwrap(),
            serialize_dbr(6, &val, 0, 0, ts).unwrap()
        );
    }

    #[test]
    fn test_encode_sts_matches_serialize() {
        let val = EpicsValue::Short(77);
        let ts = SystemTime::UNIX_EPOCH;
        let mut snap = bare_snapshot(val.clone());
        snap.alarm = AlarmInfo { status: 5, severity: 1 };
        assert_eq!(
            encode_dbr(8, &snap).unwrap(),
            serialize_dbr(8, &val, 5, 1, ts).unwrap()
        );
    }

    #[test]
    fn test_encode_time_matches_serialize() {
        let val = EpicsValue::Double(1.23);
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 500);
        let mut snap = bare_snapshot(val.clone());
        snap.timestamp = ts;
        snap.alarm = AlarmInfo { status: 1, severity: 2 };
        assert_eq!(
            encode_dbr(20, &snap).unwrap(),
            serialize_dbr(20, &val, 1, 2, ts).unwrap()
        );
    }

    #[test]
    fn test_encode_gr_double_with_metadata() {
        let snap = full_snapshot(EpicsValue::Double(42.0));
        let data = encode_dbr(27, &snap).unwrap();
        // GR_DOUBLE: sts(4)+prec(2)+pad(2)+units(8)+6*f64(48)+value(8) = 72
        assert_eq!(data.len(), 72);
        // status=3, severity=2
        assert_eq!(&data[0..2], &3u16.to_be_bytes());
        assert_eq!(&data[2..4], &2u16.to_be_bytes());
        // precision=3
        assert_eq!(&data[4..6], &3i16.to_be_bytes());
        // pad
        assert_eq!(&data[6..8], &[0, 0]);
        // units = "degC" + 4 null bytes
        assert_eq!(&data[8..12], b"degC");
        assert_eq!(&data[12..16], &[0, 0, 0, 0]);
        // limits (f64): upper_disp=100, lower_disp=-50, upper_alarm=90, upper_warn=80, lower_warn=-20, lower_alarm=-40
        assert_eq!(&data[16..24], &100.0f64.to_be_bytes());
        assert_eq!(&data[24..32], &(-50.0f64).to_be_bytes());
        assert_eq!(&data[32..40], &90.0f64.to_be_bytes());
        assert_eq!(&data[40..48], &80.0f64.to_be_bytes());
        assert_eq!(&data[48..56], &(-20.0f64).to_be_bytes());
        assert_eq!(&data[56..64], &(-40.0f64).to_be_bytes());
        // value
        assert_eq!(&data[64..72], &42.0f64.to_be_bytes());
    }

    #[test]
    fn test_encode_ctrl_double_with_metadata() {
        let snap = full_snapshot(EpicsValue::Double(42.0));
        let data = encode_dbr(34, &snap).unwrap();
        // CTRL_DOUBLE: same as GR + 2 more f64 limits = 72 + 16 = 88
        assert_eq!(data.len(), 88);
        // After 6 GR limits at offset 16..64, ctrl limits at 64..80
        assert_eq!(&data[64..72], &95.0f64.to_be_bytes());  // upper_ctrl
        assert_eq!(&data[72..80], &(-45.0f64).to_be_bytes()); // lower_ctrl
        // value at 80..88
        assert_eq!(&data[80..88], &42.0f64.to_be_bytes());
    }

    #[test]
    fn test_encode_gr_short_with_metadata() {
        let mut snap = Snapshot::new(EpicsValue::Short(42), 0, 0, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "mm".to_string(),
            precision: 0,
            upper_disp_limit: 1000.0,
            lower_disp_limit: -100.0,
            upper_alarm_limit: 900.0,
            upper_warning_limit: 800.0,
            lower_warning_limit: -50.0,
            lower_alarm_limit: -90.0,
        });
        let data = encode_dbr(22, &snap).unwrap();
        // GR_SHORT: sts(4)+units(8)+6*i16(12)+value(2) = 26
        assert_eq!(data.len(), 26);
        assert_eq!(&data[4..6], b"mm");
        // upper_disp_limit as i16 = 1000
        assert_eq!(&data[12..14], &1000i16.to_be_bytes());
        // value at 24..26
        assert_eq!(&data[24..26], &42i16.to_be_bytes());
    }

    #[test]
    fn test_encode_gr_float_with_metadata() {
        let mut snap = Snapshot::new(EpicsValue::Float(1.5), 0, 0, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "V".to_string(),
            precision: 2,
            upper_disp_limit: 10.0,
            lower_disp_limit: 0.0,
            ..Default::default()
        });
        let data = encode_dbr(23, &snap).unwrap();
        // GR_FLOAT: sts(4)+prec(2)+pad(2)+units(8)+6*f32(24)+value(4) = 44
        assert_eq!(data.len(), 44);
        assert_eq!(&data[4..6], &2i16.to_be_bytes()); // precision
        assert_eq!(data[8], b'V');
        // upper_disp as f32
        assert_eq!(&data[16..20], &10.0f32.to_be_bytes());
    }

    #[test]
    fn test_encode_gr_long_with_metadata() {
        let mut snap = Snapshot::new(EpicsValue::Long(99), 0, 0, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "cnt".to_string(),
            upper_disp_limit: 10000.0,
            lower_disp_limit: 0.0,
            ..Default::default()
        });
        let data = encode_dbr(26, &snap).unwrap();
        // GR_LONG: sts(4)+units(8)+6*i32(24)+value(4) = 40
        assert_eq!(data.len(), 40);
        assert_eq!(&data[12..16], &10000i32.to_be_bytes()); // upper_disp
        assert_eq!(&data[36..40], &99i32.to_be_bytes());
    }

    #[test]
    fn test_encode_gr_char_with_metadata() {
        let mut snap = Snapshot::new(EpicsValue::Char(42), 0, 0, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "raw".to_string(),
            upper_disp_limit: 255.0,
            lower_disp_limit: 0.0,
            ..Default::default()
        });
        let data = encode_dbr(25, &snap).unwrap();
        // GR_CHAR: sts(4)+units(8)+6*u8(6)+pad(1)+value(1) = 20
        assert_eq!(data.len(), 20);
        assert_eq!(data[12], 255); // upper_disp as u8
        assert_eq!(data[13], 0);   // lower_disp as u8
        assert_eq!(data[19], 42);  // value
    }

    #[test]
    fn test_encode_gr_enum_with_strings() {
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, SystemTime::UNIX_EPOCH);
        snap.enums = Some(EnumInfo {
            strings: vec!["Off".to_string(), "On".to_string()],
        });
        let data = encode_dbr(24, &snap).unwrap();
        // GR_ENUM: sts(4)+no_str(2)+strs(416)+value(2) = 424
        assert_eq!(data.len(), 424);
        // no_str = 2
        assert_eq!(&data[4..6], &2u16.to_be_bytes());
        // First string "Off" at offset 6
        assert_eq!(&data[6..9], b"Off");
        assert_eq!(data[9], 0); // null terminated
        // Second string "On" at offset 6+26=32
        assert_eq!(&data[32..34], b"On");
        // Value at 422..424
        assert_eq!(&data[422..424], &1u16.to_be_bytes());
    }

    #[test]
    fn test_encode_gr_none_metadata_matches_serialize() {
        // When metadata is None, encode_dbr should produce same bytes as serialize_dbr (all zeros)
        let val = EpicsValue::Double(42.0);
        let snap = bare_snapshot(val.clone());
        let encoded = encode_dbr(27, &snap).unwrap();
        let legacy = serialize_dbr(27, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(encoded, legacy);
    }

    #[test]
    fn test_encode_ctrl_none_metadata_matches_serialize() {
        let val = EpicsValue::Long(99);
        let snap = bare_snapshot(val.clone());
        let encoded = encode_dbr(33, &snap).unwrap();
        let legacy = serialize_dbr(33, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(encoded, legacy);
    }

    #[test]
    fn test_encode_ctrl_short_with_ctrl_limits() {
        let mut snap = Snapshot::new(EpicsValue::Short(10), 0, 0, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "mA".to_string(),
            upper_disp_limit: 100.0,
            lower_disp_limit: 0.0,
            ..Default::default()
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 80.0,
            lower_ctrl_limit: 5.0,
        });
        let data = encode_dbr(29, &snap).unwrap();
        // CTRL_SHORT: sts(4)+units(8)+8*i16(16)+value(2) = 30
        assert_eq!(data.len(), 30);
        // ctrl limits at offsets after 6 display limits
        // limits: [100, 0, 0, 0, 0, 0, 80, 5] as i16, at offset 12
        assert_eq!(&data[12..14], &100i16.to_be_bytes()); // upper_disp
        assert_eq!(&data[24..26], &80i16.to_be_bytes());  // upper_ctrl
        assert_eq!(&data[26..28], &5i16.to_be_bytes());   // lower_ctrl
        assert_eq!(&data[28..30], &10i16.to_be_bytes());  // value
    }

    #[test]
    fn test_encode_invalid_type() {
        let snap = bare_snapshot(EpicsValue::Double(0.0));
        assert!(encode_dbr(35, &snap).is_err());
        assert!(encode_dbr(100, &snap).is_err());
    }

    #[test]
    fn test_parse_menu_string_alarm_sevr() {
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "NO_ALARM").unwrap(), EpicsValue::Short(0));
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "MINOR").unwrap(), EpicsValue::Short(1));
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "MAJOR").unwrap(), EpicsValue::Short(2));
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "INVALID").unwrap(), EpicsValue::Short(3));
    }

    #[test]
    fn test_parse_menu_string_omsl() {
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "supervisory").unwrap(), EpicsValue::Short(0));
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "closed_loop").unwrap(), EpicsValue::Short(1));
    }

    #[test]
    fn test_parse_menu_string_enum_type() {
        assert_eq!(EpicsValue::parse(DbFieldType::Enum, "NO_ALARM").unwrap(), EpicsValue::Enum(0));
        assert_eq!(EpicsValue::parse(DbFieldType::Enum, "MAJOR").unwrap(), EpicsValue::Enum(2));
    }

    #[test]
    fn test_parse_menu_string_numeric_priority() {
        // Numeric parsing should take priority over menu strings
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "0").unwrap(), EpicsValue::Short(0));
        assert_eq!(EpicsValue::parse(DbFieldType::Short, "42").unwrap(), EpicsValue::Short(42));
        assert_eq!(EpicsValue::parse(DbFieldType::Enum, "3").unwrap(), EpicsValue::Enum(3));
    }

    #[test]
    fn test_parse_menu_string_unknown() {
        assert!(EpicsValue::parse(DbFieldType::Short, "UNKNOWN_MENU").is_err());
        assert!(EpicsValue::parse(DbFieldType::Enum, "UNKNOWN_MENU").is_err());
    }
}
