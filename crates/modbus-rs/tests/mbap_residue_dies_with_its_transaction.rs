//! A Modbus/TCP transaction that ends without a reply takes the transport's
//! partial frame with it.
//!
//! `MbapAccumulator` lives on the transport, which is created once per port,
//! so bytes it holds outlive the read that produced them. A PLC that sends
//! part of a large reply and then stalls past the read timeout leaves that
//! head buffered; the next poll parses its MBAP header — well formed, protocol
//! id 0, a sane length — out of the stale bytes and drains stale-tail plus
//! head-of-new-reply as one frame carrying the previous transaction id. The
//! port then counts it stale, re-reads, and lands mid-body of the real reply,
//! and because nothing clears the accumulator it never recovers: every poll
//! after that raises an I/O error and every `SCAN="I/O Intr"` record on the
//! port stays in READ_ALARM/INVALID until the IOC is restarted.
//!
//! The boundary is how much of the reply arrived before the stall: nothing, a
//! part shorter than the 6-byte MBAP header, exactly the header, and a header
//! with a truncated body. Each must recover on the following poll.

use std::collections::VecDeque;
use std::time::Duration;

use modbus_rs::ModbusDataType;
use modbus_rs::driver::{ModbusConfig, ModbusEngine, ModbusFunctionCode, OctetTransport};
use modbus_rs::error::{ModbusError, ModbusResult};
use modbus_rs::interpose::{LinkType, MbapAccumulator};

/// Registers per poll — 125 is the Modbus maximum for function 3, the read
/// whose 259-byte reply does not fit one segment.
const COUNT: usize = 125;

/// A Modbus/TCP transport shaped like `SyncIoTransport`: an `MbapAccumulator`
/// that outlives every read, fed from a scripted sequence of link reads. An
/// exhausted script reads empty, which is the underlying port's timeout.
struct ChunkedTcpTransport {
    chunks: VecDeque<Vec<u8>>,
    mbap: MbapAccumulator,
}

impl ChunkedTcpTransport {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            mbap: MbapAccumulator::new(),
        }
    }

    fn push(&mut self, bytes: Vec<u8>) {
        self.chunks.push_back(bytes);
    }
}

impl OctetTransport for ChunkedTcpTransport {
    fn write_frame(&mut self, _data: &[u8]) -> ModbusResult<()> {
        Ok(())
    }

    fn reset_stream(&mut self) {
        self.mbap.reset();
    }

    fn read_frame(&mut self, _timeout: Duration) -> ModbusResult<Vec<u8>> {
        let Self { chunks, mbap } = self;
        mbap.read_frame(|| Ok(chunks.pop_front().unwrap_or_default()))
    }
}

/// A function-3 reply carrying `COUNT` registers all holding `fill`.
fn reply(transaction_id: u16, fill: u16) -> Vec<u8> {
    let mut pdu = vec![1u8, 0x03, (COUNT * 2) as u8];
    for _ in 0..COUNT {
        pdu.extend_from_slice(&fill.to_be_bytes());
    }
    let mut frame = Vec::with_capacity(6 + pdu.len());
    frame.extend_from_slice(&transaction_id.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&(pdu.len() as u16).to_be_bytes());
    frame.extend_from_slice(&pdu);
    frame
}

fn engine() -> ModbusEngine {
    ModbusEngine::new(
        ModbusConfig {
            slave: 1,
            function: ModbusFunctionCode::ReadHoldingRegisters,
            start_address: 0,
            length: COUNT,
            data_type: ModbusDataType::UInt16,
            poll_delay: Duration::from_millis(100),
            plc_type: String::new(),
        },
        LinkType::Tcp,
    )
    .expect("valid TCP read config")
}

/// The PLC sends `arrived` bytes of its reply and then stalls past the read
/// timeout; the poll after that gets a whole reply and must succeed on it.
fn recovers_after_a_stall_at(arrived: usize) {
    let mut engine = engine();
    let mut transport = ChunkedTcpTransport::new();

    if arrived > 0 {
        transport.push(reply(1, 0x1111)[..arrived].to_vec());
    }
    let err = engine
        .poll(&mut transport)
        .expect_err("a stalled reply must fail its poll");
    assert!(
        matches!(err, ModbusError::Timeout),
        "the stall is a read timeout, got {err:?}"
    );

    transport.push(reply(2, 0x2222));
    engine
        .poll(&mut transport)
        .expect("the port must recover on the poll after a stalled reply");
    assert_eq!(
        engine.data()[0],
        0x2222,
        "the recovered poll must deliver the new reply, not the stale head"
    );
    assert_eq!(
        engine.stats.io_errors, 1,
        "one stalled transaction is one I/O error"
    );
}

#[test]
fn a_stall_before_any_byte_arrives_recovers_on_the_next_poll() {
    recovers_after_a_stall_at(0);
}

/// Under the 6-byte header. Before the fix this case self-healed one poll
/// later, because the stale bytes plus the head of the next reply parse as a
/// header the length check rejects, which clears the buffer — one lost
/// transaction rather than a wedged port. It still must not lose that poll.
#[test]
fn a_stall_below_the_mbap_header_recovers_on_the_next_poll() {
    recovers_after_a_stall_at(4);
}

#[test]
fn a_stall_at_exactly_the_mbap_header_recovers_on_the_next_poll() {
    recovers_after_a_stall_at(6);
}

/// The wedging case: a complete, well-formed header with a truncated body.
/// Nothing in the parse rejects it, so the stale head is re-read as a frame
/// for as long as the port lives.
#[test]
fn a_stall_mid_body_recovers_on_the_next_poll() {
    recovers_after_a_stall_at(20);
}

/// Negative control: nothing is dropped when no read fails.
#[test]
fn consecutive_clean_polls_each_deliver_their_own_reply() {
    let mut engine = engine();
    let mut transport = ChunkedTcpTransport::new();

    transport.push(reply(1, 0x1111));
    engine.poll(&mut transport).expect("first poll");
    assert_eq!(engine.data()[0], 0x1111);

    transport.push(reply(2, 0x2222));
    engine.poll(&mut transport).expect("second poll");
    assert_eq!(engine.data()[0], 0x2222);
    assert_eq!(engine.stats.io_errors, 0);
    assert_eq!(engine.stats.read_ok, 2);
}

/// A reply split across two link reads still assembles — the behaviour the
/// accumulator exists for, and the one the reset must not undo.
#[test]
fn a_reply_split_across_two_reads_still_assembles() {
    let mut engine = engine();
    let mut transport = ChunkedTcpTransport::new();

    let frame = reply(1, 0x3333);
    transport.push(frame[..100].to_vec());
    transport.push(frame[100..].to_vec());
    engine.poll(&mut transport).expect("split reply");
    assert_eq!(engine.data()[0], 0x3333);
    assert_eq!(engine.stats.io_errors, 0);
}
