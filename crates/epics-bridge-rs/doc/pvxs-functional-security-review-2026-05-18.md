# epics-bridge-rs vs pvxs 기능/보안 리뷰

작성일: 2026-05-18

비교 대상:

- Rust: `crates/epics-bridge-rs`
- Rust PVA runtime: `crates/epics-pva-rs`
- C++ 기준: `$PVXS_HOME`

## 기준

`epics-bridge-rs`가 pvxs와 모든 내부 구현을 맞출 필요는 없다. 수용 기준은 wire-compatible 동작이다. 즉, 다음 항목은 parity 대상이다.

- PVA handshake, command state, pvRequest, descriptor/value/BitSet shape
- QSRV record field addressing, GET/PUT/MONITOR/RPC 결과
- 같은 credential/ASG/ASL에서 관찰되는 access decision
- gateway가 upstream에 전달하는 typed value, auth method, identity 의미
- queue, cache, connection pool 같은 remote-triggerable resource limit

다음 차이는 wire-compatible이면 허용 가능하다.

- Rust async 구조, task 분해, 캐시 구현 세부사항
- 로그 문구, 내부 통계 이름, 내부 helper/API 배치
- pvxs보다 엄격한 에러 반환. 단, 정상 클라이언트의 표준 요청을 깨지 않아야 한다.

참고: 이 checkout의 `$PVXS_HOME`에는 `p2pApp`/`pva2pva` gateway 소스가 보이지 않는다. 그래서 PVA gateway 항목은 pvxs PVA client/server wire 동작과 Rust gateway 구현을 비교했다.

## C reference pins

Every C/C++ citation in this file resolves at the tree and revision below,
not at whatever the env-var checkout holds today — those checkouts run ahead
of their pins, so a citation checked against one can be graded wrong while
being right, or graded right after drifting into a neighbouring construct.
The resolve-by-symbol rule, the shared-basename rule and the verification of
each pin are in `c-reference-pins.md`.

| tree | pinned revision | cited here |
| --- | --- | --- |
| `pvxs` | `1.5.1-42-gb568e93` | `clientget.cpp`, `credentials.cpp`, `fieldconfig.h`, `groupconfigprocessor.cpp`, `groupsource.cpp`, `iocsource.cpp`, `securityclient.cpp`, `serverget.cpp`, `servermon.cpp`, `singlesource.cpp`, `typeutils.cpp`, `test/testqgroup.cpp`, `test/testqsingle.cpp`, `test/testpvalink.cpp`, and the `ioc/pvalink*` set |

`pvalink.h`, every `pvalink_*.cpp` and `testpvalink.cpp` also exist in
`pva2pva`, so the basename alone resolves in the wrong file without failing.
This review is pvxs-only (`비교 대상: C++ 기준 $PVXS_HOME`), so every one of
them means the pvxs copy; each already carries its in-tree path.

Rust `*.rs` citations are in-repo and carry no pin: they resolve at the
current worktree, not at the commit this review was written on. Where the
reviewed code has since been fixed, moved or replaced, the line names the
construct that now carries the behaviour and the sentence says so.


## Cleared (this round, 2026-05-19)

The following findings have been addressed and are no longer open;
they are kept in `## Findings` below for historical context. Every
cleared item ships with a regression test referenced in the commit
message.

- BR-R1 **CLEARED** (`40685592`) — QSRV native PVA path preserves client identity (channel cache removed; `_checked` overrides thread `ctx.account`/`ctx.host`).
- BR-R2 **CLEARED** (`399d81c4`) — single-record channels honor `record.FIELD`; field DBF type drives DBR/NT shape.
- BR-R3 **CLEARED** (`183fce3e`) — PUT honors INIT pvRequest `record._options.process` / `block` via `ChannelContext.pv_request`.
- BR-R5 **CLEARED** (`9d3b4cc7`) — MONITOR honors `record._options.DBE` via the same pvRequest channel.
- BR-R9 **CLEARED** (`3609a18f`) — IOC launcher installs `AcfAccessControl` on the QSRV `BridgeProvider`.
- BR-R16 **CLEARED** (`baabe200`) — group GET/PUT honor pvRequest `record._options.atomic`.
- BR-R17 **CLEARED** (`baabe200`) — group PUT performs per-member ACF check; any denial fails the whole PUT.
- BR-R20 **CLEARED** (`0eb62a8a`) — process=passive vs force vs inhibit routed faithfully (`put_pv` vs `put_record_field_from_ca` vs `put_pv + process_record`).
- BR-R22 **CLEARED** (`24aca4cf`) — NTEnum uses `int32` index + `display.description`.
- BR-R25 **CLEARED** (`c14e17db`) — group root meta member `""` flattens `alarm`/`timeStamp` into the group root.
- BR-R26 **CLEARED** (`0eb62a8a`) — group `+const` accepted (with `+value` legacy fallback).
- BR-R30 **CLEARED** (`9513a564`) — group members without `+putorder` are non-writable (pvxs sentinel).
- BR-R31 **CLEARED** (`0eb62a8a`) — group PUT rejects link-class field targets before any write fires.
- BR-R32 **CLEARED** (`a1e8bb3a`) — ACF CALC-gated rule disable surfaces a `WARN` at parse time (still fails closed; loud divergence).
- BR-R35 **CLEARED** (`8e898b36`) — `Snapshot` honors `info(Q:time:tag, "nsec:lsb:N")`; low N nanosecond bits split into `timeStamp.userTag`.
- BR-R36 **CLEARED** (`21dde249`) — single-record monitor uses VALUE|ALARM + separate PROPERTY subscription.
- BR-R37 **CLEARED** (`7c87eb10`) — RPC with query args requires WRITE access.
- BR-R38 **CLEARED** (`5687a6e4`) — PVA `PROCESS` actually runs the record's processing chain (rejects on group/native PVA PV).
- BR-R39 **CLEARED** (`2b9316aa`) — decoded MONITOR initial event encodes with pvRequest mask, not `BitSet::all_set`.
- BR-R19 **CLEARED** (`c455586a`) — pvalink `time=true` adopts upstream NT timestamp; new `external_link_time` consumer in `process_record_with_links_inner`.
- BR-R23 **CLEARED** (`1fcef4ce`) — pvalink INP conversion covers every pvData scalar-array variant (Float / Short / UByte / Byte / String / Long / UInt / ULong / Boolean), including `ScalarArrayTyped` from the typed-fast-path decoder.
- BR-R28 **CLEARED** (`27f9a596`) — pvalink `proc=CPP` (scanOnUpdatePassive) skips processing when owning record SCAN is not Passive.
- BR-R29 **CLEARED** (`26881621`) — group default `+trigger` is SelfOnly (new `TriggerDef` variant), not All. Wire BitSet narrowing for SelfOnly is the residual gap. The residual wire BitSet narrowing for SelfOnly landed later in `ee250057`.
- BR-R33 **CLEARED** (`552957fc`) — group GET/MONITOR carries `record._options.queueSize` + `atomic` at root; per-op queueSize negotiation is the residual gap. The residual per-op queueSize negotiation landed later in `16309573`.
- BR-R34 **CLEARED** (`46a3e247`) — group monitors include `DBE_LOG` (archive-class) in the per-member value mask; LOG-only posts now wake the group `poll()`.
- BR-R40 **CLEARED** (`eb88b3b7`) — QSRV accepts pvxs channel-filter syntax `PV.VAL{...}`; chain attaches per-subscription via new `subscribe_with_mask_and_filters`.
- BR-R42 **CLEARED** (`fad743a2`) — gateway raw monitor signals upstream descriptor change as a subscription boundary; downstream gets MONITOR FINISH instead of bytes encoded for the new (incompatible) descriptor.
- BR-R43 **CLEARED** (`fc59c1f6`) — pvalink monitor pvRequest always sends `pipeline` + `atomic=true` + `queueSize` (matches pvxs `pvaLink::makeRequest`).
- BR-R44 **CLEARED** (`8e671674`) — gateway raw monitor reencodes on byte-order mismatch instead of silently dropping every event.
- BR-R4 **CLEARED** (`db9a79e4`) — QSRV ACF adapter carries method/authority/roles/field ASL; `AcfAccessControl` builds pvxs-style credential list; `read_brace_list` extended with quoted-string support for `"role/groupname"` UAG entries.

## Cleared on the 2026-05-19 driver punchlist

The fourteen this file listed as remaining were taken up on
`punchlist-2026-05-19.md` and are all closed there, each with a named
regression; the two residual gaps noted above closed on the same list.

- BR-R6 **CLEARED** (`4b7c87eb`) — gateway typed PUT pass-through replaces the string `pvput` round-trip; regression `br_r6_gateway_typed_put_passthrough`.
- BR-R7 **CLEARED** (`52308402`) — bounded LRU upstream credential pool, cap 256, with `set_max_upstream_identities`; regression `br_r7_gateway_credential_pool_bounded`.
- BR-R8 **CLEARED** (`273d9a1d`) — the gateway records the downstream method and authority as an `AssertedIdentity` on its upstream client; regressions `br_r8_x509_downstream_recorded_as_asserted_identity` and `br_r8_ca_downstream_records_ca_method`.
- BR-R10 **CLEARED** (`dd80cdcd`) — DB JSON pvalink options survive on the parsed link as a query string; regressions `br_r10_json_pva_options_preserved_in_parsed_link` and two siblings.
- BR-R11 **CLEARED** (`a198da48`) — pvalink OUT preserves `field`, `proc`, `block` and the deferred-write option; regression `br_r11_pvalink_out_options_preserved`.
- BR-R12 **CLEARED** (`ad6331ed`) — NTScalar and NTScalarArray metadata shape matches pvxs; regressions `br_r12_array_metadata_shape_matches_pvxs` and `br_r12_string_value_omits_numeric_metadata`.
- BR-R13 **CLEARED** (`21ec21dc`) — unsigned 64-bit fields reach PVA as `ulong` through a new `DbFieldType::UInt64`; four `br_r13_` regressions.
- BR-R14 **CLEARED** (`15cc8be4`) — event-affecting monitor options are rejected at the gateway while field projection stays transparent, extended by `107bda3a`; regression `br_r14_field_projection_is_not_event_affecting`.
- BR-R15 **CLEARED** (`b093d49e`) — atomic group PUT runs under the unified `record_lock` registry; regressions `br_r15_atomic_group_excludes_direct_member_write` and `br_r15_atomic_put_blocks_on_member_record_gates`.
- BR-R18 **CLEARED** (`eff1848f`) — the pvalink atomic scan holds one multi-record lock epoch across the target loop; regression `br_r18_atomic_scan_holds_multi_record_lock_epoch`.
- BR-R21 **CLEARED** (`6f9d26c6`) — gateway GET and MONITOR route through the per-credential upstream cache; regression `br_r21_gateway_monitor_credential_scoping`.
- BR-R24 **CLEARED** (`142aa474`) — the `LinkSet` metadata hook exposes remote display, control and valueAlarm; regressions `br_r24_link_metadata_surfaces_remote_display_control_valuealarm` and one sibling.
- BR-R27 **CLEARED** (`285a24e1`) — the pvalink cache key carries per-link `field` and option state; regression `br_r27_pvalink_cache_separates_per_link_options`.
- BR-R41 **CLEARED** (`e314bb8a`) — the decoded monitor fallback fans out past the initial snapshot; regression `br_r41_typed_subscribe_delivers_updates`.

## Findings

### BR-R1. QSRV native PVA path loses client identity and caches anonymous channels

Severity: High security

Evidence:

- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:208` caches `AnyChannel` by PV name only.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:212` calls `provider.create_channel(name)`.
- `crates/epics-bridge-rs/src/qsrv/provider.rs:686` states default `create_channel()` uses anonymous access.
- `crates/epics-bridge-rs/src/qsrv/provider.rs:700` has `create_channel_for(name, user, host)`, but `QsrvPvStore` does not use it.
- `crates/epics-pva-rs/src/server_native/source.rs:55` default `ChannelSource::access()` returns an open gate.
- `crates/epics-pva-rs/src/server/pva_server.rs:86` says custom `ChannelSource` users must install ACF themselves.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:611` constructs `PvaServer::from_parts(... config.acf ...)`, but `:620` runs with the custom `QsrvPvStore`.
- pvxs preserves per-client credentials in `$PVXS_HOME/ioc/credentials.cpp:26` and checks them with AS in `$PVXS_HOME/ioc/securityclient.cpp:19`.

Impact:

The native PVA QSRV route does not use the downstream peer identity when creating QSRV channels. A channel created for one peer is reused for later peers with the same PV name. Since the default `ChannelSource` gate is open and `PvaServer::from_parts(acf)` does not automatically wrap custom sources, PVA QSRV GET/PUT/MONITOR can bypass the ACF supplied to the IOC runner. Native registered PVA PVs in `pva_pvs` also return values before provider access checks.

Wire-compatible expectation:

Access decisions must be made for the same user/host/method/authority that pvxs sees for that connection. Caching may be shared only for access-neutral state or must be keyed so an access-bearing handle is not reused across different credentials.

Fix direction:

Implement `ChannelSource::access()` or the `*_checked` methods on `QsrvPvStore`, and create channels through `create_channel_for()` using `ChannelContext`. Do not cache access-bearing `AnyChannel` only by PV name. Apply the same gate to native `pva_pvs`.

### BR-R2. QSRV single-record channels ignore requested field suffix and serve `VAL`

Severity: High functional / security-adjacent

Evidence:

- `crates/epics-bridge-rs/src/qsrv/provider.rs:718` parses `(record_name, _)` and drops the requested field.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:142` repeats the parse and ignores `_field`.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:185` reports `channel_name()` as the record name.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:203` reads `snapshot_for_field("VAL")`.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:251` writes `record.VAL` for process=false.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:260` writes field `"VAL"` for process/default PUT.
- pvxs opens the exact requested dbChannel in `$PVXS_HOME/ioc/singlesource.cpp:431`.
- pvxs tests field PVs such as `test:ai.DESC`, `test:ai.SCAN`, and `test:ai.RVAL` in `$PVXS_HOME/test/testqsingle.cpp:151`.

Impact:

A client can connect to `record.DESC`, `record.RVAL`, `record.SCAN`, `record.INP$`, or `record.PROC`, but Rust QSRV serves the `VAL` field shape and value. PUT also targets `VAL`. This breaks standard QSRV field-PV behavior and can become a security bug when field-specific ASL/writeability differs. For example, a read-only status-like field path can be transformed into a writable `VAL` write.

Wire-compatible expectation:

The channel name `record.FIELD` must bind to the EPICS dbChannel for that final field. Descriptor, value type, PUT target, monitor trigger, ASG/ASL, and writeability must be derived from that dbChannel.

Fix direction:

Store requested field information in `BridgeChannel`, not only the record name. Cache metadata by canonical channel name or `(record, field, modifier)` instead of by record. Use the final field for introspection, GET, PUT, monitor, and ASL resolution.

### BR-R3. QSRV PUT loses INIT pvRequest options `record._options.process` and `block`

Severity: High functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/channel.rs:61` parses process/block from a `PvStructure`.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:223` calls `PutOptions::from_pv_request(value)` on the PUT value payload.
- `crates/epics-bridge-rs/src/qsrv/group.rs:634` does the same for group PUT.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2313` decodes PUT INIT pvRequest.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2443` stores `OpState`, but only keeps mask/filter/pipeline/autoExec-like state.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2677` decodes the data-phase PUT value and calls `put_delta_checked(...)` at `:2713`.
- pvxs reads `record._options.process` from the INIT pvRequest in `$PVXS_HOME/ioc/iocsource.cpp:430`.
- pvxs tests `record[process=true|false|passive]` in `$PVXS_HOME/test/testqsingle.cpp:572` and `record[block=true]` in `:645`.

Impact:

Standard PVA clients send `record._options.process` and `record._options.block` in the PUT INIT pvRequest, not inside the value payload. Rust QSRV parses these options from the value payload, so normal clients cannot reliably request process inhibit, forced processing, or blocking put-notify behavior. This is visible on the wire with `pvput -r 'record[process=true]'` and similar calls.

Wire-compatible expectation:

PUT operation options are per-operation INIT state. The data-phase payload is the value/delta, not the source of QSRV process/block options.

Fix direction:

Extend `OpState` or the source API so decoded PUT INIT pvRequest options are passed into the QSRV source on each PUT. Keep `process`, `block`, and similar options separate from the value payload.

### BR-R4. QSRV ACF adapter collapses method/authority/roles and field ASL

Severity: High security

Evidence:

- `crates/epics-bridge-rs/src/qsrv/provider.rs:71` receives only `(channel, user, host)`.
- `crates/epics-bridge-rs/src/qsrv/provider.rs:79` calls `check_access_method(..., 0, "anonymous", "")`.
- `crates/epics-bridge-rs/src/qsrv/provider.rs:89` resolves only the record ASG and ignores final field ASL.
- pvxs preserves method-specific credentials and roles in `$PVXS_HOME/ioc/credentials.cpp:31`.
- pvxs supplies field ASL to access security through `dbChannelFldDes(ch)->as_level` in `$PVXS_HOME/ioc/securityclient.cpp:25`.

Impact:

Even when `AcfAccessControl` is installed, rules involving `METHOD`, `AUTHORITY`, roles, or nonzero ASL are not enforced like pvxs. The hardcoded method `"anonymous"` and ASL `0` can allow or deny the wrong clients. This is a security-visible behavior difference, not an implementation detail.

Wire-compatible expectation:

The access check must receive the same credential class and ASL that the C IOC would use for the requested dbChannel.

Fix direction:

Carry `account`, `host`, `method`, `authority`, roles, ASG, and field ASL through QSRV access checks. Prefer using the same `AccessGate`/`AccessChecked` path already present in `epics-pva-rs::server_native`.

### BR-R5. QSRV monitor ignores pvRequest DBE selection

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/monitor.rs:91` subscribes with `DbSubscription::subscribe(&self.db, &self.record_name)`.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:486` creates monitor subscriptions without passing monitor pvRequest options.
- `crates/epics-pva-rs/src/server_native/source.rs:645` `subscribe()` takes only a PV name.
- pvxs parses `record._options.DBE` in `$PVXS_HOME/ioc/singlesource.cpp:115`.
- pvxs uses the selected DBE mask for the value subscription in `$PVXS_HOME/ioc/singlesource.cpp:155`.

Impact:

A client can request monitor options such as `record[DBE=ARCHIVE]`, `record[DBE=VALUE]`, or `record[DBE=ALARM]`, but Rust QSRV cannot route that option to the DB subscription. The selected fields may still be masked in the emitted structure, but update triggering cadence and archive/alarm filtering differ from pvxs.

Wire-compatible expectation:

The monitor INIT pvRequest must affect the database event mask used to create the subscription.

Fix direction:

Preserve monitor INIT pvRequest options in the server op state and pass DBE selection into QSRV monitor construction. Extend `DbSubscription` or add a QSRV-specific subscription path that accepts the DBE mask.

### BR-R6. PVA gateway reserializes downstream PUTs through string `pvput`

Severity: High functional / wire compatibility

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:330` converts `PvField` to a string.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:332` calls `client().pvput(name, &value_str)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:376` repeats the string conversion for credential-aware PUT.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:753` supports only scalar/scalar-array and structures with a field literally named `value`.
- `crates/epics-pva-rs/src/client_native/context.rs:432` already exposes `pvput_pv_field()` for typed PUT.
- pvxs client PUT sends typed wire value through `to_wire_valid(R, temp)` in `$PVXS_HOME/src/clientget.cpp:297`.

Impact:

The PVA gateway is not wire-transparent for PUT. Nested structures, group-like structures without a top-level `value`, union/variant payloads, partial update semantics, and string arrays containing spaces can be rejected or altered. This is a gateway data-plane behavior difference that clients can observe.

Wire-compatible expectation:

A PVA-to-PVA gateway should forward the typed value/delta semantics, not reconstruct a CLI string and parse it again upstream.

Fix direction:

Use `PvaClient::pvput_pv_field()` or add a typed client API that accepts descriptor, changed BitSet, delta, and original pvRequest. Remove string serialization from the gateway data plane.

### BR-R7. PVA gateway upstream credential pool is unbounded and keyed by client-controlled identity

Severity: Medium security / DoS

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:87` defines a per-identity upstream client pool.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:137` initializes it as an empty `HashMap`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:239` chooses the upstream client from downstream `ChannelContext`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:243` keys the pool by `(ctx.account, ctx.method)`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:248` creates a new `PvaClient` and `:254` inserts it without cap, TTL, or idle cleanup.

Impact:

A downstream client that can vary account/method values can force unbounded upstream `PvaClient` allocation. The gateway has limits for subscribers and caches, but the identity-specific upstream pool has no corresponding resource limit.

Wire-compatible expectation:

Wire compatibility does not require unbounded per-credential upstream clients. A gateway must bound remote-triggerable memory and connection growth.

Fix direction:

Add `max_upstream_identities`, idle TTL, cleanup, and rejection/audit for identities past the cap. Perform gateway-side ACF before allocating a per-identity upstream client.

### BR-R8. PVA gateway does not preserve downstream auth method/authority upstream

Severity: Medium security / audit fidelity

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:248` builds an upstream `PvaClient` with only `.user(ctx.account)` and `.host(ctx.host)`.
- `crates/epics-pva-rs/src/client_native/server_conn.rs:233` chooses upstream auth as `"ca"` if offered, otherwise `"anonymous"`.
- `crates/epics-pva-rs/src/client_native/server_conn.rs:247` sends the negotiated upstream method, user, and host.
- `crates/epics-pva-rs/src/server_native/source.rs:30` carries downstream method, and `:34` carries authority, but the upstream builder path drops them.
- Gateway-side `build_gate()` resolves ASL as `0` in `crates/epics-bridge-rs/src/pva_gateway/source.rs:149`.

Impact:

A downstream `x509` or other authenticated method can be forwarded upstream as CA-style credentials with the same account string, or as anonymous if upstream does not advertise CA. Upstream `METHOD`/`AUTHORITY` rules and audit logs do not see the original downstream method/authority. If this is an intentional trust-boundary design, it must be documented as identity assertion by the gateway, not transparent credential pass-through.

Wire-compatible expectation:

The gateway should either preserve auth method/authority where the protocol and upstream configuration allow it, or explicitly expose that it converts downstream identity into a CA-style assertion.

Fix direction:

Add upstream auth configuration that can preserve method/authority where possible. If preserving is not supported, document the gateway trust model and make it visible in config and audit output.

### BR-R9. QSRV launcher accepts `config.acf` but does not install it on `BridgeProvider`

Severity: High security

Evidence:

- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:539` creates `BridgeProvider::new(db.clone())`.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:540` does not call an ACF/access setter before wrapping it in `QsrvPvStore`.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:596` passes `config.acf.clone()` to the CA server.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:611` passes `config.acf` into `PvaServer::from_parts`.
- `crates/epics-pva-rs/src/server/pva_server.rs:176` installs ACF only when `run()` creates the default `PvDatabaseSource`.
- `crates/epics-pva-rs/src/server/pva_server.rs:192` documents that caller-supplied `ChannelSource` must install ACF itself.

Impact:

The dual CA/PVA QSRV IOC runner gives the appearance that `config.acf` protects both protocols. CA is protected through `CaServer::from_parts`, but the PVA QSRV path runs through a custom `QsrvPvStore` whose provider remains `AllowAllAccess` unless a separate caller configured it. This is a configuration trap with direct security impact.

Wire-compatible expectation:

An IOC launched with one ACF should enforce the same site policy on CA and PVA QSRV paths unless the user explicitly configures otherwise.

Fix direction:

Install `AcfAccessControl` on `BridgeProvider` inside `run_ca_pva_qsrv_ioc()` when `config.acf` is present, and combine it with the credential-aware `ChannelSource` fix from BR-R1. Without BR-R1, installing ACF still uses anonymous/empty identity.

### BR-R10. DB JSON pvalink options are reduced to only the PV name

Severity: High functional

Evidence:

- `crates/epics-base-rs/src/server/record/link.rs:210` enters JSON link parsing.
- `crates/epics-base-rs/src/server/record/link.rs:239` recognizes `ca` / `pva`.
- `crates/epics-base-rs/src/server/record/link.rs:244` extracts only `pv`.
- `crates/epics-base-rs/src/server/record/link.rs:248` returns `ParsedLink::Pva(pv)` with no `field`, `proc`, `sevr`, `Q`, `pipeline`, `defer`, `retry`, `always`, `local`, or `atomic`.
- `crates/epics-bridge-rs/src/pvalink/config.rs:148` parses Rust-specific query-style `pva://PV?...` options, but the DB JSON object has already been reduced before this config parser can see it.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:233` has `open_link_for_record()`, but `rg open_link_for_record` shows only tests and no DB-loader/record-init caller.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:590` installs the pvalink resolver, but does not scan loaded records and register the full DB link object.
- pvxs parses the JSON pvalink option set in `$PVXS_HOME/ioc/pvalink_jlif.cpp:24`.
- pvxs test coverage requires JSON `field` in `$PVXS_HOME/test/testpvalink.cpp:89`, JSON `proc` in `:112`, and JSON `sevr` in `:138`.

Impact:

Standard pvxs DB syntax is accepted syntactically by Rust but loses the behavior attached to the link. An input link such as `{"pva":{"pv":"target:ai","field":"display.precision","proc":"CPP","sevr":"MS"}}` becomes a default pvalink to `target:ai`. That changes record processing, alarm propagation, field extraction, and queue behavior visible to IOC records and PVA clients.

Wire-compatible expectation:

The DB JSON pvalink object is the operator-facing contract. Rust can store it in a different internal representation, but the resulting monitor, read, write, alarm, and scan behavior must match the options in that object.

Fix direction:

Make `ParsedLink::Pva` carry structured pvalink options or add a DB-load registration phase that passes the original JSON link object to `PvaLinkConfig`. Register `open_link_for_record()` for INP links during record initialization so `CP` / `CPP` scan targets and per-link options are installed without iocsh pre-warming.

### BR-R11. pvalink OUT writes do not preserve `field`, `proc`, `block`, or deferred option semantics from the DB link

Severity: High functional / wire

Evidence:

- `crates/epics-base-rs/src/server/database/links.rs:465` routes external OUT writes as `(name, EpicsValue)` only.
- `crates/epics-base-rs/src/server/database/links.rs:501` calls `lset.put_value(body, value)` with no original link object or wait/block state.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:652` implements `put_value()`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:659` builds a fresh default `PvaLinkConfig` and hard-codes `process: true`.
- `crates/epics-bridge-rs/src/pvalink/link.rs:294` sends scalar OUT values via `client.pvput(&self.config.pv_name, value_str)`.
- `crates/epics-bridge-rs/src/pvalink/link.rs:318` sends array OUT values via `client.pvput_pv_field(&self.config.pv_name, value)`.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:427` makes default `op_put()` call `op_put_inner(..., None, ...)`.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:646` turns a missing raw pvRequest into `build_pv_request_value_only()`, so no `record._options.process` or `block` reaches the upstream server.
- pvxs builds a PUT pvRequest containing `record._options.block` and `record._options.process` in `$PVXS_HOME/ioc/pvalink_channel.cpp:31`.
- pvxs computes `process` from each link's `proc` option in `$PVXS_HOME/ioc/pvalink_channel.cpp:237`.
- pvxs sends the raw request on the upstream PUT in `$PVXS_HOME/ioc/pvalink_channel.cpp:268`.
- pvxs queues and waits for OUT puts through `$PVXS_HOME/ioc/pvalink_lset.cpp:594`.

Impact:

Rust pvalink OUT cannot express pvxs `proc=NPP/PP/CP/CPP`, `block`, DB JSON `field`, or DB JSON `defer/retry` through the normal record OUT path. Remote processing and completion semantics differ for linked output records. Because BR-R3 also shows the Rust QSRV server side loses process/block options, both ends of a Rust-to-Rust pvalink setup miss the same protocol contract.

Wire-compatible expectation:

An OUT pvalink must issue the same typed PVA PUT that a pvxs pvalink would issue, including the PUT INIT pvRequest and the selected remote field.

Fix direction:

Carry the parsed `PvaLinkConfig` through `ParsedLink` / `LinkSet::put_value`. Use `pvput_with_request()` or a typed equivalent that accepts `record._options.process` and `block`, and use a field-targeted typed PUT when the link config names a sub-field.

### BR-R12. QSRV NTScalar/NTScalarArray metadata shape differs from pvxs

Severity: Medium functional / wire ABI

Evidence:

- `crates/epics-bridge-rs/src/qsrv/pvif.rs:165` builds NTScalarArray values with `value`, `alarm`, `timeStamp`, and optional `display` only.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:351` builds NTScalarArray descriptors with `value`, `alarm`, `timeStamp`, and `display`; it omits `control` and `valueAlarm`.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:472` builds `display.limitLow` and `display.limitHigh` as `double`.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:495` represents `display.form` as scalar `int`, not `enum_t`.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:501` builds `control` limits as `double`.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:566` builds `valueAlarm` with only four double limit fields.
- pvxs expects scalar `display.form` to be `enum_t` with `index` and `choices` in `$PVXS_HOME/test/testqsingle.cpp:100`.
- pvxs expects scalar `valueAlarm` fields including `active`, alarm severities, and `hysteresis` in `$PVXS_HOME/test/testqsingle.cpp:116`.
- pvxs expects NTScalarArray `display`, `control`, and `valueAlarm` in `$PVXS_HOME/test/testqsingle.cpp:354`.
- pvxs expects integer array metadata limits to use integer scalar types, for example `int32_t` in `$PVXS_HOME/test/testqsingle.cpp:399`.
- pvxs expects unsigned 64-bit array metadata limits to use `uint64_t` in `$PVXS_HOME/test/testqsingle.cpp:530`.

Impact:

Clients that compare descriptors or bind to pvxs normative type shapes will observe different field sets and scalar types from Rust QSRV. Missing `control` / `valueAlarm` on arrays and hard-coded double metadata are wire ABI differences, not internal implementation details.

Wire-compatible expectation:

QSRV should build metadata descriptors and values from the final DBF type and the same metadata fields pvxs exposes for the record field.

Fix direction:

Build `display`, `control`, and `valueAlarm` descriptors from the member DBF type. Represent `display.form` as `enum_t`. Include the full pvxs `valueAlarm` field set and emit `control` / `valueAlarm` on NTScalarArray where pvxs does.

### BR-R13. Unsigned 64-bit EPICS fields are not represented through QSRV

Severity: Medium functional / data correctness

Evidence:

- `crates/epics-base-rs/src/types/value.rs:8` defines `EpicsValue` without `UInt64` or `UInt64Array`.
- `crates/epics-base-rs/src/types/dbr.rs:73` defines `DbFieldType` without `UInt64`.
- `crates/epics-bridge-rs/src/convert.rs:18` maps `DbFieldType::Int64` to PVA signed `long`.
- `crates/epics-bridge-rs/src/convert.rs:35` maps `EpicsValue::Int64` to `ScalarValue::Long`.
- `crates/epics-base-rs/src/server/records/waveform.rs:133` documents waveform menu indices including `UINT64=8`.
- `crates/epics-base-rs/src/server/records/waveform.rs:137` maps FTVL `1|2`, `3|4`, `5|6`, and `9`; FTVL `7` / `8` fall through to `DoubleArray`.
- `crates/epics-base-rs/src/server/record/record_instance.rs:642` documents `UTAG` as C `DBF_UINT64` but models it as signed `Int64`.
- pvxs tests `uint64_t` scalar/array behavior in `$PVXS_HOME/test/testqsingle.cpp:511`.
- pvxs expects `test:wf:u64` to return `uint64_t[]` and `uint64_t` metadata in `$PVXS_HOME/test/testqsingle.cpp:530`.

Impact:

Records and fields that are unsigned 64-bit in EPICS cannot be wire-compatible through Rust QSRV. Values above `i64::MAX` are not representable, and values in signed range are advertised as signed `int64_t` rather than `uint64_t`.

Wire-compatible expectation:

PVA supports `ulong` / `ulong[]`; QSRV should expose C `DBF_UINT64` / `DBR_UINT64` fields as those PVA types.

Fix direction:

Add `UInt64` / `UInt64Array` and `DbFieldType::UInt64`, then propagate them through DB parsing, record storage, CA/PVA conversion, QSRV descriptors, PUT conversion, waveform FTVL handling, and timestamp/user-tag handling.

### BR-R14. PVA gateway monitor fanout is not pvRequest-transparent

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:478` creates one upstream monitor entry per PV name.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:523` calls `pvmonitor_raw_frames_handle(&pv_name_owned, ...)`.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:1068` accepts fields and pipeline size for raw monitor handles.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:1168` builds a pvRequest from only the upstream handle's field list / pipeline size; the gateway cache call does not pass the downstream client's monitor pvRequest.
- `crates/epics-pva-rs/src/server_native/source.rs:277` defines `subscribe_raw(name)` without request options.
- `crates/epics-pva-rs/src/server_native/source.rs:287` defines `subscribe_raw_checked(checked, ctx)` without request options.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:542` implements raw subscribe by attaching to the cached upstream raw event broadcast.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:624` implements decoded subscribe from the same cache.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2313` decodes each downstream monitor pvRequest, but the `ChannelSource` subscribe API has no way to pass event-affecting options to the gateway upstream monitor.

Impact:

A downstream client can get local field masking, but the upstream event source is the gateway's single default monitor. Options that affect event production upstream, such as DBE selection, server-side filters, and pipeline queue semantics, are not transparent through the gateway. Clients monitoring through the gateway can receive events that a direct upstream monitor would not have produced, or miss event-class behavior a direct connection would have requested.

Wire-compatible expectation:

A PVA-to-PVA gateway should either be transparent for event-affecting monitor pvRequest options or explicitly document itself as a cache/fanout gateway with reduced monitor semantics.

Fix direction:

Key upstream monitor cache entries by the event-affecting pvRequest options, not only PV name, or add a documented mode that rejects unsupported monitor options instead of silently using a default upstream subscription.

### BR-R15. QSRV atomic group PUT is not DBManyLock-equivalent

Severity: Medium functional / consistency

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group.rs:640` enters the atomic PUT path based on the group definition.
- `crates/epics-bridge-rs/src/qsrv/group.rs:650` serializes only same-group Rust PUTs with `atomic_write_lock`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:657` documents that plain CA/PVA writes to backing records are not gated by that lock.
- `crates/epics-bridge-rs/src/qsrv/group.rs:694` applies member writes one at a time after conversion.
- pvxs creates DBManyLock objects for group value/properties channels in `$PVXS_HOME/ioc/groupconfigprocessor.cpp:1165`.
- pvxs atomic GET holds `DBManyLocker` over the group value lock in `$PVXS_HOME/ioc/groupsource.cpp:492`.
- pvxs atomic PUT holds `DBManyLocker` over the group value lock in `$PVXS_HOME/ioc/groupsource.cpp:621`.

Impact:

Rust prevents two Rust group PUT operations to the same group from interleaving, but it does not prevent a direct write to a backing member record from landing between member writes. pvxs atomic PUT holds the database locks that also protect direct record access. A client can observe or create a half-applied group state under Rust in cases pvxs excludes.

Wire-compatible expectation:

For QSRV group atomic PUT, the externally observable transition should be protected against direct backing-record writers, not only against another PUT through the same group PV.

Fix direction:

Add a database-level multi-record write lock API or a group-member write owner that routes direct member writes through the same lock. Keep the current `atomic_write_lock` only as an internal serialization aid until DB-level locking exists.

### BR-R16. QSRV group ignores per-operation `record._options.atomic`

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group.rs:615` receives a GET request but calls `read_group()` without parsing `record._options.atomic`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:622` filters the already-read full value by pvRequest after atomicity has already been chosen.
- `crates/epics-bridge-rs/src/qsrv/group.rs:634` parses PUT options through `PutOptions::from_pv_request()`.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:41` defines `PutOptions` with only `process` and `block`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:640` chooses PUT atomicity from `self.def.atomic`, not the operation pvRequest.
- pvxs GET starts with the group default and lets `record._options.atomic` override it in `$PVXS_HOME/ioc/groupsource.cpp:480-481`.
- pvxs PUT does the same in `$PVXS_HOME/ioc/groupsource.cpp:203-204`.

Impact:

Clients cannot request atomic or non-atomic group behavior per operation through Rust QSRV. A client that uses the pvxs pvRequest option to force one mode will observe group default behavior instead.

Wire-compatible expectation:

`record._options.atomic` is part of the group operation pvRequest contract and should affect the operation even when the group has a configured default.

Fix direction:

Parse group GET and PUT `record._options.atomic` from the INIT pvRequest, pass it to `read_group()` / `put()`, and include the selected atomic value in the returned `record._options.atomic` field.

### BR-R17. QSRV group PUT checks group PV access but not member field access

Severity: High security

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group.rs:627` checks `self.access.can_write(&self.def.name)`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:694` then iterates group members and writes backing records.
- `crates/epics-bridge-rs/src/qsrv/group.rs:705` calls `put_record_field_from_ca(record_name, field_name, epics_val)`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:710` calls `put_pv(&format!("{record_name}.{field_name}"), epics_val)`.
- There is no per-member ACF check between the group-level `can_write()` and the backing record writes.
- pvxs creates per-field security clients for the caller credentials in `$PVXS_HOME/ioc/groupsource.cpp:215-216`.
- pvxs initializes each security client from the member `dbChannel` in `$PVXS_HOME/ioc/groupsource.cpp:219-221`.
- pvxs performs field pre-processing through the per-member `SecurityClient` before the write in `$PVXS_HOME/ioc/groupsource.cpp:565`.
- pvxs also runs `IOCSource::doPreProcessing()` for each member before group PUT in `$PVXS_HOME/ioc/groupsource.cpp:600`.

Impact:

If the group PV is writable for a user but a backing member field is not writable for that user, Rust can still modify the member through the group. pvxs evaluates member `dbChannel` security during group PUT, so this is a security-relevant compatibility gap.

Wire-compatible expectation:

Group-level access should not bypass the access rules of the member fields being changed. A group PUT must be denied or partially rejected according to the same per-member ACF decisions pvxs applies.

Fix direction:

During group channel creation, build member access contexts keyed by the caller identity and each member channel. Before applying a member value, require that member's write access. Report a PUT error when the client attempted to change only denied members, matching pvxs's remote error behavior.

### BR-R18. pvalink `atomic` scan-on-update lacks a multi-record lock epoch

Severity: Medium functional / consistency

Evidence:

- `crates/epics-bridge-rs/src/pvalink/integration.rs:526` documents atomic target grouping for scan-on-update.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:551` sorts scan targets so atomic targets run first.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:566` processes each target record one by one.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:581` calls `process_record_with_links()` per record with no multi-record lock.
- pvxs groups atomic pvalink scan targets and builds `DBManyLock` over their records in `$PVXS_HOME/ioc/pvalink_channel.cpp:386`.
- pvxs holds `DBManyLocker` while scanning atomic targets in `$PVXS_HOME/ioc/pvalink_channel.cpp:422`.

Impact:

Rust preserves ordering, but not the lock epoch. A direct record writer, another scan, or a linked side effect can interleave between atomic pvalink target records. pvxs locks the atomic target records together before scanning them.

Wire-compatible expectation:

The pvalink `atomic` option should give clients and records a single locked scan epoch for the linked target set, not only sorted execution.

Fix direction:

Reuse the same database-level multi-record lock needed for BR-R15, then route pvalink atomic scan targets through that lock before processing the target records.

### BR-R19. pvalink `time` option is not available through Rust record links

Severity: Medium functional

Evidence:

- pvxs documents the pvalink `time` option in `$PVXS_HOME/ioc/pvalink_jlif.cpp:35`.
- pvxs parses `time` as a boolean in `$PVXS_HOME/ioc/pvalink_jlif.cpp:104`.
- pvxs latches the remote NT timestamp during `pvaGetValue()` in `$PVXS_HOME/ioc/pvalink_lset.cpp:394`.
- pvxs copies the latched remote timestamp into the owning record when `time` is set in `$PVXS_HOME/ioc/pvalink_lset.cpp:427`.
- pvxs tests the link timestamp contract in `$PVXS_HOME/test/testpvalink.cpp:387` and `$PVXS_HOME/test/testpvalink.cpp:412`.
- `crates/epics-bridge-rs/src/pvalink/config.rs:70` defines `PvaLinkConfig` without a `time` field.
- `crates/epics-bridge-rs/src/pvalink/config.rs:176` parses `field`, then later options, but no `time` option.
- `crates/epics-base-rs/src/server/database/link_set.rs:87` has a `time_stamp()` hook.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:727` implements that hook for pvalink.
- `rg "time_stamp\\(" crates/epics-base-rs/src/server/database crates/epics-base-rs/src/server/record` shows no record processing path consuming the hook for pvalink timestamp propagation.

Impact:

Rust can display a remote pvalink timestamp through diagnostics, but it cannot make a record adopt the remote timestamp through the DB link `time` option. IOC logic and downstream clients that depend on source timestamps observe local processing time instead of the linked PV's timestamp.

Wire-compatible expectation:

When a pvxs-compatible DB link sets `time=true`, the owning record timestamp should follow the linked PV's NT `timeStamp`.

Fix direction:

Add `time` to `PvaLinkConfig`, preserve it from DB JSON and query-style parsing, and update record processing to use `LinkSet::time_stamp()` only when the specific parsed link requested remote timestamp propagation.

### BR-R20. QSRV `process=passive` is handled like `process=true`

Severity: High functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/channel.rs:23` defines `ProcessMode::Passive`, `Force`, and `Inhibit`.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:78` maps unsupported strings and `"passive"` to `ProcessMode::Passive`.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:247` handles `ProcessMode::Force | ProcessMode::Passive` through the same write path.
- `crates/epics-base-rs/src/server/database/field_io.rs:653` always calls `process_record_with_links()` after `put_record_field_from_ca()`.
- pvxs reads PUT pvRequest options in `$PVXS_HOME/ioc/singlesource.cpp:339` and calls `IOCSource::setForceProcessingFlag()`.
- pvxs maps `record._options.process == "passive"` to the unset force-processing state in `$PVXS_HOME/ioc/iocsource.cpp:440-443`.
- pvxs only post-processes passive PUTs when the target field has `process_passive`, the record `SCAN` is Passive, the final DBR type is below `DBR_PUT_ACKT`, or the target is `PROC`, in `$PVXS_HOME/ioc/iocsource.cpp:397`.

Impact:

A PVA PUT with `record._options.process="passive"` can process records that pvxs would only write. That changes FLNK execution, device support side effects, alarm recalculation, and monitor emission. It also collapses `process=true` and `process=passive` for Rust QSRV single-record channels.

Wire-compatible expectation:

`process=passive` means "process only when the DB field and passive-scan rules require it"; it is not equivalent to force processing.

Fix direction:

Preserve the INIT pvRequest options from BR-R3, then distinguish `Passive` from `Force` in the database write API. The passive path needs a `dbPut` plus pvxs-equivalent `doPostProcessing()` gate using the target field's `process_passive` property, record `SCAN`, and final DBR type.

### BR-R21. PVA gateway READ/MONITOR upstream authorization uses the shared cache client

Severity: High security

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:231` stores one shared `Arc<PvaClient>` in `ChannelCache`.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:363` looks cache entries up by PV name only.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:398` creates one upstream monitor entry per PV name.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:488` clones the shared client into the upstream monitor task.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:523` opens the upstream raw monitor with that shared client.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:295` serves GET from the cached monitor snapshot.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:542` and `:624` attach MONITOR subscribers to the cached raw/decoded broadcasts.
- `crates/epics-pva-rs/src/server_native/source.rs:138` enforces downstream read access, then calls ctx-less `get_value()`.
- `crates/epics-pva-rs/src/server_native/source.rs:250` and `:287` enforce downstream read access, then call ctx-less `subscribe()` / `subscribe_raw()`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:347` and `:443` override PUT/RPC checked paths to use downstream credentials, showing READ/MONITOR do not use that credential-aware path.

Impact:

If the gateway-side ACF is disabled or less restrictive than the upstream IOC's ACF, upstream GET/MONITOR authorization is evaluated for the gateway's shared client rather than the downstream client. Once the cache opens a monitor, later downstream readers with different credentials receive values from the same upstream subscription.

Wire-compatible expectation:

A gateway may enforce policy locally, forward policy upstream, or both, but the chosen trust boundary must be explicit. It must not silently turn per-client upstream read/monitor authorization into one shared gateway-client authorization while still presenting itself as a transparent PVA gateway.

Fix direction:

Either require gateway-side ACF as the authoritative READ/MONITOR policy and document that upstream read security sees the gateway identity, or key upstream cache entries by downstream credential policy and use `upstream_client_for(ctx)` for GET/MONITOR as well. If keyed by credential, add a cap for per-credential monitor cache growth.

### BR-R22. QSRV NTEnum uses `ushort` index and omits `display`

Severity: High functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/pvif.rs:120` builds QSRV NTEnum runtime values.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:144` emits `value.index` as `ScalarValue::UShort`.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:149` emits only `value`, `alarm`, and `timeStamp`.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:330` builds the QSRV NTEnum descriptor.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:339` declares `value.index` as `ScalarType::UShort`.
- `crates/epics-pva-rs/src/nt/enum_t.rs:29` builds NTEnum with `value.index` as `ScalarType::Int` and includes `display.description`.
- pvxs QSRV single-record tests expect `value.index int32_t` and `display.description` in `$PVXS_HOME/test/testqsingle.cpp:174`.
- pvxs monitor tests expect enum monitor deltas with `value.index int32_t` and `display.description` in `$PVXS_HOME/test/testqsingle.cpp:790`.

Impact:

Enum descriptor and value shape are wire-incompatible with pvxs. Clients that validate NTEnum descriptors or decode `enum_t.index` as `int32_t` can reject Rust QSRV enum PVs or misdecode them. The missing `display.description` also changes the standard NTEnum field set that pvxs exposes.

Wire-compatible expectation:

QSRV NTEnum must use `int32` for `value.index` and include the pvxs-visible display description field.

Fix direction:

Change the QSRV NTEnum descriptor/value builder to use `ScalarType::Int` / `ScalarValue::Int` for `value.index`, and include `display.description` in both descriptor and value payloads. Keep it consistent with `epics-pva-rs/src/nt/enum_t.rs`.

### BR-R23. pvalink INP conversion supports only scalar, `double[]`, and `int32[]`

Severity: High functional

Evidence:

- `crates/epics-bridge-rs/src/pvalink/integration.rs:767` labels `pvfield_to_epics_value()` as best-effort conversion.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:783` supports scalar arrays only when the first element is `Double` or `Int`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:815` rejects other array element types.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:822` converts unsigned scalar values by narrowing to signed `Long` or `Short`.
- pvxs pvalink handles string arrays and numeric array DBR types including signed/unsigned 8/16/32/64-bit, float32, and float64 in `$PVXS_HOME/ioc/pvalink_lset.cpp:287`.
- pvxs tests cover numeric array conversion in `$PVXS_HOME/test/testpvalink.cpp:235`.
- pvxs tests cover string array pvalink reads in `$PVXS_HOME/test/testpvalink.cpp:260`.

Impact:

Rust pvalink INP cannot read many PVA waveform/array types into EPICS records. String arrays, float32 arrays, int16 arrays, char/uchar arrays, unsigned arrays, and 64-bit arrays fail or truncate. This changes linked record behavior visible through the IOC database and downstream PVA/CA clients.

Wire-compatible expectation:

pvalink array reads must convert the remote scalar-array value according to the target DBR/DBF type, not only according to the source array's first Rust enum variant.

Fix direction:

Extend conversion to be target-typed and support the pvxs DBR array set: string, signed/unsigned 8/16/32/64-bit, float32, and float64. Avoid scalar unsigned narrowing; BR-R13 needs to be closed for full unsigned 64-bit parity.

### BR-R24. pvalink DB link metadata hooks are mostly absent

Severity: Medium functional

Evidence:

- `crates/epics-base-rs/src/server/database/link_set.rs:44` defines `LinkSet` with value, alarm, and timestamp hooks only.
- `crates/epics-base-rs/src/server/database/link_set.rs:87` has a timestamp hook, but no DBF type, element count, control limits, graphic limits, alarm limits, precision, or units hooks.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:702` implements alarm-message propagation.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:711` implements alarm-severity propagation.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:727` implements timestamp access.
- `rg "GraphicLimits|ControlLimits|AlarmLimits|Precision|Units|Nelements|DBFtype" crates/epics-base-rs/src/server/database crates/epics-base-rs/src/server/record crates/epics-bridge-rs/src/pvalink` finds no lset-style metadata hooks.
- pvxs installs `pvaGetDBFtype`, `pvaGetElements`, `pvaGetControlLimits`, `pvaGetGraphicLimits`, `pvaGetAlarmLimits`, `pvaGetPrecision`, and `pvaGetUnits` in `$PVXS_HOME/ioc/pvalink_lset.cpp:700`.
- pvxs tests require linked graphic/control/alarm limits, precision, units, and element count in `$PVXS_HOME/test/testpvalink.cpp:416`.

Impact:

Rust record links can read a linked value and some alarm/timestamp data, but they do not expose remote display/control/valueAlarm metadata through DB link APIs. Records and clients that depend on linked precision, units, limits, DBF type, or element count observe default or local metadata instead of the linked PV metadata.

Wire-compatible expectation:

pvalink should make the same remote metadata available to record support and DB link callers that pvxs exposes through its lset.

Fix direction:

Extend `LinkSet` with metadata hooks, or introduce a structured link snapshot that carries value, DBF type, element count, display/control/valueAlarm, precision, units, alarm, timestamp, and user tag. Populate it from the cached NT value in pvalink.

### BR-R25. QSRV group root meta mapping (`""`) is dropped

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group_config.rs:250` parses `+type:"meta"` as `FieldMapping::Meta`.
- `crates/epics-bridge-rs/src/qsrv/group_config.rs:325` stores `field_name` exactly as parsed, so an empty key remains `""`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:502` builds meta fields containing `alarm` and `timeStamp`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:127` returns immediately when `set_nested_field()` receives an empty path.
- `crates/epics-bridge-rs/src/qsrv/group.rs:195` returns immediately when `set_nested_field_desc()` receives an empty path.
- `crates/epics-bridge-rs/src/qsrv/group.rs:357` reads each member and calls `set_nested_field(&mut pv, &member.field_name, field)`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:811` builds descriptors through `set_nested_field_desc()`.
- pvxs uses root meta mapping in `$PVXS_HOME/test/ntenum.db:6` with `"":{+type:"meta", +channel:"VAL"}`.
- pvxs group tests expect root `alarm` and `timeStamp` for that NTEnum in `$PVXS_HOME/test/testqgroup.cpp:168` and monitor updates in `:201`.

Impact:

Rust QSRV groups that use pvxs's root meta mapping lose root `alarm` and `timeStamp` fields and descriptors. The pvxs NTEnum group example becomes structurally different even when the value fields are present.

Wire-compatible expectation:

An empty-path `+type:"meta"` member should merge its metadata fields into the group root.

Fix direction:

Teach empty-path meta to merge `alarm` and `timeStamp` into the root value and descriptor instead of no-oping. Do not generalize that behavior blindly to root `+type:"const"`; pvxs currently rejects root const in `$PVXS_HOME/ioc/groupconfigprocessor.cpp:596`.

### BR-R26. QSRV group constant syntax uses `+value`, not pvxs `+const`

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group_config.rs:70` documents constant values as coming from `+value`.
- `crates/epics-bridge-rs/src/qsrv/group_config.rs:290` requires `+value` for `+type=const`.
- `crates/epics-bridge-rs/src/qsrv/group_config.rs:713` tests the Rust-only `+value` spelling.
- pvxs JSON group config uses `{"+type":"const", "+const":3}` in `$PVXS_HOME/test/qgroup.json:1`.
- pvxs DB group config uses `+const` in `$PVXS_HOME/test/const.db:2`.
- pvxs tests exercise constant group output in `$PVXS_HOME/test/testqgroup.cpp:682`.

Impact:

pvxs-compatible group configs using `+const` are rejected by Rust or lose the constant value. Operators cannot load existing pvxs DB/JSON group definitions without rewriting them to Rust-specific syntax.

Wire-compatible expectation:

QSRV group config should accept the pvxs `+const` spelling for constant members.

Fix direction:

Accept `+const` as the canonical pvxs-compatible spelling. Keeping `+value` as an alias is acceptable, but diagnostics and docs should point operators to `+const`.

### BR-R27. pvalink cache key drops per-link `field` and option state

Severity: High functional

Evidence:

- `crates/epics-bridge-rs/src/pvalink/registry.rs:15` keys the registry only by `(String, LinkDirection)`.
- `crates/epics-bridge-rs/src/pvalink/registry.rs:53` returns an existing `PvaLink` for the same PV name and direction, regardless of `field`, `sevr`, `Q`, `pipeline`, `proc`, `atomic`, or `monorder`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:86` stores one `PvaLinkConfig` per PV name in `link_options`.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:294` overwrites that per-PV config when another link to the same PV is opened.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:340` starts the notification forwarder with `link.config().field`, so the first cached link's field selector drives change detection for all scan targets on that PV.
- `crates/epics-bridge-rs/src/pvalink/link.rs:242` and `:260` extract the cached link's configured field from read paths.
- pvxs keeps per-link options in `pvaLinkConfig` fields including `fieldName`, `queueSize`, `proc`, `sevr`, `time`, `retry`, `atomic`, and `monorder` in `$PVXS_HOME/ioc/pvalink.h:65`.
- pvxs shares an upstream channel by `(channelName, pvRequest)` in `$PVXS_HOME/ioc/pvalink_lset.cpp:99`, while each attached `pvaLink` still keeps its own `fieldName`.
- pvxs resolves each link's own `fieldName` from the shared root in `$PVXS_HOME/ioc/pvalink_link.cpp:91`.
- pvxs tests use two pvalinks to the same PV with different fields `a` and `b` in `$PVXS_HOME/test/testpvalink.db:174`.

Impact:

Two Rust pvalinks to the same upstream PV cannot independently select different subfields or link options. The first or last opened config wins depending on the path: cached `PvaLink` state keeps the first link's field, while `link_options` can be overwritten by a later link. Records that should read `field:"b"` can read `field:"a"`, and scan-on-update change detection can be based on the wrong leaf.

Wire-compatible expectation:

The upstream channel or monitor may be shared, but `field`, severity mode, processing policy, scan order, atomic flag, and metadata behavior are per DB link.

Fix direction:

Separate the shared upstream channel/monitor cache from per-link state. Key the monitor by `(pv_name, pvRequest-affecting options)` and keep each DB link's selector/options in an attached link object, matching pvxs's `pvaLinkChannel` plus per-link `pvaLink` split.

### BR-R28. pvalink `proc=CPP` loses the passive-scan gate

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/pvalink/config.rs:189` and `:193` parse `CP` and `CPP` into the same `monitor=true` / `scan_on_update=true` state.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:115` stores scan targets with `always`, `monorder`, and `atomic`, but no `CP` vs `CPP` passive flag.
- `crates/epics-bridge-rs/src/pvalink/integration.rs:566` processes every selected target after a changed monitor event, with no `SCAN=Passive` check.
- pvxs distinguishes `CP` from `CPP` in `$PVXS_HOME/ioc/pvalink_link.cpp:122`: `CP` returns `scanOnUpdateYes`, while `CPP` returns `scanOnUpdatePassive`.
- pvxs applies the passive check in `$PVXS_HOME/ioc/pvalink_channel.cpp:313`: `CPP` skips processing when `prec->scan != 0`.

Impact:

Rust `proc=CPP` can process records that pvxs would leave alone because their local `SCAN` is not Passive. That changes local record side effects, output links, and FLNK chains after remote monitor updates.

Wire-compatible expectation:

`CP` means process on monitor update; `CPP` means process on monitor update only when the owning record is Passive.

Fix direction:

Carry a `scan_on_update_passive` flag through `PvaLinkConfig` and `ScanTarget`. In the notification forwarder, skip `CPP` targets whose current record `SCAN` is not Passive. Keep `always` as the no-op-update option; it is not a substitute for the passive-scan gate.

### BR-R29. QSRV group trigger mapping collapses default/self and explicit target semantics

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group_config.rs:309` maps a missing `+trigger` to `TriggerDef::All`.
- `crates/epics-bridge-rs/src/qsrv/group_config.rs:475` tests that missing `+trigger` means `All`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:1146` treats `TriggerDef::All` and `TriggerDef::Fields(_)` the same.
- pvxs defaults groups with no trigger mappings to individual self-trigger updates in `$PVXS_HOME/ioc/groupconfigprocessor.cpp:327-338`.
- pvxs expands explicit trigger targets in `$PVXS_HOME/ioc/groupconfigprocessor.cpp:381`.
- pvxs monitor callbacks update the triggered field list, not all fields unconditionally, in `$PVXS_HOME/ioc/groupsource.cpp:328-346`.
- pvxs tests require the no-`+trigger` NTEnum group update to contain only `value.index`, not `timeStamp`, in `$PVXS_HOME/test/testqgroup.cpp:220`.

Impact:

Rust group monitors can emit full-group updates where pvxs emits a narrow self-trigger update. Explicit `+trigger:"fieldA,fieldB"` is also not honored as a target list. Clients that consume monitor deltas see different changed fields, and expensive group members are re-read and re-emitted unnecessarily.

Wire-compatible expectation:

Missing `+trigger` should imply self-trigger for channeled members. `+trigger:"*"` should trigger all channeled members. Named `+trigger` values should update only those target members.

Fix direction:

Resolve trigger references at group-load time the way pvxs does: store a source-member to target-member list. The monitor path should update and mark only the resolved targets for value events; property events should update only the source field's metadata.

### BR-R30. QSRV group members without `+putorder` are writable in Rust

Severity: High functional / security-adjacent

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group_config.rs:316` defaults missing `+putorder` to `0`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:637` orders every member by `put_order`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:670` and `:723` write any present non-const/non-structure member; there is no "no putorder" sentinel.
- pvxs defaults `MappingInfo::putOrder` to `std::numeric_limits<int64_t>::min()` in `$PVXS_HOME/ioc/fieldconfig.h:37`.
- pvxs treats that sentinel as not putable in `$PVXS_HOME/ioc/groupsource.cpp:555,559-560`; marked writes without putorder are ignored with a warning.
- pvxs `batch.db` deliberately omits `+putorder` from field `C` to prevent writes in `$PVXS_HOME/test/batch.db:19`.
- pvxs tests PUT `C.value=4.0` and expect the group sum to ignore it in `$PVXS_HOME/test/testqgroup.cpp:725`.

Impact:

Rust makes omitted `+putorder` equivalent to `+putorder:0`, turning read-only group members into writable ones. Existing pvxs group definitions that rely on omission to block writes can be modified through Rust QSRV.

Wire-compatible expectation:

Only members with an explicit `+putorder` are writable through group PUT. Missing `+putorder` should keep the member readable/monitorable but ignore client writes.

Fix direction:

Represent `put_order` as `Option<i32>` or keep a sentinel. Sort writable members by explicit order, and ignore or warn on marked writes to members without `+putorder`.

### BR-R31. QSRV group PUT does not reject link fields

Severity: Medium functional / security-adjacent

Evidence:

- `crates/epics-base-rs/src/server/record/common_fields.rs:34` stores common link fields such as `FLNK`, `INP`, and `OUT` as raw strings.
- `crates/epics-base-rs/src/types/dbr.rs:73` has only native scalar DBF types; link DBF classes are not represented as distinct `DbFieldType` variants.
- `crates/epics-bridge-rs/src/qsrv/group.rs:564` falls back to a member DBF type and `:597` converts scalar input for that type.
- `crates/epics-bridge-rs/src/qsrv/group.rs:703` and `:752` write group members through `put_record_field_from_ca()` without a link-field guard.
- pvxs rejects group PUT preparation for `DBF_INLINK..DBF_FWDLINK` fields in `$PVXS_HOME/ioc/groupsource.cpp:603-605`.

Impact:

A Rust group definition can expose link fields such as `INP`, `OUT`, or `FLNK` as writable group members. A client PUT can then rewrite record links through QSRV where pvxs would reject the operation. That changes IOC topology and can redirect future reads, writes, or forward links.

Wire-compatible expectation:

Group PUT must reject backing fields whose final DBF type is a link type.

Fix direction:

Expose link-field classification from `epics-base-rs` record metadata, or add a QSRV-side field-name/type check that rejects `DBF_INLINK`, `DBF_OUTLINK`, and `DBF_FWDLINK` equivalents before any member write is applied.

### BR-R32. ACF `CALC` rules with `INP*` links are disabled instead of evaluated

Severity: Medium security / compatibility

Evidence:

- `crates/epics-base-rs/src/server/access_security.rs:363` parses `CALC` rule clauses.
- `crates/epics-base-rs/src/server/access_security.rs:368` marks unevaluable rules inert, and notes that `CALC` clauses cannot be evaluated because this crate has no `INP*` link resolution.
- `crates/epics-base-rs/src/server/access_security.rs:404` stores `INP(A..U)` declarations but does not resolve their values.
- `crates/epics-base-rs/src/server/access_security.rs:2164` tests that a `CALC("A=1")` rule is disabled and grants no access.
- pvxs QSRV installs EPICS Base access-security clients through `asAddClient()` in `$PVXS_HOME/ioc/securityclient.cpp:19`.
- pvxs QSRV write checks delegate to EPICS Base `asCheckPut()` in `$PVXS_HOME/ioc/securityclient.cpp:42`, so EPICS Base conditional ASG rules are part of the observed access decision.

Impact:

Rust fails closed for ACF rules that pvxs would evaluate dynamically from `INP*` link values. That is safer than granting access unconditionally, but it is still wire-visible: a client allowed by a true pvxs `CALC` condition is denied by Rust. Operators using conditional ASG policies cannot get the same access behavior.

Wire-compatible expectation:

For supported ACF syntax, the read/write decision should match EPICS Base. If conditional `CALC` rules are intentionally unsupported, the server should reject the policy at load time or clearly report that the ASG cannot be represented, rather than silently changing selected rule decisions.

Fix direction:

Implement `INP(A..U)` link resolution for access-security evaluation and evaluate `CALC` expressions at access recompute time. If that is deferred, surface an explicit ACF load/configuration error for CALC-dependent ASGs so operators do not assume pvxs-equivalent enforcement.

### BR-R33. QSRV group MONITOR omits `record._options.queueSize` and monitor atomic state

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group.rs:333` creates a group value with only the configured group `struct_id`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:357` and `:366` add member fields directly; no `record._options` branch is added by `read_group()`.
- `crates/epics-bridge-rs/src/qsrv/group.rs:1082`, `:1126`, `:1143`, and `:1153` use `read_group()` for monitor snapshots, so monitor events have the same omission.
- pvxs obtains the negotiated monitor queue stats in `$PVXS_HOME/ioc/groupsource.cpp:401-402`.
- pvxs writes `record._options.queueSize` and `record._options.atomic` into the group monitor value in `$PVXS_HOME/ioc/groupsource.cpp:404-405`.
- pvxs posts that value through `subscriptionControl->post(currentValue.clone())` in `$PVXS_HOME/ioc/groupsource.cpp:279`.

Impact:

Rust group monitors do not expose the pvxs monitor metadata fields that tell a client the negotiated queue size and atomic monitor-update state. A client that requests or inspects `record._options.queueSize` on a group monitor sees a different structure from pvxs, and a strict pvRequest can reject the missing branch before subscription setup.

Wire-compatible expectation:

Group monitor descriptors and monitor values should include `record._options.queueSize` and `record._options.atomic` with the same semantics pvxs exposes for group subscriptions.

Fix direction:

Add the `record._options` branch to group descriptors and values. The native PVA server already parses monitor pipeline/queue options; it needs to pass the negotiated queue size into the group monitor source or inject the option fields before encoding the monitor event.

### BR-R34. QSRV group monitors drop archive/log-triggered member updates

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/group.rs:1006` builds the group value subscription mask.
- `crates/epics-bridge-rs/src/qsrv/group.rs:1007` subscribes to `EventMask::VALUE | EventMask::ALARM` only.
- `crates/epics-base-rs/src/server/recgbl.rs:39` defines `EventMask::VALUE`, and `:40` defines `EventMask::LOG`, the Rust DBE_LOG/DBE_ARCHIVE equivalent.
- pvxs subscribes group value events with `DBE_VALUE | DBE_ALARM | DBE_ARCHIVE` in `$PVXS_HOME/ioc/groupsource.cpp:433-434`.
- pvxs single-source logic explicitly treats archive events as value updates in `$PVXS_HOME/ioc/singlesource.cpp:86-87`.

Impact:

Archive/log-only posts from a backing record can wake pvxs group monitors but not Rust group monitors. Archiver-like clients watching the group PV can miss samples that the original QSRV would publish.

Wire-compatible expectation:

Group member value subscriptions should include the archive/log event bit and map it into the same value-update path pvxs uses.

Fix direction:

Include `EventMask::LOG` in the group value subscription mask and treat log/archive events as value updates. Keep the pvRequest DBE filtering work from BR-R5 separate: this finding is about the default upstream member subscription missing an event class pvxs subscribes to.

### BR-R35. Single-record QSRV ignores `info(Q:time:tag, "nsec:lsb:N")`

Severity: Medium functional

Evidence:

- `crates/epics-base-rs/src/server/record/record_instance.rs:214` stores parsed `info(...)` tags on the record, and `:223` exposes `get_info()`.
- `crates/epics-base-rs/src/server/record/record_instance.rs:322` builds snapshots from the record value and common timestamp.
- `crates/epics-base-rs/src/server/snapshot.rs:89` defaults `Snapshot::user_tag` to zero, and the snapshot path does not read `Q:time:tag`.
- `crates/epics-bridge-rs/src/qsrv/pvif.rs:447` encodes `timeStamp.userTag` directly from `Snapshot::user_tag`.
- pvxs parses `info(Q:time:tag)` into `MappingInfo::nsecMask` in `$PVXS_HOME/ioc/typeutils.cpp:79`.
- pvxs masks nanoseconds and extracts the low bits into `timeStamp.userTag` in `$PVXS_HOME/ioc/iocsource.cpp:240`.
- pvxs tests the single-record behavior with `test:nsec` in `$PVXS_HOME/test/testqsingle.cpp:277`.

Impact:

Records that use the pvxs `Q:time:tag` convention expose different timestamps through Rust QSRV: the low nanosecond bits remain in `timeStamp.nanoseconds`, and `timeStamp.userTag` remains zero. Clients that use user tags for pulse IDs or event IDs will mis-correlate samples.

Wire-compatible expectation:

For single-record QSRV channels, `info(Q:time:tag, "nsec:lsb:N")` should split the record timestamp the same way pvxs does.

Fix direction:

Parse `Q:time:tag` into a per-record or per-channel nanosecond mask and apply it when building `Snapshot` or when converting the snapshot to NT data. Group `+nsecmask` support is not enough because pvxs also applies the record-level info tag to single-record QSRV channels.

### BR-R36. QSRV single-record default monitor mask is `VALUE|LOG`, not pvxs `VALUE|ALARM` plus `PROPERTY`

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/monitor.rs:91` starts a single-record monitor with `DbSubscription::subscribe(&self.db, &self.record_name)`.
- `crates/epics-base-rs/src/server/database/db_access.rs:250` maps that default subscription to `EventMask::VALUE | EventMask::LOG`.
- pvxs parses requested `record._options.DBE` in `$PVXS_HOME/ioc/singlesource.cpp:117`.
- pvxs defaults an empty DBE selection to `DBE_VALUE | DBE_ALARM` in `$PVXS_HOME/ioc/singlesource.cpp:142`.
- pvxs then creates a value subscription with that mask in `$PVXS_HOME/ioc/singlesource.cpp:155`.
- pvxs also creates a separate `DBE_PROPERTY` subscription in `$PVXS_HOME/ioc/singlesource.cpp:162-167`.

Impact:

Default Rust single-record QSRV monitors can miss alarm-only and metadata-only updates that pvxs publishes. They can also wake on archive/log-only events that pvxs would not publish unless the client requested `DBE=ARCHIVE`. This is separate from BR-R5: BR-R5 is about honoring explicit DBE selection, while this item is about the default event class being different even when the client sends no DBE option.

Wire-compatible expectation:

For a default single-record monitor, the backing database subscriptions must match pvxs: value events include `VALUE|ALARM`, property events are subscribed separately, and archive/log events are selected only when requested.

Fix direction:

Use `subscribe_with_mask()` for the value subscription and add a separate property subscription for single-record monitors. Start with pvxs default masks, then thread `record._options.DBE` through the monitor INIT path as part of the BR-R5 fix.

### BR-R37. QSRV RPC query arguments can write records under a READ-class gate

Severity: High security / functional

Evidence:

- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:365` implements `QsrvPvStore::rpc()`.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:377` treats `NTURI.query` or a bare structure as RPC query fields.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:431` enters the write path when the query is non-empty.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:447` calls `channel.put(&put)` from the RPC handler.
- `crates/epics-pva-rs/src/server_native/source.rs:326` allows RPC when `checked.allows_read()` is true, and `:335` delegates to `self.rpc(...)`.
- pvxs single-record QSRV registers `onGet` and `onPut` in `$PVXS_HOME/ioc/singlesource.cpp:315` and `:334`, but no `onRPC`.
- pvxs group QSRV registers `onGet` and `onPut` in `$PVXS_HOME/ioc/groupsource.cpp:194` and `:211`, but no `onRPC`.
- pvxs returns `RPC Not Implemented` when a channel has no RPC handler in `$PVXS_HOME/src/serverget.cpp:482`.

Impact:

A client with READ permission but not WRITE permission can issue an RPC with query arguments and drive `channel.put()` on Rust QSRV. pvxs QSRV records and groups would reject the same RPC because they do not install an RPC handler. This is both a wire-compatibility difference and a write-authority bypass for deployments that grant read without write.

Wire-compatible expectation:

QSRV record and group channels should not gain a mutating RPC surface that pvxs does not expose. If Rust intentionally supports RPC as an extension, mutating RPC arguments must be WRITE-class and must use the same per-field/member access checks as PUT.

Fix direction:

For pvxs compatibility, remove the QSRV record/group RPC write/read shim and return unsupported for those PVs. If an extension is kept, override `rpc_checked()` in `QsrvPvStore` so non-empty query arguments require WRITE permission before any `channel.put()` and still preserve member-field access checks.

### BR-R38. PVA `PROCESS` succeeds against QSRV records without processing them

Severity: Medium functional

Evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:1315` routes PVA `Command::Process` into `handle_process(...)`.
- `crates/epics-pva-rs/src/server_native/source.rs:352` defines the default `ChannelSource::process()` implementation.
- `crates/epics-pva-rs/src/server_native/source.rs:354` returns `Ok(())` from that default implementation.
- `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:229` implements `ChannelSource` for `QsrvPvStore`, but the implementation does not override `process()`.
- pvxs QSRV exposes processing through PUT options and record fields, not a QSRV `onProcess` handler; single-record operation setup installs only GET and PUT handlers in `$PVXS_HOME/ioc/singlesource.cpp:315` and `:334`.

Impact:

A PVA client can send the native `PROCESS` command to a Rust QSRV record and receive success even though the backing EPICS record is not processed. Operators or automated clients using `PROCESS` as a side-effect command will observe a false success. This differs from pvxs-style QSRV behavior, where processing is reached through PUT `record._options.process` or `.PROC` field semantics.

Wire-compatible expectation:

A supported processing operation must actually run the record's process path and return failure on denial or execution error. If QSRV does not implement the PVA `PROCESS` command, it should return an unsupported-operation status rather than success.

Fix direction:

Either implement `QsrvPvStore::process()` to call the same record-processing owner used by `.PROC`/PUT processing, with WRITE-class ACF, or explicitly reject `PROCESS` for QSRV-backed records and groups.

### BR-R39. Decoded QSRV monitor initial events ignore the client's field mask

Severity: Medium functional

Evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:3123` builds `full_mask = BitSet::all_set(...)` for the decoded monitor initial snapshot.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3125` sends that initial snapshot with the full mask instead of the `mask_clone` derived from pvRequest.
- pvxs computes the monitor `pvMask` from the pvRequest in `$PVXS_HOME/src/servermon.cpp:402`.
- pvxs stores that mask on the monitor operation in `$PVXS_HOME/src/servermon.cpp:414`.
- pvxs always lets the first update enter the queue in `$PVXS_HOME/src/servermon.cpp:261`, but encodes queued monitor values with `self->pvMask` in `$PVXS_HOME/src/servermon.cpp:174`.

Impact:

For decoded monitor sources such as QSRV records and groups, the first Rust monitor event can include fields the client did not request. A client that requested `field(value)` can receive alarm, timestamp, display, control, or other unrequested leaves on the first event, then receive masked deltas later. This is wire-observable and can break clients that rely on the requested BitSet shape for stable decoding, bandwidth, or field-level data minimization.

Wire-compatible expectation:

The initial monitor event should be forced through the queue even when it has no selected-field changes, but the wire BitSet/value encoding should still use the pvRequest mask, as pvxs does.

Fix direction:

For the decoded monitor initial snapshot, pass `mask_clone` into `build_monitor_payload()` instead of a full mask. If a full prior is needed for internal merge state, keep that state private to the server/client implementation rather than widening the downstream wire event.

### BR-R40. QSRV monitor channel-filter syntax is not pvxs-compatible

Severity: Medium functional

Evidence:

- pvxs tests QSRV monitor filters with channel names such as `test:ai.VAL{"dbnd":{"d":0.0}}` in `$PVXS_HOME/test/testqsingle.cpp:831`.
- `crates/epics-bridge-rs/src/qsrv/channel.rs:143` parses QSRV channel names with `parse_pv_name(name)`.
- `crates/epics-base-rs/src/server/database/mod.rs:23` implements `parse_pv_name()` as a simple last-dot split; it does not parse a trailing JSON channel-filter suffix.
- `crates/epics-pva-rs/src/server_native/tcp.rs:110` documents the Rust PVA monitor filter carrier as `record._options._filter`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:118` notes that pvxs uses field-scoped filter syntax and that Rust's `_filter` carrier is a different, subscription-wide form.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2421` reads only `record._options._filter` from the decoded pvRequest to build the monitor filter chain.

Impact:

A pvxs-compatible client using the standard channel-filter suffix, for example `PV.VAL{"dbnd":{"d":2.0}}`, will not get equivalent Rust QSRV behavior. Depending on how the name is split, the channel can fail to resolve or subscribe without the intended server-side filter. The Rust-only `_filter` option is not the pvxs wire syntax and applies one subscription-wide chain rather than the field-scoped filter that pvxs/dbChannel uses.

Wire-compatible expectation:

QSRV should accept the same channel-filter syntax accepted by pvxs/dbChannel and apply the filter to the selected dbChannel field with per-subscription filter state.

Fix direction:

Parse and strip the trailing JSON channel-filter suffix before record/field resolution, preserve the requested field, and attach the parsed filter chain to the database subscription for that channel. Keep `record._options._filter` only if it is explicitly documented as a Rust extension and does not replace the pvxs syntax.

### BR-R41. PVA gateway decoded monitor fallback emits only the initial snapshot

Severity: High functional

Evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:2926` restricts the raw monitor fast path to full-field, non-pipelined, unfiltered subscriptions.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3073` falls back to `subscribe_checked()` when the raw path is not eligible.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:558` lets operators force that fallback with `EPICS_PVA_GW_RAW_FRAMES=NO`.
- `crates/epics-bridge-rs/src/pva_gateway/source.rs:648` subscribes the decoded fallback to `entry.subscribe()`, sends `entry.snapshot()` at `:667`, then waits for broadcast updates at `:673`.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:522` explicitly retires the typed broadcast path with `let _ = tx_inner; // typed broadcast retired in raw path`.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:536` still decodes upstream raw events into the cache snapshot, but `:547`-`:552` fan out only the raw body to `tx_raw`, never a `PvField` to `tx`.

Impact:

A downstream monitor that asks for a field projection, pipeline flow control, or any server-side filter takes the decoded fallback and receives the first cached value, then no further monitor updates. The same happens when the documented raw-frame kill switch is used. That is not wire-compatible monitor behavior: a client using a valid pvRequest observes a one-shot GET-shaped response instead of a live subscription.

Wire-compatible expectation:

Raw forwarding can be an optimization, but the non-raw monitor path must remain a complete live subscription for every pvRequest shape that cannot be forwarded byte-for-byte.

Fix direction:

Restore typed fanout from the upstream monitor task, for example by sending the merged `PvField` produced by `apply_monitor_event()` on `tx` after every successful decode. Add parity tests that exercise masked, pipelined, filtered, and `EPICS_PVA_GW_RAW_FRAMES=NO` gateway monitors and verify at least one post-initial update reaches the downstream client.

### BR-R42. PVA gateway raw monitors forward type-changed bodies under the old downstream descriptor

Severity: High functional

Evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:1994` stores the channel descriptor at CREATE_CHANNEL time.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2459` builds MONITOR INIT from that descriptor, and the raw fast path later writes raw monitor data with only the downstream IOID rewritten.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3063` sends `build_monitor_payload_raw(ioid, &ev, order)` without checking that the raw event's upstream descriptor still matches `intro_clone`.
- `crates/epics-bridge-rs/src/pva_gateway/channel_cache.rs:536` detects an upstream descriptor change, `:537`-`:542` logs it as a warning, then `:547`-`:552` still broadcasts the raw body bytes.
- pvxs pvalink treats reconnect as a type-change boundary: `$PVXS_HOME/ioc/pvalink_channel.cpp:342` says reconnect implies type change and `:350`-`:351` calls `link->onTypeChange()`.

Impact:

If an upstream IOC restarts and the PV's descriptor changes, existing downstream gateway monitors keep the old PVA descriptor but receive body bytes encoded for the new descriptor. A downstream client can decode garbage, disconnect on a protocol error, or merge deltas against the wrong field numbering. This is especially risky because the code already detects the condition and then continues forwarding.

Wire-compatible expectation:

A monitor data body is valid only for the descriptor negotiated in that monitor's INIT response. When a proxy detects an upstream descriptor change, it must renegotiate, terminate the downstream monitor/channel, or suppress incompatible events until the downstream has a matching descriptor.

Fix direction:

Treat `type_changed` as a subscription boundary for gateway raw forwarding. Send MONITOR FINISH or destroy/recreate the downstream channel/op, then let the client reopen with the new descriptor. If decoded fallback is used, still validate the decoded value against the downstream descriptor before emitting it.

### BR-R43. pvalink monitor pvRequest omits pvxs `atomic=true` and default `queueSize=4`

Severity: Medium functional

Evidence:

- pvxs always builds pvalink monitor requests with `record._options.pipeline`, `record._options.atomic`, and `record._options.queueSize`: `$PVXS_HOME/ioc/pvalink_link.cpp:53`-`:65`.
- pvxs hard-codes `record._options.atomic` to true for the remote monitor request at `$PVXS_HOME/ioc/pvalink_link.cpp:64`, independent of the local pvalink `atomic` scan option.
- pvxs pvalink default queue depth is 4 at `$PVXS_HOME/ioc/pvalink.h:73`, and `makeRequest()` sends that value at `$PVXS_HOME/ioc/pvalink_link.cpp:65`.
- `crates/epics-bridge-rs/src/pvalink/link.rs:609` builds a request only when `pipeline` or a non-default queue is requested.
- `crates/epics-bridge-rs/src/pvalink/link.rs:613` returns `None` for the default monitor, so no `record._options.atomic=true` or `queueSize=4` reaches the server.
- `crates/epics-bridge-rs/src/pvalink/link.rs:617`-`:627` sends only `pipeline` and `queueSize` when a request is built; it never sends `atomic=true`.

Impact:

Rust pvalink does not ask the remote QSRV/group server for the same monitor contract pvxs asks for. A default INP monitor omits the queue-size negotiation entirely, and every INP monitor omits the forced remote `atomic=true` option. Against pvxs QSRV groups, that can change whether monitor snapshots are assembled atomically and whether `record._options.queueSize` / `record._options.atomic` appear in the stream with pvxs-equivalent values.

Wire-compatible expectation:

For pvalink INP monitors, the remote pvRequest shape should match pvxs `pvaLink::makeRequest()`: `record._options.pipeline=<configured>`, `record._options.atomic=true`, and `record._options.queueSize=<configured or default 4>`.

Fix direction:

Make `monitor_request()` always return a request for INP monitor links and include all three pvxs fields. Keep local scan `config.atomic` separate from the remote monitor `record._options.atomic=true`; they are related concepts but not the same option.

### BR-R44. PVA gateway raw monitor byte-order mismatch drops every update instead of re-encoding

Severity: Medium functional

Evidence:

- `crates/epics-bridge-rs/src/pva_gateway/source.rs:606` preserves the upstream raw event body plus its upstream `byte_order`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3045` detects that a raw event's byte order differs from the downstream connection's byte order.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3047` says the fallback is to decode and re-encode, but `:3055`-`:3061` drops the event with `continue`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3063` forwards raw bytes only when byte order already matches.
- pvxs encodes each monitor response with the downstream connection's `sendBE`: `$PVXS_HOME/src/servermon.cpp:159`, then serializes the value at `:174`.

Impact:

A gateway between peers with different negotiated byte orders sends the initial decoded snapshot, then silently drops every subsequent raw monitor event. The comment names the correct behavior, but the implementation does not perform it. This is a wire-compatibility problem even if most current deployments are little-endian, because the protocol explicitly negotiates byte order per connection.

Wire-compatible expectation:

Monitor payloads must be encoded in the downstream connection's byte order. Raw forwarding is valid only when upstream and downstream byte order match; otherwise the gateway must decode with the upstream descriptor/order and re-encode for the downstream peer.

Fix direction:

Carry the upstream descriptor with raw events or keep a decoded event stream available to the raw branch. On byte-order mismatch, build the monitor payload through the normal `build_monitor_payload()` path instead of dropping the event.

## Priority

Recommended order:

1. BR-R1, BR-R9, BR-R21, BR-R32, and BR-R37. They decide whether configured or upstream ACF protects QSRV and gateway traffic with matching policy semantics, including RPC mutation.
2. BR-R17, BR-R30, and BR-R31. Group PUT must not bypass member-field access, write read-only members, or rewrite link fields.
3. BR-R2, BR-R3, BR-R20, BR-R38, BR-R11, and BR-R28. Field addressing and PUT/scan processing semantics are core write-path wire behavior.
4. BR-R10, BR-R19, BR-R26, and BR-R27. DB pvalink and group syntax/options must preserve operator-declared state per link.
5. BR-R6 and BR-R41. Typed forwarding and decoded monitor fallback are core PVA gateway data-plane compatibility.
6. BR-R4, BR-R7, and BR-R8. These close method/authority/roles, upstream identity, and resource-control gaps.
7. BR-R12, BR-R13, and BR-R22. Descriptor/value type shape must match pvxs for metadata, enum, and unsigned 64-bit fields.
8. BR-R15, BR-R16, and BR-R18. Group and pvalink atomic semantics need a shared multi-record lock design.
9. BR-R33 and BR-R34. Group monitor metadata and archive/log events must match pvxs for monitor clients and archivers.
10. BR-R23, BR-R24, and BR-R35. pvalink value conversion, metadata hooks, and timestamp user-tag handling determine record-level compatibility.
11. BR-R25 and BR-R29. Group metadata and trigger semantics affect standard NT group layouts and monitor deltas.
12. BR-R5, BR-R14, BR-R36, BR-R39, BR-R40, BR-R42, BR-R43, and BR-R44. Monitor event-selection, filters, descriptor changes, byte order, and field-mask compatibility affect clients but are less direct than access/write corruption.

## Suggested parity tests

Add tests that use real PVA client paths, not only direct helper calls.

- QSRV ACF: launch through `run_ca_pva_qsrv_ioc()` with an ACF denying a PVA user and verify GET/PUT/MONITOR denial over PVA.
- QSRV ACF CALC: an ASG with `INPA("pv")` and `RULE(...){CALC("A=1")}` matches pvxs access decisions as the input value changes, or is rejected as unsupported at config load.
- QSRV identity cache: connect two PVA clients with different users to the same PV and verify decisions do not leak through a cached channel.
- QSRV field PVs: `record.DESC`, `record.SCAN`, `record.RVAL`, `record.STAT`, `record.INP$`, and `record.PROC` return/write the same field behavior as pvxs tests.
- QSRV RPC: RPC against a single record or group PV without an installed RPC handler returns the same unsupported status pvxs returns; a read-only user cannot mutate a record through `NTURI.query`.
- QSRV PROCESS command: PVA `PROCESS` either runs the record process path with WRITE-class access or returns unsupported; it must not return success without changing the same state pvxs processing would change.
- QSRV PUT options: `record[process=false]`, `record[process=passive]`, `record[process=true]`, and `record[block=true]` affect processing and completion timing.
- QSRV passive processing: `process=passive` processes only fields/records that satisfy the pvxs `process_passive` and passive-scan gate.
- QSRV monitor DBE: `record[DBE=VALUE]`, `record[DBE=ARCHIVE]`, and `record[DBE=ALARM]` change event selection.
- QSRV single-record default monitor mask: no-DBE monitors receive alarm and property updates like pvxs and do not receive archive/log-only events unless requested.
- QSRV monitor initial mask: a decoded monitor requested as `field(value)` emits a first event whose changed BitSet/value body includes only the requested fields, matching pvxs.
- QSRV monitor filters: `record.VAL{"dbnd":{"d":2.0}}`, `record.VAL{"arr":{"s":1,"i":2}}`, and chained filters parse and gate/transform updates like pvxs, with independent state per subscription.
- PVA gateway typed PUT: nested structure, scalar array with strings containing spaces, union/variant value, and group-like structure pass through unchanged.
- PVA gateway READ/MONITOR credentials: two downstream users with different upstream read permissions cannot share one upstream monitor cache entry unless gateway-side ACF is the documented authority.
- PVA gateway identity cap: many distinct downstream accounts stop at a configured cap and emit an auditable denial.
- PVA gateway auth model: downstream `ca`, `anonymous`, and `x509` methods produce documented upstream credentials and gateway audit records.
- pvalink DB JSON: `field`, `proc=CPP`, `sevr=MS`, `Q`, `pipeline`, `time`, and `atomic` options survive DB load without iocsh pre-open.
- pvalink per-link state: two links to the same PV but different `field`, `sevr`, `proc`, `monorder`, or `atomic` values keep independent behavior while sharing only the safe upstream channel state.
- pvalink OUT: `proc=NPP`, `proc=PP`, `block=true`, remote `field`, `defer`, and `retry` produce pvxs-equivalent upstream PUT behavior.
- pvalink CPP: `proc=CP` processes on monitor update, while `proc=CPP` processes only Passive local records.
- pvalink INP arrays: string, int8/16/32/64, uint8/16/32/64, float32, and float64 arrays convert according to the target DBF/DBR type.
- pvalink metadata: `dbGetGraphicLimits`, `dbGetControlLimits`, `dbGetAlarmLimits`, `dbGetPrecision`, `dbGetUnits`, `dbGetNelements`, and DBF type queries match pvxs.
- QSRV metadata: scalar and waveform records compare descriptor/value output against pvxs for `display.form`, `control`, `valueAlarm`, integer limits, and string waveform metadata.
- QSRV NTEnum: descriptor and data use `value.index int32_t` and include `display.description`.
- QSRV unsigned 64-bit: `int64in` and `waveform FTVL=UINT64` return and accept PVA `ulong` / `ulong[]` where pvxs does.
- QSRV group root meta: `"":{+type:"meta", +channel:"VAL"}` emits root `alarm` and `timeStamp` in GET and MONITOR.
- QSRV group constants: `+type:"const"` accepts pvxs `+const` in DB and JSON group definitions.
- QSRV group triggers: missing `+trigger` emits only self-trigger deltas, named triggers emit only their resolved target fields, and `+trigger:"*"` emits all channeled members.
- QSRV group putorder: a member without `+putorder` ignores marked writes, while explicitly ordered members write in order.
- QSRV group link fields: members backed by `INP`, `OUT`, or `FLNK` reject group PUT attempts before any partial write.
- QSRV group access: a user allowed to write the group PV but denied on one backing member cannot write that member through the group.
- QSRV group atomic override: `record._options.atomic=true/false` changes GET and PUT behavior per operation.
- QSRV group monitor options: initial and subsequent group monitor values include `record._options.queueSize` and `record._options.atomic` with pvxs-equivalent values.
- QSRV group archive events: member updates posted with only the archive/log bit wake the group monitor and produce the same value update pvxs would.
- QSRV time userTag: a record with `info(Q:time:tag, "nsec:lsb:8")` splits nanoseconds into `timeStamp.nanoseconds` and `timeStamp.userTag` exactly like pvxs for GET and MONITOR.
- Atomic consistency: direct backing-record writes cannot interleave with atomic group PUT, and pvalink `atomic` CP/CPP target records process under one multi-record lock.
- PVA gateway monitor request: downstream `DBE`, field masks, filter chains, and pipeline options either create matching upstream subscriptions or return a documented unsupported-option error.
- PVA gateway decoded monitor fallback: masked, pipelined, filtered, and `EPICS_PVA_GW_RAW_FRAMES=NO` monitors receive post-initial updates, not only the cached seed event.
- PVA gateway type change: an upstream descriptor change during a raw-forwarded monitor terminates or renegotiates the downstream subscription instead of forwarding new-descriptor bytes under the old descriptor.
- PVA gateway byte order: raw-forwarded monitors between different upstream/downstream byte orders deliver post-initial updates re-encoded for the downstream connection.
- pvalink monitor request: default and non-default INP monitors send `record._options.pipeline`, `record._options.atomic=true`, and `record._options.queueSize` exactly like pvxs `makeRequest()`.

## Conclusion

Wire-compatible is the right target. The gaps above are not requests for line-by-line pvxs parity; they are points where Rust currently changes what a PVA client, ACF file, IOC operator, linked record, or upstream server observes. Security-sensitive compatibility needs the same bar as functional compatibility because access decisions are part of the externally visible protocol contract.
