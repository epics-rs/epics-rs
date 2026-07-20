# RTEMS Runtime Portability — Design Proposal

**Status:** Draft / proposal (not yet accepted)
**Date:** 2026-07-20
**Scope:** How epics-rs reaches RTEMS (and stays clean on Linux / macOS /
Windows / FreeBSD) without owning a bespoke async runtime.

This document records a decision *proposal* and the measured facts behind it.
Every load-bearing claim cites either the C/C++ reference or a port
measurement. Items that were reasoned-but-not-compiled are called out under
**Unverified** — they must be closed with an actual `cargo check` against the
RTEMS target before this proposal is accepted.

---

## 1. Goal

Run the epics-rs CA and PVA **servers** (an IOC) on RTEMS
(`armv7-rtems-eabihf`, Rust tier-3) while keeping the four already-working
platforms (Linux, macOS, Windows, FreeBSD) on their current, battle-tested
path. Do this **without** taking on the permanent cost of authoring and
maintaining a cross-platform async runtime.

Non-goal: replacing tokio on the desktop platforms for its own sake. tokio is
fine there. The driver for this work is RTEMS + footprint/determinism, not
dissatisfaction with tokio on Linux.

---

## 2. The constraint that shapes everything

**tokio's reactor is mio, and mio has no RTEMS backend.** (`Cargo.lock`:
`mio 1.2.0`.) mio supports epoll / kqueue / IOCP only.

**What RTEMS libc actually gives us** (verified by reading rust-lang/libc
`src/unix/newlib/rtems/mod.rs` (148 lines) + `src/unix/newlib/mod.rs` + the
top-level `src/unix/mod.rs`):

- **Present:** blocking BSD sockets (`socket`/`connect`/`accept`/`listen`/
  `send`/`recv`), `bind`/`recvfrom`/`ioctl`, `fd_set` + `FD_SET/CLR/ISSET/ZERO`,
  **`poll()` and `select()`** (declared in the top-level unix extern block with
  no newlib/rtems exclusion), `getaddrinfo`, and — notably —
  `getentropy` / `arc4random_buf` (in the rtems submodule).
- **Absent:** `kqueue`/`kevent`/`EVFILT`, `epoll_*`. None appear anywhere in
  the RTEMS/newlib bindings.

So RTEMS can do blocking sockets and `poll()`/`select()` readiness today, but
**cannot** host mio's epoll/kqueue backends. mio also has no `poll`/`select`
backend. That is the whole reason tokio-as-is does not run on RTEMS.

Consequence: of the five target platforms, **four already run on tokio+mio**
(FreeBSD via the same kqueue backend as macOS). The genuine gap is exactly
one platform, RTEMS, and it needs a `poll`/`select`-based path.

---

## 3. What the C/C++ originals actually do (verified from source)

The two reference implementations we mirror have **opposite** I/O models. This
is the single most important input to the design: they do not want the same
RTEMS treatment.

### 3.1 CA server (RSRV, epics-base C) = blocking thread-per-client

Local source: `/home/stevek/work/epics-base/modules/database/src/ioc/rsrv/`.

- `req_server()` (`caservertask.c:62`) is the accept loop. On each accept it
  calls `create_tcp_client()` then spawns **one thread per connection**:
  `epicsThreadCreate("CAS-client", epicsThreadPriorityCAServerLow, camsgtask, pClient)`
  (`caservertask.c:109-111`).
- `camsgtask()` (`camsgtask.c:41`) is a `while` loop over **blocking**
  `recv(client->sock, ...)` (`camsgtask.c:71`). No select/poll/epoll/kqueue.
- Client threads run at `epicsThreadPriorityCAServerLow` — **below** the scan/
  control tasks, so the RTOS preempts network work for control work.
- Monitor (subscription) sends are split onto a separate event task via
  `db_start_events(..., "CAS-event", ...)` (`caservertask.c:1514`).

→ On RTEMS this needs **only blocking sockets** (which RTEMS libc has). **No
reactor.** The synchronous thread-per-client model is exactly what C does.

### 3.2 PVA server (pvxs C++) = single libevent reactor thread

Local source: `/home/stevek/work/epics-modules/pvxs/src/`.

- `evbase::Pvt` (`evhelper.cpp:116`) runs **one** worker `epicsThread`
  (`evhelper.cpp:138,150`) that executes `event_base_loop()`
  (`evhelper.cpp:207`). All connections are multiplexed on that one reactor as
  `bufferevent`s (`conn.cpp`, `serverchan.cpp`).
- The readiness backend is chosen by libevent
  (`event_base_new_with_config` `evhelper.cpp:185`;
  `event_config_avoid_method("kqueue")` on some platforms `evhelper.cpp:183`).

→ PVA is reactor-based **even in C++**. "sync thread-per-client" does **not**
fit it. Its RTEMS path is a `select`-backed reactor (libevent itself has a
select backend), not a collapse to blocking threads.

---

## 4. Decision (proposed)

Adopt a **thin runtime seam + per-platform drivers**, and reject a bespoke
runtime rewrite.

```
        ┌──────────────────────────────────────────────┐
        │      protocol cores (sans-io where it pays)   │   ← platform-agnostic
        │      CA state machine · PVA operations        │
        └───────────────┬──────────────────────────────┘
                        │  runtime seam (Spawn / Reactor / Timer / net traits)
        ┌───────────────┴───────────────┬──────────────────────────┐
        │  desktop driver               │  RTEMS driver             │
        │  (tokio; lighter stack later) │  CA: blocking thread/conn │
        │  Linux/macOS/Win/FreeBSD      │      (NO reactor)         │
        │                               │  PVA: select reactor +    │
        │                               │       minimal executor    │
        └───────────────────────────────┴──────────────────────────┘
```

- **Desktop (Linux/macOS/Windows/FreeBSD):** keep tokio. It already works on
  all four. (If desktop footprint later proves a real problem — measured, not
  assumed — migrate this driver to the lighter `smol`/`async-io`/`polling`
  stack. That is a driver swap behind the seam, not a rewrite.)
- **RTEMS / CA:** blocking thread-per-client. Mirrors RSRV exactly, needs no
  reactor — the cheapest possible RTEMS path.
- **RTEMS / PVA:** a `select()`-based single-thread reactor + a minimal futures
  executor that drives PVA's existing per-operation tasks. Mirrors what
  libevent does for pvxs.

### 4.1 Rejected alternative: bespoke cross-platform runtime

Writing our own reactor+executor for all five platforms means, concretely,
**re-implementing mio** (epoll/kqueue/IOCP/poll-select) plus a scheduler,
timer wheel, async net types, and re-homing ~487 `tokio::sync::*` call sites —
then owning that runtime across five kernels forever. It would spend
person-years to replace a battle-tested runtime on the four platforms that
already work, in order to serve the one platform (RTEMS) that needs only a
`poll`/`select` reactor. Runtime bugs surface as rare production hangs/races —
the worst failure class for a controls system. Cost/benefit is lopsided;
this is a fallback, triggered only if the lighter-stack driver is *measured*
unable to meet a hard RTEMS footprint/determinism target.

---

## 5. sans-io: what it is, and when it is actually required

**sans-io** = pull I/O out of the protocol logic so that
`bytes-in + state → bytes-out + effects` is a **synchronous, pure**
computation. Socket read/write and concurrency live at the edge as swappable
drivers.

sans-io and the runtime seam are **independent axes**:

- sans-io is **required** only to get CA's *lightest* RTEMS model — fully
  **synchronous** blocking thread-per-client with **no runtime at all** on
  RTEMS.
- If a minimal async runtime on RTEMS is acceptable, the **runtime seam alone**
  is enough and sans-io is optional.
- **PVA is reactor-shaped either way**, so sans-io does not unblock RTEMS for
  PVA — it only improves PVA's testability (protocol logic testable without a
  socket).

sans-io is **orthogonal to dependency portability**: perfect sans-io does not
help if `socket2`/`getrandom` will not build on RTEMS (see §8).

---

## 6. CA plan and sizing

Current shape (measured, `crates/epics-ca-rs/src/server/`):

- `handle_client` (`tcp.rs:1613`) is the per-connection loop; `dispatch_message`
  (`tcp.rs:2184`) already receives a **parsed** `(hdr, payload)` — the input
  side is already close to sans-io. Framing is separated in `handle_client`.
- The **output** side is coupled: handlers hold
  `writer: &Arc<Mutex<BufWriter<W>>>` and `.write_all().await` (~150 write
  sites). Replies are already batched into a `BufWriter` and flushed at the
  loop bottom (partial produce/flush split).
- Production `tokio::spawn` is essentially **one** non-test site
  (`tcp.rs:4006`, put-notify completion); the rest are tests. The only ongoing
  async activity is monitors (`monitor.rs`) + put-notify.
- **DB access is lock-based, not I/O.** `PvEntry::snapshot()`
  (`epics-base-rs/src/server/pv.rs:489`) awaits `self.value.read().await` — a
  `tokio::sync::RwLock`, not a remote wait. The db path is dominated by
  RwLock/Mutex, not channels.

Two cut depths:

- **Shallow (wire/reply sans-io, epics-ca-rs only):** replace the `writer`
  param with an output sink (`&mut Vec<u8>` / reply accumulator); turn the ~150
  `.write_all().await` into synchronous pushes. Wire encoding becomes testable
  without a socket. Handlers stay `async` because they still await the db, so
  this alone does **not** yield a blocking-thread RTEMS driver. One crate, low
  architectural risk; the real cost is converting the many `DuplexStream`-driven
  handler tests to byte-buffer assertions. **Order: weeks.**

- **Deep (fully-synchronous CA core — what enables RTEMS blocking-thread):** on
  top of the shallow cut, de-async the db —
  `tokio::sync::RwLock → std::sync::RwLock`, `snapshot()`/`set()`/`get_record()`/
  `put_*` from `async fn` to `fn`. Then `dispatch_message` and every handler
  drop `async`; async remains only at the socket edge, owned by the driver.
  **Blast radius crosses crates:** the de-async of `snapshot()`/`set()` ripples
  to every caller — `snapshot()` alone is ~226 call-sites across **6 crates**
  (epics-pva-rs, epics-bridge-rs, epics-base-rs, epics-ca-rs, ad-core-rs,
  asyn-rs). The monitor path (a spawned async producer fed by a broadcast
  channel) must become a synchronous "on db change, enqueue to client outbox"
  callback — a real redesign of the subscription mechanism. **Order:
  substantial — multi-week, cross-crate, redesigns the monitor path.**

Decisive enabler: because the db async is locks (not I/O), the deep cut is
*feasible* — a blocking thread uses `std` locks and needs **no reactor**.

---

## 7. PVA plan and sizing

Current shape (measured, `crates/epics-pva-rs/src/server_native/`):

- **The wire-output boundary already exists as a channel.** `tcp.rs:3082`
  ("a single dedicated writer task drains it in arrival order"), `:3215` ("All
  emit sites push framed bytes"), `:3234` the writer task drains the channel
  and `write_all`s. Handlers push frames to an mpsc; they do not write the
  socket directly. `handle_message` (`tcp.rs:6023`) is already a **sync `fn`**.
  → What CA's shallow cut must *build*, PVA already has.
- **But PVA is intrinsically multiplexed.** 68 `tokio::spawn` across the crate:
  per-connection reader/writer/heartbeat tasks, plus **per-operation** executors
  (monitor / get / put / RPC), with `MonitorFinished`/`ExecFinished` completion
  channels. One connection carries many concurrent long-lived operations —
  exactly pvxs's libevent shape.
- Same lock-based db async (`native_source.rs` uses `tokio::sync::RwLock`;
  `snapshot().await` is a lock read).

Implication — PVA is CA's mirror image:

| | CA (RSRV) | PVA (pvxs) |
|---|---|---|
| RTEMS direction | collapse to **sync** thread-per-client | keep **async**, swap the runtime |
| reactor needed? | **no** | **yes** (select-based) |
| db de-async needed? | **yes** (dominates cost) | **no** — keep async db under the executor |
| wire output split | must be built (~150 sites) | **already a channel** |
| monitor | redesign to sync callback | keep existing tasks |

So PVA's RTEMS bottleneck is **not** sans-io (largely done) — it is the
**`select`-based reactor + minimal executor** that drives the 68 spawned tasks.
That piece does not evaporate for PVA the way it does for CA, because PVA is
reactor-shaped even in C++. It is new systems work but not novel — it is what
libevent already does, expressed as a Rust futures executor. Crucially, PVA
does **not** need the db de-async (the cross-crate blast radius that dominates
CA's deep cut) because a select executor simply awaits the existing db futures.
**Order: substantial, dominated by the reactor/executor, not the protocol.**

---

## 8. Dependency audit (RTEMS)

The workspace already made portability-friendly choices; the core dependency
surface that must cross to RTEMS is smaller than the raw dep list suggests.

### 8.1 Hard blockers — core network path, must solve

**All three suspects CONFIRMED to fail `cargo check --target armv7-rtems-eabihf`
on 2026-07-20** (isolated probe crate, nightly `-Z build-std`; see §11 for the
run). The failures are deeper than "the crate doesn't recognize the target":
in several cases the symbol is genuinely **absent from rust-lang/libc's
newlib/rtems binding** (`libc-0.2.186/src/unix/newlib/arm/mod.rs`), which is
minimal.

| Crate | Status | What is missing on RTEMS | Path |
|---|---|---|---|
| **mio 1.2** (under tokio) | blocked (by design) | no epoll/kqueue | §4 — select reactor, or CA blocking-thread needs none |
| **socket2 0.5** (`features=["all"]`, **non-optional**; asyn/base/ca/pva) | **FAILS — 20 errors** | libc lacks `msghdr`/`recvmsg`/`sendmsg`/`IovLen` (scatter-gather), `ip_mreqn` (only `ip_mreq` exists), `ip_mreq_source`+`IP_{ADD,DROP}_SOURCE_MEMBERSHIP`, `SOCK_RAW`/`SOCK_RDM`/`SOCK_SEQPACKET`, `MSG_TRUNC`/`MSG_EOR`, `IPV6_RECVHOPLIMIT`/`IPV6_RECVTCLASS`/`IP_HDRINCL`/`IP_RECVTOS` | Most missing symbols are for APIs CA/PVA do **not** use (raw/seqpacket sockets, source-specific multicast, sendmsg/recvmsg). The subset they need — basic multicast join (`ip_mreq`, present), `setsockopt` for SO_REUSEADDR/buffer sizes — works over raw libc. Wrap that subset; do not port all of socket2. |
| **getrandom 0.2.17** (transitive: ed25519 / rustls / ahash …) | **FAILS — explicit `compile_error!("target is not supported")`** | RTEMS not in getrandom 0.2's supported-target list | RTEMS newlib *does* have `getentropy`/`arc4random_buf` → register a custom source (`register_custom_getrandom!`) or move to a getrandom version/backend that maps RTEMS onto them |
| **if-addrs 0.13.4** | **FAILS — 2 errors** | libc lacks `getifaddrs`/`freeifaddrs`/`ifaddrs` for rtems; the non-Apple path also needs Linux `sockaddr_nl`/`AF_NETLINK`/`NETLINK_ROUTE`/`SOCK_RAW` | Interface enumeration must use an RTEMS-specific route (libbsd `getifaddrs` via a `-sys` binding, or an RTEMS ioctl-based enumerator). Not a drop-in. |

Positive result from the same run: **`std` itself builds for
`armv7-rtems-eabihf`** via `-Z build-std` **without** an RTEMS gcc/BSP present —
so `cargo check` is a usable RTEMS gate on this machine today. The blockers are
all in these leaf crates, not in std.

#### 8.1.1 Applied gating — `epics-base-rs` (2026-07-20)

`cargo +nightly check -p epics-base-rs --lib -Zbuild-std=std,panic_abort
--target armv7-rtems-eabihf` now **exits 0** (no warnings). The `--lib` gate was
walked one failing crate at a time; each wall and the gate that closed it:

| Crate (ver) | Error class | Dependency path (from `epics-base-rs`) | Gate applied |
|---|---|---|---|
| nix 0.31.3 | 21 errs — no `sa_sigaction`/`SA_RESTART` in RTEMS libc | `nix ← rustyline` (direct) | `rustyline` → `[target.'cfg(not(target_os="rtems"))'.dependencies]`; `Iocsh::run_repl_interactive` (+ helpers `use_ansi_color`/`strip_ansi`/`format_error`) gated `cfg(not rtems)`; `run_repl` falls through to the existing piped-stdin REPL on RTEMS |
| if-addrs 0.13.4 | 2 errs — no `getifaddrs`/`ifaddrs` | `if-addrs ← epics-base-rs` (direct) | `if-addrs` → host-only dep (used only by `net::iface_map`) |
| socket2 0.5/0.6 | 20 errs — no `SOCK_RAW`/`msghdr`/cmsg | `socket2 ← epics-base-rs` (direct) **and** `socket2 ← tokio (net)` | `socket2` → host-only dep; `#[cfg(not(target_os="rtems"))] pub mod net` (no in-crate users outside `src/net`); tokio `net` feature dropped (below) |
| mio 1.2.0 | 29 errs — no epoll/kqueue selector (`sys::selector`/`waker`/`event`, `IoSourceState`) | `mio ← tokio (net, signal)` | tokio declared per-target; RTEMS drops `net`/`signal`/`process` |
| signal-hook-registry 1.4.8 | 4 errs — no `SA_RESTART`/`sa_sigaction` | `signal-hook-registry ← tokio (signal)` | RTEMS tokio drops `signal`; the `tokio::signal` SIGINT/SIGTERM race in `IocApplication::run` becomes `std::future::pending()` on RTEMS (RTEMS is `cfg(unix)` too, so the guard is `all(unix, not(target_os="rtems"))`) |

**tokio per-target split.** Declared per-target rather than in shared
`[dependencies]` because Cargo *unions* a dependency's features across all
matching tables (a shared `full` would re-add the dropped features on RTEMS).
Hosted keeps `full`; RTEMS gets `default-features=false` with
`rt, rt-multi-thread, time, sync, macros, io-util, io-std, fs, parking_lot`
— `full` minus `net`/`signal`/`process`. This retained set (larger than the
"`sync`-only" first estimate) keeps every non-net tokio API the always-compiled
base code references — `tokio::runtime::Handle`, `block_in_place`, `select!`,
`pin!`, `tokio::sync::*` — type-checking on RTEMS with **no code gating** of the
sync bridge (`block_on_sync`), `runtime_handle`, iocsh runtime dispatch, or
`IocApplication::run`. Those tokio-runtime paths are unused on RTEMS (the sans-io
task seam routes spawn/sleep/interval to the background executor) but must still
compile for `--lib`. **Open decision for sign-off:** whether to narrow the RTEMS
tokio set further (toward `sync`-only) once the RTEMS IOC entry point exists and
those runtime paths are provably unreachable — narrowing would require gating the
above always-compiled call sites, so it is deferred, not silently chosen.

**Scope note.** This closes the `epics-base-rs --lib` walls only. The §8.1
core-network blockers (socket2/if-addrs raw-libc replacements, getrandom) are
**gated out** of base here, not solved — they return when the RTEMS CA/PVA
socket driver (§9 phase 3/5) is first built for the target.

### 8.2 Gateable — excluded from the RTEMS build

- **ring / rustls / tokio-rustls (TLS)** — already `optional = true`, behind the
  `experimental-rust-tls` feature (`epics-ca-rs/Cargo.toml:28,79`). The base
  CA/PVA build does **not** pull ring. TLS on RTEMS is a separate, later
  problem (swap ring → aws-lc-rs or a RustCrypto provider). **Not a hard
  blocker.**
- **hickory-dns (DNS, tokio-runtime)** — optional; EPICS uses IP/broadcast, so
  drop it on RTEMS.
- **nix** — already `target.'cfg(unix)'`-gated. Mostly procServ/forkpty
  (`epics-tools-rs`, Unix-only, irrelevant inside an IOC) + signals; the few
  `nix::net` socket-option sites move to socket2/raw.
- **areaDetector codecs** (image/jpeg/tiff/netcdf3/rust-hdf5), **device
  drivers** (rumqttc/vxi11/usbtmc/ftdi), **observability**
  (metrics-exporter-prometheus), **interactive iocsh** (rustyline), **benches**
  (criterion) — per-driver / host-only; an RTEMS IOC includes only the drivers
  it needs.

### 8.3 Portable — pure Rust, no OS assumptions

bytes, serde, thiserror, tracing, regex, bitflags, dashmap, arc-swap, zerocopy,
libm, chrono, flate2, lz4_flex — and, importantly, hashing via **RustCrypto**
(sha1/sha2/rc-hmac) rather than ring, and signatures via **ed25519-dalek**
(pure Rust). Good portability choices already in place.

---

## 9. Phasing

1. **Runtime seam.** Introduce a thin trait boundary (`Spawn` / `Reactor` /
   `Timer` + net newtypes) with a tokio driver behind it. Desktop behaviour
   unchanged. This is the enabling refactor for everything else.
2. **CA sans-io (shallow → deep).** Shallow first (wire/reply I/O-free, one
   crate). Then the deep cut (db de-async + monitor redesign) — the largest
   single piece, cross-crate; confirm scope with the user before starting since
   it reaches epics-base-rs and 6 crates of callers.
3. **RTEMS CA driver.** Blocking thread-per-client over RTEMS libc sockets. No
   reactor.
4. **Dependency closure.** Resolve socket2 / getrandom / if-addrs on the RTEMS
   target (§8.1); gate off §8.2.
5. **PVA `select` reactor + minimal executor.** Drives PVA's existing async
   tasks on RTEMS. PVA protocol is already channel-split, so this phase is
   dominated by the reactor/executor, not protocol surgery.
6. **(Fallback only)** If a *measured* desktop footprint/determinism target
   fails, swap the desktop driver to the `smol`/`polling` stack; if *that*
   cannot meet an RTEMS hard target, only then consider a bespoke runtime.

Phases 1–4 deliver a working RTEMS **CA** IOC — the cheaper, higher-value
half — before the heavier PVA reactor work in phase 5.

---

## 10. Success criteria (verifiable)

- Desktop: `cargo nextest run --workspace` stays green throughout; no behaviour
  change on Linux/macOS/Windows/FreeBSD (the tokio driver is the default).
- CA sans-io: the CA protocol core has tests that assert wire bytes from
  `(hdr, payload, state)` **without** any socket/`DuplexStream`.
- RTEMS CA: `cargo check --target armv7-rtems-eabihf` on the CA server crate +
  its deps builds; a blocking thread-per-client IOC accepts a CA connection and
  serves a `caget`/monitor under QEMU.
- PVA RTEMS: the select reactor drives an existing PVA monitor operation to
  completion under QEMU.

---

## 11. Unverified — must close before acceptance

These were reasoned from source/library presence, **not** compiled:

- ~~Whether **socket2 0.5**, **getrandom 0.2**, and **if-addrs** actually
  compile for `armv7-rtems-eabihf`.~~ **CLOSED 2026-07-20.** Ran
  `cargo +nightly check -Z build-std=std,panic_abort
  --target armv7-rtems-eabihf` against an isolated probe crate depending on the
  three at their locked versions. Result: **std built for RTEMS with no BSP
  present; all three leaf crates FAILED** — getrandom 0.2.17 with an explicit
  `compile_error!` (unsupported target), if-addrs 0.13.4 on missing
  `getifaddrs`/netlink symbols, socket2 0.5.10 with 20 errors on missing
  msghdr/recvmsg/sendmsg/ip_mreqn/SOCK_RAW/etc. Details and remediation in §8.1.
  Net: the RTEMS core-network dependency work is real but bounded to these three
  leaves (plus mio), and CA/PVA use only a subset of socket2 that maps onto
  what RTEMS libc *does* expose.
- Whether the `polling`/`async-io` stack can host an RTEMS `poll`/`select`
  backend today (it has a poll-based fallback; RTEMS-specific support unchecked)
  — only matters if the fallback desktop-stack swap in phase 6 is triggered.
- Exact `smol` migration cost if phase 6 fires: `broadcast` (41 sites) /
  `watch` (14) / `Notify` (64) have no drop-in in the `async-*` crates and need
  mapping.
- The CA deep-cut estimate depends on how many of the ~226 `snapshot()`/`set()`
  call-sites are already outside an async context (auto-reconciled by de-async)
  vs. need rewriting — not yet classified per-file.

---

## 12. One-line summary

Do **not** rewrite the runtime. Put a thin seam under the protocol cores; keep
tokio on the four platforms that already work; give RTEMS the one thing it
lacks — a blocking-thread CA driver (no reactor) and a `select`-based PVA
reactor — mirroring exactly what RSRV (thread-per-client) and pvxs (libevent)
already do in C.
