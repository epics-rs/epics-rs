pub mod arrays;
pub mod average;
pub mod common;
pub mod enum_type;
pub mod float64;
pub mod generic_pointer;
pub mod gpib;
pub mod int32;
pub mod int64;
pub mod motor;
pub mod octet;
// No `option` module: the asynOption interface has no trait of its own. A
// driver implements it by overriding `PortDriver::get_option` / `set_option`
// and declaring `Capability::Option`, which is what `PortHandle::has_interface`
// (and so asynRecord's OPTIONIV) reads. The former `AsynOption` trait duplicated
// those two methods and no driver implemented it.
pub mod uint32_digital;
pub mod uint64;

/// Type-safe interface type enum replacing string-based dispatch.
///
/// Note: `UInt64` is a **Rust extension** with no upstream asyn C
/// counterpart (upstream issue #231 unmerged). The `"asynUInt64"`
/// token below is recognised so internal Rust drivers can request it,
/// but a real C asyn manager will not advertise this interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InterfaceType {
    Int32,
    Int64,
    /// Rust extension — see module docstring.
    UInt64,
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
    /// The `asynDrvUser` interface — the driver's `drvInfo` → `reason` resolver.
    /// A parameter-library driver registers it (C `asynPortDriver`,
    /// drvAsynUSBTMC.c:1319); a byte transport does not (drvAsynIPPort.c /
    /// drvAsynSerialPort.c register no such interface), and a client asks the
    /// port for it before ever calling `create` — asynRecord.c:1243,
    /// devAsynInt32.c:264, asynOctetSyncIO.c:143.
    DrvUser,
    Common,
}

impl InterfaceType {
    /// Parse from a C asyn interface name string.
    pub fn from_asyn_name(name: &str) -> std::option::Option<Self> {
        match name {
            "asynInt32" => Some(Self::Int32),
            "asynInt64" => Some(Self::Int64),
            "asynUInt64" => Some(Self::UInt64),
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
            "asynDrvUser" => Some(Self::DrvUser),
            "asynCommon" => Some(Self::Common),
            _ => None,
        }
    }

    /// Return the C asyn interface name string.
    pub fn asyn_name(&self) -> &'static str {
        match self {
            Self::Int32 => "asynInt32",
            Self::Int64 => "asynInt64",
            Self::UInt64 => "asynUInt64",
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
            Self::DrvUser => "asynDrvUser",
            Self::Common => "asynCommon",
        }
    }

    /// The name C's `reportInterrupt` prints for this interface's client list
    /// (asynPortDriver.cpp:3697-3708) — the short label, not the interface name.
    /// The interfaces C keeps no interrupt list for fall back to the interface
    /// name, since there is no C text to be faithful to.
    pub fn interrupt_label(&self) -> &'static str {
        match self {
            Self::Int32 => "int32",
            Self::Int64 => "int64",
            Self::UInt32Digital => "uint32",
            Self::Float64 => "float64",
            Self::Octet => "octet",
            Self::Int8Array => "int8Array",
            Self::Int16Array => "int16Array",
            Self::Int32Array => "int32Array",
            Self::Float32Array => "float32Array",
            Self::Float64Array => "float64Array",
            Self::GenericPointer => "genericPointer",
            Self::Enum => "Enum",
            other => other.asyn_name(),
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
    /// The `asynOption` interface (`get_option` / `set_option`). C drivers
    /// register it explicitly (`option.interfaceType = asynOptionType` in
    /// drvAsynIPPort.c:1079, drvAsynSerialPort.c:1098, drvVxi11.c:1777,
    /// drvAsynFTDIPort.cpp:582) and asynRecord's `optioniv` is the result of
    /// `findInterface(asynOptionType)` (asynRecord.c:1177-1186).
    Option,
    /// The `asynDrvUser` interface — see [`InterfaceType::DrvUser`]. A driver
    /// that declares it can resolve a `drvInfo` string to a parameter reason
    /// ([`crate::port::PortDriver::drv_user_create`]); a driver that does not
    /// leaves every client at reason 0, and asynRecord reports
    /// `"asynDrvUser not supported but drvInfo not blank"` for a non-empty
    /// DRVINFO (asynRecord.c:1258-1266).
    DrvUser,
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
            Self::Option => InterfaceType::Option,
            Self::DrvUser => InterfaceType::DrvUser,
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

/// C's `interfaceMask` / `interruptMask` bits (asynPortDriver.h:24-39). An
/// `asynPortDriver` registers exactly the interfaces its mask names
/// (asynPortDriver.cpp:4055-4071), which is what asynRecord's OPTIONIV, UI32IV,
/// … report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InterfaceMask(u32);

impl InterfaceMask {
    pub const NONE: Self = Self(0);
    pub const COMMON: Self = Self(0x0001);
    pub const DRV_USER: Self = Self(0x0002);
    pub const OPTION: Self = Self(0x0004);
    pub const INT32: Self = Self(0x0008);
    pub const UINT32_DIGITAL: Self = Self(0x0010);
    pub const FLOAT64: Self = Self(0x0020);
    pub const OCTET: Self = Self(0x0040);
    pub const INT8_ARRAY: Self = Self(0x0080);
    pub const INT16_ARRAY: Self = Self(0x0100);
    pub const INT32_ARRAY: Self = Self(0x0200);
    pub const FLOAT32_ARRAY: Self = Self(0x0400);
    pub const FLOAT64_ARRAY: Self = Self(0x0800);
    pub const GENERIC_POINTER: Self = Self(0x1000);
    pub const ENUM: Self = Self(0x2000);
    pub const INT64: Self = Self(0x4000);
    pub const INT64_ARRAY: Self = Self(0x8000);
    /// asynGpib, which a GPIB port registers through `asynGpib.c:606` and no
    /// asynPortDriver mask names.
    pub const GPIB: Self = Self(0x1_0000);

    /// A port not yet given its C driver's mask (asynPortDriver.cpp:4055-4071):
    /// the scalar set every port declared before masks existed. Each use is a
    /// site still to migrate.
    pub const UNMIGRATED_ALL: Self = Self::COMMON
        .or(Self::DRV_USER)
        .or(Self::OPTION)
        .or(Self::INT32)
        .or(Self::UINT32_DIGITAL)
        .or(Self::FLOAT64)
        .or(Self::OCTET)
        .or(Self::GENERIC_POINTER)
        .or(Self::ENUM)
        .or(Self::INT64);

    /// A C subclass (an areaDetector driver or plugin) whose own masks, ORed
    /// into its base class's, are not yet carried over. Each use is a site
    /// still to migrate.
    pub const UNMIGRATED_SUBCLASS: Self = Self::NONE;

    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The capabilities of the interfaces this mask names.
    pub fn capabilities(self) -> Vec<Capability> {
        use Capability::*;
        let table: [(Self, &[Capability]); 17] = [
            (Self::INT32, &[Int32Read, Int32Write]),
            (Self::INT64, &[Int64Read, Int64Write]),
            (Self::FLOAT64, &[Float64Read, Float64Write]),
            (Self::OCTET, &[OctetRead, OctetWrite]),
            (
                Self::UINT32_DIGITAL,
                &[UInt32DigitalRead, UInt32DigitalWrite],
            ),
            (Self::ENUM, &[EnumRead, EnumWrite]),
            (
                Self::GENERIC_POINTER,
                &[GenericPointerRead, GenericPointerWrite],
            ),
            (Self::INT8_ARRAY, &[Int8ArrayRead, Int8ArrayWrite]),
            (Self::INT16_ARRAY, &[Int16ArrayRead, Int16ArrayWrite]),
            (Self::INT32_ARRAY, &[Int32ArrayRead, Int32ArrayWrite]),
            (Self::INT64_ARRAY, &[Int64ArrayRead, Int64ArrayWrite]),
            (Self::FLOAT32_ARRAY, &[Float32ArrayRead, Float32ArrayWrite]),
            (Self::FLOAT64_ARRAY, &[Float64ArrayRead, Float64ArrayWrite]),
            (Self::OPTION, &[Option]),
            (Self::DRV_USER, &[DrvUser]),
            (Self::GPIB, &[Gpib]),
            (Self::COMMON, &[Flush, Connect]),
        ];
        table
            .iter()
            .filter(|(bit, _)| self.contains(*bit))
            .flat_map(|(_, caps)| caps.iter().copied())
            .collect()
    }
}

impl std::ops::BitOr for InterfaceMask {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        self.or(other)
    }
}

/// The interface set of a byte-transport port: `asynCommon` + `asynOctet` +
/// `asynOption`, exactly what drvAsynIPPort / drvAsynSerialPort /
/// drvAsynFTDIPort register (drvAsynIPPort.c:1091-1109). No register interface,
/// so a record pointed at one with IFACE=Int32 gets C's "No asynInt32 interface"
/// rather than a silent cache read. No `asynDrvUser` either — a byte transport
/// has no parameters to name, so every client stays at reason 0 and asynRecord
/// refuses a non-blank DRVINFO (asynRecord.c:1258-1266).
pub fn octet_transport_capabilities() -> Vec<Capability> {
    InterfaceMask::COMMON
        .or(InterfaceMask::OPTION)
        .or(InterfaceMask::OCTET)
        .capabilities()
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

    /// The unmigrated set is the one every port declared before masks: the
    /// scalars, no arrays, no Motor.
    #[test]
    fn unmigrated_all_has_scalars() {
        let caps = InterfaceMask::UNMIGRATED_ALL.capabilities();
        assert!(caps.contains(&Capability::Int32Read));
        assert!(caps.contains(&Capability::Float64Write));
        assert!(caps.contains(&Capability::OctetRead));
        assert!(!caps.contains(&Capability::Motor));
        assert!(!caps.contains(&Capability::Int32ArrayRead));
    }

    /// A mask names exactly its interfaces: no asynOption or asynUInt32Digital
    /// unless asked for (asynPortDriver.cpp:4060, :4069).
    #[test]
    fn a_mask_declares_only_its_interfaces() {
        let caps = (InterfaceMask::COMMON | InterfaceMask::INT32 | InterfaceMask::INT32_ARRAY)
            .capabilities();
        let types: std::collections::BTreeSet<_> = caps
            .iter()
            .map(|c| c.interface_type().asyn_name())
            .collect();
        assert_eq!(
            types.into_iter().collect::<Vec<_>>(),
            ["asynCommon", "asynInt32", "asynInt32Array"]
        );
    }

    /// R11-48: only a port that can resolve a drvInfo string declares
    /// `asynDrvUser`. C's byte transports register no such interface, which is
    /// what makes asynRecord force REASON=0 there (asynRecord.c:1258-1266).
    #[test]
    fn only_a_parameter_library_port_declares_drv_user() {
        assert!(
            InterfaceMask::UNMIGRATED_ALL
                .capabilities()
                .contains(&Capability::DrvUser)
        );
        assert!(!octet_transport_capabilities().contains(&Capability::DrvUser));
        assert_eq!(Capability::DrvUser.interface_type(), InterfaceType::DrvUser);
        assert!(!Capability::DrvUser.is_read());
        assert!(!Capability::DrvUser.is_write());
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
            InterfaceType::DrvUser,
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
