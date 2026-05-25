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

### R65 — GET_FIELD slow-path spawn not cancelled on connection teardown

Severity: Low-Medium

Status: Fixed

Evidence:

- Rust site: `src/server_native/tcp.rs:5574` — the GET_FIELD slow-path `tokio::spawn` stores no
  abort handle. If `src.get_introspection_checked()` hangs on a slow or unresponsive source,
  the task outlives the connection: it only exits when it eventually reaches
  `tx_clone.send(buf).await` and that send fails because the writer mpsc is closed. Every
  sibling task in this file (monitor subscriber via `monitor_abort: Option<Arc<AbortOnDrop>>`,
  data-phase tasks via `data_task_abort: Option<Arc<AbortOnDrop>>`) is tied to a per-connection
  abort chain; the GET_FIELD slow path bypasses it.
- C++ site: `pvxs/src/serverintrospect.cpp:63-68` — `ServerIntrospect` is registered in both
  `conn->opByIOID` and `chan->opByIOID`. On connection teardown,
  `ServerConn::cleanup()` (`serverconn.cpp:366-382`) iterates `opByIOID` and calls
  `op->cleanup()` on every entry, which fires `ServerIntrospectControl::error("Implicit Cancel")`
  — the source's `get_introspection` future receives an explicit cancellation signal and is
  expected to abort promptly. The Rust slow path leaves the source's future running until the
  source itself completes.

Impact:

If a `ChannelSource::get_introspection_checked()` implementation awaits an upstream connection
or network round-trip (e.g., a PVA gateway proxy), a client GET_FIELD followed by a disconnect
leaves one detached task per in-flight introspect alive until the upstream responds. Under a
slow/offline upstream the tasks can accumulate indefinitely.

Fix direction:

Pass the connection's cancellation token into the spawned task and `tokio::select!` between the
introspect future and the token's cancellation. This mirrors the abort-handle pattern used by
monitor and data-phase tasks, and matches pvxs's `opByIOID`-cleanup cancellation.

### R66 — `ChannelSource` trait lacks a per-channel `on_channel_close` hook

Severity: Low (feature gap, no state leak)

Status: Documented — fix deferred (breaking trait API change, needs sign-off)

Evidence:

- Rust site: `src/server_native/source.rs:174` — the `ChannelSource` trait has no
  `on_channel_close` (or equivalent) method. When a channel is destroyed (DESTROY_CHANNEL or
  connection close), the Rust server fires `notify_monitor_start(false)` via
  `MonitorStartControl::drop()` for each executing monitor op, but there is no channel-level
  teardown notification delivered to the source.
- C++ site: `pvxs/src/serverchan.cpp:57-59` — `ServerChan::cleanup()` calls `onClose("")` after
  cleaning up all ops. `ServerChannelControl::onClose()` (`:115-126`) lets a source register a
  per-channel teardown callback. pvxs uses this, for example, to notify `SharedPV` that a
  specific client channel is gone.

Impact:

A source that needs per-channel accounting (e.g., tracking which clients hold a channel open,
or releasing per-channel upstream connections) has no hook to act on channel teardown. The
existing `notify_monitor_start(false)` covers per-monitor-op teardown, but not the channel
level (a channel may have zero monitors and still benefit from an `on_channel_close` signal).
No state is leaked by the omission — RAII cleanup is correct — but sources requiring this
callback must poll or use out-of-band means.

Fix direction:

Add a default-no-op `on_channel_close(&self, name: &str, ctx: &ChannelContext)` method to
`ChannelSource`, called from `handle_destroy_channel` after `channels.remove(&sid)` and from
the connection teardown path. Default impl is empty so existing sources are unaffected; this
is still a semver-minor breaking change for object-safe vtable users (`ChannelSourceObj`).

### R67 — `BACKOFF_SECS` is dead code with a false "matches pvxs clientdiscover.cpp" claim

Severity: Low (doc inconsistency; no production runtime impact)

Status: Fixed (misleading doc corrected; constant retained as public API)

Evidence:

- Rust site: `src/client_native/search_engine.rs:44` —
  `pub const BACKOFF_SECS: &[u64] = &[1, 1, 2, 5, 10, 15, 30, 60, 120, 210];`
  with doc comment "matching pvxs `clientdiscover.cpp`". The constant is only referenced in the
  test `backoff_caps_at_last_value` (line 1928), which asserts the sequence is bounded. No
  production code path consults `BACKOFF_SECS`; retry scheduling is driven exclusively by the
  30-bucket ring mechanism in `run_engine`/`cascade_smoothed_next` (lines 713-732, 1300-1311).
  The module doc header (line 12) also states "pvxs-style backoff (15s → 30s → 60s → 120s →
  210s capped)", which is incorrect — the actual production cap is ~29 s (30-bucket ring at 1 s
  per tick, minus one rotation lag).
- C++ site: `pvxs/src/clientdiscover.cpp` — **no such sequence exists**. The values
  `[1, 1, 2, 5, 10, 15, 30, 60, 120, 210]` do not appear in any pvxs source file. The actual
  pvxs retry geometry is in `client.cpp:1118-1121`: `nSearch = min(nBuckets, nSearch+1u)`,
  placing the next retry at `(idx + nSearch) % 30`, giving linearly-growing waits of 1 s, 2 s,
  3 s, …, capping at 29 s (when nSearch reaches 30, next = idx, and the cursor takes 29 more
  ticks to return). The 210 s cap in `BACKOFF_SECS` has no pvxs equivalent.

Impact:

An external caller reading the public `BACKOFF_SECS` constant to predict channel search retry
timing would receive a sequence that caps at 210 s instead of the actual ~29 s, and would
attribute it to a file that contains no such values. The production search behaviour is correct;
only the documentation is misleading.

Fix direction:

Update the `BACKOFF_SECS` doc comment to clearly state it is not used by the engine and does
not correspond to pvxs. Update the module header to describe the actual bucket-ring geometry
(1 s tick × 30 buckets, cap ~29 s). Do not remove `BACKOFF_SECS` — it is `pub` and removal is
a semver-breaking change.

### R68 — x509 `authority` empty when peer sends partial TLS chain

Severity: Medium (functional parity gap; ACF `AUTHORITY(...)` rules fail)

Status: Fixed

Evidence:

- Rust site: `src/server_native/tcp.rs:1594` —
  `conn.peer_certificates().and_then(|chain| crate::auth::x509_credentials_from_chain(chain))`.
  `rustls::CommonState::peer_certificates()` returns **only the certificate chain the peer sent**
  (RFC 5246 §7.4.2 allows peers to omit the root CA since it is expected to be pre-installed on
  the verifying side). When the peer omits the root, `chain.last()` is the intermediate CA (or
  the leaf itself); `is_self_signed_ca` returns `false`; `authority` is left `""`.
  `ClientCredentials::authority` flows into ACF matching at
  `src/server_native/source.rs:295` (`check_with_roles`) and
  `src/server_native/composite.rs:168` (`check`).
- C++ site: `pvxs-tls/src/ossl.cpp:423-432` — pvxs calls
  `SSL_get0_verified_chain(ctx)` (not `SSL_get_peer_cert_chain`), which returns the **verified
  chain as built by OpenSSL's `X509_verify_cert`** — including root CAs fetched from the local
  trust store even when the peer did not send them. `sk_X509_value(chain, N-1)` is therefore
  reliably the root CA; `X509_check_ca(root) && (EXFLAG_SS)` passes; `authority = root_CN`.

Impact:

In a typical PKI the client sends `[leaf]` or `[leaf, intermediate]` without the root CA (the
root is already in the server trust store). pvxs always sets `authority = root_CA_CN` after a
successful TLS handshake; Rust leaves it empty. Any ACF rule of the form
`METHOD("x509") AUTHORITY("Root CA")` that works against a pvxs server would be silently denied
by the Rust server for every well-behaved client — a functional regression that is hard to
diagnose because the cert is accepted (TLS handshake passes) but access is denied.

Fix direction:

After extracting the leaf `account` from the peer chain, walk the server's `RootCertStore` trust
anchors: find the anchor whose `subject` DER matches the `issuer` DN of the last peer-chain cert.
Extract the CN from that anchor's subject as `authority`. Requires:
1. Adding `trust_roots: Arc<RootCertStore>` to `TlsServerConfig` (stored at config-build time
   when the trust store is constructed).
2. Threading the roots to the credential-extraction call at `tcp.rs:1594`.
3. A new internal helper `authority_from_trust_roots(chain, roots)` using `x509_parser` subject
   DN comparison and CN extraction.
The existing `pub fn x509_credentials_from_chain(chain)` signature is unchanged (it is public
API); the internal call site is changed to use the roots-aware path.

### R69 — PUT_GET INIT comment falsely attributes two-descriptor format to pvxs

Severity: Doc (comment cites pvxs for a protocol format pvxs never implements)

Status: Fixed

Evidence:

- Rust site: `src/server_native/tcp.rs:2863-2865` — comment reads
  "pvxs `serverget.cpp` emits two type descriptors for PUT_GET (the put-request and
  get-response structures)." pvxs `serverconn.cpp:259-260` shows `void ServerConn::handle_PUT_GET() {}`
  — an empty stub. pvxs implements GET/PUT/RPC via `ServerGPR` in `serverget.cpp`; it has no
  PUT_GET server implementation and never emits a PUT_GET INIT response.
- C++ site: `pvxs/src/serverconn.cpp:259-260` — `void ServerConn::handle_PUT_GET() {}`.
  Also `pvxs/src/serverget.cpp:19-158`: `ServerGPR` handles only `CMD_GET`, `CMD_PUT`, `CMD_RPC`;
  no PUT_GET handler. `CMD_PUT_GET=12` is defined in `pvaproto.h:628` but never dispatched
  beyond the empty stub.

Impact:

The comment misleads readers into thinking pvxs servers send PUT_GET INIT responses with two
descriptors. The actual format source is the pvAccessJava protocol specification; the Rust
implementation is correct — the comment is wrong. A reviewer trying to verify the two-descriptor
layout against pvxs would find `handle_PUT_GET()` empty and wrongly conclude the Rust code is
fabricating the format.

Fix direction:

Replace the pvxs cite with a reference to the pvAccessJava protocol wire format that defines
`PUT_GET INIT = ioid + subcmd + status + putIF + getIF`.

## EncodeTypeCache audit — Area A (R69 session)

Investigated: encode/decode round-trip for 0xFD/0xFE type-cache markers (Area A deferred from
prior session).

Outcome: No additional bugs found. Summary of verified-correct properties:

- `EncodeTypeCache` is connection-scoped (one instance at `handle_connection_io:2031`); passed
  `&mut` to all op handlers on the same connection. pvxs never emits 0xFD/0xFE (`to_wire`
  is inline only); Rust emits them when `config.emit_type_cache = true`. Parity gap is intentional
  (pvxs client accepts them via `from_wire` for pvAccessJava compat; Rust server mirrors that).
- Decode TypeCache is connection-scoped: `reader_type_cache` (local to the reader task,
  `server_conn.rs:362`) resolves 0xFD/0xFE in strict wire order via `flatten_type_cache_markers`
  before routing frames to per-op tasks. This structurally prevents 0xFE-before-0xFD races.
  pvxs `ConnBase::rxRegistry` is also per-connection (`conn.h:23`). Scoping matches.
- PUT_GET INIT double-emit (tcp.rs:2871-2872): first call emits `0xFD <slot N> <body>`;
  second call emits `0xFE <slot N>`. `flatten_type_cache_markers` expands both to inline
  before the client decoder sees them. `decode_put_get_init` (ops_v2.rs:2576-2579) decodes
  two inline descriptors correctly. Round-trip verified correct.
- R69 (doc-only fix): the comment attributing the two-descriptor format to pvxs is wrong.

### R70 — `anonymous` method sets `account = ""` instead of `"anonymous"`; `ca` with missing user falls back to `method="ca"/account=""` instead of anonymous

Severity: Medium (identity parity gap; ACF rules matching `USER("anonymous")` or `METHOD("ca")` without user constraint yield wrong decisions)

Status: Fixed

Evidence:

- Rust site: `src/server_native/tcp.rs:1781-1785` — `parse_client_credentials` returns
  `Ok(None)` only when `method.is_empty()`. For `method="anonymous"` (the string a pvxs client
  always sends — pvxs `clientconn.cpp:258`: `selected = "anonymous"; to_wire(R, selected)`) the
  early-return is NOT taken. The function continues, finds a `0xFF` null auth body (pvxs ca
  `clientconn.cpp:301-303`: `to_wire(R, Value::Helper::desc(cred))` where `cred` is empty →
  null type tag), hits the `0xFF` peek-branch at `tcp.rs:1803`, returns
  `Ok(Some({ method="anonymous", account="" }))`. Caller at `tcp.rs:2290` does `cred = claimed`
  → `cred.account` becomes `""` instead of `"anonymous"`.
- C++ site: `pvxs/src/serverconn.cpp:221-234` — for `selected="anonymous"` the
  `if(selected=="ca")` lambda is not entered, so `C->method` stays empty;
  `if(C->method.empty())` at line 229 sets `C->method = C->account = "anonymous"`.
  pvxs always produces `account="anonymous"` for the anonymous method.

Second aspect — `ca` with missing user field:

- Rust site: same `parse_client_credentials` — when `method="ca"` but the auth structure carries
  no `user` field, the extraction loop at `tcp.rs:1813-1836` leaves `creds.account = ""`. The
  caller sets `cred = { method="ca", account="" }`.
- C++ site: `pvxs/src/serverconn.cpp:223-231` — the `auth["user"].as<std::string>(lambda)`
  lambda is only called when the `user` field exists and converts to string; it is the lambda
  that sets `C->method = "ca"`. Without it, `C->method` stays empty →
  `if(C->method.empty())` triggers → `method="anonymous", account="anonymous"`.

Impact:

1. Anonymous: a normal pvxs client sends `method="anonymous"` with a null body. Rust produces
   `account=""`, pvxs produces `account="anonymous"`. Any ACF rule `USER("anonymous")` or
   `ACCOUNT("anonymous")` that works against a pvxs server fails silently against Rust because
   the account field doesn't match.
2. `ca` without user: a client that sends `method="ca"` but omits or empties the `user` field
   gets `method="ca", account=""` in Rust vs `method="anonymous", account="anonymous"` in pvxs.
   An ACF rule with `METHOD("ca")` and no user constraint grants the full `ca`-method privilege
   in Rust but only the anonymous default in pvxs — a privilege escalation vector.

Fix direction:

In `parse_client_credentials`:
1. Extend the early `Ok(None)` return to cover `method == "anonymous"` (matching the existing
   empty-method case), so the caller's pre-initialised `ClientCredentials::anonymous()`
   (account=`"anonymous"`) is preserved.
2. For `method == "ca"`, if after full auth-body decoding `creds.account` is still empty, return
   `Ok(None)` — the `ca`-method identity requires a non-empty user to be meaningful; without one
   pvxs falls back to anonymous.

### R71 — Monitor flow-control / pipeline credit accounting (parity-correct)

Severity: N/A — parity-correct

Status: No change

Evidence — credit ownership:

- Rust site: `tcp.rs:1182-1241` (`MonitorPipelineCredit::acquire()`): the ONLY site that
  decrements `monitor_window` (AtomicU32). CAS loop blocks when window==0, then atomically
  decrements. Called at `tcp.rs:5162` (initial snapshot) and `tcp.rs:5345` (each DATA event).
  No other path touches the counter downward.
- Rust ACK site: `tcp.rs:4537-4618` — the ONLY site that increments the window. On `is_ack =
  subcmd & 0x80`, reads u32 `nack` from frame, CAS saturating_add loop, then fires
  `notify_waiters()` when `prev==0` so blocked `acquire()` wakes.
- C++ sites: `pvxs/src/servermon.cpp:193` (`window--` in `doReply`, after `enqueueTxBody`),
  `servermon.cpp:652` (`op->window += nack` in ACK path). Same two-actor ownership.

Evidence — INIT / initial window:

- Rust site: `tcp.rs:3933-3948` — `pipeline_initial_nack` parsed when INIT subcmd has 0x80
  bit; stored in `monitor_window = Some(Arc<AtomicU32>)` initialized to `nack`.
- C++ site: `servermon.cpp:493-495` — `if(subcmd&0x80) from_wire(M, nack)`;
  `op->window = nack` at line 518.

Evidence — START / STOP:

- Rust site: `tcp.rs:4495-4496`: `is_pause = subcmd == 0x04`; `is_resume = subcmd & 0x40 != 0`.
- C++ site: `servermon.cpp:670-678`: `if(subcmd & 0x04)` (bit test); `bool start = subcmd & 0x40`.
- pvxs clients (`clientmon.cpp:127`): send `subcmd = 0x04` for STOP and `subcmd = 0x44` for
  START — the only two values that contain the 0x04 bit. For both, Rust produces the same
  result as the pvxs bit-test.
- Minor deviation: Rust uses exact-match (`== 0x04`) for STOP vs. pvxs bit-test (`& 0x04`). A
  hypothetical client sending `subcmd = 0x05` (unknown bit + stop bit) would be treated as STOP
  by pvxs but ignored by Rust. No conforming PVA client sends such a value; this is not a
  real-world bug.

Evidence — queue overflow / squash:

- Rust site: `tcp.rs:5230-5243` — after receiving the first event, drains remaining via
  `try_recv()`, coalescing via `coalesce_monitor_update` (keeps latest value, unions changed
  bitsets). During pause: `next_monitor_event` coalesces into `held` and flushes on resume
  (`tcp.rs:5704-5737`).
- C++ site: `servermon.cpp:280-285` — when `queue.size() >= limit && !maybe`, overwrites
  `queue.back()` (squash to the newest queued slot). Both implementations preserve the latest
  value and merge changed fields across overflows.

Evidence — LOW / HIGH watermarks:

- Rust site: `tcp.rs:1225-1238` — after `acquire()`, fires LOW (`WatermarkKind::Pause`) when
  `window_after <= lo` and crossing is freshly below (once per crossing via `cross_watermark`).
  `tcp.rs:4611-4644` — ACK path fires HIGH (`WatermarkKind::Resume`) when window crosses above
  `high`. Watermark levels clamped at INIT to `min(level, ack_at - 1)` (`tcp.rs:125-133`,
  matching `servermon.cpp:331-332`).
- C++ site: `servermon.cpp:195-207` (`onLowMark` when `window <= low`); `servermon.cpp:654-666`
  (`onHighMark` when `window > high` after ACK).

Evidence — DESTROY:

- Rust site: `tcp.rs:2501-2502` — MONITOR DESTROY handled via `Command::DestroyRequest` (a
  separate PVA command), not via subcmd bit 0x10 in the MONITOR message.
- C++ client site: `pvxs/src/clientmon.cpp:570` — "assume op has already sent
  CMD_DESTROY_REQUEST". pvxs clients use DESTROY_REQUEST, not subcmd 0x10.
- The subcmd 0x10 path in `servermon.cpp:690-707` exists but is not used by pvxs clients.
  Rust's DESTROY_REQUEST path covers the actual traffic.

Conclusion:

All six check points — credit decrement ownership, credit increment ownership, INIT window,
START/STOP, queue overflow, watermarks — are functionally equivalent to pvxs. No fix required.

### R72 — Unadvertised auth method: error + keep-alive + credential revert (parity-correct, documented)

Severity: N/A — parity-correct

Status: No change

Context:

pvxs `serverconn.cpp:238-241` rejects any `selected` method that is not exactly `"ca"` or
`"anonymous"` by calling `auth_complete(this, Status{Error,...})` and returning. There is no
`bev.reset()`, so the connection stays alive. pvxs also does NOT close the connection on this
path — the comment at lines 249-250 explains the design choice: `"No practical way to handle
auth failure. So we accept all credentials, but may not grant rights."`.

Evidence — pvxs identity state after rejection:

- C++ site: `serverconn.cpp:221-234` — a copy `C` of the current credential is built. For
  anything other than `selected=="ca"` with a `user` field, the "ca" lambda is NOT entered,
  so `C->method` stays empty. Line 229: `if(C->method.empty()) C->account = C->method =
  "anonymous"` runs for ALL non-ca-with-user selections including "x509". `cred = C` at
  line 234 stores this anonymous identity BEFORE the unadvertised check at line 238.
  Therefore `auth_complete(Status::Error)` at line 240 fires with the connection credential
  already set to `{method="anonymous", account="anonymous"}`.

Evidence — Rust identity state after rejection:

- Rust site: `tcp.rs:2320-2344` — after parse, `cred` may hold the client's claimed identity
  (e.g. `{method="x509", account="alice"}`). Before the advertised check, if the method is
  not in `ADVERTISED_AUTH_METHODS`, Rust explicitly executes `cred =
  ClientCredentials::anonymous()` (line 2344), then sends `Status::Error` in
  `CONNECTION_VALIDATED`, then calls `auth_complete` — which therefore sees
  `{method="anonymous", account="anonymous"}` exactly as pvxs does.

- Rationale documented in tcp.rs:2332-2344 (`EX-R7` comment): before this fix, the parsed
  claimed identity was installed into `cred` before the unadvertised check, so `auth_complete`
  and subsequent ACF-gated operations saw an identity the server had just rejected. The fix
  reverts `cred` to anonymous first, matching pvxs's implicit revert via the empty-method
  path.

Evidence — connection liveness:

- C++ site: `serverconn.cpp:238-241` — no `bev.reset()` call; handler returns, connection
  lives.
- Rust site: `tcp.rs:2358-2359` — frame sent, `handshake_complete = true`, loop continues.
  Both keep the connection open; subsequent operations see anonymous credentials.

Test coverage:

- `stability.rs:1339` (`auth_method_unadvertised_returns_status_error`): asserts
  `CONNECTION_VALIDATED` carries `Status::Error` for unadvertised method.
- `stability.rs:1437` (`ex_r7_unadvertised_auth_reverts_credential_to_anonymous`): asserts
  `auth_complete` hook observes `{method="anonymous", account="anonymous"}`, not the
  rejected claim.

Conclusion:

Rust matches pvxs exactly: Status::Error reply, connection kept alive, credential reverted to
anonymous. The EX-R7 label marks the bug that the credential revert fixed. No further change
required.

## Uncertain Candidates

None. All investigated paths reached a definite conclusion (correct, bug, or documented gap).
