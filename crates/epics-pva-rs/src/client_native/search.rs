//! UDP search subsystem.
//!
//! Resolves a PV name → server `SocketAddr` by broadcasting a SEARCH
//! request and reading the SEARCH_RESPONSE. Honours the standard EPICS
//! environment variables:
//!
//! - `EPICS_PVA_ADDR_LIST` — comma/whitespace-separated address list
//! - `EPICS_PVA_AUTO_ADDR_LIST` — `YES`/`NO` (default `YES`); add discovered
//!   broadcast addresses
//! - `EPICS_PVA_BROADCAST_PORT` — UDP port to send to (default 5076)
//! - `EPICS_PVA_SERVER_PORT` — TCP port the server listens on (5075).
//!   Used as the response port.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use epics_base_rs::net::AsyncUdpV4;
use tokio::time::timeout;
use tracing::debug;

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};

use super::decode::{SearchResponse, decode_search_response, try_parse_frame};

/// Parse `EPICS_PVA_ADDR_LIST` style strings into a list of IPs/SocketAddrs.
pub fn parse_addr_list(env: &str) -> Vec<SocketAddr> {
    // PVA-466: $(VAR) / ${VAR} expansion against the process env
    // so callers of this legacy public parser see the same shell
    // macro semantics as the config::env path. Unset → empty.
    let env = crate::config::env::expand_dollar_vars(env);
    env.split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Accept "host:port" first, then plain IP.
            if let Ok(sa) = s.parse::<SocketAddr>() {
                return Some(sa);
            }
            if let Ok(ip) = s.parse::<IpAddr>() {
                return Some(SocketAddr::new(ip, default_broadcast_port()));
            }
            None
        })
        .collect()
}

pub fn default_broadcast_port() -> u16 {
    std::env::var("EPICS_PVA_BROADCAST_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(5076)
}

pub fn default_server_port() -> u16 {
    std::env::var("EPICS_PVA_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(5075)
}

fn auto_addr_list_enabled() -> bool {
    match std::env::var("EPICS_PVA_AUTO_ADDR_LIST") {
        Ok(v) => {
            let v = v.trim().to_ascii_uppercase();
            v == "YES" || v == "Y" || v == "1" || v == "TRUE"
        }
        Err(_) => true,
    }
}

/// Build a list of UDP destinations to broadcast SEARCH to.
pub fn build_search_targets(extra: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut targets: Vec<SocketAddr> = Vec::new();
    let mut seen = HashSet::new();

    let push = |addr: SocketAddr, targets: &mut Vec<SocketAddr>, seen: &mut HashSet<SocketAddr>| {
        if seen.insert(addr) {
            targets.push(addr);
        }
    };

    for &addr in extra {
        push(addr, &mut targets, &mut seen);
    }

    if let Ok(env) = std::env::var("EPICS_PVA_ADDR_LIST") {
        for addr in parse_addr_list(&env) {
            push(addr, &mut targets, &mut seen);
        }
    }

    if auto_addr_list_enabled() {
        // Auto: limited broadcast 255.255.255.255 (works on most LANs without
        // requiring per-NIC bookkeeping). Multi-NIC users can add EPICS_PVA_ADDR_LIST.
        push(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), default_broadcast_port()),
            &mut targets,
            &mut seen,
        );
    }

    targets
}

/// Bind a per-NIC UDP socket bundle suitable for SEARCH: every NIC
/// shares one ephemeral port so embedded `response_port` works
/// regardless of which NIC delivers the IOC's reply.
fn bind_broadcast_socket() -> PvaResult<AsyncUdpV4> {
    AsyncUdpV4::bind_ephemeral_same_port(true).map_err(PvaError::Io)
}

/// Send a SEARCH for `pv_name` and wait for a SEARCH_RESPONSE.
///
/// Retries with exponential-ish backoff up to `timeout`. If a response is
/// received before the deadline, returns the discovered server address.
pub async fn search(pv_name: &str, total_timeout: Duration) -> PvaResult<SocketAddr> {
    let socket = bind_broadcast_socket()?;
    let response_port = socket.local_addrs().first().map(|a| a.port()).unwrap_or(0);

    let codec = PvaCodec { big_endian: false };
    let targets = build_search_targets(&[]);
    if targets.is_empty() {
        return Err(PvaError::Protocol(
            "no search targets (set EPICS_PVA_ADDR_LIST or enable AUTO_ADDR_LIST)".into(),
        ));
    }

    let search_id = 1u32;
    let deadline = Instant::now() + total_timeout;
    let mut sequence = 0u32;
    let mut attempt: u32 = 0;

    loop {
        if Instant::now() >= deadline {
            return Err(PvaError::Timeout);
        }
        sequence = sequence.wrapping_add(1);

        let pkt = codec.build_search(
            sequence,
            search_id,
            pv_name,
            [0, 0, 0, 0],
            response_port,
            false,
        );
        for &target in &targets {
            // Limited broadcast / multicast destinations need explicit
            // per-NIC fanout. Per-subnet broadcast and unicast targets
            // ride AsyncUdpV4's automatic NIC selection.
            let needs_fanout = match target {
                SocketAddr::V4(v4) => v4.ip().is_broadcast() || v4.ip().is_multicast(),
                SocketAddr::V6(_) => false,
            };
            let result = if needs_fanout {
                socket.fanout_to(&pkt, target).await.map(|_| ())
            } else {
                socket.send_to(&pkt, target).await.map(|_| ())
            };
            // Non-fatal: a single bad NIC shouldn't kill the whole search.
            if let Err(e) = result {
                debug!("search send to {target} failed: {e}");
            }
        }

        let now = Instant::now();
        let backoff = std::cmp::min(
            Duration::from_millis(100 << attempt.min(5)),
            Duration::from_secs(2),
        );
        let next_send = now + backoff;
        let wait = std::cmp::min(next_send, deadline).saturating_duration_since(now);

        let mut buf = vec![0u8; 1500];
        match timeout(wait, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _from))) => {
                if let Ok(Some((frame, _))) = try_parse_frame(&buf[..n]) {
                    if let Ok(resp) = decode_search_response(&frame) {
                        if resp.found && resp.cids.contains(&search_id) {
                            return Ok(rewrite_loopback_target(&resp));
                        }
                    }
                }
                // Otherwise: not our response; loop and keep listening.
            }
            Ok(Err(e)) => {
                debug!("search recv error: {e}");
            }
            Err(_) => {
                // Timeout for this attempt — increase backoff and resend.
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Some servers report 0.0.0.0 as their TCP address (meaning "use the source
/// address of this UDP packet"); rewrite that to a useful loopback addr.
fn rewrite_loopback_target(resp: &SearchResponse) -> SocketAddr {
    if resp.server_addr.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), resp.server_addr.port())
    } else {
        resp.server_addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_list() {
        let v = parse_addr_list("127.0.0.1, 192.168.1.1:5076 , 10.0.0.1");
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].port(), 5076);
        assert_eq!(v[1].port(), 5076);
        assert_eq!(v[1].ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn parse_skips_empty() {
        assert!(parse_addr_list("").is_empty());
        assert!(parse_addr_list(" , ,").is_empty());
    }
}
