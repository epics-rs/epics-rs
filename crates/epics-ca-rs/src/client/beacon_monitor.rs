use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use epics_base_rs::net::AsyncUdpV4;
use epics_base_rs::runtime::sync::mpsc;

use crate::protocol::*;

use super::CoordRequest;

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
/// `IdMismatch` and `PeriodCollapse` are real restart signals and
/// warrant the full treatment (search wake-up + EchoProbe to
/// operational circuits, so a half-dead TCP gets surfaced fast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BeaconAnomalyKind {
    FirstSighting,
    IdMismatch,
    PeriodCollapse,
}

// ---------------------------------------------------------------------------
// Per-server beacon state
// ---------------------------------------------------------------------------

struct BeaconState {
    last_id: u32,
    last_seen: Instant,
    /// Estimated period between beacons (exponential moving average,
    /// alpha = 0.25). `None` until the second beacon arrives — at
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

pub(crate) async fn run_beacon_monitor(coord_tx: mpsc::UnboundedSender<CoordRequest>) {
    run_beacon_monitor_inner(
        coord_tx,
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
    verifier: std::sync::Arc<crate::server::signed_beacon::SignedBeaconVerifier>,
) {
    run_beacon_monitor_inner(coord_tx, Some(verifier)).await;
}

async fn run_beacon_monitor_inner(
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    #[cfg(feature = "cap-tokens")] verifier: Option<
        std::sync::Arc<crate::server::signed_beacon::SignedBeaconVerifier>,
    >,
) {
    // The CA repeater forwards every accepted beacon to its
    // registered clients over loopback only — there's no multi-NIC
    // routing here. Bind exclusively on `127.0.0.1` so we get the
    // SO_REUSEADDR-friendly per-NIC machinery for free without
    // wasting per-NIC sockets that would never see traffic.
    let socket = match AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Initial registration with retry
    for attempt in 0..3u32 {
        if register_with_repeater(&socket).await.is_ok() {
            break;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
        }
    }

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
    // Beacons are 16 B but the repeater may concatenate VERSION + RSRV_IS_UP
    // and forward client-noop traffic. Use 4 KB so chained datagrams are
    // received intact.
    let mut buf = [0u8; 4096];

    loop {
        // Use timeout to detect beacon silence → re-register with repeater
        let recv = tokio::time::timeout(REREGISTER_INTERVAL, socket.recv_from(&mut buf)).await;
        let (len, _src) = match recv {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => continue,
            Err(_) => {
                // No beacons for 5 minutes — repeater may have restarted
                let _ = register_with_repeater(&socket).await;
                continue;
            }
        };
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
            let payload_padded = ((hdr.postsize as usize) + 7) & !7;
            let frame_len = (CaHeader::SIZE + payload_padded).max(CaHeader::SIZE);
            // Bail out before advancing if the announced frame
            // length runs past the datagram. Otherwise the
            // post-advance slice clamp would silently hand the
            // verifier a truncated body and the parser would
            // continue from a misaligned offset (CR-10/F6).
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
                    // to the UDP source IP. A recorded valid companion
                    // can otherwise be replayed from anywhere; combined
                    // with the unbounded verified_tuples map below this
                    // is a poison amplifier.
                    let src_ip = match _src.ip() {
                        std::net::IpAddr::V4(v) => v,
                        std::net::IpAddr::V6(_) => {
                            metrics::counter!("ca_client_signed_beacon_failures_total")
                                .increment(1);
                            continue;
                        }
                    };
                    match v.verify(frame) {
                        Ok((ip, port, beacon_id)) if Ipv4Addr::from(ip) != src_ip => {
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
                let key = (hdr.available, hdr.count, hdr.cid);
                if !verified_tuples.contains_key(&key) {
                    metrics::counter!("ca_client_unsigned_beacon_drops_total").increment(1);
                    if require_signed {
                        continue;
                    }
                }
            }

            handle_beacon(hdr, &mut servers, &coord_tx);
        }
    }
}

fn handle_beacon(
    hdr: CaHeader,
    servers: &mut HashMap<SocketAddr, BeaconState>,
    coord_tx: &mpsc::UnboundedSender<CoordRequest>,
) {
    // count = server TCP port (CA v4.1+), data_type = protocol version.
    let server_port = if hdr.count != 0 {
        hdr.count
    } else {
        CA_SERVER_PORT
    };
    let beacon_id = hdr.cid;

    // New servers always set available=INADDR_ANY (0).  Use 0.0.0.0
    // as-is for beacon tracking — each IOC still has a unique port,
    // matching the approach used by the C CA client (libca).
    let server_ip = Ipv4Addr::from(hdr.available.to_be_bytes());
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

    // Anomaly: beacon_id not monotonically increasing (IOC restarted
    // with a fresh sequence), OR period suddenly dropped below 1/3 of
    // the estimated steady-state period (IOC restarted and is in its
    // fast-beacon initial phase). Also: first time we've seen this
    // server — libca treats unknown-server beacons as a hint to
    // re-search immediately so channels still in `Searching` wake up
    // on the new IOC instead of waiting their full bucket cycle.
    //
    // Floor the period-collapse check at 50 ms — multi-NIC duplicate
    // beacons that happen to use the next sequence id (rare but
    // possible if the network reorders) would otherwise still
    // satisfy `actual_interval < period_estimate / 3` for any
    // nonzero period. 50 ms safely separates "duplicate" from
    // "legitimate fast-beacon initial phase" (real IOCs send
    // every 100-500 ms during startup).
    const MIN_PERIOD_COLLAPSE_INTERVAL: Duration = Duration::from_millis(50);
    // Classify in priority order: FirstSighting wins because there's
    // no prior `last_id` / `period_estimate` to make the other two
    // checks meaningful. IdMismatch beats PeriodCollapse because a
    // real restart (id reset to 1) is the dispositive signal even if
    // the inter-beacon interval also happens to be sub-period.
    let anomaly_kind = if first_sighting {
        Some(BeaconAnomalyKind::FirstSighting)
    } else if beacon_id != expected_next_id {
        Some(BeaconAnomalyKind::IdMismatch)
    } else if entry.count > 3
        && actual_interval > MIN_PERIOD_COLLAPSE_INTERVAL
        && entry
            .period_estimate
            .is_some_and(|est| actual_interval < est / 3)
    {
        Some(BeaconAnomalyKind::PeriodCollapse)
    } else {
        None
    };

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
        // PeriodCollapse cascade against ramp-up beacon emitters.
        match entry.period_estimate {
            None => {
                entry.period_estimate = Some(actual_interval);
            }
            Some(prev) => {
                let alpha = 0.25;
                let new_estimate = Duration::from_secs_f64(
                    prev.as_secs_f64() * (1.0 - alpha) + actual_interval.as_secs_f64() * alpha,
                );
                entry.period_estimate = Some(new_estimate);
            }
        }
    }

    // Search-engine wake-up (libca `udpiiu::beaconAnomalyNotify`):
    // ONLY on a classified anomaly. The earlier "soft poke on every
    // beacon" code amplified normal beacon traffic into a permanent
    // fast-tick search storm whenever multiple IOCs beaconed within
    // the engine's revolution window — keep that path lean.
    if let Some(kind) = anomaly_kind {
        let _ = coord_tx.send(CoordRequest::ForceRescanServer { server_addr, kind });
    }

    // Transport-watchdog notification (libca `tcpRecvWatchdog::
    // beaconArrivalNotify` / `beaconAnomalyNotify`). Routed via the
    // coordinator to the per-circuit read loop, where it either
    // pushes the deadline forward (healthy beacon) or sets a sticky
    // anomaly flag (id-mismatch / period-collapse) that suppresses
    // subsequent healthy-beacon refreshes until the next data
    // arrival or echo response.
    //
    // FirstSighting is intentionally skipped — and this is a
    // deliberate divergence from libca, worth being honest about.
    // libca's `bhe.cpp:137` path (BHE freshly created via the
    // TCP-connect search-reply route, then first beacon arrives)
    // calls `beaconAnomalyNotify` as a precaution, setting the
    // tcpRecvWatchdog flag. We don't, on the reasoning that:
    //   * the next healthy beacon (≤ one beacon period) will
    //     refresh the deadline naturally, and
    //   * if the server actually restarted in that one-period
    //     gap, the existing 30 s idle-timeout echo handles it.
    // This keeps the FirstSighting path purely a search-engine
    // concern and avoids per-CaClient false flags on startup,
    // which was the original disconnect-storm trigger.
    let arrival_anomaly = match anomaly_kind {
        None => Some(false),
        Some(BeaconAnomalyKind::IdMismatch | BeaconAnomalyKind::PeriodCollapse) => Some(true),
        Some(BeaconAnomalyKind::FirstSighting) => None,
    };
    if let Some(anomaly) = arrival_anomaly {
        let _ = coord_tx.send(CoordRequest::BeaconArrival {
            server_addr,
            anomaly,
        });
    }
}

// ---------------------------------------------------------------------------
// Repeater registration
// ---------------------------------------------------------------------------

/// Register our socket with the CA repeater at localhost:5065.
async fn register_with_repeater(socket: &AsyncUdpV4) -> Result<(), ()> {
    // We bound to a single loopback NIC, so `local_addrs()` gives the
    // one ephemeral port we want to announce.
    let local_ip = socket
        .local_addrs()
        .into_iter()
        .find_map(|sa| match sa {
            SocketAddr::V4(v4) => Some(*v4.ip()),
            _ => None,
        })
        .unwrap_or(Ipv4Addr::LOCALHOST);

    let mut hdr = CaHeader::new(CA_PROTO_REPEATER_REGISTER);
    hdr.available = u32::from_be_bytes(local_ip.octets());

    let repeater_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, CA_REPEATER_PORT));
    socket
        .send_to(&hdr.to_bytes(), repeater_addr)
        .await
        .map_err(|_| ())?;

    // Wait for REPEATER_CONFIRM.
    let mut buf = [0u8; 64];
    let result = tokio::time::timeout(Duration::from_millis(500), async {
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
        handle_beacon(hdr, &mut servers, &tx);
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
        handle_beacon(hdr, &mut servers, &tx);
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
        handle_beacon(hdr, &mut servers, &tx);
        assert!(rx.try_recv().is_ok());

        // Sub-50ms later, IOC restarts: id resets to 1 (not the
        // expected id=101). period_estimate is 15s default; even
        // though actual_interval < 50ms now, the id-mismatch branch
        // must catch the restart.
        hdr.cid = 1;
        handle_beacon(hdr, &mut servers, &tx);
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

    /// Period collapse with monotonic ids (e.g. an IOC abruptly
    /// switching from 15-s steady-state to 200-ms fast-beacon mode
    /// without resetting the id counter) must classify as
    /// `PeriodCollapse`, not `FirstSighting` or `IdMismatch`. The
    /// coordinator routes this to the EchoProbe path because a real
    /// restart sometimes manifests this way (re-IOC scripts that
    /// preserve the counter across restarts).
    #[test]
    fn period_collapse_classifies_as_period_collapse() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        // Pre-seed a steady-state entry: 15-s period_estimate, 10
        // beacons in, last_seen far enough back that
        // actual_interval > 50 ms but < 5 s = period_estimate / 3.
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
        handle_beacon(hdr, &mut servers, &tx);
        assert!(
            matches!(
                rx.try_recv(),
                Ok(CoordRequest::ForceRescanServer {
                    kind: BeaconAnomalyKind::PeriodCollapse,
                    ..
                })
            ),
            "monotonic-id, sub-period interval must classify as PeriodCollapse"
        );
    }

    /// Legitimate fast-beacon (e.g. 200 ms cadence) with monotonically
    /// increasing ids must NOT trip the period-collapse branch — only
    /// the `first_sighting = true` path on the very first beacon. This
    /// tests that the 50 ms floor doesn't fire spurious anomalies on
    /// Regression guard: rsrv `online_notify_task` ramp-up beacons
    /// (20 ms doubling to 15 s — same pattern epics-ca-rs's own
    /// `server/beacon.rs` emits) must NOT fire a stream of
    /// `PeriodCollapse` anomalies on the FIRST sighting of a
    /// freshly-started IOC. Pre-fix the per-server initial
    /// `period_estimate = Duration::from_secs(15)` placeholder
    /// caused every ramp-up beacon past the 50 ms floor (so the
    /// 4th beacon onwards) to satisfy
    /// `actual_interval < 15 s / 3 = 5 s` and trip
    /// `PeriodCollapse`. Mini-beamline IOC users observed this as
    /// 5-s `get_with_metadata(timeout=2.0)` failures driven by the
    /// transport watchdog flag → echo probe → reconnect cascade
    /// downstream. Fix mirrors libca `bhe.cpp:51,199` where
    /// `averagePeriod = -DBL_MAX` until the first measured
    /// `currentPeriod` defines it.
    ///
    /// We reproduce the standard rsrv ramp-up: 20 ms, 40 ms,
    /// 80 ms, 160 ms, 320 ms, 640 ms, 1.28 s, 2.56 s, 5.12 s,
    /// 10.24 s, then capped at 15 s. Only the very first beacon
    /// should fire (FirstSighting). All subsequent ramp-up
    /// beacons must classify as steady-state (no anomaly).
    #[test]
    fn rsrv_rampup_beacons_do_not_fire_period_collapse() {
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
        handle_beacon(hdr, &mut servers, &tx);
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
            handle_beacon(hdr, &mut servers, &tx);

            // Inspect every emitted CoordRequest; PeriodCollapse
            // would surface as a `ForceRescanServer { kind:
            // PeriodCollapse, .. }` here — and that's the bug.
            while let Ok(msg) = rx.try_recv() {
                if let CoordRequest::ForceRescanServer { kind, .. } = msg {
                    assert_ne!(
                        kind,
                        BeaconAnomalyKind::PeriodCollapse,
                        "ramp-up beacon #{} (interval={} ms) must not classify \
                         as PeriodCollapse — see BeaconState::period_estimate doc",
                        i + 2,
                        ms
                    );
                }
            }
        }
    }

    /// healthy fast cadences.
    #[test]
    fn fast_cadence_monotonic_ids_does_not_fire_spurious_anomaly() {
        let mut servers: HashMap<SocketAddr, BeaconState> = HashMap::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<CoordRequest>();

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);

        // Five monotonically increasing beacons (ids 100..105). First
        // is first_sighting → ForceRescanServer fires once. Rest must
        // not fire any ForceRescanServer (they will, however, fire
        // BeaconArrival{anomaly=false} — that's the libca-style
        // healthy-beacon watchdog refresh and is correct here).
        for id in 100..105 {
            hdr.cid = id;
            handle_beacon(hdr, &mut servers, &tx);
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
        handle_beacon(hdr, &mut servers, &tx);
        // Drain first-sighting messages.
        while rx.try_recv().is_ok() {}

        // Restart: id resets to 1.
        hdr.cid = 1;
        handle_beacon(hdr, &mut servers, &tx);
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
        handle_beacon(hdr, &mut servers, &tx);

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

        let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
        hdr.count = 5064;
        hdr.available = u32::from_be_bytes([127, 0, 0, 1]);
        // Establish steady-state beacons (ids 100..103).
        for id in 100..103 {
            hdr.cid = id;
            handle_beacon(hdr, &mut servers, &tx);
        }
        while rx.try_recv().is_ok() {}

        // Advance of 2 (last_id = 102, next id = 104) — must drop.
        hdr.cid = 104;
        handle_beacon(hdr, &mut servers, &tx);
        assert!(
            rx.try_recv().is_err(),
            "advance=2 must be silently dropped, not classified as anomaly"
        );

        // Advance of 3 from the just-updated 104 — also drop.
        hdr.cid = 107;
        handle_beacon(hdr, &mut servers, &tx);
        assert!(rx.try_recv().is_err(), "advance=3 must be silently dropped");

        // last_id should now be 107 (drop path still updates it).
        // The next monotonic beacon (108 = advance=1) is healthy.
        hdr.cid = 108;
        handle_beacon(hdr, &mut servers, &tx);
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
            handle_beacon(hdr, &mut servers, &tx);
        }
        while rx.try_recv().is_ok() {}

        // last_id is 102. A late copy with id=101 — wrapping_sub
        // gives u32::MAX (advance treated as 0xFFFFFFFF, > MAX-256).
        hdr.cid = 101;
        handle_beacon(hdr, &mut servers, &tx);
        assert!(
            rx.try_recv().is_err(),
            "backwards-by-1 (within 256) must drop"
        );
    }

    #[test]
    fn stale_prune_drops_idle_entries_only() {
        let now = Instant::now();
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
}
