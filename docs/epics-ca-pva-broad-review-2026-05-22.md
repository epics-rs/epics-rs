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
| BFR-5 | Medium | PVA GET/PUT/RPC subcmd lifecycle | Open |
| BFR-6 | Medium | PVA GET_FIELD descriptor fallback | Open |
| BFR-7 | Medium | CA monitor single-event filter context | Open |
| BFR-8 | Medium | PVA PROCESS payload parsing | Open |
| BFR-9 | Medium | PVA raw monitor overrun parsing | Open |
| BFR-10 | Low | CA/base monitor drop accounting | Open |
| BFR-11 | Medium | PVA raw monitor control-frame parsing | Open |

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

Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:3582-3668` handles GET data
  EXEC, echoes the incoming `subcmd` in the response, and leaves the op in
  `ch.ops`; there is no data-phase `subcmd & 0x10` cleanup for GET/PUT/RPC.
- `rg "ch\\.ops\\.remove\\(&ioid\\)" crates/epics-pva-rs/src/server_native/tcp.rs`
  shows removals only in PUT_GET/PROCESS subcmd-destroy handlers and the
  separate `DESTROY_REQUEST` path, not the generic GET/PUT/RPC EXEC path.
- `crates/epics-pva-rs/src/client_native/decode.rs:389-397` treats any op
  response whose `subcmd & 0x10 != 0` as a status-only FINISH/DESTROY frame
  before command-specific GET/PUT/RPC decoding.

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

Expected fix shape:

Keep MONITOR FINISH handling separate from GET/PUT/RPC last-request handling.
For server GET/PUT/RPC, execute normally, emit the normal data/status response,
then remove the op when `subcmd & 0x10` was set. For the client decoder, decode
GET/PUT/RPC data according to command/state even when the bit is set; reserve
status-only `0x10` for MONITOR FINISH or actual destroy-status shapes.

Regression tests to add:

- GET EXEC with `subcmd = 0x50` returns a value and removes the server op.
- Client decode of a GET data response with `subcmd = 0x50` yields
  `OpResponse::Data`.
- MONITOR FINISH with `subcmd = 0x10` still yields a status/end event.

### BFR-6 - PVA GET_FIELD fabricates a successful `Variant` descriptor

Severity: medium.

Rust evidence:

- `crates/epics-pva-rs/src/server_native/tcp.rs:4972-4976` calls
  `get_introspection_checked(...).await.unwrap_or(FieldDesc::Variant)` in the
  GET_FIELD slow path.
- `crates/epics-pva-rs/src/server_native/tcp.rs:4977-4980` then sends
  `Status::ok()` plus that descriptor.
- `crates/epics-pva-rs/src/server_native/tcp.rs:1853-1860` allows a channel
  to be created with `found == true` and `intro == None`, so this slow path is
  reachable for an existing PV whose source cannot provide a descriptor.

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

Expected fix shape:

Do not use `FieldDesc::Variant` as a fallback for GET_FIELD. If the source
returns `None`, reply with `Status::Error` and no descriptor, or make channel
creation reject `found == true && intro == None` for sources that cannot support
descriptor-late operations.

Regression tests to add:

- Existing PV with `get_introspection_checked == None` returns a GET_FIELD
  error status.
- Successful GET_FIELD still returns the exact descriptor from the source.

### BFR-7 - CA monitor single-event posts bypass event-context filters

Severity: medium.

Rust evidence:

- `crates/epics-ca-rs/src/server/tcp.rs:3312-3326` builds the first
  `ProcessVariable` monitor snapshot with a fresh filter chain and calls
  `apply_to_read_value`.
- `crates/epics-ca-rs/src/server/tcp.rs:3441-3454` does the same for
  record-field monitor snapshots.
- `crates/epics-base-rs/src/server/database/filters/mod.rs:186-204`
  implements `apply_to_read_value` by constructing a read-context event.
- `crates/epics-base-rs/src/server/database/filters/decimate.rs:58-66` and
  `crates/epics-base-rs/src/server/database/filters/sync.rs:180-188` bypass
  their state machines when `read_context` is set.
- The CA snapshot callers use `if let Some(v) = ... { snap.value = v; }`, so a
  filter drop leaves the original unfiltered value in the initial event.
- `crates/epics-ca-rs/src/server/tcp.rs:4247-4294` sends access-revoked
  `no_read_access_event` frames directly, outside the filter chain.
- `crates/epics-ca-rs/src/server/tcp.rs:4299-4331` sends access-restored
  current-value snapshots directly with `send_monitor_snapshot`, also outside
  the filter chain.

epics-base reference:

- `/Users/stevek/codes/epics-base/modules/database/src/ioc/rsrv/camessage.c:1812-1813`
  registers CA monitor subscriptions with `db_add_event(..., read_reply, ...)`.
- `/Users/stevek/codes/epics-base/modules/database/src/ioc/rsrv/camessage.c:1851-1853`
  posts the initial event with `db_post_single_event`.
- `/Users/stevek/codes/epics-base/modules/database/src/ioc/rsrv/camessage.c:1085-1093`
  also posts access-rights transition events through `db_post_single_event`
  before enabling/disabling the event subscription.
- `/Users/stevek/codes/epics-base/modules/database/src/ioc/db/dbEvent.c:746-752`
  creates event logs with `dbfl_context_event`.
- `/Users/stevek/codes/epics-base/modules/database/src/ioc/db/dbEvent.c:922-924`
  runs the pre-chain and queues the single event only when the filtered log is
  non-null.
- `/Users/stevek/codes/epics-base/modules/database/src/std/filters/decimate.c:64-76`
  and `/Users/stevek/codes/epics-base/modules/database/src/std/filters/sync.c:98-140`
  bypass only `dbfl_context_read`; event-context initial monitor events are
  still decimated or gated.

Impact:

A CA monitor created on a filtered channel can receive initial/access-transition
frames that C would drop. `sync` filters always pass Rust's initial snapshot
even when the state machine would suppress an event-context post, access
restore bypasses the chain entirely, and any filter returning `None` on the
initial path falls back to the unfiltered value. `dec` with the default C
semantics passes the first event, but Rust still uses the wrong context
boundary and would also bypass any non-default Rust decimator offset.

Expected fix shape:

Separate one-shot reads from monitor single-event posts. Keep
`apply_to_read_value` for `READ`/`READ_NOTIFY`, but add an event-context helper
that uses `FilteredMonitorEvent::new`, preserves the fresh-chain state
isolation where required, and lets `None` mean "do not send this single event."
Use the same helper for `ProcessVariable`, record-field initial snapshots, and
access-rights revoke/restore posts.

Regression tests to add:

- `sync` initial monitor snapshot is suppressed when the C event-context rule
  would suppress it.
- A filter returning `None` for the initial snapshot causes no initial
  `EVENT_ADD` frame, not an unfiltered fallback.
- Access restore runs the monitor event-context filter chain before sending the
  restore snapshot.
- Access revoke does not send `no_read_access_event` when the event-context
  filter drops the single event.
- `arr`/`ts` initial snapshot transforms still apply.

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
