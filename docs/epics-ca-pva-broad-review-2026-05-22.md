# EPICS CA/PVA Broad Review - 2026-05-22

Scope:

- `crates/epics-ca-rs`
- `crates/epics-pva-rs`
- Shared `crates/epics-base-rs` monitor/filter/database paths used by CA/PVA

References:

- Rust working tree at review time.
- epics-base C reference under `/Users/stevek/codes/epics-base`.
- pvxs C++ reference under `/Users/stevek/codes/pvxs`.

Method:

- Review current source, not only git diff. The worktree was clean at start.
- Prefer structural anchors: monitor flow control, malformed wire parsing,
  subscription coalescing, filter boundaries, lifecycle cleanup, and ID/credit
  accounting.
- Record every confirmed finding here as it is found.
- `kodex` MCP resources and the `parity-audit` skill were not available in
  this session, so this is a direct source review.

## Summary

| ID | Severity | Area | Status |
|----|----------|------|--------|
| BFR-1 | Medium | CA server monitor flow control | Fixed in current source |
| BFR-2 | Medium | PVA monitor INIT pipeline parsing | Fixed in current source |
| BFR-3 | Low | PVA monitor ACK credit accounting | Fixed in current source |
| BFR-4 | Medium | PVA RPC DATA payload parsing | Fixed in current source |
| BFR-5 | Medium | PVA GET/PUT/RPC subcmd lifecycle | Fixed in current source |
| BFR-6 | Medium | PVA GET_FIELD descriptor fallback | Fixed in current source |
| BFR-7 | Medium | CA monitor single-event filter context | Fixed in current source |
| BFR-8 | Medium | PVA PROCESS payload parsing | Open |
| BFR-9 | Medium | PVA raw monitor overrun parsing | Open |
| BFR-10 | Low | CA/base monitor drop accounting | Open |
| BFR-11 | Medium | PVA raw monitor control-frame parsing | Open |
| BFR-12 | Medium | PVA MONITOR FINISH op cleanup | Open |
| BFR-13 | Medium | PVA data-phase error response shape | Open |
| BFR-14 | Medium | PVA raw monitor re-encode error handling | Open |
| BFR-15 | Medium | PVA GET/PUT/RPC in-flight EXEC state | Open |

## Reviewed Anchors

Initial anchors:

- `rg -n "coalesce_while_paused|pop_coalesced|try_send|EVENTS_OFF|EVENTS_ON" crates/epics-ca-rs/src crates/epics-base-rs/src`
- `rg -n "parse_monitor_init_nack|pipeline_initial_nack|fetch_add\\(ack_count|monitor_window|AtomicU32" crates/epics-pva-rs/src/server_native/tcp.rs`
- `rg -n "decode_type_desc\\(|decode_pv_field\\(|\\.ok\\(\\)|unwrap_or\\(" crates/epics-pva-rs/src/server_native/tcp.rs`
- `rg -n "\\bwindow\\b|nack|ack" /Users/stevek/codes/pvxs/src/servermon.cpp /Users/stevek/codes/pvxs/src/clientmon.cpp`
- `rg -n "CMD_RPC|from_wire_type_value|M.good|bev.reset" /Users/stevek/codes/pvxs/src/serverget.cpp`
- `rg -n "0x10|lastRequest|subcmd" /Users/stevek/codes/pvxs/src/serverget.cpp /Users/stevek/codes/pvxs/src/clientget.cpp crates/epics-pva-rs/src/client_native/decode.rs crates/epics-pva-rs/src/server_native/tcp.rs`
- `rg -n "get_introspection_checked|unwrap_or\\(FieldDesc::Variant\\)|GET_FIELD" crates/epics-pva-rs/src/server_native/tcp.rs /Users/stevek/codes/pvxs/src/serverintrospect.cpp`
- `rg -n "apply_to_read_value\\(|db_post_single_event|db_create_event_log|dbfl_context_event|dbfl_context_read" crates/epics-ca-rs/src/server/tcp.rs crates/epics-base-rs/src/server/database/filters /Users/stevek/codes/epics-base/modules/database/src/ioc/db/dbEvent.c /Users/stevek/codes/epics-base/modules/database/src/ioc/rsrv/camessage.c`
- `rg -n "handle_process|decode_type_desc\\(&mut cur|op_process|Command::Process" crates/epics-pva-rs/src/server_native/tcp.rs crates/epics-pva-rs/src/client_native/ops_v2.rs crates/epics-pva-rs/src/proto/command.rs`
- `rg -n "RawMonitorEvent|reencode_raw_monitor|build_monitor_payload_raw|body_bytes|overrun|op_monitor_raw_frames" crates/epics-pva-rs/src/server_native crates/epics-pva-rs/src/client_native /Users/stevek/codes/pvxs/src/clientmon.cpp`
- `rg -n "dropped_monitor_events|record_dropped_monitor|coalesced|try_send\\(" crates/epics-base-rs/src/server crates/epics-ca-rs/src/server`
- `rg -n "frame.payload.len\\(\\) < 5|MONITOR FINISH|Status::decode|decode_op_response" crates/epics-pva-rs/src/client_native /Users/stevek/codes/pvxs/src/clientmon.cpp`
- `rg -n "build_monitor_finish|ch\\.ops\\.remove\\(&ioid\\)|ServerOp::cleanup|subcmd = 0x10" crates/epics-pva-rs/src/server_native/tcp.rs /Users/stevek/codes/pvxs/src/servermon.cpp /Users/stevek/codes/pvxs/src/serverconn.cpp`
- `rg -n "send_op_error\\(|payload.put_u8\\(0x08\\)|doReply|to_wire\\(R, subcmd\\)" crates/epics-pva-rs/src/server_native/tcp.rs /Users/stevek/codes/pvxs/src/serverget.cpp`
- `rg -n "reencode_raw_monitor|raw monitor reencode failed|M.good|clientmon.cpp" crates/epics-pva-rs/src/server_native/tcp.rs /Users/stevek/codes/pvxs/src/clientmon.cpp`
- `rg -n "data_task_abort = None|data_task_abort = Some|double-EXEC|state==ServerOp::Idle|Get exec in incorrect state" crates/epics-pva-rs/src/server_native/tcp.rs /Users/stevek/codes/pvxs/src/serverget.cpp`

## Findings

### BFR-1 - CA server `EVENTS_OFF` pause ignored producer overflow slots

Severity: medium.

Status: fixed in the current source.

Original Rust evidence:

- Earlier source drained a `ProcessVariable`/record-field coalesced slot only
  before waiting on the subscriber mpsc, while `coalesce_while_paused` drained
  only `rx.try_recv()` and `rx.recv()`.
- `crates/epics-base-rs/src/server/pv.rs:397-410` writes the newest
  `ProcessVariable` event into the per-subscriber coalesced slot when the mpsc
  is full.
- `crates/epics-base-rs/src/server/record/record_instance.rs:1902-1905`
  writes record-field overflow into the same kind of slot.
- Current `crates/epics-ca-rs/src/server/monitor.rs:68-110` now accepts a
  `pop_overflow` closure and folds that slot while paused.
- Current `crates/epics-ca-rs/src/server/monitor.rs:163-166` and
  `crates/epics-ca-rs/src/server/tcp.rs:3526-3530` pass `pop_coalesced` into
  that pause owner for `ProcessVariable` and record-field monitors.
- Current `crates/epics-ca-rs/src/server/monitor.rs:453-488` adds the overflow
  slot regression test.

Impact:

During `EVENTS_OFF`, if the per-subscriber mpsc fills, the newest producer
event can be parked in the overflow slot while `coalesce_while_paused` is
holding an older `pending` value. On `EVENTS_ON`, the server sends that older
pending value first, then the next outer loop drains the overflow slot and sends
the newer value. That violates the intended "paused monitor resumes with one
latest coalesced value" rule and can emit an avoidable stale frame after a flow
control pause.

Applied fix shape:

The pause owner now owns both sources of pending monitor data: the mpsc and the
producer overflow slot. `coalesce_while_paused` receives a slot-drain closure,
and both `ProcessVariable` and record-field monitor tasks route their
`pop_coalesced` path through it.

Regression tests to add:

- The unit test covers the pause-owner boundary directly: paused rx backlog is
  older than the overflow slot, and resume returns the slot value.
- A server-level record-field regression would still strengthen coverage.

### BFR-2 - PVA pipelined `MONITOR INIT` accepted truncated initial `nack`

Severity: medium.

Status: fixed in the current source after initial capture.

Initial Rust evidence:

- Earlier current-session source had `parse_monitor_init_nack` return `None`
  when the pipeline bit was set but the trailing u32 was missing or truncated,
  and `handle_op` converted that `None` into `opt.queue_size`.
- The current worktree now has `crates/epics-pva-rs/src/server_native/tcp.rs:607-620`
  returning `Result<Option<u32>, PvaError>`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3409` now propagates that
  decode error with `?`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:6966-6974` now asserts that a
  truncated initial `nack` is fatal.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/servermon.cpp:494-495` reads the trailing
  `nack` when the bit is set.
- `/Users/stevek/codes/pvxs/src/servermon.cpp:498-502` resets the connection
  on decode failure before the operation is registered.
- `/Users/stevek/codes/pvxs/src/servermon.cpp:546-552` warns about `nack == 0`;
  this is distinct from a missing/truncated u32.

Impact:

A malformed pipelined monitor INIT can be accepted with full default credit in
Rust, while pvxs treats the same frame as a fatal INIT decode failure. This
accepts a bad negotiation boundary and can leave client/server credit state
different from pvxs.

Applied fix shape:

The parser now distinguishes absent/present from malformed by returning
`Result<Option<u32>, PvaError>`, and `handle_op` propagates malformed input as a
decode failure. The `nack == 0` path remains distinct from truncated input.

Regression tests to add:

- Pipeline bit set with fewer than four trailing bytes returns a decode error
  from `handle_op`.
- Pipeline bit clear with no trailing bytes remains accepted.
- Pipeline bit set with `nack == 0` remains accepted but starts with zero
  window.

### BFR-3 - PVA monitor ACK credit could wrap in Rust-only `u32` accounting

Severity: low.

Status: fixed in the current source.

Original Rust evidence:

- Earlier source stored pipeline window credit in `AtomicU32`, applied ACK
  credit with `fetch_add(ack_count)`, and computed the HIGH watermark decision
  from an unwrapped `usize` sum.
- Current `crates/epics-pva-rs/src/server_native/tcp.rs:3965-4001` decodes the
  ACK count and updates the stored credit with a saturating CAS loop.
- Current `crates/epics-pva-rs/src/server_native/tcp.rs:4005-4012` drives the
  HIGH watermark check from the same saturated value now stored in the window.
- Current `crates/epics-pva-rs/src/server_native/tcp.rs:6622-6668` asserts that
  near-`u32::MAX` ACK refill saturates instead of wrapping.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/servermon.cpp:66` stores server-side monitor
  window credit in `size_t`.
- `/Users/stevek/codes/pvxs/src/servermon.cpp:653` applies `op->window += nack`.

Impact:

A large or repeated ACK can wrap the Rust server's stored window value much
earlier than pvxs on 64-bit platforms. The stored credit can become low or zero
while the watermark logic evaluates an unwrapped `usize` sum, so send gating and
HIGH watermark callbacks can diverge.

Applied fix shape:

The ACK owner now computes one saturated post-add value and stores exactly that
value before evaluating the HIGH watermark crossing. The stored credit and the
watermark decision therefore cannot diverge through `u32` wraparound.

Regression tests to add:

- The direct ACK test covers the stored-credit boundary. A gateway-level
  HIGH-callback test would still strengthen coverage.

### BFR-4 - PVA RPC DATA decode failures are converted into `Null` arguments

Severity: medium.

Status: fixed in the current source.

Original Rust evidence:

- Earlier source handled RPC DATA inline by decoding `type + full value`, but
  mapped descriptor decode failure to `(FieldDesc::Variant, PvField::Null)` and
  value decode failure to `(desc, PvField::Null)`, then passed the fabricated
  `req_value` to the source RPC handler.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2363-2386` now provides
  `decode_rpc_exec_arg`, which classifies the body as parameterless (absent or
  NULL `0xFF` type code), fully decoded, or present-but-malformed.
- `crates/epics-pva-rs/src/server_native/tcp.rs:4868-4878` now calls that helper
  and propagates a malformed body as a connection-fatal `PvaError::Decode` with
  `?`, matching pvxs `bev.reset()`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:7040-7102` adds the
  per-boundary regression test.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/serverget.cpp:443-447` decodes RPC EXEC as
  `from_wire_type_value`.
- `/Users/stevek/codes/pvxs/src/serverget.cpp:454-458` resets the connection
  when that decode leaves the message state bad.

Impact:

A malformed RPC EXEC can invoke the application RPC handler with a fabricated
`Null` argument instead of failing at the protocol boundary. That can turn a
wire corruption or truncated frame into a valid application call, which is both
non-parity with pvxs and harder for a service handler to distinguish from a
real parameterless RPC.

Applied fix shape:

`decode_rpc_exec_arg` removes the dual meaning that `decode_type_desc`'s error
previously carried (it stood for both "absent/NULL body (parameterless)" and
"present-but-undecodable descriptor (must be fatal)"). The helper now mirrors
pvxs `from_wire_type_value` exactly:

- An absent body or a NULL (`0xFF`) type code is a parameterless RPC. pvxs
  encodes a parameterless RPC as the single `0xFF` byte written by
  `clientget.cpp:308` `to_wire(R, desc(arg))` for a null arg; `decode_type_desc`
  rejects `0xFF` as caller-context dependent, so it is peek-handled. An empty
  body is additionally tolerated for Rust↔Rust interop (a deliberate lenient
  extension — pvxs underflows and resets on a truly empty body, but the Rust
  client always sends at least a descriptor, so this is benign).
- A present, non-null descriptor is decoded in full (type + value); any decode
  failure becomes a connection-fatal `PvaError::Decode`, matching pvxs
  `bev.reset()`.

Regression tests added (`tcp.rs:7040-7102`, per invariant boundary):

- Empty RPC EXEC body remains accepted as parameterless, cursor not advanced.
- NULL (`0xFF`) type code is parameterless and consumes exactly one byte even
  with trailing bytes present.
- A present, fully decodable descriptor + value round-trips to the exact arg.
- A truncated RPC EXEC descriptor is fatal.
- A valid descriptor plus a truncated value is fatal.

### BFR-5 - PVA GET/PUT/RPC data-phase `0x10` is not handled as last request

Severity: medium.

Status: fixed in the current source.

Original Rust evidence:

- Earlier source handled GET/PUT/RPC data EXEC, echoed the incoming `subcmd`
  in the response, and left the op in `ch.ops`; there was no data-phase
  `subcmd & 0x10` cleanup for GET/PUT/RPC (removals existed only in the
  PUT_GET/PROCESS subcmd-destroy handlers and the separate `DESTROY_REQUEST`
  path).
- The client decoder treated any op response whose `subcmd & 0x10 != 0` as a
  status-only FINISH/DESTROY frame before command-specific GET/PUT/RPC
  decoding, dropping the value body of a last-request data response.

Current Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:773-808` adds
  `finish_exec_data_task`, the read-loop owner's data-phase op continuation:
  on `subcmd & 0x10` it removes the op (pvxs `cleanup()`), otherwise it keeps
  the op and installs the in-flight task's abort guard.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3756`, `:3845`, `:3963`,
  and `:4953` route the GET / PUT-getback / plain-PUT / RPC EXEC spawn sites
  through that single finalizer.
- `crates/epics-pva-rs/src/client_native/decode.rs:389-403` now gates the
  status-only `0x10` branch on `cmd == Command::Monitor`, so GET/PUT/RPC data
  responses decode their value even with the last-request bit set.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/serverget.cpp:470-475` records
  `lastRequest = subcmd & 0x10` when executing a GET/PUT/RPC request.
- `/Users/stevek/codes/pvxs/src/serverget.cpp:102-115` still serializes the
  normal data response, then calls `cleanup()` when `lastRequest` is set.
- `/Users/stevek/codes/pvxs/src/clientget.cpp:445-452` decodes successful
  GET/PUT-with-getback data responses based on `get = subcmd & 0x40`; it does
  not preemptively classify every `0x10` response as status-only.

Impact:

A client that uses the pvxs-supported "execute and destroy/last request" bit on
GET/PUT/RPC can get a Rust server response with the bit echoed but the IOID left
registered. Conversely, a Rust client receiving a valid data response with that
bit set will decode only `Status` and drop the value body. Rust's own client
currently sends a separate `DESTROY_REQUEST`, so this hides in Rust-to-Rust
paths while remaining an interop and lifecycle gap.

Applied fix shape:

MONITOR FINISH handling stays separate from GET/PUT/RPC last-request handling.
Server GET/PUT/RPC EXEC executes normally and emits its data/status response
from the spawned task; the read-loop owner then applies `finish_exec_data_task`,
which removes the op on `subcmd & 0x10` (matching pvxs `cleanup()`) and
otherwise installs the abort guard. The op is dropped *without* installing the
abort guard on last-request, so removal does not cancel the still-running
response task. The client decoder reserves the status-only `0x10` shape for
MONITOR (`cmd == Command::Monitor`); GET/PUT/RPC decode their data by
`cmd`/`init`/`get` bits even when the last-request bit is set.

PUT_GET / PROCESS were classified as distinct: their handlers already treat
`subcmd & QosFlags::DESTROY` (`0x10`) as an upfront destroy that removes the op
and returns before the EXEC spawn, so they are out of this finding's scope.

Regression tests added:

- `tcp.rs` `get_exec_last_request_removes_op_after_response`: GET EXEC with
  `subcmd = 0x50` removes the op from `ch.ops` and the value response still
  arrives; a plain `subcmd = 0x00` GET EXEC keeps the op registered.
- `decode.rs` `get_data_response_with_last_request_bit_decodes_value`: a GET
  data response with `subcmd = 0x50` decodes as `OpResponse::Data` with the
  value body preserved.
- `decode.rs` `monitor_finish_remains_status_only`: MONITOR FINISH with
  `subcmd = 0x10` still yields `OpResponse::Status`.

### BFR-6 - PVA GET_FIELD fabricates a successful `Variant` descriptor

Severity: medium.

Status: fixed in the current source.

Original Rust evidence:

- The GET_FIELD slow path called
  `get_introspection_checked(...).await.unwrap_or(FieldDesc::Variant)` and then
  sent `Status::ok()` plus that fabricated descriptor.
- CREATE_CHANNEL allows a channel to be created with `found == true` and
  `intro == None` (`tcp.rs:3070-3088` stores `intro: Option`, None preserved),
  so the slow path is reachable for an existing PV whose source cannot provide a
  descriptor.

Current Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:5048-5072` now matches on the
  `Option<FieldDesc>`: `Some(desc)` → `Status::ok()` + descriptor; `None` →
  `Status::error(...)` with no descriptor word.
- The Rust client decoder already handles this shape:
  `crates/epics-pva-rs/src/client_native/decode.rs:690-696` returns
  `introspection: None` without decoding a descriptor when the status is not
  success.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/serverintrospect.cpp:74-80`
  `ServerIntrospectControl::connect` throws if asked to reply with a null
  prototype.
- `/Users/stevek/codes/pvxs/src/serverintrospect.cpp:83-87` has an explicit
  error path that replies with `Status::Error` and no descriptor.
- `/Users/stevek/codes/pvxs/src/serverintrospect.cpp:38-42` writes a descriptor
  only when `type` is non-null.

Impact:

A Rust GET_FIELD client can receive a successful `Variant` descriptor for a PV
whose source returned no descriptor. That hides the source failure and teaches
the client the wrong type tree. Later GET/PUT/MONITOR operations then fail with
"must provide prototype" or decode against the wrong expectation.

Applied fix shape:

The slow path no longer uses `FieldDesc::Variant` as a fallback. When the
source returns `None` it replies `Status::Error` with no descriptor word,
exactly mirroring pvxs `doReply(nullptr, Status::Error)` and the `if(type)`
guard (`serverintrospect.cpp:38-42,83-87`). Channel creation is left unchanged
(pvxs allows the late-descriptor channel and fails the GET_FIELD at reply time,
rather than rejecting channel creation).

Distinct sites classified and skipped: CREATE_CHANNEL (`tcp.rs:3070-3088`)
already preserves `intro: Option` (no fabrication); the client RPC-response
`response_desc.unwrap_or(FieldDesc::Variant)` (`ops_v2.rs`) is a legitimate
parameterless-RPC descriptor; the `PvField::Variant` desc fallback
(`structure.rs`) returns `Variant` for an already-Variant field.

Regression tests added (`tcp.rs`):

- `get_field_none_introspection_replies_error_no_descriptor`: slow path with a
  source that returns `None` replies `Status::Error` and `introspection: None`.
- `get_field_slow_path_returns_source_descriptor`: a valid source descriptor
  still round-trips through the slow path unchanged.

### BFR-7 - CA monitor single-event posts bypass event-context filters

Severity: medium.

Status: fixed in the current source.

Original Rust evidence:

- `crates/epics-ca-rs/src/server/tcp.rs` built the first `ProcessVariable` and
  record-field monitor snapshots with a fresh filter chain and called
  `apply_to_read_value` (read context: `dec`/`sync` bypass).
- The CA snapshot callers used `if let Some(v) = ... { snap.value = v; }`, so a
  filter drop left the original unfiltered value in the initial event.
- Access-revoke sent `no_read_access_event` frames directly, and access-restore
  sent current-value snapshots directly with `send_monitor_snapshot` — both
  outside the filter chain.
- The initial DENIED branches (SimplePv and record-field) sent the
  `ECA_NORDACCESS` frame unconditionally, never consulting the chain.

epics-base reference:

- `/Users/stevek/codes/epics-base/modules/database/src/ioc/rsrv/camessage.c:1851-1853`
  posts the initial event with `db_post_single_event` — called
  UNCONDITIONALLY at monitor creation, BEFORE the access check at 1858, so the
  initial DENIED `ECA_NORDACCESS` frame is gated by the chain too.
- `/Users/stevek/codes/epics-base/modules/database/src/ioc/rsrv/camessage.c:1085-1093`
  posts access-rights transition events through `db_post_single_event`:
  revoke = `db_post_single_event` then `db_event_disable` (1090-1092);
  restore = `db_event_enable` then `db_post_single_event` (1086-1088).
- `/Users/stevek/codes/epics-base/modules/database/src/ioc/db/dbEvent.c:746-752`
  creates event logs with `dbfl_context_event` (`db_create_event_log`).
- `/Users/stevek/codes/epics-base/modules/database/src/ioc/db/dbEvent.c:922-924`
  runs `dbChannelRunPreChain` and `db_queue_event_log` ONLY when the filtered
  log is non-null (`if(pLog)`) — a dropped post emits no frame at all.
- `/Users/stevek/codes/epics-base/modules/database/src/std/filters/decimate.c:64`
  and `.../sync.c:98` bypass only `dbfl_context_read`; event-context initial
  monitor events are still decimated or gated.

Impact:

A CA monitor created on a filtered channel received initial/access-transition
frames that C would drop. `sync` always passed Rust's initial snapshot even
when the state machine would suppress an event-context post; access restore
bypassed the chain entirely; a filter returning `None` on the initial path fell
back to the unfiltered value; and a non-default `dec` offset that decimates
window slot 0 was ignored on the initial post.

Current Rust source / Applied fix:

- `crates/epics-base-rs/src/server/database/filters/mod.rs` factors a private
  `apply_single_value(value, read_context)` and exposes two public methods:
  `apply_to_read_value` (read context, unchanged) and a NEW
  `apply_to_event_value` (event context — `FilteredMonitorEvent::new`, so
  `dec`/`sync` run; `None` means "drop the post, send no frame").
- `crates/epics-ca-rs/src/server/tcp.rs` routes every CA monitor single-event
  post (`db_post_single_event` equivalent) through `apply_to_event_value` on a
  fresh per-subscriber chain, translating `None` into "send nothing":
  SimplePv initial (granted + denied), record-field initial (granted via
  `and_then`, + denied gated under the lock), access-revoke
  (`no_read_access_event` skipped on drop), access-restore (filtered
  `send_monitor_snapshot`, skipped on drop). The genuine one-shot
  `READ`/`READ_NOTIFY` path keeps `apply_to_read_value`.

Regression tests added:

- `epics-base-rs` filters: `apply_to_event_value_decimates_while_read_value_bypasses`,
  `apply_to_event_value_suppresses_sync_while_read_value_passes`,
  `apply_to_event_value_drop_yields_none_no_fallback`,
  `apply_to_event_and_read_value_empty_chain_identity`,
  `apply_to_event_value_arr_slice_still_applies`.
- `epics-ca-rs` `bfr7_event_context_filter_tests` (end-to-end `handle_client`):
  `event_context_sync_gate_suppresses_initial_event` (no initial `EVENT_ADD`
  frame under a `sync`-while gate), `event_context_decimator_suppresses_initial_event`
  (`dec` offset 1 drops the fresh-counter slot-0 post),
  `plain_channel_sends_initial_event` (control — unfiltered channel still sends
  one). Revert-verified: flipping `apply_to_event_value` back to read context
  makes the two suppression tests observe the unfiltered initial frame
  (`got [(1, 24)]`).

### BFR-8 - PVA PROCESS accepts malformed INIT/DATA payloads

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/client_native/ops_v2.rs:2582-2588` shows the Rust
  PROCESS INIT shape: `sid + ioid + 0x08 + pvRequest`.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:2598-2603` shows the Rust
  PROCESS DATA shape: `sid + ioid + 0x00` with no payload.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2701-2706` decodes the INIT
  pvRequest with `.ok().and_then(...)`, then discards both descriptor and value
  errors.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2707-2717` registers the
  PROCESS op and replies `Status::ok()` even after that failed decode.
- `crates/epics-pva-rs/src/server_native/tcp.rs:2725-2779` treats every
  non-INIT PROCESS frame as a no-payload data phase and runs
  `process_checked`; it never verifies that the cursor is exhausted after
  `sid + ioid + subcmd`.
- `crates/epics-pva-rs/src/proto/command.rs:25-26` defines PROCESS as its own
  PVA command code, so this path is not covered by the generic GET/PUT/RPC
  malformed INIT helper.

Impact:

A malformed PROCESS INIT can be acknowledged and registered, and a PROCESS DATA
frame with trailing garbage can still trigger the source `process()` hook. That
turns a malformed wire frame into a state-changing operation. The adjacent
generic GET/PUT/MONITOR INIT path already distinguishes absent from malformed
pvRequest bodies; PROCESS has not been routed through the same boundary.

Expected fix shape:

Use the same structured pvRequest decoding policy as generic op INIT:
`Ok(None)` only when the pvRequest value is intentionally absent, and `Err` for
present but malformed bytes. For PROCESS DATA, require the cursor to be
exhausted after `sid + ioid + subcmd`; any trailing bytes should be a decode
error before `process_checked` is invoked.

Regression tests to add:

- PROCESS INIT with a truncated pvRequest descriptor/value is rejected and does
  not register the IOID.
- PROCESS DATA with extra trailing bytes returns a decode error and does not
  run the process hook.
- The Rust client's valid PROCESS INIT/DATA sequence remains accepted.

### BFR-9 - PVA raw monitor re-encode fabricates a missing overrun bitset

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/source.rs:700-703` defines
  `RawMonitorEvent.body_bytes` as the upstream `changed | value | overrun`
  triplet.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:1837-1841` captures raw
  monitor DATA as `frame.payload[5..]` and forwards it to the callback without
  decoding the triplet.
- `crates/epics-pva-rs/src/server_native/tcp.rs:4365-4379` uses
  `reencode_raw_monitor` when upstream and downstream byte order differ.
- `crates/epics-pva-rs/src/server_native/tcp.rs:5264-5277` decodes `changed`
  and the partial value, but maps an overrun-bitset decode failure to
  `BitSet::new()`.
- `crates/epics-pva-rs/src/client_native/decode.rs:497-501` requires a MONITOR
  overrun bitset in the normal typed client decode path.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/clientmon.cpp:545-550` decodes monitor DATA as
  value plus trailing overrun bitset.
- `/Users/stevek/codes/pvxs/src/clientmon.cpp:596-600` disconnects when the
  monitor message is not good after decoding.

Impact:

For same-endian raw forwarding, a malformed upstream monitor body is forwarded
as malformed downstream. For cross-endian forwarding, the re-encode path turns
the same missing/truncated overrun bitset into a valid empty overrun bitset.
That hides upstream wire corruption and loses the server-squash signal. The
same raw event therefore has different semantics depending only on negotiated
byte order.

Expected fix shape:

Treat `RawMonitorEvent.body_bytes` as a required triplet at the owner boundary.
Either validate the raw body before accepting it into the gateway stream, or at
least make `reencode_raw_monitor` return an error when the trailing overrun
bitset cannot be decoded. The direct raw path and re-encode path should agree:
valid triplet forwarded/re-encoded, malformed triplet tears down or drops with
an explicit protocol error.

Regression tests to add:

- Cross-endian raw monitor re-encode with a missing overrun bitset returns an
  error instead of emitting an empty overrun.
- Same-endian raw forwarding and cross-endian re-encode agree on malformed
  body handling.
- Non-empty overrun survives cross-endian re-encode.

### BFR-10 - Record-field monitor overflow drops are invisible to shared drop accounting

Severity: low.

Rust evidence:

- `crates/epics-base-rs/src/server/pv.rs:33-43` defines
  `DROPPED_MONITOR_EVENTS` as the process-global monitor event drop counter.
- `crates/epics-base-rs/src/server/pv.rs:348-359` and
  `crates/epics-base-rs/src/server/pv.rs:397-410` increment that counter when
  a `ProcessVariable` overflow replaces an already-occupied coalesced slot.
- `crates/epics-base-rs/src/server/record/record_instance.rs:1851-1854` and
  `crates/epics-base-rs/src/server/record/record_instance.rs:1902-1905`
  perform the same coalesced-slot overwrite for record-field monitors without
  incrementing the counter.
- `rg "dropped_monitor_events\\(" crates docs Cargo.toml` shows the counter is
  defined but not wired to a scrape/admin reader in the current workspace,
  despite the source comment saying it is exposed.

Impact:

Slow-consumer loss is counted for simple `ProcessVariable` monitors but not for
record-backed field monitors, which are the path most CA/PVA database monitors
use. Operators looking at the shared drop counter can see zero or an undercount
while record-field monitor updates are being replaced in coalesced slots.

Expected fix shape:

Move monitor drop accounting behind one shared helper/API used by both
`ProcessVariable` and `RecordInstance`, then wire `dropped_monitor_events()` to
the documented metrics/admin surface or correct the documentation. The counter
owner should be the coalesced-slot overwrite path: increment exactly when an
unobserved slot value is replaced.

Regression tests to add:

- Record-field monitor with a full subscriber queue and occupied coalesced slot
  increments the shared drop counter on replacement.
- Simple PV and record-field overflow use the same accounting helper.

### BFR-11 - Raw monitor loop treats malformed control frames as no-op or clean finish

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/client_native/ops_v2.rs:1794-1798` ignores raw
  monitor frames whose payload is shorter than `ioid + subcmd`.
- `crates/epics-pva-rs/src/client_native/ops_v2.rs:1805-1833` handles
  `subcmd & 0x10` as FINISH, but a `Status::decode` failure is ignored because
  only `Ok(non_success)` becomes fatal; truncated/invalid status falls through
  to `return Ok(())`.
- `crates/epics-pva-rs/src/client_native/decode.rs:394-397` requires the
  FINISH status in the normal typed op decoder and returns a decode error when
  it is missing or malformed.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/clientmon.cpp:471-477` decodes `ioid`,
  `subcmd`, and FINISH `Status`.
- `/Users/stevek/codes/pvxs/src/clientmon.cpp:596-600` resets the connection
  when the MONITOR message decode is not good.

Impact:

The raw monitor path is used by bridge/gateway code that wants to forward
monitor bodies without decoding. A malformed upstream MONITOR control frame can
be silently ignored, or a truncated FINISH can be reported as a clean end of the
raw monitor. That can suppress upstream protocol errors and leave the gateway
believing the monitor ended normally instead of reconnecting or surfacing a
fatal upstream frame.

Expected fix shape:

Route raw monitor control frames through the same status-decoding owner as the
typed monitor path. A payload shorter than `ioid + subcmd` and a FINISH with a
missing/malformed `Status` should return `MonitorEnd::Fatal(PvaError::Decode)`
or equivalent connection-lost handling, not `continue`/`Ok(())`.

Regression tests to add:

- Raw monitor loop receives a too-short MONITOR frame and returns a fatal
  decode error.
- Raw monitor FINISH with truncated status returns fatal decode error.
- Raw monitor FINISH with success status remains a clean end.

### BFR-12 - PVA MONITOR FINISH leaves the operation registered and executing

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:4832-4837` sends
  `build_monitor_finish(ioid, order)` when the decoded monitor source closes,
  but the spawned subscriber task has no ownership path back into `ch.ops`.
- `crates/epics-pva-rs/src/server_native/tcp.rs:4304-4306`,
  `:4367-4370`, `:4391-4393`, `:4442-4444`, `:4523-4525`,
  and `:4662-4664` have the same FINISH-and-return shape for ACL denial,
  descriptor change, raw-source close, and filter/transform boundaries.
- `crates/epics-pva-rs/src/server_native/tcp.rs:4857-4864` marks the op as
  `monitor_started = true` and fires `MonitorStartControl::set(true)`.
  Because the op is not removed when the task returns, the `MonitorStartControl`
  stored in `OpState` does not drop and does not fire the terminal
  `notify_monitor_start(false)`.
- `rg "ch\\.ops\\.remove\\(&ioid\\)" crates/epics-pva-rs/src/server_native/tcp.rs`
  shows removal paths only for PUT_GET/PROCESS destroy, DESTROY_REQUEST, and
  channel teardown, not for server-originated MONITOR FINISH.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/servermon.cpp:148-150` sets `subcmd = 0x10`
  and calls `self->cleanup()` before emitting the finish reply.
- `/Users/stevek/codes/pvxs/src/serverconn.cpp:487-508` implements
  `ServerOp::cleanup()` by marking the op dead and erasing the IOID from both
  channel and connection maps.

Impact:

After a Rust server sends MONITOR FINISH, the IOID can remain live in `ch.ops`
with `monitor_started = true`. A client that tries to re-INIT the same IOID
hits the duplicate-INIT fatal path, and a client that sends START again finds
`already_running == true` and no new subscriber is spawned. For gateway sources,
the source-side start notification can also remain in the executing state until
the client later sends DESTROY_REQUEST or the channel/connection closes.

Expected fix shape:

Route subscriber-task terminal events through the connection/op owner. The task
should report `Finished(ioid, status)` to the read-loop owner, and that owner
should atomically send FINISH, remove the op, and run the same terminal
start-control/drop finalizers used by DESTROY_REQUEST. The spawned task should
not be the only actor deciding that a monitor has ended, because it cannot
mutate `ch.ops`.

Regression tests to add:

- Source close sends MONITOR FINISH and removes the IOID from `ch.ops`.
- Source close fires `notify_monitor_start(false)` exactly once after an
  initial START.
- Re-INIT of the same IOID after server-originated FINISH is accepted as a
  fresh operation, not rejected as duplicate.
- Raw-path type-change/ACL-denial FINISH uses the same cleanup path.

### BFR-13 - PVA data-phase errors are emitted as INIT-phase responses

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:5051-5062` hardcodes
  `payload.put_u8(0x08)` in `send_op_error`, so every helper-generated error
  response is framed as INIT phase.
- Data-phase callers route through that helper:
  `crates/epics-pva-rs/src/server_native/tcp.rs:3635-3637` for a generic
  GET/PUT/RPC uninitialised op, `:3685-3701` for GET source-missing or
  descriptor-mismatch failures, `:3778-3789` for PUT readback source-missing
  failures, `:2587-2589` for PUT_GET uninitialised op, and `:2784-2792` for
  PROCESS uninitialised op.
- `crates/epics-pva-rs/src/client_native/decode.rs:404-427` treats
  `subcmd & 0x08 != 0` as an INIT response. A data-phase GET waiting for a
  value can therefore receive a legitimate server error but classify it as an
  unexpected INIT instead of a data-phase status.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/serverget.cpp:81-84` writes the operation's
  current `subcmd` into every GET/PUT/RPC reply.
- `/Users/stevek/codes/pvxs/src/serverget.cpp:86-94` handles an error by
  preserving the phase-specific reply shape: an executing op becomes idle,
  while a creating op is cleaned up.
- `/Users/stevek/codes/pvxs/src/serverget.cpp:470-475` records the data-phase
  subcmd on the op before executing the callback, so callback errors reply with
  the data-phase subcmd, not INIT.

Impact:

A Rust server can turn a data-phase failure into an INIT-shaped frame. Rust
clients then lose the original operation failure and surface a phase mismatch
such as "expected GET data, got INIT", while non-Rust clients may apply their
state-machine checks and disconnect on a server response that was only meant to
report a source/readback error. The error status payload is present, but it is
attached to the wrong wire phase.

Expected fix shape:

Split INIT-error and data-error helpers, or pass the response subcmd explicitly
from each call site. INIT negotiation failures should keep `0x08`; data-phase
errors should echo the request's data subcmd (`0x00`, `0x40`, or command-specific
shape) and let the op owner decide whether the op remains idle, is removed for
last-request, or tears down on protocol error.

Regression tests to add:

- GET data-phase source failure replies with subcmd `0x00` and status error.
- PUT readback (`subcmd & 0x40`) source failure replies with subcmd `0x40` and
  status error.
- INIT negotiation failures still reply with subcmd `0x08`.
- Rust client GET surfaces the server status from a data-phase error instead of
  reporting an unexpected INIT response.

### BFR-14 - Cross-endian raw monitor re-encode failures are dropped silently

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:4423-4433` calls
  `reencode_raw_monitor` for byte-order-mismatched raw events, but on `Err`
  only logs at debug and `continue`s the monitor loop.
- `crates/epics-pva-rs/src/server_native/tcp.rs:5317-5327` returns `Err` when
  the raw event's changed bitset or partial value cannot be decoded under the
  upstream byte order.
- Same-endian raw forwarding at
  `crates/epics-pva-rs/src/server_native/tcp.rs:4435-4437` does not decode the
  body, so malformed upstream bytes are forwarded downstream; cross-endian
  forwarding instead hides the malformed event by dropping it.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/clientmon.cpp:545-550` decodes MONITOR DATA
  as value plus overrun bitset.
- `/Users/stevek/codes/pvxs/src/clientmon.cpp:596-600` resets the connection
  when the monitor message is not good after decoding.

Impact:

A gateway can suppress upstream raw monitor corruption on cross-endian
connections. The downstream subscriber receives neither the bad frame nor a
FINISH/error frame, and the server keeps the monitor alive as if one update had
not existed. That differs from both pvxs decode behavior and Rust's same-endian
raw path, where the downstream peer still sees the malformed bytes and can fail
at its own protocol boundary.

Expected fix shape:

Make raw monitor validation/re-encode a protocol boundary. A re-encode failure
should route through the same terminal monitor-error owner as BFR-12, emitting a
non-success MONITOR FINISH or tearing down the upstream/downstream connection
according to the gateway policy. It should not be a debug-only dropped event.

Regression tests to add:

- Cross-endian raw monitor with truncated changed bitset terminates the monitor
  with an error instead of continuing.
- Cross-endian raw monitor with truncated partial value terminates the monitor
  with an error instead of continuing.
- Same-endian and cross-endian malformed raw handling have one documented
  policy, not byte-order-dependent behavior.

### BFR-15 - GET/PUT/RPC double EXEC aborts the in-flight operation

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:884-892` stores an
  `AbortOnDrop` for spawned GET/PUT/RPC/PUT_GET/PROCESS data-phase tasks.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3658-3662` explicitly drops
  the previous GET data task on a double EXEC before spawning a new GET task.
- `crates/epics-pva-rs/src/server_native/tcp.rs:3757-3759` and `:3856-3859`
  do the same for PUT readback and PUT exec.
- `crates/epics-pva-rs/src/server_native/tcp.rs:4928-4931` overwrites the RPC
  task abort guard after spawning the new task; assigning the new guard drops
  any old guard and aborts the previous RPC task.
- There is no GET/PUT/RPC `Executing` state in `OpState`, so the second EXEC
  is not rejected while the first source future is still running.

pvxs reference:

- `/Users/stevek/codes/pvxs/src/serverget.cpp:467-476` executes a GET/PUT/RPC
  request only when the op state is `Idle`, then sets the op state to
  `Executing`.
- `/Users/stevek/codes/pvxs/src/serverget.cpp:511-514` logs an incorrect-state
  EXEC and does not call the operation handler again while the first EXEC is
  still running.
- `/Users/stevek/codes/pvxs/src/serverget.cpp:86-116` moves the op back to
  `Idle` only when the original callback replies, or cleans it up when
  `lastRequest` was set.

Impact:

A peer can cancel a slow GET/PUT/RPC source future by sending another EXEC for
the same IOID before the first task replies. For PUT/RPC this is not just a
duplicate response issue: the first operation may have already entered
source-side code and then be cancelled at an await point while the second
operation runs. That diverges from pvxs' single in-flight operation invariant
and can expose cancellation boundaries inside source implementations as a wire
visible behavior.

Expected fix shape:

Model non-monitor op state explicitly: `Idle`, `Executing(task)`, and terminal
or destroying states. The data-phase owner should transition `Idle -> Executing`
exactly once per accepted EXEC, reject or ignore a second EXEC while executing
according to the pvxs policy, and transition back to `Idle` only when the
original task sends its response. DESTROY_REQUEST should remain the owner that
cancels an executing task.

Regression tests to add:

- A second GET EXEC while the first GET source future is pending does not abort
  the first future and does not start a second source call.
- A second PUT EXEC while the first PUT future is pending does not abort the
  first write.
- DESTROY_REQUEST still aborts the in-flight task.
- A completed GET/PUT/RPC task returns the op to Idle so a later explicit
  re-EXEC works when the protocol permits it.
