// RTEMS-EXEC-MODEL-ALLOW(4): the four flavored tests drive tokio::net
// UDP beacon traffic, which needs the reactor. These run and pass in the
// feature-ON suite on the tokio driver.
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use epics_base_rs::net::AsyncUdpV4;
use epics_base_rs::runtime::sync::mpsc;

use crate::protocol::*;

use super::CoordRequest;

/// Control messages sent INTO the beacon monitor by the coordinator
/// (currently only on TCP-circuit (re)connect). libca `bhe.cpp`
/// resets `averagePeriod` when a fresh client circuit comes up, on
/// the reasoning that an active TCP handshake is fresh evidence the
/// server is alive *now* and any prior beacon-cadence measurements
/// may be stale (the server may have restarted with its beacon
/// counter preserved, OR an older steady-state EMA may misclassify
/// the standard rsrv `online_notify_task` ramp-up as a
/// PeriodCollapse cascade — the cited symptom in archiver-rs's
/// reconnect logs).
pub(crate) enum BeaconControl {
    /// Clear `period_estimate` and `count` for `server_addr` so the
    /// EMA re-establishes from the next observed inter-beacon
    /// interval. `last_id` and `last_seen` are intentionally kept so
    /// the duplicate-detection and stale-prune paths still work.
    ResetServer { server_addr: SocketAddr },
}

/// Why the beacon monitor decided this beacon is "anomalous".
///
/// `FirstSighting` is benign from the *server's* point of view — the
/// IOC is fine, we just hadn't been listening before (or had pruned
/// its `BeaconState` after `BEACON_STALE_THRESHOLD`). It still
/// matters for the search engine: channels stuck in `Searching` /
/// `Disconnected` should re-search immediately because we now know
/// the server is alive. It does NOT justify probing the TCP circuit
/// of operational channels — by definition we already have a working
/// circuit, and an extra EchoProbe under load just risks tripping the
/// 5-s echo timeout in `transport.rs`.
///
/// `IdMismatch` is the sole real-restart signal and warrants the full
/// treatment (search wake-up + EchoProbe to operational circuits, so
/// a half-dead TCP gets surfaced fast).
///
/// `LongPeriod` / `ShortPeriod` are libca's two beacon-period bands
/// (`bhe.cpp:226-262`). They only label the search wake-up; the
/// per-circuit watchdog flag is carried by the separate
/// `CoordRequest::BeaconArrival` path, exactly as in libca, where
/// `bhe::beaconAnomalyNotify` (circuit watchdog) and `updatePeriod`'s
/// `netChange` return value (search timer) are independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BeaconAnomalyKind {
    FirstSighting,
    IdMismatch,
    /// `currentPeriod >= averagePeriod * 3.25` — libca's "3 contiguous
    /// missing beacons" band (`bhe.cpp:232-238`): the server was
    /// unreachable and is back, so unresolved channels re-search.
    LongPeriod,
    /// `currentPeriod <= averagePeriod * 0.80` — libca's IOC-reboot
    /// band (`bhe.cpp:255-259`): beacons come faster right after a
    /// reboot because rsrv's `online_notify_task` restarts its 0.02 s
    /// ramp-up (`online_notify.c:66`).
    ShortPeriod,
}

/// libca `bhe.cpp:226` — `currentPeriod >= averagePeriod * 1.25` means
/// at least one beacon went missing: flag the circuit watchdog.
const BEACON_LONG_PERIOD_FACTOR: f64 = 1.25;

/// libca `bhe.cpp:232` — `>= averagePeriod * 3.25` means ~3 contiguous
/// beacons went missing: additionally wake the search engine
/// (`netChange`).
const BEACON_NET_CHANGE_FACTOR: f64 = 3.25;

/// libca `bhe.cpp:255` — `currentPeriod <= averagePeriod * 0.80`: the
/// IOC is beaconing faster than its running average, i.e. it rebooted
/// into the ramp-up phase. Flags the watchdog AND wakes searches.
const BEACON_SHORT_PERIOD_FACTOR: f64 = 0.80;

/// What one beacon must trigger, mirroring libca's two independent
/// notifications in `bhe::updatePeriod` (`bhe.cpp:186-266`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BeaconAction {
    /// `Some(kind)` ⇔ `updatePeriod` returned `netChange`, so
    /// `cac::beaconNotify` (`cac.cpp:500`) calls
    /// `udpiiu::beaconAnomalyNotify` and the search timer restarts for
    /// unresolved channels. `kind` only labels the log line.
    rescan: Option<BeaconAnomalyKind>,
    /// `Some(true)` ⇔ `bhe::beaconAnomalyNotify` (sticky flag on the
    /// per-circuit receive watchdog); `Some(false)` ⇔
    /// `tcpiiu::beaconArrivalNotify` (healthy beacon — push the
    /// receive deadline forward); `None` ⇔ neither.
    watchdog: Option<bool>,
}

/// libca `bhe.cpp:268` — the running beacon period is an exponential
/// moving average with a 0.125 smoothing factor:
///
/// ```text
/// this->averagePeriod = currentPeriod * 0.125 + this->averagePeriod * 0.875;
/// ```
///
/// The value is what the anomaly bands are measured against, so the
/// factor is not free: a larger alpha lets the average chase the sample
/// and shrinks the effective width of both bands (a run of stretched
/// intervals stops reading as stretched after two or three of them).
const BEACON_PERIOD_ALPHA: f64 = 0.125;

/// Blend one inter-beacon interval into the running average.
///
/// `None` is libca's `averagePeriod = -DBL_MAX` sentinel: the 2nd
/// beacon adopts the first measured interval outright (`bhe.cpp:199`)
/// and only later samples are smoothed (`bhe.cpp:267-268`).
fn update_average_period(average: Option<Duration>, current: Duration) -> Duration {
    match average {
        None => current,
        Some(prev) => Duration::from_secs_f64(
            current.as_secs_f64() * BEACON_PERIOD_ALPHA
                + prev.as_secs_f64() * (1.0 - BEACON_PERIOD_ALPHA),
        ),
    }
}

/// libca `bhe::updatePeriod` (`bhe.cpp:226-262`) period classification,
/// for a server whose running average is already established.
///
/// The bands are checked against the average as it stood *before* this
/// sample is blended in — C updates `averagePeriod` only after the
/// `if/else if/else` chain (`bhe.cpp:267`).
fn classify_period(average: Duration, current: Duration) -> BeaconAction {
    let avg = average.as_secs_f64();
    let cur = current.as_secs_f64();
    if cur >= avg * BEACON_LONG_PERIOD_FACTOR {
        BeaconAction {
            rescan: (cur >= avg * BEACON_NET_CHANGE_FACTOR)
                .then_some(BeaconAnomalyKind::LongPeriod),
            watchdog: Some(true),
        }
    } else if cur <= avg * BEACON_SHORT_PERIOD_FACTOR {
        BeaconAction {
            rescan: Some(BeaconAnomalyKind::ShortPeriod),
            watchdog: Some(true),
        }
    } else {
        BeaconAction {
            rescan: None,
            watchdog: Some(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-server beacon state
// ---------------------------------------------------------------------------

struct BeaconState {
    last_id: u32,
    last_seen: Instant,
    /// Estimated period between beacons (exponential moving average,
    /// alpha = [`BEACON_PERIOD_ALPHA`]). `None` until the second beacon arrives — at
    /// which point we adopt the first observed inter-beacon
    /// interval as the initial estimate. Mirrors libca `bhe.cpp:51`
    /// where `averagePeriod = -DBL_MAX` is the "no estimate yet"
    /// sentinel and `bhe.cpp:199` sets it to the first measured
    /// `currentPeriod`.
    ///
    /// Why this matters: hardcoding the initial estimate (we used
    /// `Duration::from_secs(15)`) made the EMA start from a value
    /// that was unrelated to the actual server's beacon cadence.
    /// During the standard rsrv `online_notify_task` ramp-up
    /// (server-side beacon emitter starts at 20 ms and doubles up
    /// to 15 s — which `epics-ca-rs/src/server/beacon.rs` also
    /// implements), the first 4-8 beacons all have intervals well
    /// below 15 s / 3, so the PeriodCollapse branch (which fires
    /// when `actual_interval < period_estimate / 3`) tripped on
    /// every one of them. That cascaded into the transport
    /// watchdog flag → echo probe → 5 s timeout → spurious
    /// disconnect → user-visible `get_with_metadata(timeout=2.0)`
    /// failures observed in the mini-beamline IOC against its own
    /// epics-ca-rs server.
    period_estimate: Option<Duration>,
    count: u64,
}

/// Idle threshold after which a tracked server is forgotten. Mirrors
/// pvxs `beaconCleanInterval` (`client.cpp` 2 × 180 s default). When a
/// long-silent server resumes beacons, the next sighting becomes
/// `first_sighting = true` and naturally takes the anomaly path —
/// without this prune, in-sequence beacons after long silence would
/// keep `first_sighting = false` and miss the rescan kick. This
/// replaces the previous "soft poke on every beacon" mechanism, which
/// caused steady-state amplification (multi-IOC networks beaconing
/// within ~6 s aggregate kept the search engine in 200 ms fast-tick
/// mode indefinitely).
const BEACON_STALE_THRESHOLD: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------
// Beacon monitor task
// ---------------------------------------------------------------------------

/// Receives beacon messages from the CA repeater, detects anomalies (IOC
/// restart), and notifies the coordinator to rescan affected channels.
/// Re-registration interval: if no beacons for this long, re-register
/// with the repeater in case it restarted.
const REREGISTER_INTERVAL: Duration = Duration::from_secs(300);

/// C `repeaterSubscribeTimerPeriod` (`repeaterSubscribeTimer.cpp:31`).
///
/// While the repeater has not CONFIRMed, libca re-sends `REPEATER_REGISTER`
/// on every expiry and returns `expireStatus(restart, 1.0)`
/// (`repeaterSubscribeTimer.cpp:84-90`) — it never gives up. `registered` is
/// set by exactly one thing: `confirmNotify`, called from
/// `udpiiu::repeaterAckAction` (`udpiiu.cpp:793`) when a
/// `CA_PROTO_REPEATER_CONFIRM` datagram arrives. A successful `sendto` proves
/// nothing: the repeater may not be bound yet.
const REPEATER_SUBSCRIBE_PERIOD: Duration = Duration::from_secs(1);

/// C `nTriesToMsg` (`repeaterSubscribeTimer.cpp:70`) — after this many
/// unconfirmed attempts libca prints a one-shot diagnostic and keeps trying.
const REPEATER_TRIES_TO_MSG: u32 = 50;

/// `repeater_port` is the client's single resolution of
/// `EPICS_CA_REPEATER_PORT` — C `udpiiu` resolves it once in its
/// constructor (`udpiiu.cpp:168`) and every registration retry sends to
/// that stored member.
pub(crate) async fn run_beacon_monitor(
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    control_rx: mpsc::UnboundedReceiver<BeaconControl>,
    repeater_port: u16,
) {
    run_beacon_monitor_inner(
        coord_tx,
        control_rx,
        repeater_port,
        #[cfg(feature = "cap-tokens")]
        None,
    )
    .await;
}

/// Variant that gates beacon acceptance on a [`SignedBeaconVerifier`].
/// When `verifier` is `Some(...)`, the monitor only forwards beacons
/// to the search engine after a valid companion datagram (cmmd=0xCAFE,
/// see [`crate::server::signed_beacon`]) has been received and
/// verified for the same (server, beacon_id) within the
/// `max_age_secs` window.
#[cfg(feature = "cap-tokens")]
#[allow(dead_code)]
pub(crate) async fn run_beacon_monitor_with_verifier(
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    control_rx: mpsc::UnboundedReceiver<BeaconControl>,
    repeater_port: u16,
    verifier: std::sync::Arc<crate::server::signed_beacon::SignedBeaconVerifier>,
) {
    run_beacon_monitor_inner(coord_tx, control_rx, repeater_port, Some(verifier)).await;
}

async fn run_beacon_monitor_inner(
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    mut control_rx: mpsc::UnboundedReceiver<BeaconControl>,
    repeater_port: u16,
    #[cfg(feature = "cap-tokens")] verifier: Option<
        std::sync::Arc<crate::server::signed_beacon::SignedBeaconVerifier>,
    >,
) {
    // C `udpiiu` binds this socket to INADDR_ANY (`udpiiu.cpp:241-249`,
    // "force a bind to an unconstrained address"), and the registration
    // retry it drives ALTERNATES its destination between the loopback
    // address and `osiLocalAddr()` (the first non-loopback NIC,
    // `udpiiu.cpp:494-519`) for pre-3.13-beta-12 repeater compatibility.
    // A loopback-ONLY bind cannot reach the `osiLocalAddr()` destination
    // on Windows (a socket bound to `127.0.0.1` may only send over the
    // loopback interface), so every odd-numbered registration was silently
    // dropped there and the compatibility alternation was defeated. Bind
    // the same-port bundle across every NIC (loopback + non-loopback) —
    // the AsyncUdpV4 analogue of INADDR_ANY — so BOTH destinations are
    // reachable and, because all NIC sockets share ONE ephemeral port, the
    // repeater keys the alternating datagrams to a single client by port
    // (C `identicalPort`, `repeater.cpp:428`) exactly as C's single
    // INADDR_ANY socket does.
    let socket = match AsyncUdpV4::bind_ephemeral_same_port(false) {
        Ok(s) => s,
        Err(_) => return,
    };
    // pvxs `udp_collector.cpp` parity (commit a064677e3625): opt
    // the kernel into SO_RXQ_OVFL so a sustained beacon backlog
    // (slow main loop, undersized SO_RCVBUF, mass-restart storm)
    // surfaces as a debug log instead of silent loss. No-op on
    // non-Linux. Diagnostic-only failure is logged at trace.
    if let Err(e) = socket.enable_so_rxq_ovfl() {
        tracing::trace!(
            target: "epics_ca_rs::client::beacon_monitor",
            error = %e,
            "SO_RXQ_OVFL enable failed (non-fatal)"
        );
    }
    let mut prev_drops_beacon: u32 = 0;

    // Repeater registration state, mirroring C `repeaterSubscribeTimer`
    // (`repeaterSubscribeTimer.cpp`). `registered` flips only on a
    // `CA_PROTO_REPEATER_CONFIRM` (C `confirmNotify`), and while it is false
    // the 1 s ticker below re-sends `REPEATER_REGISTER` forever. The old
    // three-attempt loop gave up after ~2 s and then left the client without
    // beacon fan-out until the 5-minute silence timer — a repeater that came
    // up late (cold start, repeater restart) was never registered with.
    //
    // The CONFIRM is recognised by the main receive loop, not by a private
    // recv inside the sender: the old `register_with_repeater` drained the
    // socket for 500 ms looking for its CONFIRM and silently discarded every
    // beacon that arrived in that window.
    let mut registered = false;
    let mut attempts: u32 = 0;
    let mut tries_msg_shown = false;
    // `interval` fires its first tick immediately, so the initial
    // registration still goes out at startup rather than one period later.
    let mut subscribe_tick = tokio::time::interval(REPEATER_SUBSCRIBE_PERIOD);
    subscribe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // When `verifier` is set, this map remembers which
    // (server_ip, server_port, beacon_id) tuples have been
    // authenticated by a recent companion datagram. Beacons whose
    // tuple isn't here within `max_age_secs` get dropped (or merely
    // counted, when `require_signed` is false).
    #[cfg(feature = "cap-tokens")]
    let mut verified_tuples: HashMap<(u32, u16, u32), std::time::Instant> = HashMap::new();
    #[cfg(feature = "cap-tokens")]
    let require_signed = !matches!(
        epics_base_rs::runtime::env::get("EPICS_CA_BEACON_REQUIRE_SIGNED").as_deref(),
        Some("NO" | "no" | "0" | "false" | "FALSE")
    );
    let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
    // EPICS_RS_CLIENT_IGNORE snapshot — Rust-only client-side IP
    // quarantine (NOT C `EPICS_IOC_IGNORE_SERVERS`, which is
    // server-side; see super::epics_rs_client_ignore docstring).
    // Captured at task start so the beacon hot path stays env-read-
    // free; admins restart the IOC to apply a new ignore list.
    let ignored_servers: std::collections::HashSet<Ipv4Addr> =
        super::epics_rs_client_ignore().into_iter().collect();
    // Beacons are 16 B but the repeater may concatenate VERSION + RSRV_IS_UP
    // and forward client-noop traffic. Use 4 KB so chained datagrams are
    // received intact.
    let mut buf = [0u8; 4096];
    // Set to false once the control channel's last sender drops
    // (CaClient shutdown). After that we stop polling that branch so
    // we don't busy-loop on Ready(None); UDP / re-register continue.
    let mut control_rx_open = true;

    loop {
        // libca bhe-on-connect parity: a coordinator-issued
        // ResetServer (sent on TransportEvent::ServerConnected) clears
        // the per-server EMA so the next beacon reseeds
        // `period_estimate` from the live cadence. Without this, an
        // archiver that reconnects to a server whose `online_notify`
        // ramp-up is in progress sees a PeriodCollapse cascade against
        // its stale steady-state estimate.
        let recv_fut = tokio::time::timeout(
            REREGISTER_INTERVAL,
            socket.recv_with_meta_with_drops(&mut buf),
        );
        let (meta, drops) = tokio::select! {
            // C `repeaterSubscribeTimer::expire` (`repeaterSubscribeTimer.cpp:66-91`):
            // send a registration on every expiry and reschedule at 1 s for as
            // long as the repeater has not confirmed. Disabled once it has,
            // exactly like C's `noRestart`.
            _ = subscribe_tick.tick(), if !registered => {
                if attempts > REPEATER_TRIES_TO_MSG && !tries_msg_shown {
                    // C prints this once and keeps trying
                    // (`repeaterSubscribeTimer.cpp:70-80`).
                    tracing::warn!(
                        target: "epics_ca_rs::client::beacon_monitor",
                        tries = REPEATER_TRIES_TO_MSG,
                        "CA client library is unable to contact CA repeater after \
                         {REPEATER_TRIES_TO_MSG} tries. Silence this message by \
                         starting a CA repeater daemon."
                    );
                    tries_msg_shown = true;
                }
                // C passes the attempt number into the registration
                // (`repeaterSubscribeTimer.cpp:83` `repeaterRegistrationMessage
                // (this->attempts)`) — it selects the odd/even address form.
                let _ = send_repeater_registration(&socket, attempts, repeater_port).await;
                attempts = attempts.saturating_add(1);
                continue;
            }
            ctrl = control_rx.recv(), if control_rx_open => {
                match ctrl {
                    Some(BeaconControl::ResetServer { server_addr }) => {
                        apply_reset_server(&mut servers, server_addr);
                    }
                    None => {
                        control_rx_open = false;
                    }
                }
                continue;
            }
            recv = recv_fut => {
                match recv {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => continue,
                    Err(_) => {
                        // No beacons for 5 minutes — the repeater may have
                        // restarted and forgotten us. Drop back into the
                        // unregistered state so the 1 s ticker above resumes
                        // until a fresh CONFIRM arrives, instead of firing one
                        // unverified datagram every 5 minutes.
                        registered = false;
                        attempts = 0;
                        continue;
                    }
                }
            }
        };
        if drops != 0 && drops != prev_drops_beacon {
            tracing::debug!(
                target: "epics_ca_rs::client::beacon_monitor",
                prev = prev_drops_beacon,
                drops,
                "CA beacon RX socket buffer overflow"
            );
        }
        prev_drops_beacon = drops;
        let len = meta.n;
        if len < CaHeader::SIZE {
            continue;
        }

        // Walk every CA frame in the datagram so chained beacons aren't
        // dropped when the repeater coalesces them.
        let mut offset = 0;
        while offset + CaHeader::SIZE <= len {
            let Ok(hdr) = CaHeader::from_bytes(&buf[offset..len]) else {
                break;
            };
            // C `rsrv/camessage.c:2520` rejects misaligned m_postsize.
            // UDP path drops silently. Without this check, the
            // round-up below would advance into the next message's
            // header bytes.
            if (hdr.postsize as usize) & 0x7 != 0 {
                break;
            }
            let payload_padded = hdr.postsize as usize;
            let frame_len = (CaHeader::SIZE + payload_padded).max(CaHeader::SIZE);
            // Bail out before advancing if the announced frame
            // length runs past the datagram. Otherwise the
            // post-advance slice clamp would silently hand the
            // verifier a truncated body and the parser would
            // continue from a misaligned offset.
            if offset.saturating_add(frame_len) > len {
                break;
            }
            // Used by the cap-tokens companion-frame slice below; the
            // attribute keeps the unused-variable lint quiet when the
            // feature is off.
            #[cfg_attr(not(feature = "cap-tokens"), allow(unused_variables))]
            let frame_start = offset;
            offset += frame_len;

            // Signed-beacon companion (cmmd=0xCAFE, cap-tokens
            // feature). Verify the signature and stash the tuple as
            // "authenticated" so the matching beacon is acceptable.
            #[cfg(feature = "cap-tokens")]
            if hdr.cmmd == crate::server::signed_beacon::CA_PROTO_RSRV_BEACON_SIG {
                if let Some(ref v) = verifier {
                    let frame = &buf[frame_start..frame_start + frame_len];
                    // G3: bind the signed payload's announced server_ip
                    // to the UDP source IP — but only when the datagram
                    // didn't transit a repeater. The CA repeater
                    // (`repeater.cpp:626-630`, our `repeater.rs:224-228`)
                    // forwards the companion verbatim while the kernel
                    // rewrites the L3 source to the repeater's local
                    // socket address (typically 127.0.0.1). Under the
                    // standard production topology the client beacon
                    // socket is bound to LOCALHOST (line 160) and ONLY
                    // receives via the repeater, so `meta.src` is
                    // always loopback and a strict G3 binding would
                    // reject every legitimate companion.
                    //
                    // Replay protection without G3 here rests on (a)
                    // the cryptographic signature over (server_ip,
                    // server_port, beacon_id, ts) — an attacker can't
                    // mint a fresh tuple without the signing key —
                    // and (b) the `ts` freshness window enforced in
                    // SignedBeaconVerifier::verify. G3 still fires
                    // when a non-loopback path is observed (e.g. a
                    // future direct-LAN deployment without a
                    // repeater).
                    let src_ip = match meta.src.ip() {
                        std::net::IpAddr::V4(v) => v,
                        std::net::IpAddr::V6(_) => {
                            metrics::counter!("ca_client_signed_beacon_failures_total")
                                .increment(1);
                            continue;
                        }
                    };
                    let via_repeater = src_ip.is_loopback();
                    match v.verify(frame) {
                        Ok((ip, port, beacon_id))
                            if !via_repeater && Ipv4Addr::from(ip) != src_ip =>
                        {
                            tracing::debug!(
                                announced = %Ipv4Addr::from(ip),
                                actual = %src_ip,
                                port, beacon_id,
                                "signed beacon source-IP mismatch (G3)"
                            );
                            metrics::counter!("ca_client_signed_beacon_source_ip_mismatch_total")
                                .increment(1);
                        }
                        Ok((ip, port, beacon_id)) => {
                            // G2: cap verified_tuples on the companion-
                            // only path. The unsigned-beacon path GC's
                            // it via retain() at line 181, but a peer
                            // sending only signed companions would
                            // otherwise grow it linearly.
                            const MAX_VERIFIED_TUPLES: usize = 8192;
                            if verified_tuples.len() >= MAX_VERIFIED_TUPLES {
                                let max_age = std::time::Duration::from_secs(v.max_age_secs.max(1));
                                let now = std::time::Instant::now();
                                verified_tuples.retain(|_, t| now.duration_since(*t) <= max_age);
                            }
                            verified_tuples
                                .insert((ip, port, beacon_id), std::time::Instant::now());
                            metrics::counter!("ca_client_signed_beacon_verified_total")
                                .increment(1);
                        }
                        Err(e) => {
                            tracing::debug!(error = ?e,
                                "signed beacon companion failed verification");
                            metrics::counter!("ca_client_signed_beacon_failures_total")
                                .increment(1);
                        }
                    }
                }
                continue;
            }

            // C `udpiiu::repeaterAckAction` (`udpiiu.cpp:790-795`) — the ONLY
            // thing that marks the client registered. Handled here, on the
            // socket's one reader, so a CONFIRM can never be swallowed by a
            // private recv and a beacon can never be swallowed while we wait
            // for one.
            if hdr.cmmd == CA_PROTO_REPEATER_CONFIRM {
                if !registered {
                    tracing::debug!(
                        target: "epics_ca_rs::client::beacon_monitor",
                        attempts,
                        "CA repeater confirmed our registration"
                    );
                }
                registered = true;
                continue;
            }
            if hdr.cmmd != CA_PROTO_RSRV_IS_UP {
                continue;
            }

            // Verifier policy: by default, drop unauthenticated
            // beacons when a verifier is configured. The companion
            // signed-beacon datagram can arrive ~simultaneously; we
            // check against the verified-tuple set populated above and
            // GC stale entries every iteration to keep the map bounded.
            //
            // EPICS_CA_BEACON_REQUIRE_SIGNED=NO opts out — unsigned
            // beacons are accepted (with a counter increment) so
            // operators can run mixed deployments where some servers
            // have rolled out signing and some haven't yet.
            #[cfg(feature = "cap-tokens")]
            if let Some(ref v) = verifier {
                let max_age = std::time::Duration::from_secs(v.max_age_secs.max(1));
                let now = std::time::Instant::now();
                verified_tuples.retain(|_, t| now.duration_since(*t) <= max_age);
                // Anchor the lookup on `hdr.available` so the
                // key matches the companion-side insert under the
                // standard production topology (client receives via
                // the CA repeater on LOCALHOST). The CA server emits
                // `m_available = 0` per C `online_notify.c:69`, and
                // the repeater rewrites the field to the original
                // server's source IP before forwarding (C
                // `repeater.cpp:626-630`; our `repeater.rs:224-228`).
                // The companion datagram carries `server_ip` in its
                // signed payload (`signed_beacon.rs::build_packet`
                // bytes 12..16), which `verify()` returns as the same
                // BE-bytes-as-u32 coordinate. The two coordinates now
                // line up regardless of whether the matching beacon
                // arrives directly or via a repeater.
                //
                // The earlier `meta.src.ip()` keying was correct in
                // synthetic direct-LAN tests but broke production: a
                // loopback-bound monitor socket only ever sees
                // `meta.src = 127.0.0.1:<repeater_port>` from the
                // repeater, so the lookup key was always
                // `(127.0.0.1, port, beacon_id)` while the insert key
                // was `(real_server_ip, port, beacon_id)` — every
                // legitimate signed beacon was rejected with
                // `EPICS_CA_BEACON_REQUIRE_SIGNED=YES`. See
                // `verified_tuple_key_matches_via_repeater` for the
                // regression case.
                //
                // Fall back to `meta.src.ip()` when `hdr.available` is
                // zero (e.g. a malformed or non-rewritten beacon).
                // That key won't hit under the loopback topology, but
                // it lets the direct-LAN path keep working.
                let lookup_ip_u32 = if hdr.available != 0 {
                    hdr.available
                } else {
                    match meta.src.ip() {
                        std::net::IpAddr::V4(v) => u32::from_be_bytes(v.octets()),
                        std::net::IpAddr::V6(_) => 0,
                    }
                };
                let key = (lookup_ip_u32, hdr.count, hdr.cid);
                if !verified_tuples.contains_key(&key) {
                    metrics::counter!("ca_client_unsigned_beacon_drops_total").increment(1);
                    if require_signed {
                        continue;
                    }
                }
            }

            handle_beacon(hdr, &mut servers, &coord_tx, &ignored_servers);
        }
    }
}

/// libca `bhe.cpp` "new client connect" parity. Clears the EMA so the
/// next beacon reseeds `period_estimate` from the live cadence;
/// preserves `last_id` and `last_seen` so duplicate-detection and
/// stale-prune still work across the reset.
///
/// `circuit_addr` is the TCP `server_addr` of the freshly-connected
/// circuit. The `BeaconState` map is keyed by the beacon's *announced*
/// address — per `handle_beacon`'s comment "new servers always set
/// available=INADDR_ANY (0)", so the dominant key is `0.0.0.0:port`,
/// NOT the TCP address. Multi-homed IOCs are a second case where the
/// announced IP is one NIC and the circuit reaches the server via a
/// different NIC.
///
/// We resolve the right entry conservatively. A naive port-only
/// sweep would silently blind unrelated IOCs that share the default
/// port 5064 across the network: post-reset `count=0` gates
/// `PeriodCollapse` for the next 4 beacons, so a same-port IOC that
/// restarts with a preserved beacon counter inside that window
/// would go undetected — a real correctness regression, not just a
/// noise issue.
///
/// Resolution order — each step is **terminal** (early-return on
/// hit). The terminality matters: a single CA server announces
/// consistently with one IP per process. `server/beacon.rs` computes
/// `server_ip` once at task start and reuses it for every beacon, so
/// a given IOC produces *one* beacon-state key (real IP or INADDR_ANY,
/// never both). If both an exact-match entry and a `0.0.0.0:port`
/// entry exist simultaneously, they represent DIFFERENT IOCs sharing
/// the port. Falling through past a hit would silently blind the
/// other IOC's PeriodCollapse for the next ~4 beacons.
///
///   1. **Exact match** `circuit_addr` — works when the IOC announced
///      its real IP (rare for new IOCs, common for older / pvxs
///      servers). On hit: reset and return.
///   2. **`0.0.0.0:port`** (INADDR_ANY) — the dominant case for
///      modern IOCs. Only consulted when (1) missed. On hit: reset
///      and return.
///   3. **Single unambiguous `*:port` entry** — only when both (1)
///      and (2) missed AND exactly one other `*:port` entry exists.
///      Catches the multi-homed IOC case (announced via NIC A,
///      circuit via NIC B) without touching unrelated same-port
///      IOCs.
///
/// When (1)/(2) miss and there are multiple ambiguous `*:port`
/// entries, we deliberately do NOT reset — a multi-homed IOC sharing
/// port 5064 with unrelated IOCs reverts to the original
/// post-reconnect cascade behaviour. Acceptable trade-off: the
/// alternative was silent restart-detection failure for every other
/// IOC on that port.
fn apply_reset_server(servers: &mut HashMap<SocketAddr, BeaconState>, circuit_addr: SocketAddr) {
    let port = circuit_addr.port();
    let inaddr_any = SocketAddr::V4(SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, port));

    if let Some(s) = servers.get_mut(&circuit_addr) {
        s.period_estimate = None;
        s.count = 0;
        return;
    }
    if let Some(s) = servers.get_mut(&inaddr_any) {
        s.period_estimate = None;
        s.count = 0;
        return;
    }

    // Snapshot the matching keys to release the borrow before
    // mutating; HashMap::iter_mut filtered to a single key gets
    // gnarly fast.
    let port_keys: Vec<SocketAddr> = servers
        .keys()
        .filter(|k| k.port() == port)
        .copied()
        .collect();
    if port_keys.len() == 1 {
        if let Some(s) = servers.get_mut(&port_keys[0]) {
            s.period_estimate = None;
            s.count = 0;
        }
    }
    // else: ambiguous (zero or multiple non-INADDR_ANY *:port
    // entries). Skip — see doc comment for rationale.
}

fn handle_beacon(
    hdr: CaHeader,
    servers: &mut HashMap<SocketAddr, BeaconState>,
    coord_tx: &mpsc::UnboundedSender<CoordRequest>,
    ignored_servers: &std::collections::HashSet<Ipv4Addr>,
) {
    // count = server TCP port (CA v4.1+), data_type = protocol version.
    //
    // C `udpiiu::beaconAction` (`modules/ca/src/client/udpiiu.cpp:
    // 770-779`): when `msg.m_count == 0` (old V<4.1 server with
    // no port in the beacon), the client uses `this->serverPort` —
    // which is set from the EPICS_CA_SERVER_PORT env var at
    // udpiiu construction (`udpiiu.cpp:155-156`,
    // `envGetInetPortConfigParam`). Pre-fix Rust hardcoded
    // CA_SERVER_PORT (= 5064), ignoring the env override. A site
    // that runs its IOCs on a non-default port (via
    // EPICS_CA_SERVER_PORT) would see old-style beacons routed to
    // 5064 — effectively dropped because no listener is there.
    let server_port = if hdr.count != 0 {
        hdr.count
    } else {
        epics_base_rs::runtime::net::ca_server_port()
    };
    let beacon_id = hdr.cid;

    // New servers always set available=INADDR_ANY (0).  Use 0.0.0.0
    // as-is for beacon tracking — each IOC still has a unique port,
    // matching the approach used by the C CA client (libca).
    let server_ip = Ipv4Addr::from(hdr.available.to_be_bytes());
    // EPICS_RS_CLIENT_IGNORE: silently drop beacons announcing a
    // quarantined server so the anomaly-poke path doesn't keep
    // waking the search engine for a quarantined IOC. Rust-only
    // extension; NOT the C EPICS_IOC_IGNORE_SERVERS (server-side
    // name list, different semantics — see
    // client::epics_rs_client_ignore docstring). Filter applies only when the announced IP is concrete —
    // INADDR_ANY (0) means "I'm an IOC announcing myself, use the
    // UDP source," which the search engine resolves separately.
    if !server_ip.is_unspecified() && ignored_servers.contains(&server_ip) {
        return;
    }
    let server_addr = SocketAddr::V4(SocketAddrV4::new(server_ip, server_port));
    let now = Instant::now();

    // Drop entries idle past `BEACON_STALE_THRESHOLD` so a long-silent
    // server's revival lands on the `first_sighting = true` path and
    // triggers the anomaly poke naturally (pvxs `tickBeaconClean`
    // parity). This is what protects the search engine from staying in
    // 200 ms fast-tick mode forever in a steady-state network.
    servers.retain(|_, s| now.duration_since(s.last_seen) < BEACON_STALE_THRESHOLD);

    // G1: cap the per-server BeaconState map. With
    // EPICS_CA_BEACON_REQUIRE_SIGNED=NO an attacker can spoof
    // beacons with arbitrary `available`/`count` to grow the map.
    // Reap entries idle for ≥5× period_estimate when the cap is hit.
    const MAX_BEACON_SERVERS: usize = 4096;
    let first_sighting = !servers.contains_key(&server_addr);
    if first_sighting && servers.len() >= MAX_BEACON_SERVERS {
        let cutoff_threshold = Duration::from_secs(15 * 5);
        servers.retain(|_, s| now.duration_since(s.last_seen) < cutoff_threshold);
    }
    let entry = servers.entry(server_addr).or_insert_with(|| BeaconState {
        last_id: beacon_id.wrapping_sub(1),
        last_seen: now,
        period_estimate: None,
        count: 0,
    });

    let actual_interval = now.duration_since(entry.last_seen);
    let expected_next_id = entry.last_id.wrapping_add(1);

    // Multi-NIC / repeater duplicate detection: the SAME beacon (same
    // id, arriving microseconds apart through different paths) used to
    // trip the period-collapse branch below and fire a spurious
    // anomaly on every duplicate. Drop the second copy outright so
    // the search engine isn't woken twice for one beacon. Without
    // soft-poke-on-every-beacon (removed earlier this round) this
    // misclassification was masked by the throttle; the prune-only
    // design surfaces it.
    //
    // We deliberately do NOT refresh `last_seen` here: a server stuck
    // emitting only same-id duplicates (frozen / wedged) will be
    // pruned at `BEACON_STALE_THRESHOLD` and its next real (fresh-id)
    // beacon will land on the `first_sighting = true` path — the
    // desired anomaly behaviour for a recovered server.
    if !first_sighting && beacon_id == entry.last_id {
        return;
    }

    // libca `bhe.cpp:159-182` parity (narrowed): drop beacons
    // whose sequence number jumps FORWARD by 2 or 3 (likely a
    // duplicate route that's slightly ahead of us, or a brief
    // input-queue overrun) or BACKWARDS by 1-4 (a redundant route
    // delivering an older copy). Without this, those cases hit
    // the `IdMismatch` branch below and flag the transport
    // watchdog for ~30 s on what is in reality a healthy IOC.
    //
    // We deliberately narrow libca's backwards window from 256 to
    // 4. libca conflates "duplicate route" with "id reset to a
    // small number" because it relies on the period-collapse
    // check to detect restarts. Our `IdMismatch` branch detects
    // restart-to-1 directly via the id sequence and catches
    // sub-50 ms restarts that period-collapse misses. The wider
    // libca window would swallow those into the dedup path.
    //
    // Update `last_id` to the new value so the next genuine
    // beacon computes its advance from the most recent
    // observation (also matches libca, where
    // `lastBeaconNumber = beaconNumber` runs before the discard
    // checks). `last_seen`, `count`, and `period_estimate` are
    // left untouched — the drop-only-dups path keeps a server
    // stuck emitting nothing-but-dups on the
    // BEACON_STALE_THRESHOLD prune trajectory.
    const BACKWARDS_DUP_WINDOW: u32 = 4;
    if !first_sighting {
        let advance = beacon_id.wrapping_sub(entry.last_id);
        let backwards_dup = advance > u32::MAX - BACKWARDS_DUP_WINDOW;
        let small_forward_dup = advance == 2 || advance == 3;
        if backwards_dup || small_forward_dup {
            entry.last_id = beacon_id;
            return;
        }
    }

    // Classify. Priority order: FirstSighting wins because there is no
    // prior `last_id` / `period_estimate` to make the other checks
    // meaningful. IdMismatch beats the period bands because a fresh
    // beacon sequence is the dispositive restart signal even when the
    // interval also happens to be off-period.
    //
    // Everything else is libca `bhe::updatePeriod` (`bhe.cpp:226-262`),
    // reproduced in `classify_period`: one band for "beacons went
    // missing" (>= 1.25x average, >= 3.25x also wakes searches), one
    // for "beacons sped up, so the IOC rebooted" (<= 0.80x average),
    // and a healthy arrival otherwise. The bands replace the local
    // `actual_interval < period_estimate / 3` self-reset heuristic,
    // which reported nothing to either consumer.
    //
    // A false trigger on a live server is *not* damaging, and libca
    // says so explicitly at `bhe.cpp:216-221` / `bhe.cpp:248-253`:
    // "It may be possible to get false triggers here if the client is
    // busy, but this does not cause problems because the echo response
    // will tell us that the server is available". Our receive watchdog
    // now behaves the same way (echo timeout marks the circuit
    // unresponsive and KEEPS the socket, recovering on the next byte —
    // `transport.rs` `tcpRecvWatchdog::expire` parity), so an anomaly
    // on a healthy circuit costs one echo round-trip, not a disconnect.
    let action = if first_sighting {
        // Divergence from libca, deliberate: C creates the `bhe` and
        // returns without any notify (`cac.cpp:480-496`) — it waits for
        // the 2nd beacon. We wake the search engine immediately (a
        // channel stuck in `Searching` should not wait a whole beacon
        // period), but we do NOT flag the circuit watchdog, since by
        // definition an operational circuit for this server is already
        // receiving from it.
        BeaconAction {
            rescan: Some(BeaconAnomalyKind::FirstSighting),
            watchdog: None,
        }
    } else if beacon_id != expected_next_id {
        BeaconAction {
            rescan: Some(BeaconAnomalyKind::IdMismatch),
            watchdog: Some(true),
        }
    } else {
        match entry.period_estimate {
            // 2nd beacon: no average yet, so no band applies. C seeds
            // `averagePeriod = currentPeriod` here (`bhe.cpp:199`); the
            // seeding happens in the state-update block below.
            None => BeaconAction {
                rescan: None,
                watchdog: Some(false),
            },
            Some(average) => classify_period(average, actual_interval),
        }
    };
    let anomaly_kind = action.rescan;

    // Update state.
    entry.last_id = beacon_id;
    entry.last_seen = now;
    entry.count += 1;

    if entry.count > 1 {
        // First observed inter-beacon interval defines the initial
        // estimate; subsequent samples blend in via the EMA. Mirrors
        // libca `bhe.cpp:199` (`this->averagePeriod = currentPeriod`
        // on the second beacon, after the `averagePeriod < 0.0`
        // sentinel guard). See `BeaconState::period_estimate` doc
        // for why a hardcoded 15 s placeholder caused a false
        // period-collapse cascade against ramp-up beacon emitters.
        entry.period_estimate = Some(update_average_period(
            entry.period_estimate,
            actual_interval,
        ));
    }

    // Search-engine wake-up: libca `cac::beaconNotify` (`cac.cpp:500`)
    // calls `udpiiu::beaconAnomalyNotify` exactly when
    // `bhe::updatePeriod` returned `netChange`. The earlier "soft poke
    // on every beacon" code amplified normal beacon traffic into a
    // permanent fast-tick search storm whenever multiple IOCs beaconed
    // within the engine's revolution window — keep that path lean.
    if let Some(kind) = anomaly_kind {
        let _ = coord_tx.send(CoordRequest::ForceRescanServer { server_addr, kind });
    }

    // Transport-watchdog notification (libca `bhe::beaconAnomalyNotify`
    // → `tcpRecvWatchdog::beaconAnomalyNotify`, vs `beaconArrivalNotify`
    // for a healthy beacon). Routed via the coordinator to the
    // per-circuit read loop, where it either pushes the deadline
    // forward or sets a sticky anomaly flag that suppresses subsequent
    // healthy-beacon refreshes until the next data arrival or echo
    // response.
    //
    // `watchdog: None` (FirstSighting) is a deliberate divergence: see
    // the classify chain above.
    if let Some(anomaly) = action.watchdog {
        let _ = coord_tx.send(CoordRequest::BeaconArrival {
            server_addr,
            anomaly,
        });
    }
}

// ---------------------------------------------------------------------------
// Repeater registration
// ---------------------------------------------------------------------------

/// Send one `CA_PROTO_REPEATER_REGISTER` for attempt number `attempt`.
///
/// C `caRepeaterRegistrationMessage` (`udpiiu.cpp:465-535`) — a bare
/// `sendto`, no waiting. Confirmation arrives asynchronously as
/// `CA_PROTO_REPEATER_CONFIRM` on the same socket and is handled by the
/// monitor's receive loop (C `udpiiu::repeaterAckAction`). This function
/// deliberately does NOT read the socket: doing so would consume beacons
/// destined for the monitor.
///
/// The registration ALTERNATES its address across retries
/// (`udpiiu.cpp:494-519`): on an odd `attempt` C uses `osiLocalAddr()` — the
/// first up, non-loopback interface — and on an even one the loopback address.
/// One `osiSockAddr` serves as both the `sendto` destination and the announced
/// `m_available`, so both alternate together. The reason is compatibility: a
/// repeater from 3.13 beta 11 or earlier called `local_addr()` to decide which
/// registrations to accept and rejected everything from a different address,
/// and which of the two addresses that was depended on the release. Alternating
/// means one of every two attempts is always acceptable to any repeater vintage
/// (`udpiiu.cpp:476-493`). Both the C repeater (`repeater.cpp:115-117`) and this
/// port's (`repeater.rs:103`) bind `INADDR_ANY`, so both destinations reach it.
///
/// `repeater_port` is the value the caller resolved once via
/// `envGetInetPortConfigParam` — C's `udpiiu::repeaterPort` member, not a
/// fresh env read per attempt.
async fn send_repeater_registration(
    socket: &AsyncUdpV4,
    attempt: u32,
    repeater_port: u16,
) -> Result<(), ()> {
    let addr = if attempt & 1 == 1 {
        crate::server::addr_list::osi_local_addr()
    } else {
        Ipv4Addr::LOCALHOST
    };

    let mut hdr = CaHeader::new(CA_PROTO_REPEATER_REGISTER);
    hdr.available = u32::from_be_bytes(addr.octets());

    let repeater_addr = SocketAddr::V4(SocketAddrV4::new(addr, repeater_port));
    socket
        .send_to(&hdr.to_bytes(), repeater_addr)
        .await
        .map_err(|_| ())?;
    Ok(())
}

#[cfg(test)]
mod repeater_registration_tests {
    //! R6-22: libca re-sends `REPEATER_REGISTER` every second until the
    //! repeater CONFIRMs, and never gives up
    //! (`repeaterSubscribeTimer.cpp:84-90`; `registered` is set only by
    //! `confirmNotify` ← `udpiiu::repeaterAckAction`, `udpiiu.cpp:793`). The
    //! pre-fix client tried three times over ~2 s and then went quiet until a
    //! 5-minute silence timer, so a repeater that was not yet bound at client
    //! start-up never got a registration — no beacon fan-out for 5 minutes.
    use super::*;
    use std::time::Duration;

    async fn drain_registers(
        repeater: &tokio::net::UdpSocket,
        window: Duration,
    ) -> (usize, Option<SocketAddr>) {
        let mut buf = [0u8; 64];
        let mut registers = 0usize;
        let mut client = None;
        let deadline = tokio::time::Instant::now() + window;
        loop {
            match tokio::time::timeout_at(deadline, repeater.recv_from(&mut buf)).await {
                Ok(Ok((n, src))) if n >= CaHeader::SIZE => {
                    if let Ok(h) = CaHeader::from_bytes(&buf[..n]) {
                        if h.cmmd == CA_PROTO_REPEATER_REGISTER {
                            registers += 1;
                            client = Some(src);
                        }
                    }
                }
                Ok(Ok(_)) | Ok(Err(_)) => continue,
                Err(_) => break, // window closed
            }
        }
        (registers, client)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn registration_retries_every_second_until_confirm_then_stops() {
        // `INADDR_ANY`, like the real repeater (C `repeater.cpp:115-117`,
        // `repeater.rs:103`). A loopback-only bind would miss the odd-numbered
        // attempts, which C addresses to `osiLocalAddr()` — see
        // `registration_alternates_loopback_and_local_addr_across_attempts`.
        let repeater = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("bind fake repeater on INADDR_ANY");
        let port = repeater.local_addr().unwrap().port();

        let saved = std::env::var("EPICS_CA_REPEATER_PORT").ok();
        // SAFETY: serial_test::serial serialises env mutation; restored below.
        unsafe { std::env::set_var("EPICS_CA_REPEATER_PORT", port.to_string()) };

        let (coord_tx, _coord_rx) = mpsc::unbounded_channel();
        let (_ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let monitor = tokio::spawn(run_beacon_monitor(
            coord_tx,
            ctrl_rx,
            crate::protocol::repeater_port(),
        ));

        // ~3.5 s of an unresponsive repeater: C would have sent one
        // registration per second, so at least 4 (t ≈ 0, 1, 2, 3). The old
        // three-attempt loop could never produce a 4th.
        let (registers, client) = drain_registers(&repeater, Duration::from_millis(3500)).await;
        assert!(
            registers >= 4,
            "an unconfirmed registration must retry every second forever \
             (C repeaterSubscribeTimer); got only {registers} in 3.5 s"
        );
        let client = client.expect("registration datagrams carry a source address");

        // CONFIRM (C `repeaterAckAction` → `confirmNotify`) must stop it.
        let confirm = CaHeader::new(CA_PROTO_REPEATER_CONFIRM);
        repeater
            .send_to(&confirm.to_bytes(), client)
            .await
            .expect("send CONFIRM");

        // One registration may already be in flight from a tick that fired
        // before the CONFIRM landed; give it 400 ms to settle, then require
        // silence for 2.2 s (C returns `noRestart` once registered).
        tokio::time::sleep(Duration::from_millis(400)).await;
        let (after, _) = drain_registers(&repeater, Duration::from_millis(2200)).await;
        assert_eq!(
            after, 0,
            "registration must stop on CONFIRM (C `expire` returns noRestart \
             once `registered`); saw {after} more"
        );

        monitor.abort();
        // SAFETY: see above.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CA_REPEATER_PORT", v),
                None => std::env::remove_var("EPICS_CA_REPEATER_PORT"),
            }
        }
    }

    /// R6-22 residual — the registration address ALTERNATES across retries.
    ///
    /// C `caRepeaterRegistrationMessage` (`udpiiu.cpp:494-519`): an odd
    /// `attemptNumber` registers from `osiLocalAddr()` (first up, non-loopback
    /// interface), an even one from the loopback address, and the chosen
    /// `osiSockAddr` is BOTH the `sendto` destination and the announced
    /// `m_available`. A pre-3.13-beta-12 repeater accepted registrations from
    /// only one of those two addresses — which one depended on the release —
    /// so alternating keeps one of every two attempts acceptable to any
    /// vintage. The port always announced (and always sent to) loopback.
    ///
    /// The fake repeater binds `INADDR_ANY` like the real one
    /// (`repeater.cpp:115-117`, `repeater.rs:103`), so it receives BOTH
    /// destination forms — asserting on the 4 datagrams' `m_available` pins
    /// the alternation and proves the odd-attempt datagram is still delivered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn registration_alternates_loopback_and_local_addr_across_attempts() {
        let repeater = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("bind fake repeater on INADDR_ANY");
        let port = repeater.local_addr().unwrap().port();

        let saved = std::env::var("EPICS_CA_REPEATER_PORT").ok();
        // SAFETY: serial_test::serial serialises env mutation; restored below.
        unsafe { std::env::set_var("EPICS_CA_REPEATER_PORT", port.to_string()) };

        let (coord_tx, _coord_rx) = mpsc::unbounded_channel();
        let (_ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let monitor = tokio::spawn(run_beacon_monitor(
            coord_tx,
            ctrl_rx,
            crate::protocol::repeater_port(),
        ));

        // Attempts 0..=3 at ~1 s apart (the repeater never CONFIRMs).
        let mut announced: Vec<Ipv4Addr> = Vec::new();
        let mut buf = [0u8; 64];
        let deadline = tokio::time::Instant::now() + Duration::from_millis(3600);
        while announced.len() < 4 {
            match tokio::time::timeout_at(deadline, repeater.recv_from(&mut buf)).await {
                Ok(Ok((n, _src))) if n >= CaHeader::SIZE => {
                    if let Ok(h) = CaHeader::from_bytes(&buf[..n]) {
                        if h.cmmd == CA_PROTO_REPEATER_REGISTER {
                            announced.push(Ipv4Addr::from(h.available.to_be_bytes()));
                        }
                    }
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        monitor.abort();
        // SAFETY: see above.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CA_REPEATER_PORT", v),
                None => std::env::remove_var("EPICS_CA_REPEATER_PORT"),
            }
        }

        assert_eq!(
            announced.len(),
            4,
            "expected 4 registrations in 3.6 s (1 s retry period); the odd-attempt \
             datagram must still reach an INADDR_ANY-bound repeater"
        );
        // C's rule verbatim: even → loopback, odd → osiLocalAddr(). On a
        // loopback-only host `osi_local_addr()` IS loopback and C sends
        // loopback for both — the expectation below still holds.
        let local = crate::server::addr_list::osi_local_addr();
        assert_eq!(
            announced,
            vec![Ipv4Addr::LOCALHOST, local, Ipv4Addr::LOCALHOST, local],
            "attempt N must announce loopback for even N and osiLocalAddr() for odd N \
             (udpiiu.cpp:494-519); got {announced:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// `BEACON_STALE_THRESHOLD` is exactly 180 s (mirrors pvxs
    /// `tickBeaconClean` 2 × beaconCleanInterval / 2 default), and
    /// the prune sweep retains entries seen within the window while
    /// dropping older ones. The prune is what makes long-silent
    /// servers' revival hit the `first_sighting = true` anomaly
    /// path — without it, an in-sequence beacon after long silence
    /// wouldn't kick the search engine out of slow-cadence retry.
    #[test]
    fn beacon_stale_threshold_is_180s() {
        assert_eq!(BEACON_STALE_THRESHOLD, Duration::from_secs(180));
    }

    /// Multi-NIC / repeater duplicate detection: same beacon arriving
    /// twice in quick succession (same `cid`) must NOT fire a second
    /// anomaly request to the coordinator. Without the duplicate
    /// guard, the second copy hit the period-collapse branch
    /// (actual_interval ≈ 0 < period_estimate/3) and rescheduled
    /// every pending search a second time. With soft-poke removed
    /// earlier this round, the misclassification surfaces as a
    /// permanent fast-tick spam in dual-NIC environments.
    #[test]
    fn duplicate_beacon_does_not_double_fire_anomaly() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.cid = 100;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);

        // First beacon — first sighting → anomaly fires with the
        // FirstSighting kind so the coordinator can wake searches
        // without probing operational TCP circuits.
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        assert!(matches!(
            rx.try_recv(),
            Ok(CoordRequest::ForceRescanServer {
                kind: BeaconAnomalyKind::FirstSighting,
                ..
            })
        ));
        // Drain any further send (none expected).
        assert!(rx.try_recv().is_err());

        // Second beacon with the SAME cid (true duplicate from another
        // NIC / repeater coalesce) — must be silently dropped.
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        assert!(
            rx.try_recv().is_err(),
            "duplicate same-cid beacon must not fire ForceRescanServer"
        );
    }

    /// A real IOC restart resets the beacon sequence to a fresh value.
    /// Even if the inter-beacon interval is sub-50 ms (faster than
    /// `MIN_PERIOD_COLLAPSE_INTERVAL`), the `beacon_id != expected_next_id`
    /// branch must still classify it as anomaly — the floor only protects
    /// the period-collapse branch from misfiring on duplicates.
    #[test]
    fn sub_50ms_restart_via_id_mismatch_still_fires_anomaly() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.cid = 100;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        // First sighting — anomaly fires.
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        assert!(rx.try_recv().is_ok());

        // Sub-50ms later, IOC restarts: id resets to 1 (not the
        // expected id=101). period_estimate is 15s default; even
        // though actual_interval < 50ms now, the id-mismatch branch
        // must catch the restart.
        hdr.cid = 1;
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        assert!(
            matches!(
                rx.try_recv(),
                Ok(CoordRequest::ForceRescanServer {
                    kind: BeaconAnomalyKind::IdMismatch,
                    ..
                })
            ),
            "id-mismatch restart must fire IdMismatch anomaly even when interval < 50ms"
        );
    }

    /// libca `bhe.cpp:267-268`: `averagePeriod = currentPeriod * 0.125
    /// + averagePeriod * 0.875`. Pinned exactly — the smoothing factor
    /// sets how fast the average chases a sample, and therefore how
    /// long a stretched or collapsed cadence keeps reading as anomalous
    /// against the bands in `classify_period`.
    #[test]
    fn running_average_uses_the_libca_smoothing_factor() {
        // 2nd beacon: no average yet, so the sample is adopted
        // outright (`bhe.cpp:199`), NOT smoothed against a seed.
        assert_eq!(
            update_average_period(None, Duration::from_secs(10)),
            Duration::from_secs(10),
            "the first measured interval defines the average"
        );

        // One sample of 10 s against a 2 s average:
        //   10 * 0.125 + 2 * 0.875 = 3.0 s   (alpha = 0.25 gives 4.0 s)
        assert_eq!(
            update_average_period(Some(Duration::from_secs(2)), Duration::from_secs(10)),
            Duration::from_secs_f64(3.0),
            "0.125 smoothing: a single long sample moves the average by an eighth"
        );

        // Symmetric on the way down:
        //   2 * 0.125 + 10 * 0.875 = 9.0 s
        assert_eq!(
            update_average_period(Some(Duration::from_secs(10)), Duration::from_secs(2)),
            Duration::from_secs_f64(9.0),
        );

        // A constant cadence is a fixed point of the EMA at any alpha —
        // this is what makes the steady-state band tests deterministic.
        let steady = Duration::from_millis(1500);
        assert_eq!(update_average_period(Some(steady), steady), steady);

        // Band interaction: after ONE 3.5x sample, the average must
        // still be far enough below the next same-length interval that
        // a genuinely stretched cadence keeps flagging. With alpha
        // 0.125 the average lands at 1.3125 s, so a second 3.5 s beacon
        // is still 2.67x — inside the 1.25x anomaly band.
        let avg = update_average_period(Some(Duration::from_secs(1)), Duration::from_millis(3500));
        assert_eq!(
            avg,
            Duration::from_millis(1312) + Duration::from_micros(500)
        );
        assert_eq!(
            classify_period(avg, Duration::from_millis(3500)).watchdog,
            Some(true),
            "a sustained stretched cadence must keep flagging the watchdog"
        );
    }

    /// libca `bhe.cpp:226-262` band boundaries, exercised on the pure
    /// classifier so the thresholds are pinned exactly rather than
    /// approximately. One case per boundary, both sides of each.
    #[test]
    fn period_bands_match_the_libca_thresholds() {
        let avg = Duration::from_secs(1);
        let band = |ms: u64| classify_period(avg, Duration::from_millis(ms));

        // Healthy interior: neither band. (`bhe.cpp:260` →
        // `beaconArrivalNotify`.)
        for ms in [810u64, 1000, 1240] {
            assert_eq!(
                band(ms),
                BeaconAction {
                    rescan: None,
                    watchdog: Some(false)
                },
                "{ms} ms against a 1 s average is inside libca's healthy band"
            );
        }

        // `>= 1.25 * average` — one missing beacon: flag the circuit
        // watchdog, but do NOT wake searches (`bhe.cpp:226-231`).
        for ms in [1250u64, 3249] {
            assert_eq!(
                band(ms),
                BeaconAction {
                    rescan: None,
                    watchdog: Some(true)
                },
                "{ms} ms is in the 1.25x..3.25x band: anomaly, no netChange"
            );
        }

        // `>= 3.25 * average` — ~3 missing beacons: netChange too
        // (`bhe.cpp:232-238`).
        for ms in [3250u64, 60_000] {
            assert_eq!(
                band(ms),
                BeaconAction {
                    rescan: Some(BeaconAnomalyKind::LongPeriod),
                    watchdog: Some(true)
                },
                "{ms} ms is past 3.25x: anomaly + netChange"
            );
        }

        // `<= 0.80 * average` — IOC reboot ramp-up: anomaly +
        // netChange (`bhe.cpp:255-259`).
        for ms in [800u64, 20] {
            assert_eq!(
                band(ms),
                BeaconAction {
                    rescan: Some(BeaconAnomalyKind::ShortPeriod),
                    watchdog: Some(true)
                },
                "{ms} ms is at or below 0.80x: anomaly + netChange"
            );
        }
    }

    /// End-to-end through `handle_beacon`: a monotonic-id beacon whose
    /// interval collapsed far below the running average is libca's
    /// IOC-reboot signature (`bhe.cpp:255`) — it wakes searches AND
    /// flags the circuit watchdog. The previous implementation
    /// swallowed this case (self-reset of the EMA, no notification to
    /// either consumer).
    #[test]
    fn sub_period_beacon_fires_the_short_period_band() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        servers.insert(
            server,
            BeaconState {
                last_id: 99,
                last_seen: Instant::now() - Duration::from_millis(200),
                period_estimate: Some(Duration::from_secs(15)),
                count: 10,
            },
        );

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        hdr.cid = 100; // monotonic — rules out IdMismatch
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());

        let mut saw_rescan = false;
        let mut saw_anomaly_arrival = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                CoordRequest::ForceRescanServer {
                    kind: BeaconAnomalyKind::ShortPeriod,
                    ..
                } => saw_rescan = true,
                CoordRequest::BeaconArrival { anomaly: true, .. } => saw_anomaly_arrival = true,
                _ => {}
            }
        }
        assert!(
            saw_rescan,
            "200 ms against a 15 s average is <= 0.80x — libca returns netChange"
        );
        assert!(
            saw_anomaly_arrival,
            "the short-period band also calls bhe::beaconAnomalyNotify"
        );

        // C blends the sample into the average regardless of band
        // (`bhe.cpp:267`) — it never resets the estimate here.
        let s = servers.get(&server).expect("entry");
        assert!(
            s.period_estimate
                .is_some_and(|e| e < Duration::from_secs(15)),
            "the anomalous sample still updates the running average"
        );
        assert_eq!(s.count, 11, "count keeps advancing");
        assert_eq!(s.last_id, 100);
    }

    /// The long-period band is the finding this test was written for:
    /// an interval *longer* than the running average had no branch at
    /// all before. `bhe.cpp:226` flags the circuit watchdog from 1.25x,
    /// and only past 3.25x (`bhe.cpp:232`) does it also wake searches.
    #[test]
    fn long_period_beacon_flags_the_watchdog_before_it_wakes_searches() {
        let seed = |interval: Duration| {
            let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
            let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
            servers.insert(
                server,
                BeaconState {
                    last_id: 99,
                    last_seen: Instant::now() - interval,
                    period_estimate: Some(Duration::from_secs(1)),
                    count: 10,
                },
            );
            let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();
            let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
            hdr.count = 5064;
            hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
            hdr.cid = 100;
            handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
            let (mut rescan, mut arrival_anomaly) = (false, false);
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    CoordRequest::ForceRescanServer { .. } => rescan = true,
                    CoordRequest::BeaconArrival { anomaly, .. } => arrival_anomaly = anomaly,
                    _ => {}
                }
            }
            (rescan, arrival_anomaly)
        };

        // 2x the average: one beacon went missing. Watchdog only.
        assert_eq!(
            seed(Duration::from_millis(2000)),
            (false, true),
            "1.25x..3.25x must flag the watchdog without waking searches"
        );
        // 4x the average: netChange as well.
        assert_eq!(
            seed(Duration::from_millis(4000)),
            (true, true),
            ">= 3.25x must additionally wake searches (netChange)"
        );
    }

    /// Watching a freshly-started IOC ramp up (rsrv
    /// `online_notify.c:66,116-120`: 20 ms doubling to 15 s — the same
    /// pattern `epics-ca-rs/src/server/beacon.rs` emits) must never
    /// classify as `ShortPeriod`: every interval is *longer* than the
    /// last, so the running average only ever trails the sample, and
    /// libca's `<= 0.80x` reboot band cannot be reached. The
    /// stretching intervals do legitimately land in the long-period
    /// band (`bhe.cpp:226`) — that is libca's behaviour and is what
    /// makes an IOC coming online wake pending searches.
    ///
    /// The pre-fix implementation seeded `period_estimate` with a
    /// hardcoded 15 s, so the FIRST sighting of a ramping IOC read as
    /// a period collapse; seeding from the first measured interval
    /// (libca `bhe.cpp:51,199`, `averagePeriod = -DBL_MAX` until the
    /// first `currentPeriod`) is what makes this hold.
    #[test]
    fn rsrv_rampup_beacons_never_classify_as_short_period() {
        // Drive `handle_beacon` directly with a controlled
        // BeaconState so we can advance `last_seen` artificially —
        // the real implementation uses `Instant::now()` and we'd
        // need full virtual time to drive 11 beacons across 30+ s.
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        hdr.cid = 0;

        // Beacon #1 — first sighting.
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        // Drain the FirstSighting event.
        let mut first_sighting_seen = false;
        while let Ok(msg) = rx.try_recv() {
            if matches!(
                msg,
                CoordRequest::ForceRescanServer {
                    kind: BeaconAnomalyKind::FirstSighting,
                    ..
                }
            ) {
                first_sighting_seen = true;
            }
        }
        assert!(first_sighting_seen, "first beacon must fire FirstSighting");

        // Subsequent ramp-up: roll `last_seen` back so each
        // simulated interval is what we want, then handle_beacon
        // computes `actual_interval = now - last_seen`.
        let intervals_ms = [20u64, 40, 80, 160, 320, 640, 1280, 2560, 5120, 10240];
        for (i, &ms) in intervals_ms.iter().enumerate() {
            // Reach into the entry to back-date last_seen by `ms`.
            let s = servers.get_mut(&server).expect("entry");
            s.last_seen = std::time::Instant::now() - Duration::from_millis(ms);
            hdr.cid = (i as u32) + 1;
            handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());

            // A `ShortPeriod` here would be the "IOC may have
            // restarted" misclassification — the ramp-up is the IOC
            // *starting*, and every interval is growing.
            while let Ok(msg) = rx.try_recv() {
                if let CoordRequest::ForceRescanServer { kind, .. } = msg {
                    assert_ne!(
                        kind,
                        BeaconAnomalyKind::ShortPeriod,
                        "ramp-up beacon #{} (interval={} ms) must not classify \
                         as a period collapse — see BeaconState::period_estimate doc",
                        i + 2,
                        ms
                    );
                }
            }
        }
    }

    /// A steady cadence with monotonically increasing ids must classify
    /// as healthy: libca `bhe.cpp:260` refreshes the receive watchdog
    /// and returns no `netChange`, so the only search wake-up is the
    /// first sighting.
    #[test]
    fn steady_cadence_monotonic_ids_does_not_fire_spurious_anomaly() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);

        // Five monotonically increasing beacons (ids 100..105) at a
        // fixed 1-s cadence. First is first_sighting →
        // ForceRescanServer fires once. The rest must not fire any
        // ForceRescanServer (they do fire BeaconArrival{anomaly=false}
        // — the libca healthy-beacon watchdog refresh).
        for id in 100..105 {
            hdr.cid = id;
            if let Some(s) = servers.get_mut(&server) {
                s.last_seen = Instant::now() - Duration::from_secs(1);
            }
            handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        }
        let mut search_wakes = 0;
        let mut healthy_arrivals = 0;
        let mut anomaly_arrivals = 0;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                CoordRequest::ForceRescanServer { .. } => search_wakes += 1,
                CoordRequest::BeaconArrival { anomaly: false, .. } => healthy_arrivals += 1,
                CoordRequest::BeaconArrival { anomaly: true, .. } => anomaly_arrivals += 1,
                _ => {}
            }
        }
        assert_eq!(
            search_wakes, 1,
            "monotonic fast-cadence beacons must wake searches only on first sighting"
        );
        assert_eq!(
            anomaly_arrivals, 0,
            "monotonic fast-cadence beacons must not flag the watchdog"
        );
        assert_eq!(
            healthy_arrivals, 4,
            "each post-first-sighting healthy beacon must refresh the transport watchdog"
        );
    }

    /// libca `tcpRecvWatchdog::beaconAnomalyNotify` parity: when the
    /// monitor classifies a beacon as a real restart (`IdMismatch`
    /// here), it must emit a `BeaconArrival { anomaly: true }`
    /// alongside the search-wake `ForceRescanServer`. The transport
    /// uses that to set its sticky flag without firing an immediate
    /// echo — the receive watchdog will then expire on schedule.
    #[test]
    fn id_mismatch_emits_anomaly_beacon_arrival() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.cid = 100;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        // Establish the BHE so the second beacon isn't a first sighting.
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        // Drain first-sighting messages.
        while rx.try_recv().is_ok() {}

        // Restart: id resets to 1.
        hdr.cid = 1;
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        let mut saw_search_wake = false;
        let mut saw_anomaly_arrival = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                CoordRequest::ForceRescanServer {
                    kind: BeaconAnomalyKind::IdMismatch,
                    ..
                } => saw_search_wake = true,
                CoordRequest::BeaconArrival { anomaly: true, .. } => saw_anomaly_arrival = true,
                _ => {}
            }
        }
        assert!(saw_search_wake, "IdMismatch must wake searches");
        assert!(
            saw_anomaly_arrival,
            "IdMismatch must flag the transport watchdog"
        );
    }

    /// FirstSighting is purely a per-client bookkeeping event; we
    /// either don't have a circuit yet or just pruned the BHE for an
    /// existing circuit. In either case the watchdog must not be
    /// flagged — emitting `BeaconArrival { anomaly: true }` here was
    /// the original cause of the disconnect storms.
    #[test]
    fn first_sighting_does_not_emit_beacon_arrival() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.cid = 100;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());

        let mut saw_first_sighting = false;
        let mut saw_arrival = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                CoordRequest::ForceRescanServer {
                    kind: BeaconAnomalyKind::FirstSighting,
                    ..
                } => saw_first_sighting = true,
                CoordRequest::BeaconArrival { .. } => saw_arrival = true,
                _ => {}
            }
        }
        assert!(saw_first_sighting, "first sighting must wake searches");
        assert!(
            !saw_arrival,
            "first sighting must not touch the transport watchdog"
        );
    }

    /// libca `bhe.cpp:179` parity: a forward jump of 2 or 3 in the
    /// beacon sequence is treated as a duplicate-route artifact, not
    /// an anomaly. With lazy-echo this matters: classifying it as
    /// `IdMismatch` would set the transport watchdog flag and
    /// suppress healthy-beacon refreshes for the next ~30 s on what
    /// is in reality a perfectly healthy IOC. The drop-only-dup
    /// path must update `last_id` so the next genuine beacon
    /// computes its advance against the most recent observation.
    #[test]
    fn small_forward_advance_is_dropped_not_classified() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        // Establish steady-state beacons (ids 100..103) at a fixed 1-s
        // cadence — the period bands (`bhe.cpp:226-262`) classify
        // against the running average, so the intervals have to be
        // realistic, not the ~0 s of a back-to-back test loop.
        for id in 100..103 {
            hdr.cid = id;
            if let Some(s) = servers.get_mut(&server) {
                s.last_seen = Instant::now() - Duration::from_secs(1);
            }
            handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        }
        while rx.try_recv().is_ok() {}

        // Advance of 2 (last_id = 102, next id = 104) — must drop.
        hdr.cid = 104;
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        assert!(
            rx.try_recv().is_err(),
            "advance=2 must be silently dropped, not classified as anomaly"
        );

        // Advance of 3 from the just-updated 104 — also drop.
        hdr.cid = 107;
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        assert!(rx.try_recv().is_err(), "advance=3 must be silently dropped");

        // last_id should now be 107 (drop path still updates it).
        // The next monotonic beacon (108 = advance=1) is healthy.
        hdr.cid = 108;
        servers.get_mut(&server).expect("entry").last_seen =
            Instant::now() - Duration::from_secs(1);
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        let mut saw_arrival_healthy = false;
        let mut saw_anomaly = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                CoordRequest::BeaconArrival { anomaly: false, .. } => saw_arrival_healthy = true,
                CoordRequest::BeaconArrival { anomaly: true, .. }
                | CoordRequest::ForceRescanServer { .. } => saw_anomaly = true,
                _ => {}
            }
        }
        assert!(
            saw_arrival_healthy,
            "after dropped dups, advance=1 must classify as healthy"
        );
        assert!(
            !saw_anomaly,
            "monotonic recovery from drop sequence must not fire anomaly"
        );
    }

    /// Backwards advance (within libca's 256-id window) is also
    /// dropped — same reasoning as the small-forward case but for
    /// duplicates that arrive late through a slower NIC path.
    #[test]
    fn small_backwards_advance_is_dropped() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        for id in 100..103 {
            hdr.cid = id;
            handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        }
        while rx.try_recv().is_ok() {}

        // last_id is 102. A late copy with id=101 — wrapping_sub
        // gives u32::MAX (advance treated as 0xFFFFFFFF, > MAX-256).
        hdr.cid = 101;
        handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());
        assert!(
            rx.try_recv().is_err(),
            "backwards-by-1 (within 256) must drop"
        );
    }

    #[test]
    fn stale_prune_drops_idle_entries_only() {
        // Anchor `now` a fixed span ahead of a real `base` so the back-dated
        // `last_seen` values below subtract without underflowing Instant on
        // Windows (QPC-since-boot, where uptime may be shorter than the span).
        let base = Instant::now();
        let now = base + Duration::from_secs(300);
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let fresh: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let stale: SocketAddr = "127.0.0.1:5065".parse().unwrap();
        servers.insert(
            fresh,
            BeaconState {
                last_id: 0,
                last_seen: now - Duration::from_secs(10),
                period_estimate: Some(Duration::from_secs(15)),
                count: 5,
            },
        );
        servers.insert(
            stale,
            BeaconState {
                last_id: 0,
                last_seen: now - Duration::from_secs(300),
                period_estimate: Some(Duration::from_secs(15)),
                count: 5,
            },
        );
        // The prune logic in handle_beacon: same retain expression.
        servers.retain(|_, s| now.duration_since(s.last_seen) < BEACON_STALE_THRESHOLD);
        assert!(
            servers.contains_key(&fresh),
            "fresh entry must survive prune"
        );
        assert!(
            !servers.contains_key(&stale),
            "180-s-idle entry must be pruned"
        );
    }

    /// Regression for the archiver-rs reconnect noise: a long-lived
    /// CA client that has built a steady-state EMA (e.g. 15 s) for
    /// some server, then loses + re-establishes its TCP circuit while
    /// the server is in `online_notify_task` ramp-up, must NOT report a
    /// stream of short-period anomalies against its stale estimate.
    /// `BeaconControl::ResetServer` (issued by the coordinator on
    /// `TransportEvent::ServerConnected`, libca `bhe.cpp` "new client
    /// connect" parity) clears the EMA so the next beacon reseeds
    /// `period_estimate` from the live cadence — after which the
    /// growing ramp-up intervals can only reach the long-period band.
    #[test]
    fn reset_on_connect_breaks_the_short_period_cascade_after_reconnect() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();

        // Pre-existing steady state: 15-s EMA, 1000 beacons in,
        // last_id=999. Mirrors a long-running archiver before the
        // server's TCP circuit drops.
        servers.insert(
            server,
            BeaconState {
                last_id: 999,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 1000,
            },
        );

        // Coordinator reports the new circuit. EMA cleared.
        apply_reset_server(&mut servers, server);
        let s = servers.get(&server).expect("entry survives reset");
        assert!(
            s.period_estimate.is_none(),
            "ResetServer must clear period_estimate"
        );
        assert_eq!(s.count, 0, "ResetServer must zero count");
        assert_eq!(
            s.last_id, 999,
            "ResetServer must preserve last_id (dedup still works)"
        );

        // Standard rsrv ramp-up: 20, 40, 80, 160, 320, 640, 1280,
        // 2560, 5120, 10240 ms — same pattern as the
        // `rsrv_rampup_beacons_never_classify_as_short_period` test,
        // but arriving on top of the previously-pre-existing entry.
        let intervals_ms = [20u64, 40, 80, 160, 320, 640, 1280, 2560, 5120, 10240];
        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        // Server preserved its beacon counter across restart — ids
        // continue monotonically from 1000, so `IdMismatch` cannot
        // fire and the stale 15-s EMA is the only thing that could
        // misclassify these beacons.
        for (i, &ms) in intervals_ms.iter().enumerate() {
            let s = servers.get_mut(&server).expect("entry");
            s.last_seen = Instant::now() - Duration::from_millis(ms);
            hdr.cid = 1000 + (i as u32);
            handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());

            while let Ok(msg) = rx.try_recv() {
                if let CoordRequest::ForceRescanServer { kind, .. } = msg {
                    assert_ne!(
                        kind,
                        BeaconAnomalyKind::ShortPeriod,
                        "ramp-up beacon #{} (interval={} ms) after \
                         ResetServer must not classify as a period collapse \
                         — the cascade is the archiver-rs reconnect noise \
                         this fix targets",
                        i + 1,
                        ms
                    );
                }
            }
        }
    }

    /// The first beacon of a sped-up cadence classifies as
    /// `ShortPeriod` against a mature EMA — libca `bhe.cpp:255`, and it
    /// is unconditional: C has no "was this really a reboot?" guard,
    /// only the comment at `bhe.cpp:248-253` that a false trigger costs
    /// nothing because the echo response proves the server is alive.
    ///
    /// Concretely this fires whenever a server restarts its beacon ramp
    /// while our own circuit to it stays up (so no
    /// `BeaconControl::ResetServer` for us): a server reboot, a
    /// `ctlPause` resume, or — for an `epics-ca-rs` server — a
    /// `trigger_beacon_anomaly` pulse from the ca_gateway. A client
    /// connect no longer does it: the port's old reset-on-accept, which
    /// made every peer connect land in this band, was removed as a
    /// divergence (R6-30; rsrv restarts the ramp only at startup and
    /// after `ctlPause`, `online_notify.c:66,128`).
    #[test]
    fn peer_connect_ramp_up_fires_the_short_period_band() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();

        // Existing long-lived client: 1000 beacons in, EMA at 15 s,
        // last_id=999. Our circuit is up (so the coordinator never
        // issued ResetServer for us).
        servers.insert(
            server,
            BeaconState {
                last_id: 999,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 1000,
            },
        );

        // A peer client connects to the IOC. The IOC's
        // `beacon_emitter` interval resets to 20 ms and ramps up
        // through 20, 40, 80, 160, 320, 640, 1280, 2560, 5120,
        // 10240 ms before stabilising at 15 s. beacon_id keeps
        // counting monotonically (the IOC didn't restart).
        let intervals_ms = [20u64, 40, 80, 160, 320, 640, 1280, 2560, 5120, 10240];
        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);

        let mut short_period_kinds = 0;
        for (i, &ms) in intervals_ms.iter().enumerate() {
            let s = servers.get_mut(&server).expect("entry");
            s.last_seen = Instant::now() - Duration::from_millis(ms);
            hdr.cid = 1000 + (i as u32);
            handle_beacon(hdr, &mut servers, &tx, &std::collections::HashSet::new());

            while let Ok(msg) = rx.try_recv() {
                if let CoordRequest::ForceRescanServer { kind, .. } = msg {
                    assert_ne!(
                        kind,
                        BeaconAnomalyKind::IdMismatch,
                        "peer-connect ramp-up beacon #{} (interval={} ms): the \
                         beacon sequence is monotonic, so this is not a restart",
                        i + 1,
                        ms
                    );
                    if kind == BeaconAnomalyKind::ShortPeriod {
                        short_period_kinds += 1;
                    }
                }
            }
        }
        assert!(
            short_period_kinds >= 1,
            "the first sped-up beacon (20 ms vs a 15 s average) is <= 0.80x — \
             libca `bhe.cpp:255` calls beaconAnomalyNotify and returns netChange"
        );

        // The EMA tracked the cascade down; ids advanced normally.
        let s = servers.get(&server).expect("entry");
        assert_eq!(s.last_id, 1009, "last_id must track ramp-up ids");
        assert!(
            s.period_estimate
                .is_some_and(|e| e < Duration::from_secs(15)),
            "the running average must follow the sped-up cadence down"
        );
    }

    /// `apply_reset_server` for an unknown server is a no-op — the
    /// coordinator will issue ResetServer on every fresh circuit,
    /// including first-ever connects where we may not yet have a
    /// `BeaconState` (e.g. the server hadn't beaconed before our
    /// search reached it via name resolution).
    #[test]
    fn reset_unknown_server_is_noop() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        apply_reset_server(&mut servers, server);
        assert!(servers.is_empty());
    }

    /// The beacon-state HashMap key is the beacon's *announced*
    /// address, which for modern IOCs is INADDR_ANY (`available=0`)
    /// — so the key is `0.0.0.0:port` while the TCP-circuit
    /// `server_addr` is the real IP:port. An exact-key lookup
    /// would miss this entry and the reset would be a no-op. Mirror
    /// `beacon_arrival_targets`'s port-fallback policy so the EMA is
    /// actually cleared.
    #[test]
    fn reset_matches_inaddr_any_announced_entry() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let inaddr_any: SocketAddr = "0.0.0.0:5064".parse().unwrap();
        servers.insert(
            inaddr_any,
            BeaconState {
                last_id: 999,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 1000,
            },
        );

        // Coordinator forwards the TCP-circuit's real address, NOT
        // the INADDR_ANY beacon key.
        let circuit: SocketAddr = "10.0.0.5:5064".parse().unwrap();
        apply_reset_server(&mut servers, circuit);

        let s = servers.get(&inaddr_any).expect("entry preserved");
        assert!(
            s.period_estimate.is_none(),
            "INADDR_ANY-keyed entry must be reset by port-match"
        );
        assert_eq!(s.count, 0);
        assert_eq!(s.last_id, 999, "last_id preserved across reset");
    }

    /// Multi-homed IOC: beacon arrives via NIC A's IP, but our search
    /// reply landed via NIC B and the circuit talks to `B:port`.
    /// Beacon-state key is `A:port`. Same port-fallback applies.
    #[test]
    fn reset_matches_multihomed_announced_entry() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let nic_a: SocketAddr = "10.0.0.1:5064".parse().unwrap();
        servers.insert(
            nic_a,
            BeaconState {
                last_id: 42,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 100,
            },
        );

        let nic_b: SocketAddr = "10.0.0.2:5064".parse().unwrap();
        apply_reset_server(&mut servers, nic_b);

        let s = servers.get(&nic_a).expect("entry preserved");
        assert!(s.period_estimate.is_none());
        assert_eq!(s.count, 0);
    }

    /// Exact-match terminal regression. A single IOC announces with
    /// one IP per process (server/beacon.rs:46-58 computes
    /// `server_ip` once at task start and reuses it on every beacon),
    /// so a given IOC produces *one* beacon-state key — real IP OR
    /// INADDR_ANY, never both. If both `10.0.0.5:5064` and
    /// `0.0.0.0:5064` exist in the map at the same time, they are
    /// DIFFERENT IOCs. After exact-match resets the target,
    /// continuing on to also reset `0.0.0.0:5064` would silently
    /// blind the OTHER IOC's PeriodCollapse for the next ~4 beacons
    /// — same correctness regression as the rejected port-wide
    /// sweep, just narrower.
    #[test]
    fn reset_exact_does_not_cascade_to_inaddr_any_other_ioc() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let target: SocketAddr = "10.0.0.5:5064".parse().unwrap();
        let inaddr_any: SocketAddr = "0.0.0.0:5064".parse().unwrap();
        servers.insert(
            target,
            BeaconState {
                last_id: 1,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 1000,
            },
        );
        servers.insert(
            inaddr_any,
            BeaconState {
                last_id: 2,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 500,
            },
        );

        apply_reset_server(&mut servers, target);

        let t = servers.get(&target).expect("target preserved");
        assert!(t.period_estimate.is_none(), "exact-match target reset");
        assert_eq!(t.count, 0);

        let i = servers.get(&inaddr_any).expect("inaddr-any preserved");
        assert_eq!(
            i.period_estimate,
            Some(Duration::from_secs(15)),
            "INADDR_ANY entry from a different IOC must not be touched \
             after an exact-match hit"
        );
        assert_eq!(i.count, 500);
    }

    /// Cross-IOC blinding regression. In real CA networks many
    /// unrelated IOCs share the default port 5064. A naive port-only
    /// sweep would clear `count` and `period_estimate` for every
    /// `*:5064` entry on every reconnect — silently disabling
    /// PeriodCollapse (gated on `count > 3`) for the next ~4 beacons
    /// from each unrelated IOC. A neighbour IOC that restarts with a
    /// preserved beacon counter inside that window would go
    /// undetected.
    ///
    /// With the narrowed policy, an exact-match reset must touch ONLY
    /// the matched entry; unrelated same-port entries stay intact.
    #[test]
    fn reset_does_not_blind_other_same_port_ioc() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let target: SocketAddr = "10.0.0.5:5064".parse().unwrap();
        let neighbour: SocketAddr = "10.0.0.7:5064".parse().unwrap();
        servers.insert(
            target,
            BeaconState {
                last_id: 1000,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 1000,
            },
        );
        servers.insert(
            neighbour,
            BeaconState {
                last_id: 2000,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 5000,
            },
        );

        apply_reset_server(&mut servers, target);

        let t = servers.get(&target).expect("target preserved");
        assert!(t.period_estimate.is_none(), "exact-match target reset");
        assert_eq!(t.count, 0);

        let n = servers.get(&neighbour).expect("neighbour preserved");
        assert_eq!(
            n.period_estimate,
            Some(Duration::from_secs(15)),
            "unrelated same-port IOC must NOT have its EMA cleared — \
             that would disable PeriodCollapse on its next restart"
        );
        assert_eq!(n.count, 5000, "neighbour count untouched");
    }

    /// Ambiguous fallback: if neither exact nor INADDR_ANY hits and
    /// multiple `*:port` entries exist, we can't pick one safely —
    /// skip the reset entirely (post-reconnect cascade returns for
    /// the multi-homed-IOC + collision case, but no unrelated IOC is
    /// blinded).
    #[test]
    fn reset_skips_when_ambiguous_no_exact_no_inaddr_any() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let a: SocketAddr = "10.0.0.7:5064".parse().unwrap();
        let b: SocketAddr = "10.0.0.9:5064".parse().unwrap();
        servers.insert(
            a,
            BeaconState {
                last_id: 1,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 100,
            },
        );
        servers.insert(
            b,
            BeaconState {
                last_id: 2,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 200,
            },
        );

        // Reset for a circuit that doesn't match either entry.
        let circuit: SocketAddr = "10.0.0.5:5064".parse().unwrap();
        apply_reset_server(&mut servers, circuit);

        for key in [a, b] {
            let s = servers.get(&key).expect("entry preserved");
            assert_eq!(
                s.period_estimate,
                Some(Duration::from_secs(15)),
                "ambiguous fallback must not blind {key}"
            );
        }
    }

    /// INADDR_ANY hit must NOT cascade into a port-wide sweep —
    /// unrelated real-IP `*:port` entries stay intact even when the
    /// reset successfully resolves via INADDR_ANY.
    #[test]
    fn reset_via_inaddr_any_does_not_touch_unrelated_real_ip_entries() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let inaddr_any: SocketAddr = "0.0.0.0:5064".parse().unwrap();
        let unrelated: SocketAddr = "10.0.0.7:5064".parse().unwrap();
        servers.insert(
            inaddr_any,
            BeaconState {
                last_id: 1,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 100,
            },
        );
        servers.insert(
            unrelated,
            BeaconState {
                last_id: 2,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 200,
            },
        );

        let circuit: SocketAddr = "10.0.0.5:5064".parse().unwrap();
        apply_reset_server(&mut servers, circuit);

        let i = servers.get(&inaddr_any).expect("inaddr-any preserved");
        assert!(i.period_estimate.is_none(), "INADDR_ANY entry reset");

        let u = servers.get(&unrelated).expect("unrelated preserved");
        assert_eq!(
            u.period_estimate,
            Some(Duration::from_secs(15)),
            "unrelated same-port real-IP entry must not be touched"
        );
        assert_eq!(u.count, 200);
    }

    /// Different port = different IOC. Reset must NOT touch entries on
    /// unrelated ports (port-fallback's *only* fuzz axis is IP, not
    /// port).
    #[test]
    fn reset_leaves_other_port_entries_alone() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let other: SocketAddr = "0.0.0.0:5065".parse().unwrap();
        servers.insert(
            other,
            BeaconState {
                last_id: 7,
                last_seen: Instant::now(),
                period_estimate: Some(Duration::from_secs(15)),
                count: 50,
            },
        );

        let circuit: SocketAddr = "10.0.0.5:5064".parse().unwrap();
        apply_reset_server(&mut servers, circuit);

        let s = servers.get(&other).expect("entry preserved");
        assert_eq!(
            s.period_estimate,
            Some(Duration::from_secs(15)),
            "different-port entry must not be touched"
        );
        assert_eq!(s.count, 50);
    }

    /// Regression: under the standard production topology the
    /// client receives beacons via the CA repeater on LOCALHOST (see
    /// `run_beacon_monitor_inner` bind at line 160). The repeater
    /// rewrites `m_available` on the regular `CA_PROTO_RSRV_IS_UP`
    /// beacon to the original sender's source IP (`repeater.cpp:
    /// 626-630`, our `repeater.rs:224-228`); the 0xCAFE companion is
    /// forwarded verbatim and the kernel rewrites the L3 source IP
    /// to the repeater's loopback. The verified-tuple lookup key
    /// (post-fix: `(hdr.available, hdr.count, hdr.cid)`) therefore
    /// matches the companion-side insert key
    /// (`(signed_ip, signed_port, signed_beacon_id)`) without needing
    /// the L3 source IP to equal the announced server IP.
    ///
    /// An earlier version used `meta.src.ip()` for the lookup, which produced
    /// `127.0.0.1` under this topology — every legitimate signed
    /// beacon was dropped (`EPICS_CA_BEACON_REQUIRE_SIGNED=YES`,
    /// default). This test fixes the failure mode in place.
    #[cfg(feature = "cap-tokens")]
    #[test]
    fn verified_tuple_key_matches_via_repeater() {
        use crate::server::signed_beacon::{SignedBeaconEmitter, SignedBeaconVerifier};
        use ed25519_dalek::SigningKey;
        use rand_core::OsRng;
        use std::net::Ipv4Addr;
        use std::time::SystemTime;

        // Build a signed-beacon companion as the server would emit.
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let socket = std::sync::Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async { tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap() }),
        );

        let server_ip_u32 = u32::from_be_bytes([10, 0, 0, 5]);
        let server_port: u16 = 5064;
        let beacon_id: u32 = 42;
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let emitter = SignedBeaconEmitter::new(signing_key.clone(), socket, vec![]);
        let packet = emitter.build_packet(server_ip_u32, server_port, beacon_id, ts);

        // Verifier path (companion side).
        let mut verifier = SignedBeaconVerifier::new();
        verifier.trust(signing_key.verifying_key());
        let (verified_ip, verified_port, verified_bid) =
            verifier.verify(&packet).expect("signature verifies");

        // G3 source-IP binding: in the repeater topology meta.src is
        // 127.0.0.1, so the binding is intentionally relaxed. The
        // companion-frame insert proceeds because `via_repeater =
        // src_ip.is_loopback()` short-circuits the strict equality
        // check at line 319.
        let meta_src_via_repeater = Ipv4Addr::LOCALHOST;
        assert!(
            meta_src_via_repeater.is_loopback(),
            "topology precondition: client beacon socket binds to LOCALHOST"
        );

        // Insert as the companion path does — under the verifier
        // policy, the insert uses the SIGNED payload's announced ip.
        let mut verified_tuples: HashMap<(u32, u16, u32), Instant> = HashMap::new();
        verified_tuples.insert((verified_ip, verified_port, verified_bid), Instant::now());

        // Lookup as the regular-beacon path does (post-fix:
        // keyed by `hdr.available`, which the repeater rewrites to
        // the real server IP — equal to `verified_ip` here).
        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.data_type = 0;
        hdr.count = server_port;
        hdr.cid = beacon_id;
        // Repeater rewrite: hdr.available = original server's source IP.
        hdr.available = server_ip_u32;

        let lookup_ip_u32 = if hdr.available != 0 {
            hdr.available
        } else {
            u32::from_be_bytes(meta_src_via_repeater.octets())
        };
        let key = (lookup_ip_u32, hdr.count, hdr.cid);
        assert!(
            verified_tuples.contains_key(&key),
            "regression: regular beacon with hdr.available rewritten by \
             the repeater must hit the companion-inserted tuple"
        );

        // Sanity: the earlier key shape (meta.src.ip(), count, cid)
        // would have missed under the repeater topology.
        let r7_key = (
            u32::from_be_bytes(meta_src_via_repeater.octets()),
            hdr.count,
            hdr.cid,
        );
        assert!(
            !verified_tuples.contains_key(&r7_key),
            "documents the earlier failure mode: meta.src=127.0.0.1 key never matches"
        );
    }

    /// Direct-LAN fallback: when no repeater rewrites
    /// `hdr.available`, the lookup falls back to `meta.src.ip()` so
    /// the key still aligns with the companion-side insert. This is
    /// the original failure scenario, preserved for the case where
    /// a future caller binds the monitor socket to a non-loopback
    /// NIC.
    #[cfg(feature = "cap-tokens")]
    #[test]
    fn verified_tuple_key_falls_back_to_src_for_direct_lan() {
        use crate::server::signed_beacon::{SignedBeaconEmitter, SignedBeaconVerifier};
        use ed25519_dalek::SigningKey;
        use rand_core::OsRng;
        use std::net::Ipv4Addr;
        use std::time::SystemTime;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let socket = std::sync::Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async { tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap() }),
        );

        let server_ip = Ipv4Addr::new(10, 0, 0, 5);
        let server_ip_u32 = u32::from_be_bytes(server_ip.octets());
        let server_port: u16 = 5064;
        let beacon_id: u32 = 99;
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let emitter = SignedBeaconEmitter::new(signing_key.clone(), socket, vec![]);
        let packet = emitter.build_packet(server_ip_u32, server_port, beacon_id, ts);
        let mut verifier = SignedBeaconVerifier::new();
        verifier.trust(signing_key.verifying_key());
        let (verified_ip, verified_port, verified_bid) =
            verifier.verify(&packet).expect("signature verifies");

        let mut verified_tuples: HashMap<(u32, u16, u32), Instant> = HashMap::new();
        verified_tuples.insert((verified_ip, verified_port, verified_bid), Instant::now());

        // Direct-LAN: server emits hdr.available=0, no repeater
        // rewrites it. meta.src.ip() is the real server IP.
        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.data_type = 0;
        hdr.count = server_port;
        hdr.cid = beacon_id;
        hdr.available = 0;
        let meta_src = server_ip;

        let lookup_ip_u32 = if hdr.available != 0 {
            hdr.available
        } else {
            u32::from_be_bytes(meta_src.octets())
        };
        let key = (lookup_ip_u32, hdr.count, hdr.cid);
        assert!(
            verified_tuples.contains_key(&key),
            "direct-LAN fallback: meta.src.ip() lookup must hit when \
             hdr.available is zero"
        );
    }
}
