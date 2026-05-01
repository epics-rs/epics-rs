//! Endian-aware buffer helpers.
//!
//! PVA negotiates byte order at the connection level (server sends a
//! `SET_BYTE_ORDER` control message during handshake); thereafter both peers
//! use the same order until disconnect. We carry that order as a runtime
//! [`ByteOrder`] flag rather than relying on `to_le`/`to_be` at every call site.

use std::io::{Cursor, Read};

/// Byte order for the current PVA connection (negotiated at handshake).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Little-endian (default for x86 hosts, set by `SET_BYTE_ORDER` flag bit 7 = 0).
    Little,
    /// Big-endian (set by `SET_BYTE_ORDER` flag bit 7 = 1).
    Big,
}

impl ByteOrder {
    pub fn is_big(self) -> bool {
        matches!(self, ByteOrder::Big)
    }

    /// Header `flags` byte representation (bit 7).
    pub fn header_flag(self) -> u8 {
        if self.is_big() { 0x80 } else { 0x00 }
    }

    pub fn from_header_flag(flag_byte: u8) -> Self {
        if flag_byte & 0x80 != 0 {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        }
    }
}

// ─── Write side: extend Vec<u8> with endian-aware appenders ──────────────

/// Append-only writer trait. Implemented for `Vec<u8>`.
pub trait WriteExt {
    fn put_u8(&mut self, v: u8);
    fn put_i8(&mut self, v: i8);
    fn put_u16(&mut self, v: u16, order: ByteOrder);
    fn put_i16(&mut self, v: i16, order: ByteOrder);
    fn put_u32(&mut self, v: u32, order: ByteOrder);
    fn put_i32(&mut self, v: i32, order: ByteOrder);
    fn put_u64(&mut self, v: u64, order: ByteOrder);
    fn put_i64(&mut self, v: i64, order: ByteOrder);
    fn put_f32(&mut self, v: f32, order: ByteOrder);
    fn put_f64(&mut self, v: f64, order: ByteOrder);
    fn put_bytes(&mut self, v: &[u8]);
}

impl WriteExt for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }
    fn put_i8(&mut self, v: i8) {
        self.push(v as u8);
    }
    fn put_u16(&mut self, v: u16, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_i16(&mut self, v: i16, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_u32(&mut self, v: u32, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_i32(&mut self, v: i32, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_u64(&mut self, v: u64, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_i64(&mut self, v: i64, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_f32(&mut self, v: f32, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_f64(&mut self, v: f64, order: ByteOrder) {
        self.extend_from_slice(&match order {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        });
    }
    fn put_bytes(&mut self, v: &[u8]) {
        self.extend_from_slice(v);
    }
}

// ─── Read side: cursor-style decoder ──────────────────────────────────────

/// Decode error returned when the cursor runs out of bytes or sees an invalid
/// encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Construct a [`DecodeError`] with the call-site source file and line embedded.
#[macro_export]
#[doc(hidden)]
macro_rules! decode_err {
    ($($arg:tt)*) => {
        $crate::proto::buffer::DecodeError(
            format!("[{}:{}] {}", file!(), line!(), format_args!($($arg)*))
        )
    };
}

/// Endian-aware reader over a byte cursor. Adds methods used throughout the
/// PVA codec on top of [`Cursor<&[u8]>`].
pub trait ReadExt {
    fn get_u8(&mut self) -> Result<u8, DecodeError>;
    fn get_i8(&mut self) -> Result<i8, DecodeError>;
    fn get_u16(&mut self, order: ByteOrder) -> Result<u16, DecodeError>;
    fn get_i16(&mut self, order: ByteOrder) -> Result<i16, DecodeError>;
    fn get_u32(&mut self, order: ByteOrder) -> Result<u32, DecodeError>;
    fn get_i32(&mut self, order: ByteOrder) -> Result<i32, DecodeError>;
    fn get_u64(&mut self, order: ByteOrder) -> Result<u64, DecodeError>;
    fn get_i64(&mut self, order: ByteOrder) -> Result<i64, DecodeError>;
    fn get_f32(&mut self, order: ByteOrder) -> Result<f32, DecodeError>;
    fn get_f64(&mut self, order: ByteOrder) -> Result<f64, DecodeError>;
    fn get_bytes(&mut self, n: usize) -> Result<Vec<u8>, DecodeError>;
    fn remaining(&self) -> usize;
}

impl ReadExt for Cursor<&[u8]> {
    fn get_u8(&mut self) -> Result<u8, DecodeError> {
        let mut buf = [0u8; 1];
        self.read_exact(&mut buf)
            .map_err(|_| decode_err!("short read u8"))?;
        Ok(buf[0])
    }
    fn get_i8(&mut self) -> Result<i8, DecodeError> {
        Ok(self.get_u8()? as i8)
    }
    fn get_u16(&mut self, order: ByteOrder) -> Result<u16, DecodeError> {
        let mut buf = [0u8; 2];
        self.read_exact(&mut buf)
            .map_err(|_| decode_err!("short read u16"))?;
        Ok(match order {
            ByteOrder::Big => u16::from_be_bytes(buf),
            ByteOrder::Little => u16::from_le_bytes(buf),
        })
    }
    fn get_i16(&mut self, order: ByteOrder) -> Result<i16, DecodeError> {
        Ok(self.get_u16(order)? as i16)
    }
    fn get_u32(&mut self, order: ByteOrder) -> Result<u32, DecodeError> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)
            .map_err(|_| decode_err!("short read u32"))?;
        Ok(match order {
            ByteOrder::Big => u32::from_be_bytes(buf),
            ByteOrder::Little => u32::from_le_bytes(buf),
        })
    }
    fn get_i32(&mut self, order: ByteOrder) -> Result<i32, DecodeError> {
        Ok(self.get_u32(order)? as i32)
    }
    fn get_u64(&mut self, order: ByteOrder) -> Result<u64, DecodeError> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)
            .map_err(|_| decode_err!("short read u64"))?;
        Ok(match order {
            ByteOrder::Big => u64::from_be_bytes(buf),
            ByteOrder::Little => u64::from_le_bytes(buf),
        })
    }
    fn get_i64(&mut self, order: ByteOrder) -> Result<i64, DecodeError> {
        Ok(self.get_u64(order)? as i64)
    }
    fn get_f32(&mut self, order: ByteOrder) -> Result<f32, DecodeError> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)
            .map_err(|_| decode_err!("short read f32"))?;
        Ok(match order {
            ByteOrder::Big => f32::from_be_bytes(buf),
            ByteOrder::Little => f32::from_le_bytes(buf),
        })
    }
    fn get_f64(&mut self, order: ByteOrder) -> Result<f64, DecodeError> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)
            .map_err(|_| decode_err!("short read f64"))?;
        Ok(match order {
            ByteOrder::Big => f64::from_be_bytes(buf),
            ByteOrder::Little => f64::from_le_bytes(buf),
        })
    }
    fn get_bytes(&mut self, n: usize) -> Result<Vec<u8>, DecodeError> {
        let mut buf = vec![0u8; n];
        self.read_exact(&mut buf)
            .map_err(|_| decode_err!("short read {n} bytes"))?;
        Ok(buf)
    }
    fn remaining(&self) -> usize {
        let total = self.get_ref().len() as u64;
        let pos = self.position();
        total.saturating_sub(pos) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_u32_le_be() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            buf.put_u32(0xDEADBEEF, order);
            let mut cur = Cursor::new(buf.as_slice());
            assert_eq!(cur.get_u32(order).unwrap(), 0xDEADBEEF);
        }
    }

    #[test]
    fn round_trip_floats() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            buf.put_f64(std::f64::consts::PI, order);
            let mut cur = Cursor::new(buf.as_slice());
            let v = cur.get_f64(order).unwrap();
            assert!((v - std::f64::consts::PI).abs() < 1e-12);
        }
    }

    #[test]
    fn header_flag_round_trip() {
        assert_eq!(ByteOrder::Big.header_flag(), 0x80);
        assert_eq!(ByteOrder::Little.header_flag(), 0x00);
        assert_eq!(ByteOrder::from_header_flag(0x80), ByteOrder::Big);
        assert_eq!(ByteOrder::from_header_flag(0x00), ByteOrder::Little);
        assert_eq!(ByteOrder::from_header_flag(0xFF), ByteOrder::Big);
    }
}
