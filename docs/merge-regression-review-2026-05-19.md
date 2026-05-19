# Merge Regression Review - 2026-05-19

Branch: `integration/punchlist-2026-05-19`

Base: `origin/main` at `aa1d58b364476d8c42dc81db2ae14a6900842973`

Head reviewed: `17273682e837a6c73d00fe3052466fc48dca10dd`

Method:

- Reviewed `origin/main...HEAD` diff for the high-change surfaces: CA server/client, PVA native server/client, QSRV/pvalink, and the new record-lock layer.
- Compared selected current behavior against `origin/main` to separate branch regressions from pre-existing defects.
- Ran workspace checks listed at the end of this file.
- Line numbers below refer to the working tree at the review time.

## Branch Regression Candidates

### MR-R1 - CA `WRITE_NOTIFY` busy gate runs after side effects

Severity: high.

Evidence:

- `crates/epics-ca-rs/src/server/tcp.rs:218` introduces `ChannelEntry::put_notify_busy` as the per-channel `WRITE_NOTIFY` gate.
- `crates/epics-ca-rs/src/server/tcp.rs:2573` executes the real write (`pv.set`, write hook, or `put_record_field_from_ca`) before the busy check.
- `crates/epics-ca-rs/src/server/tcp.rs:2654` checks `put_notify_busy` only after the write has already completed enough to return `Ok(Some(rx))`.
- `crates/epics-ca-rs/src/server/tcp.rs:2249` handles `DBR_PUT_ACKT` / `DBR_PUT_ACKS` before the regular branch and never consults `put_notify_busy`; it can mutate ACK state at `:2322` while another put-notify is in flight.

Impact:

A second `CA_PROTO_WRITE_NOTIFY` arriving while the first async put-notify is pending can still mutate the PV/device state, then receive `ECA_PUTCBINPROG` as if the write had been rejected. If the second write completes synchronously, the current code never checks the busy gate at all. This is worse than the old "no serialization" behavior because the client can be told the second write did not happen when it already did.

Why tests missed it:

Current protocol tests cover bad-type `WRITE_NOTIFY`, but no test sends two same-channel `WRITE_NOTIFY` frames while the first one has an outstanding completion receiver. No ACKT/ACKS put-notify busy test exists.

Expected fix shape:

The busy gate must be acquired before any trap-write `BeforeWrite`, database write, write hook, ACKT/ACKS mutation, or async device kickoff on every `is_notify` side-effect path. A guard should clear the gate on synchronous completion, pre-write errors, and async completion/cancellation.

### MR-R2 - `SharedPV::put_delta` closes monitor queues by dropping cloned `MonitorOutbox` senders

Severity: high.

Evidence:

- `crates/epics-pva-rs/src/server_native/shared_pv.rs:117` makes `MonitorOutbox` cloneable.
- `crates/epics-pva-rs/src/server_native/shared_pv.rs:179` implements `Drop for MonitorOutbox` by setting `producer_done = true`.
- `crates/epics-pva-rs/src/server_native/shared_pv.rs:523` clones `g.subscribers` in `SharedPV::put_delta`.
- `crates/epics-pva-rs/src/server_native/shared_pv.rs:536` posts through those clones outside the lock. When the cloned vector drops at function exit, each clone marks the shared queue as producer-done.
- `crates/epics-pva-rs/src/server_native/shared_pv.rs:155` then causes future posts to that queue to be ignored once `producer_done` is true, while `is_closed()` still reports false because the receiver was not dropped.

Impact:

Any no-handler BitSet delta PUT to a `SharedPV` with subscribers can deliver one merged event and then cause the subscriber inbox to terminate or stop receiving future posts. The canonical subscriber remains in `g.subscribers`, so lifecycle accounting can also stay stale. `origin/main` used `tokio::mpsc::Sender` clones here; dropping a temporary sender clone did not close the receiver.

Why tests missed it:

`concurrent_disjoint_delta_puts_do_not_lose_updates` exercises `put_delta` without a subscriber. The new squash-to-tail tests exercise `try_post`, not `put_delta`.

Expected fix shape:

Dropping a cloned `MonitorOutbox` must not signal producer completion. Closure should be explicit from `SharedPV::close` / canonical subscriber removal, or the sender side needs clone-aware ownership so only the actual producer endpoint can close the inbox.

### MR-R3 - CA reconnect can emit false multiply-defined PV diagnostics after a legitimate server change

Severity: high.

Evidence:

- `crates/epics-ca-rs/src/client/search.rs:270` documents `resolved` as live until cancel/channel drop.
- `crates/epics-ca-rs/src/client/search.rs:311` clears `resolved` only in `remove_channel`.
- `crates/epics-ca-rs/src/client/search.rs:328` makes `mark_connected` a no-op.
- `crates/epics-ca-rs/src/client/search.rs:1124` emits `SearchResponse::MultiplyDefined` when a later search reply for the same cid comes from a different address.
- `crates/epics-ca-rs/src/client/mod.rs:3105` schedules reconnect after `ServerDisconnect` without clearing the old resolved address.
- `crates/epics-ca-rs/src/client/mod.rs:3300` schedules reconnect after TCP close without clearing the old resolved address.

Impact:

After the original server disconnects, a reconnect search for the same cid can legitimately resolve to a different IOC/server address. The stale `resolved` entry still points at the old circuit, so the first reply from the new server can be surfaced as multiply-defined (`ECA_DBLCHNL`) even though the old circuit is gone.

Why tests missed it:

Search tests cover reconnect bucket placement and duplicate replies, but not the lifecycle "connected to server A, disconnect, reconnect to server B" with `resolved` state still present.

Expected fix shape:

The duplicate detector needs a lifecycle state boundary: either clear or invalidate the resolved address when the channel enters disconnected/researching state, or compare only against currently live circuits.

### MR-R4 - pvalink typed array OUT writes ignore `field=...`

Severity: medium-high.

Evidence:

- `crates/epics-bridge-rs/src/pvalink/link.rs:321` correctly sends scalar string writes through `pvput_field_with_request` when `is_subfield(config.field)` is true.
- `crates/epics-bridge-rs/src/pvalink/link.rs:356` sends typed `PvField` writes only through `pvput_pv_field_with_request(&config.pv_name, ...)`, ignoring `config.field`.
- `crates/epics-bridge-rs/src/pvalink/link.rs:437` replays deferred `QueuedPut::Field` the same root-targeted way.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:873` now preserves query-bearing OUT options and `:906` routes array values through `write_pv_field`.

Impact:

An OUT link such as `pva://PV?field=someArray` can write scalar values to the selected subfield, but array values go to the root/value target instead. Deferred or retry replay of typed array writes has the same targeting error. This branch newly exposes per-link OUT query options, so this is a branch-visible regression for array-valued OUT links.

Why tests missed it:

The tests cover `is_subfield`, string deferred queueing, and typed queue shape. They do not assert that an immediate or deferred typed `PvField` write uses the selected subfield.

Expected fix shape:

Typed field writes need a field-targeted client path, or the pvalink layer must encode the typed value into the selected subfield request instead of always sending it as a root/value PUT.

### MR-R5 - Record-lock invariant has public write/process bypasses

Severity: high.

Evidence:

- `crates/epics-base-rs/src/server/database/record_lock.rs:129` adds `lock_record` and `lock_records` as the advisory `dbScanLock`/`DBManyLock` analogue.
- `crates/epics-base-rs/src/server/database/field_io.rs:93` protects `put_pv`.
- `crates/epics-base-rs/src/server/database/field_io.rs:438` protects `put_record_field_from_ca`.
- `crates/epics-base-rs/src/server/database/processing.rs:54` protects `process_record`.
- `crates/epics-base-rs/src/server/database/processing.rs:87` through `:96` exposes `process_record_with_links`, but its entry calls `process_record_with_links_inner(..., false)` without taking `lock_record`.
- Runtime scan/event and startup paths call that unlocked full-processing entry directly: periodic scan at `crates/epics-base-rs/src/server/scan.rs:55` and `:99`, event scan at `crates/epics-base-rs/src/server/scan_event.rs:110`, `:136`, `:146`, and `:167`, PINI/startup at `crates/epics-base-rs/src/server/ioc_app.rs:682` and `:950`, and scan-index event posting at `crates/epics-base-rs/src/server/database/scan_index.rs:135` and `:167`.
- The pvalink atomic scan path explicitly holds `lock_records` for atomic targets at `crates/epics-bridge-rs/src/pvalink/integration.rs:727`, then processes each target via `process_record_with_links` at `:782` through `:785`. Since normal scan/event callers do not acquire the same gate, the comment at `:700` through `:703` claiming that another scan cannot interleave is not enforced.
- `crates/epics-base-rs/src/server/database/field_io.rs:221` exposes `put_pv_and_post_with_origin`; its record branch takes `rec.write().await` at `:246` without taking the advisory gate.
- `crates/epics-base-rs/src/server/database/field_io.rs:772` exposes `put_pv_no_process`; its record branch takes `rec.write().await` at `:784` without taking the advisory gate.

Impact:

The new lock layer closes the atomic-group/direct-write race only for the specific entry points routed through it. Runtime callers that use `put_pv_and_post` or `put_pv_no_process` on record fields can still interleave with a QSRV atomic group or pvalink atomic epoch holding `lock_records`. Separately, every normal scan/event/FLNK-style caller of `process_record_with_links` bypasses the gate, so a pvalink atomic scan epoch can still overlap with a periodic/event scan of one member record. That reopens the invariant through public APIs in the same module and through the main runtime processing loop.

Why tests missed it:

The record-lock regression tests exercise `put_record_field_from_ca` and direct `lock_record`, but do not try `put_pv_and_post`, `put_pv_no_process`, or a normal `process_record_with_links` scan while a many-record guard is held.

Expected fix shape:

Route every foreign full-processing entry through the same advisory gate and add an `_already_locked` full-processing variant for pvalink/QSRV transaction owners. Route the write helpers through the gate as well, add `_already_locked` variants where needed, or narrow their visibility/contract so they cannot be used as runtime record-write APIs.

### MR-R6 - Gateway upstream identity cap has a stale public field

Severity: low-medium.

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:154` exposes `pub max_upstream_identities: usize`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:213` initializes the actual bounded pools with `BoundedPool::new(256)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:419` changes the actual cap only through `set_max_upstream_identities(&self, n)`, by replacing the two internal pools.
- `rg max_upstream_identities` shows no production read of the public field after construction.

Impact:

External code can mutate `source.max_upstream_identities`, but that value is no longer the cap used by `upstream_pool` or `upstream_caches`. Conversely, calling the setter changes the actual cap but leaves the public field at its old value. This is an API regression for callers that treat the public field as configuration or diagnostics.

Why tests missed it:

The new bounded-pool test calls `set_max_upstream_identities(2)`. It does not mutate or read the public field.

Expected fix shape:

Remove or privatize the field, or make it an interior-mutable diagnostic accessor backed by the same state as the pools.

### MR-R7 - CA UDP batched responder can parse a stale datagram under a new peer

Severity: high.

Evidence:

- `crates/epics-ca-rs/src/server/udp.rs:370` stores the first datagram's peer in `current_src` and `:374` copies that datagram into `current_buf`.
- The new R2-79 batching loop parses `current_buf` inside the `'parse` loop at `crates/epics-ca-rs/src/server/udp.rs:413`.
- `crates/epics-ca-rs/src/server/udp.rs:636` drains queued datagrams with `try_recv_from(&mut peek_buf)`.
- If the queued datagram comes from a different peer, `crates/epics-ca-rs/src/server/udp.rs:641` flushes the current batch and `:653` changes `current_src = peek_src`.
- The short-datagram, ignore-list, and rate-limit branches at `crates/epics-ca-rs/src/server/udp.rs:638`, `:658`, and `:662` all `continue 'parse` before `current_buf` is replaced at `:666`.

Impact:

A queued short, ignored, or rate-limited datagram can change the current peer and then re-enter the parser with the previous datagram bytes still in `current_buf`. The old client's SEARCH can be reprocessed as if it came from the new peer, and the final flush at `:676` can send replies to the wrong address. With the same peer, the same early-continue shape can also duplicate replies for the previous datagram. `origin/main` processed one datagram per recv and had no `peek_buf` batching loop, so this stale-buffer path is branch-introduced.

Why tests missed it:

The UDP parity tests cover message parsing and basic reply shape, but no test queues two datagrams where the second one is rejected by short-length, ignore-list, or rate-limit handling after a peer transition.

Expected fix shape:

After `try_recv_from` consumes a queued datagram, the code must not restart parsing unless `current_buf` has been replaced with that datagram's bytes. Rejected queued datagrams should be dropped and the loop should either drain another queued datagram or break to the outer recv path without reusing the old buffer under the new `current_src`.

### MR-R8 - Wildcard multicast responders can duplicate ordinary CA search replies

Severity: medium.

Evidence:

- `crates/epics-ca-rs/src/server/udp.rs:33` documents the duplicate-reply problem when an extra wildcard-bound multicast responder also catches unicast/broadcast traffic.
- The specific-interface case avoids that extra responder by setting `has_specific` at `crates/epics-ca-rs/src/server/udp.rs:44` and only joining multicast groups on the per-interface socket at `:52`.
- When every interface is `0.0.0.0`, the code still starts the primary wildcard responder at `crates/epics-ca-rs/src/server/udp.rs:48` and then starts one wildcard-bound multicast responder per group at `:65`.
- All these sockets are bound by `bind_responder_socket`, which enables datagram fanout with `SO_REUSEADDR` / `SO_REUSEPORT` at `crates/epics-ca-rs/src/server/udp.rs:212`.
- `IP_MULTICAST_ALL=0` at `crates/epics-ca-rs/src/server/udp.rs:226` filters multicast group cross-talk on Linux; it does not filter ordinary unicast or broadcast CA SEARCH datagrams.

Impact:

With `EPICS_CAS_INTF_ADDR_LIST` effectively wildcard and `EPICS_CAS_MCAST_ADDR_LIST` non-empty, a normal unicast or broadcast search can be delivered to the primary wildcard responder and to the extra wildcard multicast responders. Each responder can emit its own `send_to(reply, src)`, producing duplicate CA search replies for one request. `origin/main` had only the normal wildcard responder and no multicast-responder fanout path.

Why tests missed it:

The current tests do not start a wildcard responder with multicast groups configured and then inject a non-multicast SEARCH while counting replies across all bound responder sockets. The duplication is also socket-stack dependent, so it needs an explicit Linux/macOS responder test instead of only byte-level parser tests.

Expected fix shape:

The wildcard configuration needs one owner for ordinary unicast/broadcast SEARCH traffic. Either join multicast groups on the primary wildcard responder, or make the auxiliary multicast responders discard non-multicast traffic before running the normal SEARCH reply path.

### MR-R9 - `share_udp(true)` PVA clients silently drop configured TCP name servers

Severity: medium-high.

Evidence:

- `crates/epics-pva-rs/src/client_native/context.rs:151` exposes `PvaClientBuilder::name_servers`.
- `crates/epics-pva-rs/src/client_native/context.rs:366` routes `share_udp(true)` clients through the process-wide `SHARED_SEARCH_ENGINE`.
- That singleton is created with `SearchEngine::spawn(Vec::new(), Vec::new())` at `crates/epics-pva-rs/src/client_native/context.rs:372`, discarding `self.inner.name_servers`.
- The non-shared path at `crates/epics-pva-rs/src/client_native/context.rs:377` does pass `self.inner.name_servers.clone()` into the search engine.
- In `origin/main`, name servers were still passed to `Channel::new_with_name_servers` from `context.rs:315`; `Channel::ensure_active` appended those name servers as direct fallback candidates at old `channel.rs:736`. This branch removed that channel-level fallback and moved name-server handling into `SearchEngine`.

Impact:

A client built with both `.share_udp(true)` and `.name_servers(...)` (or `EPICS_PVA_NAME_SERVERS`) will no longer query the configured name servers and will also no longer get the old direct-connect fallback. The builder API accepts the configuration, but the shared search path drops it without an error or diagnostic.

Why tests missed it:

The new name-server test constructs `SearchEngine::spawn(Vec::new(), vec![ns_addr])` directly. No test builds a `PvaClient` with `share_udp(true)` and a name server, then proves the TCP name-server connection is opened or used.

Expected fix shape:

The shared engine path needs a defined contract. If per-client name servers cannot be represented by one singleton, `.share_udp(true)` must be rejected or disabled when the name-server list is non-empty. Otherwise the shared engine key must include the name-server set.

### MR-R10 - QSRV group PUT and PUT_GET ignore INIT pvRequest options on the native wire path

Severity: medium-high.

Evidence:

- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:319` correctly states that PUT options live in the INIT pvRequest, not in the data-phase value.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:325` parses those options into `opts`.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:336` passes `opts` only to `AnyChannel::Single(single).put_with_options`.
- The group and fallback arm at `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:340` calls `other.put(&pv)`, discarding the parsed INIT options.
- `crates/epics-bridge-rs/src/qsrv/group.rs:847` honors `record._options.atomic` for group GET because GET receives the request structure directly, but `GroupChannel::put` at `:860` and `:866` parses options from the data-phase value.
- The dedicated PVA `PUT_GET` path discards its INIT pvRequest value at `crates/epics-pva-rs/src/server_native/tcp.rs:1636` and builds the data-phase context with `pv_request: None` at `:1703`, so QSRV group PUT_GET cannot recover INIT options either.

Impact:

Native PVA clients cannot make group PUT or group PUT_GET honor INIT pvRequest options such as `record._options.atomic` through the normal wire path. The bridge computes the right options for normal PUT, then drops them before calling the group channel; the dedicated PUT_GET path does not stash them at all. A data-phase value can still carry options for in-process callers, but that is not the pvAccess wire contract the new comments and single-record path implement.

Why tests missed it:

The `put_value_checked_honors_pv_request_process_force` coverage is single-record oriented. There is no native PVA group PUT or PUT_GET test where the INIT pvRequest sets `record._options.atomic` and the data payload omits that option.

Expected fix shape:

Group channels need a `put_with_request_options` / `put_with_options` entry point, or `QsrvPvStore::put_value_checked` must pass the captured INIT pvRequest into the group PUT path before the value-phase structure is inspected.

### MR-R11 - PVA role claims are parsed but dropped before QSRV ACF checks

Severity: medium-high.

Evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:717` adds `ClientCredentials::roles`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:838` parses `groups` / `roles` from the `ca` auth payload into `creds.roles`.
- `crates/epics-pva-rs/src/server_native/source.rs:23` defines `ChannelContext`, but the fields end at `authority` and `pv_request`; there is no `roles` field.
- Context construction sites such as `crates/epics-pva-rs/src/server_native/tcp.rs:1697`, `:2664`, and `:3005` pass account/method/host/authority but cannot pass roles.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:152` converts `ChannelContext` to QSRV `ClientCreds`, but hardcodes `roles: Vec::new()` at `:158`.
- `crates/epics-bridge-rs/src/qsrv/provider.rs:161` can evaluate role-based credential strings only if roles reach `ClientCreds`.

Impact:

Role-based UAG/ACF checks are not enforced for real native PVA clients even though the branch parses the role list and QSRV's access-control layer can consume it. Rules that should grant access to `role/ops` style credentials will deny over the wire because the source receives an empty role list.

Why tests missed it:

The QSRV provider test constructs `ClientCreds { roles: vec![...] }` directly. It does not drive a native PVA connection through `parse_client_credentials`, `ChannelContext`, `ctx_to_creds`, and `AcfAccessControl`.

Expected fix shape:

Add roles to `ChannelContext`, populate it at every context construction from `ClientCredentials.roles`, and forward it in `ctx_to_creds`. The type-state access gate also needs role-aware input if it is expected to enforce role-scoped rules before dispatch.

### MR-R12 - QSRV anonymous/legacy ACF path no longer matches `METHOD("anonymous")`

Severity: medium.

Evidence:

- In `origin/main`, `AcfAccessControl::level_for` called `check_access_method(&asg, host, user, 0, "anonymous", "")` for the legacy `can_read` / `can_write` path.
- In the branch, `AccessContext::anonymous` sets `method: String::new()` at `crates/epics-bridge-rs/src/qsrv/provider.rs:266`.
- `AccessContext::with_identity` also defaults method to an empty string at `crates/epics-bridge-rs/src/qsrv/provider.rs:278`.
- `AcfAccessControl::can_read` and `can_write` build default `ClientCreds` at `crates/epics-bridge-rs/src/qsrv/provider.rs:211` and `:222`; the default method is empty.
- `AcfAccessControl::level_for_creds` passes that empty method to `check_access_method` at `crates/epics-bridge-rs/src/qsrv/provider.rs:179`.
- `check_access_method` only matches a rule's `METHOD(...)` list by literal method comparison, so empty method does not match `METHOD("anonymous")`.

Impact:

Default/legacy QSRV access contexts that used to behave as anonymous no longer satisfy ACF rules scoped to `METHOD("anonymous")`. A deployment that intentionally grants anonymous read access through a method-scoped rule can lose that access after the branch even though it still uses the anonymous channel creation path.

Why tests missed it:

The new method/authority/roles test passes explicit `ClientCreds` with `method: "ca"` or `method: "x509"`. It does not compare the legacy `create_channel` / `create_channel_for` path against an ACF containing `METHOD("anonymous")`.

Expected fix shape:

If a context is anonymous, store `"anonymous"` as the method. If a context has only user/host and no explicit method, either preserve the old anonymous behavior or require callers to choose a method explicitly instead of silently using an empty method.

### MR-R13 - QSRV GET and monitor initial snapshots ignore INIT pvRequest options

Severity: medium-high.

Evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:2619` retrieves the stored operation tuple as `(intro, mask, init_pv_request)`.
- The GET data branch constructs `ChannelContext` with `pv_request: None` at `crates/epics-pva-rs/src/server_native/tcp.rs:2670`, discarding the decoded INIT pvRequest before `src.get_value_checked(...)` at `:2682`.
- The PUT readback branch (`subcmd & 0x40`) also builds the read context with `pv_request: None` at `crates/epics-pva-rs/src/server_native/tcp.rs:2761` before calling `src.get_value_checked(...)` at `:2773`.
- The MONITOR branch does keep the INIT pvRequest in `mon_ctx` at `crates/epics-pva-rs/src/server_native/tcp.rs:3005` through `:3011`.
- Both monitor initial snapshot paths call `src.get_value_checked(mon_checked.clone(), mon_ctx.clone())` at `crates/epics-pva-rs/src/server_native/tcp.rs:3155` and `:3304`.
- `QsrvPvStore::get_value_checked` receives the context but creates `let empty_request = PvStructure::new("")` at `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:282` and always calls `channel.get(&empty_request)` at `:283`.
- QSRV group GET reads `record._options.atomic` from the request structure at `crates/epics-bridge-rs/src/qsrv/group.rs:847`, and group monitor queue size is negotiated from MONITOR INIT at `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:483`.

Impact:

Native PVA GET and PUT readback cannot make QSRV group reads honor INIT pvRequest options such as `record._options.atomic`. MONITOR initial snapshots can also differ from subsequent monitor events: later group monitor values use the negotiated queue size through `AnyMonitor::with_queue_size`, while the initial snapshot is built through a QSRV GET with an empty request and stamps the default group options.

Why tests missed it:

The existing pvRequest option coverage exercises single-record PUT option routing. It does not drive QSRV group GET or MONITOR initial snapshots through the native wire path with non-default `record._options`.

Expected fix shape:

Forward `init_pv_request` into GET `ChannelContext`, and make `QsrvPvStore::get_value_checked` pass `ctx.pv_request` to `channel.get(...)` when it is a structure. Monitor initial snapshots need to use the same per-operation request options as the subscription path.

### MR-R14 - pvalink shared OUT link cache collapses per-link write options

Severity: high.

Evidence:

- `crates/epics-bridge-rs/src/pvalink/registry.rs:15` defines the registry key as `(pv_name, pipeline, queue_size, direction)`, and the comment explicitly excludes `field` at `:18`.
- `PvaLinkRegistry::get_or_open` constructs the key from only those four fields at `crates/epics-bridge-rs/src/pvalink/registry.rs:75` through `:81`.
- The OUT write path preserves the full query string, registers OUT options, and builds `cfg = self.out_cfg_for(full)` at `crates/epics-bridge-rs/src/pvalink/integration.rs:870` through `:879`.
- That configured link is immediately collapsed through `self.registry.get_or_open(cfg)` at `crates/epics-bridge-rs/src/pvalink/integration.rs:901` through `:904`.
- `PvaLink::write_with_block` then uses the cached link's `self.config.defer`, `self.config.process`, and `self.config.field` at `crates/epics-bridge-rs/src/pvalink/link.rs:318`, `:321`, and `:322` through `:324`.
- `PvaLink::write_pv_field_with_block` uses the cached link's `self.config.defer` and `self.config.process` at `crates/epics-bridge-rs/src/pvalink/link.rs:364` and `:367`.

Impact:

Two OUT links to the same remote PV with different query options are not isolated. The later link silently uses the first opened link's `field`, `proc`, `defer`, and `retry` behavior. That can write the wrong remote field, process a remote record when the link asked not to, skip deferred write semantics, or inherit retry behavior from another record.

Why tests missed it:

The pvalink option tests cover individual link behavior. They do not open two OUT links with the same `(pv_name, pipeline, queue_size, direction)` and different behavior-changing query options, then verify the second link still owns its own options.

Expected fix shape:

Split shared channel/cache state from per-link behavior options, or include every behavior-changing OUT option in the registry key. The safer shape is a per-link handle that stores caller-specific config while sharing only the underlying client/channel monitor state.

### MR-R15 - pvalink alarm/time/metadata getters use shared or default config instead of caller options

Severity: medium-high.

Evidence:

- `crates/epics-bridge-rs/src/pvalink/registry.rs:15` through `:20` excludes `sevr`, `time`, and `field` from the registry key.
- `PvaLinkResolver::link_alarm_severity` strips the query and calls `try_get_any(bare, LinkDirection::Inp)` at `crates/epics-bridge-rs/src/pvalink/integration.rs:471` through `:476`, so it uses whichever cached INP link appears first.
- `LinkSet::alarm_message` and `LinkSet::alarm_severity` strip the query then open `default_inp_cfg(bare)` at `crates/epics-bridge-rs/src/pvalink/integration.rs:919` through `:943`, discarding the caller's `sevr` option.
- `LinkSet::time_stamp` parses the caller's full config at `crates/epics-bridge-rs/src/pvalink/integration.rs:959`, but `get_or_open(cfg)` can still return a previously cached link with a different `time` flag; the gate then reads `link.config().time` at `:968`.
- `LinkSet::link_metadata` strips only the scheme at `crates/epics-bridge-rs/src/pvalink/integration.rs:982`, opens `default_inp_cfg(name)` at `:985`, and calls `link.link_metadata()`.
- `PvaLink::link_alarm_severity` gates on `self.config.sevr` at `crates/epics-bridge-rs/src/pvalink/link.rs:522` through `:525`; `PvaLink::link_metadata` extracts DBF type and element count from `self.config.field` at `crates/epics-bridge-rs/src/pvalink/link.rs:625` through `:631`.

Impact:

Two INP links to the same remote PV with different `sevr`, `time`, or `field` options can report the wrong alarm propagation, timestamp adoption, DBF type, or element count. If `link_metadata` is called with a query-bearing name before the intended link is open, it can also attempt to open a remote PV whose name includes the query string.

Why tests missed it:

The current tests validate metadata/alarm/time getters against a single configured link. They do not cover same-PV option collisions or query-bearing metadata lookup before an existing cached link is present.

Expected fix shape:

Getter paths must parse the caller's full link config and apply `sevr`, `time`, and `field` from that caller-specific config, not from an arbitrary shared `PvaLink`. `link_metadata` should strip the query from the remote PV name and use `inp_cfg_for(full)`.

### MR-R16 - PVA gateway identity pools are keyed without host or authority

Severity: high.

Evidence:

- `GatewayChannelSource::upstream_client_for` keys the upstream client pool by `(ctx.account.clone(), ctx.method.clone())` at `crates/epics-bridge-rs/src/pva_gateway/source.rs:339`.
- The same function builds the upstream client with `ctx.host` at `crates/epics-bridge-rs/src/pva_gateway/source.rs:363` through `:365` and records `ctx.authority` in the asserted identity at `:366` through `:369`.
- `GatewayChannelSource::upstream_cache_for` uses the same `(account, method)` key at `crates/epics-bridge-rs/src/pva_gateway/source.rs:384`, then builds the cache from `upstream_client_for(ctx)` at `:390`.
- `PvaClient::with_asserted_identity` stores the asserted `user`, `host`, and downstream method/authority at `crates/epics-pva-rs/src/client_native/context.rs:331` through `:346`.
- The branch's tests check that different accounts/methods get different caches at `crates/epics-bridge-rs/src/pva_gateway/source.rs:1561`, but they do not vary `host` or `authority` under the same account/method key.

Impact:

Two downstream peers with the same account and auth method but different host names or x509 certificate authorities can share the first peer's upstream PVA client and ChannelCache. The upstream IOC then sees stale host/asserted-authority data for audit and access checks, and cached GET/MONITOR state can be reused under the wrong upstream identity.

Why tests missed it:

`br_r21_gateway_monitor_credential_scoping` varies account and method, not same account/method with different host or authority. The x509 audit test only checks a single client construction, not key collision.

Expected fix shape:

Key `upstream_pool` and `upstream_caches` by every identity field that affects upstream credentials or ACF behavior: account, method, host, authority, and roles once roles are present in `ChannelContext`.

### MR-R17 - CA client write timeout keeps a possibly desynchronized TCP circuit and drops queued frames

Severity: high.

Evidence:

- In `origin/main`, the CA client write loop treated `Ok(Err(_)) | Err(_)` from `timeout(send_timeout, writer.write_all(&batch))` as `TcpClosed` and returned at old `crates/epics-ca-rs/src/client/transport.rs:827` through `:829`.
- The branch timeout arm at `crates/epics-ca-rs/src/client/transport.rs:903` through `:918` sends `CircuitUnresponsive`, clears `batch`, and continues using the same socket.
- The comment at `crates/epics-ca-rs/src/client/transport.rs:911` through `:913` says partial-write state is unrecoverable, but the code neither closes the stream nor preserves the pending frames for retry on a new circuit.
- `writer.write_all(&batch)` may have written a prefix of a CA frame before the timeout future is cancelled; continuing to write later batches on the same TCP stream can leave the server parsing a truncated or concatenated message stream.
- The same timeout arm also skips the `pending_frames` decrement. `send_frame` increments the counter before enqueue at `crates/epics-ca-rs/src/client/transport.rs:621` through `:623`, and the writer only subtracts the drained count on the success path at `:883` through `:895`. The timeout path at `:914` through `:917` drops the batch without subtracting `drained`, leaving the backpressure counter inflated for frames that will never be written.

Impact:

On a write-side timeout, the client can silently drop queued commands while leaving callers waiting for replies that will never arrive. If any bytes were written before cancellation, keeping the TCP circuit alive risks protocol desynchronization with the server parser. The stale pending-frame count can also make later sends hit the `SEND_BACKPRESSURE_FRAMES` close path after frames have already been discarded. `origin/main` closed the circuit on this condition, which forced a clean reconnect path.

Why tests missed it:

The transport tests cover malformed reads and echo watchdog behavior. There is no write-side timeout test with a writer that accepts a partial frame, stalls, then observes whether the circuit closes or later writes are appended to the same stream.

Expected fix shape:

On write timeout, close the circuit and let the reconnect path rebuild a clean stream, or prove and enforce that the timed-out write is recoverable without partial bytes. If the socket is kept alive, pending frames must not be dropped without surfacing a deterministic failure to the affected operations.

### MR-R18 - PVA `pvget_many` drops the warm GET cache and leaks reusable IOIDs on every warm request

Severity: high.

Evidence:

- `PvaClient::pvget_many` initializes every result slot to `Err(PvaError::Timeout)` at `crates/epics-pva-rs/src/client_native/context.rs:1230`.
- For warm-cache requests, it takes the cached GET state at `crates/epics-pva-rs/src/client_native/context.rs:1282`, installs a fresh oneshot sender into `warm.slot` at `:1304` through `:1306`, queues the GET frame, and pushes a `WarmReq` at `:1320`.
- Phase 3 then checks `if results[idx].is_err()` at `crates/epics-pva-rs/src/client_native/context.rs:1384` before awaiting the oneshot. Because the slot was initialized to `Err(Timeout)` and never marked pending, this branch is taken for every warm request, not just Phase-2 send failures.
- In `origin/main`, the same erroneous skip existed, but it restored `*channel.cached_get.lock() = Some(warm)` before continuing at old `crates/epics-pva-rs/src/client_native/context.rs:1214` through `:1216`.
- The branch removed that restore at `crates/epics-pva-rs/src/client_native/context.rs:1384` through `:1385`. The installed reusable IOID is also not destroyed or unregistered on this skip path.

Impact:

After the first cold `pvget_many` warms a channel, the next `pvget_many` sends warm GET frames but skips every response wait, returns the initial timeout error, drops the channel's warm cache, and leaves the reusable IOID registered in the client/server operation maps. Repeated calls fall back to cold GETs while abandoned reusable IOIDs can accumulate until the TCP circuit is closed.

Why tests missed it:

There is no `pvget_many` regression test that calls the function twice against the same PV set and asserts the second call returns successful warm results while preserving or cleaning reusable IOID state.

Expected fix shape:

Track Phase-2 send failures separately from the result vector, or initialize warm slots to an explicit pending state. Phase 3 should await every successfully sent warm request. On any skipped or failed warm request, clear the oneshot slot, unregister/destroy the IOID, and only restore `cached_get` after a successful DATA response.

### MR-R19 - PVA `CMD_DESTROY_CHANNEL` cleanup leaks the new IOID command map

Severity: medium.

Evidence:

- The branch adds `ioid_to_cmd` to `ServerConn` so routed frames can be checked against the command that opened each IOID. Registrations insert it at `crates/epics-pva-rs/src/client_native/server_conn.rs:686`, `:700`, and `:720`.
- The normal `unregister_ioid` path removes all three maps at `crates/epics-pva-rs/src/client_native/server_conn.rs:724` through `:727`: `by_ioid`, `ioid_to_sid`, and `ioid_to_cmd`.
- The server-initiated `CMD_DESTROY_CHANNEL` path collects every IOID for the destroyed SID at `crates/epics-pva-rs/src/client_native/server_conn.rs:904` through `:909`, then removes only `ioid_to_sid` and `by_ioid` at `:910` through `:911`. It never removes `ioid_to_cmd`.
- The command-mismatch check runs before the `by_ioid` lookup at `crates/epics-pva-rs/src/client_native/server_conn.rs:958` through `:976`, so stale `ioid_to_cmd` entries remain active even after the corresponding dispatch slot was removed.
- The regression test `destroy_channel_drops_associated_ioid_streams` asserts `by_ioid` and `ioid_to_sid` cleanup at `crates/epics-pva-rs/src/client_native/server_conn.rs:1375` through `:1378`, but it does not assert that `ioid_to_cmd` was cleared.

Impact:

After a server destroys a channel, every operation IOID that belonged to that SID leaves a stale command expectation behind for the lifetime of the TCP connection. That is a memory leak on repeated server-side channel destroy/recreate cycles. It can also make a late frame on an already-destroyed IOID cancel the whole connection if its command differs from the stale expected command, because the new mismatch gate fires before discovering that no dispatch slot exists.

Why tests missed it:

The destroy-channel cleanup test was updated for the old two-map invariant only. It never checks `ioid_to_cmd`, and there is no test that sends a late wrong-command frame after `CMD_DESTROY_CHANNEL`.

Expected fix shape:

Treat `CMD_DESTROY_CHANNEL` cleanup as the same owner boundary as `unregister_ioid`: remove `ioid_to_cmd` for every matching IOID in the destroy branch, and add a regression assertion that all three maps are cleared for a destroyed SID.

### MR-R20 - CA TRAPWRITE dispatch marks every write as trapped

Severity: high.

Evidence:

- The ACF parser still captures the `TRAPWRITE` / `NOTRAPWRITE` rule option into `AccessRule::trap` at `crates/epics-base-rs/src/server/access_security.rs:356` through `:362` and `:1386` through `:1395`.
- The new listener message documents `rule_was_trap` as "true iff" the matched rule carried `TRAPWRITE` at `crates/epics-base-rs/src/server/access_security.rs:645` through `:649`.
- `AccessChecked` contains only `pv_name` and `level` at `crates/epics-base-rs/src/server/access_security.rs:28` through `:34`; `AccessGate::check` gets only an `AccessLevel` from `check_access_method` at `:246` through `:264`. The matched rule's trap flag is not returned from the ACF evaluation.
- The CA TCP write path hard-codes `rule_was_trap: true` for `BeforeWrite`, synchronous `AfterWrite`, and async `AfterWrite` at `crates/epics-ca-rs/src/server/tcp.rs:2568`, `:2618`, and `:2738`.
- A workspace search for `rule_was_trap` finds no production path that sets it from `AccessRule::trap`; it is only documented in `access_security.rs` and hard-coded in the CA dispatcher.

Impact:

Registering a trap-write listener makes every accepted CA write look as if it matched a `TRAPWRITE` rule. ACF rules with `NOTRAPWRITE`, rules with no trap option, and writes granted by untrapped fallback rules cannot be distinguished by the listener even though the public message field claims they can. That can over-log values that an operator deliberately excluded from put logging, and it makes `caPutLog`-style filtering impossible to implement faithfully on this branch.

Why tests missed it:

The parser test only asserts that `AccessRule::trap` is parsed. There is no end-to-end CA write test with a registered listener and an ACF containing both `TRAPWRITE` and `NOTRAPWRITE` rules.

Expected fix shape:

The ACF check that authorizes a write must return both the resolved access level and the trap mask of the rule that raised access. Thread that result through the CA write path and set `TrapWriteMessage::rule_was_trap` from it. Add tests for `TRAPWRITE`, `NOTRAPWRITE`, and omitted trap options.

### MR-R21 - Native PVA `DBF_UINT64` PUT path truncates `ulong` before database conversion

Severity: high.

Evidence:

- The branch maps database `EpicsValue::UInt64` and `UInt64Array` to native PVA `ScalarValue::ULong` on reads at `crates/epics-pva-rs/src/server/native_source.rs:123` and `:149`.
- The same source advertises those fields as PVA `ScalarType::ULong` / `ulong[]` at `crates/epics-pva-rs/src/server/native_source.rs:183` and `:192`.
- The native PUT path extracts `value`, calls `pv_field_to_epics`, and only then calls `db.put_pv(...)` at `crates/epics-pva-rs/src/server/native_source.rs:473` through `:489`.
- `pv_field_to_epics` maps scalar values through the untyped `scalar_to_epics` at `crates/epics-pva-rs/src/server/native_source.rs:549` through `:551`; that helper maps `ScalarValue::ULong(x)` to `EpicsValue::Long(*x as i32)` at `:595` through `:605`.
- The array branch handles `Double`, `Int`, `Long`, and `Float` arrays only, then returns `None` for `ScalarValue::ULong` arrays at `crates/epics-pva-rs/src/server/native_source.rs:552` through `:590`.
- Database field-type coercion happens after this collapse: `PvDatabase::put_pv` looks up the target field type and calls `value.convert_to(target)` at `crates/epics-base-rs/src/server/database/field_io.rs:100` through `:120`. At that point the upper 32 bits of a scalar `ulong` have already been discarded.

Impact:

The branch now presents native database `DBF_UINT64` values as PVA `ulong`, but a client writing the same PVA type back through the native server cannot round-trip it. Scalar `ulong` values above the signed 32-bit range are first wrapped into `EpicsValue::Long` and then coerced to the target field; `ulong[]` writes are rejected as "PUT value not representable as EpicsValue". This breaks the newly added `DBF_UINT64` / PVA `ulong` parity on the direct native PVA server path.

Why tests missed it:

The UInt64 tests cover the bridge conversion helpers and QSRV descriptor/read behavior. They do not write a scalar `ulong` or `ulong[]` through `NativePvSource::put_value` into a `DBF_UINT64` record field or waveform.

Expected fix shape:

The native source PUT conversion must preserve unsigned 64-bit values before database coercion. Either make `pv_field_to_epics` produce `EpicsValue::UInt64` / `UInt64Array` for `ScalarValue::ULong`, or make the conversion target-aware so it can use the destination DBF type before narrowing.

### MR-R22 - QSRV single-record scalar UInt64 PUT loses precision before typed conversion

Severity: high.

Evidence:

- The branch maps `DbFieldType::UInt64` to PVA `ScalarType::ULong` at `crates/epics-bridge-rs/src/convert.rs:19` through `:20`.
- The new typed conversion helper can preserve a scalar `ScalarValue::ULong` when it is called directly with a UInt64 target at `crates/epics-bridge-rs/src/convert.rs:80` through `:87` and `:145` through `:151`.
- `BridgeChannel::put_with_options` does not call that helper on the original PVA scalar. It first extracts `raw_val = pv_structure_to_epics(value)` at `crates/epics-bridge-rs/src/qsrv/channel.rs:367` through `:371`, then converts that `EpicsValue` back to a scalar before calling `scalar_to_epics_typed` at `:373` through `:384`.
- `pv_structure_to_epics` handles an NTScalar `value` scalar by calling the context-free `crate::convert::scalar_to_epics(sv)` at `crates/epics-bridge-rs/src/qsrv/pvif.rs:350` through `:354`.
- That context-free helper maps `ScalarValue::ULong(v)` to `EpicsValue::Double(*v as f64)` at `crates/epics-bridge-rs/src/convert.rs:52` through `:70`.
- The array path is not the same defect: `pv_field_to_epics` preserves `ScalarValue::ULong` arrays as `EpicsValue::UInt64Array` at `crates/epics-bridge-rs/src/convert.rs:271` through `:274`. QSRV group scalar PUT is also not the same path because it calls `scalar_to_epics_typed` on the original scalar at `crates/epics-bridge-rs/src/qsrv/group.rs:821` through `:823`.

Impact:

A native PVA client writing a scalar `ulong` to a single-record QSRV channel backed by `DBF_UINT64` can lose integer precision before the target-aware conversion runs. Values above the exact integer range of `f64` no longer round-trip as the submitted `u64`, even though the branch's tests and comments claim `ScalarValue::ULong -> EpicsValue::UInt64` preserves the full range.

Why tests missed it:

The UInt64 conversion test calls `scalar_to_epics_typed(&ScalarValue::ULong(...), DbFieldType::UInt64)` directly and tests the array path. It does not send an NTScalar structure through `pv_structure_to_epics` and `BridgeChannel::put_with_options` for a scalar `DBF_UINT64` field.

Expected fix shape:

Single-record QSRV PUT needs a target-aware extraction path, for example `pv_structure_to_epics_typed(value, self.value_dbf)`, so scalar conversion sees the original `ScalarValue::ULong`. Alternatively, the context-free scalar fallback should preserve `ULong` as `EpicsValue::UInt64` now that that variant exists, and tests should cover the full `put_with_options` path.

### MR-R23 - pvalink OUT skips the new `UInt64Array` typed path

Severity: high.

Evidence:

- The branch adds `EpicsValue::UInt64Array(Vec<u64>)` for `DBF_UINT64` waveforms at `crates/epics-base-rs/src/types/value.rs:31` through `:33`.
- The bridge converter can encode that value correctly as PVA `ulong[]` if called: `epics_to_pv_field` maps `EpicsValue::UInt64Array` to `ScalarValue::ULong` elements at `crates/epics-bridge-rs/src/convert.rs:215` through `:216`.
- The pvalink OUT dispatcher decides whether to use the typed `PvField` path via a hard-coded `array_path` match at `crates/epics-bridge-rs/src/pvalink/integration.rs:889` through `:898`. That list includes the older array variants but not `EpicsValue::UInt64Array`.
- When `array_path` is false, pvalink falls back to `value.to_string()` and `link.write(&value_str)` at `crates/epics-bridge-rs/src/pvalink/integration.rs:912` through `:913`.
- `Display` renders `UInt64Array` as a bracketed string such as `[1, 2]` at `crates/epics-base-rs/src/types/value.rs:77` through `:79`.
- The PVA string PUT array parser does not parse bracket syntax. It splits only on commas and feeds each trimmed token to `ScalarValue::parse` at `crates/epics-pva-rs/src/client_native/ops_v2.rs:2369` through `:2377`; `ScalarValue::parse(ScalarType::ULong, ...)` uses plain `u64` parsing at `crates/epics-pva-rs/src/pvdata/scalar.rs:181` through `:182`, so tokens like `[1` or `2]` are invalid.
- `origin/main` had no `UInt64Array` variant. The branch introduced the value representation but did not add it to the pvalink OUT typed-array gate.

Impact:

An OUT pvalink from a local `FTVL=UINT64` waveform can now produce `EpicsValue::UInt64Array`, but the outbound path sends it through the string parser instead of the existing typed `ulong[]` encoder. Large unsigned 64-bit arrays either fail the PUT because of the bracketed display string or avoid the code path that preserves full `u64` element width.

Why tests missed it:

The UInt64 tests cover the bridge conversion helper directly and waveform storage. They do not exercise `PvaLinkRegistry::put_value` with `EpicsValue::UInt64Array`.

Expected fix shape:

Add `EpicsValue::UInt64Array(_)` to the pvalink typed-array predicate, preferably via a single helper that classifies every `EpicsValue` array variant so future array variants cannot miss this gate.

### MR-R24 - `PvDatabaseSource` advertises UInt64 over PVA but truncates UInt64 PUTs on the way back

Severity: high.

Evidence:

- This branch adds `EpicsValue::UInt64` and `EpicsValue::UInt64Array` in `crates/epics-base-rs/src/types/value.rs:22` and `:33`.
- The generic PVA database source now advertises those values as native PVA unsigned 64-bit values: scalar `UInt64` becomes `ScalarValue::ULong` at `crates/epics-pva-rs/src/server/native_source.rs:123`, and `UInt64Array` becomes `ScalarValue::ULong` elements at `:148` through `:150`.
- Its descriptor path also exposes scalar and array UInt64 as `ScalarType::ULong` at `crates/epics-pva-rs/src/server/native_source.rs:183` and `:192`.
- The same source's PUT path extracts `value` and converts through the local `pv_field_to_epics` helper at `crates/epics-pva-rs/src/server/native_source.rs:476` through `:488`.
- That helper still maps scalar `ScalarValue::ULong(v)` to `EpicsValue::Long(*v as i32)` at `crates/epics-pva-rs/src/server/native_source.rs:595` through `:605`.
- The array branch accepts `Double`, `Int`, `Long`, and `Float` arrays, but has no `ScalarValue::ULong` arm, so a PVA `ulong[]` PUT falls through to `None` at `crates/epics-pva-rs/src/server/native_source.rs:549` through `:590`.
- The bridge conversion module already added a UInt64-aware typed conversion path for QSRV (`DbFieldType::UInt64 => EpicsValue::UInt64(...)`) at `crates/epics-bridge-rs/src/convert.rs:80` through `:91`, so this is not a protocol limitation; it is a missed generic-source reverse conversion.

Impact:

A PV served by `PvDatabaseSource` can be read as PVA `ulong` / `ulong[]`, then a client writing the same PVA type back either loses the upper 32 bits on scalar PUT or gets "PUT value not representable as EpicsValue" for arrays. This breaks round-trip semantics for the new DBF_UINT64 support outside the QSRV bridge-specific path.

Why tests missed it:

The branch tests cover QSRV/bridge UInt64 conversion helpers and PVA read-side UInt64 advertisement. They do not issue a PVA PUT through `PvDatabaseSource::put_value` with a scalar `ULong` or `ulong[]` value.

Expected fix shape:

Make `crates/epics-pva-rs/src/server/native_source.rs` use a UInt64-preserving conversion for `ScalarValue::ULong` and `ulong[]`. If the destination DBF type is available, route through a typed converter; otherwise preserve the wire type as `EpicsValue::UInt64` / `UInt64Array` instead of narrowing to `Long`.

### MR-R25 - `arr` channel filter passes new `UInt64Array` waveforms through unsliced

Severity: medium-high.

Evidence:

- This branch adds UINT64 waveform storage: `WaveformRecord::new(..., DbFieldType::UInt64)` creates `EpicsValue::UInt64Array` at `crates/epics-base-rs/src/server/records/waveform.rs:119`, runtime `FTVL=8` allocation creates `UInt64Array` at `:144`, and VAL writes preserve `UInt64Array` at `:413` through `:416`.
- The `arr` filter module documents that it operates on `*Array` `EpicsValue` variants and that the resulting slice carries the same array variant as the input at `crates/epics-base-rs/src/server/database/filters/arr.rs:1` through `:17`.
- `ArrayFilter::apply` matches `ShortArray`, `LongArray`, `Int64Array`, `FloatArray`, `DoubleArray`, `EnumArray`, `CharArray`, and `StringArray` at `crates/epics-base-rs/src/server/database/filters/arr.rs:68` through `:80`.
- That match has no `EpicsValue::UInt64Array` arm, so UINT64 waveforms hit the `other => other` scalar passthrough path at `crates/epics-base-rs/src/server/database/filters/arr.rs:79` instead of `slice_with(...)`.
- `origin/main` had no `UInt64Array` variant. The branch introduced the new array storage type but did not close the filter match that claims to handle array variants.
- `parse_filter_chain` builds `ArrayFilter` for the `arr` JSON key at `crates/epics-base-rs/src/server/database/filters/parser.rs:198` through `:203`, so the missed arm affects real CA/PVA channel-filter requests, not only a helper.

Impact:

A client subscribing to a UINT64 waveform with an `arr` filter such as `{"arr":{"s":1,"e":2}}` receives the full waveform instead of the requested slice. Other waveform element types are sliced, so this is a type-specific regression introduced by the new DBF_UINT64 array representation.

Why tests missed it:

The `arr` filter tests exercise `DoubleArray` and scalar passthrough. The new UINT64 waveform tests verify allocation, field type, and VAL preservation, but they do not run a `UInt64Array` snapshot through `ArrayFilter`.

Expected fix shape:

Add `EpicsValue::UInt64Array(v) => EpicsValue::UInt64Array(slice_with(v, cfg))` to the `arr` filter match and add a test using values above `i64::MAX` so the slice keeps both the element type and the full unsigned range.

## Pre-Existing Defects Observed During This Review

### EX-R1 - PVA monitor pipeline credit is consumed before pause/filter suppression

Severity: medium-high.

Evidence:

- Current branch: `crates/epics-pva-rs/src/server_native/tcp.rs:3407` waits for and decrements the pipeline window before checking `monitor_paused` at `:3452` or applying filters at `:3458`.
- `origin/main` has the same ordering: pipeline window at old `tcp.rs:2766`, pause at old `:2807`, filter at old `:2814`.

Impact:

Pipeline credit is supposed to account for frames emitted to the client. Here, paused or filter-dropped events can consume credit without sending a monitor frame. A client with a finite pipeline window can stall waiting to ACK frames it never received.

Expected fix shape:

Apply pause/filter decisions before pipeline credit is decremented. Only events that will produce a monitor DATA frame should consume one credit.

### EX-R2 - `EPICS_CA_MAX_SEARCH_PERIOD` comment and implementation disagree with C parity

Severity: medium.

Evidence:

- `crates/epics-ca-rs/src/client/search.rs:152` says C defaults to 300 seconds and clamps the lower bound to 60 seconds.
- `crates/epics-ca-rs/src/client/search.rs:164` parses the env var, then `:167` defaults to `30.0` and `:168` clamps to `30.0`.
- `crates/epics-ca-rs/src/client/search.rs:173` preserves the 1-second tick for unset or <=30-second period.

Impact:

Unset env keeps the Rust historical 30-second cap, not the documented C 300-second default. A configured value such as `45` is accepted as 45 seconds instead of being clamped to C's 60-second lower bound. This is a behavior gap hidden behind a comment that claims the C semantics.

Expected fix shape:

Either implement the documented C default/lower-bound semantics, or change the comment and tests to state that Rust intentionally preserves the old default and only partially honors the env var.

### EX-R3 - Default PVA delta PUT merge reads prior value through ctx-less `get_value`

Severity: high.

Evidence:

- `ChannelSource::put_delta_checked` has the same default implementation in the branch and `origin/main`: current `crates/epics-pva-rs/src/server_native/source.rs:280` through `:303`, old `crates/epics-pva-rs/src/server_native/source.rs:211` through `:234`.
- The default merge reads the prior value via `self.get_value(checked.pv_name()).await` at current `crates/epics-pva-rs/src/server_native/source.rs:296`, not via `get_value_checked` with the authenticated `ChannelContext`.
- The native PVA PUT data path calls `src.put_delta_checked(...)` at `crates/epics-pva-rs/src/server_native/tcp.rs:2870`.
- `QsrvPvStore` overrides `get_value_checked` and `put_value_checked`, but does not override `put_delta_checked` in `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs`.
- QSRV's credentialed read path creates the channel with `create_channel_with_creds(&name, ctx_to_creds(&ctx))` at `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:278` through `:280`; the ctx-less default cannot use the same identity.

Impact:

An authenticated sparse PUT can be allowed to write while its prior-value merge is read through an anonymous/default context. If that ctx-less read is denied or resolves differently, the default falls back to `None => delta`, treating the sparse data-phase value as a full value and replacing unmarked leaves with type defaults.

Why tests missed it:

The delta PUT tests cover atomic merge behavior for `SharedPV` and generic server paths. They do not combine access-controlled sources, credential-dependent reads, and sparse delta PUTs through a source that relies on the trait default.

Expected fix shape:

The trait default should not perform an identity-sensitive prior read through ctx-less `get_value`. Either merge with `get_value_checked` under the same credentials, or require sources with access control or non-atomic backing stores to override `put_delta_checked` and merge under their own credentialed owner path.

### EX-R4 - PVA `pvget_many` warm path skips all warm responses because initial result slots are errors

Severity: high.

Evidence:

- `origin/main` initialized every `pvget_many` result slot to `Err(PvaError::Timeout)` at old `crates/epics-pva-rs/src/client_native/context.rs:1073`.
- The warm path sent batched GET frames, then Phase 3 used `if results[idx].is_err()` at old `crates/epics-pva-rs/src/client_native/context.rs:1214` to decide whether the server send had failed.
- Because a warm request's result slot was still the initial timeout error, the branch skipped the response await for every warm request and returned timeout errors even when the server replied.
- The branch regression documented in MR-R18 makes this worse by dropping the cache and leaving reusable IOIDs registered, but the bad sentinel logic itself existed before this branch.

Impact:

Bulk GETs can succeed on the first cold call but fail on the warm-cache path that the API is intended to optimize. The function sends valid warm GET frames and then ignores its own response receivers.

Expected fix shape:

Use a separate `failed_warm: HashSet<usize>` / `Vec<bool>` for Phase-2 send failures, or represent result state as `Pending | Ready(Result<...>)`. Never use the initial returned-error placeholder as an internal failure marker before the warm response has been awaited.

### EX-R5 - PVA `PUT_GET` and `PROCESS` data phases do not verify that the IOID was initialized for that command

Severity: high.

Evidence:

- The branch's generic GET/PUT/MONITOR/RPC handler now rejects a data-phase command whose kind does not match the op stored at INIT: `crates/epics-pva-rs/src/server_native/tcp.rs:2631` through `:2637`.
- The dedicated `handle_put_get` data phase only checks that an op exists, then extracts `(o.intro, o.mask)` at `crates/epics-pva-rs/src/server_native/tcp.rs:1682` through `:1688`; it never checks `o.kind == OpKind::PutGet`.
- The dedicated `handle_process` data phase only checks `ch.ops.contains_key(&ioid)` at `crates/epics-pva-rs/src/server_native/tcp.rs:1880`, then runs `src.process_checked(...)` at `:1913`; it never checks `o.kind == OpKind::Process`.
- `origin/main` has the same shape in the same two handlers: old `handle_put_get` gets an op and proceeds at old `crates/epics-pva-rs/src/server_native/tcp.rs:1396` through `:1404`, and old `handle_process` uses only `contains_key` at old `:1588` through `:1596`.
- Existing protocol tests cover valid `PUT_GET`/`PROCESS` flows and the generic wrong-kind guard, but there is no test that initializes a GET/MONITOR/PUT IOID and then sends `Command::PutGet` or `Command::Process` data for that same IOID.

Impact:

A malformed client can initialize an IOID as one operation class and then drive a different dedicated command through it. `PROCESS` can trigger record processing against any live IOID on the channel. `PUT_GET` can perform a write/readback using the descriptor and mask from a non-`PUT_GET` op, without the operation ever having negotiated the `PUT_GET` lifecycle. The WRITE gate still runs during the data phase, but the server's operation-state invariant is open.

Expected fix shape:

Both dedicated handlers should fetch the stored `OpState` and reject the data phase unless `op.kind` matches the command, using the same protocol-error policy as the generic handler. Add regression tests for GET-then-PROCESS-data, MONITOR-then-PROCESS-data, GET-then-PUT_GET-data, and PUT-then-PUT_GET-data.

### EX-R6 - CA no-read-access monitor replies can be encoded as zero-payload cancel acks

Severity: medium-high.

Evidence:

- Current `EVENT_ADD` denied-read handling stores the raw request count as `requested_count = hdr.actual_count()` at `crates/epics-ca-rs/src/server/tcp.rs:2791`.
- For a denied `SimplePv` or record-field subscription, it calls `send_no_read_access_event(..., requested_count, ...)` at `crates/epics-ca-rs/src/server/tcp.rs:2949` through `:2956` and `:3071` through `:3078`.
- `send_no_read_access_event` sizes the zero-filled payload with `dbr_buffer_size(data_type, native, count)` at `crates/epics-ca-rs/src/server/tcp.rs:3929` through `:3934`.
- For a normal autosize monitor request (`count == 0`) on a plain DBR type such as `DBR_DOUBLE`, `dbr_buffer_size(..., count=0)` returns zero value bytes, so the helper emits a zero-payload `CA_PROTO_EVENT_ADD` carrying `ECA_NORDACCESS`.
- The current CA client deliberately treats zero-payload `CA_PROTO_EVENT_ADD` as an `EVENT_CANCEL` acknowledgement and drops it before looking at status: `crates/epics-ca-rs/src/client/transport.rs:1450` through `:1453`.
- `origin/main` also failed this denied-monitor notification by using a zero-payload `send_cmd_error` for `EVENT_ADD` read denial at old `crates/epics-ca-rs/src/server/tcp.rs:2400` through `:2410`; the branch's new no-read-access helper keeps the same failure when the request used autosize count.

Impact:

A client subscribing with `count=0` while read access is denied can receive no visible denial callback for scalar/plain DBR monitors. The server installs the disabled subscription, but the initial `ECA_NORDACCESS` frame is indistinguishable from the zero-payload cancel-ack shape on the client side and is silently discarded. If access is never restored, the monitor appears to hang instead of surfacing the denied-read status.

Expected fix shape:

Normalize autosize monitor counts before building no-read-access frames. For `count=0`, derive the target's actual element count and use that count to size the zero-filled payload, so the frame has a nonzero DBR body and the client status-error path runs. Add a test for denied `EVENT_ADD` with `count=0` on a scalar/plain DBR type.

### EX-R7 - PVA server keeps unadvertised auth credentials after returning status error

Severity: high.

Evidence:

- The server advertises only `"anonymous"` and `"ca"` on plain TCP at `crates/epics-pva-rs/src/server_native/tcp.rs:1017`.
- On a plain-TCP `CONNECTION_VALIDATION` reply, the branch installs the parsed client claim into `cred` before checking whether that method was advertised: `crates/epics-pva-rs/src/server_native/tcp.rs:1247` through `:1249`.
- The unadvertised-method check then returns `Status::Error("Client selects unadvertised auth")` at `crates/epics-pva-rs/src/server_native/tcp.rs:1270` through `:1283`, but the connection is still marked complete at `:1296`.
- The `auth_complete` hook receives the same `cred` after the status error at `crates/epics-pva-rs/src/server_native/tcp.rs:1302` through `:1304`, and later channel-operation access checks use the connection's `cred.account`, `cred.method`, and `cred.authority`.
- `origin/main` had the same ordering: it assigned `cred = parse_client_credentials(...).unwrap_or(cred)` at old `crates/epics-pva-rs/src/server_native/tcp.rs:1009`, computed the unadvertised-method error at old `:1030` through `:1043`, then completed the handshake and fired `auth_complete` at old `:1056` through `:1064`.
- The existing `auth_method_unadvertised_returns_status_error` test checks only the returned `CONNECTION_VALIDATED` status. It does not assert that the connection identity was reverted to anonymous after the error.

Impact:

A client can select an unadvertised method such as `x509` on plain TCP, include an auth body with `user = "alice"`, receive a status error, and still leave the server-side connection credential as that unadvertised claim. Legacy ACF rules without `METHOD(...)` clauses can then match the claimed account because the later access checks ignore method when the rule has no method scope. Even when access is denied, diagnostics and `auth_complete` report an identity the server just rejected.

Expected fix shape:

Validate the selected auth method before committing the parsed credential to the connection, or revert `cred` to anonymous/default when the advertised-method check fails. Add a regression test that sends an unadvertised method with a non-empty user, observes `Status::Error`, and then verifies `auth_complete` and an ACF-gated operation see anonymous credentials.

### EX-R8 - pvalink INP conversion truncates remote `ulong` values before local record coercion

Severity: high.

Evidence:

- The pvalink resolver converts remote cached/read `PvField` values through `pvfield_to_epics_value` on the fast and slow INP paths at `crates/epics-bridge-rs/src/pvalink/integration.rs:554`, `:582`, `:847`, and `:863`.
- `pvfield_to_epics_value` maps scalar values through the untyped `scalar_to_epics` at `crates/epics-bridge-rs/src/pvalink/integration.rs:1081` through `:1083`.
- That helper maps `ScalarValue::ULong(v)` to `EpicsValue::Long(*v as i32)` at `crates/epics-bridge-rs/src/pvalink/integration.rs:1268` through `:1277`.
- The scalar-array branch maps both `UInt` and `ULong` arrays to `EpicsValue::LongArray`, with each `ULong` element cast to `i32`, at `crates/epics-bridge-rs/src/pvalink/integration.rs:1170` through `:1177`.
- `origin/main` had the same pvalink scalar collapse at old `crates/epics-bridge-rs/src/pvalink/integration.rs:772` through `:830`, so the truncating conversion itself predates this branch.
- The branch makes the gap more visible by adding pvalink metadata mapping from remote `ScalarValue::ULong` and `ScalarType::ULong` to `LinkDbfType::UInt64` at `crates/epics-bridge-rs/src/pvalink/link.rs:879` and `:902`.

Impact:

A remote PVA `ulong` or `ulong[]` source feeding a pvalink INP/read path loses its upper 32 bits before the local database can coerce the value to the destination field type. With the branch's new `DBF_UINT64` metadata support, a link can now identify the remote field as UInt64 while the value path still delivers a signed 32-bit `Long` / `LongArray`.

Expected fix shape:

pvalink value conversion needs the same typed conversion contract as QSRV: preserve `ScalarValue::ULong` as `EpicsValue::UInt64`, preserve `ulong[]` as `UInt64Array`, or pass the destination DBF type into the conversion before collapsing the PVA scalar.

### EX-R9 - CA monitor initial and access-restore events do not pad over-requested element counts

Severity: medium.

Evidence:

- The current READ path uses `pad_dbr_to_requested_count(...)` at `crates/epics-ca-rs/src/server/tcp.rs:2195`, so a request count larger than the live element count is framed with the requested count and zero-filled payload.
- Steady-state record-field monitor delivery uses the same helper at `crates/epics-ca-rs/src/server/tcp.rs:3169` through `:3174`.
- The initial `EVENT_ADD` path does not use that helper. For simple PVs it only truncates when `requested_count > 0 && requested_count < snap.value.count()` at `crates/epics-ca-rs/src/server/tcp.rs:2972` through `:2974`, then calls `send_monitor_snapshot(...)` at `:2975`.
- The record-field initial event has the same shape at `crates/epics-ca-rs/src/server/tcp.rs:3084` through `:3087`.
- The access-restore path also only truncates when `data_count` is smaller than the live value at `crates/epics-ca-rs/src/server/tcp.rs:3851` through `:3854`.
- `send_monitor_snapshot` has no requested-count parameter and always derives the header count from `snapshot.value.count()` at `crates/epics-ca-rs/src/server/tcp.rs:3664` through `:3683`.
- `origin/main` had the same initial monitor shape: it sent the simple-PV initial snapshot via `send_monitor_snapshot(...)` at old `crates/epics-ca-rs/src/server/tcp.rs:2463` through `:2465`, the record-field initial snapshot at old `:2535` through `:2547`, and the helper derived count from `snapshot.value.count()` at old `:3057` through `:3076`.

Impact:

For `CA_PROTO_EVENT_ADD` with an explicit count greater than the current element count, the first monitor frame and the frame emitted when read access is restored are shorter than the request shape. Later update frames can use the requested count because the producer path pads them, so clients can see a count/size discontinuity within one subscription. C `read_reply` frames non-autosize monitor events at the requested count and zero-fills missing elements.

Expected fix shape:

Split `send_monitor_snapshot` into a requested-count-aware helper, or add a count parameter and route initial `EVENT_ADD` plus access-restore snapshots through `pad_dbr_to_requested_count`. Keep `count == 0` as autosize.

### EX-R10 - pvalink OUT has never sent `Int64Array` through the typed array path

Severity: medium-high.

Evidence:

- `origin/main` already had `EpicsValue::Int64Array(Vec<i64>)` at old `crates/epics-base-rs/src/types/value.rs:26`.
- The origin bridge converter could encode that value as PVA `long[]` if called at old `crates/epics-bridge-rs/src/convert.rs:180` through `:182`.
- The origin pvalink OUT dispatcher used a hard-coded `array_path` list at old `crates/epics-bridge-rs/src/pvalink/integration.rs:672` through `:681`, and it omitted `EpicsValue::Int64Array`.
- The current branch keeps the same omission: `array_path` still lists the older array variants at `crates/epics-bridge-rs/src/pvalink/integration.rs:889` through `:898` and still excludes `EpicsValue::Int64Array`.
- When the predicate is false, pvalink sends `value.to_string()` through the string PUT path at `crates/epics-bridge-rs/src/pvalink/integration.rs:912` through `:913`.
- `Display` renders `Int64Array` as bracketed text at `crates/epics-base-rs/src/types/value.rs:73` through `:75`. The PVA array string parser splits on commas and parses each token directly at `crates/epics-pva-rs/src/client_native/ops_v2.rs:2369` through `:2377`, so the brackets are not accepted by `ScalarValue::parse`.

Impact:

A pvalink OUT fed by an `FTVL=INT64` waveform does not use the existing `long[]` encoder. It instead attempts to replay the display string as a PVA array literal, which the parser does not support. This can turn a valid local int64 waveform value into a failed remote PUT.

Expected fix shape:

Route `EpicsValue::Int64Array(_)` through `crate::convert::epics_to_pv_field` in the same typed-array path as the other arrays. Use the same array-classification helper as the `UInt64Array` fix so signed and unsigned 64-bit arrays stay covered together.

### EX-R11 - pvRequest string parser cannot express the `_filter` JSON carrier the server expects

Severity: medium.

Evidence:

- The PVA server reads server-side monitor filters from `record._options._filter`, and the value must be a JSON string such as `{"dbnd":{"d":0.5},"dec":{"n":3}}`, at current `crates/epics-pva-rs/src/server_native/tcp.rs:110` through `:154`.
- The monitor INIT path parses that JSON only when the decoded pvRequest contains that string option, at current `crates/epics-pva-rs/src/server_native/tcp.rs:2513` through `:2526`.
- `PvRequestExpr::encode` documents string values like `_filter={...}` at current `crates/epics-pva-rs/src/pv_request.rs:498` through `:509`.
- The parser's option path calls `lex_value()` for record options at current `crates/epics-pva-rs/src/pv_request.rs:1068` through `:1083`.
- `lex_value()` accepts only characters allowed by `is_value_char`, and that set is alphanumeric plus `_ . : - +` at current `crates/epics-pva-rs/src/pv_request.rs:1006` through `:1027` and `:1137` through `:1139`; it rejects `{`, `}`, quotes, and commas before a JSON value can be read.
- `pvput-rs -r` routes user-supplied pvRequest strings through `PvRequestExpr::parse` at current `crates/epics-pva-rs/src/bin/pvput-rs.rs:93` through `:108`.
- `pvmonitor-rs` has no `-r` / custom pvRequest argument in current `crates/epics-pva-rs/src/bin/pvmonitor-rs.rs:11` through `:31`, so the CLI monitor path cannot send `_filter` by string either.
- `origin/main` already had the same parser shape: old `crates/epics-pva-rs/src/pv_request.rs:883` through `:901` for `lex_value`, old `:945` through `:960` for record options, and old `:1014` through `:1016` for `is_value_char`.
- The programmatic builder path can still express this carrier; `crates/epics-pva-rs/tests/stability.rs:492` builds `.record("_filter", r#"{"dec":{"n":3}}"#)` directly. The broken surface is the public string parser and CLI route.

Impact:

Users who try the documented string shape `record[_filter={...}]` cannot activate server-side PVA monitor filters through `PvRequestExpr::parse` or the current CLI surface. The feature works only when callers know to bypass the parser with `PvRequestBuilder::record("_filter", json)`, so string parity with pvxs-style request expressions is incomplete.

Expected fix shape:

Extend the pvRequest string grammar with quoted or escaped option values that can carry JSON, and add a regression test that parses a `_filter` JSON value and round-trips it through `encode`. Add a `pvmonitor-rs` custom pvRequest option if CLI parity is intended for monitor filters.

### EX-R12 - PVA server-side transformation filters drop their transformed value

Severity: medium-high.

Evidence:

- `FilterChain::apply` returns the possibly transformed `FilteredMonitorEvent`, not only a pass/drop boolean, at `crates/epics-base-rs/src/server/database/filters/mod.rs:129` through `:136`.
- `TimestampFilter` rewrites the snapshot value for `num=ts`, `num=sec`, `num=nsec`, and string timestamp modes at `crates/epics-base-rs/src/server/database/filters/ts.rs:124` through `:164`.
- `ArrayFilter` rewrites array snapshot values to sliced arrays at `crates/epics-base-rs/src/server/database/filters/arr.rs:68` through `:80`.
- The PVA monitor emit loop calls `filters.apply(fev)` only to check whether it returned `None` at current `crates/epics-pva-rs/src/server_native/tcp.rs:3455` through `:3461`.
- The payload is still built from the original `value`, not from the transformed event returned by the filter chain, at current `crates/epics-pva-rs/src/server_native/tcp.rs:3481`.
- `origin/main` already had the same shape: old `crates/epics-pva-rs/src/server_native/tcp.rs:2818` through `:2824` called `filters.apply(fev).is_none()` and then built the monitor payload from the original `value`.
- The PVA adapter comment says filters that work include `TS`, while `ARR` is the explicit remaining gap, at current `crates/epics-pva-rs/src/server_native/tcp.rs:45` through `:69`; the implementation does not consume any transformed value for either class.

Impact:

PVA server-side filters that only decide pass/drop, such as `dec`, `sync`, and scalar `dbnd`, can affect emission. Filters that are supposed to transform the event, such as `ts` and `arr`, do not change the wire payload even when their filter chain executes and returns a transformed event. Clients using the programmatic `_filter` builder can therefore believe a PVA monitor is timestamp-tagged or array-sliced while receiving the original value.

Expected fix shape:

The PVA monitor path needs either a real `PvField` transformation bridge from `FilteredMonitorEvent` back to the wire value, or it must reject/disable transformation filters for PVA until that bridge exists. Add regression coverage that a PVA monitor with `ts` changes the emitted value and that an `arr` request slices an array, or explicitly returns a protocol error for unsupported transformation filters.

## Verification Run

- `git fetch --all --prune`: passed.
- `git diff --check origin/main...HEAD`: passed.
- `cargo check --workspace --all-targets`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo nextest run --workspace --no-fail-fast`: passed, `4518` tests passed.
- `cargo test --doc --workspace`: passed; all doctests were ignored or empty in the crates that reported doctests.
- Continuation passes only changed this review document.
- `rg -n "[ \t]+$" docs/merge-regression-review-2026-05-19.md`: passed, no trailing whitespace matches.
- `rg -n "^### MR-R|^### EX-R|^## Verification|^## Untracked" docs/merge-regression-review-2026-05-19.md`: passed; headings now cover `MR-R1` through `MR-R25` and `EX-R1` through `EX-R12`.

## Untracked Files

- `.caucus/` was present before this review and was not touched.
