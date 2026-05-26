//! PVA-R11 integration: TCP-circuit SEARCH on an established
//! virtual circuit.
//!
//! Sends a raw `Command::Search` frame over TCP to a Rust PVA
//! server and asserts the server replies with a `Command::Search
//! Response` carrying the matching cid. Pre-PVA-R11 the dispatcher
//! had no `Command::Search` arm and the frame fell through
//! silently — a pvxs client doing name-server-redirect would hang
//! waiting for the response.
//!
//! pvxs source: `src/serverchan.cpp:173-255`.
//!
//! This test does not require pvxs binaries — it builds the SEARCH
//! frame from primitives and reads the response from a raw TCP
//! socket. That gives us byte-exact validation of the handler
//! without pulling in pvxs as a build dep.

use std::io::Read;
use std::time::Duration;

use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, ReadExt, WriteExt};

/// Build a TCP-circuit SEARCH frame for one PV name.
/// Wire layout (pvxs `clientdiscover.cpp:121-188` / Rust UDP
/// builder, identical body shape):
///
/// ```text
///   u32 searchSequenceID
///   u8  flags (bit 0x80 = unicast; this is TCP so set it)
///   3 × u8 reserved
///   16 × u8 reply address (zero — server should reply on this
///                          same TCP connection)
///   u16 reply port (0 — same)
///   Size(1) + "tcp" — protocols
///   u16 count = 1
///   u32 cid + Size + name
/// ```
fn build_tcp_search_frame(cid: u32, name: &str, order: ByteOrder) -> Vec<u8> {
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0xDEAD_BEEF, order); // sequence
    payload.put_u8(0x80); // unicast flag
    payload.put_u8(0);
    payload.put_u8(0);
    payload.put_u8(0);
    payload.extend_from_slice(&[0u8; 16]); // reply addr 0 → use TCP src
    payload.put_u16(0, order); // reply port
    // protocols: Size(1) + "tcp"
    payload.put_u8(1);
    payload.put_u8(b"tcp".len() as u8);
    payload.extend_from_slice(b"tcp");
    // queries: u16 count, then (cid u32, Size+name)
    payload.put_u16(1, order);
    payload.put_u32(cid, order);
    payload.put_u8(name.len() as u8);
    payload.extend_from_slice(name.as_bytes());

    let header = PvaHeader::application(false, order, Command::Search.code(), payload.len() as u32);
    let mut frame: Vec<u8> = Vec::with_capacity(PvaHeader::SIZE + payload.len());
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    frame
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_r11_tcp_circuit_search_returns_matching_cid() {
    use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

    // Set up a Rust PVA server hosting one PV.
    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))],
        },
        PvField::Structure(epics_pva_rs::pvdata::PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![(
                "value".to_string(),
                PvField::Scalar(ScalarValue::Double(0.0)),
            )],
        }),
    );
    let source = SharedSource::new();
    source.add("R11:TEST", pv);
    let source_arc = std::sync::Arc::new(source);

    let server = PvaServer::isolated(source_arc).expect("server start");
    let addr = server.tcp_addr();

    // Connect a plain TCP socket and run a minimal handshake.
    let stream = std::net::TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut stream = stream;

    // Step 1: read SET_BYTE_ORDER (control frame, 8 bytes).
    let mut prelude = [0u8; 8];
    stream
        .read_exact(&mut prelude)
        .expect("read SET_BYTE_ORDER");
    let order = if prelude[2] & 0x80 != 0 {
        ByteOrder::Big
    } else {
        ByteOrder::Little
    };

    // Step 2: drain the server's CONNECTION_VALIDATION request.
    // Header(8) + payload — we don't need to parse it.
    let mut hdr = [0u8; 8];
    stream.read_exact(&mut hdr).expect("read VALIDATION header");
    let payload_len = match order {
        ByteOrder::Big => u32::from_be_bytes(hdr[4..8].try_into().unwrap()),
        ByteOrder::Little => u32::from_le_bytes(hdr[4..8].try_into().unwrap()),
    } as usize;
    let mut body = vec![0u8; payload_len];
    stream.read_exact(&mut body).expect("read VALIDATION body");

    // Step 3: send a CONNECTION_VALIDATION reply with empty/
    // anonymous method (server accepts the null type Variant via
    // PVA-R22's parser).
    use std::io::Write;
    let mut val_reply: Vec<u8> = Vec::new();
    val_reply.put_u32(0x10000, order); // client buffer (R62: match pvxs 0x10000)
    val_reply.put_u16(32_767, order); // intro registry size
    val_reply.put_u16(0, order); // qos
    val_reply.put_u8(0); // empty method string (Size=0)
    let val_hdr = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        val_reply.len() as u32,
    );
    let mut buf: Vec<u8> = Vec::new();
    val_hdr.write_into(&mut buf);
    buf.extend_from_slice(&val_reply);
    stream.write_all(&buf).expect("write VALIDATION reply");

    // Step 4: drain CONNECTION_VALIDATED.
    let mut hdr = [0u8; 8];
    stream.read_exact(&mut hdr).expect("read VALIDATED header");
    let payload_len = match order {
        ByteOrder::Big => u32::from_be_bytes(hdr[4..8].try_into().unwrap()),
        ByteOrder::Little => u32::from_le_bytes(hdr[4..8].try_into().unwrap()),
    } as usize;
    let mut body = vec![0u8; payload_len];
    stream.read_exact(&mut body).expect("read VALIDATED body");

    // Step 5: send the SEARCH frame.
    let search = build_tcp_search_frame(0xABCD_0001, "R11:TEST", order);
    stream.write_all(&search).expect("write SEARCH");

    // Step 6: read frames until we see Command::SearchResponse.
    let response = loop {
        let mut hdr_buf = [0u8; 8];
        stream
            .read_exact(&mut hdr_buf)
            .expect("read response header");
        let is_control = hdr_buf[2] & 0x01 != 0;
        let cmd = hdr_buf[3];
        let payload_len = match order {
            ByteOrder::Big => u32::from_be_bytes(hdr_buf[4..8].try_into().unwrap()),
            ByteOrder::Little => u32::from_le_bytes(hdr_buf[4..8].try_into().unwrap()),
        } as usize;
        let mut body = vec![0u8; payload_len];
        if !is_control && payload_len > 0 {
            stream.read_exact(&mut body).expect("read response body");
        }
        if !is_control && cmd == Command::SearchResponse.code() {
            break body;
        }
        // Otherwise loop — server may interleave ECHO_RESPONSE or
        // other control frames.
    };

    // Step 7: parse the SearchResponse body and assert the cid we
    // sent appears. Layout: guid(12) + seq(4) + addr(16) +
    // port(2) + protocol-string + found(1) + count(2) + cids(N*4).
    let mut cur = std::io::Cursor::new(response.as_slice());
    let _ = cur.get_bytes(12).expect("guid");
    let _seq = cur.get_u32(order).expect("seq");
    let _ = cur.get_bytes(16).expect("addr");
    let _ = cur.get_u16(order).expect("port");
    // protocol string: Size(u8) + bytes (always < 254 for "tcp"/"tls").
    let proto_len = cur.get_u8().expect("proto len") as usize;
    let _ = cur.get_bytes(proto_len).expect("proto");
    let found = cur.get_u8().expect("found");
    assert_eq!(found, 1, "found flag should be 1 for matched PV");
    let count = cur.get_u16(order).expect("count");
    assert_eq!(count, 1, "should match exactly one cid");
    let echoed_cid = cur.get_u32(order).expect("cid");
    assert_eq!(echoed_cid, 0xABCD_0001, "cid round-trip");

    // Tear down.
    drop(stream);
    server.stop();
    let _ = _seq;
}

/// Second R11 case: a real pvxs `pvxget` configured with
/// `EPICS_PVA_NAME_SERVERS=<rust>:port` should resolve the PV and
/// fetch its value. This proves the Rust server's TCP SEARCH
/// handler interoperates with the actual pvxs client flow (not
/// just a hand-built frame). Pre-R11 the client would never get
/// a SEARCH_RESPONSE on the TCP circuit and would time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_r11_pvxget_via_name_server_resolves_pv_on_rust_server() {
    use super::interop_helpers::{PVXGET, pvxs_command, require_pvxs};
    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
    use std::sync::Arc;
    use std::time::Duration;

    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };

    // Rust server hosting a PV with a known value.
    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))],
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![(
                "value".to_string(),
                PvField::Scalar(ScalarValue::Double(42.5)),
            )],
        }),
    );
    let source = SharedSource::new();
    source.add("R11:NS:PV", pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    let output = tokio::task::spawn_blocking({
        let server_str = server_str.clone();
        move || {
            pvxs_command(&pvxget)
                .arg("-w")
                .arg("3")
                .arg("R11:NS:PV")
                .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
                .env("EPICS_PVA_ADDR_LIST", "")
                .env("EPICS_PVA_NAME_SERVERS", &server_str)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
        }
    })
    .await
    .expect("join pvxget");
    server.stop();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP: failed to spawn pvxget: {e}");
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "pvxget exited non-zero — Rust server TCP SEARCH did not resolve \
         R11:NS:PV via EPICS_PVA_NAME_SERVERS={server_str}.\n\
         stdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("value double = 42.5"),
        "pvxget output did not contain expected value 42.5.\n\
         stdout: {stdout}\nstderr: {stderr}",
    );

    // Brief breathing room so background TCP teardown completes
    // before the test process exits and trips the bind-port check
    // in the next test (cargo serialises within-binary tests).
    tokio::time::sleep(Duration::from_millis(50)).await;
}
