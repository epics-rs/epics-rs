# epics-bridge-rs Source Review — 2026-05-26

Scope:

- Crate: `crates/epics-bridge-rs`
- Upstream references (read-only):
  - pvxs C++ at `/Users/stevek/codes/epics-modules/pvxs/` — for PVA gateway, QSRV group behavior
  - EPICS base C at `/Users/stevek/codes/epics-base/` — for CA gateway, CA link semantics
  - ca-gateway C++ at `/Users/stevek/codes/epics-modules/ca-gateway/` — for CA gateway parity
  - pva2pva C++ at `/Users/stevek/codes/epics-base/modules/pva2pva/` — for PVA p2p gateway parity
- Areas reviewed: CA gateway event forwarding, PVA gateway raw subscriber lifecycle, CA gateway disconnect alarm, QSRV group priming, pvalink propagation.
- Finding-ID series: `BR-R-N` (the epics-bridge-rs round series; BR-R45–BR-R48 here).
  IDs are never reused; see `docs/review-tagging-conventions.md`.

## Findings

### BR-R45 — CA gateway upstream event forwarding strips upstream alarm status, severity, and timestamp

Severity: High

Status: Fixed

Evidence:

- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:463` — forwarding task calls `db_clone.put_pv_and_post(&name, snapshot.value.clone())`, passing only the decoded value. The `snapshot.alarm.status`, `snapshot.alarm.severity`, and `snapshot.timestamp` extracted from the upstream `DBR_TIME_*` frame (via `decode_time` at `crates/epics-base-rs/src/types/codec.rs:711-720`) are silently discarded. The shadow PV's downstream subscribers receive `Snapshot::new(value, 0, 0, now_wall())` — zero alarm, local wall-clock timestamp — regardless of the upstream alarm state or IOC timestamp.
- `epics-modules/ca-gateway/src/gateVc.cc:572-617` — `gateVcData::setEventData(dd)` stores the full `dbr_time_xxx` GDD (including `alarm_status`, `alarm_severity`, and `epicsTimeStamp`) in `event_data`.
- `epics-modules/ca-gateway/src/gateVc.cc:1143` — `vcPostEvent` calls `postEvent(event_mask, *local_event_data)`, posting the complete stored data to downstream CA clients.
- `crates/epics-ca-rs/src/client/subscription.rs:534` — CA client auto-negotiates `native_type + 14` (DBR_TIME range) for subscriptions, so the upstream frames always carry alarm and timestamp. `subscription.rs:383` calls `decode_dbr` → `decode_time` which extracts them into the Snapshot.

Impact:

Downstream CA clients monitoring a gateway PV always see zero alarm severity and a local wall-clock timestamp regardless of the upstream IOC's actual alarm state and timestamp. An upstream INVALID alarm on a process PV is invisible to monitoring clients through the gateway. Archived waveforms carry the gateway's local time, not the IOC's hardware timestamp.

Fix direction:

Add `put_pv_and_post_with_snapshot(name, snapshot)` to `PvDatabase` and `ProcessVariable::set_from_snapshot(snapshot)` in `epics-base-rs`. Thread `snapshot` (value + alarm + timestamp) through the CA gateway forwarding task instead of `snapshot.value.clone()`.

### BR-R46 — PVA gateway raw subscribers receive no initial cached snapshot

Severity: High

Status: Fixed

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:701-742` — `subscribe_raw_inner` calls `cache.lookup()` (which waits for the first upstream frame), then creates a broadcast receiver via `entry.subscribe_raw()` at line 708. No initial snapshot is delivered before entering the forwarding loop. The broadcast channel is a ring buffer that drops messages when there are no receivers, so the first upstream frame (received before any receiver existed) is permanently lost to new raw subscribers.
- `epics-base/modules/pva2pva/p2pApp/moncache.cpp:285-311` — `MonitorCacheEntry::Requester::start()` checks `entry->havedata` and, when true, copies `entry->lastelem` into the initial monitor element before posting. New subscribers always receive the cached last value as their first monitor event.

Compare: the typed path (`subscribe_inner` at `source.rs:747-801`) correctly delivers `entry.snapshot()` as the first item (lines 772-787). The raw path is missing the equivalent step.

Impact:

A downstream PVA gateway client that opens a MONITOR gets no initial value; it must wait for the next upstream event. For slowly-changing config-style PVs this wait is indefinite. The typed monitor path (`subscribe_inner`) works correctly; only the raw-frames path (F-G12 default-on) is affected.

Fix direction:

Store the most recent raw frame body bytes in `UpstreamEntry` alongside `state` (decoded snapshot). In `subscribe_raw_inner`, after `entry.subscribe_raw()`, synthesize and send an initial `RawMonitorEvent` from those stored bytes before entering the forwarder loop.

### BR-R47 — CA gateway disconnect alarm uses wrong alarm status (NO_ALARM instead of LINK_ALARM)

Severity: Medium

Status: Fixed

Evidence:

- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:502` — `db_clone.post_alarm(&name, 3, 0)` passes `status = 0` (NO_ALARM). The comment reads "status 0 = LINK alarm" which is incorrect; NO_ALARM and LINK_ALARM are different status codes.
- `epics-base/modules/database/src/ioc/db/dbCa.c:460-461` — the C EPICS base sets `pca->sevr = INVALID_ALARM; pca->stat = LINK_ALARM` when a CA link disconnects.
- `crates/epics-base-rs/src/server/recgbl.rs:23` — `pub const LINK_ALARM: u16 = 14`. Status 0 is `NO_ALARM` (recgbl.rs line 18).

Impact:

Downstream CA clients monitoring a gateway PV during upstream disconnect see `INVALID` severity (correct) paired with `NO_ALARM` status (wrong). Operator displays and alarm-management tools that use the alarm status field to determine the *reason* for the alarm (link error vs. hardware limit vs. comm error) will misclassify a disconnect as "unknown/undefined" rather than "link failure". EPICS alarm databases correlating `stat=LINK_ALARM` events with network-health data will miss gateway-proxy disconnects.

Fix direction:

Change `post_alarm(&name, 3, 0)` to `post_alarm(&name, 3, epics_base_rs::server::recgbl::alarm_status::LINK_ALARM)`.

### BR-R48 — PVA gateway raw subscriber can receive the initial cached snapshot twice

Severity: Medium

Status: Fixed

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:708-712` — `subscribe_raw_inner` calls `entry.subscribe_raw()` at line 708, then `entry.snapshot_raw()` at line 712. If the upstream monitor delivers event E between these two calls, the broadcast receiver (created at line 708) queues E, AND `snapshot_raw()` returns E (because `latest_raw` was updated before the broadcast). The spawned task delivers E once as `initial_raw` (line 724-733) and a second time from the broadcast loop (line 735-754). The downstream raw-frame client sees a duplicate initial event.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:806-813` — the monitor callback updates `latest_raw` and calls `tx_raw_inner.send(raw_ev)` unconditionally (no `was_first` guard). The typed path has a `was_first` guard at line 798 that prevents broadcasting event 1; the raw path has no such guard, making the first event the most likely duplicate.
- `epics-base/modules/pva2pva/p2pApp/moncache.cpp:285-311` — `MonitorCacheEntry::Requester::start()` delivers the cached `lastelem` synchronously before the subscription is live, so no upstream event can race the initial delivery. The Rust implementation subscribes first (asynchronous), leaving the race window open.

Impact:

Downstream raw-frame PVA clients receive a duplicate initial monitor event on every subscription to an already-connected PV. For most scalar PVs this is a benign stutter; for clients that treat the first event as a connection-established signal, it may trigger double-initialisation logic.

Fix direction:

Before the `tokio::spawn`, capture `dedup_body: Option<bytes::Bytes> = initial_raw.as_ref().map(|e| e.body.clone())`. Inside the spawn, after delivering `initial_raw`, if the first broadcast event's `body.as_ptr()` equals `dedup_body.take().map_or(null, |b| b.as_ptr())`, skip it. `bytes::Bytes::clone()` is a refcount clone; same monitor callback invocation produces identical `as_ptr()` values, making the pointer a reliable one-shot dedup key.

### BR-R49 — CA gateway shadow PV never fetches DBR_CTRL_* metadata from upstream

Severity: High

Status: Doc-only (structural fix required in `epics-base-rs`)

Evidence:

- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:324-343` — `channel.get()` returns `(DbFieldType, EpicsValue)` only; no display/control metadata is extracted. Shadow PV created at `upstream.rs:375-377` with `initial_value` (bare value) only.
- `crates/epics-base-rs/src/server/pv.rs:317-319` — `ProcessVariable::snapshot()` always returns `display=None`, `control=None`; `set_snapshot` (line 336-342) stores only `snapshot.value`, discarding display/control.
- `crates/epics-base-rs/src/types/codec.rs:413-420, 424-441` — `encode_units` writes zeros when `snapshot.display.is_none()`; `get_limits` returns all-zero limits when display/control are `None`.
- `epics-modules/ca-gateway/src/gatePv.cc:1252-1295` — `connectCB` sets `pv->data_type = DBR_CTRL_SHORT/FLOAT/DOUBLE/ENUM/LONG/CHAR` per native type at connect time.
- `epics-modules/ca-gateway/src/gatePv.cc:930-934` — `gatePvData::get()` calls `ca_array_get_callback(dataType(), 1, chID, ::getCB, this)` to fetch full CTRL metadata on connect; `dataType()` is the `DBR_CTRL_*` type set above.
- No `DBR_CTRL_*` GET and no `DBE_PROPERTY` subscription exist anywhere in `crates/epics-bridge-rs/src/ca_gateway/`.

Impact:

Every CA client that requests `DBR_CTRL_*` or `DBR_GR_*` from a gateway PV receives all-zero values for engineering units, precision, and display/alarm/warning/control limits. Operator displays, archivers, and alarm-management tools that rely on EGU or limit metadata from the gateway are silently broken.

Fix direction:

Structural change required in `epics-base-rs`:
1. Add `display: Option<DisplayInfo>` and `control: Option<ControlInfo>` fields to `ProcessVariable` in `epics-base-rs/src/server/pv.rs`.
2. Expose a `set_display_control(display, control)` method on `ProcessVariable`.
3. In the gateway connect path (`upstream.rs:ensure_subscribed`): after the initial `channel.get()`, issue a `DBR_CTRL_*` GET callback to fetch units/precision/limits; update the shadow PV on receipt.
4. Add a `DBE_PROPERTY` subscription (as in `gatePv.cc:858-862`) so property changes are forwarded throughout the connection lifetime.

### BR-R50 — PVA gateway monitor task exits on upstream disconnect with zero subscribers, leaving stale cache entry

Severity: Medium

Status: Fixed

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:873-878` — after the upstream monitor closes naturally, the task exits when `tx_for_task.receiver_count() == 0 && tx_raw_for_task.receiver_count() == 0`. The `UpstreamEntry` remains in the cache map; its `_monitor_task: AbortOnDrop` now holds an abort handle for a finished task.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:582-620` — `lookup()` returns existing entries without checking whether the monitor task is still alive. A new subscriber arriving within the ~60 s cleanup window receives the stale entry.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:928-939` — `cleanup_tick()` retains an entry when `subscriber_count() > 0`; if a new subscriber arrives after the task exited, they join a dead broadcast channel and receive no events. Their subscription then prevents cleanup from evicting the entry, potentially keeping it alive indefinitely.
- `epics-base/modules/pva2pva/p2pApp/chancache.cpp:78-83` — pva2pva `ChannelCacheEntry::CRequester::channelStateChange` immediately removes the entry from the cache map on `DISCONNECTED` / `DESTROYED`. Any subsequent `lookup()` gets a cache miss and spawns a fresh entry with a new upstream monitor.

Impact:

A client that subscribes to a gateway PV within ~60 s of the upstream IOC cleanly closing its connection (and while no other subscribers are active) receives the cached snapshot as the initial event but no further updates. The gateway neither signals disconnection nor retries the upstream subscription; the subscriber silently stalls. If retrying clients keep poking the entry (each `lookup()` sets `drop_poke=true`), the stale entry is never evicted and the client sees stale data indefinitely.

Fix direction:

Remove the three early exits that abort the monitor task when `receiver_count == 0`:
- `channel_cache.rs:831-833` (startup-error exit)
- `channel_cache.rs:858-860` (runtime-error exit)
- `channel_cache.rs:873-878` (clean-cycle exit)

With these removed, the task loops and retries the upstream reconnect regardless of subscriber count. The `cleanup_tick()` aborts the task (via `AbortOnDrop` in `UpstreamEntry`) when it evicts the entry after two consecutive 30 s ticks with `subscriber_count() == 0`. A subscriber that arrives before cleanup gets a live reconnecting task, not a dead one. This matches pva2pva's invariant that a cached entry always has an active upstream connection attempt in flight.

### BR-R51 — CA gateway access hook does not AND upstream write-access into `AccessDecision`

Severity: Medium

Status: Fixed (initial rights AND; runtime re-notification deferred cross-crate)

Evidence:

- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:694-710` — `build_access_hook` closure consults only local ACF `can_write()`; no upstream `ca_write_access(chID)` is queried or included. `AccessDecision::write` is set from `cfg.can_write()` alone.
- `crates/epics-ca-rs/src/client/mod.rs:2299-2310` — `CaChannel::on_access_rights_change()` exists and provides real-time upstream write-access notification but is never called from `ensure_subscribed` in the gateway.
- `epics-modules/ca-gateway/src/gateVc.cc:331-342` — `gateVcChan::writeAccess()` returns `asclient->writeAccess() && vc->writeAccess()` — compound AND of local ACF (`asclient`) and upstream access (`vc->write_access`). Both must permit before the downstream client is shown write-allowed.
- `epics-modules/ca-gateway/src/gatePv.cc:395-396` — at connect time, `activate()` sets `vc->setWriteAccess(ca_write_access(chID))` from the upstream channel's current access state.
- `epics-modules/ca-gateway/src/gatePv.cc:1851-1852` — in `accessCB` (upstream access-rights change callback), updates `vc->setWriteAccess(ca_write_access(args.chid))` and fires `postAccessRights()` to notify all downstream clients.

Impact:

A downstream CA client connecting to a gateway PV where the upstream IOC has write-access denied (upstream ACF, EPICS access security, or read-only field) sees `AccessDecision::write = true` in the gateway's `CA_PROTO_ACCESS_RIGHTS` reply. Operator displays and alarm-management tools that use the access-rights field to show lock icons (e.g. CSS/Phoebus, camonitor `-w`) display the channel as writable when it is upstream-read-only. Individual `caput` operations do eventually fail after being forwarded upstream (ECA_NORDACCESS is propagated back via put-notify), but the initial advertisement is wrong. Additionally, if the upstream IOC changes write access at runtime (lockout, protection enable), the Rust gateway never updates connected downstream clients — they continue seeing the stale access-rights until they disconnect and reconnect.

Fix direction:

Within `epics-bridge-rs` (initial rights AND):
1. In `ensure_subscribed` (upstream.rs), after `channel.get()`: query `channel.info().access_rights.write` and store in `Arc<AtomicBool>` as `upstream_write`.
2. Pass `upstream_write.clone()` to `build_access_hook`; AND `upstream_write.load(Relaxed)` into the write decision so `AccessDecision::write = local_acf_write && upstream_write`.
3. Call `channel.on_access_rights_change(|r| upstream_write.store(r.write, Relaxed))` and store the returned `EventWatcher` in `UpstreamSubscription` (dropped on `unsubscribe`, aborting the watcher task).

Deferred to main worker (cross-crate, `epics-ca-rs`):
When upstream write-access changes at runtime, the `AtomicBool` updates but connected downstream clients are not re-notified until they reconnect. The C gateway calls `postAccessRights()` → per-channel `postAccessRightsEvent()`. The Rust CA server has an `acf_reload_tx` broadcast that triggers `reeval_access_rights` on all client tasks, but no public "fire access-reload broadcast without reloading the ACF file" API. Adding `CaServer::notify_access_change()` (just fire the broadcast) would let the gateway watcher trigger per-server re-evaluation; deferred to main worker.

### BR-R52 — CA gateway shadow-PV events use single-class event mask; `DBE_LOG` and `DBE_ALARM`-only subscribers receive no events

Severity: Medium

Status: Doc-only (structural fix required in `epics-base-rs`)

Evidence:

- `crates/epics-base-rs/src/server/pv.rs:450` — `notify_subscribers_from_snapshot` posts with `let post = EventMask::VALUE;` (0x01 only). Every upstream monitor event forwarded via `put_pv_and_post_snapshot` fires exclusively as a `DBE_VALUE` event.
- `crates/epics-base-rs/src/server/pv.rs:358-359` — `post_alarm` posts with `let post = EventMask::ALARM;` (0x04 only). The disconnect LINK_ALARM fires exclusively as a `DBE_ALARM` event.
- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:503-505` — the gateway forwarding task calls `db_clone.put_pv_and_post_snapshot(&name, snapshot.clone())` for every upstream update, and `db_clone.post_alarm(&name, 3, LINK_ALARM)` on disconnect. Both reach subscribers via the single-class paths above.
- `epics-modules/ca-gateway/src/gateVc.cc:374-376` — the C gateway sets `select_mask |= (alarmEventMask() | valueEventMask() | logEventMask())`, i.e. `DBE_VALUE | DBE_ALARM | DBE_LOG` (0x07), before calling `postEvent(event_mask, *local_event_data)`. All downstream subscribers — regardless of whether they subscribed with `VALUE`, `ALARM`, or `LOG` — receive every upstream event.
- `crates/epics-base-rs/src/server/recgbl.rs:39-41` — `EventMask::VALUE = 0x01`, `EventMask::LOG = 0x02`, `EventMask::ALARM = 0x04`.

Impact:

Downstream CA clients that subscribe with `DBE_LOG` (0x02) — the mask used by archiver appliances such as Channel Archiver / Phoebus Archiver — receive zero events from any Rust CA gateway PV. Their subscription silently produces no data. Clients with `DBE_ALARM`-only (0x04) subscriptions (alarm systems, operator panels requesting alarm-change notification) also receive no normal upstream events, only the disconnect LINK_ALARM. Clients with the common `DBE_VALUE | DBE_ALARM` (0x05) combined mask are unaffected.

Fix direction:

Structural change required in `epics-base-rs`:
1. Change `notify_subscribers_from_snapshot` (`pv.rs:450`) from `let post = EventMask::VALUE` to `let post = EventMask::VALUE | EventMask::LOG | EventMask::ALARM` so any subscription mask that overlaps at least one of those bits fires.
2. Optionally change `post_alarm` (`pv.rs:359`) to `EventMask::ALARM | EventMask::LOG` so archiver clients with `DBE_LOG` also see disconnect/alarm events.
3. No changes required in `epics-bridge-rs`: the gateway's `put_pv_and_post_snapshot` and `post_alarm` calls are already correct; only the per-subscriber event-class gate inside `ProcessVariable` needs widening.

## Uncertain candidates

None identified this round.
