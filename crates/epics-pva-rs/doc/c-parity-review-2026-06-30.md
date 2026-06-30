# epics-pva-rs — C-parity review vs pvxs (2026-06-30)

Codex-style C-parity audit of `epics-pva-rs` against the upstream **pvxs**
C++ reference at `~/codes/epics-modules/pvxs/src/` (TLS reference at
`~/codes/pvxs-tls/src/ossl.cpp`). Read-only audit; findings only, no source
edits in this doc's commit.

This is the first Codex-methodology (enumerate-C-surface → fan-out-by-category)
parity sweep of the PVA crate. Prior PVA reviews (`critical-review-2026-05-18`,
`functional-review-2026-05-20`, `source-review-2026-05-26`) were Rust-side
reviewer rounds using `R-N`/`FR-N` IDs; this audit uses a distinct **`PVX-N`**
prefix to avoid collision.

**Round-1 fan-out:** 5 opus panels, one per category, each cross-reading the
pvxs C++ and the Rust directly (Codex principle 4 — open every cited line, do
not trust comments or green tests). The PVA port proved exceptionally faithful:
**0 DEFECT, 8 CONCERN, ~30 verified-byte-exact / intentional-better OK-notes.**

Severity legend: **DEFECT** = wire/behavior bug vs pvxs on a conforming path;
**CONCERN** = narrow / non-conforming-peer-only / latent / design-divergence;
**OK-note** = verified faithful or intentional-better (Rust correct where pvxs
is lax/buggy — kept per the don't-copy-C-bugs steer).

---

## Open Findings

### Category A — Wire protocol & framing (PVX-1 .. PVX-20)

| id | Sev | Rust | pvxs | Impact |
|----|-----|------|------|--------|
| PVX-1 | CONCERN | `proto/status.rs:162` | `pvaproto.h:468-469` | `Status::decode` rejects any type byte in `0x04..=0xFE` ("unknown status kind") and faults the whole frame; pvxs `from_wire(Status)` casts **any** non-`0xFF` byte to `type_t` with no validation and proceeds to read msg+trace (treating code≥2 as failure). Against a non-conforming peer sending an out-of-range status code, Rust tears down the decode/op where pvxs decodes-and-continues. Not triggered by conforming pvxs peers (emit only 0–3/`0xFF`). Arguably intentional-better, but the frame-fault routing differs — Rust's strictness is a robustness *regression* on malformed input (tears the op vs limp-on). |
| PVX-2 | CONCERN | `proto/size.rs:43`; `bin/pvxvct-rs.rs:367` | `pvaproto.h:284,299-304` | `decode_size` drops pvxs's `from_wire(Size, allow_null=false)` structural gate: `0xFF` **always** → `Ok(None)`, delegating null-rejection to every caller instead of faulting by default. All **core** callers correctly re-reject `None` (bitset `proto/bitset.rs:200`, array/struct/union counts in `pvdata/encode.rs`, ARRAY length `tcp.rs:4847`) and accept null only for strings/union-selectors — wire parity preserved at the core — but the invariant is now **convention, not type**. `pvxvct-rs.rs:367` does `.unwrap_or(0)`, silently treating a null protocol-count as 0 where pvxs faults. Latent negative-space gap (missing structural gate); concrete instance is a diagnostic CLI only. |
| PVX-3 | OK-note | `proto/command.rs:77-78`; `server_native/tcp.rs:2782` | `pvaproto.h:607-613`; `conn.cpp:189-193` | Rust invents control commands `EchoRequest=3`/`EchoResponse=4` (pvxs `pva_ctrl_msg` defines only SetMarker=0/AckMarker=1/SetEndian=2) and sends control cmd 3 as a heartbeat. pvxs-safe: pvxs drains+ignores unrecognized control frames; the standard `CMD_ECHO=2` keepalive is correctly implemented both directions. Intentional Rust-to-Rust extension; no interop break. |
| PVX-4 | OK-note | `proto/status.rs:144` | `pvaproto.h:446-447` | pvxs `to_wire(Status)` collapses any `code==Ok && msg.empty() && trace.empty()` to a single `0xFF` byte; Rust emits `0xFF` only for the dedicated `OkNoMsg` variant, so a decoded `Detailed{Ok,"",""}` re-encodes as `[0x00,0x00,0x00]`. Benign: Rust constructors never build `Detailed{Ok,…}`; both encodings decode identically — interoperable. A harmless `to_wire` asymmetry on gateway re-encode. |

**Verified byte-exact (no divergence):** header (magic `0xCA`, version fault, flags, command, len-endianness from MSB flag, direction-bit fault both sides — `pvaproto.h:664-699`, `conn.cpp:159-160`); all 23 application command codes 0–22 (`pva_app_msg_t`); Size (`<254` 1-byte / `0xFE`+u32 / `0xFF` null — **no u64 path**, confirming pvxs has none); BitSet/BitMask word-by-word (`bitmask.cpp:116-173`); segmentation gate + accumulation + reset (`conn.cpp:228-291`); byte-order negotiation (`SET_BYTE_ORDER`=0, MSB-flag latch); SockAddr v4-mapped-IPv6 origin-tag (`evhelper.cpp:897-907`).

**Category A verdict:** 0 DEFECT, 2 CONCERN (both low), 2 OK-note. Highest: PVX-2.

### Category B — PVData type system & encoding (PVX-21 .. PVX-40)

| id | Sev | Rust | pvxs | Impact |
|----|-----|------|------|--------|
| PVX-21 | CONCERN | `pvdata/encode.rs:750-766` | `dataencode.cpp:130-137` (+83-118) | **StructA/UnionA element decode rejects type-cache markers that pvxs accepts.** Rust reads the byte after `0x88`/`0x89` and hard-requires literal `0x80`/`0x81`; pvxs decodes the element via recursive `from_wire`, which accepts `0xfd` (FULL_WITH_ID) and `0xfe` (ONLY_ID) in element position and validates the resolved code. A pvDataCPP/pvDataJava-family peer (pva2pva, EPICS-Java IOC, older pvAccessCPP) that caches an element struct type independently of its array emits `0x88 0xfe <id>`; Rust faults the whole descriptor → channel/op breaks. pvxs↔Rust interop is **unaffected** (neither emits cache markers on encode — PVX-30), so narrow but a hard failure vs legitimate non-pvxs peers. |
| PVX-22..PVX-40 | OK-note | (see below) | (see below) | Scalar/array/struct/union/variant type-code bytes (PVX-22); Size (PVX-23); String byte-preserving (PVX-24); BitSet LE+BE (PVX-25); union member selector (PVX-26); StructA/UnionA/AnyA presence byte (PVX-27); partial encode subtree+depth-first numbering + `total_bits()==size()` (PVX-28); partial decode subtree skip (PVX-29); type-cache emit OFF by default → descriptors inlined like pvxs (PVX-30); `0xfd` emplace-no-overwrite / `0xfe` miss-faults (PVX-31); BoundedString `0x83` + bounded/fixed `0x10` strict-pvxs both reject (PVX-32/33); NTScalar (PVX-34); NTEnum/NTTable (PVX-35); NTNDArray (PVX-36); NTAttribute `:1.0` 8-field (PVX-37); NTURI `:1.0` (PVX-38); Variant decode + zero-construct defaults (PVX-39/40). All verified byte-exact or invariant-by-construction. |

**Category B verdict:** 0 DEFECT, 1 CONCERN (PVX-21). The pvData type-system and encoding layer is otherwise faithful across all focus areas.

### Category C — Client (PVX-41 .. PVX-60)

| id | Sev | Rust | pvxs | Impact |
|----|-----|------|------|--------|
| PVX-41 | CONCERN | `client_native/ops_v2.rs:1595-1599`, `codec.rs:219-222` | `clientget.cpp:255-268,299-300`, `client.h:756` | **PUT get-first uses a separate ChannelGet, not the in-PUT `0x40` phase.** pvxs `_doGet=true` default makes a builder-callback / enum-by-label PUT send `CMD_PUT` subcmd `0x40` (GetOPut) on the *same* op to read the current value through the put's own pvRequest mask, then exec. Rust never emits `CMD_PUT/0x40`; when `value_target_is_enum` it opens an entirely separate `CMD_GET` op with an **empty (all-fields) pvRequest**. Wire-divergent for every enum-by-label PUT: extra op/RTT, different ioid, wider field read. Functionally equivalent for resolving the value. |
| PVX-42 | CONCERN (top) | `client_native/context.rs:154`; `ops_v2.rs:88,132-138`; `codec.rs:244-255` | `clientmon.cpp:50-52,334-348`, `client.h:876-877` | **Default monitor is PIPELINED; pvxs default is not.** The client builder defaults `pipeline_size = DEFAULT_PIPELINE_SIZE = 4`, so `MonitorFlow::window(4)` sets `pipeline=true` on *every* default monitor. INIT then goes as subcmd `0x88` (vs pvxs `0x08`), appends a 4-byte `nack` credit trailer, injects `record._options{pipeline=true,queueSize=4}`, and the loop emits `MONITOR_ACK` (`0x80`) frames. pvxs leaves `op->pipeline=false` unless the user's pvRequest carries `record._options.pipeline`, so its default monitor is a plain `0x08` INIT, no trailer/options/ACKs, free-running server flow. **Changes the wire shape of every subscription** and switches the server into credit-windowed mode. Interoperable with pvxs servers (Rust ACKs correctly, no stall) but diverges from the reference default and alters overrun semantics (credit-hold vs server-squash). **DESIGN DIVERGENCE — needs sign-off.** |
| PVX-43 | OK-note | `client_native/ops_v2.rs:463-467` | `clientget.cpp:531-534` (+255-268) | GET pipelines INIT+GET in one TCP write before the INIT reply (pvxs waits for the type reply, then sends GET). Safe by TCP ordering + a 1-RTT win, but diverges from pvxs's strict sequential handshake; against a server that rejects INIT, a GET for the failed op is already on the wire. |
| PVX-44 | OK-note | `client_native/server_conn.rs:1423-1453` | `clientconn.cpp:260-267` | "ca" auth credential adds a `groups` (string[]) field for server-side ACF `group:` matching; pvxs `caMethod` carries only user+host. Intentional-better (pvxs decodes the cred as a generic Value, so the extra field is backward-compatible). |
| PVX-45 | OK-note | `client_native/ops_v2.rs:1719-1735` | `clientmon.cpp:69,162-240,684-690` | No client-side monitor queue/squash: pvxs keeps a bounded `deque` of `queueSize` and squashes the tail (`nCliSquash`) when the consumer lags. Rust delivers each update synchronously to a callback, so `n_queue`/`n_cli_squash` are 0 by construction; back-pressure is via the per-IOID stream. No wire consequence (squash is client-local); merged-value (`fill_unmarked_from_prior`≡`cache_sync`) and server-overrun counting preserved. |

**Verified clean (not flagged):** INIT/exec/destroy/start/stop/ack subcommand bytes `0x08`/`0x00`/`0x10`/`0x44`/`0x04`/`0x80`/`0x88`; connection-validation auth prefers "ca" then "anonymous", reply `buffer_size=0x10000`/`registry_size=0x7fff`/`qos=0`; CID base `0x12345678`, IOID base `0x10002000`, DESTROY field order, CREATE route-by-CID + stale-channel destroy; search ring (30 buckets, 1s tick, `min(attempt,30)` cascade, 200ms revolution, 30s pokeHoldoff); `MonitorFlow::from_record_options` (`pipeline`/`queueSize`/`ackAny` clamp).

**Category C verdict:** 0 DEFECT, 2 CONCERN (PVX-41, PVX-42), 3 OK-note. Highest: PVX-42.

### Category D — Server (PVX-61 .. PVX-80)

| id | Sev | Rust | pvxs | Impact |
|----|-----|------|------|--------|
| PVX-61 | CONCERN | `server_native/tcp.rs:6765`, `:7181` (`subscribe_seeded` spawned only on `is_start`) | `sharedpv.cpp:252-275`, `servermon.cpp:591` (`onSubscribe`/`connectSub` at INIT) + `:271-287` | pvxs registers the monitor subscriber at **INIT** (`connectSub` posts `current` and the subscriber thereafter accrues every `post()` into the bounded queue, flushed on START). Rust establishes the source subscription only when the **START** frame spawns the task, seeding `current`-at-START. Value transitions in the INIT-reply→START window (one client RTT) are **not delivered** — the START seed collapses them to the latest. Observable by a conformance test posting between INIT-reply and START (pvxs delivers initial+intermediate, Rust delivers only latest). Narrow window; latest state never lost. |
| PVX-62..PVX-68 | OK-note | (see below) | (see below) | Handshake bytes byte-exact incl. auth methods reverse-priority `["anonymous","ca"]` (PVX-62 — deliberate, do not "fix"); CREATE_CHANNEL reply omits the spec `uint16` access-rights word like pvxs ("useless anyway") (PVX-63 — do not add per-channel rights); DESTROY on unknown SID silent no-reply (PVX-64); MONITOR DATA single empty `0x00` overrun bitset + FINISH `0x10`+Status (PVX-65); pipeline flow control — absent-nack seeds window=0, initial-seed consumes one credit, ACK refill before START/STOP, saturating window add (PVX-66); GET_FIELD no-descriptor → `Status::error` with no type word (PVX-67); SharedPV open-state sum type, structural `value_matches_descriptor` fit (Rust safer superset) (PVX-68). |

**Category D verdict:** 0 DEFECT, 1 CONCERN (PVX-61), 7 OK-note. Highest: PVX-61.

### Category E — Discovery / transport / config / auth (PVX-81 .. PVX-100)

| id | Sev | Rust | pvxs | Impact |
|----|-----|------|------|--------|
| PVX-81 | CONCERN | `codec.rs:112`; `client_native/search_engine.rs:1760-1761,2098-2099` | `client.cpp:612-618,1180-1184`; `pvaproto.h:644` | **SEARCH `Unicast` flag (`0x80`) never set on UDP unicast destinations.** pvxs sets `isucast = !isMCast()` (cleared only for a local broadcast addr) and toggles `*pflags |= Unicast` per-destination, so a non-mcast/non-bcast host in `EPICS_PVA_ADDR_LIST` receives a SEARCH with flags byte `0x80` (`0x81` discover). Rust calls `pack_search_frames(..., unicast=false)` for *every* UDP destination, emitting `0x00` (`0x01` discover). The current pvxs reference never reads the inbound Unicast bit (only `MustReply` — `udp_collector.cpp:363`), so no impact vs this pvxs build, but the bit is protocol-defined and pvAccessCPP/Java key UDP fan-out off it: a unicast `EPICS_PVA_ADDR_LIST` entry pointed at a multi-IOC host won't trigger that host's local re-broadcast on flag-reading peers → sibling-process PVs undiscovered. **Wire-byte divergence in the SEARCH flags octet.** |
| PVX-82 | CONCERN | `config/env.rs:969-985` (INTF), `1040-1058` (IGNORE) | `config.cpp:418-424` + `151-176` (`required=true`→throw at 172-174) | **`EPICS_PVAS_INTF_ADDR_LIST` / `EPICS_PVAS_IGNORE_ADDR_LIST` parse leniency.** pvxs parses both with `required=true`, so a malformed endpoint throws and aborts server config. Rust logs a warning and silently drops the bad token. For INTF specifically, an all-bad list yields an empty interface vector → `expand()` promotes it to wildcard `0.0.0.0` (`env.rs:1373-1378`), so a typo in the bind-restriction silently makes the server listen on **every interface** instead of failing loudly — a silent over-broad-bind where pvxs hard-fails. |
| PVX-83..PVX-88 | OK-note | (see below) | (see below) | Beacon emit byte-exact: skip-8 + GUID(12) + flags + seq + change + v4-mapped addr(16) + tcp_port + "tcp" + serverStatus `0xFF`, burst-10@15s then 180s (PVX-83); SEARCH/SEARCH_RESPONSE layout, `search_seq=0x6669_6e64`, linear backoff `min(30,n+1)` + `nextN-nextnextN>100` smoothing (PVX-84); auth negotiation + `getpwnam`/`getgrouplist` doubling + CA fallbacks (PVX-85); config precedence PVAS↔PVA, 16-bit port truncation, CONN_TMO ×4/3 + 40s/2s, port-0 normalization (PVX-86); multicast-shim emits `CMD_BEACON` — cited pvxs `mshim.cpp:199` absent from this checkout, can't verify (PVX-87); `EPICS_PVAS_BEACON_PERIOD`/`_LONG` Rust-only additive env vars, defaults reproduce pvxs exactly (PVX-88). |

**Also verified faithful:** `EPICS_PVAS_IGNORE_ADDR_LIST` consumed (`udp.rs:774-783`, port-0-means-any like `server.cpp:658-669`); `dedup_endpoints` longest-TTL-first-seen ≡ `removeDups<SockEndpoint>`; BEACON_ADDR_LIST PVAS→PVA precedence; client prefer-ca; v4-mapped/raw-zero address encoding distinction.

**Category E verdict:** 0 DEFECT, 2 CONCERN (PVX-81, PVX-82), 6 OK-note. Highest: PVX-81.

---

## Round-1 disposition / triage

**Thematic cluster:** the recurring shape is *strictness-direction divergence* —
Rust is sometimes stricter than pvxs (PVX-1 status decode, PVX-21 element
markers) and sometimes more lenient (PVX-2 size null gate, PVX-82 addr-list
parse). Two are genuine wire-shape divergences (PVX-42 pipelined-default,
PVX-81 Unicast flag). The encode/decode core, framing, type system, server
op-handlers, and beacon/search layout are byte-faithful.

| Finding | Disposition |
|---------|-------------|
| PVX-21 | **CLEARED (`66149781`)** — Fix: accept `0xfd`/`0xfe` cache markers in StructA/UnionA element position via recursive decode (interop with pvData-family peers). Contained. |
| PVX-81 | **CLEARED (`579e3e1b`)** — Fix: set the SEARCH `Unicast` flag (`0x80`) per-destination for unicast dests (`isucast = !isMCast()`). Wire-parity. Contained. |
| PVX-2  | **CLEARED (`a71e168b`)** — Fix (structural): added `proto::size::decode_size_nonnull(cur, order, what)`, the non-null primitive (pvxs `from_wire(Size, allow_null=false)`) that holds the invariant by construction; routed the count-must-not-be-null family (encode.rs ×11, bitset, the `pvxvct-rs.rs:367` `.unwrap_or(0)` CLI bug) through it. Strings/union-selectors stay on `decode_size`. |
| PVX-82 | **CLEARED (`03caa4d1` INTF + `da4b0be8` IGNORE)** — Fix (behavior change vs prior Rust, matches pvxs): `server_intf_addr_list_checked` / `server_ignore_addr_list_checked` error when every token in a non-blank server addr-list is unresolvable, recorded as `intf_addr_error` / `ignore_addr_error` and surfaced as a hard `PvaServer::start` failure — closes the silent over-broad wildcard bind (INTF) and the silently-empty blocklist (IGNORE). The finding named both lists; the IGNORE half was closed in the convergence round (`da4b0be8`). Client-path `expand()` wildcard left as-is (DISTINCT — diagnostic, pvxs client parse is `required=false`). **Residual (documented, accepted):** the gate is `all`-bad (fails only when every token is unresolvable), whereas pvxs `required=true` throws on `any` bad token — but the Rust gate is fail-closed (binds/blocks a subset, never a superset/wildcard), so no over-broad exposure. |
| PVX-42 | **CLEARED (`1941d5e2`)** — Fixed by matching pvxs: default `pipeline_size = 0` so the default monitor is non-pipelined (plain `0x08` INIT, no credit trailer/options/ACKs). Pipelining stays opt-in via `pipeline_size(n)` or a pvRequest `record._options.pipeline`. Regression test pins the non-pipelined default. |
| PVX-61 | **OPEN (round-2 candidate)** — establish monitor subscription at INIT (pvxs `connectSub`) so INIT→START transitions queue instead of collapse. More involved server change. |
| PVX-41 | **OPEN (low / round-2)** — enum-by-label PUT via in-PUT `0x40` GetOPut instead of a separate GET op. Functionally equivalent; PUT state-machine change. |
| PVX-1  | **DOCUMENTED (kept) — intentional divergence** — Rust's `Status::decode` rejecting out-of-range status type bytes is a deliberate strictness choice, NOT softened to pvxs's silent `type_t` coercion. Conforming pvxs peers emit only 0–3/`0xFF`, so no interop impact; the strictness rejects malformed peers rather than limping on a coerced failure code. Softening would adopt pvxs's lenient cast — declined per "don't copy C's looser behavior". |

---

## Review Log

### Round 1 (2026-06-30) — first Codex C-parity sweep of PVA vs pvxs
5 opus panels fanned out by category (wire / pvdata / client / server /
discovery), each cross-reading pvxs C++ and the Rust directly. Result:
**0 DEFECT, 8 CONCERN, ~30 OK-note** — the PVA port is exceptionally faithful
(byte-exact framing, type system, server op-handlers, beacon/search). The 8
CONCERNs cluster as strictness-direction divergences (PVX-1/2/21/82) plus two
genuine wire-shape divergences (PVX-42 pipelined-default monitor, PVX-81 SEARCH
Unicast flag) and two narrow behavioral gaps (PVX-61 monitor-sub-at-START,
PVX-41 PUT get-first). Disposition table above. Fixes proceed in per-finding
commits; PVX-42 held for sign-off (changes every subscription's wire shape).

### Round 2 (2026-06-30) — fix phase, first batch

Applied the dispositioned fixes as per-finding commits:

- **PVX-42** `1941d5e2` — default monitor non-pipelined (matched pvxs;
  the sign-off resolved to "match pvxs default", not keep+document).
- **PVX-82** `03caa4d1` — strict INTF addr-list: all-unresolvable list
  fails `PvaServer::start` instead of binding the wildcard.
- **PVX-21** `66149781` — StructA/UnionA element decode accepts
  `0xfd`/`0xfe` type-cache markers via recursive decode.
- **PVX-81** `579e3e1b` — SEARCH `Unicast` flag (`0x80`) set
  per-destination for unicast UDP dests.
- **PVX-2** `a71e168b` — structural `decode_size_nonnull` gate; the
  count-must-not-be-null family (incl. the `pvxvct-rs.rs` CLI
  `.unwrap_or(0)`) routed through it.
- **PVX-1** — documented as an intentional strictness divergence;
  kept (not softened to pvxs's lenient status-code coercion).

Remaining open: **PVX-61** (monitor-sub-at-INIT) and **PVX-41**
(in-PUT `0x40` GetOPut) — round-2 candidates, larger state-machine
changes, deferred. A caucus opus verification round on this batch
follows.

#### Convergence (caucus opus verification round `01KWCC5K`)

Three opus panels (pvdata / client / discovery) re-read each fix against
the pvxs C++ directly. **All five CONFIRMED FIXED, 0 blocking CONCERN.**
Three residuals surfaced, dispositioned:

- **PVX-82 IGNORE missed sibling** — the finding named both server
  addr-lists, but `03caa4d1` closed only INTF; `EPICS_PVAS_IGNORE_ADDR_LIST`
  still dropped bad tokens silently where pvxs `config.cpp:422-423`
  (`required=true`) throws. **Closed in `da4b0be8`** — `server_ignore_addr_list_checked`
  + `ignore_addr_error` surfaced at `start`, mirroring the INTF gate, with
  env + start-refusal regression tests.
- **PVX-82 partial-list leniency** (all-bad gate vs pvxs per-token any-bad
  throw) — accepted; fail-closed, no over-broad bind. Documented on the
  PVX-82 row.
- **PVX-2 framing-site DRY** — 3 framing null-reject sites
  (`decode.rs:225`, `ops_v2.rs:3933`, `tcp.rs:4845`) hand-roll the
  rejection instead of reusing `decode_size_nonnull`. Accepted — correctness
  equivalent (they already fault), and they carry richer site-specific
  diagnostics (`tcp.rs` is its own `PvaError`-returning primitive) that the
  generic `what` message would flatten. No change.
