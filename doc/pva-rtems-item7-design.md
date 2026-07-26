# Phase 6 item 7 — PVA blocking accept loop + UDP search responder + beacon sender

Read-only scoping. Every claim carries file:line. Two worktrees:

* PVA — `/home/stevek/work/epics-rs/.caucus/worktrees/pva-cs-ring`, branch
  `phase6/pva-channelsource-ring` @ `93590517` (items 4+6+stage 1–2 of item 5).
* CA reference — `/home/stevek/work/epics-rs/.caucus/worktrees/manual-ca-sans-io`
  @ `bfaae8f3`, READ-ONLY (advanced from `6f5f492f` mid-investigation; the added
  commit is a test file, and every `blocking.rs` line cited below was re-verified
  against `bfaae8f3`).
* Third input, not previously in scope: `phase6/pva-rtems-dep-gate` @ `1906c7cb`
  in the main checkout — six commits, items 1+2+3. **Not an ancestor of either
  worktree.** It changes the answer to three of the five questions.

`main` is at `5cbc5313`. `git merge-base --is-ancestor` says none of
`1906c7cb` (dep-gate), `bfaae8f3` (CA), `93590517` (pva-cs-ring) is an ancestor
of it — all three are unmerged, which is what §7 decision 4 is about.

**Naming note (2026-07-25).** The target IOC binaries were later renamed —
`rtems-ca-ioc` → `realtime-ca-ioc`, `rtems-pva-ioc` → `realtime-pva-ioc`.
Every old name below is left exactly as captured, because this file is a
record of the tree as it stood, not a description of it as it stands.

---

## §0 Headline — the item as written cannot be the first stage

Item 7 is described as "put the PVA accept loop / UDP responder / beacon on the
CA backend". The evidence says the accept loop is not the blocker. Three
findings, in descending order of impact on the plan:

1. **The RTEMS build currently has no PVA server at all — by design.**
   `phase6/pva-rtems-dep-gate` commit `8f12cf30` gates `server_native::{tcp, udp,
   runtime, peers}`, `server::pva_server`, `server::iocsh`, `server::run_pva_ioc`
   and `config::Config` out of `target_os = "rtems"` (see the diff of
   `server_native/mod.rs`, and `config/mod.rs` in `73d3ec39`). Its own commit
   message names item 7 as the owner of the replacement. So item 7 is not
   "swap a backend under a compiled module" — it is "supply the module that the
   dep gate deliberately left absent".

2. **`tcp.rs` is 21,656 lines and its entire socket surface is 8 lines.**
   `rg -n "tokio::net|socket2|tokio_rustls|TcpListener"` over `tcp.rs` returns
   `:20`, `:2455`, `:2467`, `:2476`, `:2492`, `:2552`, `:2553`, and one comment
   at `:2640` — **all inside `run_tcp_server` / `run_tcp_server_on_listener`
   (`tcp.rs:2432-2727`)**. `handle_connection_io` (`tcp.rs:3242`) takes
   `SrvRead`/`SrvWrite`, which are `Box<dyn tokio::io::AsyncRead/AsyncWrite +
   Unpin + Send>` (`tcp.rs:3120-3121`) — trait objects, no sockets. The
   host-only gate on `pub mod tcp` is therefore ~300 lines of justification
   applied to 21,656 lines of code, and it takes `handle_connection_io` — the
   thing **item 5** needs compiled on RTEMS — out with it.

   → A `tcp.rs` split is a **shared prerequisite of items 5 and 7**, not a
   sub-step of either. It is also the cheapest stage in this document and the
   only one that is hosted-neutral off main today (§6 Stage A).

3. **The beacon sender has no CA reference implementation.** CA's blocking
   driver states its own scope as "name-search reply only. No beacons, no
   multicast / broadcast-secondary socket" (`blocking.rs:21`, `:249`), and
   `rtems-ca-ioc.rs:145-163` starts exactly two threads, `CAS-TCP` and
   `CAS-UDP`. Every beacon line in item 7 is PVA-new. The beacon is also the
   only part of item 7 that is genuinely BSP-blocked (§5).

---

## §1 Prerequisite state: what the dep-gate branch already did

Because this branch is not an ancestor of `pva-cs-ring`, all of its work is
invisible from the item-5 worktree. Enumerated so the plan does not re-do it:

| commit | item | effect relevant to item 7 |
|---|---|---|
| `1d5476df` | 1 | TLS behind default-on `tls` feature; 3 gated sites, two in `server_native::tcp` |
| `24d514e8` | 2 | `client_native::decode` → `crate::decode`; `client_native` behind default-on `client` feature |
| `bc7c8f53` | 3 | `socket2` + `if-addrs` → `cfg(not(rtems))` deps; tokio split per-target, RTEMS drops `net`/`signal`/`process` |
| `8f12cf30` | 3 | `server_native::{tcp,udp,runtime,peers}` + `server::pva_server` + `leaf_convert::pv_leaf_to_epics_value` gated host-only |
| `73d3ec39` | 3 | `config::{Config, list_broadcast_addresses}` gated host-only — **explicitly names item 7 as owing the RTEMS interface enumerator** |
| `1906c7cb` | 3 | `auth::plain` primary-group fallback where newlib lacks `getgrouplist` |

Two consequences the plan must carry:

* On RTEMS, tokio is built with `rt/rt-multi-thread/time/sync/macros/io-util/
  io-std/fs/parking_lot` and no `net` (base's mirror of the same split:
  `epics-base-rs/Cargo.toml:49-50`, commit `0f1349b1`). So
  `tokio::io::{AsyncRead, AsyncWrite}` (`io-util`) and `tokio::sync::mpsc`
  (`sync`) — everything `handle_connection_io` and the writer path use — **do**
  compile on RTEMS. Only `tokio::net` does not.
* `epics_base_rs::net` is gated out of RTEMS wholesale (`0207a423`:
  `#[cfg(not(target_os = "rtems"))] pub mod net;`). `AsyncUdpV4`, `IfaceMap`,
  `bind_loopback_mcast`, `enable_so_rxq_ovfl_for_socket`,
  `recv_from_with_drop_count_socket` — the entire import list at
  `server_native/udp.rs:13-16` — are absent on the target. PVA's UDP responder
  cannot be ported; it must be **re-implemented against `std::net::UdpSocket`**,
  which is what CA did.

---

## §2 Question 1 — the accept-path surface, with capture sets

### 2.1 `run_tcp_server_on_listener` (`tcp.rs:2474-2727`)

Signature at `tcp.rs:2474-2484`: `(source: DynSource, listener: TcpListener,
config: PvaServerConfig, peers: Arc<PeerRegistry>, channel_invalidator:
ChannelInvalidator) -> PvaResult<()>`.

| site | today | under a blocking driver |
|---|---|---|
| `:20` `use tokio::net::TcpListener` | reactor listener | `std::net::TcpListener`; CA `blocking.rs:297-298` holds it as a plain struct field |
| `:2487` `active = Arc<AtomicUsize>` | per-conn admission counter | kept as an `Arc<AtomicUsize>` (target-neutral), but **no longer an admission counter** — see the gate row below. It is the report behind `active_connections` (`blocking.rs:763`); the count that admits is the pool's. |
| `:2489-2492` `tls_acceptor` | `tokio_rustls::TlsAcceptor::from(...)` | absent on RTEMS — §4 |
| `:2501` `conn_tasks: JoinSet<()>` | reaps finished conn tasks via `join_next()` | **no analogue.** CA keeps no join handle for a client: `blocking.rs:454-478` borrows the client's two threads from `CAS_CLIENT_POOL` and dispatches the body with `Worker::run_detached`, which nobody joins. The reap arm exists only to bound `JoinSet` growth, and the pool needs no reaping — a set returns itself when its lease drops and its jobs finish (`doc/rtems-connection-worker-pool-design.md` §7). *(Before that conversion CA spawned a detached `thread::Builder::new().name(format!("CAS-client-blocking {peer}"))` per client; the conclusion is unchanged, the mechanism is not.)* |
| `:2503-2512` `select!{ biased; listener.accept(), conn_tasks.join_next() }` | two-arm select | one blocking `for stream in listener.incoming()` (CA `blocking.rs:420`) |
| `:2523-2532` `active.fetch_add` / `max_connections` gate | admission | **gate deleted.** As built (`blocking.rs:847` `self.conn_pool.acquire()`), admission is the worker pool refusing: the pool's capacity *is* `max_connections`, so a full pool answers `WouldBlock` and there is no second count to keep alongside it. `active.fetch_add` survives as a *report* only (`:763` `active_connections`). The plan's "moves above the thread spawn" was right about the position and wrong about the mechanism — there is no thread spawn to move above. |
| `:2533-2538` capture set | `src, cfg, active_dec, acceptor, peers_for_task, conn_invalidator` | identical `move` set into the pooled `run_detached` closure (the plan said `thread::spawn`; the bound is the same `FnOnce + Send + 'static` either way, which is why this row survived the change); every member is `Clone + Send + 'static` already (`DynSource` is `Arc`, `PvaServerConfig: Clone`, `PeerRegistry` is `Arc`, `ChannelInvalidator` is a channel handle). **No capture needs reshaping** — the closure body is the only thing that changes. |
| `:2539` `conn_tasks.spawn(async move {...})` | task | **no spawn.** As built (`blocking.rs:903`), the connection body is dispatched onto the `conn` worker of the set already borrowed at `:847` — `conn_worker.run_detached(format!("PVA connection {peer}"), move \|\| ...)`. The plan's per-connection `thread::Builder::new().name(format!("PVAS-client {peer}")).spawn(...)` is exactly the creation the worker pool was built to remove (176–179 B of RTEMS residue per creation, never returned); the thread exists already and carries the roster's name, `PVAS-conn <n>`. |
| `:2540` `stream.set_nodelay(true)` | tokio | `std::net::TcpStream::set_nodelay` — same call, portable |
| `:2552-2553` `socket2::SockRef` + `TcpKeepalive::new().with_time(15s).with_interval(5s)` | socket2 | socket2 is a `cfg(not(rtems))` dep (`bc7c8f53`). Raw `setsockopt(SO_KEEPALIVE / TCP_KEEPIDLE / TCP_KEEPINTVL)` — all three constants **present** for newlib (§5.1) — or drop keepalive on RTEMS (it is documented at `:2545-2549` as defence-in-depth over the ECHO heartbeat, which item 6 folded into the read loop and which is target-neutral). |
| `:2583` `timeout(PEEK_WINDOW=100ms, stream.peek(&mut b))` | TLS first-byte dispatch | vanishes with TLS (§4). If TLS ever returns: `set_read_timeout(100ms)` + `MSG_PEEK` `recv`. Note `std::net::TcpStream` has **no** `peek` with a timeout in one call, but `set_read_timeout` + `peek` composes. |
| `:2634` `timeout(cfg.tls_handshake_timeout, a.accept(stream))` | handshake deadline | vanishes with TLS (§4) |
| `:2662-2711` two `handle_connection_io(...)` call sites (TLS / plain) | `Box::new(r)`, `Box::new(w)` from `tokio::io::split` | **the join point with item 5.** The blocking driver supplies the same two boxes from its reader/writer thread adapters. Nothing in `handle_connection_io`'s signature (`tcp.rs:3242-3249`) changes. |
| `:2714-2719` `active_dec.fetch_sub` + `peers_for_task.remove(peer)` | teardown | unchanged; on a thread it is straight-line code, no drop-guard needed |
| `:2722-2723` `error!("accept error"); tokio::time::sleep(50ms)` | accept-error backoff | `std::thread::sleep(50ms)`. CA has **no** backoff — `blocking.rs:180-227` logs and `continue`s. The 50 ms is worth keeping (an `EMFILE` storm on a single-core BSP is worse, not better). |

### 2.2 `server_native/runtime.rs` spawn sites

| site | today | under a blocking driver |
|---|---|---|
| `:850-856` | `std::net::TcpListener::bind` → `set_nonblocking(true)` → `tokio::net::TcpListener::from_std` | drop `set_nonblocking` + `from_std`; keep the std listener. The synchronous-bind-before-spawn discipline (`:945-953`, "with `udp_port = 0` the kernel picks the number") already matches CA's `BlockingCaServer::bind` (`blocking.rs:150`) — port ownership is established before any thread starts. |
| `:902-907` dedicated TLS listener + ephemeral fallback | second listener | absent on RTEMS (§4) |
| `:956-960` `bind_udp(config.udp_port, &udp_interfaces, &config.beacon_destinations)` | returns `(AsyncUdpV4, u16)` (`udp.rs:129-133`) | **PVA-new.** `AsyncUdpV4` is a per-NIC socket *bundle* (`net/async_udp_v4.rs:112-114`, `sockets: Vec<NicSocket>`) and is absent on RTEMS. Replaced by CA's single-socket `bind_udp_search` shape (`blocking.rs:271`) — see §3.3 for what that costs. |
| `:962-975` `tokio::spawn(run_udp_responder_on_socket(...))`, 13 args | one task | `thread::Builder::new().name("PVAS-UDP")` running a blocking responder with the same 13 values captured by move |
| `:983-990` `tokio::spawn(run_udp_responder_v6(...))`, gated on `enable_ipv6_udp` | optional v6 task | out of scope for item 7: default-off (`runtime.rs:982`), and `ipv6_mreq` is absent from newlib (§5.1). Recommend: `cfg(not(rtems))`, documented, not silently dropped. |
| `:1007-1031` `tokio::spawn(async move { JoinSet over tcp_listeners })` supervisor | one supervisor task owning N accept loops | N accept threads + a supervisor that owns their `JoinHandle`s. The "first `Err` ends the service" semantics (`:1021`) needs an explicit error channel; `thread::JoinHandle` has no `join_next`. |
| `:573-611` `PvaServer` fields: three `Option<JoinHandle>` + three `AbortHandle` | `Drop` at `:615-639` calls `abort()` on each | **no thread analogue of `abort()`.** This is the same gap item 5 flagged for the connection scope. CA's answer: `AtomicBool shutdown` + a self-connect to wake the blocked `accept()` (`blocking.rs:228-247`), and a 200 ms `set_read_timeout` cap on the UDP loop so it observes the flag between datagrams (`blocking.rs:443-446`). Both are directly transplantable. |

### 2.3 `server_native/udp.rs` internal spawn

`udp.rs:500` `tokio::spawn` for the beacon emitter, guarded by a locally-defined
`AbortOnDrop` at `:487-493` and bound at `:640`. Under a blocking driver: a
third named thread (`PVAS-beacon`) plus the same shutdown `AtomicBool`. Its body
(`:501-639`) awaits exactly two things — `tokio::time::sleep(cur_period)` at
`:538` and `beacon_source.list_pvs().await` at `:542` — the latter being the same
"resolves on the first poll for a local source" shape CA drives with
`block_on_sync` (`blocking.rs:493-505`). So the beacon body is `park_on`-able;
only its **sends** (`:608-618`: `send_multicast_v4`, `fanout_to`, `send_to` on
`AsyncUdpV4`, plus `s6.send_to` on a `tokio::net::UdpSocket`) are not.

---

## §3 Question 2 — CA's `blocking.rs`: verbatim / CA-specific / PVA-new

`blocking.rs` is 2,377 lines. Classified by what item 7 can take.

### 3.1 Reusable verbatim (pattern, not code — the two crates share no module)

| CA site | what it gives item 7 |
|---|---|
| `blocking.rs:139-148` `struct BlockingCaServer { listener, db, acf, tcp_port, shutdown: AtomicBool }` | the driver struct shape: own the listener, own the shutdown flag, no runtime |
| `:150-175` `bind()` | build ⟹ listening. Matches PVA's existing synchronous-bind discipline (`runtime.rs:945-953`) |
| `:176-227` `serve()` | `for stream in self.listener.incoming()` + shutdown check + named thread per client |
| `:228-247` `shutdown()` | `AtomicBool` then `TcpStream::connect(self_addr)` to wake the blocked `accept()`. **The whole answer to `PvaServer::Drop`'s missing `abort()`** |
| `:271-340` `bind_udp_search` / `bind_udp_search_socket` / `set_reuse_opt` | raw-`libc` `socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)` + SO_REUSEADDR/SO_REUSEPORT + zeroed `sockaddr_in` + `bind`, then `UdpSocket::from_raw_fd`. Directly transplantable |
| `:341-362` `FIONREAD_REQUEST` | the constant, its derivation, and the honest "pending on-target runtime verification" framing (§5.2) |
| `:363-397` `pending_bytes` + the `#[cfg(not(unix))]` arm | FIONREAD with a fail-open contract: any ioctl error ⟹ flush |
| `:434-446` the 200 ms `set_read_timeout` shutdown-observation cap | how a blocking UDP loop stays stoppable |
| `:560-566` `is_read_timeout` | `WouldBlock | TimedOut` ⟹ the timeout fired, not an error |
| `:568-576` `write_frame_locked` / `:578-590` `drain_outbox_locked` | writes serialized under one `Mutex<TcpStream>` send lock. PVA already serializes through the writer task's mpsc (`tcp.rs:3277`), so this is item 5's business, not item 7's |
| `bin/rtems-ca-ioc.rs:1-190` | the entry-point template: `background_init()` → `block_on_sync(builder.build())` → bind → two named threads → `join`. `rtems-pva-ioc` is this file with PVA nouns |

### 3.2 CA-specific — do not port

| CA site | why it does not carry |
|---|---|
| `:398-405` `send_udp_reply`, `:407-427` `flush_held_batch` | shaped around `SearchReplyBatch`, a CA-protocol accumulator (`server/udp.rs`, factored in `ad477153`). PVA has no batch type |
| `:447-527` the same-source coalescing loop | CA coalesces because C `cast_server.c:268-281` does. pvxs does **not** coalesce SEARCH replies — `udp.rs:1048-1053` sends one reply per datagram. Porting the FIONREAD batch to PVA would be a wire deviation, not parity. FIONREAD is therefore **not needed by item 7** (contrast §5.2) |
| `:528-552` `command_drives_without_spawn` allowlist | CA command dispatch; PVA's operation dispatch is `handle_op` in `tcp.rs` |
| `:592-605` `BlockingSub` | CA EVENT_ADD/EVENT_CANCEL bookkeeping. PVA's monitor path went through `MonitorStream` in item 4 |
| `:607-...` `handle_client_blocking` | CA circuit state machine. **PVA's equivalent is item 5, not item 7** |

### 3.3 PVA-new — no CA reference exists

| what | why CA has none | size signal |
|---|---|---|
| **Beacon sender** | CA's blocking driver explicitly excludes beacons (`blocking.rs:21`, `:249`); `rtems-ca-ioc.rs` starts no beacon thread | `udp.rs:501-639`, ~140 lines of body, plus a new send path |
| **Multi-NIC send** | CA's blocking UDP binds ONE `INADDR_ANY` socket (`blocking.rs:271`). PVA replies via `send_via(resp, dest, reply_iface_ip)` (`udp.rs:1050`) — a *per-NIC-socket* send, so the reply leaves the interface the SEARCH arrived on | replacing `AsyncUdpV4` (1,680 lines in `net/async_udp_v4.rs`) with a blocking per-NIC bundle, or accepting a single-socket deviation (§7 open decision 1) |
| **Socket-free search core** | CA factored one in `ad477153` (`parse_search_datagram`, `SearchReplyBatch`, `shape_search_reply_dg`, `send_reply_dg`; 365+/312- in `server/udp.rs`). **PVA has not.** `process_search_datagram` (`udp.rs:908-919`) takes `socket: &AsyncUdpV4` and `lo_mcast: Option<&Arc<tokio::net::UdpSocket>>` and sends inline at `:974`, `:1048`, `:1050`, `:1053` | the PVA analogue of `ad477153`; ~400 lines reshaped, no behaviour change, hosted-neutral (§6 Stage B) |
| **ORIGIN_TAG loopback multicast collector** | no CA equivalent | `udp.rs:657-668` binds it via `bind_loopback_mcast` (in `epics_base_rs::net`, gated out). `IP_ADD_MEMBERSHIP` and `ip_mreq` are **present** for newlib (§5.1), so this is portable — but it is a pvxs-parity nicety for co-resident PVA peers, which an embedded IOC has none of. Recommend documented omission |
| **SO_RXQ_OVFL drop accounting** | no CA equivalent | `udp.rs:659`, `:672`, `:1130`. `SO_RXQ_OVFL` is **absent** from newlib and is Linux-only in base already (`epics-base-rs/Cargo.toml:66`, libc linked on Linux only). Omit on RTEMS; `recv_from` returns `drops = 0`, which is the documented non-Linux behaviour (`net/async_udp_v4.rs:894`, `:934`) |
| **`build_beacon`** | — | `udp.rs:1392-1400`: `(guid, tcp_port, order, sequence, change_count, protocol) -> Vec<u8>`. **Already socket-free.** Reusable as-is |

---

## §4 Question 3 — TLS on RTEMS

**Answered by measurement, and the answer is "not on the branch item 7 would
build from".**

* `git merge-base --is-ancestor 1d5476df HEAD` in `pva-cs-ring` → **absent**.
  On `phase6/pva-channelsource-ring`, `rustls`, `rustls-pemfile`, `tokio-rustls`
  and `x509-parser` are all **non-optional** dependencies of `epics-pva-rs`
  (`Cargo.toml`), and `[features] default = ["pkcs12"]` has no `tls` member.
* On `phase6/pva-rtems-dep-gate` @ `1d5476df` they are optional behind a
  default-on `tls` feature. That commit's own message names the gated sites:
  *"the `TlsAcceptor` construction plus the TLS accept arm in
  `server_native::tcp`, and `ServerConn::connect_tls` plus its one dispatch arm
  in `client_native`"* — verified there by `cargo tree --no-default-features -e
  normal -i ring` returning nothing.

So:

1. **Does item 1 already exclude the handshake path from an RTEMS build?**
   Yes — *on `phase6/pva-rtems-dep-gate`*. `tcp.rs:2489-2492` (`TlsAcceptor`
   construction), `:2583` (first-byte peek), `:2634` (handshake deadline) and
   the TLS branch of the `match (acceptor, is_tls_client)` at `:2660` all sit
   inside `#[cfg(feature = "tls")]`, and `let tls_acceptor = config.tls.clone();`
   stands in with the feature off.

2. **Must item 7 handle it?** Only insofar as item 7 must be *built on that
   branch*. Item 7 must not re-derive a TLS gate; it must not build on
   `pva-channelsource-ring` either, because there `tokio-rustls` → `tokio/net` →
   `mio`, which does not cross to `armv7-rtems-eabihf` (`bc7c8f53`: mio 29
   errors). That is a **merge-ordering constraint, not an engineering task**.

3. **Residual item-7 work on the TLS path:** two lines. `runtime.rs:902-907`
   binds a *dedicated TLS listener* with an ephemeral fallback, and
   `runtime.rs:939-944` computes `bound_tls_port`. Neither is inside `tcp.rs`,
   so `1d5476df`'s three-site gate does not cover them — they need the same
   `#[cfg(feature = "tls")]` when the blocking driver reimplements the bind
   sequence. Flagged, unverified against `1d5476df`'s final `runtime.rs` state
   because that commit's stat lists `server_native/tcp.rs` and not `runtime.rs`.

---

## §5 Question 4 — the raw-libc subset, and where the BSP boundary is

### 5.1 Symbol audit — corrected

An earlier sweep of this audit searched only
`libc-0.2.187/src/unix/newlib/**` and reported `socket`/`setsockopt`/
`getsockopt` absent. That was a false negative: they are declared in the
**shared** `src/unix/mod.rs` extern block with no newlib exclusion
(`socket`:804, `connect`:815, `listen`:821, `accept`:829, `getsockname`:846,
`setsockopt`:850, `shutdown`:890, `close`:1084, `getsockopt`:1427). Re-run over
both trees:

**Present** (usable today, no BSP work):

* fns — `socket`, `bind`, `connect`, `listen`, `accept`, `setsockopt`,
  `getsockopt`, `getsockname`, `ioctl`, `recvfrom`, `sendto`, `shutdown`,
  `close`
* constants — `AF_INET`, `AF_INET6`, `SOCK_DGRAM`, `SOCK_STREAM`,
  `IPPROTO_UDP`, `IPPROTO_IP`, `IPPROTO_IPV6`, `SOL_SOCKET`, `SO_REUSEADDR`,
  `SO_REUSEPORT`, `SO_BROADCAST`, `SO_RCVTIMEO`, `SO_SNDTIMEO`, `SO_KEEPALIVE`,
  `TCP_NODELAY`, `TCP_KEEPIDLE`, `TCP_KEEPINTVL`, `IP_ADD_MEMBERSHIP`,
  `IP_MULTICAST_IF`, `IP_MULTICAST_TTL`, `IP_MULTICAST_LOOP`, `IPV6_V6ONLY`,
  `IPV6_MULTICAST_HOPS`, `IPV6_MULTICAST_IF`, `IPV6_JOIN_GROUP`
* types — `sockaddr_in`, `sockaddr_in6`, `in_addr`, `ip_mreq`

(`IPPROTO_UDP` was also a false negative in the earlier sweep — it is in
`unix/mod.rs`, not the newlib subtree. CA already calls it: `blocking.rs`
`libc::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)`.)

**Absent** — this is the BSP boundary:

| symbol | needed by | consequence |
|---|---|---|
| `FIONREAD` | CA's reply coalescing | **item 7 does not need it** (§3.2 — pvxs does not coalesce). CA supplies it as a local constant with a derivation and a fail-open contract (`blocking.rs:341-362`) if it is ever wanted |
| `SO_RXQ_OVFL` | UDP drop accounting (`udp.rs:659`, `:672`, `:1130`) | omit; Linux-only already |
| `ipv6_mreq` | v6 multicast join | v6 responder out of scope (default-off, `runtime.rs:982`) |
| `SIOCGIFCONF`, `SIOCGIFADDR`, `SIOCGIFFLAGS`, `ifconf`, `ifreq`, `getifaddrs` | **NIC enumeration for beacon destinations and per-NIC search reply** | **the one real blocker.** No workaround inside libc |

### 5.2 The NIC-enumeration blocker — stop here, do not guess

Three call paths need live interface enumeration, and none of them has a
degraded-but-honest fallback:

* `config::env::list_broadcast_addresses(udp_port)` (`udp.rs:434`) — the
  auto-beacon destination list when `beacon_destinations` is empty and
  `auto_beacon` is on. `73d3ec39` gated `Config` out of RTEMS **rather than
  ship a server that silently never beacons**, and its message states the debt
  in exactly these terms.
* `local_v4_addrs()` (`udp.rs:1538-1550`, `if_addrs::get_if_addrs()`) — the
  origin-locality check for the ORIGIN_TAG path.
* `AsyncUdpV4`'s per-NIC bundle (`net/async_udp_v4.rs:173` `bind_with_map`,
  `net/iface_map.rs:255` `enumerate_v4`) — which NIC to bind and which to reply
  through.

`73d3ec39`'s own assessment, which this scoping confirms rather than extends:
*"newlib's libc bindings expose `ioctl` but no ifreq/ifconf/SIOCGIFCONF, and
there is no RTEMS BSP on this machine to verify that ABI against."*

**What is needed, stated as headers/symbols rather than a design:**

* `<sys/sockio.h>` (RTEMS/BSD tree) — `SIOCGIFCONF`, `SIOCGIFADDR`,
  `SIOCGIFBRDADDR`, `SIOCGIFFLAGS`, `SIOCGIFNETMASK`
* `<net/if.h>` — `struct ifconf` (`ifc_len`, `ifc_buf`/`ifc_req` union),
  `struct ifreq` (`ifr_name[IFNAMSIZ]`, `ifr_addr`/`ifr_flags` union),
  `IFNAMSIZ`, `IFF_UP`, `IFF_LOOPBACK`, `IFF_BROADCAST`
* alternatively `<ifaddrs.h>` from **libbsd** (not newlib) — `getifaddrs`,
  `freeifaddrs`, `struct ifaddrs`. This is the route C EPICS takes: base's
  `osdNetIfConf.c` (cited at `net/iface_map.rs:216-217`) does the `SIOCGIFCONF`
  walk with the `IFF_UP` / `IFF_LOOPBACK` filter this Rust code mirrors.

The struct **layouts** (field order, padding, `ifc_len` semantics, the
variable-stride `ifreq` array walk) cannot be derived from the libc crate and
must be read off the target's headers. **No RTEMS BSP is present on this
machine** (this scoping did not find one; the reference-source rule applies —
ask rather than assume). Item 7 must therefore either:

(a) obtain a BSP and write an `ioctl(SIOCGIFCONF)` walk verified against its
    headers, or
(b) accept **statically configured** interfaces on RTEMS — `EPICS_PVAS_INTF_ADDR_LIST`
    / `EPICS_PVAS_BEACON_ADDR_LIST` required, no auto-expansion — and make the
    absence loud at startup rather than silent. This is defensible for an
    embedded IOC (the NIC set is fixed at build time) and is the only option
    that is verifiable without hardware.

Recommendation: **(b) as the shipped behaviour, (a) as a later increment**, with
the "auto beacon requires an explicit address list on RTEMS" refusal written as
an error at startup. That keeps `73d3ec39`'s standard — absent rather than
present-and-wrong.

---

## §6 Question 5 — staged plan

Each stage is independently workspace-green. "Hosted-neutral off main" means it
can land on `main` today with no RTEMS involvement and no behaviour change.

| # | stage | where it can land | size |
|---|---|---|---|
| **A** | **Split the accept loop out of `tcp.rs`.** Move `run_tcp_server`, `run_tcp_server_with_peers`, `run_tcp_server_on_listener` (`tcp.rs:2432-2727`) into a new `server_native/accept.rs`; `tcp.rs` keeps `handle_connection_io` and loses its last 8 socket lines. Re-point the existing `#[cfg(not(rtems))]` gate from `pub mod tcp` to `pub mod accept`, and ungate `peers` (`rg` shows **zero** `tokio::net`/`socket2`/`tokio::time`/`tokio::spawn` in `peers.rs`, so `8f12cf30`'s reasoning — "every reader and writer is a TCP connection" — is about *users*, not deps). | **hosted-neutral off main.** The cfg re-point half must land on `phase6/pva-rtems-dep-gate`. | **S** — ~300 lines moved, 4 cfg lines. 1–2 d |
| **B** | **Factor the UDP search decode/respond core socket-free** — the PVA analogue of CA `ad477153`. `process_search_datagram` (`udp.rs:908`) loses `socket`/`lo_mcast` and returns a list of `(dest, iface_hint, bytes)`; the four inline sends (`:974`, `:1048`, `:1050`, `:1053`) move to the caller. Wire-golden tests must pass unchanged. | hosted-neutral off main | **M** — ~400 lines reshaped. 3–4 d |
| **C** | **Blocking accept driver** — `server_native/blocking.rs`: `BlockingPvaServer { listener: std::net::TcpListener, source, config, peers, shutdown: AtomicBool }`, `bind`/`serve`/`shutdown` mirroring `blocking.rs:139-247`, thread-per-client, calling `handle_connection_io` with item 5's `SrvRead`/`SrvWrite` adapters. | **blocked on the CA-branch merge** (needs `runtime::task` on the executor backend) **and on item 5** (supplies the two adapters) | **M** — ~350 lines. 4–5 d |
| **D** | **Blocking UDP search responder** — one `std::net::UdpSocket` from CA's raw-libc `bind_udp_search_socket` pattern (`blocking.rs:271-340`), 200 ms `set_read_timeout` shutdown cap, driving stage B's socket-free core with `block_on_sync`. No FIONREAD, no coalescing (§3.2). | blocked on B + the CA merge | **M** — ~250 lines. 3–4 d |
| **E** | **Beacon thread** — `build_beacon` (`udp.rs:1392`, already socket-free) + the burst/steady cadence and `change_count` logic from `udp.rs:501-639` on a `PVAS-beacon` thread with `std::thread::sleep` and `block_on_sync(list_pvs())`. Destinations from an explicit address list only (§5.2 option b). | blocked on C/D | **M** — ~200 lines. 3 d |
| **F** | **RTEMS interface enumerator** behind `list_broadcast_addresses_on` — closes `73d3ec39`'s debt and lets E auto-expand. | **BSP-blocked.** Not schedulable until a BSP is available | **L**, unsized — the `ifconf` walk plus on-target verification |
| **G** | **`rtems-pva-ioc` entry point** — `bin/rtems-ca-ioc.rs` with PVA nouns: `background_init()` → database → bind → `PVAS-TCP` / `PVAS-UDP` / `PVAS-beacon` threads → join. | blocked on C+D+E | **S** — ~200 lines. 1–2 d |

**Ordering constraint the plan must respect:** stage A is a prerequisite of
**item 5**, not just item 7 — under the current gate, `handle_connection_io` is
not compiled on RTEMS at all, so item 5 has nothing to attach its adapters to.
A should be pulled ahead of item 5's stage 3.

**Revised size estimate:** A+B (hosted-neutral, unblocked) ≈ **1 week**.
C+D+E+G (blocked on the CA merge and item 5) ≈ **2.5–3 weeks**. F is unsized and
BSP-blocked. Total schedulable: **3.5–4 weeks**, versus the roadmap's framing of
item 7 as a single unit.

---

## §7 Open decisions for sign-off

1. **Per-NIC replies on RTEMS.** PVA replies to a SEARCH through the interface
   it arrived on (`udp.rs:1050` `send_via(resp, reply_dest, reply_iface_ip)`,
   falling back to `send_to` at `:1053`). A single `INADDR_ANY` socket — CA's
   blocking shape — replies through the routing table instead. On a
   single-NIC embedded target these are identical; on multi-homed they are not.
   **Recommend:** single socket, documented as an RTEMS deviation, with the
   per-NIC bundle owed to stage F. The alternative is re-implementing
   `AsyncUdpV4` (1,680 lines) blocking-side before any of item 7 works.

2. **Auto-beacon on RTEMS** — §5.2 (a) BSP `ifconf` walk vs (b) explicit
   address list required, loud refusal otherwise. Recommend (b) now, (a) later.

3. **TCP keepalive** (`tcp.rs:2552-2553`, socket2). Raw `setsockopt` with the
   three present constants, or omit on RTEMS given item 6's folded ECHO
   heartbeat already covers inactivity? Recommend raw `setsockopt` — the
   constants are there and it is ~10 lines.

4. **Merge ordering.** Item 7 cannot be built on `phase6/pva-channelsource-ring`
   (TLS deps are non-optional there → `mio` → RTEMS build failure). It needs
   `phase6/pva-rtems-dep-gate` merged first, then the CA branch. Three unmerged
   branches are now on item 7's critical path: `manual-ca-sans-io`,
   `pva-channelsource-ring`, `pva-rtems-dep-gate`.
