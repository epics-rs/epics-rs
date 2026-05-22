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
| BFR-8 | Medium | PVA PROCESS payload parsing | Fixed in current source (INIT); DATA-phase strictness declined (reference parity) |
| BFR-9 | Medium | PVA raw monitor overrun parsing | Fixed in current source |
| BFR-10 | Low | CA/base monitor drop accounting | Fixed in current source (accounting); metrics-surface wiring deferred |
| BFR-11 | Medium | PVA raw monitor control-frame parsing | Fixed in current source |
| BFR-12 | Medium | PVA MONITOR FINISH op cleanup | Fixed in current source |
| BFR-13 | Medium | PVA data-phase error response shape | Fixed in current source |
| BFR-14 | Medium | PVA raw monitor re-encode error handling | Fixed in current source |
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

Status: INIT defect fixed in the current source. The DATA-phase
cursor-exhaustion proposal was evaluated and intentionally NOT applied
(see "DATA-phase decision" below) because it would diverge from both
pvxs and the sibling generic GET EXEC path.

Original Rust evidence (INIT defect):

- `crates/epics-pva-rs/src/server_native/tcp.rs` decoded the PROCESS INIT
  pvRequest with `decode_type_desc(..).ok().and_then(|d|
  decode_pv_field(..).ok())`, discarding both descriptor and value errors,
  then registered the op and replied `Status::ok()` even after that failed
  decode. A truncated/corrupt PROCESS INIT was acknowledged and registered.

epics-base / pvxs reference:

- `/Users/stevek/codes/epics-modules/pvxs/src/serverget.cpp:366-374` — INIT
  (`subcmd&0x08`) decodes the pvRequest with
  `from_wire_type_value(M, rxRegistry, pvRequest)` and `if(!M.good())
  bev.reset()` (connection-fatal on a malformed pvRequest).
- `/Users/stevek/codes/epics-modules/pvxs/src/serverget.cpp:357,419-446` —
  the EXEC/data phase reads `from_wire(M, subcmd)` and dispatches; a no-value
  EXEC (GET / process) does NOT assert the buffer is fully consumed, so
  trailing bytes are tolerated.

Impact:

A malformed PROCESS INIT was acknowledged and registered with `Status::ok()`.
The adjacent generic GET/PUT/MONITOR INIT path already distinguishes absent
from malformed pvRequest bodies via `decode_init_pv_request_value`; PROCESS was
the one INIT site not routed through that boundary.

Current Rust source / Applied fix:

- `crates/epics-pva-rs/src/server_native/tcp.rs` `handle_process` INIT now
  decodes the pvRequest descriptor (malformed → `send_op_error`, return WITHOUT
  registering the IOID) and runs `decode_init_pv_request_value` (present-but-
  malformed value → `send_op_error`, no registration; absent value tolerated
  for Rust↔Rust interop). This mirrors the generic GET/PUT/MONITOR INIT
  boundary. PROCESS transfers no value, so the decoded pvRequest is discarded
  after validation. The op-error boundary is the codebase's deliberate uniform
  choice; pvxs is stricter (`bev.reset()`), but the Rust port consistently uses
  an op-error reply across all INIT paths.

DATA-phase decision (proposal declined):

The original "require the cursor exhausted after `sid + ioid + subcmd` on
PROCESS DATA; trailing bytes = decode error" was NOT implemented. The generic
GET EXEC path (`tcp.rs` data phase, `OpKind::Get`) — the structural sibling, a
no-value EXEC — does not check exhaustion either, and neither does pvxs
(`serverget.cpp` `from_wire(M, subcmd)` then dispatch, no `M.empty()` assert).
Adding the check to PROCESS alone would make it stricter than pvxs AND
inconsistent with the sibling GET EXEC, i.e. defensive validation the reference
does not perform. The frame's `sid + ioid + subcmd` is itself a valid process
request; trailing bytes are simply unread and change nothing. If extra
strictness is later desired, it should be applied uniformly to every no-value
EXEC (GET + PROCESS) as its own finding, not special-cased here.

Regression tests added:

- `bfr8_process_init_truncated_value_rejected_and_unregistered` — present-but-
  truncated pvRequest value → op-error, IOID not registered.
- `bfr8_process_init_missing_descriptor_rejected_and_unregistered` — no
  decodable descriptor → op-error, IOID not registered.
- `bfr8_process_init_valid_pvrequest_registers_op` — control: the Rust client's
  descriptor+value INIT registers the IOID and replies `Status::ok()`.
- Revert-verified: restoring the `.ok().and_then(..)` swallow makes both
  negative tests observe the op registered with `Status::ok()`.

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

Applied fix (2026-05-22):

- `crates/epics-pva-rs/src/server_native/tcp.rs` `reencode_raw_monitor` — the
  trailing overrun decode changed from
  `BitSet::decode(&mut cur, ev.byte_order).unwrap_or_else(|_| BitSet::new())`
  to `BitSet::decode(&mut cur, ev.byte_order).map_err(|e| format!("decode
  overrun bitset: {e}"))?`. A truncated/corrupt upstream body now propagates an
  error to the caller instead of fabricating a valid empty overrun. This makes
  the cross-endian re-encode path agree with the same-endian forward path, which
  carries malformed bytes through verbatim (the downstream client detects them).

Defect-family audit:

- Anchor: `BitSet::decode\(` across `crates/epics-pva-rs/src`. Every production
  decode site propagates with `.map_err(..)?` (`client_native/decode.rs:496,505`,
  `client_native/ops_v2.rs:2552`, `server_native/tcp.rs:2625,3891,5386,5407`);
  the remaining `.expect()`/`.unwrap()` hits are test-only. `tcp.rs:5407` was the
  sole fabricate-on-failed-decode site and is fixed this round.
- Anchor: `BitSet::new()` overrun construction (`tcp.rs:5222,5282,5333`,
  `decode.rs:480`). Distinct, not fixed: these are the ENCODE side (server emits a
  fresh frame with genuinely no overruns) or the RPC path (RPC responses carry no
  overrun bitset on the wire) — legitimate empty construction, not a default
  fabricated from a failed decode.
- Anchor: `decode(..)\.ok\(\)\?` (`codec.rs:303`, `udp.rs:1055`). Distinct, not
  fixed: incomplete-frame detection that returns `None` to signal "need more
  bytes" / "skip malformed datagram", not a fabricated value default.

Regression tests added (`crates/epics-pva-rs/src/server_native/tcp.rs`):

- `bfr9_reencode_missing_overrun_returns_error` — cross-endian re-encode of a body
  whose required overrun trailer is missing returns `Err` naming the overrun
  bitset, not a fabricated empty overrun.
- `bfr9_reencode_nonempty_overrun_survives_cross_endian` — a non-empty overrun
  (bit 1 set) survives a Little→Big re-encode; parsing the re-encoded frame under
  the downstream order recovers the value `(1,2,3)` and `overrun.get(1)`.
- `bfr9_same_endian_forward_does_not_fabricate` — same-endian
  `build_monitor_payload_raw` forwards the truncated body byte-for-byte (no
  fabricated overrun trailer), so both paths decline to invent a valid empty
  overrun.
- Revert-verified: restoring `.unwrap_or_else(|_| BitSet::new())` makes
  `bfr9_reencode_missing_overrun_returns_error` fail (the function returns a
  fabricated `Ok` frame instead of `Err`).

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

Applied fix (2026-05-22):

- `crates/epics-base-rs/src/server/pv.rs` adds `Subscriber::coalesce_overflow`,
  the single owner of the slow-consumer coalesce overflow: it locks the slot,
  records a dropped monitor event when it displaces an unobserved value
  (`slot.is_some()`), then stores the newest event. All four overflow-write sites
  now route through it — the two `ProcessVariable` posts (`pv.rs:380` value,
  `pv.rs:423` alarm) and the two `RecordInstance` field monitors
  (`record_instance.rs:1857` snapshot, `:1910` field). The record-field path
  previously overwrote the slot directly and never incremented the counter.
- The `DROPPED_MONITOR_EVENTS` doc comment over-claimed exposure ("Exposed for the
  `/queues` admin endpoint and the `dropped_events` Prometheus metric"). It is
  corrected: the counter has no live scrape reader in this workspace — `/queues`
  renders configured limits only. Wiring `dropped_monitor_events()` into a metrics
  surface is net-new feature work (cross-crate, changes the `/queues` JSON schema)
  and is deferred, not part of this fix; the accounting defect itself is closed.

Invariant / Owner / Bypass audit:

- Invariant: only `Subscriber::coalesce_overflow` may write the coalesce slot on
  the queue-full overflow path, and it is the sole site that records a
  slow-consumer drop.
- Owner: `Subscriber::coalesce_overflow` (`pv.rs:221`); the increment owner
  `record_dropped_monitor` (`pv.rs:57`) now has exactly one caller (the overflow
  owner).
- Bypass audit: `rg "\*slot = Some"` → one site, inside `coalesce_overflow`.
  `rg "coalesced.lock()"` → the owner plus the two `pop_coalesced` consumer drains
  (`pv.rs:526`, `record_instance.rs:2007`), which `.take()` an OBSERVED value and
  are correctly distinct (no drop accounting). `rg "coalesce_overflow"` → exactly
  four overflow-write callers.

Regression tests added (`crates/epics-base-rs/src/server/record/record_instance.rs`):

- `bfr10_record_field_overflow_counts_dropped_event` — drives a record-field
  monitor whose 64-deep queue and coalesce slot are full, then asserts
  `dropped_monitor_events()` strictly increases (process-global counter →
  monotonic assertion, robust under parallel tests).
- Revert-verified in isolation: restoring the direct `*slot = Some(event)` at both
  record-field sites makes the test observe `before=0, after=0` and fail.

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

Applied fix (2026-05-22):

- The raw monitor loop's control-frame decision was extracted from
  `op_monitor_raw_frames` into one pure function
  `crates/epics-pva-rs/src/client_native/ops_v2.rs` `classify_raw_monitor_frame`,
  returning a `RawMonitorFrameKind` enum (`Data` / `Skip` / `FinishOk` /
  `Fatal(PvaError)`). Both swallow bugs are closed:
  `payload.len() < 5` now returns `Fatal(PvaError::Decode)` (not `continue`), and
  a FINISH whose required `Status` cannot be decoded returns
  `Fatal(PvaError::Decode)` (not a fall-through to `Ok(())`). Success FINISH →
  `FinishOk` (clean end); non-success FINISH → `Fatal(PvaError::Protocol)`. The
  loop matches the enum: `Fatal`/`FinishOk` unregister the IOID, clear `active`,
  and return; `Skip` continues (pipeline ACK echo); `Data` forwards `payload[5..]`.
- This routes the control-frame status decode through `Status::decode` as its
  owner — mirroring the typed path — and makes the policy one testable point.

Defect-family audit:

- Anchor: `if let Ok(..) = Status::decode` swallow + `payload.len() < N` in the
  raw monitor loop. Same defect at `op_monitor_raw_frames` (both bugs), now via
  `classify_raw_monitor_frame`.
- Distinct, not fixed: the typed `run_monitor_loop` `Err(e) => debug!("MONITOR
  decode error")` (`ops_v2.rs:~2353`) decodes the FINISH status through
  `decode_op_response` (which validates it) and on error skips-and-continues the
  stream — it never returns a false clean `Ok(())` end nor treats a too-short
  frame as clean; not cited by this finding. INIT-phase decode-error arms already
  return `Fatal`.

Regression tests added (`crates/epics-pva-rs/src/client_native/ops_v2.rs`):

- `bfr11_too_short_frame_is_fatal_decode` — a `< 5`-byte payload → `Fatal(Decode)`.
- `bfr11_finish_truncated_status_is_fatal_decode` — FINISH (`0x10`) with no status
  bytes → `Fatal(Decode)`.
- `bfr11_finish_success_status_is_clean_end` — FINISH + `Status::ok()` → `FinishOk`.
- `bfr11_finish_error_status_is_fatal_protocol` — FINISH + error status →
  `Fatal(Protocol)`.
- `bfr11_data_frame_is_data` — subcmd `0x00` → `Data`.
- Revert-verified: restoring `Skip` for too-short and `FinishOk` for a status
  decode error makes the two negative tests fail.

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

Applied fix (Fixed in current source):

Routed every MONITOR subscriber-task terminal exit back through the read-loop
owner, mirroring the existing `CreateChannelCompletion` (`cc_tx`/`cc_rx`)
pattern that already solves the "spawned task cannot mutate `channels`" problem
for CREATE_CHANNEL:

- Added a per-connection unbounded completion channel
  `(mon_fin_tx, mon_fin_rx)` in the read loop
  (`crates/epics-pva-rs/src/server_native/tcp.rs`, alongside `cc_tx`/`cc_rx`)
  and a third `tokio::select!` arm
  (`fin_opt = mon_fin_rx.recv() => apply_monitor_finish(&mut channels, fin)`).
- Added `MonitorFinishGuard`, a single RAII local installed as the FIRST
  statement of the spawned subscriber task body. Its `Drop` reports
  `MonitorFinished { sid, ioid, op_id }` on the channel. Because it is one local
  at the top of the async block, EVERY exit drops it: the normal source-close
  FINISH fall-through, all 13 early `return;` sites (raw-path ACL-deny /
  initial-revalidate-deny / type-change FINISH / per-event revalidate-deny /
  BFR-14 re-encode `Terminate` / send-error; decoded-path revalidate-deny /
  filter `DescriptorMismatch` (initial and update) / send-error), a panic, and
  an `AbortOnDrop`-driven cancellation. The op-removal invariant holds by
  construction, not by remembering to signal on each path.
- `apply_monitor_finish` (the owner side) mirrors `handle_destroy_request`'s
  removal — dropping the `OpState` drops `monitor_start_ctl` (terminal
  `notify_monitor_start(false)`) and `monitor_abort` — but is GATED on the
  op-instance id (`OpState::monitor_op_id`). A signal whose `op_id` no longer
  matches the live op is ignored, so a late signal from an aborted task cannot
  evict a fresh op that re-used the ioid (ABA guard).

The decision to keep the task's own FINISH/ERROR wire sends (rather than move
them into the owner per the literal "atomically send FINISH" wording) was
deliberate: those sends are already correct on every path and are not the
defect. The defect was purely the registry leak — the op never leaving
`ch.ops`. The single-guard structure closes that family; moving the 6+ correct
wire-send sites would be churn that does not close any additional defect.

Re-INIT race: the guard enqueues the completion signal the instant the task
body's scope ends — right after the FINISH frame is handed to the writer mpsc,
strictly before the client can receive that FINISH and send a fresh INIT. The
owner therefore removes the op before any legitimate re-INIT of the same ioid is
read, so the re-INIT is accepted as fresh rather than rejected on the
duplicate-INIT fatal path. The `select!` arm is left unbiased so the existing
`cc_rx` ordering is unchanged.

Invariant:

- MUST: an op present in `ch.ops` with `monitor_started == true` ⟺ its
  subscriber task is alive.
- Owner/Gate: the read loop (the single actor that may mutate `channels`).
  `apply_monitor_finish` is the only writer of the finish transition; the
  subscriber task only signals.

Defect-family audit:

- Anchor: the monitor subscriber `tokio::spawn(async move { … })` body
  (`tcp.rs:4402-5014`) — every `return;` plus the natural fall-through end.
- Sites: `return;` at 4482, 4505, 4546, 4569, 4611, 4615, 4620, 4631, 4701,
  4747, 4782, 4840, 5005, and the source-close FINISH fall-through at 5012-5014.
- Same defect (closed by this change): ALL of them — the single `_fin_guard`
  at the top of the body covers every exit by Rust drop semantics.
- Distinct, skip: the GET/PUT/RPC/PUT_GET/PROCESS exec spawns
  (`finish_exec_data_task`) are two-shot and removed by the owner synchronously,
  so they never leak; the CREATE_CHANNEL resolver spawn already signals the
  owner via `cc_tx`. Neither is a long-lived self-terminating subscriber.

Regression tests added (`crates/epics-pva-rs/src/server_native/tcp.rs`, mod
`tests`):

- `bfr12_monitor_source_close_signals_owner_and_removes_op` (load-bearing,
  end-to-end): drives MONITOR INIT+START through `handle_op` against a source
  whose subscription closes, asserts the guard signals
  `(sid, ioid, monitor_op_id)`, that a MONITOR FINISH (subcmd `0x10`) was
  emitted, that the owner removes the op, and that `notify_monitor_start(false)`
  fires exactly once after the START's `true`.
- `bfr12_apply_finish_ignores_stale_op_id`: ABA guard — a stale `op_id` does not
  evict the live op (no terminal notify), a matching `op_id` does.
- `bfr12_reinit_after_finish_accepted_as_fresh`: after removal the
  duplicate-INIT gate (`ch.ops.contains_key(&ioid)`) no longer trips.
- `bfr12_finish_guard_signals_on_drop`: the guard mechanism itself.

Revert-verified:

- Removing the `MonitorFinishGuard` install in the spawned task →
  `bfr12_monitor_source_close_signals_owner_and_removes_op` times out at
  `mon_fin_rx.recv()` (the pre-fix leak: op never removed, terminal notify never
  fires).
- Removing the `op_id` gate in `apply_monitor_finish` →
  `bfr12_apply_finish_ignores_stale_op_id` fails (a stale signal evicts the live
  op).
- `cargo nextest run -p epics-pva-rs`: 533 passed. `cargo clippy -p epics-pva-rs
  --all-targets -- -D warnings`: clean.

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

Applied fix (Fixed in current source):

Made the error-reply phase byte echo the request subcmd uniformly, removing the
single hardcoded phase byte. This matches what every *success* reply already
does (`payload.put_u8(subcmd)`) and what pvxs does (`op->subcmd` into every
reply, `serverget.cpp:82-84`, recorded at `:475`):

- `send_op_error` (`crates/epics-pva-rs/src/server_native/tcp.rs:5311`) now takes
  a `subcmd: u8` parameter and writes it (`:5333`) instead of the hardcoded
  `payload.put_u8(0x08)`. All 24 call sites pass their in-scope request `subcmd`,
  so an INIT request (`0x08`) stays `0x08`, a GET exec failure echoes `0x00`, and
  a PUT readback failure echoes `0x40` — the same value the matching success
  path would have written. The exec-task error sites
  (`tcp.rs:3907`/`:3921` GET, `:4011` PUT readback) capture the data-phase frame
  subcmd, identical to their success `put_u8(subcmd)` (`:3934`/`:4025`).
- Client decode (`crates/epics-pva-rs/src/client_native/decode.rs:504-510`): a
  status-only GET/PUT data-phase reply (`!status.is_success()`) now short-circuits
  to `OpResponse::Status` BEFORE attempting to decode the changed-bitset/value
  body that pvxs never sends on error (`serverget.cpp:84-94`; value branch
  `:102-104` runs only on success). Without this the `BitSet::decode` below
  faulted on EOF (`short read u8`) and lost the server status behind a decode
  error.
- Client `op_get` (`crates/epics-pva-rs/src/client_native/ops_v2.rs:312`): added
  an `OpResponse::Status(s) => Err(Protocol("GET data: {status}"))` arm to the
  data-phase match, so the surfaced status is reported cleanly instead of hitting
  the `other =>` catch-all ("expected GET data, got Status"). Mirrors the RPC
  data path (`ops_v2.rs:2444`) and the PUT-done paths.

This is a structural fix, not a patch: the dual meaning ("error reply ⟹ INIT
phase") that the hardcoded `0x08` baked in is removed. The phase byte now has one
meaning on all paths — the request's own subcmd — so there is no INIT-vs-data
special case left to spawn a new edge.

Invariant:

- MUST: every GET/PUT/RPC reply (success OR error) carries the request's data-
  phase subcmd. There is no error-only phase byte.
- Owner/Gate: `send_op_error` is the single error-reply builder; it has no
  independent phase opinion — the caller supplies `subcmd` exactly as the success
  emitters do.

Defect-family audit:

- Anchor: `rg 'send_op_error'` (the error-reply builder) plus the hardcoded
  reply phase bytes `rg 'put_u8\(0x08\)'` in production paths.
- Sites: 1 `send_op_error` definition + 24 call sites
  (`tcp.rs` GET/PUT/RPC `handle_op`, `handle_put_get`, `handle_process`, and the
  GET/PUT-readback exec tasks). The only production hardcoded phase byte was the
  one inside `send_op_error`.
- Same defect (closed by this change): the `send_op_error` body — every caller
  now threads `subcmd`.
- Distinct, skip: the MONITOR payload builders (`build_monitor_payload*` write
  `0x00` data / `build_monitor_finish`+`build_monitor_error` write `0x10`
  finish, `tcp.rs:5342-5676`) are MONITOR's own subcmd semantics, not the
  GET/PUT/RPC error path; the success emitters' `put_u8(subcmd)` were already
  correct; the `0x08` literals in `mod tests`/INIT request encoders are client
  request frames, not server replies.

Regression tests added:

- `bfr13_get_data_phase_error_echoes_data_subcmd` (`tcp.rs`, mod `tests`): GET
  INIT then GET exec against a source whose value read fails → reply subcmd is
  `0x00` with an error status, not `0x08`.
- `bfr13_put_readback_error_echoes_0x40_subcmd`: PUT readback (`subcmd & 0x40`)
  whose readback GET fails → reply subcmd `0x40` with error status.
- `bfr13_init_phase_error_still_echoes_0x08` (boundary): an INIT-phase
  "must provide prototype" negotiation failure still echoes `0x08`.
- `bfr13_client_decodes_data_phase_error_as_status`: a status-only GET data error
  frame decodes to `OpResponse::Status`, not a `short read u8` decode fault.
- `bfr13_client_get_surfaces_data_phase_error`
  (`crates/epics-pva-rs/tests/service_framework.rs`, end-to-end): a real client
  `pvget` against a server whose source fails at the data phase returns a
  "GET data:" error and does NOT report "expected GET data, got Init".

Revert-verified:

- `send_op_error` `put_u8(subcmd)` → `put_u8(0x08)`:
  `bfr13_get_data_phase_error_echoes_data_subcmd` and
  `bfr13_put_readback_error_echoes_0x40_subcmd` fail (`left: 8, right: 0`/`64`);
  the end-to-end test fails with the exact pre-fix symptom
  `expected GET data, got Init(... subcmd: 8 ... "PV not found")`. INIT and decode
  tests still pass (`0x08` is correct for INIT; the decode test builds its own
  frame).
- Decode short-circuit disabled: `bfr13_client_decodes_data_phase_error_as_status`
  fails with `short read u8` (decoding the absent value body).
- `cargo nextest run -p epics-pva-rs`: 538 passed. `cargo clippy -p epics-pva-rs
  --all-targets -- -D warnings`: clean.

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

Applied fix (2026-05-22):

- The per-event forward-or-terminate decision was extracted from the ~200-line
  monitor-send closure into one pure function,
  `crates/epics-pva-rs/src/server_native/tcp.rs` `raw_monitor_frame`, returning a
  new `RawMonitorFrame` enum (`Forward(Vec<u8>)` /
  `Terminate { frame, reason }`). This makes the malformed-raw policy a single
  testable point and enforces "same-endian and cross-endian agree" by
  construction: same-endian forwards verbatim (`build_monitor_payload_raw`),
  cross-endian re-encodes (`reencode_raw_monitor`) and, on failure, returns
  `Terminate` carrying a `build_monitor_error` frame.
- The caller (formerly `Err(e) => { debug!(..); continue; }`) now matches the
  enum: `Terminate` logs at debug, sends the terminal MONITOR error frame, and
  `return`s — the same `send(error) + return` terminal shape the decoded path
  already uses for a descriptor-mismatch transform (`tcp.rs:~4618`). The silent
  `continue` is gone.

Defect-family audit:

- Anchor: `reencode_raw_monitor\(` production callers across
  `crates/epics-pva-rs/src`. Exactly one — inside `raw_monitor_frame`
  (`tcp.rs:5405`). The previous direct caller in the send loop now routes through
  `raw_monitor_frame`. `build_monitor_payload_raw` (same-endian) is infallible.
- Anchor: `continue;` inside the raw monitor send loop (`tcp.rs:4288-4525`). The
  only remaining `continue` is the legitimate paused-squash (`held_raw =
  Some(ev); continue;`); the re-encode failure path now `send`s an error frame
  and `return`s. No other site silently drops a malformed raw monitor event.

Regression tests added (`crates/epics-pva-rs/src/server_native/tcp.rs`):

- `bfr14_cross_endian_truncated_changed_bitset_terminates` — a cross-endian event
  whose changed bitset is truncated yields `Terminate` with a MONITOR FINISH
  (subcmd `0x10`) + non-success status; the reason names the changed bitset.
- `bfr14_cross_endian_truncated_value_terminates` — a cross-endian event whose
  partial value is truncated yields `Terminate`; the reason names the value.
- `bfr14_same_and_cross_endian_malformed_one_policy` — the SAME missing-overrun
  body forwards verbatim same-endian (`Forward`, body byte-for-byte) but
  terminates cross-endian (`Terminate`, MONITOR error).
- Revert-verified: changing the `Err` arm of `raw_monitor_frame` to `Forward`
  (instead of `Terminate`) makes all three tests fail.

Note: BFR-14 makes the failure terminal at the spawned-task boundary (the task's
existing `send + return` teardown). The orthogonal `ch.ops` registry cleanup for
ALL terminal monitor exits (type-change, ACL-denial, source-close FINISH, and
this error) is BFR-12's scope and is tracked separately.

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
