//! The value types of the snc→seq contract: `pvAlarm.h`, `pvType.h`,
//! `seq_prim_types.h`, `seq_mask.h` and the scalar definitions of `seqCom.h`.

/// `MAX_STRING_SIZE`: the bytes of an SNL `string`, terminating NUL included.
pub const MAX_STRING_SIZE: usize = 40;

/// `DEFAULT_TIMEOUT` (`seqCom.h:44`), in seconds as `seq_pvGetTmo` takes it.
pub const DEFAULT_TIMEOUT: f64 = 10.0;

/// `DEFAULT_QUEUE_SIZE` (`seqCom.h`): the backward-compatibility constant.
pub const DEFAULT_QUEUE_SIZE: u32 = 100;

/// `pvStat` (`pvAlarm.h`). Values past `Disconn` are the EPICS alarm
/// statuses, so `pvStatus(x) == pvStatHIHI` compares as in C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PvStat {
    Ok = 0,
    Error = -1,
    Disconn = -2,
    Read = 1,
    Write = 2,
    Hihi = 3,
    High = 4,
    Lolo = 5,
    Low = 6,
    State = 7,
    Cos = 8,
    Comm = 9,
    Timeout = 10,
    HwLimit = 11,
    Calc = 12,
    Scan = 13,
    Link = 14,
    Soft = 15,
    BadSub = 16,
    Udf = 17,
    Disable = 18,
    Simm = 19,
    ReadAccess = 20,
    WriteAccess = 21,
}

/// `pvSevr` (`pvAlarm.h`). C's `pvSevrOK` and `pvSevrNONE` are one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PvSevr {
    None = 0,
    Error = -1,
    Minor = 1,
    Major = 2,
    Invalid = 3,
}

impl PvSevr {
    pub const OK: PvSevr = PvSevr::None;
}

/// `pvType` (`pvType.h`): the request type the pv layer asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PvType {
    Error = -1,
    Char = 0,
    Short = 1,
    Long = 2,
    Float = 3,
    Double = 4,
    String = 5,
    TimeChar = 6,
    TimeShort = 7,
    TimeLong = 8,
    TimeFloat = 9,
    TimeDouble = 10,
    TimeString = 11,
}

impl PvType {
    /// `pv_is_simple_type`.
    pub fn is_simple(self) -> bool {
        (PvType::Char as i32..=PvType::String as i32).contains(&(self as i32))
    }

    /// `pv_is_time_type`.
    pub fn is_time(self) -> bool {
        (PvType::TimeChar as i32..=PvType::TimeString as i32).contains(&(self as i32))
    }

    /// `pv_is_valid_type`.
    pub fn is_valid(self) -> bool {
        self.is_simple() || self.is_time()
    }
}

/// `enum prim_type_tag` (`seq_prim_types.h`): the base type of an assigned
/// variable, as snc records it in the channel table.
///
/// The Rust type each tag stores ([`crate::var::Prim`]) follows the host's C
/// for `char` (`core::ffi::c_char`), and is 64-bit for `long` on every
/// target, as stream-rs decided for StreamDevice: C's LLP64 truncation is not
/// copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// `prim_type_name`: the C spelling, which `seqChanShow` prints.
    pub fn c_name(self) -> &'static str {
        match self {
            PrimType::Char => "char",
            PrimType::UChar => "unsigned char",
            PrimType::Short => "short",
            PrimType::UShort => "unsigned short",
            PrimType::Int => "int",
            PrimType::UInt => "unsigned int",
            PrimType::Long => "long",
            PrimType::ULong => "unsigned long",
            PrimType::Int8T => "epicsInt8",
            PrimType::UInt8T => "epicsUInt8",
            PrimType::Int16T => "epicsInt16",
            PrimType::UInt16T => "epicsUInt16",
            PrimType::Int32T => "epicsInt32",
            PrimType::UInt32T => "epicsUInt32",
            PrimType::Float => "float",
            PrimType::Double => "double",
            PrimType::String => "string",
        }
    }

    /// The `putType` of `seq_main.c`'s `pv_type_map`.
    pub fn put_type(self) -> PvType {
        match self {
            PrimType::Char | PrimType::UChar | PrimType::Int8T | PrimType::UInt8T => PvType::Char,
            PrimType::Short | PrimType::UShort | PrimType::Int16T | PrimType::UInt16T => {
                PvType::Short
            }
            PrimType::Int
            | PrimType::UInt
            | PrimType::Long
            | PrimType::ULong
            | PrimType::Int32T
            | PrimType::UInt32T => PvType::Long,
            PrimType::Float => PvType::Float,
            PrimType::Double => PvType::Double,
            PrimType::String => PvType::String,
        }
    }

    /// The `getType` of `seq_main.c`'s `pv_type_map`: the time variant of
    /// [`Self::put_type`].
    pub fn get_type(self) -> PvType {
        match self.put_type() {
            PvType::Char => PvType::TimeChar,
            PvType::Short => PvType::TimeShort,
            PvType::Long => PvType::TimeLong,
            PvType::Float => PvType::TimeFloat,
            PvType::Double => PvType::TimeDouble,
            _ => PvType::TimeString,
        }
    }
}

/// `seqMask` (`seq_mask.h`): event masks and option words.
pub type SeqMask = u32;

/// `NBITS`.
pub const NBITS: u32 = SeqMask::BITS;

/// `NWORDS(maxBitNum)`.
pub const fn nwords(max_bit_num: u32) -> usize {
    1 + (max_bit_num / NBITS) as usize
}

/// `bitSet`.
pub fn bit_set(words: &mut [SeqMask], bitnum: u32) {
    words[(bitnum / NBITS) as usize] |= 1 << (bitnum % NBITS);
}

/// `bitClear`.
pub fn bit_clear(words: &mut [SeqMask], bitnum: u32) {
    words[(bitnum / NBITS) as usize] &= !(1 << (bitnum % NBITS));
}

/// `bitTest`.
pub fn bit_test(words: &[SeqMask], bitnum: u32) -> bool {
    words[(bitnum / NBITS) as usize] & (1 << (bitnum % NBITS)) != 0
}

/// `enum compType`: the completion argument of `pvGet` / `pvPut`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompType {
    Default,
    Async,
    Sync,
}

/// `CH_ID`: a channel's index in the program's channel table. snc emits a
/// subscripted multi-PV array element as the array's first channel plus the
/// subscript ([`ChId::offset`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChId(pub u32);

impl ChId {
    /// `chId + (CH_ID)(subscript)` (`gen_ss_code.c` `gen_pv_arg`).
    pub fn offset(self, subscript: i64) -> ChId {
        ChId((i64::from(self.0) + subscript) as u32)
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// `EF_ID`: an event flag, numbered from 1; 0 is [`NOEVFLAG`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EfId(pub u32);

/// `NOEVFLAG`: `pvSync`'s argument that removes the sync.
pub const NOEVFLAG: EfId = EfId(0);

/// An SNL `string` (`typedef char string[MAX_STRING_SIZE]`): NUL-terminated
/// bytes, not UTF-8. A copy longer than the buffer stops one byte short of
/// it, where C's `strcpy` would write past the variable.
///
/// Transparent, so its address is its first `char`'s, as a pointer into it
/// counts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SeqString([u8; MAX_STRING_SIZE]);

impl SeqString {
    pub const fn new() -> Self {
        SeqString([0; MAX_STRING_SIZE])
    }

    /// The bytes before the first NUL.
    pub fn as_bytes(&self) -> &[u8] {
        let len = self
            .0
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(MAX_STRING_SIZE);
        &self.0[..len]
    }

    /// The whole buffer, as `pvPut` sends it.
    pub fn as_buffer(&self) -> &[u8; MAX_STRING_SIZE] {
        &self.0
    }

    pub fn as_buffer_mut(&mut self) -> &mut [u8; MAX_STRING_SIZE] {
        &mut self.0
    }

    /// Replace the contents by `bytes` up to their first NUL, truncated to
    /// leave room for the terminator; the rest of the buffer is zeroed.
    pub fn set(&mut self, bytes: &[u8]) {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let len = end.min(MAX_STRING_SIZE - 1);
        self.0 = [0; MAX_STRING_SIZE];
        self.0[..len].copy_from_slice(&bytes[..len]);
    }
}

impl Default for SeqString {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SeqString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self.as_bytes()))
    }
}

/// A C `char *` an SNL program holds: what `macValueGet`, `pvName` and
/// `pvMessage` return, and the type of a `char *` program variable. It
/// points into a variable as C's does (`p = stringIn` sees later writes to
/// `stringIn`), or into a read-only string ([`crate::Ptr::from_bytes`]).
pub type CharPtr = crate::var::Ptr<std::ffi::c_char>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_longer_than_the_buffer_keeps_its_terminator() {
        let mut s = SeqString::new();
        s.set(&[b'x'; 64]);
        assert_eq!(s.as_bytes().len(), MAX_STRING_SIZE - 1);
        assert_eq!(s.as_buffer()[MAX_STRING_SIZE - 1], 0);
    }

    #[test]
    fn a_shorter_copy_clears_what_the_longer_one_left() {
        let mut s = SeqString::new();
        s.set(b"maybeStandby");
        s.set(b"idle");
        assert_eq!(s.as_bytes(), b"idle");
        assert!(s.as_buffer()[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_copy_stops_at_the_source_terminator() {
        let mut s = SeqString::new();
        s.set(b"ab\0cd");
        assert_eq!(s.as_bytes(), b"ab");
    }

    /// `seq_main.c`'s `pv_type_map`.
    #[test]
    fn every_prim_type_requests_the_pv_type_c_maps_it_to() {
        use PrimType::*;
        for (p, put) in [
            (Char, PvType::Char),
            (UChar, PvType::Char),
            (Short, PvType::Short),
            (UShort, PvType::Short),
            (Int, PvType::Long),
            (UInt, PvType::Long),
            (Long, PvType::Long),
            (ULong, PvType::Long),
            (Int8T, PvType::Char),
            (UInt8T, PvType::Char),
            (Int16T, PvType::Short),
            (UInt16T, PvType::Short),
            (Int32T, PvType::Long),
            (UInt32T, PvType::Long),
            (Float, PvType::Float),
            (Double, PvType::Double),
            (String, PvType::String),
        ] {
            assert_eq!(p.put_type(), put, "{p:?}");
            assert_eq!(p.get_type() as i32, put as i32 + 6, "{p:?}");
        }
    }

    #[test]
    fn masks_word_and_bit_as_seq_mask_h() {
        assert_eq!(nwords(31), 1);
        assert_eq!(nwords(32), 2);
        let mut m = [0; 2];
        bit_set(&mut m, 33);
        assert_eq!(m, [0, 2]);
        assert!(bit_test(&m, 33));
        bit_clear(&mut m, 33);
        assert_eq!(m, [0, 0]);
    }
}
