use crate::error::{CaError, CaResult};

// DBR type ranges: native(0-6), STS(7-13), TIME(14-20), GR(21-27), CTRL(28-34)
pub const DBR_STS_STRING: u16 = 7;
pub const DBR_TIME_STRING: u16 = 14;
pub const DBR_TIME_SHORT: u16 = 15;
pub const DBR_TIME_FLOAT: u16 = 16;
pub const DBR_TIME_ENUM: u16 = 17;
pub const DBR_TIME_CHAR: u16 = 18;
pub const DBR_TIME_LONG: u16 = 19;
pub const DBR_TIME_DOUBLE: u16 = 20;

/// EPICS DBR field types (native types only)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DbFieldType {
    String = 0,
    Short = 1, // aka Int16
    Float = 2,
    Enum = 3,
    Char = 4, // aka UInt8
    Long = 5, // aka Int32
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

    /// Return the DBR_TIME_xxx type code for this native type.
    pub fn time_dbr_type(&self) -> u16 {
        *self as u16 + 14
    }

    /// Return the DBR_CTRL_xxx type code for this native type.
    pub fn ctrl_dbr_type(&self) -> u16 {
        *self as u16 + 28
    }

    /// Calculate total buffer size for N elements of this type.
    /// Equivalent to C EPICS dbValueSize(type) * count.
    pub fn buffer_size(&self, count: usize) -> usize {
        self.element_size() * count
    }

    /// Map field type to request type (C EPICS mapDBFToDBR).
    /// DBF_MENU and DBF_DEVICE map to DBR_ENUM in C EPICS.
    /// In Rust these are already represented as DbFieldType::Enum,
    /// so this is an identity mapping for documentation/completeness.
    pub fn to_dbr_type(&self) -> DbFieldType {
        *self
    }
}

/// Calculate buffer size for a DBR type including metadata.
/// dbr_type 0-6: value only
/// dbr_type 7-13 (STS): +4 bytes (status + severity)
/// dbr_type 14-20 (TIME): +12 bytes (status + severity + stamp)
/// dbr_type 21-27 (GR): variable (includes limits, units, precision)
/// dbr_type 28-34 (CTRL): variable (includes control limits)
pub fn dbr_buffer_size(dbr_type: u16, native_type: DbFieldType, count: usize) -> usize {
    let value_size = native_type.element_size() * count;
    let meta_size = match dbr_type / 7 {
        0 => 0,  // Plain
        1 => 4,  // STS: status(2) + severity(2)
        2 => 12, // TIME: status(2) + severity(2) + stamp(8)
        3 => {
            // GR: varies by type
            match native_type {
                DbFieldType::String => 4,
                DbFieldType::Enum => 4 + 16 * 26, // status + enum strings
                _ => 4 + 8 + 16 + 8 * 6,          // status + precision + units + 6 limits
            }
        }
        4 => {
            // CTRL: varies by type
            match native_type {
                DbFieldType::String => 4,
                DbFieldType::Enum => 4 + 16 * 26,
                _ => 4 + 8 + 16 + 8 * 8, // status + precision + units + 8 limits
            }
        }
        _ => 0,
    };
    meta_size + value_size
}

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

pub fn native_type_for_dbr(dbr_type: u16) -> CaResult<DbFieldType> {
    match dbr_native_index(dbr_type) {
        Some(idx) => DbFieldType::from_u16(idx),
        None => Err(CaError::UnsupportedType(dbr_type)),
    }
}
