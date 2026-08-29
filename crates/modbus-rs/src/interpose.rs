//! Link-layer framing for Modbus.
//!
//! Port of `modbusInterpose.c`. The Modbus driver builds a bare PDU
//! (`[slave, fcode, ...]`); this layer wraps it for the physical link before
//! it reaches the underlying `asyn-rs` octet port, and unwraps the response:
//!
//! - **TCP / UDP** — prepend the 6-byte MBAP header; strip MBAP + slave byte
//!   from the reply, matching the reply's transaction ID against the request.
//! - **RTU** — append a CRC-16; verify the CRC and strip the slave byte from
//!   the reply.
//! - **ASCII** — `:`-prefixed hex with an LRC; the underlying serial port
//!   adds/strips the CR/LF terminator.
//!
//! The CRC-16 and LRC are spec-compliant (`CRC-16/MODBUS`, poly `0xA001`;
//! 8-bit two's-complement LRC).

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use crate::error::{ModbusError, ModbusResult};
use crate::protocol::{
    MAX_MODBUS_FRAME_SIZE, MBAP_HEADER_SIZE, MBAP_MIN_CMD_LENGTH, MODBUS_PROTOCOL_ID, MbapHeader,
};

/// Default response timeout when none is configured (matches the C
/// `DEFAULT_TIMEOUT` of 2.0 s).
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Physical link encoding. Mirrors the C `modbusLinkType` enum, including its
/// discriminant order so `iocsh`-style integer arguments map identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LinkType {
    /// Modbus/TCP — MBAP-framed over a stream socket.
    Tcp = 0,
    /// Modbus RTU — CRC-16-framed over a serial line.
    Rtu = 1,
    /// Modbus ASCII — `:`-prefixed hex with an LRC over a serial line.
    Ascii = 2,
    /// Modbus/TCP carried over UDP datagrams (retransmits on timeout).
    Udp = 3,
}

impl LinkType {
    /// Decode the integer form used by `modbusInterposeConfig`.
    pub fn from_i32(v: i32) -> Option<Self> {
        Some(match v {
            0 => Self::Tcp,
            1 => Self::Rtu,
            2 => Self::Ascii,
            3 => Self::Udp,
            _ => return None,
        })
    }

    /// Whether this link carries Modbus inside an MBAP header.
    pub fn is_mbap(self) -> bool {
        matches!(self, Self::Tcp | Self::Udp)
    }
}

/// Compute the `CRC-16/MODBUS` checksum (poly `0xA001`, init `0xFFFF`).
///
/// Bit-reflected algorithm; produces the same value as the dual-lookup-table
/// implementation in `modbusInterpose.c`. The result is appended to an RTU
/// frame low byte first.
pub fn compute_crc(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 0x0001 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Compute the Modbus ASCII LRC: the 8-bit two's-complement of the byte sum.
pub fn compute_lrc(data: &[u8]) -> u8 {
    let sum = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    0u8.wrapping_sub(sum)
}

/// Encode one byte as two uppercase ASCII hex digits.
fn encode_ascii(value: u8, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(HEX[(value >> 4) as usize]);
    out.push(HEX[(value & 0x0F) as usize]);
}

/// Decode one ASCII hex digit (`0`-`9`, `A`-`F`; the C decoder is
/// uppercase-only, so this matches it).
fn hex_digit(c: u8) -> ModbusResult<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ModbusError::MissingAsciiMarker),
    }
}

/// The rolling MBAP transaction-ID counter. In C this is `modbusPvt`'s
/// `transactionId` field (`modbusInterpose.c:91`, bumped at `:254`), and
/// `modbusPvt` is the interpose layer's private data — **one instance per
/// octet port**, not per Modbus port. Several `drvModbusAsyn` ports sharing
/// one octet port therefore draw their IDs from one sequence, which is what
/// makes their concurrent requests distinguishable on the wire.
///
/// Shared by handle so that ownership follows the octet port rather than the
/// caller; a framer that owns its link outright gets a private one from
/// [`ModbusFramer::new`].
#[derive(Debug, Clone, Default)]
pub struct TransactionIdCounter(Arc<AtomicU16>);

impl TransactionIdCounter {
    /// Advance and return the next transaction ID (C `(transactionId + 1) &
    /// 0xFFFF`, so the first request off a fresh counter carries 1).
    fn next(&self) -> u16 {
        self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }
}

/// Modbus link-layer framer. Holds the link type and — for MBAP links — a
/// handle on the octet port's transaction-ID counter.
#[derive(Debug)]
pub struct ModbusFramer {
    link_type: LinkType,
    transaction_id: TransactionIdCounter,
}

/// A framed request together with the transaction ID it was stamped with.
///
/// The ID is returned rather than read back from the framer afterwards: the
/// counter is shared with every other Modbus port on the same octet port, so
/// a later read would report whichever port framed most recently, not this
/// request's own ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramedRequest {
    /// The on-wire frame.
    pub bytes: Vec<u8>,
    /// Transaction ID stamped into the MBAP header (`None` for RTU/ASCII).
    pub transaction_id: Option<u16>,
}

/// An unwrapped response: the PDU bytes (function-code first) plus, for MBAP
/// links, the transaction ID echoed by the slave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwrappedResponse {
    /// Response PDU starting with the function code.
    pub pdu: Vec<u8>,
    /// Transaction ID from the MBAP header (`None` for RTU/ASCII).
    pub transaction_id: Option<u16>,
}

/// Total on-wire length of the frame an MBAP header introduces.
///
/// `cmd_length` counts the unit identifier plus the PDU. A header declaring
/// fewer than [`MBAP_MIN_CMD_LENGTH`] bytes, more than the link allows, or a
/// protocol identifier other than [`MODBUS_PROTOCOL_ID`] is not a Modbus
/// header at all, so the position of the next frame in the stream is unknown.
fn mbap_frame_len(header: &MbapHeader) -> ModbusResult<usize> {
    let len = header.cmd_length as usize;
    if header.protocol_type != MODBUS_PROTOCOL_ID
        || len < MBAP_MIN_CMD_LENGTH
        || MBAP_HEADER_SIZE + len > MAX_MODBUS_FRAME_SIZE
    {
        return Err(ModbusError::MalformedResponse(format!(
            "MBAP header declares protocol {} length {}",
            header.protocol_type, len
        )));
    }
    Ok(MBAP_HEADER_SIZE + len)
}

/// Reassembles MBAP frames from a byte stream.
///
/// Modbus/TCP carries no terminator, so a reader that treats one `recv` as one
/// frame desynchronises the moment the network splits or coalesces replies:
/// the tail of a split reply is parsed as the next transaction's MBAP header
/// and the port stays one frame behind for good. The frame length is the
/// header's `cmd_length` — the same field [`ModbusFramer::frame_request`]
/// writes — so this reads until that many bytes are present and keeps whatever
/// arrived beyond them for the next frame.
///
/// UDP needs none of this: a datagram is delivered whole or not at all, and
/// carrying leftovers between datagrams would desynchronise a link that cannot
/// otherwise lose framing.
///
/// # This is a deliberate divergence: C does not reassemble
///
/// C's `readIt` TCP/UDP arm reads into `pPvt->rxBuffer` from offset 0 on every
/// iteration of its loop (`modbusInterpose.c:346-349`) and breaks as soon as
/// two bytes have arrived whose transaction ID matches (`:366-368`) — it never
/// keeps what a previous read returned. It then hands up
/// `nbytesActual - mbapSize - 1` bytes (`:372-378`), so a reply split across
/// TCP segments is delivered **truncated and reported as success**, with the
/// tail left on the socket to be parsed as the next transaction's MBAP header.
/// Only a chunk shorter than 2 bytes makes C loop and re-read.
///
/// This port reads to the length the header declares before returning a frame,
/// so a split reply is served whole. Reassembling is the divergence; do not
/// "restore parity" by reading one chunk per frame.
#[derive(Debug, Default)]
pub struct MbapAccumulator {
    buf: Vec<u8>,
}

impl MbapAccumulator {
    /// An empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes as they arrive from the link.
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Drop whatever partial frame is buffered.
    ///
    /// Bytes held here are only meaningful as the head of the reply to the
    /// request that is still outstanding. Once that transaction ends without a
    /// frame, they belong to nothing, and leaving them makes the next reply's
    /// head read as the tail of a message that will never be completed.
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Remove and return the next complete frame, or `None` while the buffered
    /// bytes are still short of the length its header declares.
    ///
    /// A header that cannot be a Modbus header leaves the stream position
    /// unknown, so the buffer is dropped along with the error rather than
    /// re-parsed at the same offset on the next call.
    pub fn next_frame(&mut self) -> ModbusResult<Option<Vec<u8>>> {
        if self.buf.len() < MBAP_HEADER_SIZE {
            return Ok(None);
        }
        let header = MbapHeader::from_bytes(&self.buf[..MBAP_HEADER_SIZE])?;
        let need = mbap_frame_len(&header).inspect_err(|_| self.buf.clear())?;
        if self.buf.len() < need {
            return Ok(None);
        }
        Ok(Some(self.buf.drain(..need).collect()))
    }

    /// Return one whole frame, pulling more bytes through `read_chunk` until
    /// the length its header declares is satisfied.
    ///
    /// `read_chunk` returns whatever a single read of the link produced; an
    /// empty chunk is the underlying port's timeout.
    pub fn read_frame(
        &mut self,
        mut read_chunk: impl FnMut() -> ModbusResult<Vec<u8>>,
    ) -> ModbusResult<Vec<u8>> {
        loop {
            if let Some(frame) = self.next_frame()? {
                return Ok(frame);
            }
            let chunk = read_chunk()?;
            if chunk.is_empty() {
                return Err(ModbusError::Timeout);
            }
            self.extend(&chunk);
        }
    }
}

impl ModbusFramer {
    /// Create a framer with a transaction-ID counter of its own. Correct only
    /// where the caller owns the link outright; a framer over a shared octet
    /// port must be built with [`Self::with_transaction_counter`].
    pub fn new(link_type: LinkType) -> Self {
        Self::with_transaction_counter(link_type, TransactionIdCounter::default())
    }

    /// Create a framer drawing transaction IDs from the octet port's shared
    /// counter (C's per-`modbusPvt` `transactionId`).
    pub fn with_transaction_counter(link_type: LinkType, counter: TransactionIdCounter) -> Self {
        Self {
            link_type,
            transaction_id: counter,
        }
    }

    /// The configured link type.
    pub fn link_type(&self) -> LinkType {
        self.link_type
    }

    /// Wrap a bare request PDU (`[slave, fcode, ...]`) into an on-wire frame.
    ///
    /// For MBAP links this advances the octet port's transaction-ID counter
    /// and reports the ID it used. The CR/LF terminator for ASCII frames is
    /// added by the underlying serial port's output EOS, not here — matching
    /// the C driver.
    pub fn frame_request(&mut self, pdu: &[u8]) -> ModbusResult<FramedRequest> {
        match self.link_type {
            LinkType::Tcp | LinkType::Udp => {
                let transaction_id = self.transaction_id.next();
                let header = MbapHeader::new(transaction_id, pdu.len() as u16);
                let mut frame = Vec::with_capacity(MBAP_HEADER_SIZE + pdu.len());
                frame.extend_from_slice(&header.to_bytes());
                frame.extend_from_slice(pdu);
                Self::check_size(frame.len())?;
                Ok(FramedRequest {
                    bytes: frame,
                    transaction_id: Some(transaction_id),
                })
            }
            LinkType::Rtu => {
                let crc = compute_crc(pdu);
                let mut frame = Vec::with_capacity(pdu.len() + 2);
                frame.extend_from_slice(pdu);
                // CRC is appended low byte first.
                frame.push((crc & 0xFF) as u8);
                frame.push((crc >> 8) as u8);
                Self::check_size(frame.len())?;
                Ok(FramedRequest {
                    bytes: frame,
                    transaction_id: None,
                })
            }
            LinkType::Ascii => {
                let lrc = compute_lrc(pdu);
                let mut frame = Vec::with_capacity(1 + (pdu.len() + 1) * 2);
                frame.push(b':');
                for &b in pdu {
                    encode_ascii(b, &mut frame);
                }
                encode_ascii(lrc, &mut frame);
                Self::check_size(frame.len())?;
                Ok(FramedRequest {
                    bytes: frame,
                    transaction_id: None,
                })
            }
        }
    }

    /// Unwrap an on-wire response frame, returning the bare response PDU.
    ///
    /// - TCP/UDP: strips the 6-byte MBAP header and the 1-byte slave/unit ID.
    /// - RTU: verifies the CRC-16, then strips the slave byte and CRC.
    /// - ASCII: checks the `:` marker, decodes hex, verifies the LRC, strips
    ///   the slave byte. The serial port has already removed the CR/LF.
    pub fn unwrap_response(&self, frame: &[u8]) -> ModbusResult<UnwrappedResponse> {
        match self.link_type {
            LinkType::Tcp | LinkType::Udp => {
                // MBAP header (6) + slave byte (1) + at least the fcode.
                if frame.len() < MBAP_HEADER_SIZE + 2 {
                    return Err(ModbusError::FrameTooShort {
                        got: frame.len(),
                        need: MBAP_HEADER_SIZE + 2,
                    });
                }
                let header = MbapHeader::from_bytes(&frame[..MBAP_HEADER_SIZE])?;
                let need = mbap_frame_len(&header)?;
                // The header's own `cmd_length` delimits the frame: anything
                // short of it is a truncated reply, and anything past it
                // belongs to the next transaction, not to this PDU.
                if frame.len() < need {
                    return Err(ModbusError::FrameTooShort {
                        got: frame.len(),
                        need,
                    });
                }
                // Skip MBAP header + the 1-byte slave/unit ID.
                let pdu = frame[MBAP_HEADER_SIZE + 1..need].to_vec();
                Ok(UnwrappedResponse {
                    pdu,
                    transaction_id: Some(header.transaction_id),
                })
            }
            LinkType::Rtu => {
                // slave (1) + fcode (1) + CRC (2).
                if frame.len() < 4 {
                    return Err(ModbusError::FrameTooShort {
                        got: frame.len(),
                        need: 4,
                    });
                }
                // CRC over the whole frame, including the CRC bytes, is 0.
                if compute_crc(frame) != 0 {
                    return Err(ModbusError::CrcError);
                }
                // Strip slave byte (front) and CRC (last 2).
                let pdu = frame[1..frame.len() - 2].to_vec();
                Ok(UnwrappedResponse {
                    pdu,
                    transaction_id: None,
                })
            }
            LinkType::Ascii => {
                if frame.first() != Some(&b':') {
                    return Err(ModbusError::MissingAsciiMarker);
                }
                let hex = &frame[1..];
                if !hex.len().is_multiple_of(2) {
                    return Err(ModbusError::FrameTooShort {
                        got: frame.len(),
                        need: frame.len() + 1,
                    });
                }
                let mut bytes = Vec::with_capacity(hex.len() / 2);
                for pair in hex.chunks_exact(2) {
                    let hi = hex_digit(pair[0])?;
                    let lo = hex_digit(pair[1])?;
                    bytes.push((hi << 4) | lo);
                }
                // Need at least slave (1) + fcode (1) + LRC (1).
                if bytes.len() < 3 {
                    return Err(ModbusError::FrameTooShort {
                        got: bytes.len(),
                        need: 3,
                    });
                }
                // Last decoded byte is the LRC; it covers everything before.
                //
                // NOTE: upstream `modbusInterpose.c` computes the LRC over a
                // span that includes the received LRC byte and then compares
                // against `data[i]` one past the decoded region — a latent
                // off-by-one. This port follows the Modbus ASCII spec: LRC
                // over slave+data only.
                let received_lrc = *bytes.last().unwrap();
                let body = &bytes[..bytes.len() - 1];
                let computed = compute_lrc(body);
                if computed != received_lrc {
                    return Err(ModbusError::LrcError {
                        received: received_lrc,
                        computed,
                    });
                }
                // Strip the slave byte.
                Ok(UnwrappedResponse {
                    pdu: body[1..].to_vec(),
                    transaction_id: None,
                })
            }
        }
    }

    fn check_size(len: usize) -> ModbusResult<()> {
        if len > MAX_MODBUS_FRAME_SIZE {
            Err(ModbusError::FrameTooLarge(len))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vector() {
        // CRC-16/MODBUS of the ASCII string "123456789" is 0x4B37.
        assert_eq!(compute_crc(b"123456789"), 0x4B37);
    }

    #[test]
    fn crc_over_frame_with_appended_crc_is_zero() {
        let pdu = [0x01u8, 0x03, 0x00, 0x00, 0x00, 0x0A];
        let crc = compute_crc(&pdu);
        let mut frame = pdu.to_vec();
        frame.push((crc & 0xFF) as u8);
        frame.push((crc >> 8) as u8);
        assert_eq!(compute_crc(&frame), 0);
    }

    #[test]
    fn lrc_negates_sum() {
        // sum(0x01,0x03,0x00,0x6B,0x00,0x03) = 0x72; LRC = 0x100 - 0x72 = 0x8E.
        assert_eq!(compute_lrc(&[0x01, 0x03, 0x00, 0x6B, 0x00, 0x03]), 0x8E);
    }

    #[test]
    fn lrc_property_sum_with_lrc_is_zero() {
        let body = [0x01u8, 0x03, 0x00, 0x6B, 0x00, 0x03];
        let lrc = compute_lrc(&body);
        let total: u8 = body
            .iter()
            .chain(std::iter::once(&lrc))
            .fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(total, 0);
    }

    #[test]
    fn tcp_frame_roundtrip() {
        let mut framer = ModbusFramer::new(LinkType::Tcp);
        let pdu = [0x01u8, 0x03, 0x00, 0x64, 0x00, 0x0A];
        let framed = framer.frame_request(&pdu).unwrap();
        assert_eq!(framed.transaction_id, Some(1));
        let frame = framed.bytes;
        // MBAP: txid=1, proto=0, len=6.
        assert_eq!(&frame[..6], &[0x00, 0x01, 0x00, 0x00, 0x00, 0x06]);
        assert_eq!(&frame[6..], &pdu);

        // Build a response: MBAP(txid 1) + slave + fcode + byte_count + data.
        let resp_pdu = [0x01u8, 0x03, 0x02, 0xAB, 0xCD];
        let header = MbapHeader::new(1, resp_pdu.len() as u16);
        let mut resp = header.to_bytes().to_vec();
        resp.extend_from_slice(&resp_pdu);
        let unwrapped = framer.unwrap_response(&resp).unwrap();
        assert_eq!(unwrapped.transaction_id, Some(1));
        assert_eq!(unwrapped.pdu, &[0x03, 0x02, 0xAB, 0xCD]);
    }

    #[test]
    fn tcp_transaction_id_increments() {
        let mut framer = ModbusFramer::new(LinkType::Tcp);
        let first = framer.frame_request(&[0x01, 0x03]).unwrap();
        assert_eq!(first.transaction_id, Some(1));
        let second = framer.frame_request(&[0x01, 0x03]).unwrap();
        assert_eq!(second.transaction_id, Some(2));
    }

    #[test]
    fn framers_on_one_octet_port_never_reuse_a_transaction_id() {
        // C keeps `transactionId` on the interpose `modbusPvt`
        // (`modbusInterpose.c:91`), which is per OCTET port, so two Modbus
        // ports sharing one octet port draw from one sequence. With a counter
        // each, both would stamp 1 on their first request and neither could
        // tell its own reply from the other's.
        let counter = TransactionIdCounter::default();
        let mut coils = ModbusFramer::with_transaction_counter(LinkType::Tcp, counter.clone());
        let mut holding = ModbusFramer::with_transaction_counter(LinkType::Tcp, counter);
        let a = coils.frame_request(&[0x01, 0x01]).unwrap().transaction_id;
        let b = holding.frame_request(&[0x01, 0x03]).unwrap().transaction_id;
        let c = coils.frame_request(&[0x01, 0x01]).unwrap().transaction_id;
        assert_eq!((a, b, c), (Some(1), Some(2), Some(3)));

        // A framer that owns its link outright still gets a private sequence.
        let mut alone = ModbusFramer::new(LinkType::Tcp);
        assert_eq!(
            alone.frame_request(&[0x01, 0x03]).unwrap().transaction_id,
            Some(1)
        );
    }

    #[test]
    fn rtu_frame_roundtrip() {
        let mut framer = ModbusFramer::new(LinkType::Rtu);
        let pdu = [0x01u8, 0x03, 0x00, 0x00, 0x00, 0x0A];
        let framed = framer.frame_request(&pdu).unwrap();
        assert_eq!(framed.transaction_id, None);
        let frame = framed.bytes;
        assert_eq!(&frame[..pdu.len()], &pdu);
        assert_eq!(frame.len(), pdu.len() + 2);

        // Response: slave + fcode + byte_count + data + CRC.
        let resp_body = [0x01u8, 0x03, 0x02, 0x12, 0x34];
        let crc = compute_crc(&resp_body);
        let mut resp = resp_body.to_vec();
        resp.push((crc & 0xFF) as u8);
        resp.push((crc >> 8) as u8);
        let unwrapped = framer.unwrap_response(&resp).unwrap();
        assert_eq!(unwrapped.transaction_id, None);
        assert_eq!(unwrapped.pdu, &[0x03, 0x02, 0x12, 0x34]);
    }

    #[test]
    fn rtu_bad_crc_rejected() {
        let framer = ModbusFramer::new(LinkType::Rtu);
        // Valid body but a corrupted CRC.
        let resp = [0x01u8, 0x03, 0x02, 0x12, 0x34, 0x00, 0x00];
        assert!(matches!(
            framer.unwrap_response(&resp),
            Err(ModbusError::CrcError)
        ));
    }

    #[test]
    fn ascii_frame_roundtrip() {
        let mut framer = ModbusFramer::new(LinkType::Ascii);
        let pdu = [0x01u8, 0x03, 0x00, 0x6B, 0x00, 0x03];
        let framed = framer.frame_request(&pdu).unwrap();
        assert_eq!(framed.transaction_id, None);
        let frame = framed.bytes;
        assert_eq!(frame[0], b':');
        // ':' + 6 PDU bytes (12 hex) + LRC (2 hex) = 15 chars.
        assert_eq!(frame.len(), 1 + 12 + 2);
        assert_eq!(&frame[1..13], b"0103006B0003");
        // LRC of the PDU is 0x8E.
        assert_eq!(&frame[13..], b"8E");

        // Response: slave + fcode + byte_count + data, with LRC appended.
        let resp_body = [0x01u8, 0x03, 0x02, 0xAA, 0xBB];
        let lrc = compute_lrc(&resp_body);
        let mut frame = vec![b':'];
        for &b in &resp_body {
            encode_ascii(b, &mut frame);
        }
        encode_ascii(lrc, &mut frame);
        let unwrapped = framer.unwrap_response(&frame).unwrap();
        assert_eq!(unwrapped.pdu, &[0x03, 0x02, 0xAA, 0xBB]);
    }

    #[test]
    fn ascii_bad_lrc_rejected() {
        let framer = ModbusFramer::new(LinkType::Ascii);
        let mut frame = vec![b':'];
        for &b in &[0x01u8, 0x03, 0x02, 0xAA, 0xBB] {
            encode_ascii(b, &mut frame);
        }
        encode_ascii(0x00, &mut frame); // wrong LRC
        assert!(matches!(
            framer.unwrap_response(&frame),
            Err(ModbusError::LrcError { .. })
        ));
    }

    #[test]
    fn ascii_missing_marker_rejected() {
        let framer = ModbusFramer::new(LinkType::Ascii);
        assert!(matches!(
            framer.unwrap_response(b"010302AABB"),
            Err(ModbusError::MissingAsciiMarker)
        ));
    }

    #[test]
    fn link_type_from_i32_matches_c_enum() {
        assert_eq!(LinkType::from_i32(0), Some(LinkType::Tcp));
        assert_eq!(LinkType::from_i32(1), Some(LinkType::Rtu));
        assert_eq!(LinkType::from_i32(2), Some(LinkType::Ascii));
        assert_eq!(LinkType::from_i32(3), Some(LinkType::Udp));
        assert_eq!(LinkType::from_i32(4), None);
    }

    // ── F3: MBAP framing over a byte stream ──────────────────────────────

    /// A Modbus/TCP reply to a 125-register read: 6 MBAP bytes plus the 253
    /// the header declares (unit, function code, byte count, 250 data bytes).
    fn read_125_registers_reply(txid: u16) -> Vec<u8> {
        let mut frame = MbapHeader::new(txid, 253).to_bytes().to_vec();
        frame.extend_from_slice(&[0x01, 0x03, 250]);
        frame.extend((0..250u16).map(|i| i as u8));
        assert_eq!(frame.len(), 259);
        frame
    }

    /// F3: the network is free to split a 259-byte reply across two reads. The
    /// old reader took whatever one read returned as a whole frame, so the tail
    /// became the next transaction's MBAP header and the port never resynced.
    #[test]
    fn a_split_tcp_reply_is_reassembled_into_one_frame() {
        let frame = read_125_registers_reply(7);
        let mut chunks = vec![frame[..140].to_vec(), frame[140..].to_vec()].into_iter();

        let mut acc = MbapAccumulator::new();
        let got = acc
            .read_frame(|| Ok(chunks.next().unwrap_or_default()))
            .unwrap();

        assert_eq!(got, frame, "both reads must land in one frame");
        assert_eq!(chunks.next(), None, "no bytes may be left unread");
    }

    /// F3: the network is equally free to coalesce two replies into one read.
    /// The surplus belongs to the next frame, not to this one.
    #[test]
    fn two_coalesced_tcp_replies_are_returned_as_separate_frames() {
        let first = read_125_registers_reply(7);
        let second = read_125_registers_reply(8);
        let mut both = first.clone();
        both.extend_from_slice(&second);
        let mut chunks = vec![both].into_iter();

        let mut acc = MbapAccumulator::new();
        let mut read = || Ok(chunks.next().unwrap_or_default());
        assert_eq!(acc.read_frame(&mut read).unwrap(), first);
        // The second frame comes out of the buffer: reading again would block
        // on a link that has already sent everything it is going to send.
        assert_eq!(acc.read_frame(&mut read).unwrap(), second);
    }

    /// F3: `cmd_length` is the frame delimiter, so a reply cut short of it is a
    /// truncated frame and not a PDU. It used to be unwrapped into a short PDU
    /// whose transaction ID still matched.
    #[test]
    fn a_truncated_mbap_reply_is_rejected() {
        let framer = ModbusFramer::new(LinkType::Tcp);
        let frame = read_125_registers_reply(7);

        let err = framer.unwrap_response(&frame[..140]).unwrap_err();
        assert!(
            matches!(
                err,
                ModbusError::FrameTooShort {
                    got: 140,
                    need: 259
                }
            ),
            "expected a short-frame rejection, got {err:?}"
        );
    }

    /// F3: bytes past `cmd_length` are the next transaction's, so they must not
    /// reach this response's PDU.
    #[test]
    fn bytes_past_cmd_length_stay_out_of_the_pdu() {
        let framer = ModbusFramer::new(LinkType::Tcp);
        let mut frame = read_125_registers_reply(7);
        frame.extend_from_slice(&read_125_registers_reply(8)[..40]);

        let unwrapped = framer.unwrap_response(&frame).unwrap();
        assert_eq!(unwrapped.transaction_id, Some(7));
        // 253 declared bytes less the unit identifier the unwrap strips.
        assert_eq!(unwrapped.pdu.len(), 252);
    }
}
