pub mod arrays;
pub mod common;
pub mod enum_type;
pub mod float64;
pub mod generic_pointer;
pub mod gpib;
pub mod int32;
pub mod int64;
pub mod motor;
pub mod octet;
pub mod option;
pub mod uint32_digital;

/// Type-safe interface type enum replacing string-based dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InterfaceType {
    Int32,
    Int64,
    Float64,
    Octet,
    UInt32Digital,
    Enum,
    GenericPointer,
    Int8Array,
    Int16Array,
    Int32Array,
    Int64Array,
    Float32Array,
    Float64Array,
    Motor,
    Gpib,
    Option,
    Common,
}

impl InterfaceType {
    /// Parse from a C asyn interface name string.
    pub fn from_asyn_name(name: &str) -> std::option::Option<Self> {
        match name {
            "asynInt32" => Some(Self::Int32),
            "asynInt64" => Some(Self::Int64),
            "asynFloat64" => Some(Self::Float64),
            "asynOctet" => Some(Self::Octet),
            "asynUInt32Digital" => Some(Self::UInt32Digital),
            "asynEnum" => Some(Self::Enum),
            "asynGenericPointer" => Some(Self::GenericPointer),
            "asynInt8Array" => Some(Self::Int8Array),
            "asynInt16Array" => Some(Self::Int16Array),
            "asynInt32Array" => Some(Self::Int32Array),
            "asynInt64Array" => Some(Self::Int64Array),
            "asynFloat32Array" => Some(Self::Float32Array),
            "asynFloat64Array" => Some(Self::Float64Array),
            "asynMotor" => Some(Self::Motor),
            "asynGpib" => Some(Self::Gpib),
            "asynOption" => Some(Self::Option),
            "asynCommon" => Some(Self::Common),
            _ => None,
        }
    }

    /// Return the C asyn interface name string.
    pub fn asyn_name(&self) -> &'static str {
        match self {
            Self::Int32 => "asynInt32",
            Self::Int64 => "asynInt64",
            Self::Float64 => "asynFloat64",
            Self::Octet => "asynOctet",
            Self::UInt32Digital => "asynUInt32Digital",
            Self::Enum => "asynEnum",
            Self::GenericPointer => "asynGenericPointer",
            Self::Int8Array => "asynInt8Array",
            Self::Int16Array => "asynInt16Array",
            Self::Int32Array => "asynInt32Array",
            Self::Int64Array => "asynInt64Array",
            Self::Float32Array => "asynFloat32Array",
            Self::Float64Array => "asynFloat64Array",
            Self::Motor => "asynMotor",
            Self::Gpib => "asynGpib",
            Self::Option => "asynOption",
            Self::Common => "asynCommon",
        }
    }
}

impl std::fmt::Display for InterfaceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.asyn_name())
    }
}

/// Type-safe capability enum for declaring what a port driver supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Int32Read,
    Int32Write,
    Int64Read,
    Int64Write,
    Float64Read,
    Float64Write,
    OctetRead,
    OctetWrite,
    UInt32DigitalRead,
    UInt32DigitalWrite,
    EnumRead,
    EnumWrite,
    GenericPointerRead,
    GenericPointerWrite,
    Int8ArrayRead,
    Int8ArrayWrite,
    Int16ArrayRead,
    Int16ArrayWrite,
    Int32ArrayRead,
    Int32ArrayWrite,
    Int64ArrayRead,
    Int64ArrayWrite,
    Float32ArrayRead,
    Float32ArrayWrite,
    Float64ArrayRead,
    Float64ArrayWrite,
    Motor,
    Gpib,
    Flush,
    Connect,
}

impl Capability {
    /// Return the interface type this capability belongs to.
    pub fn interface_type(&self) -> InterfaceType {
        match self {
            Self::Int32Read | Self::Int32Write => InterfaceType::Int32,
            Self::Int64Read | Self::Int64Write => InterfaceType::Int64,
            Self::Float64Read | Self::Float64Write => InterfaceType::Float64,
            Self::OctetRead | Self::OctetWrite => InterfaceType::Octet,
            Self::UInt32DigitalRead | Self::UInt32DigitalWrite => InterfaceType::UInt32Digital,
            Self::EnumRead | Self::EnumWrite => InterfaceType::Enum,
            Self::GenericPointerRead | Self::GenericPointerWrite => InterfaceType::GenericPointer,
            Self::Int8ArrayRead | Self::Int8ArrayWrite => InterfaceType::Int8Array,
            Self::Int16ArrayRead | Self::Int16ArrayWrite => InterfaceType::Int16Array,
            Self::Int32ArrayRead | Self::Int32ArrayWrite => InterfaceType::Int32Array,
            Self::Int64ArrayRead | Self::Int64ArrayWrite => InterfaceType::Int64Array,
            Self::Float32ArrayRead | Self::Float32ArrayWrite => InterfaceType::Float32Array,
            Self::Float64ArrayRead | Self::Float64ArrayWrite => InterfaceType::Float64Array,
            Self::Motor => InterfaceType::Motor,
            Self::Gpib => InterfaceType::Gpib,
            Self::Flush | Self::Connect => InterfaceType::Common,
        }
    }

    /// True if this is a read capability.
    pub fn is_read(&self) -> bool {
        matches!(
            self,
            Self::Int32Read
                | Self::Int64Read
                | Self::Float64Read
                | Self::OctetRead
                | Self::UInt32DigitalRead
                | Self::EnumRead
                | Self::GenericPointerRead
                | Self::Int8ArrayRead
                | Self::Int16ArrayRead
                | Self::Int32ArrayRead
                | Self::Int64ArrayRead
                | Self::Float32ArrayRead
                | Self::Float64ArrayRead
        )
    }

    /// True if this is a write capability.
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Self::Int32Write
                | Self::Int64Write
                | Self::Float64Write
                | Self::OctetWrite
                | Self::UInt32DigitalWrite
                | Self::EnumWrite
                | Self::GenericPointerWrite
                | Self::Int8ArrayWrite
                | Self::Int16ArrayWrite
                | Self::Int32ArrayWrite
                | Self::Int64ArrayWrite
                | Self::Float32ArrayWrite
                | Self::Float64ArrayWrite
        )
    }
}

/// Default capabilities for port drivers that only support scalar cache-based I/O.
pub fn default_capabilities() -> Vec<Capability> {
    vec![
        Capability::Int32Read,
        Capability::Int32Write,
        Capability::Int64Read,
        Capability::Int64Write,
        Capability::Float64Read,
        Capability::Float64Write,
        Capability::OctetRead,
        Capability::OctetWrite,
        Capability::UInt32DigitalRead,
        Capability::UInt32DigitalWrite,
        Capability::EnumRead,
        Capability::EnumWrite,
        Capability::GenericPointerRead,
        Capability::GenericPointerWrite,
        Capability::Flush,
        Capability::Connect,
    ]
}

#[cfg(test)]
mod interface_type_tests {
    use super::*;

    #[test]
    fn from_asyn_name_roundtrip() {
        let names = [
            "asynInt32",
            "asynInt64",
            "asynFloat64",
            "asynOctet",
            "asynUInt32Digital",
            "asynEnum",
            "asynGenericPointer",
            "asynInt8Array",
            "asynInt16Array",
            "asynInt32Array",
            "asynInt64Array",
            "asynFloat32Array",
            "asynFloat64Array",
            "asynMotor",
            "asynGpib",
            "asynOption",
            "asynCommon",
        ];
        for name in &names {
            let iface = InterfaceType::from_asyn_name(name)
                .unwrap_or_else(|| panic!("failed to parse {name}"));
            assert_eq!(iface.asyn_name(), *name, "roundtrip failed for {name}");
        }
    }

    #[test]
    fn from_asyn_name_unknown() {
        assert!(InterfaceType::from_asyn_name("asynFoo").is_none());
        assert!(InterfaceType::from_asyn_name("").is_none());
    }

    #[test]
    fn display_format() {
        assert_eq!(format!("{}", InterfaceType::Int32), "asynInt32");
        assert_eq!(
            format!("{}", InterfaceType::Float64Array),
            "asynFloat64Array"
        );
    }

    #[test]
    fn capability_interface_type() {
        assert_eq!(Capability::Int32Read.interface_type(), InterfaceType::Int32);
        assert_eq!(
            Capability::Float64Write.interface_type(),
            InterfaceType::Float64
        );
        assert_eq!(Capability::Motor.interface_type(), InterfaceType::Motor);
    }

    #[test]
    fn capability_read_write() {
        assert!(Capability::Int32Read.is_read());
        assert!(!Capability::Int32Read.is_write());
        assert!(Capability::Int32Write.is_write());
        assert!(!Capability::Int32Write.is_read());
    }

    #[test]
    fn default_capabilities_has_scalars() {
        let caps = default_capabilities();
        assert!(caps.contains(&Capability::Int32Read));
        assert!(caps.contains(&Capability::Float64Write));
        assert!(caps.contains(&Capability::OctetRead));
        assert!(!caps.contains(&Capability::Motor));
        assert!(!caps.contains(&Capability::Int32ArrayRead));
    }

    #[test]
    fn all_variants_have_asyn_name() {
        let all = [
            InterfaceType::Int32,
            InterfaceType::Int64,
            InterfaceType::Float64,
            InterfaceType::Octet,
            InterfaceType::UInt32Digital,
            InterfaceType::Enum,
            InterfaceType::GenericPointer,
            InterfaceType::Int8Array,
            InterfaceType::Int16Array,
            InterfaceType::Int32Array,
            InterfaceType::Int64Array,
            InterfaceType::Float32Array,
            InterfaceType::Float64Array,
            InterfaceType::Motor,
            InterfaceType::Gpib,
            InterfaceType::Option,
            InterfaceType::Common,
        ];
        for iface in &all {
            let name = iface.asyn_name();
            assert!(!name.is_empty());
            let parsed = InterfaceType::from_asyn_name(name).unwrap();
            assert_eq!(parsed, *iface);
        }
    }
}
