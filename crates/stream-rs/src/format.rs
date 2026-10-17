//! `StreamFormat.h`: a parsed `%` conversion, and how the parser stores it
//! in compiled code.

use crate::cstr::CPtr;

/// `StreamFormatFlag`
pub const LEFT_FLAG: u16 = 0x01;
pub const SIGN_FLAG: u16 = 0x02;
pub const SPACE_FLAG: u16 = 0x04;
pub const ALT_FLAG: u16 = 0x08;
pub const ZERO_FLAG: u16 = 0x10;
pub const SKIP_FLAG: u16 = 0x20;
pub const DEFAULT_FLAG: u16 = 0x40;
pub const COMPARE_FLAG: u16 = 0x80;
pub const FIX_WIDTH_FLAG: u16 = 0x100;

/// `StreamFormatType`; a converter's `parse` returns one of these as `int`,
/// 0 meaning failure.
pub const UNSIGNED_FORMAT: i32 = 1;
pub const SIGNED_FORMAT: i32 = 2;
pub const ENUM_FORMAT: i32 = 3;
pub const DOUBLE_FORMAT: i32 = 4;
pub const STRING_FORMAT: i32 = 5;
pub const PSEUDO_FORMAT: i32 = 6;
pub const NEEDS_ORIGINAL_FORMAT: i32 = 7;

/// `StreamFormatTypeStr`, indexed by the type (0 is "none").
pub const STREAM_FORMAT_TYPE_STR: [&str; 7] = [
    "none", "unsigned", "signed", "enum", "double", "string", "pseudo",
];

/// `struct StreamFormat`.
///
/// C copies the struct bytewise into the compiled code and points `info` at
/// the info string that follows it. Here the struct is stored with
/// [`StreamFormat::encode`] in a fixed [`StreamFormat::SIZE`], and `info` is
/// the `infolen` bytes after it (its 0x00 terminator included), which
/// [`StreamFormat::extract`] returns alongside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamFormat {
    pub conv: u8,
    /// `StreamFormatType`, as returned by the converter's `parse`.
    pub type_: i32,
    pub flags: u16,
    pub prec: i64,
    pub width: u64,
    pub infolen: u64,
}

impl StreamFormat {
    /// Bytes the encoded struct takes in compiled code.
    pub const SIZE: usize = 1 + 4 + 2 + 8 + 8 + 8;

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0] = self.conv;
        b[1..5].copy_from_slice(&self.type_.to_ne_bytes());
        b[5..7].copy_from_slice(&self.flags.to_ne_bytes());
        b[7..15].copy_from_slice(&self.prec.to_ne_bytes());
        b[15..23].copy_from_slice(&self.width.to_ne_bytes());
        b[23..31].copy_from_slice(&self.infolen.to_ne_bytes());
        b
    }

    pub fn decode(b: &[u8; Self::SIZE]) -> Self {
        Self {
            conv: b[0],
            type_: i32::from_ne_bytes(b[1..5].try_into().unwrap()),
            flags: u16::from_ne_bytes(b[5..7].try_into().unwrap()),
            prec: i64::from_ne_bytes(b[7..15].try_into().unwrap()),
            width: u64::from_ne_bytes(b[15..23].try_into().unwrap()),
            infolen: u64::from_ne_bytes(b[23..31].try_into().unwrap()),
        }
    }

    /// `extract<StreamFormat>(s)` and the info after it: steps `s` past both.
    pub fn extract<'a>(s: &mut CPtr<'a>) -> (Self, &'a [u8]) {
        let mut b = [0u8; Self::SIZE];
        for (i, x) in b.iter_mut().enumerate() {
            *x = s.at(i);
        }
        s.advance(Self::SIZE);
        let f = Self::decode(&b);
        let start = s.offset().min(s.base().len());
        let end = (s.offset() + f.infolen as usize).min(s.base().len());
        s.advance(f.infolen as usize);
        (f, &s.base()[start..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_format_round_trips_with_its_info() {
        let f = StreamFormat {
            conv: b'd',
            type_: SIGNED_FORMAT,
            flags: LEFT_FLAG | SKIP_FLAG,
            prec: -1,
            width: 12,
            infolen: 3,
        };
        let mut code = f.encode().to_vec();
        code.extend_from_slice(b"ab\0rest");
        let mut p = CPtr::new(&code);
        let (g, info) = StreamFormat::extract(&mut p);
        assert_eq!(g, f);
        assert_eq!(info, b"ab\0");
        assert_eq!(p.cstr(), b"rest");
    }
}
