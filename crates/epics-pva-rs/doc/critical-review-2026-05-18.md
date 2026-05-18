# epics-pva-rs Critical Review - 2026-05-18

Scope: `crates/epics-pva-rs`.

This document is a running critical review of the PVA crate. Findings
below are limited to issues with a concrete code path and file/line
evidence. Product code was not changed during this review.

Reference implementation: `~/codes/pvxs` at git `f8d6192` (working tree
had an untracked `.DS_Store`, ignored for this review).

## Method

- Reviewed protocol framing, pvData encoding/decoding, client discovery
  and operations, server TCP/UDP paths, auth/TLS surfaces, CLIs, and tests.
- Preferred defect candidates that are externally observable:
  interoperability failures, incorrect wire semantics, hangs/resource
  leaks, panics on peer-controlled input, or test gaps hiding such issues.
- Kept speculative design preferences out of the findings list.

## Open Findings

### PVA-R1: Monitor pipeline negotiation is sent on the wrong wire phase

Severity: High

Rust client:

- `src/client_native/context.rs:155-158` exposes `pipeline_size`, defaulting
  to 4 at `context.rs:76`.
- `src/client_native/ops_v2.rs:1547-1553` builds the default monitor
  `pvRequest` without `record[pipeline=true,queueSize=N]`.
- `src/codec.rs:181-188` sends MONITOR INIT with subcmd `0x08` only.
- `src/codec.rs:191-207` sends `pipeline_size` as a 4-byte trailer on
  MONITOR START subcmd `0x44`.
- `src/client_native/ops_v2.rs:1263-1272` and `1648-1664` still emit
  MONITOR_ACK frames every `pipeline_size` events.

pvxs reference:

- `src/clientmon.cpp:327-342` puts the pipeline bit (`subcmd |= 0x80`) and
  the initial `queueSize` trailer on MONITOR INIT, not START.
- `src/clientmon.cpp:123-137` sends START/STOP as `sid + ioid + subcmd`
  only.
- `src/servermon.cpp:523-552` enables the server-side window only when the
  `pvRequest` contains `record._options.pipeline`, and treats a missing
  initial `nack` as an incompatible pipelined monitor.

Rust server code agrees with pvxs, not with the Rust client default:
`src/server_native/tcp.rs:2039-2069` enables a monitor window only when
`record[pipeline=true]` was negotiated, and `tcp.rs:201-218` reads the
initial `nack` only from an INIT whose subcmd has bit `0x80`.

Impact: the public `PvaClientBuilder::pipeline_size()` setting is misleading
for default `pvmonitor()` callers. The Rust client sends a non-pvxs START
trailer and later ACKs, but never actually asks the server to enter pipeline
mode unless the caller manually supplies a raw/custom pvRequest with
`record[pipeline=true,queueSize=N]`. This can hide missing flow control in
Rust-to-Rust tests because the Rust server correctly runs default monitors
without a window.

Fix direction: make pipeline negotiation a single owner on MONITOR INIT.
When `pipeline_size > 0`, synthesize or merge `record[pipeline=true,
queueSize=pipeline_size]`, set INIT bit `0x80`, append the initial `nack`, and
remove the START trailer. When `pipeline_size == 0`, send neither ACKs nor
pipeline options.

### PVA-R2: `PvaClientBuilder::tcp_timeout()` is stored but not applied

Severity: Medium

Rust:

- `src/client_native/context.rs:58-61` documents a client-side TCP idle
  timeout matching pvxs `Config::tcpTimeout`.
- `src/client_native/context.rs:93-97` stores the builder value.
- `src/client_native/context.rs:213-216` marks the built `ClientInner`
  field as future keepalive plumbing.
- `src/client_native/channel.rs:753-755` calls `ConnectionPool::get_or_connect`
  with only `op_timeout`; the configured `tcp_timeout` is not passed into the
  connection.
- `src/client_native/server_conn.rs:53-64` derives heartbeat interval and
  timeout from `EPICS_PVA_CONN_TMO` via `config::env::conn_timeout_secs()`.

pvxs reference:

- `src/pvxs/client.h:1038-1040` exposes `Config::tcpTimeout`.
- `src/config.cpp:596-598` maps `EPICS_PVA_CONN_TMO` into that same field.
- `src/clientconn.cpp:73-74` installs `effective.tcpTimeout` as the socket
  read/write inactivity timeout.
- `src/clientconn.cpp:162-166` derives the echo timer from
  `effective.tcpTimeout`.

Impact: users can call `.tcp_timeout(Duration::...)` and get no change in
live connection behavior. Only the environment-derived timeout affects the
heartbeat path, so programmatic contexts cannot match pvxs per-context
timeout behavior.

Fix direction: carry `tcp_timeout` into `ConnectionPool::get_or_connect` and
`ServerConn`, then derive heartbeat/idle timers from the connection's
effective setting instead of re-reading the process environment.

### PVA-R3: Nested Variant values lose the stream type-cache

Severity: Medium

Rust:

- `src/pvdata/encode.rs:665-725` can decode top-level `0xFD`/`0xFE`
  type-cache markers when a caller passes a `TypeCache`.
- `src/pvdata/encode.rs:1499-1505` exposes `decode_pv_field()` without a
  cache parameter.
- `src/pvdata/encode.rs:1637-1655` decodes a `FieldDesc::Variant` value by
  calling `decode_type_desc(cur, order)`, which creates an empty cache at
  `encode.rs:636-649`.
- `src/pvdata/encode.rs:1662-1678` repeats the same empty-cache path for each
  `VariantArray` element.
- `src/client_native/decode.rs:433-436` decodes an RPC response descriptor
  with the connection cache, then decodes its value without that cache.
- `src/client_native/decode.rs:460-463` decodes GET/MONITOR values through
  `decode_pv_field_with_bitset()`, whose Variant leaves also call the
  cache-less value decoder.
- `src/client_native/decode.rs:503-620` flattens only the descriptor region
  at the start of known op-response frames; it copies the value tail
  verbatim, so Variant-embedded descriptors remain unresolved.

pvxs reference:

- `src/dataencode.cpp:69-118` decodes `0xFD`/`0xFE` against a `TypeStore`.
- `src/dataencode.cpp:451-557` threads that same `TypeStore` through value
  decoding, including `TypeCode::Any`.
- `src/dataencode.cpp:656-674` does the same for `AnyA`.
- `src/dataencode.cpp:692-752` keeps passing the same `TypeStore` through
  `from_wire_full()` and `from_wire_type_value()`.

Impact: a peer may legally send a Variant payload whose carried descriptor is
`0xFE <slot>` referencing a descriptor already defined on the virtual circuit.
pvxs accepts this; Rust can reject it with a type-cache miss because nested
Variant descriptors are decoded with a fresh empty cache. Existing type-cache
tests cover top-level descriptors, but not cached descriptors inside Variant
value bodies.

Fix direction: add cache-aware `decode_pv_field_cached()` and
`decode_pv_field_with_bitset_cached()` variants, route GET/MONITOR/RPC data
decoding through them, and make the reader-side flattening either recurse into
Variant value bodies or stop relying on flattening for per-op decoding.

### PVA-R4: TCP name servers are direct-connect fallbacks, not pvxs name-server search

Severity: Medium

Rust:

- `src/client_native/context.rs:110-121` documents `name_servers()` as a
  fallback-only treatment.
- `src/client_native/channel.rs:724-741` appends configured name-server
  addresses as final direct-connect candidates after UDP search returns no
  responder.

pvxs reference:

- `src/pvxs/client.h:1024-1027` defines TCP name servers as maintained
  connections that receive search requests.
- `src/client.cpp:598-607` constructs the name-server list during context
  setup.
- `src/client.cpp:651-666` starts persistent TCP connections to each name
  server.
- `src/client.cpp:1193-1213` writes SEARCH frames to every ready name-server
  connection.
- `src/client.cpp:984-989` handles SEARCH_RESPONSE frames arriving on those
  TCP connections through the normal search-reply path.

Impact: Rust supports only the gateway-self-serve case where the configured
name server also hosts the target PV. It does not support pvxs-style
redirects where the TCP name server answers SEARCH with a different server
address. This is documented in code, but it is still a functional parity gap
for deployments that use PVA name servers as redirectors.

Fix direction: move name servers into `SearchEngine` as persistent TCP search
peers, send normal SEARCH frames over those connections, and feed their
SEARCH_RESPONSE bodies through the same cache/multi-server logic used for UDP
responses.

### PVA-R5: `SharedPV` does not enforce the opened value descriptor

Severity: High

Rust:

- `src/server_native/shared_pv.rs:178-185` stores an opened descriptor and
  initial value.
- `src/server_native/shared_pv.rs:214-229` accepts any later `PvField` in
  `try_post()`, stores it as current, and queues it to subscribers without
  checking it against the opened descriptor.
- `src/server_native/shared_pv.rs:649-651` returns that current value to GET
  callers, while `shared_pv.rs:641-647` continues to return the separately
  stored descriptor.
- `src/server_native/tcp.rs:3012-3045` encodes monitor data using the INIT
  descriptor captured earlier.
- `src/pvdata/encode.rs:998-1048` intentionally emits default/coerced data
  when the supplied value does not fit the descriptor, so a descriptor/value
  mismatch becomes silent data substitution rather than an error.

pvxs reference:

- `src/sharedpv.cpp:417-431` rejects `post()` before open, empty values, and
  descriptor changes.
- `src/servermon.cpp:251-258` also rejects type changes before queueing a
  monitor update.

Impact: a Rust `SharedPV` can advertise one descriptor and store/post a value
with another shape. GET and MONITOR responses can then encode coerced zeros,
empty arrays, or default structures under the old descriptor instead of
failing at the producer boundary. pvxs treats this as a producer bug and stops
it at `post()`.

Fix direction: make `SharedPV::try_post()` and the no-handler `put_delta()`
store path validate the value against the opened descriptor before mutating
`current` or notifying subscribers. Return an error for public write paths
where possible; for the current `try_post() -> usize` API, introduce a
checked method and route internal callers through it.

### PVA-R6: `SharedPV` drops the newest update when a subscriber queue is full

Severity: Medium

Rust:

- `src/server_native/shared_pv.rs:211-229` calls `try_send()` for every
  subscriber and silently keeps a full subscriber while dropping the new
  update.
- `src/server_native/shared_pv.rs:310-370` repeats the same `try_send()`
  full-queue drop on the `put_delta()` no-handler path.
- `src/server_native/shared_pv.rs:704-709` creates subscription channels with
  a fixed depth of 64.
- `src/server_native/tcp.rs:2690-2739` squashes only after the per-channel
  monitor task receives values from that channel. If the channel is already
  full because the TCP task is blocked by downstream backpressure or a
  pipeline window, the newest value never reaches the squashing loop.

pvxs reference:

- `src/servermon.cpp:271-286` appends while the monitor queue is under its
  limit; once full, normal posts squash into `queue.back()` so the newest
  value replaces the queued tail.
- `src/sharedpv.cpp:431-440` updates current and posts to every subscriber
  through the monitor control object, which applies that queue/squash policy.

Impact: a slow Rust subscriber can miss the final posted value if production
stops while its mpsc channel is full. pvxs preserves the latest value in the
queue tail for non-`maybe` posts. This matters for mailbox-style PVs where
clients expect to converge to the latest posted state after congestion clears.

Fix direction: give each subscriber an explicit mailbox/queue object with
pvxs semantics: bounded queue, full-queue squash-to-tail for normal posts,
optional drop for maybe posts, and watermark callbacks tied to that queue
instead of `tokio::mpsc::try_send()` full errors.

### PVA-R7: Client post-handshake reader swallows protocol parse errors

Severity: High

Rust:

- `src/client_native/server_conn.rs:320-365` appends bytes to the client
  connection buffer, peeks a header only when `PvaHeader::decode()` succeeds,
  then drains frames only inside `while let Ok(Some(...)) =
  try_parse_frame_role(...)`.
- `src/client_native/decode.rs:68-105` returns `Err` for invalid headers and
  direction-bit mismatches, exactly the cases the client reader comments say
  should close the connection.
- The handshake path is stricter: `server_conn.rs:733-766` propagates
  `try_parse_frame_role(...)?`. The server TCP path is also stricter:
  `src/server_native/tcp.rs:1643-1680` propagates the same error.

pvxs reference:

- `src/conn.cpp:153-165` disconnects immediately when the header magic is not
  `0xca`, the version is zero, or the direction bit does not match the
  connection role.
- `src/pvaproto.h:684-699` also treats bad magic/version as decode faults.

Impact: after handshake, a malformed server frame does not close the Rust
client connection. If the peer sends a bad 8-byte prefix and stalls, the reader
waits with an invalid prefix pinned in `buf`; if it keeps sending, `buf` can
grow because parse errors are treated like "no complete frame yet." The
documented direction-bit defense is therefore not enforced on this live client
read path.

Fix direction: split the frame parse result. `Ok(None)` should keep buffering;
`Ok(Some(...))` should drain and dispatch; `Err(e)` should log, cancel the
connection, and wake pending operations with an error. Apply the same decision
before the max-payload check so an undecodable header cannot bypass the cap.

### PVA-R8: Server advertises auth methods in the wrong order for old clients

Severity: Medium

Rust:

- `src/server_native/tcp.rs:839-848` says pvxs advertises methods in
  reverse-priority order, but `ADVERTISED_AUTH_METHODS` is
  `["ca", "anonymous"]`.
- `src/server_native/tcp.rs:1693-1705` writes the methods in slice order.

pvxs reference:

- `src/serverconn.cpp:108-114` writes `anonymous` and then `ca`, with a comment
  explaining that older pvAccess clients took the last known plugin.
- `src/clientconn.cpp:228-245` still documents that reverse-priority rule; a
  modern pvxs client explicitly keeps `ca` when present, but the compatibility
  reason is still the old "last known plugin" client behavior.

Impact: modern pvxs clients still choose `ca`, but an older client that
implements the historical "last known plugin wins" behavior will choose
`anonymous` from the Rust server's advertised list. That loses user/host
credentials and can change ACF decisions even though the Rust comment claims
pvxs-compatible ordering.

Fix direction: change the advertised order to `["anonymous", "ca"]` while
leaving validation acceptance unchanged.

### PVA-R9: Generic server responses can silently encode mismatched values

Severity: High

Rust:

- `src/server_native/source.rs:81-88` lets a `ChannelSource` return a descriptor
  from `get_introspection()` and an independent `PvField` from `get_value()`.
- `src/server_native/tcp.rs:1813-1822` captures the descriptor when the channel
  is created, and GET/MONITOR operations retain a copy in `OpState`.
- `src/server_native/tcp.rs:2179-2210` encodes GET data using the retained
  descriptor and the current value without verifying that they match.
- `src/server_native/tcp.rs:1459-1488` does the same for PUT-with-getback.
- `src/server_native/tcp.rs:3012-3045` does the same for MONITOR data.
- `src/pvdata/encode.rs:998-1048` deliberately coerces or defaults mismatched
  descriptor/value pairs so the wire stream stays well-formed.

pvxs reference:

- `src/serverget.cpp:62-67` throws if a GET or PUT-with-getback reply omits a
  value or replies with a value whose descriptor is not the exact descriptor
  passed to `connect()`.
- `src/servermon.cpp:251-258` throws if a monitor post changes descriptor.
- `src/servermon.cpp:397-404` requires a non-empty monitor prototype before the
  subscription is accepted.

Impact: this is the generic form of PVA-R5. Any custom `ChannelSource`, not only
`SharedPV`, can advertise one descriptor and later return a differently shaped
value. The server then emits default/coerced data under the old descriptor
instead of rejecting the source bug at the producer/wire boundary. That can turn
application data corruption into a valid-looking PVA response.

Fix direction: add a single descriptor/value compatibility gate in the server
wire layer before GET, PUT-with-getback, and MONITOR encoding. Treat mismatch
as a source error, report it to the client where the protocol has a status
field, and tear down or fail the monitor operation for monitor posts. Keep
`encode_pv_field*` descriptor-driven for decoder safety, but do not use it as a
producer-side validator.

## Verification

- pass: `cargo fmt --all -- --check`
- pass: `cargo clippy -p epics-pva-rs --all-targets -- -D warnings`
- pass: `cargo nextest run -p epics-pva-rs`
  - nextest run ID: `a9387b7d-c5c4-447e-bd69-15147cb7f2bf`
  - scope: `-p epics-pva-rs`
  - result: 520 tests passed, 17 skipped

These commands verify the crate's current test suite after the review doc
change. They do not prove the nine findings above are false; the findings are
recorded because the current suite lacks the specific parity regressions noted
in each section.
