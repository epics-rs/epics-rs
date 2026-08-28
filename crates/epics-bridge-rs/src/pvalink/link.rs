//! `PvaLink` — a single live PVA link bound to a remote PV.

// RTEMS-EXEC-MODEL-ALLOW(23): checked - these run and pass in the exec-backend
// suite.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use epics_base_rs::runtime::task::{self, TaskAbortHandle};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::client_native::CacheAction;
use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask, PutLeaf};
use epics_pva_rs::pv_request::PvRequestExpr;
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

use super::config::{LinkDirection, ProcMode, PvaLinkConfig};

#[derive(Debug, thiserror::Error)]
pub enum PvaLinkError {
    #[error("PVA error: {0}")]
    Pva(#[from] epics_pva_rs::error::PvaError),
    #[error("link is INP-only, write requested")]
    NotWritable,
    #[error("link is OUT-only, read requested")]
    NotReadable,
    #[error("pvalink monitor for {0:?} is disconnected (cached value is stale)")]
    Disconnected(String),
    #[error("field {0:?} not found in remote NT structure")]
    FieldNotFound(String),
    #[error("field {0:?} is not a scalar")]
    NotScalar(String),
    #[error("link config parse error: {0}")]
    Config(#[from] super::config::PvaLinkParseError),
    #[error("local-only link {0:?} has no matching local record")]
    NotLocal(String),
}

pub type PvaLinkResult<T> = Result<T, PvaLinkError>;

/// A lifecycle-aware event delivered to the scan-on-update forwarder
/// for an INP+monitor link.
///
/// pvxs `pvaLinkChannel::run()` scans every CP / eligible-CPP target
/// after BOTH the monitor value-update branch AND the
/// `catch(client::Disconnect&)` branch — the same
/// `atomic_records` / `nonatomic_records` scan loop runs with no
/// value comparison (`pvxs/ioc/pvalink_channel.cpp:360-373` sets
/// `connected=false` + `onDisconnect()`, then `:422-432` scans). A
/// value-only channel can only fire the forwarder on `Value`, so a
/// disconnect with no trailing value was silently missed and the CP
/// record never observed LINK_ALARM/INVALID. `Disconnected` carries
/// that lifecycle transition so the forwarder runs the scan path
/// even when no new `PvField` arrives.
///
/// Reconnect needs no separate variant: the re-subscribe loop's first
/// post-reconnect monitor callback delivers a `Value`, which already
/// drives the scan (matching pvxs's reconnect/`onTypeChange` scan at
/// `:342-352`).
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// A fresh monitor value arrived; drives per-field change detection.
    Value(PvField),
    /// The live monitor subscription ended (IOC restart / transient
    /// I/O). Carries no value; the forwarder scans CP/CPP targets
    /// unconditionally so the owning record processes the disconnect.
    Disconnected,
}

/// Overrun accounting for the INP-monitor scan-trigger queue.
///
/// The bounded `mpsc` between the monitor task and the scan forwarder
/// holds up to the link's `Q` triggers. When it is full, the surplus
/// monitor event is NOT dropped: its value has already coalesced into
/// the link's `latest` cache (the forwarder reads `latest`, never the
/// event payload), and this records that ONE more CP/CPP scan is owed
/// plus increments an overrun counter — so the owning record still
/// processes the newest value and the loss-of-intermediate-events is
/// explicit, never silent.
///
/// This mirrors EPICS `db_queue_event_log`'s replace-last-on-full
/// coalescing (`modules/database/src/ioc/db/dbEvent.c:812-827`): on a
/// full event ring it replaces the last queued event for the monitor
/// (latest-wins), bumps `evSubscrip::nreplace`, and skips re-signalling
/// the event task because "the event task has already been notified"
/// (`dbEvent.c:823`) — exactly the no-extra-wakeup property relied on
/// here (a full queue means a backlog `recv` will wake the forwarder).
#[derive(Debug, Default)]
pub(crate) struct ScanOverrun {
    /// A coalesced scan trigger is owed: the forwarder must run one more
    /// scan pass against the `latest` cache. Set by the monitor task on
    /// a full queue, cleared by the forwarder when it runs the pass.
    pending: AtomicBool,
    /// Total monitor events that overran the queue and coalesced — the
    /// EPICS `evSubscrip::nreplace` overrun marker, surfaced for
    /// diagnostics/tests.
    replaced: AtomicU64,
}

impl ScanOverrun {
    /// Record a coalesced overrun: arm the owed scan and bump the
    /// overrun counter. Called by [`enqueue_scan_trigger`] when the
    /// scan-trigger queue is full.
    pub(crate) fn mark(&self) {
        self.replaced.fetch_add(1, Ordering::Relaxed);
        // Release so the forwarder's `Acquire` swap observes the arm
        // together with the `latest` write that preceded it.
        self.pending.store(true, Ordering::Release);
    }

    /// Forwarder side: take the owed scan trigger (`true` => run one more
    /// scan pass). Idempotent — clears the flag.
    pub(crate) fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    /// Total coalesced overruns observed since the link opened.
    pub(crate) fn count(&self) -> u64 {
        self.replaced.load(Ordering::Relaxed)
    }
}

/// Enqueue one scan trigger onto the bounded forwarder channel,
/// coalescing on a full queue instead of dropping. This is the single
/// owner of the "deliver a scan trigger, never lose one" rule: both the
/// value path and the disconnect path of the monitor task route through
/// it, so a saturated queue coalesces to the latest cache + overrun
/// marker uniformly (EPICS `db_queue_event_log`, `dbEvent.c:812-827`),
/// never a silent best-effort drop.
fn enqueue_scan_trigger(tx: &mpsc::Sender<ScanEvent>, overrun: &ScanOverrun, event: ScanEvent) {
    if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(event) {
        overrun.mark();
    }
}

/// Take an INP link from connected to disconnected: stamp the
/// disconnect time and enqueue the payload-less scan trigger that makes
/// CP / passive-CPP targets process and expose LINK_ALARM/INVALID.
///
/// **The single owner of that transition**, called from the two places
/// that can observe it — the monitor's `Disconnected`/`Finished` event and
/// the subscription future returning — so neither can implement half of it.
/// Idempotent by construction: the `swap` gate means the second observer of
/// one outage is a no-op, and a subscription that never delivered an event
/// synthesizes no scan at all. The gate is ours, not pvxs parity: pvxs has
/// one observer, so its `catch(client::Disconnect&)` arm clears `connected`
/// and runs every link's `onDisconnect()` unconditionally
/// (`pvxs/ioc/pvalink_channel.cpp:360-373`, `pvxs/ioc/pvalink_link.cpp:75-81`); its
/// `if(!connected)` at `:342` is the value-update path's reconnect arm, not
/// a disconnect gate.
fn inp_disconnect_scan(
    connected: &AtomicBool,
    disconnect_time: &Mutex<Option<(i64, i32)>>,
    tx: &mpsc::Sender<ScanEvent>,
    overrun: &ScanOverrun,
) {
    if !connected.swap(false, Ordering::AcqRel) {
        return;
    }
    // Capture the disconnect-event time so a `time=true` link adopts it
    // (not the stale last value's time, nor local processing time) while
    // disconnected — pvxs `snap_time = e.time` in `onDisconnect`
    // (`pvxs/ioc/pvalink_channel.cpp:372`). We have no client-supplied exception
    // time, so the observation moment (now) is the closest analogue.
    if let Ok(dur) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        *disconnect_time.lock() = Some((dur.as_secs() as i64, dur.subsec_nanos() as i32));
    }
    // Notify the scan forwarder of the lifecycle transition so CP /
    // passive-CPP targets process and expose LINK_ALARM/INVALID even
    // though no value follows the disconnect. pvxs scans the
    // atomic/non-atomic target lists after the `catch(client::Disconnect&)`
    // branch (`pvxs/ioc/pvalink_channel.cpp:360-373` + `:422-432`). Same coalescing
    // rule as the value path: if the channel is saturated, mark an owed
    // scan instead of dropping the disconnect trigger. The owed scan is
    // payload-less, so the forwarder still reads `is_connected() == false`
    // and raises LINK_ALARM/INVALID on the CP/CPP targets.
    enqueue_scan_trigger(tx, overrun, ScanEvent::Disconnected);
}

/// A live PVA link.
///
/// Constructed once per record-link instance. For INP links the optional
/// monitor task spawns automatically; for OUT links the link just owns the
/// PvaClient and writes synchronously.
pub struct PvaLink {
    /// Field declaration order matters: Rust drops fields in
    /// declaration order, so `_monitor_abort` MUST come BEFORE
    /// `client`. The spawned monitor task holds its own clone of the
    /// PvaClient; if the parent client field drops first and that
    /// shutdown is cancellation-token-driven (not refcount-only),
    /// the still-running task hits I/O errors before the abort
    /// lands. Order: abort first → task stops → client drops cleanly.
    _monitor_abort: Option<MonitorAbort>,
    config: PvaLinkConfig,
    client: PvaClient,
    /// Latest received value (INP only — None until first event).
    latest: Arc<Mutex<Option<PvField>>>,
    /// Live-connection flag for INP+monitor links (B-pvalink-restart).
    ///
    /// The INP monitor task is a re-subscribe loop: it sets this
    /// `true` while a subscription is live and `false` the moment
    /// `pvmonitor` returns (IOC restart / transient I/O), then
    /// re-subscribes with exponential backoff. `is_connected()`
    /// reads this flag so a downstream IOC restart is reflected as
    /// a disconnect instead of serving the stale cached value
    /// forever. `None` for OUT / non-monitor links.
    monitor_connected: Option<Arc<AtomicBool>>,
    /// Receiver half of the INP-monitor record-notification channel.
    ///
    /// B3: every monitor event for an INP+monitor link pushes the new
    /// [`PvField`] onto this channel (the sender lives inside the
    /// spawned monitor task). [`Self::take_notify_rx`] hands the
    /// receiver to the resolver, which forwards events into
    /// `scan_on_update` / CP processing of the owning record. Wrapped
    /// in a `Mutex<Option<..>>` because the receiver is single-consumer
    /// and is moved out exactly once.
    ///
    /// Carries [`ScanEvent`], not a bare `PvField`: the monitor task
    /// also pushes a `Disconnected` lifecycle event when a live
    /// subscription ends, so the forwarder can scan CP/CPP targets on
    /// the disconnect even though no value accompanies it.
    notify_rx: Mutex<Option<mpsc::Receiver<ScanEvent>>>,
    /// Per-field staged OUT writes — this link's role as the shared
    /// `pvaLinkChannel` PUT owner (pvxs `put_scratch` / `put_queue`,
    /// `pvxs/ioc/pvalink_channel.cpp:127-263`).
    ///
    /// Sibling OUT links to the same `(pv, pipeline, queue_size)` now
    /// resolve to THIS one `PvaLink` (the registry no longer keys on
    /// per-link OUT options), and each stages its value here keyed by
    /// its own `field` selector (`""` = root). A non-deferred write
    /// flushes the whole map into ONE combined upstream PUT, with the
    /// process option resolved across every participating field
    /// (PP/CP/CPP beats NPP, `pvxs/ioc/pvalink_channel.cpp:257-263`). A
    /// `defer=true` write stays staged until a non-deferred sibling —
    /// or the production drain — flushes it, so several fields combine
    /// into one PUT (`documentation/pvalink.rst:111-113`).
    ///
    /// Keyed per field with most-recent-wins overwrite: pvxs keeps one
    /// `put_scratch` per link and a later write to the same field
    /// supersedes the earlier one before the channel PUT starts
    /// (`pvxs/ioc/pvalink_lset.cpp:645-653`), so `retry` replays only the most
    /// recent incomplete value, never a stale FIFO log.
    ///
    /// Stores [`QueuedPut`] (via [`StagedPut`]), not a bare `PvField`:
    /// a string-form write must replay through the string `pvput` path
    /// (which coerces the text against the channel's introspected
    /// type), not as a `PvField::Scalar(String)` that would mismatch a
    /// numeric record.
    out_scratch: Mutex<HashMap<String, StagedPut>>,
    /// Set when a disconnected flush left `retry=true` writes staged for
    /// replay. The `LinkSet::flush_puts` production drain re-attempts the
    /// staged writes only while this is set, so a freshly-`defer`red
    /// write awaiting its sibling is never flushed early — only a
    /// genuinely stuck retry write is replayed on the next OUT activity
    /// (pvxs replays the queued put on the client reconnect event,
    /// `pvalink.rst:99-100`).
    retry_pending: AtomicBool,
    /// `(seconds_past_epoch, nanoseconds)` captured at the moment the
    /// INP monitor subscription last dropped — pvxs `snap_time = e.time`
    /// set in `onDisconnect` (`pvxs/ioc/pvalink_channel.cpp:372`). A `time=true`
    /// link in the disconnected-stale state adopts THIS into the owning
    /// record's time, not the stale last-value timestamp and not local
    /// processing time, mirroring pvxs `pvaGetValue` copying `snap_time`
    /// on the invalid read (`pvxs/ioc/pvalink_lset.cpp:268-270`). `None` until the
    /// first disconnect; shared with the monitor task, so it is an `Arc`.
    disconnect_time: Arc<Mutex<Option<(i64, i32)>>>,
    /// Coalescing overrun accounting for the scan-trigger queue. Shared
    /// with the monitor task (producer, marks on a full queue) and the
    /// scan forwarder (consumer, drains the owed scan), so a full local
    /// queue coalesces to `latest` + an overrun marker rather than
    /// silently dropping a CP/CPP scan. `Arc` because it is shared.
    scan_overrun: Arc<ScanOverrun>,
    /// Value-read-latched remote alarm snapshot, keyed by the selected
    /// field root (`""` = top level). pvxs `pvaLink` keeps a
    /// `snap_severity` / `snap_message` pair initialized to
    /// `INVALID_ALARM` / blank and updated ONLY by `pvaGetValue` on a
    /// connected value read (`pvxs/ioc/pvalink.h:250`,
    /// `pvxs/ioc/pvalink_lset.cpp:412-422`). The ungated DB-link alarm inspection
    /// ([`Self::remote_alarm_snapshot`], the `dbGetAlarm` /
    /// `dbGetAlarmMsg` analog) reports THIS latched value, not the live
    /// cached monitor alarm — so a connected link reports the initial
    /// INVALID/blank until the first value read, exactly like pvxs
    /// `dbGetAlarm` before the first `dbGetLink`
    /// (`pvxs/test/testpvalink.cpp:373-375`).
    ///
    /// Keyed by field because one shared `PvaLink` serves sibling DB
    /// links that differ in `.field` (the registry keys on
    /// `(pv, pipeline, Q)`, not field), and pvxs resolves the alarm at
    /// `root[fieldName]` — each field root carries its own alarm, so each
    /// gets its own latch. An absent entry is the pvxs initial
    /// `INVALID_ALARM` / blank snapshot.
    snap_alarm: Mutex<HashMap<String, (i32, String)>>,
}

/// A queued OUT-link Put, preserving the caller's original value
/// form so the deferred replay uses the same type-correct path the
/// immediate Put would have used.
#[derive(Debug, Clone)]
enum QueuedPut {
    /// From [`PvaLink::write`] — replayed via the string `pvput`
    /// path so the text is coerced against the channel's native
    /// scalar type, not forced to a `String` field.
    Str(String),
    /// From [`PvaLink::write_pv_field`] — replayed verbatim via the
    /// typed `pvput_pv_field` path.
    Field(PvField),
}

/// One field's staged OUT write on the shared channel, carrying the
/// originating link's PUT options. Several of these — one per
/// participating sibling field — combine into one upstream PUT, so the
/// per-link `proc`/`retry` must travel with the value rather than being
/// read from the shared owner's representative `config`.
#[derive(Debug, Clone)]
struct StagedPut {
    /// The value to write, in its original string/typed form.
    value: QueuedPut,
    /// The originating link's process mode — folded into the combined
    /// PUT's process option (PP/CP/CPP beats NPP).
    proc: ProcMode,
    /// The originating link's `retry` flag — decides whether this field
    /// is requeued for replay or dropped on a disconnect.
    retry: bool,
    /// The originating link's `block` flag (`record._options.block`).
    block: bool,
}

struct MonitorAbort(TaskAbortHandle);

impl Drop for MonitorAbort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl PvaLink {
    /// Open a link against the configured PV, using the caller's
    /// [`PvaClient`].
    ///
    /// For INP+monitor links, this also spawns a background monitor task.
    ///
    /// The client is **injected, never built here**. pvxs holds exactly
    /// one `client::Context` for the whole IOC —
    /// `linkGlobal->provider_remote`, built once in `linkGlobal_t::alloc()`
    /// (`pvxs/ioc/pvalink.h:107`, `pvxs/ioc/pvalink.cpp:51-64`) — and every
    /// `pvaLinkChannel` runs on it. A client built per link would give each
    /// link its own `ConnectionPool` (keyed on `SocketAddr`) and its own
    /// search engine, so N links to one upstream IOC would open N TCP
    /// connections instead of one. [`super::registry::PvaLinkRegistry`] is
    /// the single owner of that client and passes a clone here; that is why
    /// this signature takes one rather than calling `PvaClient::builder()`.
    pub async fn open(config: PvaLinkConfig, client: PvaClient) -> PvaLinkResult<Self> {
        // The link owns its INP / OUT re-subscribe loops for as long as it
        // lives, so the capability is taken here, at the one async entry
        // point that builds the link.
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("PvaLink::open is awaited on the IOC's reactor");
        let latest = Arc::new(Mutex::new(None));
        let disconnect_time: Arc<Mutex<Option<(i64, i32)>>> = Arc::new(Mutex::new(None));
        let scan_overrun = Arc::new(ScanOverrun::default());
        let mut notify_rx = None;
        let mut monitor_abort = None;
        let mut monitor_connected = None;

        if matches!(config.direction, LinkDirection::Inp) && config.monitor {
            // B3 / B4-Q: the channel buffer is sized to the link's
            // `Q` (monitor queue depth). When it fills, the monitor
            // task does NOT silently drop the event — it coalesces to
            // the `latest` cache and arms an overrun marker via
            // `enqueue_scan_trigger`, so the forwarder still runs one
            // CP/CPP scan with the newest value (EPICS
            // `db_queue_event_log` replace-last-on-full,
            // `dbEvent.c:812-827`).
            let (tx, rx) = mpsc::channel::<ScanEvent>(config.queue_size.max(1));
            notify_rx = Some(rx);

            let pv_name = config.pv_name.clone();
            let latest_clone = latest.clone();
            let client_clone = client.clone();
            let connected = Arc::new(AtomicBool::new(false));
            let connected_for_task = connected.clone();
            monitor_connected = Some(connected);
            let disconnect_time_task = disconnect_time.clone();
            let scan_overrun_task = scan_overrun.clone();
            // B4-pipeline / B4-Q: when the link asks for pipeline
            // flow-control or a non-default queue depth, build a
            // pvRequest carrying `record[pipeline=...,queueSize=N]`
            // so the negotiation reaches the server. Otherwise use
            // the plain monitor (lower overhead, matches prior
            // behaviour).
            let request = monitor_request(&config);
            // B-pvalink-restart: the INP monitor is a re-subscribe
            // loop, mirroring `channel_cache.rs::spawn_upstream_monitor`.
            // The subscription runs on the TYPED EVENT STREAM
            // (`pvmonitor_events`), not on the value-only `pvmonitor`,
            // because the disconnect must arrive as an EVENT — see
            // `inp_disconnect_scan` below.
            let join = reactor.spawn(async move {
                let mut backoff = Duration::from_millis(250);
                let max_backoff = Duration::from_secs(30);
                loop {
                    let tx_inner = tx.clone();
                    let tx_disc = tx.clone();
                    let latest_inner = latest_clone.clone();
                    let connected_inner = connected_for_task.clone();
                    let connected_disc = connected_for_task.clone();
                    let overrun_inner = scan_overrun_task.clone();
                    let overrun_disc = scan_overrun_task.clone();
                    let disconnect_time_ev = disconnect_time_task.clone();
                    // pvxs's pvalink channel is driven by the monitor's
                    // event stream: `Connected` / value updates / the
                    // `catch(client::Disconnect&)` branch
                    // (`pvxs/ioc/pvalink_channel.cpp:335-373`). Ours must be too.
                    //
                    // `mask_disconnected: false` is the load-bearing half.
                    // `op_monitor_events` RE-SUBSCRIBES INTERNALLY on
                    // connection loss (`ops_v2.rs`, `MonitorEnd::ConnectionLost`
                    // → deliver `Disconnected`, sleep, loop), so the
                    // subscription future does NOT return when the upstream
                    // IOC dies. Inferring the disconnect from that future
                    // returning — what this loop did before — therefore
                    // never fired: measured on the RTEMS stage-5 target —
                    // killing the upstream left both downstream records at
                    // their stale value with `SEVR=0 STAT=0` and
                    // `is_connected() == true`, while the
                    // client itself correctly reported the peer gone
                    // (`conn alive=false channels=[]`, `active=0 searching=2`).
                    let on_event = move |ev: MonitorEvent| match ev {
                        MonitorEvent::Connected { .. } => {
                            connected_inner.store(true, Ordering::Release);
                        }
                        MonitorEvent::Data { value, .. } => {
                            connected_inner.store(true, Ordering::Release);
                            *latest_inner.lock() = Some(value.clone());
                            // The value is now cached in `latest`; enqueue a
                            // scan trigger, coalescing to the cache + overrun
                            // marker if the queue is full rather than dropping
                            // the CP/CPP scan (EPICS `db_queue_event_log`,
                            // `dbEvent.c:812-827`).
                            enqueue_scan_trigger(
                                &tx_inner,
                                &overrun_inner,
                                ScanEvent::Value(value),
                            );
                        }
                        // A clean end-of-stream is as much a loss of the
                        // live value as a dropped circuit: pvxs pushes
                        // `Finished()` into the same stream the
                        // `Disconnect` lands in, and `pvaLinkChannel`
                        // stops being `connected` either way.
                        MonitorEvent::Disconnected | MonitorEvent::Finished => {
                            inp_disconnect_scan(
                                &connected_disc,
                                &disconnect_time_ev,
                                &tx_disc,
                                &overrun_disc,
                            );
                        }
                    };
                    let result = client_clone
                        .pvmonitor_events(
                            &pv_name,
                            request.as_ref(),
                            MonitorEventMask {
                                mask_connected: false,
                                mask_disconnected: false,
                            },
                            on_event,
                        )
                        .await;
                    // The subscription future returned — the stream ended
                    // for good (`Fatal`/`Remote`, or the channel closed),
                    // rather than dropping a circuit it re-subscribes to
                    // itself. Same transition, same owner: idempotent
                    // because it is gated on the prior `connected` state,
                    // so a `Disconnected` event already handled above does
                    // not scan twice here.
                    inp_disconnect_scan(
                        &connected_for_task,
                        &disconnect_time_task,
                        &tx,
                        &scan_overrun_task,
                    );
                    match &result {
                        Ok(()) => tracing::debug!(
                            pv = %pv_name,
                            "pvalink: INP monitor ended, re-subscribing"
                        ),
                        Err(e) => tracing::warn!(
                            pv = %pv_name,
                            error = %e,
                            backoff_ms = backoff.as_millis() as u64,
                            "pvalink: INP monitor failed, will retry"
                        ),
                    }
                    task::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                }
            });
            monitor_abort = Some(MonitorAbort(join.abort_handle()));
        } else if matches!(config.direction, LinkDirection::Out) {
            // pvxs pvalink runs a monitor on EVERY channel — INP *and*
            // OUT — to maintain `lchan->connected`
            // (`pvxs/ioc/pvalink_channel.cpp:342-363`); the OUT-write gate
            // `valid()` is `connected && root` (`pvxs/ioc/pvalink_link.cpp:69`,
            // applied at `pvxs/ioc/pvalink_lset.cpp:609`). An OUT link must
            // therefore track connection too, even though it never reads
            // the cached value: without it `is_connected()` (the
            // `latest.is_some()` fallback) is permanently false for OUT
            // links, so the non-retry write gate below would drop every
            // OUT write. This is the same re-subscribe-with-backoff loop
            // as the INP monitor, stripped to connection-tracking only —
            // no value cache, no scan triggers, no disconnect-time
            // capture (those are INP-read concerns).
            let pv_name = config.pv_name.clone();
            let client_clone = client.clone();
            let connected = Arc::new(AtomicBool::new(false));
            let connected_for_task = connected.clone();
            monitor_connected = Some(connected);
            // pvxs keeps one `pvaLinkChannel` per `(channelName,
            // pvRequest)` and calls `chan.monitor(this, pvRequest)` for
            // BOTH INP and OUT channels, with `pvRequest` coming from
            // `pvaLink::makeRequest()` (`pvxs/ioc/pvalink_link.cpp:49-65`,
            // installed at `pvxs/ioc/pvalink_channel.cpp` channel open). That
            // request always carries `record._options.atomic=true`,
            // `pipeline=<cfg>`, and `queueSize=<Q/4>`. The OUT
            // connection-tracking monitor must therefore open with the
            // same request as the INP monitor — not a plain
            // option-less subscription — so the server sees the pvalink
            // atomic/pipeline/queue negotiation on the OUT liveness path.
            let request = monitor_request(&config);
            let join = reactor.spawn(async move {
                let mut backoff = Duration::from_millis(250);
                let max_backoff = Duration::from_secs(30);
                loop {
                    let connected_inner = connected_for_task.clone();
                    // Same event stream as the INP monitor, for the same
                    // reason: `op_monitor_events` re-subscribes internally
                    // on connection loss, so the future returning is NOT
                    // the disconnect signal. Without the `Disconnected`
                    // event the OUT-write gate would keep reporting a dead
                    // upstream as writable and every put would be swallowed
                    // by the peer's absence instead of deferring/retrying.
                    // Liveness is proven by a delivered event, never by
                    // entering the subscription; the value itself is ignored.
                    let on_event = move |ev: MonitorEvent| match ev {
                        MonitorEvent::Connected { .. } | MonitorEvent::Data { .. } => {
                            connected_inner.store(true, Ordering::Release);
                        }
                        MonitorEvent::Disconnected | MonitorEvent::Finished => {
                            connected_inner.store(false, Ordering::Release);
                        }
                    };
                    let result = client_clone
                        .pvmonitor_events(
                            &pv_name,
                            request.as_ref(),
                            MonitorEventMask {
                                mask_connected: false,
                                mask_disconnected: false,
                            },
                            on_event,
                        )
                        .await;
                    // The subscription future returned — the stream ended
                    // for good. Reflect the disconnect so the OUT-write gate
                    // sees `is_connected() == false` until re-subscribed.
                    connected_for_task.store(false, Ordering::Release);
                    match &result {
                        Ok(()) => tracing::debug!(
                            pv = %pv_name,
                            "pvalink: OUT connection monitor ended, re-subscribing"
                        ),
                        Err(e) => tracing::warn!(
                            pv = %pv_name,
                            error = %e,
                            backoff_ms = backoff.as_millis() as u64,
                            "pvalink: OUT connection monitor failed, will retry"
                        ),
                    }
                    task::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                }
            });
            monitor_abort = Some(MonitorAbort(join.abort_handle()));
        }

        Ok(Self {
            config,
            client,
            latest,
            monitor_connected,
            notify_rx: Mutex::new(notify_rx),
            out_scratch: Mutex::new(HashMap::new()),
            snap_alarm: Mutex::new(HashMap::new()),
            retry_pending: AtomicBool::new(false),
            disconnect_time,
            scan_overrun,
            _monitor_abort: monitor_abort,
        })
    }

    /// The shared scan-trigger overrun accounting for this link's INP
    /// monitor. Handed to the scan forwarder so a full-queue coalesce
    /// still drives one CP/CPP scan, and read by diagnostics/tests.
    pub(crate) fn scan_overrun(&self) -> Arc<ScanOverrun> {
        self.scan_overrun.clone()
    }

    /// Total monitor events that overran the bounded scan-trigger queue
    /// and coalesced into the latest value (EPICS `evSubscrip::nreplace`,
    /// `dbEvent.c:821`). A non-zero count means intermediate events were
    /// folded into the newest — not silently dropped — and the owning
    /// record still scanned. Surfaced for diagnostics and regression
    /// tests.
    pub fn monitor_overrun_count(&self) -> u64 {
        self.scan_overrun.count()
    }

    /// Take the INP-monitor notification receiver (B3). Returns the
    /// channel exactly once; subsequent calls return `None`. The
    /// resolver calls this right after `open` to spawn the
    /// scan-on-update forwarder. `None` for OUT / non-monitor links
    /// (they never created a channel) or after the receiver has
    /// already been claimed.
    pub fn take_notify_rx(&self) -> Option<mpsc::Receiver<ScanEvent>> {
        self.notify_rx.lock().take()
    }

    pub fn config(&self) -> &PvaLinkConfig {
        &self.config
    }

    /// Read the current value of the linked field.
    ///
    /// In monitor mode this returns the cached latest value; otherwise it
    /// triggers a fresh GET.
    pub async fn read(&self) -> PvaLinkResult<PvField> {
        self.read_with_field(&self.config.field.clone()).await
    }

    /// Like [`Self::read`] but selects `field` instead of
    /// `self.config.field`. Lets the resolver pass a per-link field
    /// selector when multiple DB links share a cached upstream channel
    /// but differ in which sub-field they target (pvxs
    /// `pvxs/ioc/pvalink_link.cpp:91` — `root = lchan->root[fieldName]`).
    pub async fn read_with_field(&self, field: &str) -> PvaLinkResult<PvField> {
        if matches!(self.config.direction, LinkDirection::Out) {
            return Err(PvaLinkError::NotReadable);
        }
        // a monitor link serves its cached value ONLY
        // while the subscription is live. When the upstream monitor is
        // down — an IOC restart / transient I/O, or before the first
        // event has ever arrived — the cache is not a valid current
        // read; surface a failed read so the lset's `get_value`
        // returns `None` and base processing raises LINK_ALARM/INVALID,
        // instead of handing back a stale value as if it were fresh.
        // The `latest` slot is still retained for the diagnostic /
        // timestamp accessors (`latest_value`, `time_stamp`). Mirrors
        // pvxs `pvaGetValue`'s `!self->valid()` gate
        // (pvxs/ioc/pvalink_lset.cpp:259-272), which returns failure while
        // disconnected and does NOT fall back to a one-shot GET.
        // Non-monitor INP links keep the fresh-GET path: each read
        // proves connectivity by itself, so there is no stale window.
        if self.config.monitor {
            if !self.is_connected() {
                return Err(PvaLinkError::Disconnected(self.config.pv_name.clone()));
            }
            let cached = self.latest.lock().clone();
            return match cached {
                Some(v) => {
                    // value-read latch: pvxs `pvaGetValue` copies the live
                    // `fld_severity` / `fld_message` into the snapshot on
                    // every connected read (`pvxs/ioc/pvalink_lset.cpp:412-422`).
                    self.latch_alarm_snapshot(field, &v);
                    Ok(select_link_value(&v, field))
                }
                // Connected flag set but the first event has not yet
                // written `latest` (the callback stores the flag before
                // the value): not-yet-valid, surface a failed read.
                None => Err(PvaLinkError::Disconnected(self.config.pv_name.clone())),
            };
        }
        let result = self.client.pvget_full(&self.config.pv_name).await?;
        Ok(select_link_value(&result.value, field))
    }

    /// Synchronous fast-path read: return the cached field if the
    /// monitor has delivered at least one event, without ever
    /// awaiting. Returns `None` for OUT links, non-monitor INPs,
    /// or pre-first-event INPs.
    ///
    /// Lets the record-link resolver path skip `block_on` on every
    /// process — the typical hot case where a monitor has already
    /// populated the cache. Mirrors `pvaGetValue` — the sync read of
    /// the cached `latest` slot (`pvxs/ioc/pvalink_lset.cpp:259`).
    pub fn try_read_cached(&self) -> Option<PvField> {
        self.try_read_cached_with_field(&self.config.field.clone())
    }

    /// Like [`Self::try_read_cached`] but selects `field` instead of
    /// `self.config.field`. Mirrors the per-link field override path
    /// (`pvxs/ioc/pvalink_link.cpp:91`).
    pub fn try_read_cached_with_field(&self, field: &str) -> Option<PvField> {
        if matches!(self.config.direction, LinkDirection::Out) || !self.config.monitor {
            return None;
        }
        // same disconnect gate as `read_with_field` — the
        // sync fast path must never serve a stale cached value while the
        // monitor subscription is down. Returning `None` makes the lset
        // `get_value` fall through to the (also-gated) slow path and
        // ultimately report no value, so base raises LINK_ALARM/INVALID.
        if !self.is_connected() {
            return None;
        }
        let v = self.latest.lock().clone()?;
        // value-read latch — same as `read_with_field` (pvxs
        // `pvaGetValue` snapshot, `pvxs/ioc/pvalink_lset.cpp:412-422`).
        self.latch_alarm_snapshot(field, &v);
        Some(select_link_value(&v, field))
    }

    /// Convenience: read the value as f64.
    pub async fn read_scalar_f64(&self) -> PvaLinkResult<f64> {
        let pv = self.read().await?;
        scalar_as_f64(&pv).ok_or_else(|| PvaLinkError::NotScalar(self.config.field.clone()))
    }

    /// Write a string value to the linked PV (OUT direction only),
    /// using THIS link's own `config` options. Convenience wrapper over
    /// the shared-channel staging path for direct callers and tests;
    /// the record OUT-link dispatch uses [`Self::put_out_str`] to pass
    /// each sibling's per-call options onto the shared owner.
    ///
    /// delegates to [`Self::write_with_block`] with `block=false`.
    pub async fn write(&self, value_str: &str) -> PvaLinkResult<()> {
        self.write_with_block(value_str, false).await
    }

    /// Like [`Self::write`] but passes `block` through to the PUT pvRequest
    /// (`record._options.block`). Mirrors `pvaPutValueX(wait)` →
    /// `block = !after_put.empty()` (`pvxs/ioc/pvalink_lset.cpp:650-651`,
    /// `pvxs/ioc/pvalink_channel.cpp:222-223`).
    pub async fn write_with_block(&self, value_str: &str, block: bool) -> PvaLinkResult<()> {
        self.stage_and_flush(
            &self.config.field,
            self.config.proc,
            self.config.defer,
            self.config.retry,
            block,
            QueuedPut::Str(value_str.to_string()),
        )
        .await
    }

    /// Write a typed `PvField` directly (no string round-trip), using
    /// THIS link's own `config` options. For large arrays this avoids
    /// the O(N) `Display` allocation + O(N) pvput parse-back that a
    /// string write triggers.
    ///
    /// delegates to [`Self::write_pv_field_with_block`] with
    /// `block=false`.
    pub async fn write_pv_field(&self, value: &PvField) -> PvaLinkResult<()> {
        self.write_pv_field_with_block(value, false).await
    }

    /// Like [`Self::write_pv_field`] but passes `block` through.
    pub async fn write_pv_field_with_block(
        &self,
        value: &PvField,
        block: bool,
    ) -> PvaLinkResult<()> {
        self.stage_and_flush(
            &self.config.field,
            self.config.proc,
            self.config.defer,
            self.config.retry,
            block,
            QueuedPut::Field(value.clone()),
        )
        .await
    }

    /// Stage + flush a string OUT write onto this shared channel owner
    /// using the PER-CALL link config's options.
    ///
    /// The record OUT-link dispatch resolves each sibling link's own
    /// `field` / `proc` / `defer` / `retry` and routes it here, so
    /// siblings that share this channel each apply their own options:
    /// the shared `self.config` is only the first opener's
    /// representative config and never drives a sibling's PUT behavior
    /// (pvxs keeps the per-link options on the child `pvaLink`,
    /// `pvxs/ioc/pvalink.h:65`).
    ///
    /// `block` requests a completion-aware PUT (`record._options.block`)
    /// — set when the originating record is in a put-notify /
    /// blocking-put chain (pvxs `pvaPutValueAsync`); a plain OUT write
    /// passes `false` (`pvaPutValue`).
    pub async fn put_out_str(
        &self,
        link_cfg: &PvaLinkConfig,
        value_str: &str,
        block: bool,
    ) -> PvaLinkResult<()> {
        self.stage_and_flush(
            &link_cfg.field,
            link_cfg.proc,
            link_cfg.defer,
            link_cfg.retry,
            block,
            QueuedPut::Str(value_str.to_string()),
        )
        .await
    }

    /// Typed-`PvField` twin of [`Self::put_out_str`] — keeps a large
    /// array on the typed PUT path instead of a string round-trip.
    /// `block` carries the same put-notify / blocking-put semantics as
    /// [`Self::put_out_str`].
    pub async fn put_out_field(
        &self,
        link_cfg: &PvaLinkConfig,
        value: &PvField,
        block: bool,
    ) -> PvaLinkResult<()> {
        self.stage_and_flush(
            &link_cfg.field,
            link_cfg.proc,
            link_cfg.defer,
            link_cfg.retry,
            block,
            QueuedPut::Field(value.clone()),
        )
        .await
    }

    /// Core OUT-write path: stage `value` under `field` on the shared
    /// channel scratch, then — unless `defer` — flush the whole scratch
    /// into one combined upstream PUT. Mirrors pvxs `pvaPutValue`
    /// (`pvxs/ioc/pvalink_lset.cpp:645-653`): `put_scratch = value; if(!defer)
    /// lchan->put()`.
    async fn stage_and_flush(
        &self,
        field: &str,
        proc: ProcMode,
        defer: bool,
        retry: bool,
        block: bool,
        value: QueuedPut,
    ) -> PvaLinkResult<()> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        // pvxs `pvaPutValueX` gates EVERY put — deferred or not — on
        // `if(!self->retry && !self->valid()) return -1`
        // (`pvxs/ioc/pvalink_lset.cpp:609`), BEFORE staging into `put_scratch`.
        // A non-retry write to a disconnected channel would fail
        // identically on every replay, so it must not occupy the
        // scratch (nor return Ok on the deferred path). `retry` links
        // skip this gate and queue for replay instead — the disconnect
        // is handled in `flush_scratch`, which restores the staged
        // value and arms the production drain. This `retry` is the
        // PER-CALL link's option (combine-PUT siblings carry their own),
        // and `is_connected()` is the shared channel's connection truth.
        if !retry && !self.is_connected() {
            return Err(PvaLinkError::Disconnected(self.config.pv_name.clone()));
        }
        self.stage_put(
            field,
            StagedPut {
                value,
                proc,
                retry,
                block,
            },
        );
        if defer {
            // Cached for sibling coalescing; a non-deferred sibling — or
            // the `flush_puts` production drain — flushes it later.
            return Ok(());
        }
        self.flush_scratch().await.map(|_| ())
    }

    /// Stage one field's write into the shared scratch, overwriting any
    /// earlier staged write for the same field (most-recent-wins, pvxs
    /// single `put_scratch` per link, `pvxs/ioc/pvalink_lset.cpp:645-653`).
    fn stage_put(&self, field: &str, staged: StagedPut) {
        self.out_scratch.lock().insert(field.to_string(), staged);
    }

    /// Number of distinct fields currently staged on this channel — the
    /// shared `put_scratch` size (0 when nothing is queued).
    pub fn staged_count(&self) -> usize {
        self.out_scratch.lock().len()
    }

    /// Flush every staged field into ONE combined upstream PUT. Mirrors
    /// pvxs `pvaLinkChannel::put` (`pvxs/ioc/pvalink_channel.cpp:220-263`).
    ///
    /// A single staged field keeps the typed path (a typed array PUT
    /// must NOT round-trip through `Display` + parse); multiple fields
    /// are assigned into one prototype via `pvput_fields` so siblings
    /// land in one PUT (`linkBuildPut`, `pvxs/ioc/pvalink_channel.cpp:127-184`).
    /// The process option is resolved across all participating links — PP/CP/CPP
    /// forces processing over NPP (`pvxs/ioc/pvalink_channel.cpp:257-263`).
    ///
    /// On a disconnect, `retry=true` fields are restored to the scratch
    /// (most-recent-wins) and `retry_pending` is set so the production
    /// drain replays them on reconnect; `retry=false` fields are
    /// dropped and surfaced as an error so the owning record alarms.
    /// Returns the number of upstream PUTs issued (0 or 1).
    pub async fn flush_scratch(&self) -> PvaLinkResult<usize> {
        if matches!(self.config.direction, LinkDirection::Inp) {
            return Err(PvaLinkError::NotWritable);
        }
        self.issue_put(false).await
    }

    /// Single owner of "this channel issues one upstream PUT", the port
    /// of pvxs `pvaLinkChannel::put(bool force)`
    /// (`pvxs/ioc/pvalink_channel.cpp:220-280`). `force` is the forward-link
    /// entry: it makes the PUT happen even with nothing staged and
    /// pins `record._options.process` to `"true"` (`:258-260`).
    ///
    /// No direction gate here — pvxs has none on the channel put, and
    /// the scratch is the only way a value enters, so `stage_and_flush`
    /// is where an INP link is refused.
    async fn issue_put(&self, force: bool) -> PvaLinkResult<usize> {
        // Snapshot + clear (pvxs moves used_scratch into put_queue).
        let staged: Vec<(String, StagedPut)> = {
            let mut scratch = self.out_scratch.lock();
            if scratch.is_empty() && !force {
                return Ok(0);
            }
            scratch.drain().collect()
        };
        // `if((reqProcess&2) || force) proc = "true"` — a forced put outranks even
        // a staged NPP sibling (`pvxs/ioc/pvalink_channel.cpp:258-260`).
        let combined_proc = if force {
            ProcMode::Pp
        } else {
            combine_proc(staged.iter().map(|(_, s)| s.proc))
        };
        let block = staged.iter().any(|(_, s)| s.block);
        let req = build_put_request(combined_proc, block);

        let put_result = if staged.is_empty() {
            // Forced with nothing staged: pvxs `linkBuildPut` returns the
            // prototype untouched, so the DATA phase carries an empty
            // changed bitset and no value (`pvxs/ioc/pvalink_channel.cpp:127-184`).
            self.client
                .pvput_empty_with_request(&self.config.pv_name, &req)
                .await
        } else if staged.len() == 1 {
            let (field, sp) = &staged[0];
            self.put_single(field, &sp.value, &req).await
        } else {
            // One combined PUT assigning every staged field into the
            // same prototype (pvxs `linkBuildPut`,
            // pvxs/ioc/pvalink_channel.cpp:127-184). Each leaf travels as typed
            // pvData: a staged `PvField` is assigned into the descriptor
            // leaf verbatim — matching pvxs `value = tosend` for an array
            // leaf — instead of being stringified into a bracketed list the
            // field parser would re-split on commas, which corrupts a typed
            // scalar array. String writes still lower through the CLI parser
            // so the text is coerced against the native scalar type.
            let assignments: Vec<(String, PutLeaf)> = staged
                .iter()
                .map(|(field, sp)| {
                    let leaf = match &sp.value {
                        QueuedPut::Field(f) => PutLeaf::Typed(f.clone()),
                        QueuedPut::Str(s) => PutLeaf::Str(s.clone()),
                    };
                    (put_field_path(field), leaf)
                })
                .collect();
            self.client
                .pvput_fields_typed(&self.config.pv_name, &assignments, Some(&req))
                .await
        };

        match put_result {
            Ok(()) => {
                self.retry_pending.store(false, Ordering::Release);
                Ok(1)
            }
            Err(e) if is_disconnect(&e) => {
                if staged.is_empty() {
                    // A forced put stages nothing, so there is no
                    // retry-eligible field to requeue and the caller owns
                    // reporting the disconnect.
                    return Err(PvaLinkError::Pva(e));
                }
                // Requeue retry-eligible fields for replay; drop the
                // rest. Most-recent-wins: only restore a field a newer
                // write has not already re-staged.
                let mut any_retry = false;
                let mut any_dropped = false;
                {
                    let mut scratch = self.out_scratch.lock();
                    for (field, sp) in staged {
                        if sp.retry {
                            scratch.entry(field).or_insert(sp);
                            any_retry = true;
                        } else {
                            any_dropped = true;
                        }
                    }
                }
                if any_retry {
                    self.retry_pending.store(true, Ordering::Release);
                    // Discard the dead channel so the replay re-resolves a
                    // fresh one. pvxs's channel auto-recovers on reconnect;
                    // our cached channel does not re-create after a failed
                    // `create_channel` (a pinned `EPICS_PVA_ADDR_LIST`
                    // retry link would otherwise NEVER replay — the dead
                    // channel stays cached forever). `Drop` removes it
                    // unconditionally so the next `flush_retry_pending`
                    // opens a new channel against the now-present PV.
                    self.client
                        .cache_clear_action(&self.config.pv_name, CacheAction::Drop)
                        .await;
                }
                if any_dropped {
                    // A non-retry link saw a real disconnect — its
                    // record must alarm. Retry links stay queued.
                    Err(PvaLinkError::Pva(e))
                } else {
                    Ok(0)
                }
            }
            Err(e) => Err(PvaLinkError::Pva(e)),
        }
    }

    /// Replay any retry-queued writes if a prior disconnect left them
    /// staged — the per-channel half of the `LinkSet::flush_puts`
    /// production drain. No-op unless `retry_pending` is set, so a
    /// freshly-`defer`red write awaiting its sibling is never flushed
    /// early; only a genuinely stuck retry write is replayed. Mirrors
    /// pvxs replaying the queued put on the client reconnect event
    /// (`pvalink.rst:99-100`).
    pub async fn flush_retry_pending(&self) -> PvaLinkResult<usize> {
        if !self.retry_pending.load(Ordering::Acquire) {
            return Ok(0);
        }
        self.flush_scratch().await
    }

    /// Issue one single-field PUT, choosing the string vs typed path
    /// and root vs sub-field targeting (pvxs `linkBuildPut:138`:
    /// `top[fieldName]` when `fieldName` is non-empty).
    async fn put_single(
        &self,
        field: &str,
        value: &QueuedPut,
        req: &PvRequestExpr,
    ) -> Result<(), epics_pva_rs::error::PvaError> {
        match value {
            QueuedPut::Str(s) => {
                if is_subfield(field) {
                    self.client
                        .pvput_field_with_request(&self.config.pv_name, field, req, s)
                        .await
                } else {
                    self.client
                        .pvput_with_request(&self.config.pv_name, req, s)
                        .await
                }
            }
            QueuedPut::Field(f) => {
                if is_subfield(field) {
                    self.client
                        .pvput_pv_field_field_with_request(&self.config.pv_name, field, req, f)
                        .await
                } else {
                    self.client
                        .pvput_pv_field_with_request(&self.config.pv_name, req, f)
                        .await
                }
            }
        }
    }

    /// Fire this link's forward link (FLNK): trigger the remote target to
    /// process, transferring no value. Mirrors pvxs `pvaScanForward`
    /// (`pvxs/ioc/pvalink_lset.cpp:672-688`), which is `lchan->put(true)`
    /// at `:691` — the forced arm of the same channel put every OUT write
    /// takes, so it goes through `Self::issue_put` here too.
    ///
    /// It must NOT be a PVA `PROCESS` (cmd 16): pvxs implements no handler
    /// for that command anywhere. `CMD_PROCESS` occurs once in its tree, as
    /// the enum constant at `src/pvaproto.h:632`, and `ConnBase`'s command
    /// switch (`src/conn.cpp:249-276`) drains an unrecognised command's body
    /// at `default:` without replying, so a forward link spelled that way
    /// leaves the remote record unprocessed and blocks to the op timeout.
    ///
    /// Applies pvxs's validity gate verbatim — `if(!self->retry &&
    /// !self->valid())` (`pvxs/ioc/pvalink_lset.cpp:677`), the SAME two-term test
    /// `pvaPutValueX` uses at `:617`. A non-retry link on a disconnected
    /// channel yields `Disconnected` and the caller raises LINK/INVALID,
    /// with no trigger sent and no blocking connect attempted. A `retry`
    /// link skips the gate and issues the put anyway: pvxs reaches
    /// `lchan->put(true)`, whose `doit = force` is unconditionally true
    /// (`pvxs/ioc/pvalink_channel.cpp:226,266`), so the operation is started and the
    /// client holds it until the channel connects. `retry` is the link's
    /// own option here, not a per-call flag, matching `self->retry`.
    pub async fn scan_forward(&self) -> PvaLinkResult<()> {
        if !self.config.retry && !self.is_connected() {
            return Err(PvaLinkError::Disconnected(self.config.pv_name.clone()));
        }
        // "FWD_LINK is never deferred, and always results in a Put"
        // (`pvxs/ioc/pvalink_lset.cpp:682`).
        self.issue_put(true).await.map(|_| ())
    }

    /// True when the link currently has a live upstream connection.
    /// Mirrors pvxs `pvaIsConnected` (pvxs/ioc/pvalink_lset.cpp:186).
    ///
    /// B-pvalink-restart: for INP+monitor links this reads the
    /// monitor task's live-connection flag — it goes `false` the
    /// moment the upstream subscription ends (IOC restart / transient
    /// I/O) and back `true` once the re-subscribe loop delivers a
    /// fresh event. Pre-fix this returned `latest.is_some()`, which
    /// stayed `true` forever once any value had been cached, so an
    /// IOC restart was never reflected. For non-monitor links (which
    /// never run the monitor task) it falls back to "a value has been
    /// cached".
    pub fn is_connected(&self) -> bool {
        match &self.monitor_connected {
            Some(flag) => flag.load(Ordering::Acquire),
            None => self.latest.lock().is_some(),
        }
    }

    /// Raw remote NT `alarm.severity` of the latest cached value, read
    /// relative to the link's selected field root, in EPICS severity
    /// numbering (`0 = NO_ALARM` … `3 = INVALID`). `None` when no value
    /// is cached or the selected root carries no alarm sub-field.
    ///
    /// `field` selects the metadata root exactly as pvxs does: a
    /// non-empty `fieldName` rebinds `root = lchan->root[fieldName]` and
    /// `fld_severity` is then resolved relative to that selected root
    /// (`pvxs/ioc/pvalink_link.cpp:90-110`,
    /// `pvxs/ioc/pvalink_lset.cpp:412-430`). `field=""` keeps the
    /// top-level root.
    fn remote_alarm_severity(&self, field: &str) -> Option<i32> {
        let v = self.latest.lock().clone()?;
        let PvField::Structure(s) = select_target(&v, field) else {
            return None;
        };
        let PvField::Structure(a) = s.get_field("alarm")? else {
            return None;
        };
        match a.get_field("severity")? {
            PvField::Scalar(sv) => Some(scalar_value_to_f64(sv) as i32),
            _ => None,
        }
    }

    /// Pure extraction of the remote `(severity, message)` from an NT
    /// value at the selected field root, with pvxs `pvaGetValue` snapshot
    /// semantics (`pvxs/ioc/pvalink_lset.cpp:412-422`): the severity
    /// defaults to `NO_ALARM` when the alarm sub-field is absent, and the
    /// message is the remote `alarm.message` — blank unless the severity
    /// is non-`NO_ALARM` (pvxs clears `snap_message` when
    /// `snap_severity == 0`). Pure so the value-read latch and any caller
    /// share one extraction rule.
    fn alarm_from_value(value: &PvField, field: &str) -> (i32, String) {
        let PvField::Structure(s) = select_target(value, field) else {
            // A cached value that is not a structure carries no alarm
            // sub-field — `NO_ALARM`, no message (pvxs default snapshot).
            return (0, String::new());
        };
        let alarm = match s.get_field("alarm") {
            Some(PvField::Structure(a)) => Some(a),
            _ => None,
        };
        let severity = alarm
            .and_then(|a| a.get_field("severity"))
            .and_then(|sv| match sv {
                PvField::Scalar(sv) => Some(scalar_value_to_f64(sv) as i32),
                _ => None,
            })
            .unwrap_or(0);
        // pvxs only latches the message when `snap_severity != 0`
        // (`pvxs/ioc/pvalink_lset.cpp:418-421`); a NO_ALARM snapshot has none.
        let message = if severity != 0 {
            alarm
                .and_then(|a| a.get_field("message"))
                .and_then(|m| match m {
                    PvField::Scalar(ScalarValue::String(m)) if !m.is_empty() => Some(m.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        (severity, message)
    }

    /// Latch the remote alarm snapshot for `field` from the value being
    /// read. The Rust counterpart of pvxs `pvaGetValue` copying the live
    /// `fld_severity` / `fld_message` into `snap_severity` /
    /// `snap_message` (`pvxs/ioc/pvalink_lset.cpp:412-422`): the ungated
    /// DB-link alarm inspection ([`Self::remote_alarm_snapshot`]) reports
    /// THIS latched snapshot, not the live cached monitor alarm — so a
    /// connected-but-never-read link reports the initial INVALID/blank
    /// exactly like pvxs `dbGetAlarm` before the first `dbGetLink`
    /// (`pvxs/test/testpvalink.cpp:373-375`). Called from the value-read
    /// path ([`Self::read_with_field`] / [`Self::try_read_cached_with_field`])
    /// on a connected read. Reuses the existing key allocation in steady
    /// state so a hot CP/CPP read does not allocate per process.
    fn latch_alarm_snapshot(&self, field: &str, value: &PvField) {
        let snap = Self::alarm_from_value(value, field);
        let mut g = self.snap_alarm.lock();
        match g.get_mut(field) {
            Some(slot) => *slot = snap,
            None => {
                g.insert(field.to_string(), snap);
            }
        }
    }

    /// Ungated remote alarm snapshot — the remote `(severity, status,
    /// message)` LATCHED at the last successful value read, WITHOUT the
    /// maximize-severity (`MS`/`NMS`/`MSI`) gate that
    /// [`Self::link_alarm_severity_with`] applies for owning-record
    /// propagation.
    ///
    /// The Rust counterpart of pvxs `pvaGetAlarmMsg`
    /// (`pvxs/ioc/pvalink_lset.cpp:542-569`): it reads the cached
    /// `snap_severity` / `snap_message` directly and never consults the
    /// link's `sevr` mode, so a default `NMS` link still reports its
    /// remote severity here even though it leaves the owning record
    /// unraised. The status is derived from severity by
    /// [`epics_base_rs::server::database::RemoteAlarm::from_severity_message`].
    ///
    /// Crucially it reports the value-read-LATCHED snapshot
    /// (`Self::latch_alarm_snapshot`), not the live cached monitor
    /// alarm. pvxs `snap_severity` is initialized to `INVALID_ALARM`
    /// with a blank message (`pvxs/ioc/pvalink.h:250`) and only
    /// `pvaGetValue` updates it; so a connected link that has never been
    /// read reports `INVALID_ALARM`/`LINK_ALARM`/blank here, exactly like
    /// pvxs `dbGetAlarm` before the first `dbGetLink`
    /// (`pvxs/test/testpvalink.cpp:373-375`).
    ///
    /// `CHECK_VALID` is honoured (`pvxs/ioc/pvalink_lset.cpp:548`): a
    /// disconnected-stale monitor link serves no snapshot even though
    /// its last value is retained, and a link with no cached value
    /// yields `None`.
    pub fn remote_alarm_snapshot(
        &self,
        field: &str,
    ) -> Option<epics_base_rs::server::database::RemoteAlarm> {
        use epics_base_rs::server::database::RemoteAlarm;
        // pvxs initial `snap_severity` (`pvxs/ioc/pvalink.h:250`): a
        // connected-but-never-read link reports INVALID with a blank msg.
        const INVALID_ALARM: i32 = 3;
        if self.monitor_disconnected_stale() || self.latest.lock().is_none() {
            return None;
        }
        let (severity, message) = self
            .snap_alarm
            .lock()
            .get(field)
            .cloned()
            .unwrap_or((INVALID_ALARM, String::new()));
        Some(RemoteAlarm::from_severity_message(severity, message))
    }

    /// true iff this monitor link previously delivered a
    /// value but its subscription is now down — the cached `latest` is
    /// stale. This is the precise "disconnected after connect" state
    /// that must contribute LINK_ALARM/INVALID through the alarm hooks.
    ///
    /// A link that NEVER connected (no cached value) is deliberately
    /// excluded here: the value path (`get_value` → `None` → base
    /// LINK_ALARM/INVALID, gated by the parsed link type) owns that
    /// case. Excluding it also keeps the base bare-name alarm loop
    /// (`external_link_alarm`, which tries every registered lset) from
    /// mis-claiming a name a sibling lset actually owns — a PVA lset
    /// asked about a `ca://` name lazily opens a never-connecting PVA
    /// link, and must report `None` (not INVALID) so the loop falls
    /// through to the CA lset. Mirrors pvxs `pvaLink::valid()` going
    /// false on disconnect while the snapshot is retained
    /// (`pvxs/ioc/pvalink_channel.cpp:370-373`; the retention is pvxs's own comment
    /// at `:375-376`).
    fn monitor_disconnected_stale(&self) -> bool {
        self.config.monitor && !self.is_connected() && self.latest.lock().is_some()
    }

    /// Severity to fold into the owning record's `LINK_ALARM`, after
    /// applying the link's `MS`/`NMS`/`MSI` maximize-severity mode
    /// (B2). Returns `None` when no alarm should propagate — i.e.
    /// `NMS`, or the remote severity does not meet the mode's
    /// threshold, or no value is cached yet.
    ///
    /// Mirrors `pvxs/ioc/pvalink_lset.cpp:424-430` — the `recGblSetSevrMsg`
    /// gate that propagates `snap_severity` into `LINK_ALARM` only
    /// when `(sevr==MS && sev!=NO_ALARM) || (sevr==MSI && sev==INVALID)`.
    pub fn link_alarm_severity(&self) -> Option<i32> {
        self.link_alarm_severity_with(&self.config.field, self.config.sevr)
    }

    /// Like [`Self::link_alarm_severity`] but gates on a
    /// caller-supplied `sevr` mode instead of this link's
    /// `self.config.sevr`.
    ///
    /// the registry can return a cached INP `PvaLink` whose
    /// `config.sevr` belongs to whichever caller opened it first.
    /// Two INP links to the same remote PV with different `sevr`
    /// options would otherwise share one link's `MS`/`NMS`/`MSI`
    /// gate. The resolver passes the caller's own parsed `sevr` so
    /// each link applies its own maximize-severity mode (pvxs
    /// `pvaLinkConfig::sevr` is per-link, `pvxs/ioc/pvalink.h:65`).
    pub fn link_alarm_severity_with(
        &self,
        field: &str,
        sevr: super::config::SevrMode,
    ) -> Option<i32> {
        // a disconnected monitor link is INVALID
        // regardless of the MS/NMS/MSI gate — a broken link is a local
        // failure, not a remote-severity propagation that NMS would
        // suppress. pvxs `pvaGetValue` sets LINK_ALARM/INVALID_ALARM
        // unconditionally while `!valid()` (pvxs/ioc/pvalink_lset.cpp:259-272).
        // Routing INVALID(3) through this existing `alarm_severity`
        // hook means consumers reading the alarm path — not just the
        // value path — also observe the disconnect, via the same
        // single source of truth (`is_connected`).
        if self.monitor_disconnected_stale() {
            return Some(3); // INVALID
        }
        let sev = self.remote_alarm_severity(field)?;
        if sevr.propagates(sev) {
            Some(sev)
        } else {
            None
        }
    }

    /// Best-effort alarm message for the linked PV.
    ///
    /// B2: the message is gated by the link's maximize-severity mode
    /// (`MS`/`NMS`/`MSI`). It returns `Some(..)` only when the remote
    /// severity actually propagates per [`Self::link_alarm_severity`] —
    /// the database consults this hook to decide whether to raise
    /// `LINK_ALARM` on the owning record, so an `NMS` link (the
    /// default) must report no alarm even when the remote PV is in
    /// alarm. Mirrors pvxs `pvaGetAlarmMsg` (pvxs/ioc/pvalink_lset.cpp:542),
    /// which reads the same `snap_*` slots that the `MS`/`MSI` gate
    /// at `pvxs/ioc/pvalink_lset.cpp:412-416` populates.
    ///
    /// When the remote NT structure has no `alarm.message` string but
    /// the severity does propagate, a synthetic message is returned so
    /// the alarm is still observable.
    pub fn alarm_message(&self) -> Option<String> {
        self.alarm_message_with(&self.config.field, self.config.sevr)
    }

    /// Like [`Self::alarm_message`] but gates on a caller-supplied
    /// `sevr` mode instead of this link's `self.config.sevr`.
    ///
    /// same rationale as [`Self::link_alarm_severity_with`] —
    /// the alarm-message gate must use the caller's per-link `sevr`,
    /// not whichever cached link's mode happens to be shared.
    pub fn alarm_message_with(&self, field: &str, sevr: super::config::SevrMode) -> Option<String> {
        // disconnect dominates — report the link failure,
        // not the stale remote alarm message. Pairs with the INVALID
        // severity `link_alarm_severity_with` returns for the same
        // state, so the owning record's LINK_ALARM carries a disconnect
        // message rather than a misleading snapshot of the last remote
        // alarm string.
        if self.monitor_disconnected_stale() {
            return Some("pvalink monitor disconnected".to_string());
        }
        // Severity gate first — NMS / sub-threshold links report
        // nothing.
        let sev = self.link_alarm_severity_with(field, sevr)?;
        let v = self.latest.lock().clone()?;
        // Resolve the alarm string relative to the link's selected
        // field root, matching `remote_alarm_severity` — pvxs reads
        // `fld_message` from the same selected `root[fieldName]`
        // (`pvxs/ioc/pvalink_link.cpp:90-110`,
        // `pvxs/ioc/pvalink_lset.cpp:412-430`).
        let PvField::Structure(s) = select_target(&v, field) else {
            return None;
        };
        let msg = s.get_field("alarm").and_then(|alarm| {
            let PvField::Structure(a) = alarm else {
                return None;
            };
            match a.get_field("message") {
                Some(PvField::Scalar(ScalarValue::String(m))) if !m.is_empty() => {
                    Some(m.to_string())
                }
                _ => None,
            }
        });
        Some(msg.unwrap_or_else(|| format!("remote severity {sev}")))
    }

    /// Latest cached NT value, if any. Returned as the raw [`PvField`]
    /// so callers can pull whichever sub-field they need (alarm,
    /// timeStamp, value, etc.). pvxs `pvaGetTimeStampTag`
    /// (pvxs/ioc/pvalink_lset.cpp:577) lives on top of this.
    pub fn latest_value(&self) -> Option<PvField> {
        self.latest.lock().clone()
    }

    /// Timestamp the owning record adopts on a `time=true` read,
    /// `(seconds, nanoseconds, userTag)`. This is the value-read
    /// time-adoption hook (consumed by the database INP read path), the
    /// Rust counterpart of pvxs `pvaGetValue` setting `precord->time`:
    ///
    /// * **Connected** — the remote NT `timeStamp` of the cached value,
    ///   resolved at the link's selected field root
    ///   (`pvxs/ioc/pvalink_lset.cpp:394-409`, the connected-read snapshot).
    /// * **Disconnected-stale** — the *disconnect-event* time captured
    ///   when the subscription dropped (pvxs `snap_time = e.time`,
    ///   `pvxs/ioc/pvalink_channel.cpp:372`; adopted on the invalid read at
    ///   `pvxs/ioc/pvalink_lset.cpp:268-270`). The stale last-value timestamp is
    ///   NOT served — the record's time means "upstream link
    ///   disconnected at this moment", not "local process ran" and not
    ///   "the last value's time". The last value's `userTag` is kept
    ///   alongside (pvxs leaves `snap_tag` unchanged on disconnect).
    ///   When no disconnect was ever recorded, `None` (keep local time).
    ///
    /// `None` otherwise (no cached value / no timeStamp slot). The
    /// metadata getter `pvaGetTimeStampTag` stays gated through
    /// `CHECK_VALID` via `Self::monitor_disconnected_stale` in
    /// [`Self::link_metadata`] — only this value-read hook adopts the
    /// disconnect time.
    pub fn time_stamp(&self, field: &str) -> Option<(i64, i32, u64)> {
        if self.monitor_disconnected_stale() {
            // Adopt the recorded disconnect-event time; keep the last
            // value's userTag (read from the retained snapshot, 0 when
            // absent). Without a recorded disconnect, keep local time.
            let (secs, nsec) = (*self.disconnect_time.lock())?;
            let utag = self.cached_timestamp(field).map_or(0, |(_, _, u)| u);
            return Some((secs, nsec, utag));
        }
        self.cached_timestamp(field)
    }

    /// `(seconds, nanoseconds, userTag)` from the cached NT value's
    /// `timeStamp` slot, resolved at the selected field root. The raw
    /// connected-read snapshot used by [`Self::time_stamp`]; reads the
    /// retained value regardless of connection state (the caller decides
    /// whether to serve it).
    fn cached_timestamp(&self, field: &str) -> Option<(i64, i32, u64)> {
        let v = self.latest.lock().clone()?;
        // Resolve `timeStamp` relative to the selected field root —
        // pvxs reads `fld_seconds`/`fld_nanoseconds`/`fld_usertag` from
        // the same `root[fieldName]` selected by `onTypeChange`
        // (`pvxs/ioc/pvalink_link.cpp:90-110`,
        // `pvxs/ioc/pvalink_lset.cpp:399-409`).
        let PvField::Structure(s) = select_target(&v, field) else {
            return None;
        };
        let ts = s.get_field("timeStamp")?;
        let PvField::Structure(t) = ts else {
            return None;
        };
        let secs = match t.get_field("secondsPastEpoch")? {
            PvField::Scalar(ScalarValue::Long(v)) => *v,
            PvField::Scalar(ScalarValue::ULong(v)) => *v as i64,
            _ => return None,
        };
        let nsec = match t.get_field("nanoseconds")? {
            PvField::Scalar(ScalarValue::Int(v)) => *v,
            PvField::Scalar(ScalarValue::UInt(v)) => *v as i32,
            _ => return None,
        };
        // `pvxs/ioc/pvalink_lset.cpp:406-409` reads `timeStamp.userTag` into
        // the snapshot tag, falling back to 0 when the field is absent
        // (`if(self->fld_usertag) snap_tag = ...; else snap_tag = 0;`).
        // The wire field is a signed int32; zero-extend (`as u32 as u64`)
        // rather than sign-extend so a bit-31 tag like 0x9000_0000 widens
        // to 0x0000_0000_9000_0000, not 0xFFFF_FFFF_9000_0000 (the
        // pvData `as<uint32_t>()` reinterpret, not `as<epicsUTag>()`).
        let utag = match t.get_field("userTag") {
            Some(PvField::Scalar(ScalarValue::Int(v))) => *v as u32 as u64,
            Some(PvField::Scalar(ScalarValue::UInt(v))) => *v as u64,
            _ => 0,
        };
        Some((secs, nsec, utag))
    }

    /// Remote display / control / valueAlarm metadata snapshot for
    /// this link's cached NT value.
    ///
    /// Mirrors the pvxs pvalink lset metadata getters
    /// (`pvxs/ioc/pvalink_lset.cpp:199`–`:540`):
    ///
    /// * `dbf_type` / `element_count` derive from the value at the
    ///   link's field path (`config.field`) — `pvaGetDBFtype` reads
    ///   `fld_value.type()`, `pvaGetElements` reads its array length
    ///   or `1` for a scalar.
    /// * `graphic_limits` / `control_limits` / `alarm_limits` /
    ///   `precision` / `units` / `description` are read from the
    ///   *top-level* NT `display` / `control` / `valueAlarm`
    ///   sub-structures — `pvaGetGraphicLimits` &c. read
    ///   `fld_meta["display.limitLow"]`, etc.
    ///
    /// Returns `None` when no value is cached (link not connected).
    /// Each field is `None` when the remote NT value did not carry
    /// that metadata — the caller then keeps its local default,
    /// exactly as the C getters leave the buffer untouched on a
    /// missing `Value::as`.
    pub fn link_metadata(&self) -> Option<epics_base_rs::server::database::LinkMetadata> {
        self.link_metadata_with(&self.config.field)
    }

    /// Like [`Self::link_metadata`] but derives DBF type and element
    /// count from a caller-supplied `field` path instead of this
    /// link's `self.config.field`.
    ///
    /// the registry can return a cached INP `PvaLink` whose
    /// `config.field` belongs to whichever caller opened it first, so
    /// two INP links to the same remote PV with different `field`
    /// options would otherwise report the same DBF type / element
    /// count. The resolver passes the caller's own parsed `field`
    /// (pvxs `pvaGetDBFtype` reads the per-link `fld_value`,
    /// `pvxs/ioc/pvalink_lset.cpp:199`).
    pub fn link_metadata_with(
        &self,
        field: &str,
    ) -> Option<epics_base_rs::server::database::LinkMetadata> {
        use epics_base_rs::server::database::LinkMetadata;

        // same gate as the value/timestamp getters — a
        // disconnected monitor link must not surface its stale latched
        // display/control/valueAlarm metadata, DBF type or element
        // count. pvxs's `CHECK_VALID` (`valid() = connected && root`)
        // makes every metadata getter (`pvaGetGraphicLimits`,
        // `pvaGetDBFtype`, …) a no-op while disconnected even though
        // the NT snapshot is retained.
        if self.monitor_disconnected_stale() {
            return None;
        }

        let root = self.latest.lock().clone()?;

        // DBF type and element count derive from the selected value.
        // pvxs `pvaGetDBFtype` reads `fld_value`, which is the
        // selected-root rule: empty `field` selects the top-level value
        // (its `.value` for a structure, else the root itself), a
        // non-empty `field` selects `root[field]` (then `.value` if that
        // is itself a structure). See [`select_link_value`]
        // (`pvxs/ioc/pvalink_link.cpp:90-110`, `pvxs/ioc/pvalink_lset.cpp:199-240`).
        let value_field = select_link_value(&root, field);

        let dbf_type = link_dbf_type(&value_field);
        let element_count = link_element_count(&value_field);

        // display / control / valueAlarm are read from the selected
        // metadata root, NOT the top-level root. pvxs derives `fld_meta`
        // from the same selected root as `fld_value`: when `fieldName`
        // is non-empty, `root = lchan->root[fieldName]` and every
        // `fld_meta["display.*"]` / `["control.*"]` / `["valueAlarm.*"]`
        // is resolved relative to that selected root
        // (`pvxs/ioc/pvalink_link.cpp:90-110`,
        // `pvxs/ioc/pvalink_lset.cpp:444-540`). A link selecting a
        // nested member must expose that member's engineering units,
        // precision and graphic/control/alarm limits — not the
        // container's. A scalar/array field selection has no nested
        // metadata sub-structures, so its metadata root is empty.
        let meta_root = match select_target(&root, field) {
            s @ PvField::Structure(_) => s,
            _ => PvField::Null,
        };
        let graphic_limits = limit_pair(&meta_root, "display.limitLow", "display.limitHigh");
        let control_limits = limit_pair(&meta_root, "control.limitLow", "control.limitHigh");
        let alarm_limits = {
            let lolo = scalar_as_f64(&extract_field(&meta_root, "valueAlarm.lowAlarmLimit"));
            let lo = scalar_as_f64(&extract_field(&meta_root, "valueAlarm.lowWarningLimit"));
            let hi = scalar_as_f64(&extract_field(&meta_root, "valueAlarm.highWarningLimit"));
            let hihi = scalar_as_f64(&extract_field(&meta_root, "valueAlarm.highAlarmLimit"));
            match (lolo, lo, hi, hihi) {
                // pvxs writes each of the four buffers independently
                // and leaves a missing one untouched; we only surface
                // the alarm-limit set when at least one is present,
                // defaulting the absent ones to 0.0 to mirror a
                // record's zero-initialised limit fields.
                (None, None, None, None) => None,
                (a, b, c, d) => Some((
                    a.unwrap_or(0.0),
                    b.unwrap_or(0.0),
                    c.unwrap_or(0.0),
                    d.unwrap_or(0.0),
                )),
            }
        };
        let precision =
            scalar_as_f64(&extract_field(&meta_root, "display.precision")).map(|p| p as i16);
        let units = string_field(&meta_root, "display.units");
        let description = string_field(&meta_root, "display.description");

        Some(LinkMetadata {
            dbf_type,
            element_count,
            graphic_limits,
            control_limits,
            alarm_limits,
            precision,
            units,
            description,
            // A pva link's cached value carries its own labels
            // (`EnumWithChoices`, the NTEnum read path), so the
            // metadata-side table the CA lset needs is redundant here.
            enum_choices: None,
        })
    }

    /// Test-only constructor: build a [`PvaLink`] with a pre-seeded
    /// cached value and no live connection. Lets the unit tests
    /// exercise the cache-reading accessors (`link_alarm_severity`,
    /// `alarm_message`, `try_read_cached`) and the defer queue
    /// without standing up a PVA server.
    #[cfg(test)]
    pub(crate) fn for_test(config: PvaLinkConfig, cached: Option<PvField>) -> Self {
        let client = PvaClient::builder().timeout(Duration::from_secs(1)).build();
        Self {
            _monitor_abort: None,
            config,
            client,
            latest: Arc::new(Mutex::new(cached)),
            monitor_connected: None,
            notify_rx: Mutex::new(None),
            out_scratch: Mutex::new(HashMap::new()),
            snap_alarm: Mutex::new(HashMap::new()),
            retry_pending: AtomicBool::new(false),
            disconnect_time: Arc::new(Mutex::new(None)),
            scan_overrun: Arc::new(ScanOverrun::default()),
        }
    }

    /// Test-only constructor: build a [`PvaLink`] around a
    /// caller-supplied [`PvaClient`] (typically one pinned at a test
    /// `PvaServer` address). Lets a server-backed test exercise the
    /// real `write` / `write_pv_field` wire paths without UDP
    /// discovery.
    ///
    /// The link is marked CONNECTED (`monitor_connected = Some(true)`):
    /// a link built around a live, server-pinned client stands in for
    /// one whose channel monitor has already delivered its first event,
    /// so the OUT-write disconnect gate (`stage_and_flush`) lets non-retry
    /// writes through to the real wire path. A test wanting the
    /// disconnected case uses [`Self::for_test`] (no client) or
    /// [`Self::for_test_with_monitor_flag`] with the flag left false.
    #[cfg(test)]
    pub(crate) fn for_test_with_client(config: PvaLinkConfig, client: PvaClient) -> Self {
        Self {
            _monitor_abort: None,
            config,
            client,
            latest: Arc::new(Mutex::new(None)),
            monitor_connected: Some(Arc::new(AtomicBool::new(true))),
            notify_rx: Mutex::new(None),
            out_scratch: Mutex::new(HashMap::new()),
            snap_alarm: Mutex::new(HashMap::new()),
            retry_pending: AtomicBool::new(false),
            disconnect_time: Arc::new(Mutex::new(None)),
            scan_overrun: Arc::new(ScanOverrun::default()),
        }
    }

    /// Test-only constructor for a link whose live-connection flag is
    /// externally controllable (INP+monitor or OUT). Returns the link
    /// plus the shared `AtomicBool` so a test can simulate the
    /// re-subscribe loop's connect / event / disconnect transitions
    /// (B-pvalink-restart) without standing up a PVA server. For an OUT
    /// link, setting the flag `true` stands in for the channel monitor
    /// having connected, so the `stage_and_flush` disconnect gate lets
    /// non-retry writes stage (offline staging/coalescing tests).
    #[cfg(test)]
    pub(crate) fn for_test_with_monitor_flag(
        config: PvaLinkConfig,
        cached: Option<PvField>,
    ) -> (Self, Arc<AtomicBool>) {
        let client = PvaClient::builder().timeout(Duration::from_secs(1)).build();
        let flag = Arc::new(AtomicBool::new(false));
        let link = Self {
            _monitor_abort: None,
            config,
            client,
            latest: Arc::new(Mutex::new(cached)),
            monitor_connected: Some(flag.clone()),
            notify_rx: Mutex::new(None),
            out_scratch: Mutex::new(HashMap::new()),
            snap_alarm: Mutex::new(HashMap::new()),
            retry_pending: AtomicBool::new(false),
            disconnect_time: Arc::new(Mutex::new(None)),
            scan_overrun: Arc::new(ScanOverrun::default()),
        };
        (link, flag)
    }

    /// Test-only: stamp the disconnect-event time the monitor task
    /// would record on a live circuit drop, so a disconnect-stale
    /// `time_stamp` read can be exercised without a real subscription
    /// transition.
    #[cfg(test)]
    pub(crate) fn record_disconnect_time_for_test(&self, secs: i64, nsec: i32) {
        *self.disconnect_time.lock() = Some((secs, nsec));
    }
}

/// True iff a [`epics_pva_rs::PvaError`] indicates the upstream is currently
/// unreachable (as opposed to a value-level rejection). Used to
/// decide whether a `retry` link should queue the Put (B4).
///
/// pvxs gates `retry` on `!pvaLink::valid()` — "the channel is not
/// connected" — so the classification here mirrors that: I/O errors,
/// timeouts, refused connections, an unresolved channel, and the
/// search-failure (`no servers found`) case all mean "not connected
/// yet", and a `retry` link queues the Put for replay on connect. A
/// genuine value rejection (`InvalidValue`, `Decode`) is not a
/// disconnect — retrying it would fail identically.
fn is_disconnect(e: &epics_pva_rs::error::PvaError) -> bool {
    use epics_pva_rs::error::PvaError;
    match e {
        PvaError::Io(_)
        | PvaError::Timeout
        | PvaError::ChannelNotFound(_)
        | PvaError::ConnectionRefused
        // The explicit disconnect variant: the virtual circuit dropped
        // after connecting, so a `retry` link queues the Put for replay.
        | PvaError::Disconnected
        // This side could not allocate for an inbound message and shed the
        // circuit rather than aborting the IOC (`epics-pva-rs` peer_buf).
        // The channel is down afterwards exactly as if the peer had closed
        // it, so it classifies with the transport failures, not with the
        // value rejections — a `retry` link queues for replay on reconnect.
        | PvaError::ResourceExhausted(_) => true,
        // The client reports a failed name search as a Protocol
        // error ("no servers found for PV ..."); that is a
        // not-connected condition, not a protocol violation. A
        // create_channel rejection ("create_channel(X) failed: unknown
        // PV") is the same "unresolved channel" condition expressed by
        // a pinned server that lacks the PV — the channel could not be
        // established, so a `retry` link queues for replay once the PV
        // appears (the UDP-search form, "no servers found", already
        // maps here). Mirrors `ChannelNotFound` above.
        PvaError::Protocol(msg) => {
            let m = msg.to_ascii_lowercase();
            m.contains("no servers found")
                || m.contains("not connected")
                || m.contains("disconnect")
                || m.contains("create_channel")
                || m.contains("unknown pv")
        }
        PvaError::InvalidValue(_) | PvaError::Decode(_) => false,
        // an interrupted *wait* is not a disconnect. The
        // channel stays connected and the underlying Put keeps running,
        // recoverable by a later `wait` (`epics-pva-rs` error.rs:11-16) —
        // it is neither a "not connected yet" condition nor a value
        // rejection. A `retry` link must NOT queue it: the original Put
        // is still in flight, so replaying on the next connect would
        // duplicate the write.
        PvaError::Interrupted => false,
        // `RemoteError` is a server-side rejection on a *connected*
        // channel (not transport-down); `Finished` / `Connected` are
        // lifecycle sentinels, never a "not connected yet" condition. A
        // `retry` link must not queue on any of them — the server has
        // already answered, or the channel is up.
        PvaError::RemoteError(_) | PvaError::Finished | PvaError::Connected => false,
    }
}

/// Build the pvRequest for an OUT PUT operation from the link's
/// [`ProcMode`] and the caller's `block` argument. Mirrors pvxs
/// `pvxs/ioc/pvalink_channel.cpp:28-47` (putReq template) + `220-263` (runtime
/// process/block computation):
///   - `record._options.process` ← [`ProcMode::put_process_request`]
///     (`Default → "passive"`, `Npp → "false"`, `Pp`/`Cp`/`Cpp → "true"`)
///   - `block`                   → `"true"` / `"false"` (pvxs `wait`)
///
/// the process value is the three-way wire string the
/// `ProcMode` enum derives, not a two-way bool — `Default` and `Npp`
/// were previously indistinguishable (`"passive"` for both), and
/// `Cp`/`Cpp` could not request remote processing on PUT.
fn build_put_request(proc: ProcMode, block: bool) -> PvRequestExpr {
    PvRequestExpr {
        fields: vec![],
        // `pvxs/ioc/pvalink_channel.cpp:35-36`: `String("process")` carries
        // the "true"/"false"/"passive" word; `Bool("block")` is a typed
        // boolean. Match those wire types exactly.
        record_options: vec![
            (
                "process".to_string(),
                ScalarValue::String(proc.put_process_request().into()),
            ),
            ("block".to_string(), ScalarValue::Boolean(block)),
        ],
        field_options: vec![],
    }
}

/// True iff `field` names a non-default sub-field (not `""` or `"value"`).
/// When true, PUT must use `pvput_field_with_request` to target that
/// specific field in the DATA phase. Mirrors pvxs `linkBuildPut:138`:
/// `top[fieldName]` when `fieldName` is non-empty.
fn is_subfield(field: &str) -> bool {
    !field.is_empty() && field != "value"
}

/// Resolve the combined `record._options.process` mode across all
/// fields participating in one PUT. Mirrors pvxs
/// `pvxs/ioc/pvalink_channel.cpp:257-263`: PP/CP/CPP force processing
/// (`"true"`) over NPP (`"false"`); a bare `Default` leaves the remote
/// default (`"passive"`). PP wins when both PP and NPP are present.
fn combine_proc(modes: impl Iterator<Item = ProcMode>) -> ProcMode {
    let mut any_process = false;
    let mut any_npp = false;
    for m in modes {
        match m {
            ProcMode::Pp | ProcMode::Cp | ProcMode::Cpp => any_process = true,
            ProcMode::Npp => any_npp = true,
            ProcMode::Default => {}
        }
    }
    if any_process {
        ProcMode::Pp
    } else if any_npp {
        ProcMode::Npp
    } else {
        ProcMode::Default
    }
}

/// Render a queued OUT value to its PUT string form for a combined
/// multi-field PUT. A typed `PvField` is rendered via `Display`: the
/// combined `pvput_fields` path is the documented scalar-field
/// coalescing case (`pvalink.rst:111-113`), while a lone typed array
/// stays on the single-field typed path (see [`PvaLink::flush_scratch`]).
/// The dotted field path for a staged write's `pvput_fields_typed`
/// assignment: a root write (`""`) targets `value`.
fn put_field_path(field: &str) -> String {
    if field.is_empty() {
        "value".to_string()
    } else {
        field.to_string()
    }
}

/// build the pvRequest for a pvalink monitor channel (INP value monitor
/// or OUT connection-tracking monitor — pvxs uses the same
/// `makeRequest()` output for both).
///
/// pvxs `pvaLink::makeRequest` (`pvxs/ioc/pvalink_link.cpp:49-65`) ALWAYS
/// emits three fields on every pvalink monitor request:
///
///   - `record._options.pipeline`  — boolean, honors `cfg.pipeline`
///   - `record._options.atomic`    — hard-coded `true` (forces the
///     remote QSRV/group to assemble atomic snapshots even when the
///     local pvalink isn't part of an atomic scan batch — these are
///     related but distinct concepts).
///   - `record._options.queueSize` — int, defaults to 4 even when
///     no other option requires negotiation.
///
/// The earlier Rust path returned `None` for the default monitor,
/// so a no-options INP link sent no pvRequest and the remote server
/// fell back to its own defaults — including possibly non-atomic
/// snapshots and a different queue depth. Match pvxs by always
/// returning a request with all three fields populated.
fn monitor_request(config: &PvaLinkConfig) -> Option<epics_pva_rs::pv_request::PvRequestExpr> {
    let mut req = epics_pva_rs::pv_request::PvRequestExpr::default();
    // `pvxs/ioc/pvalink_link.cpp:56-65 makeRequest`: `Bool("pipeline")`,
    // `Bool("atomic")`, `UInt32("queueSize")` — typed, not strings.
    req.record_options.push((
        "pipeline".to_string(),
        ScalarValue::Boolean(config.pipeline),
    ));
    // `pvxs/ioc/pvalink_link.cpp:64`: forced true on the remote request,
    // independent of `cfg.atomic` (the local scan-batch flag).
    req.record_options
        .push(("atomic".to_string(), ScalarValue::Boolean(true)));
    req.record_options.push((
        "queueSize".to_string(),
        ScalarValue::UInt(u32::try_from(config.queue_size.max(1)).unwrap_or(u32::MAX)),
    ));
    Some(req)
}

/// Select the link's *target* from the remote root, following pvxs's
/// `pvaLink::onTypeChange()` rule (`pvxs/ioc/pvalink_link.cpp:90-110`):
/// an empty `field` selects the top-level root (`lchan->root`); a
/// non-empty `field` selects `root[field]` (a dotted path navigates
/// through nested structures). The target is the basis both for the
/// value (see [`select_link_value`]) and for the display / control /
/// valueAlarm metadata.
fn select_target(root: &PvField, field: &str) -> PvField {
    if field.is_empty() {
        root.clone()
    } else {
        extract_field(root, field)
    }
}

/// Select the link's *value* from the remote root, following pvxs's
/// `pvaGetDBFtype` / `onTypeChange` rule
/// (`pvxs/ioc/pvalink_lset.cpp:199-240`, `pvxs/ioc/pvalink_link.cpp:90-110`):
/// after [`select_target`] picks the target, if that target is itself
/// a structure (an NTScalar/NTScalarArray) its `.value` child is the
/// value; otherwise the selected target *is* the value (a top-level
/// or sub-field scalar/array).
///
/// For the default empty `field` on an NTScalar this yields `root.value`
/// — identical to the former hard-coded `field="value"`; the new
/// behavior is that a top-level non-structure value (a bare scalar/array
/// PV) is selected directly instead of being searched for a non-existent
/// `value` child, and that an explicitly selected sub-structure drills
/// into its own `.value`.
fn select_link_value(root: &PvField, field: &str) -> PvField {
    // A selected union / non-empty variant resolves to its concrete
    // member before the value is read — pvxs `value.lookup("->")`
    // (`pvxs/ioc/pvalink_lset.cpp:278-279`); the NTNDArray `value` member is a
    // discriminated union. `deref_selected` is idempotent on plain
    // structures/scalars, so the NTScalar `.value` path is unchanged.
    match select_target(root, field).deref_selected() {
        PvField::Structure(s) => s
            .get_field("value")
            .map(|v| v.deref_selected().clone())
            .unwrap_or(PvField::Null),
        other => other.clone(),
    }
}

/// Walk a dotted field path through a [`PvField`] and return the leaf value.
///
/// A selected union / non-empty variant is dereferenced to its concrete
/// member at every descent step and at the leaf (pvxs `lookup("->")`),
/// so a path that crosses an NTNDArray-style union resolves to the
/// active member instead of stopping at the union.
fn extract_field(root: &PvField, path: &str) -> PvField {
    if path.is_empty() {
        return root.deref_selected().clone();
    }
    let mut cursor = root.clone();
    for segment in path.split('.') {
        cursor = match cursor.deref_selected() {
            PvField::Structure(s) => s.get_field(segment).cloned().unwrap_or(PvField::Null),
            other => return other.clone(),
        };
    }
    cursor.deref_selected().clone()
}

fn scalar_as_f64(field: &PvField) -> Option<f64> {
    match field {
        PvField::Scalar(sv) => Some(scalar_value_to_f64(sv)),
        PvField::Structure(s) => s.get_value().map(scalar_value_to_f64),
        _ => None,
    }
}

fn scalar_value_to_f64(v: &ScalarValue) -> f64 {
    match v {
        ScalarValue::Boolean(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        ScalarValue::Byte(x) => *x as f64,
        ScalarValue::UByte(x) => *x as f64,
        ScalarValue::Short(x) => *x as f64,
        ScalarValue::UShort(x) => *x as f64,
        ScalarValue::Int(x) => *x as f64,
        ScalarValue::UInt(x) => *x as f64,
        ScalarValue::Long(x) => *x as f64,
        ScalarValue::ULong(x) => *x as f64,
        ScalarValue::Float(x) => *x as f64,
        ScalarValue::Double(x) => *x,
        ScalarValue::String(s) => s.as_str_lossy().parse().unwrap_or(0.0),
    }
}

/// Map the cached NT value at the link's field path to a DBF type,
/// mirroring pvxs `pvaGetDBFtype` (`pvxs/ioc/pvalink_lset.cpp:199`).
///
/// An NT `enum_t` structure (an `index` integer + `choices` string
/// array) maps to `Enum`; a scalar / scalar array maps by element
/// type. Every other *connected* value shape — an unknown structure, a
/// `Null`/missing selected field, a null union — maps to `Long`, the
/// `default:` arm of pvxs `pvaGetDBFtype` (`pvxs/ioc/pvalink_lset.cpp:199-240`),
/// which returns `DBF_LONG` for any unmappable value type. This getter
/// is only reached for a connected link (the caller gates on the
/// disconnect/no-cache case and returns no metadata at all), so it
/// never returns `None`: `None` from the lset means "not connected",
/// `Some(Long)` means "connected but unmappable".
fn link_dbf_type(value_field: &PvField) -> Option<epics_base_rs::server::database::LinkDbfType> {
    use epics_base_rs::server::database::LinkDbfType;

    // Follow a selected union / non-empty variant to its concrete member
    // first — pvxs reads `fld_value.lookup("->")` for an Any/Union value
    // (`pvxs/ioc/pvalink_lset.cpp:278-279`). Idempotent on plain scalars/arrays.
    let value_field = value_field.deref_selected();

    let from_scalar = |sv: &ScalarValue| match sv {
        ScalarValue::Byte(_) => Some(LinkDbfType::Char),
        ScalarValue::UByte(_) => Some(LinkDbfType::UChar),
        ScalarValue::Short(_) => Some(LinkDbfType::Short),
        ScalarValue::UShort(_) => Some(LinkDbfType::UShort),
        ScalarValue::Int(_) => Some(LinkDbfType::Long),
        ScalarValue::UInt(_) => Some(LinkDbfType::ULong),
        ScalarValue::Long(_) => Some(LinkDbfType::Int64),
        ScalarValue::ULong(_) => Some(LinkDbfType::UInt64),
        ScalarValue::Float(_) => Some(LinkDbfType::Float),
        ScalarValue::Double(_) => Some(LinkDbfType::Double),
        ScalarValue::String(_) => Some(LinkDbfType::String),
        // pvxs maps a boolean value through `DBF_LONG` (the
        // `default:` arm of the `pvaGetDBFtype` switch — booleans are
        // not a DBF type).
        ScalarValue::Boolean(_) => Some(LinkDbfType::Long),
    };

    match value_field {
        PvField::Scalar(sv) => from_scalar(sv),
        // An empty generic array has lost its element type; a connected
        // link still reports a type, so fall back to the pvxs default.
        PvField::ScalarArray(arr) => arr
            .first()
            .and_then(from_scalar)
            .or(Some(LinkDbfType::Long)),
        PvField::ScalarArrayTyped(arr) => {
            use epics_pva_rs::pvdata::ScalarType;
            Some(match arr.scalar_type() {
                ScalarType::Byte => LinkDbfType::Char,
                ScalarType::UByte => LinkDbfType::UChar,
                ScalarType::Short => LinkDbfType::Short,
                ScalarType::UShort => LinkDbfType::UShort,
                ScalarType::Int => LinkDbfType::Long,
                ScalarType::UInt => LinkDbfType::ULong,
                ScalarType::Long => LinkDbfType::Int64,
                ScalarType::ULong => LinkDbfType::UInt64,
                ScalarType::Float => LinkDbfType::Float,
                ScalarType::Double => LinkDbfType::Double,
                ScalarType::String => LinkDbfType::String,
                ScalarType::Boolean => LinkDbfType::Long,
            })
        }
        PvField::Structure(s) => {
            // NTEnum: pvxs maps a struct with an integer `index` and a
            // `choices` string array to `DBF_ENUM`.
            let has_index = matches!(
                s.get_field("index"),
                Some(PvField::Scalar(
                    ScalarValue::Byte(_)
                        | ScalarValue::UByte(_)
                        | ScalarValue::Short(_)
                        | ScalarValue::UShort(_)
                        | ScalarValue::Int(_)
                        | ScalarValue::UInt(_)
                        | ScalarValue::Long(_)
                        | ScalarValue::ULong(_)
                ))
            );
            let has_choices = matches!(
                s.get_field("choices"),
                Some(PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_))
            );
            if has_index && has_choices {
                Some(LinkDbfType::Enum)
            } else {
                // A struct carrying a `value` sub-field (an NT struct)
                // — recurse into `value` so a link with an empty
                // field path still resolves the DBF type, matching
                // pvxs's "if fieldName empty, use top struct value". An
                // unknown struct with no mappable `value` is the
                // connected-but-unmappable case → `Long`.
                Some(
                    s.get_field("value")
                        .and_then(link_dbf_type)
                        .unwrap_or(LinkDbfType::Long),
                )
            }
        }
        // Null, a missing selected field, a null union — connected but
        // unmappable → pvxs `pvaGetDBFtype` default `DBF_LONG`.
        _ => Some(LinkDbfType::Long),
    }
}

/// Element count for the cached NT value at the link's field path:
/// the array length, or `1` for a scalar. Mirrors pvxs
/// `pvaGetElements` (`pvxs/ioc/pvalink_lset.cpp:242-257`), whose
/// non-array branch sets `*nelements = 1`. Like [`link_dbf_type`] this
/// getter is only reached for a connected link, so every connected
/// non-array / unmappable shape reports `1` and `None` is reserved for
/// "not connected".
fn link_element_count(value_field: &PvField) -> Option<i64> {
    // Selected union / non-empty variant → concrete member first, so an
    // NTNDArray union reports the active array's length, not the union
    // (pvxs `lookup("->")`, `pvxs/ioc/pvalink_lset.cpp:278-279`). Idempotent.
    match value_field.deref_selected() {
        PvField::Scalar(_) => Some(1),
        PvField::ScalarArray(arr) => Some(arr.len() as i64),
        PvField::ScalarArrayTyped(arr) => Some(arr.len() as i64),
        // NT struct: count the `value` sub-field (NTEnum index → 1); a
        // struct with no `value` is a connected non-array shape → 1.
        PvField::Structure(s) => s.get_field("value").map_or(Some(1), link_element_count),
        // Null / missing field / null union — connected, one element.
        _ => Some(1),
    }
}

/// Read a `(lo, hi)` limit pair from two dotted NT paths. Returns
/// `None` only when *neither* path resolves to a scalar — a missing
/// half defaults to `0.0`, mirroring pvxs leaving an unwritten
/// `*lo`/`*hi` buffer untouched against a record's zero-initialised
/// limit field.
fn limit_pair(root: &PvField, lo_path: &str, hi_path: &str) -> Option<(f64, f64)> {
    let lo = scalar_as_f64(&extract_field(root, lo_path));
    let hi = scalar_as_f64(&extract_field(root, hi_path));
    match (lo, hi) {
        (None, None) => None,
        (l, h) => Some((l.unwrap_or(0.0), h.unwrap_or(0.0))),
    }
}

/// Read a dotted NT path that should hold a string scalar. Empty
/// strings are treated as "absent" so an NT value that carries an
/// empty `display.units` does not override a record's local EGU.
fn string_field(root: &PvField, path: &str) -> Option<String> {
    match extract_field(root, path) {
        PvField::Scalar(ScalarValue::String(s)) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

// Suppress unused warning for fields used only via accessors.
#[allow(dead_code)]
fn _suppress(_: &PvStructure) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan-trigger overrun, producer half: a full scan-trigger queue
    /// does NOT silently drop the monitor event. `enqueue_scan_trigger`
    /// coalesces to the `latest` cache (already updated by the caller)
    /// and marks an overrun (EPICS `db_queue_event_log` replace-last,
    /// `dbEvent.c:812-827`), so the forwarder still owes one CP/CPP scan.
    #[tokio::test]
    async fn enqueue_scan_trigger_coalesces_on_full_queue() {
        let val = PvField::Scalar(ScalarValue::Double(1.0));
        let (tx, mut rx) = mpsc::channel::<ScanEvent>(1);
        let overrun = ScanOverrun::default();

        // First trigger fits the Q=1 queue — no overrun.
        enqueue_scan_trigger(&tx, &overrun, ScanEvent::Value(val.clone()));
        assert_eq!(overrun.count(), 0, "first send must not overrun");
        assert!(!overrun.take_pending(), "no owed scan after a fitting send");

        // Re-fill the (now empty after take? no — we didn't recv) queue.
        // The queue still holds the first event, so the next send finds
        // it full and must coalesce, not drop.
        enqueue_scan_trigger(&tx, &overrun, ScanEvent::Value(val.clone()));
        assert_eq!(overrun.count(), 1, "second send onto a full queue overruns");
        assert!(
            overrun.take_pending(),
            "a full-queue overrun owes exactly one more scan (no silent loss)"
        );
        // Idempotent: the owed scan is consumed exactly once.
        assert!(!overrun.take_pending(), "owed scan is taken only once");

        // The single queued event is still deliverable (the surplus
        // coalesced into the overrun marker, it did not evict the queued
        // one).
        assert!(matches!(rx.try_recv(), Ok(ScanEvent::Value(_))));
        assert!(rx.try_recv().is_err(), "exactly one event was queued");
    }

    /// Scan-trigger overrun, producer half, disconnect path: the same
    /// coalescing rule covers a `Disconnected` trigger — a full queue
    /// marks an owed (payload-less) scan instead of dropping it.
    #[tokio::test]
    async fn enqueue_scan_trigger_coalesces_disconnect_on_full_queue() {
        let (tx, _rx) = mpsc::channel::<ScanEvent>(1);
        let overrun = ScanOverrun::default();
        enqueue_scan_trigger(&tx, &overrun, ScanEvent::Disconnected); // fills Q=1
        enqueue_scan_trigger(&tx, &overrun, ScanEvent::Disconnected); // full → coalesce
        assert_eq!(overrun.count(), 1);
        assert!(
            overrun.take_pending(),
            "a dropped disconnect still owes a scan"
        );
    }

    #[test]
    fn extract_top_level_value() {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.5))));
        let root = PvField::Structure(s);
        let v = extract_field(&root, "value");
        match v {
            PvField::Scalar(ScalarValue::Double(d)) => assert_eq!(d, 1.5),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn extract_nested_field() {
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        let value = extract_field(&PvField::Structure(root), "alarm.severity");
        assert!(matches!(value, PvField::Scalar(ScalarValue::Int(2))));
    }

    #[test]
    fn missing_field_returns_null() {
        let s = PvStructure::new("epics:nt/NTScalar:1.0");
        let v = extract_field(&PvField::Structure(s), "nope");
        assert!(matches!(v, PvField::Null));
    }

    /// pvxs selected-root rule (`select_link_value`,
    /// `pvxs/ioc/pvalink_link.cpp:90-110`): empty field selects the top-level
    /// value (`.value` for a structure, else the root itself); a
    /// non-empty field selects `root[field]` then `.value` if that is a
    /// structure. Default `field=""` on an NTScalar still yields the
    /// scalar, but a bare top-level value is now selected directly
    /// instead of being searched for a missing `value` child.
    #[test]
    fn select_link_value_follows_pvxs_selected_root_rule() {
        // NTScalar, empty field → .value.
        let mut nt = PvStructure::new("epics:nt/NTScalar:1.0");
        nt.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.5))));
        let nt = PvField::Structure(nt);
        assert!(matches!(
            select_link_value(&nt, ""),
            PvField::Scalar(ScalarValue::Double(d)) if d == 2.5
        ));
        // explicit field="value" yields the same scalar.
        assert!(matches!(
            select_link_value(&nt, "value"),
            PvField::Scalar(ScalarValue::Double(d)) if d == 2.5
        ));

        // Bare top-level scalar (PV whose root is not an NT structure):
        // empty field selects the scalar itself, not a missing child.
        let bare = PvField::Scalar(ScalarValue::Long(7));
        assert!(matches!(
            select_link_value(&bare, ""),
            PvField::Scalar(ScalarValue::Long(7))
        ));

        // Non-empty field selecting a nested structure drills into its
        // own `.value`.
        let mut sub = PvStructure::new("epics:nt/NTScalar:1.0");
        sub.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(42))));
        let mut outer = PvStructure::new("some_t");
        outer.fields.push(("sub".into(), PvField::Structure(sub)));
        let outer = PvField::Structure(outer);
        assert!(matches!(
            select_link_value(&outer, "sub"),
            PvField::Scalar(ScalarValue::Long(42))
        ));
    }

    /// `link_metadata` derives the DBF type from the selected value. A
    /// link whose `field` selects a nested NTScalar-shaped structure
    /// must report that structure's `.value` DBF type. Pre-fix the
    /// metadata path read the selected target *as* the value, so a
    /// structure selection produced no DBF type at all.
    #[test]
    fn link_metadata_dbf_drills_selected_substructure_value() {
        use epics_base_rs::server::database::LinkDbfType;

        let mut sub = PvStructure::new("epics:nt/NTScalar:1.0");
        sub.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(42))));
        let mut outer = PvStructure::new("some_t");
        outer.fields.push(("sub".into(), PvField::Structure(sub)));

        let cfg = PvaLinkConfig {
            field: "sub".to_string(),
            ..PvaLinkConfig::defaults_for("META:PV", LinkDirection::Inp)
        };
        let link = PvaLink::for_test(cfg, Some(PvField::Structure(outer)));
        let meta = link.link_metadata().expect("metadata present");
        // PVA `long` → DBF Int64 (full 64-bit width); the point is that
        // a DBF type is derived at all, from the sub-structure's .value.
        assert_eq!(
            meta.dbf_type,
            Some(LinkDbfType::Int64),
            "DBF type must come from the selected sub-structure's .value"
        );
    }

    /// An NTNDArray carries its pixels in a discriminated `value` union
    /// (pvxs `nt.cpp:208-220`; `testNTNDArray` writes `value->floatValue`).
    /// A pvalink reading it must follow the union to its active member —
    /// pvxs `value.lookup("->")` (`pvxs/ioc/pvalink_lset.cpp:278-279`) — so the
    /// field-path extraction, DBF-type mapping and element-count mapping
    /// all resolve `floatValue`, not the opaque union.
    #[test]
    fn ntndarray_union_value_reads_through_selected_member() {
        use epics_base_rs::server::database::LinkDbfType;

        let mut nd = PvStructure::new("epics:nt/NTNDArray:1.0");
        nd.fields.push((
            "value".into(),
            PvField::Union {
                // exact discriminant index is immaterial to the deref
                // (any `selector >= 0` selects the active member).
                selector: 9,
                variant_name: "floatValue".into(),
                value: Box::new(PvField::scalar_array_float(vec![1.5f32, 2.5, 3.5])),
            },
        ));
        // monitor=true so the sync cached-read accessor is live; the
        // for_test link reports connected (a cached value is present).
        let cfg = PvaLinkConfig {
            monitor: true,
            ..PvaLinkConfig::defaults_for("ND:PV", LinkDirection::Inp)
        };
        let link = PvaLink::for_test(cfg, Some(PvField::Structure(nd)));

        // field-path extraction follows the union to its active member.
        let selected = link
            .try_read_cached_with_field("")
            .expect("cached value present");
        assert_eq!(
            selected,
            PvField::scalar_array_float(vec![1.5f32, 2.5, 3.5]),
            "selected value must be the union's active floatValue member"
        );

        // DBF type / element count map the dereferenced array, not the
        // union (pre-fix both were `None` for a `PvField::Union`).
        let meta = link.link_metadata().expect("metadata present");
        assert_eq!(
            meta.dbf_type,
            Some(LinkDbfType::Float),
            "DBF type must come from the selected floatValue member"
        );
        assert_eq!(
            meta.element_count,
            Some(3),
            "element count is the active array's length"
        );
    }

    /// pvxs `pvaGetDBFtype` returns `DBF_LONG` and `pvaGetElements`
    /// returns `1` for any *connected* but unmappable value
    /// (`pvxs/ioc/pvalink_lset.cpp:199-257` default arms). The Rust getters
    /// previously returned `None` for those shapes, which the DB link
    /// API reads as "not connected". The connected fallback must be
    /// distinct from the genuine no-cache (disconnected) case, which
    /// alone yields `None`.
    #[test]
    fn connected_unmappable_value_falls_back_to_dbf_long_one_element() {
        use epics_base_rs::server::database::LinkDbfType;

        // (a) connected NT struct with no mappable `value` child.
        let mut tbl = PvStructure::new("epics:nt/NTTable:1.0");
        tbl.fields
            .push(("labels".into(), PvField::Scalar(ScalarValue::Int(1))));
        let link = PvaLink::for_test(
            PvaLinkConfig::defaults_for("U:PV", LinkDirection::Inp),
            Some(PvField::Structure(tbl)),
        );
        let meta = link.link_metadata().expect("connected → Some metadata");
        assert_eq!(
            meta.dbf_type,
            Some(LinkDbfType::Long),
            "connected unmappable struct → DBF_LONG"
        );
        assert_eq!(meta.element_count, Some(1), "connected unmappable → 1");

        // (b) connected value but the selected field is missing.
        let mut nt = PvStructure::new("epics:nt/NTScalar:1.0");
        nt.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        let link2 = PvaLink::for_test(
            PvaLinkConfig {
                field: "nonexistent".into(),
                ..PvaLinkConfig::defaults_for("U:PV", LinkDirection::Inp)
            },
            Some(PvField::Structure(nt)),
        );
        let meta2 = link2.link_metadata().expect("connected → Some metadata");
        assert_eq!(
            meta2.dbf_type,
            Some(LinkDbfType::Long),
            "connected, missing field → DBF_LONG"
        );
        assert_eq!(meta2.element_count, Some(1), "missing field → 1");

        // (c) the no-cache (disconnected) case is the only `None`.
        let link3 = PvaLink::for_test(
            PvaLinkConfig::defaults_for("U:PV", LinkDirection::Inp),
            None,
        );
        assert!(
            link3.link_metadata().is_none(),
            "no cached value → None (not connected), distinct from the fallback"
        );
    }

    /// A link whose `field` selects a nested structure must read its
    /// alarm severity, alarm message AND timestamp from that selected
    /// root — not from the top-level NT alarm/timeStamp. pvxs rebinds
    /// `root = lchan->root[fieldName]` in `onTypeChange`
    /// (`pvxs/ioc/pvalink_link.cpp:90-110`) and then resolves
    /// `fld_severity`/`fld_message`/`fld_seconds`/`fld_nanoseconds`/
    /// `fld_usertag` relative to that root
    /// (`pvxs/ioc/pvalink_lset.cpp:399-430`). Pre-fix every one of
    /// these getters read the top-level alarm/timeStamp regardless of
    /// the selected field, so a link selecting a nested member adopted
    /// the wrong member's (or the container's) alarm and time.
    #[test]
    fn alarm_and_timestamp_resolve_at_selected_field_root() {
        // Top-level alarm is NO_ALARM and timeStamp is secs=1; the
        // selected `member` sub-structure carries MAJOR(2)/"member hot"
        // and a distinct timeStamp secs=999, ns=7, userTag=5.
        let mut top_alarm = PvStructure::new("alarm_t");
        top_alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(0))));
        let mut top_ts = PvStructure::new("time_t");
        top_ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(1)),
        ));
        top_ts
            .fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(0))));

        let mut mem_alarm = PvStructure::new("alarm_t");
        mem_alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
        mem_alarm.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String("member hot".into())),
        ));
        let mut mem_ts = PvStructure::new("time_t");
        mem_ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(999)),
        ));
        mem_ts
            .fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(7))));
        mem_ts
            .fields
            .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(5))));
        let mut member = PvStructure::new("epics:nt/NTScalar:1.0");
        member
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        member
            .fields
            .push(("alarm".into(), PvField::Structure(mem_alarm)));
        member
            .fields
            .push(("timeStamp".into(), PvField::Structure(mem_ts)));

        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("alarm".into(), PvField::Structure(top_alarm)));
        root.fields
            .push(("timeStamp".into(), PvField::Structure(top_ts)));
        root.fields
            .push(("member".into(), PvField::Structure(member)));
        let value = PvField::Structure(root);

        // field="member", MS: alarm/message/timestamp come from member.
        let cfg = PvaLinkConfig {
            field: "member".to_string(),
            monitor: true,
            sevr: SevrMode::Ms,
            ..PvaLinkConfig::defaults_for("SEL:PV", LinkDirection::Inp)
        };
        let link = PvaLink::for_test(cfg, Some(value.clone()));
        assert_eq!(
            link.link_alarm_severity(),
            Some(2),
            "severity must come from the selected member, not top-level NO_ALARM"
        );
        assert_eq!(
            link.alarm_message().as_deref(),
            Some("member hot"),
            "message must come from the selected member"
        );
        assert_eq!(
            link.time_stamp("member"),
            Some((999, 7, 5)),
            "timestamp/userTag must come from the selected member"
        );

        // field="" reads the top-level root: NO_ALARM does not
        // propagate (None), and the timestamp is the top-level secs=1.
        let cfg_top = PvaLinkConfig {
            monitor: true,
            sevr: SevrMode::Ms,
            ..PvaLinkConfig::defaults_for("SEL:PV", LinkDirection::Inp)
        };
        let link_top = PvaLink::for_test(cfg_top, Some(value));
        assert_eq!(
            link_top.link_alarm_severity(),
            None,
            "top-level NO_ALARM under MS propagates nothing"
        );
        assert_eq!(link_top.alarm_message(), None);
        assert_eq!(
            link_top.time_stamp(""),
            Some((1, 0, 0)),
            "empty field reads the top-level timeStamp"
        );
    }

    use super::super::config::LinkDirection;
    use super::super::config::{PvaLinkConfig, SevrMode};

    /// Build an NTScalar-shaped structure with an `alarm.severity`
    /// (and optional `alarm.message`).
    fn nt_with_alarm(severity: i32, message: Option<&str>) -> PvField {
        let mut alarm = PvStructure::new("alarm_t");
        alarm.fields.push((
            "severity".into(),
            PvField::Scalar(ScalarValue::Int(severity)),
        ));
        if let Some(m) = message {
            alarm.fields.push((
                "message".into(),
                PvField::Scalar(ScalarValue::String(m.into())),
            ));
        }
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        PvField::Structure(root)
    }

    fn inp_cfg(sevr: SevrMode) -> PvaLinkConfig {
        PvaLinkConfig {
            monitor: true,
            sevr,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Inp)
        }
    }

    // ---- B2: MS / NMS / MSI severity propagation on the read path ----

    #[test]
    fn b2_nms_drops_all_severities() {
        for sev in 1..=3 {
            let link = PvaLink::for_test(
                inp_cfg(SevrMode::Nms),
                Some(nt_with_alarm(sev, Some("bad"))),
            );
            assert_eq!(link.link_alarm_severity(), None, "sev={sev}");
            assert_eq!(link.alarm_message(), None, "sev={sev}");
        }
    }

    #[test]
    fn b2_ms_propagates_any_nonzero_severity() {
        // NO_ALARM does not propagate.
        let ok = PvaLink::for_test(inp_cfg(SevrMode::Ms), Some(nt_with_alarm(0, None)));
        assert_eq!(ok.link_alarm_severity(), None);
        assert_eq!(ok.alarm_message(), None);
        // MINOR / MAJOR / INVALID all propagate.
        for sev in 1..=3 {
            let link = PvaLink::for_test(
                inp_cfg(SevrMode::Ms),
                Some(nt_with_alarm(sev, Some("oops"))),
            );
            assert_eq!(link.link_alarm_severity(), Some(sev), "sev={sev}");
            assert_eq!(link.alarm_message(), Some("oops".to_string()), "sev={sev}");
        }
    }

    #[test]
    fn b2_msi_propagates_only_invalid() {
        let minor = PvaLink::for_test(inp_cfg(SevrMode::Msi), Some(nt_with_alarm(1, Some("m"))));
        assert_eq!(minor.link_alarm_severity(), None);
        let major = PvaLink::for_test(inp_cfg(SevrMode::Msi), Some(nt_with_alarm(2, Some("m"))));
        assert_eq!(major.link_alarm_severity(), None);
        let invalid =
            PvaLink::for_test(inp_cfg(SevrMode::Msi), Some(nt_with_alarm(3, Some("dead"))));
        assert_eq!(invalid.link_alarm_severity(), Some(3));
        assert_eq!(invalid.alarm_message(), Some("dead".to_string()));
    }

    #[test]
    fn b2_synthetic_message_when_no_alarm_message_field() {
        // MS link, severity propagates, but the NT struct has no
        // alarm.message — a synthetic message is returned.
        let link = PvaLink::for_test(inp_cfg(SevrMode::Ms), Some(nt_with_alarm(2, None)));
        assert_eq!(link.link_alarm_severity(), Some(2));
        assert_eq!(link.alarm_message(), Some("remote severity 2".to_string()));
    }

    #[test]
    fn b2_no_cached_value_means_no_alarm() {
        let link = PvaLink::for_test(inp_cfg(SevrMode::Ms), None);
        assert_eq!(link.link_alarm_severity(), None);
        assert_eq!(link.alarm_message(), None);
    }

    // ---- ungated remote alarm snapshot (pvxs pvaGetAlarmMsg) ----

    /// The ungated remote alarm snapshot is LATCHED at the value read,
    /// not read live from the cached monitor value — and it stays ungated
    /// by `sevr`. This is pvxs `testMeta()`
    /// (`pvxs/test/testpvalink.cpp:333-457`): on a connected default
    /// `NMS` pvalink, `dbGetAlarm()` reports `LINK_ALARM`/`INVALID_ALARM`
    /// with a blank message BEFORE the first `dbGetLink()` (pvxs
    /// initializes `snap_severity = INVALID_ALARM`, `pvxs/ioc/pvalink.h:250`), and
    /// only AFTER `dbGetLink()` does it report the remote
    /// `LINK_ALARM`/`MINOR`/message — because `pvaGetValue` latches
    /// `snap_*` (`pvxs/ioc/pvalink_lset.cpp:412-422`) and `pvaGetAlarmMsg` does not
    /// consult `sevr` (`pvxs/ioc/pvalink_lset.cpp:542-569`).
    #[test]
    fn alarm_snapshot_latches_on_value_read_ungated_by_sevr() {
        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), Some(nt_with_alarm(1, Some("hi"))));
        // gated path: NMS propagates nothing to the owning record, and
        // (unlike the snapshot) reads the cached value live — it does not
        // latch.
        assert_eq!(link.link_alarm_severity(), None);
        assert_eq!(link.alarm_message(), None);
        // BEFORE the first value read: the snapshot is the pvxs initial
        // INVALID_ALARM(3) + LINK_ALARM(14) + blank, NOT the cached MINOR.
        let pre = link.remote_alarm_snapshot("").expect("connected snapshot");
        assert_eq!(pre.severity, 3); // INVALID_ALARM
        assert_eq!(pre.status, 14); // LINK_ALARM
        assert_eq!(pre.message, "");
        // a value read latches the snapshot (pvxs pvaGetValue).
        assert!(link.try_read_cached_with_field("").is_some());
        // AFTER the read: remote MINOR(1) + LINK_ALARM(14) + message.
        let post = link.remote_alarm_snapshot("").expect("connected snapshot");
        assert_eq!(post.severity, 1);
        assert_eq!(post.status, 14); // LINK_ALARM
        assert_eq!(post.message, "hi");
    }

    /// `MS` link with a remote MAJOR alarm: BOTH the gated owning-record
    /// contribution (record raised) AND the ungated snapshot report the
    /// remote severity. pvxs `testMetaMS()` — `sevr:"MS"` raises the
    /// owning record's pending alarm (`pvxs/test/testpvalink.cpp:467-514`); the
    /// snapshot is unchanged by the gate.
    #[test]
    fn alarm_snapshot_matches_gated_path_for_ms() {
        let link = PvaLink::for_test(inp_cfg(SevrMode::Ms), Some(nt_with_alarm(2, Some("oops"))));
        // gated path: MS raises the owning record.
        assert_eq!(link.link_alarm_severity(), Some(2));
        // a value read latches the snapshot (pvxs pvaGetValue).
        assert!(link.try_read_cached_with_field("").is_some());
        // ungated snapshot: same remote MAJOR + LINK_ALARM + message.
        let snap = link.remote_alarm_snapshot("").expect("connected snapshot");
        assert_eq!(snap.severity, 2);
        assert_eq!(snap.status, 14); // LINK_ALARM
        assert_eq!(snap.message, "oops");
    }

    /// A NO_ALARM cached value yields a snapshot with severity 0, status
    /// NO_ALARM(0), and an empty message — pvxs clears `snap_message`
    /// unless `snap_severity != 0` (`pvxs/ioc/pvalink_lset.cpp:418-421`) and sets
    /// `status = snap_severity ? LINK_ALARM : NO_ALARM`
    /// (`pvxs/ioc/pvalink_lset.cpp:554`).
    #[test]
    fn alarm_snapshot_no_alarm_has_zero_status_and_blank_message() {
        let link = PvaLink::for_test(
            inp_cfg(SevrMode::Nms),
            Some(nt_with_alarm(0, Some("ignored"))),
        );
        // a value read latches the snapshot (pvxs pvaGetValue).
        assert!(link.try_read_cached_with_field("").is_some());
        let snap = link.remote_alarm_snapshot("").expect("connected snapshot");
        assert_eq!(snap.severity, 0);
        assert_eq!(snap.status, 0); // NO_ALARM
        assert_eq!(snap.message, "", "NO_ALARM snapshot carries no message");
    }

    /// CHECK_VALID: a disconnected-stale monitor link serves no snapshot
    /// even though its last value is retained, and a link with no cached
    /// value yields `None` (pvxs `pvaGetAlarmMsg` returns -1 while
    /// `!valid()` — `pvxs/ioc/pvalink_lset.cpp:548`).
    #[test]
    fn alarm_snapshot_refuses_stale_while_disconnected() {
        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(nt_with_alarm(2, Some("was-major"))),
        );
        flag.store(true, Ordering::Release);
        // latch a real snapshot so the disconnect-refusal below is not
        // merely refusing the initial INVALID default.
        assert!(link.try_read_cached_with_field("").is_some());
        let snap = link
            .remote_alarm_snapshot("")
            .expect("connected monitor serves the snapshot");
        assert_eq!(snap.severity, 2);
        flag.store(false, Ordering::Release);
        assert!(
            link.remote_alarm_snapshot("").is_none(),
            "disconnected-stale monitor refuses the snapshot (CHECK_VALID)"
        );
        // no cached value at all → None regardless of connection state.
        let empty = PvaLink::for_test(inp_cfg(SevrMode::Nms), None);
        assert!(empty.remote_alarm_snapshot("").is_none());
    }

    // ---- B4: monitor_request (Q / pipeline) ----

    /// pvxs `pvaLink::makeRequest` always emits pipeline +
    /// atomic + queueSize even on a defaults-only INP monitor.
    /// Regression for the prior `None`-for-defaults shortcut that
    /// silently let the remote server fall back to its own defaults.
    #[test]
    fn b4_monitor_request_always_carries_pvxs_options() {
        let cfg = PvaLinkConfig::defaults_for("X", LinkDirection::Inp);
        let req = monitor_request(&cfg).expect("defaults still yield a request");
        // pipeline = false on default config; queueSize = pvxs default 4;
        // atomic = forced true.
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "pipeline" && *v == ScalarValue::Boolean(false))
        );
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "atomic" && *v == ScalarValue::Boolean(true)),
            "atomic must be hard-coded true on remote pvalink monitor requests"
        );
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "queueSize" && *v == ScalarValue::UInt(4)),
            "queueSize must default to pvxs's 4 on no-options links"
        );
    }

    #[test]
    fn b4_monitor_request_carries_queue_size() {
        let cfg = PvaLinkConfig {
            queue_size: 16,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Inp)
        };
        let req = monitor_request(&cfg).expect("non-default Q yields a request");
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "queueSize" && *v == ScalarValue::UInt(16))
        );
    }

    #[test]
    fn b4_monitor_request_carries_pipeline() {
        let cfg = PvaLinkConfig {
            pipeline: true,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Inp)
        };
        let req = monitor_request(&cfg).expect("pipeline yields a request");
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "pipeline" && *v == ScalarValue::Boolean(true))
        );
        // pvxs `makeRequest` always sends queueSize alongside pipeline.
        assert!(req.record_options.iter().any(|(k, _)| k == "queueSize"));
    }

    /// An OUT link's connection-tracking monitor must open with the same pvalink
    /// pvRequest as an INP monitor — pvxs uses one `makeRequest()` output for both
    /// directions (`pvxs/ioc/pvalink_link.cpp:49-65`,
    /// `pvxs/ioc/pvalink_channel.cpp` channel open).
    /// Regression for the OUT path opening a plain option-less
    /// `pvmonitor` so the server saw no atomic/pipeline/queue
    /// negotiation on the liveness monitor that gates OUT writes.
    #[test]
    fn out_connection_monitor_request_matches_pvxs_make_request() {
        let cfg = PvaLinkConfig {
            pipeline: true,
            queue_size: 16,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Out)
        };
        let req = monitor_request(&cfg).expect("OUT monitor must carry a pvRequest");
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "atomic" && *v == ScalarValue::Boolean(true)),
            "OUT monitor request must force atomic=true"
        );
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "pipeline" && *v == ScalarValue::Boolean(true)),
            "OUT monitor request must carry pipeline=true"
        );
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "queueSize" && *v == ScalarValue::UInt(16)),
            "OUT monitor request must carry queueSize=16"
        );
    }

    /// A forward link on a DISCONNECTED channel must NOT issue a process:
    /// pvxs `pvaScanForward`'s `!retry && !valid()` gate
    /// (`pvxs/ioc/pvalink_lset.cpp:677`) returns immediately — no blocking connect,
    /// no wire trigger — so the owning record can alarm LINK/INVALID. The
    /// monitor flag is left false, standing in for a channel whose monitor
    /// has not connected.
    #[tokio::test]
    async fn scan_forward_on_disconnected_link_is_gated() {
        let (link, _flag) = PvaLink::for_test_with_monitor_flag(inp_cfg(SevrMode::Ms), None);
        assert!(!link.is_connected());
        match link.scan_forward().await {
            Err(PvaLinkError::Disconnected(_)) => {}
            other => {
                panic!("disconnected forward must be gated to Disconnected, got {other:?}")
            }
        }
    }

    /// The other arm of the SAME gate: `retry` is the first term of
    /// `if(!self->retry && !self->valid())` (`pvxs/ioc/pvalink_lset.cpp:677`), so a
    /// `retry` link on a disconnected channel is NOT gated — pvxs falls
    /// through to `lchan->put(true)`, which starts the operation
    /// unconditionally (`pvxs/ioc/pvalink_channel.cpp:226,266`) and lets the client
    /// hold it until the channel connects. The put here cannot succeed
    /// (no server), so what this pins is only that the failure is not the
    /// gate's `Disconnected`.
    #[tokio::test]
    async fn scan_forward_on_disconnected_retry_link_is_not_gated() {
        let cfg = PvaLinkConfig {
            retry: true,
            ..inp_cfg(SevrMode::Ms)
        };
        let (link, _flag) = PvaLink::for_test_with_monitor_flag(cfg, None);
        assert!(!link.is_connected());
        if let Err(PvaLinkError::Disconnected(_)) = link.scan_forward().await {
            panic!("a retry link must skip the !valid() gate, not return Disconnected");
        }
    }

    // ---- R5-PVA-1: the forward link is a PUT, never cmd 16 ----

    /// What the scripted pvxs-faithful server below observed.
    #[derive(Default)]
    struct ConnSwitchObs {
        /// Application commands that fell to `default:` — pvxs drains and
        /// never answers these (`conn.cpp:250-253`).
        drained: Vec<u8>,
        /// Raw PUT INIT pvRequest bytes, as they arrived.
        put_init_req: Option<Vec<u8>>,
        /// Raw PUT DATA payload, from the `changed` BitSet onwards.
        put_data_body: Option<Vec<u8>>,
    }

    /// A scripted server that IS pvxs `ConnBase`'s command switch
    /// (`pvxs/src/conn.cpp:249-276`): CREATE_CHANNEL and PUT are answered,
    /// and every other application command falls to `default:`, which
    /// debug-logs, `evbuffer_drain`s the body and replies nothing.
    ///
    /// CMD_PROCESS (16) is one of those. pvxs has no `CASE(PROCESS)`: the
    /// constant occurs exactly once in its tree, as the enum member at
    /// `src/pvaproto.h:632`.
    async fn spawn_conn_switch_server(
        obs: Arc<std::sync::Mutex<ConnSwitchObs>>,
        refuse_put: bool,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted server");
        let addr = listener.local_addr().expect("scripted server addr");
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve_conn_switch(sock, obs.clone(), refuse_put));
            }
        });
        addr
    }

    async fn serve_conn_switch(
        mut sock: tokio::net::TcpStream,
        obs: Arc<std::sync::Mutex<ConnSwitchObs>>,
        refuse_put: bool,
    ) {
        use epics_pva_rs::proto::{
            ByteOrder, Command, ControlCommand, PvaHeader, Status, WriteExt, encode_size_into,
            encode_string_into,
        };
        use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const ORDER: ByteOrder = ByteOrder::Little;
        const SID: u32 = 7;

        fn app_frame(cmd: u8, payload: Vec<u8>) -> Vec<u8> {
            let mut out = Vec::new();
            PvaHeader::application(true, ORDER, cmd, payload.len() as u32).write_into(&mut out);
            out.extend_from_slice(&payload);
            out
        }

        let mut hello = Vec::new();
        PvaHeader::control(true, ORDER, ControlCommand::SetByteOrder.code(), 0)
            .write_into(&mut hello);
        let mut p = Vec::new();
        p.put_u32(0x10000, ORDER);
        p.put_u16(32_767, ORDER);
        encode_size_into(1, ORDER, &mut p);
        encode_string_into("anonymous", ORDER, &mut p);
        hello.extend_from_slice(&app_frame(Command::ConnectionValidation.code(), p));
        if sock.write_all(&hello).await.is_err() {
            return;
        }

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let Ok(n) = sock.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);

            while buf.len() >= 8 {
                let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
                if buf.len() < 8 + len {
                    break;
                }
                let cmd = buf[3];
                let is_control = buf[2] & 0x01 != 0;
                let payload = buf[8..8 + len].to_vec();
                buf.drain(..8 + len);
                if is_control {
                    continue;
                }

                if cmd == Command::ConnectionValidation.code() {
                    let out = app_frame(Command::ConnectionValidated.code(), vec![0xFF]);
                    if sock.write_all(&out).await.is_err() {
                        return;
                    }
                } else if cmd == Command::CreateChannel.code() {
                    let cid = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
                    let mut p = Vec::new();
                    p.put_u32(cid, ORDER);
                    p.put_u32(SID, ORDER);
                    Status::ok().write_into(ORDER, &mut p);
                    if sock
                        .write_all(&app_frame(Command::CreateChannel.code(), p))
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else if cmd == Command::Put.code() {
                    // sid(u32) + ioid(u32) + subcmd(u8) + …
                    let ioid = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    let subcmd = payload[8];
                    let mut p = Vec::new();
                    p.put_u32(ioid, ORDER);
                    if subcmd & 0x08 != 0 {
                        obs.lock().unwrap().put_init_req = Some(payload[9..].to_vec());
                        p.put_u8(0x08);
                        if refuse_put {
                            Status::error("PUT refused").write_into(ORDER, &mut p);
                            if sock
                                .write_all(&app_frame(Command::Put.code(), p))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        Status::ok().write_into(ORDER, &mut p);
                        let intro = FieldDesc::Structure {
                            struct_id: "epics:nt/NTScalar:1.0".into(),
                            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
                        };
                        epics_pva_rs::pvdata::encode::encode_type_desc(&intro, ORDER, &mut p);
                    } else {
                        obs.lock().unwrap().put_data_body = Some(payload[9..].to_vec());
                        p.put_u8(0x00);
                        Status::ok().write_into(ORDER, &mut p);
                    }
                    if sock
                        .write_all(&app_frame(Command::Put.code(), p))
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else if cmd == Command::DestroyRequest.code()
                    || cmd == Command::DestroyChannel.code()
                {
                    // pvxs answers these; nothing this test reads.
                } else {
                    // `default:` — log, drain, reply nothing.
                    obs.lock().unwrap().drained.push(cmd);
                }
            }
        }
    }

    /// A pvalink forward link must reach a pvxs server, so it may not be
    /// spelled CMD_PROCESS: `pvaScanForward` is `lchan->put(true)`
    /// (`pvxs/ioc/pvalink_lset.cpp:683`), which `pvaLinkChannel::put` turns into a
    /// PUT with `record._options.process = "true"`
    /// (`pvxs/ioc/pvalink_channel.cpp:257-263`) whose `linkBuildPut` marked no field
    /// (`:127-184`), i.e. an EMPTY changed bitset.
    ///
    /// Pre-fix `scan_forward` sent cmd 16; the switch above drained it and
    /// the op blocked to the client timeout, so the remote record never
    /// processed and no in-tree epics-rs-to-epics-rs test could see it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forward_link_puts_an_empty_bitset_with_process_true() {
        use epics_pva_rs::proto::ByteOrder;
        use epics_pva_rs::pvdata::{PvField, ScalarValue};

        let obs = Arc::new(std::sync::Mutex::new(ConnSwitchObs::default()));
        let addr = spawn_conn_switch_server(obs.clone(), false).await;
        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let link = PvaLink::for_test_with_client(
            PvaLinkConfig::defaults_for("FWD:TGT", LinkDirection::Inp),
            client,
        );

        link.scan_forward()
            .await
            .expect("the forward link must complete against a pvxs command switch");

        let seen = obs.lock().unwrap();
        assert!(
            seen.drained.is_empty(),
            "the forward link sent command(s) pvxs drains at `default:`: {:?}",
            seen.drained
        );

        // The INIT pvRequest carries `record._options.process = "true"`.
        let req_bytes = seen
            .put_init_req
            .as_ref()
            .expect("the forward link must open a PUT");
        let mut cur = std::io::Cursor::new(&req_bytes[..]);
        let req_desc = epics_pva_rs::pvdata::encode::decode_type_desc(&mut cur, ByteOrder::Little)
            .expect("pvRequest descriptor");
        let req_val =
            epics_pva_rs::pvdata::encode::decode_pv_field(&req_desc, &mut cur, ByteOrder::Little)
                .expect("pvRequest value");
        let process = match &req_val {
            PvField::Structure(s) => {
                extract_field(&PvField::Structure(s.clone()), "record._options.process")
            }
            other => panic!("pvRequest is not a structure: {other:?}"),
        };
        assert_eq!(
            process,
            PvField::Scalar(ScalarValue::String("true".into())),
            "a forced put pins record._options.process to \"true\" \
             (pvxs/ioc/pvalink_channel.cpp:258-260)"
        );

        // The DATA phase marks nothing: an empty changed bitset, no value.
        let data = seen
            .put_data_body
            .as_ref()
            .expect("the forward link must send the PUT data phase");
        let mut cur = std::io::Cursor::new(&data[..]);
        let changed = epics_pva_rs::proto::BitSet::decode(&mut cur, ByteOrder::Little)
            .expect("changed bitset");
        assert_eq!(
            changed.count(),
            0,
            "a forward link writes no field, so linkBuildPut leaves the \
             prototype unmarked (pvxs/ioc/pvalink_channel.cpp:127-184)"
        );
        assert_eq!(
            cur.position() as usize,
            data.len(),
            "an unmarked bitset must be followed by no value bytes"
        );
    }

    /// A forward link whose PUT the server refuses must surface as `Err`,
    /// never as a silent `Ok`. pvxs raises no record alarm on this path —
    /// `linkPutDone` reports the failed put with `errlogPrintf` and carries
    /// an explicit `// TODO: signal INVALID_ALARM ?`
    /// (`pvxs/ioc/pvalink_channel.cpp:192-195`) — so the resolver's spawned task
    /// logs it to the console and the record stays unalarmed. The `Err`
    /// is what gives it something to log.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_forward_link_put_is_not_reported_as_success() {
        let obs = Arc::new(std::sync::Mutex::new(ConnSwitchObs::default()));
        let addr = spawn_conn_switch_server(obs.clone(), true).await;
        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let link = PvaLink::for_test_with_client(
            PvaLinkConfig::defaults_for("FWD:TGT", LinkDirection::Inp),
            client,
        );

        match link.scan_forward().await {
            Err(PvaLinkError::Pva(_)) => {}
            other => panic!("a refused forward-link put must surface, got {other:?}"),
        }
    }

    #[cfg_attr(exec_backend, allow(dead_code))]
    /// A record that counts its `process()` calls and holds a VAL, so a
    /// test can see both halves of a forward link: the remote record ran,
    /// and the value it held was not disturbed.
    struct ProcCountRecord {
        val: f64,
        processed: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl epics_base_rs::server::record::Record for ProcCountRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(
            &mut self,
        ) -> epics_base_rs::error::CaResult<epics_base_rs::server::record::ProcessOutcome> {
            self.processed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(epics_base_rs::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, n: &str) -> Option<epics_base_rs::types::EpicsValue> {
            // Only VAL: every other name must fall through to the common
            // fields, or SIMM resolves to this value and the cycle bails
            // as an illegal simulation mode before `process()` runs.
            match n {
                "" | "VAL" => Some(epics_base_rs::types::EpicsValue::Double(self.val)),
                _ => None,
            }
        }
        fn put_field(
            &mut self,
            _n: &str,
            v: epics_base_rs::types::EpicsValue,
        ) -> epics_base_rs::error::CaResult<()> {
            if let epics_base_rs::types::EpicsValue::Double(d) = v {
                self.val = d;
            }
            Ok(())
        }
        fn declared_fields(&self) -> &'static [epics_base_rs::server::record::FieldDesc] {
            &[]
        }
    }

    #[cfg(tokio_backend)]
    /// The same forward link end to end against an epics-rs PVA server
    /// over a real record: the target must process, and its VAL must not
    /// move — `pvaScanForward` (`pvxs/ioc/pvalink_lset.cpp:672-688`) calls
    /// `lchan->put(true)` without ever touching `put_scratch`, so a forward
    /// link transfers no value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forward_link_processes_a_native_record_without_moving_its_value() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::server::PvDatabaseSource;
        #[cfg(tokio_backend)]
        use epics_pva_rs::server_native::PvaServer;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let processed = Arc::new(AtomicUsize::new(0));
        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "FWD:REC",
            Box::new(ProcCountRecord {
                val: 5.0,
                processed: processed.clone(),
            }),
        )
        .await
        .expect("record added");

        let server = PvaServer::isolated(Arc::new(PvDatabaseSource::new(db.clone())))
            .expect("test PVA server starts");
        let addr = server.tcp_addr();

        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let link = PvaLink::for_test_with_client(
            PvaLinkConfig::defaults_for("FWD:REC", LinkDirection::Inp),
            client,
        );

        link.scan_forward().await.expect("forward link fires");

        assert_eq!(
            processed.load(Ordering::SeqCst),
            1,
            "record._options.process=\"true\" must process the target once"
        );
        let val = db
            .get_record("FWD:REC")
            .expect("record present")
            .read()
            .resolve_field("VAL");
        assert_eq!(
            val,
            Some(epics_base_rs::types::EpicsValue::Double(5.0)),
            "a forward link transfers no value, so VAL must be untouched"
        );
    }

    // ---- B4: defer / retry Put queue ----

    fn out_cfg(defer: bool, retry: bool) -> PvaLinkConfig {
        PvaLinkConfig {
            defer,
            retry,
            ..PvaLinkConfig::defaults_for("X", LinkDirection::Out)
        }
    }

    /// A CONNECTED OUT link for offline staging/coalescing tests:
    /// stands in for an OUT channel whose connection monitor has
    /// fired, so the `stage_and_flush` disconnect gate
    /// (`pvxs/ioc/pvalink_lset.cpp:609`) lets non-retry writes stage without a
    /// live server. The disconnected case uses [`PvaLink::for_test`].
    fn connected_out(defer: bool, retry: bool) -> PvaLink {
        let (link, flag) = PvaLink::for_test_with_monitor_flag(out_cfg(defer, retry), None);
        flag.store(true, Ordering::Release);
        link
    }

    /// Read the single staged value (tests stage exactly one field).
    fn sole_staged(link: &PvaLink) -> Option<QueuedPut> {
        link.out_scratch
            .lock()
            .values()
            .next()
            .map(|s| s.value.clone())
    }

    #[tokio::test]
    async fn b4_defer_coalesces_to_most_recent() {
        // Connected OUT link: the disconnect gate lets these non-retry
        // deferred writes stage so the most-recent-wins coalescing is
        // what's exercised, not the gate.
        let link = connected_out(true, false);
        assert_eq!(link.staged_count(), 0);
        // defer=true: write stages, returns Ok without a server.
        link.write("42").await.expect("deferred write is Ok");
        assert_eq!(link.staged_count(), 1);
        // A second deferred write to the SAME field supersedes the first
        // — one staged value per field (most-recent-PUT wins), not a
        // FIFO log of stale intermediate writes.
        link.write_pv_field(&PvField::Scalar(ScalarValue::Double(1.0)))
            .await
            .expect("deferred typed write is Ok");
        assert_eq!(link.staged_count(), 1);
        // The retained value is the most recent (the typed Double),
        // not the superseded "42".
        match sole_staged(&link) {
            Some(QueuedPut::Field(PvField::Scalar(ScalarValue::Double(d)))) => assert_eq!(d, 1.0),
            other => panic!("most-recent write must be retained, got {other:?}"),
        }
    }

    /// MINOR (pvalink string-PUT): a deferred string `write` is staged
    /// as a `QueuedPut::Str`, NOT a `PvField::Scalar(String)`. The
    /// replay then goes through the string `pvput` path, which coerces
    /// the text against the channel's native scalar type — replaying a
    /// String field to a numeric record was the bug. A typed
    /// `write_pv_field` is staged as `QueuedPut::Field` verbatim.
    #[tokio::test]
    async fn minor_deferred_string_put_keeps_string_form() {
        let str_link = connected_out(true, false);
        str_link.write("42").await.unwrap();
        match sole_staged(&str_link) {
            Some(QueuedPut::Str(s)) => assert_eq!(s, "42"),
            other => panic!("string write must stage QueuedPut::Str, got {other:?}"),
        }

        let field_link = connected_out(true, false);
        field_link
            .write_pv_field(&PvField::Scalar(ScalarValue::Double(1.0)))
            .await
            .unwrap();
        match sole_staged(&field_link) {
            Some(QueuedPut::Field(PvField::Scalar(ScalarValue::Double(d)))) => assert_eq!(d, 1.0),
            other => panic!("typed write must stage QueuedPut::Field, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn b4_retry_queues_on_disconnect() {
        // retry=true, no server reachable → write should queue rather
        // than error, and arm the retry-pending flag for the drain.
        let link = PvaLink::for_test(out_cfg(false, true), None);
        let r = link.write("7").await;
        assert!(r.is_ok(), "retry write should queue, got {r:?}");
        assert_eq!(link.staged_count(), 1);
        assert!(
            link.retry_pending.load(Ordering::Acquire),
            "a queued retry write must arm the production drain"
        );
    }

    #[tokio::test]
    async fn b4_no_retry_surfaces_disconnect_error() {
        // retry=false, disconnected (no client) → the `stage_and_flush`
        // gate (`pvxs/ioc/pvalink_lset.cpp:609` `!retry && !valid()`) drops
        // the write before staging: surface the error, stage nothing.
        let link = PvaLink::for_test(out_cfg(false, false), None);
        let r = link.write("7").await;
        assert!(
            matches!(r, Err(PvaLinkError::Disconnected(_))),
            "non-retry write must surface a disconnect error, got {r:?}"
        );
        assert_eq!(link.staged_count(), 0);
        assert!(!link.retry_pending.load(Ordering::Acquire));
    }

    /// BRPVALINK-2 regression: a DEFERRED non-retry write to a
    /// disconnected channel must error and stage NOTHING — pvxs gates
    /// every put, deferred or not, on `!retry && !valid()` at the very
    /// top of `pvaPutValueX`, BEFORE the `if(!defer) lchan->put()`
    /// branch (`pvxs/ioc/pvalink_lset.cpp:609,653`). Pre-fix this Rust path had
    /// no such gate: a deferred write returned `Ok` and left a value in
    /// the scratch even though the upstream was unreachable, so a value
    /// that could never be delivered sat queued forever and the owning
    /// record never saw the disconnect alarm. The non-deferred sibling
    /// is covered by `b4_no_retry_surfaces_disconnect_error`.
    #[tokio::test]
    async fn defer_no_retry_to_disconnected_upstream_errors() {
        // defer=true, retry=false, disconnected (no client).
        let link = PvaLink::for_test(out_cfg(true, false), None);
        assert!(!link.is_connected(), "no client ⟹ disconnected");
        let r = link.write("9").await;
        assert!(
            matches!(r, Err(PvaLinkError::Disconnected(_))),
            "deferred non-retry write to a disconnected channel must error \
             (pre-fix it returned Ok and staged), got {r:?}"
        );
        assert_eq!(
            link.staged_count(),
            0,
            "the undeliverable value must NOT occupy the scratch"
        );
        assert!(!link.retry_pending.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn b4_flush_drops_value_when_no_retry() {
        // defer link, retry=false; staged while connected, then flushed
        // against no server. The staged value's Put fails on disconnect
        // → with retry=false it is dropped (it would fail identically on
        // every replay), leaving the scratch empty and an error for the
        // record to alarm on. Connected so the staging gate passes; the
        // failure is the real PUT (no server behind the test client),
        // i.e. this exercises the flush-time drop, not the stage gate.
        let link = connected_out(true, false);
        link.write("1").await.unwrap();
        link.write("2").await.unwrap();
        // Coalesced to one staged value ("2").
        assert_eq!(link.staged_count(), 1);
        let r = link.flush_scratch().await;
        assert!(r.is_err(), "no-retry disconnect surfaces an error");
        assert_eq!(link.staged_count(), 0);
        assert!(!link.retry_pending.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn b4_flush_retry_restores_staged() {
        // defer + retry: flush against no server → the staged value is
        // restored for a later retry and the drain is armed. A queued
        // retry is NOT an error (0 PUTs issued, queued), matching the
        // immediate-write path.
        let link = PvaLink::for_test(out_cfg(true, true), None);
        link.write("1").await.unwrap();
        link.write("2").await.unwrap();
        assert_eq!(link.staged_count(), 1);
        let r = link.flush_scratch().await;
        assert!(matches!(r, Ok(0)), "retry disconnect queues, got {r:?}");
        // retry restores the most-recent staged value ("2").
        assert_eq!(link.staged_count(), 1);
        assert!(link.retry_pending.load(Ordering::Acquire));
        match sole_staged(&link) {
            Some(QueuedPut::Str(s)) => assert_eq!(s, "2"),
            other => panic!("most-recent value must be restored, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn b4_flush_retry_keeps_newer_write() {
        // defer + retry: most-recent-PUT wins across a failing flush.
        let link = PvaLink::for_test(out_cfg(true, true), None);
        link.write("1").await.unwrap();
        let r = link.flush_scratch().await;
        assert!(matches!(r, Ok(0)));
        // A newer write supersedes the restored "1".
        link.write("2").await.unwrap();
        // A second flush restores "2" (not the older "1").
        let r2 = link.flush_scratch().await;
        assert!(matches!(r2, Ok(0)));
        match sole_staged(&link) {
            Some(QueuedPut::Str(s)) => assert_eq!(s, "2"),
            other => panic!("newer write must survive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn b4_flush_on_inp_link_rejected() {
        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), None);
        assert!(matches!(
            link.flush_scratch().await,
            Err(PvaLinkError::NotWritable)
        ));
    }

    #[test]
    fn b3_take_notify_rx_only_once() {
        // INP+monitor link built via for_test has no channel (no live
        // monitor), so take_notify_rx is None — exercised here for
        // the OUT / non-monitor branch. The live-channel path is
        // covered by the integration-side forwarder test.
        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), None);
        assert!(link.take_notify_rx().is_none());
    }

    // ---- B-pvalink-restart: INP monitor disconnect reflection ----

    /// BUG 2 regression: `is_connected()` for an INP+monitor link is
    /// driven by the monitor task's live-connection flag, so an
    /// upstream disconnect (IOC restart / transient I/O) is reflected
    /// even though a value is still cached. Pre-fix `is_connected()`
    /// returned `latest.is_some()`, which stayed `true` forever once
    /// any value had been cached.
    #[test]
    fn bug2_is_connected_reflects_monitor_disconnect() {
        // Link with a cached value (a prior event) but the monitor
        // flag still false — not yet (re)connected.
        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(PvField::Scalar(ScalarValue::Double(1.0))),
        );
        assert!(
            !link.is_connected(),
            "cached value alone must NOT report connected"
        );

        // Monitor delivers an event → flag flips true.
        flag.store(true, Ordering::Release);
        assert!(link.is_connected(), "live subscription reports connected");

        // Upstream subscription ends (IOC restart) → flag false, even
        // though `latest` still holds the stale cached value.
        flag.store(false, Ordering::Release);
        assert!(
            link.latest_value().is_some(),
            "stale value is still cached after disconnect"
        );
        assert!(
            !link.is_connected(),
            "disconnect must be reflected despite the stale cached value"
        );

        // Re-subscribe loop delivers a fresh event → connected again.
        flag.store(true, Ordering::Release);
        assert!(link.is_connected(), "re-subscribe restores connected");
    }

    // ---- disconnected monitor must not serve stale ----
    // Tested by invariant boundary, not by narrative: the four
    // (cached, connected) corners each get one case.

    /// Boundary (cached=Some, connected=false): a monitor link that
    /// HAD a value but whose subscription is now down. Every read
    /// hook MUST refuse the stale cache; the alarm hooks MUST report
    /// LINK/INVALID regardless of the link's NMS mode (a disconnect is
    /// a link failure, not a remote-severity propagation NMS suppresses).
    /// The diagnostic `latest_value` slot survives.
    #[tokio::test]
    async fn fr13_disconnected_monitor_read_fails_and_reports_invalid() {
        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(nt_with_alarm(0, None)),
        );
        // Connected once, then dropped.
        flag.store(true, Ordering::Release);
        flag.store(false, Ordering::Release);

        // Value reads refuse the stale cache.
        assert!(
            matches!(link.read().await, Err(PvaLinkError::Disconnected(_))),
            "disconnected monitor read must fail, not return the stale value"
        );
        assert!(
            link.try_read_cached().is_none(),
            "sync fast path must return None while disconnected"
        );

        // Alarm hooks report INVALID(3) + a disconnect message even
        // though the link is NMS (which suppresses remote severities).
        assert_eq!(
            link.link_alarm_severity_with("", SevrMode::Nms),
            Some(3),
            "disconnect must surface INVALID regardless of NMS"
        );
        assert_eq!(
            link.alarm_message_with("", SevrMode::Nms).as_deref(),
            Some("pvalink monitor disconnected"),
            "disconnect message, not the stale remote alarm string"
        );

        // The cached value itself is preserved for diagnostics/timestamp.
        assert!(
            link.latest_value().is_some(),
            "stale value retained for diagnostic accessors"
        );
    }

    /// Boundary (cached=Some, connected=true): a live monitor link
    /// serves its cached value and propagates the MS-gated remote
    /// severity exactly as before — the FR-13 gate is inert when
    /// connected.
    #[tokio::test]
    async fn fr13_connected_monitor_serves_cached_value_and_remote_alarm() {
        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Ms),
            Some(nt_with_alarm(2, Some("hot"))),
        );
        flag.store(true, Ordering::Release);

        assert!(link.read().await.is_ok(), "live monitor read succeeds");
        assert!(
            link.try_read_cached().is_some(),
            "live monitor fast path returns the cached value"
        );
        // MS link, remote MAJOR(2) → propagates 2, not the disconnect INVALID.
        assert_eq!(link.link_alarm_severity_with("", SevrMode::Ms), Some(2));
        assert_eq!(
            link.alarm_message_with("", SevrMode::Ms).as_deref(),
            Some("hot")
        );
    }

    /// Boundary (cached=None, connected=false): a monitor link that
    /// NEVER connected. Reads fail (no GET-bootstrap for monitor
    /// links), but `alarm_severity` returns `None` — NOT INVALID — so
    /// the base `external_link_alarm` bare-name loop, which tries every
    /// registered lset, does not let a PVA lset mis-claim a name a
    /// sibling lset owns. The never-connected INVALID is owned by the
    /// value path (`get_value` → None → base LINK/INVALID, gated by the
    /// parsed link type).
    #[tokio::test]
    async fn fr13_never_connected_monitor_reports_no_alarm_but_read_fails() {
        let (link, _flag) = PvaLink::for_test_with_monitor_flag(inp_cfg(SevrMode::Ms), None);
        assert!(
            matches!(link.read().await, Err(PvaLinkError::Disconnected(_))),
            "never-connected monitor read fails (no stale, no GET-bootstrap)"
        );
        assert!(link.try_read_cached().is_none());
        assert_eq!(
            link.link_alarm_severity_with("", SevrMode::Ms),
            None,
            "never-connected link must not claim INVALID via the alarm hook"
        );
        assert_eq!(link.alarm_message_with("", SevrMode::Ms), None);
    }

    /// Boundary (cached=Some, connected=false): the FR-13 stale gate is
    /// a defect *family* — it must cover every lset getter that
    /// surfaces cached NT-derived state, not just value/alarm. pvxs
    /// gates each metadata getter through `CHECK_VALID`, so a
    /// disconnected monitor's `time_stamp` (`pvaGetTimeStampTag`) and
    /// `link_metadata` (`pvaGetGraphicLimits`/`pvaGetDBFtype`/…) must
    /// report no-data while the snapshot is retained — exactly like the
    /// value read. Pre-fix these two getters served the stale latched
    /// metadata/timestamp. The existing `for_test(None)` case
    /// only covers cached=None; this covers cached=Some+disconnected.
    #[tokio::test]
    async fn fr13_disconnected_monitor_metadata_and_timestamp_refuse_stale() {
        // NT value carrying a timeStamp slot and a display sub-structure
        // so both getters have something to (wrongly) serve if ungated.
        let mut ts = PvStructure::new("time_t");
        ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(1_700_000_000)),
        ));
        ts.fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(42))));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("display".into(), nt_display(-9.0, 9.0, "arb", "linked", 2)));
        root.fields
            .push(("timeStamp".into(), PvField::Structure(ts)));

        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(PvField::Structure(root)),
        );

        // Connected: both getters serve the cached metadata/timestamp
        // (the FR-13 gate is inert while connected).
        flag.store(true, Ordering::Release);
        assert_eq!(
            link.time_stamp(""),
            Some((1_700_000_000, 42, 0)),
            "connected monitor surfaces the cached timestamp; no userTag \
             field in the fixture, so the tag defaults to 0"
        );
        assert!(
            link.link_metadata().is_some(),
            "connected monitor surfaces the cached metadata"
        );

        // Disconnected: the metadata getter refuses the stale snapshot
        // (still gated through CHECK_VALID), and the value-read time hook
        // refuses the stale *last value* timestamp. With no disconnect
        // event yet recorded the time hook yields None (keep local time),
        // never the stale remote 1_700_000_000.
        flag.store(false, Ordering::Release);
        assert!(
            link.time_stamp("").is_none(),
            "disconnected monitor with no recorded disconnect time keeps \
             local time, never the stale latched remote timestamp"
        );
        assert!(
            link.link_metadata().is_none(),
            "disconnected monitor must not serve the stale latched metadata"
        );
        assert!(
            link.latest_value().is_some(),
            "snapshot retained (pvxs keeps root; CHECK_VALID gates the getters)"
        );

        // Once the circuit-drop time is recorded (pvxs `snap_time =
        // e.time`), a `time=true` read adopts THAT moment — not the
        // stale value's 1_700_000_000 — carrying the last value's
        // userTag (0 in this fixture). The metadata getter stays gated.
        link.record_disconnect_time_for_test(1_800_000_500, 7);
        assert_eq!(
            link.time_stamp(""),
            Some((1_800_000_500, 7, 0)),
            "disconnected monitor adopts the disconnect-event time, not \
             the stale remote value timestamp"
        );
        assert!(
            link.link_metadata().is_none(),
            "metadata getter stays gated even after a disconnect time is \
             recorded"
        );
    }

    /// On a disconnect-stale `time=true` read the adopted timestamp is
    /// the disconnect-event moment, but the `userTag` is carried over
    /// from the last cached value — pvxs leaves `snap_tag` untouched in
    /// `onDisconnect` (`pvxs/ioc/pvalink_channel.cpp:372` sets only `snap_time`),
    /// so the tag the record adopts on the invalid read
    /// (`pvxs/ioc/pvalink_lset.cpp:268-270`) is whatever the prior connected read
    /// latched. A fixture with a non-zero tag proves the value's tag —
    /// not 0, and not the stale value seconds — survives the adoption.
    #[tokio::test]
    async fn pvalink_disconnect_time_carries_last_value_usertag() {
        let mut ts = PvStructure::new("time_t");
        ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(1_700_000_000)),
        ));
        ts.fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(42))));
        ts.fields
            .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(0x55))));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("timeStamp".into(), PvField::Structure(ts)));

        let (link, flag) = PvaLink::for_test_with_monitor_flag(
            inp_cfg(SevrMode::Nms),
            Some(PvField::Structure(root)),
        );

        // Connected: the full remote timestamp+tag.
        flag.store(true, Ordering::Release);
        assert_eq!(link.time_stamp(""), Some((1_700_000_000, 42, 0x55)));

        // Disconnected with a recorded drop time: adopt the drop time
        // (seconds/nanos), but keep the last value's userTag 0x55.
        flag.store(false, Ordering::Release);
        link.record_disconnect_time_for_test(1_800_000_500, 7);
        assert_eq!(
            link.time_stamp(""),
            Some((1_800_000_500, 7, 0x55)),
            "disconnect-event time with the last value's userTag preserved"
        );
    }

    /// The remote `timeStamp.userTag` is a signed int32 on the wire, but
    /// the adopted record tag is a 64-bit `epicsUTag`. Widening must
    /// zero-extend (`as u32 as u64`), never sign-extend: a bit-31 tag
    /// like 0x9000_0000 must widen to 0x0000_0000_9000_0000, not
    /// 0xFFFF_FFFF_9000_0000. This is the pvData `as<uint32_t>()` (not
    /// `as<epicsUTag>()`) read at `pvxs/ioc/pvalink_lset.cpp:406-409`. Boundaries:
    /// absent tag, small positive, and the bit-31 sign boundary on both
    /// the signed `Int` and unsigned `UInt` carriers.
    #[tokio::test]
    async fn pvalink_usertag_widens_without_sign_extension() {
        // Build a connected monitor link whose cached NT value carries a
        // timeStamp with the given userTag field, then read the tag.
        let read_tag = |usertag: Option<PvField>| -> Option<u64> {
            let mut ts = PvStructure::new("time_t");
            ts.fields.push((
                "secondsPastEpoch".into(),
                PvField::Scalar(ScalarValue::Long(1_700_000_000)),
            ));
            ts.fields
                .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(42))));
            if let Some(f) = usertag {
                ts.fields.push(("userTag".into(), f));
            }
            let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
            root.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
            root.fields
                .push(("timeStamp".into(), PvField::Structure(ts)));
            let (link, flag) = PvaLink::for_test_with_monitor_flag(
                inp_cfg(SevrMode::Nms),
                Some(PvField::Structure(root)),
            );
            flag.store(true, Ordering::Release);
            link.time_stamp("").map(|(_, _, utag)| utag)
        };

        // Absent userTag → 0 (pvxs `else snap_tag = 0`).
        assert_eq!(read_tag(None), Some(0), "absent userTag adopts tag 0");
        // Small positive signed tag → identity.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::Int(5)))),
            Some(5),
            "small positive userTag widens to itself"
        );
        // bit-31 set on the signed `Int` carrier: 0x9000_0000 as i32 is
        // negative, so a naive `as u64` would sign-extend.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::Int(
                0x9000_0000u32 as i32
            )))),
            Some(0x0000_0000_9000_0000),
            "bit-31 signed userTag must zero-extend, not sign-extend to \
             0xFFFF_FFFF_9000_0000"
        );
        // int32 -1 widens to 0xFFFF_FFFF, not u64::MAX.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::Int(-1)))),
            Some(0x0000_0000_FFFF_FFFF),
            "int32 -1 userTag must widen to 0xFFFF_FFFF, not 0xFFFF_FFFF_FFFF_FFFF"
        );
        // Same boundary on the unsigned `UInt` carrier.
        assert_eq!(
            read_tag(Some(PvField::Scalar(ScalarValue::UInt(0x9000_0000)))),
            Some(0x0000_0000_9000_0000),
            "bit-31 unsigned userTag widens cleanly"
        );
    }

    /// BUG 2: a live INP+monitor link spawns the re-subscribe loop and
    /// installs the `monitor_connected` flag. Before the first event
    /// the link reports disconnected (no liveness proven yet); the
    /// monitor task is present so a later disconnect is observable.
    #[tokio::test]
    async fn bug2_inp_monitor_link_installs_connection_flag() {
        // INP + monitor against a PV with no server reachable. `open`
        // spawns the re-subscribe loop; `pvmonitor` fails fast and the
        // loop backs off — `is_connected()` stays false (no event
        // delivered) instead of being absent/true.
        let cfg = PvaLinkConfig {
            monitor: true,
            ..PvaLinkConfig::defaults_for("BUG2:NOPV", LinkDirection::Inp)
        };
        let link = PvaLink::open(
            cfg,
            PvaClient::builder().timeout(Duration::from_secs(1)).build(),
        )
        .await
        .expect("open INP monitor link");
        assert!(
            link.monitor_connected.is_some(),
            "INP+monitor link must install the live-connection flag"
        );
        assert!(
            !link.is_connected(),
            "no event delivered yet → not connected"
        );
    }

    #[test]
    fn b4_is_disconnect_classification() {
        use epics_pva_rs::error::PvaError;
        assert!(is_disconnect(&PvaError::Timeout));
        assert!(is_disconnect(&PvaError::ConnectionRefused));
        assert!(is_disconnect(&PvaError::ChannelNotFound("x".into())));
        // value / protocol rejections are NOT disconnects.
        assert!(!is_disconnect(&PvaError::InvalidValue("x".into())));
        assert!(!is_disconnect(&PvaError::Protocol("x".into())));
        assert!(!is_disconnect(&PvaError::Decode("x".into())));
    }

    // ---- pvalink DB-link metadata hooks ----

    /// Build a numeric `display` sub-structure with limitLow/limitHigh,
    /// units, description and precision.
    fn nt_display(lo: f64, hi: f64, units: &str, desc: &str, prec: i32) -> PvField {
        let mut d = PvStructure::new("");
        d.fields
            .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
        d.fields
            .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
        d.fields.push((
            "units".into(),
            PvField::Scalar(ScalarValue::String(units.into())),
        ));
        d.fields.push((
            "description".into(),
            PvField::Scalar(ScalarValue::String(desc.into())),
        ));
        d.fields
            .push(("precision".into(), PvField::Scalar(ScalarValue::Int(prec))));
        PvField::Structure(d)
    }

    /// Build a numeric `control` sub-structure with limitLow/limitHigh.
    fn nt_control(lo: f64, hi: f64) -> PvField {
        let mut c = PvStructure::new("");
        c.fields
            .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
        c.fields
            .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
        PvField::Structure(c)
    }

    /// Build a `valueAlarm` sub-structure with the four limit fields.
    fn nt_value_alarm(lolo: f64, lo: f64, hi: f64, hihi: f64) -> PvField {
        let mut v = PvStructure::new("");
        v.fields.push((
            "lowAlarmLimit".into(),
            PvField::Scalar(ScalarValue::Double(lolo)),
        ));
        v.fields.push((
            "lowWarningLimit".into(),
            PvField::Scalar(ScalarValue::Double(lo)),
        ));
        v.fields.push((
            "highWarningLimit".into(),
            PvField::Scalar(ScalarValue::Double(hi)),
        ));
        v.fields.push((
            "highAlarmLimit".into(),
            PvField::Scalar(ScalarValue::Double(hihi)),
        ));
        PvField::Structure(v)
    }

    /// a pvalink must surface the linked PV's remote
    /// display / control / valueAlarm metadata, DBF type and element
    /// count through the DB-link metadata hook — the Rust counterpart
    /// of the pvxs pvalink lset metadata getters installed in
    /// `pva_lset` (`pvxs/ioc/pvalink_lset.cpp:715-719`) and exercised by
    /// `pvxs/test/testpvalink.cpp:437-454`.
    ///
    /// The cached NT value carries the same metadata shape and the
    /// same numbers `pvxs/test/testpvalink.cpp:437-454` asserts (graphic -9/9,
    /// control -10/10, alarm -8/-7/7/8, precision 2, units "arb",
    /// scalar element count 1). Pre-fix `PvaLink` had no
    /// `link_metadata` accessor and `LinkSet` had no metadata hook, so
    /// every one of these was invisible to DB link callers.
    #[test]
    fn br_r24_link_metadata_surfaces_remote_display_control_valuealarm() {
        use epics_base_rs::server::database::LinkDbfType;

        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("display".into(), nt_display(-9.0, 9.0, "arb", "linked", 2)));
        root.fields
            .push(("control".into(), nt_control(-10.0, 10.0)));
        root.fields
            .push(("valueAlarm".into(), nt_value_alarm(-8.0, -7.0, 7.0, 8.0)));

        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), Some(PvField::Structure(root)));
        let meta = link
            .link_metadata()
            .expect("connected link must expose metadata");

        assert_eq!(meta.dbf_type, Some(LinkDbfType::Double), "DBF type");
        assert_eq!(meta.element_count, Some(1), "scalar element count");
        assert_eq!(meta.graphic_limits, Some((-9.0, 9.0)), "graphic limits");
        assert_eq!(meta.control_limits, Some((-10.0, 10.0)), "control limits");
        assert_eq!(
            meta.alarm_limits,
            Some((-8.0, -7.0, 7.0, 8.0)),
            "alarm limits (lolo, lo, hi, hihi)"
        );
        assert_eq!(meta.precision, Some(2), "display precision");
        assert_eq!(meta.units.as_deref(), Some("arb"), "display units");
        assert_eq!(
            meta.description.as_deref(),
            Some("linked"),
            "display description"
        );
    }

    /// A link whose `field` selects a nested member must expose THAT
    /// member's display/control/valueAlarm metadata, not the parent
    /// PV's. pvxs derives `fld_meta` from the same selected root as
    /// `fld_value` (`pvxs/ioc/pvalink_link.cpp:90-110`,
    /// `pvxs/ioc/pvalink_lset.cpp:444-540`). Pre-fix `link_metadata_with`
    /// used the selected field only for DBF type/element count and read
    /// every metadata limit/unit/precision from the top-level root,
    /// mixing the container's engineering metadata with the member's
    /// value domain.
    #[test]
    fn link_metadata_resolves_display_control_valuealarm_at_selected_field_root() {
        use epics_base_rs::server::database::LinkDbfType;

        // Nested member NTScalar with its OWN distinct metadata.
        let mut member = PvStructure::new("epics:nt/NTScalar:1.0");
        member
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.0))));
        member.fields.push((
            "display".into(),
            nt_display(-90.0, 90.0, "MEM", "member", 4),
        ));
        member
            .fields
            .push(("control".into(), nt_control(-100.0, 100.0)));
        member.fields.push((
            "valueAlarm".into(),
            nt_value_alarm(-80.0, -70.0, 70.0, 80.0),
        ));

        // Top-level NTScalar with DIFFERENT metadata + the member.
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        root.fields
            .push(("display".into(), nt_display(-9.0, 9.0, "TOP", "top", 2)));
        root.fields
            .push(("control".into(), nt_control(-10.0, 10.0)));
        root.fields
            .push(("valueAlarm".into(), nt_value_alarm(-8.0, -7.0, 7.0, 8.0)));
        root.fields
            .push(("member".into(), PvField::Structure(member)));
        let value = PvField::Structure(root);

        // field="member": every metadatum comes from the member.
        let cfg = PvaLinkConfig {
            field: "member".to_string(),
            ..PvaLinkConfig::defaults_for("META:PV", LinkDirection::Inp)
        };
        let meta = PvaLink::for_test(cfg, Some(value.clone()))
            .link_metadata()
            .expect("metadata present");
        assert_eq!(meta.dbf_type, Some(LinkDbfType::Double));
        assert_eq!(meta.element_count, Some(1));
        assert_eq!(
            meta.graphic_limits,
            Some((-90.0, 90.0)),
            "graphic limits must come from the selected member"
        );
        assert_eq!(
            meta.control_limits,
            Some((-100.0, 100.0)),
            "control limits must come from the selected member"
        );
        assert_eq!(
            meta.alarm_limits,
            Some((-80.0, -70.0, 70.0, 80.0)),
            "alarm limits must come from the selected member"
        );
        assert_eq!(meta.precision, Some(4), "member precision");
        assert_eq!(meta.units.as_deref(), Some("MEM"), "member units");
        assert_eq!(meta.description.as_deref(), Some("member"));

        // field="": every metadatum comes from the top-level root.
        let cfg_top = PvaLinkConfig::defaults_for("META:PV", LinkDirection::Inp);
        let meta_top = PvaLink::for_test(cfg_top, Some(value))
            .link_metadata()
            .expect("metadata present");
        assert_eq!(meta_top.graphic_limits, Some((-9.0, 9.0)));
        assert_eq!(meta_top.control_limits, Some((-10.0, 10.0)));
        assert_eq!(meta_top.alarm_limits, Some((-8.0, -7.0, 7.0, 8.0)));
        assert_eq!(meta_top.precision, Some(2));
        assert_eq!(meta_top.units.as_deref(), Some("TOP"));
        assert_eq!(meta_top.description.as_deref(), Some("top"));
    }

    /// A scalar/array field selection has no nested metadata
    /// sub-structures, so the metadata root is empty and no
    /// display/control/valueAlarm is surfaced — the metadata scalar
    /// value must NOT be misread as a graphic/alarm limit. (`extract_field`
    /// on a non-structure returns the scalar itself; the structural
    /// guard in `link_metadata_with` prevents that scalar from leaking
    /// into the limit reads.)
    #[test]
    fn link_metadata_scalar_field_has_no_nested_metadata() {
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(42.0))));
        root.fields
            .push(("display".into(), nt_display(-9.0, 9.0, "TOP", "top", 2)));

        // Select the bare scalar `value` field directly.
        let cfg = PvaLinkConfig {
            field: "value".to_string(),
            ..PvaLinkConfig::defaults_for("META:PV", LinkDirection::Inp)
        };
        let meta = PvaLink::for_test(cfg, Some(PvField::Structure(root)))
            .link_metadata()
            .expect("metadata present");
        assert_eq!(
            meta.graphic_limits, None,
            "a scalar field selection exposes no display limits"
        );
        assert_eq!(
            meta.units, None,
            "a scalar field selection exposes no units"
        );
        assert_eq!(meta.precision, None);
    }

    /// a not-yet-connected link (no cached value) reports no
    /// metadata — the record then keeps its local defaults. And an
    /// NTEnum value maps to `DBF_ENUM` with element count 1.
    #[test]
    fn br_r24_link_metadata_none_when_disconnected_and_enum_maps_to_dbf_enum() {
        use epics_base_rs::server::database::LinkDbfType;

        let disconnected = PvaLink::for_test(inp_cfg(SevrMode::Nms), None);
        assert!(
            disconnected.link_metadata().is_none(),
            "no cached value → no metadata snapshot"
        );

        // NTEnum: `value` is a struct with an integer `index` and a
        // `choices` string array → DBF_ENUM.
        let mut enum_value = PvStructure::new("enum_t");
        enum_value
            .fields
            .push(("index".into(), PvField::Scalar(ScalarValue::Int(1))));
        enum_value.fields.push((
            "choices".into(),
            PvField::ScalarArray(vec![
                ScalarValue::String("OFF".into()),
                ScalarValue::String("ON".into()),
            ]),
        ));
        let mut root = PvStructure::new("epics:nt/NTEnum:1.0");
        root.fields
            .push(("value".into(), PvField::Structure(enum_value)));
        let link = PvaLink::for_test(inp_cfg(SevrMode::Nms), Some(PvField::Structure(root)));
        let meta = link.link_metadata().expect("connected");
        assert_eq!(meta.dbf_type, Some(LinkDbfType::Enum), "NTEnum → DBF_ENUM");
        assert_eq!(meta.element_count, Some(1), "enum index element count");
    }

    /// pvalink OUT writes must carry proc/block/field options in
    /// the PUT pvRequest.
    ///
    /// On main: `build_put_request` did not exist — no pvRequest was built
    /// and `record._options.process` / `block` never reached the server.
    /// After fix: `build_put_request` produces the correct pvRequest, and
    /// `is_subfield` gates field-targeted vs. value-targeted dispatch.
    ///
    /// pvxs parity:
    ///   pvxs/ioc/pvalink_channel.cpp:31-38 (putReq template)
    ///   pvxs/ioc/pvalink_channel.cpp:220-263 (runtime process/block computation)
    ///   pvxs/ioc/pvalink_channel.cpp:138 (field targeting via top[fieldName])
    #[test]
    fn br_r11_pvalink_out_options_preserved() {
        // proc=PP → process="true" (typed String wire descriptor)
        let req = build_put_request(ProcMode::Pp, false);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "process" && *v == ScalarValue::String("true".into())),
            "proc=PP must produce process=true in pvRequest"
        );

        // proc=Default → process="passive" (distinct from NPP)
        let req = build_put_request(ProcMode::Default, false);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "process" && *v == ScalarValue::String("passive".into())),
            "proc=Default must produce process=passive in pvRequest"
        );

        // block=true must appear in pvRequest as a typed boolean
        let req = build_put_request(ProcMode::Pp, true);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "block" && *v == ScalarValue::Boolean(true)),
            "block=true must appear in pvRequest"
        );

        // block=false must appear in pvRequest as a typed boolean
        let req = build_put_request(ProcMode::Default, false);
        assert!(
            req.record_options
                .iter()
                .any(|(k, v)| k == "block" && *v == ScalarValue::Boolean(false)),
            "block=false must appear in pvRequest"
        );

        // field="" and field="value" are NOT sub-fields (use pvput_with_request)
        assert!(!is_subfield(""), "empty field is not a sub-field");
        assert!(!is_subfield("value"), "\"value\" is not a sub-field");

        // field="DESC" IS a sub-field (use pvput_field_with_request)
        assert!(
            is_subfield("DESC"),
            "\"DESC\" must be treated as a sub-field"
        );
        assert!(is_subfield("alarm.severity"), "dotted path is a sub-field");

        // A deferred write with field="DESC" and proc=PP queues the
        // value; when flushed the replay uses build_put_request.
        // Verify the defer path still enqueues (does not bypass).
        let cfg = PvaLinkConfig {
            field: "DESC".to_string(),
            proc: ProcMode::Pp,
            defer: true,
            ..PvaLinkConfig::defaults_for("BR11:PV", LinkDirection::Out)
        };
        // Connected so the disconnect gate lets the deferred write
        // stage (we're checking the defer path enqueues, not the gate).
        let (link, flag) = PvaLink::for_test_with_monitor_flag(cfg, None);
        flag.store(true, Ordering::Release);
        // Deferred write queues without hitting the network.
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                link.write_with_block("hello", true)
                    .await
                    .expect("deferred write_with_block must enqueue");
            });
        assert_eq!(link.staged_count(), 1, "one entry staged");
    }

    /// An upstream that dies must disconnect the INP link and drive one
    /// disconnect scan — even though the client's monitor RE-SUBSCRIBES
    /// INTERNALLY and its future never returns.
    ///
    /// `op_monitor_events` handles `MonitorEnd::ConnectionLost` by pushing
    /// `MonitorEvent::Disconnected` and looping, so a subscriber that infers
    /// the disconnect from "the subscription future returned" learns nothing
    /// when the peer dies. Measured on the RTEMS stage-5 target: with the
    /// upstream IOC killed, the client correctly reported
    /// `conn alive=false channels=[]`, `active=0 searching=2`, while both
    /// downstream records still read
    /// `SEVR=0 STAT=0` at their stale value and the link still claimed
    /// `connected=true`.
    ///
    /// This is the end-to-end shape of that: a real server, a real
    /// subscription, the server dropped underneath it.
    // Reactor-dependent, and unusually so — it is the RE-dial that needs the
    // reactor, not the first one. Losing the peer makes the client re-dial
    // from a background-executor thread, and on the exec backend that thread
    // has no tokio reactor while `dial_pva`'s hosted arm is still
    // `tokio::net` (`TcpStream::connect` → `PollEvented::new` → "no reactor
    // running"). The feature swaps the executor, not the transport; the
    // target does not have this shape at all, because `exec_backend` (which
    // every `epics_embedded_target` build sets) selects the blocking dial.
    // Gated out on the exec backend (stage 3); the on-target boot is what
    // proves this path there.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn upstream_death_disconnects_the_inp_monitor_link() {
        use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
        use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

        let pv = SharedPV::build_mailbox();
        pv.open(
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
            },
            PvField::Structure(PvStructure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), PvField::Scalar(ScalarValue::Double(1.0)))],
            }),
        )
        .unwrap();
        let source = SharedSource::new();
        source.add("DISC:PV", pv);
        let server = PvaServer::isolated(Arc::new(source)).expect("test PVA server must start");
        let addr = server.tcp_addr();

        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let cfg = PvaLinkConfig {
            monitor: true,
            proc: ProcMode::Cp,
            scan_on_update: true,
            ..PvaLinkConfig::defaults_for("DISC:PV", LinkDirection::Inp)
        };
        let link = PvaLink::open(cfg, client).await.expect("link opens");
        let mut rx = link
            .take_notify_rx()
            .expect("an INP+monitor link has a channel");

        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the initial monitor update must arrive")
            .expect("the monitor channel must stay open");
        assert!(
            matches!(first, ScanEvent::Value(_)),
            "the first trigger is the initial value, got {first:?}"
        );
        assert!(link.is_connected(), "a live subscription reports connected");

        drop(server);

        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect(
                "a dead upstream must enqueue a disconnect scan trigger; the monitor \
                 future does not return — it re-subscribes internally",
            )
            .expect("the monitor channel must stay open");
        assert!(
            matches!(ev, ScanEvent::Disconnected),
            "the trigger after the upstream dies must be Disconnected, got {ev:?}"
        );
        assert!(
            !link.is_connected(),
            "a dead upstream must not keep reporting connected"
        );
    }

    #[cfg(tokio_backend)]
    /// a typed (`PvField`) OUT write to a query-bearing link
    /// `pva://PV?field=<subfield>` must land the typed value on the
    /// selected sub-field, not on the root `value`. Pre-fix
    /// `write_pv_field` always routed through `pvput_pv_field_*`
    /// (root-targeted), so a typed array write clobbered `value` and
    /// left the requested sub-field untouched. pvxs `linkBuildPut`
    /// (`pvxs/ioc/pvalink_channel.cpp:138`) targets `top[fieldName]`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mr_r4_typed_field_put_targets_subfield() {
        use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
        #[cfg(tokio_backend)]
        use epics_pva_rs::server_native::PvaServer;
        use epics_pva_rs::server_native::{SharedPV, SharedSource};

        // Structure PV with a root `value` array and an `aux` array
        // sub-field, built as a writable mailbox so the field-targeted
        // PUT stores and posts. A plain `SharedPV::new()` rejects every
        // PUT ("PUT not supported by this PV" — pvxs `sharedpv.cpp:209-227`
        // makes a handler-less SharedPV non-writable), which would mask
        // what this test checks: that a typed write lands in `aux`.
        let desc = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("value".into(), FieldDesc::ScalarArray(ScalarType::Long)),
                ("aux".into(), FieldDesc::ScalarArray(ScalarType::Long)),
            ],
        };
        let initial = PvField::Structure(PvStructure {
            struct_id: "structure".into(),
            fields: vec![
                (
                    "value".into(),
                    PvField::ScalarArray(vec![ScalarValue::Long(1), ScalarValue::Long(2)]),
                ),
                (
                    "aux".into(),
                    PvField::ScalarArray(vec![ScalarValue::Long(7), ScalarValue::Long(8)]),
                ),
            ],
        });
        let pv = SharedPV::build_mailbox();
        pv.open(desc, initial).unwrap();
        let source = SharedSource::new();
        source.add("MR_R4:PV", pv.clone());
        let server = PvaServer::isolated(Arc::new(source)).expect("test PVA server must start");
        let addr = server.tcp_addr();

        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let cfg = PvaLinkConfig {
            field: "aux".to_string(),
            ..PvaLinkConfig::defaults_for("MR_R4:PV", LinkDirection::Out)
        };
        let link = PvaLink::for_test_with_client(cfg, client);

        // Typed OUT write of a Long array to the `aux` sub-field.
        link.write_pv_field(&PvField::ScalarArray(vec![
            ScalarValue::Long(100),
            ScalarValue::Long(200),
            ScalarValue::Long(300),
        ]))
        .await
        .expect("typed field-targeted write must succeed");

        tokio::time::sleep(Duration::from_millis(80)).await;

        let current = pv.current().expect("PV has a current value");
        let PvField::Structure(s) = current else {
            panic!("expected structure value");
        };
        // Normalize an array field to `Vec<i64>` — the wire
        // round-trip may yield `ScalarArrayTyped`, logically equal to
        // a `ScalarArray` of the same elements.
        fn longs(field: &PvField) -> Vec<i64> {
            let scalars = match field {
                PvField::ScalarArray(v) => v.clone(),
                PvField::ScalarArrayTyped(t) => t.to_scalar_values(),
                other => panic!("expected an array field, got {other:?}"),
            };
            scalars
                .into_iter()
                .map(|sv| match sv {
                    ScalarValue::Long(x) => x,
                    other => panic!("expected Long element, got {other:?}"),
                })
                .collect()
        }

        let aux = s.get_field("aux").expect("aux sub-field present");
        assert_eq!(
            longs(aux),
            vec![100, 200, 300],
            "typed write must update the `aux` sub-field"
        );
        let value = s.get_field("value").expect("value sub-field present");
        assert_eq!(
            longs(value),
            vec![1, 2],
            "root `value` must be untouched by a field-targeted write"
        );
    }

    /// Combined-PUT process precedence (`pvxs/ioc/pvalink_channel.cpp:257-263`):
    /// any PP/CP/CPP forces processing over NPP; a bare `Default` leaves
    /// the remote default. PP wins when PP and NPP both contribute.
    #[test]
    fn combine_proc_precedence() {
        use ProcMode::*;
        // PP/CP/CPP force processing, even alongside NPP/Default.
        assert_eq!(combine_proc([Npp, Pp].into_iter()), Pp);
        assert_eq!(combine_proc([Default, Cp].into_iter()), Pp);
        assert_eq!(combine_proc([Npp, Cpp, Default].into_iter()), Pp);
        // Only NPP present → explicit no-process.
        assert_eq!(combine_proc([Npp, Npp].into_iter()), Npp);
        assert_eq!(combine_proc([Default, Npp].into_iter()), Npp);
        // Only Default → remote default (passive).
        assert_eq!(combine_proc([Default, Default].into_iter()), Default);
        assert_eq!(combine_proc(std::iter::empty()), Default);
    }

    #[cfg(tokio_backend)]
    fn double_struct_desc() -> epics_pva_rs::pvdata::FieldDesc {
        use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
        FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("setpoint".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        }
    }

    #[cfg(tokio_backend)]
    fn double_struct_initial() -> PvField {
        PvField::Structure(PvStructure {
            struct_id: "structure".into(),
            fields: vec![
                ("value".into(), PvField::Scalar(ScalarValue::Double(0.0))),
                ("setpoint".into(), PvField::Scalar(ScalarValue::Double(0.0))),
            ],
        })
    }

    #[cfg(tokio_backend)]
    fn struct_double(v: &PvField, field: &str) -> f64 {
        let PvField::Structure(s) = v else {
            panic!("expected structure, got {v:?}");
        };
        match s.get_field(field).expect("field present") {
            PvField::Scalar(ScalarValue::Double(d)) => *d,
            other => panic!("expected Double at {field}, got {other:?}"),
        }
    }

    #[cfg(tokio_backend)]
    /// BRIDGE-RS OUT coalescing: two OUT links to DIFFERENT fields of one
    /// structured PV — one `defer`/NPP, one non-deferred/PP — that share
    /// one channel owner emit a SINGLE combined upstream PUT. The
    /// deferred field is NOT sent on its own; the non-deferred sibling
    /// flushes both at once (`pvxs/ioc/pvalink_channel.cpp:127-263`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn out_links_coalesce_into_one_combined_put() {
        #[cfg(tokio_backend)]
        use epics_pva_rs::server_native::PvaServer;
        use epics_pva_rs::server_native::{SharedPV, SharedSource};

        let pv = SharedPV::build_mailbox();
        pv.open(double_struct_desc(), double_struct_initial())
            .unwrap();
        let source = SharedSource::new();
        source.add("COAL:PV", pv.clone());
        let server = PvaServer::isolated(Arc::new(source)).expect("test PVA server must start");
        let addr = server.tcp_addr();

        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        // One shared channel owner; two sibling links stage onto it.
        let link = PvaLink::for_test_with_client(
            PvaLinkConfig::defaults_for("COAL:PV", LinkDirection::Out),
            client,
        );

        // Link A: field=value, defer=true, NPP — stages, does NOT send.
        let cfg_value = PvaLinkConfig {
            field: "value".to_string(),
            proc: ProcMode::Npp,
            defer: true,
            ..PvaLinkConfig::defaults_for("COAL:PV", LinkDirection::Out)
        };
        link.put_out_str(&cfg_value, "10", false)
            .await
            .expect("deferred stage is Ok");
        assert_eq!(link.staged_count(), 1, "deferred write staged, not sent");
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            struct_double(&pv.current().unwrap(), "value"),
            0.0,
            "a deferred field must NOT reach the server before the trigger"
        );

        // Link B: field=setpoint, non-deferred, PP — flushes BOTH fields
        // in one combined PUT.
        let cfg_setpoint = PvaLinkConfig {
            field: "setpoint".to_string(),
            proc: ProcMode::Pp,
            ..PvaLinkConfig::defaults_for("COAL:PV", LinkDirection::Out)
        };
        link.put_out_str(&cfg_setpoint, "20", false)
            .await
            .expect("trigger write flushes the channel");
        assert_eq!(
            link.staged_count(),
            0,
            "scratch drained by the combined PUT"
        );

        tokio::time::sleep(Duration::from_millis(80)).await;
        let current = pv.current().expect("PV value");
        assert_eq!(
            struct_double(&current, "value"),
            10.0,
            "the deferred field landed via the combined PUT"
        );
        assert_eq!(
            struct_double(&current, "setpoint"),
            20.0,
            "the triggering field landed in the same combined PUT"
        );
    }

    #[cfg(tokio_backend)]
    /// BRIDGE-RS defer drain: two `defer=true` writes to different fields
    /// are held until an explicit drain (the `LinkSet::flush_puts`
    /// production path / `flush_scratch`), then sent as ONE combined PUT
    /// (pvxs `documentation/pvalink.rst:111-113`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn deferred_fields_drain_in_one_put() {
        #[cfg(tokio_backend)]
        use epics_pva_rs::server_native::PvaServer;
        use epics_pva_rs::server_native::{SharedPV, SharedSource};

        let pv = SharedPV::build_mailbox();
        pv.open(double_struct_desc(), double_struct_initial())
            .unwrap();
        let source = SharedSource::new();
        source.add("DEFER:PV", pv.clone());
        let server = PvaServer::isolated(Arc::new(source)).expect("test PVA server must start");
        let addr = server.tcp_addr();

        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let link = PvaLink::for_test_with_client(
            PvaLinkConfig::defaults_for("DEFER:PV", LinkDirection::Out),
            client,
        );

        let defer_value = PvaLinkConfig {
            field: "value".to_string(),
            defer: true,
            ..PvaLinkConfig::defaults_for("DEFER:PV", LinkDirection::Out)
        };
        let defer_setpoint = PvaLinkConfig {
            field: "setpoint".to_string(),
            defer: true,
            ..PvaLinkConfig::defaults_for("DEFER:PV", LinkDirection::Out)
        };
        link.put_out_str(&defer_value, "11", false).await.unwrap();
        link.put_out_str(&defer_setpoint, "22", false)
            .await
            .unwrap();
        assert_eq!(
            link.staged_count(),
            2,
            "both deferred writes staged, none sent"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        let pre = pv.current().unwrap();
        assert_eq!(struct_double(&pre, "value"), 0.0, "nothing sent yet");
        assert_eq!(struct_double(&pre, "setpoint"), 0.0, "nothing sent yet");

        // Explicit drain → one combined PUT carrying both fields.
        let n = link.flush_scratch().await.expect("drain succeeds");
        assert_eq!(n, 1, "two deferred fields drain as ONE upstream PUT");
        assert_eq!(link.staged_count(), 0);

        tokio::time::sleep(Duration::from_millis(80)).await;
        let current = pv.current().unwrap();
        assert_eq!(struct_double(&current, "value"), 11.0);
        assert_eq!(struct_double(&current, "setpoint"), 22.0);
    }

    #[cfg(tokio_backend)]
    /// BRIDGE-RS retry replay: a `retry=true` write issued while the
    /// target channel does not exist is queued (not errored), and the
    /// `flush_puts` production drain (`flush_retry_pending`) replays it
    /// automatically once the PV appears (pvxs `pvalink.rst:99-100`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retry_replays_after_target_pv_appears() {
        use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
        #[cfg(tokio_backend)]
        use epics_pva_rs::server_native::PvaServer;
        use epics_pva_rs::server_native::{SharedPV, SharedSource};

        // Server is up the whole time; the channel is absent at first.
        let source = Arc::new(SharedSource::new());
        let server = PvaServer::isolated(source.clone()).expect("test PVA server must start");
        let addr = server.tcp_addr();

        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(2))
            .build();
        let cfg = PvaLinkConfig {
            retry: true,
            ..PvaLinkConfig::defaults_for("RETRY:PV", LinkDirection::Out)
        };
        let link = PvaLink::for_test_with_client(cfg, client);

        // Channel does not exist yet → disconnect → queued, not errored.
        link.write("42")
            .await
            .expect("retry write queues, not errors");
        assert_eq!(link.staged_count(), 1, "value queued for retry");
        assert!(
            link.retry_pending.load(Ordering::Acquire),
            "queued retry arms the production drain"
        );

        // The target PV appears.
        let pv = SharedPV::build_mailbox();
        pv.open(
            FieldDesc::Scalar(ScalarType::Double),
            PvField::Scalar(ScalarValue::Double(0.0)),
        )
        .unwrap();
        source.add("RETRY:PV", pv.clone());

        // Production drain replays the queued write now that it resolves.
        let n = link.flush_retry_pending().await.expect("replay");
        assert_eq!(n, 1, "the queued retry write replays as one PUT");
        assert_eq!(link.staged_count(), 0);
        assert!(!link.retry_pending.load(Ordering::Acquire));

        tokio::time::sleep(Duration::from_millis(80)).await;
        match pv.current().expect("PV value") {
            PvField::Scalar(ScalarValue::Double(d)) => assert_eq!(d, 42.0),
            other => panic!("expected Double 42, got {other:?}"),
        }
    }
}
