# `pva://` record links on the RTEMS target

**Status:** design only. No production code changed by this document; the
probes below were `cargo check` invocations and `git status --porcelain`
is empty at the commit that carries this file.
**Scope:** `doc/qsrv-rtems-design.md` §7 **stage 5** — the one stage that
document declined to schedule ("blocked, large, not scheduled"), plus the
`pvalink` items its §8 parked (items 2, 3, 8).
**Base:** `7f9a089d` (*Merge qsrv-rtems-stage2: Q:group PVs served and
verified on RTEMS target*).
**C reference:** pvxs at `/home/stevek/work/epics-modules/pvxs` (paths
below are relative to that root).

Every number in this document was produced by running the command quoted
next to it on this tree at `7f9a089d`. Where a claim could not be
measured it says so and lands in §6, not in the body.

---

## 0. The reframing, and the one number that produces it

`doc/qsrv-rtems-design.md` §3.4 states the blocker as *"47 compile errors
over a 23,881-line `client_native` tree … a peer project to the PVA
server's own sans-io work"*. Re-measured (§1.1) the 47 is exact and still
holds. **The 23,881 is the misleading half.**

Measured, the whole of `client_native`'s network surface is **eight import
statements and one `libc` cmsg block** — 3 × `tokio::net`, 3 × `socket2`,
2 × `AsyncUdpV4`, all in production code, all in four files:

| file | lines | production `tokio::net` | production `socket2` | production `libc::` |
|---|---:|---:|---:|---:|
| `udp.rs` | 1,071 | 1 (`:48`) | 1 (`:47`) | 40 |
| `search_engine.rs` | 5,782 | 1 (`:32`) | 2 (`:714`, `:796`) | 0 |
| `server_conn.rs` | 2,762 | 1 (`:33`) | 0 | 0 |
| `search.rs` | 251 | 0 | 0 | 0 |
| `ops_v2.rs` | 7,931 | **0** | 0 | 0 |
| `context.rs` | 3,694 | **0** | 0 | 0 |
| `channel.rs` | 1,485 | **0** | 0 | 0 |
| `operation.rs` | 497 | **0** | 0 | 0 |
| `beacon_throttle.rs` | 370 | **0** | 0 | 0 |
| `mod.rs` | 38 | **0** | 0 | 0 |

Command (run from `crates/epics-pva-rs/src/client_native`):

```
for f in *.rs; do printf '%-22s %6s tokio=%s sock2=%s libc=%s\n' "$f" \
  "$(wc -l < $f)" "$(rg -c 'tokio::net' $f || echo 0)" \
  "$(rg -c '\bsocket2\b' $f || echo 0)" "$(rg -c '\blibc::' $f || echo 0)"; done
rg -n 'tokio::net' *.rs        # 11 hits; 4 are production, 7 are in #[cfg(test)]
```

14,015 of `client_native`'s 23,881 lines — the operation state machines
(`ops_v2.rs`), the client context and channel cache (`context.rs`,
`channel.rs`), the operation handles (`operation.rs`) and the beacon
throttle — name no socket type at all. Behind them sit a further **24,676
lines** of `epics-pva-rs` protocol code that **already compiles for
`armv7-rtems-eabihf` today**, because the *server* needs it:

| module | lines | `tokio::` refs |
|---|---:|---:|
| `pvdata` | 10,839 | 0 |
| `nt` | 3,029 | 0 |
| `pv_request` | 2,764 | 0 |
| `format` | 2,722 | 0 |
| `decode` | 2,100 | 0 |
| `proto` | 1,901 | 0 |
| `codec` | 686 | 0 |
| `leaf_convert` | 439 | 0 |
| `peer_buf` | 196 | 0 |

None of these is `#[cfg]`-gated (`rg -n '^pub mod' crates/epics-pva-rs/src/lib.rs`),
and `./scripts/rtems-check.sh` is green (§1.4), so "PVA serialization
works on the target" is a *measured* fact, not a projection.

And the decisive structural finding (§1.5): the client's connection state
machine is **already transport-erased**. `ServerConn::run_handshake_and_spawn`
takes `Box<dyn AsyncRead>` / `Box<dyn AsyncWrite>` (`server_conn.rs:208-210`,
`:285-291`) — the identical seam shape that the PVA **server**'s blocking
driver already exploits (`server_native/tcp.rs:2862-2863`,
`server_native/blocking.rs`). Only `ServerConn::connect` (`:219`) and
`connect_tls` (`:243`) name `TcpStream`, and each does so on exactly one
line before boxing the halves.

So the honest shape of the work is **not** "port 23,881 lines". It is:

1. add a **third constructor** to `ServerConn` that produces the same
   `DynRead`/`DynWrite` from blocking `std::net` threads (the server's
   `ChannelReader`/`ChannelWriter` already exist and do exactly this);
2. make the search engine buildable with **no UDP socket** so it runs off
   `EPICS_PVA_NAME_SERVERS` alone (measured viable — §4.2);
3. route 12 production `tokio::spawn` sites through `runtime::task::spawn`;
4. **give pvalink one shared `PvaClient` instead of one per link** — a
   C-parity defect (§2.4) that is also the hardest RTEMS ceiling, and the
   only item on this list that is fixable and testable entirely on the
   host with no RTEMS work at all.

Item 4 is the one this document argues should go first.

---

## 1. The `client_native` failure surface, measured

### 1.1 Probe D, re-measured — 47, unchanged

```
cargo +nightly check --locked --no-default-features --features client \
  -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf \
  -p epics-pva-rs --lib
```

```
error: could not compile `epics-pva-rs` (lib) due to 47 previous errors; 1 warning emitted
```

Control — the same feature selection on the host:

```
cargo check --locked --no-default-features --features client -p epics-pva-rs --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 20.23s     (exit 0)
```

All 47 are target-specific. Nothing here is a pre-existing host defect.

### 1.2 The 47, classified by layer

Taken from `--message-format=json`, with each diagnostic's primary span
walked out through its macro-expansion chain so a `tokio::select!` arm is
attributed to *our* file rather than to tokio's `select.rs`.

| layer | count | sites |
|---|---:|---|
| **newlib/`libc` gaps** (no `msghdr`/`recvmsg`/`cmsghdr`/`CMSG_*`/`in6_pktinfo`/`IP_RECVDSTADDR`/`IPV6_PKTINFO`/`IPV6_RECVPKTINFO`) | **16** | `udp.rs:423,431,519,527,595,596,602,614,615,619,623,624,627×2,654,658` |
| **`tokio::net`** (`TcpStream`, `UdpSocket`) | **3** | `search_engine.rs:32`, `server_conn.rs:33`, `udp.rs:48` |
| **`tokio::io::Interest`** (readiness API, needs the reactor) | **1** | `udp.rs:508` |
| **`socket2`** | **3** | `udp.rs:47`, `search_engine.rs:714,796` |
| **`epics_base_rs::net::AsyncUdpV4`** (host-only in base) | **2** | `search.rs:18`, `search_engine.rs:30` |
| **`if-addrs`-backed helpers in our own `config::env`** (host-gated `fn`s) | **4** | `udp.rs:160`, `search_engine.rs:890,1932,1954` |
| **cascade** — `[u8]` unsized, all inside the two `select!`/`recv()` bodies whose channel types were poisoned by the imports above | **18** | `search_engine.rs:1295` (×17), `:1604` |

By rustc error code: 20 × `E0425`, 18 × `E0277`, 8 × `E0432`, 1 × `E0433`.

Two things follow.

* **29 primary + 18 cascade.** The 18 `E0277`s are not an independent
  defect class — they are inference fallout inside `run_engine`'s
  `select!` after `use tokio::net::{TcpStream, UdpSocket}` failed.
  `doc/qsrv-rtems-design.md` §0 attributed "11" to `search_engine.rs`
  and "4" to `config/env.rs`; the expansion-resolved attribution is
  **25 to `search_engine.rs`** (8 direct + 17 macro-expanded) and **0 to
  `config/env.rs`** — those four are `E0425` *call sites* in `udp.rs` /
  `search_engine.rs` naming functions that `config/env.rs` gates out with
  `#[cfg(not(target_os = "rtems"))]` (`env.rs:326`, `:1337`, `:1378`).
  The corrected attribution matters because it says the work is in the
  callers, not in `config::env`.
* **47 is a lower bound, and must be reported as one.** An unresolved
  import poisons its module, and rustc suppresses downstream type errors
  in code that names the poisoned items. `ops_v2.rs` reporting **zero**
  errors is therefore *not* proof that it compiles for the target; it is
  consistent both with "transport-independent" and with "suppressed". The
  §0 census (source-text: zero `tokio::net`, zero `socket2`, zero `libc::`
  in `ops_v2.rs`) is the evidence for transport-independence, and it is
  weaker evidence than a green build. Stage 1's gate (§5) is what converts
  it into a measured fact.

### 1.3 What this means per subsystem

| `client_native` subsystem | verdict | evidence |
|---|---|---|
| **Serialization / framing** (`crate::{codec,decode,proto,pvdata,pv_request,nt,format}`, 24,676 lines) | **already on the target** | ungated modules; `rtems-check.sh` green (§1.4) |
| **Operation state machines** — GET/PUT/MONITOR/RPC (`ops_v2.rs`, 7,931 lines) | **transport-independent by source-text census**; unproven by compiler | zero `tokio::net`/`socket2`/`libc::`; 2 production `tokio::spawn` |
| **Channel cache + connection pool** (`channel.rs`, `context.rs`) | same | zero socket types; pool keyed on `SocketAddr` (`channel.rs:187-198`) |
| **Connection framing** (`server_conn.rs`, 2,762 lines) | **transport-erased already** — one import to displace | `DynRead`/`DynWrite` at `:208-210`; `TcpStream` named only at `:225`, `:251` |
| **UDP search + beacons** (`udp.rs`, 1,071 lines) | **not portable as written** — 20 of the 47 | `IP_PKTINFO`/`recvmsg` original-destination recovery has no newlib equivalent |
| **Search engine** (`search_engine.rs`, 5,782 lines) | **split**: the UDP arms are blocked, the TCP name-server path is not | `ns_task` (`:2776`) / `ns_run_once` (`:2812`) use `TcpStream` only |

### 1.4 Baseline gate

```
./scripts/rtems-check.sh
RTEMS gate: every crate and target binary compiles for armv7-rtems-eabihf, in both
the portability and the image configuration.                              (exit 0)
```

Five crates × 2 configurations + 2 target binaries × 2 configurations,
all green. This is the gate every stage in §5 extends.

### 1.5 The seam, side by side

The server driver that already ships and the client change this document
proposes are the same move:

```
server (shipped)                          client (proposed)
─────────────────────────────────         ─────────────────────────────────
tcp.rs:2862  type SrvRead  = Box<dyn      server_conn.rs:208 type DynRead =
             AsyncRead + Unpin + Send>       Box<dyn AsyncRead + Unpin + Send>
tcp.rs:2984  handle_connection_io(        server_conn.rs:285
               source, SrvRead, SrvWrite,   run_handshake_and_spawn(
               peer, config, init)            target, DynRead, DynWrite, …)
                 ▲                                    ▲
      ┌──────────┴──────────┐                ┌────────┴────────┐
 accept.rs          blocking.rs         connect()         connect_blocking()
 (tokio reactor)    (2 std threads +    (tokio TcpStream)  (2 std threads +
                     ChannelReader/                         the SAME adapters)
                     ChannelWriter,
                     blocking.rs:607/:734)
```

`ChannelReader` / `ChannelWriter` (`server_native/blocking.rs:607`, `:734`)
are `AsyncRead`/`AsyncWrite` adapters over bounded mpsc channels fed and
drained by two blocking threads. They are `pub(super)` today; making them
reachable from `client_native` is a visibility change, not a rewrite.
That is the whole of stage 2.

---

## 2. pvalink's actual client surface

### 2.1 It is eight operations, and one file

`crates/epics-bridge-rs/src/pvalink/` is 10,855 lines over six files.
Exactly **one** of them imports the PVA client:

```
rg -n 'use epics_pva_rs::client' crates/epics-bridge-rs/src/pvalink/*.rs
link.rs:11:use epics_pva_rs::client::PvaClient;
link.rs:12:use epics_pva_rs::client_native::CacheAction;
link.rs:13:use epics_pva_rs::client_native::ops_v2::PutLeaf;
```

(`integration.rs:4347` also names `PvaClient`, inside `#[cfg(test)]`.)

The complete production call surface:

| # | operation | call site | used for | context it runs in |
|---:|---|---|---|---|
| 1 | `PvaClient::builder().timeout(d).build()` | `link.rs:297` | one client **per link** — see §2.4 | `PvaLink::open`, an `async fn` on the link work owner |
| 2 | `pvmonitor(pv, cb)` | `link.rs:374`, `:480` | INP value monitor; OUT liveness monitor | inside a spawned re-subscribe loop |
| 3 | `pvmonitor_with_request(pv, req, cb)` | `link.rs:371`, `:477` | same, with `record[pipeline=…,queueSize=N]` | same |
| 4 | `pvget_full(pv)` | `link.rs:601` | **non-monitor** INP read only | `PvaLink::read_with_field`, off the record thread |
| 5 | `pvput_{,field_,pv_field_,pv_field_field_}with_request` | `link.rs:949`, `:953`, `:960`, `:964` | single-field OUT write (4 arity variants) | `put_single`, from the link-put queue owner |
| 6 | `pvput_fields_typed(pv, &[(path, PutLeaf)], req)` | `link.rs:869` | one combined multi-field PUT | same |
| 7 | `pvprocess(pv)` | `link.rs:995` | `proc`-only write, no value | same |
| 8 | `cache_clear_action(pv, CacheAction::Drop)` | `link.rs:907` | evict the dead channel after a failed OUT write | same |

Plus `client.clone()` (`link.rs:320`, `:447`), which is an
`Arc<ClientInner>` bump.

There is no RPC, no `find_all`, no discovery, no beacon subscription, no
raw-frame monitor, no TLS. A blocking client that serves *this* list is a
strict subset of `client_native`, and it is the same subset a
`connect_blocking` seam gets for free, because every one of these eight
lowers onto `ops_v2` over a `ServerConn`.

### 2.2 The threading contract is already strict — and pvalink already obeys it

This is the finding that makes the port small, and it comes from
`epics-base-rs`, not from the bridge. `LinkSet`
(`crates/epics-base-rs/src/server/database/link_set.rs:239-318`) splits
its own surface by whether I/O is permitted:

| method | sync/async | contract, quoted from the trait docs |
|---|---|---|
| `is_connected` | `fn` | *"Synchronous: asked on the record-processing thread inside the record's advisory write gate. MUST NOT perform I/O."* (`:244-246`) |
| `get_cached_value` | `fn` | *"MUST NOT perform I/O — which is why this is a `fn` and `get_value` is an `async fn`."* (`:266-267`) |
| `put_admission` | `fn` | *"MUST NOT perform I/O. It is the one lset call left inside the record's advisory write gate."* (`:303-305`) |
| `get_value` | `async fn` | *"MAY perform I/O … called only from the database's link work owner task"* (`:252-254`) |
| `connect_link` | `async fn` | *"**Called from the database's link work owner task**, so it MAY block on the network."* (`:286-288`) |
| `put_value` / `flush_puts` | `async fn` | drained by the owner loop, `link_put_queue.rs:481`, `:540` |

And that owner is **already on the RTEMS seam**:

```
rg -n 'runtime::task::spawn' crates/epics-base-rs/src/server/database/link_put_queue.rs
446:    crate::runtime::task::spawn(owner_loop(queue, work, db));
479:                crate::runtime::task::spawn(async move {
491:                crate::runtime::task::spawn(async move {
```

So on the target, `LinkSet::get_value` / `connect_link` / `put_value` /
`flush_puts` already run on a callback-pool band worker. pvalink does not
need a thread of its own for them.

pvalink's own resolver honours the same split. `build_resolver`'s closure
(`integration.rs:749-810`) is **cache-only**: on a hit it reads
`try_read_cached_with_field` (a `parking_lot` lock, no await); on a miss
it stages the open with `resolver.handle.spawn(...)` and returns `None`
for that cycle — the C `dbCaAddLink` → `addAction(pca, CA_CONNECT)` shape
(`dbCa.c:735-800`). **No production path blocks the record thread on the
wire.** The one `handle.block_on` in the module is `iocsh.rs:44`, and the
target has no iocsh.

### 2.3 Where the monitor callback runs

`pvmonitor`'s callback is invoked *inline* by `op_monitor`
(`context.rs:1631-1640`), i.e. on whatever task drives the subscription.
In pvalink that task is `tokio::spawn`ed at `link.rs:341` (INP) and
`:463` (OUT). Routed through `runtime::task::spawn`, on RTEMS that is
`spawn_future(callbacks().handle(), DEFAULT_SPAWN_PRIORITY, fut)` —
i.e. **the `cbMedium` band**, one worker
(`callback_executor.rs:52 DEFAULT_THREADS_PER_PRIORITY = 1`;
`future_exec.rs:105 DEFAULT_SPAWN_PRIORITY = CallbackPriority::Medium`).

That is survivable, and the reason is measurable rather than hopeful: the
callback body does no blocking work. `on_event` (`link.rs:355-365`) does
a `store`, a `parking_lot` lock write, and `enqueue_scan_trigger`, whose
send is a **non-blocking `try_send`** with a coalesce-on-full fallback.
The executor is cooperative — a task that returns `Pending` releases its
worker and is re-enqueued on wake (`future_exec.rs:11-19`) — so N idle
monitor subscriptions share one worker.

The rule this imposes on stage 3 is sharp and worth stating as an
invariant:

> **MUST NOT** any pvalink task spawned onto a callback band call
> `block_on_sync` / `park_on`. A band has exactly one worker; parking it
> stops every deferred callback, every FLNK tail and every other monitor
> on that band.

`block_on_sync` already refuses the analogous unsound case
(`task.rs:114-122` returns `NotBlockable` on a current-thread runtime),
but it does **not** know about callback bands. Closing that hole
structurally — rather than by review — is stage 3's real content.

### 2.4 The defect that must be fixed first: one `PvaClient` per link

`PvaLink::open` builds a client (`link.rs:297`), and the registry caches
`PvaLink` by `(pv_name, pipeline, queue_size, direction)`
(`registry.rs:81`, `key_of` at `:103`). So **an IOC with N distinct
`pva://` links builds N independent `PvaClient`s.**

Each `PvaClient` owns:

* its own `pool: Arc<ConnectionPool>` (`context.rs:377`), keyed on
  `SocketAddr` (`channel.rs:187-188`) — so two links to the same upstream
  IOC open **two** TCP connections, each with its own reader, writer and
  heartbeat task (`server_conn.rs:357`, `:398`, `:658`);
* its own lazily-spawned `search: OnceLock<SearchEngine>`
  (`context.rs:379-380`), which on the host means its own UDP sockets and
  its own TCP connection **per name server** (`search_engine.rs:1204-1230`).

`share_udp(true)` exists (`context.rs:210`, `:399-402`) and would collapse
the search engines onto a process-wide singleton — **pvalink never calls
it** (`rg -n 'share_udp' crates/epics-bridge-rs/src/pvalink/*.rs` → no
hits). And even with it, the connection pool stays per client.

This is a **C-parity defect independent of RTEMS.** pvxs holds exactly
one client context for the whole IOC:

```
pvxs/ioc/pvalink.h:107      client::Context provider_remote;   // in linkGlobal_t
pvxs/ioc/pvalink.cpp:60     linkGlobal->provider_remote = ioc::server().clientConfig().build();
```

built once in `linkGlobal_t::alloc()` (`pvalink.cpp:51-63`).

On the host the cost is wasted sockets. On the target it is the ceiling.
With the blocking driver of §1.5, one TCP connection costs two threads;
at `StackSizeClass::Small` on `armv7-rtems-eabihf` that is
2 × 262,144 B = 512 KiB (§4.3). Twenty links to one upstream IOC would be
20 connections + 20 name-server connections = 40 × 512 KiB ≈ **20 MiB of
thread stack**, against a `CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS` of
150 (`doc/rtems-fd-ceiling-deviation.md`) of which the IOC already holds
142. Shared, the same twenty links cost **one** connection.

The structural fix is to hoist the client to the single owner that
already exists — `PvaLinkResolver` / `PvaLinkRegistry` — and pass it
down. The seam for it is already in the code:
`PvaLink::for_test_with_client(config, client)` (`link.rs:1531`) takes an
injected client today, for tests. Making that the *only* constructor and
deleting `PvaClient::builder()` from `link.rs:297` is the change; it
removes the dual meaning ("a link owns a client" vs "a link uses the
IOC's client") rather than adding a cache in front of it.

It is entirely host-testable. It should not wait for stage 2.

---

## 3. The CA precedent — what transfers, and what does not

### 3.1 What exists

```
git log --oneline --all --grep='sans-io' -i
391e94d9 refactor(ca/server): extract READ reply byte production as sans-io core
5c5d7f1a refactor(ca/server): single-owner outbox replaces shared-mutable writer
2d67942d doc(rtems): runtime-portability design — sans-io cores + per-platform drivers
```

| artefact | lines | shape |
|---|---:|---|
| `epics-ca-rs/src/server/blocking.rs` | 3,541 | thread-per-client CA server; C `rsrv`/`camsgtask` parity |
| `epics-ca-rs/src/server/recv.rs` | 378 | `RecvAccumulator` — sans-io byte accumulation |
| `epics-ca-rs/src/server/outbox.rs` | 132 | single-owner write side |
| `epics-pva-rs/src/server_native/blocking.rs` | 3,729 | thread-per-connection PVA server |

### 3.2 What transfers to a PVA client

**Three things, and they are the good ones.**

1. **"The seam is the byte source, not the frame pipeline."**
   `server_native/blocking.rs`'s module doc states it outright: *"every
   byte still reaches the same parser, the same `select!`, the same
   handlers. Nothing in the 21,000-line protocol module is `cfg`-ed."*
   The client is in a strictly better starting position, because
   `ServerConn` was *already* written against `DynRead`/`DynWrite`
   (§1.5) — the server had to introduce that type; the client has it.

2. **The adapters themselves.** `ChannelReader` (`blocking.rs:607-732`)
   and `ChannelWriter` (`:734-…`) are `AsyncRead`/`AsyncWrite` over
   bounded channels driven by two blocking threads. A client connection
   needs exactly the same pair, in the same direction. Reuse, not
   re-derivation.

3. **`block_on_sync` as the single sync↔async bridge**, plus the
   `enter_ioc_thread` prologue and `spawn_dedicated_thread`
   (`blocking.rs:501-511`, `:1266-1272`) with an explicit
   `StackSizeClass` — including the source-text guards that make the
   convention enforceable (`blocking.rs:3056-3095`,
   `rtems-pva-ioc.rs:680`).

Two further *facts* transfer, both hard-won and both binding on the
client:

* **No fd dup.** `try_clone` / `F_DUPFD` / `F_DUPFD_CLOEXEC` all fail
  `ENXIO` on any libbsd socket, while `F_DUPFD` on `/dev/console`
  succeeds — measured, and recorded identically in both drivers
  (`epics-pva-rs/src/server_native/blocking.rs:3696-3704`,
  `epics-ca-rs/src/server/blocking.rs:907-916`). The reader and writer
  threads share **one** `Arc<TcpStream>` via `impl Read for &TcpStream`.
  A client that reaches for `try_clone` compiles and fails at runtime on
  target only.
* **No `local_addr` readback.** RTEMS's libc omits the BSD `sockaddr`
  length byte, so `bind()` succeeds and `local_addr()` returns
  `InvalidInput` (`rtems-pva-ioc.rs:263-272`, which prints *"UDP search
  bound, port unreadable"* rather than an invented port). §4.2 turns this
  into a stage-ordering argument.

### 3.3 What does **not** transfer — and this is the correction

There is **no blocking CA client**, and `ca://` record links do **not**
work on the target either:

```
rg -n 'mod calink|mod client' crates/epics-ca-rs/src/lib.rs
38:pub mod calink;          # preceded at :37 by #[cfg(not(target_os = "rtems"))]
48:pub mod client;          # preceded at :47 by #[cfg(not(target_os = "rtems"))]
```

with the comment at `lib.rs:33-36`: *"Host-only (it drives a live CA
client, `tokio::net`)."*

Three consequences:

* The CA precedent is **server-side only**. Nobody has done the
  client-side version of this work in this workspace, in either protocol.
  A design that says "do what CA did" is describing a project that does
  not exist.
* The functional gap on the target IOC is **both** link schemes, not just
  `pva://`. `doc/qsrv-rtems-design.md` §7 stage 5's proposed startup
  banner should therefore say *no external record links of either
  scheme*, not *no pvalink*.
* Conversely, the client-side seam this document proposes is **generic**.
  `epics-ca-rs/src/client/transport.rs:12` names `tokio::net::TcpStream`
  on one line, exactly as `server_conn.rs:33` does. Whatever blocking
  byte-source primitive stage 2 lands should be shared, not duplicated
  into a second copy for CA later. That is the difference between fixing
  the family and patching the cited site.

One shape that does **not** carry over: the server's per-connection
`Big`-stack "operation thread" that runs the whole protocol state machine
under `block_on_sync` (`blocking.rs:1258-1272`). The client has no
equivalent, because `ServerConn` already spawns its reader/writer/heartbeat
as *tasks* (`server_conn.rs:357`, `:398`, `:658`). On the target those
land on the callback pool, so a client connection costs **two threads
(reader+writer), not three** — the reason §4.3's arithmetic is 512 KiB per
connection rather than the server's measured 1.59 MB.

---

## 4. RTEMS platform constraints that bind this design

### 4.1 No tokio on target; every spawn goes through one seam

`runtime::task` (`crates/epics-base-rs/src/runtime/task.rs`) is the seam:
`#[cfg(tokio_backend)] spawn = tokio::spawn` (`:191-199`),
`#[cfg(exec_backend)] spawn = spawn_future(callbacks().handle(),
DEFAULT_SPAWN_PRIORITY, fut)` (`:202-214`).

The bill on the client side is small:

```
for f in crates/epics-pva-rs/src/client_native/*.rs; do
  t=$(rg -n '^#\[cfg\(test\)\]' "$f" | head -1 | cut -d: -f1); t=${t:-999999}
  n=$(rg -n 'tokio::spawn' "$f" | awk -F: -v t="$t" '$1<t' | wc -l)
  [ "$n" -gt 0 ] && echo "$(basename $f) $n"
done
```

| file | production `tokio::spawn` |
|---|---:|
| `context.rs` | 3 |
| `server_conn.rs` | 3 |
| `ops_v2.rs` | 2 |
| `search_engine.rs` | 2 |
| `operation.rs` | 1 |
| `udp.rs` | 1 |
| **total** | **12** |

Plus, on the bridge side, 2 in `link.rs` (`:341`, `:463`) and 4
`handle.spawn` in `integration.rs` (`:419`, `:804`, `:1393`, `:1402`).

**The trap that a compile probe cannot catch.** `tokio::runtime::Handle`
and `Handle::current()` **compile for `armv7-rtems-eabihf`** — the RTEMS
tokio table retains `rt`/`time`/`sync`/`macros`
(`crates/epics-pva-rs/Cargo.toml:161-172`), and
`runtime::task.rs:356-358` (`pub fn runtime_handle() ->
tokio::runtime::Handle { Handle::current() }`) is **not** `cfg`-gated yet
sits inside a green gate (§1.4). So `PvaLinkResolver::new(handle)`
(`integration.rs:158`) and the `handle: tokio::runtime::Handle` field
(`:38`, `:826`) will **type-check for the target and panic at boot**.
Any future "pvalink compiles for RTEMS" green must not be read as
"pvalink runs on RTEMS".

### 4.2 TCP-only search works; UDP search must be a later stage

Three independent measured facts converge on the same ordering.

1. **PVA is reachable over TCP alone.** `doc/rtems-scope-b-session-handoff.md`
   §5.5: *"PVA is reachable over **TCP alone** via
   `EPICS_PVA_NAME_SERVERS`, no UDP or broadcast — which is what makes it
   testable under SLIRP."*
2. **The TCP name-server path is already an independent task.**
   `run_engine` (`search_engine.rs:1107-1127`) spawns one `ns_task` per
   name server (`:1221`) feeding a single `ns_response_rx`; `ns_task`
   (`:2776`) / `ns_run_once` (`:2812`) touch `TcpStream` and nothing else
   — no `socket2`, no `AsyncUdpV4`, no `libc` cmsg. All 47 errors are in
   the UDP half and in `config::env`'s host-gated interface helpers.
3. **The UDP half needs a readback the target cannot give.**
   `run_engine` computes `response_port` from
   `search_socket.local_addrs()` (`:1137-1142`) and `response_port_v6`
   from `local_addr()` (`:1151-1154`), and stamps them into every SEARCH
   frame. On target `local_addr()` returns `InvalidInput` (§3.2), so a
   UDP SEARCH would advertise port 0. **No production TCP path in
   `client_native` reads `local_addr`** (`rg -n 'local_addr' server_conn.rs`
   → test code only), so TCP-only search sidesteps the libc bug entirely.

The blocker for a later UDP stage is *not* absent primitives — the target
already binds UDP with raw `libc`, no `socket2`
(`server_native/blocking.rs:1351-1414`, the RTEMS arm at `:1376`) and
`rtems-pva-ioc` uses it (`:407-408`). What is missing is the
`IP_PKTINFO`/`recvmsg` original-destination recovery (16 of the 47
errors) and a `local_addr` substitute. Both are real work; neither is on
the critical path for a record link.

`run_engine`'s signature takes `search_socket: AsyncUdpV4` **by value,
not `Option`** (`:1109`), so "no UDP" is a signature change, not a
configuration flag. That is stage 1's content.

### 4.3 Thread and stack budget — the real numbers for this target

`StackSizeClass::bytes()` is `f * 0x10000 * size_of::<usize>()`
(`task.rs:490-498`). On `armv7-rtems-eabihf`, `usize` is 4 bytes:

| class | armv7 bytes |
|---|---:|
| `Small` | 262,144 (256 KiB) |
| `Medium` | 524,288 (512 KiB) |
| `Big` | 1,048,576 (1 MiB) |

**These are the target figures, and the commonly-quoted "2 MiB per
thread" is the 64-bit host figure** — the doc comment at `task.rs:483`
records exactly this correction ("Read *on a 64-bit target* here before:
it was wrong in the direction that matters").

Baseline threads a target IOC already runs, each with its stack class
from source and each confirmed present on a real boot in
`doc/rtems-priority-on-target-measurement.md:41-45`, `:103-107`:

| thread | created at | class | armv7 bytes |
|---|---|---|---:|
| `cbLow` | `callback_executor.rs:293-296` | Big | 1,048,576 |
| `cbMedium` | same | Big | 1,048,576 |
| `cbHigh` | same | Big | 1,048,576 |
| `cbTimer` | `delayed_timer.rs:232-239` | Medium | 524,288 |
| `scanOnce` | `scan_once.rs:187-191` | Big | 1,048,576 |
| `PVAS-TCP` | `rtems-pva-ioc.rs:460-464` | Medium | 524,288 |
| `PVAS-UDP` | `rtems-pva-ioc.rs:476-478` | Medium | 524,288 |
| per inbound conn: `PVAS-conn` | `blocking.rs:1266-1269` | Big | 1,048,576 |
| per inbound conn: `PVAS-read`, `PVAS-write` | `blocking.rs:501-505` | Small ×2 | 524,288 |

The per-inbound-connection total (1,572,864 B) matches the on-target
measurement of **1,589,000 B/connection, 97.4 % of it stack**
(`doc/rtems-scope-b-session-handoff.md:88`, §5.6).

**What a `pva://` link adds**, under the §1.5 design:

| item | threads | armv7 stack |
|---|---:|---:|
| per **upstream server** connection (reader + writer, `Small`) | 2 | 524,288 |
| per **name-server** connection (reader + writer, `Small`) | 2 | 524,288 |
| the connection's reader/writer/heartbeat *tasks* | 0 | 0 (cbMedium band) |
| the monitor re-subscribe loop, per link | 0 | 0 (cbMedium band) |
| the search engine loop | 0 | 0 (cbMedium band) |

So the cost is **per distinct TCP peer, not per link** — which is exactly
why §2.4 is stage 0 and not an optimisation. With one shared client and
one name server, an IOC with any number of `pva://` links to one upstream
costs **4 threads / 1 MiB**. With today's per-link client and 20 links it
costs 80 threads / 20 MiB, against 142 free descriptors and a heap that
already holds five `Big` stacks.

Every one of those threads must call `enter_ioc_thread` as its first
statement — RTEMS pthreads inherit `POSIX_Init`'s near-idle priority, so
a thread that skips it runs below everything. `spawn_dedicated_thread`
does it for you (`task.rs:1298-1330`); a raw `thread::Builder` does not,
and `blocking.rs:3412-3440`'s source-text guard exists because of that.

### 4.4 The clock

`Instant` on target is **1-second-quantized** (`libc`'s `time_t` is `i32`
on `arm-rtems6`, so `std`'s `timespec` is half-size;
`doc/rtems-scope-b-session-handoff.md:440-444`). Consequences for pvalink:

* `wait_for_link_connected`'s 50 ms poll (`integration.rs:733`, deadline
  at `:725`, `:730`) becomes a 1 s poll. It is a test helper
  (pvxs `testqsrvWaitForLinkConnected`, `pvalink.cpp:131`), so this is a
  correctness-of-diagnostics issue, not a data-path one — but a stage-5
  on-target test that waits 5 s for a link must not be written as a
  sub-second loop.
* `link.rs`'s reconnect backoff ladder starts at 250 ms
  (`link.rs:342`, `:464`). Below 1 s it is not expressible on target; the
  effective first retry is 1 s. `doc/qsrv-rtems-design.md` §4 already
  notes the ladder is ours, not a pvxs parity number.
* `search_engine.rs`'s `MULTI_SERVER_WINDOW` (200 ms) and
  `INITIAL_SEARCH_DELAY` coalescing windows are likewise sub-second. They
  affect UDP search only, so they arrive with the UDP stage.

---

## 5. Staged plan

Same discipline as `doc/qsrv-rtems-design.md` §7: each stage names its own
gate, and no stage depends on a later one. Stage 0 and stage 1 have **no
RTEMS dependency at all** and can land in any order relative to stages
2–4.

### Stage 0 — one client for the whole IOC (small, host-only) — *do this first*

Hoist `PvaClient` out of `PvaLink::open` (`link.rs:297`) into
`PvaLinkResolver`/`PvaLinkRegistry`, matching pvxs's single
`linkGlobal->provider_remote` (`ioc/pvalink.h:107`, `pvalink.cpp:60`).
`PvaLink::for_test_with_client` (`link.rs:1531`) already takes an
injected client; make that the only constructor.

*Size:* ~40 lines across `link.rs`, `registry.rs`, `integration.rs`.

*Why first:* it is a C-parity defect on its own (§2.4), it is the single
largest RTEMS resource lever (§4.3), and it is provable on the host with
no toolchain.

*Gate:*
* a new test asserting that two links to **the same** `pv_name` with
  **different** `queue_size` (i.e. distinct `RegistryKey`s, so distinct
  `PvaLink`s) resolve through **one** `ConnectionPool` — count the
  upstream IOC's `active_connections()`, which must be 1;
* `cargo nextest run -p epics-bridge-rs`;
* `cargo nextest run --workspace` — `tests/pvalink_seam.rs` and
  `tests/pva_gateway.rs` both drive the client.

*Risk:* a shared client means a shared `timeout`. Today each link builds
with `Duration::from_secs(5)` (`link.rs:297`); if any config path wants a
per-link timeout it must move to the per-operation call, not the client.
Check before, not after.

### Stage 1 — a search engine that runs with no UDP socket (medium, host-testable)

Change `run_engine`'s `search_socket: AsyncUdpV4` (`search_engine.rs:1109`)
to an `Option`, and make every UDP arm of the `select!` degrade to
`std::future::pending()` when absent — the shape `beacon_recv`
(`:1256-1263`) and `deadline_arm` (`:1278-1286`) already use. `ns_task`
and the TCP name-server path (`:1204-1230`, `:2776`, `:2812`) are
untouched.

Make the absence *structural*, not a runtime branch: a
`SearchTransport::{Udp(AsyncUdpV4, …), NameServersOnly}` sum type, so
"UDP socket present" and "UDP arms armed" cannot disagree. A bare
`Option` plus `if let` in six arms is the patch; the sum type is the fix.

*Size:* ~120 lines in `search_engine.rs`; no other file.

*Gate:*
* a host test that resolves a PV with **`EPICS_PVA_ADDR_LIST` empty and
  auto-address-list off**, reaching the server **only** via
  `EPICS_PVA_NAME_SERVERS`, and asserts the client bound **no** UDP
  socket;
* `cargo nextest run -p epics-pva-rs`;
* `cargo clippy -p epics-pva-rs --all-targets -- -D warnings`.

*Risk:* the `[u8]` cascade (§1.2) says inference in that `select!` is
already fragile. Expect the arm-shape change to surface real type errors
the cascade was hiding. That is the point.

### Stage 2 — `ServerConn::connect_blocking` (medium)

Add a third constructor beside `connect` (`server_conn.rs:219`) and
`connect_tls` (`:243`) that:

1. dials with `std::net::TcpStream::connect`;
2. wraps the stream in `Arc<TcpStream>` and drives it with **two**
   blocking threads through `impl Read/Write for &TcpStream` — **never**
   `try_clone` (§3.2);
3. hands `run_handshake_and_spawn` (`:285`) the same `DynRead`/`DynWrite`
   it takes today, built from the server's `ChannelReader`/`ChannelWriter`
   (`server_native/blocking.rs:607`, `:734`).

Promote those two adapters out of `server_native` into a module both
drivers use. Do the same for `ns_run_once`'s dial (`search_engine.rs:2812`)
so the name-server connection uses the same primitive — one seam, two
callers, not two seams.

*Size:* ~250 lines of new code, most of it the thread-lifecycle guards
the server driver already models (`ReaderGuard`, `ConnRegistry`
invariants, `blocking.rs:536-605`); ~80 lines of moves.

*Gate:*
* the whole `epics-pva-rs` client test suite passing **with the blocking
  constructor forced on**, on the host — the only way to show the frame
  pipeline is untouched (the server driver's own module doc makes this
  argument; reuse it);
* `./scripts/rtems-check.sh` with `epics-pva-rs`'s target feature
  selection extended to include `client` — **this is the stage that turns
  §1.2's "47 is a lower bound" into a number**;
* `cargo nextest run -p epics-pva-rs`.

*Risk:* the suppressed-error question. If `ops_v2.rs` is not in fact
transport-independent, this gate is where it shows, and the estimate
moves. Budget for that rather than treating a red gate here as a
surprise.

### Stage 3 — the spawn seam and the band-blocking invariant (small)

Convert the 12 production `tokio::spawn` in `client_native` (§4.1) and
the 6 in `pvalink` to `runtime::task::spawn`; delete
`PvaLinkResolver::handle` (`integration.rs:38`, `:158`, `:826`) and its
four `handle.spawn` calls. `TaskHandle::abort_handle()` keeps
`MonitorAbort` (`link.rs:284-291`) shape-identical.

Then close §2.3's invariant **by construction**, not by convention:

> **MUST NOT** any future spawned onto a callback band call
> `block_on_sync` / `park_on`.
> **Owner:** `runtime::background::future_exec::spawn_future`.

Candidate mechanism: a thread-local "on a callback band" marker set by
the band worker loop (`callback_executor.rs:302-311`), which
`block_on_sync` (`task.rs:114`) consults and refuses with a third
`NotBlockable`-style variant — the same shape it already uses to refuse a
current-thread runtime, extended to the one other context where parking
is unsound. That makes the illegal path *reported*, not *reviewed*.

*Gate:*
* a source-text guard in `client_native` mirroring
  `server_native/tcp.rs:8180-8191` ("production scope must spawn through
  `runtime::task::spawn`; found N bare `tokio::spawn(`");
* `cargo nextest run -p epics-pva-rs -p epics-bridge-rs --features rtems-exec-model`
  — the feature-ON suite is the only place the exec backend actually runs;
* a test that a future calling `block_on_sync` from a band worker is
  refused rather than deadlocking.

*Risk:* `rtems-exec-model` is currently declared on `epics-pva-rs` and
`epics-ca-rs` but **not** on `epics-bridge-rs` — that is
`doc/qsrv-rtems-design.md` §7 stage 4's job (the 250-site census). This
stage adds `pvalink`'s 68 `#[tokio::test]` sites to that bill
(`rg -c '#\[tokio::test' crates/epics-bridge-rs/src/pvalink/*.rs`:
`integration.rs` 38, `link.rs` 23, `registry.rs` 5, `iocsh.rs` 2). Land
that stage first or accept that this one is gated behind it.

### Stage 4 — mount pvalink in `rtems-pva-ioc` (small)

Select `pvalink` for the target: extend `CRATE_FEATURES[epics-bridge-rs]`
in `scripts/rtems-check.sh:87-89` from `qsrv-core` to
`qsrv-core,pvalink`, which pulls `epics-pva-rs/client` (`Cargo.toml:77`)
without `tls`/`pkcs12` — so no `ring`, no `getrandom 0.2`. Install the
resolver in `rtems-pva-ioc.rs` alongside the existing QSRV mount, and
replace the banner that currently says nothing about links.

*Gate:*
* `./scripts/rtems-check.sh` green in both configurations;
* `rtems-pva-ioc`'s existing four source-text guards, extended over the
  new code — in particular `entry_point_never_starts_a_runtime`;
* `cargo nextest run -p epics-bridge-rs -p epics-pva-rs`.

**Not sufficient.** This stage's real gate is stage 5.

### Stage 5 — two IOCs on the wire, one linking to the other (the actual gate)

Everything above is `cargo check` and host tests. "Type-checks for RTEMS"
and "runs on RTEMS" are different claims and this workspace has been bitten
by the gap twice (`scripts/rtems-check.sh:14-28`).

Box: the QEMU/BSP machine (`-M xilinx-zynq-a9 -m 256M`, `-nic
user,model=cadence_gem`; `doc/rtems-priority-on-target-measurement.md:20`).

**Topology A — guest links to a host IOC (the primary test).** SLIRP gives
the guest a route *out* to the host at `10.0.2.2` with no `hostfwd` and no
`tap`; the guest already DHCPs to `10.0.2.15`
(`doc/rtems-priority-on-target-measurement.md:176`). Run an upstream
PVA IOC on the host (`softIocPVX` from pvxs, or our own `qsrv-rs`) serving
one `ai`; boot `rtems-pva-ioc` with `EPICS_PVA_NAME_SERVERS=10.0.2.2:5075`
and a `.db` whose record carries `INP=@pva://UPSTREAM:AI CP`.

*Pass criteria, each stated as an observation on the console or on the
wire, because the target has no iocsh:*

1. Console banner reports the pvalink resolver installed and the link
   count, not silence.
2. `pvxget` from the host against the **guest's** downstream record
   returns the upstream's value (forwarded hostfwd `tcp::5075-:5075` —
   host port must equal guest port, `doc/rtems-scope-b-session-handoff.md`
   §5.5 trap 1).
3. `caput`/`pvxput` to the upstream changes the guest record within one
   scan period — proving the **monitor** path, not just a GET.
4. Kill the upstream: guest record goes `LINK`/`INVALID`. Restart it: the
   record recovers **without** rebooting the guest — proving the
   re-subscribe loop (`link.rs:335-430`) survives the 1 s clock quantum.
5. An OUT link (`OUT=@pva://UPSTREAM:AO`) writes and is observed
   upstream — proving `flush_puts` on the link-put-queue owner.
6. `rt stackuse` / `rt top` on the guest: thread count and per-thread
   peaks match §4.3's arithmetic to within one thread, and the connection
   count to the upstream is **1** regardless of link count (stage 0's
   property, on target).

**Topology B — guest links to guest.** Two SLIRP guests are on separate
`10.0.2.0/24` networks and cannot address each other. This needs a shared
`-netdev socket` hub or `tap`. Listed as a stretch, and its viability is
UNVERIFIED (§6).

*Gate:* all six of topology A's criteria, each with the command and its
output pasted into a measurement document, in the shape of
`doc/rtems-priority-on-target-measurement.md`. Anything short of that is
stage 4 with extra confidence.

---

## 6. Unverified — needs measurement

Everything here is a claim this document could **not** settle.

1. **`ops_v2.rs` / `context.rs` / `channel.rs` are transport-independent.**
   Established by source-text census only (§0, §1.2). rustc suppresses
   downstream errors after a poisoned import, so 47 is a lower bound and
   these four files reporting zero is not proof. Stage 2's gate settles it.
2. **pvalink's own RTEMS error surface is unmeasured, and unmeasurable
   today.** `cargo +nightly check … -p epics-bridge-rs --features
   qsrv-core,pvalink --target armv7-rtems-eabihf` stops at the dependency:
   *"error: could not compile `epics-pva-rs` (lib) due to 47 previous
   errors"*. The bridge is never reached. A stub-client probe was
   considered and rejected as **actively misleading**: `tokio::runtime::Handle`
   compiles for the target (§4.1), so such a probe would report pvalink
   green while `Handle::current()` panics at boot.
3. **Whether one `cbMedium` worker is enough.** §2.3 argues yes from the
   cooperative release-on-`Pending` design and from the callback body
   doing no blocking work. Unmeasured under load, and it composes with
   `doc/qsrv-rtems-design.md` §8 item 2 (a 20-member group already puts
   ~40 forwarder tasks on the same band).
4. **Outbound SLIRP from guest to `10.0.2.2`.** Stage 5 topology A depends
   on it. The measurements in this tree exercise *inbound* `hostfwd`
   (peers appear as `10.0.2.2:57688`,
   `doc/rtems-priority-on-target-measurement.md:49-52`); guest-initiated
   outbound TCP is standard SLIRP behaviour but is **not measured here**.
   Verify it with one `pvxget`-equivalent from the guest before building a
   stage on it.
5. **Topology B (guest↔guest).** Needs a shared netdev; untried.
6. **Whether the blocking client changes the per-connection memory
   ceiling.** §4.3 computes 512 KiB per client connection from stack
   classes. The server's equivalent arithmetic (1,572,864 B) matched the
   on-target measurement to 1 % (1,589,000 B), which is encouraging but is
   a different code path.
7. **`ChannelReader`/`ChannelWriter` promotion.** Assumed to be a
   visibility change (`pub(super)` → shared module). Whether they carry
   `server_native`-specific assumptions in their back-pressure accounting
   (`room`, `frame_tx.downgrade()`, `blocking.rs:938`, `:2267-2292`) is
   unread.
8. **The CA client's identical seam.** `epics-ca-rs/src/client/transport.rs:12`
   names `tokio::net::TcpStream` on one line, same as
   `server_conn.rs:33`. Whether the CA client is otherwise as
   transport-erased as the PVA client (which has `DynRead`/`DynWrite`
   already) was **not** measured. §3.3's "share the primitive" argument
   assumes it is at least close; that assumption is untested.
9. **Per-link timeouts under a shared client.** Stage 0's risk note. No
   config path was audited for one.
