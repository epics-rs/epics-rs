# Changelog

## v0.13.6 — 2026-05-03

Fixes a false-positive `PeriodCollapse` cascade against IOCs that
emit standard `online_notify_task` ramp-up beacons (20 ms doubling
to 15 s) — including epics-ca-rs's own server, so an IOC built on
this crate would trip its own client-side beacon monitor on every
fresh run. Symptom in the field: `get_with_metadata(timeout=2.0)`
against the mini-beamline IOC failed for the first ~10 s of the
run, driven by transport watchdog flag → echo probe → 5 s
timeout → spurious channel disconnect.

### epics-ca-rs

- **fix**: `BeaconState::period_estimate` is now `Option<Duration>`
  with `None` initial. The first observed inter-beacon interval is
  adopted as the initial estimate (libca `bhe.cpp:51,199` parity:
  `averagePeriod = -DBL_MAX` until `averagePeriod = currentPeriod`
  on the second beacon), and the EMA blends in from there. The
  prior `Duration::from_secs(15)` placeholder caused every
  ramp-up beacon past the 50 ms `MIN_PERIOD_COLLAPSE_INTERVAL`
  floor to satisfy `actual_interval < period_estimate / 3` and
  classify as `PeriodCollapse`. The PeriodCollapse classification
  now skips when `period_estimate.is_none()`, which is by
  construction true until the second beacon — and after that
  the EMA tracks the actual server cadence rather than a hardcoded
  guess.
- **test**: regression guard
  `rsrv_rampup_beacons_do_not_fire_period_collapse` simulates the
  full 11-beacon ramp-up sequence (20 ms, 40 ms, …, 10.24 s) and
  asserts no `PeriodCollapse` fires past the initial
  `FirstSighting`. Total `epics-ca-rs --lib` is now 96 tests.

## v0.13.5 — 2026-05-03

Doctest-only patch on top of v0.13.4. The `process_bucket` doc
in `epics-ca-rs/src/client/search.rs` had a 4-space-indented
pseudo-code line that rustdoc treated as a Rust code block,
breaking `cargo test -p epics-ca-rs` (doctests). Production
behaviour unchanged from v0.13.4 — only docs.

### epics-ca-rs

- **fix**: wrap the `next = (idx + min(attempt, nBuckets)) %
  nBuckets` formula in a ```text fence so rustdoc skips it
  instead of compiling. Caught by GitHub Actions CI; pre-publish
  verification used `cargo test --lib` which skips doctests.

## v0.13.4 — 2026-05-03

PVA + CA reconnect-after-IOC-restart parity with pvxs
`Channel::disconnect`. Skips v0.13.3 because the PVA-only fix
matured into a paired PVA+CA bundle in the same release cycle.

End-to-end pvmonitor-rs / camonitor-rs reconnect after a brief
IOC restart was 5-30 s + dependent on beacon arrival luck;
post-fix it's ≤ 1 s on both paths (next 1 Hz tick).

### epics-pva-rs

- **fix**: `placement_bucket` / `cascade_smoothed_next` free fns
  extracted; `Reconnect` placement is now `current_bucket`
  (pvxs `client.cpp:213` parity, holdoff = 0). Per-channel retry
  escalation follows pvxs `tickSearch:1193-1196` (`nSearch+1`
  bucket forward push: 1 s, 2 s, 3 s, …, 30 s cap) plus 100+
  delta cascade smoothing (`client.cpp:1199-1206`). Removes the
  earlier `(current+1+sid%30)` formula and the misnamed
  `holdoff_cycles + RETRY_HOLDOFF_BUCKETS=10` that conflated
  pvxs's pre-CREATE_CHANNEL holdoff with the steady-state retry
  cadence.
- **fix**: `Channel::ensure_active` no longer applies an outer
  timeout to `Reconnect` `find()`. The 200 ms cap was
  cancelling SEARCHes before their bucket fired (F6 zombie-drop
  path). The engine now drives recovery indefinitely until
  SEARCH_RESPONSE arrives or the caller drops the future.
- **fix**: paired with the above, single-shot ops
  (`pvget` / `pvput` / `pvrpc`) now wrap `ensure_active` in
  `tokio::time::timeout(op_timeout, …)`. Without this the
  user-facing future would hang forever when the server
  permanently disappears. Monitor loops keep the bare
  unbounded await — their cancel path is `SubscriptionHandle`
  drop.
- **internal**: stale "sid-hashed" comments rewritten;
  `RECONNECT_FIND_TIMEOUT` constant deleted; `Pending.holdoff_cycles`
  field deleted; SearchReason doc rewritten with the new model.
- **test**: 7 unit tests against the extracted free fns
  (placement, escalation, smoothing default, smoothing boundary
  at delta=100), 3 integration tests
  (`reconnect_search_broadcasts_within_one_tick`,
  `hurry_up_kicks_pending_searches_at_fast_tick_cadence`,
  `retry_escalation_pvxs_pattern`), 1 regression guard
  (`reconnect_find_does_not_complete_without_response` — catches
  any future PR that reintroduces a caller-side timeout). Total
  `epics-pva-rs --lib` is now 191 tests.

### epics-ca-rs

- **fix**: same five-part pvxs-parity port applied to
  `epics-ca-rs/src/client/search.rs`. CA's 3-way `SearchReason`
  (Initial / BeaconAnomaly / Reconnect) folds into the shared
  `placement_bucket` shape: Initial and BeaconAnomaly land at
  `current+1` (immediate broadcast / fast-tick-driven retransmit
  pair), Reconnect at `current_bucket`. Pre-existing comment on
  `handle_request` flagged this as the gap that "made ca-rs
  single-channel reconnect feel slower than pva-rs"; the port
  closes it.
- **fix**: `process_bucket` re-arm is inline (split-borrow on
  `state.{pending, buckets}`) so `cascade_smoothed_next` sees
  the running per-tick bucket buildup. The earlier batched
  `rearm: Vec<(u32, usize)>` defeated within-tick smoothing — a
  5000-channel mass-disconnect saw delta=0 for every cid and
  piled them all into `current+1`. PVA-rs uses the equivalent
  pattern; this commit recovers parity.
- **internal**: `RETRY_HOLDOFF_CYCLES` const + `holdoff_cycles`
  field deleted; bucket-placement / retry-escalation logic
  shared via the same free-fn pair as PVA.
- **test**: replaced the now-incorrect `reconnect_bucket_spread`
  test (it asserted cid-hash spread, which we no longer do)
  with five unit tests against the extracted free fns + 2
  integration tests (`reconnect_search_broadcasts_within_one_tick`,
  `retry_escalation_pvxs_pattern`). Total `epics-ca-rs --lib`
  is now 95 tests.

## v0.13.2 — 2026-05-03

Single-fix patch for the multi-IOC-on-one-host workflow: starting a
second PVA IOC while a first was already running used to panic the
server task with `Os { code: 98, kind: AddrInUse }` from the
hard-coded TCP port 5075 bind. CA path was unaffected (its server
defaults to ephemeral TCP). UDP 5076 was already shareable across
local PVA processes via SO_REUSEADDR + SO_REUSEPORT, and the
v0.13.0 ORIGIN_TAG forwarding path already routes SEARCH between
co-bound UDP responders — only the TCP single-bind was the
remaining bottleneck.

### epics-pva-rs

- **fix**: `PvaServer::start` falls back to ephemeral (`tcp_port = 0`)
  on `AddrInUse` / `PermissionDenied` if the requested port is
  non-zero. Single retry, mirrors pvxs `serverconn.cpp:493-499`
  (`bind_addr.setPort(0); fallback = false; continue;`). The
  actually-bound port flows out via `bound_tcp_port` and is what
  the UDP responder advertises in SEARCH_RESPONSE / beacons, so
  clients still reach the second server. A `tracing::warn!` with
  requested / bound / error fields is emitted on fallback so
  operators can see when 5075 was contended.
- **test**: 2 unit tests in `tcp_fallback_tests` —
  port-blocked-by-pinned-listener triggers fallback (no panic,
  different port returned, blocker still holds original);
  requested-port-available happy path. Total `epics-pva-rs --lib`
  is now 184 tests (up from 182).

## v0.13.1 — 2026-05-03

CA client beacon-watchdog rewrite for libca parity. Eliminates a
false-positive disconnect storm that surfaced whenever multiple
`CaClient` instances co-existed in one process (e.g. pyepics shim +
ophyd-async backend + integration-test fixture, all in the same
Python process). Each fresh client used to fire a spurious
`EchoProbe` on its very first beacon from every IOC; under load
the 5-s echo deadline tripped and produced cascading
`TcpClosed` → reconnect events that the user perceived as random
"restored N subscriptions" log spam.

The fix splits beacon handling into a libca-style dual-path
design: search-engine wake-up on classified anomalies, and a
per-circuit watchdog that healthy beacons refresh and anomaly
beacons leave alone. Internal-only API changes — no public
breaking changes.

### epics-ca-rs

- **fix**: beacon anomalies are now classified into
  `BeaconAnomalyKind::{FirstSighting, IdMismatch, PeriodCollapse}`.
  Only the latter two emit warn-level "IOC may have restarted"
  diagnostics and feed the
  `ca_client_beacon_anomalies_total` metric; `FirstSighting` is a
  per-client bookkeeping event (our beacon map was empty for this
  server, not "the IOC restarted") and now logs at debug under
  `ca_client_beacon_first_sighting_total`. Resolves the misleading
  warns that started appearing for every IOC at process start when
  several `CaClient`s came up in parallel.
- **fix**: removed the immediate `EchoProbe` on beacon anomaly
  (libca's `tcpRecvWatchdog` model). The transport's per-circuit
  read loop is now structured around a single pinned
  `tokio::time::Sleep` whose deadline is mutated in place by:
  data arrival (clear flags + reset to now + 30 s — libca
  `messageArrivalNotify`), healthy beacon arrival (reset to
  now + 30 s when the anomaly flag isn't set — libca
  `beaconArrivalNotify`), and anomaly beacon arrival (set sticky
  flag, deadline UNCHANGED — libca `beaconAnomalyNotify`).
  Idle expiry still sends an echo and switches to the 5-s
  echo-pending state; the change is that we no longer pre-empt
  the 30-s schedule on a beacon anomaly. The previous fast-track
  was the trigger for the disconnect storm under load.
- **fix**: libca `bhe.cpp:179` parity (narrowed) — a beacon whose
  sequence number jumps forward by 2 or 3, or backwards by 1-4,
  is dropped as a duplicate-route artifact rather than classified
  as `IdMismatch`. Without this, multi-NIC sites that delivered
  redundant copies tripped the watchdog flag and suppressed
  healthy-beacon refreshes for ~30 s on what was in reality a
  perfectly healthy IOC. We deliberately narrow libca's
  256-id backwards window to 4 because our `IdMismatch` branch
  catches restart-to-1 directly via the id sequence (libca relies
  on period-collapse instead and would otherwise swallow that
  signal into the dedup path).
- **fix**: `BeaconArrival` routing falls back to port-only
  matching when the beacon's announced address doesn't exactly
  match an operational circuit's `server_addr`. Two real-world
  cases fold into the same fallback: INADDR_ANY (the IOC sent
  `available = 0` and the upstream repeater didn't rewrite it)
  and multi-homed IOCs whose beacon NIC differs from the NIC the
  search-reply came in on. Cross-host port collisions cause a
  benign false-refresh — the wrong circuit's deadline is pushed
  by 30 s, but its own watchdog still detects death within
  30 + 5 s if it actually died. Routing logic extracted into
  `beacon_arrival_targets` and unit-tested.
- **fix**: `connect_server` now runs in a `tokio::task::JoinSet`
  rather than awaiting inline in the transport command loop.
  Previously a 5-s TCP / 15-s TLS handshake on server A blocked
  every other command — `BeaconArrivalNotify` for already-
  connected circuits, `CreateChannel` for server B, etc. — for
  the duration. Per-server FIFO is preserved by a
  `HashMap<SocketAddr, Vec<TransportCommand>>` queue that drains
  on connect completion. Connect failure surfaces
  `ChannelCreateFailed` for queued `CreateChannel` commands and
  a single `TcpClosed` so the coordinator can clear server
  state.
- **internal**: introduced `CoordRequest::BeaconArrival` (separate
  from `ForceRescanServer`); `TransportCommand::EchoProbe` removed
  in favour of `TransportCommand::BeaconArrivalNotify`. Read loop
  drops `Arc<Notify>` echo_probe in favour of an
  `mpsc::UnboundedSender<bool>` per circuit. The `biased;` hint on
  read_loop's select is removed — tokio's randomized polling
  gives starvation-free fairness, and the bias was risking the
  opposite of what the comment claimed.
- **test**: 3 new transport `read_loop` virtual-clock tests
  (healthy-beacon-extends, anomaly-suppresses-refresh,
  data-clears-flag) using `tokio::test(start_paused = true)` and
  `tokio::io::duplex` for the mock peer; 5 new `beacon_monitor`
  classification / dedup tests; 6 new `beacon_arrival_targets`
  routing tests. Total `epics-ca-rs --lib` is now 89 tests, up
  from 76.
- **dev-deps**: added `tokio` with `test-util` feature for the
  virtual-clock tests.

## v0.13.0 — 2026-05-01

Operational fixes for the local-IOC zero-config workflow + CLI tool
output parity with the C counterparts. Two SEARCH-path bugs that
left `pvget-rs PV` hanging against a local pva-rs server are fixed
at root cause; the four CA / four PVA tools now match `caget` /
`pvget` byte-for-byte on the legacy `-M nt` path. New ORIGIN_TAG
forwarding port lands the pvxs UDP-collector mechanism for
multi-server-on-one-host topologies.

### Breaking
- CLI tool default output (caget-rs / camonitor-rs / caput-rs /
  cainfo-rs / pvget-rs / pvmonitor-rs / pvput-rs) now matches the
  legacy C tools byte-for-byte (PV name padded to 30 chars, `%g`
  6-digit precision, double-space ts↔value, `Old :` / `New :` echo
  with timestamps). Scripts that parsed the previous shape need
  updating.

### epics-pva-rs
- **perf**: `pvget-rs <PV>` first-response latency 1 s → 5–10 ms
  (legacy `pvget` parity). Two root causes: search-engine
  `Multi`/FindAll deadline check was tied to the 1 Hz tick — a
  `SEARCH_RESPONSE` arriving in 5 ms still sat in `accumulated`
  until the next tick; `channel.ensure_active` used `find_all()`
  (always 200 ms) for the initial resolve. Added a
  `tokio::time::sleep_until(earliest_deadline)` arm to the engine
  select; switched ensure_active to `find()` (delivers on first
  response) wrapped in a `MULTI_SERVER_WINDOW` timeout.
- **fix**: `EPICS_PVA_AUTO_ADDR_LIST=YES` (the default) now adds
  per-NIC IPv4 broadcast addresses + `127.0.0.1` to SEARCH
  destinations. Previously only `255.255.255.255` was sent — macOS
  doesn't reliably translate that into per-subnet broadcast, so
  local IOCs bound to `192.168.X.255:5076` never saw the SEARCH.
- **fix**: client beacon socket no longer binds the loopback NIC.
  SO_REUSEPORT load-balanced inbound packets between the client
  beacon socket and any local pva-rs server's UDP responder,
  randomly losing SEARCH traffic.
- **fix**: `EPICS_PVA_ADDR_LIST` DNS hostnames (`localhost`,
  `ioc01.lab.local`, etc.) now resolve. Spawn-time resolve via
  `parse_addr_list_with_port`; IPv4 preferred over IPv6 since
  macOS's `localhost` → `::1` ordering broke
  `EPICS_PVA_ADDR_LIST=localhost`.
- **feat**: ORIGIN_TAG forwarding port (server-side). UDP responder
  receives `CMD_ORIGIN_TAG` packets on `224.0.0.128` loopback
  multicast and forwards unicast SEARCHes via the same channel for
  multi-server-on-one-host topologies. `Origin` enum
  (Direct / FromOriginTag) tags how a SEARCH reached the responder;
  anti-loop guard prevents re-forwarding.
- **feat**: `PvaCodec::build_origin_tag_prefix` /
  `try_peel_origin_tag` codec helpers (24-byte prefix shape, BE PVA
  header `cmd=22 len=16` + IPv4-mapped IPv6 dest).
- **feat**: legacy `pv*` flag set on every PVA tool (`-V`, `-w`,
  `-r`, `-p`, `-M`, `-v`, `-q`, `-d`).

### epics-ca-rs
- **fix**: `EPICS_CA_ADDR_LIST` with an unresolvable hostname token
  now drops the bad entry and continues. Previously the first bad
  token propagated via `?` and panicked main() (caget / camonitor /
  caput).
- **feat**: full C-tool flag set on every CA tool — `-V`, `-t`,
  `-a`, `-d`, `-c`, `-p`, `-n`, `-#`, `-S`, `-e`/`-f`/`-g`, `-s`,
  `-lx`/`-lo`/`-lb`, `-0x`/`-0o`/`-0b`, `-F`. `-a`/`-l` echo real
  DBR_TIME server timestamps + alarm pairs.

### epics-base-rs
- **feat**: `IfaceMap::spawn_refresh(period)` — periodic
  `getifaddrs()` poller (pvxs `IfMapDaemon` parity, 15 s default).
  Refreshes the shared interface snapshot so dynamic-NIC scenarios
  (DHCP / hot-plug / VM live-migration) update SEARCH/beacon
  destinations without a process restart.
- **feat**: `AsyncUdpV4::bind_non_loopback(port, broadcast)` —
  bind one socket per IPv4 NIC except loopback. Used by the PVA
  client beacon socket to avoid SO_REUSEPORT races with co-located
  servers.
- **feat**: `bind_loopback_mcast(port)` — wildcard-bound UDP
  socket joined to `224.0.0.128` via `127.0.0.1` for the
  ORIGIN_TAG forwarding channel.

## v0.12.0 — 2026-04-30

PVA/CA stability sweep. A cold-eye cross-review across pva-rs / ca-rs
/ bridge-rs surfaced dead-letter disconnect paths and silent
beacon-anomaly handling that unit tests had missed; pvxs git-history
archaeology (1220 commits triaged) ported the remaining
protocol-level gaps.

### Breaking
- `SearchEngine::find` / `find_all` gain a `reason: SearchReason`
  argument so mass-reconnect cascades spread across the bucket ring
  instead of bursting in a single tick.

### epics-pva-rs
- Server-initiated `CMD_DESTROY_CHANNEL` now cancels every in-flight
  op for the destroyed SID; monitor streams previously hung silently
  until the whole TCP connection died.
- `CMD_MESSAGE` from server is decoded and logged at the level
  matching its `mtype` — previously corrupted the monitor data
  stream (mtype=0) or was silently dropped (Warn/Err).
- `DESTROY_REQUEST` payload corrected to `sid + ioid` (no spurious
  subcmd byte) for pvxs interop.
- `DiscoverPing` uses the spec wire format (`MustReply` flag, empty
  protocol/channel list, raw `::`); old packet shape was silently
  ignored by pvxs servers.
- `random_guid()` seeds from `/dev/urandom`; `BeaconTracker` caps
  at 20 000 entries with warn-once on cap (DoS / poison-feed
  defence).
- Reason-aware search: `Reconnect` is sid-hashed across all 30
  buckets with no immediate fire; `Initial` keeps the immediate
  broadcast for fast single-channel latency.
- `find_all` flushes `Vec::new()` at deadline + drops closed
  responders so missing PVs no longer hang past the caller's
  timeout.

### epics-ca-rs
- `CMD_SERVER_DISCONN` wakes blocked `caget` / `caput` waiters with
  `CaError::Disconnected` (was leaving them parked until the
  caller's outer timeout).
- Reason-aware search scheduler matches the pva-rs split.
- Beacon stale-prune (180 s) replaces the every-beacon soft-poke
  that trapped multi-IOC networks in 200 ms fast-tick mode.
- Multi-NIC duplicate-beacon detection (same `cid`) + 50 ms
  period-collapse floor.
- Nameserver TCP read task no longer leaks on client shutdown.

### epics-bridge-rs
- `BeaconAnomaly::request()` from `ca_gateway` now actually emits a
  beacon via the new `CaServer::beacon_anomaly_handle` pulse —
  previously silent on the wire.

### Tooling / docs
- `archaeology/pvxs/` — full pvxs 1220-commit cross-check with
  per-batch verdicts.

## v0.11.1 — 2026-04-29

Patch release. v0.11.0 shipped a regression in which default
`pvmonitor` callers stalled after ~5 events; this release fixes that
and supersedes v0.11.0 (which has been yanked from crates.io).

### epics-pva-rs
- **fix**: server-side pipeline credit window (P-G11, originally added
  in v0.10.5 → 0.11.0 round-6) was applied unconditionally to every
  Monitor op. pvxs only enables flow control when the client's
  pvRequest sets `record._options.pipeline=true`; without that opt-in,
  default `pvmonitor` callers stalled after the initial snapshot + 4
  updates. The window is now gated on the actual pvRequest option.

## v0.11.0 — 2026-04-29

Highlights since v0.10.5:

### epics-pva-rs
- Axum-style PVA service framework: `#[pva_service]` attribute macro,
  `#[derive(NTScalar)]` / `#[derive(NTTable)]`, `pvget_typed` /
  `pvput_typed` / `pvmonitor_typed` typed entry points.
- Zero-copy `ScalarArray` encode/decode (memcpy fast path) and
  raw-frame monitor forwarding (default-on in bridge gateways).
- `PvaClient` API: `discover()`, `pvget_with_request()`,
  `pvput_with_request()`, `pvmonitor_with_request()`, `pvput_field()`,
  `pvinfo()` now uses `GET_FIELD` instead of full `GET`,
  per-server TLS SNI from `EPICS_CA_NAME_SERVERS` hostnames,
  peer connection stats, async `Operation` handle.
- `PvaServer::start` binds the TCP listener synchronously inside
  `start()`, removing the pick-and-drop race that previously needed
  cross-binary serialisation in tests.

### epics-bridge-rs
- `pva_gateway` — PVA-to-PVA proxy mirroring pvAccessCPP's `pva2pva`
  / `p2pApp`, with multi-downstream fan-out and tower-style
  middleware (`ReadOnlyLayer`, `AclLayer`, `AuditLayer` with mpsc
  sink + `Put` / `Get` / `Subscribe` / `Rpc` event kinds).
- Stability gap closures from kodex-driven re-audits (rounds 4–18):
  segmented-message reassembly, `Vec::with_capacity` OOM caps,
  beacon burst-then-slowdown smoothing, server task leak on
  disconnect, GET_FIELD on unknown SID, audit-string allocation
  cap, search-request OOM follow-up.

### epics-tools-rs
- `procserv` — Rust port of `epics-modules/procServ` (forkpty
  child supervisor with restart policy and telnet log shell).

### Tooling
- `cargo-nextest` adoption (`.config/nextest.toml`) — default-suite
  warm runtime drops from ~30 s to ~7 s. Test-groups cap concurrency
  on PVA listener / softIoc / tempfile-bound suites.
- Workspace clippy clean under `-D warnings`.

## v0.10.5 — 2026-04-28 — libca/RSRV deeper parity + kodex-driven review fixes

Continues the v0.10.4 line by closing the deeper libca/RSRV gaps surfaced
by kodex 0.9.0 analysis, then applying the actionable items from a
layer-by-layer code review across `pva-rs`, `ca-rs`, and `bridge-rs`.

### epics-ca-rs — libca/RSRV API parity

**Round 1 (commit `957d506`)**
- `SyncGroup` (`ca_sg_create`/`get`/`put`/`block` analog) — batch async
  ops with collective wait via `try_join_all`.
- Runtime address-list mutation: `CaClient::add_address(addr)` /
  `set_address_list(addrs)` (libca `addAddrToChannelAccessAddressList` /
  `configureChannelAccessAddressList`).
- `casr` iocsh command (RSRV `casr` analog) on `epics_ca_rs::server::iocsh`,
  reading from `Arc<ServerStats>` (connects/disconnects/uptime).
- `Channel::on_access_rights_change(cb)` callback wrapper.

**Round 2 (commit `007346e`)**
- `Channel::on_connection_change(cb)` — libca `ca_change_connection_event`
  analog. Filters Connected / Disconnected events from the broadcast.
- `Channel::host_name()` — libca `ca_host_name` analog returning the
  resolved server address.
- `Channel::receive_watchdog_delay() -> Duration` — libca
  `ca_receive_watchdog_delay`. New `CoordRequest::GetWatchdogDelay`
  variant; coordinator tracks per-server `last_rx_at` updated from every
  TransportEvent that implies an inbound frame.
- `CaClient::ioc_connection_count() -> usize` — libca
  `ca_get_ioc_connection_count`.
- Server-side ACF reload broadcast (RSRV `sendAllUpdateAS` analog):
  `CaServer.acf_reload_tx: broadcast::Sender<()>`; each accepted TCP
  client races read against reload notifications via `tokio::select!`,
  re-pushing `CA_PROTO_ACCESS_RIGHTS` for every open channel on signal.
  Both `reload_acf*()` and the introspection `POST /reload-acf` route
  fire it.

### Review-driven fixes

- **Dead code removal**: `epics-pva-rs/src/client_native/{ops.rs, conn.rs}`
  deleted (~750 LOC). The legacy one-shot `Connection`+`op_*` path was
  superseded by `ops_v2` (Channel-aware with auto-reconnect) months ago;
  only a stale doc comment in `channel.rs:19` referenced it.
- **bridge-rs group `field[N]` indexing semantic fix**: `qsrv::group::get_nested_field`
  changed return type from `Option<&PvField>` to `Option<Cow<PvField>>`.
  `field[N]` on a `ScalarArray` now returns the indexed element wrapped
  as `PvField::Scalar`; `field[N].child` on a `StructureArray` descends
  into the element and continues navigating. Previously both cases
  silently returned the whole array, breaking NTTable column[N] paths.
- **bridge-rs gateway PUT/GET channel reuse**: `UpstreamManager` now stores
  `UpstreamSubscription { channel: Arc<CaChannel>, task }` per upstream
  PV. Direct put/get reuse the subscribed channel instead of opening a
  fresh one — 3 round-trips → 1 RT per write/read.
- **CaChannel clone safety**: `CaChannel` was `Clone` but its `Drop` impl
  fired `CoordRequest::DropChannel` per drop, so cloning + dropping the
  clone tore down the original. Introduced a private `ChannelLifecycle`
  guard wrapped in `Arc`; `DropChannel` now fires exactly once when the
  last clone is dropped. Fixes a latent bug in `SyncGroup::get/put`
  where each scheduled future cloned the channel.
- **pvalink resolver sync fast path**: added `PvaLink::try_read_cached()`
  and `PvaLinkRegistry::try_get()` so the record-link `ExternalPvResolver`
  closure (and `LinkSet::get_value` / `is_connected`) hit a sync
  `parking_lot` cache without ever calling `block_on` when the monitor
  has already delivered. `block_on` is only paid on first-open / first-event.
- **pvalink scheme tightening**: `strip_scheme` no longer accepts `ca://`
  — pvalink handles PVA only; `ca://` belongs to the libca link scheme.

## v0.10.4 — 2026-04-28 — pvxs API parity (src + ioc + tools) and lset abstraction

A nine-commit pass closing every kodex-flagged gap relative to the
pvxs upstream. The `pva-rs` ↔ `pvxs/src` surface is now functionally
equivalent (modulo C++ STL idioms that don't translate to Rust);
`bridge-rs` ↔ `pvxs/ioc` covers QSRV + pvalink at iocsh + record-link
levels; new CLIs in `pva-rs/src/bin` mirror `pvxs/tools`.

### epics-pva-rs — pvxs API parity (3 rounds)

**Round 1 — lifecycle, multi-source, monitor handle, name servers**
- `PvaClient`: `close`, `hurry_up`, `cache_clear`,
  `ignore_server_guids`, per-call forced server (`pvget_from`,
  `pvput_to`), `name_servers` env wired (`EPICS_PVA_NAME_SERVERS`),
  multicast UDP join, `report` snapshot.
- `PvaServer`: `start` / `stop` / `wait` / `run` / `interrupt` (SIGINT
  trap), `client_config()`, `config()`, `report()`, `ignore_addrs`
  ACL, `monitor_*_watermark` diagnostics.
- `CompositeSource` multi-source registry with priority order.
- `pvmonitor_handle` returns `SubscriptionHandle` (pause/resume/
  stats/stop). `SubscriptionStat` metrics.
- Wire: `build_monitor_pause` / `build_monitor_resume` (subcmd
  0x04/0x44).

**Round 2 — SharedPV callbacks, builders, Discover**
- `SharedPV`: `on_first_connect`, `on_last_disconnect`, `on_put`,
  `on_rpc`, `attach`, `fetch`, `prune_subscribers`.
- `MonitorBuilder::server` → `pvmonitor_handle_from(pv, addr, cb)`.
- `ConnectBuilder`: `server()`, `sync_cancel(bool)`,
  `ConnectHandle::wait()`. `SubscriptionHandle::stop_sync` (pvxs
  syncCancel(true) analog).
- `PvaClientBuilder`: `priority(0..7)`, `tcp_timeout(Duration)`,
  `share_udp(bool)` (process-wide search engine via OnceCell),
  `MonitorEvent` / `MonitorEventMask` typed events.
- Discover: `Discovered::Online` carries `peer` + `proto`; beacon
  parser rewrites 0.0.0.0 → peer.ip(). `SearchEngine::ping_all()` /
  `PvaClient::ping_all()` (`DiscoverBuilder::pingAll`).
- `PvaServerConfig::isolated()` + `PvaServer::isolated()` factories.
- `PvRequestBuilder` (`field`/`record`/`pv_request`/`raw_request`/
  `build`) — pvxs RequestBuilder parity.

**Round 3 — TypeDef, Value coercion, ca-auth roles, log reload**
- `Value::clone_empty()` (pvxs `cloneEmpty` parity).
- `Value::copy_in` / `copy_out` / `try_copy_in` / `try_copy_out`
  (pvxs naming aliases).
- `TypeDef` + `Member` fluent builder for `FieldDesc` trees.
- `crate::log` module: `init_filter` (reload::Layer), `set_global_handle`,
  `set_log_filter(spec)`, `set_log_level(target, level)`. pvxs
  `logger_config_str` / `logger_level_set` parity.
- `auth::posix_groups()` POSIX `getgrouplist(2)` wrapper.
- ca-auth wire payload now advertises `groups: string[]`; server-side
  `ClientCredentials.roles` reads `groups`/`roles` either name.
- `ClientCredentials::peer_label(peer)` formatter.
- `PvaServerConfig::auth_complete` post-validation hook.
- `version_int()` const fn + `VERSION` const.

### epics-bridge-rs — QSRV + pvalink (3 rounds)

**Round 1 — iocsh integration**
- `dbLoadGroup` / `processGroups` / `qsrvStats` iocsh commands,
  bound to a shared `Arc<BridgeProvider>`. `BridgeProvider.groups`
  switched to `parking_lot::RwLock` for interior mutability.

**Round 2 — pvalink record-link wiring**
- `PvaLinkResolver` wraps the registry + tokio handle + read counter.
  `install_pvalink_resolver(db, handle)` registers both an
  `ExternalPvResolver` closure and (in round 3) the new
  `LinkSet`.
- `pvxr` / `pvxrefdiff` / `dbpvxr` iocsh commands.
- `wait_for_link_connected(pv, timeout)` (pvxs
  `testqsrvWaitForLinkConnected` analog).

**Round 3 — completeness pass**
- `resetGroups` iocsh + `BridgeProvider::reset_groups()`.
- `dbLoadGroup` macros arg now expands `${NAME}` against the
  `name=value,...` macros string with `std::env::var` fallback.
- `BridgeProvider::group_member` / `get_group_field` /
  `put_group_field` (pvxs `getGroupField`/`putGroupField`).
- `op_stats()` cumulative counters (channels created, GET, PUT,
  SUBSCRIBE) surfaced via `qsrvStats`.
- `PvaLinkConfig::scan_on_update`. `PvaLink::is_connected` /
  `alarm_message` / `time_stamp` / `latest_value` (pvxs lset
  helpers).
- `PvaLinkResolver::set_enabled` / `is_enabled`. `pvalink_enable`
  / `pvalink_disable` iocsh.

### epics-base-rs — LinkSet abstraction

- New `LinkSet` trait + `LinkSetRegistry` keyed on URL scheme.
- `PvDatabase::register_link_set(scheme, lset)` /
  `link_set(scheme)` / `registered_link_schemes()`.
- `resolve_external_pv` dispatches through the lset registry first,
  falls back to the legacy `ExternalPvResolver`. Backward-compat.
- `PvDatabase::record_link_fields(name)` enumerates link-shaped
  String fields by parsing each value through `parse_link_v2` —
  underpins per-record `dbpvxr`.
- `dbpvxr <record>` is now a real per-record dump: connected /
  value / alarm / timeStamp for each `pva://` link, plus single-line
  descriptions of `ca://` / db / constant links.

### epics-pva-rs/src/bin — pvxs/tools parity

- `pvcall-rs`: RPC client. `field=value` → NTURI request → `pvrpc`.
- `pvlist-rs`: server discovery via `SearchEngine::discover` +
  optional `ping_all`. Verbose mode adds GUID / proto / peer.
- `pvxvct-rs`: PVA Virtual Cable Tester. Decodes SEARCH / BEACON
  frames. `-C` / `-S` direction filter, `-H` host filter.
- `mshim-rs`: beacon / search multicast shim. `-L listen` /
  `-F forward` endpoints, auto multicast join, same-peer feedback
  guard. Bug found via `testudpfwd` integration test: send_sock
  needs `set_nonblocking(true)` for tokio::UdpSocket::from_std.

### Tests — pvxs test/* parity (final round)

- 9 `testTypeDef` parity unit tests (TypeDef + Member builder).
- 6 `testqsingle` parity tests (BridgeChannel get/put e2e: ai /
  longin / stringin / waveform).
- 4 `testqgroup` parity tests (atomic + non-atomic group put-then-get,
  config parse + finalize).
- 2 `testudpfwd` integration tests (mshim-rs binary forwarding,
  invalid-endpoint exit code).
- 7 inline `parse_endpoint` unit tests in mshim-rs.

### Workspace bump

- `0.10.3` → `0.10.4` across workspace + dependency pins.

## v0.10.3 — 2026-04-28 — pva-rs reconnect machinery brought up to pvxs parity

A focused pass on `epics-pva-rs` client-side reconnect: the v0.10.2
review identified the gaps relative to pvxs (`pva-rs reconnect gaps vs
pvxs` in kodex). All five items closed in this release. The protocol
behaviour now mirrors pvxs `client.cpp` and `clientconn.cpp` — same
constants, same trigger conditions — without the libca-only extras
(penalty box, circuit breaker, multi-lane retry).

### Cooperative search-bucket ring

- Replaces the per-channel `BACKOFF_SECS` exponential schedule with a
  30-bucket ring rotated at 1 s. Each tick processes exactly one
  bucket so steady-state UDP search load is `O(pending / 30)` packets
  per second instead of `O(pending)`. Mirrors pvxs `client.cpp`
  `searchBuckets[nBuckets=30]`.
- New searches land in `(current + 1) % 30`; first retry rotates
  back to the same slot 30 ticks later; subsequent retries shift by
  an extra `RETRY_HOLDOFF_BUCKETS = 10` (matches pvxs
  `Channel::disconnect` holdoff at client.cpp:155-163).

### `poke()` — fast-tick mode on fresh server identity

- When a beacon delivers a new (server, GUID) pair (server restart or
  brand-new server), the search engine flips its tick interval to
  200 ms for one full revolution (≈ 6 s) so every pending search
  retries quickly. Reverts to 1 s afterwards. Mirrors pvxs
  `ContextImpl::poke()` (client.cpp:713) with the 30 s pokeHoldoff
  enforced by the same `first_announce` gate that drives the
  `Discovered` events.
- Periodic same-GUID beacons no longer pull pending searches'
  `last_attempt` forward (closes a regression: every 15 s beacon
  effectively reset every backoff regardless of whether anything
  changed).

### Beacon-timeout pruning + `Discovered::Timeout`

- `BeaconTracker::prune_stale(max_age)` walks the throttle map and
  evicts entries whose `last_seen` is older than `max_age`,
  returning the (server, guid) tuples that were dropped.
- The search engine now runs a `BEACON_CLEAN_INTERVAL = 180 s` tick
  that calls `prune_stale(BEACON_TIMEOUT = 360 s)` and emits
  `Discovered::Timeout` for each pruned server. Application
  observers subscribed via `SearchEngine::discover()` see online /
  timeout transitions both ways. Mirrors pvxs `tickBeaconClean`
  (client.cpp:1254).
- New `Discovered::Timeout { server, guid }` variant on the public
  enum.

### Connect-fail holdoff per channel

- `Channel` gained `holdoff_until: Option<Instant>` and
  `connect_fail_count: AtomicU32`. `ensure_active` now sleeps until
  `holdoff_until` before issuing a fresh search; on every full
  candidate-list failure the counter increments and the holdoff is
  set to `2^min(fails-1, 4)` seconds (1 s → 16 s cap). On the next
  successful Active transition both fields reset. Mirrors pvxs
  `Channel::disconnect` 10-bucket future-push, generalised so
  callers that retry tightly (e.g. monitor reconnect loops) don't
  spin against a dead server.

### Tests

- `beacon_throttle::tests::prune_stale_returns_aged_out_entries`
  covers the new prune API.
- All existing 128 lib tests + 118 parity tests + 4 stability tests
  pass against the new tick / bucket logic.

### What was deliberately NOT added

Per the v0.10.2 kodex `tech_debt` entry: pvxs deliberately omits the
libca penalty box, circuit breaker, and multi-lane retry buckets. We
match that decision — those features remain CA-only (in `epics-ca-rs`)
and would re-introduce complexity that pvxs ships without for a
reason. If a real flapping-server incident in production calls for
them later, the data point goes through CA-rs first.

## v0.10.2 — 2026-04-28 — kodex-driven cross-crate review pass

A self-review using the kodex knowledge graph as a baseline (so we
didn't re-examine the v0.10.1 wire-format fixes) plus three parallel
agent reviews surfaced a set of polish + correctness items that
weren't worth blocking v0.10.1 on but accumulate now into a clean
release.

### ca-rs (client TLS)

- `CaClientConfig::tls_server_name: Option<String>` (env
  `EPICS_CA_TLS_SERVER_NAME`) overrides the SNI / cert-hostname-
  verification name when wrapping a TCP virtual circuit in rustls.
  Without it, SNI fell back to the server's IP literal — which only
  validates against IP-bound certs. The override unblocks
  hostname-bound rustls cert verification for hostname-bound deployments.

### ca-rs (server cap-token verification)

- `CaServerBuilder::with_cap_token_verifier(verifier)` (feature
  `cap-tokens`) installs a `TokenVerifier` on the listener. CLIENT_NAME
  payloads beginning with `cap:` now flow through the verifier; the
  resolved `sub` claim becomes the ACF username. Verification failure
  yields an `unverified:<raw>` sentinel that ACF can deliberately deny.
  Plain (non-`cap:`) usernames pass through unchanged for legacy compat.
  Earlier code stored the raw payload as the username regardless of
  prefix — closing this loophole was the original intent of cap-tokens
  but the wiring was missing.

### ca-rs (signed beacon mixed mode)

- `EPICS_CA_BEACON_REQUIRE_SIGNED=NO` opts the verifier into a soft
  mode where unsigned beacons are accepted (with a counter increment)
  alongside signed ones. Lets operators run mixed deployments while
  servers roll out signing instead of forcing a flag day. Default
  remains strict.

### pva-rs (server identity + beacons)

- `ClientCredentials` parsed from CONNECTION_VALIDATION reply (method,
  account, host) and logged at handshake. Mirrors pvxs serverconn.cpp
  `server::ClientCredentials` at the wire-parse level. Available for
  future per-op authorisation hooks; today's use is `tracing` audit.
- Beacon `change_count` (u16) now increments whenever the source's
  `list_pvs()` set churns between ticks (compared via stable hash of
  the sorted name list). Sequence (u8) was already incrementing;
  together they let clients re-issue searches on PV-set churn even
  when the beacon stream is otherwise in lock-step (pvxs
  `server.cpp::doBeacons`).

### bridge-rs (live ACF reload)

- `BridgeProvider` now stores access policy in
  `Arc<parking_lot::RwLock<Arc<dyn AccessControl>>>` and vends an
  `Arc<LiveAccessProxy>` to each `AccessContext`. `set_access_control`
  swaps the inner Arc and is picked up by every existing channel on
  its next can_read / can_write call — matches C++ QSRV "ACF reload
  takes effect without recreating channels". The earlier direct-clone
  pattern pinned each channel to the policy at creation time.
- `BridgeProvider::live_access()` is a public helper for downstream
  code that constructs its own AccessContexts.

### bridge-rs (rename + lifecycle test)

- `qsrv::spvirit_adapter` → `qsrv::pva_adapter` (the file's own header
  comment already noted "no spvirit_* types appear in this module" —
  the name was the last remaining `spvirit_*` artifact in the
  workspace).
- `qsrv::monitor::tests::monitor_stop_releases_subscription` —
  start → poll → stop → idempotent stop → re-subscribe round-trip
  against a fresh BridgeMonitor on the same record. Locks in Drop
  semantics so a future refactor can't silently leak DbSubscription
  senders.

### Tests

- `qsrv::provider::tests::live_access_proxy_observes_policy_swap` —
  AccessContext bound to `live_access()` observes
  `set_access_control` mid-flight (regression for the cached-Arc
  pattern this release replaces).
- `server_native::udp::tests::beacon_payload_carries_sequence_and_change_count`
  — beacon byte layout regression: sequence at offset 13, change_count
  little-endian at offsets 14-15.
- `client::tls_sni_config_tests::tls_server_name_round_trip` —
  `CaClientConfig::tls_server_name` default + assignment.

### Build / publish hygiene

- `epics-base-rs` dropped the `experimental-rust-tls` feature
  passthrough into a dev-dep. `cargo publish` strips dev-deps before
  parsing features, so the passthrough broke `cargo workspaces
  publish`. Nothing outside the crate referenced it.

## v0.10.1 — 2026-04-28 — pvxs / pvAccessCPP wire-format interop

`epics-pva-rs` server and client are now byte-exact compatible with
the upstream EPICS C++ implementations (`pvxs` 1.x and `pvAccessCPP`
shipping with EPICS Base 7.x). The push came from a real-world
deployment where Base's `pvmonitor` either disconnected immediately
or printed garbled values against our server. A walk through pvxs
`servermon.cpp` / `serverget.cpp` / `serverchan.cpp` /
`clientget.cpp` exposed five separate wire-format mismatches. All
are fixed; e2e and unit tests cover each one.

### Wire-format fixes (server)

- **MONITOR data field order** — the payload is now `changed bitset →
  partial value → overrun bitset`, matching pvxs `servermon.cpp:173-
  175`. The previous order (`changed → overrun → value`) shifted the
  client's value-decode cursor by one byte whenever overrun was
  empty, corrupting timestamps and double values for every Base
  client.
- **MONITOR FINISH** — when the source's broadcast channel is
  dropped the subscriber task now emits `subcmd 0x10 + Status::OK`
  before exiting (pvxs `servermon.cpp:148`). Clients receive a
  graceful end-of-stream instead of waiting for a TCP timeout.
- **INIT type-descriptor encoding** — RPC INIT no longer emits the
  type descriptor (pvxs `serverget.cpp:97` —
  `if (cmd != CMD_RPC) to_wire(R, type)`). For
  GET/PUT/MONITOR it defaults to inline; the
  `0xFD` / `0xFE` cache markers are now opt-in via
  `PvaServerConfig::emit_type_cache` because pvAccessCPP doesn't
  parse them and reads past the payload boundary, breaking the next
  frame.
- **PUT_GET (`subcmd & 0x40`)** — the response now carries
  `bitset + partial value` after the status (pvxs
  `serverget.cpp:103-104`). Previously only `Status::OK` was sent and
  the client got no readback.
- **CreateChannel access_rights** — drop the unnecessary `u16`
  trailing the status. pvxs `serverchan.cpp:349-351` emits
  `cid + sid + status` only.
- **RPC DATA request** — server now decodes `type(arg) +
  full_value(arg)` instead of treating the channel introspection
  from INIT as the argument shape (pvxs `serverget.cpp:444-446`,
  `from_wire_type_value`).
- **MESSAGE / CancelRequest** — now dispatched. MESSAGE (cmd 18) is
  surfaced through `tracing` at the matching severity. CancelRequest
  (cmd 21) aborts the in-flight monitor task and resets
  `monitor_started` so a re-START respawns cleanly. Both previously
  fell through to a no-op catchall.
- **`request_to_mask`** — an empty pvRequest with no `field`
  substructure now selects every field (pvxs convention) instead of
  "root only". This was silently dropping every leaf for the
  canonical no-filter sentinel `[0xFD,0x02,0x00,0x80,0x00,0x00]` the
  Rust client sends by default.

### Wire-format fixes (client)

- **RPC INIT response** — the type descriptor is no longer expected
  (mirrors the server fix above).
- **RPC DATA response** — decoded as `status + type + full_value`
  (pvxs `clientget.cpp:415-421`). The previous bitset-driven path
  could not parse RPC replies at all.
- **RPC DATA request** — sends `type(arg) + full_value(arg)`. v1
  (`ops.rs`) and v2 (`ops_v2.rs`) paths both updated.
- **Monitor data overrun bitset** — the trailing `BitSet` is now
  consumed.
- **`OpResponse::Status` from `subcmd & 0x10`** — the FINISH frame
  is routed to the status path so `pvmonitor` returns `Ok(())`
  instead of hanging.
- **`OpDataResponse.response_desc: Option<FieldDesc>`** — new field
  carrying the server-side response type for RPC, so callers can
  reconstruct the result without relying on the now-empty INIT
  introspection.

### pvData decode tightening

- **Structure-array presence byte** — strict `0x00` (null) / `0x01`
  (present) per pvxs `dataencode.cpp:359-361`. The previous code
  also accepted `0xFF` and silently rewound the cursor on unknown
  markers; both branches were defensive guesses with no pvxs
  counterpart and could mask real protocol errors.
- **Union / UnionArray selector** — the manual peek-and-pushback was
  replaced by a direct `decode_size` match. `decode_size` already
  returns `None` for the 0xFF null marker, so the rewind dance was
  redundant and made the future "selector ≥ 254" extended-Size case
  awkward to handle.

### Server lifecycle / resource management

- **Spawned MONITOR subscribers are now cancelled deterministically**
  on DestroyRequest, DestroyChannel, CancelRequest, and connection
  end. The abort handle is wrapped in `Arc<AbortOnDrop>` and stashed
  on `OpState`; dropping the OpState (via HashMap removal or
  HashMap drop on connection teardown) fires the abort
  automatically. Previously, orphaned monitor tasks ran until their
  next write tripped on a closed socket — keeping the source's
  broadcast subscription alive in the meantime.
- **Per-connection write queue** — replaced
  `Arc<Mutex<SrvWrite>>` with a bounded `mpsc::channel` plus a
  single dedicated writer task. Producers (main read loop,
  heartbeat, monitor subscribers) `tx.send(buf).await` instead of
  serialising on the writer Mutex across `write_all().await`.
  A slow client now backpressures monitor delivery rather than
  blocking the heartbeat or other channels' writes.
  Configurable via `PvaServerConfig::write_queue_depth` (default
  1024). Writer-task I/O failures are logged at `debug!` with
  peer info before the connection tears down.

### pvRequest field filtering

- The pvRequest sent at INIT time is now translated through
  `request_to_mask` and stored on the OpState. GET and MONITOR
  emission consult the mask via `encode_pv_field_with_bitset`, so
  the server only ships the fields the client asked for. Previously
  the request was decoded and discarded; the wire always carried
  every field.

### Tests

- `parity/test_pvrequest_filter.rs` — e2e: empty pvRequest returns
  every field; `pvget_fields(["value"])` omits alarm/timeStamp on the
  wire.
- `parity/test_monitor_finish.rs` — e2e: dropping the source's
  broadcast sender mid-stream causes the client `pvmonitor` to
  return `Ok(())` via the FINISH frame.
- `server_native::tcp::tests` — synthetic-frame unit coverage for
  `handle_message` (every severity, truncated payload guard) and
  `handle_cancel_request` (abort guard fires, `monitor_started`
  resets).
- `server_native::tcp::tests::monitor_payload_orders_overrun_after_value`
  — round-trip regression for the corrected MONITOR layout.

### Configuration additions

- `PvaServerConfig::emit_type_cache: bool` (default `false`) — opt
  in to `0xFD`/`0xFE` type-cache markers in INIT and RPC responses.
- `PvaServerConfig::write_queue_depth: usize` (default `1024`) —
  bounded write queue capacity per connection.

## v0.9.4 — 2026-04-16

### Async / reliable plugin data path

- **`asyn-rs`, `ad-core-rs`** — plugin pipeline on a fully async data
  path with bounded backpressure for parameter updates and array
  propagation.
- **`ad-core-rs`** — driver-facing async runtime facade (`rt::spawn`,
  `rt::timeout`, `rt::CommandReceiver`, …) so drivers no longer depend
  on `tokio` directly. All example acquisition tasks migrated.

### Scan scheduler

- Dedupe entries on registration — a record can no longer be scanned
  twice after rate changes.
- Preserve PINI → init-hook ordering across the dual schedulers.

### mqtt-rs

- Connected PV no longer latches at 0 after a recoverable `rumqttc`
  state error. Connected=1 is now also restored on any inbound
  `Publish` or `PingResp`, not just `ConnAck`.
- `mqtt-ioc` installs `tracing_subscriber` (EnvFilter, default `info`,
  `RUST_LOG`-controlled) so MQTT connection errors and reconnects
  reach stdout.

## v0.9.3 — 2026-04-15 — First production-ready pvAccess support

`epics-rs` now ships a full pvAccess (PVA) stack — client, server,
and QSRV-equivalent bridge — powered by
[spvirit](https://github.com/ISISNeutronMuon/spvirit). PVA was
introduced experimentally in v0.9.2; v0.9.3 is the release where it
leaves experimental status and becomes a first-class peer to Channel
Access across the entire workspace.

### What spvirit provides

[spvirit](https://github.com/ISISNeutronMuon/spvirit) is a pure-Rust
implementation of the pvAccess wire protocol maintained by the ISIS
Neutron & Muon Source. `epics-pva-rs` wraps `spvirit-server` /
`spvirit-client` / `spvirit-codec` / `spvirit-types` (v0.1.9 from
crates.io) and exposes:

- **Client** — `search`, `get`, `put`, `monitor`, `info` over UDP
  discovery (port 5076) + TCP virtual circuits (port 5075)
- **Server** — `PvaServer` that hosts a `PvDatabase` and answers the
  full pvAccess command set
- **NormativeTypes** — NTScalar, NTEnum, NTScalarArray, NTNDArray,
  NTTable
- **BitSet-delta monitors**, segmentation, `SET_BYTE_ORDER`
  handshake, and connection validation

### epics-bridge-rs — QSRV-equivalent

Pure-Rust analogue of the C++ QSRV (`modules/pva2pva/pdbApp/`):
translates `epics-base-rs` record state into pvAccess `PvStructure`
values and vice versa.

- **Single-record channels** — NTScalar, NTEnum (with choices),
  NTScalarArray with full `alarm / timeStamp / display / control /
  valueAlarm` metadata
- **Group PV channels** — composite structures defined via
  `info(Q:group, …)` JSON tags on records (C++ QSRV JSON format
  compatible)
- **Monitor bridge** — initial Snapshot on connect, full Snapshot on
  every update, fan-in group monitor with trigger rules
- **pvRequest** — field selection, `record._options.process` / `block`
- **Pluggable access control** — ChannelProvider / Channel /
  PvaMonitor traits, record metadata cache

### Dual-protocol across every example

All seven remaining example IOCs now serve CA **and** PVA
simultaneously from the same `PvDatabase` via
`epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc`:

| Example           | Protocols                   |
|-------------------|-----------------------------|
| `mini-beamline`   | CA + PVA                    |
| `xrt-beamline`    | CA + PVA                    |
| `qsrv-ioc`        | CA + PVA                    |
| `sim-detector`    | CA + PVA                    |
| `ophyd-test-ioc`  | CA + PVA *(new in 0.9.3)*   |
| `scope-ioc`       | CA + PVA *(new in 0.9.3)*   |
| `mqtt-ioc`        | CA + PVA *(new in 0.9.3)*   |

The programmatic `random-signals` demo was removed in favour of a
uniform st.cmd-driven example set.

### PVA CLI tools

Shipped alongside the CA tools (`caget-rs`, `caput-rs`,
`camonitor-rs`, `cainfo-rs`, `ca-repeater-rs`):

- `pvget-rs` — read
- `pvput-rs` — write
- `pvmonitor-rs` — subscribe
- `pvinfo-rs` — type / introspection info

### Documentation

- "Experimental" status removed from `epics-pva-rs`,
  `epics-bridge-rs`, and the pvAccess CLI tool section in the
  top-level and per-crate READMEs.
- `epics-pva-rs` README refreshed — stale "server-side is planned"
  notes replaced with a working `PvaServer` +
  `run_ca_pva_qsrv_ioc` example.

### Acknowledgements

Huge thanks to the [spvirit](https://github.com/ISISNeutronMuon/spvirit)
maintainers at ISIS Neutron & Muon Source for the pvAccess wire-protocol
implementation that makes this release possible.

## v0.9.2 — 2026-04-16

### pvAccess / QSRV

- **pvAccess protocol support** — full client & server via [spvirit](https://crates.io/crates/spvirit-server) integration
- **QSRV bridge** — map EPICS records to PVA NormativeTypes (NTScalar, NTEnum, NTNDArray) via `info(Q:group)` JSON configuration
- **NDPluginPva** — serve AreaDetector NDArray as NTNDArray over pvAccess, compatible with C++ `pvget -m`
- **Dual-protocol CA+PVA runner** — `run_ca_pva_qsrv_ioc()` for all example IOCs
- **PVA CLI tools** — `pvget-rs`, `pvmonitor-rs`, `pvput-rs`, `pvinfo-rs` (renamed from `pvaget-rs` etc.)
- **spvirit 0.1.9** from crates.io (removed `[patch.crates-io]` path overrides)

### xrt-beamline example

- **Real-time ray tracing simulation** — Undulator → DCM Si(111) → HFM → VFM → Sample at 8 keV
- 25 motors driving [xrt-rs](https://github.com/physwkim/xrt-rs) ray tracing with AreaDetector output
- Accumulation over `AcquireTime` for improved statistics
- PyDM viewer with contrast control, xrtGlow 3D viewer with pyepics PV monitoring
- Coddington-calculated mirror radii (HFM R=3.27 km, VFM R=1.82 km)

### xrt-rs fixes (companion repo)

- **position_roll**: implement as roll addition matching xrt Python behavior
- **bracketing**: increase t_min clamp from -1e-6 to -100 mm for large pitch angles (DCM at 14°)
- **reflect()**: use `state==1` filter to prevent Over ray reprocessing

### Other

- Upgrade spvirit dependencies 0.1.8 → 0.1.9
- Fix clippy warnings across workspace

## v0.9.1 — 2026-04-13

### motor-rs

- **Fix RBV monitor updates during motion**: `process()` was returning
  `AsyncPendingNotify` on every poll cycle with only DMOV/VAL/DVAL/RVAL
  fields — RBV and DRBV were missing. Now uses `AsyncPendingNotify` only
  for the initial DMOV 1→0 transition; subsequent polls return `Complete`
  which posts monitors for all changed fields including RBV.
- **Fix missing DMOV monitor on back-to-back motions**: When a new put
  arrives while the previous motion's done status is consumed in the same
  process cycle, `dmov_notified` was not reset. Fixed by resetting the
  flag in `plan_motion()`.
- **Fix same-direction NTM retarget**: `ExtendMove` accepted the new
  DVAL but never re-dispatched a `MoveAbsolute` to the driver. On
  completion, `evaluate_position_error()` only retried under retry
  conditions (RTRY>0, RDBD>0). Now sets `verify_retarget_on_completion`
  so the completion path replans if DVAL ≠ DRBV regardless of retry
  settings.

### epics-ca-rs

- **CA repeater**: Rewrite to use per-client connected UDP sockets
  matching C EPICS architecture. Fixes compatibility with C CA clients
  (camonitor, caget) that could not register with the Rust repeater.
- **Pre-connection subscription**: `subscribe()` now registers
  subscriptions even when disconnected. On connect, the coordinator
  fills in native type and element count and issues `CA_PROTO_EVENT_ADD`.
  Eliminates the need for application-level resubscribe loops.
- **Add `get_with_timeout()`** for explicit timeout control on reads.
- **Monitor flow control**: Client-side backlog tracking replaces TCP
  read count heuristic. Server-side `FlowControlGate` with
  `coalesce_while_paused()` matching C EPICS `dbEvent.c` behavior.
- **Add `ioc` feature** to umbrella crate for IOC builds.
- **Fix proc macro path resolution**: `epics_main`/`epics_test` now
  resolve `epics_base_rs` path for umbrella crate users via
  `proc-macro-crate`.

### CA tools (C parity)

- **camonitor-rs**: Use server timestamp, print disconnect to stdout
  as `*** disconnected`, add `-w` initial connection timeout. Subscribe
  once and rely on library auto-restore (no resubscribe loop).
- **caput-rs**: Re-read value from server for `New` line. Apply `-w`
  timeout to all reads. Fix `-c` description.
- **caget-rs**: Parallel PV connect+read via `tokio::spawn`. Add `-w`
  timeout. Distinguish "Not connected" from "timeout" errors.
- **cainfo-rs**: Add `-w` timeout, use explicit channel connect.
- All tools: Rename help text from `rcaXXX` to `caXXX`.

## v0.9.0

### motor-rs — Complete C parity (~95 fixes across 12 review rounds)

#### State machine
- Fix MSTA bit positions for wire compatibility with C clients
- Fix all 4 retry modes (Default/Arithmetic/Geometric/InPosition)
- Fix SPMG Pause/Stop/Go transitions to match C postProcess pipeline
- Add MIP_EXTERNAL detection for externally-initiated motion
- Add clear_buttons on limit switch hit or PROBLEM
- Add stop-first pattern for home-while-moving and jog-while-moving
- Add DLY → DELAY_ACK → fresh poll → retry evaluation flow
- Add limit switch direction guard before retries (user_cdir)
- Implement two-phase jog backlash (BL1 slew + BL2 backlash velocity)
- Add sub-step deadband check with DMOV pulse for ophyd compatibility

#### Coordinate system
- Fix CDIR to account for MRES sign
- Fix DIR handler FOFF branching (Variable preserves VAL)
- Fix SET+FOFF=Frozen cascade for VAL/DVAL/RVAL
- Fix FOFF=Frozen in non-SET mode (no effect, matches C)
- Fix RDIF type (i32) and formula (NINT(diff/mres))
- Fix LVIO escape logic using ldvl, pretarget only for non-preferred direction
- Fix soft limit disable only when dhlm==dllm==0
- Add RHLM/RLLM fields for MRES cascade invariance

#### New features
- Add MoveRelative command and use_rel logic (ueip/urip)
- Add FRAC progressive approach scaling
- Add dual poll rate (moving/idle intervals, forced fast polls)
- Add auto power on/off with configurable delays
- Add deferred moves and profile moves framework
- Add RDBL/URIP readback link support
- Add velocity cross-calculation and range validation

#### Driver interface
- Expand MotorStatus with direction, slip_stall, comms_error, homed, gain_support, has_encoder, velocity
- Add move_velocity, move_relative, set_deferred_moves trait methods
- Add profile move trait methods (initialize, define, build, execute, abort, readback)
- Fix SetPosition to send dial coordinates (not raw steps)
- Fix MOVN ls_active to use raw limit switches before user mapping

### asyn-rs
- Fix race condition in PortManager register/unregister
- Fix COMM_ALARM constant, HTTP connect-per-transaction
- Fix write retry timeout, HTTP write reconnect, EOS storage
- Fix param defined tracking, IP port auto-disconnect
- Fix trace masks, serial flush, baud rates, break/ixany
- Fix asyn_record connect_device clearing drv_user_create error
- Add PortHandle convenience methods for new operations
- Add `set_params_and_notify` for atomic background thread parameter updates
- Add ParamSetValue::Float64Array for waveform parameter updates
- Add AsynMotor::move_relative, set_deferred_moves, profile move methods
- Move set_rs485_option out of PortDriver trait impl
- Document `set_params_and_notify` vs `write_int32_no_wait` for driver authors

### epics-base-rs
- Fix ai/ao conversion pipeline (ASLO/AOFF/ESLO/EOFF)
- Fix bi/bo records and COS alarm
- Fix calc division by zero to return NaN
- Fix mbbi/mbbo state handling and field access
- Fix sel record High/Low/Median algorithms
- Fix calcout missing OUT link write (pval timing + cached should_output)
- Fix WriteDbLink to use resolve_field for common fields (OUT/DOL)
- Fix monitor deadband for binary records (bi/bo/busy/mbbi/mbbo always post)
- Document DeviceReadOutcome ok() vs computed() convention

### ad-core-rs
- Fix ADDriverBase MaxSizeX/Y init from constructor args
- Fix NDArrayPool threshold and free-list logic
- Fix plugin runtime interrupt notifications
- Add ParamUpdate::Float64Array for waveform param updates in plugins

### ad-plugins-rs
- Fix ROIStat time series waveform readback (was accumulating but never writing to params)
- Fix ROI, Stats, Process, HDF5, TIFF, JPEG, NetCDF, Nexus plugins
- Add attr_plot param indices and buffer output infrastructure

### examples
- Migrate all acquisition tasks to set_params_and_notify
- Fix beam_current and time_of_day DeviceReadOutcome to skip ai conversion
- Fix moving_dot acquire_busy and status in writeInt32

## v0.8.3

### asyn-rs

- Remove unbounded sync channel from `InterruptManager`, replacing it with a simpler notification mechanism to eliminate memory leaks when interrupt callbacks accumulate faster than consumed.

### motor-rs

- Fix tight poll loop consuming excessive CPU when motor is in motion.
- Defer `StartPolling` to `after_init` hook to prevent premature polling during st.cmd and autosave restore.
- Throttle `StartPolling` and send only on idle-to-active transition, removing redundant poll requests.
- Clear `last_write` in init to prevent restore-triggered moves.
- Sync driver position from pass0-restored VAL during initialization.

### epics-base-rs

- Add `after_init` hooks that run after PINI processing, matching C EPICS `initHookAfterIocRun` timing.

### epics-ca-rs

#### Client

- **Fix**: Slow reconnection after IOC restart (~50s → ~5s). Beacon monitor was skipping `available=INADDR_ANY` beacons (all modern IOCs), reading the wrong header field for server port, and doing per-server rescan instead of global rescan.
- **Fix**: ECHO ping-pong loop causing 50%+ CPU usage. Client was echoing back the server's echo responses, creating a tight infinite loop after the first 30-second idle timeout.
- **Fix**: Search response `INADDR_ANY` check (`0xFFFFFFFF` → `0`) for C server interoperability.
- **Fix**: `handle_disconnect` operator precedence bug causing channels on unrelated servers to be incorrectly disconnected.
- **Fix**: Pending read/write waiters now receive `CaError::Disconnected` on server disconnect instead of hanging forever.
- **Fix**: `DropChannel` now properly cleans up all channel states (Connecting, Disconnected, Unresponsive).
- Beacon-TCP watchdog integration: immediate echo probe on beacon anomaly detects dead connections in ~5s instead of ~35s.
- Send buffer backpressure: close stalled connections at 4096 pending frames.
- Search datagram sequence validation to reject stale responses from previous rounds.
- TCP read buffer capped at 1MB to protect against malformed servers.
- Defensive bounds checks and malformed message logging.
- `align8` overflow protection with `saturating_add`.

#### Server

- **Fix**: Beacon header field swap (`data_type`/`count` were swapped), breaking C client interop.
- **Fix**: Search response `INADDR_ANY` sentinel (`0xFFFFFFFF` → `0`), matching C protocol.
- **Fix**: `WRITE_NOTIFY` response `count` field was hardcoded to 1 instead of echoing the request count.
- **Fix**: `CLEAR_CHANNEL` response was missing `data_type` and `count` fields.

#### Repeater

- **Fix**: Accept zero-length UDP registration for C client backward compatibility (pre-3.12 protocol).
- **Fix**: Fill in beacon `available` field with source IP on relay, matching C repeater behavior.

### optics-rs

- Add HSC and QXBPM async driver support with deferred poll start.

## v0.8.2

### epics-bridge-rs (new crate)

New umbrella crate for EPICS protocol bridges. Hosts feature-gated sub-modules:

- **`qsrv`** (default) — Record ↔ pvAccess channels (C++ EPICS QSRV equivalent). Single PVs (NTScalar/NTEnum/NTScalarArray) and multi-record group PVs with full metadata, pvRequest filtering, process/block put options, AccessControl enforcement on get/put/monitor, nested field paths, info(Q:group, ...) parsing, and trigger validation.
- **`ca-gateway`** (default) — CA fan-out gateway (C++ ca-gateway equivalent). Includes `.pvlist` parser with regex backreferences, ACF integration, lazy on-demand resolution via search hook, per-host connection tracking, statistics PVs, beacon throttle, putlog, runtime command interface, and an auto-restart supervisor.
- **`pvalink`**, **`pva-gateway`** — placeholders for future implementations.

The `ca-gateway-rs` daemon binary builds via `cargo build --release -p epics-bridge-rs --bin ca-gateway-rs` and lands in `target/release/ca-gateway-rs`.

The umbrella `epics-rs` crate gains a `bridge` feature that re-exports `epics-bridge-rs` as `epics_rs::bridge`.

### epics-base-rs

#### **Behavior change**: `PvDatabase::has_name()` / `find_entry()` now invoke an optional async search resolver on miss

`PvDatabase` gained `set_search_resolver(SearchResolver)` / `clear_search_resolver()` plus a new `SearchResolver` type alias. When set, both `has_name()` and `find_entry()` invoke the resolver on a database miss; the resolver may populate the database (e.g. by subscribing to an upstream IOC) and return `true` to make the lookup succeed on the immediate re-check.

**Compatibility**: with no resolver installed (the default), behavior is unchanged. However, callers that previously assumed `has_name()`/`find_entry()` were *cheap, side-effect-free* lookups should be aware these methods can now `.await` arbitrary work when a resolver is registered. The current in-tree usage (CA UDP search responder, TCP create-channel handler) is consistent with this design.

This hook is what enables `epics-bridge-rs::ca_gateway` to lazily subscribe upstream PVs on first downstream search instead of requiring a `--preload` file.

#### `Snapshot` / `DisplayInfo` — additive fields

- `DisplayInfo` gained `form: i16` (display format hint, from `Q:form` info tag) and `description: String` (DESC). Existing initializers need `..Default::default()` to remain forward-compatible — internal call sites have been updated.
- `Snapshot` gained `user_tag: i32` (from `Q:time:tag` nsec LSB splitting). Defaults to 0.

These fields propagate into PVA NTScalar `display.form` / `display.description` and `timeStamp.userTag` via `epics-bridge-rs::qsrv::pvif`.

### epics-ca-rs

#### **Breaking**: `tcp::run_tcp_listener()` signature changed

Added a 6th parameter:

```rust
pub async fn run_tcp_listener(
    db: Arc<PvDatabase>,
    port: u16,
    acf: Arc<Option<AccessSecurityConfig>>,
    tcp_port_tx: tokio::sync::oneshot::Sender<u16>,
    beacon_reset: Arc<tokio::sync::Notify>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>, // ← new
) -> CaResult<()>;
```

External callers of `run_tcp_listener()` must pass `None` (opt out of connection lifecycle events) or a `broadcast::Sender` to subscribe.

In-workspace consumers (`server::ca_server::CaServer::run` and `crates/epics-base-rs/tests/client_server.rs`) have been updated.

#### Additive: `CaServer::connection_events()` and `ServerConnectionEvent`

`CaServer` now exposes `connection_events()` which returns a `broadcast::Receiver<ServerConnectionEvent>` (`Connected(SocketAddr)` / `Disconnected(SocketAddr)`). Used by `epics-bridge-rs::ca_gateway` for per-host downstream client tracking. Servers that don't subscribe see no behavior change.

## v0.8.1

### Fix: Plugin param update re-entrancy (CPU 100% on idle)

Plugin `on_param_change` handlers that return `ParamUpdate` values (readback pushes)
previously used `write_int32_no_wait` which sends `Int32Write` to the port actor.
The port actor then calls `io_write_int32` → `on_param_change` again, causing
**infinite re-entrancy loops** (e.g., Overlay Position↔Center bidirectional update).

This is now fixed by introducing `ParamSetValue` and `set_params_and_notify()`,
which mirrors C ADCore's `setIntegerParam()` + `callParamCallbacks()` pattern:
values are stored directly in the param store without going through the driver's
write path, so `on_param_change` is never re-triggered.

- **asyn-rs**: Add `ParamSetValue` enum, extend `CallParamCallbacks` with inline param updates, add `PortHandle::set_params_and_notify()`
- **ad-core-rs**: `publish_result` now uses `set_params_and_notify` instead of `write_int32_no_wait` for plugin readback values
- **ad-plugins-rs**: Restore Overlay Position↔Center bidirectional readback (safe with new path)
- **commonPlugins.cmd**: Add missing `NDTimeSeriesConfigure` commands for Stats/ROIStat/Attr TS ports

## v0.8.0

### HDF5 Plugin — Complete Rewrite
- **Pure Rust HDF5**: Switch from fallback binary format to real HDF5 via `rust-hdf5` (crates.io `0.2`). No C dependencies.
- **Compression**: zlib, SZIP, LZ4, Blosc (with sub-codecs: BloscLZ, LZ4, LZ4HC, Snappy, Zlib, Zstd). All via `rust-hdf5` filter pipeline.
- **SWMR streaming**: Single Writer Multiple Reader support — `SwmrFileWriter` with `append_frame`, periodic flush, ordered fsyncs.
- **Store performance**: Write timing measurement with Run time / I/O speed readback.
- **Store attributes**: Controllable via param (on/off).
- **File number fix**: Last filename now shows the actual written file, not the next incremented number.

### NeXus File Plugin (New)
- **NDFileNexus**: HDF5-based NeXus format writer with `/entry/instrument/detector/data` group hierarchy via `rust-hdf5` group API.

### Plugin on_param_change — All Plugins Complete
- **Process**: Full `on_param_change` for all 34 params. Filter type presets (RecursiveAve, Average, Sum, Difference, RecursiveAveDiff, CopyToFilter). Auto offset/scale calc. Separate low/high clip threshold and value. Scale flat field param.
- **Transform**: `on_param_change` for TRANSFORM_TYPE.
- **ColorConvert**: `on_param_change` for COLOR_MODE_OUT and FALSE_COLOR.
- **Overlay**: 8 runtime-configurable overlay slots via addr, with Position↔Center bidirectional readback.
- **FFT**: `on_param_change` for direction, suppress DC, num_average, reset_average. Num averaged readback.
- **CircularBuff**: `on_param_change` for Start/Stop, trigger A/B attributes, calc expression, pre/post count, preset triggers, soft trigger, flush on trigger. Status/triggered/trigger count readback.
- **Codec**: `on_param_change` for mode, compressor (LZ4/JPEG/Blosc), JPEG quality, Blosc sub-compressor/level/shuffle. Compression factor and status readback. Blosc compress/decompress via `rust-hdf5` filter pipeline.
- **Stats**: `on_param_change` for compute_statistics toggle.
- **BadPixel**: `on_param_change` for BAD_PIXEL_FILE_NAME — loads JSON bad pixel list at runtime. Moved from stub to real processor.
- **Attribute**: 8-channel multi-addr attribute extraction with TimeSeries integration. Moved from stub to real processor.

### Scatter/Gather — C ADCore Compatible
- **Scatter**: Round-robin distribution via `ProcessResult::scatter_index`. New `NDArrayOutput::publish_to(index)` for selective delivery.
- **Gather**: Multi-upstream wiring in `NDGatherConfigure` — accepts multiple port names.

### TimeSeries Refactor
- **`TsReceiverRegistry`**: Shared registry pattern. Stats/ROIStat/Attribute store TS receivers; `NDTimeSeriesConfigure` picks them up. Eliminates duplicate TS port creation code.
- **`NDTimeSeriesConfigure`**: Fully implemented (no longer a stub).

### File Plugin Infrastructure
- **Lazy open / Delete driver file / Free buffer**: Params wired in `FilePluginController` (shared by all file plugins).
- **ROIStat**: 32 ROIs (up from 8), with `NDROIStatN.template` × 32 in commonPlugins.cmd.

### Dependencies
- **rust-hdf5**: Switch from git dependency to crates.io `0.2`. Pure Rust HDF5 with all compression filters.

## v0.7.12

### CA Client Connection Stability
- **TCP keepalive**: Enable `SO_KEEPALIVE` with 15s idle time and 5s probe interval on all CA TCP connections. OS detects dead sockets within ~30s on idle circuits.
- **Client-side echo heartbeat**: Send `CA_PROTO_ECHO` after 30s of idle (matching C EPICS `CA_CONN_VERIFY_PERIOD`). If no response within 5s (`CA_ECHO_TIMEOUT`), declare connection dead and trigger automatic re-search + subscription recovery. Detects hung server processes that TCP keepalive alone cannot catch.
- **`EPICS_CA_CONN_TMO` support**: Echo interval configurable via environment variable, matching C EPICS behavior.

### Motor Record
- **Fix MOVN not resetting to 0**: `finalize_motion()` now clears MOVN when motion completes. Previously MOVN was computed before the phase transition to Idle and never updated, causing ophyd `PVPositionerPC` (which reads `.MOVN`) to report moving=true after `move(wait=True)` returned.

### areaDetector Plugins
- **NDFileMagick plugin**: New file writer using the `image` crate. Supports PNG, JPEG, BMP, GIF, TIFF (format determined by file extension), UInt8/UInt16 data, mono and RGB color modes. Parameters: `MAGICK_QUALITY`, `MAGICK_BIT_DEPTH`, `MAGICK_COMPRESS_TYPE`.
- **Idempotent plugin Configure commands**: Skip if port already exists, allowing `commonPlugins.cmd` to be loaded multiple times with different `PREFIX` for alias records.
- **Activate NDFileMagick** in `commonPlugins.cmd`.

### Asyn Device Support
- **Initial readback for input records**: Enable `with_initial_readback()` for input records (stringin, longin, etc.), matching C EPICS `devAsynXxx` `init_common()` behavior. Fixes `PluginType_RBV` and other I/O Intr input records returning template defaults ("Unknown") instead of the driver's current value.

### Wiring
- **Fix sender loss on failed rewire**: Validate new upstream exists before extracting sender from old upstream. Previously a failed rewire (e.g., invalid port name) would drop the sender, causing all subsequent rewires to fail.

## v0.7.11

### CA Client Transport Rewrite
- **Single-owner writer task**: Replace `Arc<Mutex<OwnedWriteHalf>>` with a dedicated `write_loop` task + mpsc channel. Eliminates writer lock contention between command dispatch and read_loop (ECHO responses).
- **Batch coalescing**: Writer task drains all pending frames via `try_recv` before issuing a single `write_all`, reducing TCP segment count under burst load.
- **TCP_NODELAY**: Set on all CA transport connections. Fixes ~45ms stall on `get()` immediately after `put()` caused by Nagle's algorithm + delayed ACK interaction.
- **Immediate write-error propagation**: `write_loop` sends `TcpClosed` on socket write failure, so pending `get()`/`put()` waiters fail immediately instead of hanging until timeout.

### CA Client Connection Fix
- **Channel starvation during concurrent PV creation**: `WaitConnected` and `Found` responses arriving before `RegisterChannel` are now buffered in `pending_wait_connected` / `pending_found` maps and drained on registration, preventing lost connections and infinite search loops.

## v0.7.10

### CA Client Search Engine Rewrite (libca++ level)
- **Adaptive deadline scheduler**: BTreeSet-based global scheduler replaces per-PV exponential backoff — lane-indexed retry with `period = (1 << lane) * RTT estimate`, max 5 min (configurable via `EPICS_CA_MAX_SEARCH_PERIOD`, floor 60s)
- **Per-path RTT estimation**: Jacobson/Karels algorithm (RFC 6298) per server address, 32ms floor — backoff adapts to actual network conditions instead of fixed 100ms→2s
- **Batch UDP search**: multiple SEARCH commands packed into single datagrams (≤1024 bytes), reducing packet count by ~30-50x for large PV sets
- **AIMD congestion control**: `frames_per_try` with additive increase (+1 on >50% response rate) / multiplicative decrease (reset to 1 on <10%) — prevents network flooding during mass PV search
- **Beacon anomaly detection**: dedicated `BeaconMonitor` task registers with CA repeater, tracks per-server beacon sequence/period, detects IOC restart (ID gap or period drop) and triggers selective rescan with 5s fast-rescan window
- **Connect-feedback penalty box**: servers that fail TCP create are deprioritized for 30s — prevents repeated connection attempts to unreachable servers
- **Selective rescan**: coordinator maintains server→channel reverse index, beacon anomaly rescans only affected channels (not global storm)
- **Immediate search on Schedule**: drain queued requests and send in same event loop iteration — fixes starvation where burst `create_channel` calls could delay first UDP search indefinitely

### CA Client Connection Improvements
- **Keep connect waiters on ChannelCreateFailed**: waiters stay pending so immediate re-search can still resolve before caller timeout (was: drain waiters on first failure)
- **AccessRightsChanged on channel create and reconnect**: fire event immediately after channel becomes connected
- **DBE_LOG in monitor mask**: match pyepics default (DBE_VALUE | DBE_LOG | DBE_ALARM)
- **Search recv buffer**: 256KB SO_RCVBUF for burst search response handling
- **Internal CA timeouts**: read/subscribe raised from 5s to 30s

### CA Client API
- **`CaChannel::info()`**: get channel metadata (native type, element count, host, access rights) without performing a CA read
- **`Snapshot` monitors**: `CaChannel::subscribe()` returns `Snapshot` with EPICS timestamp and alarm status

### IOC Shell
- **Output redirection**: `> file` and `>> file` support in iocsh without libc dependency

### Asyn
- **Synchronous write**: `can_block=false` ports use direct write instead of async channel, fixing write_op type coercion

## v0.7.9

### File Plugin Architecture (C ADCore NDPluginFile pattern)
- **`FilePluginController<W: NDFileWriter>`**: generic file plugin controller extracted to `ad-core-rs`, matching C ADCore's `NDPluginFile` base class — all file control logic (auto_save, capture, stream, temp_suffix rename, create_dir, param updates, error reporting) in one place
- All file plugins (TIFF, HDF5, JPEG, NetCDF) now delegate to `FilePluginController` via composition, eliminating ~300 lines of duplicated control logic
- **Auto-save**: write each incoming array as a single file when `AutoSave=Yes` (matches C `processCallbacks` autoSave)
- **Stream mode auto-stop**: close stream when `NumCaptured >= NumCapture` (NumCapture > 0), matching C `doCapture(0)` pattern
- **Capture mode**: full buffer → flush → close cycle with `NumCaptured` tracking
- **Temp suffix rename**: write to `path.tmp`, rename to `path` on close (all three modes)
- **Create dir**: `create_dir != 0` triggers `create_dir_all` (was `> 0` only, negative values like `-5` were ignored)
- **Write message cleared on success**: prevents stale error messages from persisting after successful writes
- **printf-style file template**: proper `%s%s_%3.3d.tif` expansion with sequential `%s` → filePath/fileName, `%d` with width/precision

### Waveform FTVL=CHAR Support
- asynOctetWrite device support for waveform records with `FTVL=CHAR`
- `write_only` flag: `read()` performs write (waveform is input record type in EPICS)
- Dynamic `field_list()` returns FTVL-appropriate VAL type (prevents CA write coercion errors)
- String → CharArray coercion in `put_field` for FTVL=CHAR
- NELM padding preserved on put (resize to NELM, prevents element count shrink)
- Trailing null trimming from CharArray before OctetWrite

### Plugin Infrastructure
- `register_params` implemented for all 12+ areaDetector plugins (was missing, causing silent `drv_user_create` failures)
- `on_param_change` with `Vec<ParamUpdate>` return for immediate param feedback (FILE_PATH_EXISTS, FULL_FILE_NAME, etc.)
- `ParamUpdate::Octet` variant for string param updates from data plane
- Fix NDArrayPort rewire: skip no-op rewire when `new_port == current_upstream` (eliminates startup race condition errors)

### Other
- `AdIoc::register_record_type()` for custom record type registration
- `put_notify` completion: `complete_async_record` fires `put_notify_tx.send(())` for CA WRITE_NOTIFY responses
- ophyd-test-ioc: all plugin ports reused for ADSIM prefix, motor record type registered

## v0.7.8

### Universal Asyn Device Support (C EPICS pattern)
- **`universal_asyn_factory`**: single factory handles all standard asyn DTYPs (`asynInt32`, `asynFloat64`, `asynOctet`, all array types) by parsing `@asyn(PORT,ADDR,TIMEOUT)DRVINFO` links and resolving params via `drv_user_create` → `find_param`, matching C EPICS asyn behavior exactly
- **All custom device support eliminated**: `MovingDotDeviceSupport`, `PointDetectorDeviceSupport`, `SimDeviceSupport`, `ScopeDeviceSupport`, `PluginDeviceSupport` — replaced by universal factory (~1,800 lines removed)
- **`ParamRegistry` infrastructure removed**: `ParamRegistry`, `ParamInfo`, `RegistryParamType`, all `build_param_registry` functions — `drv_user_create`/`find_param` replaces them
- **Plugin dynamic factory removed**: `PluginManager` no longer provides device support dispatch — only manages lifecycle, port registration, and NDArray wiring

### Template Migration
- All templates converted from `$(DTYP)` to standard asyn DTYPs with `@asyn(PORT,...)DRVINFO` links
- CP-linked records use 2-stage pattern (C ADCore `NDOverlayN` pattern): Soft Channel link receiver → asyn record via `OUT PP`
- `commonPlugins_settings.req` aligned with C ADCore (added StdArrays, Scatter/Gather, AttributeN, file-type-specific .req)

### Array Data (C EPICS pattern)
- Full array type support: `Int8`, `Int16`, `Int32`, `Int64`, `Float32`, `Float64` (read + write)
- `PluginPortDriver::read_*_array` overrides serve pixel data from NDArray (matching C `NDPluginStdArrays::readArray`)
- Array data pushed via direct interrupt (bypasses port actor channel), matching C `arrayInterruptCallback` pattern
- `param_value_to_epics_value` handles all array `ParamValue` variants

### Param Names (C ADCore alignment)
- All `create_param` names aligned with C ADCore `#define` strings: `ACQ_TIME`, `ACQ_PERIOD`, `NIMAGES`, `STATUS`, `ENABLE_CALLBACKS`, `ARRAY_NDIMENSIONS`, etc.
- Added missing `NDPluginDriver` params: `MAX_THREADS`, `NUM_THREADS`, `SORT_MODE`, `SORT_TIME`, `SORT_SIZE`, `SORT_FREE`, `DISORDERED_ARRAYS`, `DROPPED_OUTPUT_ARRAYS`, `PROCESS_PLUGIN`, `MIN_CALLBACK_TIME`, `MAX_BYTE_RATE`

### Other
- Per-parameter callback flush (`call_param_callback`) to avoid unintended side-flush
- `normalize_asyn_dtyp`: strips direction suffixes (`asynOctetRead` → `asynOctet`, `asynFloat64ArrayIn` → `asynFloat64Array`)
- Graceful `drv_user_create` failure: silently disables device support for records without matching driver param
- MovingDot: binning support (BinX/BinY), fix NDArray dims order
- Autosave for MovingDot cam1, `commonPlugins_settings.req` fixes
- `PvDatabase::get_pv_blocking` for sync access from std::threads
- `AdIoc::keep_alive` for driver runtime lifetime management
- `EpicsTimestamp::to_system_time` for interrupt timestamp consistency
- Fix array interrupt: handle I64/U64 types, use NDArray timestamp (not wall clock)
- Fix ADCORE path in AdIoc (`ad-core` → `ad-core-rs`)
- ophyd-test-ioc: switch from MovingDot to SimDetector (provides GainX/Y, Noise, etc.)
- ophyd-test-ioc: use AdIoc, add ADSIM: prefix for ophyd test compatibility
- All crate READMEs: fix license to EPICS Open License, add missing READMEs

## v0.7.7

_Superseded by v0.7.8 — v0.7.7 was an intermediate release._

## v0.7.6

### Runtime Facade
- **asyn-rs**: add `runtime::sync` (mpsc, oneshot, broadcast, Notify, Mutex, RwLock), `runtime::task` (spawn, sleep, interval, RuntimeHandle), and `runtime::select!` re-exports — driver authors no longer need to depend on tokio directly
- **epics-base-rs**: add matching re-exports in `runtime::sync` and `runtime::task`, plus `select!` macro re-export and hidden `__tokio` re-export for macro hygiene

### Proc Macros
- **`#[epics_main]`**: attribute macro replacing `#[tokio::main]` — validates `async fn main()`, no args, no generics, no attribute arguments; builds multi-thread runtime via `epics_base_rs::__tokio`
- **`#[epics_test]`**: attribute macro replacing `#[tokio::test]` — validates async fn with no args/generics, rejects duplicate `#[test]`; builds current-thread runtime (matching `#[tokio::test]` default)

### Examples Modernized
- All examples (`mini-beamline`, `scope-ioc`, `sim-detector`, `ophyd-test-ioc`, `random-signals`) now use the runtime facade instead of tokio directly
- `scope-ioc`: `epics-base-rs` promoted from optional to required dependency
- Zero `tokio::` references remain in example code (except `#[tokio::main]` → `#[epics_main]`)

### Docs
- Quick Start: add binary location (`target/release/`) and PATH setup
- Quick Start: fix build command to use `--release`
- Update copyright name in LICENSE

## v0.7.5

### areaDetector PV Convention
- Adopt standard areaDetector PV convention (`P=mini:dot:`, `R=cam1:`) in mini-beamline
- Add NDStdArrays `image1` plugin to `commonPlugins.cmd`
- Include `ADBase.template` for full ADBase PV set (TriggerMode, Gain, etc.)
- Add missing param registry entries for NDArrayBase PVs
- Fix param name mismatches with C ADCore templates

### CA Server
- Non-blocking WRITE_NOTIFY: spawn background task for completion instead of blocking `dispatch_message`, matching C EPICS rsrv behavior
- Remove arbitrary 30s timeout — wait indefinitely for record completion

### MovingDot Driver
- Non-blocking port writes in device support and acquisition task to prevent tokio thread starvation
- Remove `call_param_callbacks` from driver write methods to prevent re-entrant message storms
- Add slit aperture simulation (SlitLeft/Right/Top/Bottom in pixels)
- Output UInt16 image data (realistic photon counts)
- Tolerate read failures during config refresh instead of aborting acquisition

### Waveform Record
- Add SHORT/USHORT and FLOAT FTVL support (was falling through to DOUBLE)
- Fix `DbFieldType`-to-`menuFtype` mapping in `new()`
- `PluginDeviceSupport`: native `EpicsValue` types for NDArray data

### AsynDeviceSupport
- Add public accessors (`reason`, `addr`, `handle`, `write_op_pub`)

### Docs
- Quick Start: add binary location (`target/release/`) and PATH setup
- Quick Start: fix build command to use `--release`
- Update copyright name in LICENSE

## v0.7.4

### New Crate
- **optics-rs**: Port of EPICS optics synApps module — table record (6-DOF, 4 geometry modes), Kohzu/HR/ML-mono DCM controllers, 4-circle orientation matrix, XIA PF4 dual filter, auto filter drive, HSC-1 slit, quad BPM, ion chamber, Chantler X-ray absorption data (22 elements), 36 database templates, PyDM UI screens, 362 tests including 46 golden tests vs C tableRecord.c

### dbAccess: C EPICS Parity
- **Three-tier DB write API** matching C EPICS semantics:
  - `put_pv` / `put_f64` = C `dbPut` — value + special, no monitor, no process
  - `put_pv_and_post` / `put_f64_post` = C `dbPut` + `db_post_events` — value + monitor on change
  - `put_record_field_from_ca` / `put_f64_process` = C `dbPutField` — value + process + monitor
- **Event source tagging** — origin ID prevents sequencer self-feedback loops; `DbChannel::with_origin()`, `DbMultiMonitor::new_filtered()`, origin-aware `DbSubscription`
- **DbChannel API**: add `put_i16_process`, `put_i32_process`, `put_string_process`, `get_i32`
- **TPRO** trace processing output when field is set
- **Pre-write special** hook in CA put path (`special(field, false)` before write)
- **Read-only field** enforcement in `put_record_field_from_ca`
- **ACKS/ACKT** alarm acknowledge with severity comparison
- **Menu string resolution** in type conversion (String → Enum/Short)
- **dbValueSize / dbBufferSize** equivalents
- **is_soft_dtyp**: recognize "Raw Soft Channel", "Async Soft Channel", "Soft Timestamp", "Sec Past Epoch"
- **stringout**: add OMSL/DOL fields and framework DOL processing support

### SNL Programs: CA → DbChannel Migration
- All 7 optics-rs SNL programs converted from CA client to direct database access:
  kohzu_ctl, hr_ctl, ml_mono_ctl, kohzu_ctl_soft, orient, pf4, filter_drive
- Origin tagging + filtered monitors prevent write-back loops
- Kohzu DCM: non-blocking move with `tokio::select!` retarget support

### Bug Fixes
- **I/O Intr read timeout**: cache interrupt value in adapter, skip blocking read on cache miss
- **ao DOL/OIF conflict**: remove duplicate DOL handling from ao process() (framework handles it)
- **put_pv_and_post timestamp**: update `common.time` before posting monitor events
- **Redundant monitors**: suppress duplicate events when value unchanged

### Breaking Changes
- Remove `epics-seq-rs`, `snc-core-rs`, `snc-rs` (replaced by native Rust async state machines in optics-rs and std-rs)

## v0.7.3

### New Crates
- **std-rs**: Port of EPICS std module — epid (PID/MaxMin feedback), throttle (rate-limited output), timestamp (formatted time strings) records, plus device support (Soft/Async/Fast Epid, Time of Day, Sec Past Epoch) and SNL programs (femto gain control, delayDo state machine)
- **scaler-rs**: Port of EPICS scaler module — 64-channel 32-bit counter record with preset-based counting, OneShot/AutoCount modes, DLY/DLY1 delayed start, RATE periodic display update, asyn device support, and software scaler driver

### Framework: ProcessOutcome / ProcessAction
- **Breaking**: `Record::process()` now returns `CaResult<ProcessOutcome>` instead of `CaResult<RecordProcessResult>`
- `ProcessOutcome` contains `result` (Complete/AsyncPending) + `actions` (side-effect requests)
- `ProcessAction::WriteDbLink` — record requests a DB link write without direct DB access
- `ProcessAction::ReadDbLink` — record requests a DB link read (pre-process execution)
- `ProcessAction::ReprocessAfter(Duration)` — delayed self re-process (replaces C `callbackRequestDelayed` + `scanOnce`)
- `ProcessAction::DeviceCommand` — record sends named commands to device support via `handle_command()`
- Processing layer executes actions at the correct point in the cycle (ReadDbLink before process, WriteDbLink/DeviceCommand after, ReprocessAfter via tokio::spawn)

### Framework: DeviceReadOutcome
- **Breaking**: `DeviceSupport::read()` now returns `CaResult<DeviceReadOutcome>` instead of `CaResult<()>`
- `DeviceReadOutcome` carries `did_compute` flag and `actions` list
- `did_compute`: signals that device support already performed the record's compute step (e.g., PID), passed to record via `set_device_did_compute()` before `process()`
- Device support actions are merged into the record's ProcessOutcome by the framework

### Framework: Other Improvements
- `Record::pre_process_actions()` — return ReadDbLink actions executed BEFORE process() (matches C `dbGetLink` immediate semantics)
- `Record::put_field_internal()` — bypasses read-only checks for framework-internal writes
- `Record::set_device_did_compute()` — framework signals device support compute status
- `DeviceSupport::handle_command()` — handle named commands from ProcessAction::DeviceCommand
- `field_io.rs`: `put_pv()` and `put_record_field_from_ca()` now call `on_put()` + `special()` for record-owned fields (was previously only for common fields)
- ReprocessAfter timer cancellation via generation counter in RecordInstance (prevents stale timer accumulation)

### Workspace Integration
- Add `std-rs` and `scaler-rs` to workspace members and default-members
- Add `std` and `scaler` feature flags to epics-rs umbrella crate
- Bundle 70+ database templates (.db) and autosave request files (.req)

### Testing
- Add 390+ new tests across all crates:
  - std-rs: 94 tests (epid PID algorithm, throttle rate limiting, timestamp formats, SNL state machines, framework integration, e2e autosave)
  - scaler-rs: 40 tests (64-channel field access, state machine, TP↔PR1 conversion, soft driver, DLY delayed start, COUT/COUTP link firing)
  - asyn-rs: 20 integration tests (port driver parameters, octet echo, error handling, interrupt callbacks, enum, blocking API)
  - ad-core-rs: 47 tests (NDArray types/dimensions, pool allocation/reuse/memory limits, attributes, concurrent access)
  - epics-macros-rs: 27 tests (derive macro field generation, type mapping, read-only, snake_case conversion)
  - epics-ca-rs: 30 tests (protocol header encoding, server builder, get/put API, field access, multiple record types)
  - epics-pva-rs: 49 tests (scalar types, PvStructure, serialization roundtrip, protocol header, codec)
  - epics-seq-rs: 30 tests (event flags, channel store, program builder, variable traits)
  - snc-core-rs: 42 tests (lexer tokenization, parser AST, codegen output, end-to-end pipeline)
  - snc-rs: 11 tests (CLI help, compilation, error handling, debug flags)

## v0.7.2

- Fix asyn-rs epics feature compilation (get_port export, AsynRecord import)
- Migrate record factory registration from global registry to IocApplication injection
- Replace global port registry with shared PortRegistry instance
- Add feature matrix to CI (asyn-rs/epics, ad-core-rs/ioc, ad-plugins-rs/ioc)
- Add IocApplication::register_record_type() method
- Add motor_record_factory() and asyn_record_factory() returning injectable tuples

## v0.7.1

### Architecture
- Extract `IocBuilder` from `CaServerBuilder` into epics-base-rs (protocol-agnostic IOC bootstrap)
- Move `IocApplication` to epics-base-rs with pluggable protocol runner closure
- Split `database.rs` into modules: field_io, processing, links, scan_index
- Split `record.rs` into modules: alarm, scan, link, common_fields, record_trait, record_instance
- Split `types.rs` into modules: value, dbr, codec
- Split `db_loader.rs` into parser + include expander modules
- Split `asyn_record.rs` registry into separate module
- Extract motor field dispatch to `field_access.rs`
- Remove thin wrapper crates (autosave-rs, busy-rs, epics-calc-rs) — now re-exported from epics-base-rs
- Remove legacy autosave API, migrate to SaveSetConfig/AutosaveManager
- Remove unused calc feature flags
- Crate directory names now match crate names (crates/motor → crates/motor-rs, etc.)

### API
- Reduce public API surface: 7 internal modules → pub(crate) (recgbl, scan_event, exception, interpose, protocol, transport, channel)
- Motor lib.rs: fields, coordinate → pub(crate); remove pub use fields::*, flags::*
- Add `create_record_with_factories()` for dependency injection (avoids global registry)
- `IocApplication::run()` now accepts a protocol runner: `.run(run_ca_ioc).await`

### Testing
- Move large inline test blocks to tests/ directory (3,337 lines)
- Add autosave integration test with mini-beamline (save + restore on restart)

### Fixes
- Fix ad-core path references after directory rename
- Fix remaining old crate directory references in README and examples
- Clean all clippy warnings

## v0.7.0

- **Breaking**: Separate Channel Access into `epics-ca-rs` crate
- **Breaking**: Separate pvAccess into `epics-pva-rs` crate
- **Breaking**: Rename crates for consistent `-rs` suffix (ad-core-rs, ad-plugins-rs, epics-macros-rs, epics-seq-rs, snc-core-rs, snc-rs)
- Add `epics-rs` umbrella crate with feature flags (ca, pva, motor, ad, calc, full, etc.)
- Remove msi from workspace (moved to separate repo)
- Add 113 C EPICS parity tests (ai/bi/bo record, deadband, alarm, calc engine, FLNK chains, CA wire protocol, .db parsing, autosave)
- Add SAFETY comments for production unwrap sites
- Clippy lint cleanup across all crates

## v0.6.1

- Fix monitor deadband for records without MDEL field
- Reset beacon interval on TCP connect/disconnect (C EPICS parity)
- Fix caput-rs to use fire-and-forget write like C caput, add `-c` flag for callback mode
- Show Old/New values in caput-rs output
- Support multiple PV names in CA/PVA CLI tools (caget, camonitor, cainfo, pvget, etc.)
- Add per-field change detection for monitor notifications
- Add DMOV same-position transition tests
- Poll motor immediately on StartPolling for faster DMOV response
- Add motor tests ported from ophyd (sequential moves, calibration, RBV updates, homing)
- Update minimum Rust version to 1.85+ for edition 2024

## v0.6.0

- Deferred write_notify via callback for motor records
- Motor display/ctrl metadata support
- SET mode RBV updates

## v0.5.2

- Fix monitor notify, DMOV transition, timestamp, and IPv4 resolution

## v0.5.1

- Add DMOV 1->0->1 monitor transition for motor moves

## v0.5.0

- Fix motor record process chain, client error handling, and connection speed
- Add ophyd-test-ioc example

## v0.4.6

- Add client-side DBR_TIME/CTRL decode and get_with_metadata() API

## v0.4.5

- Upgrade Rust edition 2021 -> 2024

## v0.4.4

- Bug fixes

## v0.4.3

- Add generalTime framework for priority-based time providers
- Add random-signals example
- Add GitHub Actions CI workflow

## v0.4.2

- Implement C-compatible autosave iocsh commands and request file infrastructure

## v0.4.1

- Implement full YUV color mode support and refactor color convert plugin

## v0.4.0

- Initial crates.io publish
- Move to epics-rs GitHub organization

## v0.3.0

- Unify workspace version management
