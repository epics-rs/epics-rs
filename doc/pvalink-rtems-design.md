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
| `main` (POSIX_Init) † | `rtems_config.c:35` | — | 65,536 |
| `status-pv` † | `status_pv.rs:291-294` | Small | 262,144 |
| per inbound conn: `PVAS-conn` | `blocking.rs:1266-1269` | Big | 1,048,576 |
| per inbound conn: `PVAS-read`, `PVAS-write` | `blocking.rs:501-505` | Small ×2 | 524,288 |

† Both rows were **added by stage 5's census** (§12.6): they were absent
from this table, so "count the threads and compare against §4.3" was off
by two before a single `pva://` link existed. `main` is not a
`StackSizeClass` thread at all — it is `CONFIGURE_POSIX_INIT_THREAD_STACK_SIZE`,
and it is the deepest user of its own stack of any thread on the box
(21,912 of 65,520 B used, 33 %).

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

### Stage 0 — one client for the whole IOC (small, host-only) — **DONE** (see §7)

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

### Stage 1 — a search engine that runs with no UDP socket (medium, host-testable) — **DONE** (see §8)

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

### Stage 2 — `ServerConn::connect_blocking` (medium) — **DONE** (see §9)

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

### Stage 3 — the spawn seam and the band-blocking invariant (small) — **DONE** (see §10)

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

### Stage 4 — mount pvalink in `rtems-pva-ioc` (small) — **DONE** (see §11)

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

### Stage 5 — two IOCs on the wire, one linking to the other (the actual gate) — **DONE**

*Status: run on the box on 2026-07-23; all six topology-A criteria pass.
The measurements, the four defects it found, and where reality deviated
from this section are §12. Two corrections to the text below, both
recorded there: the topology-A paragraph spelled the record links
`@pva://…`, which does not load (§12.2 — `@` is the INST_IO sigil), and
the §4.3 thread table it points at was missing two baseline threads
(§12.6). The link spellings are corrected in place below; §12.2 keeps the
rejected form, because naming it is that section's job.*

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
and a `.db` whose record carries `INP=pva://UPSTREAM:AI CP`.

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
5. An OUT link (`OUT=pva://UPSTREAM:AO`) writes and is observed
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
   **PARTLY SETTLED by stage 2 — §9.7.** The count is now **28**, all of
   them UDP. ~~`ops_v2.rs`~~ is settled: it is at zero, it names nothing
   from `search_engine`/`udp`, and `server_conn` — the one poisoned module
   it depended on — now compiles for the target, so nothing is left to
   suppress it. `context.rs` and `channel.rs` are **still open**: both are
   at zero too, but both name `search_engine::SearchEngine`, which the 7
   remaining UDP errors still poison. The UDP stage settles those two.
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
   **MEASURED — the wire works, and (CORRECTED) so does the client.**
   Guest-initiated outbound TCP flows: the target emits well-formed
   SYNs (checksums verified correct in the capture) and the stage-5 NS
   retry dials every ~10 s. The first capture read the abort after the
   peer's SYN-ACK as a client-side defect ("`connect_timeout` does not
   honor its bound on RTEMS") and the ns-dial round moved the dial to
   a plain blocking `connect` on a dedicated thread — after which the
   identical failure signature reproduced, which broke the attribution.
   The RST was never the guest's: re-reading the captures with
   ethernet headers shows every mid-handshake RST carries the SLIRP
   router's source MAC `52:55:0a:00:02:02`, not the client NIC's
   `52:54:00:12:34:57`. On a QEMU hub every frame floods to the SLIRP
   hub port, libslirp processes frames regardless of destination MAC,
   and a TCP segment belonging to no SLIRP flow is answered with a
   forged RST impersonating the flow's endpoint — the peer's SYN-ACK
   gets a forged "client" RST ~60 µs later, killing the server-side
   connection while the real client completes its handshake. With the
   SLIRP hub port link-downed after both DHCP leases (`set_link n1
   off` on the QEMU monitor), the *same* target image dials out and
   connects: `DIAL-PROBE 10.0.2.15:5075 OK local=Ok(10.0.2.16:21301)`,
   and the item-5 E2E passes end-to-end. The "`connect_timeout` is
   broken on RTEMS" claim is therefore **withdrawn as a rig artifact**
   — it was never measured outside the poisoned hub, and remains
   simply unmeasured. The ns-dial refactor stands on its other two
   legs (band isolation, CA/C `tcpiiu.cpp` blocking-connect parity),
   not on target brokenness.
5. ~~**Topology B (guest↔guest).** Needs a shared netdev; untried.~~
   **PASSED end-to-end (2026-07-23, QEMU/BSP box).** Mechanics: guest 1
   joins its own SLIRP and a `-netdev socket,listen=` to a QEMU hub
   (modern spelling: `-netdev hubport,id=…,hubid=0,netdev=…` per
   member, NIC attached with `-nic hubport,hubid=0,model=cadence_gem`,
   distinct `mac=` per guest — mandatory, both guests otherwise
   default to the same MAC); guest 2 attaches directly with
   `-nic socket,connect=`. Both guests lease from guest 1's SLIRP DHCP
   (10.0.2.15 / 10.0.2.16). **The hub-resident SLIRP port must then be
   cut** — `set_link n1 off` on guest 1's QEMU monitor once both
   leases are bound — or libslirp forges RSTs into every guest↔guest
   TCP flow and no connection survives its handshake (item 4; rig
   runner: `~/rtems-bringup/topoB/run-probe.sh`). With the cut in
   place the full pvalink chain works on target: downstream STAGE5
   report reaches `connections=1`, both links `connected=true`, and
   `RTEMS:PVA:DOWN`/`DOWN2` track the upstream guest's 10 Hz
   `RTEMS:PVA:V0` live (VAL 240 → 746 across the observation window,
   SEVR 0; conn `rx=50037` bytes in ~70 s of CP monitor flow).
   `RTEMS:PVA:UPLNK` stays SEVR 3/STAT 17 — expected, the OUT link is
   on a Passive supervisory record nothing processes. Two rig caveats,
   measured on this QEMU: SLIRP **inbound `hostfwd` does not work
   behind a hubport** — the host side accepts but SLIRP never opens
   the guest-side connection (no SYN toward the guest in the capture),
   with both the legacy `-net` spelling and the explicit-guest-address
   form — and after the `set_link` cut SLIRP is unreachable entirely,
   so host-side observation of a hubbed guest goes through a peer
   guest or the serial console; keep `-nic user` (no hub) for any run
   that needs `hostfwd`. Downstream image recipe for the E2E:
   patch `STAGE5_NAME_SERVER` to `10.0.2.15:5075` and point the three
   stage-5 links at PVs the upstream guest serves (`RTEMS:PVA:V0` for
   the two INP CP links, `RTEMS:PVA:B00` for the OUT link).
6. **Whether the blocking client changes the per-connection memory
   ceiling.** §4.3 computes 512 KiB per client connection from stack
   classes. The server's equivalent arithmetic (1,572,864 B) matched the
   on-target measurement to 1 % (1,589,000 B), which is encouraging but is
   a different code path.
7. **`ChannelReader`/`ChannelWriter` promotion.** Assumed to be a
   visibility change (`pub(super)` → shared module). Whether they carry
   `server_native`-specific assumptions in their back-pressure accounting
   (`room`, `frame_tx.downgrade()`, `blocking.rs:938`, `:2267-2292`) is
   unread. **SETTLED by stage 2 — §9.1.** They did carry one, and it was
   not in the back-pressure accounting: both pumps took a registry-issued
   `ConnWake`, i.e. the server's *authority to end a connection*. Both now
   derive it from the `Arc<TcpStream>` each already held. The destination
   was also wrong — `epics-base-rs`, not a module inside `epics-pva-rs`,
   or `epics-ca-rs` could not have called it.
8. **The CA client's identical seam.** `epics-ca-rs/src/client/transport.rs:12`
   names `tokio::net::TcpStream` on one line, same as
   `server_conn.rs:33`. Whether the CA client is otherwise as
   transport-erased as the PVA client (which has `DynRead`/`DynWrite`
   already) was **not** measured. §3.3's "share the primitive" argument
   assumes it is at least close; that assumption is untested.
9. ~~**Per-link timeouts under a shared client.** Stage 0's risk note. No
   config path was audited for one.~~ **SETTLED by stage 0 — §7.2.** The
   audit found no per-link timeout path, in this tree or in pvxs.

---

## 7. Stage 0 as built — where reality deviated from §5

Stage 0 landed as `aac14a1b` (the hoist) / `1bc49a6d` (the gate) on top of
`8c305c37`. The change itself matched §5; three of its statements did not
survive contact.

### 7.1 The owner is `PvaLinkRegistry` alone, not "`PvaLinkResolver`/`PvaLinkRegistry`"

§5 named both. Only one can own the client without re-opening the dual
meaning the stage exists to close: the registry is what *constructs*
links (`registry.rs` `get_or_open` → `PvaLink::open`), so the client has
to reach `open` from there. `PvaLinkResolver` owns exactly one
`PvaLinkRegistry` and needed no change at all — `PvaLinkResolver::new`
is untouched, and so is every production caller including
`install_pvalink_resolver`.

Consequently the predicted `integration.rs` edits did not happen: the
change is `link.rs` (signature + the deleted builder), `registry.rs`
(the `client` field, `new`/`with_client`, the one call site) and a
`mod.rs` doc example. §5's "~40 lines across `link.rs`, `registry.rs`,
`integration.rs`" was right on size, wrong on the third file.

### 7.2 The risk is closed: there is no per-link timeout path

§5 asked for this check *before*, not after. Result, stated either way as
required:

* `PvaLinkConfig` (`config.rs:139-219`) has **no** timeout member, and no
  `timeout` token appears anywhere in the option parser — neither in the
  `?key=value` query form nor in the pvxs-parity JSON longhand.
* pvxs's `pvaLinkConfig` has no timeout option either; the only
  `timeout` strings under `pvxs/ioc/pvalink*` are `testAbort` messages in
  the test harness (`pvalink.cpp:141`, `:178`).
* The per-link `Duration::from_secs(5)` was **already** the
  `PvaClientBuilder::new()` default (`client_native/context.rs:161`), so
  sharing changes no operation's deadline. It is now
  `PVALINK_CLIENT_TIMEOUT` in `registry.rs`, written explicitly rather
  than inherited, so a later change to the client-library default cannot
  silently move pvalink's.

Nothing had to move to the per-operation call. If a per-link timeout is
ever added it still must, and the constant's doc comment says so.

### 7.3 `share_udp` is moot for pvalink, and the single client is not yet pvxs's

§2.4 flagged that pvalink never calls `share_udp(true)`. With one client
per IOC there is nothing left to share *within* pvalink: the one client
lazily spawns one `SearchEngine` (`context.rs:379-380`) and every link
resolves through it. `share_udp` now only matters for an IOC that also
runs a second client — a `pva_gateway` upstream, say — which is a
different question from this stage's.

What the single client does **not** yet match is pvxs's *provenance*.
pvxs builds `linkGlobal->provider_remote` from
`ioc::server().clientConfig()` (`pvalink.cpp:60`) — the IOC server's own
address list, ports and TLS. `PvaLinkRegistry::new()` builds from bare
`PvaClient::builder()`, i.e. the process environment, which is what every
link did before and so is not a regression. `with_client` is the seam
that closes it; wiring the IOC server's config into it is left out of
stage 0 deliberately, because it changes discovery behaviour and belongs
with the stage 4 mount.

### 7.4 The `#[cfg(test)]` doubles still build clients, and that is deliberate

The anchor sweep (`PvaClient::builder`) found two surviving construction
sites inside `link.rs`: `for_test` (`:1501`) and
`for_test_with_monitor_flag` (`:1560`). They are not part of the defect
family and were left alone:

* they are `#[cfg(test)]`, so no production path can reach them — the
  invariant "a link never builds a client" holds by compilation for
  every shipped build;
* their links deliberately face **no server**. Several tests drive real
  `write`/`flush_scratch` calls through them and assert on the resulting
  disconnect, which needs a short (1 s) timeout and an isolated channel
  cache. Folding them onto one shared test client would couple those
  tests through that cache for no resource saving — nothing connects.

`for_test_with_client` (`:1531`) is unchanged and remains the
inject-a-live-client double.

### 7.5 One gate could not run: `tests/pva_gateway.rs`

§5 lists it as a stage 0 gate ("both drive the client"). It is
`#![cfg(feature = "pva-gateway")]`, which is **not** in the crate's
default feature set, so `cargo nextest run --workspace` compiles and runs
**zero** tests from it — it contributed nothing to this stage's evidence.
It also cannot be enabled today: `cargo check -p epics-bridge-rs
--features pva-gateway` fails with five pre-existing
`cannot find type MonitorStream` errors in `src/pva_gateway/control.rs`
and `src/pva_gateway/middleware.rs`, files stage 0 does not touch. That
break is unrelated to this stage and is left as-is.

`tests/pvalink_seam.rs` — the other named gate — does run, and passes.

### 7.6 The gate as measured

`server.report().peer_count` stands in for §5's `active_connections()`:
the two are the same quantity under two names, the latter belonging to
the *blocking* server (`server_native/blocking.rs:1156`), which the test
IOC is not.

The gate is proved sensitive, not merely green: the same assertion with
each link on its own registry (hence its own client — the pre-fix shape)
reports `left: 2, right: 1`.

| gate | result |
|---|---|
| `stage0_distinct_link_variants_share_one_upstream_connection` | pass (`peer_count == 1`); pre-fix shape gives 2 |
| `cargo nextest run -p epics-bridge-rs` | 684 passed, 0 failed |
| `cargo nextest run --workspace` | 10104 passed, 0 failed, 2 skipped |
| `cargo clippy -p epics-bridge-rs --all-targets -- -D warnings` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --doc -p epics-bridge-rs` | 0 run, 3 ignored (all `ignore`d examples) |
| `tests/pva_gateway.rs` | **did not run** — see §7.5 |

---

## 8. Stage 1 as built — where reality deviated from §5

Stage 1 landed as `1d9170b3` on top of `e9dda2b6`. The sum type matched §5;
four of §5's statements did not survive contact, and one pre-existing
C-parity defect surfaced and was fixed at source.

### 8.1 The variant holds a bundle, not a socket — and it holds the beacons too

§5 wrote the type as `SearchTransport::{Udp(AsyncUdpV4, …), NameServersOnly}`.
What the `…` had to absorb is larger than "the search socket plus a bit":

```
SearchTransport::Udp(Box<UdpTransport>)
    search_socket        AsyncUdpV4          beacon_socket      Option<AsyncUdpV4>
    search_socket_v6     Option<Arc<..>>     beacon_socket_v6   Option<Arc<..>>
    extra_targets        Vec<SocketAddr>     client_interfaces  Vec<Ipv4Addr>
    broadcast_port       u16                 auto_addr_list     bool
    response_port        u16                 response_port_v6   u16
SearchTransport::NameServersOnly            (no fields)
```

The test is whether a field can outlive the socket it describes. Every one
of these fails it: `extra_targets` and `client_interfaces` are UDP SEARCH
destinations and UDP egress constraints, `broadcast_port`/`auto_addr_list`
are the knobs that compute those destinations, and the two response ports
are `local_addr()` read back from the sockets themselves. Leaving any of
them beside the variant would have reproduced, one level up, exactly the
"present but unusable" split the sum type exists to close.

The **beacon** sockets are the consequential inclusion, and it is what
makes the gate's "bound **no** UDP socket" literally true rather than
"bound no UDP *search* socket". They are not dead in a name-servers-only
configuration the way the search sockets provably are: with
`auto_addr_list` off and an empty `addr_list`, `search_targets()` returns
an empty list (`search_targets_empty_when_auto_off_and_no_extras`), so a
UDP SEARCH is never transmitted and no reply can arrive — but a beacon
listener on `broadcast_port` would still receive. Folding them in
therefore **costs** beacon-driven fast reconnect (`poke`) and `discover()`
events. That is a real semantic loss, and it is why the choice is
**explicit** rather than derived (§8.2).

`Box` on the payload variant: without it `SearchTransport` is as large as
`UdpTransport` everywhere, including in the `NameServersOnly` case that
carries nothing (`clippy::large_enum_variant`).

### 8.2 The selection is an explicit entry point, not derived from config

§5's gate reads as though "`addr_list` empty + auto-address-list off +
name servers present" should *derive* `NameServersOnly`. It was not
wired that way, and deliberately.

Deriving it would silently drop the beacon sockets (§8.1) for every host
client that already runs in that configuration today — losing
beacon-driven fast reconnect for users who never asked for it. So the
selection is `SearchEngine::spawn_name_servers_only(…)`, a fourth entry
point beside `spawn` / `spawn_with_config` / `spawn_with_auth`, whose
signatures are unchanged. `ClientSearchConfig` gained no field, so no
public struct-literal breaks.

Consequences worth stating plainly:

* **No production caller selects it yet.** Stage 1 builds the capability;
  the RTEMS mount (stage 4) is what will choose it. That is the staging
  §5 asked for, not an oversight.
* **`PvaClient` cannot select it.** There is no `PvaClientBuilder` knob —
  that would be a `context.rs` change, and §5 scoped stage 1 to
  `search_engine.rs` and no other file. The gate therefore drives
  `SearchEngine` directly, which is where PV *resolution* lives.
* If the main worker wants auto-derivation instead, the beacon question
  above is the decision to make first — it is a behaviour change, not a
  refactor.

### 8.3 The `[u8]` cascade did not surface anything — and §1.2 predicts that

§5's stated risk: "Expect the arm-shape change to surface real type errors
the cascade was hiding. That is the point." It surfaced **none**. The host
build was clean on the first compile after the arm conversion, and stayed
clean through `--all-targets`.

This is not luck, and §1.2 already contains the reason: the 18 `E0277`s
are "inference fallout inside `run_engine`'s `select!` after
`use tokio::net::{TcpStream, UdpSocket}` failed" — i.e. artefacts of a
*target-only* poisoned import. On the host those imports resolve, so
there was never a hidden host error for the change to expose. The risk was
correctly identified as real for the target and is still unretired: it
will be re-measured when the RTEMS arm actually compiles this file
(stage 2+), not here.

### 8.4 A pvxs clause was missing, and stage 1 walked straight into it

`spawn_inner` warned "no search destinations … All searches will time
out" on `extra_targets.is_empty() && addr_list.is_empty() && !auto_on`.
pvxs gates the same warning on `searchDest.empty() && **nameServers.empty()**`
(`client.cpp:633`) — both clauses.

Our port carried only the first. A client with TCP name servers, an empty
`addr_list` and `AUTO_ADDR_LIST=NO` was therefore told every search would
time out while it was about to resolve every one of them over TCP — and
that is precisely the stage-1 configuration, so the defect would have
been emitted by the new path on every spawn. Fixed at source rather than
worked around: the warning now lives in `spawn_inner` where both the
transport and the name-server list are in scope, and fires only when
neither can reach anything.

One deliberate behavioural edge came with it: the condition is now
evaluated *after* the `addr_list` → `extra_targets` merge rather than
before. The two differ only when the address list held IPv6 entries on a
host with no v6 socket — those are dropped (with their own warning), and
the client genuinely has no destination left, so the warning now fires
where it previously stayed silent. That is the more correct answer.

### 8.5 Deviation from pvxs, recorded

pvxs always binds its search sockets (`client.cpp:578-590`) and a beacon
listener per interface (`:638-650`), in every configuration.
`NameServersOnly` binds nothing. This is an intentional deviation for the
RTEMS target (§4.2) and is documented on
`SearchEngine::spawn_name_servers_only`, including the cost (no beacons,
hence no beacon-driven fast reconnect and no `discover()`). No host
default changes: every existing entry point still takes the UDP path.

### 8.6 Collateral simplification

Folding the destination policy into the transport removed two types and
two free functions rather than adding to them:

| removed | absorbed by |
|---|---|
| `struct UdpSearchParams` | `UdpTransport` fields |
| `struct SearchDestinations<'a>` | `UdpTransport` fields |
| `async fn broadcast(socket, socket_v6, …, dests, errs)` | `UdpTransport::broadcast(&self, …)` |
| `async fn recv_from_v6_opt(…) -> Option<Result<…>>` | `recv_from_v6(…) -> Result<…>` (parks instead of yielding `None`) |

`run_engine` lost 6 parameters (13 → 7) and `flush_initial_searches` 4
(10 → 6); both dropped their `#[allow(clippy::too_many_arguments)]`. The
two v6 `select!` arms lost their `if let Some(Ok(…))` double-unwrap,
because a parked arm never yields at all — the same shape the four UDP
arms now share.

Net `search_engine.rs`: +590 / −361. §5 estimated "~120 lines"; the
larger figure is the two struct definitions, their doc comments, the six
transport methods and the gate test, not extra machinery.

### 8.7 The census, and the two pre-existing RTEMS-gate warnings

`search_engine.rs` carries `RTEMS-EXEC-MODEL-ALLOW(N)`; the new
`#[tokio::test]` took it from 16 to 17. The bump is not self-certifying —
`tests/rtems_exec_model_gate.rs` runs *inside* the feature-ON suite — so
it was verified there: `cargo nextest run -p epics-pva-rs --features
rtems-exec-model` gives 1382 passed, with both
`stage1_name_servers_only_resolves_without_binding_udp` and
`every_reactor_dependent_test_is_accounted_for` passing.

`./scripts/rtems-check.sh` stays exit 0. Its two known `epics-pva-rs`
warnings are unchanged in count and identity — `tcp.rs:1407` (deprecated
`fetch_update`) and `server_native/search_engine.rs:501` (dead `Origin`
variants). Both live in `server_native/`, which this stage does not
touch; neither was silently fixed nor worsened. Note that §5's brief
cited the `Origin` warning as `search_engine.rs:501` without a directory
— it is the **server**-side file of that name, not the client one this
stage rewrote.

### 8.8 The gate as measured

The "no UDP socket" assertion is made twice on purpose: structurally
(`NameServersOnly` is a fieldless variant, so no socket can be held) and
at runtime (`bound_udp_addrs()` must be empty). The runtime half is
guarded against being a tautology — the same test first builds a UDP
transport from the *same* environment and asserts it *does* bind, so an
empty list is a property of the variant and not of the test's config.

| gate | result |
|---|---|
| `stage1_name_servers_only_resolves_without_binding_udp` | pass — resolves via TCP NS, binds 0 UDP sockets |
| `cargo nextest run -p epics-pva-rs` | 1389 passed, 0 failed, 2 skipped |
| `cargo nextest run -p epics-pva-rs --features rtems-exec-model` | 1382 passed, 0 failed, 2 skipped |
| `cargo clippy -p epics-pva-rs --all-targets -- -D warnings` | clean |
| `cargo nextest run --workspace` | 10105 passed, 0 failed, 2 skipped |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --doc -p epics-pva-rs` | 1 passed, 15 ignored |
| `./scripts/rtems-check.sh` | exit 0, 2 pre-existing warnings unchanged |

---

## 9. Stage 2 as built — where reality deviated from §5

Stage 2 landed as `8024b175` (the byte source promoted into
`epics-base-rs`) plus seven commits: `5d19cd7f`, `9dc71482`, `e3fddcc2`,
`d2f21489`, `3ae270ae`, `2c5155c6`, `573166ce`.

### 9.1 The primitive went to `epics-base-rs`, not "a module both drivers use"

§5 said *"promote those two adapters out of `server_native` into a module
both drivers use"*, which reads as a move within `epics-pva-rs`. Measured,
that destination is wrong: `epics-ca-rs` does not depend on
`epics-pva-rs` and must not — the only crate depending on both is
`epics-bridge-rs`, which sits *above* them. A primitive promoted inside
`epics-pva-rs` is one the CA client structurally cannot call, so the next
CA increment writes a third copy, which is what "one seam, two callers"
exists to prevent. `doc/calink-rtems-design.md` §3.3 measured this and
names `epics-base-rs`; `runtime::blocking_io` is where it landed.

What was entangled with the server turned out to be the *authority to end
a connection*, not the plumbing: both pumps took a registry-issued
`ConnWake`. Both now derive it from the `Arc<TcpStream>` each already
held, so they retire themselves with no registry type, and `ConnRegistry`
keeps exactly the authority it had — the server-wide stop.

### 9.2 The seam is `dial_pva`, and the third constructor is not how RTEMS reaches it

§5 describes one new constructor. As built there are two entry points
with different jobs, because "one seam, two callers" and "a third
constructor" are different requirements:

* `dial_pva` — the client's **one** TCP dial. `ServerConn::connect` and
  `ns_run_once` both come through it, and it selects the transport at
  compile time (`target_os = "rtems"` or `--cfg pva_blocking_client`).
  This is what actually puts the client on the target.
* `ServerConn::connect_blocking` — the third constructor beside `connect`
  and `connect_tls`, always blocking regardless of `cfg`.

Keeping both is deliberate. The forced-on suite rebuilds the crate with
`dial_pva` selecting the blocking transport, so what it exercises is
`connect`; nothing in it ever calls `connect_blocking`, and a `--cfg` no
manifest can set is not something a default build can be argued from. So
the constructor carries its own test on an ordinary host build
(`connect_blocking_completes_the_same_handshake_as_connect`), asserting
that the two transports are interchangeable **at runtime** rather than
one replacing the other at build time.

### 9.3 `spawn_dedicated_thread` had to be fixed first, and it is a real defect

Not anticipated by §5, and it blocked the stage's own gate. Under
`--cfg pva_blocking_client` the suite failed 10 tests, all reporting
*"server closed during handshake"*.

`spawn_dedicated_thread` entered whatever ambient tokio handle the caller
was running under. `block_on_sync` read that handle back through
`Handle::try_current()`, saw a `CurrentThread` flavor, and returned
`Err(NotBlockable)` — so the reader pump's first
`block_on_sync(tx.send(..))` failed, the pump broke out of its loop, and
the closed channel read as EOF. `#[tokio::test]` is `CurrentThread` by
default, so this was every test that drove a pump.

The dual meaning is in `try_current()`: it answers both *"am I running on
this runtime's thread"* and *"has this thread merely entered this handle"*
with one value. `block_on_sync` cannot distinguish them, so it must assume
the first and refuse — correct on the runtime's own thread, where parking
halts the task that would wake you, and wrong on a dedicated thread, where
it halts nothing.

Fixed at the one place a dedicated thread's context is decided, rather
than by teaching `block_on_sync` to guess: a `CurrentThread` ambient is
not inherited. Nothing is lost, because what inheriting buys —
`spawn` and the timer *inside* `block_on_sync` — is unreachable when
`block_on_sync` never returns `Ok`. Inheriting could only convert a
thread that would have worked into one that cannot block.

Boundary coverage was two of three: multi-thread ambient and no ambient
each had a test; the case between them had none. It has one now. This is
**not** a test-only fix — a hosted caller of `connect_blocking` on a
current-thread runtime hit it in production shape.

### 9.4 Two bounds, because one duration cannot do both jobs

The blocking transport needs deadlines the hosted one does not, and the
first cut gave both to `op_timeout`. That is wrong in the direction that
looks harmless:

* **`SO_RCVTIMEO`.** `reader_pump` ends the connection when its receive
  timeout expires, so a finite value there is an *idle-disconnect bound*.
  An idle PVA circuit is supposed to be silent between echoes (15 s), with
  `tcp_timeout` (40 s) the only thing entitled to call it dead — so
  `op_timeout` made every circuit quieter than one operation
  self-destruct. It keeps `PumpConfig`'s effectively-infinite default;
  what ends a parked reader is `ReaderPumpGuard`'s `shutdown`, driven by
  the cancellation token the heartbeat already fires. The regression test
  is the 2.5 s idle assertion in
  `connect_blocking_completes_the_same_handshake_as_connect`.
* **The one-whole-frame write deadline.** This one has no hosted
  counterpart at all — only on the blocking side is a write a parked
  thread something has to be entitled to reclaim
  (`blocking_io::write_frame_deadline`). `connect` passes `tcp_timeout`,
  the connection's own liveness bound, so the pump can never end a circuit
  the protocol would still consider alive. **Recorded as a deviation:** a
  frame that cannot reach the wire within `tcp_timeout` ends a blocking
  connection and would not end a hosted one. It is not removable — a
  blocking write with no deadline holds its thread forever — so the choice
  is only *which* bound, and any value is a deviation.

`ns_run_once` passes `handshake_timeout` for both, because it is the only
bound an NS circuit is configured with: no `ConnConfig`, no heartbeat, no
`tcp_timeout`. That also adds a connect deadline the bare
`TcpStream::connect(ns_addr).await` did not have — an unreachable name
server used to park that future on the OS's connect timeout.

### 9.5 The forcing mechanism

`--cfg pva_blocking_client`, declared in `epics-pva-rs/build.rs`
(`cargo::rustc-check-cfg`) and emitted by nobody:

```
RUSTFLAGS="--cfg pva_blocking_client" cargo nextest run -p epics-pva-rs
```

A cargo feature was the obvious alternative and is the wrong tool:
features unify across the graph, so any crate in a workspace build
enabling it would silently move every other crate's PVA client onto the
blocking transport. A runtime env var would ship the switch in release
binaries, where an operator setting it changes the transport of a
production IOC. A `--cfg` no manifest can turn on reaches neither — it
exists only for a build someone typed the flag for, the same mechanism
`scripts/rtems-check.sh` uses for `rtems_boot_linked`.

### 9.6 The gate is a ratchet, because the literal reading is vacuous

§5 asks for `rtems-check.sh` with this crate's target selection *extended
to include `client`*. Taken literally — adding `client` to
`CRATE_FEATURES` — the whole gate goes red, because every remaining
client error is UDP and §4.2 stages that work **after** this one,
deliberately. Stages 3–5 all extend the green gate, so turning it red for
work nobody has started reports nothing.

The count is the artefact, so the selection is measured rather than built,
and pinned (`PVA_CLIENT_TARGET_ERRORS`) so it cannot drift unobserved in
either direction: more is the regression; fewer is someone doing the work
and not lowering the number, fatal for the same reason the binary census
is bidirectional — a bound nobody updates stops being a measurement.

### 9.7 §1.2's "47 is a lower bound", settled

| point | errors |
|---|---:|
| before stage 1 | **47** (29 primary + an 18-error `[u8]` cascade) |
| after stages 1 and 2 | **29** (cascade gone with `search_engine`'s `TcpStream`) |
| after `server_conn`'s unconditional `tokio::net` import was removed | **28** |

All 28 are UDP — `udp.rs` 20, `search_engine.rs` 7, `search.rs` 1 —
newlib/`libc` gaps, `socket2`, `if-addrs` and `AsyncUdpV4`.

The last step is worth stating on its own. `use tokio::net::TcpStream`
sat at module scope in `server_conn.rs` while both remaining uses were
already inside `cfg` blocks the target does not compile. An import is
resolved whether or not anything reaches the item, so that one line was
an E0432 poisoning the whole module — and rustc suppresses downstream
errors in code naming a poisoned module's items, which is exactly why 47
had to be reported as a lower bound.

**§5's named risk did not materialise.** `ops_v2.rs` is at zero, it names
nothing from `search_engine`/`udp`, and `server_conn` — the one poisoned
module it depended on — is now clean, so that zero is a real result.
**§6 item 1 is settled for `ops_v2.rs`.** It is *not* settled for
`channel.rs` and `context.rs`: both are also at zero, but both still name
`search_engine::SearchEngine`, so their zeros stay suppressed until the
UDP stage lands.

### 9.8 A stale census marker from step 1

`8024b175` moved the pump, guard and deadline-loop tests out of
`server_native/blocking.rs` into `runtime::blocking_io` but left that
file's `RTEMS-EXEC-MODEL-ALLOW` at 18 against 16 actual sites, so
`every_reactor_dependent_test_is_accounted_for` had been failing
feature-ON since that commit. Corrected to 16. The marker's whole purpose
is that it is *not* self-certifying, so a stale one is worse than a
missing one.

### 9.9 The two pre-existing RTEMS-gate warnings

Unchanged in count and identity: `server_native/tcp.rs:1407` (deprecated
`fetch_update`) and `server_native/search_engine.rs:501` (dead `Origin`
variants). Both are in `server_native/`; neither was silently fixed nor
worsened. As §8.7 notes, the second is the **server**-side file of that
name, not the client one this stage changed.

### 9.10 The gate as measured

| gate | result |
|---|---|
| `epics-pva-rs` suite, blocking transport **forced on** (`--cfg pva_blocking_client`) | **1380 passed, 0 failed, 2 skipped** |
| `connect_blocking_completes_the_same_handshake_as_connect` (default build) | pass — verified by mutation: fails at 2.5 s if the reader pump is given a finite receive timeout |
| `a_dedicated_thread_can_block_under_a_current_thread_ambient` | pass — verified by mutation: `None != Some(9)` without the fix |
| `cargo nextest run -p epics-pva-rs -p epics-base-rs` | 4900 passed, 0 failed, 2 skipped |
| `cargo nextest run -p epics-pva-rs --features rtems-exec-model` | 1374 passed, 0 failed, 2 skipped |
| `cargo nextest run -p epics-base-rs --features rtems-exec-model` | 3520 passed, 0 failed |
| `cargo clippy -p epics-pva-rs -p epics-base-rs --all-targets -- -D warnings` | clean, in both the default and the forced-on configuration |
| `./scripts/rtems-check.sh` | exit 0; client probe 28, 2 pre-existing warnings unchanged |

## 10. Stage 3 as built — where reality deviated from §5

### 10.1 The invariant closure landed at the facility loop, not the band loop

§5's candidate put the "on a callback band" marker at
`callback_executor.rs:302-311`. It went one level down instead, into
`runtime::background::facility::run_facility_loop` — the single function
*every* facility worker's loop funnels through, not just the callback
bands but the timer thread and the `scanOnce` worker too. That is the
correct owner for the invariant as stated (§2.3: "no future spawned onto a
callback band"), and strictly wider: parking is unsound on any of those
single-worker loops, not only the priority bands. The loop was already
pinned by `no_facility_propagates_a_poisoned_lock`, so the marker sits
behind an existing guard rather than a new one.

Mechanically: a private thread-local `ON_FACILITY_THREAD` set by an RAII
`FacilityThreadMark` as the loop's first act; `on_facility_thread()` reads
it; `block_on_sync` (`task.rs`) consults it *first* and returns
`Err(NotBlockable::BackgroundWorker)`. `NotBlockable` went from a struct to
a two-variant enum (`CurrentThreadRuntime`, `BackgroundWorker`) — the §5
"third variant" framing, realised as an enum because the type carried no
variants before. `server/scan.rs`'s `unreachable!` was widened to cover
both. The invariant is now *reported* (a typed refusal), not *reviewed*.

> **Invariant.** MUST NOT: a future running on any facility worker thread
> call `block_on_sync` / `park_on` (it would deadlock the single-worker
> loop).
> **Owner/Gate.** `block_on_sync`, guarded by
> `facility::on_facility_thread()`; the mark is owned solely by
> `run_facility_loop`.
> **Bypass audit.** `rg 'block_on_sync|park_on'` — every caller
> (`scan.rs`, `blocking.rs` pumps, `audit.rs`, `status_pv.rs`, the
> `rtems-*-ioc` bins, `blocking_io` pumps) is either on a non-facility
> thread or already routes through `block_on_sync`; none construct a
> facility-thread park directly.
> **Structural closure.** The mark is private to the facility module and
> set by exactly one function, so a future added to any worker loop
> inherits the refusal by construction — no per-call-site discipline.
> **Tests.** `a_future_on_a_callback_band_is_refused_a_blocking_bridge`,
> `a_blocking_closure_on_a_callback_band_is_refused_too`, and
> `a_thread_that_only_submits_to_a_band_still_blocks` — each written so a
> broken gate *fails* rather than hangs (the awaited future is completable
> from the test thread and released before the assertion). Mutation-checked.

### 10.2 The unforeseen fork: exec backend cannot host the tokio-net transport

§5 read the spawn conversion as mechanical and expected the feature-ON
suite to stay green. It does not, and the reason is structural: converting
the `client_native` connection spawns to the seam moves their
`tokio::net` sockets and `tokio::time` timers onto the reactor-less
callback pool. The host feature-ON tests drive the **hosted** transport
(`tokio::net::TcpStream`), which panics "there is no reactor running" on a
band worker — whereas the RTEMS target uses the **blocking** transport
(`pva_blocking_client`, stage 2) that needs no reactor. So exec-backend +
hosted-transport is a combination that exists only in the host test suite,
never on target.

The forcing experiment confirmed it is not a blanket-fixable timing issue:
`--cfg pva_blocking_client` with `--features rtems-exec-model` still fails,
because `search_engine`'s own `tokio::time` (the UDP-search half §4.2
defers) runs on the pool regardless of transport.

> **Superseded — the combination is no longer constructible.** This section's
> conclusion, that exec-backend + hosted-transport "exists only in the host
> test suite", was the diagnosis stopping one step short: the combination
> existed because the transport was selected by `target_os`, not by the
> backend. Both selections — the UDP SEARCH transport here and the TCP dial in
> `client_native/server_conn.rs` — now take `cfg(tokio_backend)`, emitted by
> `build.rs` as the negation of `exec_backend` (`target_os = "rtems"` **or**
> the `rtems-exec-model` feature). On the exec backend `SearchTransport` has
> the single `NameServersOnly` variant and the dial is the blocking one, on
> the host exactly as on target. The eleven tests this section gated stay
> gated; they now carry `cfg(tokio_backend)`, the predicate their subject
> carries, which the census tool recognises as a gate. Full account:
> `doc/calink-rtems-design.md` §12. The reactor-dependent
tests are therefore genuinely gated, per the rtems-exec-gate contract and
the `server_native/tcp.rs` precedent — each `#[cfg(not(feature =
"rtems-exec-model"))]`, each census marker reduced to match, helpers used
only by gated tests carrying the same predicate:

| file | gated | census |
|---|---:|---|
| `client_native/search_engine.rs` | 11 (live UDP search) | ALLOW 17 → 6 |
| `client_native/context.rs` | 1 (search-timeout) | ALLOW 9 → 8 |
| `client_native/channel.rs` | 1 (search-timeout) | ALLOW 4 → 3 |
| `client_native/udp.rs` | 1 (live UDP `recv_loop`) | ALLOW 5 → 4 |
| `tests/stability.rs` | 5 (live monitor) | ALLOW 31 → 26 |
| `tests/monitor_finish_body.rs` | whole file (2/2) | marker → `#![cfg(not…)]` |
| `tests/monitor_decode_fault_resets_circuit.rs` | whole file (2/2) | marker → `#![cfg(not…)]` |
| `pvalink/registry.rs` | 1 (live upstream monitor) | ALLOW 6 → 5 |

This is faithful to §4.2, which already defers the UDP-search half: the
gated tests are exactly the ones that exercise a live socket or the search
engine, and they run green again once §4.2's UDP stage lands and the
target's blocking transport is what the suite drives.

### 10.3 Driver-agnostic connection bodies had their timers ported too

A spawn converted to the seam but still calling `tokio::time::interval`
inside its body panics on the pool. So the timer calls in the converted,
transport-independent connection bodies moved to the seam as well —
`server_conn`'s heartbeat interval, `context`'s cache-clean interval, and
`ops_v2`/`operation`'s sleeps and timeouts now use
`runtime::task::{interval,sleep,timeout}`. The seam `Interval` exposes only
`.tick()` (default Burst), so the dropped `set_missed_tick_behavior` call
is a deliberate, behaviour-preserving simplification for these periodic
loops. This mirrors the server precedent (`tcp.rs` connection bodies
already use the seam timers).

### 10.4 A structural fix surfaced: `PvaOperation::is_done`'s dual source of truth

Under the exec seam, `cancel_aborts_op` flaked: `cancel()` synchronises on
the operation's RAII termination guard (`terminated_rx`, documented as the
single source of truth for "no longer running"), but `is_done()` read a
*second* signal, `join.is_finished()`. The two flip at different instants
on the seam's join handle, so `cancel()` could return — guard closed —
while `is_done()` still read `false`. `is_done()` now reads the same guard
(`terminated_rx.has_changed().is_err()`), removing the dual meaning by
construction rather than by timing. 30/30 stress iterations green
feature-ON. This is the "suppressed-error question" §5's stage-2 gate
warned to budget for, surfacing one seam behaviour the conversion exposed.

### 10.5 §5's stage-3 risk is stale: the bridge already carries the feature

> **Not stale enough, as it turned out.** The declaration this section found —
> `rtems-exec-model = ["epics-base-rs/rtems-exec-model"]` — forwards to
> `epics-base-rs` and to neither client. Cargo features unify per *package*, so
> `-p epics-bridge-rs --features rtems-exec-model` moved `runtime::task::spawn`
> onto the reactor-free backend while `epics-pva-rs` still compiled its
> reactor-backed UDP transport in and selected it: `rtems-pva-ioc`, the binary
> the feature exists for, was built in the one state that panics at boot. The
> `const` assertion described in `doc/calink-rtems-design.md` §12.2 is what
> found it; the manifest now forwards to both clients.

§5 warned that `rtems-exec-model` was not declared on `epics-bridge-rs`
and that this stage would be gated behind `doc/qsrv-rtems-design.md` §7's
census. That census has since landed: `epics-bridge-rs/Cargo.toml`
declares `rtems-exec-model = ["epics-base-rs/rtems-exec-model"]`, the crate
has its own `tests/rtems_exec_model_gate.rs`, and the pvalink files carry
census markers (`integration.rs` 38, `link.rs` 24, `registry.rs` 6→5,
`iocsh.rs` 2). No blocking dependency remained. The `:826` "third handle
field" §5 named was in fact the `install_pvalink_resolver` parameter, not a
struct field; `new(handle)` became `new()` with a `Default` impl.

### 10.6 The gate as measured

| gate | result |
|---|---|
| `cargo nextest run -p epics-pva-rs` (default) | 1382 passed, 2 skipped |
| `cargo nextest run -p epics-pva-rs --features rtems-exec-model` | 1352 passed, 0 failed, 2 skipped |
| `cargo nextest run -p epics-bridge-rs --features pvalink,qsrv-core` (default) | pvalink 142 passed |
| `cargo nextest run -p epics-bridge-rs --features pvalink,qsrv-core,rtems-exec-model` | 681 passed, 0 failed |
| `cargo nextest run -p epics-base-rs --features rtems-exec-model` | 3523 passed, 0 failed |
| `cancel_aborts_op`, feature-ON, ×30 | 30/30 pass (the is_done fix, stressed) |
| both crates' `rtems_exec_model_gate` census tests | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo nextest run --workspace` | 10116 passed, 0 failed, 2 skipped |
| `./scripts/rtems-check.sh` | exit 0; client probe **28** (ratchet held), 2 pre-existing `server_native/` warnings unchanged (§9.9) |

## 11. Stage 4 as built — where reality deviated from §5

### 11.1 The "small" stage was the ratchet closure, not the mount

§5 scoped stage 4 as *small*: "extend `CRATE_FEATURES`… which pulls
`epics-pva-rs/client`… Install the resolver… replace the banner." That
prose reads as if `epics-pva-rs/client` already compiled for the target
and only the wiring remained. It did **not**: `required-features =
["qsrv-core", "pvalink"]` pulls `epics-pva-rs/client`, and the client's
UDP SEARCH transport was **28 compile errors** on `armv7-rtems-eabihf`
(the `PVA_CLIENT_TARGET_ERRORS` ratchet stood at 28, all UDP-confined —
`client_native/udp.rs` 20, `search_engine.rs` 7, `search.rs` 1). Selecting
the feature without first closing those 28 would make `--bin rtems-pva-ioc`
fail the target build. So the mount could not land until the client
compiled for the triple, and the real work of stage 4 was the structural
closure §5 had folded invisibly into "pulls `epics-pva-rs/client`". It is
split across two commits: the ratchet closure first, the mount second.

### 11.2 The closure is a cfg-gate on the existing seam, not `#[allow]`

The 28 errors are the UDP transport, and §4.2 already named the seam:
`SearchTransport` is a sum type whose `NameServersOnly` variant resolves
over TCP name servers with no socket. Stage 1 built that variant; stage 4
makes it the *only* variant the target compiles. Every UDP-only item in
`client_native/search_engine.rs` — the `SearchTransport::Udp(UdpTransport)`
variant and its `impl`, `bind_udp`, the `recv_search`/`recv_beacon`
families' `Udp` arms, the ephemeral/beacon socket binders, the multicast
and interface-fanout helpers, the `SearchTarget` type — is now behind
`#[cfg(not(target_os = "rtems"))]`, and `client_native/mod.rs` gates the
two host-only modules (`search`, the legacy standalone path; `udp`, the
client UDP manager) the same way. This is structural: the illegal
configuration (a UDP socket on a target with no `recvmsg`/`IP_PKTINFO` and
no `local_addr()` readback) is **not constructible** on the target, rather
than constructed-then-suppressed. No `#[allow]`, no `#[cfg]`'d-away panic.

The `NameServersOnly` arms that previously delegated to UDP now discard
their unused inputs explicitly (`let _ = buf;` in the `recv_*` families,
`let _ = (codec, entries, send_errs);` in `broadcast_*`) and, where a
receive loop must never resolve, `std::future::pending().await`. That is
why the closure produced **zero** new warnings, not ten: the discards are
part of the structural arm, not an afterthought.

### 11.3 The predicate is `target_os`, not a Cargo feature

The gate key is `cfg(not(target_os = "rtems"))`, a **target-capability**
predicate, not a feature flag. This is deliberate and load-bearing in two
directions: the ratchet probe compiles the client for the target triple
*without* any bespoke feature, so `target_os` is what it keys on; and the
host's forced-on RTEMS-exec-model test suite runs on Linux, where UDP is
present and must stay compiled — a feature predicate would risk stripping
UDP from the host build too. `PVA_CLIENT_TARGET_ERRORS` moved 28 → **0**
in `scripts/rtems-check.sh`, and the bidirectional ratchet (fail if the
measured count is either above *or* below the pin) now holds the line at
fully-closed. The log line reads "0 target errors (UDP transport
cfg-gated out)".

### 11.4 pvalink itself compiles for the target — §6 item 2, settled

§6 listed as unverified whether pvalink's own surface (over
`client_native`) would compile for the target once the client did. It
does: with the 28 client errors closed, `--bin rtems-pva-ioc` with
`required-features = ["qsrv-core", "pvalink"]` builds for
`armv7-rtems-eabihf` with **0 errors** in both the portability and the
image configuration. Stage 3's target-compatibility work (every spawn
through `runtime::task::spawn`, `PvaLinkResolver` construction with no
runtime handle) is what makes that true; stage 4 is the first build that
exercises it end-to-end for the target.

### 11.5 The banner and its guard flipped from absence to presence

The old banner said `pva:// record links do NOT resolve on this target`
and its guard `the_banner_states_that_pva_links_do_not_resolve` pinned
that sentence. Both are gone. The banner now reports
`pvalink resolver installed — {link_count} pva:// record link(s)
pre-registered`, unconditionally including `link_count == 0` (the target
has no shell; the console is the only place an operator confirms the
resolver came up). The guard became
`the_pvalink_resolver_is_mounted_and_the_banner_reports_it`: it asserts
the `install_pvalink_resolver(&db)` call, the banner text, and the
`link_count` report all survive in the production slice — the stage-4
counterpart of `the_group_source_is_mounted_at_the_pvxs_order`. The other
three source-text guards (`entry_point_never_starts_a_runtime`,
`every_thread_here_states_a_stack_size`,
`the_group_source_is_mounted_at_the_pvxs_order`) already cover the new
code unchanged.

### 11.6 What stage 4 does **not** prove

Everything here is `cargo check` for the target and host tests. Whether a
`pva://` link on the guest actually resolves over `EPICS_PVA_NAME_SERVERS`
and forwards a monitor — the claim an operator cares about — is stage 5's
gate (two IOCs on the wire, §5), unrun. In particular, no census marker
needed bumping: the mount adds a `block_on_sync(install_pvalink_resolver)`
call, which is neither a spawn nor a thread the `rtems_exec_model_gate`
census counts, and the UDP gating removed no `RTEMS-EXEC-MODEL-ALLOW`
marker (source-text scans see cfg'd-out code). Both crates' census gates
pass feature-ON unchanged.

### 11.7 The two pre-existing RTEMS-gate warnings, unchanged

The client probe still surfaces the same two warnings §9.9 recorded, both
in `server_native/` (outside this change): `tcp.rs:1407` deprecated
`fetch_update`, and `search_engine.rs:501` dead `Origin::{FromOriginTag,
Forwarded}` variants. Neither is client-side; neither moved.

*Both are now fixed at source — `f9008445` and `7235e6a5`, recorded with
their before/after warning text in `doc/calink-rtems-design.md` §10.11
items 4 and 5. `./scripts/rtems-check.sh` emits no warning at all in
either configuration. This section, §9.9 and §8.7 are left as written:
each was a true statement about the stage that measured it, and "the
gate carried two warnings for this long" is the fact those rows exist to
record.*

### 11.8 The gate as measured

| gate | result |
|---|---|
| `./scripts/rtems-check.sh` | exit 0; client probe **0** target errors (was 28; UDP transport cfg-gated out), both configs green |
| `cargo clippy -p epics-bridge-rs -p epics-pva-rs --all-targets -- -D warnings` (default) | clean |
| `cargo clippy -p epics-bridge-rs --all-targets --features pvalink,qsrv-core,rtems-exec-model -- -D warnings` | clean |
| `cargo clippy -p epics-pva-rs --all-targets --features rtems-exec-model -- -D warnings` | clean |
| `cargo nextest run -p epics-pva-rs` (default) | 1382 passed, 2 skipped |
| `cargo nextest run -p epics-pva-rs --features rtems-exec-model` | 1352 passed, 2 skipped |
| `cargo nextest run -p epics-bridge-rs` (default) | 684 passed |
| `cargo nextest run -p epics-bridge-rs --features pvalink,qsrv-core,rtems-exec-model` | 681 passed |
| four `rtems-pva-ioc` source-text guards (incl. the new pvalink guard) | pass, feature-ON |
| both crates' `rtems_exec_model_gate` census | pass, feature-ON, no marker bump needed |

---

## 12. Stage 5 as built — the on-target gate, and what it found

Run on the QEMU/BSP box on 2026-07-23. Topology A only (topology B stays
UNVERIFIED, §6). All six of §5's criteria pass. Getting there took four
production fixes, three of which no host test and no `cargo check` could
have produced: the failures are *runtime* failures of the target's
execution model, which is the whole reason §5 says stage 4 is not the
gate.

### 12.1 The rig

The probe — the three link records, the compiled-in name server, the
console reporter thread and the C task/stack census it calls — is
`doc/pvalink-stage5-probe.patch` (commits `a82ee9fe` + `c9907585`,
verified to apply to `d28b1c6d`). It is measurement scaffolding, not
product: it exists because the image configures the RTEMS shell's
`stackuse`/`top` commands but starts no shell, and because a `-kernel`
boot has no `.db` to load (§12.3). The four production fixes below are
separate commits and stand without it.

Guest image — built on the box, not cross-checked and hoped for
(`~/rtems-bringup/build-stage5.sh`):

```
cargo +nightly build --release --target armv7-rtems-eabihf \
    -Zbuild-std=std,panic_abort \
    --no-default-features --features qsrv-core,pvalink \
    -p epics-bridge-rs --bin rtems-pva-ioc
```

Upstream IOC — pvxs `softIocPVX` on the host, PVA on **15076**, because
5075 belongs to the guest's `hostfwd` (`~/rtems-bringup/stage5/run-upstream.sh`,
`upstream.db`):

```
export EPICS_PVAS_SERVER_PORT=15076 EPICS_PVAS_BROADCAST_PORT=15076
export EPICS_PVA_SERVER_PORT=15076  EPICS_PVA_BROADCAST_PORT=15076
exec .../softIocPVX -D .../softIocPVX.dbd -d .../stage5/upstream.db
# record(ai, "UPSTREAM:AI") { VAL 1.0, PREC 3, EGU V, SCAN Passive }
# record(ao, "UPSTREAM:AO") { VAL 0.0, PREC 3, EGU V, OMSL supervisory }
```

Guest boot (`~/rtems-bringup/stage5/boot-stage5.sh`) — the measured
invocation of `doc/rtems-priority-on-target-measurement.md`, plus one
`hostfwd` whose **host port equals the guest port**:

```
qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
  -serial null -serial mon:stdio \
  -nic user,model=cadence_gem,hostfwd=tcp:127.0.0.1:5075-:5075 \
  -kernel stage5ioc.exe
```

Two directions, two mechanisms, and they are not symmetric: the guest
reaches the upstream **outbound** at `10.0.2.2:15076` with no `hostfwd`
at all, and the host reaches the guest **inbound** only through the
`hostfwd` — as `127.0.0.1:5075`, because SLIRP presents the host to the
guest and the guest to the host as loopback. No UDP is forwarded in
either direction; both IOCs are found by `EPICS_PVA_NAME_SERVERS` over
TCP.

### 12.2 §5's `.db` spelling does not load — `@pva://` is INST_IO

§5 stage 5 said the guest record carries `INP=@pva://UPSTREAM:AI CP`. It
does not load, in this tree or in C. A leading `@` is INST_IO, and
`dbCanSetLink` (`record/link.rs:487`, C `dbStaticLib.c:2400`) refuses
INST_IO on a record whose bound device support declares CONSTANT — which
a soft `ai` does. Measured on the target: `iocInit` fails with

```
ai.INP: can't initialize link type CONSTANT with "@pva://UPSTREAM:AI CP" (type INST_IO)
```

and the image exits. The `@` prefix belongs to device-support links
(`INP=@asyn(...)`), not to soft links. Two spellings do load and both are
exercised by the probe image, so the gate covers the JSON longhand *and*
this tree's scheme form:

```
record(ai, "RTEMS:PVA:DOWN")  { field(INP, "{pva: { pv: 'UPSTREAM:AI', proc: 'CP' }}") }
record(ai, "RTEMS:PVA:DOWN2") { field(INP, "pva://UPSTREAM:AI CP") }
record(ao, "RTEMS:PVA:UPLNK") { field(OUT, "{pva: { pv: 'UPSTREAM:AO' }}") }
```

§5 stage 5 carried the rejected spelling in prose and is now corrected in
place; this section keeps it, because the rejected form is what it exists
to name. The same `@` defect on the CA side, and the twelve source sites
that carried it across both IOC binaries, are
`doc/calink-rtems-design.md` §10.7.

### 12.3 The topology is compiled in, because the target has neither a filesystem nor argv

§5 assumes "boot `rtems-pva-ioc` with `EPICS_PVA_NAME_SERVERS=...` and a
`.db`". A `-kernel` boot has no filesystem to name a `.db` on and
`rtems_init.c:195` hands `main` a fixed one-element argv, so there is no
`-d` and no environment to inherit. Both are therefore compiled into the
probe image: the three records above live in `DEMO_DB`, and the address
is `const STAGE5_NAME_SERVER: &str = "10.0.2.2:15076"`, `set_var`'d
before the client is built. This is a property of the probe, not of
pvalink: the resolver reads `EPICS_PVA_NAME_SERVERS` normally.

### 12.4 The six criteria, as measured

All six on one boot of the `f75f1e56` image. Guest booted 02:16;
`STAGE5 seq=N` lines are the probe reporter thread, every 10 s.

**(1) Banner — resolver installed, link count, not silence. PASS.**
Console, `~/rtems-bringup/stage5/guest.log`:

```
rtems-pva-ioc: serving 6 records on PVA TCP port 5075 (UDP search on 5076), GUID 4245e0646d8db428da5936bc, RTEMS execution model, no tokio runtime
rtems-pva-ioc: QSRV2 ENABLED — sources: qsrvSingle(0), qsrvGroup(1)
rtems-pva-ioc: pvalink resolver installed — 2 pva:// record links pre-registered; @pva://... INP/OUT resolve over EPICS_PVA_NAME_SERVERS (TCP name servers; UDP search is compiled out on this target)
STAGE5 probe: EPICS_PVA_NAME_SERVERS=10.0.2.2:15076 (compiled in), reporting every 10 s
STAGE5 seq=1 links=2 channels_total=2 active=2 searching=0 connecting=0 name_servers=1 connections=1
STAGE5 seq=1 link pv=UPSTREAM:AI dir=Inp connected=true records=["RTEMS:PVA:DOWN", "RTEMS:PVA:DOWN2"]
STAGE5 seq=1 link pv=UPSTREAM:AO dir=Out connected=true records=["RTEMS:PVA:UPLNK"]
```

"2 links" for three records is correct and is the §2.4 property on
target: `UPSTREAM:AI` is shared by both INP records.

That banner is quoted as it was printed by the `f75f1e56` image and is
left byte for byte, so the `@pva://...` in its second half stays. It was
itself one of the twelve source sites `doc/calink-rtems-design.md` §10.7
corrected: the banner *text* named a spelling the loader rejects, while
the links the same image actually loaded were the two in §12.2 that do.
`rtems-pva-ioc.rs:671-673` now prints `pva://... INP/OUT resolve over
EPICS_PVA_NAME_SERVERS`.

**(2) `pvxget` from the host against the guest's record returns the
upstream value. PASS.** Command and output (host, through the `hostfwd`):

```
$ EPICS_PVA_NAME_SERVERS=127.0.0.1:15076 pvxget -w 5 UPSTREAM:AI
UPSTREAM:AI
    value double = 3.875
    alarm.severity int32_t = 0
    alarm.status int32_t = 0
$ EPICS_PVA_NAME_SERVERS=127.0.0.1:5075 pvxget -w 5 RTEMS:PVA:DOWN RTEMS:PVA:DOWN2
RTEMS:PVA:DOWN
    value double = 3.875
    alarm.severity int32_t = 0
    alarm.status int32_t = 0
    ...
    display.units string = "V"
    display.precision int32_t = 3
RTEMS:PVA:DOWN2
    value double = 3.875
    alarm.severity int32_t = 0
```

(`...` elides the unchanged NTScalar metadata fields.) The first run of
this criterion, before criterion 3 moved the upstream, read `42.125` on
all three PVs. Both spellings of the link carry the value; the guest's
own `EGU`/`PREC` are served, i.e. the value is a *record* value produced
by the link, not a proxied upstream structure.

**(3) A put upstream reaches the guest record — the monitor path. PASS.**
`~/rtems-bringup/stage5/crit3.sh 3.875`: a `pvxmonitor` on the guest's
two records, then a `pvxput` to the upstream 6 s later.

```
---- put side ----
02:19:23.428 PUT UPSTREAM:AI = 3.875
02:19:23.459 PUT returned rc=0
---- monitor side ----
02:19:17.697 MON     value double = 42.125         <- RTEMS:PVA:DOWN, initial
02:19:17.741 MON     value double = 42.125         <- RTEMS:PVA:DOWN2, initial
02:19:23.498 MON RTEMS:PVA:DOWN
02:19:23.500 MON     value double = 3.875
02:19:23.509 MON RTEMS:PVA:DOWN2
02:19:23.511 MON     value double = 3.875
```

70 ms and 81 ms from the upstream put to the downstream monitor update,
end to end through SLIRP twice. `SCAN` is `Passive` on both records: the
update is the `CP` monitor driving `scanOnce`, which is exactly the path
the criterion is about — nothing polls here.

**(4) Kill the upstream → `LINK`/`INVALID`; restart → recovery with no
guest reboot. PASS.** Upstream killed 02:17:54:

```
STAGE5 seq=5 links=2 channels_total=2 active=0 searching=2 connecting=0 name_servers=1 connections=1
STAGE5 seq=5 conn peer=10.0.2.2:15076 alive=false rx=1393 tx=274 channels=[]
STAGE5 seq=5 link pv=UPSTREAM:AI dir=Inp connected=false records=["RTEMS:PVA:DOWN", "RTEMS:PVA:DOWN2"]
STAGE5 seq=5 link pv=UPSTREAM:AO dir=Out connected=false records=["RTEMS:PVA:UPLNK"]
STAGE5 seq=5 record RTEMS:PVA:DOWN  VAL=Ok("99.5") SEVR=Ok("3") STAT=Ok("14")
STAGE5 seq=5 record RTEMS:PVA:DOWN2 VAL=Ok("99.5") SEVR=Ok("3") STAT=Ok("14")
```

`SEVR=3` is INVALID, `STAT=14` is LINK. Upstream restarted 02:18:26 and
`pvxput UPSTREAM:AI 42.125`; 40 s later, same boot of the guest, no
reboot, no restart of anything on the target:

```
STAGE5 seq=9 links=2 channels_total=2 active=2 searching=0 connecting=0 name_servers=1 connections=1
STAGE5 seq=9 conn peer=10.0.2.2:15076 alive=true rx=1377 tx=258 channels=["UPSTREAM:AO", "UPSTREAM:AI"]
STAGE5 seq=9 link pv=UPSTREAM:AI dir=Inp connected=true records=["RTEMS:PVA:DOWN", "RTEMS:PVA:DOWN2"]
STAGE5 seq=9 record RTEMS:PVA:DOWN  VAL=Ok("42.125") SEVR=Ok("0") STAT=Ok("0")
STAGE5 seq=9 record RTEMS:PVA:DOWN2 VAL=Ok("42.125") SEVR=Ok("0") STAT=Ok("0")
```

The re-subscribe loop survives the 1 s clock quantum (§4.4) — but only
after §12.5 and §12.8; on the pre-fix images this criterion failed twice,
in two different ways.

**(5) An OUT link writes upstream. PASS.** `~/rtems-bringup/stage5/crit5.sh 6.25`:
a `pvxmonitor` on the *upstream* `UPSTREAM:AO`, then a `pvxput` to the
*guest's* `RTEMS:PVA:UPLNK` through the `hostfwd`.

```
---- put side ----
02:19:47.595 PUT RTEMS:PVA:UPLNK = 6.25 (guest, via hostfwd 127.0.0.1:5075)
02:19:47.872 PUT returned rc=0
02:19:50.876 GET UPSTREAM:AO
UPSTREAM:AO
    value double = 6.25
    alarm.severity int32_t = 0
    alarm.status int32_t = 0
    alarm.message string = ""
---- upstream UPSTREAM:AO monitor ----
02:19:41.616 MON     value double = 0
02:19:41.620 MON     alarm.status int32_t = 2
02:19:41.622 MON     alarm.message string = "UDF"      <- before
02:19:47.886 MON UPSTREAM:AO
02:19:47.888 MON     value double = 6.25
02:19:47.890 MON     alarm.severity int32_t = 0
02:19:47.894 MON     alarm.message string = ""         <- after
```

291 ms from the guest-side put to the upstream monitor event, and the
upstream's UDF alarm clears — the write reached the upstream *record*,
not just its server. This is `flush_puts` on the link-put-queue owner
executing on the callback pool.

**(6) Thread census and per-thread peaks vs §4.3; one connection
regardless of link count. PASS.** `TASKDUMP`/`STACKUSE` from the probe
(`rtems_task_iterate` + `rtems_stack_checker_report_usage`), tag `s5-18`,
IOC threads only (`0x0b......`; the `0x0a......` block is libbsd and
`0x09010001` is IDLE):

```
TASKDUMP begin tag=s5-18 count=30 scheduler_sc=0
TASKDUMP id=0x0b010001 core=254 posix=   1 sc=0 obj=       thread=<empty>
TASKDUMP id=0x0b010002 core=140 posix= 115 sc=0 obj=       thread=cbLow
TASKDUMP id=0x0b010003 core=135 posix= 120 sc=0 obj=       thread=cbMedium
TASKDUMP id=0x0b010004 core=128 posix= 127 sc=0 obj=       thread=cbHigh
TASKDUMP id=0x0b010005 core=129 posix= 126 sc=0 obj=       thread=cbTimer
TASKDUMP id=0x0b010006 core=132 posix= 123 sc=0 obj=       thread=scanOnce
TASKDUMP id=0x0b010007 core=189 posix=  66 sc=0 obj=       thread=status-pv
TASKDUMP id=0x0b010008 core=181 posix=  74 sc=0 obj=       thread=PVAS-TCP
TASKDUMP id=0x0b010009 core=183 posix=  72 sc=0 obj=       thread=PVAS-UDP
TASKDUMP id=0x0b01000a core=189 posix=  66 sc=0 obj=       thread=stage5-probe
TASKDUMP id=0x0b01000f core=181 posix=  74 sc=0 obj=       thread=PVAC-reader 10.
TASKDUMP id=0x0b010010 core=181 posix=  74 sc=0 obj=       thread=PVAC-writer 10.
TASKDUMP id=0x0b010011 core=181 posix=  74 sc=0 obj=       thread=PVAC-reader 10.
TASKDUMP id=0x0b010012 core=181 posix=  74 sc=0 obj=       thread=PVAC-writer 10.
TASKDUMP end tag=s5-18

STACKUSE begin tag=s5-18
ID         NAME                  LOW        HIGH       CURRENT     AVAIL   USED
0x0b010001                       0x00948650 0x0095863f 0x00957c10  65520  21912
0x0b010002 cbLow                 0x00a2d878 0x00b2d867 0x00b2d6d0 1048560    632
0x0b010003 cbMedium              0x00b2deb0 0x00c2de9f 0x00c2dd08 1048560  28160
0x0b010004 cbHigh                0x00c2e4e8 0x00d2e4d7 0x00d2e340 1048560    632
0x0b010005 cbTimer               0x00d2eb20 0x00daeb0f 0x00dae910 524272    760
0x0b010006 scanOnce              0x00daf158 0x00eaf147 0x00eaefc0 1048560    616
0x0b010007 status-pv             0x00eb6e28 0x00ef6e17 0x00ef6bf0 262128   1960
0x0b010008 PVAS-TCP              0x00ef74a0 0x00f7748f 0x00f76aa8 524272   3144
0x0b010009 PVAS-UDP              0x00f77bd0 0x00ff7bbf 0x00ff7538 524272   1752
0x0b01000a stage5-probe          0x00ff8328 0x01078317 0x01077f00 524272   1648
0x0b01000f PVAC-reader 10.       0x010bcd20 0x010fcd0f 0x010fc860 262128   1288
0x0b010010 PVAC-writer 10.       0x010fd5a0 0x0113d58f 0x0113d330 262128   2264
0x0b010011 PVAC-reader 10.       0x01183460 0x011c344f 0x011c2fa0 262128   1320
0x0b010012 PVAC-writer 10.       0x01140bd0 0x01180bbf 0x01180960 262128   2264
STACKUSE end tag=s5-18
```

Against §4.3's arithmetic:

* **pvalink's delta is exactly the predicted 4 threads** — two
  `PVAC-reader`/`PVAC-writer` pairs, one pair per TCP peer (the data
  connection and the name-server connection), each `Small` (262,128 B
  usable of the 262,144 class). Not one thread more: the re-subscribe
  loops, the search-engine loop and the heartbeat are tasks on the
  callback bands, exactly as §4.3's zero-thread rows claim.
* **`connections=1` regardless of link count**, on target, with two links
  and three records: `STAGE5 seq=18 links=2 channels_total=2 active=2
  searching=0 connecting=0 name_servers=1 connections=1`. Confirmed
  independently on the wire, host side:

  ```
  $ ss -tn state established '( sport = :15076 )'
  Recv-Q Send-Q      Local Address:Port        Peer Address:Port
  0      0      [::ffff:127.0.0.1]:15076 [::ffff:127.0.0.1]:40390
  0      0      [::ffff:127.0.0.1]:15076 [::ffff:127.0.0.1]:40584
  ```

  Two sockets, and that is the correct number: one data circuit plus one
  name-server circuit. §2.4's property is *one connection per upstream
  peer*, and the name server here happens to be the same process.
* **Baseline threads match §4.3 to zero, not to "within one"**, once
  §12.9's two missing rows are added to that table. Ten baseline IOC
  threads are present: `main`, `cbLow`, `cbMedium`, `cbHigh`, `cbTimer`,
  `scanOnce`, `status-pv`, `PVAS-TCP`, `PVAS-UDP`, and `stage5-probe` —
  the last of which belongs to the probe, not to an IOC.
* **No per-inbound-connection triple is in this sample** because the
  `pvxget`s had already closed; a sample taken while a host client was
  attached showed the predicted `PVAS-conn` (Big, 25,616 B used) plus
  `PVAS-read`/`PVAS-write`.
* **Peak stack use is nowhere near a ceiling.** The deepest IOC thread is
  `cbMedium` at 28,160 of 1,048,560 B (2.7 %) — that is where every
  pvalink task, the search engine and the record processing run. The
  PVAC pump threads peak at 2,264 of 262,128 B (0.9 %). The tightest
  ratio on the box is `main` at 21,912 of 65,520 B (33 %), and `main` is
  not a `StackSizeClass` thread — it is
  `CONFIGURE_POSIX_INIT_THREAD_STACK_SIZE` (`rtems_config.c:35`), which
  is the one stack size in this system that is *not* generous.

### 12.5 Finding 1 — a task moved to the callback pool takes its timers with it

The first boot of the probe image killed `cbMedium` three times over:

```
panic on thread `cbMedium` at tokio-1.51.1/src/time/interval.rs:138:
  there is no reactor running, must be called from the context of a Tokio 1.x runtime
panic on thread `cbMedium` at pvalink/link.rs:440: (same)
panic on thread `cbMedium` at client_native/search_engine.rs:3046: (same)
```

The IOC kept listening, kept answering searches and kept serving its
local records — it looked healthy from the network — while every
downstream record sat at `SEVR=3 STAT=17` forever.

Stage 3 put every client **spawn** on the `runtime::task` seam and pinned
that with a source-text guard. A task moved onto the callback pool takes
its **timer** calls with it, and nothing pinned those; stage 4 then made
the search engine target-live, which carried its `tokio::time::interval`
tick straight onto the pool. Fixed at every site the anchor `rg -n
'tokio::time'` found in target-compiled production code, and closed
structurally: the seam guard grew a timer half in both crates
(`client_scope_timers_go_through_the_runtime_seam`,
`pvalink_scope_timers_go_through_the_runtime_seam`), both of which fail
on the pre-fix tree. Commit `b76971ef`.

This is the finding that justifies §5's insistence that stage 4 is not
the gate: `scripts/rtems-check.sh` was green, all host tests were green,
and the feature-ON simulation could not see it either, because
`rtems-exec-model` keeps a tokio *runtime* alive for `tokio::net` — so
the timer calls that panic on target find a reactor on the host.

### 12.6 Finding 2 — the install scan skipped every option-less link, in both directions

Criterion 5 could not even be attempted: `OUT="{pva: {pv: 'UPSTREAM:AO'}}"`
produced no link at all, and the banner read `1 pva:// record link`
instead of 2. A `pv`-only JSON longhand collapses to `ParsedLink::Pva`
with no option suffix, and the install scan had an early-out —
`if link_pv_name(s) == s { continue; }` — that dropped exactly those
links. pvxs has no such branch: `pvaOpenLink` opens every link, and
options are defaults, not a precondition for opening. Removing the
early-out is the whole fix; the arm already had both directions.
Commit `44c4ef3e`.

Its sibling is why this took a scratch test to find rather than a glance
at the console: the install scan's four open calls were `let _ = ...`, so
an open that failed said nothing at all — on a target with no iocsh,
silence is the only symptom you get. Every one now reports through one
helper, in the shape C uses (`record.FIELD Error: pvalink to 'chan' not
opened: ...`). Commit `669b4d53`.

### 12.7 Finding 3 — an idle name-server circuit is silent, and the server reaps it

With both links resolved and nothing to search for, `ns_run_once` wrote
nothing, and a pvxs server closes a client that has been silent for its
`tcpTimeout`. Host-side `ss -tn state established '( sport = :15076 )'`,
sampled every 2 s, with the guest idle:

```
01:51:02  ...:37802  ...:56928      <- NS circuit re-dialled
01:51:41  ...:37802  ...:56928      <- alive 39 s
01:51:43             ...:56928      <- server dropped it
01:51:53  ...:44726  ...:56928      <- next dial, 10 s later
```

`...:56928` is the data connection, which has a heartbeat and never
churned. The same churn is visible from the target, in the census: the
data pair keeps its object ids while the name-server pair gets new ones
at every sample, and at one sample is missing entirely
(`guest-prefix-boot3.log`, `PVAC-*` rows: `0x12/0x13` → `0x1a/0x1b` →
`0x1f/0x20` → *absent* → `0x13/0x14`).

pvxs has no such gap because a name-server connection is not special
there: `Connection::build()` makes it and `nameserver = true` is flipped
only afterwards (`client.cpp:674-685`), so it carries `clientconn.cpp`'s
echo timer and inactivity bound like any data circuit. Ours now does the
same — application `CMD_ECHO` every `max(1, min(15, tcpTimeout*3/8))` s
and a `tcpTimeout` idle bound. Commit `94868064`.

On target the cost of the bug was not cosmetic: each cycle re-created two
blocking pump threads (every RTEMS `std::thread` leaks 128 B of TLS-key
bookkeeping) and left a ≥10 s window — `tcpNSCheckInterval` — in which no
PV could be resolved at all. Post-fix, the `f75f1e56` image held both
circuits open for the whole 46-report (≈8 min) run with no re-dial:
`seq=46` still reads `alive=true`, and `ss` still shows the same two
sockets.

### 12.8 Finding 4 — the disconnect that never arrives

Criterion 4's first half failed on the otherwise-good image: the upstream
was killed and the client saw it immediately and correctly, while every
link and record kept claiming health.

```
STAGE5 seq=23 links=2 channels_total=2 active=0 searching=2 connections=1
STAGE5 seq=23 conn peer=10.0.2.2:15076 alive=false rx=1486 tx=370 channels=[]
STAGE5 seq=23 link pv=UPSTREAM:AI dir=Inp connected=true records=[...]
STAGE5 seq=23 record RTEMS:PVA:DOWN  VAL=Ok("12.5") SEVR=Ok("0") STAT=Ok("0")
STAGE5 seq=23 record RTEMS:PVA:DOWN2 VAL=Ok("12.5") SEVR=Ok("0") STAT=Ok("0")
```

A stale value served as good, with no LINK/INVALID — the exact failure
`is_connected()` exists to prevent. Root cause: both monitors inferred
"the upstream is gone" from the subscription future returning, and it
never returns. `op_monitor_events` handles `MonitorEnd::ConnectionLost`
by re-subscribing **internally** (deliver `Disconnected`, sleep 200 ms,
loop), so the future comes back only for a fatal/remote end.

Both monitors now take the transition from the event stream
(`pvmonitor_events` with `mask_connected: false, mask_disconnected:
false`), which is the shape pvxs has — `pvaLinkChannel` is driven by the
monitor's event stream and its `catch(client::Disconnect&)` branch
(`pvalink_channel.cpp:335-373`), not by a subscription call returning.
The INP transition got a single owner, `inp_disconnect_scan`, called from
both places that can observe it and idempotent by construction (a `swap`
gate makes the second observer of one outage a no-op, and a subscription
that never delivered an event synthesizes no scan). Commit `f75f1e56`.

### 12.9 §4.3's thread table was two threads short

Criterion 6 says "match §4.3's arithmetic to within one thread", which
requires §4.3 to be right. It listed 7 baseline threads; the target runs
9 before any `pva://` link exists. Missing: `main` (the POSIX_Init
thread, 65,536 B from `rtems_config.c:35`, and the deepest user of its
own stack on the box at 33 %) and `status-pv` (`status_pv.rs:291-294`,
`Small`). Both rows are now in §4.3, marked. Nobody had counted the
threads on a target before, which is why an omission in the table
survived four stages.

### 12.10 What stage 5 does **not** prove

* **Topology B** is untouched and stays UNVERIFIED (§6): two SLIRP guests
  are on separate `10.0.2.0/24` networks and cannot address each other.
* **Scale.** Two links, one upstream, one name server, three records. The
  per-peer cost is confirmed; the 20-link figure in §4.3 is still
  arithmetic.
* ~~**The gateway's monitor path has the §12.8 defect and is UNFIXED.**~~
  **CLOSED at the monitor-handle API.** The anchor for "disconnect
  inferred from the subscription future returning" had a third site:
  `pva_gateway/channel_cache.rs`'s `handle.wait().await`, where the
  raw-frames handle re-subscribes internally on `ConnectionLost` in the
  same way, so `signal_disconnect_boundary` did not fire on a plain
  upstream loss. As predicted here, the fix was an `epics-pva-rs`
  monitor-handle API change rather than a call-site edit, and it closes
  the family rather than the site:

  * **Invariant.** A monitor consumer MUST learn connection transitions
    from the monitor's event/state stream and MUST NOT infer them from
    the subscription handle/future terminating.
  * **Owner.** `ConnEventOwner` in `client_native/ops_v2.rs` — the one
    place a `MonitorConnEvent` is emitted, for BOTH handle constructors
    (`op_monitor_handle` and the raw-frames pair). It emits `Connected`
    only from the disconnected state and exactly one of `Disconnected` /
    `Finished` per `Connected`, so the alternation holds by construction.
  * **Structural closure.** The connection-state callback is a REQUIRED
    constructor parameter, so no call site can open a handle monitor with
    no way to observe a disconnect; and `SubscriptionHandle::wait()` is
    gone, replaced by `wait_terminal() -> MonitorTermination`, a type
    with no variant that can stand in for a connection state. The old
    inference does not compile.

  The gateway now takes the transition from `MonitorConnEvent` and keeps
  the post-termination `signal_disconnect_boundary` call as the second
  observer of one idempotent owner — the same shape `inp_disconnect_scan`
  has in pvalink (§12.8, commit f75f1e56).
* **Long-run stability** is 8 minutes, not days. What that does establish
  is that §12.7's churn is gone.

### 12.11 The gate as measured

| gate | result |
|---|---|
| §5 criterion 1 — banner reports resolver + link count | **pass** (`pvalink resolver installed — 2 pva:// record links pre-registered`) |
| §5 criterion 2 — `pvxget` host → guest record returns the upstream value | **pass** (`RTEMS:PVA:DOWN`/`DOWN2` = `3.875` = `UPSTREAM:AI`) |
| §5 criterion 3 — upstream put reaches the guest record via the monitor | **pass** (70 ms / 81 ms, `SCAN=Passive`) |
| §5 criterion 4 — kill → LINK/INVALID, restart → recovery, no guest reboot | **pass** (`SEVR=3 STAT=14` → `SEVR=0 STAT=0`, same boot) |
| §5 criterion 5 — OUT link writes and is observed upstream | **pass** (`UPSTREAM:AO` = `6.25`, UDF cleared, 291 ms) |
| §5 criterion 6 — census vs §4.3, and one connection per peer | **pass** (+4 threads exactly; `connections=1`, 2 sockets = data + NS) |
| `./scripts/rtems-check.sh` | exit 0, both configurations |
| target release build (`armv7-rtems-eabihf`, `-Zbuild-std`) | links; image boots and runs (this is the check `cargo check` cannot make) |
| `cargo clippy -p epics-bridge-rs -p epics-pva-rs --all-targets -- -D warnings`, default and feature-ON | clean |
| `cargo nextest run -p epics-pva-rs` | 1384 passed, 2 skipped (was 1382) |
| `cargo nextest run -p epics-pva-rs --features rtems-exec-model` | 1353 passed, 2 skipped (was 1352) |
| `cargo nextest run -p epics-bridge-rs` | 687 passed (was 684) |
| `cargo nextest run -p epics-bridge-rs --features pvalink,qsrv-core,rtems-exec-model` | 683 passed (was 681) |
| new tests | 5: two seam guards (one per crate), the NS echo, the install scan opening option-less links both ways, and upstream-death → link disconnect |
| both crates' `rtems_exec_model_gate` census | pass, marker `ALLOW(38)` → `ALLOW(39)` in `pvalink/integration.rs` |
| the two pre-existing RTEMS-gate warnings (§9.9, §11.7) | unchanged, still `server_native/` |
