//! Primitive types (C `src/common/seq_prim_types.h`, shared with the
//! run-time in C).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrimType {
    Char,
    UChar,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    Int8T,
    UInt8T,
    Int16T,
    UInt16T,
    Int32T,
    UInt32T,
    Float,
    Double,
    String,
}

impl PrimType {
    /// C `prim_type_name[tag]`.
    pub fn name(self) -> &'static str {
        PRIM_TYPE_NAME[self as usize]
    }

    /// C `prim_type_tag_name[tag]`.
    pub fn tag_name(self) -> &'static str {
        PRIM_TYPE_TAG_NAME[self as usize]
    }
}

const PRIM_TYPE_NAME: [&str; 17] = [
    "char",
    "unsigned char",
    "short",
    "unsigned short",
    "int",
    "unsigned int",
    "long",
    "unsigned long",
    "epicsInt8",
    "epicsUInt8",
    "epicsInt16",
    "epicsUInt16",
    "epicsInt32",
    "epicsUInt32",
    "float",
    "double",
    "string",
];

const PRIM_TYPE_TAG_NAME: [&str; 17] = [
    "P_CHAR",
    "P_UCHAR",
    "P_SHORT",
    "P_USHORT",
    "P_INT",
    "P_UINT",
    "P_LONG",
    "P_ULONG",
    "P_INT8T",
    "P_UINT8T",
    "P_INT16T",
    "P_UINT16T",
    "P_INT32T",
    "P_UINT32T",
    "P_FLOAT",
    "P_DOUBLE",
    "P_STRING",
];
