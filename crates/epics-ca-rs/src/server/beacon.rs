use std::collections::HashMap;
#[cfg(all(feature = "cap-tokens", tokio_backend))]
use std::net::Ipv4Addr;
use std::net::SocketAddr;
#[cfg(tokio_backend)]
use std::sync::Arc;
use std::time::Duration;
#[cfg(tokio_backend)]
use tokio::net::UdpSocket;
#[cfg(tokio_backend)]
use tokio::sync::Notify;

use crate::protocol::*;
use epics_base_rs::runtime::taskwd::{self, CheckIn, TaskwdEntry};

/// The ramp's first period, C `online_notify.c:66` `delay = 0.02`.
const INITIAL_INTERVAL: Duration = Duration::from_millis(20);

/// C `rsrv_online_notify_task`'s state between two sends, whichever driver
/// sleeps it: the wire id, the ramp, and the per-destination error dedup.
pub(crate) struct BeaconCycle {
    server_port: u16,
    // `beacon_id` is what each beacon carries on the wire; `beacon_counter`
    // is the source counter. C `online_notify.c:124` sets
    // `msg.m_cid = htonl(beaconCounter++)` AFTER the send + sleep, so the
    // value placed on the wire lags the counter by one cycle: the first
    // two beacons both carry 0.
    beacon_id: u32,
    beacon_counter: u32,
    interval: Duration,
    max_interval: Duration,
    // Per-destination last-error dedup. Mirrors rsrv c23012d
    // (`rsrv_online_notify_task` `lastError[]`): a persistently
    // unreachable beacon address emits a sendto() failure every
    // beacon period. We log on first occurrence and on errno change,
    // then once on recovery — repeats are silent.
    send_errors: HashMap<SocketAddr, std::io::ErrorKind>,
}

impl BeaconCycle {
    pub(crate) fn new(server_port: u16, max_period: Duration) -> Self {
        Self {
            server_port,
            beacon_id: 0,
            beacon_counter: 0,
            interval: INITIAL_INTERVAL,
            max_interval: max_period.max(INITIAL_INTERVAL),
            send_errors: HashMap::new(),
        }
    }

    /// C `rsrv_online_notify_task` registers with the watchdog as its first
    /// act (`online_notify.c:50`) and removes on the way out (`:136`). This
    /// is the one CA server task with a period of its own, so it promises
    /// one: two of its steady-state intervals plus the watchdog's
    /// granularity, which the ramp-up cannot trip because that only sends
    /// faster. With nowhere to send there is no period to promise.
    pub(crate) fn watch(&self, has_destinations: bool) -> TaskwdEntry {
        taskwd::taskwd_insert(
            "CAS-beacon",
            if has_destinations {
                CheckIn::Every(self.max_interval * 2 + taskwd::TASKWD_DELAY)
            } else {
                CheckIn::Unbounded
            },
            None,
        )
    }

    /// The beacon to send now: CA_PROTO_RSRV_IS_UP.
    ///
    /// C `online_notify.c:69-72` emits beacons with `memset(&msg, 0, sizeof
    /// msg)` then sets only m_cmmd, m_count (server port), m_dataType
    /// (CA_MINOR_PROTOCOL_REVISION), and m_cid (beacon counter). `m_available`
    /// is left as 0 (INADDR_ANY) on the wire — and C client `udpiiu.cpp`
    /// explicitly documents (line 762): "new servers: always set this field
    /// to INADDR_ANY". When non-zero, the client treats it as the OVERRIDING
    /// server IP and bypasses the source-address resolution that handles NAT
    /// / multi-NIC / repeater fan-out.
    pub(crate) fn frame(&self) -> [u8; 16] {
        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.data_type = CA_MINOR_VERSION;
        hdr.count = self.server_port;
        hdr.cid = self.beacon_id;
        hdr.to_bytes()
    }

    #[cfg(all(feature = "cap-tokens", tokio_backend))]
    fn beacon_id(&self) -> u32 {
        self.beacon_id
    }

    /// Record one destination's send result, logging only a change.
    pub(crate) fn sent(&mut self, addr: SocketAddr, result: std::io::Result<usize>) {
        match result {
            Ok(_) => {
                if let Some(prev) = self.send_errors.remove(&addr) {
                    tracing::info!(
                        target: "epics_ca_rs::beacon",
                        %addr, prev_error = ?prev,
                        "CA beacon send recovered"
                    );
                }
            }
            Err(e) => {
                let kind = e.kind();
                if self.send_errors.insert(addr, kind) != Some(kind) {
                    tracing::warn!(
                        target: "epics_ca_rs::beacon",
                        %addr, error = %e,
                        "CA beacon send failed"
                    );
                }
            }
        }
    }

    /// How long to sleep after this cycle's send.
    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// The sleep ended: doubled up to the cap, or back to the start on a
    /// `reset`; then C `online_notify.c:124` latches the wire id.
    pub(crate) fn advance(&mut self, reset: bool) {
        self.interval = if reset {
            INITIAL_INTERVAL
        } else {
            (self.interval * 2).min(self.max_interval)
        };
        self.beacon_id = self.beacon_counter;
        self.beacon_counter = self.beacon_counter.wrapping_add(1);
    }
}

/// C `rsrv_init`'s `beaconSocket` (`caservertask.c:307-318`): unbound until
/// the first send, broadcast on, `EPICS_CA_MCAST_TTL` for multicast
/// destinations, and `IP_MULTICAST_LOOP` set explicitly — Linux defaults to
/// 1, other kernels and site policies do not, and a co-located repeater
/// subscribed to a multicast beacon group needs the loopback copy.
pub(crate) fn beacon_socket() -> std::io::Result<std::net::UdpSocket> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    let _ = socket.set_multicast_ttl_v4(epics_base_rs::runtime::net::ca_mcast_ttl());
    let _ = socket.set_multicast_loop_v4(true);
    Ok(socket)
}

/// The tokio `run_beacon_emitter`'s cycle on a blocking thread, on the
/// [`beacon_socket`] `rsrv_init` opened: `sleep(interval)` returns `true` when
/// serving ends, and the thread returns with it.
pub(crate) fn run_blocking_beacon_emitter(
    socket: &std::net::UdpSocket,
    server_port: u16,
    beacon_addrs: &[SocketAddr],
    max_period: Duration,
    mut sleep: impl FnMut(Duration) -> bool,
) {
    let mut cycle = BeaconCycle::new(server_port, max_period);
    let watched = cycle.watch(!beacon_addrs.is_empty());
    loop {
        watched.check_in();
        if !beacon_addrs.is_empty() {
            let bytes = cycle.frame();
            for addr in beacon_addrs {
                cycle.sent(*addr, socket.send_to(&bytes, addr));
            }
        }
        if sleep(cycle.interval()) {
            return;
        }
        cycle.advance(false);
    }
}

/// Run the beacon emitter. Broadcasts CA_PROTO_RSRV_IS_UP at exponentially
/// increasing intervals (starting at 20ms, doubling up to `max_period`).
///
/// `beacon_addrs` lists every destination — usually the per-interface
/// broadcast addresses plus any operator-supplied entries. Unicast routes
/// (e.g. site-wide gateways) are sent the same beacon stream.
///
/// When `reset` is notified, the emitter sends a beacon at once and the
/// interval goes back to the initial 20ms — C `rsrv`'s beacon anomaly, whose
/// point is that other clients re-search after a server state change.
///
/// Its ONE notifier is
/// [`CaServer::trigger_beacon_anomaly`](crate::server::CaServer::trigger_beacon_anomaly)
/// (also reachable as a long-lived handle through
/// [`CaServer::beacon_anomaly_handle`](crate::server::CaServer::beacon_anomaly_handle)),
/// which the bridge ca_gateway pulses when it registers a new upstream PV —
/// the `generateBeaconAnomaly` analogue. The TCP path does NOT pulse it: C
/// `rsrv_online_notify_task` sets the ramp's initial period once at task start
/// (`online_notify.c:66` `delay = 0.02`) and restarts it in exactly one other
/// place, the `beacon_ctl == ctlPause` wait loop (`online_notify.c:126-129`);
/// a client connect or disconnect never touches it. The port used to pulse on
/// every accept and every disconnect, which restarted the 20 ms ramp and made
/// every other client of the server flag `ShortPeriod` beacon anomalies (R6-30,
/// pinned by `tests/beacon_ramp_connect.rs`).
///
// The `signer` paragraph is gated on the same feature as the parameter it
// describes. Without `cap-tokens` there is no `signer` argument, so an
// ungated paragraph documented an argument the built function does not have
// — and its link to the equally gated `SignedBeaconEmitter` was the crate's
// last unresolved intra-doc link on a default-feature build.
#[cfg_attr(
    feature = "cap-tokens",
    doc = "",
    doc = "`signer` is an opt-in Ed25519 [`crate::server::signed_beacon::SignedBeaconEmitter`]",
    doc = "that emits a companion datagram immediately after each beacon so",
    doc = "clients with a configured keyring can authenticate the server",
    doc = "identity. C clients ignore the companion (unknown command); the",
    doc = "regular beacon stream is unchanged."
)]
///
/// `socket` is broadcast-enabled and opened before serving starts, as
/// `rsrv_init` opens it (`caservertask.c:297-299`), so the emitter itself
/// cannot fail.
#[cfg(tokio_backend)]
pub async fn run_beacon_emitter(
    socket: UdpSocket,
    server_port: u16,
    beacon_addrs: Vec<SocketAddr>,
    max_period: Duration,
    reset: Arc<Notify>,
    #[cfg(feature = "cap-tokens")] signer: Option<
        Arc<crate::server::signed_beacon::SignedBeaconEmitter>,
    >,
) -> std::convert::Infallible {
    // Signed-beacon companion (cap-tokens feature) binds the resolved IP
    // into its OWN signed payload, so a probe is kept for that path. The
    // companion datagram is a separate cmmd=0xCAFE message that C clients
    // ignore; it is not the main beacon, whose m_available stays 0.
    #[cfg(feature = "cap-tokens")]
    let server_ip: u32 = if signer.is_some() {
        let probe_dest = beacon_addrs
            .first()
            .copied()
            .unwrap_or(SocketAddr::from((Ipv4Addr::BROADCAST, CA_REPEATER_PORT)));
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok();
        probe
            .and_then(|s| {
                s.connect(probe_dest).ok()?;
                match s.local_addr().ok()? {
                    SocketAddr::V4(a) if !a.ip().is_unspecified() => {
                        Some(u32::from_be_bytes(a.ip().octets()))
                    }
                    _ => None,
                }
            })
            .unwrap_or(0)
    } else {
        0
    };

    let mut cycle = BeaconCycle::new(server_port, max_period);
    let watched = cycle.watch(!beacon_addrs.is_empty());

    if beacon_addrs.is_empty() {
        // Nothing to send to — quietly idle but still consume reset
        // notifications so the channel doesn't fill up.
        loop {
            reset.notified().await;
        }
    }

    loop {
        watched.check_in();
        let bytes = cycle.frame();
        for addr in &beacon_addrs {
            cycle.sent(*addr, socket.send_to(&bytes, addr).await);
        }

        // Signed-beacon companion: send a separate Ed25519-signed
        // datagram so clients with a configured keyring can verify
        // server identity. Same destinations.
        #[cfg(feature = "cap-tokens")]
        if let Some(ref s) = signer {
            s.emit(server_ip, server_port, cycle.beacon_id()).await;
        }

        let reset = tokio::select! {
            () = epics_base_rs::runtime::task::sleep(cycle.interval()) => false,
            () = reset.notified() => true,
        };
        cycle.advance(reset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blocking driver sends C's stream: the wire id lags the counter,
    /// the sleeps double from 20 ms, and a `true` from `sleep` ends it.
    #[test]
    fn the_blocking_emitter_sends_rsrvs_ramp_and_ids() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut sleeps = Vec::new();
        run_blocking_beacon_emitter(
            &beacon_socket().unwrap(),
            5064,
            &[rx.local_addr().unwrap()],
            Duration::from_millis(50),
            |d| {
                sleeps.push(d);
                sleeps.len() == 4
            },
        );
        let ms = |n| Duration::from_millis(n);
        assert_eq!(sleeps, [ms(20), ms(40), ms(50), ms(50)], "doubling, capped");

        let mut ids = Vec::new();
        let mut buf = [0u8; 64];
        for _ in 0..4 {
            let n = rx.recv(&mut buf).unwrap();
            assert_eq!(n, 16);
            let hdr = CaHeader::from_bytes(&buf[..n]).unwrap();
            assert_eq!(
                (hdr.cmmd, hdr.count, hdr.data_type, hdr.available),
                (CA_PROTO_RSRV_IS_UP, 5064, CA_MINOR_VERSION, 0)
            );
            ids.push(hdr.cid);
        }
        assert_eq!(
            ids,
            [0, 0, 1, 2],
            "online_notify.c:124 latches after the sleep"
        );
    }

    /// A reset restarts the ramp without touching the id sequence.
    #[test]
    fn a_reset_restarts_the_ramp_only() {
        let mut cycle = BeaconCycle::new(5064, Duration::from_secs(15));
        cycle.advance(false);
        cycle.advance(false);
        assert_eq!(cycle.interval(), Duration::from_millis(80));
        cycle.advance(true);
        assert_eq!(cycle.interval(), INITIAL_INTERVAL);
        assert_eq!(cycle.beacon_id, 2);
    }
}
