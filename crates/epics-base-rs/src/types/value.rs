use crate::error::{CaError, CaResult};
use std::fmt;

use super::DbFieldType;

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
            Self::CharArray(arr) => match std::str::from_utf8(arr) {
                Ok(s) => write!(f, "{s}"),
                Err(_) => write!(f, "{arr:?}"),
            },
        }
    }
}

impl EpicsValue {
    /// Deserialize a value from raw bytes based on DBR type
    pub fn from_bytes(dbr_type: DbFieldType, data: &[u8]) -> CaResult<Self> {
        match dbr_type {
            DbFieldType::String => {
                let end = data
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(data.len().min(40));
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
                    if offset + 2 > data.len() {
                        break;
                    }
                    arr.push(i16::from_be_bytes([data[offset], data[offset + 1]]));
                }
                Ok(Self::ShortArray(arr))
            }
            DbFieldType::Float => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 4;
                    if offset + 4 > data.len() {
                        break;
                    }
                    arr.push(f32::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]));
                }
                Ok(Self::FloatArray(arr))
            }
            DbFieldType::Enum => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 2;
                    if offset + 2 > data.len() {
                        break;
                    }
                    arr.push(u16::from_be_bytes([data[offset], data[offset + 1]]));
                }
                Ok(Self::EnumArray(arr))
            }
            DbFieldType::Double => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 8;
                    if offset + 8 > data.len() {
                        break;
                    }
                    arr.push(f64::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                        data[offset + 4],
                        data[offset + 5],
                        data[offset + 6],
                        data[offset + 7],
                    ]));
                }
                Ok(Self::DoubleArray(arr))
            }
            DbFieldType::Long => {
                let mut arr = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = i * 4;
                    if offset + 4 > data.len() {
                        break;
                    }
                    arr.push(i32::from_be_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
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

    /// Truncate an array value to at most `max` elements. Scalars are unchanged.
    pub fn truncate(&mut self, max: usize) {
        match self {
            Self::ShortArray(arr) => arr.truncate(max),
            Self::FloatArray(arr) => arr.truncate(max),
            Self::EnumArray(arr) => arr.truncate(max),
            Self::DoubleArray(arr) => arr.truncate(max),
            Self::LongArray(arr) => arr.truncate(max),
            Self::CharArray(arr) => arr.truncate(max),
            _ => {}
        }
    }

    /// Convert to a different native type (scalar only; arrays use first element).
    pub fn convert_to(&self, target: DbFieldType) -> EpicsValue {
        if self.dbr_type() == target {
            return self.clone();
        }
        // Menu string resolution: when converting String to Short/Enum,
        // try resolve_menu_string first (e.g. "MINOR" -> 1).
        if let EpicsValue::String(s) = self {
            match target {
                DbFieldType::Short => {
                    if let Some(idx) = Self::resolve_menu_string(s) {
                        return EpicsValue::Short(idx);
                    }
                }
                DbFieldType::Enum => {
                    if let Some(idx) = Self::resolve_menu_string(s) {
                        return EpicsValue::Enum(idx as u16);
                    }
                }
                _ => {}
            }
        }
        match target {
            DbFieldType::String => EpicsValue::String(format!("{self}")),
            DbFieldType::Short => EpicsValue::Short(self.to_f64().unwrap_or(0.0) as i16),
            DbFieldType::Float => EpicsValue::Float(self.to_f64().unwrap_or(0.0) as f32),
            DbFieldType::Enum => EpicsValue::Enum(self.to_f64().unwrap_or(0.0) as u16),
            DbFieldType::Char => {
                // String → CharArray (for waveform FTVL=CHAR)
                if let EpicsValue::String(s) = self {
                    EpicsValue::CharArray(s.as_bytes().to_vec())
                } else {
                    EpicsValue::Char(self.to_f64().unwrap_or(0.0) as u8)
                }
            }
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
        // C EPICS treats empty/whitespace strings as zero for numeric fields
        let s = s.trim();
        if s.is_empty() {
            return match dbr_type {
                DbFieldType::String => Ok(Self::String(String::new())),
                DbFieldType::Short => Ok(Self::Short(0)),
                DbFieldType::Float => Ok(Self::Float(0.0)),
                DbFieldType::Enum => Ok(Self::Enum(0)),
                DbFieldType::Char => Ok(Self::Char(0)),
                DbFieldType::Long => Ok(Self::Long(0)),
                DbFieldType::Double => Ok(Self::Double(0.0)),
            };
        }
        match dbr_type {
            DbFieldType::String => Ok(Self::String(s.to_string())),
            DbFieldType::Short => Self::parse_int(s)
                .map(|v| Self::Short(v as i16))
                .or_else(|_| {
                    Self::resolve_menu_string(s)
                        .map(Self::Short)
                        .ok_or_else(|| {
                            CaError::InvalidValue(format!("invalid short or menu string: {s}"))
                        })
                }),
            DbFieldType::Float => s
                .parse::<f32>()
                .map(Self::Float)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Enum => Self::parse_int(s)
                .map(|v| Self::Enum(v as u16))
                .or_else(|_| {
                    Self::resolve_menu_string(s)
                        .map(|v| Self::Enum(v as u16))
                        .ok_or_else(|| {
                            CaError::InvalidValue(format!("invalid enum or menu string: {s}"))
                        })
                }),
            DbFieldType::Char => Self::parse_int(s)
                .map(|v| Self::Char(v as u8))
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Long => Self::parse_int(s)
                .map(|v| Self::Long(v as i32))
                .map_err(|e| CaError::InvalidValue(e.to_string())),
            DbFieldType::Double => s
                .parse::<f64>()
                .map(Self::Double)
                .map_err(|e| CaError::InvalidValue(e.to_string())),
        }
    }

    /// Parse an integer string with C-style radix prefixes (0x for hex, 0 for octal).
    fn parse_int(s: &str) -> CaResult<i64> {
        let s = s.trim();
        if s.starts_with("0x") || s.starts_with("0X") {
            i64::from_str_radix(&s[2..], 16).map_err(|e| CaError::InvalidValue(e.to_string()))
        } else if s.starts_with('0')
            && s.len() > 1
            && s.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
        {
            i64::from_str_radix(&s[1..], 8).map_err(|e| CaError::InvalidValue(e.to_string()))
        } else {
            s.parse::<i64>()
                .map_err(|e| CaError::InvalidValue(e.to_string()))
        }
    }
}
