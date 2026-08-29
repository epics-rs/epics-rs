# epics-bridge-rs functional review - 2026-05-20

Scope:

- Rust bridge code: `ca_gateway`, `pva_gateway`, `qsrv`, `calink`, `pvalink`.
- Reference code: `$EPICS_BASE`, `$PVXS_HOME`, and `$EPICS_MODULES/ca-gateway`.
- Static review only. Runtime tests were not run by request.
- The 2026-05-19 punchlist marks the earlier BR-R findings as cleared; this file records additional findings found after that baseline.

Resolution (2026-05-21): BRIDGE-FR-1 through BRIDGE-FR-16 are all implemented in the current workspace and covered by tests. This file is retained as the review record; each finding below documents the gap that was closed and its fix direction.

Recording note (2026-08-26): that resolution sentence was the whole of this
file's closure record, and no per-row scan could act on it — it names no
commit and it uses a word outside every verdict vocabulary, so all sixteen
rows still read open. Each row now carries a `Status:` line naming the
commit that closed it. Six commits cover the sixteen rows; each of them
spells out its own per-row claim in its message body.

## C reference pins

Every C/C++ citation in this file resolves at the tree and revision below,
not at whatever the env-var checkout holds today — those checkouts run ahead
of their pins, so a citation checked against one can be graded wrong while
being right, or graded right after drifting into a neighbouring construct.
The resolve-by-symbol rule, the shared-basename rule and the verification of
each pin are in `c-reference-pins.md`.

| tree | pinned revision | cited here |
| --- | --- | --- |
| `pvxs` | `1.5.1-42-gb568e93` | `clientmon.cpp`, `fieldsubscriptionctx.cpp`, `groupconfigprocessor.cpp`, `groupsource.cpp`, `serverchan.cpp`, `serverconn.cpp`, `serverintrospect.cpp`, `servermon.cpp`, `source.h`, and the `ioc/pvalink*.cpp` set |
| `ca-gateway` | `R2-1-3-0-54-g0666f21` | `gateAs.cc`, `gateAs.h`, `gateServer.cc`, `gateVc.cc` |
| `epics-base` | `R7.0.10` | `dbCa.c`, `dbStaticLib.c`, `recGbl.c` |

This review cites no pva2pva source, but `server.cpp` and every
`pvalink*.cpp` exist there too, so the basename alone would resolve in the
wrong file without failing. Every such citation here already carries its
pvxs in-tree path (`$PVXS_HOME/src/server.cpp:252`,
`$PVXS_HOME/ioc/pvalink_jlif.cpp:24`); keep it that way.

Rust `*.rs` citations are in-repo and carry no pin: they resolve at the
current worktree, not at the commit this review was written on. Where the
reviewed code has since been fixed, moved or replaced, the line names the
construct that now carries the behaviour and the sentence says so.


## Findings

### BRIDGE-FR-1. CA gateway ACF read and monitor access is not enforced

Severity: high security parity gap.

Status: **CLEARED** `4e9079a4` — the downstream CA server path now takes the ACF read/monitor decision, so a deny-read rule blocks caget and camonitor and not only writes.

Rust evidence:

- `crates/epics-bridge-rs/src/ca_gateway/access.rs:83` implements `AccessConfig::can_read(...)`.
- Production `ca_gateway` code does not call `can_read(...)`; the only callers are tests/static definitions.
- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:704` applies `AccessConfig::can_write(...)` in the write hook, so write ACF is wired.
- `crates/epics-bridge-rs/src/ca_gateway/downstream.rs:286` constructs the downstream CA server with `CaServer::from_parts(..., None, None, None, None)`, so no server-side ACF object is installed for client reads.
- `crates/epics-bridge-rs/src/ca_gateway/server.rs:341` resolves searches through `.pvlist` and subscribes upstream without a downstream user/host access decision.

C ca-gateway reference:

- `$EPICS_MODULES/ca-gateway/src/gateVc.cc:315` implements `gateVcChan::readAccess()` using both upstream read access and local `asCheckGet`.
- `$EPICS_MODULES/ca-gateway/src/gateVc.cc:333` implements the analogous write path.
- `$EPICS_MODULES/ca-gateway/src/gateAs.h:159` exposes `readAccess()` and `writeAccess()`.
- `$EPICS_MODULES/ca-gateway/src/gateAs.cc:255` creates `gateAsClient` entries from downstream user/host.

Impact:

An ACF rule that denies read access can still allow `caget`/`camonitor` through the Rust CA gateway. Access-rights reporting also cannot faithfully report per-client read denial because the downstream read path never consults ACF.

Fix direction:

Route downstream CA search/create/read/monitor access-rights decisions through `AccessConfig::can_read(...)` using the downstream user/host, matching the existing write hook's `can_write(...)` path. Do not rely on `.pvlist` alone; `.pvlist` controls name admission, not per-client ACF.

### BRIDGE-FR-2. CA gateway `.pvlist` `ALIAS` subscribes the real PV but does not serve the alias name

Severity: high functional parity gap.

Status: **CLEARED** `4e9079a4` — the gateway serves the `.pvlist` ALIAS name downstream while subscribing the real upstream PV.

Rust evidence:

- `crates/epics-bridge-rs/src/ca_gateway/pvlist.rs:192` expands an `ALIAS` entry into `resolved_name`.
- `crates/epics-bridge-rs/src/ca_gateway/server.rs:341` receives the downstream requested name, matches `.pvlist`, then calls upstream subscription with `m.resolved_name`.
- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:354` registers the shadow PV with `add_pv_with_hook(upstream_name, ...)`; `upstream_name` is the resolved real PV, not the downstream alias.
- `crates/epics-base-rs/src/server/database/mod.rs:883` calls the resolver and then looks up the original requested name again. If the resolver inserted only the real name, lookup by alias still fails.

C ca-gateway reference:

- `$EPICS_MODULES/ca-gateway/src/gateServer.cc:1536` resolves a requested `pvname` through the `.pvlist`.
- `$EPICS_MODULES/ca-gateway/src/gateServer.cc:1747` attaches the requested alias name to the real `gateVcData`.
- `$EPICS_MODULES/ca-gateway/src/gateAs.cc:127` performs alias-to-real-name expansion.

Impact:

A client searching for an alias can trigger an upstream subscription to the real PV, but the downstream database does not expose an entry under the alias name. Clients using the alias name can fail `CREATE_CHANNEL`/lookup, and the gateway may keep an unnecessary upstream subscription created during the failed resolution.

Fix direction:

Represent a shadow PV with both a downstream served name and an upstream real name. The downstream database must register the alias key while the upstream manager connects to the resolved real PV. The alias and real PV should share the same monitor/update hook without requiring downstream clients to know the real name.

### BRIDGE-FR-3. CA link alarm severity modifiers are parsed away or malformed, then every nonzero remote alarm propagates

Severity: high IOC functional parity gap.

Status: **CLEARED** `6b3d60ea` — MS, NMS, MSI and MSS are parsed and applied at the link fold boundary instead of being dropped.

Rust evidence:

- `crates/epics-base-rs/src/server/record/link.rs:396` returns `ParsedLink::Ca(rest.to_string())` for `ca://...` before legacy link modifiers are stripped, so `ca://PV MS` is treated as PV name `PV MS`.
- `crates/epics-base-rs/src/server/record/link.rs:460` strips bare `CA MS/NMS/MSI/MSS` modifiers but stores only `ParsedLink::Ca("PV")`; the selected severity policy is lost.
- `crates/epics-base-rs/src/server/record/link.rs:758` tests currently assert that `REC.VAL CA MS` and `REC.VAL PP CA` both reduce to a bare `ParsedLink::Ca("REC.VAL")`.
- `crates/epics-ca-rs/src/calink/resolver.rs:1048` returns `Some(sev)` for every nonzero remote severity. (The resolver relocated from `epics-bridge-rs` to `epics-ca-rs` in `4d7e3860`; the three `calink/resolver.rs` citations below are repointed to its current home and lines.)
- `crates/epics-base-rs/src/server/database/processing.rs:620` assumes external link sets already applied link-level alarm policy and folds returned severity into the owning record.

C EPICS reference:

- `$EPICS_BASE/modules/database/src/ioc/dbStatic/dbStaticLib.c:2375` parses `NMS`, `MSI`, `MSS`, and `MS` into the link option.
- `$EPICS_BASE/modules/database/src/ioc/db/recGbl.c:263-281` applies those policies: `NMS` ignores remote alarms, `MSI` inherits only invalid alarms, `MS` raises `LINK_ALARM`, and `MSS` preserves remote status/message.
- `$EPICS_BASE/modules/database/src/ioc/db/dbCa.c:672` exposes the raw remote alarm through the CA link set; record processing applies the modifier.

Impact:

`INP="REMOTE CA NMS"` can still propagate a remote nonzero alarm. `MSI` can propagate minor/major alarms that should be ignored. `MSS` cannot preserve the remote status/message because the Rust CA resolver only returns severity and base processing maps the contribution to `LINK_ALARM`. URI-style `ca://PV MS` is additionally malformed into the wrong PV name.

Fix direction:

Keep CA link alarm policy in the parsed link model, not as discarded syntax. Apply the policy at the record-processing/link-set boundary before severity is folded into the record. URI-style CA links should parse modifiers outside the PV name.

### BRIDGE-FR-4. CA link metadata getters are missing

Severity: medium functional parity gap.

Status: **CLEARED** `6b3d60ea` — the CA link metadata getters (timestamp, alarm, units, precision, limits) are implemented.

Rust evidence:

- `crates/epics-ca-rs/src/calink/resolver.rs:950` implements `LinkSet` for `CaLinkResolver`.
- That implementation provides `is_connected`, `get_value`, `put_value`, `alarm_severity`, `time_stamp`, and `link_names`, but no `link_metadata(...)`.
- `crates/epics-base-rs/src/server/database/link_set.rs:169` defaults `link_metadata(...)` to `None`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:1016` and `crates/epics-bridge-rs/src/pvalink/link.rs:673` do implement metadata for PVA links, so this is a CA-link-specific gap.

C EPICS reference:

- `$EPICS_BASE/modules/database/src/ioc/db/dbCa.c:726` implements CA link control limits.
- `$EPICS_BASE/modules/database/src/ioc/db/dbCa.c:742` implements graphic/display limits.
- `$EPICS_BASE/modules/database/src/ioc/db/dbCa.c:758` implements alarm limits.
- `$EPICS_BASE/modules/database/src/ioc/db/dbCa.c:776` implements precision.
- `$EPICS_BASE/modules/database/src/ioc/db/dbCa.c:788` implements units.
- `$EPICS_BASE/modules/database/src/ioc/db/dbCa.c:819` registers those getters in `dbCa_lset`.

Impact:

Records using CA input links do not inherit remote display/control/alarm limits, precision, or engineering units through the Rust CA link set. PVA links already have a structured metadata path, so IOC behavior differs by link protocol even when both remote PVs carry equivalent metadata.

Fix direction:

Extend `CaLink`'s cached snapshot/attribute state with CA metadata and implement `LinkSet::link_metadata(...)` for `CaLinkResolver`. The result should map CA control/display/alarm limits, precision, units, DBF type, and element count into the existing `LinkMetadata` structure where possible.

### BRIDGE-FR-5. PVA gateway credential-aware PUT/RPC/PROCESS still performs shared-cache preflight

Severity: high security/audit parity gap.

Status: **CLEARED** `ea5a3fa6` — credential-aware PUT, RPC and PROCESS no longer run the shared-cache preflight that ignored downstream credentials.

Rust evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:699` implements credential-aware `put_value_checked(...)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:728` calls `self.cache.lookup(name, self.connect_timeout)` before selecting `self.upstream_client_for(&ctx)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:794` implements `rpc_checked(...)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:823` calls the same shared cache before forwarding RPC through the per-credential upstream client.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:862` implements `process_checked(...)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:889` again calls the shared cache before selecting the per-credential client.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:916`, `:933`, and `:946` show that GET and MONITOR already use `self.upstream_cache_for(&ctx)`, the credential-scoped cache.

PVA/pvxs reference:

- `crates/epics-pva-rs/src/server_native/source.rs:23` carries downstream `ChannelContext` credentials through the server source API.
- `$PVXS_HOME/src/serverconn.cpp:217` parses authenticated connection credentials.
- `$PVXS_HOME/src/servermon.cpp:469` obtains monitor credentials from the operation setup.
- `$PVXS_HOME/src/server.cpp:252` stores connection credentials on the server connection.

Impact:

A credentialed downstream PUT/RPC/PROCESS can first create or use an upstream monitor lookup under the gateway/shared identity, before the actual operation is forwarded with the downstream identity. If the shared identity lacks upstream read/monitor rights, a downstream user that could legitimately write/process via its own identity can be rejected before the per-credential operation is attempted. If the shared identity has broader read rights, the gateway opens an upstream monitor under the wrong audit identity as a side effect of a write-class operation.

Fix direction:

Remove the shared-cache preflight from credential-aware PUT/RPC/PROCESS, or route it through `upstream_cache_for(&ctx)` if an existence check is still required. Prefer relying on the selected per-credential `PvaClient` operation to resolve and return the upstream error; that keeps identity, audit, and access behavior aligned with GET/MONITOR.

### BRIDGE-FR-6. QSRV group monitors subscribe to the backing record, not the configured member field

Severity: high functional parity gap.

Status: **CLEARED** `608c6c87` — a group monitor subscribes the configured member field rather than the backing record.

Rust evidence:

- `crates/epics-bridge-rs/src/qsrv/group.rs:1363` parses `member.channel` but discards the field suffix with `let (record_name, _) = parse_pv_name(&member.channel)`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:1386` subscribes value events with `DbSubscription::subscribe_with_mask(&self.db, record_name, ...)`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:1417` does the same for property events.
- The read path does preserve the member field: `crates/epics-bridge-rs/src/qsrv/group.rs:663` parses `(record_name, field_name)` and `decode_member(...)` reads that field.

pvxs reference:

- `$PVXS_HOME/ioc/fieldsubscriptionctx.cpp:25` subscribes a `FieldSubscriptionCtx` against `field->value` for value events and `field->properties` for property events.
- `$PVXS_HOME/ioc/groupsource.cpp:431` and `:434` call `subscribeField(...)` for each configured field.
- `$PVXS_HOME/ioc/groupsource.cpp:331` treats the triggering `pChannel` as the actual subscribed dbChannel.

Impact:

A group member configured with `"+channel": "REC.RVAL"` or any non-`VAL` field reads the correct field on GET, but its monitor subscriptions wake on `REC.VAL`. Changes posted only by the configured field can be missed, and unrelated `VAL` posts can wake the group and re-read a member that did not trigger the event.

Fix direction:

Subscribe using the full `member.channel` for value events. Property subscriptions also need to target the member's property channel equivalent, not the record default. Keep the current `record_name` parse only for record-lock grouping, not for event subscription identity.

### BRIDGE-FR-7. QSRV group `meta` members use the scalar/plain value-event mask

Severity: medium functional parity gap.

Status: **CLEARED** `608c6c87` — a `meta` member uses the meta event mask rather than the scalar value-event mask.

Rust evidence:

- `crates/epics-bridge-rs/src/qsrv/group.rs:1366` builds one `VALUE | ALARM | LOG` value mask for every group member with a backing channel.
- `crates/epics-bridge-rs/src/qsrv/group.rs:1386` uses that mask without checking `member.mapping`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:729` implements `FieldMapping::Meta` as only `{alarm, timeStamp}`.

pvxs reference:

- `$PVXS_HOME/ioc/groupsource.cpp:427` documents two subscriptions for each group channel.
- `$PVXS_HOME/ioc/groupsource.cpp:429-431` subscribes `MappingInfo::Meta` value-side events with `DBE_ALARM` only.
- `$PVXS_HOME/ioc/groupsource.cpp:432-434` uses `DBE_VALUE | DBE_ALARM | DBE_ARCHIVE` for non-meta value mappings.
- `$PVXS_HOME/ioc/groupsource.cpp:437-440` separately subscribes meta/scalar property events with `DBE_PROPERTY`.

Impact:

A meta-only group member can emit monitor updates for ordinary value/log events even though pvxs would wake it only for alarm and property changes. That can create extra group monitor traffic and changed-bitset updates whose only changed fields are metadata timestamps, which makes Rust QSRV noisier than pvxs for meta members.

Fix direction:

Choose the value subscription mask per member mapping: `Meta -> ALARM`, non-meta record-backed mappings -> `VALUE | ALARM | LOG`, with the existing property subscription retained for `Meta` and `Scalar`.

### BRIDGE-FR-8. PVA gateway CREATE_CHANNEL/GET_FIELD opens the shared upstream cache without downstream credentials

Severity: high security/audit parity gap.

Status: **CLEARED** `ea5a3fa6`, over the server-native half in `5d9327b0` — CREATE_CHANNEL and GET_FIELD open the upstream cache keyed by downstream credentials; regression `fr8_wrapper_stack_threads_checked_existence_and_introspection`.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:2361` resolves `CREATE_CHANNEL` by calling `src.get_introspection(&nm).await`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:4049` resolves `GET_FIELD` by calling `src.get_introspection(&pv_name).await`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:633` implements `has_pv(...)` by calling `self.cache.lookup(...)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:640` implements `get_introspection(...)` by calling the same shared `self.cache.lookup(...)`.
- Credential-aware GET/MONITOR uses `upstream_cache_for(&ctx)` at `crates/epics-bridge-rs/src/pva_gateway/source.rs:916`, `:933`, and `:946`, so the correct per-credential primitive exists but is unavailable to `has_pv`/`get_introspection`.

pvxs/PVA reference:

- `$PVXS_HOME/src/serverchan.cpp:62` constructs `ServerChannelControl` with `conn->cred`.
- `$PVXS_HOME/src/pvxs/source.h:167` stores credentials on `ChannelControl` through `OpBase`.
- `$PVXS_HOME/src/serverintrospect.cpp:66` constructs the GET_FIELD `ConnectOp` with `conn->cred`.

Impact:

Opening a credentialed downstream PVA channel through the Rust gateway can create the upstream monitor/cache entry under the gateway/shared identity before any credential-scoped operation runs. That has the same isolation problem as BRIDGE-FR-5, but at channel setup and GET_FIELD time: upstream audit logs and upstream access decisions see the gateway identity for descriptor discovery, not the authenticated downstream peer.

Fix direction:

Add credential-aware source methods for channel existence and introspection, or defer upstream cache creation until the first checked operation. A PVA gateway should not open or refresh upstream state under the shared identity for a credentialed downstream connection.

### BRIDGE-FR-9. DB pvalink legacy suffix options are parsed as part of the PV name

Severity: high IOC functional parity gap.

Status: **CLEARED** `8bec248a` — legacy suffix options are parsed as link options instead of being folded into the PV name.

Rust evidence:

- `crates/epics-base-rs/src/server/record/link.rs:398` returns `ParsedLink::Pva(rest.to_string())` for `pva://...` before the generic legacy modifier stripping runs.
- `crates/epics-bridge-rs/src/pvalink/config.rs:174` and `:264` can parse legacy suffix modifiers (`PP`, `CP`, `CPP`, `MS`, `MSI`, `MSS`, `NMS`) when a full `pva://...` string reaches `PvaLinkConfig::parse(...)`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:632` pre-registers DB pvalink options only when the stored `ParsedLink::Pva` string contains `?`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:399` falls back to `default_inp_cfg(bare)` when no option entry was registered; for `ParsedLink::Pva("TARGET MS")`, `bare` is still `TARGET MS`.

pvxs reference:

- `$PVXS_HOME/ioc/pvalink_jlif.cpp:24` documents pvalink option fields.
- `$PVXS_HOME/ioc/pvalink_jlif.cpp:158` through `:166` parse `proc` modes.
- `$PVXS_HOME/ioc/pvalink_jlif.cpp:172` through `:183` parse `sevr` modes.
- `$PVXS_HOME/ioc/pvalink.cpp:285` through `:294` reports the parsed proc/severity modes from the stored pvalink.

Impact:

A DB link such as `pva://TARGET:AI MS` or `pva://TARGET:AI CPP` does not select severity or scan behavior. The Rust resolver can instead attempt to open an upstream PV literally named `TARGET:AI MS` or `TARGET:AI CPP`. JSON/query-style pvalink options are covered by the earlier BR-R10 fix, but legacy suffix syntax remains broken through the normal DB record path.

Fix direction:

Run pvalink suffix parsing before storing `ParsedLink::Pva`, or make the pvalink resolver pre-scan pass reconstruct and parse `pva://{s}` for suffix-bearing strings as well as query-bearing strings. The stored PV name and the stored per-link options must be separated.

### BRIDGE-FR-10. CA gateway `.pvlist` `DENY FROM` is not evaluated with the downstream client host at search time

Severity: high access-control parity gap.

Status: **CLEARED** `4e9079a4` — `match_name_for_host` evaluates DENY FROM against the downstream peer at search time; regressions `fr10_host_deny_preempts_allow_in_deny_allow_order` and three siblings.

Rust evidence:

- `crates/epics-bridge-rs/src/ca_gateway/pvlist.rs:146` implements `PvList::is_host_denied(name, host)`, but this is only consulted by the write hook.
- `crates/epics-bridge-rs/src/ca_gateway/upstream.rs:679` applies `is_host_denied(...)` after a downstream put has already reached the write path.
- `crates/epics-bridge-rs/src/ca_gateway/server.rs:347` resolves downstream searches with `pvlist.match_name(&name)` and has no client host argument.
- `crates/epics-bridge-rs/src/ca_gateway/server.rs:402` uses the same host-less `match_name(...)` path for preload decisions.
- `crates/epics-bridge-rs/src/ca_gateway/pvlist.rs:180` treats any `DENY` rule whose pattern matches the PV as a `deny_match`, including `DENY FROM host` entries.
- `crates/epics-bridge-rs/src/ca_gateway/pvlist.rs:210` through `:222` then applies evaluation order without knowing whether the requester host matches the `FROM` list.

C ca-gateway reference:

- `$EPICS_MODULES/ca-gateway/src/gateServer.cc:1526` converts the downstream client's socket address to a host string when `DENY FROM` rules are present.
- `$EPICS_MODULES/ca-gateway/src/gateServer.cc:1537` calls `getAs()->findEntry(pvname, hostname)` during `pvExistTest`.
- `$EPICS_MODULES/ca-gateway/src/gateAs.h:257` through `:267` checks `deny_from_table` only when the passed host matches, then applies the normal allow/deny decision.
- `$EPICS_MODULES/ca-gateway/src/gateAs.cc:455` through `:520` resolves `DENY FROM` host names into IP-address entries before installing the rule table.

Impact:

The Rust gateway cannot reproduce host-scoped `.pvlist` admission. With `EVALUATION ORDER ALLOW,DENY`, a rule such as `PV.* DENY FROM bad.host` is seen as a deny for every host during search because `match_name(...)` ignores `from_hosts`. With `EVALUATION ORDER DENY,ALLOW`, an allow rule can make the PV searchable and readable for the denied host, while only puts are later rejected by the write hook. In both cases, search/create/read/monitor behavior diverges from C ca-gateway's host-aware `pvExistTest`.

Fix direction:

Split name admission into a host-aware path, for example `match_name_for_host(name, host)`, and use it from downstream CA search/create resolution. Host-targeted `DENY FROM` entries should not participate as global denies in host-less matching; when a downstream host is known, the host-specific deny table must be evaluated before the normal allow rule exactly as the C gateway does. Hostname/IP normalization also needs to match the socket-address form used by CA clients.

### BRIDGE-FR-11. PVA gateway upstream pipeline pause hook is not reachable and its high/low actions are reversed

Severity: medium functional/resource-control parity gap.

Status: **CLEARED** `ea5a3fa6`, over the hook added in `5d9327b0` — one `PauseControl` owner drives the upstream pause, reconnect reinstalls the standing vote, and the high and low watermark actions are the right way round; regressions `fr11_cross_watermark_is_once_per_crossing_parity_and_monotonic` and two siblings.

Rust evidence:

- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:219` exposes an upstream monitor `Pauser` for the current upstream subscription.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:1026` and `:1046` implement `notify_watermark_high(...)` / `notify_watermark_low(...)`, but `GatewayChannelSource` does not override `monitor_watermarks(...)`.
- `crates/epics-pva-rs/src/server_native/source.rs:552` defaults `monitor_watermarks(...)` to `None`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3564` snapshots `wm_levels = src.monitor_watermarks(&pv_name)`, and `:3822` / `:3864` call the source callbacks only when those levels are `Some`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:1039` through `:1040` pauses upstream on `notify_watermark_high(...)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:1055` through `:1056` resumes upstream on `notify_watermark_low(...)`.
- Credentialed monitors use per-credential upstream caches at `crates/epics-bridge-rs/src/pva_gateway/source.rs:933` and `:946`, but both watermark callbacks look only in the shared `self.cache` at `:1035` and `:1051`.

pvxs/PVA reference:

- `$PVXS_HOME/src/pvxs/source.h:120` through `:128` defines watermarks on the outbound pipeline window, and `onHighMark` fires when a client ACK refills the window above the high mark.
- `$PVXS_HOME/src/servermon.cpp:192` through `:206` fires `onLowMark` after emitting DATA drains the window to or below low.
- `$PVXS_HOME/src/servermon.cpp:653` through `:666` fires `onHighMark` when ACKs add enough credit.
- `$PVXS_HOME/src/clientmon.cpp:329` through `:342` sends the client monitor INIT with the pipeline bit and queue-size trailer.

Impact:

The gateway stores a pause/resume handle for upstream monitors, but no monitor operation can currently reach those callbacks because no gateway watermark levels are exposed. If a future call path does reach them, the actions are inverted relative to pipeline-window semantics: high means the downstream client has credited more window and upstream production can resume, while low means the window was consumed and upstream should be paused. Credentialed monitor streams have an additional miss because the callbacks search the shared cache instead of the per-credential cache that actually feeds the monitor.

Fix direction:

Expose gateway-specific `monitor_watermarks(...)` levels and align callback semantics with pvxs pipeline-window behavior: low should pause the feeding upstream subscription, high should resume it. The downstream monitor op also needs to carry enough source/cache identity for credentialed subscriptions so pause/resume targets the same upstream cache that `subscribe_checked(...)` selected.

### BRIDGE-FR-12. QSRV explicit group trigger target sets are ignored after parsing

Severity: high functional parity gap.

Status: **CLEARED** `608c6c87`, over the encode half in `5d9327b0` — an explicit trigger target set survives parsing and reaches the wire as a selection bitset.

Rust evidence:

- `crates/epics-bridge-rs/src/qsrv/group_config.rs:118` through `:132` models four trigger states, including `TriggerDef::Fields(Vec<String>)`.
- `crates/epics-bridge-rs/src/qsrv/provider.rs:699` through `:711` validates `TriggerDef::Fields(...)` references during `process_groups()`, but does not resolve them into an executable trigger-target graph.
- `crates/epics-bridge-rs/src/qsrv/group.rs:1527` through `:1546` treats `SelfOnly`, `All`, and `Fields(_)` identically: any value event returns `group_channel.read_group()`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:566` through `:581` shows `read_group()` reads every non-`Proc`/`Structure` member.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:825` through `:833` restricts partial monitor emission to pure self-trigger groups; any explicit `+trigger` group keeps the full request mask.

pvxs reference:

- `$PVXS_HOME/ioc/groupconfigprocessor.cpp:297` through `:309` parses each field's `+trigger` list.
- `$PVXS_HOME/ioc/groupconfigprocessor.cpp:327` through `:338` defaults groups with no trigger mappings to self-trigger.
- `$PVXS_HOME/ioc/groupconfigprocessor.cpp:381` through `:409` resolves `*` or named trigger targets into each field's `triggerNames`.
- `$PVXS_HOME/ioc/groupsource.cpp:328` through `:346` iterates only `field.triggers` on a subscription event and refreshes those target fields before posting the group.

Impact:

A group member configured with `"+trigger": "otherField"` should refresh and mark only the named target field when the source member posts. The Rust monitor instead re-reads the complete group and emits with a full request mask. That can expose updates from fields that did not trigger, collapse deliberately split monitor updates into full-group updates, and make named-trigger groups behave like `+trigger:"*"` from a downstream client's perspective.

Fix direction:

Resolve `TriggerDef::Fields` and `TriggerDef::All` into explicit target member indexes during `process_groups()` or group construction. On a member event, refresh only the target set and carry that target set into the monitor emission so the PVA changed-bitset matches the configured trigger graph. `SelfOnly`, `All`, named fields, and explicit silence should remain distinct states after parsing.

### BRIDGE-FR-13. pvalink disconnected monitor reads can return stale cached values without LINK/INVALID alarm

Severity: high IOC functional parity gap.

Status: **CLEARED** `8bec248a` — a disconnected monitor read fails with LINK/INVALID through an `is_connected` gate instead of serving the stale cache; regressions `fr13_disconnected_monitor_read_fails_and_reports_invalid` and three siblings.

Rust evidence:

- `crates/epics-bridge-rs/src/pvalink/link.rs:175` through `:178` stores each monitor event in `latest`.
- `crates/epics-bridge-rs/src/pvalink/link.rs:188` through `:190` flips `monitor_connected` to false when the upstream monitor returns.
- `crates/epics-bridge-rs/src/pvalink/link.rs:252` through `:256` returns `latest` from `read_with_field(...)` for monitor links without checking `monitor_connected`.
- `crates/epics-bridge-rs/src/pvalink/link.rs:277` through `:282` does the same in the synchronous `try_read_cached_with_field(...)` fast path.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:865` through `:870` returns that cached value from `LinkSet::get_value(...)`.
- `crates/epics-base-rs/src/server/database/processing.rs:810` through `:832` raises `LINK_ALARM/INVALID` only when the external link read returns `None`.
- `crates/epics-base-rs/src/server/database/links.rs:300` through `:328` builds external-link alarm state only from `lset.alarm_severity(...)`; it does not synthesize a disconnect alarm from `is_connected(...)`.

pvxs reference:

- `$PVXS_HOME/ioc/pvalink_lset.cpp:259` through `:272` checks `!self->valid()` in `pvaGetValue(...)`, sets `LINK_ALARM/INVALID_ALARM`, and returns failure while disconnected.
- `$PVXS_HOME/ioc/pvalink_channel.cpp:370` through `:376` deliberately keeps the previous value on disconnect, but only with the disconnect alarm state.

Impact:

After a monitor link has received one value, an upstream IOC restart or network break can leave `latest` populated while `monitor_connected == false`. The Rust read path can then return the stale value as a successful external-link read, so the owning record misses the `LINK_ALARM/INVALID` state that pvxs reports for the same disconnected pvalink. Consumers can treat stale data as fresh until the resubscribe loop delivers a new event or the cache is otherwise disturbed.

Fix direction:

Separate "cached value exists" from "link read is valid". Monitor reads should preserve the last value for diagnostics/timestamp parity, but when `monitor_connected` is false they must surface a failed link read or a structured alarm contribution that base processing folds into `LINK_ALARM/INVALID`. Avoid fixing only the fast path; `read_with_field(...)`, `try_read_cached_with_field(...)`, `alarm_severity(...)`, and the base `external_link_alarm(...)` boundary need one shared disconnect invariant.

### BRIDGE-FR-14. PVA gateway control `flush` and `drop` miss credential-scoped upstream caches

Severity: medium resource-control and operator-control parity gap.

Status: **CLEARED** `ea5a3fa6` — control `flush` and `drop` reach the credential-scoped upstream caches, not just the default-credential one.

Rust evidence:

- `crates/epics-bridge-rs/src/pva_gateway/control.rs:31` documents `<prefix>:flush` as "Drop every cached upstream entry".
- `crates/epics-bridge-rs/src/pva_gateway/control.rs:241` through `:250` implements `flush` by calling only `self.cache.flush().await`.
- `crates/epics-bridge-rs/src/pva_gateway/control.rs:253` through `:275` implements `drop` by calling only `self.cache.drop_entry(&target).await`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:421` through `:447` routes credentialed GET/MONITOR traffic through per-credential `upstream_caches`, not the shared `self.cache`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:460` through `:463` shows the per-credential cache pool is a separate structure; it is cleared only by `set_max_upstream_identities(...)`, not by control RPCs.
- `crates/epics-bridge-rs/src/pva_gateway/control.rs:352` through `:362` also reports `cacheSize` from the shared cache only, so control diagnostics undercount credentialed cache entries.

Reference behavior:

- `crates/epics-bridge-rs/src/pva_gateway/control.rs:21` through `:40` states the control PVs mutate gateway state, not just one anonymous/shared identity cache.
- `$PVXS_HOME/src/server.cpp:252` and `$PVXS_HOME/src/serverchan.cpp:62` keep credentials attached to operations, but gateway operator controls are expected to act on gateway-owned upstream state as a whole.

Impact:

An operator can call `<prefix>:flush` or `<prefix>:drop` and receive a successful reply while credentialed downstream clients continue to use already-open per-credential upstream entries and monitors. After an ACF reload, a manual flush/drop does not force credential-scoped lookups to reconnect under the new intended state. The diagnostic `cacheSize` / `upstreamCount` values also report only the shared cache, so the operator can see zero cached entries while per-credential upstream monitors remain alive.

Fix direction:

Make `GatewayChannelSource` own cache administration for all cache layers. Add a single gateway-level `flush_all_caches()` / `drop_entry_all_caches(name)` style API that clears the shared cache plus every cache inside `upstream_caches`, and make control diagnostics aggregate across both shared and credential-scoped caches. The control source should not reach into only the shared `ChannelCache` when credential-aware routing exists.

### BRIDGE-FR-15. pvalink `LinkSet::link_names()` is empty, so IOC init wait never waits for pvalinks

Severity: medium startup/diagnostic parity gap.

Status: **CLEARED** `8bec248a` — the `LinkSet` enumeration surfaces the opened INP upstream PV names, so iocInit waits for pvalinks; regression `fr15_link_names_reports_opened_inp_pvs_queryable_by_is_connected`.

Rust evidence:

- `crates/epics-base-rs/src/server/database/mod.rs:452` through `:468` defines `wait_for_external_links(...)` as a wait over every registered link set's `link_names()`.
- `crates/epics-base-rs/src/server/database/mod.rs:485` through `:493` builds the target set from `lset.link_names()` and returns `(0, 0)` when every registered link set returns an empty list.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:1046` through `:1051` implements `PvaLinkResolver::link_names()` as `Vec::new()`.
- `crates/epics-bridge-rs/src/pvalink/registry.rs:68` through `:79` stores opened pvalinks in a keyed map, and `:259` through `:260` already exposes the count, but there is no name iteration hook.
- `crates/epics-ca-rs/src/calink/resolver.rs:1076` through `:1078` returns actual CA link names, so this gap is pvalink-specific.

Reference behavior:

- `crates/epics-base-rs/src/server/database/mod.rs:452` through `:454` explicitly mirrors the EPICS `dbCa` iocInit wait for local CA links to connect.
- `$PVXS_HOME/ioc/pvalink_lset.cpp:259` through `:272` treats a disconnected pvalink as a link-level failure, which makes startup visibility of disconnected PVA links meaningful for IOC operators.

Impact:

Loaded pvalink records can be pre-opened by `install_pvalink_resolver(...)`, but the base startup wait sees no pvalink target names and can report no external links to wait for. A missing or slow PVA upstream therefore does not affect the external-link wait count and is harder to diagnose at IOC initialization time, while CA links registered in the same IOC do participate.

Fix direction:

Expose registry iteration from `PvaLinkRegistry` and return the opened PVA link names from `PvaLinkResolver::link_names()`. Preserve enough key detail to avoid collapsing distinct per-option links when diagnostics need them, but at minimum the wait path must see each opened upstream PV name and query `is_connected(...)` against the same resolver key family.

### BRIDGE-FR-16. pvalink `proc` collapses pvxs Default/PP/NPP/CP/CPP into one boolean for PUT requests

Severity: high IOC functional parity gap.

Status: **CLEARED** `8bec248a` — `ProcMode` keeps Default, PP, NPP, CP and CPP distinct through parsing, scan-flag derivation and the PUT request; regressions `fr16_proc_enum_preserved_and_put_request_derived` and three siblings.

Rust evidence:

- `crates/epics-bridge-rs/src/pvalink/config.rs:78` through `:79` stores OUT-side process behavior as a single `bool`.
- `crates/epics-bridge-rs/src/pvalink/config.rs:201` through `:214` maps query `proc=PP` and `proc=PASSIVE` to `process = true`, maps `proc=NPP` to `process = false`, and maps `proc=CP` / `proc=CPP` only to monitor scan flags without setting `process = true`.
- `crates/epics-bridge-rs/src/pvalink/config.rs:266` through `:276` applies the same collapse for legacy suffix modifiers: `NPP` becomes `false`, while `CP` / `CPP` do not affect the OUT process flag.
- `crates/epics-bridge-rs/src/pvalink/link.rs:844` through `:856` builds PUT pvRequests from that boolean: `false -> record._options.process="passive"` and `true -> "true"`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:902` through `:923` uses the parsed `PvaLinkConfig` for OUT `put_value(...)`, so the collapsed process state reaches real OUT writes.

pvxs reference:

- `$PVXS_HOME/ioc/pvalink_jlif.cpp:69` through `:77` maps null `proc` to `Default`.
- `$PVXS_HOME/ioc/pvalink_jlif.cpp:90` through `:99` maps boolean `proc` to `PP` or `NPP`.
- `$PVXS_HOME/ioc/pvalink_jlif.cpp:156` through `:166` maps string `proc` only for `CP`, `CPP`, `PP`, and `NPP`.
- `$PVXS_HOME/ioc/pvalink_channel.cpp:237` through `:263` preserves the enum at PUT time: `Default -> "passive"`, `NPP -> "false"`, and `PP` / `CP` / `CPP -> "true"`.
- `$PVXS_HOME/ioc/pvalink_link.cpp:122` through `:132` separately derives INP scan-on-update behavior from `CP` / `CPP`, so scan behavior and PUT process behavior are related but not the same field.

Impact:

The Rust representation cannot distinguish pvxs `Default` from `NPP`, so an explicit `proc=NPP` OUT pvalink sends `"passive"` instead of `"false"`. It also cannot represent the pvxs rule that `CP` and `CPP` request remote processing on PUT, because Rust stores them as scan-only flags and leaves `process = false`. Conversely, `proc=PASSIVE` is accepted as `process = true` even though pvxs's JSON `proc` parser does not accept `"PASSIVE"` as a pvalink enum value; `"passive"` is the later wire request value for `Default`, not a config enum variant. These cases can make OUT pvalinks process when they should not, fail to process when they should, or silently accept syntax pvxs would warn about.

Fix direction:

Replace the OUT `process: bool` primitive with an enum that preserves `Default`, `PP`, `NPP`, `CP`, and `CPP` through parsing and registry keying. Derive two outputs from that enum: INP scan-on-update state (`CP`/`CPP`) and PUT `record._options.process` (`Default -> passive`, `NPP -> false`, `PP/CP/CPP -> true`). Do not accept `PASSIVE` as a pvalink `proc` string unless it is intentionally documented as a Rust-only extension and not treated as pvxs parity.
