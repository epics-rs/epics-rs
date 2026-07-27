// The remaining #[tokio::test]s here are mod-gated to `tokio_backend`: they
// drive the UDP SEARCH transport, which only exists there. The one per-test
// #[cfg(not(feature = "rtems-exec-model"))] site
// (stage_c1_name_servers_only_resolves_without_binding_udp) cannot run on the
// exec backend until Stage C2/C3.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

// The UDP SEARCH transport is compiled out wherever a spawned future has no
// tokio reactor — `cfg(exec_backend)`, which is either embedded target
// (RTEMS or VxWorks) *and* a host `--features rtems-exec-model` build. Two
// facts, one gate:
//
//   * On RTEMS `AsyncUdpV4` does not exist at all (it is host-only in
//     `epics-base-rs`): newlib has no `recvmsg`/`IP_PKTINFO` receive path and
//     cannot read a socket's `local_addr()` back. It is gated out on VxWorks
//     too — `tokio::net`/`socket2`/`if-addrs` do not build for either
//     embedded target, so `AsyncUdpV4` stays one host-only module rather than
//     splitting its absence into a separate reason per target.
//   * On either backend-free build the engine runs on a callback-pool worker
//     (`runtime::task::spawn`), and `tokio::net::UdpSocket` panics there —
//     "there is no reactor running" — even when the process has a runtime
//     somewhere else, because it is not entered on that worker.
//
// Gating this on `not(target_os = "rtems")` named the first fact and missed
// the second, so a hosted `rtems-exec-model` build compiled the UDP transport
// in, selected it, and panicked at the first search
// (`doc/calink-rtems-design.md` §10.10 item 2, measured). `tokio_backend` is
// the predicate that means "a reactor exists" and is the one the whole UDP
// surface below carries, so "this build binds no UDP socket" holds by
// construction rather than by a runtime branch. The target's compiled surface
// is unchanged: `epics_embedded_target` (RTEMS or VxWorks) implies
// `exec_backend`.
//
// Either way the client resolves every PV over TCP name servers through
// `SearchTransport::NameServersOnly` (design §4.2, §4.5).
#[cfg(tokio_backend)]
use epics_base_rs::net::AsyncUdpV4;
use epics_base_rs::runtime::sync::mpsc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
// The `runtime::task` seam, not `tokio::time::interval`: this engine is
// compiled for the RTEMS target, where the periodic ticker is the delayed
// callback timer and there is no tokio timer to drive tokio's own.
use epics_base_rs::runtime::task::interval;

use crate::protocol::*;

use super::circuit_breaker::CircuitBreakerRegistry;
use super::types::{SearchAttempts, SearchReason, SearchRequest, SearchResponse};
use std::sync::atomic::{AtomicU32, Ordering};

/// Snippet of a UDP/TCP search-response datagram, plus the address it
/// arrived from. Used to feed nameserver TCP responses through the same
/// `handle_udp_response` parser as plain UDP search replies.
type ParsedDatagram = (Vec<u8>, SocketAddr);

/// What the engine's SEARCH-receive arm needs from one datagram.
///
/// The three fields `epics_base_rs::net::RecvMeta` carries that this loop
/// reads, restated as a transport-neutral type — because `RecvMeta` is part of
/// the UDP stack and does not exist for `armv7-rtems-eabihf`, while the
/// `select!` arm that consumes it must still compile there (a `select!` branch
/// cannot carry a `#[cfg]`). On the target the arm parks forever, so no value
/// of this type is ever produced; what matters is that its *type* is nameable.
struct SearchDatagram {
    /// Datagram length in the caller's buffer.
    n: usize,
    /// Sender address.
    src: SocketAddr,
    /// IPv4 address of the NIC that received it — the key the per-NIC
    /// `SO_RXQ_OVFL` drop counters are tracked under.
    iface_ip: Ipv4Addr,
}

/// Send `buf` toward `addr`, expanding to a per-NIC fanout when the
/// destination is the limited broadcast `255.255.255.255` or an IPv4
/// multicast group (`224.0.0.0/4`). Per-subnet broadcasts and
/// unicast destinations route via the NIC chosen by [`AsyncUdpV4`].
#[cfg(tokio_backend)]
async fn send_with_fanout(
    socket: &AsyncUdpV4,
    buf: &[u8],
    addr: SocketAddr,
    site: &'static str,
    send_errors: &mut HashMap<SocketAddr, std::io::ErrorKind>,
) {
    let needs_fanout = match addr {
        SocketAddr::V4(v4) => v4.ip().is_broadcast() || v4.ip().is_multicast(),
        SocketAddr::V6(_) => false,
    };
    let result = if needs_fanout {
        socket.fanout_to(buf, addr).await.map(|_| ())
    } else {
        socket.send_to(buf, addr).await.map(|_| ())
    };
    match result {
        Ok(()) => {
            // libca cae597d: log once-on-recovery so operators know
            // when a broken destination came back.
            if let Some(prev) = send_errors.remove(&addr) {
                tracing::info!(
                    target: "epics_ca_rs::search",
                    %addr, site, prev_error = ?prev,
                    "search send_to: recovered"
                );
            }
        }
        Err(e) => {
            // P-7 + libca cae597d (`udpiiu::SearchDestUDP::_lastError`):
            // log on first occurrence and on error-kind change; suppress
            // repeated identical errors so a persistent EHOSTUNREACH
            // doesn't flood the log at search rate.
            let kind = e.kind();
            let prev = send_errors.insert(addr, kind);
            if prev != Some(kind) {
                tracing::warn!(
                    target: "epics_ca_rs::search",
                    %addr,
                    site,
                    error = %e,
                    "search send_to failed"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration constants
// ---------------------------------------------------------------------------

/// pvxs `client.cpp::nBuckets`. 30 buckets at 1 s normal interval gives
/// each pending search a 30-second slot rotation — cooperative tick
/// caps UDP search traffic at roughly `pending.len() / 30` packets per
/// second instead of letting every channel fire on its own backoff.
const N_SEARCH_BUCKETS: usize = 30;

/// Decide which bucket to drop a fresh search into based on the
/// caller's intent. Pure function so the production handler and the
/// unit tests share the formula and can't drift apart.
///
/// - `Initial` / `BeaconAnomaly` (new cid): `current_bucket + 1`. The
///   handler ALSO fires an immediate broadcast for `Initial`; the +1
///   placement is so the first scheduled retry lands one tick after
///   the immediate fire. `BeaconAnomaly` for a new cid relies on
///   the engine's fast-tick mode to retransmit within ~6 s, so the
///   +1 placement gets caught by the next fast tick.
/// - `Reconnect`: `current_bucket`. Mirrors pvxs `Channel::disconnect`
///   (client.cpp:213) with `holdoff = 0` — the typical Active→
///   disconnect case sits in the current bucket and the next 1 Hz
///   tick fires the broadcast. Latency ≤ 1 s.
///
/// Cascade-spread (5000 channels disconnecting simultaneously) is
/// handled by the natural O(N / nBuckets) per-tick rate-limit and
/// the runtime-side smoothing in `cascade_smoothed_next` — no
/// per-channel cid hashing needed for the first attempt.
fn placement_bucket(current_bucket: usize, reason: SearchReason) -> usize {
    match reason {
        SearchReason::Initial | SearchReason::BeaconAnomaly => {
            (current_bucket + 1) % N_SEARCH_BUCKETS
        }
        SearchReason::Reconnect => current_bucket,
    }
}

/// Compute the next-retry bucket for a search that just transmitted.
/// Mirrors pvxs `tickSearch` (client.cpp:1193-1206):
///
///   `next = (idx + nSearch) % nBuckets`, where `nSearch` is the
///   per-channel attempt counter, capped at `nBuckets`. Each retry
///   pushes the search forward by one more bucket: 1 s, 2 s, 3 s,
///   ..., capping at the 30 s ring period.
///
/// Cascade smoothing (line 1199-1206 in pvxs): when the chosen
/// `next` bucket is overloaded relative to the bucket immediately
/// after it (>100 entries more), defer to that one. Distributes a
/// mass-disconnect across two ticks instead of one. Threshold is
/// strictly `>` 100, matching pvxs.
///
/// `attempt` is 1-based (1 means "this is the first retransmit
/// after the initial bucket-fire"). The earlier
/// `RETRY_HOLDOFF_CYCLES = 10` mechanism conflated pvxs's pre-
/// CREATE_CHANNEL holdoff (which only applies to the
/// `Channel::Connecting` state) with the steady-state retry
/// cadence; pvxs uses the `nSearch` increment for the latter.
fn cascade_smoothed_next(
    current_bucket: usize,
    attempt: u32,
    bucket_sizes: impl Fn(usize) -> usize,
) -> usize {
    let n_search = (attempt as usize).min(N_SEARCH_BUCKETS);
    let next = (current_bucket + n_search) % N_SEARCH_BUCKETS;
    let nextnext = (next + 1) % N_SEARCH_BUCKETS;
    let next_n = bucket_sizes(next);
    let nextnext_n = bucket_sizes(nextnext);
    if next_n > nextnext_n && next_n - nextnext_n > 100 {
        nextnext
    } else {
        next
    }
}

/// C default for `EPICS_CA_MAX_SEARCH_PERIOD`
/// (`epics-base:modules/ca/src/client/udpiiu.h:87`,
/// `maxSearchPeriodDefault = 5.0 * 60.0`) — a hand-copy, in C, of the parameter's
/// compiled default. Here it comes from the generated `ENV_PARAM` table, so the
/// two cannot disagree.
fn max_search_period_default_secs() -> f64 {
    epics_base_rs::runtime::env_table::EPICS_CA_MAX_SEARCH_PERIOD
        .default_str()
        .parse()
        .expect("EPICS_CA_MAX_SEARCH_PERIOD's compiled default is a number")
}

/// C lower bound for `EPICS_CA_MAX_SEARCH_PERIOD`
/// (`epics-base:modules/ca/src/client/udpiiu.h:88`,
/// `maxSearchPeriodLowerLimit = 60.0`).
const MAX_SEARCH_PERIOD_LOWER_LIMIT_SECS: f64 = 60.0;

/// C `minRoundTripEstimate` (`epics-base:modules/ca/src/client/udpiiu.h:85`,
/// `32e-3`) — the bottom rung of C's search-timer ladder, and the unit the
/// upper bound below is expressed in.
const MIN_ROUND_TRIP_ESTIMATE_SECS: f64 = 32e-3;

/// C `channelNode::getMaxSearchTimerCount()`
/// (`epics-base:modules/ca/src/client/nciu.cpp:606-611`): the channel-state
/// enum holds `cs_searchReqPending0 .. cs_searchReqPending17`, so the ladder
/// is 18 rungs and cannot express a period past
/// `(1 << 17) * minRoundTripEstimate`.
const MAX_SEARCH_TIMER_COUNT: u32 = 18;

/// The period C's ladder tops out at: `(1 << (nTimers - 1)) * RTT`
/// = `131072 * 0.032` = 4194.304 s. C prints exactly this figure when it
/// clamps (`udpiiu.cpp:105-107`).
fn max_search_period_upper_limit_secs() -> f64 {
    f64::from(1u32 << (MAX_SEARCH_TIMER_COUNT - 1)) * MIN_ROUND_TRIP_ESTIMATE_SECS
}

/// C `getNTimers` (`epics-base:modules/ca/src/client/udpiiu.cpp:96-99`):
/// `static_cast<unsigned>(1.0 + log(maxPeriod / minRoundTripEstimate) / log(2.0))`.
///
/// Written with the same `ln(x) / ln(2)` C uses rather than `log2`, which is a
/// different libm routine and can land the boundary case on the other side of
/// the truncation.
fn search_timer_count(period_secs: f64) -> u32 {
    (1.0 + (period_secs / MIN_ROUND_TRIP_ESTIMATE_SECS).ln() / std::f64::consts::LN_2) as u32
}

/// `EPICS_CA_MAX_SEARCH_PERIOD` resolution, faithful
/// to C `udpiiu.cpp::getMaxPeriod` (`epics-base:modules/ca/src/client/udpiiu.cpp:68-94`):
///
/// - env unset → the documented default of 300 s.
/// - env set and parses as a real number → that value, clamped *up*
///   to the 60 s lower limit if below it. C applies no upper clamp
///   to the period itself (the upper bound is on the derived timer
///   count, not the period).
/// - env set but not a real number → keep the 300 s default
///   (C's `longStatus != 0` branch).
///
/// C does not reject negative or zero values — they pass `parse` and
/// are caught by the `< 60` lower-limit clamp — so this mirrors C by
/// clamping rather than filtering.
///
/// Resolved once per process (C `udpiiu`'s constructor calls `getMaxPeriod`
/// once and keeps the result): a second read could pick up a mutated
/// environment and would repeat the diagnostics below.
fn max_search_period_secs() -> f64 {
    static RESOLVED: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(resolve_max_search_period_secs)
}

/// The uncached resolution behind [`max_search_period_secs`].
fn resolve_max_search_period_secs() -> f64 {
    let param = epics_base_rs::runtime::env_table::EPICS_CA_MAX_SEARCH_PERIOD;
    let name = param.name();
    let default_secs = max_search_period_default_secs();
    // Unset resolves to the compiled default string and parses, silently — so
    // only a set-but-bad value reaches the error arm.
    let v = match param.double() {
        Ok(v) => v,
        // C `getMaxPeriod` (`udpiiu.cpp:85-90`), verbatim.
        Err(_) => {
            eprintln!("EPICS \"{name}\" wasn't a real number");
            eprintln!("Setting \"{name}\" = {default_secs:.6} seconds");
            return default_secs;
        }
    };
    // C `udpiiu.cpp:78-83`: below the 60 s lower limit, say so and clamp.
    // NaN takes this branch too — C's `maxPeriod < lowerLimit` is false for
    // it and C then drives its timer wheel off a NaN period; clamping is the
    // one deviation here, because a NaN tick would stop the client searching
    // altogether.
    if v.is_nan() || v < MAX_SEARCH_PERIOD_LOWER_LIMIT_SECS {
        eprintln!("\"{name}\" out of range (low)");
        eprintln!("Setting \"{name}\" = {MAX_SEARCH_PERIOD_LOWER_LIMIT_SECS:.6} seconds");
        return MAX_SEARCH_PERIOD_LOWER_LIMIT_SECS;
    }
    // C's upper bound is not on the period: `getNTimers` (`udpiiu.cpp:96-111`)
    // turns the period into a rung count on the RTT-doubling ladder and clamps
    // THAT to 18, so a period the ladder cannot reach is refused with the
    // "(high)" pair and the search cadence tops out at the 18th rung. Recompute
    // the count with C's expression — `1.0 + log(period/RTT)/log(2.0)`,
    // truncated by `static_cast<unsigned>` — rather than a threshold in
    // seconds, so the boundary lands on the same side as C's for every input:
    // 8388.607 passes, 8388.608 (== 0.032 * 2^18) clamps, verified against the
    // compiled caget.
    //
    // The one deviation is `inf`, where C's cast is undefined and the compiled
    // caget aborts in malloc with a nTimers of garbage size. Rust's `as u32`
    // saturates, so an infinite period clamps to the same 4194.304 s any other
    // out-of-range period gets.
    if search_timer_count(v) > MAX_SEARCH_TIMER_COUNT {
        let capped = max_search_period_upper_limit_secs();
        eprintln!("\"{name}\" out of range (high)");
        eprintln!("Setting \"{name}\" = {capped:.6} seconds");
        return capped;
    }
    v
}

/// Normal tick cadence. Rust's search model is structurally
/// different from C's per-cid exponential-backoff timer wheel — a
/// fixed `N_SEARCH_BUCKETS = 30` ring advancing one bucket per tick
/// caps the per-cid retry period at `N_SEARCH_BUCKETS * tick`. To
/// honour `EPICS_CA_MAX_SEARCH_PERIOD` we derive the tick so that
/// one full ring revolution equals the resolved period:
/// `tick = period / N_SEARCH_BUCKETS`.
///
/// With the C-faithful period (default 300 s, lower-limited at
/// 60 s — see [`max_search_period_secs`]) the tick is always
/// `>= 60/30 = 2 s`; the default 300 s yields a 10 s tick.
///
/// DESIGN NOTE — intentional cadence deviation from libca. Upstream CA
/// seeds each channel's UDP search timer from `minRoundTripEstimate`
/// (32 ms; `epics-base:modules/ca/src/client/udpiiu.h:85`) and doubles
/// the period per miss — `(1 << index) * RTT`
/// (`searchTimer.cpp:391-395`) — so a lost initial SEARCH is re-sent
/// several times within the first second, with `maxSearchPeriod` acting
/// only as the cap on that exponential ladder. This client deliberately
/// does NOT replicate the RTT ladder: it uses the max-period-derived
/// 30-bucket ring as the *normal* cadence, trading libca's aggressive
/// sub-second early retries for bucketed load shaping (a bounded,
/// even-rate retransmit volume across many channels). The operational
/// cost is that a dropped *initial* UDP SEARCH waits one bucket tick
/// (seconds-to-tens-of-seconds at the default period) before its first
/// retry, which can lengthen short client-side discovery waits such as
/// `caget -w`. Fast discovery is instead recovered out-of-band by the
/// beacon-poke `FAST_TICK` path. If libca-style short-wait discovery
/// becomes a goal, the fix is an RTT-derived early-retry path for
/// `Initial` searches, not a change to this normal-cadence tick.
fn normal_tick() -> Duration {
    normal_tick_for(max_search_period_secs())
}

/// `tick = period / N_SEARCH_BUCKETS`, split out so the env-boundary tests
/// can drive it from an uncached period.
fn normal_tick_for(period_secs: f64) -> Duration {
    crate::estdlib::duration_from_secs(period_secs / N_SEARCH_BUCKETS as f64)
}

/// Fast-mode tick cadence after a beacon poke. One full bucket
/// revolution fits in `N_SEARCH_BUCKETS * FAST_TICK = 6 s`.
const FAST_TICK: Duration = Duration::from_millis(200);

/// Maximum bytes per outbound UDP datagram.
const MAX_UDP_SEND: usize = 1024;

/// Penalty hold-off after a failed connect to a server.
const PENALTY_DURATION: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Per-channel search state
// ---------------------------------------------------------------------------

struct PendingSearch {
    #[allow(dead_code)]
    cid: u32,
    #[allow(dead_code)]
    pv_name: String,
    /// Pre-built payload: SEARCH header + padded PV name (no VERSION prefix).
    search_payload: Vec<u8>,
    /// Which bucket this search currently lives in.
    bucket: usize,
    /// Number of times this search has been broadcast. 0 before the
    /// first transmit; doubles as the pvxs `nSearch` counter that
    /// controls retry-bucket escalation in `cascade_smoothed_next`
    /// — each retry pushes the search forward by `min(attempt,
    /// nBuckets)` buckets, giving the 1 s, 2 s, 3 s, ..., 30 s
    /// pattern.
    attempt: u32,
    #[allow(dead_code)]
    last_attempt: Option<Instant>,
}

// ---------------------------------------------------------------------------
// Penalty box
// ---------------------------------------------------------------------------

struct PenaltyEntry {
    until: Instant,
}

// ---------------------------------------------------------------------------
// Top-level engine state
// ---------------------------------------------------------------------------

struct SearchEngineState {
    pending: HashMap<u32, PendingSearch>,
    buckets: Vec<Vec<u32>>,
    current_bucket: usize,
    /// Shared per-channel SEARCH attempt counter — bumped by
    /// `fire_searches` on every fanout (immediate first SEARCH AND
    /// each bucket-tick retransmit) so
    /// [`super::CaChannel::search_attempts`] (CA-035) returns the
    /// same number `ca_search_attempts(chid)` returns in libca.
    /// Entry is removed on Cancel and on successful CREATE_CHANNEL
    /// reply (mirrors C reset on circuit attach).
    attempts: SearchAttempts,
    /// After a beacon poke we run one full revolution at FAST_TICK
    /// cadence so all pending searches retry within ~6 s.
    fast_ticks_remaining: u32,
    penalty: HashMap<SocketAddr, PenaltyEntry>,
    /// Per-server failure-pattern tracker. Sits on top of the single-shot
    /// `penalty` box: when failures repeat within a window, the breaker
    /// trips OPEN with an exponentially-doubled cooldown so we don't
    /// hammer a flapping server.
    breakers: CircuitBreakerRegistry,
    /// Rolling per-datagram sequence number, embedded in the outgoing
    /// VERSION header CID field with the `sequenceNoIsValid` marker
    /// (matches C `dgSeqNo`). libca's server echoes it so libca's
    /// `searchTimer` can score RTT and drop stale rounds; the Rust
    /// client sends it for wire-parity but — like C `searchRespAction`
    /// — resolves every SEARCH reply unconditionally, so the echoed
    /// value is never consumed (libca's timer tuning is not modelled by
    /// the Rust retry-ring; see `handle_search_response`).
    dgram_seq: u32,
    /// `EPICS_RS_CLIENT_IGNORE` filter snapshot taken at startup.
    /// Rust-only client-side extension — NOT the C
    /// `EPICS_IOC_IGNORE_SERVERS` (server-side; see
    /// `client::epics_rs_client_ignore` docstring for the naming
    /// rationale). Any SEARCH reply whose announced server IP — or
    /// the datagram source IP — appears here is dropped before the
    /// per-channel attempt counter is consulted. Held in `HashSet`
    /// for O(1) lookup; updated only at engine start (env changes
    /// mid-run are not picked up to keep the hot path lock-free).
    ignored_servers: std::collections::HashSet<Ipv4Addr>,
    /// Per-cid resolved-server tracker for
    /// multiply-defined-PV detection. libca
    /// `cac.cpp::transferChanToVirtCircuit` (lines 591-661) consults
    /// the channel's currently-resolved circuit address on EVERY
    /// SEARCH reply for a known cid — the detection window extends
    /// until the `nciu` is destroyed (Cancel / channel drop), not
    /// just until first CREATE_CHAN ack. Earlier Rust cleared this
    /// map on `remove_channel` which fired from `ConnectResult{
    /// success:true}` too, closing the detection window at the very
    /// moment the duplicate-detect was most useful (a slower second
    /// IOC replying after the connect handshake completed). Now
    /// `Cancel`-only clears. Bounded at
    /// `MULTIPLY_DEFINED_RESOLVED_CAP` to cap memory.
    resolved: HashMap<u32, (String, SocketAddr)>,
}

const MULTIPLY_DEFINED_RESOLVED_CAP: usize = 1024;

impl SearchEngineState {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_attempts(std::sync::Arc::new(dashmap::DashMap::new()))
    }

    fn with_attempts(attempts: SearchAttempts) -> Self {
        Self {
            pending: HashMap::new(),
            buckets: (0..N_SEARCH_BUCKETS).map(|_| Vec::new()).collect(),
            current_bucket: 0,
            attempts,
            fast_ticks_remaining: 0,
            penalty: HashMap::new(),
            breakers: CircuitBreakerRegistry::new(),
            dgram_seq: 0,
            ignored_servers: super::epics_rs_client_ignore().into_iter().collect(),
            resolved: HashMap::new(),
        }
    }

    /// Remove a channel entirely (Cancel, channel drop).
    fn remove_channel(&mut self, cid: u32) {
        if let Some(p) = self.pending.remove(&cid) {
            self.buckets[p.bucket].retain(|x| *x != cid);
        }
        self.attempts.remove(&cid);
        // drop the multiply-defined tracker only on Cancel /
        // channel destruction. A new CREATE_CHAN for the same cid
        // (which only happens via reuse after cancel) is a fresh
        // lifecycle. NOT cleared on `ConnectResult{success:true}`
        // alone — that path now calls `mark_connected` instead so
        // the duplicate-detect window stays open for the channel's
        // connected lifetime (matches libca
        // `cac.cpp:621-641`).
        self.resolved.remove(&cid);
    }

    /// bookkeeping hook called on connect-success (the cid
    /// stays in `resolved` so post-handshake duplicate SEARCH replies
    /// from a *different* server still fire the multiply-defined
    /// diagnostic, matching libca's connected-lifetime detection
    /// window).
    fn mark_connected(&mut self, _cid: u32) {
        // Intentionally a no-op today — the `resolved` entry is
        // already kept past Found. The helper exists so the
        // coordinator's `ConnectResult{success:true}` path can
        // declare intent (vs. silently calling `remove_channel`).
    }

    /// pvxs `client.cpp:713 poke()` parity: reset every pending
    /// search's attempt + holdoff counters and start the engine's
    /// fast-tick revolution. Searches stay in their assigned buckets;
    /// fast-tick (200 ms) covers the full ring in 6 s so each pending
    /// search retries once within that window.
    fn poke(&mut self) {
        for p in self.pending.values_mut() {
            // NOTE: more aggressive than pvxs's `poked` semantic
            // (which preserves nSearch and just skips its increment
            // for one tick). Resetting attempt to 0 means the
            // post-poke retries cascade from the 1-bucket forward
            // push from scratch — rapid retransmits during the
            // fast-tick window. Acceptable trade for single-channel
            // recovery; under mass-disconnect cascades it spends
            // more UDP bandwidth than pvxs would.
            p.attempt = 0;
            p.last_attempt = None;
        }
        self.fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
    }
}

// ---------------------------------------------------------------------------
// Search transport
// ---------------------------------------------------------------------------

/// The UDP half of the search transport: the per-NIC socket bundle, plus
/// the destination policy and the per-destination diagnostics that only
/// mean anything while those sockets exist.
///
/// Bundled into one value so a socket and the addresses it would transmit
/// to cannot be configured independently. The membership test is whether a
/// field can outlive the socket it describes: `addr_list` holds UDP SEARCH
/// destinations (CA never opens a TCP circuit to an `EPICS_CA_ADDR_LIST`
/// entry — only `EPICS_CA_NAME_SERVERS` entries get one), `send_errors`
/// keys the libca `_lastError` suppression by those same destinations, and
/// `prev_drops_per_iface` keys the `SO_RXQ_OVFL` transition log by the NIC
/// of the socket that received the datagram. None of the three survives the
/// socket's absence.
#[cfg(tokio_backend)]
struct UdpTransport {
    /// libca-style multi-NIC bundle: one bound socket per IPv4 interface so
    /// `255.255.255.255` and per-subnet broadcasts each leave via the
    /// matching NIC.
    socket: AsyncUdpV4,
    /// Working UDP SEARCH destination list — `EPICS_CA_ADDR_LIST` as
    /// parsed at startup, plus any discovery / programmatic mutation
    /// applied since (libca `addAddrToChannelAccessAddressList`,
    /// `configureChannelAccessAddressList`, `iocinf.cpp:45`, `:166`).
    addr_list: Vec<super::AddrEntry>,
    /// Per-destination last UDP send-error kind. Mirrors libca cae597d
    /// (`udpiiu::SearchDestUDP::_lastError`): a persistent sendto()
    /// failure (e.g. firewall, unreachable broadcast) repeats at search
    /// rate (~30 ms) and would otherwise spam logs. We log on first
    /// occurrence, on errno change, and on recovery; suppress repeats.
    send_errors: HashMap<SocketAddr, std::io::ErrorKind>,
    /// pvxs parity: per-NIC `SO_RXQ_OVFL` counters, logged on transitions
    /// only. Keyed on the receiving NIC's `iface_ip`, which the
    /// `AsyncUdpV4` `RecvMeta` carries on every datagram.
    prev_drops_per_iface: HashMap<Ipv4Addr, u32>,
}

/// How the search engine reaches servers.
///
/// These are two transports, not "a socket, optionally". Every UDP-only
/// piece of state lives inside the variant that owns the sockets, and every
/// UDP operation is a method on this type — so "a UDP socket is bound" and
/// "the arm that reads it is armed" are the same match rather than two facts
/// that can disagree. An `Option<AsyncUdpV4>` plus an `if let` at each of
/// the fanout sites is the patch; this is the fix
/// (`doc/calink-rtems-design.md` §4.3, mirroring the PVA client's
/// `SearchTransport` — `doc/pvalink-rtems-design.md` §8).
///
/// The configuration that needs the second variant is C's documented
/// TCP-only name resolution mode (`modules/ca/src/client/CAref.html:515-520`):
/// `EPICS_CA_NAME_SERVERS` set, `EPICS_CA_ADDR_LIST` empty and
/// `EPICS_CA_AUTO_ADDR_LIST=NO`. On `exec_backend` it is the only mode
/// available at all — see the module-head note on why the gate is the
/// backend and not the target.
enum SearchTransport {
    /// UDP SEARCH fanout and reply receive, alongside any TCP name servers.
    ///
    /// Compiled out on `exec_backend`, so there `NameServersOnly` is the
    /// *only* variant: "this build binds no UDP socket" is then a property of
    /// the type, which no later edit can reintroduce a branch around.
    #[cfg(tokio_backend)]
    Udp(Box<UdpTransport>),
    /// No UDP socket is bound at all. Every SEARCH goes out over the
    /// configured TCP name servers (`run_nameserver_connection`), replies
    /// arrive on those same circuits, and the UDP `select!` arm parks
    /// forever.
    NameServersOnly,
}

/// One line for a UDP-only address-list mutation that arrived at an engine
/// with no UDP socket.
///
/// `site` names the caller so the record says *which* operation was dropped.
/// Callers that legitimately do nothing without UDP and would repeat per tick
/// (the SEARCH fanout, the DNS refresh) match on the variant without logging.
fn log_dropped_udp_mutation(site: &'static str) {
    tracing::debug!(
        target: "epics_ca_rs::client::search",
        site,
        "ignoring UDP address-list change: this search engine binds \
         no UDP socket (EPICS_CA_NAME_SERVERS-only mode)"
    );
}

impl SearchTransport {
    /// Bind the per-NIC SEARCH socket bundle and take ownership of the UDP
    /// destination list, so that binding a socket and deciding where it
    /// transmits are one step.
    ///
    /// `None` when the bind fails — the caller's only sane response is to
    /// abandon the engine, which is what `run_search_engine` did before this
    /// type existed.
    #[cfg(tokio_backend)]
    fn bind_udp(addr_list: Vec<super::AddrEntry>) -> Option<Self> {
        // SO_REUSEADDR + (Linux) IP_MULTICAST_ALL=0 are applied to every
        // per-NIC socket inside `AsyncUdpV4::bind`.
        let socket = AsyncUdpV4::bind(0, true).ok()?;
        // Larger receive buffer absorbs multi-PV SEARCH response bursts.
        let _ = socket.set_recv_buffer_size(256 * 1024);
        // Apply `EPICS_CA_MCAST_TTL` (epics-base 3.16, f2a1834d). Affects
        // outgoing packets only when the destination falls in 224.0.0.0/4;
        // setting it unconditionally is safe and lets sites that
        // multicast SEARCH across routed segments raise the TTL via env.
        let _ = socket.set_multicast_ttl_v4(epics_base_rs::runtime::net::ca_mcast_ttl());
        // pvxs `client.cpp` parity (commit a064677e3625): opt every per-NIC
        // SEARCH socket into SO_RXQ_OVFL so a sustained reply backlog
        // (slow main-loop, undersized SO_RCVBUF, mass-disconnect storm)
        // surfaces as a debug log instead of silent reply loss. No-op on
        // non-Linux. Failure is logged at trace and ignored — the
        // counter is diagnostic-only.
        if let Err(e) = socket.enable_so_rxq_ovfl() {
            tracing::trace!(
                target: "epics_ca_rs::client::search",
                error = %e,
                "SO_RXQ_OVFL enable on per-NIC SEARCH bundle failed (non-fatal)"
            );
        }
        Some(Self::Udp(Box::new(UdpTransport {
            socket,
            addr_list,
            send_errors: HashMap::new(),
            prev_drops_per_iface: HashMap::new(),
        })))
    }

    /// Select C's TCP-only name resolution mode: bind **no** UDP socket and
    /// reach every server over `EPICS_CA_NAME_SERVERS` alone.
    ///
    /// Refused with an empty `nameserver_addrs`, because the result would be
    /// an engine with no way to reach anything: every SEARCH would be built,
    /// fanned to nobody, and retried forever. That is a configuration error
    /// and it fails here — before any task is spawned — rather than
    /// presenting as every channel timing out.
    fn name_servers_only(nameserver_addrs: &[SocketAddr]) -> epics_base_rs::error::CaResult<Self> {
        if nameserver_addrs.is_empty() {
            return Err(epics_base_rs::error::CaError::InvalidValue(
                "name-servers-only search engine requires a non-empty \
                 EPICS_CA_NAME_SERVERS: with no UDP socket and no name server \
                 the engine can reach no server at all"
                    .to_string(),
            ));
        }
        Ok(Self::NameServersOnly)
    }

    /// Append a unicast UDP SEARCH destination — libca
    /// `addAddrToChannelAccessAddressList` (`iocinf.cpp:45`).
    fn add_address(&mut self, addr: SocketAddr) {
        match self {
            #[cfg(tokio_backend)]
            Self::Udp(u) => {
                if !u.addr_list.iter().any(|e| e.sock == addr) {
                    let port = match addr {
                        SocketAddr::V4(a) => a.port(),
                        SocketAddr::V6(a) => a.port(),
                    };
                    u.addr_list.push(super::AddrEntry::new(addr, None, port));
                    tracing::info!(?addr, "ca-rs: addr_list += (programmatic)");
                }
            }
            Self::NameServersOnly => {
                let _ = addr;
                log_dropped_udp_mutation("AddAddress");
            }
        }
    }

    /// Drop a unicast UDP SEARCH destination (a discovery backend reporting
    /// `DiscoveryEvent::Removed`).
    #[cfg(feature = "client")]
    fn remove_address(&mut self, addr: SocketAddr) {
        match self {
            #[cfg(tokio_backend)]
            Self::Udp(u) => {
                let before = u.addr_list.len();
                u.addr_list.retain(|e| e.sock != addr);
                if u.addr_list.len() != before {
                    tracing::info!(?addr, "ca-rs: addr_list -= (discovery removal)");
                }
            }
            Self::NameServersOnly => {
                let _ = addr;
                log_dropped_udp_mutation("RemoveAddress");
            }
        }
    }

    /// Replace the whole UDP SEARCH destination list — libca
    /// `configureChannelAccessAddressList` (`iocinf.cpp:166`).
    fn set_address_list(&mut self, list: Vec<SocketAddr>) {
        match self {
            #[cfg(tokio_backend)]
            Self::Udp(u) => {
                tracing::info!(count = list.len(), "ca-rs: addr_list replaced");
                u.addr_list = list
                    .into_iter()
                    .map(|sock| {
                        let port = match sock {
                            SocketAddr::V4(a) => a.port(),
                            SocketAddr::V6(a) => a.port(),
                        };
                        super::AddrEntry::new(sock, None, port)
                    })
                    .collect();
            }
            Self::NameServersOnly => {
                let _ = list;
                log_dropped_udp_mutation("SetAddressList");
            }
        }
    }

    /// `select!` arm: the next SEARCH reply datagram, with its receiving-NIC
    /// metadata and that NIC's kernel drop counter.
    ///
    /// Parks forever without a UDP transport — the degradation shape the
    /// PVA client's optional beacon socket already used, and the reason the
    /// arm needs no `if` of its own.
    async fn recv(&self, buf: &mut [u8]) -> std::io::Result<(SearchDatagram, u32)> {
        match self {
            #[cfg(tokio_backend)]
            Self::Udp(u) => u
                .socket
                .recv_with_meta_with_drops(buf)
                .await
                .map(|(meta, drops)| {
                    (
                        SearchDatagram {
                            n: meta.n,
                            src: meta.src,
                            iface_ip: meta.iface_ip,
                        },
                        drops,
                    )
                }),
            Self::NameServersOnly => {
                let _ = buf;
                std::future::pending().await
            }
        }
    }

    /// Record this NIC's `SO_RXQ_OVFL` counter and log the transitions —
    /// pvxs `udp_collector.cpp:55-67` logs at debug on
    /// `prev != current && current != 0`.
    fn note_drops(&mut self, iface_ip: Ipv4Addr, drops: u32) {
        match self {
            #[cfg(tokio_backend)]
            Self::Udp(u) => {
                let prev = u.prev_drops_per_iface.insert(iface_ip, drops).unwrap_or(0);
                if drops != 0 && drops != prev {
                    tracing::debug!(
                        target: "epics_ca_rs::client::search",
                        %iface_ip,
                        prev,
                        drops,
                        "CA client SEARCH per-NIC socket buffer overflow"
                    );
                }
            }
            Self::NameServersOnly => {
                let _ = (iface_ip, drops);
            }
        }
    }

    /// Send one built SEARCH datagram to every UDP destination.
    ///
    /// Nothing is sent without a UDP transport: the destinations a datagram
    /// would go to live inside `UdpTransport` (`#[cfg(tokio_backend)]`, so not
    /// in scope on the exec-backend doc build), so there is no list to walk
    /// rather than a list that must be checked for emptiness.
    async fn fanout(&mut self, frame: &[u8], site: &'static str) {
        match self {
            #[cfg(tokio_backend)]
            Self::Udp(u) => {
                // Split-borrow so the per-destination error-suppression map can
                // be updated while the socket is borrowed for the send.
                let UdpTransport {
                    socket,
                    addr_list,
                    send_errors,
                    ..
                } = &mut **u;
                for entry in addr_list.iter() {
                    send_with_fanout(socket, frame, entry.sock, site, send_errors).await;
                }
            }
            Self::NameServersOnly => {
                let _ = (frame, site);
            }
        }
    }

    /// Re-resolve every `EPICS_CA_ADDR_LIST` entry that was configured as a
    /// hostname rather than an IP literal. No-op without a UDP transport —
    /// the entries being refreshed are UDP destinations.
    fn refresh_dns(&mut self) {
        match self {
            // `refresh_dns()` is a no-op for IP-literal entries; for DNS
            // entries it does a fresh `to_socket_addrs()` and replaces the
            // cached IP when it differs. Changes are logged at info so
            // operators can correlate an IOC migration with the client's
            // discovery of the new address.
            #[cfg(tokio_backend)]
            Self::Udp(u) => {
                for entry in u.addr_list.iter_mut() {
                    let prev_sock = entry.sock;
                    match entry.refresh_dns() {
                        Ok(new_sock) if new_sock != prev_sock => {
                            tracing::info!(
                                hostname = ?entry.hostname,
                                old = %prev_sock,
                                new = %new_sock,
                                "ca-rs: EPICS_CA_ADDR_LIST entry re-resolved"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!(
                                hostname = ?entry.hostname,
                                error = %e,
                                "ca-rs: DNS refresh failed; keeping cached IP"
                            );
                        }
                    }
                }
            }
            Self::NameServersOnly => {}
        }
    }

    /// Every UDP address this transport bound. Empty for
    /// [`Self::NameServersOnly`] because that variant holds no socket — the
    /// "bound no UDP socket" property is a fact about the type, and this is
    /// the runtime read of it.
    ///
    /// Test-only: nothing in the engine needs to know its own bound
    /// addresses, so this exists for the stage-C1 gate
    /// (`stage_c1_name_servers_only_resolves_without_binding_udp`) rather
    /// than being production surface that happens to be tested. Scoped to the
    /// configuration that actually runs that gate — it is per-test gated off
    /// under `rtems-exec-model`, so the method would be dead there.
    #[cfg(all(test, not(feature = "rtems-exec-model")))]
    fn bound_udp_addrs(&self) -> Vec<SocketAddr> {
        match self {
            Self::NameServersOnly => Vec::new(),
            #[cfg(tokio_backend)]
            Self::Udp(u) => u.socket.local_addrs(),
        }
    }

    /// How many UDP SEARCH destinations this transport holds. Zero for
    /// [`Self::NameServersOnly`] by construction.
    ///
    /// Separate from [`Self::addr_list`] because it is nameable on both
    /// backends: `AddrEntry` is part of the UDP surface and does not exist on
    /// `exec_backend`, so a test that asserts "this transport holds no UDP
    /// destinations" — which is exactly the assertion that must still run
    /// there — cannot go through a slice of it.
    #[cfg(test)]
    fn udp_dest_count(&self) -> usize {
        match self {
            Self::NameServersOnly => 0,
            #[cfg(tokio_backend)]
            Self::Udp(u) => u.addr_list.len(),
        }
    }

    /// The current UDP SEARCH destinations. Empty for
    /// [`Self::NameServersOnly`], which has none by construction.
    ///
    /// `tokio_backend` as well as `test`: `AddrEntry` is part of the UDP
    /// surface and does not exist on `exec_backend`. `feature = "client"`
    /// because its one caller — `add_then_remove_address_round_trip` — is the
    /// runtime address-list mutation path, which `client-core` does not have.
    #[cfg(all(test, tokio_backend, feature = "client"))]
    fn addr_list(&self) -> &[super::AddrEntry] {
        match self {
            Self::NameServersOnly => &[],
            #[cfg(tokio_backend)]
            Self::Udp(u) => &u.addr_list,
        }
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// The ordinary client engine: bind the UDP SEARCH bundle and additionally
/// use any `EPICS_CA_NAME_SERVERS` TCP circuits.
///
/// Host-only, because binding that bundle is: the RTEMS target selects
/// [`name_servers_only_search_engine`] instead (design §4.5).
#[cfg(tokio_backend)]
pub(crate) async fn run_search_engine(
    addr_list: Vec<super::AddrEntry>,
    nameserver_addrs: Vec<SocketAddr>,
    request_rx: mpsc::UnboundedReceiver<SearchRequest>,
    response_tx: mpsc::UnboundedSender<SearchResponse>,
    attempts: SearchAttempts,
) {
    let Some(transport) = SearchTransport::bind_udp(addr_list) else {
        return;
    };
    run_engine(
        transport,
        nameserver_addrs,
        request_rx,
        response_tx,
        attempts,
    )
    .await;
}

/// Build a search engine that binds **no UDP socket at all** and resolves
/// every PV over the `EPICS_CA_NAME_SERVERS` TCP circuits alone.
///
/// This is C's documented UDP-free mode
/// (`modules/ca/src/client/CAref.html:515-520`): "When used in combination
/// with an empty EPICS_CA_ADDR_LIST and EPICS_CA_AUTO_ADDR_LIST set to
/// \"NO\", Channel Access can be run without using UDP for name
/// resolution." libca reaches it by registering a `SearchDestTCP` per
/// `EPICS_CA_NAME_SERVERS` address (`cac.cpp:250-280`); it is what the RTEMS
/// target needs, because `AsyncUdpV4` does not exist there at all
/// (`doc/calink-rtems-design.md` §4.5).
///
/// Selection is an **explicit entry point**, not derived from the address
/// list being empty — deriving it would silently drop UDP search for every
/// host client that already runs with an empty `EPICS_CA_ADDR_LIST` and
/// `AUTO_ADDR_LIST=NO` but still expects a later `AddAddress` / discovery
/// event to work. Same reasoning as the PVA client's
/// `SearchEngine::spawn_name_servers_only` (`doc/pvalink-rtems-design.md`
/// §8.2).
///
/// The cost, stated plainly: no UDP SEARCH at all, so no reply can arrive
/// from a server that is not behind one of the configured name servers, and
/// runtime `AddAddress` / `SetAddressList` mutations have nowhere to go
/// (they are logged and dropped).
///
/// Returns the engine future rather than running it, so the empty-name-server
/// refusal is observed by the caller **before** anything is spawned.
///
/// On `exec_backend` this is the *only* engine: `CaClient` selects it there
/// (`client/mod.rs`), because `SearchTransport` has no UDP variant compiled in.
/// That is the RTEMS target and a host `--features rtems-exec-model` build
/// alike. On `tokio_backend` it stays capability-only — the client binds UDP —
/// so it is dead code there outside the gate test. The `expect` covers exactly
/// that, and is *not* applied where the call is live.
#[cfg_attr(
    all(tokio_backend, not(test)),
    expect(
        dead_code,
        reason = "\
    a client with a reactor binds UDP; this entry point is what the reactor-free \
    exec backend selects (doc/calink-rtems-design.md §4.5, §6 stage C5)"
    )
)]
pub(crate) fn name_servers_only_search_engine(
    nameserver_addrs: Vec<SocketAddr>,
    request_rx: mpsc::UnboundedReceiver<SearchRequest>,
    response_tx: mpsc::UnboundedSender<SearchResponse>,
    attempts: SearchAttempts,
) -> epics_base_rs::error::CaResult<impl std::future::Future<Output = ()> + Send + 'static> {
    let transport = SearchTransport::name_servers_only(&nameserver_addrs)?;
    Ok(run_engine(
        transport,
        nameserver_addrs,
        request_rx,
        response_tx,
        attempts,
    ))
}

async fn run_engine(
    mut transport: SearchTransport,
    nameserver_addrs: Vec<SocketAddr>,
    mut request_rx: mpsc::UnboundedReceiver<SearchRequest>,
    response_tx: mpsc::UnboundedSender<SearchResponse>,
    attempts: SearchAttempts,
) {
    // Spawn a connection task per EPICS_CA_NAME_SERVERS entry.
    // Each task auto-reconnects on C's EPICS_CA_CONN_TMO cadence and
    // forwards outgoing search bytes to its TCP socket. Incoming responses are
    // queued via tcp_response_tx for the main loop to process through
    // the shared handle_udp_response parser.
    let (tcp_response_tx, mut tcp_response_rx) = mpsc::unbounded_channel::<ParsedDatagram>();
    // Reproducer for Launchpad bug #739789: pre-fix, this was an
    // unbounded mpsc — when the nameserver TCP socket was unresponsive
    // the per-tick search frames piled up indefinitely (each frame
    // ~MAX_UDP_SEND bytes), eventually consuming process memory. Use
    // a bounded mpsc so a stuck TCP peer drops messages instead of
    // leaking. Cap is per-nameserver, not global. Override via
    // EPICS_CA_NAMESERVER_QUEUE_DEPTH; default 256 is large enough to
    // ride out a few-second TCP stall without observable search loss
    // and small enough to bound RSS at a few MB worst-case.
    let ns_queue_cap = epics_base_rs::runtime::env::get("EPICS_CA_NAMESERVER_QUEUE_DEPTH")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(256)
        .max(8);
    let mut nameserver_send_txs: Vec<mpsc::Sender<Vec<u8>>> = Vec::new();
    for addr in nameserver_addrs {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(ns_queue_cap);
        nameserver_send_txs.push(tx);
        let resp_tx = tcp_response_tx.clone();
        epics_base_rs::runtime::task::spawn(async move {
            run_nameserver_connection(addr, rx, resp_tx).await;
        });
    }

    let mut state = SearchEngineState::with_attempts(attempts);
    let mut recv_buf = [0u8; 65536];

    // pvxs `client.cpp::tickSearch`: a single steady tick advances the
    // bucket cursor. fast_tick is engaged after a beacon poke for one
    // full revolution, then we revert to the `normal_tick()` cadence.
    let mut tick = interval(normal_tick());
    tick.tick().await; // skip immediate fire
    let mut tick_is_fast = false;

    // Periodic DNS refresh for `EPICS_CA_ADDR_LIST`
    // entries whose `hostname` was set at startup (i.e. non-IP-literal
    // entries). On each tick the engine walks `addr_list` and calls
    // `AddrEntry::refresh_dns`; a changed resolution updates the
    // entry's `sock` so subsequent `fire_searches` use the new IP.
    // Period is operator-tunable via `EPICS_CA_DNS_REFRESH_SECS`;
    // default 60 s balances responsiveness against DNS load. Literal
    // IP entries (`hostname == None`) short-circuit inside
    // `refresh_dns` so the cost is bounded by hostname count.
    let dns_refresh_secs: u64 = epics_base_rs::runtime::env::get("EPICS_CA_DNS_REFRESH_SECS")
        .and_then(|s| s.parse().ok())
        .filter(|&n: &u64| n > 0)
        .unwrap_or(60);
    let mut dns_refresh = interval(Duration::from_secs(dns_refresh_secs));
    dns_refresh.tick().await; // skip immediate fire

    loop {
        tokio::select! {
            req = request_rx.recv() => {
                let Some(req) = req else { return };
                let mut immediate: Vec<u32> = Vec::new();
                if let Some(cid) = handle_request_or_addr(&mut state, &mut transport, req) {
                    immediate.push(cid);
                }
                // Drain any additional queued requests so a burst of
                // Schedule messages all land before the next tick.
                drain_pending_requests(&mut state, &mut transport, &mut request_rx, &mut immediate);
                // pvxs `clientdiscover.cpp` parity: send the first SEARCH
                // packet right now instead of waiting up to one tick for
                // the bucket to come around. The bucket placement still
                // governs all subsequent retries.
                if !immediate.is_empty() {
                    fire_searches(&mut state, &immediate, &mut transport, &nameserver_send_txs).await;
                }
            }

            result = transport.recv(&mut recv_buf) => {
                let Ok((meta, drops)) = result else { continue };
                // drain any queued `SearchRequest` before parsing
                // this datagram. A `Schedule{Reconnect}` enqueued by the
                // coordinator (mod.rs ServerDisconnect / TcpClosed paths)
                // invalidates the `resolved` multiply-defined tracker via
                // `remove_channel`. `tokio::select!` picks a ready arm at
                // random, so without this drain a SEARCH reply for a
                // legitimately-migrated PV could be parsed while the
                // stale `resolved` entry still names the old server,
                // emitting a false `ECA_DBLCHNL`. libca processes the
                // circuit teardown and the SEARCH reply on one thread
                // under one mutex (`cac.cpp:591-661`), so the disconnect
                // is always observed first; this drain restores that
                // ordering for the decoupled search-engine task.
                let mut immediate: Vec<u32> = Vec::new();
                drain_pending_requests(&mut state, &mut transport, &mut request_rx, &mut immediate);
                if !immediate.is_empty() {
                    fire_searches(&mut state, &immediate, &mut transport, &nameserver_send_txs).await;
                }
                // Surface per-NIC kernel drop transitions.
                transport.note_drops(meta.iface_ip, drops);
                handle_udp_response(&mut state, &recv_buf[..meta.n], meta.src, &response_tx);
            }

            tcp_dgram = tcp_response_rx.recv() => {
                let Some((bytes, src)) = tcp_dgram else { continue };
                // same ordering guarantee as the UDP arm — drain
                // queued `SearchRequest`s (notably `Schedule{Reconnect}`)
                // so a stale `resolved` entry cannot survive into the
                // multiply-defined check for this nameserver reply.
                let mut immediate: Vec<u32> = Vec::new();
                drain_pending_requests(&mut state, &mut transport, &mut request_rx, &mut immediate);
                if !immediate.is_empty() {
                    fire_searches(&mut state, &immediate, &mut transport, &nameserver_send_txs).await;
                }
                // TCP nameserver path uses the libca-equivalent
                // SEARCH-reply contract (no per-reply VERSION header).
                handle_tcp_response(&mut state, &bytes, src, &response_tx);
            }

            _ = tick.tick() => {
                process_bucket(&mut state, &mut transport, &nameserver_send_txs).await;
                if state.fast_ticks_remaining > 0 {
                    state.fast_ticks_remaining -= 1;
                }
            }

            _ = dns_refresh.tick() => {
                transport.refresh_dns();
            }
        }

        // Tick-cadence transitions are evaluated outside the select! arm so
        // every event path (Schedule, response, tick) gets the same chance
        // to flip the engine in/out of fast mode based on the current
        // `fast_ticks_remaining`.
        if state.fast_ticks_remaining > 0 && !tick_is_fast {
            tick = interval(FAST_TICK);
            tick.tick().await; // skip immediate fire
            tick_is_fast = true;
        } else if state.fast_ticks_remaining == 0 && tick_is_fast {
            tick = interval(normal_tick());
            tick.tick().await; // skip immediate fire
            tick_is_fast = false;
        }
    }
}

/// Long-lived task: maintain a TCP connection to one nameserver, forward
/// outgoing search bytes from `outgoing_rx`, and feed parsed response
/// frames into `response_tx`.
///
/// This is libca's name-service circuit (`tcpiiu::isNameService()`), and
/// C's retry rule for it is a fixed cadence, not a backoff:
/// `tcpRecvThread::connect` (`tcpiiu.cpp:606-661`) issues a *blocking*
/// `::connect()` — bounded by the OS, never by an application deadline —
/// and on failure sleeps `cacRef.connectionTimeout()` (EPICS_CA_CONN_TMO,
/// default 30 s) before trying the same address again, indefinitely. The
/// port's 5 s connect cap plus 1→30 s exponential backoff diverged in
/// both directions: it abandoned a slow-but-live name server C would have
/// reached, and it hammered a down one far harder than C does.
async fn run_nameserver_connection(
    addr: SocketAddr,
    mut outgoing_rx: mpsc::Receiver<Vec<u8>>,
    response_tx: mpsc::UnboundedSender<ParsedDatagram>,
) {
    loop {
        // The client's one dial (`transport::dial_ca`), which is also what
        // decides the transport: on a target with no reactor this circuit
        // comes up on the same two pump threads an upstream circuit does. The
        // receive-queue probe it returns belongs to libca's flow control,
        // which is a property of a *data* circuit — a name-service circuit
        // carries only SEARCH and its reply, so it is dropped here rather than
        // threaded through the inline reader below.
        let Some((stream, _bytes_pending_in_os)) = super::transport::dial_ca(addr).await else {
            // `dial_ca` already logged the OS error.
            tracing::debug!(
                target: "epics_ca_rs::client::search",
                nameserver = %addr,
                "EPICS_CA_NAME_SERVERS connect failed; retrying after EPICS_CA_CONN_TMO"
            );
            epics_base_rs::runtime::task::sleep(super::transport::connection_timeout()).await;
            continue;
        };

        match serve_nameserver_circuit(addr, stream, &mut outgoing_rx, &response_tx).await {
            // Outgoing channel closed → no more senders ever → don't
            // reconnect; exit the per-nameserver task.
            NameserverCircuitEnd::Shutdown => return,
            NameserverCircuitEnd::Retired => {}
        }

        // Both halves of the circuit were locals of the call above, so they are
        // already gone here — the descriptor is released at the instant the
        // circuit is retired, not one reconnect interval later.
        //
        // Same cadence as a failed connect, whichever half gave out: one knob
        // (CONN_TMO), so a nameserver that accepts and immediately drops — or
        // one the watchdog just retired for not answering CA_PROTO_ECHO —
        // cannot be hammered any harder than one that refuses outright.
        // Applied to the read-side exits too since the watchdog started
        // producing them; before that, only a failed write waited, and a peer
        // that closed on us was redialled at once.
        epics_base_rs::runtime::task::sleep(super::transport::connection_timeout()).await;
    }
}

/// Why a name-service circuit ended, and therefore whether to redial.
enum NameserverCircuitEnd {
    /// The outgoing channel closed: the client is shutting down and no sender
    /// can ever appear again.
    Shutdown,
    /// The circuit is gone — the watchdog retired it, a half failed, or the
    /// peer sent a frame this client will not parse. Redial after CONN_TMO.
    Retired,
}

/// Serve one dialled name-service circuit until it ends.
///
/// **This function is the circuit's lifetime.** Both halves and the
/// [`CircuitWatchdog`](super::transport::CircuitWatchdog) are locals of it, so
/// returning releases the descriptor — there is no reachable state in which the
/// watchdog has retired the circuit and the socket is still open. That is why
/// the reconnect backoff lives in the caller: written here it would hold the
/// send half across `EPICS_CA_CONN_TMO`, which on a hosted build (`into_split`,
/// no shutdown-on-reader-drop) keeps the connection ESTABLISHED for 30 s after
/// the peer was declared unresponsive, and on the blocking backend keeps the
/// descriptor allocated for the same 30 s after `ReaderPumpGuard` has already
/// shut the socket down.
///
/// The data circuit gets the same guarantee from a different owner:
/// `transport::spawn_guarded_pump`'s `CircuitDeathGuard` reports the first half
/// to exit, and the transport manager drops the whole `ServerConnection`, which
/// aborts the other half.
async fn serve_nameserver_circuit(
    addr: SocketAddr,
    stream: super::transport::CaCircuit,
    outgoing_rx: &mut mpsc::Receiver<Vec<u8>>,
    response_tx: &mpsc::UnboundedSender<ParsedDatagram>,
) -> NameserverCircuitEnd {
    // The watchdog arrives with the halves — see
    // `transport::CircuitWatchdog`. This circuit is retired on the same
    // `echo_idle_secs() + ECHO_TIMEOUT_SECS` bound a data circuit is,
    // because it is the same rule object, not a second copy of the rule.
    let (mut reader, mut writer, mut watchdog) = super::transport::split_circuit(stream);

    // Send initial VERSION + HOST_NAME + CLIENT_NAME so the nameserver
    // accepts our search frames (mirrors transport.rs handshake).
    // libca handshake order (`tcpiiu.cpp:755-762`):
    // VERSION → CLIENT_NAME → HOST_NAME. Mirror exactly.
    let mut handshake = Vec::new();
    let mut version = CaHeader::new(CA_PROTO_VERSION);
    version.count = CA_MINOR_VERSION;
    handshake.extend_from_slice(&version.to_bytes());
    let user = epics_base_rs::runtime::env::get("USER")
        .or_else(|| epics_base_rs::runtime::env::get("USERNAME"))
        .unwrap_or_else(|| "unknown".to_string());
    // extended-form headers when the USER / hostname
    // payload exceeds 16-bit postsize (libca's
    // `insertRequestHeader` parity). See the matching note in
    // `client/transport.rs` connect path.
    handshake.extend_from_slice(&super::transport::build_identity_frame(
        CA_PROTO_CLIENT_NAME,
        &user,
    ));
    handshake.extend_from_slice(&super::transport::build_identity_frame(
        CA_PROTO_HOST_NAME,
        &epics_base_rs::runtime::env::hostname(),
    ));
    if writer.write_all(&handshake).await.is_err() {
        return NameserverCircuitEnd::Retired;
    }

    let resp_tx = response_tx.clone();
    // The watchdog lives with the reader and the writer lives with the
    // pump, so the probe crosses between them. That is the shape
    // `read_loop` already has — it holds the watchdog and pushes the echo
    // bytes out through `write_tx` — and it is why the reader can own the
    // liveness rule without owning the socket's send half.
    let (echo_tx, mut echo_rx) = mpsc::unbounded_channel::<()>();
    let read_task = epics_base_rs::runtime::task::spawn(async move {
        let mut buf = vec![0u8; 8192];
        let mut accumulated: Vec<u8> = Vec::new();
        // The client's one receive-side body limit
        // (`transport::RecvBodyPolicy`), shared with the upstream
        // circuit's `read_loop`. C applies `processIncoming`'s
        // over-`EPICS_CA_MAX_ARRAY_BYTES` ignore-and-drain to a
        // name-service `tcpiiu` exactly as to a data one; pre-fix this
        // reader had no limit at all, so with
        // `EPICS_CA_AUTO_ARRAY_BYTES=NO` one 24-byte extended header
        // from a misbehaving name server could grow `accumulated`
        // toward the announced 4 GiB while the data circuit refused
        // the same frame.
        let mut body_policy = super::transport::RecvBodyPolicy::new();
        loop {
            watchdog.note_iteration();
            let n = tokio::select! {
                _ = epics_base_rs::runtime::task::sleep_until(watchdog.deadline()),
                    if watchdog.is_armed() =>
                {
                    match watchdog.expired() {
                        super::transport::WatchdogExpiry::SendEcho {
                            suspend_wake,
                            wall_skip,
                        } => {
                            if suspend_wake {
                                tracing::info!(
                                    nameserver = %addr,
                                    wall_skip_secs = wall_skip.as_secs(),
                                    "suspend wake detected; probing with shortened \
                                     echo timeout"
                                );
                            }
                            // The pump writes it; a closed channel means
                            // the pump is already gone.
                            if echo_tx.send(()).is_err() {
                                break;
                            }
                            continue;
                        }
                        super::transport::WatchdogExpiry::Unresponsive => {
                            // The probe went unanswered. C reaches the same
                            // verdict on a name-service `tcpiiu` — its
                            // `recvDog` is armed with no `isNameService()`
                            // branch — and this port's recovery for a
                            // name server is the redial its outer loop
                            // already owns, on the one CONN_TMO knob.
                            tracing::warn!(
                                nameserver = %addr,
                                bound_secs = super::transport::CircuitWatchdog::retire_bound()
                                    .as_secs(),
                                "EPICS_CA_NAME_SERVERS peer did not answer \
                                 CA_PROTO_ECHO; retiring the circuit"
                            );
                            break;
                        }
                    }
                }
                read_result = reader.read(&mut buf) => match read_result {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                },
            };
            // Any byte proves the peer is alive, an ECHO reply included.
            watchdog.data_arrived();
            accumulated.extend_from_slice(&buf[..n]);
            // Bytes owed to an already-refused oversize message are
            // consumed before framing resumes.
            if body_policy.drain_refused(&mut accumulated) {
                continue;
            }
            // Forward only the prefix that contains complete CA
            // messages. Without this framing, the kernel splitting a
            // server response across read syscalls causes the
            // dispatcher to miss leading frames (when the partial
            // buffer is < 16 bytes) and misalign subsequent parses.
            //
            // Where the message boundaries are is NOT decided here —
            // `transport::next_frame` is the client's one framing step,
            // shared with the upstream circuit's `read_loop`
            // (`doc/calink-rtems-design.md` §6 C2: "one seam, two
            // callers"). This loop only measures how long a prefix of
            // whole messages it can hand on.
            let mut consumed = 0usize;
            // Distinguishes "wait for more bytes" from "the bytes we
            // have are definitively malformed". Pre-fix every exit path
            // used the same `break`, so a parse error or a misaligned
            // `m_postsize` left the bad prefix sitting at the head of
            // `accumulated`; the next socket read appended fresh bytes
            // but the inner loop re-parsed the same bad prefix on every
            // iteration, wedging the circuit. C client
            // `tcpiiu.cpp::processIncoming:1197-1202` returns `false` on
            // a misaligned payload — the surrounding tcpiiu shuts the
            // connection. We mirror by exiting the outer read loop,
            // which drops the read_task and lets the reconnect path
            // rebuild.
            let mut bad_frame = None;
            loop {
                match super::transport::next_frame(&accumulated[consumed..]) {
                    super::transport::Frame::Incomplete => break,
                    super::transport::Frame::Malformed(e) => {
                        bad_frame = Some(e);
                        break;
                    }
                    super::transport::Frame::Header {
                        hdr_size, body_len, ..
                    } => {
                        let msg_size = hdr_size + body_len;
                        // Over-limit message: ignored, never fatal —
                        // the same `RecvBodyPolicy` rule as the data
                        // circuit. Ship the clean prefix first so the
                        // refused bytes never reach the dispatcher,
                        // then drop the message (across reads if its
                        // body is still arriving).
                        if body_policy.refuses(addr, body_len) {
                            if consumed > 0 {
                                let frame_bytes = accumulated[..consumed].to_vec();
                                let _ = resp_tx.send((frame_bytes, addr));
                                accumulated.drain(..consumed);
                                consumed = 0;
                            }
                            if msg_size <= accumulated.len() {
                                accumulated.drain(..msg_size);
                                continue;
                            }
                            body_policy.owe(msg_size - accumulated.len());
                            accumulated.clear();
                            break;
                        }
                        if accumulated.len() - consumed < msg_size {
                            break;
                        }
                        consumed += msg_size;
                    }
                }
            }
            if consumed > 0 {
                let frame_bytes = accumulated[..consumed].to_vec();
                let _ = resp_tx.send((frame_bytes, addr));
                accumulated.drain(..consumed);
            }
            if let Some(reason) = bad_frame {
                tracing::warn!(
                    addr = ?addr,
                    %reason,
                    "TCP nameserver framing error; closing circuit \
                     (C tcpiiu.cpp:1197-1202 parity)"
                );
                break;
            }
        }
    });

    // Pipe outgoing search frames to the TCP writer until the reader
    // task ends or the channel closes.
    // Closed outgoing channel = client shutdown. Track it so we
    // fall through to read_task cleanup, then exit the outer
    // reconnect loop. Earlier code `return`-ed directly which
    // skipped the cleanup and leaked the read task per
    // nameserver on every shutdown.
    let mut shutdown = false;
    'pump: loop {
        tokio::select! {
            msg = outgoing_rx.recv() => {
                let Some(bytes) = msg else {
                    shutdown = true;
                    break 'pump;
                };
                if writer.write_all(&bytes).await.is_err() {
                    break 'pump;
                }
            }
            probe = echo_rx.recv() => {
                // The reader's watchdog asked for a probe. Pre-fix this
                // arm was a hardcoded 60 s `sleep` whose echo nobody ever
                // looked for a reply to, which is why a name server that
                // accepted and went silent was held indefinitely.
                if probe.is_none() {
                    // Reader gone; the `read_task.is_finished()` check
                    // below ends the pump.
                    break 'pump;
                }
                let echo = CaHeader::new(CA_PROTO_ECHO);
                if writer.write_all(&echo.to_bytes()).await.is_err() {
                    break 'pump;
                }
            }
        }
        if read_task.is_finished() {
            break 'pump;
        }
    }
    read_task.abort();
    let _ = read_task.await;
    if shutdown {
        NameserverCircuitEnd::Shutdown
    } else {
        NameserverCircuitEnd::Retired
    }
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

/// Wrapper that handles the address-list mutation variants
/// inline (they need mutable access to `addr_list` which
/// `handle_request` doesn't have) and delegates everything else.
///
/// `addr_list` is `Vec<AddrEntry>` so the
/// engine carries the original hostname (if any) for DNS
/// re-resolution. Programmatic adds via `SearchRequest::AddAddress`
/// arrive as `SocketAddr` (no hostname context) and are wrapped
/// as `AddrEntry` with `hostname=None` — they're effectively IP
/// literals on the wire.
fn handle_request_or_addr(
    state: &mut SearchEngineState,
    transport: &mut SearchTransport,
    req: SearchRequest,
) -> Option<u32> {
    match req {
        SearchRequest::AddAddress(addr) => {
            transport.add_address(addr);
            None
        }
        #[cfg(feature = "client")]
        SearchRequest::RemoveAddress(addr) => {
            transport.remove_address(addr);
            None
        }
        SearchRequest::SetAddressList(list) => {
            transport.set_address_list(list);
            None
        }
        other => handle_request(state, other),
    }
}

/// drain every `SearchRequest` already queued on `request_rx`
/// into `state`, appending any cid that needs an immediate first-attempt
/// SEARCH to `immediate`.
///
/// Called both from the request-handling `select!` arm (so a burst of
/// `Schedule` messages all land before the next tick) and at the top of
/// the UDP / TCP response arms. The latter use: it
/// guarantees a `Schedule{Reconnect}` — which invalidates the
/// `resolved` multiply-defined tracker through `remove_channel` — is
/// applied before a SEARCH reply for the same cid is parsed, so a
/// legitimate server migration cannot surface as a false `ECA_DBLCHNL`.
/// This restores libca's single-threaded ordering (`cac.cpp:591-661`),
/// where circuit teardown and SEARCH-reply handling share one mutex.
fn drain_pending_requests(
    state: &mut SearchEngineState,
    transport: &mut SearchTransport,
    request_rx: &mut mpsc::UnboundedReceiver<SearchRequest>,
    immediate: &mut Vec<u32>,
) {
    while let Ok(req) = request_rx.try_recv() {
        if let Some(cid) = handle_request_or_addr(state, transport, req) {
            immediate.push(cid);
        }
    }
}

/// Process a search request. Returns `Some(cid)` when the new entry
/// needs an immediate first-attempt SEARCH packet sent (matches pvxs
/// `clientdiscover.cpp` immediate-broadcast on Find). The bucket
/// scheduler controls only retries; without immediate fire the first
/// attempt waits up to one full tick, which is the gap that made
/// ca-rs single-channel reconnect feel slower than pva-rs.
///
/// `None` means no immediate fire — either the request didn't add a
/// new pending entry (Cancel / ConnectResult) or it was a BeaconAnomaly
/// poke for an already-pending channel (counters reset only; fast-tick
/// mode handles the retransmit).
fn handle_request(state: &mut SearchEngineState, req: SearchRequest) -> Option<u32> {
    match req {
        SearchRequest::Schedule {
            cid,
            pv_name,
            reason,
        } => {
            // pvxs `poke()` semantic: BeaconAnomaly for an ALREADY-pending
            // channel must NOT move it to a new bucket. The whole point of
            // bucket distribution is lost if a mass-anomaly piles every
            // pending search into bucket=current+1. Just reset its retry
            // counters and engage fast-tick mode; the search fires within
            // ~6 s when its existing bucket comes around in fast cadence.
            if reason == SearchReason::BeaconAnomaly && state.pending.contains_key(&cid) {
                if let Some(p) = state.pending.get_mut(&cid) {
                    p.attempt = 0;
                    p.last_attempt = None;
                }
                state.fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                return None;
            }

            let search_payload = build_search_payload(cid, &pv_name);

            // Drop any stale entry before re-scheduling.
            state.remove_channel(cid);

            // Bucket placement (pvxs `Channel::disconnect` parity):
            // Initial / BeaconAnomaly land in `current+1` and pair
            // with an immediate broadcast or fast-tick retransmit;
            // Reconnect lands in `current_bucket` so the very next
            // 1-Hz tick fires it (≤ 1 s reconnect latency). The
            // earlier `(current+1+cid%30)` Reconnect formula gave
            // 1-30 s reconnect latency that combined with the
            // channel layer's wait-for-Found path made ca-rs
            // reconnect feel slower than pva-rs; the comment at
            // the top of `handle_request` flagged this gap. See
            // `placement_bucket` for the full rationale.
            let bucket = placement_bucket(state.current_bucket, reason);
            let p = PendingSearch {
                cid,
                pv_name,
                search_payload,
                bucket,
                attempt: 0,
                last_attempt: None,
            };
            state.buckets[bucket].push(cid);
            state.pending.insert(cid, p);

            if reason == SearchReason::BeaconAnomaly {
                state.poke();
            }

            // Immediate first-attempt SEARCH only on `Initial` (typical
            // single-channel `find()`). Skipping it for `Reconnect` is the
            // whole point of the cid-hashed bucket spread above — without
            // this gate a TCP-close affecting N channels would batch N
            // immediate sends from the main loop's `try_recv` drain
            // (`fire_searches` at the top of `run`), defeating the spread
            // and producing the very burst the bucket scheduler exists to
            // avoid. `BeaconAnomaly` for a NEW cid likewise relies on
            // fast-tick mode (`poke()` above) to retransmit within ~6 s
            // instead of firing right away.
            match reason {
                SearchReason::Initial => Some(cid),
                SearchReason::Reconnect | SearchReason::BeaconAnomaly => None,
            }
        }

        SearchRequest::Cancel { cid } => {
            state.remove_channel(cid);
            None
        }

        SearchRequest::ConnectResult {
            cid,
            success,
            server_addr,
        } => {
            if success {
                // take this cid out of the *search* state
                // (pending, buckets, attempts) but KEEP the
                // multiply-defined `resolved` entry so a late SEARCH
                // reply from a second IOC announcing the same PV
                // still triggers ECA_DBLCHNL. libca
                // `cac.cpp:621-641` runs the duplicate-detect for
                // the connected-channel lifetime, not just until
                // first CREATE_CHAN ack.
                if let Some(p) = state.pending.remove(&cid) {
                    state.buckets[p.bucket].retain(|x| *x != cid);
                }
                state.attempts.remove(&cid);
                state.mark_connected(cid);
                state.penalty.remove(&server_addr);
                state.breakers.record_success(server_addr);
            } else {
                state.penalty.insert(
                    server_addr,
                    PenaltyEntry {
                        until: Instant::now() + PENALTY_DURATION,
                    },
                );
                let was_open = state.breakers.is_open(server_addr);
                state.breakers.record_failure(server_addr);
                if !was_open && state.breakers.is_open(server_addr) {
                    tracing::warn!(server = %server_addr, "circuit breaker tripped OPEN");
                    metrics::counter!("ca_client_circuit_breaker_open_total",
                        "server" => server_addr.to_string())
                    .increment(1);
                }
            }
            None
        }
        // Address-list variants are intercepted by
        // `handle_request_or_addr` before they reach this match.
        // Defensive no-op so adding new variants doesn't crash if
        // future code paths plumb them straight to handle_request.
        SearchRequest::AddAddress(_) | SearchRequest::SetAddressList(_) => None,
        #[cfg(feature = "client")]
        SearchRequest::RemoveAddress(_) => None,
    }
}

// ---------------------------------------------------------------------------
// UDP response handling
// ---------------------------------------------------------------------------

fn handle_udp_response(
    state: &mut SearchEngineState,
    data: &[u8],
    src: SocketAddr,
    response_tx: &mpsc::UnboundedSender<SearchResponse>,
) {
    handle_search_response(state, data, src, response_tx);
}

/// C `libca/tcpiiu.cpp::searchRespNotify` accepts TCP search replies
/// directly — TCP search replies from
/// `rsrv/camessage.c::search_reply_tcp` carry no per-reply VERSION
/// header. Since `handle_search_response` now resolves every reply
/// unconditionally (matching C `searchRespAction`), the TCP and UDP
/// paths are identical; this wrapper remains as the named TCP entry
/// point for the nameserver read loop.
fn handle_tcp_response(
    state: &mut SearchEngineState,
    data: &[u8],
    src: SocketAddr,
    response_tx: &mpsc::UnboundedSender<SearchResponse>,
) {
    handle_search_response(state, data, src, response_tx);
}

fn handle_search_response(
    state: &mut SearchEngineState,
    data: &[u8],
    src: SocketAddr,
    response_tx: &mpsc::UnboundedSender<SearchResponse>,
) {
    if data.len() < CaHeader::SIZE {
        return;
    }

    // C `udpiiu.cpp::searchRespAction` transfers the channel to its
    // virtual circuit on EVERY SEARCH reply, and `cac.cpp:651` /
    // `searchTimer.cpp:323` uninstall the channel from the search list
    // unconditionally — the per-datagram sequence number
    // (`lastReceivedSeqNo`, recorded by `versionAction`) gates only
    // libca's RTT estimate and immediate-resend optimisation, never
    // whether the channel is found. We follow that: a reply resolves
    // its cid whenever the cid is still `pending` (the natural
    // resolve-once guard), regardless of any VERSION seq marker in the
    // datagram, so a legacy or third-party reply that arrives without a
    // leading VERSION — which libca still accepts — is no longer
    // dropped. The rolling `dgram_seq` is still sent (see
    // `fire_searches`) for wire-parity; Rust just does not consume the
    // echo, as its retry-ring does not model libca's `searchTimer`.

    let recv_time = Instant::now();
    let mut offset = 0;

    while offset + CaHeader::SIZE <= data.len() {
        let Ok(hdr) = CaHeader::from_bytes(&data[offset..]) else {
            break;
        };

        // C `rsrv/camessage.c:2452` rejects misaligned `m_postsize`.
        // For UDP (where this loop runs), C silently drops the
        // datagram without emitting an error — we do the same by
        // breaking out of the chained-message parse. Without this
        // guard, the `align8(postsize)` advancement would walk into
        // the middle of the next message and stale parses would
        // poison search/beacon state.
        if (hdr.postsize as usize) & 0x7 != 0 {
            break;
        }

        match hdr.cmmd {
            // A per-datagram CA_PROTO_VERSION carries libca's echoed
            // sequence number, which in C gates only the RTT estimate
            // and the immediate-resend timer optimisation — never
            // channel resolution (`searchRespAction` resolves without
            // consulting it). The Rust retry-ring does not model that
            // timer, so we ignore the VERSION reply; the offset advance
            // at the bottom of the loop skips its (empty) body.
            CA_PROTO_VERSION => {}
            CA_PROTO_SEARCH => {
                let server_port = hdr.data_type;
                // CA v4.8+: cid contains server IP. Both 0 (INADDR_ANY)
                // and 0xFFFFFFFF (~0u32, libca's "address unknown" sentinel
                // — see udpiiu.cpp searchRespAction) mean "use UDP source
                // address". Without handling both, real C softIoc replies
                // (cid=~0u32) get rerouted to 255.255.255.255 and the
                // search appears to fail.
                let server_ip = if hdr.cid == 0 || hdr.cid == u32::MAX {
                    src.ip()
                } else {
                    std::net::IpAddr::V4(Ipv4Addr::from(hdr.cid.to_be_bytes()))
                };
                metrics::counter!("ca_client_search_responses_total").increment(1);
                let server_addr = SocketAddr::new(server_ip, server_port as u16);
                let cid = hdr.available;

                // EPICS_RS_CLIENT_IGNORE: drop SEARCH replies
                // announcing a quarantined server so a beacon-
                // discovered server can't sneak past the
                // EPICS_CA_ADDR_LIST filter. Both the announced
                // server IP and the source IP are checked — most
                // upstream IOCs announce ~0 ("use UDP src"), but a
                // misconfigured server announcing its own IP must
                // also be filtered. Rust-only extension; NOT the C
                // EPICS_IOC_IGNORE_SERVERS — see
                // client::epics_rs_client_ignore docstring.
                if let std::net::IpAddr::V4(v4) = server_ip {
                    if state.ignored_servers.contains(&v4) {
                        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                        continue;
                    }
                }
                if let std::net::IpAddr::V4(v4) = src.ip() {
                    if state.ignored_servers.contains(&v4) {
                        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                        continue;
                    }
                }

                // multiply-defined-PV detection runs
                // BEFORE the penalty / breaker gates. libca
                // `cac.cpp:591-661` runs this check on
                // every SEARCH reply for a known cid with no per-
                // server filtering and no seq-number gating between.
                // Pre-fix Rust put the duplicate-detect after those
                // gates, so a flaky/penalized duplicate server's
                // reply was silently discarded — exactly when the
                // diagnostic is most operationally valuable. Emit
                // does not consume any reply state, so it is safe to
                // fire even on stale/penalized datagrams. Note:
                // resolved entries live past `ConnectResult{success}`
                // for the channel's connected lifetime.
                if let Some((pv_name, prev_addr)) = state.resolved.get(&cid) {
                    if *prev_addr != server_addr {
                        let pv_name = pv_name.clone();
                        let prev_addr = *prev_addr;
                        tracing::warn!(
                            target: "epics_ca_rs::client::search",
                            pv = %pv_name,
                            cid,
                            connected_to = %prev_addr,
                            but_also_on = %server_addr,
                            "Channel multiply defined: PV is also hosted on a second server"
                        );
                        metrics::counter!("ca_client_multiply_defined_pv_total").increment(1);
                        // dispatch ECA_DBLCHNL via the
                        // exception-handler path so library users
                        // who registered a `set_exception_handler`
                        // (the documented analog of libca
                        // `ca_add_exception_event`) see this
                        // condition. The coordinator translates the
                        // SearchResponse into a CaException of kind
                        // ServerError with status=ECA_DBLCHNL.
                        let _ = response_tx.send(SearchResponse::MultiplyDefined {
                            pv_name,
                            prev_addr,
                            new_addr: server_addr,
                        });
                    }
                }

                // Check penalty box — skip penalized servers so the channel
                // can potentially find a non-penalized one.
                let penalized = state
                    .penalty
                    .get(&server_addr)
                    .map(|p| p.until > recv_time)
                    .unwrap_or(false);

                // Circuit breaker hard-blocked → reject responses from this
                // server entirely. This is a READ-ONLY check: `is_blocking()`
                // does not perform the OPEN→HALF_OPEN transition or consume
                // the single HALF_OPEN probe slot.
                //
                // `is_blocking()` (not `is_open()`) is deliberate: it returns
                // false once an OPEN breaker's cooldown has elapsed, so a
                // probe-ready breaker falls through to the `allow()` call
                // below. `is_open()` here would reject probe-ready breakers
                // too — and since `allow()` is the only code that leaves
                // OPEN, the breaker would be stranded OPEN forever.
                //
                // Probe-slot consumption is still deferred until we confirm
                // a real connect will follow (the cid is in `state.pending`);
                // a passive SEARCH reply for an unknown cid must not burn the
                // probe slot, which would strand the breaker in HALF_OPEN for
                // up to `probe_timeout` (30s) with no connect to resolve it.
                if penalized || state.breakers.is_blocking(server_addr) {
                    // Don't consume this response — let the channel keep
                    // searching for a better server.
                    offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                    continue;
                }

                if let Some(p) = state.pending.get(&cid) {
                    // A connect normally follows this Found — consume the
                    // breaker probe slot here. `allow()` performs the
                    // OPEN→HALF_OPEN transition (a probe-ready breaker
                    // passed the `is_blocking()` gate above) and returns
                    // false when a probe is already in flight; in that case
                    // leave the cid pending so a later round can retry.
                    // Caveat: if the downstream `Found` handler drops this
                    // event (e.g. the channel already advanced to
                    // Connecting via another server), the probe slot is
                    // consumed without a paired record_success/_failure —
                    // `allow()`'s `probe_timeout` self-heal admits a fresh
                    // probe after 30s, so the breaker is delayed, not
                    // stranded.
                    if !state.breakers.allow(server_addr) {
                        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                        continue;
                    }
                    let bucket = p.bucket;
                    let pv_name = p.pv_name.clone();
                    state.pending.remove(&cid);
                    state.buckets[bucket].retain(|x| *x != cid);
                    tracing::debug!(
                        pv = %pv_name, cid, server = %server_addr,
                        "PV search resolved"
                    );
                    // record the resolved server so a second
                    // SEARCH reply for the same cid (from a different
                    // IOC) can be diagnosed as multiply-defined.
                    if state.resolved.len() >= MULTIPLY_DEFINED_RESOLVED_CAP {
                        if let Some(&victim) = state.resolved.keys().next() {
                            state.resolved.remove(&victim);
                        }
                    }
                    state.resolved.insert(cid, (pv_name, server_addr));
                    let _ = response_tx.send(SearchResponse::Found { cid, server_addr });
                }
                // Duplicate-detect was here; moved above the
                // penalty / breaker gates.
            }
            CA_PROTO_NOT_FOUND => {
                // Server explicitly told us the PV is not on it. We don't
                // remove the channel — another server in the addr list may
                // still answer Found.
            }
            _ => {}
        }

        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
    }
}

// ---------------------------------------------------------------------------
// Per-tick bucket processing
// ---------------------------------------------------------------------------

/// Process exactly one search bucket. Each pending in this bucket
/// gets a UDP retransmit and is then re-armed into a future bucket
/// using pvxs's `nSearch+1` escalation (`tickSearch` line 1193-1196):
///
/// ```text
/// next = (idx + min(attempt, nBuckets)) % nBuckets
/// ```
///
/// `attempt` is bumped immediately after the send so the first
/// retry lands at idx+1 (1 s later), the second at idx+2 (2 s
/// after that), the third at idx+3 (4 s total), …, capping at
/// idx+30 (one full ring = 30 s steady-state). The earlier
/// `holdoff_cycles=10` design conflated pvxs's pre-CREATE_CHANNEL
/// holdoff with the Active-disconnect retry path; pvxs only uses
/// the 10-bucket holdoff for `Channel::Connecting` drops, never
/// for the steady reconnect cadence.
///
/// Cascade smoothing: when the chosen `next` bucket is overloaded
/// vs `next+1` by 100+ entries, defer to `next+1` (mirrors pvxs
/// `client.cpp:1199-1206`). Lets a mass-disconnect spread across
/// two ticks instead of one.
///
/// Steady-state UDP search load = O(1) datagrams per tick regardless
/// of how many channels are pending — the bucket distributes load
/// across the ring. The previous lane-based scheduler had every channel
/// fire on its own deadline and relied on AIMD to dampen storms after
/// the fact; the bucket scheduler prevents storms by construction.
async fn process_bucket(
    state: &mut SearchEngineState,
    transport: &mut SearchTransport,
    nameserver_txs: &[mpsc::Sender<Vec<u8>>],
) {
    let now = Instant::now();

    // Expire old penalties.
    state.penalty.retain(|_, entry| entry.until > now);

    let current = state.current_bucket;
    let bucket_ids = std::mem::take(&mut state.buckets[current]);

    let mut to_send: Vec<u32> = Vec::new();
    {
        // Split-borrow `pending` and `buckets` so cascade_smoothed_next
        // (which only reads bucket sizes via a closure capture of
        // `&buckets`) can run inline with the per-sid push back to
        // `&mut buckets[next]`. Without the split, the closure's
        // immutable borrow of `state.buckets` would conflict with
        // the subsequent mutable access — which is why the prior
        // version had to batch the rearm into a Vec and apply it
        // post-loop. That batching defeated the within-tick
        // smoothing benefit: a 5000-channel mass-disconnect saw
        // delta=0 for every sid (all 5000 saw an empty `next`
        // bucket because nothing was pushed yet) and piled into
        // `current+1`. With inline push the second sid sees the
        // first's buildup, the third sees two, etc., so smoothing
        // kicks in around the 100-entry boundary just like pvxs's
        // tickSearch line 1199-1206. PVA-rs uses the equivalent
        // pattern where `pending` and `search_buckets` are
        // top-level locals; here we recover the same effect via
        // explicit split-borrow.
        let pending = &mut state.pending;
        let buckets = &mut state.buckets;
        for sid in bucket_ids {
            let Some(p) = pending.get_mut(&sid) else {
                continue;
            };
            p.last_attempt = Some(now);
            p.attempt = p.attempt.saturating_add(1);
            let attempt = p.attempt;
            // Diagnostic counter (CaChannel::search_attempts) is bumped
            // by fire_searches when the SEARCH actually goes on the
            // wire — covers both this bucket-tick path AND the
            // immediate-fire path right after Schedule (which never
            // reaches process_bucket).
            to_send.push(sid);

            let bucket_sizes = |idx: usize| buckets[idx].len();
            let next = cascade_smoothed_next(current, attempt, bucket_sizes);
            // Closure dropped at `cascade_smoothed_next` return —
            // immutable borrow on `buckets` is gone, so the
            // mutable accesses below compile.
            if let Some(p) = pending.get_mut(&sid) {
                p.bucket = next;
            }
            buckets[next].push(sid);
        }
    }

    state.current_bucket = (state.current_bucket + 1) % N_SEARCH_BUCKETS;

    if to_send.is_empty() {
        return;
    }

    fire_searches(state, &to_send, transport, nameserver_txs).await;
}

/// Build batched UDP SEARCH datagrams for `cids` and send via every
/// destination + nameserver channel. One VERSION header per datagram
/// carries the rolling sequence number with the `sequenceNoIsValid`
/// marker (matches C EPICS `dgSeqNo`); libca's server echoes it to
/// feed libca's `searchTimer` RTT/stale-round scoring. The Rust client
/// sends it for wire-parity but resolves replies unconditionally (see
/// `handle_search_response`). Used both by the per-tick bucket
/// processor and by the immediate-fire path that runs right after
/// handle_request to avoid the up-to-1-tick wait on the first attempt.
async fn fire_searches(
    state: &mut SearchEngineState,
    cids: &[u32],
    transport: &mut SearchTransport,
    nameserver_txs: &[mpsc::Sender<Vec<u8>>],
) {
    state.dgram_seq = state.dgram_seq.wrapping_add(1);
    let version_hdr = {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        // C `caProto.h:128` defines `sequenceNoIsValid = 1`: this
        // marker in the per-datagram VERSION header's `m_dataType`
        // tells the server its `m_cid` carries a valid seqno that
        // must be echoed in the reply VERSION (C `cas_send_dg_msg`,
        // `caserverio.c:194-197`). Pre-fix Rust sent `0x8000`, which
        // libca never recognises — the server then never echoed the
        // seqno. We send the correct marker for wire-parity; the
        // echoed value feeds only libca's own RTT timer, which the
        // Rust client does not consume (it resolves replies
        // unconditionally, matching C `searchRespAction`).
        h.data_type = 1;
        h.cid = state.dgram_seq;
        h.to_bytes()
    };

    // Build batched UDP datagrams (multi-search per packet, MTU-bounded).
    // Bucket distribution caps per-tick load at ~pending/N_SEARCH_BUCKETS,
    // so no AIMD throttling is needed.
    let mut current_frame = Vec::with_capacity(MAX_UDP_SEND);
    current_frame.extend_from_slice(&version_hdr);

    for sid in cids {
        let Some(p) = state.pending.get(sid) else {
            continue;
        };
        let payload = p.search_payload.clone();
        // CA-035 diagnostic counter: bump per-cid each time we
        // commit to fanning a SEARCH out. Single fire_searches call
        // == one logical attempt for the cid regardless of how many
        // UDP datagrams the addr_list / nameserver fanout produces
        // (matches libca ca_search_attempts(chid) "attempt" semantic).
        // Use fetch_add so beacon poke (which resets p.attempt to 0)
        // does NOT make this counter regress.
        state
            .attempts
            .entry(*sid)
            .or_insert_with(|| AtomicU32::new(0))
            .fetch_add(1, Ordering::Relaxed);

        if current_frame.len() + payload.len() > MAX_UDP_SEND
            && current_frame.len() > CaHeader::SIZE
        {
            transport.fanout(&current_frame, "bucket").await;
            for ns_tx in nameserver_txs {
                ns_try_send(ns_tx, current_frame.clone());
            }
            current_frame.clear();
            current_frame.extend_from_slice(&version_hdr);
        }

        if CaHeader::SIZE + payload.len() > MAX_UDP_SEND {
            // Single payload exceeds MTU — solo send.
            let mut solo = Vec::with_capacity(CaHeader::SIZE + payload.len());
            solo.extend_from_slice(&version_hdr);
            solo.extend_from_slice(&payload);
            transport.fanout(&solo, "solo").await;
            for ns_tx in nameserver_txs {
                ns_try_send(ns_tx, solo.clone());
            }
        } else {
            current_frame.extend_from_slice(&payload);
        }
    }

    // Flush the final frame.
    if current_frame.len() > CaHeader::SIZE {
        transport.fanout(&current_frame, "flush").await;
        for ns_tx in nameserver_txs {
            ns_try_send(ns_tx, current_frame.clone());
        }
    }
}

/// Drop-on-full helper for nameserver TCP send queues. Mirrors libca
/// behavior under TCP stall: bounded queue, drop excess, log + bump
/// the metric so operators can see queue pressure. Lp #739789.
fn ns_try_send(ns_tx: &mpsc::Sender<Vec<u8>>, frame: Vec<u8>) {
    use tokio::sync::mpsc::error::TrySendError;
    match ns_tx.try_send(frame) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            metrics::counter!("ca_client_nameserver_queue_drops_total").increment(1);
            tracing::warn!(
                "EPICS_CA_NAME_SERVERS queue full — dropping search frame \
                 (peer is slow/unresponsive; raise EPICS_CA_NAMESERVER_QUEUE_DEPTH \
                 if the peer is healthy)"
            );
        }
        Err(TrySendError::Closed(_)) => {
            // Receiver task exited — nothing more we can do here.
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build per-channel search payload (SEARCH header + padded PV name).
/// Does NOT include the VERSION header — that is prepended once per datagram.
fn build_search_payload(cid: u32, pv_name: &str) -> Vec<u8> {
    let pv_payload = pad_string(pv_name);

    let mut search_hdr = CaHeader::new(CA_PROTO_SEARCH);
    search_hdr.postsize = pv_payload.len() as u16;
    // C `libca/udpiiu.cpp::searchMsg()` sets
    // `m_dataType = DONTREPLY`. The TCP search path on the server
    // only sends CA_PROTO_NOT_FOUND when `DOREPLY` is set, and
    // libca's TCP response table treats CA_PROTO_NOT_FOUND as a
    // bad TCP response. Pre-fix Rust used `CA_DO_REPLY` for every
    // search, eliciting negative replies that libca never asks
    // for and that the Rust parser then ignores.
    search_hdr.data_type = CA_DONT_REPLY;
    search_hdr.count = CA_MINOR_VERSION;
    search_hdr.cid = cid;
    search_hdr.available = cid;

    let mut payload = Vec::with_capacity(CaHeader::SIZE + pv_payload.len());
    payload.extend_from_slice(&search_hdr.to_bytes());
    payload.extend_from_slice(&pv_payload);
    payload
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// C `getNTimers` (`udpiiu.cpp:96-111`) caps the search-timer ladder at 18
    /// rungs, so the effective ceiling on `EPICS_CA_MAX_SEARCH_PERIOD` is
    /// `(1 << 17) * 32e-3 == 4194.304 s` and the boundary sits at
    /// `(1 << 18) * 32e-3 == 8388.608 s`. Every row here was run against the
    /// compiled `caget`: 8388.607 is silent, 8388.608 prints the "(high)" pair.
    #[test]
    fn search_timer_ladder_bounds_the_max_search_period() {
        assert_eq!(max_search_period_upper_limit_secs(), 4194.304);

        // Inside the ladder: no clamp, no diagnostic.
        for period in [60.0, 300.0, 4194.304, 8388.607] {
            assert!(
                search_timer_count(period) <= MAX_SEARCH_TIMER_COUNT,
                "{period} s fits C's 18-rung ladder"
            );
        }
        // Past it: C clamps and says so.
        for period in [8388.608, 8389.0, 100_000.0, f64::INFINITY] {
            assert!(
                search_timer_count(period) > MAX_SEARCH_TIMER_COUNT,
                "{period} s is past C's 18-rung ladder"
            );
        }
    }

    fn schedule_initial(state: &mut SearchEngineState, cid: u32, pv_name: &str) {
        handle_request(
            state,
            SearchRequest::Schedule {
                cid,
                pv_name: pv_name.to_string(),
                reason: SearchReason::Initial,
            },
        );
    }

    /// `EPICS_CA_MAX_SEARCH_PERIOD` must follow the C
    /// `udpiiu.cpp::getMaxPeriod` semantics — default 300 s when
    /// unset, lower-limited at 60 s when explicitly set below it,
    /// default kept on a non-numeric value.
    ///
    /// Pre-fix Rust defaulted to 30 s when unset (not the documented
    /// C 300 s) and accepted any positive value verbatim, so a
    /// configured `45` was honoured as 45 s instead of being clamped
    /// up to C's 60 s lower bound. `normal_tick` is the consumer:
    /// `tick = period / N_SEARCH_BUCKETS`.
    #[test]
    #[serial_test::serial]
    fn ex_r2_max_search_period_matches_c_default_and_lower_bound() {
        // SAFETY: serial_test::serial guarantees no concurrent env
        // access; mutations are confined to this test.
        let restore = std::env::var("EPICS_CA_MAX_SEARCH_PERIOD").ok();

        // Unset → documented C default of 300 s (NOT the pre-fix
        // historical Rust 30 s). tick = 300/30 = 10 s.
        unsafe { std::env::remove_var("EPICS_CA_MAX_SEARCH_PERIOD") };
        assert_eq!(
            resolve_max_search_period_secs(),
            300.0,
            "unset env must default to C's 300 s, not the old 30 s"
        );
        assert_eq!(
            normal_tick_for(resolve_max_search_period_secs()),
            Duration::from_secs(10)
        );

        // Configured value below the 60 s lower limit → clamped up
        // to 60 s (C `maxPeriod < maxSearchPeriodLowerLimit`).
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "45") };
        assert_eq!(
            resolve_max_search_period_secs(),
            60.0,
            "a configured 45 s must clamp UP to C's 60 s lower bound"
        );
        assert_eq!(
            normal_tick_for(resolve_max_search_period_secs()),
            Duration::from_secs(2)
        );

        // Configured value at/above the lower limit → honoured.
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "120") };
        assert_eq!(resolve_max_search_period_secs(), 120.0);
        assert_eq!(
            normal_tick_for(resolve_max_search_period_secs()),
            Duration::from_secs(4)
        );

        // The documented C default expressed explicitly.
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "300") };
        assert_eq!(resolve_max_search_period_secs(), 300.0);

        // Non-numeric value → C keeps the default (longStatus != 0).
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "not-a-number") };
        assert_eq!(
            resolve_max_search_period_secs(),
            300.0,
            "a non-numeric value must fall back to the 300 s default"
        );

        // Negative / zero are not real-number rejections in C — they
        // parse and are caught by the lower-bound clamp.
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "-5") };
        assert_eq!(
            resolve_max_search_period_secs(),
            60.0,
            "a negative value must clamp to the 60 s lower bound, not default"
        );
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "0") };
        assert_eq!(resolve_max_search_period_secs(), 60.0);

        // Restore the environment for any later serial test.
        match restore {
            Some(v) => unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", v) },
            None => unsafe { std::env::remove_var("EPICS_CA_MAX_SEARCH_PERIOD") },
        }
    }

    /// Reproducer for Launchpad bug #739789 (TCP nameserver send queue
    /// memory leak): a stuck/slow TCP peer caused libca's `sendQue` to
    /// grow unbounded as the UDP search agent kept pushing frames.
    /// In epics-rs the nameserver-send channel is now bounded via
    /// `EPICS_CA_NAMESERVER_QUEUE_DEPTH` (default 256), and
    /// `ns_try_send` drops the frame instead of blocking or queuing.
    /// This test exercises the helper directly: with a 2-slot channel
    /// and no consumer, the third send must drop.
    #[epics_macros_rs::epics_test]
    async fn nameserver_queue_drops_when_full_no_leak() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(2);
        ns_try_send(&tx, vec![1, 2, 3]);
        ns_try_send(&tx, vec![4, 5, 6]);
        // Capacity is exhausted — third call must drop, not block.
        ns_try_send(&tx, vec![7, 8, 9]);
        // Drain: only the first two frames are present. The third was
        // dropped, not queued — that is the regression guard.
        assert_eq!(rx.try_recv().unwrap(), vec![1, 2, 3]);
        assert_eq!(rx.try_recv().unwrap(), vec![4, 5, 6]);
        assert!(
            rx.try_recv().is_err(),
            "third frame must be dropped, not queued (lp #739789)"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn nameserver_queue_handles_closed_receiver() {
        // Receiver dropped — ns_try_send must not panic.
        let (tx, rx) = mpsc::channel::<Vec<u8>>(2);
        drop(rx);
        ns_try_send(&tx, vec![1, 2, 3]);
        // Reaching this line means the call did not panic.
    }

    #[test]
    fn build_search_payload_size() {
        let payload = build_search_payload(42, "TEST:PV");
        // CaHeader::SIZE (16) + pad_string("TEST:PV") = 16 + 8 = 24
        assert_eq!(payload.len(), 24);
    }

    #[test]
    fn build_search_payload_alignment() {
        let payload = build_search_payload(1, "A");
        // pad_string("A") = 8 bytes (1 char + null + 6 padding)
        assert_eq!(payload.len(), CaHeader::SIZE + 8);
        assert_eq!(payload.len() % 8, 0);
    }

    #[test]
    fn schedule_places_into_next_bucket() {
        let mut state = SearchEngineState::new();
        state.current_bucket = 5;
        schedule_initial(&mut state, 1, "PV:1");
        let p = state.pending.get(&1).unwrap();
        assert_eq!(p.bucket, 6);
        assert_eq!(state.buckets[6], vec![1]);
        assert_eq!(state.buckets[5], Vec::<u32>::new());
    }

    #[test]
    fn cancel_removes_from_bucket() {
        let mut state = SearchEngineState::new();
        schedule_initial(&mut state, 1, "PV:1");
        let bucket = state.pending.get(&1).unwrap().bucket;
        handle_request(&mut state, SearchRequest::Cancel { cid: 1 });
        assert!(state.pending.is_empty());
        assert!(state.buckets[bucket].is_empty());
    }

    /// `SearchRequest::RemoveAddress` must drop an entry that
    /// `AddAddress` previously appended — this is the path a discovery
    /// backend's `DiscoveryEvent::Removed` feeds. A removal for an
    /// address not in the list is a silent no-op.
    ///
    /// Async because the destination list now lives inside `UdpTransport`,
    /// whose construction binds the per-NIC socket bundle — the point of the
    /// sum type is that there is no way to hold UDP destinations without the
    /// socket that would transmit to them.
    #[cfg(all(feature = "client", tokio_backend))]
    #[epics_macros_rs::epics_test]
    async fn add_then_remove_address_round_trip() {
        let mut state = SearchEngineState::new();
        let mut transport = SearchTransport::bind_udp(Vec::new()).expect("bind UDP transport");
        let a: SocketAddr = "10.0.0.7:5064".parse().unwrap();
        let b: SocketAddr = "10.0.0.8:5064".parse().unwrap();

        handle_request_or_addr(&mut state, &mut transport, SearchRequest::AddAddress(a));
        handle_request_or_addr(&mut state, &mut transport, SearchRequest::AddAddress(b));
        assert_eq!(transport.addr_list().len(), 2);

        handle_request_or_addr(&mut state, &mut transport, SearchRequest::RemoveAddress(a));
        assert_eq!(transport.addr_list().len(), 1);
        assert!(transport.addr_list().iter().all(|e| e.sock == b));

        // Removing an address not present is a no-op, not a panic.
        handle_request_or_addr(&mut state, &mut transport, SearchRequest::RemoveAddress(a));
        assert_eq!(transport.addr_list().len(), 1);
    }

    /// The same three requests on a [`SearchTransport::NameServersOnly`]
    /// engine have nowhere to go: an `EPICS_CA_ADDR_LIST` entry is a UDP
    /// SEARCH destination, and that variant binds no UDP socket. They must
    /// be dropped (with a debug line) rather than accumulating in a list
    /// nothing will ever transmit to.
    #[test]
    fn name_servers_only_drops_address_list_mutations() {
        let mut state = SearchEngineState::new();
        let ns: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let mut transport = SearchTransport::name_servers_only(&[ns]).expect("non-empty NS list");
        let a: SocketAddr = "10.0.0.7:5064".parse().unwrap();

        handle_request_or_addr(&mut state, &mut transport, SearchRequest::AddAddress(a));
        handle_request_or_addr(
            &mut state,
            &mut transport,
            SearchRequest::SetAddressList(vec![a]),
        );
        #[cfg(feature = "client")]
        handle_request_or_addr(&mut state, &mut transport, SearchRequest::RemoveAddress(a));
        assert_eq!(
            transport.udp_dest_count(),
            0,
            "NameServersOnly must hold no UDP SEARCH destinations"
        );
        assert!(
            matches!(transport, SearchTransport::NameServersOnly),
            "an address-list mutation must not promote the transport to Udp"
        );
    }

    /// A name-servers-only engine with an empty `EPICS_CA_NAME_SERVERS` can
    /// reach nothing at all — no UDP socket and no TCP circuit. Refuse it at
    /// construction rather than spawning a task that resolves nothing
    /// forever (`doc/calink-rtems-design.md` §6 stage C1).
    #[test]
    fn name_servers_only_refuses_empty_name_server_list() {
        let err = SearchTransport::name_servers_only(&[])
            .err()
            .expect("empty name-server list must be refused");
        assert!(
            err.to_string().contains("EPICS_CA_NAME_SERVERS"),
            "the refusal must name the parameter the operator has to set; got {err}"
        );
    }

    #[test]
    fn poke_resets_attempts_and_engages_fast_mode() {
        let mut state = SearchEngineState::new();
        schedule_initial(&mut state, 1, "PV:1");
        // Simulate one prior attempt.
        if let Some(p) = state.pending.get_mut(&1) {
            p.attempt = 3;
        }
        state.poke();
        let p = state.pending.get(&1).unwrap();
        assert_eq!(p.attempt, 0, "poke must reset per-channel retry counter");
        assert_eq!(state.fast_ticks_remaining, N_SEARCH_BUCKETS as u32);
    }

    #[test]
    fn beacon_anomaly_for_pending_channel_keeps_bucket() {
        // pvxs poke() semantic: a BeaconAnomaly Schedule for an
        // already-pending channel must NOT move it to a new bucket.
        // Otherwise a mass-anomaly piles every pending search into
        // bucket=current+1 and defeats bucket distribution.
        let mut state = SearchEngineState::new();
        // Use Reconnect so it's placed into a non-current+1 bucket.
        handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 7,
                pv_name: "PV:7".into(),
                reason: SearchReason::Reconnect,
            },
        );
        let original_bucket = state.pending.get(&7).unwrap().bucket;
        // Pretend prior attempts happened.
        if let Some(p) = state.pending.get_mut(&7) {
            p.attempt = 4;
        }
        // Now apply a BeaconAnomaly poke for cid=7.
        handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 7,
                pv_name: "PV:7".into(),
                reason: SearchReason::BeaconAnomaly,
            },
        );
        let p = state.pending.get(&7).unwrap();
        assert_eq!(p.bucket, original_bucket, "poke must not relocate bucket");
        assert_eq!(p.attempt, 0);
        assert_eq!(state.fast_ticks_remaining, N_SEARCH_BUCKETS as u32);
        // And the bucket vector still has the cid exactly once.
        let count = state.buckets[original_bucket]
            .iter()
            .filter(|x| **x == 7)
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn beacon_anomaly_schedule_pokes_engine() {
        let mut state = SearchEngineState::new();
        schedule_initial(&mut state, 1, "PV:1");
        // Pretend channel #1 had multiple prior failures.
        if let Some(p) = state.pending.get_mut(&1) {
            p.attempt = 2;
        }
        handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 2,
                pv_name: "PV:2".into(),
                reason: SearchReason::BeaconAnomaly,
            },
        );
        // Both channels should now be at attempt=0 and the engine in fast mode.
        assert_eq!(state.pending.get(&1).unwrap().attempt, 0);
        assert_eq!(state.pending.get(&2).unwrap().attempt, 0);
        assert_eq!(state.fast_ticks_remaining, N_SEARCH_BUCKETS as u32);
    }

    #[test]
    fn connect_success_clears_pending_and_penalty() {
        let mut state = SearchEngineState::new();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        schedule_initial(&mut state, 1, "PV:1");
        state.penalty.insert(
            server,
            PenaltyEntry {
                until: Instant::now() + Duration::from_secs(60),
            },
        );
        handle_request(
            &mut state,
            SearchRequest::ConnectResult {
                cid: 1,
                success: true,
                server_addr: server,
            },
        );
        assert!(state.pending.is_empty());
        assert!(!state.penalty.contains_key(&server));
    }

    #[test]
    fn connect_failure_inserts_penalty() {
        let mut state = SearchEngineState::new();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        schedule_initial(&mut state, 1, "PV:1");
        handle_request(
            &mut state,
            SearchRequest::ConnectResult {
                cid: 1,
                success: false,
                server_addr: server,
            },
        );
        // Pending entry stays — channel still searching for another server.
        assert!(state.pending.contains_key(&1));
        assert!(state.penalty.contains_key(&server));
    }

    #[test]
    fn n_search_buckets_is_30() {
        // Sanity: pvxs uses 30, our bucket vector must match.
        let state = SearchEngineState::new();
        assert_eq!(state.buckets.len(), N_SEARCH_BUCKETS);
        assert_eq!(N_SEARCH_BUCKETS, 30);
    }

    #[test]
    fn fast_tick_revolution_covers_full_ring() {
        // FAST_TICK * N_SEARCH_BUCKETS should be ~6 s (matches pvxs poke cadence).
        let revolution = FAST_TICK * N_SEARCH_BUCKETS as u32;
        assert!(revolution >= Duration::from_secs(5));
        assert!(revolution <= Duration::from_secs(7));
    }

    /// `Initial` is the only reason that earns the immediate-fire
    /// `Some(cid)` return — `Reconnect` and `BeaconAnomaly` must
    /// return `None` so the main loop's `try_recv` drain doesn't
    /// batch a 5000-channel disconnect cascade into a single-tick
    /// burst (review finding HIGH#1).
    #[test]
    fn reconnect_and_beacon_anomaly_skip_immediate_fire() {
        let mut state = SearchEngineState::new();
        // Initial → Some(cid)
        let cid_initial = handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 100,
                pv_name: "PV:Initial".into(),
                reason: SearchReason::Initial,
            },
        );
        assert_eq!(
            cid_initial,
            Some(100),
            "Initial must return Some for immediate fire"
        );
        // Reconnect → None (bucket-spread, no burst)
        let cid_reconnect = handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 101,
                pv_name: "PV:Reconnect".into(),
                reason: SearchReason::Reconnect,
            },
        );
        assert_eq!(cid_reconnect, None, "Reconnect must NOT immediately fire");
        // BeaconAnomaly (NEW cid) → None (fast-tick handles retransmit)
        let cid_anomaly = handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 102,
                pv_name: "PV:Anomaly".into(),
                reason: SearchReason::BeaconAnomaly,
            },
        );
        assert_eq!(
            cid_anomaly, None,
            "BeaconAnomaly NEW must NOT immediately fire"
        );
    }

    /// pvxs `Channel::disconnect` parity: `Reconnect` schedules
    /// must land in `current_bucket` (zero holdoff for the typical
    /// Active disconnect — `client.cpp:213`). Cascade-spread on
    /// first reconnect is achieved by the natural one-bucket-per-
    /// tick rate-limit, not by per-cid hashing. The earlier
    /// `(current+1+cid%30)` formula gave 1-30 s reconnect latency
    /// that the channel layer's wait-for-Found path couldn't hide.
    #[test]
    fn placement_reconnect_uses_current_bucket() {
        for current in 0..N_SEARCH_BUCKETS {
            assert_eq!(
                placement_bucket(current, SearchReason::Reconnect),
                current,
                "Reconnect must drop in current bucket (got {current})"
            );
        }
    }

    /// `Initial` and `BeaconAnomaly` both pair with an immediate
    /// broadcast / fast-tick retransmit, so their bucket placement
    /// is one tick ahead — that's where the FIRST scheduled
    /// retransmit (after the immediate fire) lands. Wrap-around at
    /// the ring boundary is part of the contract.
    #[test]
    fn placement_initial_and_beacon_anomaly_one_bucket_ahead() {
        for reason in [SearchReason::Initial, SearchReason::BeaconAnomaly] {
            assert_eq!(placement_bucket(0, reason), 1);
            assert_eq!(placement_bucket(13, reason), 14);
            assert_eq!(
                placement_bucket(N_SEARCH_BUCKETS - 1, reason),
                0,
                "wrap-around at ring boundary"
            );
        }
    }

    /// pvxs `tickSearch` line 1193-1196 escalates the retry bucket
    /// by `nSearch+1` after each transmit. Pattern: 1, 2, 3, ...,
    /// capping at `N_SEARCH_BUCKETS` (where the cap means "full
    /// ring", which lands back on the same bucket → 30 s
    /// steady-state retry cadence).
    #[test]
    fn cascade_next_implements_pvxs_nsearch_escalation() {
        let no_imbalance = |_| 0usize;
        let current = 7;

        assert_eq!(
            cascade_smoothed_next(current, 1, no_imbalance),
            (current + 1) % N_SEARCH_BUCKETS,
        );
        assert_eq!(
            cascade_smoothed_next(current, 2, no_imbalance),
            (current + 2) % N_SEARCH_BUCKETS,
        );
        assert_eq!(
            cascade_smoothed_next(current, 10, no_imbalance),
            (current + 10) % N_SEARCH_BUCKETS,
        );
        assert_eq!(
            cascade_smoothed_next(current, N_SEARCH_BUCKETS as u32, no_imbalance),
            current,
            "attempt at cap wraps to current (full-ring steady state)",
        );
        assert_eq!(
            cascade_smoothed_next(current, 1_000_000, no_imbalance),
            current,
            "attempt > cap stays clamped",
        );
    }

    /// pvxs `client.cpp:1199-1206` smoothing: when the chosen
    /// `next` bucket is overloaded versus `next+1` by 100+ entries,
    /// defer to `next+1`. Crosses two ticks instead of one.
    #[test]
    fn cascade_smoothing_defers_when_next_is_overloaded() {
        let current = 5;
        let attempt = 1; // → next=6, nextnext=7

        let overloaded = |idx: usize| if idx == 6 { 200 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, overloaded),
            7,
            "delta > 100 must defer"
        );

        let below = |idx: usize| if idx == 6 { 90 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, below),
            6,
            "delta < 100 stays in next"
        );

        let balanced = |idx: usize| if idx == 6 || idx == 7 { 200 } else { 0 };
        assert_eq!(cascade_smoothed_next(current, attempt, balanced), 6);

        let reverse = |idx: usize| if idx == 7 { 200 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, reverse),
            6,
            "smoothing only defers forward, never backward"
        );
    }

    /// Smoothing boundary cases — pvxs's threshold is strictly
    /// `delta > 100`. Catches the easy-to-introduce off-by-one
    /// (`>= 100`).
    #[test]
    fn cascade_smoothing_boundary_at_delta_100() {
        let current = 5;
        let attempt = 1;
        let exactly_100 = |idx: usize| if idx == 6 { 100 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, exactly_100),
            6,
            "delta == 100 must NOT trigger"
        );
        let just_over_100 = |idx: usize| if idx == 6 { 101 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, just_over_100),
            7,
            "delta == 101 must trigger"
        );
    }

    /// Issue #372 mass-channel scenario, single-tick view: simulate
    /// the rearm half of one `process_bucket` call against a
    /// 5000-channel reconnect storm and verify the inline-push
    /// `cascade_smoothed_next` placement at least bisects the load
    /// instead of piling every channel into a single bucket.
    ///
    /// pvxs's smoothing rule (`client.cpp:1199-1206`) defers ONLY by
    /// one bucket (`next` → `nextnext`) when the chosen bucket
    /// exceeds `nextnext + 100`, so within one tick a flat-attempt
    /// reconnect storm can land in at most two buckets. The
    /// follow-on test
    /// `mass_5000_multi_tick_distribution_covers_full_ring`
    /// pins the ring-wide spread that emerges across multiple ticks.
    #[test]
    fn mass_5000_reconnect_spreads_at_least_two_buckets() {
        const N_CHANNELS: usize = 5000;
        let current = 0;
        let attempt = 1; // Reconnect → first retry uses attempt=1

        let mut buckets = vec![0usize; N_SEARCH_BUCKETS];
        for _sid in 0..N_CHANNELS {
            let bucket_sizes = |idx: usize| buckets[idx];
            let next = cascade_smoothed_next(current, attempt, bucket_sizes);
            buckets[next] += 1;
        }

        let total: usize = buckets.iter().sum();
        assert_eq!(
            total, N_CHANNELS,
            "every channel must be placed exactly once"
        );

        let nonempty = buckets.iter().filter(|&&n| n > 0).count();
        assert!(
            nonempty >= 2,
            "smoothing must split the load across ≥2 buckets; got {} non-empty: {buckets:?}",
            nonempty
        );

        // No single bucket may carry more than 60% of the total —
        // a regressed smoothing threshold would let bucket 1 take
        // all 5000 entries.
        let max_load = *buckets.iter().max().unwrap();
        let cap = (N_CHANNELS * 60) / 100;
        assert!(
            max_load <= cap,
            "no single bucket may carry > {cap} entries (60% of {N_CHANNELS}); \
             got max {max_load} in {buckets:?}"
        );
    }

    /// Issue #372 multi-tick scenario: simulate `process_bucket`
    /// running for `2 * N_SEARCH_BUCKETS` ticks against an initial
    /// bulk reconnect of 5000 channels, advancing `current_bucket`
    /// each tick and rearming sids via the inline-push smoothing.
    /// Verify that across the full ring rotation the load distributes
    /// across the majority of buckets and no bucket dominates more
    /// than a fraction of the total — proving the per-tick send rate
    /// stays bounded under sustained mass-channel load.
    #[test]
    fn mass_5000_multi_tick_distribution_covers_full_ring() {
        const N_CHANNELS: usize = 5000;
        const TICKS: usize = 2 * N_SEARCH_BUCKETS;

        // Initial state: all sids placed in bucket 0 with attempt=0
        // (mirrors a fresh Reconnect storm at process_bucket entry).
        let mut buckets: Vec<Vec<u32>> = (0..N_SEARCH_BUCKETS).map(|_| Vec::new()).collect();
        buckets[0] = (0..N_CHANNELS as u32).collect();
        let mut attempts = vec![0u32; N_CHANNELS];

        // Track maximum bucket load observed at the moment of
        // processing — that is the per-tick send rate ceiling.
        let mut max_per_tick = 0usize;
        let mut buckets_visited = [false; N_SEARCH_BUCKETS];

        let mut current = 0;
        for _ in 0..TICKS {
            buckets_visited[current] = true;
            let processing = std::mem::take(&mut buckets[current]);
            max_per_tick = max_per_tick.max(processing.len());

            // Rearm each sid via inline-push smoothing.
            for sid in processing {
                attempts[sid as usize] = attempts[sid as usize].saturating_add(1);
                let attempt = attempts[sid as usize];
                let bucket_sizes = |idx: usize| buckets[idx].len();
                let next = cascade_smoothed_next(current, attempt, bucket_sizes);
                buckets[next].push(sid);
            }

            current = (current + 1) % N_SEARCH_BUCKETS;
        }

        // Across one full ring + extra slack, every bucket should
        // have been visited as `current` rotates.
        let visited_count = buckets_visited.iter().filter(|&&v| v).count();
        assert_eq!(
            visited_count, N_SEARCH_BUCKETS,
            "current_bucket must rotate through every slot in {TICKS} ticks; got {visited_count}"
        );

        // The first tick processes the entire 5000-bulk; subsequent
        // ticks see the smoothed redistribution. Cap is the initial
        // bulk size — anything over that means the smoothing
        // accumulated load *back* into a single bucket faster than
        // the ring could drain it (regression).
        assert!(
            max_per_tick <= N_CHANNELS,
            "per-tick processing load must not exceed initial burst {N_CHANNELS}; got {max_per_tick}"
        );

        // Conservation: every sid still accounted for somewhere.
        let still_pending: usize = buckets.iter().map(|b| b.len()).sum();
        assert_eq!(
            still_pending, N_CHANNELS,
            "sids must not be lost across {TICKS} ticks; got {still_pending} pending of {N_CHANNELS}"
        );
    }

    /// End-to-end Reconnect bucket-fire test. Boots `run_search_engine`
    /// with a sniffer socket as the only addr_list destination,
    /// submits a `Schedule { Reconnect }`, and asserts that a
    /// SEARCH packet for the right cid lands on the sniffer within
    /// one tick after Schedule arrival, mirroring pvxs
    /// `Channel::disconnect` recovery timing. Without the
    /// pvxs-parity placement the search would have been placed in a
    /// cid-hashed bucket a full ring away and never fired within a
    /// reasonable window.
    ///
    /// the production tick cadence is now `normal_tick()` =
    /// `EPICS_CA_MAX_SEARCH_PERIOD / N_SEARCH_BUCKETS`. The test
    /// pins the env var to C's 60 s lower limit so the tick is the
    /// fastest the C-faithful clamp allows — 2 s — and asserts
    /// against that, not the earlier 1 s tick.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn reconnect_search_broadcasts_within_one_tick() {
        use std::net::Ipv4Addr;

        // pin the search period to C's 60 s lower bound so
        // the tick is the minimum the clamp allows (60/30 = 2 s).
        // SAFETY: serial_test::serial guarantees no concurrent env
        // access; the var is restored before the test returns.
        let restore = std::env::var("EPICS_CA_MAX_SEARCH_PERIOD").ok();
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "60") };

        // Sniffer on loopback ephemeral. Used as the engine's
        // ONLY addr_list destination.
        let sniffer = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("bind sniffer");
        let sniffer_addr = sniffer
            .local_addrs()
            .first()
            .copied()
            .expect("sniffer local_addr");

        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, _resp_rx) = mpsc::unbounded_channel();
        let engine_handle = tokio::spawn(run_search_engine(
            vec![crate::client::AddrEntry::new(
                sniffer_addr,
                None,
                sniffer_addr.port(),
            )],
            Vec::new(),
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        ));

        // Schedule a Reconnect for cid=42. Engine places it in
        // current_bucket; the next tick fires the broadcast.
        let cid = 42u32;
        let pv = "TEST:CA:RECONNECT:PV";
        let started = std::time::Instant::now();
        req_tx
            .send(SearchRequest::Schedule {
                cid,
                pv_name: pv.into(),
                reason: SearchReason::Reconnect,
            })
            .expect("schedule send");

        let mut buf = vec![0u8; 4096];
        let recv_result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (n, _from) = sniffer.recv_from(&mut buf).await?;
                if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                    return Ok::<usize, std::io::Error>(n);
                }
            }
        })
        .await;

        let elapsed = started.elapsed();
        engine_handle.abort();

        // Restore the environment for any later serial test.
        match restore {
            Some(v) => unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", v) },
            None => unsafe { std::env::remove_var("EPICS_CA_MAX_SEARCH_PERIOD") },
        }

        let n = recv_result
            .expect("Reconnect SEARCH must arrive within 5 s")
            .expect("recv_from must not error");
        assert!(
            n > 0,
            "received an empty datagram — Reconnect SEARCH path is broken"
        );
        // Reconnect lands in current_bucket → fires on the next
        // tick (2 s at the pinned 60 s period). 4 s gives ~2 s slack
        // for scheduler / mio jitter on loaded CI; the regression
        // this guards against (cid-hashed full-ring latency) would
        // delay the fire by up to a whole ring revolution.
        assert!(
            elapsed < Duration::from_millis(4000),
            "Reconnect should broadcast within one tick (~2 s at the \
             pinned 60 s period); took {elapsed:?} — bucket placement \
             / tick handler may have regressed"
        );
    }

    /// End-to-end retry escalation timing test. Verifies that the
    /// production process_bucket loop reproduces pvxs's `nSearch+1`
    /// pattern at the actual scheduler level — unit tests of
    /// `cascade_smoothed_next` cover the formula in isolation, but
    /// only this test catches an accumulator drift between the
    /// pure fn and the live `current_bucket`-advancing tick loop.
    ///
    /// with `EPICS_CA_MAX_SEARCH_PERIOD` pinned to C's 60 s
    /// lower bound the tick is 60/30 = 2 s. Expected SEARCH arrival
    /// times (relative to Schedule submission):
    ///   #1 at ~2 s   (first tick after Schedule lands)
    ///   #2 at ~4 s   (idx+1, +1 cycle = 2 s)
    ///   #3 at ~8 s   (idx+(1+2)=idx+3, +2 cycles = 4 s)
    ///
    /// Slack: ±1 s per gap to absorb scheduler / mio jitter on
    /// loaded CI. Total runtime ~8 s.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn retry_escalation_pvxs_pattern() {
        use std::net::Ipv4Addr;

        // pin the search period to C's 60 s lower bound →
        // 2 s tick. SAFETY: serial_test::serial guarantees no
        // concurrent env access; restored before return.
        let restore = std::env::var("EPICS_CA_MAX_SEARCH_PERIOD").ok();
        unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "60") };

        let sniffer = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("bind sniffer");
        let sniffer_addr = sniffer
            .local_addrs()
            .first()
            .copied()
            .expect("sniffer addr");

        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, _resp_rx) = mpsc::unbounded_channel();
        let engine_handle = tokio::spawn(run_search_engine(
            vec![crate::client::AddrEntry::new(
                sniffer_addr,
                None,
                sniffer_addr.port(),
            )],
            Vec::new(),
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        ));

        let cid = 77u32;
        let pv = "ESCALATION:CA";
        let started = std::time::Instant::now();
        req_tx
            .send(SearchRequest::Schedule {
                cid,
                pv_name: pv.into(),
                reason: SearchReason::Reconnect,
            })
            .expect("schedule");

        let mut buf = vec![0u8; 4096];
        let mut packet_times = Vec::new();
        for i in 0..3 {
            let t = tokio::time::timeout(Duration::from_secs(12), async {
                loop {
                    let (n, _) = sniffer.recv_from(&mut buf).await.expect("recv");
                    if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                        return started.elapsed();
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("SEARCH #{} did not arrive within 12 s", i + 1));
            packet_times.push(t);
        }

        engine_handle.abort();

        // Restore the environment for any later serial test.
        match restore {
            Some(v) => unsafe { std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", v) },
            None => unsafe { std::env::remove_var("EPICS_CA_MAX_SEARCH_PERIOD") },
        }

        assert!(
            packet_times[0] < Duration::from_millis(3000),
            "first SEARCH should arrive ~2 s after Schedule (one tick \
             at the pinned 60 s period); got {:?}",
            packet_times[0]
        );
        let gap_12 = packet_times[1].saturating_sub(packet_times[0]);
        let gap_23 = packet_times[2].saturating_sub(packet_times[1]);
        assert!(
            (1500..=3000).contains(&(gap_12.as_millis() as u64)),
            "gap #1→#2 should be ~2 s (nSearch=1, one 2 s cycle); \
             got {gap_12:?}. Production retry escalation may have regressed."
        );
        assert!(
            (3000..=5400).contains(&(gap_23.as_millis() as u64)),
            "gap #2→#3 should be ~4 s (nSearch=2, two 2 s cycles); \
             got {gap_23:?}. Production retry escalation may have regressed."
        );
    }

    /// `doc/calink-rtems-design.md` §6 stage C1 gate: the client must resolve a
    /// PV with `EPICS_CA_ADDR_LIST` empty and `EPICS_CA_AUTO_ADDR_LIST=NO`,
    /// reaching the server **only** through `EPICS_CA_NAME_SERVERS`, having
    /// bound **no UDP socket at all**. That is C's documented TCP-only name
    /// resolution mode (`modules/ca/src/client/CAref.html:515-520`, §4.1) and
    /// we had no test for it.
    ///
    /// The "no UDP socket" half is asserted twice, deliberately:
    ///
    /// * **structurally** — [`SearchTransport::NameServersOnly`] is a variant
    ///   with no fields, so there is no socket for the engine to hold and no
    ///   `select!` arm that could read one. That is the property the sum type
    ///   exists to provide; an `Option<AsyncUdpV4>` would leave "bound" and
    ///   "armed" free to disagree (§4.3).
    /// * **at runtime** — [`SearchTransport::bound_udp_addrs`] reads back every
    ///   address the transport actually bound, and it must be empty. A future
    ///   change that gives `NameServersOnly` a socket fails here even if it
    ///   still type-checks.
    ///
    /// The runtime half is guarded against being a tautology: the same
    /// environment is first pushed through the real
    /// `parse_addr_list_with_hostnames` and then through
    /// `SearchTransport::bind_udp`, which **does** bind — so an empty list
    /// below is a property of the variant, not of the test's configuration.
    ///
    /// The resolution half is what proves the degradation is not merely inert:
    /// a SEARCH still goes out (over TCP) and the reply still resolves the cid.
    ///
    /// Gated off under `rtems-exec-model`: the resolution half dials the TCP
    /// name server through `run_nameserver_connection`, which is spawned on
    /// the exec backend's callback band (`runtime::task::spawn`) — a band
    /// worker with no tokio I/O reactor, so `TcpStream::connect` panics there.
    /// Making that path drive real sockets under the exec backend is Stage C2
    /// (the blocking byte source) / C3 (the spawn seam), not this stage
    /// (`doc/calink-rtems-design.md` §6). The structural half of the claim —
    /// that `NameServersOnly` can hold no UDP socket — is a property of the
    /// type and is additionally covered by
    /// `name_servers_only_drops_address_list_mutations`, which runs in both
    /// configurations.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn stage_c1_name_servers_only_resolves_without_binding_udp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // C's documented UDP-free configuration. SAFETY: serial_test::serial
        // guarantees no concurrent env access; both vars are restored below.
        let restore_list = std::env::var("EPICS_CA_ADDR_LIST").ok();
        let restore_auto = std::env::var("EPICS_CA_AUTO_ADDR_LIST").ok();
        unsafe {
            std::env::set_var("EPICS_CA_ADDR_LIST", "");
            std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        }

        let addr_list =
            super::super::parse_addr_list_with_hostnames().expect("parse EPICS_CA_ADDR_LIST");
        assert!(
            addr_list.is_empty(),
            "precondition: an empty EPICS_CA_ADDR_LIST with AUTO_ADDR_LIST=NO \
             must yield no UDP SEARCH destination; got {addr_list:?}"
        );

        // Control, and the reason the assertion below is not a tautology: the
        // UDP transport built from this very configuration DOES bind sockets.
        let udp = SearchTransport::bind_udp(addr_list).expect("bind_udp must succeed on the host");
        assert!(
            !udp.bound_udp_addrs().is_empty(),
            "control: the UDP transport must bind at least one per-NIC SEARCH \
             socket, otherwise the NameServersOnly assertion proves nothing"
        );
        drop(udp);

        let ns_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock name-server listener bind");
        let ns_addr = ns_listener.local_addr().expect("mock name-server addr");

        assert!(
            SearchTransport::name_servers_only(&[ns_addr])
                .expect("non-empty name-server list")
                .bound_udp_addrs()
                .is_empty(),
            "NameServersOnly must bind no UDP socket"
        );

        // The server the mock name server will name in its SEARCH reply. A
        // literal address (not the reply's source) so the assertion can tell
        // "resolved through the name server" from "defaulted to the peer".
        let server_addr: SocketAddr = "10.0.0.9:5064".parse().unwrap();
        let cid = 4242u32;
        let pv = "TEST:CA:NSONLY:PV";

        let ns_handle = tokio::spawn(async move {
            let (mut stream, _peer) = ns_listener.accept().await.expect("mock NS: accept");
            let mut buf = vec![0u8; 8192];
            let mut seen: Vec<u8> = Vec::new();
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                seen.extend_from_slice(&buf[..n]);
                // The client sends VERSION + CLIENT_NAME + HOST_NAME first,
                // then the SEARCH frames; answer as soon as the PV name shows
                // up anywhere in the stream.
                if seen.windows(pv.len()).any(|w| w == pv.as_bytes()) {
                    let _ = stream.write_all(&search_reply(cid, server_addr)).await;
                    let _ = stream.flush().await;
                    return;
                }
            }
        });

        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
        let engine = name_servers_only_search_engine(
            vec![ns_addr],
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        )
        .expect("name-servers-only engine must build with a non-empty NS list");
        let engine_handle = tokio::spawn(engine);

        req_tx
            .send(SearchRequest::Schedule {
                cid,
                pv_name: pv.into(),
                reason: SearchReason::Initial,
            })
            .expect("schedule send");

        let resolved = tokio::time::timeout(Duration::from_secs(10), resp_rx.recv()).await;

        engine_handle.abort();
        ns_handle.abort();

        // Restore the environment for any later serial test.
        match restore_list {
            Some(v) => unsafe { std::env::set_var("EPICS_CA_ADDR_LIST", v) },
            None => unsafe { std::env::remove_var("EPICS_CA_ADDR_LIST") },
        }
        match restore_auto {
            Some(v) => unsafe { std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", v) },
            None => unsafe { std::env::remove_var("EPICS_CA_AUTO_ADDR_LIST") },
        }

        match resolved.expect("resolution must complete within 10 s with no UDP socket bound") {
            Some(SearchResponse::Found {
                cid: found_cid,
                server_addr: found_addr,
            }) => {
                assert_eq!(found_cid, cid, "the reply must resolve the scheduled cid");
                assert_eq!(
                    found_addr, server_addr,
                    "the resolved server must be the one the TCP name server named"
                );
            }
            Some(SearchResponse::MultiplyDefined { .. }) => {
                panic!("a single name-server reply must resolve as Found")
            }
            None => panic!("search-response channel closed before a reply arrived"),
        }
    }

    /// An `EPICS_CA_NAME_SERVERS` peer that accepts, keeps reading and never
    /// answers is retired on the bound every CA TCP connection is held to —
    /// `CircuitWatchdog::retire_bound()`, i.e. `echo_idle_secs()` of quiet plus
    /// `ECHO_TIMEOUT_SECS` for the `CA_PROTO_ECHO` probe to come back.
    ///
    /// Measured before this rule existed, on a VxWorks target against exactly
    /// this peer: ten consecutive descriptor censuses on one local port over
    /// ≈600 s, one accept, dial-pool `attempts=1`
    /// (`doc/vxworks-circuit-wedge-on-target-measurement.md` §3.5). The reader
    /// wrote `CA_PROTO_ECHO` on a hardcoded 60 s tick and never looked for the
    /// reply, so nothing could ever end the circuit short of TCP itself giving
    /// up — which a *silent but reachable* peer never makes happen.
    ///
    /// Retirement is observed from the peer's side, as a SECOND accept: the
    /// per-name-server task redials after the same `CONN_TMO` a failed connect
    /// waits. Against the pre-fix reader the first accept is the only one there
    /// will ever be, so this test times out instead of passing.
    ///
    /// Virtual time (`start_paused`), because the bound is built from
    /// `EPICS_CA_CONN_TMO` and defaults to 30 s + 5 s, with another 30 s before
    /// the redial. Real sockets under a paused clock still work — tokio
    /// auto-advances only while nothing is runnable, which is precisely what a
    /// silent peer produces.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(start_paused = true)]
    async fn a_silent_name_server_is_retired_on_the_circuit_bound() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let ns_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock name-server listener bind");
        let ns_addr = ns_listener.local_addr().expect("mock name-server addr");

        let (accept_tx, mut accept_rx) = mpsc::unbounded_channel::<()>();
        let _ns_handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = ns_listener.accept().await else {
                    return;
                };
                if accept_tx.send(()).is_err() {
                    return;
                }
                // Read everything and answer nothing — the peer shape measured
                // on target. Draining matters: a peer whose receive buffer
                // filled would stall the client's writes instead, which is a
                // different failure and not the one under test.
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                    }
                });
            }
        });

        let (_req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, _resp_rx) = mpsc::unbounded_channel();
        let engine = name_servers_only_search_engine(
            vec![ns_addr],
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        )
        .expect("name-servers-only engine must build with a non-empty NS list");
        let _engine_handle = tokio::spawn(engine);

        // The circuit is dialled by `run_engine` itself, so the first accept
        // needs no search request.
        tokio::time::timeout(Duration::from_secs(30), accept_rx.recv())
            .await
            .expect("the engine must dial the name server")
            .expect("accept channel closed");

        // From here the peer says nothing at all. Retirement (the watchdog's
        // verdict) plus one CONN_TMO of redial cadence is the whole budget;
        // the slack covers the dial itself.
        let budget = crate::client::transport::CircuitWatchdog::retire_bound()
            + crate::client::transport::connection_timeout()
            + Duration::from_secs(10);
        let started = tokio::time::Instant::now();
        tokio::time::timeout(budget, accept_rx.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "a silent name server must be retired within \
                     echo_idle_secs() + ECHO_TIMEOUT_SECS and redialled; no \
                     second accept in {budget:?}"
                )
            })
            .expect("accept channel closed");
        let elapsed = started.elapsed();
        assert!(
            elapsed <= budget,
            "redial took {elapsed:?}, past the {budget:?} the bound allows"
        );
    }

    /// Retiring the circuit releases the socket *then*, not one reconnect
    /// interval later.
    ///
    /// The watchdog ends the reader, but a CA circuit's descriptor lives until
    /// **both** halves are gone. While the reconnect backoff sat in the same
    /// scope as the halves, the send half stayed alive across
    /// `EPICS_CA_CONN_TMO`: on a hosted build (`into_split`, which has no
    /// shutdown-on-reader-drop) the connection stayed ESTABLISHED for 30 s
    /// after the peer had been declared unresponsive, and on the blocking
    /// backend the descriptor stayed allocated for the same 30 s after
    /// `ReaderPumpGuard` had already shut the socket down — which is what a
    /// descriptor census on target sees. `serve_nameserver_circuit` owns the
    /// halves and the backoff is in its caller, so returning releases them.
    ///
    /// Measured from the peer, which is where the difference is observable: the
    /// FIN must arrive on the bound, not on the bound plus the redial cadence.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(start_paused = true)]
    async fn retiring_a_name_service_circuit_closes_it_before_the_backoff() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        let ns_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock name-server listener bind");
        let ns_addr = ns_listener.local_addr().expect("mock name-server addr");

        // Accept once and report how long that first connection stayed open.
        let (eof_tx, mut eof_rx) = mpsc::unbounded_channel::<Duration>();
        let _ns_handle = tokio::spawn(async move {
            let Ok((mut stream, _peer)) = ns_listener.accept().await else {
                return;
            };
            let accepted_at = tokio::time::Instant::now();
            let mut buf = vec![0u8; 8192];
            // Drain and answer nothing until the client closes on us.
            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
            let _ = eof_tx.send(accepted_at.elapsed());
        });

        let (_req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, _resp_rx) = mpsc::unbounded_channel();
        let engine = name_servers_only_search_engine(
            vec![ns_addr],
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        )
        .expect("name-servers-only engine must build with a non-empty NS list");
        let _engine_handle = tokio::spawn(engine);

        let bound = crate::client::transport::CircuitWatchdog::retire_bound();
        let backoff = crate::client::transport::connection_timeout();
        // Generous enough to absorb the dial and the probe round trip, and
        // still far below the `bound + backoff` the pre-fix scope produced.
        let allowed = bound + Duration::from_secs(10);
        let held = tokio::time::timeout(bound + backoff + Duration::from_secs(30), eof_rx.recv())
            .await
            .expect("the retired circuit must close; the peer saw no FIN at all")
            .expect("eof channel closed");
        assert!(
            held <= allowed,
            "the socket outlived its retirement by the reconnect backoff: held \
             {held:?}, bound {bound:?}, backoff {backoff:?}"
        );
    }

    /// The name-service circuit reassembles a reply the kernel tore across
    /// read syscalls — the property its reader's framing exists for, and the
    /// one the fold onto `transport::next_frame` had to preserve.
    ///
    /// `stage_c1_name_servers_only_resolves_without_binding_udp` above proves
    /// the circuit resolves, but it lets the mock name server write the reply
    /// in one `write_all`, so it passes against a reader with **no framing at
    /// all**: one read delivers the whole 24-byte message and the dispatcher
    /// parses it. That is why the inline framing loop's rules had no
    /// regression cover, and why fixing them twice (once per copy) was
    /// possible.
    ///
    /// Here the mock writes byte by byte with a yield between each, so the
    /// reader is guaranteed to observe every prefix length: 0..15 (short of a
    /// header), 16..23 (header present, body still in flight), and finally the
    /// whole message. A reader that forwarded on any of those prefixes hands
    /// the dispatcher a truncated frame and the cid never resolves; a reader
    /// that mis-measured the message length desyncs and never resolves the
    /// *second* message, which is why two are sent.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nameserver_reply_torn_across_reads_still_resolves() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let ns_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock name-server listener bind");
        let ns_addr = ns_listener.local_addr().expect("mock name-server addr");

        // Two cids, two distinct servers: the second only resolves if the
        // reader measured the first message's length exactly right.
        let first: (u32, SocketAddr) = (7001, "10.0.0.11:5064".parse().unwrap());
        let second: (u32, SocketAddr) = (7002, "10.0.0.12:5064".parse().unwrap());
        let pvs = ["TEST:CA:TORN:ONE", "TEST:CA:TORN:TWO"];

        let ns_handle = tokio::spawn(async move {
            let (mut stream, _peer) = ns_listener.accept().await.expect("mock NS: accept");
            let mut buf = vec![0u8; 8192];
            let mut seen: Vec<u8> = Vec::new();
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                seen.extend_from_slice(&buf[..n]);
                // Answer once both PV names have been searched for, so the
                // two replies leave together and share the torn stream.
                if !pvs
                    .iter()
                    .all(|pv| seen.windows(pv.len()).any(|w| w == pv.as_bytes()))
                {
                    continue;
                }
                let mut wire = search_reply(first.0, first.1);
                wire.extend_from_slice(&search_reply(second.0, second.1));
                for byte in wire {
                    if stream.write_all(&[byte]).await.is_err() {
                        return;
                    }
                    // Force the write out on its own so the client's reader
                    // wakes on a one-byte read rather than a coalesced one.
                    let _ = stream.flush().await;
                    tokio::task::yield_now().await;
                }
                // Hold the connection open; closing here would let a reader
                // that only reassembles on EOF pass.
                std::future::pending::<()>().await;
            }
        });

        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
        let engine = name_servers_only_search_engine(
            vec![ns_addr],
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        )
        .expect("name-servers-only engine must build with a non-empty NS list");
        let engine_handle = tokio::spawn(engine);

        for ((cid, _), pv) in [first, second].iter().zip(pvs) {
            req_tx
                .send(SearchRequest::Schedule {
                    cid: *cid,
                    pv_name: pv.into(),
                    reason: SearchReason::Initial,
                })
                .expect("schedule send");
        }

        let mut found: Vec<(u32, SocketAddr)> = Vec::new();
        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            while found.len() < 2 {
                match resp_rx.recv().await {
                    Some(SearchResponse::Found { cid, server_addr }) => {
                        if !found.iter().any(|(c, _)| *c == cid) {
                            found.push((cid, server_addr));
                        }
                    }
                    Some(SearchResponse::MultiplyDefined { pv_name, .. }) => {
                        panic!("{pv_name} resolved as MultiplyDefined; one reply each was sent")
                    }
                    None => panic!("search-response channel closed before both replies arrived"),
                }
            }
        })
        .await;

        engine_handle.abort();
        ns_handle.abort();

        outcome.unwrap_or_else(|_| {
            panic!(
                "both cids must resolve from a byte-at-a-time reply stream; \
                 resolved {found:?}"
            )
        });
        found.sort_by_key(|(cid, _)| *cid);
        assert_eq!(
            found,
            vec![first, second],
            "each cid must resolve to the server its own message named"
        );
    }

    /// Build a single-message CA_PROTO_SEARCH reply datagram naming
    /// `server` as the host of client-cid `cid`. Mirrors the wire
    /// shape parsed by `handle_search_response`: `data_type` carries
    /// the server port, `cid` carries the server IPv4 (big-endian),
    /// `available` carries the client cid, and an 8-byte payload holds
    /// the minor version.
    fn search_reply(cid: u32, server: SocketAddr) -> Vec<u8> {
        let ip = match server.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => unreachable!("test uses IPv4 only"),
        };
        let mut hdr = CaHeader::new(CA_PROTO_SEARCH);
        hdr.data_type = server.port();
        hdr.cid = u32::from_be_bytes(ip.octets());
        hdr.available = cid;
        hdr.set_payload_size(8, 1, crate::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut buf = hdr.to_bytes().to_vec();
        buf.extend_from_slice(&(CA_MINOR_VERSION).to_be_bytes());
        buf.extend_from_slice(&[0u8; 6]); // pad to 8-byte payload
        buf
    }

    /// Regression: after a channel connected to server A is torn
    /// down (ServerDisconnect / TcpClosed) and re-searched, a SEARCH
    /// reply from a legitimately-different server B must resolve as a
    /// normal `Found`, NOT a false `MultiplyDefined` (`ECA_DBLCHNL`).
    ///
    /// The coordinator enqueues `Schedule{Reconnect}` on disconnect;
    /// the search-engine task and the coordinator are decoupled, and
    /// `tokio::select!` can pick a ready UDP/TCP reply arm before the
    /// queued request arm. The fix drains `request_rx` (via
    /// `drain_pending_requests`) at the top of every reply arm so the
    /// `Schedule{Reconnect}` — which invalidates `resolved` through
    /// `remove_channel` — is always applied before the reply is
    /// parsed, matching libca's single-thread ordering
    /// (`cac.cpp:591-661`).
    ///
    /// Pre-fix (reply parsed before the drain), the stale `resolved`
    /// entry for server A is still present, so the server-B reply
    /// trips the `prev_addr != server_addr` branch and emits
    /// `MultiplyDefined`.
    #[test]
    fn mr_r3_reconnect_to_new_server_no_false_multiply_defined() {
        let server_a: SocketAddr = "10.0.0.1:5064".parse().unwrap();
        let server_b: SocketAddr = "10.0.0.2:5064".parse().unwrap();
        let src_b: SocketAddr = "10.0.0.2:5064".parse().unwrap();
        let cid = 1u32;
        let pv = "MR:R3:PV";

        let mut state = SearchEngineState::new();
        // The replies under test arrive on a name-server circuit
        // (`handle_tcp_response`), so the drain runs against the transport
        // that path actually uses — and binds no socket for a pure
        // state-ordering test.
        let mut transport =
            SearchTransport::name_servers_only(&[server_a]).expect("non-empty NS list");
        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<SearchResponse>();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<SearchRequest>();

        // 1. Channel finds server A and connects.
        schedule_initial(&mut state, cid, pv);
        handle_tcp_response(&mut state, &search_reply(cid, server_a), server_a, &resp_tx);
        match resp_rx.try_recv() {
            Ok(SearchResponse::Found { server_addr, .. }) => {
                assert_eq!(server_addr, server_a);
            }
            Ok(SearchResponse::MultiplyDefined { .. }) => {
                panic!("first reply must resolve as Found, not MultiplyDefined")
            }
            Err(e) => panic!("expected Found from server A, got recv error {e:?}"),
        }
        handle_request(
            &mut state,
            SearchRequest::ConnectResult {
                cid,
                success: true,
                server_addr: server_a,
            },
        );
        assert!(
            state.resolved.contains_key(&cid),
            "resolved entry kept past ConnectResult{{success}}"
        );

        // 2. Server A disconnects — coordinator enqueues a reconnect
        //    Schedule. It sits on `req_rx` until the engine drains it.
        req_tx
            .send(SearchRequest::Schedule {
                cid,
                pv_name: pv.into(),
                reason: SearchReason::Reconnect,
            })
            .expect("reconnect schedule send");

        // 3. A SEARCH reply from the NEW server B is ready at the same
        //    time. The fix drains queued requests before parsing it.
        let mut immediate: Vec<u32> = Vec::new();
        drain_pending_requests(&mut state, &mut transport, &mut req_rx, &mut immediate);
        assert!(
            !state.resolved.contains_key(&cid),
            "Schedule{{Reconnect}} must invalidate the stale resolved \
             entry before the server-B reply is parsed"
        );
        handle_tcp_response(&mut state, &search_reply(cid, server_b), src_b, &resp_tx);

        // 4. The server-B reply must resolve as Found, never as a
        //    false MultiplyDefined.
        match resp_rx.try_recv() {
            Ok(SearchResponse::Found { server_addr, .. }) => {
                assert_eq!(
                    server_addr, server_b,
                    "reconnect must resolve to the new server B"
                );
            }
            Ok(SearchResponse::MultiplyDefined {
                prev_addr,
                new_addr,
                ..
            }) => panic!(
                "false ECA_DBLCHNL after legitimate server migration: \
                 prev={prev_addr} new={new_addr} — the reconnect Schedule \
                 was not drained before the reply was parsed"
            ),
            Err(e) => panic!("expected Found from server B, got recv error {e:?}"),
        }
        assert!(
            resp_rx.try_recv().is_err(),
            "no further responses expected after the single Found"
        );
    }

    /// R2-26 regression: a UDP SEARCH reply that arrives WITHOUT a
    /// leading `CA_PROTO_VERSION` in its datagram must still resolve the
    /// channel. C `udpiiu.cpp::searchRespAction` transfers the channel
    /// to its virtual circuit on every reply, and `cac.cpp:651` /
    /// `searchTimer.cpp:323` uninstall it from the search list
    /// unconditionally — the per-datagram sequence marker gates only
    /// libca's RTT/timer tuning, never resolution. Pre-fix Rust gated
    /// resolution on a same-datagram VERSION (`last_valid_seq.is_none()`
    /// → drop), silently dropping legacy / third-party replies and never
    /// connecting the channel.
    #[test]
    fn r2_26_unsequenced_udp_search_reply_resolves() {
        let server: SocketAddr = "10.0.0.7:5064".parse().unwrap();
        let cid = 7u32;

        let mut state = SearchEngineState::new();
        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<SearchResponse>();

        schedule_initial(&mut state, cid, "R2:26:PV");
        // `search_reply` builds a SEARCH-only datagram — no VERSION
        // header precedes it, exactly the case the pre-fix gate dropped.
        handle_udp_response(&mut state, &search_reply(cid, server), server, &resp_tx);

        match resp_rx.try_recv() {
            Ok(SearchResponse::Found {
                cid: found_cid,
                server_addr,
            }) => {
                assert_eq!(found_cid, cid);
                assert_eq!(server_addr, server);
            }
            Ok(_) => {
                panic!("unsequenced UDP SEARCH reply must resolve as Found, got another variant")
            }
            Err(e) => {
                panic!("unsequenced UDP SEARCH reply must resolve as Found, got recv error {e:?}")
            }
        }
        // The cid is now resolved (removed from pending); the
        // pending-membership check — not any seq marker — is the
        // resolve-once guard.
        assert!(
            !state.pending.contains_key(&cid),
            "resolved cid must leave the pending set"
        );
    }

    /// R2-26 companion: the modern common case — a UDP datagram that
    /// DOES carry a leading `CA_PROTO_VERSION` before the SEARCH reply —
    /// must still resolve after the VERSION arm became a no-op. Guards
    /// against the VERSION message being mis-parsed or its payload
    /// mis-skipped now that it no longer records a sequence number.
    #[test]
    fn r2_26_version_prefixed_udp_search_reply_still_resolves() {
        let server: SocketAddr = "10.0.0.8:5064".parse().unwrap();
        let cid = 8u32;

        let mut state = SearchEngineState::new();
        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<SearchResponse>();

        schedule_initial(&mut state, cid, "R2:26:PV2");
        // Prepend a per-datagram VERSION header (data_type=1 marks
        // sequenceNoIsValid, cid carries the echoed seq), as a modern
        // rsrv reply datagram does, then the SEARCH reply.
        let mut dgram = {
            let mut v = CaHeader::new(CA_PROTO_VERSION);
            v.data_type = 1;
            v.cid = 42;
            v.count = CA_MINOR_VERSION;
            v.to_bytes().to_vec()
        };
        dgram.extend_from_slice(&search_reply(cid, server));
        handle_udp_response(&mut state, &dgram, server, &resp_tx);

        match resp_rx.try_recv() {
            Ok(SearchResponse::Found {
                cid: found_cid,
                server_addr,
            }) => {
                assert_eq!(found_cid, cid);
                assert_eq!(server_addr, server);
            }
            Ok(_) => {
                panic!(
                    "VERSION-prefixed UDP SEARCH reply must resolve as Found, got another variant"
                )
            }
            Err(e) => panic!(
                "VERSION-prefixed UDP SEARCH reply must resolve as Found, got recv error {e:?}"
            ),
        }
    }
}
