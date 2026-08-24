//! Diagnostic: dump pvxs softIocPVX's GET response bytes so we can see
//! exactly what's on the wire and where our decoder diverges.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command as TokioCommand;

/// Locate a pvxs binary in the resolved checkout (`PVXS_HOME` overrides it).
///
/// This read `PVXS_HOME` and `EPICS_HOST_ARCH` itself, defaulting the arch
/// to `darwin-aarch64`, so on any host that had set neither it returned
/// `None` and the test below printed a line and passed. The tree is now
/// resolved fatally; `None` means only that the binary is not built.
fn find_pvxs_bin(name: &str) -> Option<PathBuf> {
    let root =
        epics_base_rs::reference::reference_root(epics_base_rs::reference::ReferenceTree::Pvxs);
    let p = root
        .join("bin")
        .join(crate::interop_helpers::pvxs_arch())
        .join(name);
    if p.is_file() { Some(p) } else { None }
}

#[tokio::test]
#[ignore]
async fn dump_pvxs_get_init_response_bytes() {
    let Some(softioc) = find_pvxs_bin("softIocPVX") else {
        eprintln!("PVXS_HOME not set");
        return;
    };

    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        dbfile.path(),
        "record(ai, \"WIRE:VAL\") { field(VAL, \"42.5\") }\n",
    )
    .unwrap();

    let port = 25575u16;
    let mut child = TokioCommand::new(&softioc)
        .env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", "25576")
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            break;
        }
    }

    // Speak PVA by hand and dump every response.
    let mut sock = std::net::TcpStream::connect(server_addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    sock.set_nodelay(true).ok();

    // Server sends: SET_BYTE_ORDER + CONNECTION_VALIDATION request
    let mut buf = vec![0u8; 16384];
    let n = sock.read(&mut buf).unwrap();
    eprintln!("\n=== Server initial bytes ({n}) ===");
    eprintln!("{}", hex_dump(&buf[..n]));

    // Send our CONNECTION_VALIDATION reply: must include a Variant after
    // the auth method string (null variant = 0xFF for anonymous).
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, WriteExt};
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(87040, ByteOrder::Little);
    payload.put_u16(32767, ByteOrder::Little);
    payload.put_u16(0, ByteOrder::Little);
    epics_pva_rs::proto::encode_string_into("anonymous", ByteOrder::Little, &mut payload);
    payload.put_u8(0xFF); // null variant (no AuthZ block)
    let h = PvaHeader::application(
        false,
        ByteOrder::Little,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();

    let n = sock.read(&mut buf).unwrap();
    eprintln!("\n=== After CONN_VALIDATION reply ({n}) ===");
    eprintln!("{}", hex_dump(&buf[..n]));

    // CREATE_CHANNEL
    use epics_pva_rs::codec::PvaCodec;
    let codec = PvaCodec { big_endian: false };
    let cc = codec.build_create_channel(1, "WIRE:VAL");
    sock.write_all(&cc).unwrap();
    let n = sock.read(&mut buf).unwrap();
    eprintln!("\n=== CREATE_CHANNEL response ({n}) ===");
    eprintln!("{}", hex_dump(&buf[..n]));

    // Header (8) + cid (4) = sid at offset 12.
    let sid = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    eprintln!("sid={sid}");

    // GET_FIELD (no field selector)
    let gf = codec.build_get_field(sid, 1, "");
    sock.write_all(&gf).unwrap();
    let n = sock.read(&mut buf).unwrap();
    eprintln!("\n=== GET_FIELD response ({n}) ===");
    eprintln!("{}", hex_dump(&buf[..n]));

    // GET INIT (empty pvRequest sentinel)
    let pv_req = epics_pva_rs::pv_request::build_pv_request_value_only(false);
    let gi = codec.build_get_init(sid, 2, &pv_req);
    sock.write_all(&gi).unwrap();
    let n = sock.read(&mut buf).unwrap();
    eprintln!("\n=== GET_INIT response ({n}) ===");
    eprintln!("{}", hex_dump(&buf[..n]));

    // GET data
    let g = codec.build_get(sid, 2);
    sock.write_all(&g).unwrap();
    let n = sock.read(&mut buf).unwrap();
    eprintln!("\n=== GET DATA response ({n}) ===");
    eprintln!("{}", hex_dump(&buf[..n]));

    let _ = child.start_kill();
}

fn hex_dump(data: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in data.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i * 16));
        for (j, b) in chunk.iter().enumerate() {
            out.push_str(&format!("{:02x}", b));
            if j == 7 {
                out.push(' ');
            }
            out.push(' ');
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        if chunk.len() <= 8 {
            out.push(' ');
        }
        out.push_str("|");
        for &b in chunk {
            out.push(if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}
