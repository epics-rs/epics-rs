//! PVA Status type (operation result code).
//!
//! Wire encoding (pvxs `pvaproto.h::to_wire(Status)`):
//!
//! ```text
//! type byte:
//!   0xFF → OK with no message ("OK_NO_MSG")
//!   0x00 → OK    + message + stack
//!   0x01 → WARN  + message + stack
//!   0x02 → ERROR + message + stack
//!   0x03 → FATAL + message + stack
//! ```
//!
//! When the type byte is anything except `0xFF`, the next two PVA strings
//! are the human-readable message and a (possibly empty) stack trace.

use std::io::Cursor;

use super::buffer::{ByteOrder, DecodeError, ReadExt, WriteExt};
use super::string::{decode_string, encode_string_into};
use crate::decode_err;

/// Severity / kind of a `Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StatusKind {
    Ok = 0,
    Warning = 1,
    Error = 2,
    Fatal = 3,
}

impl StatusKind {
    pub fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Ok,
            1 => Self::Warning,
            2 => Self::Error,
            3 => Self::Fatal,
            _ => return None,
        })
    }
}

/// Result of a PVA operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Bare OK — wire byte `0xFF`, no message.
    OkNoMsg,
    /// OK / Warning / Error / Fatal carrying message + stack trace strings.
    Detailed {
        kind: StatusKind,
        message: String,
        stack: String,
    },
}

impl Status {
    pub fn ok() -> Self {
        Status::OkNoMsg
    }

    pub fn error<S: Into<String>>(msg: S) -> Self {
        Status::Detailed {
            kind: StatusKind::Error,
            message: msg.into(),
            stack: String::new(),
        }
    }

    pub fn warning<S: Into<String>>(msg: S) -> Self {
        Status::Detailed {
            kind: StatusKind::Warning,
            message: msg.into(),
            stack: String::new(),
        }
    }

    pub fn fatal<S: Into<String>>(msg: S) -> Self {
        Status::Detailed {
            kind: StatusKind::Fatal,
            message: msg.into(),
            stack: String::new(),
        }
    }

    pub fn is_success(&self) -> bool {
        match self {
            Status::OkNoMsg => true,
            Status::Detailed { kind, .. } => matches!(kind, StatusKind::Ok | StatusKind::Warning),
        }
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            Status::OkNoMsg => None,
            Status::Detailed { message, .. } => Some(message.as_str()),
        }
    }

    /// Encode and return a freshly allocated buffer.
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_into(order, &mut out);
        out
    }

    /// Append the encoded form to `buf`.
    pub fn write_into(&self, order: ByteOrder, buf: &mut Vec<u8>) {
        match self {
            Status::OkNoMsg => buf.put_u8(0xFF),
            Status::Detailed {
                kind,
                message,
                stack,
            } => {
                buf.put_u8(*kind as u8);
                encode_string_into(message, order, buf);
                encode_string_into(stack, order, buf);
            }
        }
    }

    pub fn decode(cur: &mut Cursor<&[u8]>, order: ByteOrder) -> Result<Self, DecodeError> {
        let kind_byte = cur.get_u8()?;
        if kind_byte == 0xFF {
            return Ok(Status::OkNoMsg);
        }
        let kind = StatusKind::from_byte(kind_byte)
            .ok_or_else(|| decode_err!("unknown status kind 0x{kind_byte:02X}"))?;
        let message = decode_string(cur, order)?.unwrap_or_default();
        let stack = decode_string(cur, order)?.unwrap_or_default();
        Ok(Status::Detailed {
            kind,
            message,
            stack,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_no_msg_is_single_byte() {
        let buf = Status::ok().encode(ByteOrder::Little);
        assert_eq!(buf, vec![0xFF]);
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(
            Status::decode(&mut cur, ByteOrder::Little).unwrap(),
            Status::OkNoMsg
        );
    }

    #[test]
    fn error_round_trip() {
        let s = Status::error("bad request");
        let buf = s.encode(ByteOrder::Little);
        // type=0x02, "bad request" (size 0x0B + 11 bytes), empty stack (0x00)
        assert_eq!(buf[0], 0x02);
        assert_eq!(buf[1] as usize, "bad request".len());
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(Status::decode(&mut cur, ByteOrder::Little).unwrap(), s);
    }

    #[test]
    fn matches_spvirit_layout() {
        // spvirit::encode_status_error("oops", false) yields:
        //   [0x02, 0x04, b'o', b'o', b'p', b's', 0x00]
        let s = Status::error("oops");
        assert_eq!(
            s.encode(ByteOrder::Little),
            vec![0x02, 0x04, b'o', b'o', b'p', b's', 0x00]
        );
    }

    #[test]
    fn is_success_classification() {
        assert!(Status::ok().is_success());
        assert!(Status::warning("careful").is_success());
        assert!(!Status::error("nope").is_success());
        assert!(!Status::fatal("dead").is_success());
    }
}
