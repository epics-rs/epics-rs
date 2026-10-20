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
use crate::server::pv::{MonitorEvent, SubscribeRefused};
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

    /// The field as `dbGetField(DBR_STRING)` reads it.
    pub async fn get_string(&self) -> String {
        match self.db.get_pv(&self.name) {
            Ok(v) => self.db.pv_dbr_string(&self.name, &v),
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
    /// Boxed: a subscription is held inline in PVA's monitor enums.
    channel: Box<Channel>,
    /// If non-zero, events with this origin are silently skipped.
    /// Used to filter out self-triggered events from the same writer.
    ignore_origin: u64,
    slot: SubscriberSlot,
    /// What is done to each event as it is read; `None` for most
    /// channels, which have neither.
    read: Option<Box<PostStage>>,
}

/// The channel a subscription reads through at delivery.
struct Channel {
    pv_name: Box<[u8]>,
    /// Upper-cased.
    field: String,
    /// The GET door [`DbSubscription::recv_snapshot`] reads options through.
    db: PvDatabase,
}

/// The record's subscriber row for one subscription, for the enable/disable
/// (`db_event_disable`) path; dropping it removes the row.
struct SubscriberSlot {
    record: std::sync::Arc<crate::server::record::RecordCell>,
    id: crate::server::event_queue::AttachId,
}

impl SubscriberSlot {
    /// `field` is the slot's upper-cased field.
    fn activation_handle(&self, field: &str) -> SubscriptionActivation {
        SubscriptionActivation {
            record: self.record.clone(),
            id: self.id,
            field: field.into(),
        }
    }
}

impl Drop for SubscriberSlot {
    /// Without this, an in-process consumer that drops its subscription
    /// leaves a dead row in `RecordInstance.subscribers`, and every later
    /// `notify_field_with_origin` builds and posts an event nobody reads.
    fn drop(&mut self) {
        let record = self.record.clone();
        let id = self.id;
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
                record.write().remove_subscriber(id);
            },
        );
    }
}

/// A subscription's read-time work, C's post chain on its `dbChannel`, run
/// off the record lock as the event is read (`dbEvent.c:984-985`).
pub struct PostStage {
    /// The channel's post-chain filters.
    post: crate::server::database::filters::FilterChain,
    /// A `$` channel's view: the post chain sees its char image.
    dollar: Option<crate::server::database::char_view::DollarView>,
}

impl PostStage {
    /// The stage for post chain `post` on a channel with `$` view `dollar`,
    /// `None` where it would do nothing.
    pub fn new(
        post: crate::server::database::filters::FilterChain,
        dollar: Option<crate::server::database::char_view::DollarView>,
    ) -> Option<Self> {
        (!post.is_empty() || dollar.is_some()).then_some(Self { post, dollar })
    }

    /// The event as read, `None` where the post chain drops it.
    pub fn read(&self, event: MonitorEvent) -> Option<MonitorEvent> {
        self.read_request(event, usize::MAX)
    }

    /// [`Self::read`] by a `dbGet` of `n_request` elements, a CA client's
    /// count, which a `$` channel's chars are cut to
    /// ([`DollarView::read_request`](crate::server::database::char_view::DollarView::read_request)).
    pub fn read_request(&self, mut event: MonitorEvent, n_request: usize) -> Option<MonitorEvent> {
        let image = self.dollar.map(|view| {
            let image = view.render(event.replace_value(EpicsValue::Long(0)));
            event.replace_value(image.clone());
            (view, image)
        });
        if !self.post.is_empty() {
            use crate::server::database::filters::FilteredMonitorEvent;
            event = self.post.apply(FilteredMonitorEvent::new(event))?.event;
        }
        if let Some((view, image)) = image {
            let log = event.replace_value(EpicsValue::Long(0));
            event.replace_value(view.read_request(image, log, n_request));
        }
        Some(event)
    }

    /// Whether the stage reads a `$` channel's chars itself.
    pub fn reads_chars(&self) -> bool {
        self.dollar.is_some()
    }
}

/// A queued update whose delivery read failed: the `dbGet` error a C
/// monitor turns into an `ECA_GETFAIL` event, never a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetFailed;

/// Detachable enable/disable handle for one [`DbSubscription`], holding
/// only the record back-reference + the subscriber's slot id (an `Arc` +
/// an integer, so it is cheap to clone and outlives the `DbSubscription`
/// itself). A consumer can keep the `DbSubscription` moved into its
/// receive task while a separate owner toggles the same subscriber slot's
/// event flow via this handle — the in-process equivalent of holding a
/// `db_event_disable`/`db_event_enable` capability apart from the event
/// queue. Both toggle the SAME subscriber (keyed by its slot), so the view
/// stays consistent.
#[derive(Clone)]
pub struct SubscriptionActivation {
    record: std::sync::Arc<crate::server::record::RecordCell>,
    id: crate::server::event_queue::AttachId,
    field: std::sync::Arc<str>,
}

impl SubscriptionActivation {
    /// Pause (`active == false`) or resume (`true`) the subscriber's event
    /// flow at the source. Same semantics as [`DbSubscription::set_active`]
    /// — see that method for the `db_event_disable` parity rationale.
    pub async fn set_active(&self, active: bool) {
        self.record.write().set_subscriber_active(self.id, active);
    }

    /// pvxs's `enable()` (`subscriptionctx.h:56-57`): `db_event_enable`,
    /// then `db_post_single_event`, so the field's current value reaches
    /// this subscriber under its own `select`. The read is
    /// `dbChannelGet` under the scan lock, as the CA seed reads it.
    pub async fn enable(&self, db: &PvDatabase) {
        self.set_active(true).await;
        crate::server::database::channel_io::post_single_event(
            db,
            &self.record,
            &self.field,
            self.id,
        );
    }
}

impl DbSubscription {
    /// Subscribe to a record field, refused with the reason the monitor was
    /// not installed.
    pub async fn subscribe(
        db: &PvDatabase,
        pv_name: impl AsRef<[u8]>,
    ) -> Result<Self, SubscribeRefused> {
        Self::subscribe_filtered(db, pv_name, 0).await
    }

    /// Subscribe with origin filtering. Events tagged with `ignore_origin`
    /// will be silently skipped by `recv_f64`/`recv`/`try_recv_f64`.
    pub async fn subscribe_filtered(
        db: &PvDatabase,
        pv_name: impl AsRef<[u8]>,
        ignore_origin: u64,
    ) -> Result<Self, SubscribeRefused> {
        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        Self::subscribe_with_mask(db, pv_name, ignore_origin, mask).await
    }

    /// Subscribe with a custom event mask and origin filtering.
    ///
    /// Use `EventMask::PROPERTY` to receive display/control/enum metadata
    /// change events separately from value events (pvxs DBE_PROPERTY).
    ///
    /// The name is C's `dbChannelCreate`'s: refused where it returns NULL,
    /// a `$` name is read as its chars, and a `{json}` suffix is the
    /// channel's filter chain, per subscriber as a `dbChannel`'s. The
    /// pre-chain filters run on the post, the post-chain ones as the event
    /// is read
    /// ([`SubscriptionFilter::post_chain`](crate::server::database::filters::SubscriptionFilter::post_chain)).
    pub async fn subscribe_with_mask(
        db: &PvDatabase,
        pv_name: impl AsRef<[u8]>,
        ignore_origin: u64,
        mask: u16,
    ) -> Result<Self, SubscribeRefused> {
        use crate::server::database::filters::{
            ChannelCreateError, db_channel_create, parse_channel_name,
        };
        let channel = match db_channel_create(db, pv_name.as_ref()) {
            Ok(channel) => channel,
            Err(ChannelCreateError::Filter(_)) => return Err(SubscribeRefused::BadFilter),
            Err(ChannelCreateError::NotFound) => {
                let name = parse_channel_name(pv_name.as_ref());
                let record = parse_pv_name(&name.record_path).0;
                return Err(match db.get_record(record) {
                    Some(_) => SubscribeRefused::FieldNotFound,
                    None => SubscribeRefused::NoRecord,
                });
            }
        };
        let (record_name, field) = parse_pv_name(&channel.name.record_path);
        let field = field.to_ascii_uppercase();
        let rec = db
            .get_record(record_name)
            .ok_or(SubscribeRefused::NoRecord)?;
        let dollar = if channel.name.string_view {
            let view = crate::server::database::char_view::DollarView::of(
                &rec.read(),
                &field,
                &channel.chain,
            );
            Some(view.ok_or(SubscribeRefused::FieldNotFound)?)
        } else {
            None
        };
        let sid = next_sid();
        let (pre, post) = channel.chain.split();
        let reader = {
            let mut instance = rec.write();
            let reader = instance.add_subscriber(&field, sid, DbFieldType::Double, mask)?;
            for filter in pre {
                instance.attach_filter_to_last_subscriber(&field, filter);
            }
            reader
        };
        Ok(Self {
            channel: Box::new(Channel {
                pv_name: pv_name.as_ref().into(),
                field,
                db: db.clone(),
            }),
            ignore_origin,
            slot: SubscriberSlot {
                record: rec,
                id: reader.id(),
            },
            reader,
            read: PostStage::new(post, dollar).map(Box::new),
        })
    }

    /// The event as read, `None` where it is skipped: the one place
    /// `ignore_origin`, the read of a log that holds no value (C
    /// `dbfl_type_ref`, read through `dbChannelField` at delivery), the `$`
    /// view and the post chain apply, for every reader. A field the read
    /// cannot reach leaves the log without a value.
    fn read_event(&self, mut event: MonitorEvent) -> Option<MonitorEvent> {
        if self.ignore_origin != 0 && event.origin() == self.ignore_origin {
            return None;
        }
        read_reference(&mut event, &self.slot.record, &self.channel.field);
        match self.read.as_deref() {
            Some(read) => read.read(event),
            None => Some(event),
        }
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
        let slot = &self.slot;
        slot.record.write().set_subscriber_active(slot.id, active);
    }

    /// A detachable [`SubscriptionActivation`] for this subscription's
    /// subscriber slot. Lets a separate owner enable/disable the event
    /// flow after this `DbSubscription` has been moved into a receive
    /// task — both toggle the same slot, so the source-side gating stays
    /// consistent with the queue this handle was minted from.
    pub fn activation_handle(&self) -> SubscriptionActivation {
        self.slot.activation_handle(&self.channel.field)
    }

    /// Await the next event from this subscription's queue (C `event_read`).
    /// A consumer that falls behind sees what a C monitor sees: its earlier
    /// distinct queued updates, then a tail entry carrying the latest value,
    /// because once the queue ran short of room further posts replaced that
    /// entry in place instead of appending (`dbEvent.c:812-827`).
    async fn next_event(&mut self) -> Option<MonitorEvent> {
        loop {
            let event = self.reader.recv().await?;
            if let Some(event) = self.read_event(event) {
                return Some(event);
            }
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
            if let Some(event) = self.read_event(event) {
                return Ok(event);
            }
        }
    }

    /// Non-blocking [`Self::recv_snapshot`].
    pub fn try_recv_snapshot(
        &mut self,
    ) -> Result<Result<crate::server::snapshot::Snapshot, GetFailed>, TryRecvError> {
        let event = self.try_next_event()?;
        Ok(self.delivered(&event))
    }

    /// `event` as `dbGet` reads it: the options from the record now, on this
    /// thread (`getOptions`, `dbAccess.c:336-414`), the value, alarm, time
    /// and utag from the log. A field the read cannot reach is a failed get,
    /// as `dbGet`'s error is to `event_read`.
    fn delivered(
        &self,
        event: &MonitorEvent,
    ) -> Result<crate::server::snapshot::Snapshot, GetFailed> {
        if event.value().as_value().is_none() {
            return Err(GetFailed);
        }
        let channel = &self.channel;
        let mut snapshot = channel
            .db
            .channel_snapshot_for_field(&self.slot.record, &channel.field, false)
            .ok_or(GetFailed)?;
        event.write_log_over(&mut snapshot);
        Ok(snapshot)
    }

    /// Non-blocking [`Self::recv_event`].
    pub fn try_recv_event(&mut self) -> Result<MonitorEvent, TryRecvError> {
        self.try_next_event()
    }

    /// Wait for the next value change. Returns the new value as f64.
    /// Silently skips events matching `ignore_origin`, and events whose
    /// field could not be read.
    pub async fn recv_f64(&mut self) -> Option<f64> {
        loop {
            let event = self.next_event().await?;
            if let Some(value) = event.value().as_value() {
                return value.to_f64();
            }
        }
    }

    /// Wait for the next value change. Returns the raw EpicsValue.
    /// Silently skips events matching `ignore_origin`, and events whose
    /// field could not be read.
    pub async fn recv(&mut self) -> Option<EpicsValue> {
        loop {
            let event = self.next_event().await?;
            if let Some(value) = event.value().as_value() {
                return Some(value.clone());
            }
        }
    }

    /// Wait for the next change, returning the full Snapshot with metadata.
    /// Includes alarm, display, control, and enum info — not just the value.
    /// Silently skips events matching `ignore_origin`.
    pub async fn recv_snapshot(
        &mut self,
    ) -> Option<Result<crate::server::snapshot::Snapshot, GetFailed>> {
        let event = self.next_event().await?;
        Some(self.delivered(&event))
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
                std::task::Poll::Ready(Some(event)) => {
                    if let Some(event) = self.read_event(event) {
                        return std::task::Poll::Ready(Some(event));
                    }
                }
                other => return other,
            }
        }
    }

    pub fn pv_name(&self) -> &[u8] {
        &self.channel.pv_name
    }
}

// ---------------------------------------------------------------------------
// DbEventSource — one client context's event_user and its event task
// ---------------------------------------------------------------------------

/// Where a [`DbEventSource`]'s drain hands one subscription's entries — C
/// `db_add_event`'s `user_sub` with its `user_arg`.
pub trait EventDelivery: Send + Sync + 'static {
    /// One entry, already read through the subscription's post chain. Runs
    /// on the drain thread, which holds no record lock.
    fn deliver(&self, event: MonitorEvent) -> impl std::future::Future<Output = ()> + Send;

    /// The subscription's producer is gone and its entries are delivered.
    fn ended(&self) {}
}

/// What one attachment's `DeliveryContext` carries to the drain.
struct Route<D> {
    record: std::sync::Arc<crate::server::record::RecordCell>,
    /// Upper-cased.
    field: String,
    read: Option<PostStage>,
    delivery: D,
    /// C `user_sub = NULL`: set by the attachment's drop under
    /// [`DrainSync::in_flight`].
    canceled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Give a log that holds no value (C `dbfl_type_ref`) the field read now,
/// as `dbChannelGet` reads it through `dbChannelField` at delivery. A field
/// the read cannot reach leaves the log without a value.
fn read_reference(
    event: &mut MonitorEvent,
    record: &crate::server::record::RecordCell,
    field: &str,
) {
    if event.value().as_value().is_none()
        && let Some(value) = event.delivered_value(&record.read(), field)
    {
        event.set_value(value);
    }
}

/// One client context's C `event_user` and the `event_task` that drains it
/// (`dbEvent.c:1016-1061`): pvxs's `qsrvSingle` and `qsrvGroup`
/// (`ioc/singlesource.cpp:396-416`, `ioc/groupsource.cpp:72-96`). Every
/// subscription attached here shares one queue chain and one `ppendsem`, so
/// a post wakes the drain only when it finds the drain parked (C
/// `firstEventFlag`); a private chain per subscription made every post a
/// wake.
///
/// The drain is a dedicated thread that exists exactly while a
/// [`DbAttachment`] does: each attachment holds it, and the last one
/// dropping ends it.
pub struct DbEventSource<D> {
    name: &'static str,
    priority: crate::runtime::task::ThreadPriority,
    stack: crate::runtime::task::StackSizeClass,
    drain: parking_lot::Mutex<std::sync::Weak<Drain>>,
    live_drains: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    _delivery: std::marker::PhantomData<fn(D)>,
}

/// The live drain of a [`DbEventSource`].
struct Drain {
    user: std::sync::Arc<crate::server::event_queue::EventUser>,
    sync: std::sync::Arc<DrainSync>,
    /// Dropped with the last attachment; the drain thread returns on it.
    _stop: tokio::sync::oneshot::Sender<()>,
    #[cfg(test)]
    probe: std::sync::Arc<DrainProbe>,
}

/// C `callBackInProgress` and `db_sync_event` (`dbEvent.c:567-632`): a
/// [`DbAttachment`] dropped while the drain delivers one of its entries
/// waits for that delivery, and one dropped before it starts cancels it.
#[derive(Default)]
struct DrainSync {
    /// The attachment whose entry the drain is delivering.
    in_flight: parking_lot::Mutex<Option<crate::server::event_queue::AttachId>>,
    done: parking_lot::Condvar,
    /// The drain thread, which never waits on itself (C compares `taskid`).
    thread: std::sync::OnceLock<std::thread::ThreadId>,
}

impl DrainSync {
    /// Start delivering `id`'s entry, unless its attachment is dropped.
    fn begin(
        &self,
        id: crate::server::event_queue::AttachId,
        canceled: &std::sync::atomic::AtomicBool,
    ) -> Option<InFlight<'_>> {
        let mut in_flight = self.in_flight.lock();
        if canceled.load(Ordering::Relaxed) {
            return None;
        }
        *in_flight = Some(id);
        Some(InFlight(self))
    }

    /// C `db_cancel_event`'s sync: on return the drain delivers nothing
    /// more to `id`.
    fn cancel(
        &self,
        id: crate::server::event_queue::AttachId,
        canceled: &std::sync::atomic::AtomicBool,
    ) {
        let mut in_flight = self.in_flight.lock();
        canceled.store(true, Ordering::Relaxed);
        if self.thread.get() == Some(&std::thread::current().id()) {
            return;
        }
        while *in_flight == Some(id) {
            self.done.wait(&mut in_flight);
        }
    }
}

/// One delivery in progress; ends on every exit, a panicking delivery's
/// included, so no drop waits on a drain that is gone.
struct InFlight<'a>(&'a DrainSync);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        *self.0.in_flight.lock() = None;
        self.0.done.notify_all();
    }
}

/// What the pins read off a drain: how often it parked on an empty chain
/// and how often a wake brought it back.
#[cfg(test)]
#[derive(Default)]
struct DrainProbe {
    parks: std::sync::atomic::AtomicU64,
    wakes: std::sync::atomic::AtomicU64,
}

/// A subscription attached to a [`DbEventSource`]. Its entries reach the
/// source's drain and nothing else, so it has no `recv`: a second consumer
/// on the shared `ppendsem` would steal the drain's wakes. Dropping it
/// cancels the slot (C `db_cancel_event`) and removes the record's row.
/// The drop waits for a delivery of this attachment's entry already under
/// way: it runs on server threads, never a scan thread, and holds no
/// record lock.
pub struct DbAttachment {
    reader: EventReader,
    slot: SubscriberSlot,
    /// Upper-cased.
    field: Box<str>,
    canceled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    drain: std::sync::Arc<Drain>,
}

impl Drop for DbAttachment {
    fn drop(&mut self) {
        self.drain.sync.cancel(self.reader.id(), &self.canceled);
    }
}

impl DbAttachment {
    /// See [`DbSubscription::activation_handle`].
    pub fn activation_handle(&self) -> SubscriptionActivation {
        self.slot.activation_handle(&self.field)
    }

    /// C `db_event_enable`: put an [`DbEventSource::attach_disabled`]
    /// subscription on the record's monitor list.
    pub fn enable(&self) {
        self.slot
            .record
            .write()
            .set_subscriber_active(self.slot.id, true);
    }

    /// The queue this subscription is attached to, for diagnostics and tests.
    pub fn queue(&self) -> std::pin::Pin<std::sync::Arc<crate::server::event_queue::EvQue>> {
        self.reader.queue()
    }
}

impl<D: EventDelivery> DbEventSource<D> {
    /// A source whose drain thread, while one exists, is `name` at
    /// `priority` on a `stack` stack.
    pub fn new(
        name: &'static str,
        priority: crate::runtime::task::ThreadPriority,
        stack: crate::runtime::task::StackSizeClass,
    ) -> Self {
        Self {
            name,
            priority,
            stack,
            drain: parking_lot::Mutex::new(std::sync::Weak::new()),
            live_drains: Default::default(),
            _delivery: std::marker::PhantomData,
        }
    }

    /// C `db_add_event` on this source's `event_user`: subscribe `pv_name`
    /// with `mask`, its pre-chain `filters` run on the post, and deliver each
    /// entry through the post chain and `$` view `dollar` to `delivery`.
    /// Refused when the record does not exist or takes no more monitors.
    pub fn attach(
        &self,
        db: &PvDatabase,
        pv_name: impl AsRef<[u8]>,
        mask: u16,
        filters: Option<&crate::server::database::filters::FilterChain>,
        dollar: Option<crate::server::database::char_view::DollarView>,
        delivery: D,
    ) -> Result<DbAttachment, SubscribeRefused> {
        let attached = self.attach_disabled(db, pv_name, mask, filters, dollar, delivery)?;
        attached.enable();
        Ok(attached)
    }

    /// [`Self::attach`] without `db_event_enable`, as `db_add_event` itself
    /// adds it: nothing is posted to it until [`DbAttachment::enable`].
    pub fn attach_disabled(
        &self,
        db: &PvDatabase,
        pv_name: impl AsRef<[u8]>,
        mask: u16,
        filters: Option<&crate::server::database::filters::FilterChain>,
        dollar: Option<crate::server::database::char_view::DollarView>,
        delivery: D,
    ) -> Result<DbAttachment, SubscribeRefused> {
        let (record_name, field) = parse_pv_name(pv_name.as_ref());
        let field = field.to_ascii_uppercase();
        let rec = db
            .get_record(record_name)
            .ok_or(SubscribeRefused::NoRecord)?;
        let (pre, post) = filters.map(|chain| chain.split()).unwrap_or_default();
        let canceled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let context: crate::server::event_queue::DeliveryContext = std::sync::Arc::new(Route {
            record: rec.clone(),
            field: field.clone(),
            read: PostStage::new(post, dollar),
            delivery,
            canceled: canceled.clone(),
        });
        let drain = self.drain();
        let reader = {
            let mut instance = rec.write();
            let reader = instance.add_subscriber_on(
                &drain.user,
                &field,
                next_sid(),
                DbFieldType::Double,
                mask,
                &context,
            )?;
            for filter in pre {
                instance.attach_filter_to_last_subscriber(&field, filter);
            }
            instance.set_subscriber_active(reader.id(), false);
            reader
        };
        Ok(DbAttachment {
            slot: SubscriberSlot {
                record: rec,
                id: reader.id(),
            },
            reader,
            field: field.into(),
            canceled,
            drain,
        })
    }

    /// Drain threads running, for tests: 0 once the last attachment's drain
    /// has returned.
    pub fn live_drains(&self) -> usize {
        self.live_drains.load(Ordering::Acquire)
    }

    /// The live drain, or a new one with a fresh `event_user`.
    fn drain(&self) -> std::sync::Arc<Drain> {
        let mut slot = self.drain.lock();
        if let Some(drain) = slot.upgrade() {
            return drain;
        }
        let user = std::sync::Arc::new(crate::server::event_queue::EventUser::new());
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let sync = std::sync::Arc::new(DrainSync::default());
        let drain = std::sync::Arc::new(Drain {
            user: user.clone(),
            sync: sync.clone(),
            _stop: stop,
            #[cfg(test)]
            probe: Default::default(),
        });
        let task = DrainTask::<D> {
            user,
            sync,
            stop: stop_rx,
            #[cfg(test)]
            probe: drain.probe.clone(),
            _delivery: std::marker::PhantomData,
        };
        let live = self.live_drains.clone();
        live.fetch_add(1, Ordering::AcqRel);
        let name = self.name;
        let spawned = crate::runtime::task::spawn_dedicated_thread(
            name.into(),
            self.priority,
            self.stack,
            move || {
                if let Err(e) = crate::runtime::task::block_on_sync(task.run()) {
                    // Unreachable: a fresh dedicated thread is neither a
                    // facility worker nor a current-thread runtime.
                    tracing::debug!(error = ?e, "{name} drain could not block on its event task");
                }
                live.fetch_sub(1, Ordering::AcqRel);
            },
        );
        if let Err(e) = spawned {
            self.live_drains.fetch_sub(1, Ordering::AcqRel);
            // pvxs throws from source setup when `db_start_events` fails.
            panic!("{name} drain thread could not be created: {e}");
        }
        *slot = std::sync::Arc::downgrade(&drain);
        drain
    }
}

/// What the drain thread owns.
struct DrainTask<D> {
    user: std::sync::Arc<crate::server::event_queue::EventUser>,
    sync: std::sync::Arc<DrainSync>,
    stop: tokio::sync::oneshot::Receiver<()>,
    #[cfg(test)]
    probe: std::sync::Arc<DrainProbe>,
    _delivery: std::marker::PhantomData<fn(D)>,
}

impl<D: EventDelivery> DrainTask<D> {
    /// C `event_task`: take the chain's entries in ring order and hand each
    /// to its subscription's delivery, until the last attachment drops.
    async fn run(mut self) {
        use crate::server::event_queue::CircuitEvent;
        use std::task::Poll;
        let _ = self.sync.thread.set(std::thread::current().id());
        #[cfg(test)]
        let mut parked = false;
        loop {
            let next = std::future::poll_fn(|cx| {
                #[cfg(test)]
                if std::mem::take(&mut parked) {
                    self.probe.wakes.fetch_add(1, Ordering::Relaxed);
                }
                if std::pin::Pin::new(&mut self.stop).poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
                match self.user.poll_next(cx) {
                    Poll::Ready(step) => Poll::Ready(Some(step)),
                    Poll::Pending => {
                        #[cfg(test)]
                        {
                            parked = true;
                            self.probe.parks.fetch_add(1, Ordering::Release);
                        }
                        Poll::Pending
                    }
                }
            })
            .await;
            match next {
                None => return,
                Some(CircuitEvent::Event {
                    id, context, event, ..
                }) => {
                    let Some(route) = context.downcast_ref::<Route<D>>() else {
                        continue;
                    };
                    let Some(_in_flight) = self.sync.begin(id, &route.canceled) else {
                        continue;
                    };
                    let mut event = event;
                    read_reference(&mut event, &route.record, &route.field);
                    let event = match &route.read {
                        Some(read) => read.read(event),
                        None => Some(event),
                    };
                    if let Some(event) = event {
                        route.delivery.deliver(event).await;
                    }
                }
                Some(CircuitEvent::Ended(id)) => {
                    if let Some(context) = self.user.context(id)
                        && let Some(route) = context.downcast_ref::<Route<D>>()
                        && let Some(_in_flight) = self.sync.begin(id, &route.canceled)
                    {
                        route.delivery.ended();
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DbMultiMonitor — select! over multiple subscriptions
// ---------------------------------------------------------------------------

/// Monitor multiple PVs simultaneously. Returns the name and value of
/// whichever PV changes first.
pub struct DbMultiMonitor {
    /// Each subscription beside the name its caller asked for, which is what
    /// [`Self::wait_change`] answers with.
    subs: Vec<(String, DbSubscription)>,
    /// SNL `monitor` variables with no event flag, each with the last value
    /// posted to it.
    vars: Vec<(String, DbSubscription, f64)>,
}

impl DbMultiMonitor {
    /// Create subscriptions for all given PV names. The sequencer's own puts
    /// reach them, as its `pvPut` monitors raise its flags in C. A refused PV
    /// is skipped without a line: C's sequencer monitors through a CA circuit,
    /// where a refusal is OOM-only and reaches CA's default exception handler.
    pub async fn new(db: &PvDatabase, pv_names: &[String]) -> Self {
        let mut subs = Vec::new();
        for name in pv_names {
            if let Ok(sub) = Self::subscribe(db, name).await {
                subs.push((name.clone(), sub));
            }
        }
        Self {
            subs,
            vars: Vec::new(),
        }
    }

    /// Monitor `names` as SNL `monitor` variables that no event flag is
    /// `sync`ed to: C writes such a variable as each monitor arrives, so it
    /// raises no [`Self::wait_change`] and [`Self::var`] reads its last post.
    /// Returns how many of `names` were subscribed.
    pub async fn add_vars(&mut self, db: &PvDatabase, names: &[String]) -> usize {
        let before = self.vars.len();
        for name in names {
            if let Ok(sub) = Self::subscribe(db, name).await {
                let seed = db.get_pv(name).ok().and_then(|v| v.to_f64());
                self.vars.push((name.clone(), sub, seed.unwrap_or(0.0)));
            }
        }
        self.vars.len() - before
    }

    /// The sequencer's monitor: `DBE_VALUE | DBE_ALARM` (`pv.c:133`), so an
    /// alarm-only post raises the flag as a value change does.
    async fn subscribe(db: &PvDatabase, name: &str) -> Result<DbSubscription, SubscribeRefused> {
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        DbSubscription::subscribe_with_mask(db, name, 0, mask).await
    }

    /// The last value posted to the variable `name` added by
    /// [`Self::add_vars`], or 0 if there is none.
    pub fn var(&mut self, name: &str) -> f64 {
        self.drain_vars();
        self.vars
            .iter()
            .find(|(var, ..)| var == name)
            .map_or(0.0, |(.., last)| *last)
    }

    fn drain_vars(&mut self) {
        for (_, sub, last) in &mut self.vars {
            while let Ok(event) = sub.try_next_event() {
                if let Some(v) = event.value().as_value().and_then(EpicsValue::to_f64) {
                    *last = v;
                }
            }
        }
    }

    /// Number of active subscriptions.
    pub fn sub_count(&self) -> usize {
        self.subs.len()
    }

    /// Drop the events queued for `name`, as SNL's `efClear` on a sync'd
    /// event flag discards the monitors that raised it.
    pub fn clear(&mut self, name: &str) {
        for (sub_name, sub) in &mut self.subs {
            if sub_name == name {
                while sub.try_next_event().is_ok() {}
            }
        }
    }

    /// [`Self::wait_change`]'s answer if a change is already queued.
    pub fn try_change(&mut self) -> Option<(String, f64)> {
        for (name, sub) in &mut self.subs {
            if let Ok(mut event) = sub.try_next_event() {
                while let Ok(newer) = sub.try_next_event() {
                    event = newer;
                }
                let val = event
                    .value()
                    .as_value()
                    .and_then(EpicsValue::to_f64)
                    .unwrap_or(0.0);
                return Some((name.clone(), val));
            }
        }
        None
    }

    /// Wait for any subscribed PV to change. Returns (pv_name, new_value)
    /// with the newest value queued for that PV: one SNL sync'd event flag
    /// stands for every monitor since it was last cleared, and its variable
    /// holds the last of them.
    pub async fn wait_change(&mut self) -> (String, f64) {
        loop {
            if let Some(change) = self.try_change() {
                return change;
            }
            // No events ready — yield briefly then retry. The background
            // timer, not the ambient one: a caller can await this from a
            // blocking connection thread that has no runtime.
            crate::runtime::task::sleep_background(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(test)]
mod multi_monitor_tests {
    use super::*;

    /// A cleared channel's queued events are gone; the others' still arrive.
    #[epics_macros_rs::epics_test]
    async fn clear_drops_only_that_channels_queued_events() {
        let db = PvDatabase::new();
        for name in ["X", "Y"] {
            db.add_record(
                name,
                Box::new(crate::server::records::ai::AiRecord::new(0.0)),
            )
            .await
            .unwrap();
        }
        let names = ["X".to_string(), "Y".to_string()];
        let mut monitor = DbMultiMonitor::new(&db, &names).await;
        db.put_pv_and_post("X", EpicsValue::Double(1.0))
            .await
            .unwrap();
        db.put_pv_and_post("X", EpicsValue::Double(2.0))
            .await
            .unwrap();
        db.put_pv_and_post("Y", EpicsValue::Double(3.0))
            .await
            .unwrap();
        monitor.clear("X");
        assert_eq!(monitor.wait_change().await, ("Y".to_string(), 3.0));
        db.put_pv_and_post("X", EpicsValue::Double(4.0))
            .await
            .unwrap();
        assert_eq!(monitor.wait_change().await, ("X".to_string(), 4.0));
    }

    /// Two queued events on one channel arrive as one, with the later value.
    #[epics_macros_rs::epics_test]
    async fn queued_events_arrive_as_the_latest_value() {
        let db = PvDatabase::new();
        db.add_record(
            "X",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        let mut monitor = DbMultiMonitor::new(&db, &["X".to_string()]).await;
        for v in [1.0, 2.0] {
            db.put_pv_and_post("X", EpicsValue::Double(v))
                .await
                .unwrap();
        }
        assert_eq!(monitor.wait_change().await, ("X".to_string(), 2.0));
        monitor.clear("X");
        db.put_pv_and_post("X", EpicsValue::Double(3.0))
            .await
            .unwrap();
        assert_eq!(monitor.wait_change().await, ("X".to_string(), 3.0));
    }

    async fn db_with(records: &[(&str, f64)]) -> PvDatabase {
        let db = PvDatabase::new();
        for &(name, v) in records {
            db.add_record(name, Box::new(crate::server::records::ai::AiRecord::new(v)))
                .await
                .unwrap();
        }
        db
    }

    /// A var reads its current value before any post and its last post
    /// after, and its posts raise no change: the flagged channel's event is
    /// the one returned, though the var's were posted first. An absent name
    /// is not counted.
    #[epics_macros_rs::epics_test]
    async fn a_var_reads_its_last_post_and_raises_no_change() {
        let db = db_with(&[("M", 5.0), ("X", 0.0)]).await;
        let mut monitor = DbMultiMonitor::new(&db, &["X".to_string()]).await;
        assert_eq!(
            monitor
                .add_vars(&db, &["M".to_string(), "NONE".to_string()])
                .await,
            1
        );
        assert_eq!(monitor.var("M"), 5.0);
        for v in [1.0, 2.0] {
            db.put_pv_and_post("M", EpicsValue::Double(v))
                .await
                .unwrap();
        }
        db.put_pv_and_post("X", EpicsValue::Double(3.0))
            .await
            .unwrap();
        assert_eq!(monitor.wait_change().await, ("X".to_string(), 3.0));
        assert_eq!(monitor.var("M"), 2.0);
    }

    /// The sequencer's own puts reach both its vars and its flagged channels,
    /// as its pvPut monitors do in C.
    #[epics_macros_rs::epics_test]
    async fn the_writers_own_puts_reach_its_vars_and_flags() {
        let db = db_with(&[("M", 0.0), ("X", 0.0)]).await;
        let mut monitor = DbMultiMonitor::new(&db, &["X".to_string()]).await;
        monitor.add_vars(&db, &["M".to_string()]).await;
        DbChannel::with_origin(&db, "M", 7)
            .put_f64_process(4.0)
            .await
            .unwrap();
        assert_eq!(monitor.var("M"), 4.0);
        DbChannel::with_origin(&db, "X", 7)
            .put_f64_process(5.0)
            .await
            .unwrap();
        assert_eq!(monitor.wait_change().await, ("X".to_string(), 5.0));
    }

    /// `get_string` is `dbGetField(DBR_STRING)`: a real at the record's
    /// `get_precision`.
    #[epics_macros_rs::epics_test]
    async fn get_string_renders_at_the_record_precision() {
        let db = PvDatabase::new();
        db.add_record(
            "P",
            Box::new(crate::server::records::ai::AiRecord::new(1.5)),
        )
        .await
        .unwrap();
        db.put_pv("P.PREC", EpicsValue::Short(2)).await.unwrap();
        assert_eq!(DbChannel::new(&db, "P").get_string().await, "1.50");
    }

    /// The first put of an unchanged value clears UDF, an alarm-only post,
    /// which the sequencer's `DBE_VALUE | DBE_ALARM` monitor (`pv.c:133`)
    /// delivers.
    #[epics_macros_rs::epics_test]
    async fn an_alarm_only_post_raises_a_change() {
        let db = PvDatabase::new();
        db.add_record(
            "X",
            Box::new(crate::server::records::mbbo::MbboRecord::new(0)),
        )
        .await
        .unwrap();
        let mut monitor = DbMultiMonitor::new(&db, &["X".to_string()]).await;
        DbChannel::new(&db, "X").put_i16_process(0).await.unwrap();
        let change = crate::runtime::task::timeout(Duration::from_secs(1), monitor.wait_change());
        assert_eq!(change.await.ok(), Some(("X".to_string(), 0.0)));
    }
}

#[cfg(test)]
mod delivery_read_tests {
    use super::*;

    /// A field the record does not have is no channel: C's
    /// `dbChannelCreate` returns NULL from `pvNameLookup`
    /// (`dbChannel.c:460-462`), so there is nothing to subscribe.
    #[epics_macros_rs::epics_test]
    async fn a_field_the_record_lacks_is_refused() {
        let db = PvDatabase::new();
        db.add_record(
            "X",
            Box::new(crate::server::records::ai::AiRecord::new(1.0)),
        )
        .await
        .unwrap();
        assert!(matches!(
            DbSubscription::subscribe(&db, "X.NOSUCH").await,
            Err(SubscribeRefused::FieldNotFound)
        ));
    }

    /// The channel keeps the name it was opened by, as C's `dbChannel` does.
    #[epics_macros_rs::epics_test]
    async fn a_subscription_keeps_its_latin1_name() {
        let db = PvDatabase::new();
        db.add_record(
            &b"caf\xe9"[..],
            Box::new(crate::server::records::ai::AiRecord::new(1.0)),
        )
        .await
        .unwrap();
        let sub = DbSubscription::subscribe(&db, b"caf\xe9.VAL")
            .await
            .unwrap();
        assert_eq!(sub.pv_name(), b"caf\xe9.VAL");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::{Duration, Instant};

    use crate::server::recgbl::EventMask;
    use crate::server::records::ai::AiRecord;

    // RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the
    // exec-backend suite.

    /// Counts entries per subscription and keeps each one's last value; the
    /// first entry it sees can hold the drain until `release`.
    #[derive(Clone, Default)]
    struct Tally {
        delivered: Arc<AtomicUsize>,
        last: Arc<parking_lot::Mutex<HashMap<usize, f64>>>,
        hold_first: Option<Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>>,
        entered: Arc<AtomicBool>,
    }

    struct Probe {
        index: usize,
        tally: Tally,
    }

    impl EventDelivery for Probe {
        fn deliver(&self, event: MonitorEvent) -> impl std::future::Future<Output = ()> + Send {
            let t = &self.tally;
            if let Some(crate::types::EpicsValue::Double(v)) = event.value().as_value() {
                t.last.lock().insert(self.index, *v);
            }
            t.delivered.fetch_add(1, Ordering::AcqRel);
            if let Some(gate) = &t.hold_first
                && !t.entered.swap(true, Ordering::AcqRel)
            {
                let (open, cv) = &**gate;
                let mut open = open.lock();
                while !*open {
                    cv.wait(&mut open);
                }
            }
            std::future::ready(())
        }
    }

    type Source = DbEventSource<Probe>;

    fn source() -> Source {
        DbEventSource::new(
            "dbEvPin",
            crate::runtime::task::ThreadPriority::Custom(17),
            crate::runtime::task::StackSizeClass::Medium,
        )
    }

    async fn ai_db(n: usize) -> PvDatabase {
        let db = PvDatabase::new();
        for i in 0..n {
            db.add_record(&format!("EV:{i}"), Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        }
        db
    }

    fn post(db: &PvDatabase, i: usize, v: f64) {
        let rec = db.get_record(format!("EV:{i}")).unwrap();
        let mut rec = rec.write();
        rec.record
            .put_field("VAL", crate::types::EpicsValue::Double(v))
            .unwrap();
        rec.notify_field("VAL", EventMask::VALUE);
    }

    fn probe(source: &Source) -> Arc<DrainProbe> {
        source.drain.lock().upgrade().unwrap().probe.clone()
    }

    /// Wait until the drain is parked and stays parked, then return its wake
    /// count.
    fn settled_wakes(probe: &DrainProbe) -> u64 {
        let parked = || {
            let wakes = probe.wakes.load(Ordering::Acquire);
            (probe.parks.load(Ordering::Acquire) > wakes).then_some(wakes)
        };
        loop {
            wait_until("the drain to park", || parked().is_some());
            let wakes = parked().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            if parked() == Some(wakes) {
                return wakes;
            }
        }
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn attach_all(db: &PvDatabase, source: &Source, n: usize, tally: &Tally) -> Vec<DbAttachment> {
        (0..n)
            .map(|index| {
                source
                    .attach(
                        db,
                        format!("EV:{index}"),
                        EventMask::VALUE.bits(),
                        None,
                        None,
                        Probe {
                            index,
                            tally: tally.clone(),
                        },
                    )
                    .unwrap()
            })
            .collect()
    }

    /// A scan pass over 2001 monitors of one source while its drain is busy
    /// wakes the drain once: every post after the first finds it running
    /// (C `firstEventFlag` per queue, one `ppendsem` per `event_user`). With
    /// a private `event_user` per monitor each post is a wake.
    #[tokio::test]
    async fn one_scan_pass_over_2001_monitors_wakes_the_busy_drain_once() {
        const N: usize = 2001;
        let db = ai_db(N).await;
        let source = source();
        let gate = Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new()));
        let tally = Tally {
            hold_first: Some(gate.clone()),
            ..Tally::default()
        };
        let subs = attach_all(&db, &source, N, &tally);
        let probe = probe(&source);
        assert!(subs.iter().all(|s| Arc::ptr_eq(&s.drain, &subs[0].drain)));
        let before = settled_wakes(&probe);

        post(&db, 0, 1.0);
        wait_until("the drain to take the first entry", || {
            tally.entered.load(Ordering::Acquire)
        });
        for i in 1..N {
            post(&db, i, 1.0);
        }
        *gate.0.lock() = true;
        gate.1.notify_all();
        wait_until("every entry", || {
            tally.delivered.load(Ordering::Acquire) == N
        });

        let wakes = probe.wakes.load(Ordering::Relaxed) - before;
        assert_eq!(wakes, 1, "{wakes} wakes for {N} posts");
        assert!(wakes as f64 / N as f64 <= 0.061);
    }

    /// An empty chain's first post wakes the parked drain exactly once.
    #[tokio::test]
    async fn the_first_post_on_an_empty_chain_wakes_once() {
        let db = ai_db(1).await;
        let source = source();
        let tally = Tally::default();
        let _subs = attach_all(&db, &source, 1, &tally);
        let probe = probe(&source);
        let before = settled_wakes(&probe);

        post(&db, 0, 1.0);
        wait_until("the entry", || tally.delivered.load(Ordering::Acquire) == 1);
        assert_eq!(settled_wakes(&probe) - before, 1);
    }

    /// Lost-wake probe: three producers post into one source while a
    /// toggler flips its `event_user` between EVENTS_OFF and EVENTS_ON;
    /// once both stop, every monitor's last delivered value is its record's
    /// last posted value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn producers_and_flow_control_toggles_lose_no_wake() {
        const PER: usize = 4;
        const POSTS: usize = 20_000;
        let db = Arc::new(ai_db(3 * PER).await);
        let source = source();
        let tally = Tally::default();
        let _subs = attach_all(&db, &source, 3 * PER, &tally);
        let user = source.drain.lock().upgrade().unwrap().user.clone();

        let stop = Arc::new(AtomicBool::new(false));
        let toggler = {
            let (stop, user) = (stop.clone(), user.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    user.flow_ctrl_on();
                    std::thread::yield_now();
                    user.flow_ctrl_off();
                    std::thread::yield_now();
                }
            })
        };
        let producers: Vec<_> = (0..3)
            .map(|p| {
                let db = db.clone();
                std::thread::spawn(move || {
                    for n in 1..=POSTS {
                        let i = p * PER + n % PER;
                        post(&db, i, n as f64);
                    }
                })
            })
            .collect();
        for p in producers {
            p.join().unwrap();
        }
        stop.store(true, Ordering::Release);
        toggler.join().unwrap();
        user.flow_ctrl_off();

        let want: HashMap<usize, f64> = (0..3 * PER)
            .map(|i| {
                let (p, r) = (i / PER, i % PER);
                let last = (1..=POSTS).rev().find(|n| n % PER == r).unwrap();
                (p * PER + r, last as f64)
            })
            .collect();
        wait_until("every monitor's last value", || *tally.last.lock() == want);
    }
    /// Records a delivery that finishes after its attachment's drop returned.
    struct Late {
        entered: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        late: Arc<AtomicUsize>,
    }

    impl EventDelivery for Late {
        fn deliver(&self, _: MonitorEvent) -> impl std::future::Future<Output = ()> + Send {
            self.entered.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(2));
            if self.dropped.load(Ordering::Acquire) {
                self.late.fetch_add(1, Ordering::AcqRel);
            }
            std::future::ready(())
        }
    }

    /// C `db_cancel_event` syncs with `event_task` (`dbEvent.c:613-631`):
    /// once an attachment's drop returns, the drain delivers nothing more to
    /// it, whether the drop lands mid-delivery or between entries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nothing_is_delivered_to_an_attachment_after_its_drop_returns() {
        let db = ai_db(1).await;
        let source = DbEventSource::<Late>::new(
            "dbEvPin",
            crate::runtime::task::ThreadPriority::Custom(17),
            crate::runtime::task::StackSizeClass::Medium,
        );
        let late = Arc::new(AtomicUsize::new(0));
        for round in 0..100u64 {
            let (entered, dropped) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            );
            let sub = source
                .attach(
                    &db,
                    "EV:0",
                    EventMask::VALUE.bits(),
                    None,
                    None,
                    Late {
                        entered: entered.clone(),
                        dropped: dropped.clone(),
                        late: late.clone(),
                    },
                )
                .unwrap();
            for n in 0..3 {
                post(&db, 0, (round * 3 + n) as f64);
            }
            if round % 2 == 0 {
                wait_until("a delivery under way", || entered.load(Ordering::Acquire));
            } else {
                std::thread::sleep(Duration::from_micros(round % 7 * 300));
            }
            let at = Instant::now();
            drop(sub);
            dropped.store(true, Ordering::Release);
            assert!(
                at.elapsed() < Duration::from_secs(2),
                "round {round}: drop hung"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(late.load(Ordering::Acquire), 0, "deliveries after drop");
    }
}
