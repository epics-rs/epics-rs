# epics-pva-rs Source Review — 2026-05-26

Scope:

- Crate: `crates/epics-pva-rs`
- Upstream reference: pvxs C++ at `/Users/stevek/codes/pvxs/src` (read-only)
- Areas reviewed: server op lifecycle (GET/PUT/RPC/MONITOR/PROCESS INIT and data-phase error
  handling), CONNECTION_VALIDATION buffer-size field, server-side type registry on data-phase
  frame decode.
- Finding-ID series: `R-N` (the global parity-round series; this document records the
  epics-pva-rs slice — R60–R63 here). IDs are globally unique by prefix and never reused;
  see `docs/review-tagging-conventions.md`.

References:

- pvxs `serverget.cpp`: GET/PUT/RPC INIT and data-phase dispatch
- pvxs `servermon.cpp`: MONITOR INIT and data-phase dispatch
- pvxs `serverconn.cpp`: server → client CONNECTION_VALIDATION message
- pvxs `clientconn.cpp`: client → server CONNECTION_VALIDATION reply

Method:

Line-by-line comparison of server op-lifecycle error paths, handshake message fields, and
data-phase frame handling against the upstream C++ reference. Evidence bar: both a Rust
`path:line` and a C++ `path:line` required before classifying a finding as high-confidence.

## Findings

### R60 — INIT with unknown SID is not connection-fatal

Severity: Medium

Status: Fixed

Evidence:

- Rust site: `src/server_native/tcp.rs:3733-3740` — `handle_op` performs the SID lookup before
  checking the `INIT` bit (`subcmd & 0x08`); on unknown SID the current code calls
  `send_op_error(...)` and returns `Ok(())`, keeping the connection alive.
- C++ site: `pvxs/src/serverget.cpp:378-384` (GET/PUT/RPC) and
  `pvxs/src/servermon.cpp:505-511` (MONITOR) — inside the `if(subcmd & 0x08)` (INIT) branch,
  `lookupSID(sid)` returns a null reference on unknown SID; pvxs treats this as a protocol
  error and calls `bev.reset()` (connection-fatal), then returns.

Impact:

A buggy or malicious client can send GET/PUT/RPC/MONITOR INIT frames on a SID it never
received from the server. pvxs closes the TCP connection; our server sends an error reply and
remains connected. The practical effect is that protocol violations are not detected as such —
the peer can keep the connection open indefinitely by sending INITs on garbage SIDs.

Fix direction:

When the subcmd has the INIT bit set and the SID is unknown, propagate a
`PvaError::Decode(...)` so the connection-owner loop closes the connection (mirrors
`bev.reset()`). When the INIT bit is clear (data-phase), silently drop the frame (see
R61 for that half of the fix).

### R61 — Data-phase EXEC on unknown IOID sends error reply instead of silently dropping

Severity: Low-Medium

Status: Fixed

Evidence:

- Rust site: `src/server_native/tcp.rs:4071-4074` — the `None` arm in the `handle_op`
  data-phase IOID lookup calls `send_op_error(...)` and returns `Ok(())`. The same pattern
  appears in the PROCESS data-phase handler at `src/server_native/tcp.rs:3174-3186`.
- C++ site: `pvxs/src/serverget.cpp:423-428` (GET/PUT/RPC data phase) and
  `pvxs/src/servermon.cpp:611-619` (MONITOR non-INIT) — when `opByIOID.find(ioid)` returns
  `end()` or the op is `Dead`, pvxs sets `rxRegistryDirty = true` and returns silently (no
  reply). The comment at `:613-615` explains the design intent: "since server destroy commands
  aren't acknowledged, we can race with traffic sent by the client before processing our
  destroy. so we can't fault hard, so just ignore."

Impact:

A client can race a `DESTROY_REQUEST` with an in-flight EXEC frame. In pvxs, the server's
handler silently drops the stale frame. Our server sends an unexpected error reply on the
already-destroyed IOID, which the client may not be prepared to route. In well-behaved clients
this is harmless (the stale reply will be ignored), but it is a wire-level divergence and
could confuse strict client implementations.

Fix direction:

Change the `None` arm to a silent drop (`return Ok(())` with no `send_op_error`), matching
pvxs's intent. Also: for unknown SID in the data-phase (R60's `None` arm before the
subcmd split), silently drop as well.

### R62 — CONNECTION_VALIDATION `serverReceiveBufferSize` is 87 040 instead of 0x10000

Severity: Low

Status: Fixed

Evidence:

- Rust server site: `src/server_native/tcp.rs:1991` —
  `build_server_connection_validation(order, 87_040, 32_767, ...)`. The constant 87 040 is
  also the value of `DEFAULT_BUFFER_SIZE` in `src/client_native/server_conn.rs:806`, used by
  the client in its `CONNECTION_VALIDATION` reply.
- C++ server site: `pvxs/src/serverconn.cpp:103-104` — `to_wire(M, uint32_t(0x10000))`, with
  comment "serverReceiveBufferSize, not used".
- C++ client site: `pvxs/src/clientconn.cpp:292-293` — `to_wire(R, uint32_t(0x10000))`, with
  comment "serverReceiveBufferSize, not used".

Impact:

The `serverReceiveBufferSize` field is annotated "not used" in pvxs and is skipped by both
sides on decode (`M.skip(4u + 2u, ...)` in `serverconn.cpp:226` on the server receive side;
the field is read but unused on the client receive side per `decode_connection_validation` in
our code). Zero runtime effect. Wire-level divergence is visible in packet captures and may
confuse tooling that infers buffer capacity from this field.

Fix direction:

Change the hard-coded `87_040` to `0x10000` (65536) in both the server builder call and the
client's `DEFAULT_BUFFER_SIZE` constant, matching pvxs.

### R63 — `QosFlags::MONITOR_START` and `MONITOR_STOP` hold wrong subcmd values

Severity: Low (unused in production paths)

Status: Fixed

Evidence:

- Rust site: `src/proto/command.rs:113-115` — `MONITOR_START = 0x40` and `MONITOR_STOP = 0x80`.
  The correct protocol values are `START = 0x44` (`0x04 | 0x40`) and `STOP = 0x04`.
  Additionally `MONITOR_STOP = 0x80` aliases `PIPELINE_ACK = 0x80`, making both names refer to
  different protocol concepts with identical bit patterns.
- C++ site: `pvxs/src/clientmon.cpp:127` — `subcmd = p ? 0x04 : 0x44; // STOP | START`.
  `p` is the pause flag: pause → `0x04` (STOP), resume → `0x44` (START).
- C++ site: `pvxs/src/servermon.cpp:671-675` — `if(subcmd & 0x04) { bool start = subcmd & 0x40; }`
  confirms STOP = `0x04`, START = `0x04 | 0x40 = 0x44`.

Impact:

`MONITOR_START` and `MONITOR_STOP` are not used in any wire-building production code
(`codec.rs` uses hard-coded `0x44` and `0x04` directly). However, the constants are public
API; any caller relying on them to build monitor control frames would produce malformed
subcmds. The `MONITOR_STOP = 0x80` alias with `PIPELINE_ACK` is a silent semantic collision.

Fix direction:

Correct `MONITOR_START = 0x44` and `MONITOR_STOP = 0x04`. Also update the doc-comment for
`MONITOR_START` to note it combines the control bit (`0x04`) with the GET bit (`0x40`).

### R64 — RPC client INIT sends type-only; pvxs server expects type + full value

Severity: High (all Rust client RPC against a pvxs server)

Status: Fixed

Evidence:

- Rust site: `src/client_native/ops_v2.rs:2379-2390` — `op_rpc` builds the INIT payload as
  `encode_type_desc(request_desc)` only, omitting the full value. The comment at
  `src/server_native/tcp.rs:2582-2590` explicitly acknowledges "the Rust client's RPC INIT
  sends only the descriptor — tolerated for interop."
- C++ site: `pvxs/src/serverget.cpp:366-376` — INIT branch calls
  `from_wire_type_value(M, rxRegistry, pvRequest)` which reads type **and** full value, then
  `if(!M.good()) { bev.reset(); return; }`. If any non-null type is present, pvxs expects
  value bytes to follow; absent value bytes leave M bad → connection-fatal.
- C++ client: `pvxs/src/clientget.cpp:348-352` (`createOp`) — sends
  `to_wire(R, Value::Helper::desc(pvRequest)); to_wire_full(R, pvRequest);` at INIT.

Impact:

Any Rust PVA client RPC operation targeting a real pvxs server is immediately connection-fatal
after the RPC INIT frame when the argument type is non-null (i.e. any real RPC argument).
The Rust server tolerates type-only via `decode_init_pv_request_value`'s absent-body path,
so Rust↔Rust interop works; cross-server interop (Rust client → pvxs server) does not.

Fix direction:

Append the full value encoding after the type descriptor in the RPC INIT payload:
`encode_pv_field(request_value, request_desc, order, &mut pv_req)`.

## Uncertain Candidates

None. All investigated paths reached a definite conclusion (correct or bug). A full audit of
the `EncodeTypeCache` emit/decode round-trip, the search-engine backoff bucket algebra, and
the TLS x.509 peer-credential extraction is deferred to a future review.
