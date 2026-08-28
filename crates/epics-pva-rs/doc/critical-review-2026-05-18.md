# epics-pva-rs Critical Review - 2026-05-18

Scope: `crates/epics-pva-rs`.

This document is a running critical review of the PVA crate. Findings
below are limited to issues with a concrete code path and file/line
evidence. Product code was not changed during this review.

Reference implementation: `$PVXS_HOME` at git `f8d6192` (working tree
had an untracked `.DS_Store`, ignored for this review).

The `src/decode.rs` rows below were written as `src/client_native/decode.rs`;
`24d514e8` (2026-07-21) moved that file to the crate root, keeping
`client_native::decode` as a re-export. Only the path is rewritten — the line
numbers still belong to this review's own revision, not to today's file.

## Method

- Reviewed protocol framing, pvData encoding/decoding, client discovery
  and operations, server TCP/UDP paths, auth/TLS surfaces, CLIs, and tests.
- Preferred defect candidates that are externally observable:
  interoperability failures, incorrect wire semantics, hangs/resource
  leaks, panics on peer-controlled input, or test gaps hiding such issues.
- Kept speculative design preferences out of the findings list.

## Recording note (2026-08-26)

Every row below now carries a `Status:` line naming the commit that ended
it. None of that is new work. Each verdict was already written down — in
the implementation-status section at the end of this file, and on the
driver punchlist `punchlist-2026-05-19.md` — but in a shape no per-row
scan can attribute: a ledger bullet leading with a row id starts its own
block, so the verdict standing in the section heading above it belongs to
no row at all. The `Status:` lines move each verdict inside the row it is
about. Two of them cite a commit whose subject carries the wrong id
(`BR-R4` and `BR-R14`), which is why a subject search finds no fix for
those two rows.

## Open Findings

### PVA-R1: Monitor pipeline negotiation is sent on the wrong wire phase

Severity: High

Status: **CLEARED** `5a690343` — `pv_request::build_pv_request_pipeline` puts `record._options.pipeline` into the MONITOR INIT pvRequest, and `codec::build_monitor_start` no longer carries the 4-byte trailer.

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

Status: **CLEARED** `479f77c0` — `tcp_timeout` is threaded through `ConnectionPool::get_or_connect` into the heartbeat task; regression `pva_r2_tcp_timeout_applied` in `src/client_native/server_conn.rs`.

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

Status: **CLEARED** `cf5a0e5d` — `decode_pv_field_cached` threads the connection-scope `TypeCache` through nested Variant decode; regression `pva_r3_nested_variant_uses_typecache`.

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
- `src/decode.rs:433-436` decodes an RPC response descriptor
  with the connection cache, then decodes its value without that cache.
- `src/decode.rs:460-463` decodes GET/MONITOR values through
  `decode_pv_field_with_bitset()`, whose Variant leaves also call the
  cache-less value decoder.
- `src/decode.rs:503-620` flattens only the descriptor region
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

Status: **CLEARED** `7dfe5de6` — `SearchEngine::spawn` keeps a persistent `ns_task` per TCP name server instead of a direct-connect fallback; regression `pva_r4_tcp_nameserver_persistent_peer`. That commit's subject reads `BR-R4`, which is why a subject search for this id finds nothing.

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
- `src/client.cpp:1221-1236` writes SEARCH frames to every ready name-server
  connection.
- `src/client.cpp:1007-1018` handles SEARCH_RESPONSE frames arriving on those
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

Status: **CLEARED** `88232dea` — `SharedPV::try_post_checked` and the no-handler put-delta path both check the posted value against the opened descriptor.

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

Status: **CLEARED** `c4bb773a` — `MonitorOutbox` / `MonitorInbox` give the subscriber queue squash-to-tail semantics; regression `pva_r6_squash_to_tail`.

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

Status: **CLEARED** `46cbb997` — the post-handshake reader splits `Ok(None)` / `Ok(Some)` / `Err` and closes the circuit on a frame-parse fault.

Rust:

- `src/client_native/server_conn.rs:320-365` appends bytes to the client
  connection buffer, peeks a header only when `PvaHeader::decode()` succeeds,
  then drains frames only inside `while let Ok(Some(...)) =
  try_parse_frame_role(...)`.
- `src/decode.rs:68-105` returns `Err` for invalid headers and
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

Status: **CLEARED** `e4056db8` — `ADVERTISED_AUTH_METHODS` is `["anonymous", "ca"]`.

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

Status: **CLEARED** `88232dea` — the server GET data phase calls `value_matches_descriptor` before it encodes.

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

### PVA-R10: Discovery transport protocol is parsed but not enforced

Severity: Medium

Status: **CLEARED** `ec131970` — the client rejects a SEARCH_RESPONSE whose advertised protocol is not `tcp`, and the broadcast SEARCH path gates on the requested protocol list.

Rust:

- `src/decode.rs:145-147` parses the SEARCH_RESPONSE protocol
  string, but `src/client_native/search_engine.rs:1442-1488` ignores
  `resp.protocol` and resolves only a `SocketAddr`.
- The one-shot search helper has the same shape:
  `src/client_native/search.rs:183-187` returns the server address when the CID
  matches, without checking the advertised protocol.
- `src/server_native/udp.rs:1055-1058` parses and discards the client's protocol
  list. `udp.rs:682-707` then answers based only on PV name matches or
  `MustReply`.
- TLS runtime setup chooses a protocol string at
  `src/server_native/runtime.rs:530`, and SEARCH_RESPONSE uses it through
  `udp.rs:700-707`, but beacons still hard-code `"tcp"` at `udp.rs:947-964`.

pvxs reference:

- `src/udp_collector.cpp:408-421` records whether the incoming SEARCH included
  protocol `"tcp"`.
- `src/udp_collector.cpp:424-443` only queues channel names for matching if
  `"tcp"` was requested.
- `src/client.cpp:849-880` decodes the response protocol and ignores normal
  search replies unless `proto == "tcp"`.
- `src/server.cpp:738-745` and `src/server.cpp:773-781` consistently advertise
  `"tcp"` in both SEARCH_RESPONSE and beacon frames.

Impact: Rust can answer SEARCH requests from clients that did not ask for the
transport it advertises, and the Rust client can try to dial a response whose
transport protocol it does not support for that connection. In TLS mode the
server also sends inconsistent discovery metadata: SEARCH_RESPONSE says `"tls"`
while beacons say `"tcp"`. That makes discovery behavior depend on which packet
the client sees first.

Fix direction: carry the protocol through resolution instead of collapsing
SEARCH_RESPONSE to `SocketAddr`. Drop unsupported protocols, make the server
answer only when the request includes the advertised protocol, and pass the
runtime protocol into beacon construction.

### PVA-R11: Server TCP SEARCH frames are silently dropped

Severity: Medium

Status: **CLEARED** `cc66c95f` — `server_native::tcp::handle_tcp_search` answers SEARCH arriving on an established circuit.

Rust:

- `src/server_native/tcp.rs:1075-1269` dispatches TCP application commands but
  has no `Command::Search` arm; the final `_` arm silently keeps going.

pvxs reference:

- `src/serverchan.cpp:173-255` implements `ServerConn::handle_SEARCH()` on an
  established TCP connection and enqueues a `CMD_SEARCH_RESPONSE`.
- `src/client.cpp:1221-1236` sends SEARCH frames to ready TCP name-server
  connections, and `src/client.cpp:1007-1018` handles SEARCH_RESPONSE frames from
  those connections.

Impact: this is the server-side half of the TCP name-server parity gap. A pvxs
client configured to use a Rust server or gateway as a TCP search peer can open
the virtual circuit, but SEARCH frames on that circuit are ignored. Redirecting
name-server deployments therefore fail even if the underlying PVs exist.

Fix direction: add a TCP `Command::Search` handler sharing the UDP request
parser's name/protocol filtering and the SEARCH_RESPONSE builder, with TCP
semantics for reply address and port matching pvxs `serverchan.cpp`.

### PVA-R12: Warm GET failure drops the cleanup owner for the cached operation

Severity: Medium

Status: **CLEARED** `add08dd2` — a failed warm GET sends DESTROY_REQUEST and unregisters the IOID before the cold fallback runs.

Rust:

- `src/client_native/ops_v2.rs:47-129` defines `IoidGuard`, which unregisters
  the client router entry and sends a best-effort DESTROY_REQUEST unless it is
  defused.
- `src/client_native/ops_v2.rs:286-300` defuses that guard after a successful
  default GET so the operation can be reused as a warm GET.
- `src/client_native/channel.rs:131-138` stores the warm GET state but gives
  `CachedGet` no cleanup behavior.
- `src/client_native/ops_v2.rs:201-217` removes the warm cache, calls
  `try_warm_get()`, and on `Err(_)` falls through to the cold INIT path without
  destroying or unregistering the old `(sid, ioid)`.
- `src/client_native/ops_v2.rs:337-360` can return an error on send failure,
  timeout, or response routing failure after refilling the reusable slot.

pvxs reference:

- `src/clientget.cpp:188-200` sends `DestroyRequest` and erases both connection
  and channel IOID maps when a GET/PUT/RPC operation is cancelled before `Done`.
- `src/clientget.cpp:313-325` treats `Done` as an explicit
  `CMD_DESTROY_REQUEST` send and then forgets the IOID because the destroy is
  not acknowledged.

Impact: a lost warm-GET reply or send/routing error abandons the reusable server
operation while the next cold GET allocates a new IOID. Repeated warm failures
on a live TCP circuit can accumulate server-side operation slots until the
server's per-channel operation cap rejects new operations. The leak is masked on
connection close because both sides then reap all IOIDs.

Fix direction: make warm GET state have a single cleanup owner. On warm failure,
either restore the cache only when the operation is still known reusable, or send
DESTROY_REQUEST plus `unregister_ioid()` before falling back to a cold INIT.

### PVA-R13: Compound array elements use the wrong null/presence wire shape

Severity: High

Status: **CLEARED** `46cbb997` — structure, union and variant array elements carry a per-element presence byte on both encode and decode.

Rust:

- `src/pvdata/encode.rs:903-914` writes a presence byte for every
  `StructureArray` element, but always writes `0x01`.
- `src/pvdata/encode.rs:1546-1578` decodes a `StructureArray` element with
  presence `0x00` as an empty `PvStructure`, so null elements cannot round-trip.
- `src/pvdata/encode.rs:937-953` encodes `UnionArray` elements as selector
  directly, with `0xFF` used for null.
- `src/pvdata/encode.rs:1606-1635` decodes `UnionArray` elements as selector
  directly, with no per-element presence byte.
- `src/pvdata/encode.rs:962-972` encodes `VariantArray` elements as descriptor
  directly, with `0xFF` used for null.
- `src/pvdata/encode.rs:1662-1684` decodes `VariantArray` elements as descriptor
  directly, with no per-element presence byte.

pvxs reference:

- `src/dataencode.cpp:354-365` encodes `StructA` as array length followed by
  `0x00` for null elements or `0x01 + value` for present elements.
- `src/dataencode.cpp:607-619` decodes `StructA` the same way and leaves the
  element null when the presence byte is zero.
- `src/dataencode.cpp:368-378` encodes `UnionA` as `0x00` for null or
  `0x01 + union value` for present.
- `src/dataencode.cpp:624-650` decodes `UnionA` by reading that presence byte
  before selector/value.
- `src/dataencode.cpp:382-393` encodes `AnyA` as `0x00` for null or
  `0x01 + descriptor + value` for present.
- `src/dataencode.cpp:656-674` decodes `AnyA` with the same presence byte.

Impact: Rust and pvxs disagree on the wire layout for arrays of unions and
variants. A Rust `VariantArray` whose first non-null element is an `int` starts
with the descriptor tag; pvxs consumes that byte as the presence flag and then
tries to decode the following byte as a descriptor, corrupting the stream.
Conversely, Rust reads pvxs's presence byte as a selector or descriptor tag.
`StructureArray` is partially shaped correctly, but null elements become empty
structures and are re-emitted as present default structures.

Fix direction: model compound-array nullability explicitly. Encode/decode
`StructureArray`, `UnionArray`, and `VariantArray` with a leading per-element
presence byte. For structures, either represent `Vec<Option<PvStructure>>` or
reject null elements explicitly instead of converting them to present defaults.

### PVA-R14: Server source calls run inside the per-connection read loop

Severity: Medium

Status: **CLEARED** `601a568f` — every data-phase source call is spawned off the read loop with `OpState::data_task_abort` as its cancel owner; regression `pva_r14_source_calls_no_head_of_line_block`. That commit's subject reads `BR-R14`, which is why a subject search for this id finds nothing.

Rust:

- `src/server_native/tcp.rs:903-1217` reads one frame and awaits the full
  command handler before reading the next frame from the same TCP connection.
- `src/server_native/tcp.rs:1778-1814` awaits `has_pv()` and
  `get_introspection()` during CREATE_CHANNEL handling.
- `src/server_native/tcp.rs:2179-2215` awaits `get_value_checked()` in the GET
  data phase before the read loop can process later frames.
- `src/server_native/tcp.rs:2272-2352` awaits PUT and optional readback source
  calls in the same inline path.
- `src/server_native/tcp.rs:1619-1634` awaits `process_checked()` inline.
- `src/server_native/tcp.rs:2970-2984` awaits `get_introspection()` inline for
  GET_FIELD fallback.

pvxs reference:

- `src/serverget.cpp:366-417` parses GET/PUT/RPC INIT, registers an operation,
  and hands a control object to the source without waiting for the eventual
  reply in the socket read path.
- `src/serverget.cpp:473-509` does the same for EXEC: it marks the operation
  executing and invokes `onGet`/`onPut`/`onRPC` with an `ExecOp`.
- `src/serverget.cpp:285-294` sends the eventual reply later by dispatching
  back to the acceptor loop.
- `src/serverintrospect.cpp:166-183` follows the same operation/control-object
  pattern for GET_FIELD.

Impact: one slow or wedged Rust `ChannelSource` method can head-of-line block
every later frame on that TCP connection, including CANCEL_REQUEST,
DESTROY_REQUEST, ECHO responses, and operations for other channels multiplexed
on the same virtual circuit. pvxs keeps the protocol reader responsive by
decoupling source completion from socket parsing; Rust's async `.await` yields
the Tokio worker, but it still serializes that connection's protocol state
behind the source future.

Fix direction: split per-connection frame parsing from operation execution.
Register op state synchronously, spawn or dispatch source work behind an
operation owner, and route completions back through the writer channel. Keep
CANCEL/DESTROY able to mark or abort that owner without waiting for the source
future to return.

### PVA-R15: PVA/PVAS environment alias parity is incomplete

Severity: Medium

Status: **CLEARED** `b0f37eef` — `config::env::pvas_server_port` plus the auto-beacon and beacon-address-list fallbacks mirror the pvxs PickOne precedence.

Rust:

- `src/config/env.rs:157-163` reads `EPICS_PVA_SERVER_PORT` for the PVA TCP
  port, but does not read `EPICS_PVAS_SERVER_PORT`.
- `src/config/env.rs:314-316` uses that same helper as the default TCP port
  for `EPICS_PVA_NAME_SERVERS`.
- `src/config/env.rs:174-182` reads only
  `EPICS_PVAS_AUTO_BEACON_ADDR_LIST`, without the pvxs fallback to
  `EPICS_PVA_AUTO_ADDR_LIST`.
- `src/config/env.rs:408-412` reads only `EPICS_PVAS_BEACON_ADDR_LIST`,
  without the pvxs fallback to `EPICS_PVA_ADDR_LIST`.
- `src/server_native/runtime.rs:256-272` applies those helpers directly to
  `PvaServerConfig`.

pvxs reference:

- `src/config.cpp:402-408` accepts `EPICS_PVAS_SERVER_PORT` first, then
  `EPICS_PVA_SERVER_PORT`.
- `src/config.cpp:426-432` accepts `EPICS_PVAS_BEACON_ADDR_LIST` before
  `EPICS_PVA_ADDR_LIST`, and `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` before
  `EPICS_PVA_AUTO_ADDR_LIST`.
- `src/config.cpp:476-479` exports both server-specific and shared PVA names
  for the same server settings.
- `src/config.cpp:568-578` also lets the client TCP port come from
  `EPICS_PVAS_SERVER_PORT` when `EPICS_PVA_SERVER_PORT` is not set.

Impact: sites configured for pvxs can set the documented
`EPICS_PVAS_SERVER_PORT` and still get the Rust server bound to 5075. Shared
deployment config using `EPICS_PVA_ADDR_LIST` or `EPICS_PVA_AUTO_ADDR_LIST` for
server beacon targets is also ignored. That changes bind and beacon behavior
without an error, so clients search one port/address set while the Rust server
advertises or listens on another. Client-side name-server defaults can also
miss a site-wide `EPICS_PVAS_SERVER_PORT` value that pvxs would honor.

Fix direction: make the server helpers mirror pvxs `PickOne` precedence:
server-specific `EPICS_PVAS_*` first, compatible `EPICS_PVA_*` fallback second.
Add isolated env tests for all alias pairs, including precedence when both are
set.

### PVA-R16: Missing non-RPC prototypes are converted into successful Variant operations

Severity: High

Status: **CLEARED** `bed630f9` — a non-RPC INIT that carries no prototype no longer answers as a successful Variant operation.

Rust:

- `src/server_native/tcp.rs:1813-1822` accepts a channel whose source returns
  no introspection descriptor.
- `src/server_native/tcp.rs:1356-1382` does the same in the Rust-only PUT_GET
  handler.
- `src/server_native/tcp.rs:1563-1580` does the same in the Rust-only PROCESS
  handler, even though PROCESS has no value payload and still stores the
  fallback descriptor in op state.
- `src/server_native/tcp.rs:2028-2133` turns that missing descriptor into
  `FieldDesc::Variant` and sends a successful GET/PUT/MONITOR INIT response.
- `src/server_native/tcp.rs:2968-2979` does the same for GET_FIELD: if the
  channel has no descriptor and the source still returns none, it replies OK
  with a Variant descriptor.

pvxs reference:

- `src/serverget.cpp:182-193` rejects missing prototypes for non-RPC
  operations with `Must provide prototype`.
- `src/serverintrospect.cpp:74-80` rejects GET_FIELD replies with a null
  prototype descriptor.

Impact: a broken or incomplete Rust `ChannelSource` can advertise that a PV
exists, omit its descriptor, and still get successful protocol replies. Clients
then build a Variant decode path for a PV whose real value may be scalar or
structured. This masks source bugs, makes later mismatched-value encoding look
valid, and diverges from pvxs' explicit "prototype required" contract for
non-RPC operations.

Fix direction: make descriptor absence an operation creation error for GET,
PUT, MONITOR, and GET_FIELD. RPC can keep its descriptor-late behavior. The
error should be surfaced through the command's status field where available.
If the Rust-only PUT_GET and PROCESS handlers are kept, route them through the
same descriptor-present gate so source implementations that cannot provide
introspection do not create successful non-RPC operations.

### PVA-R17: Raw monitor FINISH leaves the handle pointing at a stale operation

Severity: Medium

Status: **CLEARED** `b0f37eef` — FINISH releases the subscription through `MonitorTeardown::release`, which clears the handle's `active` tuple.

Rust:

- `src/client_native/ops_v2.rs:1194-1198` records the active
  `(ServerConn, sid, ioid)` for raw monitor handles.
- `src/client_native/ops_v2.rs:1209-1217` clears that active tuple when the
  stream ends because the connection disappears.
- `src/client_native/ops_v2.rs:1230-1246` handles a monitor FINISH by
  unregistering the IOID and returning, but does not clear the handle's active
  tuple.
- `src/client_native/ops_v2.rs:916-933` later uses the active tuple on handle
  drop to send DESTROY_REQUEST and unregister the IOID.

pvxs reference:

- `src/clientmon.cpp:720-729` makes FINISH the operation owner cleanup path:
  state becomes Done, IOID maps are erased, pipeline ack timers are removed,
  and no destroy is sent for a server-finalized monitor.

Impact: after a raw monitor receives FINISH, `SubscriptionHandle::pause()`,
`resume()`, or `drop()` can act on an IOID that the client already unregistered
and the server already finished. The extra frames are stale traffic at best;
with IOID reuse or a reconnect race they can target the wrong operation owner.
The typed monitor path clears `active` on status FINISH, so the bug is confined
to the raw monitor path.

Fix direction: in the raw monitor FINISH branch, clear `state.active` before
returning for both success and fatal-status exits. Keep FINISH as a server-owned
cleanup, matching pvxs, so Drop does not send a second DESTROY_REQUEST for the
same completed operation.

### PVA-R18: Client drops all-zero wildcard addresses in discovery replies

Severity: Medium

Status: **CLEARED** `ec131970` — `proto::ip::ip_from_bytes_allow_unspec` routes an all-zero reply address through the wildcard substitution path.

Rust:

- `src/proto/ip.rs:24-30` treats an all-zero 16-byte PVA address as
  `None`.
- `src/decode.rs:167-169` turns that `None` into a protocol
  error while decoding SEARCH_RESPONSE.
- `src/client_native/search_engine.rs:1454-1456` drops such SEARCH_RESPONSE
  frames after consuming them.
- `src/client_native/search_engine.rs:1544-1546` drops BEACON frames with the
  same all-zero address.
- `src/codec.rs:122-127` intentionally emits raw all-zero `::` as the
  discover SEARCH reply address, so the client is asymmetric: it sends a valid
  wildcard shape it will not accept in responses.

pvxs reference:

- `src/evhelper.cpp:911-938` decodes a non-IPv4-mapped all-zero address as an
  IPv6 `SockAddr`.
- `src/util.cpp:552-558` classifies IPv6 unspecified as `isAny()`.
- `src/client.cpp:841-843` substitutes the UDP packet source address when a
  SEARCH_RESPONSE server address is any.
- `src/udp_collector.cpp:471-476` applies the same substitution for BEACON.

Impact: an IPv6-capable server or relay that advertises wildcard using the
raw-zero IPv6-any encoding will be accepted by pvxs but ignored by the Rust
client. Search responses are consumed without resolving pending PVs, and
beacons fail to enter the tracker or trigger reconnect searches. This is most
visible on IPv6 or mixed-stack deployments where the sender relies on the
standard "use datagram source when advertised address is any" rule.

Fix direction: do not represent all-zero PVA addresses as decode failure.
Carry an explicit wildcard/unspecified address through the discovery decoder,
then apply the same peer-source substitution that already exists for
`0.0.0.0` and loopback addresses.

### PVA-R19: Invalid pvRequest masks fall back to all fields

Severity: High

Status: **CLEARED** `bed630f9` — a pvRequest mask that selects no existing field returns `RequestMaskError::EmptyMask`, which becomes an INIT-status error.

Rust:

- `src/pv_request.rs:147-149` correctly returns `Err(EmptyMask)` when a
  pvRequest selects no field that exists in the value descriptor.
- `src/server_native/tcp.rs:2028-2037` discards both pvRequest decode errors
  and `request_to_mask()` errors for GET, PUT, MONITOR, and RPC INIT handling
  and falls back to `BitSet::all_set(...)`.
- `src/server_native/tcp.rs:1357-1366` has the same fallback in the Rust-only
  PUT_GET handler.

pvxs reference:

- `src/serverget.cpp:367-375` treats an invalid pvRequest type/value decode as
  a bad INIT and closes the connection.
- `src/servermon.cpp:491-502` does the same for MONITOR INIT.
- `src/pvrequest.cpp:61-62` throws when the pvRequest selects no field.
- `src/serverget.cpp:198-200` and `src/servermon.cpp:401-402` compute the
  field mask through that throwing `request2mask()` path after the source
  supplies the prototype.

Impact: `field(noSuch)` or a malformed pvRequest body becomes a full-field
GET/MONITOR/PUT_GET response in Rust instead of a protocol/source error. That
can leak fields the client did not successfully request, inflate monitor
traffic, and hide client-side pvRequest bugs that pvxs surfaces immediately.
For PUT paths, a malformed INIT also negotiates an operation under a mask the
client did not actually describe.

Fix direction: keep "no field sub-structure" and `field()` as the all-fields
cases, but treat decode failures and `RequestMaskError::EmptyMask` as operation
creation errors. Reply with INIT status error when the command has an INIT
status field; otherwise close/drop consistently with pvxs.

### PVA-R20: Server monitor pipeline parser ignores typed pvxs options

Severity: Medium

Status: **CLEARED** `5a690343` — the pipeline option parser accepts the typed pvxs shapes; `cc66c95f` added the unit and interop coverage.

Rust:

- `src/server_native/tcp.rs:245-251` enables pipeline only when
  `record._options.pipeline` is a string equal to `"true"` or `"1"`.
- `src/server_native/tcp.rs:252-267` silently defaults an unparseable
  `queueSize` to 4 and clamps parsed values to at least 1.
- `src/server_native/tcp.rs:2061-2068` enables the server-side pipeline window
  only after that parser returns an enabled `PipelineOptions`.

pvxs reference:

- `src/clientreq.cpp:85-90` stores `RequestBuilder::record()` values with their
  concrete pvData type.
- `test/testpvreq.cpp:175-192` demonstrates `.record("pipeline", true)`
  producing a boolean option, while `test/testpvreq.cpp:237-253` demonstrates
  the string form from parsed `record[pipeline=true]`.
- `src/servermon.cpp:523-530` parses `pipeline` via `Value::as(bool)`, not only
  a string comparison.
- `src/servermon.cpp:533-540` rejects invalid or `<2` `queueSize` when
  pipeline is enabled.

Impact: a pvxs client using the typed builder form
`.record("pipeline", true).record("queueSize", N)` sends a valid pvRequest
that Rust decodes but treats as non-pipelined because the option is boolean,
not string. Conversely, `record[pipeline=true,queueSize=1]` or an unparseable
queue size is accepted by Rust, while pvxs rejects it for the pipeline
sub-protocol. That changes flow-control semantics and can make client and
server disagree about whether ACK/window behavior is active.

Fix direction: parse `pipeline` through the same scalar conversion rules used
by pvxs: bool, integer, and recognized strings. Preserve the pvxs queue rule:
when pipeline is true, `queueSize` must parse and must be at least 2, otherwise
the monitor INIT should return an error.

### PVA-R21: Duplicate operation INIT replaces active IOID state

Severity: High

Status: **CLEARED** `bed630f9` — a duplicate INIT on a live IOID is a protocol fault rather than a state replacement.

Rust:

- `src/server_native/tcp.rs:2013-2024` explicitly allows an INIT for an IOID
  that is already present in the channel's `ops` map.
- `src/server_native/tcp.rs:2100-2114` then inserts the new `OpState`, replacing
  the existing operation.
- `src/server_native/tcp.rs:1343-1371` applies the same "existing IOID may
  proceed" rule in the PUT_GET handler.
- `src/server_native/tcp.rs:1552-1572` applies the same rule in the PROCESS
  handler.

pvxs reference:

- `src/serverget.cpp:378-384` treats INIT with an existing IOID as a protocol
  error and resets the connection.
- `src/servermon.cpp:505-511` applies the same rule for MONITOR INIT.
- `src/serverintrospect.cpp:153-160` also drops GET_FIELD when the IOID is
  already active.

Impact: a peer can re-INIT a live GET/PUT/MONITOR/RPC IOID and make Rust drop
or replace the operation owner under the same wire identity. For MONITOR this
can abort the previous task through `OpState` drop; for other operations it
can redirect later data frames to a different descriptor/mask than the
original operation negotiated. pvxs treats this as connection-fatal protocol
misuse, so accepting it widens the state machine and hides client bugs.

Fix direction: reject duplicate INIT before decoding or storing replacement op
state. For pvxs parity, close the connection on duplicate INIT for GET, PUT,
MONITOR, and RPC; at minimum, do not overwrite the existing op. Apply the same
guard to Rust-only PUT_GET and PROCESS if those commands remain implemented.

### PVA-R22: Malformed client auth payload can still validate

Severity: High

Status: **CLEARED** `e4056db8` — `parse_client_credentials` returns `PvaResult`, so an auth-payload decode fault is connection-fatal.

Rust:

- `src/server_native/tcp.rs:650-663` says `parse_client_credentials()` returns
  `None` on truncation, but after it reads the selected auth method it no
  longer treats the auth Value as required.
- `src/server_native/tcp.rs:679-703` calls `decode_type_desc()` and
  `decode_pv_field()` inside `if let Ok(...)` blocks. If either decode fails,
  the function still returns `Some(ClientCredentials)`.
- `src/server_native/tcp.rs:705-708` fills an empty account with the selected
  method name. A truncated `method="ca"` handshake therefore becomes
  `method="ca", account="ca"` instead of a failed credential decode.
- `src/server_native/tcp.rs:1000-1049` accepts that parsed credential, checks
  only that the method was advertised, and sends `CONNECTION_VALIDATED` with
  OK status for advertised methods.

pvxs reference:

- `src/serverconn.cpp:204-208` always decodes both the selected method and the
  auth Value with `from_wire_type_value()`.
- `src/serverconn.cpp:211-214` resets the connection when the auth Value is
  truncated or invalid.
- `src/serverconn.cpp:221-234` updates `method/account` from a successfully
  decoded `ca` credential only; otherwise it falls back to anonymous.

Impact: a peer can select an advertised auth method and omit or corrupt the
credential Value while still completing the Rust server handshake. For `ca`,
the resulting account is the literal method name rather than the authenticated
user field. Any ACF rule that keys on method/account/host is then evaluating a
credential tuple pvxs would never create from that wire payload.

Fix direction: make the auth Value mandatory once the method string is present,
including the empty-method/anonymous case. Decode failure should abort the
connection or reply with `CONNECTION_VALIDATED` error before updating `cred`.
Only a fully decoded `ca` structure should populate user, host, groups, or
roles; otherwise keep the previous anonymous credential and reject the
handshake consistently with pvxs.

### PVA-R23: Client accepts operation responses under the wrong command

Severity: High

Status: **CLEARED** `bed630f9` — `ServerConn::ioid_to_cmd` gates every operation reply on the command the IOID was opened with.

Rust:

- `src/client_native/server_conn.rs:76-88` stores IOID routes as `TwoShot`,
  `Stream`, or `Reusable` sinks, without the command the operation was opened
  with.
- `src/client_native/server_conn.rs:852-875` routes any application frame whose
  payload starts with a registered IOID to that sink. It does not check whether
  the frame command is the expected GET, PUT, MONITOR, or RPC command.
- `src/decode.rs:341-353` accepts any of GET, PUT, MONITOR, or
  RPC as an op response and decodes according to the incoming frame command.
- `src/client_native/ops_v2.rs:250-278` shows the consequence for GET: the
  caller accepts `OpResponse::Init` and `OpResponse::Data` without verifying
  that the frames were actually `CMD_GET`.

pvxs reference:

- `src/clientget.cpp:463-470` compares the incoming command with the stored
  operation command and faults the connection if they differ.
- `src/clientget.cpp:481-492` also validates the subcommand against the stored
  operation state before delivering the response.
- `src/clientmon.cpp:570-579` applies the same command check for MONITOR.
- `src/clientmon.cpp:582-600` validates monitor subcommand/state before
  delivering monitor events.

Impact: a malformed or buggy server can satisfy a Rust GET with a MONITOR or
PUT-shaped frame if the IOID matches and the payload happens to decode into an
`OpResponse::Init`/`Data`. That can advance the wrong client state, cache an
introspection descriptor under the wrong operation type, or deliver data that
pvxs would reject by closing the virtual circuit. It also makes IOID reuse and
stale-frame races harder to detect because the router has no expected-command
owner.

Fix direction: store the expected `Command` and phase/state with every IOID
route. `route_frame()` or the per-operation await path should reject a command
mismatch before decoding the payload, cancel the connection on protocol misuse,
and keep pvxs' per-state subcommand validation for INIT, data, FINISH, and
PUT-getback phases.

### PVA-R24: Server data phase ignores the operation kind bound at INIT

Severity: High

Status: **CLEARED** `bed630f9` — a data-phase frame whose command does not match `OpState::kind` is a protocol fault.

Rust:

- `src/server_native/tcp.rs:294-309` stores an `OpKind` in each `OpState`.
- `src/server_native/tcp.rs:2100-2114` records that kind during INIT.
- `src/server_native/tcp.rs:2143-2151` retrieves only the descriptor and mask
  from the existing IOID for data-phase frames; it does not compare
  `OpState.kind` with the incoming command's `kind`.
- `src/server_native/tcp.rs:2354-2410` can therefore run MONITOR start/ack
  logic against any existing IOID, including one opened as GET, PUT, or RPC.

pvxs reference:

- `src/serverget.cpp:421-436` requires data-phase GET/PUT/RPC frames to find a
  `ServerGPR`; if the IOID belongs to another operation class or is still
  creating, the server resets the connection.
- `src/servermon.cpp:611-632` requires MONITOR data/control frames to find a
  `MonitorOp`; a different operation type is a protocol error and resets the
  connection.

Impact: a client can INIT one operation type and then send data/control frames
under another command using the same IOID. For example, a GET IOID can be used
to enter the Rust MONITOR branch and spawn a subscriber task, or a MONITOR IOID
can be used to trigger a GET response path. pvxs treats that as operation-type
confusion and closes the connection. Rust widens the protocol state machine and
can create operation owners that were never negotiated for that command.

Fix direction: make `OpState.kind` the single gate for every non-INIT frame.
If the incoming command kind does not match the stored kind, close the
connection or return a fatal protocol error before source calls, monitor task
spawns, ACK processing, or value decoding. Apply the same guard to Rust-only
PUT_GET and PROCESS state if those operations stay enabled.

### PVA-R25: Client treats a null auth-method count as an empty advertised list

Severity: Medium

Status: **CLEARED** `e4056db8` — the null `Size` marker (0xFF) for the auth-method count is rejected instead of read as an empty list.

Rust:

- `src/proto/size.rs:44-55` returns `Ok(None)` for the `0xFF` null marker from
  the generic `decode_size()` helper.
- `src/decode.rs:210-214` decodes the
  `CONNECTION_VALIDATION` auth-method count with that helper and maps
  `None` to `0`.
- `src/client_native/server_conn.rs:220-232` then chooses `anonymous` whenever
  `ca` is absent from the parsed list, including this malformed null-count
  case.

pvxs reference:

- `src/pvaproto.h:284-305` rejects `0xFF` for `Size` unless the caller passes
  `allow_null=true`.
- `src/clientconn.cpp:228-232` decodes the server auth-method count with the
  default non-null `from_wire(M, nauth)`.
- `src/clientconn.cpp:247-251` disconnects when that decode leaves the buffer
  in a bad state.

Impact: a malformed server handshake that pvxs rejects can be accepted by the
Rust client as "zero advertised methods", after which the client sends an
anonymous validation reply. That hides protocol corruption and can turn a bad
auth advertisement into an authentication downgrade instead of a failed
virtual-circuit setup.

Fix direction: split the size decoder into non-null `Size` and nullable
`String`/`Selector` helpers, or require every non-null call site to use
`ok_or(...)`. The auth-method count should reject `0xFF` and propagate a
handshake decode error.

### PVA-R26: Stale CREATE_CHANNEL success is not destroyed after waiter timeout

Severity: Medium

Status: **CLEARED** `b0f37eef` — a late CREATE_CHANNEL success with no waiter goes through `maybe_destroy_stale_create_channel`.

Rust:

- `src/client_native/channel.rs:849-856` registers a one-shot CID waiter, sends
  `CREATE_CHANNEL`, and returns `Timeout` or cancellation if the waiter does not
  complete in `op_timeout`.
- `src/client_native/server_conn.rs:585-589` stores that waiter in `by_cid`.
- `src/client_native/server_conn.rs:840-848` removes the CID entry when a later
  `CREATE_CHANNEL` response arrives, then ignores `tx.send(frame)` failure.
  There is no stale-success path that emits `DESTROY_CHANNEL` for a server-side
  channel the caller has already abandoned.

pvxs reference:

- `src/clientconn.cpp:359-379` handles a `CREATE_CHANNEL` reply whose CID no
  longer has an interested channel. If the status is success, pvxs immediately
  sends `CMD_DESTROY_CHANNEL` with the returned SID/CID to dispose of the newly
  stale channel.

Impact: when the Rust client times out waiting for `CREATE_CHANNEL` but the
server later succeeds, the server-side channel can remain open until TCP close.
The response is removed from `by_cid` and dropped because the one-shot receiver
was already gone. A retry can also make late replies harder to distinguish from
the currently wanted channel state.

Fix direction: make timed-out CREATE ownership explicit. Either remove the CID
waiter and keep a stale-response tombstone, or have the router detect a
closed/dropped waiter. On a late successful `CREATE_CHANNEL` response, send
`DESTROY_CHANNEL` with the returned SID/CID before discarding the frame.

### PVA-R27: `pvget_many()` re-caches failed warm GET operations

Severity: Medium

Status: **CLEARED** `add08dd2` — `pvget_many` restores the warm-GET cache only after a successful DATA response.

Rust:

- `src/client_native/context.rs:1124-1130` takes `channel.cached_get` for the
  bulk warm path.
- `src/client_native/context.rs:1145-1150` installs a fresh one-shot sender in
  the cached operation and sends a GET using the old SID/IOID.
- `src/client_native/context.rs:1177-1194` marks a per-server send failure in
  the result array and clears only the temporary slot.
- `src/client_native/context.rs:1213-1216` restores the same cached operation
  after that send failure.
- `src/client_native/context.rs:1218-1235` also restores the cache after
  timeout, decode error, wrong response kind, channel-closed one-shot, or
  non-success GET status.

pvxs reference:

- `src/clientget.cpp:188-200` sends `DestroyRequest` and erases IOID mappings
  when an active GET/PUT/RPC operation is cancelled or implicitly abandoned.
- `src/clientget.cpp:313-329` sends `CMD_DESTROY_REQUEST` for a done operation
  and then erases both connection-level and channel-level IOID mappings.

Impact: the single warm GET path in `ops_v2.rs` falls through to a cold GET and
does not restore a failed warm cache, but `pvget_many()` keeps the stale
`CachedGet` after the same classes of failure. Repeated batched GETs can keep
reusing a server operation that timed out, returned an error status, was already
forgotten by the peer, or whose response was delivered to a dropped one-shot.
That can create repeated failures and abandoned IOID state instead of forcing a
fresh INIT.

Fix direction: restore `channel.cached_get` only after a successful DATA
response. On send failure, timeout, decode error, wrong response kind, or remote
error status, clear the reusable slot, unregister the IOID routing state, send
`DESTROY_REQUEST` when the connection is still alive, and require the next call
to repopulate the cache through the cold path.

### PVA-R28: Server silently ignores malformed management command payloads

Severity: Medium

Status: **CLEARED** `46cbb997` — the management-command handlers propagate a truncated payload as `PvaError::Decode`.

Rust:

- `src/server_native/tcp.rs:1738-1747` breaks out of a multi-name
  `CREATE_CHANNEL` frame on truncated CID/name or bad string decode, then
  returns `Ok(())`.
- `src/server_native/tcp.rs:1925-1933` returns silently when `CANCEL_REQUEST`
  lacks SID or IOID.
- `src/server_native/tcp.rs:1951-1958` returns or substitutes an empty string
  when `MESSAGE` lacks IOID/type/string.
- `src/server_native/tcp.rs:1967-1974` returns silently when `DESTROY_REQUEST`
  lacks SID or IOID.
- `src/server_native/tcp.rs:1845-1857` already treats truncated
  `DESTROY_CHANNEL` as a decode error, so the management-command behavior is not
  internally consistent.

pvxs reference:

- `src/serverchan.cpp:364-368` resets the connection if a `CREATE_CHANNEL`
  frame leaves the decoder in a bad state after the per-name loop.
- `src/serverconn.cpp:262-270` throws on malformed `CANCEL_REQUEST`.
- `src/serverconn.cpp:297-305` throws on malformed `DESTROY_REQUEST`.
- `src/serverconn.cpp:323-336` throws on malformed `MESSAGE`.

Impact: malformed clients can keep a Rust server connection alive after payload
decode faults that pvxs treats as protocol-fatal. For `CREATE_CHANNEL`, valid
pairs before the truncated entry may be accepted and stay open even though pvxs
disconnects after detecting the bad frame. For `CANCEL_REQUEST` and
`DESTROY_REQUEST`, a client can send malformed cleanup/control frames without
losing the connection.

Fix direction: make these management handlers return `PvaResult<()>` and
propagate malformed wire shapes through the dispatch loop as fatal protocol
errors. Keep semantic soft misses, such as unknown IOID/SID, separate from
truncated or invalid payloads.

## Verification

- pass: `cargo fmt --all -- --check`
- pass: `cargo clippy -p epics-pva-rs --all-targets -- -D warnings`
- pass: `cargo nextest run -p epics-pva-rs`
  - nextest run ID: `34f67f0c-4893-4f42-9160-925ca0452399`
  - scope: `-p epics-pva-rs`
  - result: 520 tests passed, 17 skipped
- pass: review-doc stale-marker scan
  - result: no stale markers or stale finding-count prose
- pass: review-doc trailing-whitespace scan
  - result: no trailing whitespace

These commands verify the crate's current test suite after the review doc
change. They do not prove the twenty-eight findings above are false; the findings
are recorded because the current suite lacks the specific parity regressions
noted in each section.

## Implementation Status (2026-05-18 fix round)

22 of 28 findings landed in this session. Each fix is anchored
to its commit and the file/line changes; deferred items are listed
with reason.

### Cleared (22)

- **PVA-R1** (HIGH) — `5a69034` codec::build_monitor_init pipeline
  initial nack + new pv_request::build_pv_request_pipeline; codec::
  build_monitor_start no longer carries pipeline_size.
- **PVA-R5** (HIGH) — `88232de` SharedPV::try_post_checked +
  put_delta no-handler path enforce opened descriptor.
- **PVA-R7** (HIGH) — `46cbb99` client_native reader splits
  Ok(None)/Ok(Some)/Err on frame parse + cancels on header decode
  fault.
- **PVA-R8** (MEDIUM) — `e4056db` ADVERTISED_AUTH_METHODS reordered
  to ["anonymous", "ca"].
- **PVA-R9** (HIGH) — `88232de` server GET data phase calls
  value_matches_descriptor before encoding.
- **PVA-R10** (MEDIUM) — `ec13197` client filters SEARCH_RESPONSE
  protocol != "tcp"; server filters SEARCH by requested protocol;
  beacons advertise runtime transport.
- **PVA-R12** (MEDIUM) — `add08dd` warm-GET failure sends
  DESTROY_REQUEST + unregister_ioid before cold fallback.
- **PVA-R13** (HIGH) — `46cbb99` Union/VariantArray encode/decode
  use per-element presence byte.
- **PVA-R15** (MEDIUM) — `b0f37ee` pvas_server_port + auto_beacon
  fallback to EPICS_PVA_AUTO_ADDR_LIST + beacon_addr_list fallback
  to EPICS_PVA_ADDR_LIST.
- **PVA-R16** (HIGH) — `bed630f` GET/PUT/MONITOR/PUT_GET/PROCESS
  INIT reject missing prototype; RPC retains descriptor-late.
- **PVA-R17** (MEDIUM) — `b0f37ee` raw monitor FINISH clears
  state.active.
- **PVA-R18** (MEDIUM) — `ec13197` ip_from_bytes_allow_unspec
  routes through wildcard substitution path.
- **PVA-R19** (HIGH) — `bed630f` pvRequest descriptor / EmptyMask
  → INIT-status error.
- **PVA-R20** (MEDIUM) — `5a69034` pipeline option accepts
  bool/integer/string; queueSize<2 disables pipeline.
- **PVA-R21** (HIGH) — `bed630f` duplicate INIT on live IOID is
  protocol-fatal.
- **PVA-R22** (HIGH) — `e4056db` parse_client_credentials returns
  PvaResult; auth value decode fault is connection-fatal.
- **PVA-R23** (HIGH) — `bed630f` client ioid_to_cmd + route_frame
  command-match gate.
- **PVA-R24** (HIGH) — `bed630f` server data-phase frames must
  match OpState.kind.
- **PVA-R25** (MEDIUM) — `e4056db` client rejects 0xFF null Size
  for auth-method count.
- **PVA-R26** (MEDIUM) — `b0f37ee` route_frame emits
  CMD_DESTROY_CHANNEL on late CREATE_CHANNEL success without
  waiter.
- **PVA-R27** (MEDIUM) — `add08dd` pvget_many only restores cache
  on successful DATA; emits DESTROY_REQUEST on failure.
- **PVA-R28** (MEDIUM) — `46cbb99` handle_cancel_request /
  handle_destroy_request / handle_message / handle_create_channel
  propagate truncated payloads as PvaError::Decode.

### Cleared in the follow-up commits

- **PVA-R1** (LOW, interop) — Wire-level proof that the Rust
  client puts `record._options.pipeline` into the pvRequest sent
  to a real pvxs server. `tests/interop_pvxs_mods/pipeline_r1.rs`
  spawns `softIocPVX` with `PVXS_LOG=pvxs.tcp.setup=DEBUG`,
  drives a Rust `PvaClient` with `pipeline_size(4)`, then
  asserts the pvxs server's captured stderr contains the
  `Monitor INIT pipeline ioid=` line emitted by
  `pvxs/src/servermon.cpp:587` only when `op->pipeline == true`.
- **PVA-R11** (MEDIUM) — `server_native/tcp.rs::handle_tcp_search`
  added; dispatched from the `Command::Search` arm. Reuses the
  UDP `parse_search_request` + `build_search_response_proto`
  helpers (now `pub(crate)`). Plumbed `guid` through
  `PvaServerConfig` and added a `pub fn tcp_addr()` accessor on
  `PvaServer`. Two reproducers:
  - `tests/interop_pvxs_mods/tcp_search_r11.rs::interop_r11_tcp_circuit_search_returns_matching_cid`
    builds a raw SEARCH frame on TCP and asserts cid round-trip
    (no pvxs dep — pure-Rust validation of the handler).
  - `tests/interop_pvxs_mods/tcp_search_r11.rs::interop_r11_pvxget_via_name_server_resolves_pv_on_rust_server`
    runs the real `pvxget` configured with
    `EPICS_PVA_NAME_SERVERS=<rust>:port` against a Rust-hosted
    PV and asserts the value is read end-to-end.
- **PVA-R20** (MEDIUM) — Parser coverage from five unit tests in
  `server_native::tcp::tests::pva_r20_*` (typed Bool, typed Int,
  string `"true"`, typed Bool false, queueSize<2). Wire-level
  interop added via a C++ harness
  `tests/interop_pvxs_mods/cpp_helpers/r20_typed_monitor.cpp`
  built on demand against `$PVXS_HOME/include` + libpvxs; it
  subscribes with the typed-builder
  `Context::request().record("pipeline", true)` shape against the
  Rust server and the test asserts both event delivery and the
  Rust server's new
  `debug!("MONITOR INIT pipeline negotiated")` tracing event
  (captured via a global `tracing_subscriber::fmt` writer). The
  log assertion is the discriminator: with the parser sabotaged
  to ignore typed Bool, the helper still receives events (no
  flow control) but the log line is absent and the test fails
  — verified by manually flipping the parser branch and re-
  running.

### Cross-impl interop matrix (added across 12 batches)

Beyond the original audit fixes, the following pvxs↔Rust
interop tests now exist (all gated behind
`cargo nextest run --profile interop`):

| # | Test | Direction |
|---|---|---|
| 1 | `complex_types::interop_complex_types_pvxget_against_rust_server` | Rust→pvxget |
| 2 | `reverse_complex_types::interop_reverse_complex_types_rust_client_decodes_pvxs` | pvxs→Rust client |
| 3 | `put_cross_impl::interop_put_a_pvxput_writes_into_rust_server` | pvxput→Rust |
| 4 | `put_cross_impl::interop_put_b_rust_client_writes_into_pvxs_server` | Rust→pvxs |
| 5 | `be_byte_order::interop_be_a_rust_server_emits_be_to_pvxget` | BE Rust→pvxget |
| 6 | `be_byte_order::interop_be_b_rust_client_decodes_pvxs_be_server` | pvxs BE→Rust |
| 7 | `type_cache::interop_type_cache_emit_pvxget_accepts_backrefs` | Rust 0xFD/0xFE→pvxget |
| 8 | `rpc_and_get_field::interop_rpc_pvxcall_against_rust_server` | pvxcall→Rust |
| 9 | `rpc_and_get_field::interop_get_field_pvxinfo_against_rust_server` | pvxinfo→Rust |
| 10 | `monitor_stream::interop_monitor_a_pvxmonitor_streams_from_rust_server` | pvxmonitor↔Rust |
| 11 | `monitor_stream::interop_monitor_b_rust_client_streams_from_pvxs_server` | Rust↔pvxs |
| 12 | `field_projection::interop_field_projection_a_pvxget_field_filter_against_rust_server` | Rust→pvxget |
| 13 | `large_array::interop_large_array_a_pvxget_reads_huge_rust_array` | 100K array Rust→pvxget |
| 14 | `large_array::interop_large_array_b_rust_client_reads_huge_pvxs_array` | pvxs 100K→Rust |
| 15 | `beacon_udp::interop_beacon_a_pvxlist_discovers_rust_server` | Rust beacon→pvxlist |
| 16 | `beacon_udp::interop_beacon_b_rust_client_receives_pvxs_beacons` | pvxs beacon→Rust |
| 17 | `access_denied::interop_access_denied_a_pvxput_to_rejecting_handler_fails` | pvxput→Rust deny |
| 18 | `tls_interop::interop_tls_a_pvxget_over_tls_to_rust_server` | TLS pvxget→Rust |

Plus the default-suite golden replays
(`wire_golden_complex_types_byte_exact` and
`wire_golden_decode_roundtrip`) replay 13 pvxs-verified fixtures
through the Rust encoder + decoder on every push without
needing pvxs at runtime.

Two real Rust source bugs surfaced during this work and were
fixed:

- **PUT subcmd 0x40 missing handler** (tcp.rs) — pvxput's
  `fetchPresent(true)` default sent CMD_PUT with subcmd 0x40
  (GET-before-PUT) which the Rust dispatcher tried to decode as
  bitset+value, tripping "short read u8". Fix added the GET-leg
  branch.
- **NTNDArray schema divergence** (nt/nd_array.rs) — Rust's
  layout had `descriptor` + `display` trailing fields not in
  pvxs's canonical schema, and the field order was wrong.
  Rewrote the descriptor + value builders + NdAttribute struct
  to match pvxs nt.cpp:196 byte-exact.

### Deferred by this round (5 — substantial architectural changes)

All five were taken up afterwards on the driver punchlist
`punchlist-2026-05-19.md`; each bullet names the commit that ended it,
and each row's own `Status:` line above carries the same commit.

- **PVA-R2** (MEDIUM) — `tcp_timeout` plumbing through
  ConnectionPool::get_or_connect + ServerConn::connect + spawned
  heartbeat task. Multi-layer signature change; out of scope for
  this fix round.
  Ended by `479f77c0`.
- **PVA-R3** (MEDIUM) — Nested Variant TypeCache plumbing.
  Requires `decode_pv_field` family to accept `&mut TypeCache` and
  thread through all op-response decoders + reader flattening.
  Ended by `cf5a0e5d`.
- **PVA-R4** (MEDIUM) — TCP name servers as persistent search
  peers (not direct-connect fallbacks). Architectural change to
  SearchEngine. The server side now answers TCP SEARCH (R11
  cleared), so a pvxs client with
  `EPICS_PVA_NAME_SERVERS=<rust>:port` can resolve PVs against a
  Rust gateway — the missing piece is the *client*-side
  persistent name-server connection.
  Ended by `7dfe5de6`, whose subject reads `BR-R4`.
- **PVA-R6** (MEDIUM) — SharedPV subscriber queue with squash-to-
  tail semantics. tokio::mpsc has no sender-side "drop oldest";
  faithful fix needs custom Mutex<VecDeque>+Notify or
  tokio::sync::watch.
  Ended by `c4bb773a`.
- **PVA-R14** (MEDIUM) — Decouple server source calls from per-
  connection read loop. Requires operation-state-machine
  restructure so source futures don't head-of-line-block the
  socket parser.
  Ended by `601a568f`, whose subject reads `BR-R14`.

### Coverage gaps cleared in the follow-up

- **Real ACF/ASG cross-impl** — `SharedSource::set_access_gate`
  added; tests `asg_cross_impl::interop_asg_denied_put_via_real_acf`
  parses an inline `.acf` via `parse_acf`, builds
  `AccessGate::required(cell, resolver)`, installs it on the
  source, and asserts pvxput is denied on the wire. Full
  ACF → gate → tcp.rs → pvxs round-trip now covered.
- **TLS client auth (mTLS)** — `tls_mtls::interop_tls_mtls_pvxget_with_client_cert_to_rust_server`.
  Rust server with `WebPkiClientVerifier` requires a client
  cert signed by the CA. pvxget presents the CA-signed leaf
  via `EPICS_PVA_TLS_KEYCHAIN` and the GET round-trips.

### Deferred by this round, second list (1 — architectural)

- **TLS via name-server (mixed-mode listener)** — Batch 12 uses
  UDP search because pvxs's plaintext name-server query
  collides with our TLS-only listener. Closing this requires
  the TCP accept loop to peek at the first byte and dispatch
  either to plain handshake or `TlsAcceptor` — a substantive
  refactor of `server_native/tcp.rs:460-590`. Not blocked
  technically, just out of scope for this fix round.
  Ended by `2d30aebc` (`run_tcp_server_on_listener` peeks the first
  byte and dispatches; regression `pva_tls_nameserver_mixed_mode_listener`).

### Verification

After all 22 fixes:

- `cargo fmt --all -- --check`: pass
- `cargo clippy -p epics-pva-rs --all-targets -- -D warnings`: pass
- `cargo nextest run -p epics-pva-rs`: 520 / 520 pass, 17 skipped
