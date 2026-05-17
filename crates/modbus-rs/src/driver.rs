//! The Modbus driver — port of `drvModbusAsyn`.
//!
//! [`ModbusEngine`] owns a port's register/coil buffer, builds Modbus
//! requests, runs the write/read cycle over an [`OctetTransport`], parses
//! responses, and tracks I/O statistics. The asyn-record fan-out (the
//! `readPoller` interrupt callbacks and the `readInt32`/`writeInt32`/…
//! interface methods) is layered on top in the `ioc` module; this module is
//! the transport-agnostic core and is fully unit-testable with a mock
//! transport.

use std::time::{Duration, Instant};

use crate::datatype::ModbusDataType;
use crate::error::{ExceptionCode, ModbusError, ModbusResult};
use crate::interpose::{LinkType, ModbusFramer};
use crate::protocol::{FunctionCode, RequestPdu, ResponsePdu};

/// Modbus limit on the number of words read in one request.
pub const MAX_READ_WORDS: usize = 125;
/// Modbus limit on the number of words written in one request.
pub const MAX_WRITE_WORDS: usize = 123;
/// Default timeout for one write/read cycle (C `MODBUS_READ_TIMEOUT`).
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Number of bins in the I/O-time histogram (C `HISTOGRAM_LENGTH`).
pub const HISTOGRAM_LENGTH: usize = 200;
/// Register-readback address offset applied for Wago PLCs (C `WAGO_OFFSET`).
pub const WAGO_OFFSET: i32 = 0x200;
/// Number of UDP retransmits on a read failure before giving up.
const UDP_MAX_RETRIES: u32 = 5;

/// The Modbus operation a driver port is configured for.
///
/// The `*F23` variants are driver-internal pseudo-codes (C `123` / `223`):
/// they read/write registers using the real combined read/write function
/// code `0x17`, for slaves that only support function 23.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModbusFunctionCode {
    /// Read coils (FC 0x01).
    ReadCoils,
    /// Read discrete inputs (FC 0x02).
    ReadDiscreteInputs,
    /// Read holding registers (FC 0x03).
    ReadHoldingRegisters,
    /// Read input registers (FC 0x04).
    ReadInputRegisters,
    /// Report slave ID (FC 0x11).
    ReportSlaveId,
    /// Read input registers via FC 0x17 (driver pseudo-code 123).
    ReadInputRegistersF23,
    /// Write a single coil (FC 0x05).
    WriteSingleCoil,
    /// Write multiple coils (FC 0x0F).
    WriteMultipleCoils,
    /// Write a single register (FC 0x06).
    WriteSingleRegister,
    /// Write multiple registers (FC 0x10).
    WriteMultipleRegisters,
    /// Write multiple registers via FC 0x17 (driver pseudo-code 223).
    WriteMultipleRegistersF23,
}

impl ModbusFunctionCode {
    /// Decode the integer code accepted by `drvModbusAsynConfigure`, including
    /// the driver pseudo-codes 123 and 223.
    pub fn from_i32(v: i32) -> Option<Self> {
        Some(match v {
            1 => Self::ReadCoils,
            2 => Self::ReadDiscreteInputs,
            3 => Self::ReadHoldingRegisters,
            4 => Self::ReadInputRegisters,
            5 => Self::WriteSingleCoil,
            6 => Self::WriteSingleRegister,
            15 => Self::WriteMultipleCoils,
            16 => Self::WriteMultipleRegisters,
            17 => Self::ReportSlaveId,
            123 => Self::ReadInputRegistersF23,
            223 => Self::WriteMultipleRegistersF23,
            _ => return None,
        })
    }

    /// Whether this is a read operation (drives a poller thread).
    pub fn is_read(self) -> bool {
        matches!(
            self,
            Self::ReadCoils
                | Self::ReadDiscreteInputs
                | Self::ReadHoldingRegisters
                | Self::ReadInputRegisters
                | Self::ReportSlaveId
                | Self::ReadInputRegistersF23
        )
    }

    /// Whether this is a write operation.
    pub fn is_write(self) -> bool {
        !self.is_read()
    }

    /// Whether this function addresses bit (coil) data rather than registers.
    pub fn is_bit(self) -> bool {
        matches!(
            self,
            Self::ReadCoils
                | Self::ReadDiscreteInputs
                | Self::WriteSingleCoil
                | Self::WriteMultipleCoils
        )
    }

    /// Maximum valid `modbusLength` for a port using this function. Bit
    /// functions allow 16× the word limit; see the C constructor.
    pub fn max_length(self) -> usize {
        match self {
            Self::ReadCoils | Self::ReadDiscreteInputs => MAX_READ_WORDS * 16,
            Self::ReadHoldingRegisters
            | Self::ReadInputRegisters
            | Self::ReportSlaveId
            | Self::ReadInputRegistersF23 => MAX_READ_WORDS,
            Self::WriteSingleCoil | Self::WriteMultipleCoils => MAX_WRITE_WORDS * 16,
            Self::WriteSingleRegister
            | Self::WriteMultipleRegisters
            | Self::WriteMultipleRegistersF23 => MAX_WRITE_WORDS,
        }
    }

    /// For a write function, the read function used by the initial readback
    /// (`readOnceFunction_`); `None` for read functions.
    pub fn readonce_function(self) -> Option<Self> {
        Some(match self {
            Self::WriteSingleCoil | Self::WriteMultipleCoils => Self::ReadCoils,
            Self::WriteSingleRegister | Self::WriteMultipleRegisters => Self::ReadHoldingRegisters,
            Self::WriteMultipleRegistersF23 => Self::ReadInputRegistersF23,
            _ => return None,
        })
    }
}

/// Static configuration of a Modbus driver port — the arguments to
/// `drvModbusAsynConfigure`.
#[derive(Debug, Clone)]
pub struct ModbusConfig {
    /// Modbus slave (unit) address.
    pub slave: u8,
    /// The configured Modbus function.
    pub function: ModbusFunctionCode,
    /// Starting Modbus address, or `-1` to select absolute addressing.
    pub start_address: i32,
    /// Number of words or bits of Modbus data this port covers.
    pub length: usize,
    /// Default data type for the port's registers.
    pub data_type: ModbusDataType,
    /// Poll interval; zero means "no periodic polling" (event-driven only).
    pub poll_delay: Duration,
    /// PLC type string (e.g. `"Koyo"`, `"Wago750"`).
    pub plc_type: String,
}

impl ModbusConfig {
    /// Whether absolute addressing is in effect (`start_address == -1`).
    pub fn absolute_addressing(&self) -> bool {
        self.start_address == -1
    }

    /// Register-readback offset: `WAGO_OFFSET` when the PLC type names a Wago
    /// device, else 0. Mirrors the `strstr(plcType_, "Wago")` check.
    pub fn readback_offset(&self) -> i32 {
        if self.plc_type.contains("Wago") {
            WAGO_OFFSET
        } else {
            0
        }
    }

    /// Validate the configuration, returning the effective start address used
    /// for non-absolute polling. Mirrors the constructor's length checks.
    pub fn validate(&self) -> ModbusResult<()> {
        if self.length == 0 {
            return Err(ModbusError::InvalidFunction(0));
        }
        if self.length > self.function.max_length() {
            return Err(ModbusError::FrameTooLarge(self.length));
        }
        Ok(())
    }
}

/// I/O statistics for a driver port — port of the `readOK_` / `writeOK_` /
/// `IOErrors_` counters and the read-time histogram.
#[derive(Debug, Clone)]
pub struct IoStatistics {
    /// Count of successful read operations.
    pub read_ok: u32,
    /// Count of successful write operations.
    pub write_ok: u32,
    /// Cumulative count of I/O errors.
    pub io_errors: u32,
    /// I/O errors since the last successful write/read cycle.
    pub current_io_errors: u32,
    /// Duration of the most recent I/O cycle, in milliseconds.
    pub last_io_msec: u32,
    /// Longest I/O cycle observed, in milliseconds.
    pub max_io_msec: u32,
    /// Whether the read-time histogram is being accumulated.
    pub histogram_enabled: bool,
    /// Milliseconds per histogram bin.
    pub histogram_ms_per_bin: u32,
    /// Read-time histogram; the last bin catches all longer times.
    pub histogram: Vec<u32>,
}

impl Default for IoStatistics {
    fn default() -> Self {
        Self {
            read_ok: 0,
            write_ok: 0,
            io_errors: 0,
            current_io_errors: 0,
            last_io_msec: 0,
            max_io_msec: 0,
            histogram_enabled: false,
            histogram_ms_per_bin: 1,
            histogram: vec![0; HISTOGRAM_LENGTH],
        }
    }
}

impl IoStatistics {
    /// Record a completed I/O cycle of `elapsed` duration into the timing
    /// stats and, if enabled, the histogram.
    fn record_timing(&mut self, elapsed: Duration) {
        let msec = (elapsed.as_secs_f64() * 1000.0 + 0.5) as u32;
        self.last_io_msec = msec;
        if msec > self.max_io_msec {
            self.max_io_msec = msec;
        }
        if self.histogram_enabled {
            let bin = (msec / self.histogram_ms_per_bin.max(1)) as usize;
            let bin = bin.min(HISTOGRAM_LENGTH - 1);
            self.histogram[bin] = self.histogram[bin].saturating_add(1);
        }
    }
}

/// A byte-stream transport — the underlying `asyn-rs` octet port the framed
/// Modbus messages travel over. One `write_frame` is followed by one or more
/// `read_frame` calls per request.
pub trait OctetTransport: Send + Sync {
    /// Send a fully framed request.
    fn write_frame(&mut self, data: &[u8]) -> ModbusResult<()>;
    /// Receive one framed response, waiting up to `timeout`.
    fn read_frame(&mut self, timeout: Duration) -> ModbusResult<Vec<u8>>;
}

/// The Modbus driver engine: request construction, the write/read cycle,
/// response parsing, and the in-memory register buffer.
pub struct ModbusEngine {
    config: ModbusConfig,
    framer: ModbusFramer,
    /// Register/coil buffer; `length` words. For coil ports each word is 0/1.
    data: Vec<u16>,
    /// Snapshot of `data` after the previous poll, for change detection.
    prev_data: Vec<u16>,
    /// I/O statistics.
    pub stats: IoStatistics,
}

impl ModbusEngine {
    /// Create an engine for `config` on the given physical link.
    pub fn new(config: ModbusConfig, link_type: LinkType) -> ModbusResult<Self> {
        config.validate()?;
        let len = config.length;
        Ok(Self {
            config,
            framer: ModbusFramer::new(link_type),
            data: vec![0; len],
            prev_data: vec![0; len],
            stats: IoStatistics::default(),
        })
    }

    /// The port configuration.
    pub fn config(&self) -> &ModbusConfig {
        &self.config
    }

    /// The register/coil buffer.
    pub fn data(&self) -> &[u16] {
        &self.data
    }

    /// Mutable access to the register/coil buffer (used by write paths that
    /// stage values before flushing them with `do_modbus_io`).
    pub fn data_mut(&mut self) -> &mut [u16] {
        &mut self.data
    }

    /// Validate a register/coil offset against the configured range. Port of
    /// `checkOffset`.
    pub fn check_offset(&self, offset: i32) -> ModbusResult<()> {
        if offset < 0 {
            return Err(ModbusError::OffsetOutOfRange {
                offset,
                length: self.config.length as i32,
            });
        }
        let ok = if self.config.absolute_addressing() {
            offset <= 65535
        } else {
            (offset as usize) < self.config.length
        };
        if ok {
            Ok(())
        } else {
            Err(ModbusError::OffsetOutOfRange {
                offset,
                length: self.config.length as i32,
            })
        }
    }

    /// Build the bare request PDU (`[slave, fcode, …]`) for one operation.
    ///
    /// `data` supplies the words/bits to write (ignored for read functions);
    /// `len` is the number of words/bits. Port of the request-building switch
    /// in `doModbusIO`.
    pub fn build_request(
        &self,
        function: ModbusFunctionCode,
        start: u16,
        data: &[u16],
        len: usize,
    ) -> ModbusResult<RequestPdu> {
        use ModbusFunctionCode::*;
        let slave = self.config.slave;
        let count = len as u16;
        Ok(match function {
            ReadCoils => RequestPdu::read(slave, FunctionCode::ReadCoils, start, count),
            ReadDiscreteInputs => {
                RequestPdu::read(slave, FunctionCode::ReadDiscreteInputs, start, count)
            }
            ReadHoldingRegisters => {
                RequestPdu::read(slave, FunctionCode::ReadHoldingRegisters, start, count)
            }
            ReadInputRegisters => {
                RequestPdu::read(slave, FunctionCode::ReadInputRegisters, start, count)
            }
            ReportSlaveId => RequestPdu::report_slave_id(slave),
            ReadInputRegistersF23 => RequestPdu::f23_read(slave, start, count, start),
            WriteSingleCoil => {
                let value = if data.first().copied().unwrap_or(0) != 0 {
                    0xFF00
                } else {
                    0x0000
                };
                RequestPdu::write_single(slave, FunctionCode::WriteSingleCoil, start, value)
            }
            WriteSingleRegister => {
                let value = data.first().copied().unwrap_or(0);
                RequestPdu::write_single(slave, FunctionCode::WriteSingleRegister, start, value)
            }
            WriteMultipleCoils => {
                let coils: Vec<bool> = data[..len].iter().map(|&w| w != 0).collect();
                RequestPdu::write_multiple_coils(slave, start, &coils)
            }
            WriteMultipleRegisters => {
                RequestPdu::write_multiple_registers(slave, start, &data[..len])
            }
            WriteMultipleRegistersF23 => {
                RequestPdu::read_write_multiple_registers(slave, start, 1, start, &data[..len])
            }
        })
    }

    /// Parse a response PDU into the words it carries (empty for writes).
    ///
    /// Port of the response-handling switch in `doModbusIO`. For register
    /// reads the C code requires the returned word count to match `len`.
    pub fn parse_response(
        &self,
        function: ModbusFunctionCode,
        len: usize,
        resp: &ResponsePdu,
    ) -> ModbusResult<Vec<u16>> {
        use ModbusFunctionCode::*;
        match function {
            ReadCoils | ReadDiscreteInputs => {
                let payload = resp.read_data()?;
                // We are only told a byte count; unpack `len` bits LSB-first.
                let mut out = Vec::with_capacity(len);
                for i in 0..len {
                    let byte = payload.get(i / 8).copied().unwrap_or(0);
                    out.push(if byte & (1 << (i % 8)) != 0 { 1 } else { 0 });
                }
                Ok(out)
            }
            ReadHoldingRegisters | ReadInputRegisters | ReadInputRegistersF23 => {
                let payload = resp.read_data()?;
                let nread = payload.len() / 2;
                if nread != len {
                    return Err(ModbusError::FrameTooShort {
                        got: nread,
                        need: len,
                    });
                }
                Ok(payload
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect())
            }
            ReportSlaveId => {
                let payload = resp.read_data()?;
                if payload.len() != len {
                    return Err(ModbusError::FrameTooShort {
                        got: payload.len(),
                        need: len,
                    });
                }
                Ok(payload.iter().map(|&b| b as u16).collect())
            }
            WriteSingleCoil
            | WriteSingleRegister
            | WriteMultipleCoils
            | WriteMultipleRegisters
            | WriteMultipleRegistersF23 => Ok(Vec::new()),
        }
    }

    /// Run one full Modbus write/read cycle: build the request, frame it,
    /// transmit, receive, unwrap, and parse. Updates I/O statistics.
    ///
    /// Returns the words read (empty for write functions). Port of
    /// `doModbusIO`.
    pub fn do_modbus_io(
        &mut self,
        transport: &mut dyn OctetTransport,
        function: ModbusFunctionCode,
        start: u16,
        data: &[u16],
        len: usize,
    ) -> ModbusResult<Vec<u16>> {
        let pdu = self.build_request(function, start, data, len)?;
        let framed = self.framer.frame_request(pdu.as_bytes())?;
        let expected_txid = self.framer.last_transaction_id();
        let is_udp = self.framer.link_type() == LinkType::Udp;

        let started = Instant::now();
        let response_pdu = match self.transact(transport, &framed, expected_txid, is_udp) {
            Ok(pdu) => pdu,
            Err(e) => {
                self.stats.io_errors += 1;
                self.stats.current_io_errors += 1;
                return Err(e);
            }
        };
        self.stats.record_timing(started.elapsed());

        // Decode the response PDU, tolerating the "Acknowledge" exception
        // (code 5) — the C code treats it as a non-fatal warning.
        let resp = match ResponsePdu::parse(&response_pdu) {
            Ok(resp) => resp,
            Err(ModbusError::Exception(ExceptionCode::Acknowledge)) => {
                if function.is_read() {
                    self.stats.read_ok += 1;
                } else {
                    self.stats.write_ok += 1;
                }
                self.stats.current_io_errors = 0;
                return Ok(Vec::new());
            }
            Err(e) => {
                self.stats.io_errors += 1;
                self.stats.current_io_errors += 1;
                return Err(e);
            }
        };

        let words = match self.parse_response(function, len, &resp) {
            Ok(words) => words,
            Err(e) => {
                self.stats.io_errors += 1;
                self.stats.current_io_errors += 1;
                return Err(e);
            }
        };
        if function.is_read() {
            self.stats.read_ok += 1;
        } else {
            self.stats.write_ok += 1;
        }
        self.stats.current_io_errors = 0;
        Ok(words)
    }

    /// Transmit `framed` and receive the matching response PDU. For TCP the
    /// read loops until the MBAP transaction ID matches; for UDP a read
    /// failure retransmits up to [`UDP_MAX_RETRIES`] times.
    fn transact(
        &mut self,
        transport: &mut dyn OctetTransport,
        framed: &[u8],
        expected_txid: u16,
        is_udp: bool,
    ) -> ModbusResult<Vec<u8>> {
        transport.write_frame(framed)?;
        let mut udp_retries = 0u32;
        loop {
            match transport.read_frame(READ_TIMEOUT) {
                Ok(raw) => {
                    let unwrapped = self.framer.unwrap_response(&raw)?;
                    match unwrapped.transaction_id {
                        // TCP/UDP: a stale reply from an earlier request —
                        // keep reading without retransmitting.
                        Some(tid) if tid != expected_txid => continue,
                        _ => return Ok(unwrapped.pdu),
                    }
                }
                Err(e) => {
                    if is_udp && udp_retries < UDP_MAX_RETRIES {
                        udp_retries += 1;
                        transport.write_frame(framed)?;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Perform one poll cycle for a read port: read `length` words from the
    /// configured start address into the buffer and report whether any word
    /// changed since the previous poll. Port of the data-acquisition half of
    /// `readPoller` (the interrupt fan-out lives in the `ioc` module).
    pub fn poll(&mut self, transport: &mut dyn OctetTransport) -> ModbusResult<bool> {
        if !self.config.function.is_read() {
            return Err(ModbusError::InvalidFunction(0));
        }
        let start = self.config.start_address.max(0) as u16;
        let len = self.config.length;
        let words = self.do_modbus_io(transport, self.config.function, start, &[], len)?;
        self.data.copy_from_slice(&words);
        let changed = self.data != self.prev_data;
        self.prev_data.copy_from_slice(&self.data);
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock transport: records writes, replays a queue of canned responses.
    struct MockTransport {
        written: Vec<Vec<u8>>,
        responses: std::collections::VecDeque<ModbusResult<Vec<u8>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<ModbusResult<Vec<u8>>>) -> Self {
            Self {
                written: Vec::new(),
                responses: responses.into_iter().collect(),
            }
        }
    }

    impl OctetTransport for MockTransport {
        fn write_frame(&mut self, data: &[u8]) -> ModbusResult<()> {
            self.written.push(data.to_vec());
            Ok(())
        }
        fn read_frame(&mut self, _timeout: Duration) -> ModbusResult<Vec<u8>> {
            self.responses
                .pop_front()
                .unwrap_or(Err(ModbusError::Timeout))
        }
    }

    fn read_config(function: ModbusFunctionCode, length: usize) -> ModbusConfig {
        ModbusConfig {
            slave: 1,
            function,
            start_address: 0,
            length,
            data_type: ModbusDataType::UInt16,
            poll_delay: Duration::from_millis(100),
            plc_type: String::new(),
        }
    }

    /// Build a Modbus/TCP response frame from a bare PDU and a transaction ID.
    fn tcp_response(txid: u16, pdu: &[u8]) -> Vec<u8> {
        let mut frame = crate::protocol::MbapHeader::new(txid, pdu.len() as u16)
            .to_bytes()
            .to_vec();
        frame.extend_from_slice(pdu);
        frame
    }

    #[test]
    fn function_code_decoding() {
        assert_eq!(
            ModbusFunctionCode::from_i32(3),
            Some(ModbusFunctionCode::ReadHoldingRegisters)
        );
        assert_eq!(
            ModbusFunctionCode::from_i32(123),
            Some(ModbusFunctionCode::ReadInputRegistersF23)
        );
        assert_eq!(
            ModbusFunctionCode::from_i32(223),
            Some(ModbusFunctionCode::WriteMultipleRegistersF23)
        );
        assert_eq!(ModbusFunctionCode::from_i32(99), None);
        assert!(ModbusFunctionCode::ReadCoils.is_read());
        assert!(ModbusFunctionCode::WriteSingleCoil.is_write());
        assert_eq!(
            ModbusFunctionCode::ReadCoils.max_length(),
            MAX_READ_WORDS * 16
        );
        assert_eq!(
            ModbusFunctionCode::WriteMultipleRegisters.readonce_function(),
            Some(ModbusFunctionCode::ReadHoldingRegisters)
        );
    }

    #[test]
    fn config_validation_and_addressing() {
        let mut cfg = read_config(ModbusFunctionCode::ReadHoldingRegisters, 10);
        assert!(cfg.validate().is_ok());
        assert!(!cfg.absolute_addressing());
        assert_eq!(cfg.readback_offset(), 0);

        cfg.start_address = -1;
        assert!(cfg.absolute_addressing());
        cfg.plc_type = "Wago750-352".to_string();
        assert_eq!(cfg.readback_offset(), WAGO_OFFSET);

        cfg.length = 0;
        assert!(cfg.validate().is_err());
        cfg.length = MAX_READ_WORDS + 1;
        assert!(matches!(cfg.validate(), Err(ModbusError::FrameTooLarge(_))));
    }

    #[test]
    fn build_read_holding_registers_request() {
        let engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 10),
            LinkType::Tcp,
        )
        .unwrap();
        let pdu = engine
            .build_request(ModbusFunctionCode::ReadHoldingRegisters, 100, &[], 10)
            .unwrap();
        assert_eq!(pdu.as_bytes(), &[0x01, 0x03, 0x00, 0x64, 0x00, 0x0A]);
    }

    #[test]
    fn build_write_single_coil_request() {
        let engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::WriteSingleCoil, 16),
            LinkType::Tcp,
        )
        .unwrap();
        // Non-zero data → 0xFF00 (coil on).
        let on = engine
            .build_request(ModbusFunctionCode::WriteSingleCoil, 5, &[1], 1)
            .unwrap();
        assert_eq!(on.as_bytes(), &[0x01, 0x05, 0x00, 0x05, 0xFF, 0x00]);
        // Zero data → 0x0000 (coil off).
        let off = engine
            .build_request(ModbusFunctionCode::WriteSingleCoil, 5, &[0], 1)
            .unwrap();
        assert_eq!(off.as_bytes(), &[0x01, 0x05, 0x00, 0x05, 0x00, 0x00]);
    }

    #[test]
    fn f23_read_request_has_no_data_field() {
        let engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadInputRegistersF23, 4),
            LinkType::Tcp,
        )
        .unwrap();
        let pdu = engine
            .build_request(ModbusFunctionCode::ReadInputRegistersF23, 0x10, &[], 4)
            .unwrap();
        // slave, 0x17, readStart, readCount, writeStart, numOutput=1, byteCount=2.
        assert_eq!(
            pdu.as_bytes(),
            &[
                0x01, 0x17, 0x00, 0x10, 0x00, 0x04, 0x00, 0x10, 0x00, 0x01, 0x02
            ]
        );
    }

    #[test]
    fn do_modbus_io_read_holding_registers() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 3),
            LinkType::Tcp,
        )
        .unwrap();
        // Response PDU: fcode 0x03, byte_count 6, three registers.
        let resp_pdu = [0x01u8, 0x03, 0x06, 0x00, 0x0A, 0x00, 0x14, 0x00, 0x1E];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);

        let words = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                3,
            )
            .unwrap();
        assert_eq!(words, vec![10, 20, 30]);
        assert_eq!(engine.stats.read_ok, 1);
        assert_eq!(engine.stats.io_errors, 0);
    }

    #[test]
    fn do_modbus_io_read_coils_unpacks_bits() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadCoils, 10),
            LinkType::Tcp,
        )
        .unwrap();
        // 10 coils: on at 0,1,9 → byte0 = 0x03, byte1 = 0x02.
        let resp_pdu = [0x01u8, 0x01, 0x02, 0x03, 0x02];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);

        let bits = engine
            .do_modbus_io(&mut transport, ModbusFunctionCode::ReadCoils, 0, &[], 10)
            .unwrap();
        assert_eq!(bits, vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn do_modbus_io_write_increments_write_ok() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::WriteSingleRegister, 1),
            LinkType::Tcp,
        )
        .unwrap();
        // Write-single response echoes address + value.
        let resp_pdu = [0x01u8, 0x06, 0x00, 0x00, 0xAB, 0xCD];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);

        let words = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::WriteSingleRegister,
                0,
                &[0xABCD],
                1,
            )
            .unwrap();
        assert!(words.is_empty());
        assert_eq!(engine.stats.write_ok, 1);
    }

    #[test]
    fn do_modbus_io_skips_stale_tcp_transaction_id() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Tcp,
        )
        .unwrap();
        let resp_pdu = [0x01u8, 0x03, 0x02, 0x12, 0x34];
        // First reply carries a stale transaction ID (0) — must be skipped;
        // the request's ID is 1.
        let mut transport = MockTransport::new(vec![
            Ok(tcp_response(0, &resp_pdu)),
            Ok(tcp_response(1, &resp_pdu)),
        ]);
        let words = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                1,
            )
            .unwrap();
        assert_eq!(words, vec![0x1234]);
    }

    #[test]
    fn do_modbus_io_modbus_exception_is_error() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Tcp,
        )
        .unwrap();
        // slave 0x01, fcode 0x83 = exception, code 0x02 (illegal data address).
        let resp_pdu = [0x01u8, 0x83, 0x02];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        let err = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                1,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ModbusError::Exception(ExceptionCode::IllegalDataAddress)
        ));
        assert_eq!(engine.stats.io_errors, 1);
    }

    #[test]
    fn do_modbus_io_acknowledge_exception_is_not_fatal() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::WriteSingleRegister, 1),
            LinkType::Tcp,
        )
        .unwrap();
        // slave 0x01, fcode 0x86 = exception, code 0x05 (Acknowledge).
        let resp_pdu = [0x01u8, 0x86, 0x05];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        let words = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::WriteSingleRegister,
                0,
                &[1],
                1,
            )
            .unwrap();
        assert!(words.is_empty());
        assert_eq!(engine.stats.write_ok, 1);
        assert_eq!(engine.stats.io_errors, 0);
    }

    #[test]
    fn do_modbus_io_timeout_counts_io_error() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Tcp,
        )
        .unwrap();
        let mut transport = MockTransport::new(vec![Err(ModbusError::Timeout)]);
        let err = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                1,
            )
            .unwrap_err();
        assert!(matches!(err, ModbusError::Timeout));
        assert_eq!(engine.stats.io_errors, 1);
        assert_eq!(engine.stats.current_io_errors, 1);
    }

    #[test]
    fn poll_detects_change() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 2),
            LinkType::Tcp,
        )
        .unwrap();
        let r1 = [0x01u8, 0x03, 0x04, 0x00, 0x0A, 0x00, 0x14];
        let r2 = [0x01u8, 0x03, 0x04, 0x00, 0x0A, 0x00, 0x14];
        let r3 = [0x01u8, 0x03, 0x04, 0x00, 0x0A, 0x00, 0x63];
        let mut transport = MockTransport::new(vec![
            Ok(tcp_response(1, &r1)),
            Ok(tcp_response(2, &r2)),
            Ok(tcp_response(3, &r3)),
        ]);
        // First poll: change from the zero-initialised buffer.
        assert!(engine.poll(&mut transport).unwrap());
        // Second poll: identical data → no change.
        assert!(!engine.poll(&mut transport).unwrap());
        // Third poll: a register changed.
        assert!(engine.poll(&mut transport).unwrap());
        assert_eq!(engine.data(), &[10, 99]);
    }

    #[test]
    fn check_offset_enforces_range() {
        let engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 10),
            LinkType::Tcp,
        )
        .unwrap();
        assert!(engine.check_offset(0).is_ok());
        assert!(engine.check_offset(9).is_ok());
        assert!(engine.check_offset(10).is_err());
        assert!(engine.check_offset(-1).is_err());
    }

    #[test]
    fn histogram_accumulates_when_enabled() {
        let mut stats = IoStatistics {
            histogram_enabled: true,
            histogram_ms_per_bin: 1,
            ..IoStatistics::default()
        };
        stats.record_timing(Duration::from_millis(3));
        assert_eq!(stats.histogram[3], 1);
        assert_eq!(stats.last_io_msec, 3);
        // Very long times fall into the last bin.
        stats.record_timing(Duration::from_millis(10_000));
        assert_eq!(stats.histogram[HISTOGRAM_LENGTH - 1], 1);
    }
}
