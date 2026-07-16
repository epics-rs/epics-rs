//! End-to-end integration test: drive [`ModbusEngine`] over a real TCP
//! socket against a minimal in-process Modbus/TCP slave.
//!
//! This exercises the full stack — request build → MBAP framing → socket →
//! slave → response framing → unwrap → parse → register decode — with no
//! external dependency.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use modbus_rs::ModbusDataType;
use modbus_rs::driver::{ModbusConfig, ModbusEngine, ModbusFunctionCode, OctetTransport};
use modbus_rs::error::{ModbusError, ModbusResult};
use modbus_rs::interpose::LinkType;

/// A Modbus/TCP `OctetTransport` over a blocking [`TcpStream`].
struct TcpTransport {
    stream: TcpStream,
}

impl OctetTransport for TcpTransport {
    fn write_frame(&mut self, data: &[u8]) -> ModbusResult<()> {
        self.stream
            .write_all(data)
            .map_err(|e| ModbusError::Io(e.to_string()))
    }

    fn read_frame(&mut self, _timeout: Duration) -> ModbusResult<Vec<u8>> {
        // Read the 6-byte MBAP header, then the PDU it sizes.
        let mut header = [0u8; 6];
        self.stream
            .read_exact(&mut header)
            .map_err(|e| ModbusError::Io(e.to_string()))?;
        let pdu_len = u16::from_be_bytes([header[4], header[5]]) as usize;
        let mut pdu = vec![0u8; pdu_len];
        self.stream
            .read_exact(&mut pdu)
            .map_err(|e| ModbusError::Io(e.to_string()))?;
        let mut frame = header.to_vec();
        frame.extend_from_slice(&pdu);
        Ok(frame)
    }
}

/// A minimal Modbus/TCP slave: serves 64 holding registers, handling
/// function 3 (read) and function 6 (write single). Runs until the client
/// disconnects.
fn spawn_slave(initial: Vec<u16>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut regs = [0u16; 64];
        for (i, &v) in initial.iter().enumerate() {
            regs[i] = v;
        }
        let (mut sock, _) = listener.accept().expect("accept");
        loop {
            let mut header = [0u8; 6];
            if sock.read_exact(&mut header).is_err() {
                return; // client closed
            }
            let pdu_len = u16::from_be_bytes([header[4], header[5]]) as usize;
            let mut pdu = vec![0u8; pdu_len];
            if sock.read_exact(&mut pdu).is_err() {
                return;
            }
            let unit = pdu[0];
            let fcode = pdu[1];
            let resp_pdu: Vec<u8> = match fcode {
                0x03 => {
                    let start = u16::from_be_bytes([pdu[2], pdu[3]]) as usize;
                    let count = u16::from_be_bytes([pdu[4], pdu[5]]) as usize;
                    let mut r = vec![unit, 0x03, (count * 2) as u8];
                    for i in 0..count {
                        r.extend_from_slice(&regs[start + i].to_be_bytes());
                    }
                    r
                }
                0x06 => {
                    let reg = u16::from_be_bytes([pdu[2], pdu[3]]) as usize;
                    let value = u16::from_be_bytes([pdu[4], pdu[5]]);
                    regs[reg] = value;
                    // Write-single response echoes address + value.
                    vec![unit, 0x06, pdu[2], pdu[3], pdu[4], pdu[5]]
                }
                other => vec![unit, other | 0x80, 0x01], // illegal function
            };
            let mut reply = header[..4].to_vec(); // tid + proto
            reply.extend_from_slice(&(resp_pdu.len() as u16).to_be_bytes());
            reply.extend_from_slice(&resp_pdu);
            if sock.write_all(&reply).is_err() {
                return;
            }
        }
    });
    port
}

fn config(function: ModbusFunctionCode, start: i32, length: usize) -> ModbusConfig {
    ModbusConfig {
        slave: 1,
        function,
        start_address: start,
        length,
        data_type: ModbusDataType::UInt16,
        poll_delay: Duration::from_millis(100),
        plc_type: String::new(),
    }
}

fn connect(port: u16) -> TcpTransport {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_nodelay(true).ok();
    TcpTransport { stream }
}

#[test]
fn read_holding_registers_over_tcp() {
    let port = spawn_slave(vec![10, 20, 30, 40]);
    let mut transport = connect(port);
    let mut engine = ModbusEngine::new(
        config(ModbusFunctionCode::ReadHoldingRegisters, 0, 4),
        LinkType::Tcp,
    )
    .unwrap();

    let words = words_of(
        engine
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                4,
            )
            .expect("read"),
    );
    assert_eq!(words, vec![10, 20, 30, 40]);
    assert_eq!(engine.stats.read_ok, 1);
    assert_eq!(engine.stats.io_errors, 0);
}

#[test]
fn poll_refreshes_register_buffer_over_tcp() {
    let port = spawn_slave(vec![100, 200]);
    let mut transport = connect(port);
    let mut engine = ModbusEngine::new(
        config(ModbusFunctionCode::ReadHoldingRegisters, 0, 2),
        LinkType::Tcp,
    )
    .unwrap();

    assert!(engine.poll(&mut transport).expect("first poll")); // changed from zero
    assert_eq!(engine.data(), &[100, 200]);
    assert!(!engine.poll(&mut transport).expect("second poll")); // unchanged
}

#[test]
fn write_single_register_round_trip_over_tcp() {
    let port = spawn_slave(vec![0; 8]);
    let mut transport = connect(port);

    // Write 0xABCD to register 5.
    let mut writer = ModbusEngine::new(
        config(ModbusFunctionCode::WriteSingleRegister, 0, 8),
        LinkType::Tcp,
    )
    .unwrap();
    writer
        .do_modbus_io(
            &mut transport,
            ModbusFunctionCode::WriteSingleRegister,
            5,
            &[0xABCD],
            1,
        )
        .expect("write");
    assert_eq!(writer.stats.write_ok, 1);

    // Read it back through the same connection.
    let mut reader = ModbusEngine::new(
        config(ModbusFunctionCode::ReadHoldingRegisters, 0, 8),
        LinkType::Tcp,
    )
    .unwrap();
    let words = words_of(
        reader
            .do_modbus_io(
                &mut transport,
                ModbusFunctionCode::ReadHoldingRegisters,
                0,
                &[],
                8,
            )
            .expect("read back"),
    );
    assert_eq!(words[5], 0xABCD);
}

#[test]
fn modbus_exception_surfaces_over_tcp() {
    let port = spawn_slave(vec![]);
    let mut transport = connect(port);
    // The slave answers function 4 (read input registers) with an exception.
    let mut engine = ModbusEngine::new(
        config(ModbusFunctionCode::ReadInputRegisters, 0, 2),
        LinkType::Tcp,
    )
    .unwrap();
    let err = engine
        .do_modbus_io(
            &mut transport,
            ModbusFunctionCode::ReadInputRegisters,
            0,
            &[],
            2,
        )
        .unwrap_err();
    assert!(matches!(err, ModbusError::Exception(_)));
    // C parity (drvModbusAsyn.cpp:2239-2246): a Modbus exception response sets
    // asynError and `goto done` past the OK switch. It is not a transport
    // `writeRead` failure (the only IOErrors_ site, :2204-2208), so neither
    // IOErrors_ nor readOK_ moves.
    assert_eq!(engine.stats.io_errors, 0);
    assert_eq!(engine.stats.read_ok, 0);
}

/// Unwrap the data words of an I/O the test expects to be a normal response.
fn words_of(response: modbus_rs::ModbusIoResponse) -> Vec<u16> {
    match response {
        modbus_rs::ModbusIoResponse::Data(words) => words,
        modbus_rs::ModbusIoResponse::Acknowledged => {
            panic!("expected data, got exception-05 Acknowledge")
        }
    }
}
