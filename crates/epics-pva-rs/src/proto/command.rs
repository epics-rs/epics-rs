//! PVA command codes and QoS subcommand flags.
//!
//! Source: pvxs `pvaproto.h::pva_app_msg_t`, `pva_ctrl_msg_t`, `pva_search_flags`,
//! plus the `QOS_*` bit definitions from `serverget.cpp`/`servermon.cpp`.

/// Application-level command codes (header `command` byte when flag bit 0 = 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    Beacon = 0,
    ConnectionValidation = 1,
    Echo = 2,
    Search = 3,
    SearchResponse = 4,
    AuthNZ = 5,
    AclChange = 6,
    CreateChannel = 7,
    DestroyChannel = 8,
    ConnectionValidated = 9,
    Get = 10,
    Put = 11,
    PutGet = 12,
    Monitor = 13,
    Array = 14,
    DestroyRequest = 15,
    Process = 16,
    GetField = 17,
    Message = 18,
    MultipleData = 19,
    Rpc = 20,
    CancelRequest = 21,
    OriginTag = 22,
}

impl Command {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Beacon,
            1 => Self::ConnectionValidation,
            2 => Self::Echo,
            3 => Self::Search,
            4 => Self::SearchResponse,
            5 => Self::AuthNZ,
            6 => Self::AclChange,
            7 => Self::CreateChannel,
            8 => Self::DestroyChannel,
            9 => Self::ConnectionValidated,
            10 => Self::Get,
            11 => Self::Put,
            12 => Self::PutGet,
            13 => Self::Monitor,
            14 => Self::Array,
            15 => Self::DestroyRequest,
            16 => Self::Process,
            17 => Self::GetField,
            18 => Self::Message,
            19 => Self::MultipleData,
            20 => Self::Rpc,
            21 => Self::CancelRequest,
            22 => Self::OriginTag,
            _ => return None,
        })
    }
}

/// Control message types (header `command` byte when flag bit 0 = 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlCommand {
    SetMarker = 0,
    AckMarker = 1,
    SetByteOrder = 2,
    EchoRequest = 3,
    EchoResponse = 4,
}

impl ControlCommand {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::SetMarker,
            1 => Self::AckMarker,
            2 => Self::SetByteOrder,
            3 => Self::EchoRequest,
            4 => Self::EchoResponse,
            _ => return None,
        })
    }
}

/// QoS / subcommand flags carried in the operation `subcmd` byte.
///
/// From pvxs (`serverget.cpp`/`servermon.cpp`/`clientreq.cpp`):
pub struct QosFlags;

impl QosFlags {
    /// `0x04` — invoke `process()` on the underlying record (PUT only).
    pub const PROCESS: u8 = 0x04;
    /// `0x08` — INIT phase (carries pvRequest / introspection setup).
    pub const INIT: u8 = 0x08;
    /// `0x10` — DESTROY phase (release operation, no further requests).
    pub const DESTROY: u8 = 0x10;
    /// `0x40` — `QOS_GET`. On PUT_GET this is the EPICS `getGet`
    /// subcommand: read the current get-side data with no put payload
    /// (pvAccess `remote.h:78`).
    pub const GET: u8 = 0x40;
    /// `0x44` — START a paused monitor (control bit `0x04` | GET bit `0x40`; subscriber → server).
    pub const MONITOR_START: u8 = 0x44;
    /// `0x04` — STOP a running monitor (control bit only; subscriber → server).
    pub const MONITOR_STOP: u8 = 0x04;
    /// `0x80` — `QOS_GET_PUT`. On PUT_GET this is the EPICS `getPut`
    /// subcommand: read the current put-side data with no put payload
    /// (pvAccess `remote.h:82`). The same byte means `PIPELINE_ACK` only
    /// in MONITOR context, so callers must scope it to the command.
    pub const GET_PUT: u8 = 0x80;
    /// `0x80` — pipelined ack (number of free slots in flow window).
    /// MONITOR-scoped; see [`Self::GET_PUT`] for the PUT_GET meaning of
    /// the same bit.
    pub const PIPELINE_ACK: u8 = 0x80;
}

/// PVA `MESSAGE` payload `messageType` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Info = 0,
    Warning = 1,
    Error = 2,
    Fatal = 3,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_codes_round_trip() {
        for cmd in [
            Command::Search,
            Command::CreateChannel,
            Command::Get,
            Command::Put,
            Command::Monitor,
            Command::GetField,
            Command::Rpc,
            Command::DestroyRequest,
        ] {
            assert_eq!(Command::from_code(cmd.code()), Some(cmd));
        }
    }

    #[test]
    fn control_codes_round_trip() {
        for cmd in [
            ControlCommand::SetByteOrder,
            ControlCommand::EchoRequest,
            ControlCommand::EchoResponse,
        ] {
            assert_eq!(ControlCommand::from_code(cmd.code()), Some(cmd));
        }
    }

    #[test]
    fn unknown_codes_return_none() {
        assert_eq!(Command::from_code(255), None);
        assert_eq!(ControlCommand::from_code(99), None);
    }
}
