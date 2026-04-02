use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

use crate::protocol::*;
use epics_base_rs::error::CaResult;

/// Run the beacon emitter. Broadcasts CA_PROTO_RSRV_IS_UP at exponentially
/// increasing intervals (starting at 20ms, doubling up to 15 seconds).
///
/// When `reset` is notified (e.g. on TCP connect/disconnect), the interval
/// resets to the initial 20ms, matching C EPICS behavior. This lets clients
/// detect server state changes quickly via beacon anomaly detection.
pub async fn run_beacon_emitter(server_port: u16, reset: Arc<Notify>) -> CaResult<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;

    let broadcast_addr = (Ipv4Addr::BROADCAST, CA_REPEATER_PORT);

    // Get server IP — use 0.0.0.0 to indicate "this host"
    let server_ip: u32 = 0;

    let mut beacon_id: u32 = 0;
    let initial_interval = Duration::from_millis(20);
    let max_interval = Duration::from_secs(15);
    let mut interval = initial_interval;

    loop {
        // Build beacon message: CA_PROTO_RSRV_IS_UP
        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.data_type = server_port;
        hdr.count = 0;
        hdr.cid = beacon_id;
        hdr.available = server_ip;

        let _ = socket.send_to(&hdr.to_bytes(), broadcast_addr).await;

        beacon_id = beacon_id.wrapping_add(1);

        tokio::select! {
            () = epics_base_rs::runtime::task::sleep(interval) => {
                if interval < max_interval {
                    interval = (interval * 2).min(max_interval);
                }
            }
            () = reset.notified() => {
                interval = initial_interval;
            }
        }
    }
}
