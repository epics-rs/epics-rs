use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use tokio::net::UdpSocket;

use crate::protocol::*;

/// Run the CA repeater daemon.
/// Binds to UDP 5065, accepts client registrations, and fans out beacons.
pub async fn run_repeater() -> std::io::Result<()> {
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, CA_REPEATER_PORT);
    let socket = UdpSocket::bind(bind_addr).await?;

    let mut clients: HashSet<SocketAddr> = HashSet::new();
    let mut buf = [0u8; 4096];

    loop {
        let (len, src) = socket.recv_from(&mut buf).await?;
        if len < CaHeader::SIZE {
            continue;
        }

        let Ok(hdr) = CaHeader::from_bytes(&buf[..len]) else {
            continue;
        };

        match hdr.cmmd {
            CA_PROTO_REPEATER_REGISTER => {
                // Client wants to register. The client's IP is in available field,
                // but we use the actual source address.
                clients.insert(src);

                // Send REPEATER_CONFIRM back
                let mut confirm = CaHeader::new(CA_PROTO_REPEATER_CONFIRM);
                // Put the client's IP in the available field
                if let SocketAddr::V4(v4) = src {
                    confirm.available = u32::from_be_bytes(v4.ip().octets());
                }
                let _ = socket.send_to(&confirm.to_bytes(), src).await;
            }
            _ => {
                // Beacon or other message from server — fan out to all registered clients
                let data = &buf[..len];
                // Remove dead clients on send failure
                let mut dead = Vec::new();
                for client in &clients {
                    // Don't echo back to the source
                    if *client == src {
                        continue;
                    }
                    if socket.send_to(data, client).await.is_err() {
                        dead.push(*client);
                    }
                }
                for d in dead {
                    clients.remove(&d);
                }
            }
        }
    }
}

/// Try to register with an existing repeater. If none is running, spawn one
/// as a background process using the current executable's `ca-repeater` binary,
/// then register again.
pub async fn ensure_repeater() {
    if try_register().await.is_ok() {
        return;
    }

    // No repeater running — spawn one
    spawn_repeater();

    // Give it a moment to start, then register
    crate::runtime::task::sleep(std::time::Duration::from_millis(100)).await;
    let _ = try_register().await;
}

/// Send a REPEATER_REGISTER to localhost:5065 and wait for CONFIRM.
async fn try_register() -> Result<(), ()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|_| ())?;

    let local_ip = match socket.local_addr().ok() {
        Some(SocketAddr::V4(v4)) => *v4.ip(),
        _ => Ipv4Addr::LOCALHOST,
    };

    let mut hdr = CaHeader::new(CA_PROTO_REPEATER_REGISTER);
    hdr.available = u32::from_be_bytes(local_ip.octets());

    let repeater_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, CA_REPEATER_PORT);
    socket
        .send_to(&hdr.to_bytes(), repeater_addr)
        .await
        .map_err(|_| ())?;

    // Wait for confirm with short timeout
    let mut buf = [0u8; 64];
    let result = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        loop {
            let (len, _) = socket.recv_from(&mut buf).await.map_err(|_| ())?;
            if len >= CaHeader::SIZE {
                if let Ok(resp) = CaHeader::from_bytes(&buf[..len]) {
                    if resp.cmmd == CA_PROTO_REPEATER_CONFIRM {
                        return Ok::<(), ()>(());
                    }
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// Spawn the repeater as a detached background process.
fn spawn_repeater() {
    // Use the current executable with a special subcommand
    let exe = std::env::current_exe().unwrap_or_default();

    // Try our own binary first (ca-repeater), fall back to spawning via cargo
    // We look for a `ca-repeater` binary next to the current executable
    let repeater_bin = exe.parent().map(|p| p.join("ca-repeater-rs"));

    let cmd = if let Some(ref bin) = repeater_bin {
        if bin.exists() {
            bin.clone()
        } else {
            // Fallback: cannot find repeater binary, skip
            return;
        }
    } else {
        return;
    };

    use std::process::{Command, Stdio};
    let _ = Command::new(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
