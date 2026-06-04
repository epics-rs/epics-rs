//! Scalar type tag and runtime value enum.

use std::fmt;

use epics_base_rs::types::PvString;

/// PVA scalar types. The wire encoding uses the type-code lookup table from
/// pvData (`FieldCreateFactory.cpp`), not the enum's discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ScalarType {
    Boolean = 0,
    Byte = 1,
    Short = 2,
    Int = 3,
    Long = 4,
    UByte = 5,
    UShort = 6,
    UInt = 7,
    ULong = 8,
    Float = 9,
    Double = 10,
    String = 11,
}

impl ScalarType {
    /// Wire type code (from C++ typeCodeLUT in FieldCreateFactory.cpp).
    pub fn type_code(self) -> u8 {
        match self {
            Self::Boolean => 0x00,
            Self::Byte => 0x20,
            Self::Short => 0x21,
            Self::Int => 0x22,
            Self::Long => 0x23,
            Self::UByte => 0x24,
            Self::UShort => 0x25,
            Self::UInt => 0x26,
            Self::ULong => 0x27,
            Self::Float => 0x42,
            Self::Double => 0x43,
            Self::String => 0x60,
        }
    }

    /// Decode from wire type code.
    pub fn from_type_code(code: u8) -> Option<Self> {
        match code {
            0x00 => Some(Self::Boolean),
            0x20 => Some(Self::Byte),
            0x21 => Some(Self::Short),
            0x22 => Some(Self::Int),
            0x23 => Some(Self::Long),
            0x24 => Some(Self::UByte),
            0x25 => Some(Self::UShort),
            0x26 => Some(Self::UInt),
            0x27 => Some(Self::ULong),
            0x42 => Some(Self::Float),
            0x43 => Some(Self::Double),
            0x60 => Some(Self::String),
            _ => None,
        }
    }

    /// Scalar array type code (`scalar_code | 0x08`).
    pub fn array_type_code(self) -> u8 {
        self.type_code() | 0x08
    }

    /// Element size in bytes for fixed-width scalars. Variable-length
    /// types (`Boolean`, `String`) report `1` since the wire encoding
    /// of `Boolean` is a single byte and `String` has no fixed width.
    /// Mirrors pvxs `TypeCode::size()`.
    pub fn element_size(self) -> usize {
        match self {
            Self::Boolean | Self::Byte | Self::UByte => 1,
            Self::Short | Self::UShort => 2,
            Self::Int | Self::UInt | Self::Float => 4,
            Self::Long | Self::ULong | Self::Double => 8,
            Self::String => 1, // variable; pvxs reports 1 too
        }
    }

    /// Decode scalar type from an array type code.
    pub fn from_array_type_code(code: u8) -> Option<Self> {
        if code & 0x08 != 0 {
            Self::from_type_code(code & !0x08)
        } else {
            None
        }
    }
}

impl fmt::Display for ScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boolean => write!(f, "boolean"),
            Self::Byte => write!(f, "byte"),
            Self::Short => write!(f, "short"),
            Self::Int => write!(f, "int"),
            Self::Long => write!(f, "long"),
            Self::UByte => write!(f, "ubyte"),
            Self::UShort => write!(f, "ushort"),
            Self::UInt => write!(f, "uint"),
            Self::ULong => write!(f, "ulong"),
            Self::Float => write!(f, "float"),
            Self::Double => write!(f, "double"),
            Self::String => write!(f, "string"),
        }
    }
}

/// Runtime scalar value.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Boolean(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    UByte(u8),
    UShort(u16),
    UInt(u32),
    ULong(u64),
    Float(f32),
    Double(f64),
    String(PvString),
}

/// Source-compatibility `Default` for the public [`ScalarValue`] API.
///
/// `ScalarValue` is re-exported from [`crate::pvdata`], so downstream crates
/// can name it directly and rely on `Default` for optional scalar slots, test
/// fixtures, or `T: Default` bounds. The historical default is `Int(0)`; it is
/// kept purely for that compatibility — the NTNDArray attribute model that was
/// the original reason for deriving it now carries its own manual `Default`
/// (`VariantValue::null()`), so this impl no longer has an internal caller and
/// exists only so the exported type's `Default` does not silently disappear.
/// Regression R0604-PVDATA-SCALARVALUE-DEFAULT-API-1.
///
/// ```
/// use epics_pva_rs::pvdata::ScalarValue;
/// assert_eq!(ScalarValue::default(), ScalarValue::Int(0));
/// ```
impl Default for ScalarValue {
    fn default() -> Self {
        ScalarValue::Int(0)
    }
}

/// Typed conversions into [`ScalarValue`], so the pvRequest builder's
/// `record(key, value)` preserves the caller's scalar type — pvxs
/// `CommonBuilder::record<T>` stores `Value::Helper::build(value, vtype)`
/// (client.h:661-675), giving `bool pipeline = true` / `uint queueSize = 8`
/// rather than collapsing every option to a string.
macro_rules! scalar_from {
    ($($t:ty => $variant:ident),+ $(,)?) => {$(
        impl From<$t> for ScalarValue {
            fn from(v: $t) -> Self {
                Self::$variant(v)
            }
        }
    )+};
}
scalar_from! {
    bool => Boolean,
    i8 => Byte,
    i16 => Short,
    i32 => Int,
    i64 => Long,
    u8 => UByte,
    u16 => UShort,
    u32 => UInt,
    u64 => ULong,
    f32 => Float,
    f64 => Double,
}

impl From<String> for ScalarValue {
    fn from(v: String) -> Self {
        Self::String(v.into())
    }
}

impl From<&str> for ScalarValue {
    fn from(v: &str) -> Self {
        Self::String(v.into())
    }
}

impl fmt::Display for ScalarValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boolean(v) => write!(f, "{v}"),
            Self::Byte(v) => write!(f, "{v}"),
            Self::Short(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::Long(v) => write!(f, "{v}"),
            Self::UByte(v) => write!(f, "{v}"),
            Self::UShort(v) => write!(f, "{v}"),
            Self::UInt(v) => write!(f, "{v}"),
            Self::ULong(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Double(v) => write!(f, "{v}"),
            Self::String(v) => write!(f, "{v}"),
        }
    }
}

impl ScalarValue {
    pub fn scalar_type(&self) -> ScalarType {
        match self {
            Self::Boolean(_) => ScalarType::Boolean,
            Self::Byte(_) => ScalarType::Byte,
            Self::Short(_) => ScalarType::Short,
            Self::Int(_) => ScalarType::Int,
            Self::Long(_) => ScalarType::Long,
            Self::UByte(_) => ScalarType::UByte,
            Self::UShort(_) => ScalarType::UShort,
            Self::UInt(_) => ScalarType::UInt,
            Self::ULong(_) => ScalarType::ULong,
            Self::Float(_) => ScalarType::Float,
            Self::Double(_) => ScalarType::Double,
            Self::String(_) => ScalarType::String,
        }
    }

    /// Parse a string into a `ScalarValue` of the given type.
    pub fn parse(scalar_type: ScalarType, s: &str) -> Result<Self, String> {
        match scalar_type {
            ScalarType::Boolean => match s {
                "true" | "1" => Ok(Self::Boolean(true)),
                "false" | "0" => Ok(Self::Boolean(false)),
                _ => Err(format!("invalid boolean: {s}")),
            },
            ScalarType::Byte => s.parse::<i8>().map(Self::Byte).map_err(|e| e.to_string()),
            ScalarType::Short => s.parse::<i16>().map(Self::Short).map_err(|e| e.to_string()),
            ScalarType::Int => s.parse::<i32>().map(Self::Int).map_err(|e| e.to_string()),
            ScalarType::Long => s.parse::<i64>().map(Self::Long).map_err(|e| e.to_string()),
            ScalarType::UByte => s.parse::<u8>().map(Self::UByte).map_err(|e| e.to_string()),
            ScalarType::UShort => s
                .parse::<u16>()
                .map(Self::UShort)
                .map_err(|e| e.to_string()),
            ScalarType::UInt => s.parse::<u32>().map(Self::UInt).map_err(|e| e.to_string()),
            ScalarType::ULong => s.parse::<u64>().map(Self::ULong).map_err(|e| e.to_string()),
            ScalarType::Float => s.parse::<f32>().map(Self::Float).map_err(|e| e.to_string()),
            ScalarType::Double => s
                .parse::<f64>()
                .map(Self::Double)
                .map_err(|e| e.to_string()),
            ScalarType::String => Ok(Self::String(s.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_type_round_trip() {
        for st in [
            ScalarType::Boolean,
            ScalarType::Byte,
            ScalarType::Short,
            ScalarType::Int,
            ScalarType::Long,
            ScalarType::UByte,
            ScalarType::UShort,
            ScalarType::UInt,
            ScalarType::ULong,
            ScalarType::Float,
            ScalarType::Double,
            ScalarType::String,
        ] {
            assert_eq!(ScalarType::from_type_code(st.type_code()), Some(st));
        }
    }

    #[test]
    fn array_codes_have_array_bit() {
        assert_eq!(ScalarType::Double.array_type_code(), 0x4B);
        assert_eq!(ScalarType::String.array_type_code(), 0x68);
        assert_eq!(
            ScalarType::from_array_type_code(0x4B),
            Some(ScalarType::Double)
        );
        // Non-array codes should reject
        assert_eq!(ScalarType::from_array_type_code(0x43), None);
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn parse_scalar() {
        assert_eq!(
            ScalarValue::parse(ScalarType::Double, "3.14").unwrap(),
            ScalarValue::Double(3.14)
        );
        assert_eq!(
            ScalarValue::parse(ScalarType::Boolean, "true").unwrap(),
            ScalarValue::Boolean(true)
        );
        assert!(ScalarValue::parse(ScalarType::Int, "abc").is_err());
    }
}
