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

## Uncertain Candidates

None. All investigated paths reached a definite conclusion (correct, bug, or documented gap).
A full audit of the `EncodeTypeCache` emit/decode round-trip is deferred to a future review.
