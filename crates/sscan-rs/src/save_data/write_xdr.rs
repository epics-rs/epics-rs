//! `writeXDR.c`: the big-endian XDR encoding saveData writes MDA files in.
//!
//! Every function returns C's success flag, which saveData folds into
//! `writeFailed`, and prints C's message on failure. `write_XDR_Init`'s
//! endianness probe has no port: `to_be_bytes` is the encoding on every host.

use std::io::{self, Seek, SeekFrom, Write};

/// A file being written in XDR (C's `FILE *fd`).
pub struct Xdr<W> {
    w: W,
}

/// C `strerror(errno)`: the OS text without std's `(os error N)` suffix.
pub(super) fn strerror(e: &io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// C `strlen`: the bytes before the first NUL.
fn c_str(s: &[u8]) -> &[u8] {
    &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
}

impl<W: Write + Seek> Xdr<W> {
    pub fn new(w: W) -> Self {
        Self { w }
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.w
    }

    pub fn into_inner(self) -> W {
        self.w
    }

    /// `writeXDR_char`: one C `char` as an int. C's `char` is signed on
    /// x86, so 0x80..=0xFF sign-extend there; this follows x86.
    pub fn char(&mut self, c: u8) -> bool {
        self.int(i32::from(c as i8))
    }

    /// `writeXDR_short`.
    pub fn short(&mut self, s: i16) -> bool {
        self.epics_int32(i32::from(s))
    }

    /// `writeXDR_int`.
    pub fn int(&mut self, i: i32) -> bool {
        self.epics_int32(i)
    }

    /// `writeXDR_long`: C `long` truncated to 32 bits.
    pub fn long(&mut self, l: i64) -> bool {
        self.epics_int32(l as i32)
    }

    /// `writeXDR_epicsInt32`.
    pub fn epics_int32(&mut self, l: i32) -> bool {
        match self.w.write_all(&l.to_be_bytes()) {
            Ok(()) => true,
            Err(e) => {
                println!(
                    "writeXDR_epicsInt32: fwrite failed, status=0, errno={}, ('{}')",
                    e.raw_os_error().unwrap_or(0),
                    strerror(&e)
                );
                false
            }
        }
    }

    /// `writeXDR_float`: the IEEE bits as an int.
    pub fn float(&mut self, f: f32) -> bool {
        self.epics_int32(f.to_bits() as i32)
    }

    /// `writeXDR_double`: the high word, then the low word.
    pub fn double(&mut self, d: f64) -> bool {
        let bits = d.to_bits();
        self.epics_int32((bits >> 32) as i32) && self.epics_int32(bits as i32)
    }

    /// `writeXDR_counted_string`: the length, then, when it is not zero,
    /// `writeXDR_string` — which writes the length again.
    pub fn counted_string(&mut self, s: &[u8]) -> bool {
        let s = c_str(s);
        if !self.int(s.len() as i32) {
            return false;
        }
        s.is_empty() || self.string(s, s.len())
    }

    /// `writeXDR_string`: at most `maxsize` bytes, length first.
    pub fn string(&mut self, s: &[u8], maxsize: usize) -> bool {
        let s = c_str(s);
        let s = &s[..s.len().min(maxsize)];
        if !self.int(s.len() as i32) {
            return false;
        }
        self.opaque(s)
    }

    /// `writeXDR_opaque`: the bytes, zero-padded to a multiple of 4.
    pub fn opaque(&mut self, b: &[u8]) -> bool {
        if b.is_empty() {
            return true;
        }
        if !self.bytes(b) {
            return false;
        }
        let pad = b.len() % 4;
        pad == 0 || self.bytes(&[0; 4][..4 - pad])
    }

    /// `writeXDR_bytes`.
    pub fn bytes(&mut self, b: &[u8]) -> bool {
        if b.is_empty() {
            return true;
        }
        match self.w.write_all(b) {
            Ok(()) => true,
            Err(e) => {
                println!(
                    "writeXDR_bytes: fwrite failed, status=0, errno={}, ('{}')",
                    e.raw_os_error().unwrap_or(0),
                    strerror(&e)
                );
                false
            }
        }
    }

    /// `writeXDR_vector`: `elem` on each element, stopping at the first
    /// failure.
    pub fn vector<T: Copy>(&mut self, v: &[T], elem: fn(&mut Self, T) -> bool) -> bool {
        v.iter().all(|&x| elem(self, x))
    }

    /// `writeXDR_getpos`: `ftell`, -1 on failure.
    pub fn getpos(&mut self) -> i64 {
        match self.w.stream_position() {
            Ok(pos) => pos as i64,
            Err(e) => {
                println!(
                    "writeXDR_getpos: ftell failed, retval = -1, errno={}, ('{}')",
                    e.raw_os_error().unwrap_or(0),
                    strerror(&e)
                );
                -1
            }
        }
    }

    /// `writeXDR_setpos`: `fflush`, whose failure is only printed, then
    /// `fseek(SEEK_SET)`, which decides the result.
    pub fn setpos(&mut self, pos: i64) -> bool {
        if let Err(e) = self.w.flush() {
            println!(
                "writeXDR_setpos: fflush failed, status = -1, errno={}, ('{}')",
                e.raw_os_error().unwrap_or(0),
                strerror(&e)
            );
        }
        // A negative offset is fseek's EINVAL.
        let seek = match u64::try_from(pos) {
            Ok(pos) => self.w.seek(SeekFrom::Start(pos)),
            Err(_) => Err(io::Error::from_raw_os_error(22)),
        };
        match seek {
            Ok(_) => true,
            Err(e) => {
                println!(
                    "writeXDR_setpos: fseek failed, status = -1, errno={}, ('{}')",
                    e.raw_os_error().unwrap_or(0),
                    strerror(&e)
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The calls `tests/fixtures/save_data/write_xdr.c` makes, in its order,
    /// give the bytes C's `writeXDR.c` wrote.
    #[test]
    fn bytes_match_c_write_xdr() {
        let mut x = Xdr::new(Cursor::new(Vec::new()));
        assert!(x.char(b'A'));
        assert!(x.char(0xFF));
        assert!(x.short(-2));
        assert!(x.int(0x0102_0304));
        assert!(x.long(0x1122_3344_5566_7788));
        assert!(x.float(1.5));
        assert!(x.double(-2.25));
        assert!(x.counted_string(b""));
        assert!(x.counted_string(b"abc"));
        assert!(x.counted_string(b"four"));
        assert!(x.counted_string(b"fives"));
        assert!(x.string(b"fives", 2));
        assert!(x.vector(&[0.5f32, -1.0, 3.25], Xdr::float));
        assert!(x.vector(&[1e-300f64, 6.02e23], Xdr::double));
        let pos = x.getpos();
        assert_eq!(pos, 112);
        assert!(x.setpos(pos + 8));
        assert!(x.int(0x0102_0304));
        assert!(x.setpos(4));
        assert!(x.short(-2));
        assert_eq!(
            x.into_inner().into_inner(),
            include_bytes!("../../tests/fixtures/save_data/write_xdr.bin")
        );
    }

    /// C's `strlen` stops a string at its NUL.
    #[test]
    fn string_stops_at_nul() {
        let mut x = Xdr::new(Cursor::new(Vec::new()));
        assert!(x.counted_string(b"ab\0cd"));
        assert_eq!(
            x.into_inner().into_inner(),
            [0, 0, 0, 2, 0, 0, 0, 2, b'a', b'b', 0, 0]
        );
    }

    /// `fseek` to a negative offset fails.
    #[test]
    fn setpos_negative_fails() {
        let mut x = Xdr::new(Cursor::new(Vec::new()));
        assert!(!x.setpos(-1));
    }
}
