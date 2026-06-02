//! PVA wire-protocol primitives.
//!
//! This module is the byte-level foundation for the rest of the crate. It
//! intentionally has zero dependency on `epics-base-rs` or higher-level
//! types (`PvField`, `NormativeTypes`, ...) so that protocol code can be
//! exercised with raw fixtures.
//!
//! Layered roughly after pvxs `src/pvaproto.h`:
//!
//! - [`buffer`]   — endian-aware read/write helpers over `bytes::{Buf, BufMut}`
//! - [`size`]     — variable-length size encoding (`Size`)
//! - [`string`]   — length-prefixed UTF-8 strings
//! - [`status`]   — operation status codes
//! - [`header`]   — 8-byte PVA frame header (`PvaHeader`)
//! - [`command`]  — application/control command codes + QoS subcommand flags
//! - [`bitset`]   — `BitSet` for monitor delta encoding (pvxs `bitmask.cpp`)
//! - [`selector`] — field selectors used by `pvRequest`
//! - [`ip`]       — IPv4/IPv6 ↔ 16-byte PVA address conversion

pub mod bitset;
pub mod buffer;
pub mod command;
pub mod header;
pub mod ip;
pub mod selector;
pub mod size;
pub mod status;
pub mod string;

pub use bitset::BitSet;
pub use buffer::{ByteOrder, DecodeError, ReadExt, WriteExt};
pub use command::{Command, ControlCommand, MessageType, QosFlags};
pub use header::{HeaderFlags, MAGIC, PVA_VERSION, PvaHeader};
pub use ip::{ip_from_bytes, ip_from_bytes_allow_unspec, ip_to_bytes};
pub use selector::Selector;
pub use size::{NULL_MARKER, decode_size, encode_size, encode_size_into};
pub use status::{Status, StatusKind};
pub use string::{
    decode_string, decode_string_bytes, decode_string_value, encode_string,
    encode_string_bytes_into, encode_string_into,
};
