//! `StreamCore.h`: the boundary between the protocol engine (`StreamCore`,
//! a client of both the protocol parser and a bus) and the record layer
//! that subclasses it (`StreamEpics`'s `Stream`).
//!
//! The engine runs the compiled commands; the record layer implements
//! [`StreamCoreHooks`], C's virtual methods:
//!
//! - `get_field_address(fieldname, address)`: for `%(FIELD)` formats, put an
//!   address structure for the field into `address`; it is copied bytewise
//!   into the compiled code and handed back to `format_value` /
//!   `match_value`. False when there is no such field.
//! - `format_value(io, format, fieldaddress)`: take the value from the field
//!   (or the default location for `format.type_`) and print it with the
//!   matching `io.print_value_*`, once per element of an array (the
//!   separator is added by the engine).
//! - `match_value(io, format, fieldaddress)`: scan with the matching
//!   `io.scan_value_*` and store the value; for arrays, once per element
//!   until a scan fails (the separator is matched by the engine).
//! - `start_timer`, `lock_mutex` / `release_mutex` (recursive), and the
//!   start / input / finish hooks.

use crate::buffer::StreamBuffer;
use crate::format::StreamFormat;

// Flags: 0x00FFFFFF reserved for StreamCore
pub const NONE: u64 = 0x0000;
pub const IGNORE_EXTRA_INPUT: u64 = 0x0001;
pub const INIT_RUN: u64 = 0x0002;
pub const ASYNC_MODE: u64 = 0x0004;
pub const GOT_VALUE: u64 = 0x0008;
pub const BUS_OWNER: u64 = 0x0010;
pub const SEPARATOR: u64 = 0x0020;
pub const SCAN_TRIED: u64 = 0x0040;
pub const ACCEPT_INPUT: u64 = 0x0100;
pub const ACCEPT_EVENT: u64 = 0x0200;
pub const LOCK_PENDING: u64 = 0x0400;
pub const WRITE_PENDING: u64 = 0x0800;
pub const WAIT_PENDING: u64 = 0x1000;
pub const ABORTED: u64 = 0x2000;
pub const BUS_PENDING: u64 = LOCK_PENDING | WRITE_PENDING | WAIT_PENDING;
pub const CLEAR_ON_START: u64 = INIT_RUN
    | ASYNC_MODE
    | GOT_VALUE
    | ABORTED
    | BUS_OWNER
    | SEPARATOR
    | SCAN_TRIED
    | ACCEPT_INPUT
    | ACCEPT_EVENT
    | BUS_PENDING;

/// `ENUM(ProtocolResult, ...)`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolResult {
    Success,
    LockTimeout,
    WriteTimeout,
    ReplyTimeout,
    ReadTimeout,
    ScanError,
    FormatError,
    Abort,
    Fault,
    Offline,
}

impl ProtocolResult {
    /// `toStr`
    pub fn to_str(self) -> &'static str {
        match self {
            Self::Success => "Success",
            Self::LockTimeout => "LockTimeout",
            Self::WriteTimeout => "WriteTimeout",
            Self::ReplyTimeout => "ReplyTimeout",
            Self::ReadTimeout => "ReadTimeout",
            Self::ScanError => "ScanError",
            Self::FormatError => "FormatError",
            Self::Abort => "Abort",
            Self::Fault => "Fault",
            Self::Offline => "Offline",
        }
    }
}

/// `ENUM(StartMode, ...)`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartMode {
    StartNormal,
    StartInit,
    StartAsync,
}

impl StartMode {
    /// `toStr`
    pub fn to_str(self) -> &'static str {
        match self {
            Self::StartNormal => "StartNormal",
            Self::StartInit => "StartInit",
            Self::StartAsync => "StartAsync",
        }
    }
}

/// `ENUM(Commands, ...)`: the first byte of each compiled command.
pub mod commands {
    pub const END: u8 = 0;
    pub const IN: u8 = 1;
    pub const OUT: u8 = 2;
    pub const WAIT: u8 = 3;
    pub const EVENT: u8 = 4;
    pub const EXEC: u8 = 5;
    pub const CONNECT: u8 = 6;
    pub const DISCONNECT: u8 = 7;

    /// `toStr`
    pub fn to_str(c: u8) -> &'static str {
        match c {
            END => "end",
            IN => "in",
            OUT => "out",
            WAIT => "wait",
            EVENT => "event",
            EXEC => "exec",
            CONNECT => "connect",
            DISCONNECT => "disconnect",
            _ => "invalid",
        }
    }
}

/// `StreamCore`'s `printValue` / `scanValue`, which the record layer calls
/// from `format_value` / `match_value`.
pub trait ValueIo {
    fn print_value_long(&mut self, format: &StreamFormat, value: i64) -> bool;
    fn print_value_double(&mut self, format: &StreamFormat, value: f64) -> bool;
    fn print_value_string(&mut self, format: &StreamFormat, value: &[u8]) -> bool;
    fn scan_value_long(&mut self, format: &StreamFormat, value: &mut i64) -> isize;
    fn scan_value_double(&mut self, format: &StreamFormat, value: &mut f64) -> isize;
    /// `value.len()` is C's `size` on entry; `size` is the length read.
    fn scan_value_string(
        &mut self,
        format: &StreamFormat,
        value: &mut [u8],
        size: &mut usize,
    ) -> isize;
    /// `scanValue(format)`: a pseudo or skipped conversion.
    fn scan_value(&mut self, format: &StreamFormat) -> isize;
}

/// StreamCore's virtual methods, implemented by the record layer.
pub trait StreamCoreHooks: Send + Sync {
    fn protocol_start_hook(&self) {}
    fn input_hook(&self, _input: &[u8]) {}
    fn protocol_finish_hook(&self, _result: ProtocolResult) {}
    fn start_timer(&self, timeout_ms: u64);
    fn format_value(
        &self,
        io: &mut dyn ValueIo,
        format: &StreamFormat,
        fieldaddress: Option<&[u8]>,
    ) -> bool;
    fn match_value(
        &self,
        io: &mut dyn ValueIo,
        format: &StreamFormat,
        fieldaddress: Option<&[u8]>,
    ) -> bool;
    fn lock_mutex(&self);
    fn release_mutex(&self);
    /// `execute`: start the `exec` command `command` (the formatted output
    /// line); its completion is reported to the engine's `execCallback`.
    /// The default is C's: not supported.
    fn execute(&self, _command: &[u8]) -> bool {
        crate::error::stream_error(
            format!(
                "{}: Command 'exec' not implemented on this system\n",
                self.name()
            )
            .as_bytes(),
        );
        false
    }
    /// `getFieldAddress` (the parser client's, implemented here).
    fn get_field_address(&self, fieldname: &[u8], address: &mut StreamBuffer) -> bool;
    fn name(&self) -> String;
}
