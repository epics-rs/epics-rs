use crate::error::{CaError, CaResult};

// DBR type ranges (matches db_access.h):
//   native (0..=6), STS (7..=13), TIME (14..=20), GR (21..=27),
//   CTRL (28..=34), special PUT_ACKT (35), PUT_ACKS (36),
//   STSACK_STRING (37), CLASS_NAME (38).

// Native scalar types (also exposed via DbFieldType enum)
pub const DBR_STRING: u16 = 0;
pub const DBR_SHORT: u16 = 1;
pub const DBR_FLOAT: u16 = 2;
pub const DBR_ENUM: u16 = 3;
pub const DBR_CHAR: u16 = 4;
pub const DBR_LONG: u16 = 5;
pub const DBR_DOUBLE: u16 = 6;
// `DBR_INT` is the libca alias for `DBR_SHORT`.
pub const DBR_INT: u16 = DBR_SHORT;

// Status-only metadata layer (CA-261)
pub const DBR_STS_STRING: u16 = 7;
pub const DBR_STS_SHORT: u16 = 8;
pub const DBR_STS_FLOAT: u16 = 9;
pub const DBR_STS_ENUM: u16 = 10;
pub const DBR_STS_CHAR: u16 = 11;
pub const DBR_STS_LONG: u16 = 12;
pub const DBR_STS_DOUBLE: u16 = 13;
pub const DBR_STS_INT: u16 = DBR_STS_SHORT;

// Status + timestamp layer (CA-262)
pub const DBR_TIME_STRING: u16 = 14;
pub const DBR_TIME_SHORT: u16 = 15;
pub const DBR_TIME_FLOAT: u16 = 16;
pub const DBR_TIME_ENUM: u16 = 17;
pub const DBR_TIME_CHAR: u16 = 18;
pub const DBR_TIME_LONG: u16 = 19;
pub const DBR_TIME_DOUBLE: u16 = 20;
pub const DBR_TIME_INT: u16 = DBR_TIME_SHORT;

// Status + graphic (display limits / units / precision) layer (CA-263)
pub const DBR_GR_STRING: u16 = 21;
pub const DBR_GR_SHORT: u16 = 22;
pub const DBR_GR_FLOAT: u16 = 23;
pub const DBR_GR_ENUM: u16 = 24;
pub const DBR_GR_CHAR: u16 = 25;
pub const DBR_GR_LONG: u16 = 26;
pub const DBR_GR_DOUBLE: u16 = 27;
pub const DBR_GR_INT: u16 = DBR_GR_SHORT;

// Status + graphic + control limits layer (CA-264)
pub const DBR_CTRL_STRING: u16 = 28;
pub const DBR_CTRL_SHORT: u16 = 29;
pub const DBR_CTRL_FLOAT: u16 = 30;
pub const DBR_CTRL_ENUM: u16 = 31;
pub const DBR_CTRL_CHAR: u16 = 32;
pub const DBR_CTRL_LONG: u16 = 33;
pub const DBR_CTRL_DOUBLE: u16 = 34;
pub const DBR_CTRL_INT: u16 = DBR_CTRL_SHORT;

// Special alarm-acknowledgement / introspection types
pub const DBR_PUT_ACKT: u16 = 35;
pub const DBR_PUT_ACKS: u16 = 36;
pub const DBR_STSACK_STRING: u16 = 37;
/// Returns the IOC's record-type class name as a 40-byte string
/// (CA-268, db_access.h: `DBR_CLASS_NAME`).
pub const DBR_CLASS_NAME: u16 = 38;

/// Last allocated DBR type code, matching the C `LAST_BUFFER_TYPE` macro.
pub const LAST_BUFFER_TYPE: u16 = DBR_CLASS_NAME;

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
    /// Internal-only type for int64in/int64out records.
    /// No CA wire type 7 exists; over CA these PVs appear as Double (type 6).
    Int64 = 7,
    /// Internal-only type for unsigned 64-bit EPICS fields (C `DBF_UINT64`,
    /// dbStatic `dbfType` index 8). The CA wire protocol has no 64-bit
    /// type, so over CA these PVs appear as Double (type 6); over PVA they
    /// are served natively as `ulong`. Mirrors `Int64`'s CA handling.
    UInt64 = 8,
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
            Self::Double | Self::Int64 | Self::UInt64 => 8,
        }
    }

    /// Return the wire type code as a `u16`. Int64/UInt64 have no CA wire
    /// type and are reported as `DBR_DOUBLE` (6) for over-the-wire purposes.
    fn ca_wire_type(&self) -> u16 {
        match self {
            Self::Int64 | Self::UInt64 => Self::Double as u16,
            other => *other as u16,
        }
    }

    /// Return the `DBR_STS_xxx` type code for this native type
    /// (Int64 maps to `DBR_STS_DOUBLE`).
    pub fn sts_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 7
    }

    /// Return the `DBR_TIME_xxx` type code for this native type
    /// (Int64 maps to `DBR_TIME_DOUBLE`).
    pub fn time_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 14
    }

    /// Return the `DBR_GR_xxx` type code for this native type
    /// (Int64 maps to `DBR_GR_DOUBLE`).
    pub fn gr_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 21
    }

    /// Return the `DBR_CTRL_xxx` type code for this native type
    /// (Int64 maps to `DBR_CTRL_DOUBLE`).
    pub fn ctrl_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 28
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
/// dbr_type 38 (CLASS_NAME): one fixed 40-byte string
pub fn dbr_buffer_size(dbr_type: u16, native_type: DbFieldType, count: usize) -> usize {
    let value_size = native_type.element_size() * count;
    let meta_size = match dbr_type / 7 {
        0 => 0, // Plain
        1 => {
            // STS: status(2) + severity(2), plus the per-type RISC
            // alignment pad that sits before `value` in the C
            // `dbr_sts_*` structs (db_access.h:192-240):
            //   sts_char:   +RISC_pad(1)  → meta 5
            //   sts_double: +RISC_pad(4)  → meta 8 (dbr_long_t)
            //   others:     no pad        → meta 4
            // Int64/UInt64 are served over CA as DBR_DOUBLE so they share
            // the double pad.
            match native_type {
                DbFieldType::Char => 5,
                DbFieldType::Double | DbFieldType::Int64 | DbFieldType::UInt64 => 8,
                _ => 4,
            }
        }
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
    // DBR_CLASS_NAME (38) is always one MAX_STRING_SIZE (40) string,
    // regardless of `count` or `native_type`. Use the inverse path so
    // buffer-sizing callers don't accidentally over-allocate.
    if dbr_type == DBR_CLASS_NAME {
        return 40;
    }
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
        // Alarm-acknowledge writes carry a single u16, so map them to
        // Short for codec purposes. STSACK_STRING returns a string body
        // so it maps to String.
        35 | 36 => Some(1), // DBR_PUT_ACKT / DBR_PUT_ACKS — u16
        37 => Some(0),      // DBR_STSACK_STRING — value is a string
        // DBR_CLASS_NAME is a single fixed 40-byte string carrying the
        // record's recordType. Treat as String for codec purposes.
        38 => Some(0),
        _ => None,
    }
}

pub fn native_type_for_dbr(dbr_type: u16) -> CaResult<DbFieldType> {
    match dbr_native_index(dbr_type) {
        Some(idx) => DbFieldType::from_u16(idx),
        None => Err(CaError::UnsupportedType(dbr_type)),
    }
}

#[cfg(test)]
mod buffer_size_tests {
    use super::*;

    /// H-1: STS meta size is per-type. `dbr_sts_double` carries a
    /// 4-byte `dbr_long_t` RISC_pad (db_access.h:233-238) → meta 8.
    #[test]
    fn sts_double_meta_is_8() {
        // scalar: 8 (meta) + 8 (value) = 16
        assert_eq!(dbr_buffer_size(DBR_STS_DOUBLE, DbFieldType::Double, 1), 16);
        // n elements: 8 + 8*n
        assert_eq!(
            dbr_buffer_size(DBR_STS_DOUBLE, DbFieldType::Double, 5),
            8 + 8 * 5
        );
    }

    /// H-1: `dbr_sts_char` carries a 1-byte RISC_pad
    /// (db_access.h:218-223) → meta 5.
    #[test]
    fn sts_char_meta_is_5() {
        assert_eq!(dbr_buffer_size(DBR_STS_CHAR, DbFieldType::Char, 1), 6);
        assert_eq!(dbr_buffer_size(DBR_STS_CHAR, DbFieldType::Char, 10), 5 + 10);
    }

    /// H-1: types with no STS RISC pad keep the flat 4-byte meta.
    #[test]
    fn sts_short_meta_is_4() {
        assert_eq!(dbr_buffer_size(DBR_STS_SHORT, DbFieldType::Short, 1), 6);
        assert_eq!(dbr_buffer_size(DBR_STS_LONG, DbFieldType::Long, 1), 8);
        assert_eq!(dbr_buffer_size(DBR_STS_FLOAT, DbFieldType::Float, 1), 8);
    }

    /// Plain and TIME layers are unaffected by the STS pad fix.
    #[test]
    fn plain_and_time_unchanged() {
        assert_eq!(dbr_buffer_size(DBR_DOUBLE, DbFieldType::Double, 3), 24);
        assert_eq!(
            dbr_buffer_size(DBR_TIME_DOUBLE, DbFieldType::Double, 1),
            12 + 8
        );
    }
}
