use std::collections::HashMap;
#[cfg(feature = "cap-tokens")]
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

use crate::protocol::*;
use epics_base_rs::error::CaResult;

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
pub async fn run_beacon_emitter(
    server_port: u16,
    beacon_addrs: Vec<SocketAddr>,
    max_period: Duration,
    reset: Arc<Notify>,
    #[cfg(feature = "cap-tokens")] signer: Option<
        Arc<crate::server::signed_beacon::SignedBeaconEmitter>,
    >,
) -> CaResult<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;
    // Honor `EPICS_CA_MCAST_TTL` (epics-base 3.16, f2a1834d). Only
    // affects multicast destinations; unicast and limited-broadcast
    // are unaffected. Beacon_addrs frequently includes multicast
    // groups when a site fans out beacons across routed segments.
    let _ = socket.set_multicast_ttl_v4(epics_base_rs::runtime::net::ca_mcast_ttl());
    // C `caservertask.c:307-318` `rsrv_init` explicitly
    // `setsockopt(beaconSocket, IPPROTO_IP, IP_MULTICAST_LOOP, &flag=1)`
    // on the beacon socket. Linux default is loop=1 so behaviour
    // happens to match, but non-Linux (BSD/macOS/Windows) and Linux
    // kernels with site policy disabling default loop diverge. A
    // local CA repeater or co-located client subscribed to multicast
    // beacons (a documented self-test pattern) needs the explicit-1
    // contract to receive loopback delivery.
    let _ = socket.set_multicast_loop_v4(true);

    // C `online_notify.c::rsrv_online_notify_task` (line 69-72) emits
    // beacons with `memset(&msg, 0, sizeof msg)` then sets only m_cmmd,
    // m_count (server port), m_dataType (CA_MINOR_PROTOCOL_REVISION),
    // and m_cid (beacon counter) per iteration. `m_available` is left
    // as 0 (INADDR_ANY) on the wire — and C client `udpiiu.cpp`
    // explicitly documents (line 762): "new servers: always set this
    // field to INADDR_ANY". When non-zero, the client treats it as
    // the OVERRIDING server IP and bypasses the source-address
    // resolution that handles NAT / multi-NIC / repeater fan-out.
    //
    // Our previous probe-based resolution wrote the resolved IP into
    // `m_available`, which (a) drifted from C byte-exact, (b) made
    // us look like an OLD server per the spec, and (c) could give
    // wrong results on multi-homed hosts where the probe destination
    // doesn't route through the correct NIC for every recipient.
    // Beacon fan-out (repeater + per-NIC sendto) already uses the
    // correct source IP per datagram, so the client can derive the
    // server address from the receive metadata.
    //
    // Signed-beacon companion (cap-tokens feature) still binds the
    // resolved IP into its OWN signed payload, so a probe is kept for
    // that path. The companion datagram is a separate cmmd=0xCAFE
    // message that C clients ignore; it is not the main beacon.
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

    // `beacon_id` is what each beacon carries on the wire; `beacon_counter`
    // is the source counter. C `online_notify.c:124` sets
    // `msg.m_cid = htonl(beaconCounter++)` AFTER the send + sleep, so the
    // value placed on the wire lags the counter by one cycle: the first
    // two beacons both carry 0 (the initial m_cid 0, then beaconCounter's
    // first read which is also 0). We mirror that lag: emit `beacon_id`,
    // then after send+sleep latch `beacon_id = beacon_counter` and
    // post-increment `beacon_counter`.
    let mut beacon_id: u32 = 0;
    let mut beacon_counter: u32 = 0;
    let initial_interval = Duration::from_millis(20);
    let max_interval = max_period.max(initial_interval);
    let mut interval = initial_interval;

    // Per-destination last-error dedup. Mirrors rsrv c23012d
    // (`rsrv_online_notify_task` `lastError[]`): a persistently
    // unreachable beacon address emits a sendto() failure every
    // beacon period (~15 s steady-state, faster during ramp-up). We
    // log on first occurrence and on errno change, then once on
    // recovery — repeats are silent.
    let mut send_errors: HashMap<SocketAddr, std::io::ErrorKind> = HashMap::new();

    // C `rsrv_online_notify_task` registers with the watchdog as its first act
    // (`online_notify.c:50`) and removes on the way out (`:136`). This is the
    // one CA server task with a period of its own, so it promises one: two of
    // its steady-state intervals plus the watchdog's granularity, which the
    // ramp-up cannot trip because that only sends faster. With nowhere to send
    // there is no period to promise, so the same registration carries no
    // deadline instead of a second one being made below.
    let watched = epics_base_rs::runtime::taskwd::taskwd_insert(
        "CAS-beacon",
        if beacon_addrs.is_empty() {
            epics_base_rs::runtime::taskwd::CheckIn::Unbounded
        } else {
            epics_base_rs::runtime::taskwd::CheckIn::Every(
                max_interval * 2 + epics_base_rs::runtime::taskwd::TASKWD_DELAY,
            )
        },
        None,
    );

    if beacon_addrs.is_empty() {
        // Nothing to send to — quietly idle but still consume reset
        // notifications so the channel doesn't fill up.
        loop {
            reset.notified().await;
        }
    }

    loop {
        watched.check_in();
        // Build beacon message: CA_PROTO_RSRV_IS_UP.
        //
        // m_available stays 0 (INADDR_ANY) per C `online_notify.c:69`
        // and the client-side comment in `udpiiu.cpp:762`. See the
        // top-of-loop block above for the full reasoning.
        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.data_type = CA_MINOR_VERSION;
        hdr.count = server_port;
        hdr.cid = beacon_id;
        let bytes = hdr.to_bytes();

        for addr in &beacon_addrs {
            match socket.send_to(&bytes, addr).await {
                Ok(_) => {
                    if let Some(prev) = send_errors.remove(addr) {
                        tracing::info!(
                            target: "epics_ca_rs::beacon",
                            %addr, prev_error = ?prev,
                            "CA beacon send recovered"
                        );
                    }
                }
                Err(e) => {
                    let kind = e.kind();
                    if send_errors.insert(*addr, kind) != Some(kind) {
                        tracing::warn!(
                            target: "epics_ca_rs::beacon",
                            %addr, error = %e,
                            "CA beacon send failed"
                        );
                    }
                }
            }
        }

        // Signed-beacon companion: send a separate Ed25519-signed
        // datagram so clients with a configured keyring can verify
        // server identity. Same destinations.
        #[cfg(feature = "cap-tokens")]
        if let Some(ref s) = signer {
            s.emit(server_ip, server_port, beacon_id).await;
        }

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
        // C `online_notify.c:124`: `msg.m_cid = htonl(beaconCounter++)`
        // runs here, after the send + sleep. Latch the wire id from the
        // counter, then post-increment the counter (wrapping).
        beacon_id = beacon_counter;
        beacon_counter = beacon_counter.wrapping_add(1);
    }
}
