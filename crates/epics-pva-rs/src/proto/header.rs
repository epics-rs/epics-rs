//! 8-byte PVA frame header.
//!
//! Layout (pvxs `pvaproto.h::pva_version`, `pva_flags`):
//!
//! ```text
//! offset  size  field
//!   0      1    magic = 0xCA
//!   1      1    version (currently PVA_VERSION = 2)
//!   2      1    flags
//!   3      1    command (or control-command id when bit 0 of flags is set)
//!   4      4    payload length (u32, byte order = flags bit 7)
//! ```
//!
//! Flags layout (`flags` byte):
//! - bit 0 (`0x01`) — control message (no payload-length-prefixed body)
//! - bit 4 (`0x10`) — segment first
//! - bit 5 (`0x20`) — segment last
//! - bit 6 (`0x40`) — server→client direction
//! - bit 7 (`0x80`) — big-endian wire byte order

use std::io::Cursor;

use super::buffer::{ByteOrder, DecodeError, ReadExt, WriteExt};
use crate::decode_err;

/// PVA magic byte. Always `0xCA`.
pub const MAGIC: u8 = 0xCA;

/// PVA wire-protocol version negotiated by this implementation.
///
/// pvxs uses 2 today; bumped to 3 in upcoming releases for additional features
/// — keep at 2 for v1 parity.
pub const PVA_VERSION: u8 = 2;

/// Bitfield representation of the header `flags` byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeaderFlags(pub u8);

impl HeaderFlags {
    pub const CONTROL: u8 = 0x01;
    pub const SEGMENT_FIRST: u8 = 0x10;
    pub const SEGMENT_LAST: u8 = 0x20;
    pub const SERVER: u8 = 0x40;
    pub const BIG_ENDIAN: u8 = 0x80;
    pub const SEGMENT_MASK: u8 = 0x30;

    pub fn new(server: bool, control: bool, order: ByteOrder) -> Self {
        let mut b = 0u8;
        if control {
            b |= Self::CONTROL;
        }
        if server {
            b |= Self::SERVER;
        }
        if matches!(order, ByteOrder::Big) {
            b |= Self::BIG_ENDIAN;
        }
        HeaderFlags(b)
    }

    pub fn is_control(self) -> bool {
        self.0 & Self::CONTROL != 0
    }
    pub fn is_server(self) -> bool {
        self.0 & Self::SERVER != 0
    }
    pub fn byte_order(self) -> ByteOrder {
        ByteOrder::from_header_flag(self.0)
    }
    pub fn segment_first(self) -> bool {
        self.0 & Self::SEGMENT_FIRST != 0
    }
    pub fn segment_last(self) -> bool {
        self.0 & Self::SEGMENT_LAST != 0
    }
    /// True iff this is a single un-segmented application message.
    pub fn unsegmented(self) -> bool {
        self.0 & Self::SEGMENT_MASK == 0
    }
}

/// Parsed PVA header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PvaHeader {
    pub version: u8,
    pub flags: HeaderFlags,
    pub command: u8,
    pub payload_length: u32,
}

impl PvaHeader {
    pub const SIZE: usize = 8;

    /// Build a non-control, non-segmented header.
    pub fn application(server: bool, order: ByteOrder, command: u8, payload_length: u32) -> Self {
        Self {
            version: PVA_VERSION,
            flags: HeaderFlags::new(server, false, order),
            command,
            payload_length,
        }
    }

    /// Build a control header. The `payload_length` field carries the
    /// control-message data word per pvxs `pvaproto.h`.
    pub fn control(server: bool, order: ByteOrder, command: u8, data: u32) -> Self {
        Self {
            version: PVA_VERSION,
            flags: HeaderFlags::new(server, true, order),
            command,
            payload_length: data,
        }
    }

    /// Encode into 8 bytes.
    pub fn encode(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = MAGIC;
        out[1] = self.version;
        out[2] = self.flags.0;
        out[3] = self.command;
        let len_bytes = match self.flags.byte_order() {
            ByteOrder::Big => self.payload_length.to_be_bytes(),
            ByteOrder::Little => self.payload_length.to_le_bytes(),
        };
        out[4..8].copy_from_slice(&len_bytes);
        out
    }

    /// Append the encoded form to `buf`.
    pub fn write_into(&self, buf: &mut Vec<u8>) {
        buf.put_u8(MAGIC);
        buf.put_u8(self.version);
        buf.put_u8(self.flags.0);
        buf.put_u8(self.command);
        buf.put_u32(self.payload_length, self.flags.byte_order());
    }

    /// Decode an 8-byte header. Validates magic.
    pub fn decode(cur: &mut Cursor<&[u8]>) -> Result<Self, DecodeError> {
        let magic = cur.get_u8()?;
        if magic != MAGIC {
            return Err(decode_err!("bad magic 0x{magic:02X}, expected 0xCA"));
        }
        let version = cur.get_u8()?;
        let flags = HeaderFlags(cur.get_u8()?);
        let command = cur.get_u8()?;
        let payload_length = cur.get_u32(flags.byte_order())?;
        Ok(Self {
            version,
            flags,
            command,
            payload_length,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_application_le() {
        let h = PvaHeader::application(false, ByteOrder::Little, 7, 42);
        let bytes = h.encode();
        // magic, version, flags, cmd, len_le=42
        assert_eq!(bytes, [MAGIC, PVA_VERSION, 0x00, 7, 0x2A, 0x00, 0x00, 0x00]);
        let mut cur = Cursor::new(bytes.as_slice());
        let decoded = PvaHeader::decode(&mut cur).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn round_trip_application_be_server() {
        let h = PvaHeader::application(true, ByteOrder::Big, 4, 0x100);
        let bytes = h.encode();
        // flags = 0x40 (server) | 0x80 (BE) = 0xC0
        assert_eq!(bytes, [MAGIC, PVA_VERSION, 0xC0, 4, 0x00, 0x00, 0x01, 0x00]);
        let mut cur = Cursor::new(bytes.as_slice());
        assert_eq!(PvaHeader::decode(&mut cur).unwrap(), h);
    }

    #[test]
    fn control_message() {
        let h = PvaHeader::control(true, ByteOrder::Little, 2, 0xDEADBEEF);
        let bytes = h.encode();
        // flags = 0x01 (control) | 0x40 (server) = 0x41
        assert_eq!(bytes[2], 0x41);
        assert_eq!(bytes[3], 2);
        assert_eq!(&bytes[4..], &[0xEF, 0xBE, 0xAD, 0xDE]);
        let mut cur = Cursor::new(bytes.as_slice());
        let decoded = PvaHeader::decode(&mut cur).unwrap();
        assert!(decoded.flags.is_control());
        assert_eq!(decoded.payload_length, 0xDEADBEEF);
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = [0xAB, PVA_VERSION, 0, 0, 0, 0, 0, 0];
        let mut cur = Cursor::new(bytes.as_slice());
        assert!(PvaHeader::decode(&mut cur).is_err());
    }

    #[test]
    fn flags_helpers() {
        let f = HeaderFlags(0x80 | 0x40);
        assert!(f.is_server());
        assert!(matches!(f.byte_order(), ByteOrder::Big));
        assert!(!f.is_control());
        assert!(f.unsegmented());
    }
}
