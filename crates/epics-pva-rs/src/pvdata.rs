use std::fmt;

/// PVA scalar types (wire encoding uses typeCodeLUT mapping, not these ordinals directly)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Wire type code (from C++ typeCodeLUT in FieldCreateFactory.cpp)
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

    /// Decode from wire type code
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

    /// Scalar array type code (scalar code | 0x08)
    pub fn array_type_code(self) -> u8 {
        self.type_code() | 0x08
    }

    /// Decode scalar type from array type code
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

/// Runtime scalar value
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
    String(String),
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

    /// Parse a string into a ScalarValue of the given type
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
            ScalarType::String => Ok(Self::String(s.to_string())),
        }
    }
}

/// Runtime PV field value (recursive)
#[derive(Debug, Clone)]
pub enum PvField {
    Scalar(ScalarValue),
    ScalarArray(Vec<ScalarValue>),
    Structure(PvStructure),
}

impl fmt::Display for PvField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scalar(v) => write!(f, "{v}"),
            Self::ScalarArray(arr) => {
                write!(f, "[")?;
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Self::Structure(s) => write!(f, "{s}"),
        }
    }
}

/// A PVA structure with ordered named fields
#[derive(Debug, Clone)]
pub struct PvStructure {
    pub struct_id: String,
    pub fields: Vec<(String, PvField)>,
}

impl PvStructure {
    pub fn new(struct_id: &str) -> Self {
        Self {
            struct_id: struct_id.to_string(),
            fields: Vec::new(),
        }
    }

    pub fn get_field(&self, name: &str) -> Option<&PvField> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn get_value(&self) -> Option<&ScalarValue> {
        match self.get_field("value")? {
            PvField::Scalar(v) => Some(v),
            _ => None,
        }
    }

    pub fn get_alarm(&self) -> Option<&PvStructure> {
        match self.get_field("alarm")? {
            PvField::Structure(s) => Some(s),
            _ => None,
        }
    }

    pub fn get_timestamp(&self) -> Option<&PvStructure> {
        match self.get_field("timeStamp")? {
            PvField::Structure(s) => Some(s),
            _ => None,
        }
    }
}

impl fmt::Display for PvStructure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // For display, just show the value field if it exists (NTScalar-like)
        if let Some(val) = self.get_value() {
            write!(f, "{val}")
        } else {
            write!(f, "structure {} {{", self.struct_id)?;
            for (name, field) in &self.fields {
                write!(f, " {name}={field}")?;
            }
            write!(f, " }}")
        }
    }
}

/// Description of a field's type (for introspection, no values)
#[derive(Debug, Clone)]
pub enum FieldDesc {
    Scalar(ScalarType),
    ScalarArray(ScalarType),
    Structure {
        struct_id: String,
        fields: Vec<(String, FieldDesc)>,
    },
}

impl FieldDesc {
    /// Get the scalar type of a "value" field in a structure
    pub fn value_scalar_type(&self) -> Option<ScalarType> {
        match self {
            FieldDesc::Structure { fields, .. } => {
                for (name, desc) in fields {
                    if name == "value" {
                        if let FieldDesc::Scalar(st) = desc {
                            return Some(*st);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Get the number of fields (for structures)
    pub fn field_count(&self) -> usize {
        match self {
            FieldDesc::Structure { fields, .. } => fields.len(),
            _ => 0,
        }
    }
}

impl fmt::Display for FieldDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indent(f, 0)
    }
}

impl FieldDesc {
    fn fmt_indent(&self, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
        let pad = "    ".repeat(indent);
        match self {
            FieldDesc::Scalar(st) => write!(f, "{st}"),
            FieldDesc::ScalarArray(st) => write!(f, "{st}[]"),
            FieldDesc::Structure { struct_id, fields } => {
                if struct_id.is_empty() {
                    writeln!(f, "structure")?;
                } else {
                    writeln!(f, "structure {struct_id}")?;
                }
                for (name, desc) in fields {
                    write!(f, "{pad}    {name}: ")?;
                    desc.fmt_indent(f, indent + 1)?;
                    writeln!(f)?;
                }
                Ok(())
            }
        }
    }
}
