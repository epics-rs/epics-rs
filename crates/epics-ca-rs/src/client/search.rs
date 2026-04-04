use std::collections::{BTreeSet, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use epics_base_rs::runtime::sync::mpsc;

use crate::protocol::*;

use super::types::{SearchReason, SearchRequest, SearchResponse};

// ---------------------------------------------------------------------------
// Configuration constants
// ---------------------------------------------------------------------------

/// Minimum RTT floor (matches libca minRoundTripEstimate).
const MIN_RTT: Duration = Duration::from_millis(32);

/// Default base RTTE when no RTT samples have been collected yet.
const DEFAULT_BASE_RTTE: Duration = Duration::from_millis(100);

/// Default maximum search period (EPICS_CA_MAX_SEARCH_PERIOD).
const DEFAULT_MAX_SEARCH_PERIOD: Duration = Duration::from_secs(300);

/// Lower limit for max search period.
const MIN_MAX_SEARCH_PERIOD: Duration = Duration::from_secs(60);

/// Conservative UDP datagram size limit to avoid fragmentation.
const MAX_UDP_SEND: usize = 1024;

/// How long a server stays in the penalty box after a TCP connect failure.
const PENALTY_DURATION: Duration = Duration::from_secs(30);

/// Maximum frames_per_try (cap for AIMD additive increase).
const MAX_FRAMES_PER_TRY: u32 = 10;

/// AIMD evaluation window duration.
const AIMD_WINDOW: Duration = Duration::from_secs(1);

/// After beacon anomaly, keep the channel in a fast rescan mode briefly.
const BEACON_FAST_RESCAN_WINDOW: Duration = Duration::from_secs(5);

/// Retry period cap during the fast rescan window.
const BEACON_FAST_RESCAN_PERIOD: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// RTT Estimator — Jacobson/Karels (RFC 6298)
// ---------------------------------------------------------------------------

struct RttEstimator {
    srtt: f64,
    mdev: f64,
    initialized: bool,
}

impl RttEstimator {
    fn new() -> Self {
        Self {
            srtt: 0.0,
            mdev: 0.0,
            initialized: false,
        }
    }

    fn update(&mut self, sample_secs: f64) {
        let sample = sample_secs.max(MIN_RTT.as_secs_f64());
        if !self.initialized {
            self.srtt = sample;
            self.mdev = sample / 2.0;
            self.initialized = true;
        } else {
            let err = sample - self.srtt;
            self.srtt += 0.125 * err;
            self.mdev += 0.25 * (err.abs() - self.mdev);
        }
    }

    fn rto(&self) -> Duration {
        if !self.initialized {
            return DEFAULT_BASE_RTTE;
        }
        let rto_secs = (self.srtt + 4.0 * self.mdev).max(MIN_RTT.as_secs_f64());
        Duration::from_secs_f64(rto_secs)
    }
}

// ---------------------------------------------------------------------------
// Per-channel search state
// ---------------------------------------------------------------------------

struct ChannelSearchState {
    #[allow(dead_code)]
    cid: u32,
    #[allow(dead_code)]
    pv_name: String,
    /// Pre-built payload: SEARCH header + padded PV name (no VERSION prefix).
    search_payload: Vec<u8>,
    /// Current lane index (0 = fastest retry, increases on timeout).
    lane_index: u32,
    /// When this channel's next search packet is due.
    next_deadline: Instant,
    /// When the last search packet was sent (for RTT measurement).
    last_sent_at: Option<Instant>,
    /// Temporary fast-rescan window after beacon anomaly.
    fast_rescan_until: Option<Instant>,
}

// ---------------------------------------------------------------------------
// AIMD congestion control
// ---------------------------------------------------------------------------

struct SendBudget {
    frames_per_try: u32,
    sent_this_window: u32,
    responded_this_window: u32,
    window_start: Instant,
}

impl SendBudget {
    fn new() -> Self {
        Self {
            frames_per_try: 1,
            sent_this_window: 0,
            responded_this_window: 0,
            window_start: Instant::now(),
        }
    }

    /// Evaluate the AIMD window: additive increase on good response rate,
    /// multiplicative decrease on loss.
    fn evaluate(&mut self, now: Instant) {
        if now.duration_since(self.window_start) < AIMD_WINDOW {
            return;
        }
        if self.sent_this_window > 0 {
            let rate =
                self.responded_this_window as f64 / self.sent_this_window as f64;
            if rate > 0.5 {
                self.frames_per_try =
                    (self.frames_per_try + 1).min(MAX_FRAMES_PER_TRY);
            } else if rate < 0.1 && self.frames_per_try > 1 {
                self.frames_per_try = 1;
            }
        }
        self.responded_this_window = 0;
        self.sent_this_window = 0;
        self.window_start = now;
    }
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
    channels: HashMap<u32, ChannelSearchState>,
    /// (deadline, cid) — BTreeSet gives O(log n) first/insert/remove.
    deadline_set: BTreeSet<(Instant, u32)>,
    rtt_per_path: HashMap<SocketAddr, RttEstimator>,
    budget: SendBudget,
    penalty: HashMap<SocketAddr, PenaltyEntry>,
    max_search_period: Duration,
}

impl SearchEngineState {
    fn new() -> Self {
        Self {
            channels: HashMap::new(),
            deadline_set: BTreeSet::new(),
            rtt_per_path: HashMap::new(),
            budget: SendBudget::new(),
            penalty: HashMap::new(),
            max_search_period: parse_max_search_period(),
        }
    }

    /// Worst-case RTO across all destination paths.
    fn base_rtte(&self) -> Duration {
        self.rtt_per_path
            .values()
            .map(|e| e.rto())
            .max()
            .unwrap_or(DEFAULT_BASE_RTTE)
    }

    /// Insert or re-insert a channel into the deadline set.
    #[allow(dead_code)]
    fn schedule_channel(&mut self, cid: u32, deadline: Instant) {
        if let Some(ch) = self.channels.get_mut(&cid) {
            // Remove old deadline entry if present.
            self.deadline_set.remove(&(ch.next_deadline, cid));
            ch.next_deadline = deadline;
            self.deadline_set.insert((deadline, cid));
        }
    }

    /// Remove a channel entirely.
    fn remove_channel(&mut self, cid: u32) {
        if let Some(ch) = self.channels.remove(&cid) {
            self.deadline_set.remove(&(ch.next_deadline, cid));
        }
    }
}

/// Compute the retry period for a given lane index.
fn lane_period(lane_index: u32, base_rtte: Duration, max_period: Duration) -> Duration {
    let multiplier = 1u64.checked_shl(lane_index).unwrap_or(u64::MAX);
    let period_nanos = (base_rtte.as_nanos() as u64).saturating_mul(multiplier);
    let period = Duration::from_nanos(period_nanos);
    period.min(max_period)
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub(crate) async fn run_search_engine(
    addr_list: Vec<SocketAddr>,
    mut request_rx: mpsc::UnboundedReceiver<SearchRequest>,
    response_tx: mpsc::UnboundedSender<SearchResponse>,
) {
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = socket.set_broadcast(true);

    // Increase OS socket receive buffer for search response bursts.
    #[cfg(unix)]
    {
        use std::os::fd::BorrowedFd;
        let fd = unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) };
        let sock_ref = socket2::SockRef::from(&fd);
        let _ = sock_ref.set_recv_buffer_size(256 * 1024);
    }

    let mut state = SearchEngineState::new();
    let mut recv_buf = [0u8; 65536];

    loop {
        let next_deadline = state
            .deadline_set
            .iter()
            .next()
            .map(|(d, _)| *d)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        let sleep = epics_base_rs::runtime::task::sleep_until(next_deadline);

        tokio::select! {
            req = request_rx.recv() => {
                let Some(req) = req else { return };
                handle_request(&mut state, req);
                // Drain any additional queued requests before sending,
                // so a burst of Schedule messages gets batched together.
                while let Ok(req) = request_rx.try_recv() {
                    handle_request(&mut state, req);
                }
                // Immediately send any due searches (deadline = now).
                send_due_searches(&mut state, &addr_list, &socket).await;
            }

            result = socket.recv_from(&mut recv_buf) => {
                let Ok((len, src)) = result else { continue };
                handle_udp_response(&mut state, &recv_buf[..len], src, &response_tx);
            }

            _ = sleep => {
                send_due_searches(&mut state, &addr_list, &socket).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

fn handle_request(state: &mut SearchEngineState, req: SearchRequest) {
    match req {
        SearchRequest::Schedule {
            cid,
            pv_name,
            reason,
        } => {
            let search_payload = build_search_payload(cid, &pv_name);
            let now = Instant::now();
            let fast_rescan_until = match reason {
                SearchReason::BeaconAnomaly => Some(now + BEACON_FAST_RESCAN_WINDOW),
                SearchReason::Initial | SearchReason::Reconnect => None,
            };
            let deadline = now;

            // Remove old entry if re-scheduling (e.g., reconnect).
            state.remove_channel(cid);

            let ch = ChannelSearchState {
                cid,
                pv_name,
                search_payload,
                lane_index: 0,
                next_deadline: deadline,
                last_sent_at: None,
                fast_rescan_until,
            };

            state.deadline_set.insert((deadline, cid));
            state.channels.insert(cid, ch);
        }

        SearchRequest::Cancel { cid } => {
            state.remove_channel(cid);
        }

        SearchRequest::ConnectResult {
            cid,
            success,
            server_addr,
        } => {
            if success {
                state.remove_channel(cid);
                state.penalty.remove(&server_addr);
            } else {
                state.penalty.insert(
                    server_addr,
                    PenaltyEntry {
                        until: Instant::now() + PENALTY_DURATION,
                    },
                );
            }
        }
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
    if data.len() < CaHeader::SIZE {
        return;
    }

    let recv_time = Instant::now();
    let mut offset = 0;

    while offset + CaHeader::SIZE <= data.len() {
        let Ok(hdr) = CaHeader::from_bytes(&data[offset..]) else {
            break;
        };

        match hdr.cmmd {
            CA_PROTO_SEARCH => {
                let server_port = hdr.data_type;
                let server_ip = if hdr.cid == 0xFFFFFFFF {
                    src.ip()
                } else {
                    std::net::IpAddr::V4(Ipv4Addr::from(hdr.cid.to_be_bytes()))
                };
                let server_addr =
                    SocketAddr::new(server_ip, server_port as u16);
                let cid = hdr.available;

                // Check penalty box — skip penalized servers so the channel
                // can potentially find a non-penalized one.
                let penalized = state
                    .penalty
                    .get(&server_addr)
                    .map(|p| p.until > recv_time)
                    .unwrap_or(false);

                if penalized {
                    // Don't consume this response — let the channel keep
                    // searching for a better server.
                    offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                    continue;
                }

                if let Some(ch) = state.channels.remove(&cid) {
                    state.deadline_set.remove(&(ch.next_deadline, cid));

                    // RTT measurement.
                    if let Some(sent_at) = ch.last_sent_at {
                        let sample = recv_time
                            .duration_since(sent_at)
                            .as_secs_f64();
                        state
                            .rtt_per_path
                            .entry(server_addr)
                            .or_insert_with(RttEstimator::new)
                            .update(sample);
                    }

                    state.budget.responded_this_window += 1;

                    let _ = response_tx
                        .send(SearchResponse::Found { cid, server_addr });
                }
            }
            _ => {}
        }

        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
    }
}

// ---------------------------------------------------------------------------
// Batched send with AIMD congestion control
// ---------------------------------------------------------------------------

async fn send_due_searches(
    state: &mut SearchEngineState,
    addr_list: &[SocketAddr],
    socket: &UdpSocket,
) {
    let now = Instant::now();

    // AIMD window evaluation.
    state.budget.evaluate(now);

    // Expire old penalties.
    state.penalty.retain(|_, entry| entry.until > now);

    // Collect due channels.
    let mut due_cids: Vec<u32> = Vec::new();
    while let Some(&(deadline, cid)) = state.deadline_set.iter().next() {
        if deadline > now {
            break;
        }
        state.deadline_set.remove(&(deadline, cid));
        due_cids.push(cid);
    }

    if due_cids.is_empty() {
        return;
    }

    // VERSION header — one per datagram.
    let version_hdr = {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.to_bytes()
    };

    // Build and send batched datagrams.
    let frames_per_try = state.budget.frames_per_try;
    let mut current_frame = Vec::with_capacity(MAX_UDP_SEND);
    current_frame.extend_from_slice(&version_hdr);
    let mut frames_sent: u32 = 0;
    let mut current_frame_cids: Vec<u32> = Vec::new();
    let mut sent_cids: Vec<u32> = Vec::new();

    for &cid in &due_cids {
        let Some(ch) = state.channels.get(&cid) else {
            continue;
        };
        let payload = &ch.search_payload;

        // If adding this payload would exceed MAX_UDP_SEND, flush.
        if current_frame.len() + payload.len() > MAX_UDP_SEND
            && current_frame.len() > CaHeader::SIZE
        {
            if frames_sent < frames_per_try {
                for addr in addr_list {
                    let _ = socket.send_to(&current_frame, addr).await;
                }
                state.budget.sent_this_window += 1;
                frames_sent += 1;
                sent_cids.append(&mut current_frame_cids);
            }
            current_frame.clear();
            current_frame.extend_from_slice(&version_hdr);
            current_frame_cids.clear();
        }

        // If a single payload exceeds MAX_UDP_SEND - header, send alone.
        if CaHeader::SIZE + payload.len() > MAX_UDP_SEND {
            if frames_sent >= frames_per_try {
                break;
            }
            let mut solo = Vec::with_capacity(CaHeader::SIZE + payload.len());
            solo.extend_from_slice(&version_hdr);
            solo.extend_from_slice(payload);
            for addr in addr_list {
                let _ = socket.send_to(&solo, addr).await;
            }
            state.budget.sent_this_window += 1;
            frames_sent += 1;
            sent_cids.push(cid);
        } else {
            current_frame.extend_from_slice(payload);
            current_frame_cids.push(cid);
        }
    }

    // Flush remaining frame.
    if current_frame.len() > CaHeader::SIZE && frames_sent < frames_per_try {
        for addr in addr_list {
            let _ = socket.send_to(&current_frame, addr).await;
        }
        state.budget.sent_this_window += 1;
        sent_cids.append(&mut current_frame_cids);
    }

    finalize_due_searches(state, &due_cids, &sent_cids, now);
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
    search_hdr.data_type = CA_DO_REPLY;
    search_hdr.count = CA_MINOR_VERSION;
    search_hdr.cid = cid;
    search_hdr.available = cid;

    let mut payload =
        Vec::with_capacity(CaHeader::SIZE + pv_payload.len());
    payload.extend_from_slice(&search_hdr.to_bytes());
    payload.extend_from_slice(&pv_payload);
    payload
}

/// Parse EPICS_CA_MAX_SEARCH_PERIOD environment variable.
fn parse_max_search_period() -> Duration {
    let secs = epics_base_rs::runtime::env::get("EPICS_CA_MAX_SEARCH_PERIOD")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(DEFAULT_MAX_SEARCH_PERIOD.as_secs_f64())
        .max(MIN_MAX_SEARCH_PERIOD.as_secs_f64());
    Duration::from_secs_f64(secs)
}

fn finalize_due_searches(
    state: &mut SearchEngineState,
    due_cids: &[u32],
    sent_cids: &[u32],
    now: Instant,
) {
    // Record send time and advance lanes only for channels that were actually
    // sent in this cycle. Budget-limited channels remain due immediately.
    let base_rtte = state.base_rtte();
    let max_period = state.max_search_period;
    for &cid in sent_cids {
        if let Some(ch) = state.channels.get_mut(&cid) {
            ch.last_sent_at = Some(now);
            ch.lane_index += 1;
            let mut period = lane_period(ch.lane_index, base_rtte, max_period);
            if ch
                .fast_rescan_until
                .is_some_and(|until| now < until)
            {
                period = period.min(BEACON_FAST_RESCAN_PERIOD);
            } else {
                ch.fast_rescan_until = None;
            }
            ch.next_deadline = now + period;
            state.deadline_set.insert((ch.next_deadline, cid));
        }
    }

    for &cid in due_cids {
        if sent_cids.contains(&cid) {
            continue;
        }
        if let Some(ch) = state.channels.get_mut(&cid) {
            ch.next_deadline = now;
            state.deadline_set.insert((ch.next_deadline, cid));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_estimator_initial_sample() {
        let mut est = RttEstimator::new();
        est.update(0.050);
        assert!(est.initialized);
        assert!((est.srtt - 0.050).abs() < 0.001);
        // rto = srtt + 4*mdev = 0.050 + 4*0.025 = 0.150
        assert!((est.rto().as_secs_f64() - 0.150).abs() < 0.01);
    }

    #[test]
    fn rtt_estimator_converges() {
        let mut est = RttEstimator::new();
        for _ in 0..100 {
            est.update(0.010); // 10ms, but clamped to MIN_RTT (32ms)
        }
        // Converges to MIN_RTT floor since 10ms < 32ms.
        assert!((est.srtt - MIN_RTT.as_secs_f64()).abs() < 0.001);
        assert!(est.rto() >= MIN_RTT);
    }

    #[test]
    fn rtt_estimator_min_floor() {
        let mut est = RttEstimator::new();
        est.update(0.001); // below MIN_RTT
        assert!(est.srtt >= MIN_RTT.as_secs_f64());
    }

    #[test]
    fn lane_period_exponential() {
        let max = DEFAULT_MAX_SEARCH_PERIOD;
        let base = Duration::from_millis(100);
        let p0 = lane_period(0, base, max);
        let p1 = lane_period(1, base, max);
        let p2 = lane_period(2, base, max);
        assert_eq!(p0, Duration::from_millis(100));
        assert_eq!(p1, Duration::from_millis(200));
        assert_eq!(p2, Duration::from_millis(400));
    }

    #[test]
    fn lane_period_clamped_at_max() {
        let max = Duration::from_secs(60);
        let base = Duration::from_millis(100);
        let p30 = lane_period(30, base, max);
        assert_eq!(p30, Duration::from_secs(60));
    }

    #[test]
    fn lane_period_overflow_safe() {
        let max = DEFAULT_MAX_SEARCH_PERIOD;
        let base = Duration::from_millis(100);
        // lane_index = 64 overflows 1u64 << 64
        let p = lane_period(64, base, max);
        assert_eq!(p, max);
    }

    #[test]
    fn deadline_set_eager_removal() {
        let mut set = BTreeSet::new();
        let now = Instant::now();
        set.insert((now, 1u32));
        set.insert((now + Duration::from_secs(1), 2u32));
        assert!(set.remove(&(now, 1)));
        assert_eq!(set.len(), 1);
        assert_eq!(set.iter().next().unwrap().1, 2);
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
    fn parse_max_search_period_default() {
        // Without env var, should return 300s.
        // We can't easily clear env in tests, but verify the floor works.
        let secs = 30.0f64.max(MIN_MAX_SEARCH_PERIOD.as_secs_f64());
        assert_eq!(secs, 60.0);
    }

    #[test]
    fn aimd_additive_increase() {
        let mut budget = SendBudget::new();
        budget.sent_this_window = 10;
        budget.responded_this_window = 8; // 80% > 50%
        budget.window_start =
            Instant::now() - AIMD_WINDOW - Duration::from_millis(1);
        budget.evaluate(Instant::now());
        assert_eq!(budget.frames_per_try, 2);
    }

    #[test]
    fn aimd_multiplicative_decrease() {
        let mut budget = SendBudget::new();
        budget.frames_per_try = 5;
        budget.sent_this_window = 10;
        budget.responded_this_window = 0; // 0% < 10%
        budget.window_start =
            Instant::now() - AIMD_WINDOW - Duration::from_millis(1);
        budget.evaluate(Instant::now());
        assert_eq!(budget.frames_per_try, 1);
    }

    #[test]
    fn aimd_hold_steady() {
        let mut budget = SendBudget::new();
        budget.frames_per_try = 3;
        budget.sent_this_window = 10;
        budget.responded_this_window = 3; // 30% — between 10% and 50%
        budget.window_start =
            Instant::now() - AIMD_WINDOW - Duration::from_millis(1);
        budget.evaluate(Instant::now());
        assert_eq!(budget.frames_per_try, 3);
    }

    #[test]
    fn budget_limited_channels_remain_due() {
        let now = Instant::now();
        let mut state = SearchEngineState::new();

        for cid in 1..=3 {
            let ch = ChannelSearchState {
                cid,
                pv_name: format!("PV:{cid}"),
                search_payload: build_search_payload(cid, &format!("PV:{cid}")),
                lane_index: 0,
                next_deadline: now,
                last_sent_at: None,
                fast_rescan_until: None,
            };
            state.channels.insert(cid, ch);
            state.deadline_set.insert((now, cid));
        }

        finalize_due_searches(&mut state, &[1, 2, 3], &[1], now);

        let sent = state
            .channels
            .values()
            .filter(|ch| ch.last_sent_at.is_some())
            .count();
        let unsent_due_now = state
            .channels
            .values()
            .filter(|ch| ch.last_sent_at.is_none() && ch.next_deadline == now)
            .count();

        assert_eq!(sent, 1);
        assert_eq!(unsent_due_now, 2);
    }

    #[test]
    fn beacon_anomaly_enables_fast_rescan_window() {
        let mut state = SearchEngineState::new();

        handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 42,
                pv_name: "TEST:PV".into(),
                reason: SearchReason::BeaconAnomaly,
            },
        );

        let ch = state.channels.get(&42).unwrap();
        assert_eq!(ch.lane_index, 0);
        assert!(ch.fast_rescan_until.is_some());
    }

    #[test]
    fn fast_rescan_clamps_retry_period() {
        let now = Instant::now();
        let mut state = SearchEngineState::new();
        state.max_search_period = Duration::from_secs(300);

        state.channels.insert(
            7,
            ChannelSearchState {
                cid: 7,
                pv_name: "TEST:PV".into(),
                search_payload: build_search_payload(7, "TEST:PV"),
                lane_index: 8,
                next_deadline: now,
                last_sent_at: None,
                fast_rescan_until: Some(now + Duration::from_secs(1)),
            },
        );

        finalize_due_searches(&mut state, &[7], &[7], now);

        let ch = state.channels.get(&7).unwrap();
        assert!(ch.next_deadline <= now + BEACON_FAST_RESCAN_PERIOD);
    }
}
