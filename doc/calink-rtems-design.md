# `ca://` record links and the CA client on the RTEMS target

**Status:** design only. No production code changed by this document. The
probes below were `cargo check` invocations over a temporary un-gate of
`epics-ca-rs/src/lib.rs`, reverted from a byte-identical backup before this
file was written; `git status --porcelain` is empty at the commit that
carries it, and `./scripts/rtems-check.sh` is green after the revert (§1.5).
**Scope:** the CA half of the gap `doc/pvalink-rtems-design.md` §3.3 named —
*"the functional gap on the target IOC is **both** link schemes, not just
`pva://`"*. That document designed the `pva://` half; this one designs the
`ca://` half, and the two tracks share seams that must be built once (§6).
**Base:** `8c305c37` (*Merge pvalink-rtems-design: measured design for
`pva://` links on RTEMS (stage 5)*).
**C reference:** EPICS base at `/home/stevek/work/epics-base` (paths below
are relative to that root).
**Sibling document:** `doc/pvalink-rtems-design.md`. Where the two designs
share infrastructure — the byte-source seam, the spawn seam, the callback-band
invariant, the on-target topology, the stack-class table — this document
**cites** it rather than re-deriving, and says explicitly where CA differs.

Every number here was produced by running the command quoted next to it on
this tree at `8c305c37`. Where a claim could not be measured it says so and
lands in §7, not in the body.

---

## 0. The reframing, and the numbers that produce it

`doc/pvalink-rtems-design.md` §3.3 recorded the CA client as an unmeasured
peer of the PVA client, and its §6 item 8 listed *"whether the CA client is
otherwise as transport-erased as the PVA client … was **not** measured"* as
UNVERIFIED. This document measures it. The answer is **more** transport-erased,
by every metric the sibling document used:

| metric | PVA client (`client_native`) | **CA client (`client`)** | source |
|---|---:|---:|---|
| target compile errors | 47 | **18** (probe A) / **29** (probe B) | §1.1 |
| of those, **primary** (non-cascade) | 29 | **7** | §1.3 |
| production bare `tokio::spawn` | 12 | **1** | §5.1 |
| production spawns already on `runtime::task::spawn` | 0 | **17** | §5.1 |
| task-handle fields typed as the seam alias | 0 | **all of them** | §5.1 |
| connection state machine transport-erased | yes — boxed `DynRead`/`DynWrite` | **yes — generic `R: AsyncRead` / `W: AsyncWrite`** | §1.6 |
| TCP-only name resolution implemented | yes (`EPICS_PVA_NAME_SERVERS`) | **yes (`EPICS_CA_NAME_SERVERS`)** | §4 |
| search frame needs a `local_addr` readback | **yes** (blocks UDP on target) | **no** | §4.4 |
| one shared client per IOC (C parity) | **no** — one per link, a defect | **yes** — `OnceCell`, already correct | §2.4 |

Two of those rows carry the whole design.

**The CA client is already on the spawn seam.** The sibling document's stage 3
— *"convert the 12 production `tokio::spawn` in `client_native` … to
`runtime::task::spawn`"* — has, on the CA side, already happened. Measured
(§5.1): 17 production spawns go through `epics_base_rs::runtime::task::spawn`,
every task-handle field is `runtime::task::TaskHandle<T>` rather than a bare
`tokio::task::JoinHandle`, and exactly **one** bare `tokio::spawn` survives
(`client/mod.rs:1374`), inside a `Handle::try_current().is_ok()` guard in
`Drop`.

**calink does not have pvalink's stage-0 defect.** `CaLinkResolver` holds
**one** shared `CaClient` in a `tokio::sync::OnceCell` (`resolver.rs:268`,
`:288-296`), with an eager-seed injection seam `with_client`
(`resolver.rs:302-309`). That is C `dbCa` parity as written: C creates exactly
one client context, `ca_context_create` at `dbCa.c:1162`, stored in
`dbCaClientContext` (`dbCa.c:78`, `:1164`). So the sibling document's stage 0
— its "do this first", ~40 lines, the single largest RTEMS resource lever —
**has no CA equivalent to build.** Verified in §2.4.

What the CA side has instead, and pvalink does not, is a **band-occupancy
problem in the monitor callback** (§5.4): `run_monitor` calls
`dispatch_external_cp_targets`, which runs full synchronous record processing —
FLNK chains included — inline on the monitor task. pvalink's `on_event` does a
store and a non-blocking `try_send`. On RTEMS both land on the same
single-worker `cbMedium` band. That is the CA-specific risk this design must
answer, and it is the one place where CA is *harder* than PVA.

So the honest shape of the CA work is:

1. give the client a **blocking byte source** for its already-generic
   `read_loop`/`write_loop` — the same primitive stage 2 of the sibling
   document builds, which means it must be built **once, in `epics-base-rs`**
   (§3.3 — this is a correction to that document's stage 2);
2. make the search engine buildable with **no UDP socket** so it runs off
   `EPICS_CA_NAME_SERVERS` alone — measured viable, and cheaper than the PVA
   equivalent because no `local_addr` readback is involved (§4);
3. replace three `libc`/`socket2` call sites the CA **server** already solved
   in this same crate (§3.2) — including one, `libc::FIONREAD`, that is
   literally a second instance of a defect family the server closed with a
   named constant;
4. put `calink` on the spawn seam — the one place CA is *behind* the client
   it drives (§2.5).

---

## 1. The CA client's RTEMS failure surface, measured

### 1.1 The probes

`epics-ca-rs/src/lib.rs` gates the whole client front end out of the target
build. Measured, at the exact lines the brief cited:

```
rg -n 'mod calink|mod client|mod channel|mod discovery|mod repeater|mod hostname' \
   crates/epics-ca-rs/src/lib.rs
31:pub mod audit;
38:pub mod calink;          # preceded at :37 by #[cfg(not(target_os = "rtems"))]
43:pub(crate) mod channel;  # :42
46:pub mod cli;             # :45
48:pub mod client;          # :47
52:pub mod copt;            # :51
54:pub mod discovery;       # :53
59:pub mod hostname;        # :58
64:pub mod repeater;        # :63
```

with the block comment at `lib.rs:26-30`: *"The async CA client, discovery,
repeater, and CA-link resolver are the `tokio::net` host-only front-end …
The RTEMS build serves CA only through the `std::net` blocking server driver
(`server::blocking`); client-side connectivity and the discovery stack are a
later increment."*

**Probe A** — un-gate `client` and the two private modules it needs
(`channel`, `hostname`), nothing else:

```
cargo +nightly check --locked --no-default-features \
  -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf -p epics-ca-rs --lib
```

```
error: could not compile `epics-ca-rs` (lib) due to 18 previous errors
```

**Probe B** — additionally un-gate `discovery` and `repeater`, which
`client/mod.rs` names at `:29` (`use crate::repeater`) and `:456`/`:598`/`:606`/
`:682-684`/`:790`/`:799` (`crate::discovery`):

```
error: could not compile `epics-ca-rs` (lib) due to 29 previous errors
```

**Control** — the same feature selection on the host, unmodified tree:

```
cargo check --locked --no-default-features -p epics-ca-rs --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 17.81s     (exit 0)
```

Every one of the 18 / 29 is target-specific. Nothing here is a pre-existing
host defect.

### 1.2 Why there are two numbers, and why B > A is the interesting direction

`doc/pvalink-rtems-design.md` §1.2 argued that *"47 is a lower bound, and must
be reported as one"* — an unresolved import poisons its module and rustc
suppresses downstream type errors in code that names the poisoned items. That
argument was made from theory there, because no second probe existed to test
it.

Probe A → probe B **demonstrates it directly.** Un-gating two more modules
*removed* 8 errors (the `crate::discovery` path resolutions) and *added* 13
that had not previously been reported at all — the `E0277` `[u8]`-unsized
cascade at `client/search.rs:592` (×12) and `:648` (×1), inside the
`tokio::select!` body whose channel types were poisoned by `search.rs:7`
and `:10`.

So: **a smaller error count from a narrower un-gate is not better news.** The
18 of probe A is a lower bound on the 20 that belong to `client/` proper, and
both are lower bounds until a stage builds green. Stage 2's gate (§6) is what
converts this into a settled number.

### 1.3 The 29, classified by layer

Taken from `--message-format=json`, with each diagnostic's primary span walked
out through its macro-expansion chain so a `tokio::select!` arm is attributed
to *our* file rather than to tokio's `select.rs` — the same method as
`doc/pvalink-rtems-design.md` §1.2.

| layer | count | sites |
|---|---:|---|
| **newlib/`libc` gaps** (`FIONREAD` absent in the `libc` RTEMS bindings) | **1** | `client/transport.rs:113` |
| **`tokio::net`** (`TcpStream`, `UdpSocket`) | **3** | `client/search.rs:10`, `client/transport.rs:12`, `repeater.rs:5` |
| **`socket2`** | **6** | `client/transport.rs:991,992`, `hostname.rs:61,67`, `repeater.rs:102,400` |
| **`epics_base_rs::net::AsyncUdpV4`** (host-only in base) | **2** | `client/beacon_monitor.rs:7`, `client/search.rs:7` |
| **`if-addrs`-backed helpers in `epics_base_rs::net`** (host-gated `fn`s) | **4** | `repeater.rs:149,172,551,576` |
| **cascade** — `[u8]` unsized, all inside the `select!` body poisoned by the imports above | **13** | `client/search.rs:592` (×12), `:648` |

By rustc error code: 13 × `E0277`, 6 × `E0432`, 5 × `E0433`, 5 × `E0425`.

By file:

| file | errors | of which cascade |
|---|---:|---:|
| `client/search.rs` | 15 | 13 |
| `repeater.rs` | 7 | 0 |
| `client/transport.rs` | 4 | 0 |
| `hostname.rs` | 2 | 0 |
| `client/beacon_monitor.rs` | 1 | 0 |
| `client/mod.rs` | **0** | — |
| `client/subscription.rs`, `types.rs`, `state.rs`, `sync_group.rs`, `circuit_breaker.rs` | **0** | — |

**Seven primary errors in the client proper** — `search.rs:7`, `:10`;
`transport.rs:12`, `:113`, `:991`, `:992`; `beacon_monitor.rs:7`. That is the
number this design is about. `repeater.rs` (7) and `hostname.rs` (2) are
support modules whose disposition is a scoping decision, not a port (§1.4).

`client/mod.rs` — 6,456 lines, the client context, channel cache, coordinator
and the whole public API — reports **zero**, and unlike `ops_v2.rs` in the
sibling document that is *corroborated* rather than merely un-contradicted: a
source-text census over the same file finds zero `tokio::net`, zero `socket2`,
zero `libc::`, and every task handle already typed as the seam alias (§5.1).
It is still not proof (§1.2), and it is still §7 item 1.

### 1.4 What this means per subsystem

| subsystem | lines | verdict | evidence |
|---|---:|---|---|
| **Wire protocol** (`crate::protocol`) | 1,409 | **already on the target** | ungated in `lib.rs`; zero `tokio::`; `rtems-check.sh` green (§1.5) |
| **Channel state** (`channel.rs` — `AccessRights`, id allocators) | 102 | already portable | zero `tokio::`; gated only because *the client* is |
| **`estdlib`, `iocinf`, `observability`, `audit`, `cap_token`, `server/recv`** | 3,082 | already on the target | ungated, in the green gate |
| **Client context + channel cache + public API** (`client/mod.rs`) | 6,456 | **zero errors**; corroborated by source-text census | §1.3, §5.1 |
| **Subscription / types / state / sync_group / circuit_breaker** | 3,821 | **zero errors** | §1.3 |
| **Circuit framing** (`client/transport.rs`) | 4,343 | **transport-erased already** — 4 errors, all on 4 lines | `read_loop`/`write_loop` generic (§1.6); `TcpStream` at `:12` only |
| **Search engine** (`client/search.rs`) | 2,739 | **split** — the UDP arms are blocked, the TCP name-server path is not | `run_nameserver_connection` (`:731`) uses `TcpStream` only; §4 |
| **Beacon monitor** (`client/beacon_monitor.rs`) | 2,438 | **not portable as written**, and **not needed** for a record link | one error (`AsyncUdpV4`); §6 stage C1 defers it |
| **`repeater.rs`** | 39,111 B | out of scope for a link; a CA client can run without a local repeater | 7 errors; C's repeater is a separate process (`caRepeater`) |
| **`hostname.rs`** | 11,677 B | reverse-DNS for the peer-name cache; diagnostic only | 2 errors, both `socket2::SockAddr` |

The decisive line: the CA client's **protocol layer is shared with the CA
server**, not duplicated. All four client files import `use
crate::protocol::*` (`mod.rs:28`, `search.rs:13`, `transport.rs:15`,
`beacon_monitor.rs:10`), and `crate::protocol` is ungated, 1,409 lines, zero
`tokio::` — it compiles for `armv7-rtems-eabihf` today because the blocking
CA server needs it. The brief asked whether the client shares the sans-io
protocol types the server's refactor produced or duplicates them: **it shares
them.** The CA sans-io work (`391e94d9`, `5c5d7f1a`) extracted the server's
*reply-byte production* and *outbox ownership*; the wire types under both
sides were already one copy.

### 1.5 Baseline gate

```
./scripts/rtems-check.sh
RTEMS gate: every crate and target binary compiles for armv7-rtems-eabihf, in both
the portability and the image configuration.                              (exit 0)
```

Run **after** the probe revert, on the committed tree. Five crates × 2
configurations + 2 target binaries × 2 configurations. This is the gate every
stage in §6 extends.

### 1.6 The seam, three ways

`doc/pvalink-rtems-design.md` §1.5 put the PVA server driver and the proposed
PVA client side by side. The CA client belongs in that picture, one column
further along:

```
PVA server (shipped)          PVA client (proposed)         CA client (measured)
────────────────────────      ─────────────────────────     ─────────────────────────────
tcp.rs:2862                   server_conn.rs:208            transport.rs:1275
 type SrvRead = Box<dyn        type DynRead = Box<dyn         async fn write_loop<
 AsyncRead + Unpin + Send>     AsyncRead + Unpin + Send>        W: AsyncWrite + Unpin + Send>
                                                            transport.rs:1388
                                                             async fn read_loop<
                                                               R: AsyncRead + Unpin + Send>
        ▲                              ▲                              ▲
   ┌────┴────┐                    ┌────┴────┐                    ┌─────┴─────┐
accept.rs  blocking.rs        connect()  connect_blocking()  connect_server()   ??? 
(reactor)  (2 threads)        (TcpStream)  (to build)        transport.rs:958   (to build)
```

The CA client is in the **best** starting position of the three: its loops are
*generic* over `AsyncRead`/`AsyncWrite`, not boxed. The PVA server had to
introduce the erasure; the PVA client had it as a boxed trait object; the CA
client has it as a type parameter, which costs no vtable and forces no
allocation.

`TcpStream` is named in the CA client on exactly **two** production lines,
`transport.rs:12` and `search.rs:10` — the sibling document's §3.3 measured
`transport.rs:12` and concluded *"the client-side seam this document proposes
is generic … whatever blocking byte-source primitive stage 2 lands should be
shared, not duplicated into a second copy for CA later."* That reading is
confirmed, and §3.3 below sharpens it into a crate-boundary requirement that
document did not state.

The three constructor sites that need a blocking sibling:

| site | what it names | who consumes it |
|---|---|---|
| `transport.rs:978` — `TcpStream::connect(server_addr).await` | the upstream circuit | `read_loop`/`write_loop` via `stream.into_split()` (`:1110`, `:1138`) |
| `transport.rs:991-992` — `socket2::SockRef` / `TcpKeepalive` | keepalive on that circuit | — |
| `search.rs:737` — `TcpStream::connect(addr).await` in `run_nameserver_connection` | the `EPICS_CA_NAME_SERVERS` circuit | an **inline** read loop (`:783`), *not* the generic `read_loop` |

That last row is the one asymmetry: the name-server connection's read loop is
written inline inside `run_nameserver_connection` rather than reusing the
generic `read_loop`. It is ~60 lines of re-implemented CA framing
(`search.rs:783-885`). Stage C2 should route it through the same primitive
rather than growing a third framing loop — one seam, two callers.

**Both halves are now closed.** The dial at `aa91860b` (`transport::dial_ca`),
the framing at `36102cc7` (`transport::next_frame`) — §10.11 item 1. The
primitive is header-level by design: it answers how long a message is and
whether the peer is still speaking CA, and leaves the receive-side body policy
(`EPICS_CA_MAX_ARRAY_BYTES`, the drain-across-reads) with the loop that owns
the buffer, because that policy genuinely differs between a data circuit and a
name-service circuit. Wholesale reuse of `read_loop` would have been the wrong
fold for the reason its own code states — the echo watchdog and libca flow
control are properties of a *data* circuit, and a name-service circuit carries
only SEARCH and its reply.

---

## 2. calink's actual client surface

### 2.1 It is nine operations, and one file

`crates/epics-ca-rs/src/calink/` is 1,396 lines over three files — an order of
magnitude smaller than pvalink's 10,855. Exactly **one** of them imports the
CA client:

```
rg -n 'use crate::client' crates/epics-ca-rs/src/calink/*.rs
resolver.rs:16:use crate::client::{CaChannel, CaClient};
```

(`resolver.rs:593` and `:895` also name `crate::client::ConnectionEvent`,
both inside `#[cfg(test)]`.)

The complete production call surface:

| # | operation | call site | used for | context it runs in |
|---:|---|---|---|---|
| 1 | `CaClient::new()` | `resolver.rs:318` | **one** client for the IOC, inside `OnceCell::get_or_try_init` — see §2.4 | first `open`, on the link work owner |
| 2 | `client.create_channel(pv_name)` | `resolver.rs:349` | one CA channel per distinct PV name | `CaLinkResolver::open` |
| 3 | `channel.connection_events()` | `resolver.rs:358` | connection-event stream; subscribed **before** `subscribe()` so `Connected` cannot be missed | same |
| 4 | `channel.subscribe_with_mask(0.0, DBE_VALUE\|DBE_ALARM)` | `resolver.rs:368-369` | the monitor that backs the cache | same |
| 5 | `channel.info()` | `resolver.rs:621` | native DBF type + element count after connect | `fetch_link_metadata`, spawned |
| 6 | `channel.get_with_metadata_count(DbrClass::Ctrl, 1)` | `resolver.rs:629` | one-shot CTRL attribute fetch per connect | same |
| 7 | `channel.native_field_type()` / `channel.element_count()` | `resolver.rs:172-173`, `:501` | cache-servability gate (C `dbCa.c`, per `resolver.rs:44` — see §7 item 10) | synchronous read path + monitor task |
| 8 | `channel.put_nowait(&value)` | `resolver.rs:810` | `LinkPutOp::Plain` — fire-and-forget `CA_PROTO_WRITE` | `put_value`, link-put-queue owner |
| 9 | `channel.put(&value)` | `resolver.rs:811` | `LinkPutOp::Async` — put-with-callback | same |

There is no RPC, no discovery, no beacon subscription, no repeater
registration, no `sync_group`, no TLS. **A blocking CA client that serves this
list needs neither `beacon_monitor.rs` nor `repeater.rs`** — which removes 8 of
the 29 measured errors from the critical path by scoping alone, not by porting
(§1.4).

C `dbCa`'s own libca surface is the same shape: `ca_create_channel`
(`dbCa.c:1206`), `ca_add_array_event` (`:1290`, `:1305`), `ca_array_put` /
`ca_array_put_callback` (`:1229`/`:1233`, `:1252`/`:1256`), `ca_get_callback`
(`:1278`), `ca_clear_channel` (`:181`), `replace_access_rights_event`
(`:1219`). Our list maps one-to-one except that C's access-rights event is not
yet wired on the calink side.

### 2.2 The threading contract — identical to pvalink's, and calink already obeys it

The `LinkSet` sync/async split is `epics-base-rs` property, not a per-scheme
one. It is documented in full in `doc/pvalink-rtems-design.md` §2.2 (the table
of `is_connected` / `get_cached_value` / `put_admission` as `fn` with **MUST
NOT perform I/O**, against `get_value` / `connect_link` / `put_value` /
`flush_puts` as `async fn` on the database's link work owner, plus the
measurement that the owner is already on the RTEMS seam via
`link_put_queue.rs:446`, `:479`, `:491`). **That section applies verbatim to
calink; it is not re-derived here.**

What is CA-specific is that `CaLinkResolver`'s implementation of it is, if
anything, tighter than pvalink's. Measured at `resolver.rs:724-800`:

* `get_cached_value` (`:749`) goes through `cached_link` (`:466`), a plain
  `parking_lot` read of the registry with **no lazy open** — and the doc
  comment names why: *"Every synchronous `LinkSet` accessor goes through this,
  so none of them can suspend the record thread."*
* `put_admission` (`:775`) reads the same registry plus the
  `CaLink::connected` `AtomicBool`, and distinguishes `Unopened` from
  `Disconnected` so that the very write whose staging performs the open is not
  refused.
* `connect_link` (`:758`) is the deferred-open path, and its doc cites the C
  original: *"C `dbCaAddLink`'s `CA_CONNECT` work … Runs on the link work
  owner, so the `subscribe` round trip inside `open` is off the record
  thread."* (Measured in this checkout: `dbCaAddLink` at `dbCa.c:425`,
  `addAction(pca, CA_CONNECT)` at `:415`; the comment's own `:735-800` does
  not match — §7 item 10.)

That is C `dbCa`'s deferred channel setup exactly: `dbCaAddLink`
(`dbCa.c:425`) posts `CA_CONNECT` to a work list (`:415`), and the `dbCaLink`
worker thread later calls `ca_create_channel` (`:1206`). **No production path blocks the record thread on the wire.**

### 2.3 The one shared assumption that is not CA's to keep

C gives `dbCa` a **dedicated thread**:

```
dbCa.c:339   opts.stackSize = epicsThreadGetStackSize(epicsThreadStackBig);
dbCa.c:340   opts.priority  = epicsThreadPriorityMedium;
dbCa.c:355   dbCaWorker = epicsThreadCreateOpt("dbCaLink", dbCaTask, NULL, &opts);
```

One `Big`-stack, Medium-priority, joinable thread — and that thread is where
both the libca work list *and* `db_process(prec)` for CP holders run
(`dbCa.c:1320`). Our design puts the equivalent work on a shared callback band
instead. §5.4 is about whether that is survivable; it is the CA delta that
pvalink does not have.

### 2.4 The defect pvalink has, that calink does **not** — verified

`doc/pvalink-rtems-design.md` §2.4 is its longest section and its stage 0: an
IOC with N distinct `pva://` links builds N independent `PvaClient`s, each
with its own connection pool and search engine, against pvxs's single
`linkGlobal->provider_remote`. The brief asked this document to verify and
state that calink has no equivalent. **Verified. It does not.**

```rust
// crates/epics-ca-rs/src/calink/resolver.rs:255-268 (struct at :262)
pub struct CaLinkResolver {
    /// Shared CA client, created lazily on the first link [`Self::open`].
    client: Arc<tokio::sync::OnceCell<Arc<CaClient>>>,
    handle: tokio::runtime::Handle,
    /// Open links keyed by bare PV name (`ca://` scheme stripped).
    links: Arc<RwLock<HashMap<String, Arc<CaLink>>>>,
    db: Arc<RwLock<Option<PvDatabase>>>,
}
```

Three properties, each measured:

1. **One client, structurally.** `client()` (`resolver.rs:315-324`) is
   `get_or_try_init`, so *"the `OnceCell` guarantees exactly one client even
   under concurrent first opens"*, and a client-init failure is returned
   rather than cached so a later open retries — *"matching C `dbCa`'s deferred
   channel setup"*.
2. **An injection seam that is production, not test-only.** `with_client`
   (`resolver.rs:302-309`) seeds the cell eagerly via `OnceCell::new_with`,
   documented for *"a caller [to] share one client across the CA gateway and
   the CA links, or pin the client to a specific server in tests."* pvalink's
   equivalent (`PvaLink::for_test_with_client`, `link.rs:1531`) is named for
   tests and is not the only constructor; calink's is neither.
3. **One channel per PV, one circuit per server.** `links` is keyed by bare PV
   name, so *"multiple records pointing at the same remote PV share one CA
   channel + subscription"* (`resolver.rs:255-259`). Below that,
   `CircuitKey = (SocketAddr, u8)` (`client/types.rs:30`) — address plus
   priority — so **all** channels to one upstream IOC at one priority share
   **one** TCP circuit. N links to M upstream IOCs cost M circuits, not N.

C parity holds at both levels: one `ca_context_create` for the whole IOC
(`dbCa.c:1162`, stored at `:78`/`:1164`), and libca's own virtual-circuit
sharing below it.

**Consequence for the plan.** The sibling document's stage 0 — the change it
argued should go first, before any RTEMS work, because it was simultaneously a
C-parity defect and the largest resource lever — has **no CA counterpart**.
The CA track starts at the equivalent of pvalink's stage 1. That is the single
biggest reason the CA track is shorter.

### 2.5 What calink *does* have: the `tokio::runtime::Handle` trap, one level worse

`doc/pvalink-rtems-design.md` §4.1 identified the trap that no compile probe
catches: `tokio::runtime::Handle` and `Handle::current()` **compile for
`armv7-rtems-eabihf`**, because the RTEMS tokio table retains
`rt`/`time`/`sync`/`macros`, so a resolver holding a `Handle` will *type-check
for the target and panic at boot*.

`epics-ca-rs` has the identical manifest shape (`Cargo.toml:81-92`): on
`cfg(target_os = "rtems")` tokio keeps `rt`, `rt-multi-thread`, `sync`,
`time`, `macros`, `io-util`, `io-std`, `fs`, `parking_lot` — no `net` — while
`socket2` and `if-addrs` are absent from the RTEMS dependency set entirely
(`Cargo.toml:76-79`). So the trap applies unchanged.

calink is caught by it in **three** places, and one of them is worse than
anything in pvalink:

| site | what it is | why it fails on target |
|---|---|---|
| `calink/mod.rs:75` | `install_calink_resolver(&db, tokio::runtime::Handle::current())` | `Handle::current()` panics with no tokio runtime |
| `calink/resolver.rs:269`, `:288`, `:302` | `handle: tokio::runtime::Handle` field + both constructors | same |
| `calink/resolver.rs:383`, `:391`, `:568` | `self.handle.spawn(...)` ×3 | schedules onto a tokio runtime that does not exist |
| **`calink/resolver.rs:129`** | **`struct AbortOnDrop(tokio::task::JoinHandle<()>);`** | **hard-typed to the tokio handle** |

That last row is the structural one. `runtime::task::TaskHandle<T>` is
`tokio::task::JoinHandle<T>` under the hosted default but
`background::future_exec::JoinFuture<T>` under `exec_backend`
(`runtime/task.rs:136`, `:145`). A struct field spelled
`tokio::task::JoinHandle<()>` cannot hold what `runtime::task::spawn` returns
on the target — so this is not a "route the call through the seam" edit, it is
a type change.

The contrast with the client it drives is exact, and it is the inverse of the
PVA situation:

| | production bare `tokio::spawn` | `handle.spawn` | seam `spawn` | seam-typed handles | `Handle::current()` |
|---|---:|---:|---:|---:|---:|
| **CA client** (`client/*.rs`) | 1 | 0 | 17 | all | 0 |
| **calink** (`calink/*.rs`) | 0 | **3** | 0 | none | **1** |
| *PVA client* (`client_native`, from sibling §4.1) | *12* | *0* | *0* | *none* | *—* |
| *pvalink* (from sibling §4.1) | *2* | *4* | *0* | *none* | *yes* |

The CA client did this work; calink did not. §6 stage C3 is that edit, and it
is small.

---

## 3. The in-crate precedent — the blocking CA **server** driver

This is where the CA track has an advantage no PVA document could claim: the
blocking driver and the client that needs one **live in the same crate**.

### 3.1 What exists

| artefact | lines | shape |
|---|---:|---|
| `epics-ca-rs/src/server/blocking.rs` | 3,541 | thread-per-client CA server; C `rsrv`/`camsgtask` parity |
| `epics-ca-rs/src/server/recv.rs` | 378 | `RecvAccumulator` — sans-io byte accumulation with a server-chosen ceiling |
| `epics-ca-rs/src/server/outbox.rs` | 132 | single-owner write side |
| `epics-ca-rs/src/protocol.rs` | 1,409 | the wire types **both** sides already share (§1.4) |

Its module doc states the reuse rule this design inherits (`blocking.rs:38-49`):
*"The wire logic is NOT reimplemented. This driver constructs the shared
`ClientState` and drives the shared `dispatch_message` … to completion on the
client thread via `block_on_sync`."*

### 3.2 What transfers to a blocking CA client — four primitives, precisely

**1. `pending_bytes` / `FIONREAD_REQUEST` — this one is not "transferable", it
is the *same defect already fixed once*.**

The client's `transport.rs:106-116` is:

```rust
#[cfg(unix)]
fn fd_recv_queue_probe(fd: std::os::fd::RawFd) -> OsRecvQueueProbe {
    ... libc::ioctl(fd, libc::FIONREAD, &mut pending) ...
}
```

`libc::FIONREAD` is absent from the `armv7-rtems-eabihf` bindings — that is
measured error `E0425` at `transport.rs:113` (§1.3). The **server already
solved exactly this**, with a named constant and a derivation
(`blocking.rs:616-637`):

```rust
#[cfg(all(unix, not(target_os = "rtems")))]
const FIONREAD_REQUEST: libc::c_ulong = libc::FIONREAD as libc::c_ulong;
#[cfg(target_os = "rtems")]
const FIONREAD_REQUEST: libc::c_ulong = 0x4004_667F;
```

with the doc comment deriving `0x4004_667F` from RTEMS newlib's
`sys/rtems/include/sys/filio.h` (`_IOR('f', 127, int)`) through `sys/ioccom.h`'s
`_IOR(g,n,t) = IOC_OUT | (sizeof(t) << 16) | (g << 8) | n`, and noting that a
wrong value only makes the `ioctl` error, after which every caller flushes —
*"never a hang or a crash."*

Per the project's rule that a defect citation names a *sample* and not the
population: the anchor is `libc::FIONREAD`, and `rg -n 'FIONREAD' --glob '*.rs'
crates/` returns exactly two definition sites — the server's guarded constant
and the client's bare use. The client's site is the same defect. It should be
fixed by **hoisting `FIONREAD_REQUEST` + `pending_bytes` to a shared home**,
not by copying the `#[cfg]` pair into `transport.rs`.

This is also a live instance of the recorded hazard *"RTEMS satisfies
`cfg(unix)`"*: `#[cfg(unix)]` did hand RTEMS the Linux-shaped path here. It
failed loudly only because the *constant* is missing — `libc::ioctl` itself
exists on RTEMS. Had the client spelled the request number inline, this would
have compiled and misbehaved on target only.

**2. The one-descriptor read/write split — `Arc<TcpStream>`, never `try_clone`.**

`blocking.rs:903-921` states the rule and the measurement:

> *"The split is made by SHARING one `TcpStream` through an `Arc`, not by
> duplicating the descriptor. `try_clone` is `fcntl(F_DUPFD_CLOEXEC)`, and on
> RTEMS 6 that cannot work for a socket: RTEMS's `fcntl` has no
> `F_DUPFD_CLOEXEC` case at all (`cpukit/libcsupport/src/fcntl.c:146-220`
> falls to `default: errno = EINVAL`), and even plain `F_DUPFD` fails because
> `duplicate_iop` (`fcntl.c:47-77`) calls the file's `open_h` while
> rtems-libbsd installs `rtems_bsd_sysgen_nodeops` on every socket … Measured
> on the target: `dup`, `F_DUPFD` and `F_DUPFD_CLOEXEC` all fail on an
> accepted socket while `F_DUPFD` on `/dev/console` succeeds."*

`impl Read for &TcpStream` gives the two roles from one descriptor. A blocking
client's reader and writer threads must do the same. Identical to the sibling
document's §3.2 finding for PVA; recorded twice, in both drivers, because it
is the kind of thing that compiles and then fails only on target.

**3. Raw-`libc` socket setup instead of `socket2`.** `socket2` is not in the
RTEMS dependency set at all (`Cargo.toml:76-79`), which is why
`transport.rs:991-992`'s keepalive block is 2 of the 7 primary errors. The
server already sets socket options with raw `libc` — `set_reuse_opt`
(`blocking.rs:551`), `bind_udp_search_socket` (`:571`, with a `#[cfg(not(unix))]`
fallback at `:610`). The same shape covers `SO_KEEPALIVE` / `TCP_KEEPIDLE` /
`TCP_KEEPINTVL` for the client circuit, and `hostname.rs`'s two `socket2::SockAddr`
uses are reverse-DNS diagnostics that a target build should simply not carry.

**4. `bind_udp_search` — a working UDP bind for the target, already written.**
`blocking.rs:546` is a `std::net::UdpSocket` bound through raw `libc` with the
`SO_REUSEADDR`/`SO_REUSEPORT` pre-bind setup, needing no `socket2`. If a later
stage wants UDP search on the CA client (§6 stage C6), this is the primitive —
not new work.

Beyond primitives, one *fact* transfers unchanged and binds the client:
**no `local_addr` readback.** RTEMS's libc omits the BSD `sockaddr` length
byte, so `bind()` succeeds and `local_addr()` returns `InvalidInput`
(sibling §3.2). §4.4 is where CA gets to shrug this off and PVA cannot.

### 3.3 What does **not** transfer — and the crate-boundary correction

**The CA server driver has no `AsyncRead`/`AsyncWrite` adapter to lend.**

This is the substantive correction this document owes its sibling. The two
blocking drivers took *different* shapes:

| | PVA server (`server_native/blocking.rs`) | CA server (`server/blocking.rs`) |
|---|---|---|
| how blocking bytes meet async code | `ChannelReader` / `ChannelWriter` — `AsyncRead`/`AsyncWrite` adapters over bounded mpsc, fed by two threads (`:607`, `:734`) | **none** — the handler future is driven to completion *on the client thread* by `block_on_sync` / `park_on` |
| what the thread runs | a pump | the protocol dispatch itself |

`doc/pvalink-rtems-design.md` stage 2 says: *"Promote those two adapters out of
`server_native` into a module both drivers use."* Measured, "both drivers"
cannot include CA, because of the dependency graph:

```
rg -n 'epics-base-rs|epics-ca-rs|epics-pva-rs' crates/epics-ca-rs/Cargo.toml crates/epics-pva-rs/Cargo.toml
crates/epics-ca-rs/Cargo.toml:11:epics-base-rs = { workspace = true }
crates/epics-pva-rs/Cargo.toml:11:epics-base-rs = { workspace = true }
```

`epics-ca-rs` does not depend on `epics-pva-rs`, and must not — the only crate
that depends on both is `epics-bridge-rs`, which sits *above* them. And
`epics-base-rs` has no such adapter today:

```
rg -n 'impl AsyncRead|impl AsyncWrite' --glob '*.rs' crates/epics-base-rs/src
(no output)
```

So the sibling document's *"one seam, two callers, not two seams"* argument is
right and its *destination* is wrong. Promoting `ChannelReader`/`ChannelWriter`
within `epics-pva-rs` yields a primitive the CA client structurally cannot
call, and the next CA increment then writes the third copy — which is exactly
the outcome that argument exists to prevent.

**The structural fix: the blocking byte-source primitive lands in
`epics-base-rs`, once, and both protocol crates call it.** `epics-base-rs`
already owns every other member of this family — `runtime::task::spawn`,
`block_on_sync`/`park_on`, `StackSizeClass`, `spawn_dedicated_thread`,
`enter_ioc_thread`. A blocking-socket→`AsyncRead`/`AsyncWrite` adapter pair is
the same kind of object and belongs beside them (`runtime::net` or a new
`runtime::io`). Doing it there also gives `FIONREAD_REQUEST`/`pending_bytes`
(§3.2 item 1) and the `Arc<TcpStream>` no-dup rule (item 2) a home that is not
inside one protocol's server module.

This has an ordering consequence, and it is the one hard dependency between
the two tracks: **whichever track reaches this primitive first must build it
in `epics-base-rs`.** If the PVA track lands stage 2 into `epics-pva-rs`
first, the CA track's stage C2 has to move it afterwards — a strictly larger
change than putting it in the right crate once. §6 states this as an explicit
cross-track gate.

One further shape does **not** carry over. The CA server's per-client
`Big`-stack thread runs the whole dispatch under `park_on`
(`blocking.rs:339`). A CA *client* has no analogue: `read_loop`/`write_loop`
are already async tasks spawned through the seam (`transport.rs:1086`, `:1095`,
`:1111`, `:1120`, `:1139`, `:1148`), so on target they land on the callback
pool. A client circuit therefore costs **two threads (a reader pump and a
writer pump), not three** — the same arithmetic as the sibling's §4.3, for the
same reason.

---

## 4. CA search on the target: `EPICS_CA_NAME_SERVERS`

### 4.1 C supports it, and documents it as the UDP-free mode

The brief's premise is correct, and the C reference is explicit
(`modules/ca/src/client/CAref.html:515-520`):

> *"For any IP addresses specified in the EPICS environment variable
> EPICS_CA_NAME_SERVERS, TCP connections are opened and used for CA client
> name resolution requests. (Thus, broadcast addresses are not allowed in
> EPICS_CA_NAME_SERVERS.) When used in combination with an empty
> EPICS_CA_ADDR_LIST and EPICS_CA_AUTO_ADDR_LIST set to "NO", **Channel Access
> can be run without using UDP for name resolution.** Such an TCP-only mode
> allows for Channel Access to work e.g. through SSH tunnels."*

The implementation is in `cac`'s constructor (`cac.cpp:250-280`): it loads the
list, and for each address registers a `SearchDestTCP` and creates a virtual
circuit via `findOrCreateVirtCircuit` at minor version 11. The parameter is
declared at `envDefs.h:55` and defaults empty at `configure/CONFIG_ENV:32`.

This is the exact CA analogue of `EPICS_PVA_NAME_SERVERS`, which
`doc/pvalink-rtems-design.md` §4.2 used to defer UDP search out of the PVA
critical path.

### 4.2 We implement it — measured

The brief asked whether our client implements it, *because if it does the UDP
search can be deferred exactly like the pvalink doc's stage 1*. It does.

A first grep for the literal `EPICS_CA_NAME_SERVERS` under
`crates/epics-ca-rs/src` returns only `bin/ca-lint-rs.rs:70`, which is
misleading: the implementation is spelled `nameserver` throughout.

```
rg -n 'nameserver|NAME_SERVERS' --glob '*.rs' crates/epics-ca-rs/src crates/epics-base-rs/src
```

| piece | site |
|---|---|
| env parameter declaration | `epics-base-rs/src/runtime/env_table.rs:34` — `EnvParam::new("EPICS_CA_NAME_SERVERS", "")`, listed in the table at `:102` |
| parse | `client/mod.rs:5339` — `pub(crate) fn parse_nameserver_list() -> Vec<(SocketAddr, Option<String>)>`, reading `env_table::EPICS_CA_NAME_SERVERS` at `:5340` |
| wire-up | `client/mod.rs:718`, `:742-743`, `:756` — passed into `run_search_engine` |
| one TCP task per name server | `search.rs:551-559` — `for addr in nameserver_addrs { … runtime::task::spawn(run_nameserver_connection(addr, rx, resp_tx)) }` |
| the connection itself | `search.rs:731-737` — `TcpStream::connect`, libca handshake order VERSION → CLIENT_NAME → HOST_NAME (`tcpiiu.cpp:755-762`), reconnect on `EPICS_CA_CONN_TMO` |
| bounded send queue | `search.rs:547` — `EPICS_CA_NAMESERVER_QUEUE_DEPTH`, default 256; drop-on-full with a metric (`ns_try_send`, `:1623`), a fix for Launchpad #739789 |

So the TCP name-server path is real, C-faithful, already on the spawn seam,
and — critically — it consumes the **same** search frames as the UDP path.
`fire_searches` (`search.rs:1516-1618`) builds `current_frame` **once** and
fans it to both:

```rust
for entry in addr_list { send_with_fanout(socket, &current_frame, entry.sock, …).await; }
for ns_tx in nameserver_txs { ns_try_send(ns_tx, current_frame.clone()); }
```

and replies from both are parsed by the one shared `handle_udp_response`. The
frame producer is sans-io already.

### 4.3 But the UDP socket is by value, not `Option` — that is the stage

```rust
// crates/epics-ca-rs/src/client/search.rs:507-510
let socket = match AsyncUdpV4::bind(0, true) {
    Ok(s) => s,
    Err(_) => return,
};
```

`run_search_engine` binds a per-NIC `AsyncUdpV4` bundle unconditionally and
**returns** — killing the whole engine, name-server tasks included — if the
bind fails. `AsyncUdpV4` is `#[cfg(not(target_os = "rtems"))]` in base
(`epics-base-rs/src/net/mod.rs:28-54`), which is 2 of the 7 primary errors and
the root of all 13 cascade errors (§1.3).

This is the same shape as the PVA `run_engine`'s `search_socket: AsyncUdpV4`
by-value signature, and it takes the same fix — and it should take the sibling
document's *structural* form, not the patch form:

> a `SearchTransport::{Udp(AsyncUdpV4, …), NameServersOnly}` sum type, so
> "UDP socket present" and "UDP arms armed" cannot disagree. A bare `Option`
> plus `if let` in the fanout sites is the patch; the sum type is the fix.
> — `doc/pvalink-rtems-design.md` §5 stage 1

The CA version is smaller than PVA's: the UDP surface inside
`run_search_engine` is one bind, one `set_recv_buffer_size`, one
`set_multicast_ttl_v4`, one `enable_so_rxq_ovfl`, the `recv` arm of the
`select!`, the per-NIC `SO_RXQ_OVFL` bookkeeping, and three `send_with_fanout`
call sites in `fire_searches`.

### 4.4 The CA advantage: no `local_addr` readback

This is where the two protocols genuinely diverge, and it is in CA's favour.

`doc/pvalink-rtems-design.md` §4.2 item 3 established that PVA's UDP search is
blocked on the target by more than missing primitives: `run_engine` computes
`response_port` from `search_socket.local_addrs()` and stamps it into **every
SEARCH frame**, and on target `local_addr()` returns `InvalidInput`, so a UDP
SEARCH would advertise port 0.

CA has no such field. A `CA_PROTO_SEARCH` reply is returned to the datagram's
source address, so the frame carries no response port to stamp. Measured
against our own frame builder — `build_search_payload` (`search.rs:1647`) and
the per-datagram VERSION header (`fire_searches:1523-1540`) construct the
bytes from the cid, the padded PV name, `CA_MINOR_VERSION` and
`state.dgram_seq`, and nothing else:

```
rg -n 'local_addr' crates/epics-ca-rs/src/client/{search,transport,mod}.rs
client/search.rs:2345,2348,2446      # all inside #[cfg(test)] — a test "sniffer" socket
client/transport.rs:3856,4108,4175   # all inside #[cfg(test)]
```

**No production line of the CA client reads `local_addr` at all.** So the
RTEMS libc `sockaddr` bug does not block CA UDP search the way it blocks PVA's
— and the 16 `IP_PKTINFO`/`recvmsg`/`CMSG_*` errors that dominate PVA's `udp.rs`
have **no CA counterpart either** (CA's UDP surface is `AsyncUdpV4`, 2 errors,
not raw cmsg recovery).

Which means: for CA, UDP search is deferred in §6 because it is *not needed for
a record link*, not because it is *blocked*. That is a weaker reason, and the
plan should say so rather than borrowing PVA's stronger one.

### 4.5 What the target actually needs

For stage C5's on-target gate, the guest IOC needs exactly:

```
EPICS_CA_NAME_SERVERS=10.0.2.2:5064
EPICS_CA_ADDR_LIST=
EPICS_CA_AUTO_ADDR_LIST=NO
```

— the C-documented TCP-only mode (§4.1), with `EPICS_CA_AUTO_ADDR_LIST=NO`
load-bearing because auto-address-list would otherwise reintroduce a broadcast
destination.

---

## 5. RTEMS constraints — CA deltas only

`doc/pvalink-rtems-design.md` §4 is the platform chapter for both tracks. Its
§4.1 (the single spawn seam, and the `Handle::current()` compile-but-panic
trap), §4.3's `StackSizeClass` table (`Small` 262,144 / `Medium` 524,288 /
`Big` 1,048,576 bytes on `armv7-rtems-eabihf`, and the correction that the
commonly-quoted 2 MiB is the 64-bit host figure), its baseline thread census,
and §4.4's 1-second `Instant` quantum **apply unchanged and are not repeated
here.** What follows is only what differs for CA.

### 5.1 The spawn seam: the CA client is already on it

Measured with a brace-aware scanner that skips `#[cfg(test)]` and
`#[cfg(all(test, …))]` modules by matching their closing brace (a first-
`#[cfg(test)]`-line heuristic like the sibling document's mis-classifies
`transport.rs`, whose first `#[cfg(test)]` is a single-item gate at `:127`):

| file | production bare `tokio::spawn` | production `runtime::task::spawn` |
|---|---:|---:|
| `client/mod.rs` | **1** (`:1374`) | 9 |
| `client/transport.rs` | 0 | 6 |
| `client/search.rs` | 0 | 2 |
| `client/beacon_monitor.rs`, `subscription.rs`, `types.rs`, `state.rs`, `sync_group.rs`, `circuit_breaker.rs` | 0 | 0 |
| **total** | **1** | **17** |

and every handle field is the seam alias, with the reasoning already written
into the source:

```rust
// client/transport.rs:265-268
// Spawned via `runtime::task::spawn`, so typed as the seam handle.
// Byte-identical to `tokio::task::JoinHandle` under the hosted default;
// the executor's `JoinFuture` under `rtems-exec-model`.
_read_task:  epics_base_rs::runtime::task::TaskHandle<()>,
_write_task: epics_base_rs::runtime::task::TaskHandle<()>,
```

Same at `client/mod.rs:443-449` (`_coordinator`, `_search_task`),
`client/mod.rs:3221` (`EventWatcher`), and `client/sync_group.rs:33-37`, which
even imports the alias under the old name (`use …::TaskHandle as JoinHandle`)
so the seam is what the file means by "JoinHandle".

**The one survivor**, `client/mod.rs:1373-1374`:

```rust
if tokio::runtime::Handle::try_current().is_ok() {
    tokio::spawn(async move { /* bounded coordinator shutdown */ });
}
```

It does not panic on target — `try_current()` returns `Err` and the block is
skipped. But it is a silent functional gap: on RTEMS, `CaClient::drop` would
never run the bounded coordinator shutdown. It should be routed through the
seam like its 17 siblings, which also removes the guard.

**Consequence:** the sibling document's stage 3 splits for CA. Its
client-side half is already done; only its calink-side half (§2.5) and its
band invariant (§5.4) remain.

### 5.2 Threads and stacks per CA connection, blocking shape

Using the sibling §4.3 stack classes and the §3.3 finding that a client
circuit costs two pump threads rather than three:

| item | threads | armv7 stack | notes |
|---|---:|---:|---|
| per **upstream server** circuit (reader + writer pumps, `Small`) | 2 | 524,288 | one circuit per `(SocketAddr, priority)` — §2.4 |
| per **name-server** circuit (reader + writer pumps, `Small`) | 2 | 524,288 | needed only if a name server is configured |
| `read_loop` / `write_loop` / coordinator / search-engine *tasks* | 0 | 0 | cbMedium band |
| calink monitor + connection-watcher tasks, per link | 0 | 0 | cbMedium band — and see §5.4 |

So the cost is **per distinct TCP peer, not per link** — and unlike pvalink,
calink has that property *today* (§2.4), not after a stage-0 fix. An IOC with
any number of `ca://` links to one upstream IOC, reached through one name
server, costs **4 threads / 1 MiB** of stack.

Every one of those threads must call `enter_ioc_thread` as its first statement
— RTEMS pthreads inherit `POSIX_Init`'s near-idle priority.
`spawn_dedicated_thread` (`runtime/task.rs:1298`, `:1324`) does it;
a raw `thread::Builder` does not, which is why the CA server driver carries a
source-text guard for it. A new blocking client must be inside that guard's
scope, not beside it.

### 5.3 The fd budget — a client spends from the *server's* 142

`doc/rtems-fd-ceiling-deviation.md` §2-3 measured, on the bring-up box:

| | value | source |
|---|---:|---|
| `CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS` | 150 | `epics-rtems-boot/csrc/rtems_config.c` §F |
| descriptors the IOC holds at idle | 8 | arithmetic, confirmed by `FD_CNT + FD_FREE = FD_MAX = 150` |
| **last inbound client served** | **142** | measured ramp; `#143` refused with `ENFILE` |
| memory wall (free heap ÷ 1,589,000 B) | 151 | measured |

The interaction that matters here, and that neither prior document states:
**a CA client's descriptors come out of the IOC's idle hold, so every one of
them lowers the 142 by one.** The fd wall is `MAXIMUM_FILE_DESCRIPTORS − (idle
hold)`, and adding link connectivity increases the idle hold:

| configuration | added fds | new idle hold | inbound clients served |
|---|---:|---:|---:|
| today (`rtems-ca-ioc`, no client) | 0 | 8 | 142 |
| + 1 name-server circuit + 1 upstream circuit | 2 | 10 | **140** |
| + 4 upstream IOCs, 1 name server | 5 | 13 | **137** |
| + UDP search (per-NIC bundle, ≥1 socket) | ≥1 more | ≥14 | ≤136 |

This is small in absolute terms and it is *not* a reason to defer anything —
but it is a real coupling, it is arithmetic rather than estimate, and it is
the reason §6's on-target gate reads the status PVs' `FD_CNT` rather than
assuming the link cost is free. It is also a second, independent argument for
TCP-only search on the target: the per-NIC UDP bundle costs one descriptor per
IPv4 interface, for a capability a record link does not need.

Note the memory wall (151) and fd wall (140-ish with links) stay in the same
order, so links do not change *which* wall binds.

### 5.4 The band-occupancy delta — the one place CA is harder than PVA

`doc/pvalink-rtems-design.md` §2.3 argued that one `cbMedium` worker suffices
for pvalink, and the argument was specific: *"the callback body does no
blocking work. `on_event` (`link.rs:355-365`) does a `store`, a `parking_lot`
lock write, and `enqueue_scan_trigger`, whose send is a **non-blocking
`try_send`** with a coalesce-on-full fallback."*

**calink's callback body is not that.** `run_monitor`
(`calink/resolver.rs:477-530`) does the store — and then:

```rust
let db_handle = db.read().clone();
if let Some(db_handle) = db_handle {
    db_handle.dispatch_external_cp_targets(&pv_name);
}
```

`dispatch_external_cp_targets` (`epics-base-rs/src/server/database/processing.rs:5037`)
is a **synchronous** call that walks every registered CP/CPP holder of that PV
and, for each, calls `process_one_cp_target` (`:4982`) →
`process_record_with_links_recursive` — full record processing, FLNK chains
and all, inline on the calling task.

On the host that task is a tokio worker among many. On RTEMS,
`runtime::task::spawn` puts it on the `cbMedium` band, which has **one**
worker. So on target, one monitor event on one `ca://` link runs an entire
record-processing chain on the band that also carries every deferred callback
and every other link's monitor.

Three things to say about that precisely, because it is easy to overstate:

1. **It is C-faithful in shape.** C `dbCa`'s event callback
   (`eventCallbackComm`, `dbCa.c:940`) updates the cache and adds
   `CA_DBPROCESS` (`dbCa.c:871`, `:1011`), and the worker thread runs
   `db_process(prec)` (`dbCa.c:1314-1320`) — also on the link worker, not on a
   scan thread. The Rust code cites this ordering deliberately
   (`resolver.rs:507-517`), though with line numbers from a different base
   revision (§7 item 10). So the design is not wrong; it is the *thread
   supply* that differs.
2. **What differs is dedicated vs shared.** C gives that work its own `Big`-
   stack thread (`dbCa.c:339`, `:355`; §2.3). Our exec backend gives it a
   shared band with `DEFAULT_THREADS_PER_PRIORITY = 1`.
3. **The executor is cooperative**, so an *idle* monitor releases its worker
   (sibling §2.3). The occupancy is for the duration of the processing chain,
   not permanent. But a chain is unbounded in a way a `try_send` is not.

The invariant the sibling document stated for pvalink therefore needs a
**stronger** CA form. Its version:

> **MUST NOT** any pvalink task spawned onto a callback band call
> `block_on_sync` / `park_on`.

is necessary but not sufficient here, because `dispatch_external_cp_targets`
parks nothing — it simply *runs for a long time*. The CA-specific addition:

> **Invariant.** A `ca://` link's monitor task MUST NOT run record processing
> inline on the band worker that received the event.
> **Owner.** `CaLinkResolver::run_monitor` is the single site that turns a CA
> monitor event into local processing (`resolver.rs:519-521`); it is the only
> place the transition can be made.

Two candidate closures, and the structural one is preferred:

* **Patch:** give calink its own dedicated thread, C-style, and spawn
  `run_monitor` onto it. Closest to C, but it re-introduces a per-subsystem
  thread the exec model exists to avoid, and it does not generalise —
  pvalink's forwarders, QSRV's group forwarders and FLNK tails have the same
  shape on the same band.
* **Structural:** make the CP fan-out an *enqueue* rather than a call —
  route `dispatch_external_cp_targets` through the same scan-trigger path
  pvalink's `enqueue_scan_trigger` already uses, so the monitor task's body is
  bounded by construction on both schemes and the band never runs a
  record chain. This makes the illegal shape unrepresentable rather than
  reviewed, and it removes the dual meaning of "the monitor task" (a cache
  updater on one path, a record processor on the other).

The structural option is a **semantic change** — it moves CP-holder processing
off the monitor callback's own stack — and it needs sign-off rather than a
silent pick. It is stage C4 in §6, and it is measured-unknown whether it is
required at all: §7 item 3.

### 5.5 The clock

Sibling §4.4 applies: `Instant` is 1-second-quantized on target. Two CA sites:

* `CaLinkResolver::wait_for_link_connected` (`resolver.rs:422`, sleeping at `:440`) polls at 25 ms
  — finer than pvalink's 50 ms, and equally unexpressible. It is a test
  helper; the consequence is that an on-target link-up test must be written
  with second-scale deadlines.
* `run_nameserver_connection` (`search.rs:742-746`) backs off by
  `EPICS_CA_CONN_TMO` (default 30 s), which is comfortably above the quantum.
  Unlike pvalink's 250 ms reconnect ladder, this is *already* C's cadence and
  needs no adjustment — the port comment at `search.rs:718-726` records that
  an earlier 1→30 s exponential backoff was removed precisely to match C.

---

## 6. Staged plan, sequenced against the pvalink track

Same discipline as `doc/pvalink-rtems-design.md` §5: each stage names its own
gate, and no stage depends on a later one. Stage numbering is `C*` to keep the
two tracks distinguishable in cross-references.

**Cross-track ordering — three dependencies, and only three.**

| # | dependency | direction | why |
|---:|---|---|---|
| 1 | the blocking byte-source primitive must land in **`epics-base-rs`** | whichever track builds it first | `epics-ca-rs` cannot reach `epics-pva-rs` (§3.3). Building it inside `epics-pva-rs` forces a later move. |
| 2 | `pvalink` stage 0 (one shared client) | **PVA only** | calink has no equivalent (§2.4). No CA stage waits on it. |
| 3 | `epics-bridge-rs` gaining `rtems-exec-model` (sibling §5 stage 3 risk, the 250-site census) | **PVA only** | calink lives in `epics-ca-rs`, which already declares the feature (`Cargo.toml:113`) and already carries the census gate (`tests/rtems_exec_model_gate.rs`). Stage C3 is not blocked by it. |

Everything else is independent, and the two tracks' stage 1s (search-without-UDP)
touch different crates and can run concurrently.

### Stage C1 — a CA search engine that runs with no UDP socket (medium, host-testable) — **DONE** (see §8)

Replace `run_search_engine`'s unconditional `AsyncUdpV4::bind` (`search.rs:507`)
with a `SearchTransport::{Udp(AsyncUdpV4, …), NameServersOnly}` sum type, so
"UDP socket present" and "UDP arms armed" cannot disagree (§4.3). The
name-server path (`:551-559`, `:731`) and `handle_udp_response` are untouched.
Refuse construction of `NameServersOnly` with an empty name-server list — a
search engine that can reach nothing should fail loudly at build, not resolve
nothing forever.

*Size:* ~100 lines in `search.rs`; no other file.

*Gate:*
* a host test that resolves a PV with `EPICS_CA_ADDR_LIST` empty and
  `EPICS_CA_AUTO_ADDR_LIST=NO`, reaching the server **only** via
  `EPICS_CA_NAME_SERVERS`, asserting the client bound **no** UDP socket —
  this is C's documented TCP-only mode (§4.1) and we have no test for it today;
* `cargo nextest run -p epics-ca-rs`;
* `cargo clippy -p epics-ca-rs --all-targets -- -D warnings`.

*Risk:* the 13-error `[u8]` cascade at `search.rs:592` (§1.3) says inference in
that `select!` is already fragile. Expect the arm-shape change to surface real
type errors the cascade was hiding. That is the point.

### Stage C2 — the blocking byte source, in `epics-base-rs` (medium; **shared with the PVA track**) — **DONE, inside C5** (see §10)

Add to `epics-base-rs` a blocking-socket → `AsyncRead`/`AsyncWrite` adapter
pair plus the two facts that must not be re-derived: `Arc<TcpStream>` with
`impl Read/Write for &TcpStream` (never `try_clone`, §3.2 item 2), and
`FIONREAD_REQUEST`/`pending_bytes` hoisted out of
`epics-ca-rs/src/server/blocking.rs:616-670` (§3.2 item 1).

Then in `epics-ca-rs`, add a blocking sibling to `connect_server`
(`transport.rs:958`) that dials with `std::net::TcpStream::connect`, drives the
socket with two threads through the new adapters, and hands the **existing
generic** `read_loop`/`write_loop` (`:1388`, `:1275`) their `R`/`W`. Replace
the `socket2` keepalive block (`:991-992`) with raw-`libc` setsockopt in the
shape of `server/blocking.rs:551`. Route `run_nameserver_connection`'s dial
(`search.rs:737`) through the same primitive, and fold its inline framing loop
(`:783-890`) into `read_loop` — one seam, two callers, not two seams and three
framing loops (§1.6).

*Size:* ~200 lines new in `epics-base-rs`, ~150 in `epics-ca-rs`, ~80 lines of
moves. The PVA track's stage 2 then *consumes* the base primitive instead of
promoting `ChannelReader`/`ChannelWriter` within `epics-pva-rs`.

*Gate:*
* the whole `epics-ca-rs` client test suite passing **with the blocking
  constructor forced on**, on the host — the only way to show the frame
  pipeline is untouched;
* `./scripts/rtems-check.sh` with `epics-ca-rs`'s `lib.rs` gate lifted from
  `client` (and `channel`) — **this is the stage that turns §1.2's "18/29 is a
  lower bound" into a number**, and in particular settles whether
  `client/mod.rs`'s zero is real (§7 item 1);
* `cargo nextest run -p epics-ca-rs`;
* a source-text guard that `client/` names no `socket2` and no bare
  `libc::FIONREAD`.

*Risk:* the suppressed-error question, same as the sibling's. If
`client/mod.rs` or `subscription.rs` is not in fact transport-independent, this
gate is where it shows.

*Not in this stage:* `beacon_monitor.rs`, `repeater.rs`, `hostname.rs`,
`discovery`. A record link needs none of them (§2.1); they stay gated and the
target build selects the client without them. That scoping removes 9 of the 29
errors without a line of porting, and it must be a **stated** feature split
(`client-core` vs `client`) rather than an absence — the same argument
`scripts/rtems-check.sh:70-84` makes for `qsrv-core`.

### Stage C3 — put calink on the spawn seam (small, host-only) — **DONE** (see §9)

Delete `CaLinkResolver::handle` (`resolver.rs:269`, `:288`, `:302`) and its
three `handle.spawn` calls (`:383`, `:391`, `:568`); spawn through
`runtime::task::spawn`. Change `AbortOnDrop` (`resolver.rs:129`) from
`tokio::task::JoinHandle<()>` to `runtime::task::TaskHandle<()>` — a type
change, not a call change (§2.5). Drop `Handle::current()` from
`calink_link_set_install` (`mod.rs:75`) and from `iocsh.rs:150`. Route
`client/mod.rs:1374`'s surviving bare `tokio::spawn` through the seam and
delete its `try_current` guard (§5.1).

*Size:* ~50 lines across `calink/{mod,resolver,iocsh}.rs` and `client/mod.rs`.

*Why it can go first:* it has no RTEMS dependency and no dependency on C1 or
C2. It is provable on the host.

*Gate:*
* a source-text guard in `epics-ca-rs` mirroring
  `epics-pva-rs/src/server_native/tcp.rs:8180-8191` — "production scope must
  spawn through `runtime::task::spawn`; found N bare `tokio::spawn(`" —
  extended to also reject `handle.spawn(` and a `tokio::runtime::Handle`
  field in production types. **The client would pass this guard today; calink
  would not.** That asymmetry is exactly what the guard is for;
* `cargo nextest run -p epics-ca-rs --features rtems-exec-model` — the
  feature-ON suite is the only place the exec backend actually runs, and
  `epics-ca-rs` already declares the feature and carries the census gate;
* `cargo nextest run -p epics-ca-rs -p epics-bridge-rs` (the bridge's
  `calink_lset_contexts.rs` drives this surface).

### Stage C4 — the band invariant, and calink's CP fan-out (small, needs sign-off)

Close the sibling's band invariant by construction, per its stage 3 — a
thread-local "on a callback band" marker consulted by `block_on_sync`
(`runtime/task.rs:114`) so parking a band worker is *reported*, not reviewed.
That half is shared with the PVA track and should be built once.

Then close the CA-specific half (§5.4): the monitor task must not run a record
chain inline. **Preferred: the structural form** — make
`dispatch_external_cp_targets` an enqueue onto the existing scan-trigger path
rather than a synchronous call, so both schemes' monitor bodies are bounded by
construction. This is a semantic change (CP-holder processing moves off the
monitor callback's stack) and it is stated here for sign-off rather than
picked silently. The patch alternative — a dedicated calink thread, C-style —
is recorded as a fallback and is *not* the proposal.

*Gate:*
* a test that a future calling `block_on_sync` from a band worker is refused
  rather than deadlocking;
* a test that a CA monitor event on a link with a deep CP→FLNK chain returns
  the monitor task to the executor within a bounded number of polls;
* `cargo nextest run -p epics-ca-rs -p epics-base-rs --features rtems-exec-model`.

*Risk:* whether this stage is needed at all is unmeasured (§7 item 3). If
stage C5's on-target measurement shows band latency is not affected, the
structural change may still be worth having for the invariant, but the urgency
changes. Measure before committing to the semantic change.

**Sign-off outcome (user, 2026-07-23): DEFERRED pending measurement.** The
semantic change is NOT approved on the current evidence — with the necessity
unproven, breaking the event↔processing atomicity (one processing pass per
event; cache update and record processing in lockstep on the monitor task) is
not acceptable. The deadlock half is already closed structurally by the shared
band invariant (pvalink stage 3, `block_on_sync` facility-thread refusal), so
the residual exposure is latency only. Decision rule: stage C6's on-target
gate MUST include a band-occupancy measurement (deep CP→FLNK chain on one
link; observe delay of other callbacks on the same band). Only if that
measurement shows a real problem do the two closures (enqueue restructure vs
C-style dedicated thread) come back for a second sign-off, with numbers. Until
then no code change alters `run_monitor`'s inline `dispatch_external_cp_targets`.

**Measurement taken (2026-07-23): §11.6.** Headline, for the second sign-off:
at 10.00 Hz on a link whose CP holder runs an 9-record inline FLNK chain, an
INDEPENDENT `ca://` link on the same band takes **+9.45 ms typical / +15.67 ms
worst-case** added monitor-to-record delay, and a 200 ms `cbMedium` timer task
takes **no measurable delay at all** (4.70 tick/s in every phase, identical to
three significant figures). Nothing in this stage altered
`dispatch_external_cp_targets`; §11.4's fix ADDS a dispatch on the disconnect
edge, which is the C behaviour that was missing, not a change to the fan-out
the sign-off froze.

### Stage C5 — mount calink in `rtems-ca-ioc` (small) — **DONE** (see §10)

Select the target client feature (stage C2's `client-core`) for
`epics-ca-rs` in `scripts/rtems-check.sh`'s `CRATE_FEATURES`, un-gate `calink`
in `lib.rs:37-38`, install the resolver in `rtems-ca-ioc.rs` beside the
existing database assembly, and replace the startup banner — which today says
nothing about links — with the resolver state and link count.

Note for the banner: the target installs no tracing subscriber, so only
`eprintln!`/`println!` reach the console (`rtems-ca-ioc.rs:279`, `:285`,
`:314` are already the only diagnostics that survive). Any link diagnostic
written with `tracing::` is discarded.

*Gate:*
* `./scripts/rtems-check.sh` green in both configurations;
* `rtems-ca-ioc`'s existing source-text guards, extended over the new code;
* `cargo nextest run -p epics-ca-rs`.

**Not sufficient.** This stage's real gate is C6.

### Stage C6 — two IOCs on the wire, one `ca://`-linking to the other (the actual gate) — **DONE** (see §11)

Everything above is `cargo check` and host tests. "Type-checks for RTEMS" and
"runs on RTEMS" are different claims and this workspace has been bitten by the
gap twice (`scripts/rtems-check.sh:14-28`).

Box and topology: **identical to `doc/pvalink-rtems-design.md` §5 stage 5
topology A** — QEMU `-M xilinx-zynq-a9 -m 256M -nic user,model=cadence_gem`,
SLIRP, guest at `10.0.2.15`, host reachable at `10.0.2.2`, `hostfwd` host port
equal to guest port. That section's setup is not repeated; only the CA
substitutions are:

| pvalink stage 5 | this stage |
|---|---|
| upstream `softIocPVX` on the host | upstream `softIoc` (C base) on the host, serving one `ai` and one `ao` |
| `EPICS_PVA_NAME_SERVERS=10.0.2.2:5075` | `EPICS_CA_NAME_SERVERS=10.0.2.2:5064` **plus** `EPICS_CA_ADDR_LIST=` and `EPICS_CA_AUTO_ADDR_LIST=NO` (§4.5) |
| `INP=pva://UPSTREAM:AI CP` (that doc spelled it `@pva://` when this row was written; corrected there — see §10.7 and `doc/pvalink-rtems-design.md` §12.2) | `INP=UPSTREAM:AI CP` (the bare ` CA`-modifier form, C `pvlOptCA`) **and** a second record with `INP=ca://UPSTREAM:AI CP` — both spellings resolve through `strip_ca_scheme`, and both should be in the gate |
| `pvxget` / `pvxput` from the host | `caget` / `caput` from the host |
| forwarded `tcp::5075-:5075` | forwarded `tcp::5064-:5064` |

*Pass criteria, each an observation on the console or on the wire, because the
target has no iocsh:*

1. Console banner reports the calink resolver installed and the link count,
   not silence.
2. `caget` from the host against the **guest's** downstream record returns the
   upstream's value.
3. `caput` to the upstream changes the guest record within one scan period —
   proving the **monitor** path, not just a GET.
4. Kill the upstream: guest record goes `LINK`/`INVALID`. Restart it: the
   record recovers **without** rebooting the guest — proving reconnect
   survives the 1 s clock quantum (§5.5).
5. An OUT link (`OUT=ca://UPSTREAM:AO`) writes and is observed upstream —
   and both `LinkPutOp::Plain` (`put_nowait`) and `LinkPutOp::Async` (`put`)
   are exercised, because `resolver.rs:810-811` routes them to different wire
   operations and only the C-parity split makes that distinction meaningful.
6. Thread count and per-thread stack peaks match §5.2's arithmetic to within
   one thread, **and** the circuit count to the upstream is **1** regardless
   of link count (§2.4's property, on target).
7. `FD_CNT` on the status PVs equals the pre-link value plus exactly the
   circuits opened (§5.3) — this is the measurement that says the fd coupling
   is arithmetic and not a surprise.

*Gate:* all seven criteria, each with the command and its output pasted into a
measurement document, in the shape of
`doc/rtems-priority-on-target-measurement.md`. Anything short of that is stage
C5 with extra confidence.

**Status (2026-07-23): all seven criteria PASS on `bb7b40ab`.** Measured on
the bring-up box, guest image built from that commit ("boot 5"), console at
`~/rtems-bringup/c6/guest5.log`. Four defects were found on target and fixed at
source, one commit each, before the criteria could be claimed — two runtime-seam
holes that panicked `cbMedium` (§§11.1-11.2), a banner that reported a link registry
still being filled (§11.3), and a `ca://` link disconnect that never processed
its CP holders, which is criterion 4 itself (§11.4). The stage C4 measurement
this stage was made a precondition of is §11.6.

**Sequencing against pvalink stage 5:** both need the same box, the same SLIRP
route and the same `hostfwd` discipline. Whichever runs first should record the
outbound-SLIRP verification (sibling §6 item 4, currently UNVERIFIED) once, for
both.

---

## 7. Unverified — needs measurement

Everything here is a claim this document could **not** settle.

1. **`client/mod.rs`, `subscription.rs`, `types.rs`, `state.rs`,
   `sync_group.rs`, `circuit_breaker.rs` are transport-independent.** They
   report zero errors under both probes and the source-text census
   corroborates (zero `tokio::net`, zero `socket2`, zero `libc::`, all handles
   on the seam). But rustc suppresses downstream errors after a poisoned
   import, and probe A → probe B *demonstrated* that suppression is real in
   this crate (§1.2). Stage C2's gate settles it.
2. **calink's own RTEMS error surface is unmeasured, and unmeasurable today.**
   Un-gating `calink` for the target stops at its dependency — the client's
   errors — so the bridge of `calink` is never reached. A stub-client probe
   was considered and rejected for the same reason the sibling document
   rejected it, and here the reason is concrete rather than general:
   `tokio::runtime::Handle` and `tokio::task::JoinHandle` both **compile** for
   the target (`Cargo.toml:81-92` keeps tokio's `rt`), so such a probe would
   report calink green while `Handle::current()` (`calink/mod.rs:75`) panics at
   boot. Source-text census is what we have: `calink/*.rs` names zero socket
   types, zero `socket2`, zero `libc::` — and four seam violations (§2.5).
3. **Whether §5.4's band occupancy is a real problem.** The mechanism is
   measured (`run_monitor` → `dispatch_external_cp_targets` →
   `process_record_with_links_recursive`, all synchronous, all on one band
   worker). What is *not* measured is whether it causes observable latency on
   target for realistic link and CP-holder counts. This is the one place where
   the CA design might need a semantic change (stage C4), so it is the
   highest-value unknown in this document. It composes with the sibling's §6
   item 3 and with QSRV group forwarders on the same band.
4. **Whether `FIONREAD_REQUEST = 0x4004_667F` is right on target.** Derived
   from RTEMS newlib headers, not measured on the box — the CA server's own
   doc comment says so (`blocking.rs:628-631`) and notes the failure mode is
   benign (every caller flushes). Hoisting it in stage C2 does not make it
   more verified; the QEMU/BSP phase should check it once, for both users.
5. **Whether the CA client's circuit sharing holds under `calink`'s access
   pattern on target.** `CircuitKey = (SocketAddr, u8)` is source-measured
   (§2.4); that N links to one IOC yield one circuit is a host property that
   stage C6 criterion 6 checks on target for the first time.
6. **`repeater.rs` scoping.** §2.1 asserts a record link needs no repeater
   registration, from calink's call surface. C's client registers with a
   repeater for beacon fan-out (`repeaterSubscribeTimer.cpp:84-90`), and
   beacons drive libca's search re-poke on server restart. Whether a
   target IOC's `ca://` links reconnect acceptably *without* beacon-driven
   re-poke — relying only on the `EPICS_CA_CONN_TMO` retry cadence — is
   untested. Stage C6 criterion 4 is the first evidence either way.
7. **`hostname.rs`'s two errors.** Assumed to be droppable (reverse-DNS for a
   diagnostic peer-name cache). Whether any production client path *requires*
   a resolved peer name — as opposed to logging one — was not audited.
8. **Whether stage C2's base-crate primitive fits the PVA track unchanged.**
   §3.3 argues `ChannelReader`/`ChannelWriter` should be built once in
   `epics-base-rs`. Whether they carry `server_native`-specific assumptions in
   their back-pressure accounting is the sibling document's §6 item 7, still
   unread, and it is now a *cross-track* risk rather than a PVA-only one.
9. **Outbound SLIRP from guest to `10.0.2.2`.** Inherited unchanged from the
   sibling's §6 item 4. Stage C6 depends on it exactly as pvalink stage 5
   does; verify once, for both.
10. **calink's C-parity line citations drift from the checked-out
    reference.** Spot-checked against
    `/home/stevek/work/epics-base/modules/database/src/ioc/db/dbCa.c`: some of
    `calink/resolver.rs`'s citations are exact (`dbCaGetLink` at `:448`,
    `pcaGetCheck` at `:650`), while others are off by 15-300 lines
    (`eventCallback` cited `:925`, measured `:940`; `db_process` cited
    `:1295`, measured `:1320`; `CA_DBPROCESS` cited `:993-994`, measured
    `:871`/`:1011`; `dbCaAddLink` cited `:735-800`, measured `:425`/`:415`).
    The *claims* those comments make were verified correct here; only the
    coordinates are stale, presumably written against a different base
    revision. Not a defect in this design, but it means a reader cannot
    navigate from calink to C by line number, and no citation in this
    document is inherited from those comments without re-measurement.

---

## 8. Stage C1 as built — where reality deviated from §6

Stage C1 landed as `800857e7` on top of `8535d998`. The sum type matched
§6, and the CA-specific claims §4 made about it held. Five points did not
survive contact with the compiler untouched, and the feature-ON gate
diverged from the sibling in a way the two protocols' spawn seams predict.

### 8.1 The variant holds the destinations and the diagnostics, not just the socket

§6 wrote the type as `SearchTransport::{Udp(AsyncUdpV4, …), NameServersOnly}`.
The membership test is the sibling's (`doc/pvalink-rtems-design.md` §8.1):
a field belongs in the variant iff it cannot outlive the socket it
describes. Three did, beyond the socket bundle itself:

```
SearchTransport::Udp(Box<UdpTransport>)
    socket                 AsyncUdpV4
    addr_list              Vec<AddrEntry>                 // UDP SEARCH destinations
    send_errors            HashMap<SocketAddr, ErrorKind> // libca _lastError suppression
    prev_drops_per_iface   HashMap<Ipv4Addr, u32>         // SO_RXQ_OVFL transition log
SearchTransport::NameServersOnly                          // no fields
```

`addr_list` was previously a `run_search_engine` parameter threaded through
`fire_searches` / `process_bucket` by reference; it is a list of UDP SEARCH
destinations (CA never opens a TCP circuit to an `EPICS_CA_ADDR_LIST` entry —
only to an `EPICS_CA_NAME_SERVERS` one), so it moved inside. `send_errors`
keys its suppression by those same destinations, and `prev_drops_per_iface`
keys by the receiving NIC of the bound bundle; both were function-local state
in the old `run_search_engine` and both moved in. Leaving any of them outside
would have reproduced, one level up, the "present but unusable" split the sum
type exists to close.

This is smaller than PVA's `UdpTransport` (§4.3 predicted it): CA has no v6
sockets, no beacon sockets, no `response_port` fields — a `CA_PROTO_SEARCH`
reply returns to the datagram source, so no response port is ever stamped
(§4.4), and there is no beacon listener to fold in. So the CA variant has
none of the "beacons cost fast-reconnect" tension the PVA one carried
(`doc/pvalink-rtems-design.md` §8.1): `NameServersOnly` loses UDP search and
nothing else, because there was nothing else on the UDP socket to lose.

`Box` on the payload variant for the same reason PVA needed it: without it
`SearchTransport` is as large as `UdpTransport` in the `NameServersOnly` case
that carries nothing (`clippy::large_enum_variant`).

### 8.2 The selection is an explicit entry point, not derived from config

Identical decision to the sibling (`doc/pvalink-rtems-design.md` §8.2), same
reasoning. `name_servers_only_search_engine(…)` is a second free function
beside `run_search_engine`, not a config-derived branch. Deriving
`NameServersOnly` from "`addr_list` empty + `AUTO_ADDR_LIST=NO`" would drop
UDP search for every host client already in that configuration — including
one that expects a later `AddAddress` / discovery event to add a destination.
So the address-list mutations (`AddAddress` / `RemoveAddress` /
`SetAddressList`) are not errors on a `NameServersOnly` engine; they are
logged at debug and dropped, because their target — a UDP SEARCH destination
list — does not exist on that variant.

Consequences worth stating plainly, matching the sibling:

* **No production caller selects it yet.** Stage C1 builds the capability;
  the RTEMS mount (stage C5) is what will choose it. `name_servers_only_search_engine`
  therefore carries a conditional `expect(dead_code)` that is live in every
  configuration except the host test that exercises it.
* The refusal is load-bearing: `SearchTransport::name_servers_only` returns
  `Err(CaError::InvalidValue)` on an empty `EPICS_CA_NAME_SERVERS`, because
  an engine with no UDP socket and no name server can reach nothing. The
  free function returns the engine *future* rather than running it, so that
  error is observed by the caller before any task is spawned.

### 8.3 The `[u8]` cascade did not surface anything on the host — and §1.3 predicts that

§6's stated risk: "Expect the arm-shape change to surface real type errors
the cascade was hiding." On the host it surfaced **none** — the first compile
after the arm conversion was clean. This is the sibling's §8.3 result for the
same reason: the 13-error `[u8]` cascade at `search.rs:592` is inference
fallout from a *target-only* poisoned import (`AsyncUdpV4` is
`#[cfg(not(target_os = "rtems"))]`), so on the host there was never a hidden
error for the change to expose. The risk is real for the target and stays
unretired — it is re-measured when the RTEMS arm actually compiles this file
(stage C2+), not here.

### 8.4 The feature-ON gate diverges from PVA — because the CA name-server spawn does

The sibling's stage-1 gate (`stage1_name_servers_only_resolves_without_binding_udp`)
runs and passes under `--features rtems-exec-model` (`doc/pvalink-rtems-design.md`
§8.7). The CA equivalent **cannot**, and the reason is a real protocol-port
difference, not a test artefact:

* PVA spawns its `ns_task` and its engine with **`tokio::spawn`**
  (`search_engine.rs:752`, `:1406`) — the real tokio runtime, whose I/O
  reactor drives the name-server `TcpStream`.
* CA spawns `run_nameserver_connection` with
  **`epics_base_rs::runtime::task::spawn`** (`search.rs:556`) — §4.2 recorded
  it as "already on the spawn seam", which under the exec backend routes the
  task onto the `cbMedium` band. That band worker has no tokio I/O reactor, so
  `TcpStream::connect` panics there (`there is no reactor running`).

So the CA gate's *resolution* half — which dials a mock TCP name server — is
per-test `#[cfg(not(feature = "rtems-exec-model"))]`. Making
`run_nameserver_connection` drive real sockets under the exec backend is
Stage C2 (the blocking byte source) / C3 (the spawn seam), not this stage.
The *structural* half of the claim — that `NameServersOnly` can hold no UDP
socket — is a property of the type and is additionally asserted by
`name_servers_only_drops_address_list_mutations`, which runs in **both**
configurations.

This is the first concrete instance of the §4.2 note that the CA name-server
path being "on the spawn seam" is a host fact whose target behaviour is
Stage C2/C3's to establish — and it means the CA and PVA stage-1s are *not*
byte-for-byte the same exercise under feature-ON, though they are on the host.

### 8.5 The census marker, and the two pre-existing RTEMS-gate warnings

`search.rs` carries `RTEMS-EXEC-MODEL-ALLOW(N)`. The change added one
end-to-end `#[tokio::test]` (`stage_c1_…`, per-test gated off under the
feature, so **not** counted) and converted one existing `#[test]`
(`add_then_remove_address_round_trip`) to `#[tokio::test]` because the
destination list now lives inside `UdpTransport`, whose construction binds a
socket and needs a reactor. Net ungated count 4 → 5; the marker moved `4 → 5`
with a note that the sixth site is gated. Verified inside the feature-ON
suite: `cargo nextest run -p epics-ca-rs --features rtems-exec-model` gives
608 passed, `every_reactor_dependent_test_is_accounted_for` among them.

`./scripts/rtems-check.sh` stays exit 0. Its two known warnings are unchanged
in count and identity and both live in **`epics-pva-rs`** — `tcp.rs:1407`
(deprecated `fetch_update`) and `server_native/search_engine.rs:501` (dead
`Origin` variants). This stage touches only `epics-ca-rs/src/client/search.rs`,
whose `client` module is `#[cfg(not(target_os = "rtems"))]`, so it contributes
nothing to the RTEMS-target compile at all — C1 has zero target footprint by
construction, which is exactly what §1.4 said the search-without-UDP capability
would have until C2 brings the client into the target build.

### 8.6 The gate as measured

| gate | result |
|---|---|
| `stage_c1_name_servers_only_resolves_without_binding_udp` | pass — resolves via TCP NS, `bound_udp_addrs()` empty, UDP-transport control non-empty |
| `name_servers_only_drops_address_list_mutations` | pass — runs in both feature configs |
| `name_servers_only_refuses_empty_name_server_list` | pass |
| `cargo nextest run -p epics-ca-rs` | 724 passed, 0 skipped |
| `cargo nextest run -p epics-ca-rs --features rtems-exec-model` | 608 passed, 0 skipped (census gate green) |
| `cargo clippy -p epics-ca-rs --all-targets -- -D warnings` | clean |
| `cargo clippy -p epics-ca-rs --all-targets --features rtems-exec-model -- -D warnings` | clean |
| `cargo test --doc -p epics-ca-rs` | 0 passed, 4 ignored |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo nextest run --workspace` | 10115 passed, 2 skipped |
| `./scripts/rtems-check.sh` | exit 0, 2 pre-existing `epics-pva-rs` warnings unchanged |

## 9. Stage C3 as built — where reality deviated from §6

Stage C3 landed on top of `f724ef81` as three commits (calink resolver
onto the seam; the `CaClient::drop` survivor; the source-text guard),
with this doc as the fourth. The §6 shape held — the three `handle.spawn`
calls, the `AbortOnDrop` retype, and the `Handle::current()` drops are
exactly as written. Four points did not survive contact with the
compiler untouched.

### 9.1 A nullary `new()` forces a `Default` impl

§6 said "delete `CaLinkResolver::handle`", which drops the
`tokio::runtime::Handle` argument from `new`, `with_client` and
`install_calink_resolver`. `CaLinkResolver::new()` thereby becomes
nullary and public, which `clippy::new_without_default` refuses without a
`Default`. So an `impl Default for CaLinkResolver { fn default() ->
Self { Self::new() } }` was added — not in §6's line budget, but the
resolver's fields (`Arc<OnceCell>`, two `Arc<RwLock<…>>`) all default to
the same empty state `new()` builds, so it is delegation, not a second
constructor. The four caller sites the signature change ripples to —
`calink/mod.rs:75`, `iocsh.rs`'s `dummy_resolver`, and the two test
files `tests/calink.rs` (7 `with_client`, 2 `install_calink_resolver`)
and `tests/calink_lset_contexts.rs` (2 `new`) — were updated to compile;
they are the only workspace callers (`register_link_set_installer(calink_link_set_install)`
in `ad-plugins-rs` and the bridge is unaffected because
`calink_link_set_install`'s own signature is unchanged).

### 9.2 The `CaClient::drop` survivor loses its guard *and* its fallback

§6 / §5.1 said "route `client/mod.rs:1374`'s surviving bare
`tokio::spawn` through the seam and delete its `try_current` guard." The
guard was one arm of an `if try_current().is_ok() { spawn graceful } else
{ abort four handles directly }`. Removing the guard makes the graceful
spawn unconditional, which leaves the `else` immediate-abort branch dead
— so it was removed too. The observable consequence, stated for the
record: on the host `tokio_backend`, a `CaClient` dropped on a thread
with no entered runtime previously fell back to a synchronous abort;
it now spawns through the seam unconditionally (which on that backend is
`tokio::spawn`). That path is the same one every one of the client's 17
already-converted production spawns takes; on the exec backend the seam
dispatches onto the callback band, which is the whole point — the guard
had been silently skipping the bounded coordinator shutdown there.

### 9.3 The guard is scoped to the three calink files, not `client/mod.rs`

§6 asked for a guard "in `epics-ca-rs` mirroring
`epics-pva-rs/src/server_native/tcp.rs`", noting "the client would pass
this guard today; calink would not." The PVA guard slices production
scope at the file's *first* column-0 `#[cfg(test)]`. That works for the
three calink files (`resolver.rs`, `mod.rs`, `iocsh.rs`), whose test
modules sit at the end — but not for `client/mod.rs`, which interleaves
production code *after* its first `#[cfg(test)]` at `:1559` (e.g. the
already-converted `runtime::task::spawn` at `:2951`). A single-slice
guard over `client/mod.rs` would either miss that production or trip on
test code. So the guard is scoped to the calink surface — which is
precisely the asymmetry §6 named it to close — and covers all three
files, asserting each production slice spawns through
`runtime::task::spawn` and holds no bare `tokio::spawn(`, no
`handle.spawn(`, and no `tokio::runtime::Handle` field/call. It is
mutation-proven (injecting a bare `tokio::spawn` into `mod.rs` fails it).
The `client/mod.rs:1374` fix is verified by the feature-ON suite, not by
this guard.

### 9.4 No census marker

The guard is a plain `#[test]` doing source-text inspection with no
runtime — not a `#[tokio::test]` and not a hand-built runtime — so it is
not a reactor-dependent site under the `rtems-exec-gate` census, and
`resolver.rs` carries (and needs) no `RTEMS-EXEC-MODEL-ALLOW(N)` marker.
The feature-ON `epics-ca-rs` count rose 608 → 609 purely by that one
extra plain test; the census floor is untouched.

### 9.5 The gate as measured

| gate | result |
|---|---|
| `calink_production_spawns_go_through_the_runtime_seam` | pass; mutation-proven (fails on an injected bare `tokio::spawn`) |
| `cargo fmt --all` | clean |
| `cargo clippy -p epics-ca-rs -p epics-bridge-rs --all-targets -- -D warnings` | clean |
| `cargo nextest run -p epics-ca-rs --features rtems-exec-model` | 609 passed, 0 skipped |
| `cargo nextest run -p epics-ca-rs -p epics-bridge-rs` | 1409 passed, 0 skipped |

Stage C4's `dispatch_external_cp_targets` CP fan-out semantics are
deliberately untouched — that change awaits sign-off (§6 Stage C4); C3 is
spawn plumbing only.

## 10. Stage C5 as built — where reality deviated from §6

Ten commits, `d28b1c6d..11175169`. §6 sized C5 as "small: select the feature,
un-gate `calink`, install the resolver, replace the banner". Four of the ten do
that. The other six are what stood between the mount and a client that could
compile for the target at all — stage C2's CA half, which had never landed —
plus three defects the first boot of the mounted binary surfaced.

### 10.1 C5 subsumed stage C2's CA half

§6 C2 is written as a `epics-base-rs` primitive plus a CA consumer. The base
primitive landed with the PVA track; the CA consumer did not, so C5 opened with
the client still naming `tokio::net` and `socket2` and still 15 errors from the
target. Four commits closed it, all through the `SearchTransport`/`dial_ca`
shapes §6 asked for:

| commit | what |
|---|---|
| `274a734b` | the `client-core` / `client` feature split C2 called for as a *stated* split; ratchet opened at 15 |
| `a58322d7` | `FIONREAD_REQUEST`/`pending_bytes` hoisted to `runtime::blocking_io` — one owner for the receive-queue probe (C2 item 1). 15 → 14 |
| `8d834de3` | `dial_ca`: one seam, `tokio::net::TcpStream` hosted / `runtime::blocking_io`'s two-thread pump on target, with the `socket2` keepalive replaced by raw `libc::setsockopt` there. 14 → 11 |
| `aa91860b` | `run_nameserver_connection`'s dial through the same seam. 11 → 10 |

**Deviation from C2's text, stated:** the inline framing loop in
`run_nameserver_connection` (`search.rs:783-890`) was **not** folded into the
generic `read_loop`. C2 asks for "one seam, two callers, not two seams and three
framing loops". The dial is now one seam; the framing is still two loops. Left
open deliberately — the fold is a behaviour-preserving refactor of ~100 lines
with no target consequence, and doing it inside the stage that had to keep the
target build moving would have mixed a refactor into a portability change.

*Closed at `36102cc7`, as its own change and with the coverage the refactor
needed: §10.11 item 1. The deviation stood for exactly as long as this
paragraph said it would.*

**Deviation, measured, worth keeping:** the first `dial_blocking` used
`std::net::TcpStream::connect_timeout(addr, connection_timeout())` and failed
`tcp_connect_has_no_application_level_deadline` — a test that exists precisely
to forbid an application deadline on connect (C parity: connect is the OS's
business). The structural answer is not a longer timeout: the unbounded
`std::net::TcpStream::connect` runs on a dedicated thread
(`spawn_dedicated_thread`, `CAC_RECV_PRIORITY`, `StackSizeClass::Small`) and
delivers over a `oneshot`, so no shared cooperative-executor worker is parked
and no deadline is invented.

**Priorities, from C, not chosen:** libca gives a circuit's recv and send pumps
*different* priorities — `tcpiiu.cpp:677-682` takes the highest-below and
lowest-above of the initializing thread. `drive_socket_blocking` therefore takes
two priorities rather than one; `CAC_RECV_PRIORITY` = Medium−1 = 49,
`CAC_SEND_PRIORITY` = Medium+1 = 51, around `dbCa.c:340`'s
`epicsThreadPriorityMedium`.

### 10.2 The last 10 errors were UDP, and closed like PVA's 28

`2db37555`. Identical shape to pvalink stage 4: on the target
`search::SearchTransport` has the single `NameServersOnly` variant, so "no UDP
socket" is a fact about the type. `UdpTransport`, `bind_udp`, the
fanout/`note_drops`/DNS-refresh helpers, `run_search_engine`, and the whole
`EPICS_CA_ADDR_LIST` parse (`AddrEntry`, `resolve_host`,
`parse_addr_list_with_hostnames`, `append_auto_addr_entries`) are
`#[cfg(not(target_os = "rtems"))]`.

Two mechanical consequences §6 did not anticipate:

* `tokio::select!` rejects `#[cfg]` on a branch (measured). The receive arm's
  payload became an unconditional `SearchDatagram { n, src, iface_ip }` returned
  by a cfg'd `recv`, so the `select!` body has one shape on both targets.
* `add_address`/`remove_address`/`set_address_list` stay public and callable on
  the target; their `NameServersOnly` arm routes through one
  `log_dropped_udp_mutation` so a mutation nothing will read says so once.

### 10.3 The ratchet was retired, not pinned at zero

`afe3c812`. §6 asks for `CRATE_FEATURES[epics-ca-rs]="client-core"`. Once that
lands, the `--lib` loop builds exactly what `CA_CLIENT_TARGET_ERRORS` measured —
in **both** configurations, where the probe only ever ran the portability one —
and the `--bin` loop then links `rtems-ca-ioc` on top of it. "The count is 0" and
"the build must succeed" are the same assertion, so keeping both would leave a
check that can only fail after another already has. The measured history
(22 → 15 → 14 → 11 → 10 → 0) stays in the script: it is not reconstructible from
the code. The PVA probe stays, because `CRATE_FEATURES` has no `epics-pva-rs`
entry and `--features client` is built nowhere else.

This is stricter than the pin, not looser. Nothing was allowed to regress.

### 10.4 `lib.rs` needed no un-gating

§6 says "un-gate `calink` in `lib.rs:37-38`". By the time C5 reached the mount,
`274a734b` had already put `calink` behind `feature = "client-core"` with no
target predicate, which is the un-gated state. No further edit.

### 10.5 The mount alone is inert — iocInit's link phases were missing

`fb62f164`, and the largest deviation from "small".

`initialize_link_locality`, `setup_cp_links` and `setup_external_link_opens` are
called from exactly one place in the workspace: `IocApplication::run`
(`ioc_app.rs:913-925`). `rtems-ca-ioc` does not go through it — it assembles the
database with `IocBuilder::build`, which runs none of them. So a resolver
installed by itself is registered and unreachable:

* without `initialize_link_locality`, `INP=UPSTREAM:AI CP` — the bare
  ` CA`-modifier spelling, C `pvlOptCA`, and one of the two spellings §6 C6
  gates on — stays a local `Db` link to a record that does not exist;
* without `setup_cp_links`, a Passive holder of an external CP link never opens
  it (never scanned ⇒ never lazily opened ⇒ no monitor);
* without `setup_external_link_opens`, every other external link waits for a
  cold scan cycle C does not spend.

In all three the IOC boots, serves and answers searches with its links dead.
They run **after** the mount: `setup_cp_links`' warm path goes through
`resolve_external_pv`, documented as a no-op when no link set is installed.

Family audit. `rtems-pva-ioc` is **distinct** — `install_pvalink_resolver` walks
the database itself and pre-registers every `pva://` link including its CP/CPP
scan targets, so its mount is not inert; `install_calink_resolver` scans nothing
because for CA the scan *is* these three passes. `dual_ioc_rs` and `oracle_ioc`
are distinct: neither mounts an external link resolver at all.

### 10.6 The target client's timers were tokio's — measured by booting it

`46942711`. Booting the mounted binary with `--features rtems-exec-model` and a
`ca://` link panics a callback-pool worker: *"there is no reactor running, must
be called from the context of a Tokio 1.x runtime"*. This entry point starts no
runtime, so every `tokio::time::*` the client reaches is a panic on target.

`runtime::task` already owns `sleep`/`interval`/`timeout`. Anchor
`tokio::time::`, every production site classified against the target build:

**Routed through the seam** — `client/search.rs:22,1011,1028,1109,1113` (the
search engine's tick and DNS-refresh intervals; `run_engine` is *not* UDP-gated,
it is the shared engine the name-servers-only path runs too),
`client/search.rs:1153,1183,1339` (name-server reconnect backoff, including the
one `aa91860b` added), `client/mod.rs:1084` and `:1461` (the two drain bounds —
`:1461` sits *inside* a future already spawned through `runtime::task::spawn`, so
§5.1 fixed the spawn and left the timer in it), `client/sync_group.rs:173`,
`calink/resolver.rs:447`.

**Distinct** — `server/tcp.rs:58` and `server/udp.rs:773` are already
`cfg(not(target_os = "rtems"))`; `repeater`, `discovery/*`, `server/ca_server`,
`server/introspection`, `client/beacon_monitor` are module-gated off the target
or behind the full `client` feature; `chaos.rs:137` compiles on target but its
only caller is `server::tcp::handle_client`, itself off-target; `replay.rs:236`
is reachable only from `ca-replay-rs` (HOST_ONLY); `sync_group.rs:259,306` and
`bin/*` are tests and host CLIs.

**What this does not do, stated** (closed since, §10.11): the hosted
`rtems-exec-model` build still cannot boot a link. `AsyncUdpV4` needs a reactor,
the host build compiles the UDP SEARCH transport in and selects it, so the same
boot panics one layer down at `net/async_udp_v4.rs:1275` — and `rtems-pva-ioc`
panics **identically at the same line**, measured. That is a property of the hosted exec-model build, not of this
stage; on the target the UDP transport does not exist. It also means §6 C6 is not
merely the *better* gate for the client path, it is the **only** one.

### 10.7 Both banners named a link spelling the loader rejects

`11175169`. Measured:

```
rtems-ca-ioc: iocInit failed: invalid value: ai.INP: can't initialize link
type CONSTANT with "@ca://UPSTREAM:AI CP" (type INST_IO)
```

`@` is the INST_IO sigil: `try_parse_hw_link` (`link.rs:1074-1086`) claims any
field starting with `@` and returns `ParsedLink::Hw` before the scheme arm
(`link.rs:1343`) is consulted. Only `ca://PV` reaches it. Twelve sites across
`rtems-ca-ioc.rs` and `rtems-pva-ioc.rs` said `@ca://` / `@pva://`; `@pva://` is
rejected identically (`try_parse_hw_link` is protocol-agnostic), measured with
the same boot. All twelve corrected.

**§6's C6 table carries the same error** — it specifies
`INP=@ca://UPSTREAM:AI CP` as the second spelling to gate. Corrected in place
below; the two spellings C6 must exercise are `UPSTREAM:AI CP` and
`ca://UPSTREAM:AI CP`.

### 10.8 What the mount does on a host, measured

Booting with three links (`UPSTREAM:AI CP` bare, `ca://UPSTREAM:AI CP`,
`OUT=ca://UPSTREAM:AO`), `EPICS_CA_NAME_SERVERS=127.0.0.1:15999`,
`EPICS_CA_ADDR_LIST=` and `AUTO_ADDR_LIST=NO`:

```
iocInit: 1 non-local DB link(s) made external
iocInit: 2 external CP link subscriptions (1 PVs warmed)
iocInit: 1 external link opens staged
rtems-ca-ioc: calink resolver installed — 0 ca:// record links registered; ...
```

Every link was seen and classified: the bare form converted, both CP links
subscribed, the OUT link staged. `link_count` is 0 because the open that would
populate the registry is the one the host build's UDP panic kills — on target
that path does not exist. Turning that 0 into a non-zero number on the wire is
C6 criterion 1.

### 10.9 The gate as measured

| gate | result |
|---|---|
| `cargo fmt --all` | clean |
| `./scripts/rtems-check.sh` (portability + image) | pass; `epics-ca-rs --lib` and `--bin rtems-ca-ioc` both `--features client-core` |
| `cargo +nightly check --target armv7-rtems-eabihf -p epics-ca-rs --lib --features client-core` | 0 errors, 0 warnings |
| `cargo clippy -p epics-ca-rs --all-targets -- -D warnings` | clean |
| … `--features rtems-exec-model` | clean |
| … `--no-default-features --features client-core` | clean |
| … `--no-default-features --features client-core,rtems-exec-model` | clean |
| `cargo clippy -p epics-base-rs -p epics-bridge-rs --all-targets -- -D warnings` | clean |
| `cargo nextest run -p epics-ca-rs` | 727 passed, 0 skipped |
| `cargo nextest run -p epics-ca-rs --features rtems-exec-model` | 611 passed, 0 skipped |
| `RUSTFLAGS="--cfg ca_blocking_client" cargo nextest run -p epics-ca-rs` | 727 passed, 0 skipped |
| `cargo nextest run -p epics-ca-rs -p epics-bridge-rs` | 1411 passed, 0 skipped |
| `cargo nextest run -p epics-base-rs` | 3522 passed, 0 skipped |
| `cargo test --doc -p epics-ca-rs` | pass |
| `rtems_exec_model_gate::every_reactor_dependent_test_is_accounted_for` | pass (no census marker needed: the two new guards are plain `#[test]`) |

Source-text guards on `rtems-ca-ioc.rs`, five now:
`entry_point_never_starts_a_runtime`, `the_entry_point_publishes_its_status`,
`a_panic_reaches_the_errlog_and_says_what_it_costs`, and new here
`the_calink_resolver_is_mounted_and_the_banner_reports_it` (the CA counterpart
of pvalink stage 4's) and `iocinit_link_phases_run_after_the_mount`.

### 10.10 Open after C5

1. ~~`run_nameserver_connection`'s inline framing loop is still not folded into
   `read_loop` (§6 C2, §10.1).~~ **CLOSED — §10.11 item 1.**
2. ~~The hosted `rtems-exec-model` build cannot exercise either link resolver's
   client path — `AsyncUdpV4` needs a reactor and both RTEMS IOC binaries panic
   at `net/async_udp_v4.rs:1275`.~~ **CLOSED — §12.** Not by a seam under
   `AsyncUdpV4`: the transport selection itself was the defect, and it now
   selects on the backend rather than on the target, so the exec backend has no
   UDP transport to reach a reactor with.
3. ~~`epics-bridge-rs/Cargo.toml`'s comment on its `epics-ca-rs` dependency
   still says the crate's "default feature list is empty"; `274a734b` gave it
   `default = ["client"]`.~~ **CLOSED — §10.11 item 2.**
3b. ~~`doc/pvalink-rtems-design.md` still spells the link `@pva://` throughout.
   The binaries it produced were corrected (§10.7); that doc was not, because
   it belongs to the PVA track.~~ **CLOSED — §10.11 item 3.**
4. Stage C4 remains deferred by sign-off. Nothing here touches `run_monitor`'s
   inline `dispatch_external_cp_targets` or any CP fan-out semantics; the three
   iocInit phases in §10.5 are existing functions called unchanged.
5. Stage C6 is the gate for everything above.

### 10.11 The five small UNFIXED items, closed

One commit each, off `5b4ad849`. Items 1–3 are this document's own open rows
above; items 4–5 are the two warnings every stage record in this document and
in `doc/pvalink-rtems-design.md` has had to repeat as "unchanged in count and
identity" (§8.5, pvalink §8.7, §9.9, §11.7). Those stage records stay as
written — each was true of the stage that measured it.

| # | item | commit | what closed it |
|---|---|---|---|
| 1 | the name-server circuit's inline framing loop (§6 C2, §10.1, §10.10 item 1) | `36102cc7` | `client/transport.rs::next_frame` — one framing step, two callers. The dial became one seam at `aa91860b`; this is the framing half, so C2's "one seam, two callers, not two seams and three framing loops" now holds in both halves. |
| 2 | the stale `epics-ca-rs` "default feature list is empty" comment (§10.10 item 3) | `61756a31` | Restated on the true reason — no target-selectable feature of `epics-bridge-rs` pulls the crate. The same false premise in `doc/qsrv-rtems-design.md` §9.2 went with it. |
| 3 | `doc/pvalink-rtems-design.md`'s `@pva://` spelling (§10.10 item 3b, §10.7) | `74122e72` | §5 stage 5's records now read `pva://`, as do seven `epics-bridge-rs` module/API doc comments carrying the same claim. Kept verbatim: §12.2's record of the refusal, the quoted `iocInit` error, and two quoted boot consoles. This document's `@ca://` was swept the same way and has **no** stale site — all three hits are §10.7 naming the rejected form. |
| 4 | `server_native/tcp.rs:1407` deprecated `fetch_update` | `f9008445` | The CAS loop `fetch_update` compiled to. Not the suggested rename: `try_update`/`update` are unstable (`atomic_try_update`, rust#135894) and do not build on the pinned 1.94 toolchain; `fetch_sub` wraps where the code needs saturation. |
| 5 | `server_native/search_engine.rs:501` dead `Origin::{FromOriginTag, Forwarded}` | `7235e6a5` | **Investigated, not deleted.** pvxs `udp_collector.cpp:63-68` does distinguish these and acts on it at `:373-374`, `:385-389`, `:508`, `:524`, so the hosted server needs them for anti-loop parity. They are dead only on the target, where their sole producer (`server_native::udp`) is not compiled and `blocking.rs`'s responder can emit `Origin::Direct` alone. Gated `#[cfg(not(target_os = "rtems"))]`, the shape `SearchTransport::NameServersOnly` took (§10.1). |

**The two long-carried warnings are gone.** Before, on every `rtems-check.sh`
run, six times each — once per lib check across both configurations:

```
warning: use of deprecated method `std::sync::atomic::Atomic::<u32>::fetch_update`: renamed to `try_update` for consistency
    --> crates/epics-pva-rs/src/server_native/tcp.rs:1407:19
warning: variants `FromOriginTag` and `Forwarded` are never constructed
   --> crates/epics-pva-rs/src/server_native/search_engine.rs:501:5
warning: `epics-pva-rs` (lib) generated 2 warnings (run `cargo fix --lib -p epics-pva-rs` to apply 1 suggestion)
```

After: `./scripts/rtems-check.sh` exit 0 with **zero** lines matching `^warning`
in the whole run, both the portability and the image configuration.

Coverage added, because two of these were refactors of code no test reached:

* `client/transport.rs::framing_tests` — seven per-boundary cases on
  `next_frame` (short header 0..15, exactly 16, partial extended annex 16..23,
  header-before-body, every misaligned base postsize, misaligned extended
  postsize, chained base+extended with a torn tail). The rules were previously
  reachable only through a live socket, which is how the two copies drifted.
* `client/search.rs::nameserver_reply_torn_across_reads_still_resolves` — two
  chained replies written one byte at a time. Measured: with framing stubbed
  out this fails ("resolved []") while the pre-existing
  `stage_c1_name_servers_only_resolves_without_binding_udp` still passes, which
  is why the inline loop had no regression cover.
* `server_native/search_engine.rs::the_blocking_udp_responder_produces_only_a_direct_origin`
  — pins the premise item 5's `cfg` rests on.



---

## 11. Stage C6 as built — the on-target two-IOC gate

Everything in §§8-10 was `cargo check`, host tests, and one host boot. This
section is the first time calink ran on the target with a real upstream IOC on
the other end of a real socket, and it is the only section in this document
whose claims are observations rather than readings of source.

**Image.** `bb7b40ab`, built on the bring-up box with `~/rtems-bringup/build-c6.sh`:

```
cargo +nightly build --release --target armv7-rtems-eabihf \
  -Zbuild-std=std,panic_abort --no-default-features --features client-core \
  -p epics-ca-rs --bin rtems-ca-ioc
```

**Rig.** Topology A, the pvalink stage-5 rig (`doc/pvalink-rtems-design.md`
§12) with the CA substitutions §6 tabulates. Scripts in `~/rtems-bringup/c6/`;
qemu invocation unchanged from the measured one:

```
qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
  -serial null -serial mon:stdio \
  -nic user,model=cadence_gem,hostfwd=tcp:127.0.0.1:5064-:5064 \
  -kernel c6ioc.exe
```

Upstream: C base `softIoc -d upstream.db` on the host, `EPICS_CA_SERVER_PORT`
and `EPICS_CAS_SERVER_PORT` both `15076`. Guest: DHCP `10.0.2.15`, host
`10.0.2.2`, `EPICS_CA_NAME_SERVERS=10.0.2.2:15076` compiled in (a `-kernel`
boot has no filesystem and `rtems_init.c:195` hands `main` a fixed
one-element argv, so the topology cannot come from the command line —
`doc/pvalink-rtems-design.md` §12.3's forcing, unchanged for CA).

**Outbound SLIRP is verified** (the sibling's §6 item 4, previously
UNVERIFIED, recorded here once for both tracks): the guest reaches
`10.0.2.2:15076` with **no** `hostfwd` for that port. Only the INBOUND
direction needs one, and there the host port must equal the guest port.

### 11.1 Target finding 1 — `tokio::task::JoinSet`, a fourth spelling of `tokio::spawn`

First boot, console verbatim:

```
panic on thread cbMedium at crates/epics-ca-rs/src/client/transport.rs:790:
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

`JoinSet::spawn` in the transport manager's pending-connect set. Stage C3
pinned *spawns* and §10.6 pinned *timers*, and this shape is neither: no
"no bare `tokio::spawn`" needle matches `JoinSet`, and a `JoinSet` field is
not a `TaskHandle`. It compiles for `armv7-rtems-eabihf` and panics the band
worker at first use.

Fixed structurally in `a04684bb` by adding `runtime::task::TaskSet` to the
seam — a join-set with the same shape on both backends — rather than by
open-coding a `Vec<TaskHandle>` at the one call site. The same commit moved
`read_loop`/`write_loop`'s timers onto the seam, which required adding
`runtime::task::Instant` (a `std::time::Instant` handed to a tokio timer is
a deadline in a different timeline under `#[tokio::test(start_paused = true)]`
— that surfaced as three host failures and is a latent host bug the target
found).

### 11.2 Target finding 2 — the channel round-trip bounds

Second boot, four of these:

```
panic on thread cbMedium at crates/epics-ca-rs/src/client/mod.rs:2605:
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

`tokio::time::timeout` in the channel read path. The finding-1 sweep missed
this file because the production slice it used cut at the first
`#[cfg(test)]`, which in `client/mod.rs` sits above most of the channel API.

Fixed in `0d56cf43`: ten sites in `client/mod.rs` onto the seam, with
`runtime::task::timeout_at` added because `caget_many` shares ONE deadline
across N channels and per-call `timeout` would have changed the semantics.
The seam guard (`runtime_seam_guard`) now runs over a `TARGET_LIVE` table of
(file, anchors) covering `client/transport.rs` AND `client/mod.rs`, with the
slice rule corrected to "first column-0 *inline* `mod X {`" so a file that
opens with eight `mod x;` declarations is sliced correctly. Verified to fail
on the pre-fix tree:

```
client/mod.rs: production slice no longer contains `runtime::task::timeout_at(`
```

**What this commit cost elsewhere, and what caught it.** `timeout_at` needs an
absolute instant, and on the hosted backend that instant must be tokio's:
tokio's clock is virtual under `#[tokio::test(start_paused = true)]`, so a
`std::time::Instant` handed to a tokio timer is a deadline in a different
timeline. `0d56cf43` therefore named `runtime::task::Instant` as a backend
alias and re-typed `sleep_until` from `std::time::Instant` to it. That is a
signature change on a seam function with callers outside the two crates the
per-crate gate covers, and it broke one: `epics-pva-rs`'s
`client_native/search_engine.rs` imported `Instant` from `std` and passed it
to the seam's `sleep_until` (two sites). The crate-scoped
`clippy -p epics-ca-rs -p epics-base-rs` was green with the workspace not
compiling. `cargo clippy --workspace --all-targets` caught it; fixed in
`935c59ac` by taking `Instant` from the seam in that file too — the same
correction, since it also stamps its deadlines with `now() + window` and waits
on them with that same `sleep_until`. **The rule this pins: a change to a
`runtime::task` signature is by definition cross-crate and must be gated at
workspace scope, never per-crate.**

### 11.3 Target finding 3 — the banner counted a registry that was still filling

Criterion 1, third boot:

```
iocInit: 4 external CP link subscriptions (3 PVs warmed)
iocInit: 1 external link opens staged
rtems-ca-ioc: calink resolver installed — 0 ca:// record links registered; ...
...
C6 seq=1 links=4 circuits=Some(1)
```

Not a display bug. iocInit's two link phases STAGE opens; each open runs on
the link work owner and registers its `CaLink` only after a `subscribe()`
round trip to the upstream returns. A count sampled the instant `run()`
returns is a race against the network, and the banner's own comment ("Taken
at the last moment before the banner, it is the registry as the first client
will find it") asserted the opposite of what the target does.

Fixed in `a487dd07`. `PvDatabase::external_link_pv_names` is new: the
DISTINCT external PV names every link field names, enumerated through
`record_link_fields` + `external_pv_name`, the same two owners both iocInit
phases use. That is the set the registry converges to and therefore the only
honest denominator — iocInit's own counts are per link FIELD (two records
reading one upstream PV are two links, one registry entry), which is why they
print 4 and 1 while the registry holds 4 PVs. The banner waits for the
registry to reach it, bounded at 10 s so an unreachable upstream still boots
and still prints, with the shortfall named.

### 11.4 Target finding 4 — criterion 4 itself: a dropped link never alarms

Fourth boot, the criterion-4 run. The resolver saw the drop; the records did
not. Console verbatim during the outage:

```
C6 seq=30 links=4 circuits=Some(0)
C6 seq=30 link pv=UPSTREAM:AI connected=false
C6 seq=30 record RTEMS:CA:DOWN VAL=Ok("133.75") SEVR=Ok("0") STAT=Ok("0")
```

Across that whole boot: 24 `connected=false` link samples, 6 samples at
`circuits=Some(0)`, and **63 of 63** `RTEMS:CA:DOWN` samples at `SEVR=0
STAT=0`. Host side, same window:

```
$ ss -tn state established '( dport = :15076 )' | wc -l
0
```

Root cause. C `connectionCallback` (`dbCa.c:848-873`) does TWO things on a
disconnect: clears `pca->isConnected` AND sets `CA_DBPROCESS` for a
`pvlOptCP` link (or a `pvlOptCPP` link whose holder is Passive). The worker
then runs `db_process(prec)`, `dbCaGetLink` returns `-1` with
`INVALID_ALARM`/`LINK_ALARM` (`dbCa.c:459-463`), and the record commits
LINK/INVALID. calink did only the first. A Passive CP holder is by definition
never scanned, so nothing else could ever notice — the guest served its last
good value with `SEVR=NO_ALARM` for the entire 65 s outage.

Fixed structurally in `bb7b40ab`. The flag was a bare `Arc<AtomicBool>` with
three independent `store` sites, which is what let the second half be
forgotten. `LinkConnState` now owns it:

> **Invariant.** A `ca://` link's servability flag MUST NOT go from connected
> to disconnected without dispatching that PV's CP/CPP holders.
> **Owner.** `LinkConnState`. The flag is private; `mark_connected` /
> `mark_disconnected` / `is_set` are the only surface, and the dispatch lives
> inside `mark_disconnected` on the true→false EDGE (`swap`), so repeated
> `Disconnected` events — the watcher's, then `run_monitor`'s
> subscription-ended tail — dispatch once per outage rather than once per
> event, and a new fourth site cannot skip it.

Reconnect deliberately does NOT dispatch here: C's connect arm schedules work
and it is the monitor event that drives processing, which `run_monitor`
already does.

**Not the same defect, and not fixed here:** C `accessRightsCallback`
(`dbCa.c:1094-1099`) adds the same `CA_DBPROCESS` when read access is lost.
calink has no read-access gate in `with_servable` at all, so there is no state
for a dispatch to expose; closing that is a separate change with its own
parity work. Recorded in §11.7.

### 11.5 The seven criteria, as measured

All on the boot-5 image (`bb7b40ab`), console `~/rtems-bringup/c6/guest5.log`,
zero panics for the life of the boot.

**1. Banner reports the resolver and the link count — PASS.**

```
iocInit: 1 non-local DB link(s) made external
iocInit: 4 external CP link subscriptions (3 PVs warmed)
iocInit: 1 external link opens staged
rtems-ca-ioc: serving 17 records on CA port 5064 (TCP + UDP search), RTEMS execution model, no tokio runtime
INFO  epics_ca_rs::client: channel connected pv=UPSTREAM:OTHER cid=1 sid=4 server=10.0.2.2:15076
INFO  epics_ca_rs::client: channel connected pv=UPSTREAM:AI cid=2 sid=5 server=10.0.2.2:15076
INFO  epics_ca_rs::client: channel connected pv=UPSTREAM:FAST cid=3 sid=6 server=10.0.2.2:15076
INFO  epics_ca_rs::client: channel connected pv=UPSTREAM:AO cid=4 sid=7 server=10.0.2.2:15076
rtems-ca-ioc: calink resolver installed — 4/4 ca:// record links registered (UPSTREAM:AI, UPSTREAM:AO, UPSTREAM:FAST, UPSTREAM:OTHER); ` CA`-modified and ca://... INP/OUT resolve over EPICS_CA_NAME_SERVERS (TCP name servers; UDP search is compiled out on this target)
```

**2. `caget` from the host against the guest's downstream record returns the
upstream's value — PASS**, for BOTH spellings (`INP="UPSTREAM:AI CP"` and
`INP="ca://UPSTREAM:AI CP"`).

```
$ . guest-env.sh; caget -a RTEMS:CA:DOWN RTEMS:CA:DOWN2
RTEMS:CA:DOWN                  <undefined> 1
RTEMS:CA:DOWN2                 <undefined> 1
$ . up-env.sh; caget -a UPSTREAM:AI
UPSTREAM:AI                    <undefined> 1 UDF NO_ALARM
```

**3. `caput` to the upstream changes the guest record within one scan period,
through the MONITOR path — PASS.** Both records are `SCAN=Passive`, so no scan
can be doing it. Latency is host-arrival-to-host-arrival on the wire, because
the target's `Instant` is 1-second-quantized (§5.5) and cannot resolve this:

```
$ crit3.sh
=== upstream (host arrival | server ts | value):
1784781395.952850 UPSTREAM:AI                    2026-07-23 04:36:35.952601 42.5
1784781397.993735 UPSTREAM:AI                    2026-07-23 04:36:37.993496 7.25
1784781400.034792 UPSTREAM:AI                    2026-07-23 04:36:40.034561 133.75
=== guest   (host arrival | server ts | value):
1784781395.956926 RTEMS:CA:DOWN                  2026-07-23 04:36:35.952601 42.5
1784781395.957798 RTEMS:CA:DOWN2                 2026-07-23 04:36:35.952601 42.5
1784781397.997508 RTEMS:CA:DOWN                  2026-07-23 04:36:37.993496 7.25
1784781397.998513 RTEMS:CA:DOWN2                 2026-07-23 04:36:37.993496 7.25
1784781400.039159 RTEMS:CA:DOWN                  2026-07-23 04:36:40.034561 133.75
1784781400.039978 RTEMS:CA:DOWN2                 2026-07-23 04:36:40.034561 133.75
=== added latency, upstream arrival -> guest arrival:
  RTEMS:CA:DOWN    value=42.5      +4.076 ms
  RTEMS:CA:DOWN2   value=42.5      +4.948 ms
  RTEMS:CA:DOWN    value=7.25      +3.773 ms
  RTEMS:CA:DOWN2   value=7.25      +4.778 ms
  RTEMS:CA:DOWN    value=133.75    +4.367 ms
  RTEMS:CA:DOWN2   value=133.75    +5.186 ms
```

2.2-5.2 ms against a fastest-possible scan period of 100 ms. Note the guest
record adopts the UPSTREAM's timestamp verbatim (`04:36:35.952601` on both
sides) — the link timestamp is propagated, not restamped locally, which is
also why the guest's own 1 s clock quantum does not appear in these records.

**4. Kill the upstream: guest record goes LINK/INVALID. Restart it: the record
recovers WITHOUT rebooting the guest — PASS** (after §11.4).

```
== BEFORE: upstream alive (04:04:45)
RTEMS:CA:DOWN                  <undefined> 1
RTEMS:CA:DOWN2                 <undefined> 1
RTEMS:CA:OTHER                 <undefined> 0
FD_CNT FD_FREE CA_CONN_CNT = 10 140 0
guest->upstream TCP (all states): 2

##### KILL upstream softIoc pid=2633251
== AFTER KILL +15 s (04:05:00)
RTEMS:CA:DOWN                  <undefined> 1 LINK INVALID
RTEMS:CA:DOWN2                 <undefined> 1 LINK INVALID
RTEMS:CA:OTHER                 <undefined> 0 LINK INVALID
FD_CNT FD_FREE CA_CONN_CNT = 9 141 0
guest->upstream TCP (all states): 0
== AFTER KILL +35 s (04:05:20)              [unchanged: LINK INVALID, 9 141 0, 0]

##### RESTART upstream
== AFTER RESTART +30 s (04:05:50)
RTEMS:CA:DOWN                  <undefined> 1
RTEMS:CA:DOWN2                 <undefined> 1
RTEMS:CA:OTHER                 <undefined> 0
FD_CNT FD_FREE CA_CONN_CNT = 9 141 0
guest->upstream TCP (all states): 2
== AFTER RESTART +55 s (04:06:16)
FD_CNT FD_FREE CA_CONN_CNT = 10 140 0
guest->upstream TCP (all states): 2

##### proof the guest never rebooted:
RTEMS:UPTIME = 00:02:34
boot markers in guest5.log = 1
```

`RTEMS:UPTIME` is monotonic across the whole outage and `rtems-boot: main()
reached` appears exactly once, so the recovery is a reconnect, not a restart.
The alarm clears and the value returns with the upstream, and this survives
the 1 s clock quantum (§5.5) — the reconnect ladder is
`EPICS_CA_CONN_TMO`-paced (30 s default), comfortably above it.

**5. An OUT link writes and is observed upstream, in BOTH `LinkPutOp`
flavours — PASS.** One `ao` covers both because the op is chosen by the
originating write, not by the link: `Database::external_put_op`
(`database/links.rs:1801-1806`) returns `Async` when the source record carries
a put-notify wait-set and `Plain` otherwise, so `caput` exercises `put_nowait`
and `caput -c` exercises `put`.

```
$ crit5.sh
--- caput (plain -> put_nowait) RTEMS:CA:UPLNK 21.5
21.5
--- caput -c (put-notify -> put) RTEMS:CA:UPLNK 64.25
64.25
--- guest-side readback of the OUT record:
RTEMS:CA:UPLNK                 2014-04-14 08:02:36.416902 64.25
=== UPSTREAM:AO at the upstream IOC (host arrival | server ts | value):
1784781361.519473 UPSTREAM:AO                    2026-07-23 04:36:01.519244 21.5
1784781364.570160 UPSTREAM:AO                    2026-07-23 04:36:04.569948 64.25
```

The `caput -c` returned, which is the put-notify completing end to end through
the guest's async external put. (The guest record's own timestamp reads
`2014-04-14` — the 1 s-quantized target clock with no NTP, §5.5. It does not
affect the write, and the upstream restamps.)

**6. Thread and stack census matches §5.2's arithmetic, and the circuit count
to the upstream is 1 regardless of link count — PASS.**

```
TASKDUMP begin tag=c6-180 count=30 scheduler_sc=0
TASKDUMP id=0x0b010002 core=140 posix= 115 thread=cbLow
TASKDUMP id=0x0b010003 core=135 posix= 120 thread=cbMedium
TASKDUMP id=0x0b010004 core=128 posix= 127 thread=cbHigh
TASKDUMP id=0x0b010005 core=129 posix= 126 thread=cbTimer
TASKDUMP id=0x0b010006 core=132 posix= 123 thread=scanOnce
TASKDUMP id=0x0b010007 core=189 posix=  66 thread=status-pv
TASKDUMP id=0x0b010008 core=181 posix=  74 thread=CAS-TCP
TASKDUMP id=0x0b010009 core=183 posix=  72 thread=CAS-UDP
TASKDUMP id=0x0b01000c core=150 posix= 105 thread=CAC-reader 10.0
TASKDUMP id=0x0b010010 core=189 posix=  66 thread=c6-probe
TASKDUMP id=0x0b010014 core=148 posix= 107 thread=CAC-writer 10.0
TASKDUMP id=0x0b010015 core=148 posix= 107 thread=CAC-writer 10.0
TASKDUMP id=0x0b01001d core=150 posix= 105 thread=CAC-reader 10.0

ID         NAME                  AVAIL     USED
0x0b010003 cbMedium            1048560   266296
0x0b01000c CAC-reader 10.0      262128     1576
0x0b010014 CAC-writer 10.0      262128     2264
0x0b010015 CAC-writer 10.0      262128     2264
0x0b01001d CAC-reader 10.0      262128     1288
```

* **4 CAC threads**, in two reader/writer pairs, for **4 links** — the
  per-peer-not-per-link property (§2.4), on target. §5.2's arithmetic is
  2 threads per upstream circuit + 2 per name-server circuit = 4; measured 4,
  stable across every census in the boot.
* **`AVAIL 262128` = `StackSizeClass::Small`** (262,144 − 16 bytes of guard),
  exactly §5.2's table. Peak use 1,288-2,264 bytes, i.e. under 1 % — the CA
  client's per-circuit stack is nowhere near its class.
* **The data circuit is 1**: `C6 seq=185 links=4 circuits=Some(1)` on the
  console (`CaClient::ioc_connection_count`), with 4 links open and connected.
  Host side, `ss -tn '( dport = :15076 )'` shows **2** guest→upstream sockets:
  the one IOC data circuit plus the name-server circuit. Those are the same 2
  peers the 4 CAC threads serve.
* **Base census is 30 tasks.** A census taken while an inbound CA client is
  connected reads 32; the delta is exactly `CAS-client-bloc` and
  `CAS-event-block`, the per-client server pair. §5.2's arithmetic is about
  the CLIENT side and is met to the thread.
* `cbMedium`'s 266,296-byte peak against a `Big` 1,048,560-byte stack is the
  inline CP fan-out running record chains on the band — the §5.4 shape,
  visible in the stack numbers. 25 % of a `Big` stack, on a 9-record chain.

**7. `FD_CNT` equals the pre-link value plus exactly the circuits opened —
PASS, arithmetic in every sample.**

```
$ crit7.sh
== baseline: links up, only the reading caget inbound
FD_CNT FD_FREE FD_MAX = 10 140 150
guest->upstream TCP  = 2
+1 inbound camonitor: FD_CNT FD_FREE FD_MAX = 11 139 150
+2 inbound camonitor: FD_CNT FD_FREE FD_MAX = 12 138 150
+3 inbound camonitor: FD_CNT FD_FREE FD_MAX = 13 137 150
+4 inbound camonitor: FD_CNT FD_FREE FD_MAX = 15 135 150
back to 0 extra inbound: FD_CNT FD_FREE FD_MAX = 11 139 150

-- with only the reading caget inbound:
CA_CONN_CNT FD_CNT = 0 10
+1 inbound: CA_CONN_CNT FD_CNT = 1 11
+2 inbound: CA_CONN_CNT FD_CNT = 2 12
+3 inbound: CA_CONN_CNT FD_CNT = 4 14
back to 0 extra: CA_CONN_CNT FD_CNT = 1 10
```

* `FD_CNT + FD_FREE = FD_MAX = 150` in **every** sample — the configured
  `CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS` (§5.3), confirmed on target.
* `FD_CNT − CA_CONN_CNT = 10` in every sample: the descriptor cost of an
  inbound client is exactly 1 and it is exactly what `CA_CONN_CNT` counts.
  (The two samples reading `4 14` and `1 10` are a teardown not yet reaped —
  the invariant still holds in both.)
* **The link cost, isolated by the criterion-4 outage:** with both outbound
  circuits up `FD_CNT` is 10, with both closed it is 9, `CA_CONN_CNT`
  unchanged at 0. Stated exactly: the two circuits account for **one
  descriptor more** than the reconnecting client holds. The absolute per-
  circuit cost is NOT isolated by this measurement, because a client with a
  link configured never sits at zero descriptors — it retains one while
  retrying. §5.3's table ("+1 fd per circuit, +2 for name server + upstream")
  is therefore an upper bound that this image meets or beats; the fd wall it
  predicts (140 inbound clients with links) is not lowered by anything
  measured here.

### 11.6 The stage C4 band-occupancy measurement — THE DECISION INPUT

This is the measurement the C4 sign-off (§6, 2026-07-23) made a precondition
of C6 and of any second sign-off on the two closures.

**What is loaded.** `UPSTREAM:FAST` → `RTEMS:CA:FAST` (a `ca:// … CP` link)
→ `FLNK` chain `RTEMS:CA:C1 … C8`: **9 records processed inline** on the
`cbMedium` worker that received the monitor event, which is exactly
`run_monitor`'s inline `dispatch_external_cp_targets` (§5.4).

**What is watched, and why two victims.**

1. `UPSTREAM:OTHER` → `RTEMS:CA:OTHER`, an **independent `ca://` link** whose
   monitor task shares the same one-worker band. Latency is (host arrival of
   the guest's update) − (host arrival of the same value from the upstream).
   Both hops are the same host over loopback, so the SLIRP term is common to
   every sample and cancels out of the baseline-vs-load comparison.
2. `RTEMS:CA:TICK`, written every 200 ms by a `runtime::task::spawn` +
   `runtime::task::sleep` loop, which on this target **is** a `cbMedium` band
   task. This is the timer half of the question.

Everything is timestamped on the WIRE from the host: the target's `Instant`
is 1-second-quantized (§5.5) and can resolve none of this.

**Method.** 90 s baseline (load link idle) → 90 s load → 30 s recovery, one
continuous pair of monitors across all three phases. The load is driven by a
host-side `calcout` at `.1 second`, flipped on and off with
`caput UPSTREAM:DRV.SCAN`. The victim link is driven by a second host-side
`calcout` at `1 second`, so every phase collects the same number of
independent latency samples. `RTEMS:CA:TICK` is POLLED rather than monitored,
because the probe writes it through `put_pv`, which posts no CA monitor;
polling and differentiating gives tick RATE, which on a one-worker band is a
better occupancy measure than jitter anyway — occupancy shows up as ticks the
timer task could not take.

**Results.**

```
== victim 1: independent ca:// link  UPSTREAM:OTHER -> RTEMS:CA:OTHER
   (monitor-to-record latency, host wire timestamps)
   baseline  n= 90  median=   2.98 ms  mean=   3.06 ms  p95=   3.50 ms  max=   4.54 ms
   load      n= 90  median=  12.43 ms  mean=  12.62 ms  p95=  13.71 ms  max=  18.65 ms
   recovery  n= 30  median=   2.92 ms  mean=   3.03 ms  p95=   4.01 ms  max=   4.39 ms
   ADDED DELAY  typical (median-median) = +9.45 ms
   ADDED DELAY  worst   (max-max)       = +14.11 ms
   ADDED DELAY  worst   (load max - baseline median) = +15.67 ms
   load link UPSTREAM:FAST during baseline    0 events
   load link UPSTREAM:FAST during load      900 events /  89.9 s = 10.00 Hz
   load link UPSTREAM:FAST during recovery    0 events

== victim 2: cbMedium timer task  RTEMS:CA:TICK  (nominal 5.00 tick/s)
   baseline  n= 27  median= 4.70 tick/s  min= 4.64  max= 5.18
   load      n= 42  median= 4.70 tick/s  min= 4.67  max= 5.17
   recovery  n= 14  median= 4.70 tick/s  min= 4.69  max= 5.17
   TICK RATE  baseline 4.70/s -> load 4.70/s  (+0.0 %)
   implied per-tick period  baseline 212.8 ms -> load 212.8 ms  (added -0.0 ms)
   worst 2 s window under load: 4.67 tick/s = 214.0 ms/tick

== chain end RTEMS:CA:C8 monitor events: 902
```

**The numbers, stated for the sign-off.**

| quantity | value |
|---|---|
| load actually applied | **10.00 Hz**, 900 events over 89.9 s |
| chains actually run | **902** (`RTEMS:CA:C8` monitor events) — one per event |
| victim link, typical added delay | **+9.45 ms** (median 2.98 → 12.43 ms) |
| victim link, worst-case added delay | **+15.67 ms** (baseline median → load max 18.65 ms) |
| victim link, worst-vs-worst | +14.11 ms (max 4.54 → 18.65 ms) |
| `cbMedium` 200 ms timer, added delay | **0.0 ms** (4.70 tick/s in all three phases) |
| recovery | complete and immediate: median back to 2.92 ms |

**Reading these numbers.**

* The occupancy is **real and it is measurable** — a 4× rise in an independent
  link's monitor-to-record latency is not noise, and it is caused by nothing
  but the other link's chain, since the victim's own rate, path and payload
  are identical in all three phases and recovery is complete.
* It is **bounded and small in absolute terms**: mid-teens milliseconds at the
  worst, against a fastest EPICS scan period of 100 ms. No sample in any phase
  came close to one scan period.
* The load distribution is **tight**, not long-tailed (median 12.43, mean
  12.62, p95 13.71). Every victim update under load is delayed by about the
  same amount. That is the signature of a band whose run queue always has
  pending work at 10 Hz — the victim waits its turn behind queued chain work —
  rather than of occasional collisions with an in-flight chain.
* The **timer half is unaffected**, which is the more surprising result and
  the one that most constrains the interpretation: the cooperative executor
  releases the worker between chains (§5.4 point 3), so a 200 ms timer never
  misses its slot even at 10 chains/s. The 4.70 tick/s (vs a nominal 5.00) is
  a constant present in ALL three phases — sleep granularity and probe
  overhead, not load.
* Stack evidence agrees: `cbMedium` peaks at 266,296 bytes of its `Big`
  1,048,560-byte stack (§11.5) — the chain runs on the band, deeply, but well
  inside its class.

**What this does NOT settle.** The measurement is one shape: 9 records, one
loaded link, 10 Hz, on an otherwise-idle IOC. It does not bound a 50-record
chain, a chain containing an async record, several loaded links at once, or a
band that also carries QSRV group forwarders and pvalink forwarders (the
generalisation §5.4's structural option was argued from). The invariant in
§5.4 remains stated and remains unenforced by construction. What the numbers
say is that on the C6 topology the *urgency* is low: nothing here is a missed
deadline, a starved timer, or an unbounded occupancy — it is a single-digit-
to-mid-teens millisecond latency tax on other links sharing the band.

### 11.7 Open after C6

1. **Read-access loss does not process CP holders.** C
   `accessRightsCallback` (`dbCa.c:1094-1099`) adds `CA_DBPROCESS` when read
   or write access is lost, the same as the disconnect path §11.4 closed.
   calink has no read-access gate in `with_servable` at all, so both halves
   are missing: the gate and the dispatch. Distinct from §11.4 and not fixed
   here.
2. **`RTEMS:CA:TICK` is UDF/INVALID and posts no CA monitor.** The C6 probe
   writes it through `PvDatabase::put_pv`, which neither clears UDF nor posts
   a monitor. Harmless for the probe (the measurement polls instead) but it
   means `put_pv` is not a record-processing write; anything that needs a
   monitor must not use it.
3. **The C6 probe records and threads are in `rtems-ca-ioc`.** `DEMO_DB`
   carries 14 probe records and `main` starts two probe threads (`c6-probe`,
   the tick task). They are the measurement rig, not IOC content, and should
   come out — or move behind a feature — before the binary is anything but a
   bring-up image.
4. **§5.3's fd table is an upper bound, not an equality.** §11.5 criterion 7
   could not isolate the absolute per-circuit descriptor cost because a client
   with a link configured never holds zero descriptors. Isolating it needs an
   image built with no link configured at all.
5. **Stage C4's second sign-off** is now unblocked with numbers (§11.6). The
   two closures (enqueue restructure vs C-style dedicated thread) are
   unchanged and still not picked.
6. Everything in §10.10 that C6 did not touch remains as §10.10/§10.11/§12
   record it. Item 2's boot panic is CLOSED (§12: transport selection now keys
   on the backend, and both IOC binaries boot feature-ON), but the narrower
   gap stands: the hosted `rtems-exec-model` build still stands up no live
   upstream, so neither link resolver's *resolution* path runs on a host. That
   gap is precisely why all four defects in this section had to be found by
   booting the target.

## 12. §10.10 item 2 as built — the predicate was the defect

### 12.1 What was actually wrong

§10.10 item 2 read the panic as a missing seam: `AsyncUdpV4` needs a reactor,
so give UDP a runtime seam in `epics-base-rs` the way `dial` and the timers
have one. That is one layer too low. The seam already existed — `SearchTransport`
is a two-variant sum type and its `NameServersOnly` variant needs no socket at
all — and what was broken was the predicate that chose between the variants.

Both clients gated UDP on `not(target_os = "rtems")`, which is a true statement
about the target (no `recvmsg`/`IP_PKTINFO`, `tokio::net` does not build for the
triple) and the wrong fact. What a `tokio::net` socket needs is a **reactor on
the thread its future runs on**, and that is a property of the *task backend*:
every client task starts through `runtime::task::spawn`, and under the exec
backend that lands on a callback-pool worker the runtime was never entered on.
A hosted process with a tokio runtime elsewhere does not help — which is also
why no `#[tokio::test]` in either crate could ever see this, since the test's
own thread has a reactor.

So the rule is now emitted by each client's `build.rs`,

```text
exec_backend  <=>  target_os == "rtems" || feature "rtems-exec-model"
tokio_backend <=>  otherwise
```

and the UDP transport, the CA beacon monitor and the TCP dial all take
`tokio_backend`. On `exec_backend` `SearchTransport` has the single
`NameServersOnly` variant in both crates. The illegal state — exec backend plus
a reactor-needing socket — is not checked for at runtime; it cannot be
constructed. The compiled surface on target is unchanged, because
`target_os = "rtems"` implies `exec_backend`; what changed is that the host
exec-model build now reproduces the target's configuration instead of
approximating it, which is the only reason it is worth booting.

### 12.2 The drift the rule invites, and the guard for it

Three copies of a four-line rule (`epics-base-rs`, `epics-ca-rs`,
`epics-pva-rs`) is two too many to trust, and cargo features unify per
*package*: a manifest could turn on `epics-base-rs/rtems-exec-model` alone,
giving `spawn` a reactor-free backend while a client still compiled the
reactor-backed transport in and selected it — precisely the panicking
configuration, reachable without anyone meaning to. `SearchTransport`'s type
cannot rule that out, because the two crates disagree about which variants
exist.

`epics-base-rs` therefore exports `runtime::task::HAS_TOKIO_REACTOR` and both
clients assert their own view against it in a `const`. A split build fails to
compile instead of panicking at boot. The guard found a live instance on its
first run: `epics-bridge-rs`'s `rtems-exec-model` forwarded to `epics-base-rs`
and nothing else, so `rtems-pva-ioc` — the binary that feature exists for — was
built in exactly that state.

### 12.3 The gate

`rtems-ca-ioc` and `rtems-pva-ioc` are each booted as a child process over a
temporary database (`epics-ca-rs/tests/rtems_ca_ioc_boots.rs`,
`epics-bridge-rs/tests/rtems_pva_ioc_boots.rs`), watched to the resolver-install
line and then to positive evidence that the client reached the seam — the CA
IOC's refused name-server dial, the PVA IOC's STAGE-5 probe reporting a search
in flight. The assertion is liveness *and* a clean console: a panic on a
callback-pool worker kills that worker and leaves the IOC serving, so liveness
alone proves nothing. Both tests fail on the pre-fix tree, each on
`net/async_udp_v4.rs:1275`.

| gate | result |
|---|---|
| `cargo fmt --all` | clean |
| `cargo clippy -p epics-base-rs -p epics-ca-rs -p epics-pva-rs -p epics-bridge-rs -p rtems-exec-gate --all-targets -- -D warnings` | clean |
| … `--features …/rtems-exec-model` (all four) | clean |
| `cargo nextest run -p epics-base-rs -p epics-ca-rs -p epics-pva-rs -p epics-bridge-rs -p rtems-exec-gate` | 6337 passed, 2 skipped |
| … `--features …/rtems-exec-model` (all four) | 6126 passed, 2 skipped |
| `./scripts/rtems-check.sh` (portability + image) | exit 0; PVA client probe 0 target errors (ratchet held) |
| `rtems_exec_model_gate` census, all three crates | pass |
| `rtems-ca-ioc` / `rtems-pva-ioc` boot feature-ON | no panic; pre-fix tree panics at `net/async_udp_v4.rs:1275` |

### 12.4 What this does not do

The dial reached in §12.3 is refused, not completed: no upstream server is
stood up, so neither resolver's *resolution* is verified feature-ON, only that
its client path runs. Driving a link to `connected` on the host exec model is
the next step and is not this change.

Two warnings in `epics-pva-rs` were outside this change: a deprecated
`fetch_update` and the never-constructed `Origin::{FromOriginTag, Forwarded}`
in `server_native/search_engine.rs`. Both were closed in the same integration
round — §10.11 items 4–5.
