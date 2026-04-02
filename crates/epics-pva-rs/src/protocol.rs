use crate::error::{PvaError, PvaResult};

// PVA protocol constants
pub const PVA_MAGIC: u8 = 0xCA;
pub const PVA_VERSION: u8 = 2;
pub const PVA_SERVER_PORT: u16 = 5075;
pub const PVA_BROADCAST_PORT: u16 = 5076;

// Header flags
pub const FLAG_CONTROL: u8 = 0x01;
pub const FLAG_FIRST_SEG: u8 = 0x10;
pub const FLAG_LAST_SEG: u8 = 0x20;
pub const FLAG_FROM_SERVER: u8 = 0x40;
pub const FLAG_BIG_ENDIAN: u8 = 0x80;

// Application message flags (non-segmented)
pub const FLAGS_APP_NONSEG_LE: u8 = FLAG_FIRST_SEG | FLAG_LAST_SEG;
pub const FLAGS_APP_NONSEG_BE: u8 = FLAGS_APP_NONSEG_LE | FLAG_BIG_ENDIAN;

// Control commands
pub const CMD_SET_BYTE_ORDER: u8 = 2;

// Application commands
pub const CMD_BEACON: u8 = 0;
pub const CMD_CONNECTION_VALIDATION: u8 = 1;
pub const CMD_ECHO: u8 = 2;
pub const CMD_SEARCH: u8 = 3;
pub const CMD_SEARCH_RESPONSE: u8 = 4;
pub const CMD_AUTHNZ: u8 = 5;
pub const CMD_CREATE_CHANNEL: u8 = 7;
pub const CMD_DESTROY_CHANNEL: u8 = 8;
pub const CMD_CONNECTION_VALIDATED: u8 = 9;
pub const CMD_GET: u8 = 10;
pub const CMD_PUT: u8 = 11;
pub const CMD_MONITOR: u8 = 13;
pub const CMD_DESTROY_REQUEST: u8 = 15;
pub const CMD_GET_FIELD: u8 = 17;

// QoS / subcommand flags
pub const QOS_INIT: u8 = 0x08;
pub const QOS_DESTROY: u8 = 0x10;
pub const QOS_GET: u8 = 0x40;
pub const QOS_PROCESS: u8 = 0x04;

/// 8-byte PVA message header
#[derive(Debug, Clone, Copy)]
pub struct PvaHeader {
    pub magic: u8,
    pub version: u8,
    pub flags: u8,
    pub command: u8,
    pub payload_size: u32,
}

impl PvaHeader {
    pub const SIZE: usize = 8;

    pub fn new(command: u8, flags: u8) -> Self {
        Self {
            magic: PVA_MAGIC,
            version: PVA_VERSION,
            flags,
            command,
            payload_size: 0,
        }
    }

    pub fn is_big_endian(&self) -> bool {
        self.flags & FLAG_BIG_ENDIAN != 0
    }

    pub fn is_control(&self) -> bool {
        self.flags & FLAG_CONTROL != 0
    }

    pub fn to_bytes(&self, big_endian: bool) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0] = self.magic;
        buf[1] = self.version;
        buf[2] = self.flags;
        buf[3] = self.command;
        if big_endian {
            buf[4..8].copy_from_slice(&self.payload_size.to_be_bytes());
        } else {
            buf[4..8].copy_from_slice(&self.payload_size.to_le_bytes());
        }
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> PvaResult<Self> {
        if buf.len() < 8 {
            return Err(PvaError::Protocol(format!(
                "header too short: {} bytes",
                buf.len()
            )));
        }
        if buf[0] != PVA_MAGIC {
            return Err(PvaError::Protocol(format!("bad magic: {:#04x}", buf[0])));
        }
        let flags = buf[2];
        let big_endian = flags & FLAG_BIG_ENDIAN != 0;
        let payload_size = if big_endian {
            u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]])
        } else {
            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]])
        };
        Ok(Self {
            magic: buf[0],
            version: buf[1],
            flags,
            command: buf[3],
            payload_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip_le() {
        let hdr = PvaHeader {
            magic: PVA_MAGIC,
            version: PVA_VERSION,
            flags: FLAGS_APP_NONSEG_LE,
            command: CMD_SEARCH,
            payload_size: 1234,
        };
        let bytes = hdr.to_bytes(false);
        let hdr2 = PvaHeader::from_bytes(&bytes).unwrap();
        assert_eq!(hdr.magic, hdr2.magic);
        assert_eq!(hdr.version, hdr2.version);
        assert_eq!(hdr.flags, hdr2.flags);
        assert_eq!(hdr.command, hdr2.command);
        assert_eq!(hdr.payload_size, hdr2.payload_size);
    }

    #[test]
    fn test_header_roundtrip_be() {
        let hdr = PvaHeader {
            magic: PVA_MAGIC,
            version: PVA_VERSION,
            flags: FLAGS_APP_NONSEG_BE,
            command: CMD_GET,
            payload_size: 0xDEADBEEF,
        };
        let bytes = hdr.to_bytes(true);
        let hdr2 = PvaHeader::from_bytes(&bytes).unwrap();
        assert_eq!(hdr.payload_size, hdr2.payload_size);
        assert!(hdr2.is_big_endian());
    }

    #[test]
    fn test_bad_magic() {
        let buf = [0xFF, 2, 0x30, 3, 0, 0, 0, 0];
        assert!(PvaHeader::from_bytes(&buf).is_err());
    }
}
