//! Direct database access for in-process state machines.
//!
//! Replaces CA client access with direct `PvDatabase::get_pv`/`put_pv` calls.
//! This is the Rust equivalent of C sequencer's `dbGet`/`dbPut` — no network
//! round-trip, no CA search, works immediately after iocInit.
//!
//! `DbChannel` provides get/put. `DbSubscription` provides real-time
//! monitor notifications via `RecordInstance::add_subscriber`.
//!
//! # Usage
//!
//! ```ignore
//! let ch = DbChannel::new(&db, "IOC:motor.VAL");
//! ch.put_f64_process(10.0).await;  // write + trigger processing
//! let v = ch.get_f64().await;       // read current value
//!
//! let mut sub = DbSubscription::subscribe(&db, "IOC:sensor.VAL").await.unwrap();
//! let val = sub.recv_f64().await;   // wait for next change
//! ```

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::error::CaResult;
use crate::server::event_queue::{EventReader, TryRecvError};
use crate::server::pv::MonitorEvent;
use crate::server::recgbl::EventMask;
use crate::types::{DbFieldType, EpicsValue};

use super::{PvDatabase, parse_pv_name};

static NEXT_SID: AtomicU32 = AtomicU32::new(1_000_000);
static NEXT_ORIGIN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_sid() -> u32 {
    NEXT_SID.fetch_add(1, Ordering::Relaxed)
}

/// Allocate a unique origin ID for self-write filtering.
pub fn alloc_origin() -> u64 {
    NEXT_ORIGIN.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// DbChannel — single PV get/put
// ---------------------------------------------------------------------------

/// A handle to a single PV for direct database access.
///
/// Optionally carries an `origin` ID. When set, `put_f64_post` tags
/// monitor events with this origin, allowing `DbSubscription` to
/// filter out self-triggered events.
#[derive(Clone)]
pub struct DbChannel {
    db: PvDatabase,
    name: String,
    origin: u64,
}

impl DbChannel {
    pub fn new(db: &PvDatabase, name: &str) -> Self {
        Self {
            db: db.clone(),
            name: name.to_string(),
            origin: 0,
        }
    }

    /// Create with an origin ID for self-write filtering.
    /// All `put_*_post` calls will tag events with this origin.
    /// `DbSubscription::subscribe_filtered` with the same origin will
    /// skip these events.
    pub fn with_origin(db: &PvDatabase, name: &str, origin: u64) -> Self {
        Self {
            db: db.clone(),
            name: name.to_string(),
            origin,
        }
    }

    /// Get the origin ID of this channel.
    pub fn origin(&self) -> u64 {
        self.origin
    }

    pub async fn get_f64(&self) -> f64 {
        self.db
            .get_pv(&self.name)
            .ok()
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0)
    }

    pub async fn get_i16(&self) -> i16 {
        self.db
            .get_pv(&self.name)
            .ok()
            .and_then(|v| v.to_f64())
            .map(|f| f as i16)
            .unwrap_or(0)
    }

    pub async fn get_string(&self) -> String {
        match self.db.get_pv(&self.name) {
            Ok(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            Ok(v) => v.to_string(),
            Err(_) => String::new(),
        }
    }

    /// Write a value without triggering record processing.
    /// Use for status/readback PVs where you just want to update the displayed value.
    pub async fn put_f64(&self, v: f64) -> CaResult<()> {
        self.db.put_pv(&self.name, EpicsValue::Double(v)).await
    }

    /// Write a value without triggering record processing.
    pub async fn put_i16(&self, v: i16) -> CaResult<()> {
        self.db.put_pv(&self.name, EpicsValue::Short(v)).await
    }

    /// Write a value without triggering record processing.
    pub async fn put_string(&self, v: &str) -> CaResult<()> {
        self.db
            .put_pv(&self.name, EpicsValue::String(v.to_string().into()))
            .await
    }

    /// Write a value and post monitor events (without processing).
    /// Equivalent to C EPICS `dbPut` + `db_post_events`.
    /// Use for readback/status mirror PVs that need to be visible to
    /// CA monitors but should NOT trigger record processing.
    pub async fn put_f64_post(&self, v: f64) -> CaResult<()> {
        self.db
            .put_pv_and_post_with_origin(&self.name, EpicsValue::Double(v), self.origin)
            .await
    }

    /// Write an i16 value and post monitor events (without processing).
    pub async fn put_i16_post(&self, v: i16) -> CaResult<()> {
        self.db
            .put_pv_and_post_with_origin(&self.name, EpicsValue::Short(v), self.origin)
            .await
    }

    /// Write a string value and post monitor events (without processing).
    pub async fn put_string_post(&self, v: &str) -> CaResult<()> {
        self.db
            .put_pv_and_post_with_origin(
                &self.name,
                EpicsValue::String(v.to_string().into()),
                self.origin,
            )
            .await
    }

    /// Write a value AND trigger record processing (like CA put).
    /// Use for motor VAL, busy records, etc. where processing drives hardware.
    /// Fire-and-forget — C `dbPutField` semantics; no put-notify is
    /// parked, so concurrent `ca_put_callback`s on the record stay legal.
    ///
    /// Carries the channel's `origin` (when set): every event the put's
    /// synchronous process cascade posts is tagged with it, so a
    /// `DbSubscription`/`DbMultiMonitor` filtering on the same origin does
    /// not hear the writer's own put — same self-write contract as the
    /// `put_*_post` tier.
    pub async fn put_f64_process(&self, v: f64) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        self.db
            .put_record_field_from_ca_no_notify_with_origin(
                record_name,
                field,
                EpicsValue::Double(v),
                self.origin,
            )
            .await
    }

    /// Write i16 + trigger processing. For bo/mbbo commands.
    pub async fn put_i16_process(&self, v: i16) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        self.db
            .put_record_field_from_ca_no_notify_with_origin(
                record_name,
                field,
                EpicsValue::Short(v),
                self.origin,
            )
            .await
    }

    /// Write i32 + trigger processing. For longout commands.
    pub async fn put_i32_process(&self, v: i32) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        self.db
            .put_record_field_from_ca_no_notify_with_origin(
                record_name,
                field,
                EpicsValue::Long(v),
                self.origin,
            )
            .await
    }

    /// Write string + trigger processing. For stringout commands.
    pub async fn put_string_process(&self, v: &str) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        self.db
            .put_record_field_from_ca_no_notify_with_origin(
                record_name,
                field,
                EpicsValue::String(v.to_string().into()),
                self.origin,
            )
            .await
    }

    /// Read i32 value. For longin/longout.
    pub async fn get_i32(&self) -> i32 {
        self.db
            .get_pv(&self.name)
            .ok()
            .and_then(|v| match v {
                EpicsValue::Long(i) => Some(i),
                // C `dbGetLink(.., DBR_LONG, ..)` picks its conversion routine by
                // the SOURCE type, so this goes through the coercion owner rather
                // than `c_cast` direct: an integer source takes C's defined
                // modular conversion, only a float source takes the UB cast.
                other => other.to_dbf_i32(),
            })
            .unwrap_or(0)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// DbSubscription — real monitor via RecordInstance::add_subscriber
// ---------------------------------------------------------------------------

/// Subscribe to value changes on a PV via the database's subscriber mechanism.
/// No polling — the record's process cycle pushes changes through the channel.
pub struct DbSubscription {
    /// Consumer half of this subscription's slot in the record's event queue
    /// (C `evSubscrip` + `event_read`). An in-process consumer is its own C
    /// `event_user`, so nothing else shares its queue.
    reader: EventReader,
    pv_name: String,
    /// If non-zero, events with this origin are silently skipped.
    /// Used to filter out self-triggered events from the same writer.
    ignore_origin: u64,
    /// Reference back to the record + this subscription's sid, for the
    /// enable/disable (`db_event_disable`) path and the `Drop` reaper.
    record: std::sync::Arc<parking_lot::RwLock<crate::server::record::RecordInstance>>,
    sid: u32,
}

/// Detachable enable/disable handle for one [`DbSubscription`], holding
/// only the record back-reference + the subscriber `sid` (an `Arc` + a
/// `u32`, so it is cheap to clone and outlives the `DbSubscription`
/// itself). A consumer can keep the `DbSubscription` moved into its
/// receive task while a separate owner toggles the same subscriber slot's
/// event flow via this handle — the in-process equivalent of holding a
/// `db_event_disable`/`db_event_enable` capability apart from the event
/// queue. Both toggle the SAME subscriber (keyed by `sid`), so the view
/// stays consistent.
#[derive(Clone)]
pub struct SubscriptionActivation {
    record: std::sync::Arc<parking_lot::RwLock<crate::server::record::RecordInstance>>,
    sid: u32,
}

impl SubscriptionActivation {
    /// Pause (`active == false`) or resume (`true`) the subscriber's event
    /// flow at the source. Same semantics as [`DbSubscription::set_active`]
    /// — see that method for the `db_event_disable` parity rationale.
    pub async fn set_active(&self, active: bool) {
        self.record.write().set_subscriber_active(self.sid, active);
    }
}

impl DbSubscription {
    /// Subscribe to a record field. Returns `None` if the record doesn't exist.
    pub async fn subscribe(db: &PvDatabase, pv_name: &str) -> Option<Self> {
        Self::subscribe_filtered(db, pv_name, 0).await
    }

    /// Subscribe with origin filtering. Events tagged with `ignore_origin`
    /// will be silently skipped by `recv_f64`/`recv`/`try_recv_f64`.
    pub async fn subscribe_filtered(
        db: &PvDatabase,
        pv_name: &str,
        ignore_origin: u64,
    ) -> Option<Self> {
        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        Self::subscribe_with_mask(db, pv_name, ignore_origin, mask).await
    }

    /// Subscribe with a custom event mask and origin filtering.
    ///
    /// Use `EventMask::PROPERTY` to receive display/control/enum metadata
    /// change events separately from value events (pvxs DBE_PROPERTY).
    pub async fn subscribe_with_mask(
        db: &PvDatabase,
        pv_name: &str,
        ignore_origin: u64,
        mask: u16,
    ) -> Option<Self> {
        Self::subscribe_with_mask_and_filters(db, pv_name, ignore_origin, mask, None).await
    }

    /// Subscribe with both a custom event mask and an
    /// optional pvxs-compatible channel filter chain. The chain's
    /// filters are attached to the new subscriber so the per-event
    /// filter framework gates / transforms events at posting time
    /// (matches pvxs/dbChannel field-scoped filter semantics —
    /// state is per-subscriber, not subscription-wide).
    pub async fn subscribe_with_mask_and_filters(
        db: &PvDatabase,
        pv_name: &str,
        ignore_origin: u64,
        mask: u16,
        filters: Option<&crate::server::database::filters::FilterChain>,
    ) -> Option<Self> {
        let (record_name, field) = parse_pv_name(pv_name);
        let field = field.to_ascii_uppercase();
        let rec = db.get_record(record_name)?;
        let sid = next_sid();
        let reader = {
            let mut instance = rec.write();
            let reader = instance.add_subscriber(&field, sid, DbFieldType::Double, mask)?;
            if let Some(chain) = filters {
                for filter in chain.iter() {
                    instance.attach_filter_to_last_subscriber(&field, filter.clone());
                }
            }
            reader
        };
        Some(Self {
            reader,
            pv_name: pv_name.to_string(),
            ignore_origin,
            record: rec,
            sid,
        })
    }

    /// Pause (`active == false`) or resume (`true`) this subscription's
    /// event flow at the source, the in-process equivalent of EPICS
    /// `db_event_disable` / `db_event_enable`. While paused, the record
    /// posts no events to this subscriber, so it stops doing per-event
    /// work for this monitor (pvxs `onStart(false)`); the subscription
    /// object survives so the same handle resumes on re-enable, and
    /// entries already queued still drain (C `db_event_disable` unlinks
    /// the monitor from the record, it does not touch the event queue).
    /// Idempotent.
    pub async fn set_active(&self, active: bool) {
        self.record.write().set_subscriber_active(self.sid, active);
    }

    /// A detachable [`SubscriptionActivation`] for this subscription's
    /// subscriber slot. Lets a separate owner enable/disable the event
    /// flow after this `DbSubscription` has been moved into a receive
    /// task — both toggle the same `sid`, so the source-side gating stays
    /// consistent with the queue this handle was minted from.
    pub fn activation_handle(&self) -> SubscriptionActivation {
        SubscriptionActivation {
            record: self.record.clone(),
            sid: self.sid,
        }
    }

    /// Await the next event from this subscription's queue (C `event_read`).
    /// A consumer that falls behind sees what a C monitor sees: its earlier
    /// distinct queued updates, then a tail entry carrying the latest value,
    /// because once the queue ran short of room further posts replaced that
    /// entry in place instead of appending (`dbEvent.c:812-827`).
    async fn next_event(&mut self) -> Option<MonitorEvent> {
        loop {
            let event = self.reader.recv().await?;
            if self.ignore_origin != 0 && event.origin == self.ignore_origin {
                continue;
            }
            return Some(event);
        }
    }

    /// Non-blocking [`Self::next_event`] — the same `ignore_origin` filter,
    /// no suspension. Delegates to [`EventReader::try_recv`]
    /// (`event_queue.rs:807`), which this subscription's queue already
    /// provides; nothing new is queued or gated here.
    ///
    /// Exists so a consumer that adapts this stream (PVA's monitor sources)
    /// can be polled from a blocking drain loop without a reactor — the RTEMS
    /// backend. A skipped `ignore_origin` event does not end the poll: the
    /// loop keeps taking until it finds a deliverable event or the queue
    /// reports empty, so a self-origin post cannot be misread as "nothing
    /// available".
    fn try_next_event(&mut self) -> Result<MonitorEvent, TryRecvError> {
        loop {
            let event = self.reader.try_recv()?;
            if self.ignore_origin != 0 && event.origin == self.ignore_origin {
                continue;
            }
            return Ok(event);
        }
    }

    /// Non-blocking [`Self::recv_snapshot`].
    pub fn try_recv_snapshot(&mut self) -> Result<crate::server::snapshot::Snapshot, TryRecvError> {
        self.try_next_event()
            .map(|e| std::sync::Arc::unwrap_or_clone(e.snapshot))
    }

    /// Non-blocking [`Self::recv_event`].
    pub fn try_recv_event(&mut self) -> Result<MonitorEvent, TryRecvError> {
        self.try_next_event()
    }

    /// Wait for the next value change. Returns the new value as f64.
    /// Silently skips events matching `ignore_origin`.
    pub async fn recv_f64(&mut self) -> Option<f64> {
        let event = self.next_event().await?;
        event.snapshot.value.to_f64()
    }

    /// Wait for the next value change. Returns the raw EpicsValue.
    /// Silently skips events matching `ignore_origin`.
    pub async fn recv(&mut self) -> Option<EpicsValue> {
        let event = self.next_event().await?;
        Some(event.snapshot.value.clone())
    }

    /// Wait for the next change, returning the full Snapshot with metadata.
    /// Includes alarm, display, control, and enum info — not just the value.
    /// Silently skips events matching `ignore_origin`.
    pub async fn recv_snapshot(&mut self) -> Option<crate::server::snapshot::Snapshot> {
        let event = self.next_event().await?;
        Some(std::sync::Arc::unwrap_or_clone(event.snapshot))
    }

    /// Wait for the next change, returning the full [`MonitorEvent`] —
    /// snapshot plus the per-event `DBE_*` mask (C `db_field_log.mask`,
    /// the discriminator pvxs narrows monitor updates with,
    /// `groupsource.cpp:331-337`). Consumers that decode differently per
    /// event class (e.g. a QSRV group monitor updating only alarm leaves
    /// on a `DBE_ALARM`-only post) use this instead of
    /// [`recv_snapshot`](Self::recv_snapshot). When events coalesced
    /// under a slow consumer, the mask is the OR of every squashed
    /// event's class — what changed since the previous delivery.
    /// Silently skips events matching `ignore_origin`.
    pub async fn recv_event(&mut self) -> Option<MonitorEvent> {
        self.next_event().await
    }

    /// Poll-based [`Self::recv_event`] — the same `ignore_origin` filter,
    /// `Poll::Pending` with the caller's waker registered on the queue where
    /// `recv_event()` would suspend.
    ///
    /// Exists so ONE task can await MANY subscriptions without spawning a
    /// forwarder per reader: the QSRV group drain polls every member's
    /// subscription in turn and parks once, its waker held in each polled
    /// queue's [`EvQue`](crate::server::event_queue::EvQue) until
    /// `wake_readers` flushes it. A skipped `ignore_origin` event does not
    /// end the poll — the loop keeps taking until it finds a deliverable
    /// event or the queue parks it, so a self-origin post cannot be misread
    /// as "nothing available".
    pub fn poll_recv_event(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<MonitorEvent>> {
        loop {
            match self.reader.poll_recv(cx) {
                std::task::Poll::Ready(Some(event))
                    if self.ignore_origin != 0 && event.origin == self.ignore_origin =>
                {
                    continue;
                }
                other => return other,
            }
        }
    }

    pub fn pv_name(&self) -> &str {
        &self.pv_name
    }
}

impl Drop for DbSubscription {
    /// Remove this subscription's `Subscriber` slot from the record's
    /// per-field subscriber Vec. Without this, an in-process consumer
    /// that drops a `DbSubscription` (pvalink, qsrv, gateway, etc.)
    /// leaves a dead subscriber row in `RecordInstance.subscribers`.
    /// Every subsequent `notify_field_with_origin` then builds and posts
    /// an event to a queue slot nobody reads — wasted clones, wasted
    /// contention on the record lock, and over time an O(N_dropped_subs)
    /// tax on every record process cycle.
    ///
    /// CA TCP server-side cleanup at `server/tcp.rs:2214-2224` already
    /// calls `remove_subscriber` on disconnect; this Drop closes the
    /// gap for the in-process API.
    fn drop(&mut self) {
        let record = self.record.clone();
        let sid = self.sid;
        // Drop runs in sync context but we need an async write lock, so the
        // unsubscribe is a fire-and-forget tail. It goes to the background
        // executor rather than the ambient seam because a subscription can be
        // dropped on any thread — including a blocking CA connection thread
        // that has no runtime — and dropping the cleanup there would leak the
        // subscriber into the record for the life of the IOC.
        // Middle band: unsubscribing is CA/PVA server teardown, which C runs
        // on the caller's thread through `db_cancel_event` — not record work
        // deferred to a callback queue, so PRIO does not select it.
        crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            async move {
                record.write().remove_subscriber(sid);
            },
        );
    }
}

// ---------------------------------------------------------------------------
// DbMultiMonitor — select! over multiple subscriptions
// ---------------------------------------------------------------------------

/// Monitor multiple PVs simultaneously. Returns the name and value of
/// whichever PV changes first.
pub struct DbMultiMonitor {
    subs: Vec<DbSubscription>,
}

impl DbMultiMonitor {
    /// Create subscriptions for all given PV names. PVs that don't exist are skipped.
    pub async fn new(db: &PvDatabase, pv_names: &[String]) -> Self {
        Self::new_filtered(db, pv_names, 0).await
    }

    /// Create subscriptions with origin filtering. Events from `ignore_origin`
    /// are silently skipped in `wait_change`.
    pub async fn new_filtered(db: &PvDatabase, pv_names: &[String], ignore_origin: u64) -> Self {
        let mut subs = Vec::new();
        for name in pv_names {
            if let Some(sub) = DbSubscription::subscribe_filtered(db, name, ignore_origin).await {
                subs.push(sub);
            }
        }
        Self { subs }
    }

    /// Number of active subscriptions.
    pub fn sub_count(&self) -> usize {
        self.subs.len()
    }

    /// Wait for any subscribed PV to change. Returns (pv_name, new_value).
    /// Silently skips events matching the subscription's `ignore_origin`.
    pub async fn wait_change(&mut self) -> (String, f64) {
        loop {
            for sub in &mut self.subs {
                match sub.reader.try_recv() {
                    Ok(event) => {
                        // Skip self-triggered events
                        if sub.ignore_origin != 0 && event.origin == sub.ignore_origin {
                            continue;
                        }
                        let val = event.snapshot.value.to_f64().unwrap_or(0.0);
                        return (sub.pv_name.clone(), val);
                    }
                    Err(_) => continue,
                }
            }
            // No events ready — yield briefly then retry. The background
            // timer, not the ambient one: a caller can await this from a
            // blocking connection thread that has no runtime.
            crate::runtime::task::sleep_background(Duration::from_millis(10)).await;
        }
    }
}
