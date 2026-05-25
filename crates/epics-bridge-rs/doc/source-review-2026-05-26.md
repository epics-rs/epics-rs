# epics-bridge-rs Source Review — 2026-05-26

Scope:

- Crate: `crates/epics-bridge-rs`
- Upstream references (read-only):
  - pvxs C++ at `/Users/stevek/codes/epics-modules/pvxs/` — for PVA gateway, QSRV group behavior
  - EPICS base C at `/Users/stevek/codes/epics-base/` — for CA gateway, CA link semantics
  - ca-gateway C++ at `/Users/stevek/codes/epics-modules/ca-gateway/` — for CA gateway parity
  - pva2pva C++ at `/Users/stevek/codes/epics-base/modules/pva2pva/` — for PVA p2p gateway parity
- Areas reviewed: CA gateway event forwarding, PVA gateway raw subscriber lifecycle, CA gateway disconnect alarm, QSRV group priming, pvalink propagation.
- Finding-ID series: `BR-SR-N`

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

## Uncertain candidates

None identified this round.
