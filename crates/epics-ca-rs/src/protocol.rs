use epics_base_rs::error::{CaError, CaResult};

// CA protocol command codes
pub const CA_PROTO_VERSION: u16 = 0;
pub const CA_PROTO_EVENT_ADD: u16 = 1;
pub const CA_PROTO_EVENT_CANCEL: u16 = 2;
pub const CA_PROTO_SEARCH: u16 = 6;
pub const CA_PROTO_NOT_FOUND: u16 = 14;
pub const CA_PROTO_READ_NOTIFY: u16 = 15;
pub const CA_PROTO_CREATE_CHAN: u16 = 18;
pub const CA_PROTO_WRITE_NOTIFY: u16 = 19;
pub const CA_PROTO_HOST_NAME: u16 = 21;
pub const CA_PROTO_CLIENT_NAME: u16 = 20;
pub const CA_PROTO_ACCESS_RIGHTS: u16 = 22;
pub const CA_PROTO_ECHO: u16 = 23;
pub const CA_PROTO_REPEATER_CONFIRM: u16 = 17;
pub const CA_PROTO_REPEATER_REGISTER: u16 = 24;
pub const CA_PROTO_CLEAR_CHANNEL: u16 = 12;
pub const CA_PROTO_RSRV_IS_UP: u16 = 13;
pub const CA_PROTO_SERVER_DISCONN: u16 = 27;
pub const CA_PROTO_READ: u16 = 3; // deprecated but exists in spec
pub const CA_PROTO_WRITE: u16 = 4; // fire-and-forget write
pub const CA_PROTO_EVENTS_OFF: u16 = 8;
pub const CA_PROTO_EVENTS_ON: u16 = 9;
pub const CA_PROTO_READ_SYNC: u16 = 10; // legacy echo (used by older clients)
pub const CA_PROTO_ERROR: u16 = 11;
pub const CA_PROTO_CREATE_CH_FAIL: u16 = 26;

// Ports
pub const CA_SERVER_PORT: u16 = 5064;
pub const CA_REPEATER_PORT: u16 = 5065;

// CA protocol version
pub const CA_MINOR_VERSION: u16 = 13;

// Monitor masks
pub const DBE_VALUE: u16 = 1;
pub const DBE_LOG: u16 = 2;
pub const DBE_ALARM: u16 = 4;
pub const DBE_PROPERTY: u16 = 8;

// Reply flags
pub const CA_DO_REPLY: u16 = 10;

// ECA status codes — DEFMSG(severity, msg_no) encoding per caerr.h
pub const CA_K_SUCCESS: u32 = 1;
pub const CA_K_WARNING: u32 = 0;
pub const CA_K_ERROR: u32 = 2;

pub const fn defmsg(sev: u32, num: u32) -> u32 {
    ((num << 3) & 0x0000FFF8) | (sev & 0x00000007)
}

pub const ECA_NORMAL: u32 = defmsg(CA_K_SUCCESS, 0); // 1
pub const ECA_BADTYPE: u32 = defmsg(CA_K_ERROR, 14); // 114 (0x72)
pub const ECA_PUTFAIL: u32 = defmsg(CA_K_WARNING, 20); // 160 (0xA0)
pub const ECA_TIMEOUT: u32 = defmsg(CA_K_WARNING, 10); // 80 (0x50)
pub const ECA_NOWTACCESS: u32 = defmsg(CA_K_WARNING, 47); // 376 (0x178)
pub const ECA_BADCHID: u32 = defmsg(CA_K_ERROR, 15); // 122 (0x7A)
pub const ECA_GETFAIL: u32 = defmsg(CA_K_WARNING, 19); // 152 (0x98)
pub const ECA_BADCOUNT: u32 = defmsg(CA_K_ERROR, 21); // 170 (0xAA)
pub const ECA_INTERNAL: u32 = defmsg(CA_K_ERROR, 11); // 90 (0x5A)

/// Maximum payload size for DoS prevention (16 MB).
pub const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// Extra bytes consumed by extended header fields.
pub const EXTENDED_EXTRA: usize = 8;

/// 16-byte CA message header (big-endian), with optional extended fields.
#[derive(Debug, Clone, Copy)]
pub struct CaHeader {
    pub cmmd: u16,
    pub postsize: u16,
    pub data_type: u16,
    pub count: u16,
    pub cid: u32,
    pub available: u32,
    pub extended_postsize: Option<u32>,
    pub extended_count: Option<u32>,
}

impl CaHeader {
    pub const SIZE: usize = 16;

    pub fn new(cmmd: u16) -> Self {
        Self {
            cmmd,
            postsize: 0,
            data_type: 0,
            count: 0,
            cid: 0,
            available: 0,
            extended_postsize: None,
            extended_count: None,
        }
    }

    /// Whether this header uses extended form.
    pub fn is_extended(&self) -> bool {
        self.postsize == 0xFFFF && self.count == 0 && self.extended_postsize.is_some()
    }

    /// Actual payload size in bytes.
    pub fn actual_postsize(&self) -> usize {
        if self.postsize == 0xFFFF && self.count == 0 {
            if let Some(ext) = self.extended_postsize {
                return ext as usize;
            }
        }
        self.postsize as usize
    }

    /// Actual element count.
    pub fn actual_count(&self) -> u32 {
        if self.postsize == 0xFFFF && self.count == 0 {
            if let Some(ext) = self.extended_count {
                return ext;
            }
        }
        self.count as u32
    }

    /// Set payload size and count, automatically switching to extended form if needed.
    /// `size` is the actual data length (unpadded). Wire-level 8-byte alignment padding
    /// is handled by the caller when writing to the socket, NOT stored in the header.
    pub fn set_payload_size(&mut self, size: usize, count: u32) {
        if size > 0xFFFE || count > 0xFFFF {
            self.postsize = 0xFFFF;
            self.count = 0;
            self.extended_postsize = Some(size as u32);
            self.extended_count = Some(count);
        } else {
            self.postsize = size as u16;
            self.count = count as u16;
            self.extended_postsize = None;
            self.extended_count = None;
        }
    }

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..2].copy_from_slice(&self.cmmd.to_be_bytes());
        buf[2..4].copy_from_slice(&self.postsize.to_be_bytes());
        buf[4..6].copy_from_slice(&self.data_type.to_be_bytes());
        buf[6..8].copy_from_slice(&self.count.to_be_bytes());
        buf[8..12].copy_from_slice(&self.cid.to_be_bytes());
        buf[12..16].copy_from_slice(&self.available.to_be_bytes());
        buf
    }

    /// Serialize header, including extended fields if present.
    pub fn to_bytes_extended(&self) -> Vec<u8> {
        let mut buf = self.to_bytes().to_vec();
        if self.is_extended() {
            // SAFETY: is_extended() guarantees extended_postsize.is_some()
            buf.extend_from_slice(&self.extended_postsize.unwrap().to_be_bytes());
            // SAFETY: extended_count is always set alongside extended_postsize
            // in both set_payload_size() and from_bytes_extended()
            buf.extend_from_slice(&self.extended_count.unwrap_or(0).to_be_bytes());
        }
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> CaResult<Self> {
        if buf.len() < 16 {
            return Err(CaError::Protocol(format!(
                "header too short: {} bytes",
                buf.len()
            )));
        }
        Ok(Self {
            cmmd: u16::from_be_bytes([buf[0], buf[1]]),
            postsize: u16::from_be_bytes([buf[2], buf[3]]),
            data_type: u16::from_be_bytes([buf[4], buf[5]]),
            count: u16::from_be_bytes([buf[6], buf[7]]),
            cid: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            available: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
            extended_postsize: None,
            extended_count: None,
        })
    }

    /// Parse header with extended form support.
    /// Returns (header, total_bytes_consumed).
    pub fn from_bytes_extended(buf: &[u8]) -> CaResult<(Self, usize)> {
        if buf.len() < 16 {
            return Err(CaError::Protocol(format!(
                "header too short: {} bytes",
                buf.len()
            )));
        }
        let mut hdr = Self::from_bytes(buf)?;
        let mut consumed = 16;

        if hdr.postsize == 0xFFFF && hdr.count == 0 {
            if buf.len() < 24 {
                return Err(CaError::Protocol("extended header incomplete".into()));
            }
            let ext_post = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
            let ext_count = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
            if ext_post as usize > MAX_PAYLOAD_SIZE {
                return Err(CaError::Protocol("payload too large".into()));
            }
            hdr.extended_postsize = Some(ext_post);
            hdr.extended_count = Some(ext_count);
            consumed = 24;
        }

        Ok((hdr, consumed))
    }
}

/// Round up to 8-byte alignment
pub fn align8(size: usize) -> usize {
    (size + 7) & !7
}

/// Build a padded, null-terminated, 8-byte aligned payload from a string
pub fn pad_string(s: &str) -> Vec<u8> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0); // null terminator
    let padded_len = align8(bytes.len());
    bytes.resize(padded_len, 0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let hdr = CaHeader {
            cmmd: CA_PROTO_SEARCH,
            postsize: 16,
            data_type: 5,
            count: 13,
            cid: 42,
            available: 100,
            extended_postsize: None,
            extended_count: None,
        };
        let bytes = hdr.to_bytes();
        let hdr2 = CaHeader::from_bytes(&bytes).unwrap();
        assert_eq!(hdr.cmmd, hdr2.cmmd);
        assert_eq!(hdr.postsize, hdr2.postsize);
        assert_eq!(hdr.data_type, hdr2.data_type);
        assert_eq!(hdr.count, hdr2.count);
        assert_eq!(hdr.cid, hdr2.cid);
        assert_eq!(hdr.available, hdr2.available);
    }

    #[test]
    fn test_align8() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(7), 8);
        assert_eq!(align8(8), 8);
        assert_eq!(align8(9), 16);
    }

    #[test]
    fn test_pad_string() {
        let padded = pad_string("TEST");
        assert_eq!(padded.len(), 8); // "TEST\0" = 5 -> align8 -> 8
        assert_eq!(&padded[..4], b"TEST");
        assert_eq!(padded[4], 0);
    }

    #[test]
    fn test_extended_header_roundtrip() {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.data_type = 6; // Double
        hdr.cid = 42;
        hdr.available = 100;
        hdr.set_payload_size(100_000, 12500);
        assert!(hdr.is_extended());
        assert_eq!(hdr.actual_postsize(), 100_000);
        assert_eq!(hdr.actual_count(), 12500);

        let bytes = hdr.to_bytes_extended();
        assert_eq!(bytes.len(), 24);

        let (hdr2, consumed) = CaHeader::from_bytes_extended(&bytes).unwrap();
        assert_eq!(consumed, 24);
        assert!(hdr2.is_extended());
        assert_eq!(hdr2.actual_postsize(), 100_000);
        assert_eq!(hdr2.actual_count(), 12500);
        assert_eq!(hdr2.cmmd, CA_PROTO_READ_NOTIFY);
    }

    #[test]
    fn test_actual_postsize_normal() {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.postsize = 1024;
        hdr.count = 128;
        assert!(!hdr.is_extended());
        assert_eq!(hdr.actual_postsize(), 1024);
        assert_eq!(hdr.actual_count(), 128);
    }

    #[test]
    fn test_set_payload_size_auto() {
        // Small payload — stays normal
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.set_payload_size(1000, 100);
        assert!(!hdr.is_extended());
        assert_eq!(hdr.postsize, 1000);
        assert_eq!(hdr.count, 100);

        // Large payload — auto-extends
        hdr.set_payload_size(70_000, 8750);
        assert!(hdr.is_extended());
        assert_eq!(hdr.postsize, 0xFFFF);
        assert_eq!(hdr.count, 0);
        assert_eq!(hdr.actual_postsize(), 70_000);
        assert_eq!(hdr.actual_count(), 8750);
    }

    #[test]
    fn test_extended_count_overflow() {
        // count > 0xFFFF triggers extended even if size is small
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(100, 100_000);
        assert!(hdr.is_extended());
        assert_eq!(hdr.actual_postsize(), 100);
        assert_eq!(hdr.actual_count(), 100_000);
    }

    #[test]
    fn test_extended_payload_too_large() {
        let mut buf = vec![0u8; 24];
        // Set postsize=0xFFFF, count=0
        buf[2] = 0xFF;
        buf[3] = 0xFF;
        buf[6] = 0;
        buf[7] = 0;
        // Set extended_postsize to > MAX_PAYLOAD_SIZE
        let big: u32 = (MAX_PAYLOAD_SIZE + 1) as u32;
        buf[16..20].copy_from_slice(&big.to_be_bytes());
        buf[20..24].copy_from_slice(&1u32.to_be_bytes());

        let result = CaHeader::from_bytes_extended(&buf);
        assert!(result.is_err());
    }
}
