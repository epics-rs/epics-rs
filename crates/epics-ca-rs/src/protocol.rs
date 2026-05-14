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

// ECA status codes — DEFMSG(severity, msg_no) encoding per caerr.h.
// Values match epics-base verbatim so the wire protocol is interoperable.
pub const CA_K_INFO: u32 = 3;
pub const CA_K_ERROR: u32 = 2;
pub const CA_K_SUCCESS: u32 = 1;
pub const CA_K_WARNING: u32 = 0;
pub const CA_K_SEVERE: u32 = 4;
pub const CA_K_FATAL: u32 = CA_K_ERROR | CA_K_SEVERE; // 6

pub const fn defmsg(sev: u32, num: u32) -> u32 {
    ((num << 3) & 0x0000FFF8) | (sev & 0x00000007)
}

// Full ECA table — see caerr.h for canonical definitions.
pub const ECA_NORMAL: u32 = defmsg(CA_K_SUCCESS, 0);
pub const ECA_MAXIOC: u32 = defmsg(CA_K_ERROR, 1);
pub const ECA_UKNHOST: u32 = defmsg(CA_K_ERROR, 2);
pub const ECA_UKNSERV: u32 = defmsg(CA_K_ERROR, 3);
pub const ECA_SOCK: u32 = defmsg(CA_K_ERROR, 4);
pub const ECA_CONN: u32 = defmsg(CA_K_WARNING, 5);
pub const ECA_ALLOCMEM: u32 = defmsg(CA_K_WARNING, 6);
pub const ECA_UKNCHAN: u32 = defmsg(CA_K_WARNING, 7);
pub const ECA_UKNFIELD: u32 = defmsg(CA_K_WARNING, 8);
pub const ECA_TOLARGE: u32 = defmsg(CA_K_WARNING, 9);
pub const ECA_TIMEOUT: u32 = defmsg(CA_K_WARNING, 10);
pub const ECA_NOSUPPORT: u32 = defmsg(CA_K_WARNING, 11);
pub const ECA_STRTOBIG: u32 = defmsg(CA_K_WARNING, 12);
pub const ECA_DISCONNCHID: u32 = defmsg(CA_K_ERROR, 13);
pub const ECA_BADTYPE: u32 = defmsg(CA_K_ERROR, 14);
pub const ECA_CHIDNOTFND: u32 = defmsg(CA_K_INFO, 15);
pub const ECA_CHIDRETRY: u32 = defmsg(CA_K_INFO, 16);
pub const ECA_INTERNAL: u32 = defmsg(CA_K_FATAL, 17);
pub const ECA_DBLCLFAIL: u32 = defmsg(CA_K_WARNING, 18);
pub const ECA_GETFAIL: u32 = defmsg(CA_K_WARNING, 19);
pub const ECA_PUTFAIL: u32 = defmsg(CA_K_WARNING, 20);
pub const ECA_ADDFAIL: u32 = defmsg(CA_K_WARNING, 21);
pub const ECA_BADCOUNT: u32 = defmsg(CA_K_WARNING, 22);
pub const ECA_BADSTR: u32 = defmsg(CA_K_ERROR, 23);
pub const ECA_DISCONN: u32 = defmsg(CA_K_WARNING, 24);
pub const ECA_DBLCHNL: u32 = defmsg(CA_K_WARNING, 25);
pub const ECA_EVDISALLOW: u32 = defmsg(CA_K_ERROR, 26);
pub const ECA_BUILDGET: u32 = defmsg(CA_K_WARNING, 27);
pub const ECA_NEEDSFP: u32 = defmsg(CA_K_WARNING, 28);
pub const ECA_OVEVFAIL: u32 = defmsg(CA_K_WARNING, 29);
pub const ECA_BADMONID: u32 = defmsg(CA_K_ERROR, 30);
pub const ECA_NEWADDR: u32 = defmsg(CA_K_WARNING, 31);
pub const ECA_NEWCONN: u32 = defmsg(CA_K_INFO, 32);
pub const ECA_NOCACTX: u32 = defmsg(CA_K_WARNING, 33);
pub const ECA_DEFUNCT: u32 = defmsg(CA_K_FATAL, 34);
pub const ECA_EMPTYSTR: u32 = defmsg(CA_K_WARNING, 35);
pub const ECA_NOREPEATER: u32 = defmsg(CA_K_WARNING, 36);
pub const ECA_NOCHANMSG: u32 = defmsg(CA_K_WARNING, 37);
pub const ECA_DLCKREST: u32 = defmsg(CA_K_WARNING, 38);
pub const ECA_SERVBEHIND: u32 = defmsg(CA_K_WARNING, 39);
pub const ECA_NOCAST: u32 = defmsg(CA_K_WARNING, 40);
pub const ECA_BADMASK: u32 = defmsg(CA_K_ERROR, 41);
pub const ECA_IODONE: u32 = defmsg(CA_K_INFO, 42);
pub const ECA_IOINPROGRESS: u32 = defmsg(CA_K_INFO, 43);
pub const ECA_BADSYNCGRP: u32 = defmsg(CA_K_ERROR, 44);
pub const ECA_PUTCBINPROG: u32 = defmsg(CA_K_ERROR, 45);
pub const ECA_NORDACCESS: u32 = defmsg(CA_K_WARNING, 46);
pub const ECA_NOWTACCESS: u32 = defmsg(CA_K_WARNING, 47);
pub const ECA_ANACHRONISM: u32 = defmsg(CA_K_ERROR, 48);
pub const ECA_NOSEARCHADDR: u32 = defmsg(CA_K_WARNING, 49);
pub const ECA_NOCONVERT: u32 = defmsg(CA_K_WARNING, 50);
pub const ECA_BADCHID: u32 = defmsg(CA_K_ERROR, 51);
pub const ECA_BADFUNCPTR: u32 = defmsg(CA_K_ERROR, 52);
pub const ECA_ISATTACHED: u32 = defmsg(CA_K_WARNING, 53);
pub const ECA_UNAVAILINSERV: u32 = defmsg(CA_K_WARNING, 54);
pub const ECA_CHANDESTROY: u32 = defmsg(CA_K_WARNING, 55);
pub const ECA_BADPRIORITY: u32 = defmsg(CA_K_ERROR, 56);
pub const ECA_NOTTHREADED: u32 = defmsg(CA_K_ERROR, 57);
pub const ECA_16KARRAYCLIENT: u32 = defmsg(CA_K_WARNING, 58);
pub const ECA_CONNSEQTMO: u32 = defmsg(CA_K_WARNING, 59);
pub const ECA_UNRESPTMO: u32 = defmsg(CA_K_WARNING, 60);

/// Extract the message number (caerr.h MSG_NO_OF_STATUS).
pub const fn eca_msg_no(status: u32) -> u32 {
    (status >> 3) & 0x1FFF
}

/// Extract severity bits (caerr.h SEVERITY_OF_STATUS).
pub const fn eca_severity(status: u32) -> u32 {
    status & 0x7
}

/// Human-readable text for an ECA status, mirroring libca `ca_message`.
pub fn eca_message(status: u32) -> &'static str {
    let msg_no = eca_msg_no(status) as usize;
    ECA_MESSAGE_TEXT
        .get(msg_no)
        .copied()
        .unwrap_or("Unknown ECA status")
}

/// Strings copied verbatim from `epics-base/modules/ca/src/client/access.cpp`
/// `ca_message_text[]`.
pub const ECA_MESSAGE_TEXT: &[&str] = &[
    "Normal successful completion",
    "Maximum simultaneous IOC connections exceeded",
    "Unknown internet host",
    "Unknown internet service",
    "Unable to allocate a new socket",
    "Unable to connect to internet host or service",
    "Unable to allocate additional dynamic memory",
    "Unknown IO channel",
    "Record field specified inappropriate for channel specified",
    "The requested data transfer is greater than available memory or EPICS_CA_MAX_ARRAY_BYTES",
    "User specified timeout on IO operation expired",
    "Sorry, that feature is planned but not supported at this time",
    "The supplied string is unusually large",
    "The request was ignored because the specified channel is disconnected",
    "The data type specified is invalid",
    "Remote Channel not found",
    "Unable to locate all user specified channels",
    "Channel Access Internal Failure",
    "The requested local DB operation failed",
    "Channel read request failed",
    "Channel write request failed",
    "Channel subscription request failed",
    "Invalid element count requested",
    "Invalid string",
    "Virtual circuit disconnect",
    "Identical process variable names on multiple servers",
    "Request inappropriate within subscription (monitor) update callback",
    "Database value get for that channel failed during channel search",
    "Unable to initialize without the vxWorks VX_FP_TASK task option set",
    "Event queue overflow has prevented first pass event after event add",
    "Bad event subscription (monitor) identifier",
    "Remote channel has new network address",
    "New or resumed network connection",
    "Specified task isn't a member of a CA context",
    "Attempt to use defunct CA feature failed",
    "The supplied string is empty",
    "Unable to spawn the CA repeater thread- auto reconnect will fail",
    "No channel id match for search reply- search reply ignored",
    "Resetting dead connection- will try to reconnect",
    "Server (IOC) has fallen behind or is not responding- still waiting",
    "No internet interface with broadcast available",
    "Invalid event selection mask",
    "IO operations have completed",
    "IO operations are in progress",
    "Invalid synchronous group identifier",
    "Put callback timed out",
    "Read access denied",
    "Write access denied",
    "Requested feature is no longer supported",
    "Empty PV search address list",
    "No reasonable data conversion between client and server types",
    "Invalid channel identifier",
    "Invalid function pointer",
    "Thread is already attached to a client context",
    "Not supported by attached service",
    "User destroyed channel",
    "Invalid channel priority",
    "Preemptive callback not enabled - additional threads may not join context",
    "Client's protocol revision does not support transfers exceeding 16k bytes",
    "Virtual circuit connection sequence aborted",
    "Virtual circuit unresponsive",
];

/// Maximum payload size for DoS prevention (16 MB).
/// Maximum payload size for DoS prevention.
/// Default 16 MB, configurable via EPICS_CA_MAX_ARRAY_BYTES (matches C EPICS).
pub fn max_payload_size() -> usize {
    epics_base_rs::runtime::env::get("EPICS_CA_MAX_ARRAY_BYTES")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(16 * 1024 * 1024)
}

/// Compile-time constant for tests that need a fixed value.
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
    ///
    /// Wire detection is by `postsize == 0xFFFF` alone, matching C
    /// `tcpiiu.cpp::processIncoming` (line 1168), `cac.cpp:1097`, and
    /// `rsrv/camessage.c:2410`. The `count == 0` field is set by the
    /// emit-side per the spec but is NOT checked on receive — a peer
    /// sending garbage in `m_count` of an extended header is still
    /// correctly parsed by C. We mirror C's lenient receive behavior.
    pub fn is_extended(&self) -> bool {
        self.postsize == 0xFFFF && self.extended_postsize.is_some()
    }

    /// Actual payload size in bytes.
    pub fn actual_postsize(&self) -> usize {
        if self.postsize == 0xFFFF {
            if let Some(ext) = self.extended_postsize {
                return ext as usize;
            }
        }
        self.postsize as usize
    }

    /// Actual element count.
    pub fn actual_count(&self) -> u32 {
        if self.postsize == 0xFFFF {
            if let Some(ext) = self.extended_count {
                return ext;
            }
        }
        self.count as u32
    }

    /// Set payload size and count, automatically switching to extended form if needed.
    /// `size` is the actual data length (unpadded). Wire-level 8-byte alignment padding
    /// is handled by the caller when writing to the socket, NOT stored in the header.
    ///
    /// Extended-form trigger matches C `comQueSend.cpp:285`:
    /// `payloadSize < 0xffff && nElem < 0xffff` → normal; equivalently,
    /// extended if `size >= 0xFFFF` OR `count >= 0xFFFF`. The previous
    /// Rust threshold (`count > 0xFFFF`) under-triggered for the exact
    /// `count == 0xFFFF` case, sending a normal-form header where C
    /// would have used extended — byte-mismatch on the wire.
    pub fn set_payload_size(&mut self, size: usize, count: u32) {
        if size >= 0xFFFF || count >= 0xFFFF {
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

        // C parity: extended-form detection is `m_postsize == 0xffff`
        // alone — see `tcpiiu.cpp:1168`, `cac.cpp:1097`, and
        // `rsrv/camessage.c:2410`. The `m_count == 0` half was an
        // overly-strict Rust addition that rejected legal extended
        // headers if a peer left non-zero garbage in `m_count`.
        if hdr.postsize == 0xFFFF {
            if buf.len() < 24 {
                return Err(CaError::Protocol("extended header incomplete".into()));
            }
            let ext_post = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
            let ext_count = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
            if ext_post as usize > max_payload_size() {
                return Err(CaError::Protocol("payload too large".into()));
            }
            hdr.extended_postsize = Some(ext_post);
            hdr.extended_count = Some(ext_count);
            consumed = 24;
        }

        Ok((hdr, consumed))
    }
}

/// Round up to 8-byte alignment.
/// Uses saturating_add to prevent overflow on pathological values.
pub fn align8(size: usize) -> usize {
    size.saturating_add(7) & !7
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
        // count >= 0xFFFF triggers extended even if size is small.
        // C `comQueSend.cpp:285` uses `nElem < 0xffff` as the normal
        // threshold, so exact 0xFFFF must take the extended branch.
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(100, 100_000);
        assert!(hdr.is_extended());
        assert_eq!(hdr.actual_postsize(), 100);
        assert_eq!(hdr.actual_count(), 100_000);

        // Exact 0xFFFF boundary — must trigger extended (regression
        // for the prior `count > 0xFFFF` under-trigger).
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(100, 0xFFFF);
        assert!(hdr.is_extended());
        assert_eq!(hdr.actual_count(), 0xFFFF);
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
