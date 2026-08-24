//! Modbus protocol primitives — function codes, the MBAP header, and the
//! request/response PDUs.
//!
//! Port of `modbus.h`. The C structures are `#pragma pack(1)` packed; here
//! they are serialised/parsed explicitly so layout is endianness-correct on
//! every target. All multi-byte protocol fields are big-endian ("network
//! order"), matching the Modbus specification.

use crate::error::{ExceptionCode, ModbusError, ModbusResult};

/// Buffer size for input and output packets. 513 (max for ASCII serial)
/// would be enough; the C driver uses 600 to be safe.
pub const MAX_MODBUS_FRAME_SIZE: usize = 600;

/// Bit OR-ed into a function code in an exception response.
pub const MODBUS_EXCEPTION_FCN: u8 = 0x80;

/// Size of the Modbus/TCP MBAP header in bytes.
pub const MBAP_HEADER_SIZE: usize = 6;

/// Protocol identifier carried in the MBAP header — always 0 for Modbus.
pub const MODBUS_PROTOCOL_ID: u16 = 0;

/// Smallest `cmd_length` an MBAP header may declare: the unit identifier plus
/// the function code.
pub const MBAP_MIN_CMD_LENGTH: usize = 2;

/// Modbus function codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FunctionCode {
    /// 0x01 — read 1..2000 coils (discrete outputs).
    ReadCoils = 0x01,
    /// 0x02 — read 1..2000 discrete inputs.
    ReadDiscreteInputs = 0x02,
    /// 0x03 — read 1..125 holding registers.
    ReadHoldingRegisters = 0x03,
    /// 0x04 — read 1..125 input registers.
    ReadInputRegisters = 0x04,
    /// 0x05 — write a single coil.
    WriteSingleCoil = 0x05,
    /// 0x06 — write a single holding register.
    WriteSingleRegister = 0x06,
    /// 0x0F — write 1..1968 coils.
    WriteMultipleCoils = 0x0F,
    /// 0x10 — write 1..123 holding registers.
    WriteMultipleRegisters = 0x10,
    /// 0x11 — report slave ID.
    ReportSlaveId = 0x11,
    /// 0x17 — combined read/write of multiple registers.
    ReadWriteMultipleRegisters = 0x17,
}

impl FunctionCode {
    /// Decode a raw function-code byte (with any exception bit already
    /// stripped). Returns `None` for codes this driver does not implement.
    pub fn from_u8(code: u8) -> Option<Self> {
        Some(match code {
            0x01 => Self::ReadCoils,
            0x02 => Self::ReadDiscreteInputs,
            0x03 => Self::ReadHoldingRegisters,
            0x04 => Self::ReadInputRegisters,
            0x05 => Self::WriteSingleCoil,
            0x06 => Self::WriteSingleRegister,
            0x0F => Self::WriteMultipleCoils,
            0x10 => Self::WriteMultipleRegisters,
            0x11 => Self::ReportSlaveId,
            0x17 => Self::ReadWriteMultipleRegisters,
            _ => return None,
        })
    }

    /// The raw function-code byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether this function reads bit-addressed data (coils / discrete
    /// inputs) as opposed to 16-bit registers.
    pub fn is_bit_function(self) -> bool {
        matches!(
            self,
            Self::ReadCoils
                | Self::ReadDiscreteInputs
                | Self::WriteSingleCoil
                | Self::WriteMultipleCoils
        )
    }

    /// Whether this function reads data from the slave.
    pub fn is_read(self) -> bool {
        matches!(
            self,
            Self::ReadCoils
                | Self::ReadDiscreteInputs
                | Self::ReadHoldingRegisters
                | Self::ReadInputRegisters
                | Self::ReportSlaveId
                | Self::ReadWriteMultipleRegisters
        )
    }

    /// Whether this function writes data to the slave.
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Self::WriteSingleCoil
                | Self::WriteSingleRegister
                | Self::WriteMultipleCoils
                | Self::WriteMultipleRegisters
                | Self::ReadWriteMultipleRegisters
        )
    }
}

/// The Modbus/TCP application protocol (MBAP) header.
///
/// Mirrors `modbusMBAPHeader` from `modbus.h`. Serialised big-endian; the
/// `protocolType` is always 0 for Modbus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbapHeader {
    /// Transaction identifier, echoed by the slave so replies can be matched.
    pub transaction_id: u16,
    /// Protocol identifier — always 0 for Modbus.
    pub protocol_type: u16,
    /// Number of following bytes (the PDU length).
    pub cmd_length: u16,
}

impl MbapHeader {
    /// Build an MBAP header for a PDU of `cmd_length` bytes.
    pub fn new(transaction_id: u16, cmd_length: u16) -> Self {
        Self {
            transaction_id,
            protocol_type: 0,
            cmd_length,
        }
    }

    /// Serialise the 6-byte header (big-endian).
    pub fn to_bytes(self) -> [u8; MBAP_HEADER_SIZE] {
        let mut b = [0u8; MBAP_HEADER_SIZE];
        b[0..2].copy_from_slice(&self.transaction_id.to_be_bytes());
        b[2..4].copy_from_slice(&self.protocol_type.to_be_bytes());
        b[4..6].copy_from_slice(&self.cmd_length.to_be_bytes());
        b
    }

    /// Parse a 6-byte header (big-endian).
    pub fn from_bytes(b: &[u8]) -> ModbusResult<Self> {
        if b.len() < MBAP_HEADER_SIZE {
            return Err(ModbusError::FrameTooShort {
                got: b.len(),
                need: MBAP_HEADER_SIZE,
            });
        }
        Ok(Self {
            transaction_id: u16::from_be_bytes([b[0], b[1]]),
            protocol_type: u16::from_be_bytes([b[2], b[3]]),
            cmd_length: u16::from_be_bytes([b[4], b[5]]),
        })
    }
}

/// A Modbus request PDU. The first byte is the slave (unit) address, the
/// second the function code, followed by function-specific data.
///
/// This is the "data" the driver hands to the framing layer; the framing
/// layer (TCP MBAP / RTU CRC / ASCII LRC) wraps it before transmission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPdu {
    bytes: Vec<u8>,
}

impl RequestPdu {
    /// Build a read request (`ReadCoils`, `ReadDiscreteInputs`,
    /// `ReadHoldingRegisters`, `ReadInputRegisters`).
    pub fn read(slave: u8, function: FunctionCode, start_reg: u16, count: u16) -> Self {
        let mut bytes = Vec::with_capacity(6);
        bytes.push(slave);
        bytes.push(function.as_u8());
        bytes.extend_from_slice(&start_reg.to_be_bytes());
        bytes.extend_from_slice(&count.to_be_bytes());
        Self { bytes }
    }

    /// Build a single-write request (`WriteSingleCoil`,
    /// `WriteSingleRegister`). For a coil, `value` must be 0xFF00 (on) or
    /// 0x0000 (off).
    pub fn write_single(slave: u8, function: FunctionCode, reg: u16, value: u16) -> Self {
        let mut bytes = Vec::with_capacity(6);
        bytes.push(slave);
        bytes.push(function.as_u8());
        bytes.extend_from_slice(&reg.to_be_bytes());
        bytes.extend_from_slice(&value.to_be_bytes());
        Self { bytes }
    }

    /// Build a multiple-register write request (`WriteMultipleRegisters`).
    pub fn write_multiple_registers(slave: u8, start_reg: u16, values: &[u16]) -> Self {
        let count = values.len() as u16;
        let byte_count = (values.len() * 2) as u8;
        let mut bytes = Vec::with_capacity(7 + values.len() * 2);
        bytes.push(slave);
        bytes.push(FunctionCode::WriteMultipleRegisters.as_u8());
        bytes.extend_from_slice(&start_reg.to_be_bytes());
        bytes.extend_from_slice(&count.to_be_bytes());
        bytes.push(byte_count);
        for v in values {
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        Self { bytes }
    }

    /// Build a multiple-coil write request (`WriteMultipleCoils`). `coils`
    /// are packed LSB-first into bytes, the standard Modbus coil layout.
    pub fn write_multiple_coils(slave: u8, start_reg: u16, coils: &[bool]) -> Self {
        let count = coils.len() as u16;
        let byte_count = coils.len().div_ceil(8) as u8;
        let mut data = vec![0u8; byte_count as usize];
        for (i, &on) in coils.iter().enumerate() {
            if on {
                data[i / 8] |= 1 << (i % 8);
            }
        }
        let mut bytes = Vec::with_capacity(7 + data.len());
        bytes.push(slave);
        bytes.push(FunctionCode::WriteMultipleCoils.as_u8());
        bytes.extend_from_slice(&start_reg.to_be_bytes());
        bytes.extend_from_slice(&count.to_be_bytes());
        bytes.push(byte_count);
        bytes.extend_from_slice(&data);
        Self { bytes }
    }

    /// Build a combined read/write request (`ReadWriteMultipleRegisters`,
    /// function code 0x17).
    pub fn read_write_multiple_registers(
        slave: u8,
        read_start: u16,
        read_count: u16,
        write_start: u16,
        write_values: &[u16],
    ) -> Self {
        let write_count = write_values.len() as u16;
        let byte_count = (write_values.len() * 2) as u8;
        let mut bytes = Vec::with_capacity(11 + write_values.len() * 2);
        bytes.push(slave);
        bytes.push(FunctionCode::ReadWriteMultipleRegisters.as_u8());
        bytes.extend_from_slice(&read_start.to_be_bytes());
        bytes.extend_from_slice(&read_count.to_be_bytes());
        bytes.extend_from_slice(&write_start.to_be_bytes());
        bytes.extend_from_slice(&write_count.to_be_bytes());
        bytes.push(byte_count);
        for v in write_values {
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        Self { bytes }
    }

    /// Build the read-only form of a combined read/write request used by the
    /// driver's "F23 read" mode: function code 0x17 with `numOutput` forced to
    /// 1 and `byteCount` to 2 but **no** write-data bytes appended.
    ///
    /// A `numOutput` of 0 is rejected by some slaves (and simulators), so the
    /// driver advertises one output word while sending a zero-length data
    /// field — nothing is actually written. Mirrors the `requestSize - 1`
    /// truncation in `doModbusIO`.
    pub fn f23_read(slave: u8, read_start: u16, read_count: u16, write_start: u16) -> Self {
        let mut bytes = Vec::with_capacity(11);
        bytes.push(slave);
        bytes.push(FunctionCode::ReadWriteMultipleRegisters.as_u8());
        bytes.extend_from_slice(&read_start.to_be_bytes());
        bytes.extend_from_slice(&read_count.to_be_bytes());
        bytes.extend_from_slice(&write_start.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes()); // numOutput = 1
        bytes.push(0x02); // byteCount = 2, but no data follows
        Self { bytes }
    }

    /// Build a "report slave ID" request (function code 0x11).
    pub fn report_slave_id(slave: u8) -> Self {
        Self {
            bytes: vec![slave, FunctionCode::ReportSlaveId.as_u8()],
        }
    }

    /// The slave (unit) address.
    pub fn slave(&self) -> u8 {
        self.bytes[0]
    }

    /// The raw PDU bytes (slave address first).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the PDU, returning the owned bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// A parsed Modbus response PDU.
///
/// After the framing layer strips the slave address byte (RTU/ASCII) or the
/// MBAP header (TCP), what remains starts with the function code. This type
/// is parsed from that remainder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsePdu {
    /// The function code echoed by the slave (exception bit already stripped).
    pub function: u8,
    /// Function-specific payload bytes (the data following any byte-count
    /// field; for write responses, the echoed address/value bytes).
    pub data: Vec<u8>,
}

impl ResponsePdu {
    /// Parse a response whose first byte is the function code.
    ///
    /// If the exception bit (`0x80`) is set, the second byte is decoded as an
    /// [`ExceptionCode`] and returned as [`ModbusError::Exception`].
    pub fn parse(buf: &[u8]) -> ModbusResult<Self> {
        if buf.is_empty() {
            return Err(ModbusError::FrameTooShort { got: 0, need: 2 });
        }
        let fcode = buf[0];
        if fcode & MODBUS_EXCEPTION_FCN != 0 {
            if buf.len() < 2 {
                return Err(ModbusError::FrameTooShort {
                    got: buf.len(),
                    need: 2,
                });
            }
            return Err(ModbusError::Exception(ExceptionCode::from_u8(buf[1])));
        }
        Ok(Self {
            function: fcode,
            data: buf[1..].to_vec(),
        })
    }

    /// For a read-data response (`0x01`..`0x04`, `0x17`), the response is
    /// `[fcode, byte_count, data...]`. Returns the `data...` slice, verifying
    /// the byte count against the actual payload length.
    pub fn read_data(&self) -> ModbusResult<&[u8]> {
        if self.data.is_empty() {
            return Err(ModbusError::FrameTooShort { got: 1, need: 2 });
        }
        let byte_count = self.data[0] as usize;
        let payload = &self.data[1..];
        if payload.len() < byte_count {
            return Err(ModbusError::FrameTooShort {
                got: payload.len(),
                need: byte_count,
            });
        }
        Ok(&payload[..byte_count])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbap_header_roundtrip() {
        let h = MbapHeader::new(0x1234, 6);
        let bytes = h.to_bytes();
        assert_eq!(bytes, [0x12, 0x34, 0x00, 0x00, 0x00, 0x06]);
        assert_eq!(MbapHeader::from_bytes(&bytes).unwrap(), h);
    }

    #[test]
    fn mbap_header_short_buffer() {
        assert!(matches!(
            MbapHeader::from_bytes(&[0, 1, 2]),
            Err(ModbusError::FrameTooShort { got: 3, need: 6 })
        ));
    }

    #[test]
    fn read_request_layout() {
        // Read 10 holding registers starting at address 100, slave 1.
        let pdu = RequestPdu::read(1, FunctionCode::ReadHoldingRegisters, 100, 10);
        assert_eq!(pdu.as_bytes(), &[0x01, 0x03, 0x00, 0x64, 0x00, 0x0A]);
        assert_eq!(pdu.slave(), 1);
    }

    #[test]
    fn write_single_register_layout() {
        let pdu = RequestPdu::write_single(2, FunctionCode::WriteSingleRegister, 0x0010, 0xABCD);
        assert_eq!(pdu.as_bytes(), &[0x02, 0x06, 0x00, 0x10, 0xAB, 0xCD]);
    }

    #[test]
    fn write_multiple_registers_layout() {
        let pdu = RequestPdu::write_multiple_registers(1, 0x0000, &[0x000A, 0x0102]);
        assert_eq!(
            pdu.as_bytes(),
            &[
                0x01, 0x10, 0x00, 0x00, 0x00, 0x02, 0x04, 0x00, 0x0A, 0x01, 0x02
            ]
        );
    }

    #[test]
    fn write_multiple_coils_packs_lsb_first() {
        // 10 coils: on at indices 0, 1, 9.
        let mut coils = [false; 10];
        coils[0] = true;
        coils[1] = true;
        coils[9] = true;
        let pdu = RequestPdu::write_multiple_coils(1, 0x0000, &coils);
        // byte_count = ceil(10/8) = 2; byte0 bits 0,1 set = 0x03; byte1 bit1 = 0x02
        assert_eq!(
            pdu.as_bytes(),
            &[0x01, 0x0F, 0x00, 0x00, 0x00, 0x0A, 0x02, 0x03, 0x02]
        );
    }

    #[test]
    fn read_write_multiple_layout() {
        let pdu = RequestPdu::read_write_multiple_registers(
            1,
            0x03,
            0x06,
            0x0E,
            &[0x00FF, 0x00FF, 0x00FF],
        );
        assert_eq!(
            pdu.as_bytes(),
            &[
                0x01, 0x17, 0x00, 0x03, 0x00, 0x06, 0x00, 0x0E, 0x00, 0x03, 0x06, 0x00, 0xFF, 0x00,
                0xFF, 0x00, 0xFF
            ]
        );
    }

    #[test]
    fn response_parse_read_data() {
        // fcode 0x03, byte_count 4, two registers.
        let resp = ResponsePdu::parse(&[0x03, 0x04, 0x00, 0x0A, 0x01, 0x02]).unwrap();
        assert_eq!(resp.function, 0x03);
        assert_eq!(resp.read_data().unwrap(), &[0x00, 0x0A, 0x01, 0x02]);
    }

    #[test]
    fn response_parse_exception() {
        // fcode 0x83 = read holding registers exception, code 0x02.
        let err = ResponsePdu::parse(&[0x83, 0x02]).unwrap_err();
        assert!(matches!(
            err,
            ModbusError::Exception(ExceptionCode::IllegalDataAddress)
        ));
    }

    #[test]
    fn response_read_data_rejects_short_payload() {
        // byte_count says 4 but only 2 payload bytes present.
        let resp = ResponsePdu::parse(&[0x03, 0x04, 0x00, 0x0A]).unwrap();
        assert!(matches!(
            resp.read_data(),
            Err(ModbusError::FrameTooShort { got: 2, need: 4 })
        ));
    }

    #[test]
    fn function_code_classification() {
        assert!(FunctionCode::ReadCoils.is_bit_function());
        assert!(FunctionCode::ReadCoils.is_read());
        assert!(!FunctionCode::ReadCoils.is_write());
        assert!(!FunctionCode::ReadHoldingRegisters.is_bit_function());
        assert!(FunctionCode::WriteMultipleCoils.is_bit_function());
        assert!(FunctionCode::WriteMultipleCoils.is_write());
        assert!(FunctionCode::ReadWriteMultipleRegisters.is_read());
        assert!(FunctionCode::ReadWriteMultipleRegisters.is_write());
    }
}
