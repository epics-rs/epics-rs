# Functional Review - 2026-05-20

Scope: `epics-pva-rs` current workspace state, compared against
`~/codes/pvxs` client/server/pvData behavior. This file records open
functional gaps and bugs that are still visible in the current Rust code.
Older review items that current code has closed, such as NTNDArray schema
support, OriginTag UDP handling, and partial monitor bitsets, are not repeated
as open findings.

## Summary

| ID | Type | Severity | Area | Status |
|----|------|----------|------|--------|
| PVA-FR-1 | Bug / missing pvxs wire case | High | compound arrays | Done |
| PVA-FR-2 | Missing pvxs API behavior | Medium | report diagnostics | Done |
| PVA-FR-3 | Missing pvxs IOC access behavior | Medium | ACF roles / CALC | Done |
| PVA-FR-4 | Missing pvxs monitor behavior | Medium | pipeline watermarks | Done |
| PVA-FR-5 | Bug / lossy wire fallback | Medium | `any` descriptor recovery | Done |
| PVA-FR-6 | Missing pvxs tool behavior | Medium | `pvput-rs` field assignment | Done |
| PVA-FR-7 | Bug | Medium | operation wait timeout | Done |
| PVA-FR-8 | Bug | High | monitor pause/resume queueing | Done |
| PVA-FR-9 | Missing pvxs API behavior | Medium | operation interrupt result | Done |
| PVA-FR-10 | Bug / missing pvxs API behavior | Medium | subscription stats | Done |
| PVA-FR-11 | Missing pvxs API behavior | Medium | monitor start / pause callback | Done |
| PVA-FR-12 | Bug / missing pvxs behavior | Medium | initial search timeout | Done |

## PVA-FR-1: compound arrays cannot preserve null elements

pvxs distinguishes a null compound-array element from a present element whose
inner body is null. Rust's `PvField` model cannot represent that distinction,
so the encoder cannot emit pvxs's `0x00` null-element wire shape and the decoder
collapses incoming null elements into ordinary Rust values.

Rust evidence:

- `crates/epics-pva-rs/src/pvdata/structure.rs:25`, `:34`, and `:38` model
  `StructureArray`, `UnionArray`, and `VariantArray` as plain `Vec<...>` values
  without per-element `Option`.
- `crates/epics-pva-rs/src/pvdata/encode.rs:903-914` always emits `0x01`
  before each `StructureArray` element.
- `encode.rs:937-963` always emits `0x01` before each `UnionArray` element and
  routes null selectors through the inner `0xFF` sentinel.
- `encode.rs:971-998` always emits `0x01` before each `VariantArray` element
  and encodes `desc: None` as an inner `0xFF`.
- `encode.rs:1715-1747`, `:1775-1822`, and `:1849-1894` accept incoming
  `0x00` null elements, but decode them into empty structures or null-like
  union / variant values, losing whether the element was absent or present.
- `crates/epics-pva-rs/tests/pva_cases/compound_edge.rs:9-27` documents the
  gap and keeps pvxs golden fixtures ignored until the model can express null
  elements.

pvxs reference:

- `~/codes/pvxs/src/dataencode.cpp:354-365` writes `0x00` for null
  `StructA` elements and `0x01 + body` for present elements.
- `dataencode.cpp:368-378` does the same for `UnionA`.
- `dataencode.cpp:382-393` does the same for `AnyA`.

Impact:

Rust can interoperate with arrays where every compound element is present, but
it cannot faithfully encode or round-trip arrays containing null structure,
union, or variant elements. A pvxs fixture such as `struct_array_all_null`
expects `Size(3) + 00 00 00`; current Rust has no value that can produce those
bytes.

Fix direction:

Add an explicit compound-array element type instead of overloading inner null
sentinels. Acceptable shapes include:

```rust
StructureArray(Vec<Option<PvStructure>>)
UnionArray(Vec<Option<UnionItem>>)
VariantArray(Vec<Option<VariantValue>>)
```

or a compatibility-preserving wrapper enum if the public API cannot change
directly. Then update encode, decode, formatting, value checking, typed builders
and the ignored pvxs golden tests together.

Regression tests to add:

- Unignore `golden_pvxs_struct_array_all_null`.
- Unignore `golden_pvxs_struct_array_present_null_present`.
- Unignore `golden_pvxs_union_array_null_element`.
- Unignore `golden_pvxs_variant_array_null_descriptor`.
- Add decode-then-encode tests that preserve null vs present-null element
  identity.

## PVA-FR-2: `report()` lacks pvxs channel-level diagnostics and reset behavior

pvxs `Context::report(bool zero)` and `Server::report(bool zero)` return a
connection list with peer endpoint, byte counters, and per-channel entries.
When `zero` is true, the byte counters are reset after snapshot. Rust exposes
summary counters on the client and partial peer counters on the server, but no
client connection list, no channel-level report, and no reset flag.

Rust evidence:

- `crates/epics-pva-rs/src/client_native/context.rs:1192-1234` builds only
  aggregate client counts.
- `context.rs:1504-1522` defines `ClientReport` as summary-only.
- `crates/epics-pva-rs/src/server_native/runtime.rs:692-720` builds a server
  report with server state plus peer snapshots.
- `runtime.rs:779-797` defines `ServerReport` with peer counters but no
  per-channel report entries, `ReportInfo`, credentials field, or zero/reset
  option.

pvxs reference:

- `~/codes/pvxs/src/pvxs/netcommon.h:43-68` defines `Report` with
  connections and per-connection channel lists.
- `~/codes/pvxs/src/client.cpp:463-505` snapshots client connection and
  channel byte counters, then zeros counters when requested.
- `~/codes/pvxs/src/server.cpp:237-278` snapshots server connections,
  credentials, channel counters, and `ReportInfo`, then zeros counters when
  requested.

Impact:

Operational tooling cannot ask Rust PVA for the same diagnostics pvxs exposes:
which PV names are open on which peer, per-channel byte counters, server-side
credential metadata, or delta counters since the previous report.

Fix direction:

Introduce a pvxs-shaped report type alongside the existing summary structs, or
extend the current structs in a versioned way. The server already has peer
book-keeping in `server_native/peers.rs`; it needs per-channel entries and a
counter reset path. The client needs per-connection and per-channel counters in
the channel pool / server connection layer.

Regression tests to add:

- Client `report(false)` includes one connection and one channel after a GET.
- Client `report(true)` returns non-zero counters and the next report returns
  zero deltas.
- Server `report(false)` includes credentials and channel name for an active
  client.
- Server `report(true)` resets connection and channel byte counters.

## PVA-FR-3: native PVA access security does not match pvxs IOC role and CALC behavior

The native PVA server routes access checks through the shared
`epics-base-rs` `AccessGate`. That gives the PVA server the same ACF
`CALC` / `INP*` limitation recorded in the CA review, and it also does not
fully model pvxs IOC role credentials.

Rust evidence:

- `crates/epics-pva-rs/src/server/native_source.rs:73-98` builds the native
  source gate from `epics_base_rs::server::access_security::AccessGate`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:990-1032` parses `groups` or
  `roles` from the PVA auth payload into `ClientCredentials::roles`.
- `tcp.rs:1934-1945` calls `AccessGate::check()` with host, account, method,
  and authority, but not roles.
- `crates/epics-base-rs/src/server/access_security.rs:562-568` matches UAG
  members only against the single `user` string.
- `access_security.rs:1531-1562` disables syntactically valid CALC-gated rules
  because `INP*` link values are not resolved.

pvxs / epics-base reference:

- `~/codes/pvxs/ioc/securityclient.cpp:19-31` creates AS clients through
  epics-base `asAddClient()`, so QSRV uses the C access-security engine for IOC
  records.
- `~/codes/pvxs/documentation/ioc.rst:181-188` documents QSRV-specific access
  policy differences, including `role/...` UAG members matched against local
  group-derived role credentials.
- `~/codes/epics-base/modules/libcom/src/as/asLibRoutines.c:957-964` and
  `:1038-1042` dynamically evaluate CALC-gated rules.

Impact:

An ACF that grants PVA access through `UAG(special) { "role/op" }` will not be
enforced the same way by the native Rust server unless the client account string
itself is `role/op`. Likewise, dynamic CALC-gated ACF rules are disabled rather
than evaluated. This is fail-closed for CALC but it is not pvxs IOC parity.

Fix direction:

Extend `AccessGate` or wrap it for PVA so role credentials are part of UAG
matching. The owner must define whether roles are trusted from the PVA auth
payload, derived locally like QSRV, or only accepted from an authenticated
method. CALC parity needs the same `INP*` value resolver described in the CA
review.

Regression tests to add:

- A native PVA client with authenticated role `op` matches a UAG member
  `"role/op"`.
- The same rule denies a client without that role.
- A CALC-gated PVA record grants and revokes access as the linked input value
  changes.

## PVA-FR-4: monitor watermark callbacks use queue occupancy instead of pvxs pipeline window

pvxs defines monitor watermarks against the flow-control window of a pipelined
monitor. The high callback fires when a client ACK refills the window above the
high mark; the low callback is tied to consuming window credit as DATA frames are
sent. Rust exposes watermark callbacks on `SharedPV`, but the TCP monitor task
fires them from server-side mpsc queue occupancy and uses global server
configuration instead of the per-monitor / per-PV watermark levels.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/runtime.rs:66`, `:235`, and `:249-250`
  define global `monitor_queue_depth`, `monitor_high_watermark`, and
  `monitor_low_watermark`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3247-3248` captures only the
  global queue depth and high watermark for a monitor task.
- `tcp.rs:3220-3230` handles pipeline ACKs by refilling `monitor_window` but
  does not evaluate watermark callbacks at that pvxs trigger point.
- `tcp.rs:3647-3660` computes `pending` from the outbound mpsc channel capacity,
  fires high at `pending >= high_watermark`, and fires low only when `pending`
  returns to zero.
- `crates/epics-pva-rs/src/server_native/shared_pv.rs:731-744` stores per-PV
  low/high watermark setters, but the monitor loop does not read those levels.

pvxs reference:

- `~/codes/pvxs/src/pvxs/source.h:120-128` defines watermarks as flow-control
  levels on the outbound window and says high fires after client ACK when the
  window is above `high`.
- `~/codes/pvxs/src/servermon.cpp:321-334` stores per-operation low/high levels
  from `setWatermarks()`.
- `servermon.cpp:653-663` fires `onHighMark` in the ACK path after adding window
  credit.
- `servermon.cpp:192-203` fires `onLowMark` when DATA emission consumes credit
  down to the low level.

Impact:

Producers using `SharedPV::set_on_high_mark()` to throttle monitor generation do
not get pvxs-equivalent signals. A slow client can exhaust pipeline credit
without triggering the high callback at the ACK boundary, a non-pipelined monitor
can trigger callbacks from local queue pressure, and `set_low_watermark()` /
`set_high_watermark()` cannot tune the callback thresholds because those stored
levels are not used by the TCP monitor loop.

Fix direction:

Separate diagnostics from pvxs watermark API behavior. Keep queue-depth warnings
as server diagnostics, but fire `SharedPV` watermark callbacks from the pipeline
window owner:

- carry per-monitor low/high levels into `OpState` or `MonitorOptions`;
- on ACK, after adding credit, fire high when `window > high`;
- after emitting a DATA frame and decrementing credit, fire low when
  `window <= low`;
- use the `SharedPV` per-PV levels or an explicit monitor-control API, not the
  global queue-occupancy threshold.

Regression tests to add:

- A pipelined monitor with `high = 2` fires high after ACK raises the window to
  `3`.
- The same monitor fires low when DATA emission consumes the window to `low`.
- `SharedPV::set_high_watermark()` changes the callback threshold for that PV.
- A non-pipelined monitor does not fire pvxs high/low callbacks from mpsc queue
  occupancy alone.

## PVA-FR-5: `any` fallback can infer a wire-incompatible descriptor

pvxs `Value` carries its original descriptor pointer, so encoding an `any`
payload writes the exact descriptor attached to that value. Rust can carry an
explicit descriptor in `VariantValue`, but the generic `FieldDesc::Variant`
fallback infers a descriptor from the concrete `PvField`. That inference is
documented as lossy for empty scalar arrays, empty structure arrays, unions, and
union arrays, and it is used on the wire when a bare value is encoded as `any`.

Rust evidence:

- `crates/epics-pva-rs/src/pvdata/structure.rs:51-56` makes
  `VariantValue.desc` optional.
- `structure.rs:151-180` documents lossy descriptor recovery cases, including
  empty `ScalarArray`, empty `StructureArray`, `Union`, `UnionArray`, and bare
  `Null`.
- `structure.rs:198-226` implements those degraded descriptors:
  `StructureArray(Vec::new())` becomes empty `struct_id` and fields, and
  `UnionArray` always returns an empty variants list.
- `crates/epics-pva-rs/src/pvdata/encode.rs:1103-1114` encodes a
  non-`Variant` value against `FieldDesc::Variant` by calling
  `other.descriptor()` and then writing the inferred descriptor to the wire.

pvxs reference:

- `~/codes/pvxs/src/dataimpl.h:35` exposes `Value::Helper::desc(val)` as the
  stored descriptor pointer.
- `~/codes/pvxs/src/type.cpp:317-333` builds a `TypeDef` from the descriptor
  attached to a `Value`, not by reconstructing schema from the current contents.
- `~/codes/pvxs/src/dataencode.cpp:411` encodes values through
  `Value::Helper::desc(val)`, and `:416-418` does the same for masked valid
  encoding.

Impact:

A gateway, RPC result, or user source that passes a bare compound value through
an `any` field can advertise the wrong schema. For example, an empty
`StructureArray` inside `any` is encoded with no element fields, and a
`UnionArray` inside `any` is encoded with no variants. The receiver can no
longer reconstruct the intended channel schema even though pvxs would preserve
the descriptor independently of the current value contents.

Fix direction:

Do not let wire paths rely on lossy value-only descriptor recovery for
descriptor-sensitive shapes. Options:

- require `PvField::Variant(VariantValue { desc: Some(...), ... })` for
  compound arrays and unions carried through `any`;
- extend `PvField` so descriptor-sensitive variants can carry their canonical
  `FieldDesc` even when empty;
- make the `FieldDesc::Variant` fallback reject lossy shapes with an explicit
  error instead of emitting a degraded descriptor.

Regression tests to add:

- Encoding an `any` containing an empty `StructureArray` preserves the declared
  element schema.
- Encoding an `any` containing a `UnionArray` preserves all variants and their
  selector indices.
- A descriptor-less lossy `any` value returns an error instead of silently
  emitting a degraded schema.

## PVA-FR-6: `pvput-rs` does not implement pvxs `field=value` assignment

pvxs `pvxput` accepts one bare value as shorthand for `value=<arg>`, and accepts
one or more `<field>=<value>` assignments for structure subfields. It builds from
the server prototype, clears the changed marks while keeping current values for
conversion help, assigns every requested field, and sends the resulting delta.
Rust `pvput-rs` documents the legacy field-assignment form, but joins every value
token into one string and routes it through value-only PUT calls.

Rust evidence:

- `crates/epics-pva-rs/src/bin/pvput-rs.rs:55-61` documents
  `pvput <PV> <field>=<value> ...` as a supported legacy form.
- `pvput-rs.rs:82-84` joins all value tokens into one `value_str`; it never
  splits tokens at `=` or validates mixed bare/field assignment input.
- `pvput-rs.rs:96-108` calls only `client.pvput()` or
  `client.pvput_with_request()`, not the field-targeting API.
- `crates/epics-pva-rs/src/client_native/context.rs:818-827` exposes only a
  single-field `pvput_field()` helper.
- `context.rs:860-880` exposes only a single-field `pvput_field_with_request()`
  helper for custom requests.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:2578-2608` builds structure
  PUT values by parsing the string into a field literally named `value` and
  default-filling other fields.

pvxs reference:

- `~/codes/pvxs/tools/put.cpp:83-104` parses one bare value as `value=<arg>`;
  otherwise every argument must be `<field>=<value>` and is stored by field path.
- `put.cpp:115-134` builds from the channel prototype, calls
  `val.unmark(false, true)`, assigns every parsed field with `val[pair.first] =
  pair.second`, and writes the delta value.

Impact:

`pvput-rs PV alarm.severity=2 timeStamp.nanoseconds=5` does not update those two
fields the way pvxs does. Depending on the target schema, Rust instead writes the
literal joined string into `.value`, fails to parse it as the `.value` type, or
default-fills unrelated fields under a custom pvRequest. Multi-field structure
updates also cannot be expressed as one prototype-based delta PUT through the
current public helper surface.

Fix direction:

Parse CLI arguments into an assignment map matching pvxs: one bare value maps to
`value`, while multiple arguments must all be `field=value`. Add a multi-field PUT
builder that starts from the server prototype, applies every assignment by dotted
field path, preserves current/prototype values needed for conversions such as
NTEnum lookup, and marks only assigned fields in the changed bitset. `pvput-rs`
should route the parsed assignments through that builder instead of concatenating
tokens into `value_str`.

Regression tests to add:

- `pvput-rs PV 42` maps to a single `value=42` assignment.
- `pvput-rs PV alarm.severity=2 timeStamp.nanoseconds=5` sends a delta with both
  requested fields marked and `.value` untouched.
- A mixed invocation such as `pvput-rs PV 42 alarm.severity=2` is rejected with
  the pvxs-style "expected <fld>=<value>" error.
- `-r record[process=true] PV alarm.severity=2` preserves the record options
  while still targeting the assigned field path.

## PVA-FR-7: `PvaOperation::wait(timeout)` loses the result after a timeout

pvxs `Operation::wait(timeout)` can time out while the operation remains
in-progress; a later `wait()` on the same operation can still receive the result.
Rust's `PvaOperation::wait()` takes ownership of the `oneshot::Receiver` before it
enters the timeout wrapper. If the timeout fires first, the receiver is dropped,
the spawned operation continues with no consumer, and the next `wait()` reports
that the result was already consumed.

Rust evidence:

- `crates/epics-pva-rs/src/client_native/operation.rs:31-33` stores the final
  result receiver as an `Option<oneshot::Receiver<_>>`.
- `operation.rs:72-80` calls `self.result_rx.take()` before waiting.
- `operation.rs:87-103` moves that receiver into the timed body. When
  `tokio::time::timeout()` expires, the body and receiver are dropped.
- A second `wait()` then hits `operation.rs:73-79` and returns
  `PvaError::Protocol("Operation result already consumed")`.

pvxs reference:

- `~/codes/pvxs/src/pvxs/client.h:132-142` defines `Operation::wait(timeout)` as
  waiting for completion, timeout, or interruption.
- `~/codes/pvxs/src/client.cpp:287-299` throws `Timeout()` when
  `notify.wait(timeout)` expires, without changing the waiter's `outcome`.
- `client.cpp:301-310` changes the outcome only when the operation completes or
  is interrupted.

Impact:

A caller that starts a long GET/RPC, waits with a short deadline, and then wants
to wait again cannot recover the eventual result through the same handle. pvxs
callers can use repeated bounded waits as a polling pattern; Rust turns the first
timeout into a permanent loss of the operation result.

Fix direction:

Keep the operation result in shared state instead of moving the receiver out for
each wait attempt. A timeout should leave the state in `Busy`, while completion
stores either the value or error for exactly one successful consumption policy.
If the Rust API intentionally wants single-consumer final results, consume only
when the operation actually completes, not when a deadline expires.

Regression tests to add:

- Start an operation that completes after the first wait deadline; assert the
  first `wait(Some(short))` returns timeout and the second `wait(None)` returns
  the operation result.
- Repeated short timeouts do not consume the result.
- A completed operation still enforces the chosen single-consumer policy after
  one successful result read.

## PVA-FR-8: server monitor pause drops queued updates instead of holding them

pvxs `Subscription::pause(true)` sends MONITOR STOP and moves the server-side
monitor operation to `Idle`, but source `post()` calls still enqueue updates in
the monitor queue. A later START resumes emission from that queue, subject to
the normal queue limit and squash policy. Rust flips a pause flag in the server
monitor loop and drops every source event observed while the flag is set.

Rust evidence:

- `crates/epics-pva-rs/src/codec.rs:223-236` sends MONITOR STOP as subcmd
  `0x04` and START/resume as subcmd `0x44`, matching pvxs wire commands.
- `crates/epics-pva-rs/src/server_native/tcp.rs:549-555` documents
  `monitor_paused` as a task-local gate that skips before emit.
- `tcp.rs:3191-3211` sets the pause flag on STOP and clears it on START.
- `tcp.rs:3662-3679` checks the pause flag after receiving a source event and
  `continue`s, so the value is neither queued for resume nor squashed to the
  latest paused value.
- `tcp.rs:2459-2468` maps CANCEL_REQUEST to the same pause flag, so cancel-like
  pause has the same drop behavior.

pvxs reference:

- `~/codes/pvxs/src/clientmon.cpp:115-140` sends STOP (`0x04`) and START
  (`0x44`) for `Subscription::pause()`.
- `~/codes/pvxs/src/servermon.cpp:671-688` changes the monitor operation state
  between `Idle` and `Executing` and calls `maybeReply()` on START.
- `servermon.cpp:271-296` queues posted values before `maybeReply()` without
  requiring the operation state to be `Executing`.
- `servermon.cpp:211-220` only schedules replies when the state is `Executing`,
  so queued paused values remain server-side until resume.

Impact:

A Rust PVA client that pauses a monitor while the server changes value loses
those paused updates when the server is also Rust. Against pvxs, the same pause
preserves updates up to the monitor queue limit and delivers the queued or
squashed value after resume. This affects gateway backpressure and UI pause
features that rely on pause/resume preserving the latest state without a new
monitor INIT.

Fix direction:

Move pause from a drop gate to the monitor queue owner. While paused, source
events should still enter the per-monitor queue and use the same limit/squash
rules as normal backlog. START should drain queued events through the existing
pipeline credit path. CANCEL_REQUEST should follow the same Idle-state queueing
semantics unless the request is a destroy/teardown command.

Regression tests to add:

- Pause a monitor, post two values, resume, and assert the queued or squashed
  latest value is delivered.
- Verify paused posts do not consume pipeline credit until a DATA frame is sent.
- Verify CANCEL_REQUEST followed by START resumes from the retained monitor
  queue.
- Verify DESTROY still releases the source-side subscription.

## PVA-FR-9: `PvaOperation::interrupt()` is reported as a timeout

pvxs distinguishes a deadline timeout from an explicit operation interruption:
`wait(timeout)` throws `Timeout` when the deadline expires and `Interrupted`
when `Operation::interrupt()` wakes the waiter. Rust has an internal
`WaitOutcome::Interrupted`, but maps it to the same public `PvaError::Timeout`
variant used by real deadlines.

Rust evidence:

- `crates/epics-pva-rs/src/error.rs:3-25` defines `PvaError::Timeout` but no
  `Interrupted` or operation-specific wait error.
- `crates/epics-pva-rs/src/client_native/operation.rs:34-37` has distinct
  internal `Interrupted` and `Cancelled` outcomes.
- `operation.rs:55-58` documents that interrupt returns `PvaError::Timeout`
  because it is the closest existing variant.
- `operation.rs:120-126` returns `PvaError::Timeout` for a real deadline.
- `operation.rs:134-136` returns the same `PvaError::Timeout` for
  `WaitOutcome::Interrupted`.

pvxs reference:

- `~/codes/pvxs/src/pvxs/client.h:67-79` declares separate `Interrupted` and
  `Timeout` exception types.
- `client.h:132-150` documents `Operation::wait()` as throwing `Timeout` for
  deadline expiry and `Interrupted` for `interrupt()`.
- `~/codes/pvxs/src/client.cpp:287-299` throws `Timeout()` when the wait expires
  and `Interrupted()` when the waiter outcome is aborted by interrupt.
- `client.cpp:333-337` implements `OperationBase::interrupt()` by completing the
  waiter with the interrupt path.

Impact:

Callers cannot tell whether an operation exceeded its deadline or another task
explicitly interrupted the wait. Retry, cancellation, logging, and operator UI
code can report the wrong cause or apply timeout-specific policy to a manual
interrupt.

Fix direction:

Add a distinct public error for operation interruption, for example
`PvaError::Interrupted`, or introduce an operation-specific wait error type that
separates timeout, interrupt, cancellation, protocol failure, and operation
result errors. `interrupt()` should leave the operation result recoverable by a
later wait, while returning the distinct interrupted outcome for the current
waiter.

Regression tests to add:

- A timed wait that expires returns the timeout variant.
- `interrupt()` wakes a pending wait with the interrupted variant.
- After an interrupted wait, a later wait can still receive the operation result.
- Timeout-specific caller logic does not match the interrupted variant.

## PVA-FR-10: subscription stats discard monitor overrun information

pvxs exposes per-subscription queue statistics, including server-reported
squash/overrun counts from the MONITOR DATA overrun bitset and client-side queue
high-water data. Rust defines similar fields on `SubscriptionStat`, but the
typed monitor decoder reads and discards the overrun bitset, and the handle stats
track ACK batching rather than the subscription queue shape.

Rust evidence:

- `crates/epics-pva-rs/src/decode.rs:326-338` defines
  `OpDataResponse` with `changed` and `value`. (`decode.rs` split out of
  `client_native/` in `24d514e8`; this gap is since closed — `overrun` is now
  a field on `OpDataResponse`, line 338.)
- `crates/epics-pva-rs/src/decode.rs:647` decodes the MONITOR DATA overrun
  bitset — into the real `overrun` field now, not dropped (regression-guarded by
  `monitor_data_preserves_overrun_bitset`, `decode.rs:1588`).
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:1152-1169` exposes
  `n_cli_squash`, `n_srv_squash`, `max_queue`, and `limit_queue` in
  `SubscriptionStat`.
- `ops_v2.rs:2092-2110` handles typed monitor DATA without consulting any
  overrun bits, so `n_srv_squash` cannot increment.
- `ops_v2.rs:2105-2109` updates `max_queue` from `events_since_ack`, not from a
  client-side subscription queue depth.
- `ops_v2.rs:1155-1158` documents `n_cli_squash` as currently always zero.
- The raw monitor path preserves the overrun bytes for gateway forwarding at
  `ops_v2.rs:1688-1692`, but its handle stats at `ops_v2.rs:1694-1699` also do
  not parse them into `n_srv_squash`.

pvxs reference:

- `~/codes/pvxs/src/pvxs/client.h:165-177` defines `SubscriptionStat` with
  `nQueue`, `nSrvSquash`, `nCliSquash`, `maxQueue`, and `limitQueue`.
- `~/codes/pvxs/src/clientmon.cpp:549-558` parses the MONITOR overrun bitset and
  sets a `servSquash` flag when any word is non-zero.
- `clientmon.cpp:676-685` increments `nCliSquash` when the client queue squashes
  a value update.
- `clientmon.cpp:710-717` updates `queueMax` from actual queue depth and
  increments `nSrvSquash` when the server overrun flag was present.
- `clientmon.cpp:144-153` reports the queue counters through
  `Subscription::stats(reset)`.

Impact:

`SubscriptionHandle::stats()` cannot report server-side monitor squash even when
an upstream PVA server sends a non-empty overrun bitset. It also cannot report
pvxs-equivalent queue depth or client squash because the public monitor API calls
the user callback synchronously instead of owning a pop queue. Operators and
gateways lose the same backpressure diagnostics that pvxs applications use to
detect dropped or coalesced monitor updates.

Fix direction:

Carry the MONITOR overrun bitset through `OpDataResponse` or a monitor-specific
data response type. Increment `n_srv_squash` when any overrun bit is set. If
Rust keeps the callback-first monitor API, either document that queue stats are
not pvxs-equivalent or add a queued subscription API whose stats owner tracks
`nQueue`, `maxQueue`, and `nCliSquash` like pvxs `Subscription::pop()`.

Regression tests to add:

- Decode a MONITOR DATA frame with a non-empty overrun bitset and assert the
  typed response preserves that information.
- A `SubscriptionHandle` increments `n_srv_squash` after such a frame.
- `stats(true)` resets `n_srv_squash` while preserving the configured queue
  limit.
- A queued subscription path, if added, reports `nQueue`, `maxQueue`, and
  `nCliSquash` from actual client queue behavior.

## PVA-FR-11: server monitor pause/resume does not expose pvxs `onStart(bool)`

pvxs gives the server-side monitor producer a callback when a client starts,
stops, pauses, resumes, or cancels monitor updates. Producers use
`MonitorControlOp::onStart(bool)` to gate expensive sampling or upstream
subscriptions. Rust implements pause/resume by toggling an atomic in the TCP
operation state, but there is no `ChannelSource` / `SharedPV` API for the
source to observe those state transitions.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/source.rs:523-538` exposes only
  `notify_watermark_high()` / `notify_watermark_low()` for monitor control
  callbacks.
- `source.rs:719-721` confirms the dynamic `ChannelSourceObj` surface has only
  the two watermark notifications; no start/stop notification is available.
- `crates/epics-pva-rs/src/server_native/shared_pv.rs:747-760` lets users
  install high / low watermark handlers, but there is no `set_on_start()` or
  equivalent pause/resume handler.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3197-3211` handles MONITOR
  PAUSE / RESUME by updating `op.monitor_paused` and waking the window notify;
  it does not call into the source.
- `tcp.rs:3677-3678` then drops events while paused locally, so the producer can
  keep doing work without being told the client is stopped.

pvxs reference:

- `~/codes/pvxs/src/pvxs/source.h:130-133` declares
  `MonitorControlOp::onStart(std::function<void(bool start)>)`.
- `~/codes/pvxs/src/servermon.cpp:34-39` calls `onStart(false)` when a running
  monitor is cancelled back to idle.
- `servermon.cpp:339-346` stores the user callback.
- `servermon.cpp:677-683` calls `onStart(start)` when MONITOR START / STOP
  changes the executing state.

Impact:

A Rust server source cannot suspend production when a pvxs client calls
`Subscription::pause(true)` or when a monitor is cancelled to idle. That loses
the main server-side resource-saving behavior of pvxs monitor control: upstream
pollers, gateway subscriptions, and hardware sampling tasks keep running and
the Rust TCP loop discards the resulting events after the fact.

Fix direction:

Add a source-level monitor state callback, for example
`ChannelSource::notify_monitor_start(name, start)` plus a `SharedPV` setter that
mirrors pvxs `onStart`. Route MONITOR START / PAUSE / CANCEL_REQUEST / DESTROY
state transitions through one monitor-control owner so callbacks fire exactly
once per Executing <-> Idle transition.

Regression tests to add:

- A `SharedPV` monitor receives `on_start(true)` on MONITOR START.
- MONITOR PAUSE or CANCEL_REQUEST fires `on_start(false)` without destroying the
  subscription.
- MONITOR RESUME fires `on_start(true)` once.
- DESTROY releases the subscription and does not double-fire after a prior
  pause.

## PVA-FR-12: initial channel search is capped at 200 ms instead of waiting for operation timeout

pvxs keeps a newly opened channel in the initial search list and retries through
the search scheduler until a server responds or the caller's operation wait
times out. Rust's initial resolve path wraps the first `SearchEngine::find()` in
`MULTI_SERVER_WINDOW` (200 ms), and if no response arrives in that window the
channel creation fails immediately with no server found.

Rust evidence:

- `crates/epics-pva-rs/src/client_native/search_engine.rs:172-174` defines
  `MULTI_SERVER_WINDOW` as 200 ms for collecting extra multi-server replies.
- `crates/epics-pva-rs/src/client_native/channel.rs:663-668` documents that
  initial search uses the same short window to surface a missing PV quickly.
- `channel.rs:700-707` wraps only `SearchReason::Initial` in
  `tokio::time::timeout(MULTI_SERVER_WINDOW, engine.find(...))`.
- `search_engine.rs:1236-1255` removes a pending search when the responder was
  closed by such an outer timeout, so later UDP or TCP name-server responses no
  longer complete that channel attempt.
- `channel.rs:722-723` maps an empty candidate list to
  `PvaError::Protocol("no servers found for PV")`.

pvxs reference:

- `~/codes/pvxs/src/client.cpp:42` sets the initial search delay to 10 ms, not a
  failure deadline.
- `client.cpp:377-380` places a new channel into `initialSearchBucket` and
  schedules the initial searcher.
- `client.cpp:740-746` schedules the initial search callback.
- `client.cpp:1243-1249` runs initial search by calling
  `tickSearch(SearchKind::initial, false)`.
- `client.cpp:287-293` and `:326-330` make the public operation wait timeout the
  timeout that returns `pvxs::client::Timeout`.

Impact:

On a busy host, high-latency network, slow TCP name server, or server that needs
more than 200 ms to answer the first search, Rust can fail `pvget-rs PV` with
"no servers found" even though pvxs would keep the channel search alive until
the caller's configured wait timeout. The problem is amplified for
`EPICS_PVA_NAME_SERVERS` because the TCP name-server task may still be
connecting when the initial 200 ms resolve timeout closes the responder.

Fix direction:

Use `MULTI_SERVER_WINDOW` only for `find_all()` / duplicate-PV collection after
the first response. A single-server initial `find()` should stay pending until a
SEARCH_RESPONSE arrives, and the operation-level timeout should own user-visible
failure. If a fast "missing PV" mode is needed, expose it as an explicit tool or
builder policy rather than baking it into pvxs-compatible initial search.

Regression tests to add:

- Initial `find()` remains pending past 200 ms when no server has responded.
- The public operation timeout, not `MULTI_SERVER_WINDOW`, terminates an initial
  unresolved channel.
- A TCP name-server response arriving after 200 ms still completes the initial
  channel attempt.
- `find_all()` still uses the 200 ms post-first-response collection window.

## Reviewed but not open

- NTNDArray is implemented in `crates/epics-pva-rs/src/nt/nd_array.rs`; older
  notes that list it as absent are stale.
- OriginTag UDP search handling is implemented in `server_native/udp.rs` and
  `codec.rs`; it is not an open item in this review.
- Partial monitor changed-bitset narrowing is implemented in
  `server_native/tcp.rs`; older BR-R29 notes are stale.
