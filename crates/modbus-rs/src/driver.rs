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
use crate::interpose::{LinkType, ModbusFramer, TransactionIdCounter};
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
/// UDP read-failure retransmit threshold. C `modbusInterpose.c:356`
/// retransmits the frame while `++retries < 5` (increment, then compare),
/// so a frame is resent at most `UDP_MAX_RETRIES - 1` (four) times and the
/// fifth consecutive read failure gives up.
const UDP_MAX_RETRIES: u32 = 5;

/// Maximum number of stale (mismatched-transaction-ID) frames `transact`
/// will skip before giving up. A peer that keeps sending mismatched-TXID
/// frames would otherwise loop forever — each `read_frame` succeeds so the
/// per-read timeout never fires.
const MAX_STALE_FRAMES: u32 = 32;

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
    /// The read function C uses to seed a write-function port's register
    /// cache at construction (`drvModbusAsyn.cpp:248/253/257`). `None` for a
    /// read function, which has a poller instead.
    pub fn read_once_function(self) -> Option<Self> {
        match self {
            Self::WriteSingleCoil | Self::WriteMultipleCoils => Some(Self::ReadCoils),
            Self::WriteSingleRegister | Self::WriteMultipleRegisters => {
                Some(Self::ReadHoldingRegisters)
            }
            Self::WriteMultipleRegistersF23 => Some(Self::ReadInputRegistersF23),
            _ => None,
        }
    }

    /// The function code this port puts on the wire. The two driver
    /// pseudo-codes 123 and 223 are not wire codes: both ride on FC 0x17, so
    /// a slave's echo cannot distinguish them from each other.
    pub fn wire_code(self) -> u8 {
        let code = match self {
            Self::ReadCoils => FunctionCode::ReadCoils,
            Self::ReadDiscreteInputs => FunctionCode::ReadDiscreteInputs,
            Self::ReadHoldingRegisters => FunctionCode::ReadHoldingRegisters,
            Self::ReadInputRegisters => FunctionCode::ReadInputRegisters,
            Self::WriteSingleCoil => FunctionCode::WriteSingleCoil,
            Self::WriteSingleRegister => FunctionCode::WriteSingleRegister,
            Self::WriteMultipleCoils => FunctionCode::WriteMultipleCoils,
            Self::WriteMultipleRegisters => FunctionCode::WriteMultipleRegisters,
            Self::ReportSlaveId => FunctionCode::ReportSlaveId,
            Self::ReadInputRegistersF23 | Self::WriteMultipleRegistersF23 => {
                FunctionCode::ReadWriteMultipleRegisters
            }
        };
        code.as_u8()
    }

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

    /// Validate the configuration. Mirrors the constructor's length checks.
    ///
    /// Absolute addressing (`start_address == -1`) is accepted: in that mode
    /// `length` sizes the per-record scratch buffer rather than a polled
    /// block, and every record issues an individual Modbus request to its own
    /// absolute wire address. See [`ModbusEngine`] for the I/O paths.
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

    /// Erase the read-time histogram counts. The single owner of the
    /// "zero the histogram" transition that C performs on an ENABLE_HISTOGRAM
    /// OFF→ON edge (drvModbusAsyn.cpp:636-638) and on a HISTOGRAM_BIN_TIME
    /// change (:799-800), so stale counts are neither carried across a
    /// re-enable nor misattributed when the bin width changes. Only the `ioc`
    /// histogram-control write arms invoke it, so it is gated with them.
    #[cfg(feature = "ioc")]
    pub(crate) fn clear_histogram(&mut self) {
        self.histogram.fill(0);
    }
}

/// What one successful `doModbusIO` cycle delivered.
///
/// C `doModbusIO` writes the response words straight into the caller's `data`
/// buffer and reports only a status, so "success" and "the buffer now holds new
/// words" are separate facts: exception 05 (Acknowledge — *the command was
/// accepted and will take a long time*) is mapped to `asynSuccess` and
/// `goto done` **past** the data copy (drvModbusAsyn.cpp:2231-2237), leaving the
/// buffer exactly as it was. Returning a `Vec` collapses the two, and an empty
/// `Vec` then means both "a write function returns no data" and "the read
/// succeeded but delivered nothing" — which is what let an Acknowledge reach
/// `copy_from_slice` with a length of 0 and panic the port.
///
/// This type keeps them apart: a caller must decide what to do with
/// [`Acknowledged`](Self::Acknowledged) before it can touch any words, and the
/// only correct answer — C's — is to leave its buffer alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModbusIoResponse {
    /// The slave answered normally. For a read function the vector holds exactly
    /// the requested word count (every [`ModbusEngine::parse_response`] read arm
    /// enforces it); for a write function it is empty.
    Data(Vec<u16>),
    /// The slave answered with exception 05 (Acknowledge). C counts this as
    /// success and copies nothing, so the caller's data buffer keeps its
    /// previous contents and its records keep their previous values.
    Acknowledged,
}

/// A byte-stream transport — the underlying `asyn-rs` octet port the framed
/// Modbus messages travel over. One `write_frame` is followed by one or more
/// `read_frame` calls per request.
pub trait OctetTransport: Send + Sync {
    /// Send a fully framed request.
    fn write_frame(&mut self, data: &[u8]) -> ModbusResult<()>;
    /// Re-send an already-transmitted frame after a UDP read failure. C
    /// resends via the raw `pasynOctet->write` (`modbusInterpose.c:358`),
    /// bypassing `writeIt` and therefore its pre-write `writeDelay` pacing
    /// (`:246`). The default mirrors `write_frame`; a transport that paces
    /// writes overrides this to skip the delay so retransmits are not slowed.
    fn resend_frame(&mut self, data: &[u8]) -> ModbusResult<()> {
        self.write_frame(data)
    }
    /// Receive one framed response, waiting up to `timeout`.
    fn read_frame(&mut self, timeout: Duration) -> ModbusResult<Vec<u8>>;

    /// Send a framed request and receive its response as ONE locked,
    /// flush-first exchange — C `asynOctetSyncIO.c:231-276`, which every
    /// `drvModbusAsyn` transaction goes through
    /// (`drvModbusAsyn.cpp:2177-2182` calls `pasynOctetSyncIO->writeRead`).
    ///
    /// The flush is what makes a Modbus link resynchronise: a reply that
    /// arrived after its own transaction timed out is discarded before the
    /// next request goes out, instead of being handed to it. The lock is what
    /// keeps two Modbus ports sharing one octet port from consuming each
    /// other's replies.
    ///
    /// The default is the unlocked, unflushed pair, which is correct only for
    /// a transport that owns its link outright — in-crate test doubles. Any
    /// transport over a shared asyn port MUST override it.
    fn write_read(&mut self, data: &[u8], timeout: Duration) -> ModbusResult<Vec<u8>> {
        self.write_frame(data)?;
        self.read_frame(timeout)
    }
    /// Discard any partially received frame.
    ///
    /// `ModbusEngine::transact` calls this whenever a transaction ends
    /// without a matching reply. Modbus/TCP is strictly request/response, so
    /// bytes still held at that point are the head of a reply nobody will
    /// finish reading, and a stream transport that reassembles frames must
    /// drop them or read the next reply at the wrong offset. The default is a
    /// no-op for the message-oriented links that keep nothing between reads.
    fn reset_stream(&mut self) {}
}

/// The Modbus driver engine: request construction, the write/read cycle,
/// response parsing, and the in-memory register buffer.
///
/// # Relative vs. absolute addressing
///
/// In the default *relative* mode the engine owns a single contiguous
/// [`data`](Self::data) buffer of `config.length` words and [`poll`](Self::poll)
/// refreshes the whole block in one Modbus request from `config.start_address`.
///
/// When `config.start_address == -1` the C `drvModbusAsyn` selects *absolute*
/// addressing: the poller is disabled and every record issues an individual
/// Modbus request to its own absolute wire address with a per-record length
/// (drvModbusAsyn.cpp:204-206, 1121, and the per-accessor `absoluteAddressing_`
/// branches). [`read_absolute`](Self::read_absolute) and
/// [`write_absolute`](Self::write_absolute) are that per-record I/O path; the
/// `data` buffer is then unused for polling and serves only as scratch sizing.
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
    /// Create an engine for `config` on the given physical link, with a
    /// transaction-ID counter of its own (see
    /// [`ModbusFramer::new`](crate::interpose::ModbusFramer::new)).
    pub fn new(config: ModbusConfig, link_type: LinkType) -> ModbusResult<Self> {
        Self::with_transaction_counter(config, link_type, TransactionIdCounter::default())
    }

    /// Create an engine drawing transaction IDs from the octet port's shared
    /// counter — required whenever more than one Modbus port can sit on that
    /// octet port.
    pub fn with_transaction_counter(
        config: ModbusConfig,
        link_type: LinkType,
        counter: TransactionIdCounter,
    ) -> ModbusResult<Self> {
        config.validate()?;
        let len = config.length;
        Ok(Self {
            config,
            framer: ModbusFramer::with_transaction_counter(link_type, counter),
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
    /// `checkOffset` (drvModbusAsyn.cpp:2395-2404).
    ///
    /// In relative mode the offset must be a valid index into the
    /// [`data`](Self::data) buffer, i.e. `0 <= offset < config.length`. In
    /// absolute mode (`config.start_address == -1`) the offset *is* the
    /// absolute Modbus wire address, so the C check instead allows any
    /// `0 <= offset <= 65535` — the addressable 16-bit Modbus range.
    pub fn check_offset(&self, offset: i32) -> ModbusResult<()> {
        let ok = if self.config.absolute_addressing() {
            (0..=0xFFFF).contains(&offset)
        } else {
            offset >= 0 && (offset as usize) < self.config.length
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
        // The slave must echo the function code it was asked for. C leaves the
        // intent as an unimplemented question — `drvModbusAsyn.cpp:2248-2249`
        // reads `/* Make sure the function code in the response is the same as
        // the one in the request? */` and `:2251` then switches on the
        // REQUEST's code regardless — so this is a deliberate deviation.
        // Without it a reply belonging to a different exchange decodes under
        // this request's function: a `WriteMultipleRegisters` acknowledgement
        // read as a 2-register response passes the word-count check and yields
        // two registers of garbage reported as good data.
        let expected = function.wire_code();
        if resp.function != expected {
            return Err(ModbusError::FunctionMismatch {
                requested: expected,
                got: resp.function,
            });
        }
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
                // The C driver derives the word count by integer-dividing the
                // reply byte count by two and requires it to equal the
                // configured length: drvModbusAsyn.cpp doModbusIO,
                // MODBUS_READ_HOLDING_REGISTERS case
                // `nread = readResp->byteCount/2;` at drvModbusAsyn.cpp:2281
                // followed by `if ((int)nread != len)` at drvModbusAsyn.cpp:2284
                // returning asynError on mismatch. An odd byte count is
                // tolerated by C (the trailing byte is dropped by the integer
                // division); match that rather than rejecting frames C accepts.
                let payload = resp.read_data()?;
                let nread = payload.len() / 2;
                if nread != len {
                    return Err(ModbusError::MalformedResponse(format!(
                        "register read word count {nread} does not match expected {len}",
                    )));
                }
                Ok(payload
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect())
            }
            ReportSlaveId => {
                // The C driver requires the reply byte count to exactly equal
                // the configured length: drvModbusAsyn.cpp doModbusIO,
                // MODBUS_REPORT_SLAVE_ID case `if ((int)nread != len)` at
                // drvModbusAsyn.cpp:2306 returns asynError on mismatch. Each
                // byte maps to one output word (`data[i] = pCharIn[i]`).
                let payload = resp.read_data()?;
                if payload.len() != len {
                    return Err(ModbusError::MalformedResponse(format!(
                        "report-slave-id byte count {} does not match expected {len}",
                        payload.len(),
                    )));
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
    /// Returns what the slave delivered — the words read (empty for a write
    /// function), or [`ModbusIoResponse::Acknowledged`] when it answered
    /// exception 05, which is a *success with no data*. Port of `doModbusIO`.
    pub fn do_modbus_io(
        &mut self,
        transport: &mut dyn OctetTransport,
        function: ModbusFunctionCode,
        start: u16,
        data: &[u16],
        len: usize,
    ) -> ModbusResult<ModbusIoResponse> {
        let pdu = self.build_request(function, start, data, len)?;
        let framed = self.framer.frame_request(pdu.as_bytes())?;
        let expected_txid = framed.transaction_id;
        let is_udp = self.framer.link_type() == LinkType::Udp;
        let framed = framed.bytes;

        let started = Instant::now();
        // The transport write/read cycle. C `doModbusIO` increments
        // `IOErrors_`/`currentIOErrors_` ONLY here, gated on the `writeRead`
        // transport status (drvModbusAsyn.cpp:2204-2208) — it is the single
        // I/O-error site. A Modbus exception response or a malformed frame is
        // not a transport failure and must never reach it.
        let response_pdu = match self.transact(transport, &framed, expected_txid, is_udp) {
            Ok(pdu) => pdu,
            Err(e) => {
                self.stats.io_errors += 1;
                self.stats.current_io_errors += 1;
                return Err(e);
            }
        };
        // Transport succeeded: record the cycle time (C updates LastIOTime /
        // MaxIOTime / the histogram on every successful writeRead, before the
        // exception check, drvModbusAsyn.cpp:2211-2225) and clear the
        // consecutive-error counter (C resets `currentIOErrors_` on the
        // error→success transport edge, :2200; an unconditional reset on
        // success yields the same observable value — it stays 0 across further
        // successes — so no `prevIOStatus_` field is needed).
        self.stats.record_timing(started.elapsed());
        self.stats.current_io_errors = 0;

        // Decode the response PDU. A Modbus exception response (`fcode & 0x80`)
        // is not a transport error: C `goto done` past the OK switch without
        // touching readOK_/writeOK_/IOErrors_ (drvModbusAsyn.cpp:2229-2246).
        // Exception 5 ("Acknowledge" — the command will take a long time)
        // returns asynSuccess *with the data buffer untouched*; any other
        // exception returns asynError. Either way no counter moves.
        let resp = match ResponsePdu::parse(&response_pdu) {
            Ok(resp) => resp,
            Err(ModbusError::Exception(ExceptionCode::Acknowledge)) => {
                return Ok(ModbusIoResponse::Acknowledged);
            }
            Err(e) => return Err(e),
        };

        // A real (non-exception) response of this function class arrived. C
        // bumps readOK_/writeOK_ at the TOP of each function case, BEFORE the
        // per-function content validation (`readOK_++` at drvModbusAsyn.cpp:
        // 2254/2278/2300, `writeOK_++` at :2325/2333/2341). A frame whose word
        // count then fails to match still counts as readOK_ and returns
        // asynError with no IOErrors_ bump (:2284-2290/2306-2312) — so the OK
        // counter must move before `parse_response` runs its `nread == len`
        // check.
        if function.is_read() {
            self.stats.read_ok += 1;
        } else {
            self.stats.write_ok += 1;
        }
        self.parse_response(function, len, &resp)
            .map(ModbusIoResponse::Data)
    }

    /// Transmit `framed` and receive the matching response PDU. For TCP the
    /// read loops until the MBAP transaction ID matches; for UDP a read
    /// failure retransmits up to [`UDP_MAX_RETRIES`] times.
    ///
    /// A transaction that ends in error takes the transport's partial frame
    /// with it. Doing it here rather than at each failing site is the point:
    /// the inner loop has a write `?`, a resend `?`, a read error, a malformed
    /// header and two stale-frame ceilings, and every one of them leaves the
    /// same residue, so the single exit is what keeps the next fallible call
    /// someone adds from re-opening it.
    fn transact(
        &mut self,
        transport: &mut dyn OctetTransport,
        framed: &[u8],
        expected_txid: Option<u16>,
        is_udp: bool,
    ) -> ModbusResult<Vec<u8>> {
        let result = self.transact_frame(transport, framed, expected_txid, is_udp);
        if result.is_err() {
            transport.reset_stream();
        }
        result
    }

    /// The body of [`Self::transact`] — every exit is routed through that
    /// function so the partial-frame reset cannot be missed on a new one.
    fn transact_frame(
        &mut self,
        transport: &mut dyn OctetTransport,
        framed: &[u8],
        expected_txid: Option<u16>,
        is_udp: bool,
    ) -> ModbusResult<Vec<u8>> {
        // The request and its reply are ONE locked, flush-first exchange (C
        // `asynOctetSyncIO.c:231-276`). Only the re-reads below — a stale TCP
        // transaction ID, a short frame, a UDP retransmit — go back to the
        // port separately, and each of those is entered only after this port's
        // own reply failed to arrive, so there is nothing of ours left to
        // steal.
        let mut pending = Some(transport.write_read(framed, READ_TIMEOUT));
        let mut udp_retries = 0u32;
        let mut stale_frames = 0u32;
        loop {
            let attempt = match pending.take() {
                Some(first) => first,
                None => transport.read_frame(READ_TIMEOUT),
            };
            match attempt {
                Ok(raw) => {
                    match self.framer.unwrap_response(&raw) {
                        Ok(unwrapped) => match unwrapped.transaction_id {
                            // TCP/UDP: a stale reply from an earlier request —
                            // keep reading without retransmitting. Bound the
                            // skip count so a peer that keeps sending
                            // mismatched-TXID frames cannot trap us in an
                            // unbounded loop (each `read_frame` succeeds, so the
                            // timeout never fires).
                            Some(tid) if Some(tid) != expected_txid => {
                                stale_frames += 1;
                                if stale_frames > MAX_STALE_FRAMES {
                                    return Err(ModbusError::Timeout);
                                }
                                continue;
                            }
                            _ => return Ok(unwrapped.pdu),
                        },
                        // C's TCP/UDP read loop (modbusInterpose.c:366) only
                        // breaks when the reply is >= 2 bytes AND its
                        // transaction ID matches; a reply too short to yield a
                        // matching TXID falls through and re-reads. Mirror that
                        // for the MBAP-framed links: skip a too-short frame
                        // (bounded by the same stale-frame guard) instead of
                        // ending the transaction on a single spurious short
                        // read. RTU/ASCII read once in C (no loop), so their
                        // short-frame / CRC failures still propagate.
                        Err(ModbusError::FrameTooShort { .. })
                            if matches!(self.framer.link_type(), LinkType::Tcp | LinkType::Udp) =>
                        {
                            stale_frames += 1;
                            if stale_frames > MAX_STALE_FRAMES {
                                return Err(ModbusError::Timeout);
                            }
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => {
                    // C parity (modbusInterpose.c:356): a UDP read failure
                    // retransmits the frame while `++retries < 5` — increment
                    // first, then compare — so the frame is resent at most four
                    // times and the fifth consecutive failure gives up. Mirror
                    // the increment-before-compare order exactly; checking the
                    // counter before incrementing would allow one extra resend.
                    if is_udp {
                        udp_retries += 1;
                        if udp_retries < UDP_MAX_RETRIES {
                            // C resends through the raw octet write
                            // (modbusInterpose.c:358), not `writeIt`, so the
                            // pre-write `writeDelay` pacing (:246) is skipped on
                            // a retransmit. `resend_frame` is the no-delay path.
                            transport.resend_frame(framed)?;
                            continue;
                        }
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
    ///
    /// Not valid in absolute-addressing mode: the C `readPoller` thread is
    /// never started for an absolute port (`if (absoluteAddressing_)
    /// needReadThread = 0;`, drvModbusAsyn.cpp:267). Use
    /// [`read_absolute`](Self::read_absolute) for per-record I/O instead.
    pub fn poll(&mut self, transport: &mut dyn OctetTransport) -> ModbusResult<bool> {
        if self.config.absolute_addressing() {
            return Err(ModbusError::PollNotValidInAbsoluteMode);
        }
        if !self.config.function.is_read() {
            return Err(ModbusError::InvalidFunction(0));
        }
        let start = self.config.start_address.max(0) as u16;
        let len = self.config.length;
        // `len == self.data.len()` (both are `config.length`, which
        // `ModbusConfig::validate` requires to be non-zero) and every read arm of
        // `parse_response` returns exactly `len` words, so the copy cannot
        // mismatch. An Acknowledge (exception 05) delivers no words at all: C
        // skips the copy and leaves the buffer holding the previous poll's data
        // (drvModbusAsyn.cpp:2231-2237), so the callbacks fan the previous values
        // out unchanged — `changed` then falls out as false on its own.
        if let ModbusIoResponse::Data(words) =
            self.do_modbus_io(transport, self.config.function, start, &[], len)?
        {
            self.data.copy_from_slice(&words);
        }
        let changed = self.data != self.prev_data;
        self.prev_data.copy_from_slice(&self.data);
        Ok(changed)
    }

    /// Issue one individual Modbus read at the absolute wire address `addr`
    /// and return the `count` words read.
    ///
    /// This is the absolute-addressing read path: where relative-mode records
    /// index the polled [`data`](Self::data) buffer, an absolute-mode record
    /// reads its own wire address on every access. Port of the
    /// `absoluteAddressing_` branch shared by `readInt32` / `readInt64` /
    /// `readFloat64` / `readUInt32Digital` / `readOctet` / `readInt32Array` /
    /// `readFloat64Array` (e.g. drvModbusAsyn.cpp:672-679): each does
    /// `doModbusIO(slave, modbusFunction, offset, data_, len)` with `offset`
    /// the absolute address.
    ///
    /// `function` is resolved by the caller (`ioc` layer) the way C
    /// `checkModbusFunction` does — the read function for a read port, or the
    /// write port's `readOnceFunction`. `count` is the per-record length,
    /// clamped to `config.length` so it can never exceed the scratch buffer
    /// or the protocol read limit established by [`ModbusConfig::validate`].
    ///
    /// The words land in the engine's [`data`](Self::data) buffer before they are
    /// returned, because that is C's receive buffer here too (`doModbusIO(...,
    /// offset, data_, len)` followed by a decode of `data_[0]`). It is what makes
    /// exception 05 behave as C does: the copy is skipped, so the record is
    /// served the buffer's previous contents with a success status rather than an
    /// empty read.
    pub fn read_absolute(
        &mut self,
        transport: &mut dyn OctetTransport,
        function: ModbusFunctionCode,
        addr: i32,
        count: usize,
    ) -> ModbusResult<Vec<u16>> {
        if !function.is_read() {
            return Err(ModbusError::InvalidFunction(0));
        }
        self.check_offset(addr)?;
        // `config.length >= 1` (validate), so `len <= data.len()` and both slices
        // below are in range.
        let len = count.min(self.config.length).max(1);
        if let ModbusIoResponse::Data(words) =
            self.do_modbus_io(transport, function, addr as u16, &[], len)?
        {
            self.data[..len].copy_from_slice(&words);
        }
        Ok(self.data[..len].to_vec())
    }

    /// Seed the register cache with one read at the wire address `addr`, the
    /// `readOnce` C performs in its constructor for a write-function port
    /// (`drvModbusAsyn.cpp:311-317`).
    ///
    /// Unlike [`Self::read_absolute`] this does not bound `addr` against the
    /// port's own length: C passes `modbusStartAddress_ + readbackOffset_`
    /// straight to `doModbusIO`, and the Wago readback offset deliberately
    /// lands outside the written block.
    pub fn read_once(
        &mut self,
        transport: &mut dyn OctetTransport,
        function: ModbusFunctionCode,
        addr: u16,
    ) -> ModbusResult<()> {
        let len = self.config.length;
        if let ModbusIoResponse::Data(words) =
            self.do_modbus_io(transport, function, addr, &[], len)?
        {
            let n = words.len().min(self.data.len());
            self.data[..n].copy_from_slice(&words[..n]);
        }
        Ok(())
    }

    /// Issue one individual Modbus write of `data` at the absolute wire
    /// address `addr`.
    ///
    /// The absolute-addressing write path. Port of the `absoluteAddressing_`
    /// branch shared by `writeInt32` / `writeInt64` / `writeFloat64` /
    /// `writeUInt32Digital` / `writeOctet` / `writeInt32Array` /
    /// `writeFloat64Array` (e.g. drvModbusAsyn.cpp:751-753): each sets
    /// `modbusAddress = offset` and calls `doModbusIO` at that absolute
    /// address. The configured write `function` is used directly.
    pub fn write_absolute(
        &mut self,
        transport: &mut dyn OctetTransport,
        function: ModbusFunctionCode,
        addr: i32,
        data: &[u16],
    ) -> ModbusResult<()> {
        if !function.is_write() {
            return Err(ModbusError::InvalidFunction(0));
        }
        self.check_offset(addr)?;
        self.do_modbus_io(transport, function, addr as u16, data, data.len())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unwrap the data words of an I/O the test expects to be a normal
    /// response. An `Acknowledged` here would be a silent behaviour change, so
    /// it panics rather than degrading to an empty slice — the collapse this
    /// type exists to prevent.
    fn words_of(response: ModbusIoResponse) -> Vec<u16> {
        match response {
            ModbusIoResponse::Data(words) => words,
            ModbusIoResponse::Acknowledged => panic!("expected data, got exception-05 Acknowledge"),
        }
    }

    /// A mock transport: records initial writes and retransmits separately,
    /// and replays a queue of canned responses.
    struct MockTransport {
        written: Vec<Vec<u8>>,
        resent: Vec<Vec<u8>>,
        responses: std::collections::VecDeque<ModbusResult<Vec<u8>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<ModbusResult<Vec<u8>>>) -> Self {
            Self {
                written: Vec::new(),
                resent: Vec::new(),
                responses: responses.into_iter().collect(),
            }
        }
    }

    impl OctetTransport for MockTransport {
        fn write_frame(&mut self, data: &[u8]) -> ModbusResult<()> {
            self.written.push(data.to_vec());
            Ok(())
        }
        fn resend_frame(&mut self, data: &[u8]) -> ModbusResult<()> {
            self.resent.push(data.to_vec());
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
    fn absolute_addressing_config_is_accepted() {
        // Absolute addressing (start_address == -1) is a supported mode: the
        // config validates and the engine builds. `length` then sizes the
        // per-record scratch buffer rather than a polled block.
        let mut cfg = read_config(ModbusFunctionCode::ReadHoldingRegisters, 10);
        cfg.start_address = -1;
        assert!(cfg.absolute_addressing());
        assert!(cfg.validate().is_ok());
        let engine = ModbusEngine::new(cfg, LinkType::Tcp).expect("absolute config must build");
        assert!(engine.config().absolute_addressing());
    }

    #[test]
    fn check_offset_rejects_beyond_buffer_length_in_relative_mode() {
        let engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 4),
            LinkType::Tcp,
        )
        .unwrap();
        // Relative mode: an offset past the contiguous buffer must be a clean
        // error, never a panic source for the ioc-layer accessors.
        assert!(engine.check_offset(4).is_err());
        assert!(engine.check_offset(70_000).is_err());
        assert_eq!(engine.data().len(), 4);
    }

    #[test]
    fn check_offset_absolute_allows_full_wire_range() {
        // Absolute mode: `checkOffset` allows any 0..=65535 wire address
        // regardless of the scratch buffer length (drvModbusAsyn.cpp:2398).
        let mut cfg = read_config(ModbusFunctionCode::ReadHoldingRegisters, 4);
        cfg.start_address = -1;
        let engine = ModbusEngine::new(cfg, LinkType::Tcp).unwrap();
        assert!(engine.check_offset(0).is_ok());
        assert!(engine.check_offset(4).is_ok());
        assert!(engine.check_offset(40_000).is_ok());
        assert!(engine.check_offset(65_535).is_ok());
        // Outside the 16-bit wire range, and negative, still rejected.
        assert!(engine.check_offset(65_536).is_err());
        assert!(engine.check_offset(-1).is_err());
    }

    #[test]
    fn read_absolute_issues_request_at_wire_address() {
        // Absolute mode read: one individual Modbus request at the wire
        // address, decoding `count` words from the response.
        let mut cfg = read_config(ModbusFunctionCode::ReadHoldingRegisters, 4);
        cfg.start_address = -1;
        let mut engine = ModbusEngine::new(cfg, LinkType::Tcp).unwrap();
        // Response PDU: fc 0x03, byte_count 4, two registers 0x1111 0x2222.
        let resp_pdu = [0x01u8, 0x03, 0x04, 0x11, 0x11, 0x22, 0x22];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        let words = engine
            .read_absolute(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0x4000,
                2,
            )
            .unwrap();
        assert_eq!(words, vec![0x1111, 0x2222]);
        // The request must address the absolute wire address 0x4000, not 0.
        // The framed TCP message is the 6-byte MBAP header followed by the
        // bare PDU `[slave, fcode, addr_hi, addr_lo, …]`.
        assert_eq!(
            transport.written[0][6..10],
            [0x01, 0x03, 0x40, 0x00],
            "absolute read must target the wire address"
        );
        assert_eq!(engine.stats.read_ok, 1);
    }

    #[test]
    fn write_absolute_issues_request_at_wire_address() {
        // Absolute mode write: one individual Modbus request at the wire
        // address carrying the record's data.
        let mut cfg = read_config(ModbusFunctionCode::WriteSingleRegister, 4);
        cfg.start_address = -1;
        let mut engine = ModbusEngine::new(cfg, LinkType::Tcp).unwrap();
        // Write-single response echoes the address + value.
        let resp_pdu = [0x01u8, 0x06, 0x12, 0x34, 0xAB, 0xCD];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        engine
            .write_absolute(
                &mut transport,
                ModbusFunctionCode::WriteSingleRegister,
                0x1234,
                &[0xABCD],
            )
            .unwrap();
        // 6-byte MBAP header, then the bare write PDU.
        assert_eq!(
            transport.written[0][6..12],
            [0x01, 0x06, 0x12, 0x34, 0xAB, 0xCD],
            "absolute write must target the wire address"
        );
        assert_eq!(engine.stats.write_ok, 1);
    }

    #[test]
    fn poll_rejected_in_absolute_mode() {
        // The C readPoller thread is never started for an absolute port
        // (drvModbusAsyn.cpp:267); `poll` must refuse rather than read a
        // block that has no meaning in absolute mode.
        let mut cfg = read_config(ModbusFunctionCode::ReadHoldingRegisters, 4);
        cfg.start_address = -1;
        let mut engine = ModbusEngine::new(cfg, LinkType::Tcp).unwrap();
        let mut transport = MockTransport::new(vec![]);
        assert!(matches!(
            engine.poll(&mut transport),
            Err(ModbusError::PollNotValidInAbsoluteMode)
        ));
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

        let words = words_of(
            engine
                .do_modbus_io(
                    &mut transport,
                    ModbusFunctionCode::ReadHoldingRegisters,
                    0,
                    &[],
                    3,
                )
                .unwrap(),
        );
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

        let bits = words_of(
            engine
                .do_modbus_io(&mut transport, ModbusFunctionCode::ReadCoils, 0, &[], 10)
                .unwrap(),
        );
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

        let words = words_of(
            engine
                .do_modbus_io(
                    &mut transport,
                    ModbusFunctionCode::WriteSingleRegister,
                    0,
                    &[0xABCD],
                    1,
                )
                .unwrap(),
        );
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
        let words = words_of(
            engine
                .do_modbus_io(
                    &mut transport,
                    ModbusFunctionCode::ReadHoldingRegisters,
                    0,
                    &[],
                    1,
                )
                .unwrap(),
        );
        assert_eq!(words, vec![0x1234]);
    }

    /// R1: a spurious too-short TCP reply (one that cannot yield a matching
    /// transaction ID) is skipped and the loop re-reads, mirroring C's
    /// `if (nbytesActual >= 2)` fall-through (modbusInterpose.c:366). A single
    /// short read must not end the transaction.
    #[test]
    fn do_modbus_io_skips_too_short_tcp_frame() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Tcp,
        )
        .unwrap();
        let resp_pdu = [0x01u8, 0x03, 0x02, 0x12, 0x34];
        // First reply is a spurious short frame (below the MBAP header); the
        // second is the real, txid-matching reply.
        let mut transport = MockTransport::new(vec![
            Ok(vec![0xAA, 0xBB, 0xCC]),
            Ok(tcp_response(1, &resp_pdu)),
        ]);
        let words = words_of(
            engine
                .do_modbus_io(
                    &mut transport,
                    ModbusFunctionCode::ReadHoldingRegisters,
                    0,
                    &[],
                    1,
                )
                .unwrap(),
        );
        assert_eq!(words, vec![0x1234]);
    }

    /// R1 boundary: RTU reads exactly once in C (no MBAP loop), so a too-short
    /// RTU frame is a hard error, not a re-read. If it wrongly re-read, the
    /// queue would drain to the `Timeout` fallback; assert it surfaces the
    /// `FrameTooShort` instead.
    #[test]
    fn do_modbus_io_rtu_short_frame_errors_without_reread() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Rtu,
        )
        .unwrap();
        let mut transport = MockTransport::new(vec![Ok(vec![0x01, 0x03])]);
        let err = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                1,
            )
            .unwrap_err();
        assert!(
            matches!(err, ModbusError::FrameTooShort { .. }),
            "RTU short frame must error, not re-read into a timeout: {err:?}"
        );
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
        // C parity (drvModbusAsyn.cpp:2239-2246): a non-Acknowledge Modbus
        // exception sets asynError and `goto done` past the OK switch. It is
        // NOT a transport `writeRead` failure (the only IOErrors_ site,
        // :2204-2208), so neither IOErrors_ nor readOK_ moves.
        assert_eq!(engine.stats.io_errors, 0);
        assert_eq!(engine.stats.read_ok, 0);
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
        let response = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::WriteSingleRegister,
                0,
                &[1],
                1,
            )
            .unwrap();
        // R8-70: exception 05 is success *with no data delivered*, a distinct
        // outcome from "a write returns no words" — the caller must leave its
        // buffer alone rather than copy an empty slice over it.
        assert_eq!(response, ModbusIoResponse::Acknowledged);
        // C parity (drvModbusAsyn.cpp:2231-2238/2245): exception 5 sets
        // asynSuccess and `goto done` past the writeOK_/readOK_ switch — no OK
        // counter moves, and it is not a transport error so IOErrors_ stays 0.
        assert_eq!(engine.stats.write_ok, 0);
        assert_eq!(engine.stats.io_errors, 0);
    }

    /// R8-70: a *read* poll answered with exception 05 (Acknowledge) delivered an
    /// empty word vector into `data.copy_from_slice(&words)` — a length mismatch,
    /// which panics and takes the port's poller thread with it. C copies nothing
    /// and reports success (drvModbusAsyn.cpp:2231-2237), leaving the register
    /// buffer on the previous poll's data; the next callbacks fan those same
    /// values out.
    #[test]
    fn poll_acknowledge_exception_keeps_the_previous_register_block() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 4),
            LinkType::Tcp,
        )
        .unwrap();
        // Poll 1: a normal 4-register response. Poll 2: exception 05.
        let good = [
            0x01u8, 0x03, 0x08, 0x00, 0x0A, 0x00, 0x14, 0x00, 0x1E, 0x00, 0x28,
        ];
        let ack = [0x01u8, 0x83, 0x05];
        let mut transport =
            MockTransport::new(vec![Ok(tcp_response(1, &good)), Ok(tcp_response(2, &ack))]);

        assert!(
            engine.poll(&mut transport).unwrap(),
            "first poll changes data"
        );
        assert_eq!(engine.data(), &[10, 20, 30, 40]);

        // Must not panic, must not zero the buffer, and reports no change.
        let changed = engine
            .poll(&mut transport)
            .expect("exception 05 is a success, not an error");
        assert!(
            !changed,
            "an Acknowledge copies nothing, so nothing changed"
        );
        assert_eq!(
            engine.data(),
            &[10, 20, 30, 40],
            "the register block keeps the previous poll's data"
        );
        // Not a transport error and not a completed read: no counter moves.
        assert_eq!(engine.stats.io_errors, 0);
        assert_eq!(engine.stats.read_ok, 1);
    }

    /// R8-70 sibling: the absolute-mode per-record read is the same
    /// `doModbusIO`-into-a-buffer shape (C passes `data_` as the destination,
    /// drvModbusAsyn.cpp:675, and decodes `data_[0]`). An Acknowledge leaves that
    /// buffer alone, so the record is served the previous contents — not an empty
    /// read that decodes to nothing.
    #[test]
    fn read_absolute_acknowledge_exception_serves_the_previous_buffer() {
        let mut cfg = read_config(ModbusFunctionCode::ReadHoldingRegisters, 4);
        cfg.start_address = -1;
        let mut engine = ModbusEngine::new(cfg, LinkType::Tcp).unwrap();
        let good = [0x01u8, 0x03, 0x04, 0x11, 0x11, 0x22, 0x22];
        let ack = [0x01u8, 0x83, 0x05];
        let mut transport =
            MockTransport::new(vec![Ok(tcp_response(1, &good)), Ok(tcp_response(2, &ack))]);

        let words = engine
            .read_absolute(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0x4000,
                2,
            )
            .expect("first absolute read");
        assert_eq!(words, vec![0x1111, 0x2222]);

        let words = engine
            .read_absolute(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0x4000,
                2,
            )
            .expect("exception 05 is a success, not an error");
        assert_eq!(
            words,
            vec![0x1111, 0x2222],
            "an Acknowledge leaves the receive buffer on the previous read's words"
        );
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
    fn register_read_matches_c_odd_byte_count_truncation() {
        // C parity: drvModbusAsyn.cpp:2281 computes `nread = byteCount/2` with
        // integer division, so an odd byte count of 3 with len 1 yields
        // `nread == 1 == len` and C ACCEPTS the frame (drvModbusAsyn.cpp:2284),
        // copying one register and dropping the trailing byte. The Rust engine
        // must not reject a frame C accepts.
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Tcp,
        )
        .unwrap();
        // fcode 0x03, byte_count 3 (odd), three data bytes. 3/2 == 1 == len.
        let resp_pdu = [0x01u8, 0x03, 0x03, 0x12, 0x34, 0x56];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        let words = words_of(
            engine
                .do_modbus_io(
                    &mut transport,
                    ModbusFunctionCode::ReadHoldingRegisters,
                    0,
                    &[],
                    1,
                )
                .unwrap(),
        );
        // One register decoded from the first two bytes; trailing 0x56 dropped.
        assert_eq!(words, vec![0x1234]);
        assert_eq!(engine.stats.io_errors, 0);
    }

    #[test]
    fn register_read_rejects_byte_count_mismatch() {
        // BUG 1 regression: an even byte count that does not equal len * 2
        // must also be rejected.
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 3),
            LinkType::Tcp,
        )
        .unwrap();
        // byte_count 4 (two registers) but three registers were requested.
        let resp_pdu = [0x01u8, 0x03, 0x04, 0x00, 0x0A, 0x00, 0x14];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        let err = engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                3,
            )
            .unwrap_err();
        assert!(matches!(err, ModbusError::MalformedResponse(_)));
        // C parity (drvModbusAsyn.cpp:2278-2290): readOK_++ runs before the
        // register `nread != len` check, so the mismatch frame counts as
        // readOK_ and returns asynError with no IOErrors_ bump.
        assert_eq!(engine.stats.read_ok, 1);
        assert_eq!(engine.stats.io_errors, 0);
    }

    #[test]
    fn report_slave_id_requires_exact_length_match() {
        // C parity: drvModbusAsyn.cpp:2306 `if ((int)nread != len)` returns
        // asynError when the report-slave-id reply byte count differs from the
        // configured length. A reply of 5 bytes with len 5 is accepted and
        // each byte becomes one word.
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReportSlaveId, 5),
            LinkType::Tcp,
        )
        .unwrap();
        // slave 0x01, fcode 0x11, byte_count 5, five identification bytes.
        let resp_pdu = [0x01u8, 0x11, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        let words = words_of(
            engine
                .do_modbus_io(&mut transport, ModbusFunctionCode::ReportSlaveId, 0, &[], 5)
                .unwrap(),
        );
        assert_eq!(words, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn report_slave_id_rejects_length_mismatch() {
        // C parity: drvModbusAsyn.cpp:2306 returns asynError when the reply
        // byte count does not equal the configured length. Commit cff0152d
        // removed this check on the false premise that C "copies whatever the
        // slave returns" — C does not; it enforces `nread == len`.
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReportSlaveId, 1),
            LinkType::Tcp,
        )
        .unwrap();
        // byte_count 5 but the port is configured for len 1.
        let resp_pdu = [0x01u8, 0x11, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let mut transport = MockTransport::new(vec![Ok(tcp_response(1, &resp_pdu))]);
        let err = engine
            .do_modbus_io(&mut transport, ModbusFunctionCode::ReportSlaveId, 0, &[], 1)
            .unwrap_err();
        assert!(matches!(err, ModbusError::MalformedResponse(_)));
        // C parity (drvModbusAsyn.cpp:2300-2312): readOK_++ runs at the top of
        // the REPORT_SLAVE_ID case BEFORE the `nread != len` check, so a
        // count-mismatch frame still counts as readOK_ then returns asynError
        // via `goto done` — with no IOErrors_ bump (that is transport-only).
        assert_eq!(engine.stats.read_ok, 1);
        assert_eq!(engine.stats.io_errors, 0);
    }

    /// Finding 1, second half: a reply must carry the function code it was
    /// asked for. C intends this and never does it — `drvModbusAsyn.cpp`
    /// carries the comment `/* Make sure the function code in the response is
    /// the same as the one in the request? */` and then switches on the
    /// request's code — so this is a deliberate deviation (CBUG-B28). A
    /// `WriteMultipleRegisters` acknowledgement for address 0x0400 decodes
    /// under a two-register read as byte count 4, passing the word-count
    /// check and yielding two garbage registers reported as good data.
    #[test]
    fn a_reply_echoing_another_function_code_is_rejected() {
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 2),
            LinkType::Rtu,
        )
        .unwrap();
        let mut transport = OneFrame {
            frame: rtu_frame(&[0x01, 0x10, 0x04, 0x00, 0x00, 0x02]),
        };
        assert!(matches!(
            engine.poll(&mut transport),
            Err(ModbusError::FunctionMismatch {
                requested: 0x03,
                got: 0x10
            })
        ));

        // The matching echo still decodes, including the F23 pseudo-code whose
        // wire code is 0x17 rather than its own 123.
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Rtu,
        )
        .unwrap();
        let mut transport = OneFrame {
            frame: rtu_frame(&[0x01, 0x03, 0x02, 0x12, 0x34]),
        };
        engine
            .poll(&mut transport)
            .expect("a matching echo decodes");
        assert_eq!(engine.data(), [0x1234]);

        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadInputRegistersF23, 1),
            LinkType::Rtu,
        )
        .unwrap();
        let mut transport = OneFrame {
            frame: rtu_frame(&[0x01, 0x17, 0x02, 0x56, 0x78]),
        };
        engine
            .poll(&mut transport)
            .expect("FC 0x17 is the F23 pseudo-code's wire echo");
        assert_eq!(engine.data(), [0x5678]);
    }

    /// A transport that answers every exchange with the same canned frame.
    struct OneFrame {
        frame: Vec<u8>,
    }

    impl OctetTransport for OneFrame {
        fn write_frame(&mut self, _data: &[u8]) -> ModbusResult<()> {
            Ok(())
        }
        fn read_frame(&mut self, _timeout: Duration) -> ModbusResult<Vec<u8>> {
            Ok(self.frame.clone())
        }
    }

    /// Append the RTU CRC-16 (low byte first) to a bare PDU.
    fn rtu_frame(pdu: &[u8]) -> Vec<u8> {
        let crc = crate::interpose::compute_crc(pdu);
        let mut frame = pdu.to_vec();
        frame.push((crc & 0xFF) as u8);
        frame.push((crc >> 8) as u8);
        frame
    }

    #[test]
    fn transact_bounds_stale_tcp_frame_loop() {
        // BUG 2 regression: a peer that keeps sending mismatched-TXID frames
        // must not trap `transact` in an unbounded loop. Each `read_frame`
        // succeeds so the per-read timeout never fires.
        struct StaleFlood {
            pdu: Vec<u8>,
        }
        impl OctetTransport for StaleFlood {
            fn write_frame(&mut self, _data: &[u8]) -> ModbusResult<()> {
                Ok(())
            }
            fn read_frame(&mut self, _timeout: Duration) -> ModbusResult<Vec<u8>> {
                // Always a stale transaction ID (0); the request's ID is 1.
                let mut frame = crate::protocol::MbapHeader::new(0, self.pdu.len() as u16)
                    .to_bytes()
                    .to_vec();
                frame.extend_from_slice(&self.pdu);
                Ok(frame)
            }
        }
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Tcp,
        )
        .unwrap();
        let mut transport = StaleFlood {
            pdu: vec![0x01, 0x03, 0x02, 0x12, 0x34],
        };
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
    }

    #[test]
    fn udp_retransmits_at_most_four_times_then_gives_up() {
        // C parity (modbusInterpose.c:356): a UDP read failure retransmits
        // while `++retries < 5`, i.e. four resends, and the fifth consecutive
        // failure returns the error. Five read failures must therefore give up
        // *before* a sixth read could observe success. Boundary: the first
        // write is the initial send, then exactly four retransmits = five
        // writes total, and the queued success frame is never consumed.
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Udp,
        )
        .unwrap();
        // Five read failures, then a valid reply that the C-parity loop must
        // never reach (it gives up on the fifth failure).
        let valid = tcp_response(1, &[0x01, 0x03, 0x02, 0x12, 0x34]);
        let mut transport = MockTransport::new(vec![
            Err(ModbusError::Timeout),
            Err(ModbusError::Timeout),
            Err(ModbusError::Timeout),
            Err(ModbusError::Timeout),
            Err(ModbusError::Timeout),
            Ok(valid),
        ]);
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
        // 1 initial send via `write_frame` + 4 retransmits via `resend_frame`;
        // the success frame stays queued.
        assert_eq!(transport.written.len(), 1);
        assert_eq!(transport.resent.len(), 4);
        assert_eq!(transport.responses.len(), 1);
        assert_eq!(engine.stats.io_errors, 1);
    }

    #[test]
    fn udp_retransmit_resends_via_no_delay_path() {
        // R53/3a regression: C applies `writeDelay` only in `writeIt`
        // (modbusInterpose.c:246); the UDP read-failure retransmit resends
        // through the raw `pasynOctet->write` (:358), bypassing the delay.
        // `transact` must therefore issue retransmits via `resend_frame`
        // (the no-delay path), not `write_frame`, and resend the exact same
        // framed bytes as the initial send.
        let mut engine = ModbusEngine::new(
            read_config(ModbusFunctionCode::ReadHoldingRegisters, 1),
            LinkType::Udp,
        )
        .unwrap();
        let valid = tcp_response(1, &[0x01, 0x03, 0x02, 0x12, 0x34]);
        // One read failure forces a single retransmit, then a valid reply.
        let mut transport = MockTransport::new(vec![Err(ModbusError::Timeout), Ok(valid)]);
        engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                1,
            )
            .unwrap();
        assert_eq!(transport.written.len(), 1, "initial send via write_frame");
        assert_eq!(transport.resent.len(), 1, "retransmit via resend_frame");
        // The retransmit must resend the identical framed request.
        assert_eq!(transport.resent[0], transport.written[0]);
        assert_eq!(engine.stats.read_ok, 1);
        assert_eq!(engine.stats.io_errors, 0);
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
