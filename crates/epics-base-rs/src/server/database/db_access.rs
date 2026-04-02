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
use crate::runtime::sync::mpsc;
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
            .await
            .ok()
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0)
    }

    pub async fn get_i16(&self) -> i16 {
        self.db
            .get_pv(&self.name)
            .await
            .ok()
            .and_then(|v| v.to_f64())
            .map(|f| f as i16)
            .unwrap_or(0)
    }

    pub async fn get_string(&self) -> String {
        match self.db.get_pv(&self.name).await {
            Ok(EpicsValue::String(s)) => s,
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
            .put_pv(&self.name, EpicsValue::String(v.to_string()))
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
            .put_pv_and_post_with_origin(&self.name, EpicsValue::String(v.to_string()), self.origin)
            .await
    }

    /// Write a value AND trigger record processing (like CA put).
    /// Use for motor VAL, busy records, etc. where processing drives hardware.
    pub async fn put_f64_process(&self, v: f64) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        let _ = self
            .db
            .put_record_field_from_ca(record_name, field, EpicsValue::Double(v))
            .await?;
        Ok(())
    }

    /// Write i16 + trigger processing. For bo/mbbo commands.
    pub async fn put_i16_process(&self, v: i16) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        let _ = self
            .db
            .put_record_field_from_ca(record_name, field, EpicsValue::Short(v))
            .await?;
        Ok(())
    }

    /// Write i32 + trigger processing. For longout commands.
    pub async fn put_i32_process(&self, v: i32) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        let _ = self
            .db
            .put_record_field_from_ca(record_name, field, EpicsValue::Long(v))
            .await?;
        Ok(())
    }

    /// Write string + trigger processing. For stringout commands.
    pub async fn put_string_process(&self, v: &str) -> CaResult<()> {
        let (record_name, field) = parse_pv_name(&self.name);
        let _ = self
            .db
            .put_record_field_from_ca(record_name, field, EpicsValue::String(v.to_string()))
            .await?;
        Ok(())
    }

    /// Read i32 value. For longin/longout.
    pub async fn get_i32(&self) -> i32 {
        self.db
            .get_pv(&self.name)
            .await
            .ok()
            .and_then(|v| match v {
                EpicsValue::Long(i) => Some(i),
                other => other.to_f64().map(|f| f as i32),
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
    rx: mpsc::Receiver<MonitorEvent>,
    pv_name: String,
    /// If non-zero, events with this origin are silently skipped.
    /// Used to filter out self-triggered events from the same writer.
    ignore_origin: u64,
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
        let (record_name, field) = parse_pv_name(pv_name);
        let field = field.to_ascii_uppercase();
        let rec = db.get_record(record_name).await?;
        let sid = next_sid();
        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        let rx = {
            let mut instance = rec.write().await;
            instance.add_subscriber(&field, sid, DbFieldType::Double, mask)
        };
        Some(Self {
            rx,
            pv_name: pv_name.to_string(),
            ignore_origin,
        })
    }

    /// Wait for the next value change. Returns the new value as f64.
    /// Silently skips events matching `ignore_origin`.
    pub async fn recv_f64(&mut self) -> Option<f64> {
        loop {
            let event = self.rx.recv().await?;
            if self.ignore_origin != 0 && event.origin == self.ignore_origin {
                continue; // Skip self-triggered event
            }
            return event.snapshot.value.to_f64();
        }
    }

    /// Wait for the next value change. Returns the raw EpicsValue.
    /// Silently skips events matching `ignore_origin`.
    pub async fn recv(&mut self) -> Option<EpicsValue> {
        loop {
            let event = self.rx.recv().await?;
            if self.ignore_origin != 0 && event.origin == self.ignore_origin {
                continue;
            }
            return Some(event.snapshot.value);
        }
    }

    pub fn pv_name(&self) -> &str {
        &self.pv_name
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
                match sub.rx.try_recv() {
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
            // No events ready — yield briefly then retry
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
